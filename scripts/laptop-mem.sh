#!/usr/bin/env bash
# Memory comparison on the laptop: the Python app, then the Rust app with
# each GTK renderer, one at a time, each idle for $WAIT seconds after start.
# Read-only (nothing is sent). Prints numbers only: no message text.
# Quit the Python app first. Usage:
#   bash scripts/laptop-mem.sh            (or copy it to the laptop)
set -u
WAIT=${WAIT:-90}
RUST=${RUST:-$HOME/phonelink-rs/phonelink-rs}
PY=${PY:-$HOME/phonelink/run.py}
if pgrep -f 'phonelink/run.py' >/dev/null || pgrep -x phonelink-rs >/dev/null; then
    echo "Quit Phone Link (Python and Rust) first." >&2; exit 1
fi

measure() {  # name pid
    local name=$1 pid=$2
    echo "== $name: $(awk '/VmRSS/{print $2}' /proc/$pid/status) KB RSS, $(ls /proc/$pid/task | wc -l) threads"
    grep -E '^(Pss|Anonymous|Private_Dirty|Shared_Clean|Private_Clean)' /proc/$pid/smaps_rollup | tr -s ' ' | paste -sd' '
    awk '/^[0-9a-f]+-[0-9a-f]+ /{n=($6==""?"[anon]":$6)} /^Rss:/{r[n]+=$2}
         END{for(k in r) if(r[k]>4000) printf "   %7d KB %s\n", r[k], k}' /proc/$pid/smaps | sort -rn | head -14
}
run() {  # name command...
    local name=$1; shift
    "$@" >/dev/null 2>&1 &
    local pid=$!
    sleep "$WAIT"
    if kill -0 "$pid" 2>/dev/null; then measure "$name" "$pid"; else echo "== $name: exited early"; fi
    kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; sleep 3
}

echo "GTK $(pkg-config --modversion gtk4 2>/dev/null), renderer default: ${GSK_RENDERER:-unset}"
run python python3 "$PY"
run rust-default "$RUST"
run rust-ngl env GSK_RENDERER=ngl "$RUST"
run rust-vulkan env GSK_RENDERER=vulkan "$RUST"
run rust-cairo env GSK_RENDERER=cairo "$RUST"
echo "Done. Start your usual app again (python3 ~/phonelink/run.py, or log out and in)."
