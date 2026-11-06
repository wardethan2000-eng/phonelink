//! Optional timing logs for comparing with the Python app (port brief
//! baselines: chat open 11–28 ms for the biggest threads; the main loop never
//! blocked over 20 ms during a 2,600-message replay). Off unless
//! `PHONELINK_PERF=1`; then lines go to the log at info level.

use std::cell::Cell;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("PHONELINK_PERF").is_ok_and(|v| v == "1"))
}

thread_local! {
    /// When the chat being opened was asked for, and its size.
    static OPENING: Cell<Option<(Instant, usize)>> = const { Cell::new(None) };
}

/// A chat is being opened (before building its bubbles).
pub fn opening(messages: usize) {
    if enabled() {
        OPENING.set(Some((Instant::now(), messages)));
    }
}

/// The bubbles are built (still transparent).
pub fn built() {
    if let Some((t, n)) = OPENING.get().filter(|_| enabled()) {
        log::info!("perf: chat with {n} messages built in {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
    }
}

/// The chat appeared on screen, at the bottom.
pub fn revealed() {
    if let Some((t, n)) = OPENING.take().filter(|_| enabled()) {
        log::info!("perf: chat with {n} messages on screen in {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
    }
}

/// Watch the main loop: a 10 ms timer that notes how late it fires. Logs any
/// stall over 20 ms, and every 30 s the worst one seen.
pub fn watch_main_loop() {
    if !enabled() {
        return;
    }
    let last = Cell::new(Instant::now());
    let worst = Cell::new(Duration::ZERO);
    let since_report = Cell::new(Instant::now());
    glib::timeout_add_local(Duration::from_millis(10), move || {
        let now = Instant::now();
        let late = now.duration_since(last.get()).saturating_sub(Duration::from_millis(10));
        last.set(now);
        if late > Duration::from_millis(20) {
            log::info!("perf: main loop blocked {:.0} ms", late.as_secs_f64() * 1000.0);
        }
        worst.set(worst.get().max(late));
        if since_report.get().elapsed() >= Duration::from_secs(30) {
            log::info!("perf: worst main-loop stall in the last 30 s: {:.0} ms", worst.get().as_secs_f64() * 1000.0);
            worst.set(Duration::ZERO);
            since_report.set(now);
        }
        glib::ControlFlow::Continue
    });
}

/// Times a block; logs it if it took over 10 ms (with PHONELINK_PERF=1).
pub struct Span(Option<(&'static str, Instant)>);

pub fn span(name: &'static str) -> Span {
    Span(enabled().then(|| (name, Instant::now())))
}

impl Drop for Span {
    fn drop(&mut self) {
        if let Some((name, t)) = self.0.take() {
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            if ms > 10.0 {
                log::info!("perf: {name} took {ms:.1} ms");
            }
        }
    }
}
