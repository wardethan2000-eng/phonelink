//! Google Contacts (port of `phonelink/google_contacts.py`): the OAuth
//! desktop sign-in (loopback on 127.0.0.1), the saved token, the People API
//! import with photos for chat contacts, and the write-back of a rename.
//!
//! The client config and token file are the Python app's, in the same
//! format (google-auth's `Credentials.to_json`), so switching apps needs no
//! new sign-in. All network calls block; the UI runs them off the main
//! thread. HTTP goes through [`Http`] so the logic is tested without a
//! network.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::{Map, Value, json};

use super::contacts::{delete_contact_photo_in, normalize_phone, phone_key, store_contact_photo_in};
use super::settings::{atomic_write_text, data_dir};

pub const SCOPE: &str = "https://www.googleapis.com/auth/contacts";
const PEOPLE: &str = "https://people.googleapis.com/v1";
const AUTH_URI: &str = "https://accounts.google.com/o/oauth2/auth";
const TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
/// google-auth refreshes this long before the expiry.
const REFRESH_EARLY_SECONDS: i64 = 225;
pub const BACKGROUND_SYNC_SECONDS: f64 = 24.0 * 60.0 * 60.0;
/// How long the browser sign-in may take.
pub const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq)]
pub enum GoogleError {
    /// No OAuth client configured.
    Config(String),
    /// The saved sign-in can't be used silently; reconnect from Settings.
    AuthRequired(String),
    Other(String),
}

impl GoogleError {
    pub fn message(&self) -> &str {
        match self {
            GoogleError::Config(m) | GoogleError::AuthRequired(m) | GoogleError::Other(m) => m,
        }
    }

    /// The dialog heading for an interactive failure, as Python.
    pub fn heading(&self) -> &'static str {
        match self {
            GoogleError::Config(_) => "Google Contacts Not Configured",
            GoogleError::AuthRequired(_) => "Google Contacts Reconnection Needed",
            GoogleError::Other(_) => "Google Contacts Import Failed",
        }
    }
}

type Result<T> = std::result::Result<T, GoogleError>;

fn other(e: impl std::fmt::Display) -> GoogleError {
    GoogleError::Other(e.to_string())
}

// ── files ──────────────────────────────────────────────────────────

pub fn client_file(home: &Path) -> PathBuf {
    home.join(".config").join("phonelink").join("google_oauth.json")
}

pub fn token_file(home: &Path) -> PathBuf {
    data_dir(home).join("google_token.json")
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClientConfig {
    pub client_id: String,
    pub client_secret: String,
    pub auth_uri: String,
    pub token_uri: String,
}

/// `PHONELINK_GOOGLE_CLIENT_ID` / `_SECRET`, else the client file (an
/// "installed" or "web" client as Google's console downloads it).
pub fn load_client_config(env_id: Option<&str>, env_secret: Option<&str>, file: &Path) -> Result<ClientConfig> {
    if let Some(id) = env_id.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(ClientConfig {
            client_id: id.to_string(),
            client_secret: env_secret.unwrap_or_default().trim().to_string(),
            auth_uri: AUTH_URI.into(),
            token_uri: TOKEN_URI.into(),
        });
    }
    if !file.is_file() {
        return Err(GoogleError::Config(format!(
            "Google Contacts import needs an OAuth desktop client configuration. Set \
             PHONELINK_GOOGLE_CLIENT_ID/PHONELINK_GOOGLE_CLIENT_SECRET or add {}.",
            file.display()
        )));
    }
    let invalid = || GoogleError::Config(format!("Invalid Google OAuth config at {}", file.display()));
    let v: Value = std::fs::read_to_string(file).ok().and_then(|t| serde_json::from_str(&t).ok()).ok_or_else(invalid)?;
    let c = v.get("installed").or_else(|| v.get("web")).ok_or_else(invalid)?;
    let s = |k: &str, d: &str| c.get(k).and_then(Value::as_str).unwrap_or(d).to_string();
    let client_id = s("client_id", "");
    if client_id.is_empty() {
        return Err(invalid());
    }
    Ok(ClientConfig { client_id, client_secret: s("client_secret", ""), auth_uri: s("auth_uri", AUTH_URI), token_uri: s("token_uri", TOKEN_URI) })
}

pub fn load_client_config_default(home: &Path) -> Result<ClientConfig> {
    let id = std::env::var("PHONELINK_GOOGLE_CLIENT_ID").ok();
    let secret = std::env::var("PHONELINK_GOOGLE_CLIENT_SECRET").ok();
    load_client_config(id.as_deref(), secret.as_deref(), &client_file(home))
}

// ── the saved token ────────────────────────────────────────────────

/// google-auth's authorized-user JSON. Unknown keys are kept.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Token(pub Map<String, Value>);

fn parse_expiry(s: &str) -> Option<DateTime<Utc>> {
    // `datetime.isoformat() + "Z"`, with or without fractions.
    let s = s.trim_end_matches('Z');
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").ok().map(|n| n.and_utc())
}

impl Token {
    pub fn load(path: &Path) -> Option<Token> {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
        v.as_object().cloned().map(Token)
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        atomic_write_text(path, &Value::Object(self.0.clone()).to_string())
    }

    fn str(&self, k: &str) -> &str {
        self.0.get(k).and_then(Value::as_str).unwrap_or_default()
    }

    pub fn access_token(&self) -> &str {
        self.str("token")
    }

    pub fn refresh_token(&self) -> &str {
        self.str("refresh_token")
    }

    pub fn expiry(&self) -> Option<DateTime<Utc>> {
        parse_expiry(self.str("expiry"))
    }

    pub fn has_scope(&self) -> bool {
        match self.0.get("scopes") {
            Some(Value::Array(a)) => a.iter().any(|s| s.as_str() == Some(SCOPE)),
            Some(Value::String(s)) => s.split_whitespace().any(|x| x == SCOPE),
            _ => false,
        }
    }

    /// Usable now: an access token not within 3m45s of its expiry.
    pub fn is_valid(&self, now: DateTime<Utc>) -> bool {
        !self.access_token().is_empty()
            && self.expiry().is_none_or(|e| (e - now).num_seconds() > REFRESH_EARLY_SECONDS)
    }

    /// Apply a token endpoint answer (`access_token`, `expires_in`, …).
    pub fn apply_grant(&mut self, grant: &Value, now: DateTime<Utc>) {
        if let Some(t) = grant.get("access_token").and_then(Value::as_str) {
            self.0.insert("token".into(), t.into());
        }
        if let Some(t) = grant.get("refresh_token").and_then(Value::as_str) {
            self.0.insert("refresh_token".into(), t.into());
        }
        if let Some(t) = grant.get("id_token").and_then(Value::as_str) {
            self.0.insert("id_token".into(), t.into());
        }
        if let Some(secs) = grant.get("expires_in").and_then(Value::as_i64) {
            let e = now + chrono::Duration::seconds(secs);
            self.0.insert("expiry".into(), format!("{}Z", e.format("%Y-%m-%dT%H:%M:%S%.6f")).into());
        }
        if let Some(scope) = grant.get("scope").and_then(Value::as_str) {
            let scopes: Vec<Value> = scope.split_whitespace().map(Value::from).collect();
            self.0.insert("scopes".into(), Value::Array(scopes));
        }
    }

    /// A fresh token from a sign-in, in google-auth's shape.
    pub fn from_grant(grant: &Value, cfg: &ClientConfig, now: DateTime<Utc>) -> Token {
        let mut t = Token(Map::new());
        t.0.insert("token_uri".into(), cfg.token_uri.clone().into());
        t.0.insert("client_id".into(), cfg.client_id.clone().into());
        t.0.insert("client_secret".into(), cfg.client_secret.clone().into());
        t.0.insert("scopes".into(), json!([SCOPE]));
        t.0.insert("universe_domain".into(), "googleapis.com".into());
        t.0.insert("account".into(), "".into());
        t.apply_grant(grant, now);
        t
    }
}

// ── HTTP ───────────────────────────────────────────────────────────

/// What the Google code needs from an HTTP client. `Err` carries a message
/// (status and body for HTTP errors).
pub trait Http {
    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> std::result::Result<Value, String>;
    fn get_json(&self, url: &str, bearer: &str) -> std::result::Result<Value, String>;
    fn send_json(&self, method: &str, url: &str, bearer: &str, body: &Value) -> std::result::Result<Value, String>;
    /// `(bytes, content type)`; `None` for a non-200 answer.
    fn get_bytes(&self, url: &str, bearer: &str) -> Option<(Vec<u8>, String)>;
}

pub struct UreqHttp {
    agent: ureq::Agent,
}

impl UreqHttp {
    pub fn new() -> std::result::Result<Self, String> {
        let tls = native_tls::TlsConnector::new().map_err(|e| e.to_string())?;
        let agent = ureq::AgentBuilder::new()
            .tls_connector(std::sync::Arc::new(tls))
            .timeout(Duration::from_secs(30))
            .build();
        Ok(Self { agent })
    }

    fn json(r: std::result::Result<ureq::Response, ureq::Error>) -> std::result::Result<Value, String> {
        match r {
            Ok(resp) => {
                let text = resp.into_string().map_err(|e| e.to_string())?;
                if text.trim().is_empty() { Ok(Value::Null) } else { serde_json::from_str(&text).map_err(|e| e.to_string()) }
            }
            Err(ureq::Error::Status(code, resp)) => Err(format!("HTTP {code}: {}", resp.into_string().unwrap_or_default())),
            Err(e) => Err(e.to_string()),
        }
    }
}

impl Http for UreqHttp {
    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> std::result::Result<Value, String> {
        Self::json(self.agent.post(url).send_form(form))
    }
    fn get_json(&self, url: &str, bearer: &str) -> std::result::Result<Value, String> {
        Self::json(self.agent.get(url).set("Authorization", &format!("Bearer {bearer}")).call())
    }
    fn send_json(&self, method: &str, url: &str, bearer: &str, body: &Value) -> std::result::Result<Value, String> {
        Self::json(self.agent.request(method, url).set("Authorization", &format!("Bearer {bearer}")).send_json(body.clone()))
    }
    fn get_bytes(&self, url: &str, bearer: &str) -> Option<(Vec<u8>, String)> {
        let resp = self
            .agent
            .get(url)
            .timeout(Duration::from_secs(20))
            .set("Authorization", &format!("Bearer {bearer}"))
            .call()
            .ok()?;
        if resp.status() != 200 {
            return None;
        }
        let kind = resp.content_type().to_string();
        let mut bytes = Vec::new();
        resp.into_reader().take(10 * 1024 * 1024).read_to_end(&mut bytes).ok()?;
        (!bytes.is_empty()).then_some((bytes, kind))
    }
}

// ── credentials ────────────────────────────────────────────────────

/// A usable access token from the saved file, refreshed if needed. A failed
/// refresh (or a token without the contacts scope) removes the file, as
/// Python did for background syncs: the user must reconnect.
pub fn saved_credentials(http: &dyn Http, cfg: Option<&ClientConfig>, path: &Path, now: DateTime<Utc>) -> Result<Token> {
    let reconnect = || GoogleError::AuthRequired("Google Contacts needs you to reconnect from Settings.".into());
    let Some(mut token) = Token::load(path) else {
        return Err(GoogleError::AuthRequired("Google Contacts needs interactive authorization from Settings.".into()));
    };
    if !token.has_scope() {
        let _ = std::fs::remove_file(path);
        return Err(reconnect());
    }
    if token.is_valid(now) {
        return Ok(token);
    }
    if token.refresh_token().is_empty() {
        let _ = std::fs::remove_file(path);
        return Err(reconnect());
    }
    let token_uri = match token.str("token_uri") {
        "" => cfg.map_or(TOKEN_URI.to_string(), |c| c.token_uri.clone()),
        u => u.to_string(),
    };
    let (id, secret) = (token.str("client_id").to_string(), token.str("client_secret").to_string());
    let refresh = token.refresh_token().to_string();
    let form = [("grant_type", "refresh_token"), ("refresh_token", refresh.as_str()), ("client_id", id.as_str()), ("client_secret", secret.as_str())];
    match http.post_form(&token_uri, &form) {
        Ok(grant) => {
            token.apply_grant(&grant, now);
            token.save(path).map_err(other)?;
            Ok(token)
        }
        Err(e) => {
            log::warn!("google: token refresh failed: {e}");
            let _ = std::fs::remove_file(path);
            Err(reconnect())
        }
    }
}

pub fn disconnect(path: &Path) -> bool {
    std::fs::remove_file(path).is_ok()
}

// ── browser sign-in ────────────────────────────────────────────────

fn random_urlsafe(n: usize) -> String {
    let mut buf = vec![0u8; n];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)).is_err() {
        // Not expected on Linux; fall back to the clock.
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        buf.iter_mut().enumerate().for_each(|(i, b)| *b = (t >> (i % 16 * 8)) as u8 ^ i as u8);
    }
    base64_url(&buf)
}

fn base64_url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = (bytes[i] == b'%' && i + 2 < bytes.len())
            .then(|| std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()))
            .flatten();
        match (bytes[i], hex) {
            (_, Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b'+', None) => {
                out.push(b' ');
                i += 1;
            }
            (b, None) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The query of `GET /?a=b&c=d HTTP/1.1`.
pub fn query_params(request_line: &str) -> HashMap<String, String> {
    let target = request_line.split_whitespace().nth(1).unwrap_or_default();
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or_default();
    query
        .split('&')
        .filter_map(|kv| kv.split_once('=').or(Some((kv, ""))))
        .filter(|(k, _)| !k.is_empty())
        .map(|(k, v)| (url_decode(k), url_decode(v)))
        .collect()
}

/// A browser sign-in in progress: open [`SignIn::url`], then call
/// [`SignIn::finish`] (it blocks until the browser comes back).
pub struct SignIn {
    pub url: String,
    listener: TcpListener,
    redirect_uri: String,
    state: String,
    verifier: String,
    cfg: ClientConfig,
}

const SUCCESS_PAGE: &str = "<html><body style=\"font-family:sans-serif\"><h2>Google Contacts access granted.</h2>\
     <p>You can close this window and return to Phone Link.</p></body></html>";

impl SignIn {
    /// Listen on 127.0.0.1 at a random port and build the consent URL
    /// (with PKCE, as google-auth-oauthlib does).
    pub fn start(cfg: &ClientConfig) -> Result<SignIn> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(other)?;
        let port = listener.local_addr().map_err(other)?.port();
        let redirect_uri = format!("http://127.0.0.1:{port}/");
        let state = random_urlsafe(24);
        let verifier = random_urlsafe(48);
        let challenge = {
            use sha2::Digest;
            base64_url(&sha2::Sha256::digest(verifier.as_bytes()))
        };
        let params = [
            ("response_type", "code"),
            ("client_id", cfg.client_id.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("scope", SCOPE),
            ("state", state.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("access_type", "offline"),
            ("prompt", "consent"),
        ];
        let query: Vec<String> = params.iter().map(|(k, v)| format!("{k}={}", url_encode(v))).collect();
        let url = format!("{}?{}", cfg.auth_uri, query.join("&"));
        Ok(SignIn { url, listener, redirect_uri, state, verifier, cfg: cfg.clone() })
    }

    /// Wait for Google's redirect (ignoring stray requests such as
    /// `/favicon.ico`), answer it, and trade the code for a token.
    pub fn finish(self, http: &dyn Http, token_path: &Path, timeout: Duration) -> Result<Token> {
        let deadline = std::time::Instant::now() + timeout;
        self.listener.set_nonblocking(true).map_err(other)?;
        let params = loop {
            if std::time::Instant::now() > deadline {
                return Err(GoogleError::Other("The Google sign-in timed out. Try again from Settings.".into()));
            }
            let (mut stream, _) = match self.listener.accept() {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                Err(e) => return Err(other(e)),
            };
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            let mut line = String::new();
            let _ = BufReader::new(&stream).read_line(&mut line);
            let params = query_params(&line);
            let ours = params.get("state").is_some_and(|s| *s == self.state);
            let body = if ours { SUCCESS_PAGE } else { "" };
            let status = if ours { "200 OK" } else { "404 Not Found" };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            if ours {
                break params;
            }
        };
        if let Some(err) = params.get("error") {
            return Err(GoogleError::Other(format!("Google sign-in was not completed ({err}).")));
        }
        let code = params.get("code").ok_or_else(|| GoogleError::Other("Google sent no authorization code.".into()))?;
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("client_id", self.cfg.client_id.as_str()),
            ("client_secret", self.cfg.client_secret.as_str()),
            ("code_verifier", self.verifier.as_str()),
        ];
        let grant = http.post_form(&self.cfg.token_uri, &form).map_err(other)?;
        let token = Token::from_grant(&grant, &self.cfg, Utc::now());
        token.save(token_path).map_err(other)?;
        Ok(token)
    }
}

// ── People API ─────────────────────────────────────────────────────

fn is_primary(item: &Value) -> bool {
    item.pointer("/metadata/primary").and_then(Value::as_bool) == Some(true)
}

fn primary_or_first(list: Option<&Value>) -> Option<&Value> {
    let a = list?.as_array()?;
    a.iter().find(|i| is_primary(i)).or_else(|| a.first())
}

pub fn primary_name(person: &Value) -> String {
    primary_or_first(person.get("names"))
        .map(|n| {
            let s = |k: &str| n.get(k).and_then(Value::as_str).unwrap_or_default().trim().to_string();
            Some(s("displayName")).filter(|x| !x.is_empty()).unwrap_or_else(|| s("unstructuredName"))
        })
        .unwrap_or_default()
}

/// Primary email, else name, else "Google account".
pub fn account_label(profile: &Value) -> String {
    let email = profile
        .get("emailAddresses")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().find(|e| is_primary(e)))
        .and_then(|e| e.get("value"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !email.is_empty() {
        return email.to_string();
    }
    Some(primary_name(profile)).filter(|n| !n.is_empty()).unwrap_or_else(|| "Google account".into())
}

/// The person's own photo (primary first); Google's placeholder doesn't count.
pub fn photo_url(person: &Value) -> String {
    let Some(a) = person.get("photos").and_then(Value::as_array) else { return String::new() };
    let mut ordered: Vec<&Value> = a.iter().filter(|p| is_primary(p)).collect();
    ordered.extend(a.iter().filter(|p| !is_primary(p)));
    ordered
        .into_iter()
        .filter(|p| p.get("default").and_then(Value::as_bool) != Some(true))
        .filter_map(|p| p.get("url").and_then(Value::as_str).map(str::trim))
        .find(|u| !u.is_empty())
        .unwrap_or_default()
        .to_string()
}

fn phone_values(person: &Value) -> Vec<String> {
    person
        .get("phoneNumbers")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|p| p.get("value").and_then(Value::as_str)).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

/// Same number: equal digits, or equal last 10 digits when both have 10+.
pub fn phones_match(a: &str, b: &str) -> bool {
    let (a, b) = (normalize_phone(a), normalize_phone(b));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a == b || (a.len() >= 10 && b.len() >= 10 && a[a.len() - 10..] == b[b.len() - 10..])
}

fn profile_label(http: &dyn Http, token: &str) -> String {
    match http.get_json(&format!("{PEOPLE}/people/me?personFields=names,emailAddresses"), token) {
        Ok(p) => account_label(&p),
        Err(e) => {
            log::warn!("google: profile fetch failed: {e}");
            "Google account".into()
        }
    }
}

/// Every contact (`people/me/connections`, 1000 per page).
fn connections(http: &dyn Http, token: &str, fields: &str) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut page = String::new();
    loop {
        let mut url = format!(
            "{PEOPLE}/people/me/connections?pageSize=1000&personFields={fields}&sortOrder=FIRST_NAME_ASCENDING"
        );
        if !page.is_empty() {
            url += &format!("&pageToken={}", url_encode(&page));
        }
        let resp = http.get_json(&url, token).map_err(other)?;
        if let Some(a) = resp.get("connections").and_then(Value::as_array) {
            out.extend(a.iter().cloned());
        }
        match resp.get("nextPageToken").and_then(Value::as_str) {
            // A repeated token would loop forever; stop instead.
            Some(t) if !t.is_empty() && t != page => page = t.to_string(),
            _ => return Ok(out),
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct ImportResult {
    /// Normalized number → name.
    pub contacts: HashMap<String, String>,
    pub seen_people: usize,
    pub account_label: String,
    pub photos_changed: usize,
}

/// Names for every number, plus photos for the numbers in current chats
/// (matched on the last 10 digits). A chat contact whose Google photo is
/// gone loses the cached one.
pub fn import_contacts(http: &dyn Http, token: &str, photo_numbers: &HashSet<String>, photo_dir: &Path) -> Result<ImportResult> {
    let account_label = profile_label(http, token);
    let keys: HashSet<String> = photo_numbers.iter().map(|n| phone_key(n)).filter(|k| !k.is_empty()).collect();
    let mut result = ImportResult { account_label, ..Default::default() };
    for person in connections(http, token, "names,phoneNumbers,photos")? {
        result.seen_people += 1;
        let name = primary_name(&person);
        if name.is_empty() {
            continue;
        }
        let mut chat_numbers = Vec::new();
        for value in phone_values(&person) {
            let norm = normalize_phone(&value);
            if norm.is_empty() {
                continue;
            }
            if keys.contains(&phone_key(&norm)) {
                chat_numbers.push(norm.clone());
            }
            result.contacts.insert(norm, name.clone());
        }
        if chat_numbers.is_empty() {
            continue;
        }
        let url = photo_url(&person);
        if url.is_empty() {
            result.photos_changed += chat_numbers.iter().filter(|n| delete_contact_photo_in(photo_dir, n)).count();
        } else if let Some((bytes, kind)) = http.get_bytes(&url, token) {
            result.photos_changed +=
                chat_numbers.iter().filter(|n| store_contact_photo_in(photo_dir, n, &bytes, Some(&kind))).count();
        }
    }
    Ok(result)
}

#[derive(Debug, PartialEq)]
pub enum Upsert {
    Updated,
    Created,
}

fn contact_names(name: &str) -> Value {
    json!([{ "unstructuredName": name, "displayName": name }])
}

/// A rename in Phone Link updates the Google contact with that number, or
/// creates one.
pub fn upsert_contact(http: &dyn Http, token: &str, address: &str, name: &str) -> Result<Upsert> {
    if name.trim().is_empty() || normalize_phone(address).is_empty() {
        return Err(GoogleError::Other("A valid phone number and contact name are required.".into()));
    }
    for person in connections(http, token, "names,phoneNumbers,metadata")? {
        if !phone_values(&person).iter().any(|v| phones_match(v, address)) {
            continue;
        }
        let resource = person.get("resourceName").and_then(Value::as_str).unwrap_or_default().to_string();
        let mut body = person.clone();
        body["names"] = contact_names(name);
        let url = format!("{PEOPLE}/{resource}:updateContact?updatePersonFields=names&personFields=names,phoneNumbers");
        http.send_json("PATCH", &url, token, &body).map_err(other)?;
        return Ok(Upsert::Updated);
    }
    let body = json!({ "names": contact_names(name), "phoneNumbers": [{ "value": address }] });
    http.send_json("POST", &format!("{PEOPLE}/people:createContact"), token, &body).map_err(other)?;
    Ok(Upsert::Created)
}

/// "today at 3:05 PM" / "Nov 4 at 9:10 AM" (Python's `_format_timestamp`).
pub fn last_sync_label(ts: f64, now: chrono::DateTime<chrono::Local>) -> String {
    let dt = super::models::local_from_ms((ts * 1000.0) as i64);
    let fmt = if dt.date_naive() == now.date_naive() { "today at %I:%M %p" } else { "%b %d at %I:%M %p" };
    dt.format(fmt).to_string().replace(" 0", " ")
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use chrono::TimeZone;

    use super::*;

    #[derive(Default)]
    struct Fake {
        gets: HashMap<String, Value>,
        form_reply: Option<Value>,
        calls: RefCell<Vec<String>>,
        photo: Option<(Vec<u8>, String)>,
    }

    impl Http for Fake {
        fn post_form(&self, url: &str, form: &[(&str, &str)]) -> std::result::Result<Value, String> {
            let grant = form.iter().find(|(k, _)| *k == "grant_type").map(|(_, v)| *v).unwrap_or_default();
            self.calls.borrow_mut().push(format!("POST {url} {grant}"));
            self.form_reply.clone().ok_or_else(|| "HTTP 400: invalid_grant".to_string())
        }
        fn get_json(&self, url: &str, _: &str) -> std::result::Result<Value, String> {
            self.calls.borrow_mut().push(format!("GET {url}"));
            // The longest matching key wins, so "pageToken=next" beats
            // "connections?" whatever the map's order.
            self.gets
                .iter()
                .filter(|(k, _)| url.contains(k.as_str()))
                .max_by_key(|(k, _)| k.len())
                .map(|(_, v)| v.clone())
                .ok_or_else(|| "HTTP 404".into())
        }
        fn send_json(&self, method: &str, url: &str, _: &str, body: &Value) -> std::result::Result<Value, String> {
            self.calls.borrow_mut().push(format!("{method} {url} {body}"));
            Ok(Value::Null)
        }
        fn get_bytes(&self, url: &str, _: &str) -> Option<(Vec<u8>, String)> {
            self.calls.borrow_mut().push(format!("PHOTO {url}"));
            self.photo.clone()
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 5, 12, 0, 0).unwrap()
    }

    fn token_json(expiry: &str) -> String {
        format!(
            r#"{{"token":"old","refresh_token":"r1","token_uri":"https://t/token","client_id":"cid","client_secret":"s","scopes":["{SCOPE}"],"universe_domain":"googleapis.com","account":"","expiry":"{expiry}"}}"#
        )
    }

    #[test]
    fn client_config_from_env_or_file() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("c.json");
        assert!(matches!(load_client_config(None, None, &f), Err(GoogleError::Config(_))));
        let c = load_client_config(Some(" id "), Some("sec"), &f).unwrap();
        assert_eq!((c.client_id.as_str(), c.client_secret.as_str()), ("id", "sec"));
        std::fs::write(&f, r#"{"installed":{"client_id":"x","client_secret":"y","token_uri":"https://t"}}"#).unwrap();
        let c = load_client_config(None, None, &f).unwrap();
        assert_eq!((c.client_id.as_str(), c.token_uri.as_str(), c.auth_uri.as_str()), ("x", "https://t", AUTH_URI));
        std::fs::write(&f, "{").unwrap();
        assert!(load_client_config(None, None, &f).unwrap_err().message().starts_with("Invalid"));
    }

    #[test]
    fn valid_token_is_used_as_is() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("tok.json");
        std::fs::write(&p, token_json("2026-11-05T13:00:00.123456Z")).unwrap();
        let fake = Fake::default();
        let tok = saved_credentials(&fake, None, &p, now()).unwrap();
        assert_eq!(tok.access_token(), "old");
        assert!(fake.calls.borrow().is_empty());
    }

    #[test]
    fn expired_token_is_refreshed_and_saved_in_python_shape() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("tok.json");
        // 3 minutes left: inside google-auth's early-refresh window.
        std::fs::write(&p, token_json("2026-11-05T12:03:00Z")).unwrap();
        let fake = Fake { form_reply: Some(json!({"access_token":"new","expires_in":3599})), ..Default::default() };
        let tok = saved_credentials(&fake, None, &p, now()).unwrap();
        assert_eq!(tok.access_token(), "new");
        assert_eq!(fake.calls.borrow()[0], "POST https://t/token refresh_token");
        let saved = Token::load(&p).unwrap();
        assert_eq!(saved.str("expiry"), "2026-11-05T12:59:59.000000Z");
        assert_eq!(saved.refresh_token(), "r1");
        assert_eq!(saved.str("universe_domain"), "googleapis.com");
    }

    #[test]
    fn failed_refresh_disconnects() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("tok.json");
        std::fs::write(&p, token_json("2026-11-01T00:00:00Z")).unwrap();
        let err = saved_credentials(&Fake::default(), None, &p, now()).unwrap_err();
        assert!(matches!(err, GoogleError::AuthRequired(_)));
        assert!(!p.exists());
        assert!(matches!(saved_credentials(&Fake::default(), None, &p, now()), Err(GoogleError::AuthRequired(_))));
    }

    #[test]
    fn sign_in_url_and_redirect_parsing() {
        let cfg = ClientConfig { client_id: "cid".into(), client_secret: "s".into(), auth_uri: AUTH_URI.into(), token_uri: TOKEN_URI.into() };
        let s = SignIn::start(&cfg).unwrap();
        assert!(s.url.starts_with(AUTH_URI));
        assert!(s.url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A"));
        assert!(s.url.contains("code_challenge_method=S256"));
        assert!(s.url.contains(&format!("scope={}", url_encode(SCOPE))));
        let q = query_params("GET /?state=ab%2Bc&code=4%2F0AX+y&scope=x HTTP/1.1");
        assert_eq!(q["state"], "ab+c");
        assert_eq!(q["code"], "4/0AX y");
        assert!(query_params("GET /favicon.ico HTTP/1.1").is_empty());
    }

    #[test]
    fn sign_in_round_trip_over_loopback() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("tok.json");
        let cfg = ClientConfig { client_id: "cid".into(), client_secret: "s".into(), auth_uri: AUTH_URI.into(), token_uri: "https://t/token".into() };
        let s = SignIn::start(&cfg).unwrap();
        let q = query_params(&format!("GET /?{} HTTP/1.1", s.url.split_once('?').unwrap().1));
        let (redirect, state) = (q["redirect_uri"].clone(), q["state"].clone());
        // The "browser": a stray favicon request first, then Google's redirect.
        let browser = std::thread::spawn(move || {
            let port = redirect.trim_end_matches('/').rsplit(':').next().unwrap().to_string();
            for path in ["/favicon.ico".to_string(), format!("/?state={state}&code=abc")] {
                let mut c = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
                write!(c, "GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
                let mut reply = String::new();
                let _ = c.read_to_string(&mut reply);
                if path.contains("code") {
                    assert!(reply.contains("access granted"));
                }
            }
        });
        let fake = Fake { form_reply: Some(json!({"access_token":"a","refresh_token":"r","expires_in":3600})), ..Default::default() };
        let tok = s.finish(&fake, &p, Duration::from_secs(10)).unwrap();
        browser.join().unwrap();
        assert_eq!(tok.refresh_token(), "r");
        assert!(tok.has_scope());
        assert!(fake.calls.borrow()[0].ends_with("authorization_code"));
        assert_eq!(Token::load(&p).unwrap().str("client_id"), "cid");
    }

    #[test]
    fn people_helpers() {
        let person = json!({
            "names": [{"displayName":"Alex Old"}, {"displayName":"Alex Kim","metadata":{"primary":true}}],
            "photos": [{"url":"https://p/default","default":true,"metadata":{"primary":true}}, {"url":"https://p/real"}],
        });
        assert_eq!(primary_name(&person), "Alex Kim");
        assert_eq!(photo_url(&person), "https://p/real");
        assert_eq!(account_label(&json!({"emailAddresses":[{"value":"a@b.c","metadata":{"primary":true}}]})), "a@b.c");
        assert_eq!(account_label(&json!({})), "Google account");
        assert!(phones_match("+1 (555) 555-0101", "5555550101"));
        assert!(!phones_match("0101", "5555550101"));
        assert!(!phones_match("", ""));
    }

    #[test]
    fn import_names_and_chat_photos() {
        let t = tempfile::tempdir().unwrap();
        let photos = t.path().join("photos");
        std::fs::create_dir_all(&photos).unwrap();
        std::fs::write(photos.join("5555550102.jpg"), b"old").unwrap();
        let page1 = json!({"connections":[
            {"names":[{"displayName":"Alex"}],"phoneNumbers":[{"value":"+1 555-555-0101"}],"photos":[{"url":"https://p/alex"}]},
            {"names":[{"displayName":"Sam"}],"phoneNumbers":[{"value":"(555) 555-0102"}]},
        ],"nextPageToken":"next"});
        let page2 = json!({"connections":[{"phoneNumbers":[{"value":"5555550199"}]}]});
        let fake = Fake {
            gets: [
                ("pageToken=next".to_string(), page2),
                ("connections?".to_string(), page1),
                ("people/me?".to_string(), json!({"names":[{"displayName":"Me"}]})),
            ]
            .into(),
            photo: Some((b"jpegbytes".to_vec(), "image/jpeg".into())),
            ..Default::default()
        };
        let chats: HashSet<String> = ["5555550101".to_string(), "+15555550102".to_string()].into();
        let r = import_contacts(&fake, "tok", &chats, &photos).unwrap();
        assert_eq!(r.seen_people, 3);
        assert_eq!(r.account_label, "Me");
        assert_eq!(r.contacts.get("15555550101").map(String::as_str), Some("Alex"));
        assert_eq!(r.contacts.get("5555550102").map(String::as_str), Some("Sam"));
        assert_eq!(r.contacts.len(), 2);
        // Alex's photo stored, Sam's stale one removed.
        assert_eq!(r.photos_changed, 2);
        assert_eq!(std::fs::read(photos.join("15555550101.jpg")).unwrap(), b"jpegbytes");
    }

    #[test]
    fn rename_updates_or_creates() {
        let fake = Fake {
            gets: [(
                "connections?".to_string(),
                json!({"connections":[{"resourceName":"people/c1","etag":"e","names":[{"displayName":"Old"}],"phoneNumbers":[{"value":"+15555550101"}]}]}),
            )]
            .into(),
            ..Default::default()
        };
        assert_eq!(upsert_contact(&fake, "t", "5555550101", "Alex").unwrap(), Upsert::Updated);
        let call = fake.calls.borrow().last().unwrap().clone();
        assert!(call.starts_with("PATCH https://people.googleapis.com/v1/people/c1:updateContact?updatePersonFields=names"));
        assert!(call.contains("\"etag\":\"e\"") && call.contains("Alex"));
        assert_eq!(upsert_contact(&fake, "t", "5555550199", "New").unwrap(), Upsert::Created);
        assert!(fake.calls.borrow().last().unwrap().starts_with("POST https://people.googleapis.com/v1/people:createContact"));
        assert!(upsert_contact(&fake, "t", "abc", "x").is_err());
    }

    #[test]
    fn sync_time_label() {
        let now = chrono::Local.with_ymd_and_hms(2026, 11, 5, 18, 0, 0).unwrap();
        let today = chrono::Local.with_ymd_and_hms(2026, 11, 5, 9, 5, 0).unwrap().timestamp() as f64;
        assert_eq!(last_sync_label(today, now), "today at 9:05 AM");
        let earlier = chrono::Local.with_ymd_and_hms(2026, 11, 4, 15, 30, 0).unwrap().timestamp() as f64;
        assert_eq!(last_sync_label(earlier, now), "Nov 4 at 3:30 PM");
    }
}
