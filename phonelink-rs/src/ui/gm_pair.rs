//! Linking Google Messages from Settings (`phonelink-gm pair`) and
//! unlinking it (`phonelink-gm unpair`).
//!
//! Linking: the helper opens a sign-in window in a throwaway browser
//! profile; once signed in, the phone shows emoji and this dialog shows the
//! one to tap. The helper prints each step as a JSON line (`pair.go`).

use std::cell::RefCell;
use std::ffi::OsStr;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use serde_json::Value;

use crate::core::phone_delete;

/// One step `phonelink-gm pair` or `unpair` reports.
#[derive(Debug, Clone, PartialEq)]
pub enum PairStep {
    /// The sign-in window opened.
    Browser,
    /// Signed in; checking with Google Messages.
    Checking,
    /// Tap this emoji on the phone.
    Emoji(String),
    Paired,
    /// Unlinked (`phone`: the phone was told too).
    Unpaired {
        phone: bool,
    },
    Failed {
        kind: String,
        message: String,
    },
}

pub fn parse_step(line: &str) -> Option<PairStep> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    Some(match v.get("event")?.as_str()? {
        "browser" => PairStep::Browser,
        "checking" => PairStep::Checking,
        "emoji" => PairStep::Emoji(s("emoji")),
        "paired" => PairStep::Paired,
        "unpaired" => PairStep::Unpaired { phone: v.get("phone").and_then(Value::as_bool).unwrap_or(false) },
        "error" => PairStep::Failed { kind: s("error"), message: s("message") },
        _ => return None,
    })
}

/// What to tell the user when linking stopped.
pub fn failure_text(kind: &str) -> &'static str {
    match kind {
        "no_browser" => "Linking needs Chrome, Chromium, Brave, Edge or Vivaldi installed. Install one and try again.",
        "no_display" => "Linking needs a desktop session.",
        "cancelled" => "Linking was cancelled.",
        "wrong_emoji" => "A different emoji was tapped on the phone. Try again.",
        "timeout" => "That took too long. Try again.",
        "busy" => "Google Messages is still busy with the old link. Wait a moment and try again.",
        _ => "Linking didn't work. Check the phone is online and try again.",
    }
}

/// Run the helper with `args`, calling `on_step` for each step, then
/// `on_exit` with whether it exited 0. Returns the process (to cancel).
fn run_helper(
    args: &[&str],
    on_step: impl Fn(PairStep) + 'static,
    on_exit: impl FnOnce(bool) + 'static,
) -> Option<gio::Subprocess> {
    let helper = phone_delete::helper_path()?;
    let mut argv: Vec<&OsStr> = vec![helper.as_os_str()];
    argv.extend(args.iter().map(OsStr::new));
    let proc = match gio::Subprocess::newv(&argv, gio::SubprocessFlags::STDOUT_PIPE) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("Google Messages: couldn't run the helper: {e}");
            return None;
        }
    };
    let stdout = proc.stdout_pipe()?;
    let p = proc.clone();
    glib::spawn_future_local(async move {
        let reader = gio::DataInputStream::new(&stdout);
        while let Ok(Some(line)) = reader.read_line_utf8_future(glib::Priority::DEFAULT).await {
            if let Some(step) = parse_step(&line) {
                on_step(step);
            }
        }
        let ok = p.wait_future().await.is_ok() && p.has_exited() && p.exit_status() == 0;
        on_exit(ok);
    });
    Some(proc)
}

/// Unlink: tell the phone and delete the session. `done(ok, phone_told)`.
pub fn unpair(done: impl FnOnce(bool, bool) + 'static) {
    let told = Rc::new(RefCell::new(false));
    let t = told.clone();
    let done = Rc::new(RefCell::new(Some(done)));
    let d = done.clone();
    let started = run_helper(
        &["unpair"],
        move |step| {
            if let PairStep::Unpaired { phone } = step {
                *t.borrow_mut() = phone;
            }
        },
        move |ok| {
            if let Some(f) = d.borrow_mut().take() {
                f(ok, *told.borrow());
            }
        },
    );
    if started.is_none()
        && let Some(f) = done.borrow_mut().take()
    {
        f(false, false);
    }
}

/// The linking dialog. `on_linked` runs once the session is saved.
pub fn link_dialog(parent: Option<&gtk::Window>, on_linked: impl Fn() + 'static) {
    let window = adw::Window::new();
    window.set_title(Some("Link Google Messages"));
    window.set_modal(true);
    window.set_default_size(440, 420);
    window.set_transient_for(parent);
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    column.append(&header);
    let status = adw::StatusPage::new();
    status.set_vexpand(true);
    status.set_icon_name(Some("phone-symbolic"));
    status.set_title("Link Google Messages");
    status.set_description(Some(
        "On your phone, open Google Messages → your profile picture → Device pairing, and check which Google account it uses.\n\nThen sign in with that account in the window that opens.",
    ));
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    buttons.set_halign(gtk::Align::Center);
    let start = gtk::Button::with_label("Sign in");
    start.add_css_class("suggested-action");
    start.add_css_class("pill");
    buttons.append(&start);
    let close = gtk::Button::with_label("Cancel");
    close.add_css_class("pill");
    buttons.append(&close);
    status.set_child(Some(&buttons));
    column.append(&status);
    window.set_content(Some(&column));

    let proc: Rc<RefCell<Option<gio::Subprocess>>> = Rc::default();
    let linked = Rc::new(std::cell::Cell::new(false));
    let on_linked = Rc::new(on_linked);
    {
        let (w, proc) = (window.clone(), proc.clone());
        close.connect_clicked(move |_| {
            if let Some(p) = proc.borrow_mut().take() {
                p.force_exit();
            }
            w.close();
        });
    }
    {
        let proc = proc.clone();
        window.connect_close_request(move |_| {
            if let Some(p) = proc.borrow_mut().take() {
                p.force_exit();
            }
            glib::Propagation::Proceed
        });
    }
    let (status_c, start_c, close_c, proc_c, linked_c) =
        (status.clone(), start.clone(), close.clone(), proc.clone(), linked.clone());
    start.connect_clicked(move |_| {
        start_c.set_visible(false);
        status_c.set_icon_name(Some(&super::icons::resolve("web-browser-symbolic")));
        status_c.set_title("Opening the sign-in window…");
        status_c.set_description(None);
        let (st, st2, cl, lk, lk2, on_linked, again) =
            (status_c.clone(), status_c.clone(), close_c.clone(), linked_c.clone(), linked_c.clone(), on_linked.clone(), start_c.clone());
        let p = run_helper(
            &["pair"],
            move |step| match step {
                PairStep::Browser => {
                    st.set_title("Sign in to Google");
                    st.set_description(Some(
                        "Sign in in the window that opened, with the Google account Google Messages uses on your phone. It closes by itself when done.",
                    ));
                }
                PairStep::Checking => {
                    st.set_icon_name(Some("phone-symbolic"));
                    st.set_title("Checking with your phone…");
                    st.set_description(None);
                }
                PairStep::Emoji(e) => {
                    st.set_icon_name(None);
                    st.set_title(&e);
                    st.add_css_class("pair-emoji");
                    st.set_description(Some("On your phone, Google Messages shows some emoji. Tap this one."));
                }
                PairStep::Paired => {
                    lk.set(true);
                    st.remove_css_class("pair-emoji");
                    st.set_icon_name(Some("object-select-symbolic"));
                    st.set_title("Linked");
                    st.set_description(Some("Your texts now go through Google Messages, as RCS where the chat is RCS."));
                    cl.set_label("Done");
                    cl.add_css_class("suggested-action");
                }
                PairStep::Failed { kind, message } => {
                    log::warn!("Google Messages linking: {kind}: {message}");
                    st.remove_css_class("pair-emoji");
                    st.set_icon_name(Some(&super::icons::resolve("dialog-warning-symbolic")));
                    st.set_title("Not linked");
                    st.set_description(Some(failure_text(&kind)));
                }
                PairStep::Unpaired { .. } => {}
            },
            move |ok| {
                if ok && lk2.get() {
                    on_linked();
                } else {
                    if st2.title() != "Not linked" {
                        st2.remove_css_class("pair-emoji");
                        st2.set_icon_name(Some(&super::icons::resolve("dialog-warning-symbolic")));
                        st2.set_title("Not linked");
                        st2.set_description(Some(failure_text("")));
                    }
                    again.set_label("Try again");
                    again.set_visible(true);
                }
            },
        );
        if p.is_none() {
            status_c.set_title("Not linked");
            status_c.set_description(Some("The Google Messages helper (phonelink-gm) isn't installed."));
        }
        *proc_c.borrow_mut() = p;
    });
    window.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_helpers_steps() {
        assert_eq!(parse_step(r#"{"event":"browser"}"#), Some(PairStep::Browser));
        assert_eq!(parse_step(r#"{"event":"emoji","emoji":"🦊"}"#), Some(PairStep::Emoji("🦊".into())));
        assert_eq!(parse_step(r#"{"event":"paired","account":"x"}"#), Some(PairStep::Paired));
        assert_eq!(parse_step(r#"{"event":"unpaired","phone":true}"#), Some(PairStep::Unpaired { phone: true }));
        assert_eq!(
            parse_step(r#"{"event":"error","error":"wrong_emoji","message":"m"}"#),
            Some(PairStep::Failed { kind: "wrong_emoji".into(), message: "m".into() })
        );
        assert_eq!(parse_step("not json"), None);
        assert!(failure_text("no_browser").contains("Chromium"));
    }
}
