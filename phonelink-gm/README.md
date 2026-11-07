# phonelink-gm

Deletes chats and syncs read state with the phone for PhoneRelay. KDE
Connect can only read texts (it can't mark them read, and never says when
the phone did), and Android lets only the default SMS app (Google Messages)
delete them, so this helper works through the Google Messages web link using libgm
(mautrix-gmessages) and the session paired by
[gmcli](https://github.com/fdsouvenir/gmcli) (`~/.local/state/gmcli/session.json`).

    phonelink-gm status
    phonelink-gm serve
    phonelink-gm delete [--dry-run] "+13165551234" "+13165551234,+13165555678"
    phonelink-gm members        (debugging: every chat's members as Google sees them)

`serve` keeps one connection open for the app: JSON lines on stdin/stdout
(`mark_read` and `delete` requests; `ready` and per-chat `conversation`
events with the phone's unread state). The protocol is at the top of
`serve.go`. The app starts it when Google Messages is paired and restarts it
if it exits. While it runs it holds the session lock, so a one-shot
`delete` by hand answers `busy`.

Each chat argument is one chat's other members, or a business chat's
address (`name_agent@rbm.goog`). A chat that matches no Google
Messages conversation, or more than one (an SMS and an RCS group with the
same people), is left alone.

Build: `CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o phonelink-gm .`
(on the build box; `data/install-rust.sh` installs it next to the app).

License: AGPL-3.0-or-later, because it links libgm. The rest of PhoneRelay
stays MIT; the app only runs this as a separate program.
