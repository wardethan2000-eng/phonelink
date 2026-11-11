//! Runs `phonelink-gm serve` (the lasting Google Messages link), or the
//! messages helper chosen in its place, as a child process and talks to it
//! in JSON lines (`core::gm`, docs/HELPER_PROTOCOL.md).
//!
//! Started only when the helper and a link (Settings → Google Messages) exist,
//! or another helper was chosen. Requests for what the helper's hello
//! didn't list are answered here with `unsupported`. If it exits it
//! is restarted, waiting longer each time (1 s, doubling, at most 10 min);
//! a signed-out account stops it until the app restarts. When the app
//! exits, the helper's stdin closes and it quits.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::rc::Rc;
use std::time::Duration;

use gio::prelude::*;

use crate::core::gm::{HelperInfo, Incoming, PROTOCOL, Reply, parse_line};
use crate::core::phone_delete;

type Done = Box<dyn FnOnce(Reply)>;

#[derive(Default)]
struct State {
    proc: Option<gio::Subprocess>,
    stdin: Option<gio::OutputStream>,
    ready: bool,
    next_id: u64,
    pending: HashMap<u64, Done>,
    /// Requests written once the link is ready: (id, line).
    queued: Vec<(u64, String)>,
    /// The running helper's hello (or [`HelperInfo::without_hello`] once
    /// it is ready without one).
    info: Option<HelperInfo>,
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

    /// What the running helper said about itself, once it has.
    pub fn info(&self) -> Option<HelperInfo> {
        self.state.borrow().info.clone()
    }

    /// Does the helper handle `what` (an op, or `reply`)? Yes until it
    /// has said otherwise.
    pub fn supports(&self, what: &str) -> bool {
        self.state.borrow().info.as_ref().is_none_or(|i| i.supports(what))
    }

    /// Is the helper running (connected or still connecting)?
    pub fn is_running(&self) -> bool {
        self.state.borrow().proc.is_some()
    }

    /// Stop the helper for good (unlinking): it isn't restarted, and its
    /// exit isn't reported. `done` runs once it has exited (it then no
    /// longer holds the session).
    pub fn stop(self: &Rc<Self>, done: impl FnOnce() + 'static) {
        let (proc, stdin) = {
            let mut s = self.state.borrow_mut();
            s.stopped = true;
            if let Some(id) = s.restart.take() {
                id.remove();
            }
            (s.proc.clone(), s.stdin.take())
        };
        let Some(proc) = proc else {
            done();
            return;
        };
        // Closing stdin makes the helper disconnect and exit.
        if let Some(stdin) = stdin {
            let _ = stdin.close(gio::Cancellable::NONE);
        }
        // Still there after 10 s: kill it.
        let killer: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
        let id = glib::timeout_add_local_once(Duration::from_secs(10), {
            let (proc, killer) = (proc.clone(), killer.clone());
            move || {
                killer.borrow_mut().take();
                proc.force_exit();
            }
        });
        *killer.borrow_mut() = Some(id);
        glib::spawn_future_local(async move {
            let _ = proc.wait_future().await;
            if let Some(id) = killer.borrow_mut().take() {
                id.remove();
            }
            done();
        });
    }

    /// Start again after [`Self::stop`] (linked again).
    pub fn resume(self: &Rc<Self>) {
        {
            let mut s = self.state.borrow_mut();
            s.stopped = false;
            s.failures = 0;
            if s.proc.is_some() {
                return;
            }
        }
        self.spawn();
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
                s.queued.push((id, line));
                return true;
            }
            if let Some(what) = s.info.as_ref().and_then(|i| i.refuses(&line)) {
                drop(s);
                self.refuse(id, &what);
                return true;
            }
            (line, s.stdin.clone())
        };
        if let Some(stdin) = write_now {
            write_line(&stdin, &line);
        }
        true
    }

    /// Answer request `id` with `unsupported` (later, as a reply would come).
    fn refuse(&self, id: u64, what: &str) {
        let Some(done) = self.state.borrow_mut().pending.remove(&id) else { return };
        let title = self.state.borrow().info.as_ref().map(|i| i.title().to_string()).unwrap_or_default();
        log::info!("messages helper: {title} doesn't do {what}");
        let reply = Reply {
            id,
            error: "unsupported".into(),
            message: format!("{title} can't do that ({what})"),
            ..Default::default()
        };
        glib::idle_add_local_once(move || done(reply));
    }

    fn spawn(self: &Rc<Self>) {
        let Some(helper) = phone_delete::serve_helper() else {
            return;
        };
        // phonelink-gm gets its connect timeout; another helper just `serve`.
        let mut args: Vec<&OsStr> = vec![helper.as_os_str(), OsStr::new("serve")];
        if phone_delete::custom_helper().is_none() {
            args.push(OsStr::new("--timeout=90s"));
        }
        let launcher =
            gio::SubprocessLauncher::new(gio::SubprocessFlags::STDIN_PIPE | gio::SubprocessFlags::STDOUT_PIPE);
        launcher.setenv("PHONERELAY_PROTOCOL", PROTOCOL.to_string(), true);
        let proc = match launcher.spawn(&args) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("Google Messages link: couldn't start the helper: {e}");
                self.schedule_restart();
                return;
            }
        };
        let (Some(stdin), Some(stdout)) = (proc.stdin_pipe(), proc.stdout_pipe()) else {
            return;
        };
        {
            let mut s = self.state.borrow_mut();
            s.proc = Some(proc.clone());
            s.stdin = Some(stdin);
            s.ready = false;
            s.info = None;
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
            Incoming::Hello(info) => {
                log::info!(
                    "messages helper: {} {} (protocol {}), supports {:?}",
                    info.name,
                    info.version,
                    info.protocol,
                    info.supports
                );
                if info.protocol > PROTOCOL {
                    // Too new to talk to: stop it for good.
                    let proc = {
                        let mut s = self.state.borrow_mut();
                        s.stopped = true;
                        s.proc.clone()
                    };
                    if let Some(proc) = proc {
                        proc.force_exit();
                    }
                    let message = format!("{} needs a newer PhoneRelay (protocol {})", info.title(), info.protocol);
                    (self.on_event)(Incoming::Fatal { kind: "protocol".into(), message });
                    return;
                }
                self.state.borrow_mut().info = Some(info.clone());
                (self.on_event)(Incoming::Hello(info));
            }
            Incoming::Ready { me } => {
                let (stdin, queued, said_hello) = {
                    let mut s = self.state.borrow_mut();
                    s.ready = true;
                    s.failures = 0;
                    let said_hello = s.info.is_some();
                    let info = s.info.get_or_insert_with(HelperInfo::without_hello).clone();
                    let (refused, queued): (Vec<_>, Vec<_>) =
                        std::mem::take(&mut s.queued).into_iter().partition(|(_, line)| info.refuses(line).is_some());
                    drop(s);
                    for (id, line) in refused {
                        self.refuse(id, &info.refuses(&line).unwrap_or_default());
                    }
                    (self.state.borrow().stdin.clone(), queued, said_hello)
                };
                log::info!("messages helper: connected");
                if !said_hello {
                    (self.on_event)(Incoming::Hello(HelperInfo::without_hello()));
                }
                if let Some(stdin) = stdin {
                    for (_, line) in queued {
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
        let stopped = self.state.borrow().stopped;
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
        if !stopped {
            (self.on_event)(Incoming::Fatal { kind: "exited".into(), message: "the helper stopped".into() });
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
