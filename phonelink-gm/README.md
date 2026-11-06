# phonelink-gm

Deletes chats on the phone for Phone Link. KDE Connect can only read texts,
and Android lets only the default SMS app (Google Messages) delete them, so
this helper deletes through the Google Messages web link using libgm
(mautrix-gmessages) and the session paired by
[gmcli](https://github.com/fdsouvenir/gmcli) (`~/.local/state/gmcli/session.json`).

    phonelink-gm status
    phonelink-gm delete [--dry-run] "+13165551234" "+13165551234,+13165555678"

Each argument is one chat's other members. A chat that matches no Google
Messages conversation, or more than one (an SMS and an RCS group with the
same people), is left alone.

Build: `CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o phonelink-gm .`
(on the build box; `data/install-rust.sh` installs it next to the app).

License: AGPL-3.0-or-later, because it links libgm. The rest of Phone Link
stays MIT; the app only runs this as a separate program.
