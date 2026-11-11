# PhoneRelay messages helper protocol (version 1)

PhoneRelay's Messages tab has two sources. KDE Connect is built in. A
**messages helper** is a separate program that PhoneRelay starts, and
while one is running it is the tab's only source: every chat, message
and send goes through it. The built-in helper is `phonelink-gm` (Google
Messages through its web link). Any other program that speaks this
protocol can take its place: for example a bridge to another SMS app on
the phone.

PhoneRelay doesn't care where the texts come from. It sees chats,
messages and requests with ids it doesn't interpret. Code that is
specific to one source belongs in that source's helper.

## Using another helper
Open **Settings → Google Messages → Messages Helper → Choose…** and pick
the program. PhoneRelay then starts it in place of `phonelink-gm`, and
Google Messages' own Link/Unlink is set aside. **Use Built-in** goes back
to `phonelink-gm`.

When you switch between helpers, PhoneRelay forgets the texts it saved
from the previous one, because ids from two helpers can clash. The new
helper sends its chats again on connect.

## Running
PhoneRelay runs `HELPER serve`. The built-in helper also gets
`--timeout=90s`. The environment holds `PHONERELAY_PROTOCOL=1`, the
newest protocol version this PhoneRelay speaks.

- **stdin:** requests, one JSON object per line.
- **stdout:** the helper's hello, replies and events, one JSON object per
  line. Write nothing else on stdout.
- **stderr:** the helper's own logging. PhoneRelay passes it through to
  its own stderr. Don't log message text there.
- When stdin closes (PhoneRelay quits or stops the helper), exit with 0.
  If the helper is still running 10 s after a stop, PhoneRelay kills it.
- If the helper exits, PhoneRelay starts it again after 1 s, doubling the
  wait up to 10 minutes. The wait resets once a start reaches `ready`.

## Hello
The first line on stdout, written at once, before connecting to anything:

```json
{"event":"hello","protocol":1,"name":"my-bridge","version":"0.1.0","title":"Fossify Messages",
 "supports":["send_text","send_media","download","fetch","mark_read","typing"]}
```

| Field | Meaning |
|---|---|
| `protocol` | The protocol version the helper speaks. If it is newer than `PHONERELAY_PROTOCOL`, PhoneRelay stops the helper, says it needs a newer PhoneRelay, and doesn't restart it. |
| `name`, `version` | Shown in Settings and in the log. |
| `title` | What the user sees the texts coming through, in banners such as "Connecting to Fossify Messages…". Defaults to `name`. |
| `supports` | The optional requests the helper handles (the `op` names below, plus `reply`). PhoneRelay hides what isn't listed, and answers a request for it itself with `{"ok":false,"error":"unsupported"}` without sending it. |

A helper that writes no hello before `ready` is taken to be an older
`phonelink-gm` that supports everything.

## Ids and keys
- **`conv`** is a chat's id and **`msg`** a message's. Both are strings
  the helper chooses. They must be stable across restarts, since
  PhoneRelay keeps them in its database.
- **`tmp_id`** is the id PhoneRelay gives a message it sends, so that the
  sent message's echo can replace the "Sending" bubble.
- **A chat key** names a chat by its members. Each member other than me
  is reduced to the last 10 digits of their number (or, for an address
  with `@`, the whole address in lower case). The keys are sorted and
  joined with commas, for example `5555550101,5555550102`. A chat with
  only me keeps my own key.
- **Timestamps** are milliseconds since 1970 (UTC).

## Events (helper → PhoneRelay)
Events have `event` and no `id`.

### Required
- `{"event":"ready","me":["5555550100"]}`: connected. `me` are my own
  number keys (last 10 digits). PhoneRelay names its store after the
  first one, so send at least one. Right after `ready`, send a
  `conversation` event for every chat, then any messages that arrived
  while connecting. PhoneRelay holds its requests until `ready`.
- `conversation`: a new or changed chat (all fields are optional except
  `conv`):

  ```json
  {"event":"conversation","conv":"42","chat":"5555550101","name":"",
   "members":[{"number":"+15555550101","name":"Alex"}],
   "group":false,"rcs":false,"read_only":false,"archived":false,"pinned":false,
   "blocked":false,"unread":true,"last_ts_ms":1762800000000,"preview":"See you at 6"}
  ```

  `chat` is its chat key (above). `members` are the other people in the
  chat. `name` is a group's name (or a business's), and otherwise empty.
  `read_only` chats can't be replied to. `blocked` chats leave the list.
- `message`: a new or changed message. Send it again whenever it changes
  (status, reactions, a full-size file arriving).

  ```json
  {"event":"message","conv":"42","msg":"m9","tmp_id":"","ts_ms":1762800000000,
   "from_me":false,"from":"+15555550101","name":"Alex","text":"See you at 6",
   "status":"received","rcs":false,"deleted":false,"old":false,
   "attachments":[{"id":"a1","mime":"image/jpeg","name":"photo.jpg","size":123456,"key":"","thumb":false,"part":""}],
   "reply_to":"","reactions":[{"emoji":"👍","count":1,"mine":false,"by":["+15555550101"]}]}
  ```

  - `status` is `received` for theirs. For mine it is `sending`, `sent`,
    `delivered`, `read` or `failed`.
  - `old` marks messages replayed on connect, which aren't new.
  - A message I sent from PhoneRelay carries the request's `tmp_id`.
  - An attachment's `id` and `key` are passed back unchanged in
    `download`. `key` may be empty.

### Optional
- `{"event":"typing","conv":"42","from":"+15555550101","typing":true}`:
  someone else started (or stopped) typing.
- `{"event":"phone","responding":false}`: the phone stopped answering
  (`true` when it is back).
- `{"event":"session","state":"inactive"}`: something else took the
  connection over. PhoneRelay offers "Use here", which sends `set_active`.
- `{"event":"error","error":"logged_out"|"connect_failed","message":"…"}`:
  fatal; exit 1 right after. `logged_out` stops PhoneRelay from restarting
  the helper and sends the Messages tab back to KDE Connect.
  `connect_failed` is retried as above.

## Requests (PhoneRelay → helper)
Every request has an `id` and an `op`. Answer each one exactly once, on a
line with the same `id`: `{"id":7,"ok":true,...}` or
`{"id":7,"ok":false,"error":"failed","message":"why, for the log"}`. The
answers can come in any order. Answer an `op` you don't know with
`"error":"usage"`.

### Always sent
None. A helper with an empty `supports` list is a read-only view of the
phone's texts.

### Optional (list them in `supports`)
| `op` | Fields | Answer and behaviour |
|---|---|---|
| `send_text` | `conv`, `text`, `tmp_id`, `reply_to`? | `ok` once the phone has taken it; the message then arrives as a `message` event with this `tmp_id`. `reply_to` is a `msg`, sent only if `supports` has `reply`. Error `read_only` for a chat that can't be replied to. |
| `send_media` | `conv`, `path`, `mime`, `name`, `text`, `tmp_id`, `reply_to`? | As `send_text`, with a file. The helper reads the file at `path` itself. `mime` may be empty. |
| `download` | `media`, `key`, `path` | Write the attachment (attachment `id` → `media`) to `path`, mode 0600, and answer once it is complete. Write it to a temporary name and rename it, so it never shows half written. |
| `fetch` | `conv`, `count`, `before_id`?, `before_ts_ms`? | `"messages":[…]` (message objects without `event`, newest first) and `"cursor":{"id","ts_ms"}` when older ones remain. PhoneRelay sends this when a chat opens, and the cursor back for older pages. |
| `mark_read` | `conv` (+ `msg`?), or `chat` (a chat key) | Mark the chat read on the phone, up to `msg` or the newest message. |
| `delete` | `chats` (chat keys) | Delete those chats on the phone. Answer with `"results":[…]`, one per chat in the order asked: `{"chat":KEY,"ok":true}`, or `"ok":false` with `"error"`: `not_found` (no such chat), `ambiguous` (more than one) or anything else for a failure, and a `"message"`. |
| `react` | `conv`, `msg`, `emoji`, `action` (`add`, `remove`, `switch`) | |
| `typing` | `conv` | "I'm typing", sent every few seconds while I type. |
| `update_chat` | `conv`, `change` (`archive`, `unarchive`, `mute`, `unmute`, `block`, `spam`, `unblock`) | Then send the chat's `conversation` event. |
| `open_conversation` | `numbers` | `"conv":ID` of the chat with exactly these people, made if new. Send its `conversation` event before the answer. Without this, a new chat can't be started from PhoneRelay. |
| `set_active` | | Take the connection back after `session` `inactive`. |
| `list` | | Send a `conversation` event for every chat again. |
| `reply` | (not an op) | `send_text` and `send_media` take `reply_to`. |

## Versions
Version 1 is what `phonelink-gm` speaks as of PhoneRelay 0.3.6, the
first release with the hello. A change that an older helper or an older
PhoneRelay could misread raises the version. New optional fields and
new `supports` entries don't, since both sides ignore what they don't
know.

## Testing a helper
`phonelink-rs/examples/fake_gm.rs` is a scripted helper used in
PhoneRelay's own tests, and a short example of the event side.
`phonelink-gm/serve.go` is the full built-in one.
