//! Calls, for the calls button in the header (not in the chats).
//!
//! KDE Connect 26.08 emits `telephony.callReceived(type, number, name)` with
//! `type` = `"callReceived"` when the phone rings and `"missedCall"` when a
//! call was missed (read in `plugins/telephony/telephonyplugin.cpp`,
//! v26.08.3). Nothing is sent when a call is answered or ends, so a call
//! shows as "Incoming call" and turns into "Missed call" if a miss follows.
//! Only calls made while KDE Connect runs are known: it can't read the
//! phone's call log.

use super::contacts::normalize_phone;
use super::models::Call;

/// A miss this soon after a ring with the same number is that ring.
pub const MISS_JOINS_RING_MS: i64 = 10 * 60_000;
/// A second ring this soon with the same number is the same call (the
/// phone can report one call twice).
pub const RING_REPEAT_MS: i64 = 60_000;

/// Nothing says when a ringing call is answered or ends: it shows as
/// ringing this long unless a miss comes first.
pub const RING_SHOWN_MS: i64 = 30_000;

/// What a `callReceived` signal means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallEvent {
    Ringing,
    Missed,
}

impl CallEvent {
    /// `"ringing"` is accepted too (older daemons, and our own tests).
    pub fn parse(kind: &str) -> Option<Self> {
        match kind {
            "callReceived" | "ringing" => Some(Self::Ringing),
            "missedCall" => Some(Self::Missed),
            _ => None,
        }
    }
}

/// A number worth listing: at least 3 digits (KDE Connect sends "unknown
/// number", localized, for hidden callers).
pub fn callable(number: &str) -> bool {
    normalize_phone(number).chars().count() >= 3
}

fn same(a: &str, b: &str) -> bool {
    let (a, b) = (normalize_phone(a), normalize_phone(b));
    let tail = |s: &str| s.chars().rev().take(10).collect::<String>();
    a == b || (a.len() >= 10 && b.len() >= 10 && tail(&a) == tail(&b))
}

/// Apply one event to `calls` (oldest first). Returns the call added or
/// changed, or `None` when nothing changed.
pub fn record(calls: &mut Vec<Call>, event: CallEvent, number: &str, now_ms: i64) -> Option<Call> {
    if !callable(number) {
        return None;
    }
    let last = calls.iter_mut().rev().find(|c| same(&c.number, number));
    match (event, last) {
        (CallEvent::Ringing, Some(c)) if !c.missed && now_ms - c.date < RING_REPEAT_MS => None,
        (CallEvent::Missed, Some(c)) if c.missed && now_ms - c.date < RING_REPEAT_MS => None,
        (CallEvent::Missed, Some(c)) if !c.missed && now_ms - c.date < MISS_JOINS_RING_MS => {
            c.missed = true;
            c.seen = false;
            Some(c.clone())
        }
        _ => {
            let call = Call { number: number.to_string(), date: now_ms, missed: event == CallEvent::Missed, seen: false };
            calls.push(call.clone());
            Some(call)
        }
    }
}

/// The call ringing now, if any: the newest, not missed, recent.
pub fn ringing(calls: &[Call], now_ms: i64) -> Option<&Call> {
    calls.last().filter(|c| !c.missed && (0..RING_SHOWN_MS).contains(&(now_ms - c.date)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::store::MessageStore;

    #[test]
    fn parses_the_daemons_types() {
        assert_eq!(CallEvent::parse("callReceived"), Some(CallEvent::Ringing));
        assert_eq!(CallEvent::parse("missedCall"), Some(CallEvent::Missed));
        assert_eq!(CallEvent::parse(""), None);
        assert_eq!(CallEvent::parse("talking"), None);
    }

    #[test]
    fn a_miss_turns_its_ring_into_a_missed_call() {
        let mut calls = Vec::new();
        let rang = record(&mut calls, CallEvent::Ringing, "+1 316-555-1111", 1_000).unwrap();
        assert!(!rang.missed);
        // The phone reports the same ring twice: one call.
        assert_eq!(record(&mut calls, CallEvent::Ringing, "3165551111", 5_000), None);
        let missed = record(&mut calls, CallEvent::Missed, "3165551111", 30_000).unwrap();
        assert!(missed.missed);
        assert_eq!(missed.date, 1_000);
        assert_eq!(calls.len(), 1);
        // A later miss with no ring is its own call.
        record(&mut calls, CallEvent::Missed, "3165551111", 2 * MISS_JOINS_RING_MS).unwrap();
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn hidden_callers_and_other_numbers_are_kept_apart() {
        let mut calls = Vec::new();
        assert_eq!(record(&mut calls, CallEvent::Ringing, "unknown number", 1), None);
        record(&mut calls, CallEvent::Ringing, "3165551111", 1).unwrap();
        record(&mut calls, CallEvent::Missed, "3165552222", 2).unwrap();
        assert_eq!(calls.len(), 2);
        assert!(!calls[0].missed && calls[1].missed);
        assert!(same(&calls[0].number, "+13165551111") && !same(&calls[0].number, "3165552222"));
    }

    #[test]
    fn rings_until_missed_or_for_a_while() {
        let mut calls = Vec::new();
        assert_eq!(ringing(&calls, 0), None);
        record(&mut calls, CallEvent::Ringing, "3165551111", 1_000).unwrap();
        assert!(ringing(&calls, 2_000).is_some());
        assert_eq!(ringing(&calls, 1_000 + RING_SHOWN_MS), None);
        record(&mut calls, CallEvent::Missed, "3165551111", 9_000).unwrap();
        assert_eq!(ringing(&calls, 10_000), None);
    }

    #[test]
    fn calls_round_trip_through_the_store() {
        let store = MessageStore::in_memory();
        let c = Call { number: "+13165551111".into(), date: 42, missed: true, seen: false };
        store.upsert_call("dev", &c);
        store.upsert_call("dev", &Call { seen: true, ..c.clone() });
        store.upsert_call("other", &c);
        assert_eq!(store.load_calls("dev"), vec![Call { seen: true, ..c.clone() }]);
        store.clear_calls("dev");
        assert!(store.load_calls("dev").is_empty());
        assert_eq!(store.load_calls("other").len(), 1);
    }
}
