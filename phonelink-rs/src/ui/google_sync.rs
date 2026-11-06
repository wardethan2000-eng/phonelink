//! Google Contacts sync (port of the Google parts of `phonelink/ui/sms_panel.py`):
//! Connect / Refresh Now / Disconnect from Settings, "Import Google
//! Contacts" from Messages, a background refresh at most once a day, and
//! pushing renames to Google. The network work runs off the main thread.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::{Rc, Weak};

use adw::prelude::*;

use super::messages_page::MessagesPage;
use crate::core::contacts::contact_photos_dir;
use crate::core::google::{self, GoogleError, ImportResult, SignIn, UreqHttp};
use crate::core::settings::{Settings, home_dir};

/// What Settings shows.
pub struct Status {
    pub configured: bool,
    pub connected: bool,
    pub in_flight: bool,
    pub account_label: String,
    pub last_sync_ts: f64,
    pub config_path: String,
}

pub struct GoogleSync {
    settings: Rc<RefCell<Settings>>,
    messages: RefCell<Weak<MessagesPage>>,
    window: glib::WeakRef<gtk::Window>,
    toast: Rc<dyn Fn(&str)>,
    in_flight: Cell<bool>,
    listeners: RefCell<Vec<Rc<dyn Fn()>>>,
}

fn epoch_now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

fn http() -> Result<UreqHttp, GoogleError> {
    UreqHttp::new().map_err(GoogleError::Other)
}

/// The blocking half of a sync, given a usable access token.
fn import(token: &str, photo_numbers: &HashSet<String>) -> Result<ImportResult, GoogleError> {
    google::import_contacts(&http()?, token, photo_numbers, &contact_photos_dir(&home_dir()))
}

impl GoogleSync {
    pub fn new(settings: Rc<RefCell<Settings>>, window: &gtk::Window, toast: Rc<dyn Fn(&str)>) -> Rc<Self> {
        Rc::new(Self {
            settings,
            messages: RefCell::default(),
            window: window.downgrade(),
            toast,
            in_flight: Cell::new(false),
            listeners: RefCell::default(),
        })
    }

    pub fn set_messages(&self, messages: &Rc<MessagesPage>) {
        *self.messages.borrow_mut() = Rc::downgrade(messages);
    }

    /// Call `f` whenever the status changes.
    pub fn connect_changed(&self, f: impl Fn() + 'static) {
        self.listeners.borrow_mut().push(Rc::new(f));
    }

    fn changed(&self) {
        let ls = self.listeners.borrow().clone();
        for l in ls {
            l();
        }
    }

    pub fn is_connected(&self) -> bool {
        google::token_file(&home_dir()).is_file()
    }

    pub fn status(&self) -> Status {
        let home = home_dir();
        let s = self.settings.borrow();
        Status {
            configured: google::load_client_config_default(&home).is_ok(),
            connected: self.is_connected(),
            in_flight: self.in_flight.get(),
            account_label: s.google_account_label().trim().to_string(),
            last_sync_ts: s.google_last_sync_ts(),
            config_path: google::client_file(&home).to_string_lossy().into_owned(),
        }
    }

    /// Settings "Connect" / "Refresh Now", and Messages "Import Google Contacts".
    pub fn sync_now(self: &Rc<Self>) {
        self.start(true);
    }

    /// On (re)connect: only with a saved sign-in, the switch on, and the
    /// last attempt over a day ago.
    pub fn maybe_background(self: &Rc<Self>) {
        let due = {
            let s = self.settings.borrow();
            s.google_background_sync() && epoch_now() - s.google_last_attempt_ts() >= google::BACKGROUND_SYNC_SECONDS
        };
        if due && self.is_connected() {
            self.start(false);
        }
    }

    pub fn disconnect(&self) {
        let removed = google::disconnect(&google::token_file(&home_dir()));
        {
            let mut s = self.settings.borrow_mut();
            let _ = s.batch(|s| {
                let _ = s.set_google_background_sync(false);
                let _ = s.clear_google_account();
            });
        }
        self.changed();
        if removed {
            (self.toast)("Google Contacts disconnected");
        }
    }

    fn start(self: &Rc<Self>, interactive: bool) {
        if self.in_flight.replace(true) {
            return;
        }
        let _ = self.settings.borrow_mut().set_google_last_attempt_ts(epoch_now());
        self.changed();
        if interactive {
            (self.toast)("Opening Google Contacts import in your browser…");
        }
        let numbers = self.messages.borrow().upgrade().map(|m| m.chat_numbers()).unwrap_or_default();
        let this = self.clone();
        glib::spawn_future_local(async move {
            let result = this.run(interactive, numbers).await;
            this.finish(result, interactive);
        });
    }

    async fn run(&self, interactive: bool, numbers: HashSet<String>) -> Result<ImportResult, GoogleError> {
        let home = home_dir();
        let token_path = google::token_file(&home);
        let cfg = google::load_client_config_default(&home);
        let (cfg2, path2) = (cfg.clone().ok(), token_path.clone());
        let saved = gio::spawn_blocking(move || google::saved_credentials(&http()?, cfg2.as_ref(), &path2, chrono::Utc::now()))
            .await
            .map_err(|_| GoogleError::Other("The Google sync stopped unexpectedly.".into()))?;
        let token = match saved {
            Ok(t) => t,
            Err(GoogleError::AuthRequired(_)) if interactive => {
                // Sign in through the browser (loopback on 127.0.0.1).
                let sign_in = SignIn::start(&cfg?)?;
                if let Err(e) = gio::AppInfo::launch_default_for_uri(&sign_in.url, gio::AppLaunchContext::NONE) {
                    return Err(GoogleError::Other(format!("Couldn't open your browser: {e}")));
                }
                gio::spawn_blocking(move || sign_in.finish(&http()?, &token_path, google::SIGN_IN_TIMEOUT))
                    .await
                    .map_err(|_| GoogleError::Other("The Google sign-in stopped unexpectedly.".into()))??
            }
            Err(e) => return Err(e),
        };
        let access = token.access_token().to_string();
        gio::spawn_blocking(move || import(&access, &numbers))
            .await
            .map_err(|_| GoogleError::Other("The Google sync stopped unexpectedly.".into()))?
    }

    fn finish(&self, result: Result<ImportResult, GoogleError>, interactive: bool) {
        self.in_flight.set(false);
        let r = match result {
            Ok(r) => r,
            Err(e) => {
                if matches!(e, GoogleError::AuthRequired(_)) {
                    let _ = self.settings.borrow_mut().clear_google_account();
                }
                self.changed();
                if interactive {
                    self.alert(e.heading(), e.message());
                } else {
                    log::info!("google background sync: {}", e.message());
                }
                return;
            }
        };
        {
            let mut s = self.settings.borrow_mut();
            let _ = s.batch(|s| {
                let _ = s.set_google_account_label(&r.account_label);
                let _ = s.set_google_last_sync_ts(epoch_now());
            });
        }
        let changed = self.messages.borrow().upgrade().map_or(0, |m| m.learn_contacts(r.contacts.clone()));
        self.changed();
        if interactive {
            self.alert(
                "Google Contacts Imported",
                &format!(
                    "Imported {changed} contact mappings from {}.\n\nUpdated {} contact photos.\nGoogle returned {} people in total.",
                    r.account_label, r.photos_changed, r.seen_people
                ),
            );
            (self.toast)("Google Contacts import finished");
        } else if changed > 0 || r.photos_changed > 0 {
            (self.toast)("Google Contacts refreshed in the background");
        }
    }

    /// A contact renamed in PhoneRelay: update (or create) it in Google, so
    /// the phone gets it; without an account, say it stayed local.
    pub fn push_rename(&self, address: &str, name: &str) {
        if !self.is_connected() {
            (self.toast)(&format!("Saved {name} in PhoneRelay only; connect Google Contacts in Settings to update your phone too"));
            return;
        }
        let label = Some(self.settings.borrow().google_account_label().trim().to_string())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| "Google account".into());
        let (address, name, toast) = (address.to_string(), name.to_string(), self.toast.clone());
        glib::spawn_future_local(async move {
            let n = name.clone();
            let done = gio::spawn_blocking(move || {
                let http = http()?;
                let token = google::saved_credentials(&http, None, &google::token_file(&home_dir()), chrono::Utc::now())?;
                google::upsert_contact(&http, token.access_token(), &address, &n)
            })
            .await;
            match done {
                Ok(Ok(google::Upsert::Updated)) => toast(&format!("{name} updated in {label}")),
                Ok(Ok(google::Upsert::Created)) => toast(&format!("{name} created in {label}")),
                Ok(Err(e)) => {
                    log::warn!("google rename: {}", e.message());
                    toast(&format!("Saved {name} locally; reconnect Google Contacts in Settings if you want cloud updates"));
                }
                Err(_) => {}
            }
        });
    }

    fn alert(&self, heading: &str, body: &str) {
        let dialog = adw::MessageDialog::new(self.window.upgrade().as_ref(), Some(heading), Some(body));
        dialog.add_response("ok", "OK");
        dialog.present();
    }
}
