#!/usr/bin/env bash
# Pack a release archive from a finished build (run by `remote-build.sh release`
# on the build box, from the checkout root):
#   scripts/pack-release.sh VERSION
# Needs target/release/phonelink-rs and target/gm/phonelink-gm. Writes
# target/dist/phonerelay-VERSION-x86_64.tar.gz and its .sha256.
set -euo pipefail

version="${1:?usage: $0 VERSION}"
name="phonerelay-$version-x86_64"
dist="target/dist"
stage="$dist/$name"

for f in target/release/phonelink-rs target/gm/phonelink-gm; do
    [[ -x $f ]] || { echo "missing $f" >&2; exit 1; }
done

rm -rf "$stage" "$dist/$name.tar.gz" "$dist/$name.tar.gz.sha256"
mkdir -p "$stage/data/icons" "$stage/phonelink-gm"

install -m 755 target/release/phonelink-rs "$stage/phonelink-rs"
install -m 755 data/install-rust.sh "$stage/data/install-rust.sh"
cp data/icons/phonelink-*.png data/icons/phonelink.svg "$stage/data/icons/"
install -m 755 target/gm/phonelink-gm "$stage/phonelink-gm/phonelink-gm"
cp phonelink-gm/LICENSE phonelink-gm/README.md "$stage/phonelink-gm/"
cp LICENSE README.md "$stage/"

cat > "$stage/INSTALL.txt" <<TXT
PhoneRelay $version

Install (no root needed):
    ./data/install-rust.sh ./phonelink-rs
    phonelink-rs

This copies phonelink-rs and phonelink-gm to ~/.local/bin and adds a
"PhoneRelay" menu entry. Turn on Settings -> Open on Login to start it at
login. To update, quit PhoneRelay (tray icon -> Quit) and run the install
line again from the new release.

Needs GTK 4.10+, libadwaita 1.4+, glibc 2.39+ and KDE Connect (on the PC
and the phone). Phone setup: README.md, "Install", steps 2 to 5.

phonelink-rs is MIT (LICENSE). phonelink-gm, the optional Google Messages
helper, is AGPL-3.0-or-later (phonelink-gm/LICENSE); its source is in the
phonelink-gm/ folder of https://github.com/wardethan2000-eng/phonelink
at tag v$version.
TXT

# The built-in Google client must never ship as a file.
if find "$stage" -name 'google_oauth_client.json' | grep -q .; then
    echo "refusing to pack google_oauth_client.json" >&2
    exit 1
fi

tar -C "$dist" --owner=0 --group=0 -czf "$dist/$name.tar.gz" "$name"
(cd "$dist" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
ls -l "$dist/$name.tar.gz"
cat "$dist/$name.tar.gz.sha256"
