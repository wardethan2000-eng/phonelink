#!/usr/bin/env bash
# Compare the Rust and Python apps' memory on the build box, against the
# demo daemon with Ethan-sized data, under GTK's Broadway backend (no GPU, so
# this measures the apps, not the graphics driver). Run on the box:
#   scripts/remote-build.sh run -- bash scripts/mem-compare.sh
# Optional: HEAPTRACK=1 also records the Rust run with heaptrack.
set -u
cargo build -q --release --example mock_daemon && cargo build -q --release || exit 1
H=$HOME/memtest-home
export DEMO_EXTRA=${DEMO_EXTRA:-330} DEMO_FILLER_MSGS=${DEMO_FILLER_MSGS:-10} DEMO_LONG=${DEMO_LONG:-2226}
rm -rf "$H"; mkdir -p "$H"
pkill -x gtk4-broadwayd; pkill -x mock_daemon; sleep 0.5
gtk4-broadwayd --address 127.0.0.1 :7 >/dev/null 2>&1 &
sleep 1
HOME=$H target/release/examples/mock_daemon & sleep 1

measure() {  # name pid
    local name=$1 pid=$2
    echo "== $name: $(awk '/VmRSS/{print $2}' /proc/$pid/status) KB RSS, $(ls /proc/$pid/task | wc -l) threads"
    grep -E '^(Pss|Anonymous|Private_Dirty|Shared_Clean|Private_Clean)' /proc/$pid/smaps_rollup | tr -s ' ' | paste -sd' '
    awk '/^[0-9a-f]+-[0-9a-f]+ /{n=($6==""?"[anon]":$6)} /^Rss:/{r[n]+=$2}
         END{for(k in r) if(r[k]>3000) printf "   %7d KB %s\n", r[k], k}' /proc/$pid/smaps | sort -rn | head -12
}
run_app() {  # name seconds command...
    local name=$1 secs=$2; shift 2
    HOME=$H GDK_BACKEND=broadway BROADWAY_DISPLAY=:7 "$@" >"$HOME/memtest-$name.log" 2>&1 &
    local pid=$!
    sleep "$secs"
    measure "$name" "$pid"
    kill "$pid"; wait "$pid" 2>/dev/null
    sleep 1
}

# 1. Fill the store the way a first connect does (catch-up pulls everything).
run_app rust-first-connect 60 target/release/phonelink-rs
echo "store: $(sqlite3 "$H/.local/share/phonelink/messages.db" 'select count(*) from messages; select count(*) from conversations' 2>/dev/null | paste -sd/) messages/conversations"
# 2. Idle with the full store loaded, as on the laptop.
run_app rust-idle 45 target/release/phonelink-rs
run_app python-idle 45 python3 run.py
if [ "${HEAPTRACK:-0}" = 1 ]; then
    HOME=$H GDK_BACKEND=broadway BROADWAY_DISPLAY=:7 timeout -s INT 45 heaptrack -o "$HOME/memtest-heaptrack" target/release/phonelink-rs >/dev/null 2>&1
    f=$(ls -t "$HOME"/memtest-heaptrack*.zst | head -1)
    heaptrack_print -f "$f" --print-peaks 1 --print-allocators 0 --print-temporary 0 --print-leaks 0 -n 25 2>/dev/null | sed -n '/PEAK MEMORY CONSUMERS/,$p' | head -150
fi
pkill -x mock_daemon; pkill -x gtk4-broadwayd
