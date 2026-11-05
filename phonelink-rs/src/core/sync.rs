//! Catch-up and gap fill: pulling texts the phone never pushed.
//!
//! In the Python app this logic is spread through `ui/sms_panel.py` as GLib
//! timers. Here it is a pure state machine: the caller passes the current
//! monotonic time in seconds, and the machine returns [`Action`]s (D-Bus
//! requests) for the caller to send. The caller runs one GLib timer for
//! [`CatchUp::next_deadline`] and calls [`CatchUp::tick`] when it fires.
//!
//! Why this exists: KDE Connect's Android SMS plugin resets its "newest
//! message" timestamp to *now* on every connection, so texts that arrive
//! while the laptop sleeps are never pushed. On connect, reconnect, resume and
//! window show we ask for every thread's newest message
//! (`requestAllConversationThreads`); any thread whose newest message moved
//! is gap-filled:
//!
//! 1. `sms.requestConversation(thread, -1, 100)` asks the *phone* (bypassing
//!    the daemon's cache, which can be missing the middle of a thread).
//! 2. On `conversationLoaded`, `conversations.requestConversation(thread,
//!    start, end)` replays the daemon's now-complete cache as
//!    `conversationUpdated` signals (the daemon only signals the newest
//!    message per thread on its own). Indices are newest-first.
//! 3. Three seconds later, if that page was ≥90% full and ≥50% new, the next
//!    older page is fetched (`older_than` = oldest date seen), up to 10 pages.

use std::collections::{HashMap, VecDeque};

pub const CATCH_UP_MIN_INTERVAL_SECONDS: f64 = 20.0;
/// Window shown: catch up at most this often.
pub const CATCH_UP_ON_SHOW_INTERVAL_SECONDS: f64 = 120.0;
pub const CATCH_UP_DELAY_SECONDS: f64 = 3.0;
/// First pull after a device appears waits until startup is interactive.
pub const CATCH_UP_STARTUP_DELAY_SECONDS: f64 = 10.0;
/// After resume from sleep, give Wi-Fi and the phone link a moment.
pub const RESUME_DELAY_SECONDS: f64 = 8.0;
pub const GAP_FILL_MESSAGES: i64 = 100;
pub const GAP_FILL_MAX_PAGES: u32 = 10;
pub const GAP_FILL_SETTLE_SECONDS: f64 = 3.0;
pub const GAP_FILL_SPACING_SECONDS: f64 = 0.25;
pub const GAP_FILL_TIMEOUT_SECONDS: f64 = 60.0;
/// Incoming messages are merged this many at a time, yielding to the UI
/// between batches. One callback + two commits per message froze the app.
pub const SIGNAL_BATCH_SIZE: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// `conversations.requestAllConversationThreads()`
    RequestAllThreads,
    /// `sms.requestConversation(thread, older_than or -1, count)` on the
    /// device's `/sms` path.
    RequestFromPhone { thread_id: i64, count: i64, older_than: Option<i64> },
    /// `conversations.requestConversation(thread, start, end)` — replay from
    /// the daemon's cache, newest-first indices.
    ReplayFromDaemon { thread_id: i64, start: i64, end: i64 },
}

#[derive(Debug, Clone, Default)]
struct Page {
    /// Pages requested so far for this gap fill (1-based).
    pages: u32,
    added: i64,
    known: i64,
    oldest: Option<i64>,
    /// When to judge the page (set once its replay was requested).
    check_at: Option<f64>,
}

#[derive(Debug, Default)]
pub struct CatchUp {
    reachable: bool,
    last_catch_up: Option<f64>,
    catch_up_at: Option<f64>,
    queue: VecDeque<i64>,
    next_drain_at: Option<f64>,
    /// thread → when the phone was last asked (de-dupes requests for 60 s).
    pending: HashMap<i64, f64>,
    pages: HashMap<i64, Page>,
}

impl CatchUp {
    pub fn new() -> Self {
        Self::default()
    }

    /// The device's reachability changed (or the device went away). Going
    /// unreachable cancels everything in flight.
    pub fn set_reachable(&mut self, reachable: bool) {
        self.reachable = reachable;
        if !reachable {
            self.cancel();
        }
    }

    /// Drop every scheduled pull and gap fill (device switched or lost).
    ///
    /// Python left `_gap_fill_pending` uncleared here (an unreachable line in
    /// `_check_gap_fill_page`), which blocked re-asking a thread for up to
    /// 60 s after a device switch; clearing it is the intended behavior.
    pub fn cancel(&mut self) {
        self.catch_up_at = None;
        self.queue.clear();
        self.next_drain_at = None;
        self.pending.clear();
        self.pages.clear();
    }

    /// Schedule a pull of every thread's newest message `delay` seconds from
    /// now, unless one is already scheduled or one ran within `min_interval`.
    /// Called on connect (delay 10, min 0), reconnect and resume (defaults),
    /// and window show (min 120).
    pub fn catch_up(&mut self, now: f64, delay: f64, min_interval: f64) {
        if !self.reachable || self.catch_up_at.is_some() {
            return;
        }
        if let Some(last) = self.last_catch_up {
            if now - last < min_interval {
                return;
            }
        }
        self.catch_up_at = Some(now + delay);
    }

    /// Gap-fill one phone thread (no-op for drafts, duplicates, and threads
    /// asked within the last 60 s).
    pub fn queue_gap_fill(&mut self, thread_id: i64, now: f64) {
        if thread_id < 0 || self.queue.contains(&thread_id) {
            return;
        }
        if let Some(&asked) = self.pending.get(&thread_id) {
            if now - asked < GAP_FILL_TIMEOUT_SECONDS {
                return;
            }
        }
        self.queue.push_back(thread_id);
        if self.next_drain_at.is_none() {
            self.next_drain_at = Some(now + GAP_FILL_SPACING_SECONDS);
        }
    }

    /// Gap-fill a conversation: its primary thread and every phone thread
    /// merged into it (SMS/MMS split, dual SIM).
    pub fn queue_conversation_gap_fill(&mut self, thread_ids: &[i64], now: f64) {
        for &t in thread_ids {
            self.queue_gap_fill(t, now);
        }
    }

    /// A message was merged; `added` = it was new to the store.
    pub fn note_message(&mut self, thread_id: i64, date: i64, added: bool) {
        let Some(page) = self.pages.get_mut(&thread_id) else { return };
        if added {
            page.added += 1;
        } else {
            page.known += 1;
        }
        if page.oldest.is_none_or(|o| date < o) {
            page.oldest = Some(date);
        }
    }

    /// `conversationLoaded(thread)` arrived. If we asked the phone for this
    /// thread, returns the replay request for the page that just loaded.
    pub fn on_conversation_loaded(&mut self, thread_id: i64, now: f64) -> Option<Action> {
        self.pending.remove(&thread_id)?;
        let pages = self.pages.get(&thread_id).map_or(1, |p| p.pages) as i64;
        let start = (GAP_FILL_MESSAGES * (pages - 1) - 10).max(0);
        if let Some(page) = self.pages.get_mut(&thread_id) {
            page.check_at = Some(now + GAP_FILL_SETTLE_SECONDS);
        }
        Some(Action::ReplayFromDaemon { thread_id, start, end: GAP_FILL_MESSAGES * pages })
    }

    /// Earliest time [`tick`](Self::tick) has work to do.
    pub fn next_deadline(&self) -> Option<f64> {
        let checks = self.pages.values().filter_map(|p| p.check_at);
        self.catch_up_at
            .into_iter()
            .chain(self.next_drain_at)
            .chain(checks)
            .min_by(f64::total_cmp)
    }

    /// Run everything due at `now`. `active_threads` is the open
    /// conversation's phone threads (primary first), gap-filled with every
    /// catch-up.
    pub fn tick(&mut self, now: f64, active_threads: &[i64]) -> Vec<Action> {
        let mut out = Vec::new();

        if self.catch_up_at.is_some_and(|t| t <= now) {
            self.catch_up_at = None;
            if self.reachable {
                self.last_catch_up = Some(now);
                out.push(Action::RequestAllThreads);
                self.queue_conversation_gap_fill(active_threads, now);
            }
        }

        // One phone request per spacing interval.
        if self.next_drain_at.is_some_and(|t| t <= now) {
            self.next_drain_at = None;
            if self.reachable {
                if let Some(thread_id) = self.queue.pop_front() {
                    self.pending.insert(thread_id, now);
                    let page = self.pages.entry(thread_id).or_insert(Page { pages: 1, ..Default::default() });
                    page.added = 0;
                    page.known = 0;
                    page.oldest = None;
                    out.push(Action::RequestFromPhone { thread_id, count: GAP_FILL_MESSAGES, older_than: None });
                }
                if !self.queue.is_empty() {
                    self.next_drain_at = Some(now + GAP_FILL_SPACING_SECONDS);
                }
            } else {
                self.queue.clear();
            }
        }

        let due: Vec<i64> = self
            .pages
            .iter()
            .filter(|(_, p)| p.check_at.is_some_and(|t| t <= now))
            .map(|(&t, _)| t)
            .collect();
        for thread_id in due {
            if let Some(action) = self.check_page(thread_id, now) {
                out.push(action);
            }
        }
        out
    }

    /// After a page's replay settles: fetch the next older page, or stop.
    fn check_page(&mut self, thread_id: i64, now: f64) -> Option<Action> {
        let page = self.pages.get_mut(&thread_id)?;
        page.check_at = None;
        let seen = page.added + page.known;
        let more = self.reachable
            && page.pages < GAP_FILL_MAX_PAGES
            && page.oldest.is_some()
            && seen as f64 >= GAP_FILL_MESSAGES as f64 * 0.9 // the page was full …
            && page.added >= GAP_FILL_MESSAGES / 2; // … and mostly new to us
        if !more {
            self.pages.remove(&thread_id);
            return None;
        }
        let oldest = page.oldest;
        page.pages += 1;
        page.added = 0;
        page.known = 0;
        page.oldest = None;
        self.pending.insert(thread_id, now);
        Some(Action::RequestFromPhone { thread_id, count: GAP_FILL_MESSAGES, older_than: oldest })
    }
}

/// Queue for `conversationCreated/Updated` payloads, drained
/// [`SIGNAL_BATCH_SIZE`] at a time from an idle callback, each batch inside
/// one store transaction.
#[derive(Debug)]
pub struct SignalQueue<T> {
    items: VecDeque<T>,
    drain_scheduled: bool,
}

impl<T> Default for SignalQueue<T> {
    fn default() -> Self {
        Self { items: VecDeque::new(), drain_scheduled: false }
    }
}

impl<T> SignalQueue<T> {
    /// Queue an item. Returns true when the caller must schedule a drain.
    pub fn push(&mut self, item: T) -> bool {
        self.items.push_back(item);
        !std::mem::replace(&mut self.drain_scheduled, true)
    }

    /// Take the next batch. `more` is false once the queue is empty, which
    /// also ends the scheduled drain.
    pub fn next_batch(&mut self) -> (Vec<T>, bool) {
        let n = self.items.len().min(SIGNAL_BATCH_SIZE);
        let batch: Vec<T> = self.items.drain(..n).collect();
        let more = !self.items.is_empty();
        if !more {
            self.drain_scheduled = false;
        }
        (batch, more)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn online() -> CatchUp {
        let mut s = CatchUp::new();
        s.set_reachable(true);
        s
    }

    fn run_until(s: &mut CatchUp, until: f64, active: &[i64]) -> Vec<Action> {
        let mut out = Vec::new();
        while let Some(t) = s.next_deadline() {
            if t > until {
                break;
            }
            out.extend(s.tick(t, active));
        }
        out
    }

    #[test]
    fn catch_up_is_rate_limited_and_gap_fills_the_open_chat() {
        let mut s = online();
        s.catch_up(0.0, 3.0, CATCH_UP_MIN_INTERVAL_SECONDS);
        s.catch_up(1.0, 3.0, CATCH_UP_MIN_INTERVAL_SECONDS); // already scheduled
        let acts = run_until(&mut s, 5.0, &[7, 9]);
        assert_eq!(
            acts,
            vec![
                Action::RequestAllThreads,
                Action::RequestFromPhone { thread_id: 7, count: 100, older_than: None },
                Action::RequestFromPhone { thread_id: 9, count: 100, older_than: None },
            ]
        );
        s.catch_up(10.0, 3.0, CATCH_UP_MIN_INTERVAL_SECONDS); // 7 s after the last: skipped
        assert_eq!(s.next_deadline(), None);
        s.catch_up(30.0, 3.0, CATCH_UP_MIN_INTERVAL_SECONDS);
        assert_eq!(s.next_deadline(), Some(33.0));
    }

    #[test]
    fn nothing_happens_while_unreachable() {
        let mut s = CatchUp::new();
        s.catch_up(0.0, 3.0, 0.0);
        s.queue_gap_fill(5, 0.0);
        assert!(run_until(&mut s, 100.0, &[]).is_empty());
    }

    #[test]
    fn going_offline_cancels_in_flight_work() {
        let mut s = online();
        s.queue_gap_fill(5, 0.0);
        s.queue_gap_fill(6, 0.0);
        s.set_reachable(false);
        s.set_reachable(true);
        assert_eq!(s.next_deadline(), None);
        s.queue_gap_fill(5, 1.0); // pending was cleared, so it is not deduped
        assert_eq!(s.next_deadline(), Some(1.25));
    }

    #[test]
    fn gap_fill_requests_are_spaced_and_deduped() {
        let mut s = online();
        s.queue_gap_fill(1, 0.0);
        s.queue_gap_fill(1, 0.0);
        s.queue_gap_fill(-4, 0.0); // drafts are never asked for
        s.queue_gap_fill(2, 0.0);
        assert_eq!(s.tick(0.25, &[]).len(), 1);
        assert_eq!(s.next_deadline(), Some(0.5));
        assert_eq!(s.tick(0.5, &[]).len(), 1);
        // Asked within 60 s: not asked again.
        s.queue_gap_fill(1, 10.0);
        assert_eq!(s.next_deadline(), None);
        s.queue_gap_fill(1, 61.0);
        assert!(s.next_deadline().is_some());
    }

    #[test]
    fn loaded_page_replays_then_pages_back_while_full_and_new() {
        let mut s = online();
        s.queue_gap_fill(3, 0.0);
        s.tick(0.25, &[]);
        assert_eq!(s.on_conversation_loaded(99, 1.0), None); // not ours
        assert_eq!(
            s.on_conversation_loaded(3, 1.0),
            Some(Action::ReplayFromDaemon { thread_id: 3, start: 0, end: 100 })
        );
        for i in 0..95 {
            s.note_message(3, 10_000 - i, i < 60);
        }
        assert_eq!(s.next_deadline(), Some(4.0));
        assert_eq!(
            s.tick(4.0, &[]),
            vec![Action::RequestFromPhone { thread_id: 3, count: 100, older_than: Some(9_906) }]
        );
        // Page 2 replays the second hundred (with 10 of overlap).
        assert_eq!(
            s.on_conversation_loaded(3, 5.0),
            Some(Action::ReplayFromDaemon { thread_id: 3, start: 90, end: 200 })
        );
        // Mostly known: stop.
        for i in 0..100 {
            s.note_message(3, 5_000 - i, i < 10);
        }
        assert!(s.tick(8.0, &[]).is_empty());
        assert_eq!(s.next_deadline(), None);
    }

    #[test]
    fn paging_stops_at_ten_pages() {
        let mut s = online();
        s.queue_gap_fill(3, 0.0);
        s.tick(0.25, &[]);
        let mut now = 1.0;
        let mut requests = 1;
        loop {
            s.on_conversation_loaded(3, now).unwrap();
            for i in 0..100 {
                s.note_message(3, 1_000_000 - (requests * 100 + i), true);
            }
            now += GAP_FILL_SETTLE_SECONDS;
            let acts = s.tick(now, &[]);
            if acts.is_empty() {
                break;
            }
            requests += 1;
            now += 1.0;
        }
        assert_eq!(requests, GAP_FILL_MAX_PAGES as i64);
    }

    #[test]
    fn signal_queue_batches_and_reschedules_once() {
        let mut q = SignalQueue::default();
        assert!(q.push(0));
        for i in 1..450 {
            assert!(!q.push(i));
        }
        let (b, more) = q.next_batch();
        assert_eq!((b.len(), more), (200, true));
        let (b, more) = q.next_batch();
        assert_eq!((b.len(), more), (200, true));
        let (b, more) = q.next_batch();
        assert_eq!((b.len(), more), (50, false));
        assert!(q.push(1)); // drain ended, so the next push schedules again
    }
}
