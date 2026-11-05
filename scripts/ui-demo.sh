#!/usr/bin/env bash
# Run the app on the build box against the fake daemon, shown through GTK's
# Broadway backend on 127.0.0.1:8085 (ssh -L 8085:127.0.0.1:8085 the box to
# look at it). Uses a throwaway HOME, so no real store or settings.
#   scripts/remote-build.sh run -- bash scripts/ui-demo.sh
set -u
cargo build -q --example mock_daemon && cargo build -q || exit 1
demo=$HOME/demo-home; rm -rf "$demo"; mkdir -p "$demo"
pkill -x gtk4-broadwayd; pkill -x mock_daemon; sleep 0.5
gtk4-broadwayd --address 127.0.0.1 :5 & 
HOME=$demo target/debug/examples/mock_daemon & sleep 1
HOME=$demo GDK_BACKEND=broadway BROADWAY_DISPLAY=:5 GDK_SCALE=${DEMO_SCALE:-2} RUST_LOG=info timeout ${DEMO_SECONDS:-90} target/debug/phonelink-rs
echo "app exit=$?"
pkill -x mock_daemon; pkill -x gtk4-broadwayd
