#!/usr/bin/env bash
# Build and test Phone Link on the build box, never on the laptop.
#
#   scripts/remote-build.sh build [--release]   cargo build, binaries come back to ./target/
#   scripts/remote-build.sh test  [ARGS...]     cargo test
#   scripts/remote-build.sh check [ARGS...]     cargo check
#   scripts/remote-build.sh clippy [ARGS...]    cargo clippy
#   scripts/remote-build.sh run -- CMD...       any command in the synced checkout
#   scripts/remote-build.sh gm                  build the Google Messages helper -> phonelink-gm/phonelink-gm
#   scripts/remote-build.sh release             release archive -> target/dist/ (needs data/google_oauth_client.json)
#   scripts/remote-build.sh log                 show the latest job's log (follows it if still running)
#
# The box is an ssh host. Set PHONELINK_BUILD_HOST (env, or in
# ~/.config/phonelink-build.env); the default is the ssh alias "phonelink-build".
# Jobs run detached under systemd on the box, so a dropped ssh connection
# does not kill a build: rerun `log` to pick it back up, then `build` again
# (nothing left to compile) to copy the binaries back.
set -euo pipefail

CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/phonelink-build.env"
# shellcheck source=/dev/null
[[ -f "$CONFIG" ]] && source "$CONFIG"
HOST="${PHONELINK_BUILD_HOST:-phonelink-build}"
REMOTE_DIR="${PHONELINK_BUILD_DIR:-work/phonelink}"   # relative to the box's $HOME
MEMORY_MAX="${PHONELINK_BUILD_MEMORY:-3500M}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() { sed -n '2,16p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 2; }

# sync_up [SRC]: copy SRC (default: this checkout) to the box.
sync_up() {
    local src="${1:-$ROOT}"
    ssh "$HOST" "mkdir -p '$REMOTE_DIR' work/logs"
    # --checksum without --times: only files whose content changed are
    # rewritten, and they get a fresh mtime, so cargo never builds stale.
    rsync -rlpz --checksum --delete \
        --exclude '/target/' --exclude '.git/' --exclude '__pycache__/' \
        --exclude '*.pyc' --exclude '.venv/' \
        "$src/" "$HOST:$REMOTE_DIR/"
}

# Start CMD detached on the box, then stream its log until it ends.
# Exits with the job's exit code.
run_remote() {
    local id log
    id="$(date +%Y%m%dT%H%M%S)-$$"
    log="work/logs/$id.log"
    # The command travels as a file, so its quoting survives untouched.
    printf '%q ' "$@" | ssh "$HOST" "cat > '$log.cmd'"
    ssh "$HOST" bash -s -- "$id" "$REMOTE_DIR" "$MEMORY_MAX" <<'EOF'
set -eu
id=$1 dir=$2 mem=$3
log=$HOME/work/logs/$id.log
: > "$log"
# ExecStopPost records the exit status even when the job is OOM-killed.
# It shares the job's stdout, so the log must be append:, not truncate:.
systemd-run --user --quiet --collect --unit="phonelink-$id" \
    -p MemoryMax="$mem" --nice=10 --working-directory="$HOME/$dir" \
    -p StandardOutput="append:$log" -p StandardError=inherit \
    -p ExecStopPost="/bin/sh -c 'echo \$EXIT_STATUS > $log.exit'" \
    flock -n -E 75 "$HOME/work/phonelink.lock" \
    bash -lc "ln -sfn '$log' ~/work/logs/latest.log; source ~/.cargo/env; source '$log.cmd'"
EOF
    echo "job $id on $HOST (log: ~/$log)" >&2
    follow "$log"
}

follow() {
    ssh "$HOST" bash -s -- "$1" <<'EOF'
log=$HOME/$1
while [ ! -e "$log" ]; do sleep 0.5; done
tail -n +1 -F "$log" 2>/dev/null &
t=$!
while [ ! -s "$log.exit" ]; do sleep 1; done
sleep 0.5; kill $t 2>/dev/null
code=$(cat "$log.exit")
[ "$code" = 75 ] && echo "another job holds the build lock; see the previous log" >&2
case $code in ''|*[!0-9]*) echo "job killed ($code)" >&2; exit 137 ;; esac
exit "$code"
EOF
}

# Cargo updates Cargo.lock on the box when dependencies change; bring it back
# so the committed lock matches what was built (sync_up would overwrite it).
lock_back() {
    rsync -az --checksum "$HOST:$REMOTE_DIR/Cargo.lock" "$ROOT/Cargo.lock" 2>/dev/null || true
}

bring_back() {
    local profile=$1
    mkdir -p "$ROOT/target/$profile"
    ssh "$HOST" "cd '$REMOTE_DIR/target/$profile' 2>/dev/null && find . -maxdepth 1 -type f -executable -printf '%P\n'" \
        | rsync -az --files-from=- "$HOST:$REMOTE_DIR/target/$profile/" "$ROOT/target/$profile/"
    echo "binaries copied to target/$profile/" >&2
}

# Static Go build of the helper, into target/ (which the sync leaves alone).
GM_BUILD='(export PATH=$HOME/opt/go/bin:$PATH && mkdir -p target/gm && cd phonelink-gm && CGO_ENABLED=0 go build -mod=readonly -trimpath -ldflags="-s -w" -o ../target/gm/phonelink-gm .)'

[[ $# -ge 1 ]] || usage
action=$1; shift
case "$action" in
    build)
        profile=debug
        for a in "$@"; do [[ $a == --release ]] && profile=release; done
        sync_up
        code=0; run_remote cargo build "$@" || code=$?
        lock_back
        [[ $code -eq 0 ]] || exit "$code"
        bring_back "$profile"
        ;;
    test|check|clippy)
        sync_up
        code=0; run_remote cargo "$action" "$@" || code=$?
        lock_back
        exit "$code"
        ;;
    run)
        [[ ${1:-} == -- ]] && shift
        [[ $# -ge 1 ]] || usage
        sync_up
        run_remote "$@"
        ;;
    gm)
        sync_up
        code=0; run_remote bash -c "$GM_BUILD" || code=$?
        lock_back
        [[ $code -eq 0 ]] || exit "$code"
        scp -q "$HOST:$REMOTE_DIR/target/gm/phonelink-gm" "$ROOT/phonelink-gm/phonelink-gm"
        echo "helper copied to phonelink-gm/phonelink-gm" >&2
        ;;
    release)
        # Release builds carry PhoneRelay's own Google client (build.rs).
        [[ -f "$ROOT/data/google_oauth_client.json" ]] || { echo "data/google_oauth_client.json is missing: a release needs the built-in Google client" >&2; exit 1; }
        version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/phonelink-rs/Cargo.toml" | head -1)"
        # Build exactly what tag vVERSION holds, so anyone can rebuild it from
        # the tag (the AGPL helper's source must be there): an export of the
        # tag, never this working tree, and the committed Cargo.lock as is.
        tag="v$version"
        git -C "$ROOT" rev-parse -q --verify "refs/tags/$tag^{commit}" >/dev/null \
            || { echo "tag $tag doesn't exist: commit, then tag the release first" >&2; exit 1; }
        [[ "$(git -C "$ROOT" rev-parse HEAD)" == "$(git -C "$ROOT" rev-parse "$tag^{commit}")" ]] \
            || echo "note: HEAD is not $tag; building $tag" >&2
        export_dir="$(mktemp -d)"
        trap 'rm -rf "$export_dir"' EXIT
        git -C "$ROOT" archive "$tag" | tar -x -C "$export_dir"
        install -m 600 "$ROOT/data/google_oauth_client.json" "$export_dir/data/google_oauth_client.json"
        sync_up "$export_dir"
        code=0; run_remote bash -c "cargo build --release --locked && $GM_BUILD && bash scripts/pack-release.sh '$version'" || code=$?
        [[ $code -eq 0 ]] || exit "$code"
        mkdir -p "$ROOT/target/dist"
        rsync -az "$HOST:$REMOTE_DIR/target/dist/phonerelay-$version-x86_64.tar.gz"* "$ROOT/target/dist/"
        echo "release copied to target/dist/" >&2
        ;;
    log)
        latest="$(ssh "$HOST" 'readlink work/logs/latest.log' || true)"
        [[ -n $latest ]] || { echo "no jobs yet" >&2; exit 1; }
        follow "${latest#"$(ssh "$HOST" 'echo $HOME')/"}"
        ;;
    *) usage ;;
esac
