//! Runs `phonelink-gm serve` (the lasting Google Messages link) as a child
//! process and talks to it in JSON lines (`core::gm`).
//!
//! Started only when the helper and gmcli's pairing exist. If it exits it
//! is restarted, waiting longer each time (1 s, doubling, at most 10 min);
//! a signed-out account stops it until the app restarts. When the app
//! exits, the helper's stdin closes and it quits.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::rc::Rc;
use std::time::Duration;

use gio::prelude::*;

use crate::core::gm::{Incoming, Reply, parse_line};
use crate::core::phone_delete;

type Done = Box<dyn FnOnce(Reply)>;

#[derive(Default)]
struct State {
    proc: Option<gio::Subprocess>,
    stdin: Option<gio::OutputStream>,
    ready: bool,
    next_id: u64,
    pending: HashMap<u64, Done>,
    /// Requests written once the link is ready.
    queued: Vec<String>,
    /// Exits in a row without reaching ready.
    failures: u32,
    /// Signed out: don't restart.
    stopped: bool,
    restart: Option<glib::SourceId>,
}

pub struct GmLink {
    state: RefCell<State>,
    on_event: Box<dyn Fn(Incoming)>,
}

impl GmLink {
    /// Start the link if Google Messages is paired on this computer.
    /// `on_event` gets every event but replies.
    pub fn start(on_event: impl Fn(Incoming) + 'static) -> Option<Rc<Self>> {
        if !phone_delete::available() {
            return None;
        }
        let link = Rc::new(Self { state: RefCell::default(), on_event: Box::new(on_event) });
        link.spawn();
        Some(link)
    }

    /// Is the helper running (connected or still connecting)?
    pub fn is_running(&self) -> bool {
        self.state.borrow().proc.is_some()
    }

    /// Send a request built from its id. `done` gets the reply, or a
    /// failed one if the helper stops first. Returns false (and drops
    /// `done`) when the helper isn't running.
    pub fn request(&self, make: impl FnOnce(u64) -> String, done: impl FnOnce(Reply) + 'static) -> bool {
        let (line, write_now) = {
            let mut s = self.state.borrow_mut();
            if s.proc.is_none() {
                return false;
            }
            s.next_id += 1;
            let id = s.next_id;
            s.pending.insert(id, Box::new(done));
            let line = make(id);
            if !s.ready {
                s.queued.push(line);
                return true;
            }
            (line, s.stdin.clone())
        };
        if let Some(stdin) = write_now {
            write_line(&stdin, &line);
        }
        true
    }

    fn spawn(self: &Rc<Self>) {
        let Some(helper) = phone_delete::helper_path() else { return };
        let args: [&OsStr; 3] = [helper.as_os_str(), OsStr::new("serve"), OsStr::new("--timeout=90s")];
        let proc = match gio::Subprocess::newv(&args, gio::SubprocessFlags::STDIN_PIPE | gio::SubprocessFlags::STDOUT_PIPE) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("Google Messages link: couldn't start the helper: {e}");
                self.schedule_restart();
                return;
            }
        };
        let (Some(stdin), Some(stdout)) = (proc.stdin_pipe(), proc.stdout_pipe()) else { return };
        {
            let mut s = self.state.borrow_mut();
            s.proc = Some(proc.clone());
            s.stdin = Some(stdin);
            s.ready = false;
        }
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let reader = gio::DataInputStream::new(&stdout);
            while let Ok(Some(line)) = reader.read_line_utf8_future(glib::Priority::DEFAULT).await {
                let Some(this) = weak.upgrade() else { return };
                if let Some(msg) = parse_line(&line) {
                    this.handle(msg);
                }
            }
            let _ = proc.wait_future().await;
            if let Some(this) = weak.upgrade() {
                this.exited();
            }
        });
    }

    fn handle(&self, msg: Incoming) {
        match msg {
            Incoming::Reply(reply) => {
                let done = self.state.borrow_mut().pending.remove(&reply.id);
                if let Some(done) = done {
                    done(reply);
                }
            }
            Incoming::Ready { me } => {
                let (stdin, queued) = {
                    let mut s = self.state.borrow_mut();
                    s.ready = true;
                    s.failures = 0;
                    (s.stdin.clone(), std::mem::take(&mut s.queued))
                };
                log::info!("Google Messages link: connected");
                if let Some(stdin) = stdin {
                    for line in queued {
                        write_line(&stdin, &line);
                    }
                }
                (self.on_event)(Incoming::Ready { me });
            }
            Incoming::Fatal { kind, message } => {
                log::warn!("Google Messages link: {kind}: {message}");
                if kind == "logged_out" {
                    self.state.borrow_mut().stopped = true;
                }
                (self.on_event)(Incoming::Fatal { kind, message });
            }
            other => (self.on_event)(other),
        }
    }

    fn exited(self: &Rc<Self>) {
        let pending = {
            let mut s = self.state.borrow_mut();
            s.proc = None;
            s.stdin = None;
            s.queued.clear();
            if !s.ready {
                s.failures += 1;
            }
            s.ready = false;
            std::mem::take(&mut s.pending)
        };
        for (id, done) in pending {
            done(Reply { id, message: "the Google Messages link stopped".into(), ..Default::default() });
        }
        self.schedule_restart();
    }

    fn schedule_restart(self: &Rc<Self>) {
        let mut s = self.state.borrow_mut();
        if s.stopped || !phone_delete::available() {
            return;
        }
        let secs = 1u64 << s.failures.min(10);
        let delay = Duration::from_secs(secs.min(600));
        log::info!("Google Messages link: restarting in {}s", delay.as_secs());
        let weak = Rc::downgrade(self);
        s.restart = Some(glib::timeout_add_local_once(delay, move || {
            if let Some(this) = weak.upgrade() {
                this.state.borrow_mut().restart = None;
                this.spawn();
            }
        }));
    }
}

/// Lines are small and the helper reads all the time, so a blocking write
/// on the pipe is fine (and keeps requests in order).
fn write_line(stdin: &gio::OutputStream, line: &str) {
    let mut bytes = line.as_bytes().to_vec();
    bytes.push(b'\n');
    if let Err(e) = stdin.write_all(&bytes, gio::Cancellable::NONE) {
        log::warn!("Google Messages link: write failed: {e}");
    }
}
