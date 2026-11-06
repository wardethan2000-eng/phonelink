#!/usr/bin/env bash
# The app on Broadway with no daemon at all (checks the setup page at startup).
set -u
cargo build -q || exit 1
demo=$HOME/demo-home; rm -rf "$demo"; mkdir -p "$demo"
pkill -x gtk4-broadwayd; pkill -x mock_daemon; sleep 0.5
gtk4-broadwayd --address 127.0.0.1 :5 &
sleep 1
HOME=$demo GDK_BACKEND=broadway BROADWAY_DISPLAY=:5 GDK_SCALE=${DEMO_SCALE:-2} RUST_LOG=info timeout ${DEMO_SECONDS:-40} target/debug/phonelink-rs
echo "app exit=$?"
pkill -x gtk4-broadwayd
