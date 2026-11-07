#!/usr/bin/env bash
# Run the app on the build box against the fake daemon, shown through GTK's
# Broadway backend on 127.0.0.1:8085 (ssh -L 8085:127.0.0.1:8085 the box to
# look at it). Uses a throwaway HOME, so no real store or settings.
# DEMO_GM=1 adds the fake Google Messages helper (examples/fake_gm.rs), so
# the Messages tab runs on Google Messages.
#   scripts/remote-build.sh run -- bash scripts/ui-demo.sh
set -u
cargo build -q --example mock_daemon --example fake_gm && cargo build -q || exit 1
demo=$HOME/demo-home; rm -rf "$demo"; mkdir -p "$demo"
if [ "${DEMO_GM:-0}" = 1 ]; then
    mkdir -p "$demo/.local/state/gmcli" && echo '{}' > "$demo/.local/state/gmcli/session.json"
    export PHONELINK_GM=$PWD/target/debug/examples/fake_gm
fi
pkill -x gtk4-broadwayd; pkill -x mock_daemon; sleep 0.5
gtk4-broadwayd --address 127.0.0.1 :5 & 
HOME=$demo target/debug/examples/mock_daemon & sleep 1
HOME=$demo GDK_BACKEND=broadway BROADWAY_DISPLAY=:5 GDK_SCALE=${DEMO_SCALE:-2} RUST_LOG=info timeout ${DEMO_SECONDS:-90} target/debug/phonelink-rs
echo "app exit=$?"
pkill -x mock_daemon; pkill -x gtk4-broadwayd
