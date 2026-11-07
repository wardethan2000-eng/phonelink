//! A fake `phonelink-gm serve` for demos and UI checks on the build box:
//! speaks the helper's JSON-lines protocol (see phonelink-gm/serve.go) with
//! made-up chats, so the Messages tab can run on "Google Messages" without
//! a phone. Point the app at it with `PHONELINK_GM=.../fake_gm` (and any
//! file at gmcli's session path).
//!
//! Sends echo back as `sending`, then `delivered`, then `read`. A text
//! containing "fail" fails. Weekend crew's last message has a picture;
//! `download` writes a made-up one.

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
        texts: &[
            (240, Some(0), "Are we still on for Saturday?"),
            (230, None, "Yes! 10am at the trailhead"),
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
        texts: &[(2000, Some(0), "Your table is ready"), (1990, None, "Thanks, on our way")],
    },
    Chat {
        conv: "4",
        members: &[("parcels_agent@rbm.goog", "")],
        name: "Parcel Tracking",
        rcs: true,
        unread: false,
        read_only: true,
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

fn message(c: &Chat, i: usize, ts: i64, from: Option<usize>, text: &str, status: &str) -> Value {
    let pic = c.conv == "2" && i + 1 == c.texts.len();
    json!({
        "attachments": if pic { json!([{"id": "pic-1", "mime": "image/png", "name": "album.png", "size": 1, "key": "a2V5"}]) } else { json!([]) },
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
        .map(|(i, (ago, from, text))| message(c, i, now - ago * 60_000, *from, text, if from.is_none() { "read" } else { "received" }))
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
    let mut sent = 0;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let id = req["id"].as_i64().unwrap_or(0);
        let conv = req["conv"].as_str().unwrap_or_default().to_string();
        match req["op"].as_str().unwrap_or_default() {
            "fetch" => {
                let msgs = CHATS.iter().find(|c| c.conv == conv).map(|c| messages(c, start)).unwrap_or_default();
                put(&out, &json!({"id": id, "ok": true, "messages": msgs}));
            }
            "download" => {
                let path = req["path"].as_str().unwrap_or_default();
                let ok = std::fs::write(path, png(480, 320)).is_ok();
                std::thread::sleep(Duration::from_millis(700));
                put(&out, &json!({"id": id, "ok": ok}));
            }
            "send_text" | "send_media" => {
                let Some(c) = CHATS.iter().find(|c| c.conv == conv) else {
                    put(&out, &json!({"id": id, "ok": false, "error": "not_found"}));
                    continue;
                };
                let text = req["text"].as_str().unwrap_or_default().to_string();
                let atts = match req["path"].as_str() {
                    Some(_) => json!([{"id": format!("up-{}", sent + 1), "mime": req["mime"], "key": "a2V5"}]),
                    None => json!([]),
                };
                let tmp = req["tmp_id"].as_str().unwrap_or_default().to_string();
                put(&out, &json!({"id": id, "ok": true, "tmp_id": tmp}));
                sent += 1;
                let (out, rcs, conv, n) = (out.clone(), c.rcs, c.conv, sent);
                std::thread::spawn(move || {
                    let ts = now_ms();
                    let steps: &[&str] = if text.contains("fail") { &["sending", "failed"] } else { &["sending", "delivered", "read"] };
                    for status in steps {
                        put(&out, &json!({"event": "message", "conv": conv, "msg": format!("sent-{n}"), "tmp_id": tmp,
                            "ts_ms": ts, "from_me": true, "text": text, "status": status, "rcs": rcs, "attachments": atts.clone()}));
                        std::thread::sleep(Duration::from_millis(1500));
                    }
                });
            }
            "mark_read" | "set_active" | "list" => put(&out, &json!({"id": id, "ok": true})),
            "delete" => put(&out, &json!({"id": id, "ok": true, "results": [{"chat": "x", "ok": true}]})),
            op => put(&out, &json!({"id": id, "ok": false, "error": "usage", "message": format!("unknown op {op}")})),
        }
    }
}
