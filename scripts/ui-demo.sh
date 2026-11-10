#!/usr/bin/env bash
# Run the app on the build box against the fake daemon, shown through GTK's
# Broadway backend on 127.0.0.1:8085 (ssh -L 8085:127.0.0.1:8085 the box to
# look at it). Uses a throwaway HOME, so no real store or settings.
# DEMO_GM=1 adds the fake Google Messages helper (examples/fake_gm.rs), so
# the Messages tab runs on Google Messages; DEMO_TYPING=1 with it keeps
# someone typing in the first chat and the group.
#   scripts/remote-build.sh run -- bash scripts/ui-demo.sh
set -u
cargo build -q --example mock_daemon --example fake_gm && cargo build -q || exit 1
demo=$HOME/demo-home; rm -rf "$demo"; mkdir -p "$demo"
# DEMO_SETTINGS='{...}' starts with those settings; DEMO_FONT='Noto Sans 10'
# with that desktop font.
if [ -n "${DEMO_SETTINGS:-}" ]; then
    mkdir -p "$demo/.local/share/phonelink" && echo "$DEMO_SETTINGS" > "$demo/.local/share/phonelink/settings.json"
fi
if [ -n "${DEMO_FONT:-}" ]; then
    mkdir -p "$demo/.config/gtk-4.0" && printf '[Settings]\ngtk-font-name=%s\n' "$DEMO_FONT" > "$demo/.config/gtk-4.0/settings.ini"
fi
# DEMO_ICONS=breeze-dark uses that icon theme from ~/demo-icons.
if [ -n "${DEMO_ICONS:-}" ]; then
    mkdir -p "$demo/.local/share" "$demo/.config/gtk-4.0" && ln -sfn "$HOME/demo-icons" "$demo/.local/share/icons"
    printf 'gtk-icon-theme-name=%s\n' "$DEMO_ICONS" >> "$demo/.config/gtk-4.0/settings.ini"
fi
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
