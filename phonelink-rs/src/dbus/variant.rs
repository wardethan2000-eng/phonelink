//! GVariant → core types (port of `_parse_message_tuple` and
//! `_parse_attachment_tuple` in `phonelink/ui/sms_panel.py`).
//!
//! The Python parsed whatever `Variant.unpack()` produced, so it never cared
//! whether a field arrived as `x` or `i`, or whether an address list was
//! `a(s)`, `av` or `as`. These helpers keep that tolerance: integers of any
//! width (and booleans) read as `i64`, `v` wrappers are stripped wherever they
//! appear, and tuples and arrays are both treated as "a container".

use glib::{Variant, VariantClass};

use crate::core::message::{RawAttachment, RawMessage};

/// Strip any number of `v` wrappers.
pub fn unbox(v: &Variant) -> Variant {
    let mut cur = v.clone();
    // `as_variant` on a non-`v` value is a GLib-CRITICAL, so check the type.
    while cur.type_() == glib::VariantTy::VARIANT {
        match cur.as_variant() {
            Some(inner) => cur = inner,
            None => break,
        }
    }
    cur
}

/// An integer field, as Python's `int(x)` would read it.
pub fn int(v: &Variant) -> Option<i64> {
    let v = unbox(v);
    match v.classify() {
        VariantClass::Boolean => v.get::<bool>().map(i64::from),
        VariantClass::Byte => v.get::<u8>().map(i64::from),
        VariantClass::Int16 => v.get::<i16>().map(i64::from),
        VariantClass::Uint16 => v.get::<u16>().map(i64::from),
        VariantClass::Int32 => v.get::<i32>().map(i64::from),
        VariantClass::Uint32 => v.get::<u32>().map(i64::from),
        VariantClass::Int64 => v.get::<i64>(),
        VariantClass::Uint64 => v.get::<u64>().and_then(|x| i64::try_from(x).ok()),
        VariantClass::Double => v.get::<f64>().map(|f| f as i64),
        VariantClass::String => v.str().and_then(|s| s.trim().parse().ok()),
        _ => None,
    }
}

/// A string field, as Python's `str(x)` would read it.
pub fn string(v: &Variant) -> String {
    let v = unbox(v);
    match v.classify() {
        VariantClass::String | VariantClass::ObjectPath | VariantClass::Signature => {
            v.str().unwrap_or_default().to_string()
        }
        _ => match int(&v) {
            Some(n) => n.to_string(),
            None => v.print(false).to_string(),
        },
    }
}

/// The children of a tuple or array (after unboxing), or `None` for a scalar.
pub fn children(v: &Variant) -> Option<Vec<Variant>> {
    let v = unbox(v);
    if !v.is_container() {
        return None;
    }
    Some((0..v.n_children()).map(|i| v.child_value(i)).collect())
}

/// One address entry: `(s)` (KDE Connect's usual shape), or a bare string.
fn address(entry: &Variant) -> Option<String> {
    match children(entry) {
        Some(parts) => parts.first().map(string),
        None => Some(string(entry)),
    }
}

/// `(partID, mimeType, payload, uniqueIdentifier)`; `None` if malformed.
pub fn parse_attachment(v: &Variant) -> Option<RawAttachment> {
    let f = children(v)?;
    if f.len() < 4 {
        return None;
    }
    Some(RawAttachment {
        part_id: int(&f[0])?,
        mime_type: string(&f[1]),
        payload: string(&f[2]),
        unique_identifier: string(&f[3]),
    })
}

/// `(event, body, addresses, date, type, read, threadID, uID, subID,
/// attachments)`, possibly wrapped in `v`. `None` if malformed, as Python.
pub fn parse_message(v: &Variant) -> Option<RawMessage> {
    let f = children(v)?;
    if f.len() < 8 {
        return None;
    }
    let addresses = match children(&f[2]) {
        Some(entries) => entries.iter().filter_map(address).collect(),
        None => vec![string(&f[2])].into_iter().filter(|s| !s.is_empty()).collect(),
    };
    let attachments = f
        .get(9)
        .and_then(children)
        .map(|list| list.iter().filter_map(parse_attachment).collect())
        .unwrap_or_default();
    Some(RawMessage {
        event: int(&f[0]).unwrap_or(0) as i32,
        body: string(&f[1]),
        addresses,
        date: int(&f[3])?,
        msg_type: int(&f[4])? as i32,
        read: int(&f[5])? as i32,
        thread_id: int(&f[6])?,
        uid: int(&f[7])?,
        sub_id: f.get(8).and_then(int).unwrap_or(0),
        attachments,
    })
}

/// The messages in an `activeConversations` reply (`(av)`), skipping any
/// entry that does not parse.
pub fn parse_message_list(reply: &Variant) -> Vec<RawMessage> {
    message_variants(reply).iter().filter_map(parse_message).collect()
}

/// The raw entries of an `(av)` reply, for callers that also want their
/// signatures.
pub fn message_variants(reply: &Variant) -> Vec<Variant> {
    let reply = unbox(reply);
    // `(av)` → the array; a bare `av` (no tuple) is accepted too.
    if reply.type_().is_tuple() {
        reply.try_child_value(0).and_then(|a| children(&a)).unwrap_or_default()
    } else {
        children(&reply).unwrap_or_default()
    }
}

/// The message in a `conversationCreated` / `conversationUpdated` signal
/// (`(v)`).
pub fn parse_message_signal(params: &Variant) -> Option<RawMessage> {
    let f = children(params)?;
    match f.as_slice() {
        [only] => parse_message(only),
        // Some daemons may send the tuple itself as the parameters.
        _ => parse_message(params),
    }
}

/// The thread id in `conversationLoaded` / `conversationRemoved` (first arg).
pub fn first_int(params: &Variant) -> Option<i64> {
    children(params)?.first().and_then(int)
}

/// A string → value map (`a{sv}`, `a{ss}`), e.g. `getDirectories` or a
/// notification's `GetAll`.
pub fn string_map(v: &Variant) -> Vec<(String, Variant)> {
    children(v)
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| {
            let kv = children(entry)?;
            match kv.as_slice() {
                [k, v] => Some((string(k), unbox(v))),
                _ => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use glib::prelude::*;

    /// The shape kdeconnectd 26.08 marshals (ConversationMessage +
    /// `a(xsss)` attachments). TODO(Phase 2 verify): confirm against the
    /// laptop's `--dump-signatures` output.
    const WIRE_SIGNATURE: &str = "(isa(s)xiixxxa(xsss))";

    fn wire_message(addresses: &[&str], attachments: &[(i64, &str, &str, &str)]) -> Variant {
        let addrs: Vec<(String,)> = addresses.iter().map(|a| (a.to_string(),)).collect();
        let atts: Vec<(i64, String, String, String)> = attachments
            .iter()
            .map(|(p, m, pl, u)| (*p, m.to_string(), pl.to_string(), u.to_string()))
            .collect();
        Variant::tuple_from_iter([
            1i32.to_variant(),
            "hello".to_variant(),
            addrs.to_variant(),
            1_700_000_000_123i64.to_variant(),
            2i32.to_variant(),
            1i32.to_variant(),
            615i64.to_variant(),
            72001i64.to_variant(),
            (-1i64).to_variant(),
            atts.to_variant(),
        ])
    }

    #[test]
    fn wire_shape_parses() {
        let v = wire_message(&["+15550001111", "+15550002222"], &[(9, "image/jpeg", "", "a/b/IMG.jpg")]);
        assert_eq!(v.type_().as_str(), WIRE_SIGNATURE);
        let m = parse_message(&v).unwrap();
        assert_eq!(m.event, 1);
        assert_eq!(m.body, "hello");
        assert_eq!(m.addresses, vec!["+15550001111", "+15550002222"]);
        assert_eq!(m.date, 1_700_000_000_123);
        assert_eq!((m.msg_type, m.read, m.thread_id, m.uid, m.sub_id), (2, 1, 615, 72001, -1));
        assert_eq!(
            m.attachments,
            vec![RawAttachment {
                part_id: 9,
                mime_type: "image/jpeg".into(),
                payload: String::new(),
                unique_identifier: "a/b/IMG.jpg".into(),
            }]
        );
    }

    #[test]
    fn active_conversations_reply_and_signal() {
        let a = Variant::from_variant(&wire_message(&["22550"], &[]));
        let b = Variant::from_variant(&wire_message(&["+15550001111"], &[]));
        let reply = Variant::tuple_from_iter([vec![a.clone(), b].to_variant()]);
        assert_eq!(reply.type_().as_str(), "(av)");
        let list = parse_message_list(&reply);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].addresses, vec!["22550"]);

        let signal = Variant::tuple_from_iter([a]);
        assert_eq!(signal.type_().as_str(), "(v)");
        assert_eq!(parse_message_signal(&signal).unwrap().addresses, vec!["22550"]);
    }

    #[test]
    fn tolerant_of_other_widths_and_containers() {
        // Every int as `i`/`u`, addresses as `av` of bare strings, attachments
        // as `av`, and the whole thing as `av` instead of a tuple.
        let addrs = vec!["111".to_variant().to_variant(), ("222",).to_variant().to_variant()];
        let att = (3u32, "text/plain", "", "").to_variant().to_variant();
        let fields: Vec<Variant> = vec![
            1u32.to_variant(),
            "x".to_variant(),
            addrs.to_variant(),
            5i32.to_variant(),
            1u32.to_variant(),
            true.to_variant(),
            7i32.to_variant(),
            8u64.to_variant(),
            0i32.to_variant(),
            vec![att].to_variant(),
        ]
        .into_iter()
        .map(|f| Variant::from_variant(&f))
        .collect();
        let v = fields.to_variant();
        let m = parse_message(&v).unwrap();
        assert_eq!(m.addresses, vec!["111", "222"]);
        assert_eq!((m.date, m.read, m.thread_id, m.uid), (5, 1, 7, 8));
        assert_eq!(m.attachments.len(), 1);
        assert_eq!(m.attachments[0].part_id, 3);
    }

    #[test]
    fn malformed_is_none() {
        assert!(parse_message(&"x".to_variant()).is_none());
        assert!(parse_message(&(1i32, "a", 2i64).to_variant()).is_none());
        // Non-integer thread id, as Python's int() would reject.
        let bad = Variant::tuple_from_iter([
            0i32.to_variant(),
            "".to_variant(),
            Vec::<(String,)>::new().to_variant(),
            0i64.to_variant(),
            0i32.to_variant(),
            0i32.to_variant(),
            "nope".to_variant(),
            0i64.to_variant(),
        ]);
        assert!(parse_message(&bad).is_none());
        // Short or non-integer attachments are dropped, the message kept.
        let atts = vec![
            ("x", "image/png", "", "").to_variant().to_variant(),
            (1i64, "image/png").to_variant().to_variant(),
        ];
        let mut fields: Vec<Variant> = (0..9).map(|i| (i as i64).to_variant()).collect();
        fields[1] = "b".to_variant();
        fields[2] = Vec::<(String,)>::new().to_variant();
        fields.push(atts.to_variant());
        let m = parse_message(&Variant::tuple_from_iter(fields)).unwrap();
        assert!(m.attachments.is_empty());
    }

    #[test]
    fn maps_and_ints() {
        let mut map = std::collections::HashMap::new();
        map.insert("/storage/emulated/0".to_string(), "All files".to_variant());
        let reply = Variant::tuple_from_iter([map.to_variant()]);
        let entries = string_map(&children(&reply).unwrap()[0]);
        assert_eq!(entries.len(), 1);
        assert_eq!(string(&entries[0].1), "All files");
        assert_eq!(first_int(&(719i64, 3i64).to_variant()), Some(719));
        assert_eq!(string(&42i32.to_variant()), "42");
    }
}
