//! Searching message text (docs/NEXT_FEATURES.md, item A).
//!
//! Every message is already in memory (`Store::load_conversations`), so a
//! search is a scan over folded copies of the bodies, cached per
//! conversation. Folding ignores case and accents and treats curly quotes as
//! straight ones; match ranges are mapped back to the original text, so the
//! UI can bold them.

use std::collections::HashMap;

use unicode_normalization::char::{decompose_canonical, is_combining_mark};

use super::contacts::normalize_phone;
use super::models::{Conversation, SmsMessage};

/// Characters of context in a snippet, around the match. Little goes
/// before it: the list row shows about 30 characters and cuts the end.
const SNIPPET_CHARS: usize = 60;
const SNIPPET_BEFORE: usize = 12;
const ELLIPSIS: &str = "…";

/// Text folded for matching, with the original byte offset of each folded
/// byte.
#[derive(Debug, Clone)]
pub struct Folded {
    text: String,
    /// `origin[i]` = byte offset in the original of folded byte `i`.
    origin: Vec<usize>,
    /// The original's length in bytes of the character at each origin.
    char_len: Vec<u8>,
}

/// Lowercase, accents removed, curly quotes made straight.
pub fn fold(s: &str) -> Folded {
    let mut text = String::with_capacity(s.len());
    let mut origin = Vec::with_capacity(s.len());
    let mut char_len = Vec::with_capacity(s.len());
    for (at, ch) in s.char_indices() {
        let width = ch.len_utf8() as u8;
        let mut push = |c: char| {
            let c = match c {
                '\u{2018}' | '\u{2019}' | '\u{02BC}' => '\'',
                '\u{201C}' | '\u{201D}' => '"',
                _ => c,
            };
            for _ in 0..c.len_utf8() {
                origin.push(at);
                char_len.push(width);
            }
            text.push(c);
        };
        for lower in ch.to_lowercase() {
            decompose_canonical(lower, |d| {
                if !is_combining_mark(d) {
                    push(d);
                }
            });
        }
    }
    Folded { text, origin, char_len }
}

impl Folded {
    /// Every match of `needle` (already folded, not empty) as byte ranges in
    /// the original text, in order, not overlapping.
    pub fn find_all(&self, needle: &str) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(i) = self.text[from..].find(needle) {
            let s = from + i;
            let e = s + needle.len();
            let last = e - 1;
            out.push((self.origin[s], self.origin[last] + usize::from(self.char_len[last])));
            from = e;
        }
        out
    }
}

/// The folded search text, or None when there is nothing to search for.
pub fn fold_query(needle: &str) -> Option<String> {
    let f = fold(needle.trim()).text;
    if f.is_empty() { None } else { Some(f) }
}

/// A query that looks like part of a phone number ("(316) 555", "+1 316"),
/// as digits only. Needs at least three digits.
pub fn digits_query(needle: &str) -> Option<String> {
    let needle = needle.trim();
    if !needle.chars().all(|c| c.is_ascii_digit() || " +-().".contains(c)) {
        return None;
    }
    let digits = normalize_phone(needle);
    if digits.len() >= 3 { Some(digits) } else { None }
}

/// Whether any of `addresses` contains the digits of a number-like query.
pub fn number_matches(addresses: &[String], needle: &str) -> bool {
    digits_query(needle).is_some_and(|d| addresses.iter().any(|a| normalize_phone(a).contains(&d)))
}

/// One conversation's message-text matches.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// The newest matching message.
    pub uid: i64,
    /// How many messages match.
    pub count: usize,
    /// Text around the first match in the newest matching message.
    pub snippet: String,
    /// The matches inside `snippet`, as byte ranges.
    pub ranges: Vec<(usize, usize)>,
}

/// Cut `body` to about [`SNIPPET_CHARS`] around `matches[0]`, at word
/// boundaries, with line breaks flattened.
pub fn snippet(body: &str, matches: &[(usize, usize)]) -> (String, Vec<(usize, usize)>) {
    let flat: String = body.chars().map(|c| if matches!(c, '\n' | '\r' | '\t') { ' ' } else { c }).collect();
    let Some(&(ms, me)) = matches.first() else {
        return (String::new(), Vec::new());
    };
    let bounds: Vec<usize> = flat.char_indices().map(|(i, _)| i).chain([flat.len()]).collect();
    let char_at = |byte: usize| bounds.partition_point(|&b| b < byte);
    let (cs, ce, total) = (char_at(ms), char_at(me), bounds.len() - 1);
    let match_chars = ce - cs;
    let before = SNIPPET_BEFORE.min(cs);
    let mut start = cs - before;
    let mut end = (cs + match_chars.max(SNIPPET_CHARS - before)).min(total).max(ce);
    let chars: Vec<char> = flat.chars().collect();
    // Start after a space, and end before one, rather than mid-word.
    if start > 0 && !chars[start - 1].is_whitespace()
        && let Some(k) = (start..cs).find(|&k| chars[k].is_whitespace())
    {
        start = k + 1;
    }
    if end < total && !chars[end].is_whitespace()
        && let Some(k) = (ce..end).rev().find(|&k| chars[k].is_whitespace())
    {
        end = k;
    }
    while start < cs && chars[start].is_whitespace() {
        start += 1;
    }
    while end > ce && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    let (bs, be) = (bounds[start], bounds[end]);
    let prefix = if start > 0 { ELLIPSIS } else { "" };
    let mut out = format!("{prefix}{}", &flat[bs..be]);
    if end < total {
        out.push_str(ELLIPSIS);
    }
    let shift = prefix.len();
    let ranges = matches
        .iter()
        .filter(|&&(s, e)| s >= bs && e <= be)
        .map(|&(s, e)| (s - bs + shift, e - bs + shift))
        .collect();
    (out, ranges)
}

struct Cached {
    /// (message count, newest date, largest uid): changes when messages do.
    sig: (usize, i64, i64),
    /// Newest first: (uid, folded body).
    messages: Vec<(i64, Folded)>,
}

fn signature(conv: &Conversation) -> (usize, i64, i64) {
    let newest = conv.messages.iter().map(|m| m.date).max().unwrap_or(0);
    let top_uid = conv.messages.iter().map(|m| m.uid).max().unwrap_or(0);
    (conv.messages.len(), newest, top_uid)
}

/// Folded message text per conversation, built the first time a search
/// needs it and rebuilt when the conversation changes.
#[derive(Default)]
pub struct SearchIndex {
    threads: HashMap<i64, Cached>,
}

impl SearchIndex {
    /// Message-text hits in `convs`, by thread id. Only conversations with
    /// a matching message are in the result.
    pub fn search<'a>(&mut self, convs: impl IntoIterator<Item = &'a Conversation>, needle: &str) -> HashMap<i64, Hit> {
        let Some(q) = fold_query(needle) else { return HashMap::new() };
        let mut out = HashMap::new();
        for conv in convs {
            let sig = signature(conv);
            let cached = self.threads.entry(conv.thread_id).or_insert_with(|| Cached { sig: (usize::MAX, 0, 0), messages: Vec::new() });
            if cached.sig != sig {
                let mut msgs: Vec<&SmsMessage> = conv.messages.iter().filter(|m| !m.body.is_empty()).collect();
                msgs.sort_by_key(|m| std::cmp::Reverse((m.date, m.uid)));
                cached.messages = msgs.iter().map(|m| (m.uid, fold(&m.body))).collect();
                cached.sig = sig;
            }
            let mut count = 0;
            let mut first: Option<(i64, Vec<(usize, usize)>)> = None;
            for (uid, folded) in &cached.messages {
                if !folded.text.contains(q.as_str()) {
                    continue;
                }
                count += 1;
                if first.is_none() {
                    first = Some((*uid, folded.find_all(&q)));
                }
            }
            let Some((uid, ranges)) = first else { continue };
            let body = conv.messages.iter().find(|m| m.uid == uid).map_or("", |m| m.body.as_str());
            let (snippet, ranges) = snippet(body, &ranges);
            out.insert(conv.thread_id, Hit { uid, count, snippet, ranges });
        }
        out
    }

    /// Drop cached text for conversations that are gone.
    pub fn retain(&mut self, keep: impl Fn(i64) -> bool) {
        self.threads.retain(|tid, _| keep(*tid));
    }
}

/// The uids of `messages` whose text matches, in the order given.
pub fn matching_uids(messages: &[SmsMessage], needle: &str) -> Vec<i64> {
    let Some(q) = fold_query(needle) else { return Vec::new() };
    messages.iter().filter(|m| !m.body.is_empty() && fold(&m.body).text.contains(q.as_str())).map(|m| m.uid).collect()
}

/// `text` as Pango markup with `ranges` in bold.
pub fn bold_markup(text: &str, ranges: &[(usize, usize)]) -> String {
    let mut out = String::new();
    let mut at = 0;
    for &(s, e) in ranges {
        if s < at || e > text.len() || !text.is_char_boundary(s) || !text.is_char_boundary(e) {
            continue;
        }
        out.push_str(&glib_escape(&text[at..s]));
        out.push_str("<b>");
        out.push_str(&glib_escape(&text[s..e]));
        out.push_str("</b>");
        at = e;
    }
    out.push_str(&glib_escape(&text[at..]));
    out
}

/// Markup escaping as `g_markup_escape_text` (kept here so core stays free
/// of GLib).
fn glib_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&#39;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(uid: i64, date: i64, body: &str) -> SmsMessage {
        SmsMessage { uid, date, body: body.into(), thread_id: 1, ..Default::default() }
    }

    fn conv(thread_id: i64, address: &str, msgs: Vec<SmsMessage>) -> Conversation {
        let mut c = Conversation::new(thread_id);
        c.address = address.into();
        c.addresses = vec![address.into()];
        c.messages = msgs;
        c
    }

    fn matched<'a>(text: &'a str, ranges: &[(usize, usize)]) -> Vec<&'a str> {
        ranges.iter().map(|&(s, e)| &text[s..e]).collect()
    }

    #[test]
    fn folding_ignores_case_accents_and_curly_quotes() {
        let f = fold("Café CRÈME don’t");
        assert_eq!(f.text, "cafe creme don't");
        let body = "Le CAFÉ est prêt";
        let found = fold(body).find_all("cafe");
        assert_eq!(matched(body, &found), ["CAFÉ"]);
        assert_eq!(matched("Ünïcödé", &fold("Ünïcödé").find_all("unicode")), ["Ünïcödé"]);
    }

    #[test]
    fn matches_map_back_to_multibyte_text() {
        let body = "😀 see you at the PARK 🌳, park later";
        let found = fold(body).find_all("park");
        assert_eq!(matched(body, &found), ["PARK", "park"]);
    }

    #[test]
    fn empty_query_finds_nothing() {
        let c = conv(1, "+13165551234", vec![msg(1, 10, "hello")]);
        let mut idx = SearchIndex::default();
        assert!(idx.search([&c], "").is_empty());
        assert!(idx.search([&c], "   ").is_empty());
        assert!(matching_uids(&c.messages, " ").is_empty());
    }

    #[test]
    fn newest_match_wins_and_matches_are_counted() {
        let c = conv(7, "+13165551234", vec![
            msg(1, 100, "Dinner at 6?"),
            msg(2, 200, "ok"),
            msg(3, 300, "dinner was great"),
            msg(4, 400, "bye"),
        ]);
        let other = conv(8, "+13165550000", vec![msg(9, 50, "nothing here")]);
        let mut idx = SearchIndex::default();
        let hits = idx.search([&c, &other], "DINNER");
        assert_eq!(hits.len(), 1);
        let hit = &hits[&7];
        assert_eq!(hit.uid, 3);
        assert_eq!(hit.count, 2);
        assert_eq!(hit.snippet, "dinner was great");
        assert_eq!(matched(&hit.snippet, &hit.ranges), ["dinner"]);
    }

    #[test]
    fn cache_follows_new_messages() {
        let mut c = conv(1, "+1", vec![msg(1, 1, "apple")]);
        let mut idx = SearchIndex::default();
        assert!(idx.search([&c], "banana").is_empty());
        c.messages.push(msg(2, 2, "banana bread"));
        assert_eq!(idx.search([&c], "banana")[&1].uid, 2);
    }

    #[test]
    fn snippets_cut_at_words_with_ellipses() {
        let body = "This is a fairly long message that goes on and on before it finally mentions the \
                    birthday party on Saturday and then keeps going for quite a while longer after that";
        let found = fold(body).find_all("birthday");
        let (s, r) = snippet(body, &found);
        assert!(s.starts_with('…') && s.ends_with('…'), "{s}");
        assert_eq!(matched(&s, &r), ["birthday"]);
        let inner = s.trim_matches('…');
        assert!(body.contains(inner), "{s}");
        // No word is cut in half at either end.
        let first = inner.split(' ').next().unwrap();
        let last = inner.split(' ').next_back().unwrap();
        assert!(body.split(' ').any(|w| w == first), "{s}");
        assert!(body.split(' ').any(|w| w.trim_matches(',') == last.trim_matches(',')), "{s}");
        assert!(s.chars().count() <= SNIPPET_CHARS + 2, "{s}");
    }

    #[test]
    fn snippet_of_short_text_is_the_text_flattened() {
        let body = "See you\nat 5";
        let found = fold(body).find_all("at 5");
        let (s, r) = snippet(body, &found);
        assert_eq!(s, "See you at 5");
        assert_eq!(matched(&s, &r), ["at 5"]);
    }

    #[test]
    fn match_stays_near_the_start() {
        // The list row shows ~30 characters; the match must be in them.
        let body = "Don't forget the birthday party at the lake house on Saturday";
        let found = fold(body).find_all("lake");
        let (s, r) = snippet(body, &found);
        assert_eq!(s, "…at the lake house on Saturday");
        assert_eq!(matched(&s, &r), ["lake"]);
        assert!(s[..r[0].0].chars().count() <= SNIPPET_BEFORE + 1, "{s}");
    }

    #[test]
    fn snippet_of_multibyte_text() {
        let body = "日本語のテキストです。明日は東京で会いましょう。それではまた。".repeat(3);
        let found = fold(&body).find_all("東京");
        let (s, r) = snippet(&body, &found);
        assert_eq!(matched(&s, &r)[0], "東京");
    }

    #[test]
    fn digit_queries_match_numbers() {
        let addrs = vec!["+1 (316) 555-1234".to_string()];
        assert!(number_matches(&addrs, "(316) 555"));
        assert!(number_matches(&addrs, "5551234"));
        assert!(!number_matches(&addrs, "777"));
        assert!(!number_matches(&addrs, "31"), "too short to mean a number");
        assert!(!number_matches(&addrs, "316 pizza"));
        assert_eq!(digits_query("+1-316"), Some("1316".into()));
    }

    #[test]
    fn thread_matches_in_order() {
        let msgs = vec![msg(1, 1, "Party?"), msg(2, 2, "no"), msg(3, 3, "the PARTY was fun"), msg(4, 4, "")];
        assert_eq!(matching_uids(&msgs, "party"), [1, 3]);
    }

    #[test]
    fn markup_bolds_and_escapes() {
        let text = "a <b> & party";
        let ranges = fold(text).find_all("party");
        assert_eq!(bold_markup(text, &ranges), "a &lt;b&gt; &amp; <b>party</b>");
    }
}
