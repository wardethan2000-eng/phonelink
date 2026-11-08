//! Deleting chats on the phone through Google Messages.
//!
//! KDE Connect only reads texts, and only the default SMS app may delete
//! them, so PhoneRelay runs the `phonelink-gm` helper (Go, libgm), which
//! deletes through the Google Messages web link (linked in Settings). The
//! helper is matched by the chat's members; it refuses a chat it can't pin
//! to exactly one Google Messages conversation.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use super::settings::home_dir;

/// Where the helper lives: `$PHONELINK_GM`, next to this binary, or
/// `~/.local/bin`.
pub fn helper_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PHONELINK_GM").map(PathBuf::from) {
        return p.is_file().then_some(p);
    }
    let beside = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("phonelink-gm")));
    let local = home_dir().join(".local").join("bin").join("phonelink-gm");
    beside.into_iter().chain([local]).find(|p| p.is_file())
}

/// PhoneRelay's Google Messages session (the helper's `ownSession`).
pub fn session_path(home: &Path) -> PathBuf {
    let data =
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local").join("share"));
    data.join("phonelink").join("gm").join("session.json")
}

/// A pairing gmcli made, which the helper copies in once.
fn gmcli_session_path(home: &Path) -> PathBuf {
    let state =
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local").join("state"));
    state.join("gmcli").join("session.json")
}

/// Is Google Messages linked on this computer? (Ours, or gmcli's not yet
/// copied in; after an unlink gmcli's no longer counts.)
pub fn linked(home: &Path) -> bool {
    let own = session_path(home);
    if own.is_file() {
        return true;
    }
    let copied = own.parent().is_some_and(|d| d.join(".from-gmcli").exists());
    !copied && gmcli_session_path(home).is_file()
}

/// Can the helper run? (No network: helper + a link exist.)
pub fn available() -> bool {
    helper_path().is_some() && linked(&home_dir())
}

/// One chat's outcome, in the order asked.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Deleted,
    /// Not in Google Messages, or more than one chat with those members.
    NotMatched,
    Failed(String),
}

/// Parse the helper's JSON. `Err` = the whole run failed (not paired,
/// couldn't connect...), with a message for the user.
pub fn parse_output(stdout: &str, chats: usize) -> Result<Vec<Outcome>, String> {
    let v: Value =
        serde_json::from_str(stdout.trim()).map_err(|_| "the Google Messages helper gave no answer".to_string())?;
    let Some(results) = v.get("results").and_then(Value::as_array) else {
        let msg = v.get("message").and_then(Value::as_str).unwrap_or("unknown error");
        return Err(match v.get("error").and_then(Value::as_str) {
            Some("not_paired") => "Google Messages isn't paired on this computer".to_string(),
            Some("connect_failed") => format!("couldn't reach Google Messages ({msg})"),
            _ => msg.to_string(),
        });
    };
    let mut out: Vec<Outcome> = results
        .iter()
        .map(|r| {
            if r.get("ok").and_then(Value::as_bool) == Some(true) {
                return Outcome::Deleted;
            }
            match r.get("error").and_then(Value::as_str) {
                Some("not_found") | Some("ambiguous") => Outcome::NotMatched,
                _ => Outcome::Failed(r.get("message").and_then(Value::as_str).unwrap_or("delete failed").to_string()),
            }
        })
        .collect();
    out.resize(chats, Outcome::Failed("no answer for this chat".into()));
    Ok(out)
}

/// The helper's argument for one chat: its members, comma-separated.
pub fn chat_arg(addresses: &[String]) -> String {
    addresses.iter().map(|a| a.trim()).filter(|a| !a.is_empty()).collect::<Vec<_>>().join(",")
}

/// The helper's arguments. `--` ends the options: a chat key is the
/// sender's address, and an e-mail sender like "-session=…@x.com" must not
/// be read as an option.
fn delete_args(chats: &[Vec<String>]) -> Vec<String> {
    let mut args = vec!["delete".to_string(), "--timeout=90s".to_string(), "--".to_string()];
    args.extend(chats.iter().map(|c| chat_arg(c)));
    args
}

/// Delete chats on the phone (blocking; run it off the main thread). Each
/// chat is its members' addresses.
pub fn delete_chats(chats: &[Vec<String>]) -> Result<Vec<Outcome>, String> {
    let helper = helper_path().ok_or("the Google Messages helper (phonelink-gm) isn't installed")?;
    let out = Command::new(helper)
        .args(delete_args(chats))
        .output()
        .map_err(|e| format!("couldn't run the Google Messages helper: {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !stderr.trim().is_empty() {
        log::warn!("phonelink-gm: {}", stderr.trim());
    }
    parse_output(&String::from_utf8_lossy(&out.stdout), chats.len())
}

/// What to tell the user afterwards.
pub fn summary(result: &Result<Vec<Outcome>, String>) -> String {
    let outcomes = match result {
        Err(e) => return format!("Removed from PhoneRelay only: {e}"),
        Ok(o) => o,
    };
    let total = outcomes.len();
    let deleted = outcomes.iter().filter(|o| **o == Outcome::Deleted).count();
    let unmatched = outcomes.iter().filter(|o| **o == Outcome::NotMatched).count();
    let failed = total - deleted - unmatched;
    if deleted == total {
        return if total == 1 {
            "Deleted on your phone too".into()
        } else {
            format!("Deleted all {total} on your phone too")
        };
    }
    let mut parts = Vec::new();
    if deleted > 0 {
        parts.push(format!("{deleted} deleted on your phone"));
    }
    if unmatched > 0 {
        parts.push(format!("{unmatched} not found in Google Messages"));
    }
    if failed > 0 {
        let why = outcomes
            .iter()
            .find_map(|o| if let Outcome::Failed(m) = o { Some(m.as_str()) } else { None })
            .unwrap_or("");
        parts.push(format!("{failed} failed ({why})"));
    }
    let tail = if total == 1 { "removed from PhoneRelay only" } else { "the rest removed from PhoneRelay only" };
    let head = if total == 1 && unmatched == 1 { "Not found in Google Messages".to_string() } else { parts.join(", ") };
    format!("{head}; {tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_per_chat_results() {
        let out = r#"{"ok":false,"results":[{"chat":"1","ok":true},{"chat":"2","ok":false,"error":"ambiguous"},{"chat":"3","ok":false,"error":"delete_failed","message":"boom"}]}"#;
        assert_eq!(
            parse_output(out, 3),
            Ok(vec![Outcome::Deleted, Outcome::NotMatched, Outcome::Failed("boom".into())])
        );
    }

    #[test]
    fn a_failed_run_is_one_error() {
        let out = r#"{"ok":false,"error":"connect_failed","message":"the phone did not answer in 1m30s"}"#;
        assert_eq!(
            parse_output(out, 2),
            Err("couldn't reach Google Messages (the phone did not answer in 1m30s)".into())
        );
        assert!(parse_output("", 1).is_err());
    }

    #[test]
    fn summaries_say_what_happened_on_the_phone() {
        assert_eq!(summary(&Ok(vec![Outcome::Deleted])), "Deleted on your phone too");
        assert_eq!(summary(&Ok(vec![Outcome::Deleted, Outcome::Deleted])), "Deleted all 2 on your phone too");
        assert_eq!(
            summary(&Ok(vec![Outcome::NotMatched])),
            "Not found in Google Messages; removed from PhoneRelay only"
        );
        assert_eq!(
            summary(&Ok(vec![Outcome::Deleted, Outcome::NotMatched, Outcome::Failed("x".into())])),
            "1 deleted on your phone, 1 not found in Google Messages, 1 failed (x); the rest removed from PhoneRelay only"
        );
        assert_eq!(summary(&Err("boom".into())), "Removed from PhoneRelay only: boom");
    }

    #[test]
    fn chat_keys_can_never_be_options() {
        let args = delete_args(&[vec!["-session=/tmp/x@evil.com".into()], vec!["+1316".into()]]);
        assert_eq!(args, ["delete", "--timeout=90s", "--", "-session=/tmp/x@evil.com", "+1316"]);
    }

    #[test]
    fn chat_arg_joins_members() {
        assert_eq!(chat_arg(&["+1316".into(), " ".into(), "+1317".into()]), "+1316,+1317");
    }
}
