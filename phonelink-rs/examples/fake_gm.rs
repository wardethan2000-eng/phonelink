//! A fake `phonelink-gm serve` for demos and UI checks on the build box:
//! speaks the helper's JSON-lines protocol (see phonelink-gm/serve.go) with
//! made-up chats, so the Messages tab can run on "Google Messages" without
//! a phone. Point the app at it with `PHONELINK_GM=.../fake_gm` (and any
//! file at gmcli's session path).
//!
//! Sends echo back as `sending`, then `delivered`, then `read`; in RCS
//! chats they then type a moment, answer, and like it. A text containing
//! "fail" fails. Weekend crew's last message has a picture (a preview at
//! first, the full one a few seconds later); Avery Lane is pinned, has a
//! voice message and pages of older history; one chat is archived.
//! `update_chat` archives, blocks... as the phone would.

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

type Out = Arc<Mutex<std::io::Stdout>>;

fn put(out: &Out, v: &Value) {
    let mut o = out.lock().unwrap();
    let _ = writeln!(o, "{v}");
    let _ = o.flush();
}

struct Chat {
    conv: &'static str,
    members: &'static [(&'static str, &'static str)],
    name: &'static str,
    rcs: bool,
    unread: bool,
    read_only: bool,
    archived: bool,
    pinned: bool,
    /// (minutes ago, from member index or None for me, text)
    texts: &'static [(i64, Option<usize>, &'static str)],
}

const CHATS: &[Chat] = &[
    Chat {
        conv: "1",
        members: &[("+15555550101", "Avery Lane")],
        name: "",
        rcs: true,
        unread: true,
        read_only: false,
        archived: false,
        pinned: true,
        texts: &[
            (240, Some(0), "Are we still on for Saturday?"),
            (230, None, "Yes! 10am at the trailhead"),
            (14, Some(0), ""),
            (12, Some(0), "Running 5 min late, save me a spot"),
        ],
    },
    Chat {
        conv: "2",
        members: &[("+15555550102", "Jordan Pike"), ("+15555550103", "Sam Ortiz"), ("+15555550104", "")],
        name: "Weekend crew",
        rcs: true,
        unread: false,
        read_only: false,
        archived: false,
        pinned: false,
        texts: &[
            (600, Some(1), "Who's bringing the cooler?"),
            (590, None, "I can"),
            (580, Some(0), "👍"),
            (95, Some(2), "Photos from last time are in the album"),
        ],
    },
    Chat {
        conv: "3",
        members: &[("+15555550105", "")],
        name: "",
        rcs: false,
        unread: false,
        read_only: false,
        archived: false,
        pinned: false,
        texts: &[(2000, Some(0), "Your table is ready"), (1990, None, "Thanks, on our way")],
    },
    Chat {
        conv: "5",
        members: &[("+15555550106", "Riley Moss")],
        name: "",
        rcs: true,
        unread: false,
        read_only: false,
        archived: true,
        pinned: false,
        texts: &[(9000, Some(0), "Thanks for the couch!"), (8990, None, "Enjoy it")],
    },
    Chat {
        conv: "4",
        members: &[("parcels_agent@rbm.goog", "")],
        name: "Parcel Tracking",
        rcs: true,
        unread: false,
        read_only: true,
        archived: false,
        pinned: false,
        texts: &[(3000, Some(0), "Your package was delivered.")],
    },
];

/// A width×height PNG of a soft gradient (stored, not compressed).
fn png(width: u32, height: u32) -> Vec<u8> {
    fn crc(data: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in data {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
        out.extend((data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend(data);
        out.extend(&body);
        out.extend(crc(&body).to_be_bytes());
    }
    let mut raw = Vec::new();
    for y in 0..height {
        raw.push(0);
        for x in 0..width {
            raw.extend([(60 + 150 * x / width) as u8, (110 + 100 * y / height) as u8, 200]);
        }
    }
    // zlib: stored blocks of at most 65535 bytes, then adler32.
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push(u8::from(i + 1 == blocks.len()));
        z.extend((b.len() as u16).to_le_bytes());
        z.extend((!(b.len() as u16)).to_le_bytes());
        z.extend(*b);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    z.extend(((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend(width.to_be_bytes());
    ihdr.extend(height.to_be_bytes());
    ihdr.extend([8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// A short WAV: a soft two-note chime (a stand-in voice message).
fn wav() -> Vec<u8> {
    let rate = 8000u32;
    let samples: Vec<i16> = (0..rate * 2)
        .map(|i| {
            let t = f64::from(i) / f64::from(rate);
            let f = if t < 1.0 { 440.0 } else { 554.0 };
            (f64::sin(2.0 * std::f64::consts::PI * f * t) * 6000.0 * (1.0 - (t % 1.0))) as i16
        })
        .collect();
    let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let mut out = b"RIFF".to_vec();
    out.extend((36 + data.len() as u32).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes()); // PCM
    out.extend(1u16.to_le_bytes()); // mono
    out.extend(rate.to_le_bytes());
    out.extend((rate * 2).to_le_bytes());
    out.extend(2u16.to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(data);
    out
}

/// Pages of older history for Avery Lane: (texts, more after this).
const OLDER_PAGES: usize = 3;

fn older_page(c: &Chat, page: usize, now: i64) -> Vec<Value> {
    // Newest first, each page older than the last, one a day apart.
    (0..50)
        .map(|k| {
            let n = page * 50 + k;
            let ts = now - 300 * 60_000 - (n as i64 + 1) * 86_400_000 / 4;
            let mine = n.is_multiple_of(3);
            json!({"conv": c.conv, "msg": format!("old-{n}"), "ts_ms": ts, "from_me": mine,
                "from": if mine { "" } else { c.members[0].0 }, "text": format!("Older message {}", n + 1),
                "status": if mine { "read" } else { "received" }, "rcs": true})
        })
        .collect()
}

fn message(c: &Chat, i: usize, ts: i64, from: Option<usize>, text: &str, status: &str) -> Value {
    let pic = c.conv == "2" && i + 1 == c.texts.len();
    let voice = c.conv == "1" && text.is_empty();
    // The group chat shows a reply and reactions.
    let group = c.conv == "2";
    let reply_to = if group && i == 1 { "2-0" } else { "" };
    let reactions = if group && i == 0 {
        json!([{"emoji": "👍", "count": 2, "mine": true, "by": [c.members[1].0]}, {"emoji": "😂", "count": 1, "by": [c.members[0].0]}])
    } else {
        json!([])
    };
    json!({
        "reply_to": reply_to,
        "reactions": reactions,
        // The picture is a preview until the phone sends the full one.
        "attachments": if pic {
            json!([{"id": "pic-thumb", "mime": "image/png", "name": "album.png", "size": 1, "key": "a2V5", "thumb": true, "part": "p1"}])
        } else if voice {
            json!([{"id": "voice-1", "mime": "audio/wav", "size": 32044, "key": "a2V5", "part": "p1"}])
        } else {
            json!([])
        },
        "conv": c.conv,
        "msg": format!("{}-{i}", c.conv),
        "ts_ms": ts,
        "from_me": from.is_none(),
        "from": from.map(|f| c.members[f].0).unwrap_or(""),
        "text": text,
        "status": status,
        "rcs": c.rcs,
    })
}

fn messages(c: &Chat, now: i64) -> Vec<Value> {
    let mut v: Vec<Value> = c
        .texts
        .iter()
        .enumerate()
        .map(|(i, (ago, from, text))| {
            message(c, i, now - ago * 60_000, *from, text, if from.is_none() { "read" } else { "received" })
        })
        .collect();
    v.reverse(); // newest first, as the helper's fetch
    v
}

fn conversation(c: &Chat, now: i64) -> Value {
    let last = c.texts.last().unwrap();
    json!({
        "event": "conversation",
        "chat": c.members.iter().map(|m| m.0.trim_start_matches("+1")).collect::<Vec<_>>().join(","),
        "conv": c.conv,
        "name": c.name,
        "members": c.members.iter().map(|(n, name)| json!({"number": n, "name": name})).collect::<Vec<_>>(),
        "group": c.members.len() > 1,
        "rcs": c.rcs,
        "read_only": c.read_only,
        "archived": c.archived,
        "pinned": c.pinned,
        "unread": c.unread,
        "last_ts_ms": now - last.0 * 60_000,
        "preview": last.2,
    })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some("serve") {
        println!(r#"{{"ok":true,"paired":true}}"#);
        return;
    }
    let out: Out = Arc::new(Mutex::new(std::io::stdout()));
    let start = now_ms();
    std::thread::sleep(Duration::from_millis(800));
    put(&out, &json!({"event": "ready", "me": ["5555550100"]}));
    for c in CHATS {
        put(&out, &conversation(c, start));
    }
    // A little later, someone in the group starts typing, then the
    // group's picture arrives full size.
    {
        let out = out.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(6));
            put(&out, &json!({"event": "typing", "conv": "2", "from": "+15555550103", "typing": true}));
            std::thread::sleep(Duration::from_secs(4));
            let c = &CHATS[1];
            let i = c.texts.len() - 1;
            let mut full = message(c, i, start - c.texts[i].0 * 60_000, c.texts[i].1, c.texts[i].2, "received");
            full["event"] = json!("message");
            full["attachments"] = json!([{"id": "pic-1", "mime": "image/png", "name": "album.png", "size": 1, "key": "a2V5", "part": "p1"}]);
            put(&out, &full);
        });
    }
    // Chats made with open_conversation: conv -> (members, rcs).
    let mut made: std::collections::HashMap<String, Vec<String>> = Default::default();
    let mut sent = 0;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = req["id"].as_i64().unwrap_or(0);
        let conv = req["conv"].as_str().unwrap_or_default().to_string();
        match req["op"].as_str().unwrap_or_default() {
            "fetch" => {
                let Some(c) = CHATS.iter().find(|c| c.conv == conv) else {
                    put(&out, &json!({"id": id, "ok": true, "messages": []}));
                    continue;
                };
                // Avery Lane has older pages: the cursor is the page number.
                let page = req["before_id"]
                    .as_str()
                    .and_then(|b| b.strip_prefix("page-"))
                    .and_then(|p| p.parse::<usize>().ok());
                let (msgs, next) = match page {
                    None => (messages(c, start), (c.conv == "1").then_some(0)),
                    Some(p) => {
                        std::thread::sleep(Duration::from_millis(600));
                        (older_page(c, p, start), (p + 1 < OLDER_PAGES).then_some(p + 1))
                    }
                };
                let cursor = next.map(|n| json!({"id": format!("page-{n}"), "ts_ms": 1}));
                put(&out, &json!({"id": id, "ok": true, "messages": msgs, "cursor": cursor}));
            }
            "download" => {
                let path = req["path"].as_str().unwrap_or_default();
                let media = req["media"].as_str().unwrap_or_default();
                let body = if media.starts_with("voice") {
                    wav()
                } else if media.contains("thumb") {
                    png(96, 64)
                } else {
                    png(480, 320)
                };
                let ok = std::fs::write(path, body).is_ok();
                std::thread::sleep(Duration::from_millis(700));
                put(&out, &json!({"id": id, "ok": ok}));
            }
            "open_conversation" => {
                let numbers: Vec<String> = req["numbers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
                let conv = format!("new-{}", made.len() + 1);
                put(
                    &out,
                    &json!({"event": "conversation", "chat": numbers.join(","), "conv": conv, "name": "",
                    "members": numbers.iter().map(|n| json!({"number": n})).collect::<Vec<_>>(), "group": numbers.len() > 1,
                    "rcs": true, "unread": false, "last_ts_ms": now_ms(), "preview": ""}),
                );
                made.insert(conv.clone(), numbers);
                put(&out, &json!({"id": id, "ok": true, "conv": conv}));
            }
            "update_chat" => {
                let change = req["change"].as_str().unwrap_or_default();
                put(&out, &json!({"id": id, "ok": true}));
                if let Some(c) = CHATS.iter().find(|c| c.conv == conv) {
                    let mut ev = conversation(c, start);
                    match change {
                        "archive" => ev["archived"] = json!(true),
                        "unarchive" => ev["archived"] = json!(false),
                        "block" | "spam" => ev["blocked"] = json!(true),
                        _ => continue,
                    }
                    put(&out, &ev);
                }
            }
            "send_text" | "send_media" => {
                let chat = CHATS.iter().find(|c| c.conv == conv);
                let Some((rcs, them)) = chat
                    .map(|c| (c.rcs, c.members[0].0.to_string()))
                    .or_else(|| made.get(&conv).map(|m| (true, m[0].clone())))
                else {
                    put(&out, &json!({"id": id, "ok": false, "error": "not_found"}));
                    continue;
                };
                let text = req["text"].as_str().unwrap_or_default().to_string();
                let atts = match req["path"].as_str() {
                    Some(_) => {
                        json!([{"id": format!("up-{}", sent + 1), "mime": req["mime"], "key": "a2V5"}])
                    }
                    None => json!([]),
                };
                let tmp = req["tmp_id"].as_str().unwrap_or_default().to_string();
                let reply_to = req["reply_to"].as_str().unwrap_or_default().to_string();
                put(&out, &json!({"id": id, "ok": true, "tmp_id": tmp}));
                sent += 1;
                let (out, conv, n) = (out.clone(), conv.clone(), sent);
                std::thread::spawn(move || {
                    let ts = now_ms();
                    let steps: &[&str] =
                        if text.contains("fail") { &["sending", "failed"] } else { &["sending", "delivered", "read"] };
                    for status in steps {
                        put(
                            &out,
                            &json!({"event": "message", "conv": conv, "msg": format!("sent-{n}"), "tmp_id": tmp,
                            "ts_ms": ts, "from_me": true, "text": text, "status": status, "rcs": rcs, "attachments": atts.clone(),
                            "reply_to": reply_to}),
                        );
                        std::thread::sleep(Duration::from_millis(1500));
                    }
                    // They type a moment, answer, and like it.
                    if rcs && !text.contains("fail") {
                        put(&out, &json!({"event": "typing", "conv": conv, "from": them, "typing": true}));
                        std::thread::sleep(Duration::from_secs(4));
                        put(
                            &out,
                            &json!({"event": "message", "conv": conv, "msg": format!("answer-{n}"), "ts_ms": now_ms(),
                            "from_me": false, "from": them, "text": "Sounds good!", "status": "received", "rcs": true}),
                        );
                        put(
                            &out,
                            &json!({"event": "message", "conv": conv, "msg": format!("sent-{n}"), "ts_ms": ts, "from_me": true,
                            "text": text, "status": "read", "rcs": rcs, "attachments": atts, "reply_to": reply_to,
                            "reactions": [{"emoji": "😍", "count": 1, "by": [them]}]}),
                        );
                    }
                });
            }
            "react" if req["msg"].as_str().is_some_and(|m| m.contains("fail")) => put(
                &out,
                &json!({"id": id, "ok": false, "error": "refused", "message": "the phone refused the reaction"}),
            ),
            "mark_read" | "set_active" | "list" | "react" | "typing" => put(&out, &json!({"id": id, "ok": true})),
            "delete" => put(&out, &json!({"id": id, "ok": true, "results": [{"chat": "x", "ok": true}]})),
            op => put(&out, &json!({"id": id, "ok": false, "error": "usage", "message": format!("unknown op {op}")})),
        }
    }
}
