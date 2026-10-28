//! Google OAuth for Bothy apps that run their own sign-in: the CLI, the Python binding, embedded
//! agents.
//!
//! The core crate never does OAuth; it asks an [`AccessTokenProvider`] for tokens. This crate
//! provides one ([`RefreshingTokens`]) plus the flows to get the long-lived grant behind it:
//! a browser sign-in with a loopback redirect and PKCE ([`loopback_login`]), and the device
//! flow for machines without a browser ([`device_login`]). [`TokenStore`] keeps the grant in
//! the OS keyring or a file readable only by the user.
//!
//! Tokens and client secrets never appear in `Debug` output or error messages.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
pub use bothy::auth::AccessTokenProvider;
use bothy::{Error, ErrorKind, Result};
use serde_json::{Value as J, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub mod official;

/// The hidden per-app folder (`appDataFolder`). Not available through the device flow.
pub const SCOPE_APPDATA: &str = "https://www.googleapis.com/auth/drive.appdata";
/// Files the app created, such as the visible `Bothy/<name>` folders.
pub const SCOPE_FILE: &str = "https://www.googleapis.com/auth/drive.file";

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const DEVICE_URL: &str = "https://oauth2.googleapis.com/device/code";
const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message.into())
}

fn auth_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::AuthRequired, message.into())
}

fn offline(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Offline, message.into())
}

/// An OAuth client of the app's Google Cloud project.
#[derive(Clone)]
pub struct Client {
    /// The client id (`…apps.googleusercontent.com`).
    pub id: String,
    secret: String,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// A client from its id and secret (installed-app secrets are not confidential, but
    /// Google requires them).
    pub fn new(id: &str, secret: &str) -> Self {
        Self {
            id: id.to_owned(),
            secret: secret.to_owned(),
        }
    }

    /// Parses the JSON file downloaded from the Cloud console (`installed` or `web`).
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let j: J =
            serde_json::from_slice(bytes).map_err(|_| invalid("the client file is not JSON"))?;
        let c = j.get("installed").or_else(|| j.get("web")).unwrap_or(&j);
        match c["client_id"].as_str().filter(|s| !s.is_empty()) {
            Some(id) => Ok(Self::new(
                id,
                c["client_secret"].as_str().unwrap_or_default(),
            )),
            None => Err(invalid(
                "the client file has no client_id; download it from Google Cloud console → APIs & Services → Credentials",
            )),
        }
    }

    /// Reads a client JSON file.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = std::fs::read(path)
            .map_err(|e| invalid(format!("cannot read {}", path.display())).with_source(e))?;
        Self::from_json(&bytes)
    }
}

/// A long-lived grant: the refresh token and the scopes it covers.
#[derive(Clone)]
pub struct Grant {
    /// The refresh token. Keep it secret.
    pub refresh_token: String,
    /// Scopes the user granted.
    pub scopes: Vec<String>,
    /// Which client issued it: `desktop` (loopback) or `device` (device flow).
    pub client: String,
    /// The id of that client. `None` for grants stored before 0.5.
    pub client_id: Option<String>,
}

impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grant")
            .field("scopes", &self.scopes)
            .field("client", &self.client)
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

impl Grant {
    /// Whether the grant covers `scope`.
    pub fn has(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }

    /// The stored form.
    pub fn to_json(&self) -> String {
        json!({
            "refresh_token": self.refresh_token,
            "scopes": self.scopes,
            "client": self.client,
            "client_id": self.client_id,
        })
        .to_string()
    }

    /// Parses the stored form.
    pub fn from_json(s: &str) -> Option<Self> {
        let j: J = serde_json::from_str(s).ok()?;
        Some(Self {
            refresh_token: j["refresh_token"].as_str()?.to_owned(),
            scopes: j["scopes"]
                .as_array()?
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            client: j["client"].as_str()?.to_owned(),
            client_id: j["client_id"].as_str().map(str::to_owned),
        })
    }
}

/// The HTTP client for Google's OAuth endpoints (rustls with ring).
pub fn http() -> Result<reqwest::Client> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60));
    #[cfg(target_os = "android")]
    let builder = builder.tls_backend_preconfigured(android_tls());
    builder
        .build()
        .map_err(|e| Error::new(ErrorKind::Internal, "cannot build HTTP client").with_source(e))
}

/// Android has no platform verifier without JNI setup; verify against Mozilla's roots.
#[cfg(target_os = "android")]
fn android_tls() -> rustls::ClientConfig {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let mut config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring supports the default TLS versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config
}

fn random_b64(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("system random generator");
    URL_SAFE_NO_PAD.encode(b)
}

async fn post_form(http: &reqwest::Client, url: &str, form: &[(&str, &str)]) -> Result<(u16, J)> {
    let resp = http
        .post(url)
        .form(form)
        .send()
        .await
        .map_err(|e| offline("cannot reach Google").with_source(e))?;
    let status = resp.status().as_u16();
    let body = resp
        .bytes()
        .await
        .map_err(|e| offline("Google closed the connection").with_source(e))?;
    Ok((status, serde_json::from_slice(&body).unwrap_or(J::Null)))
}

fn grant_from(j: &J, kind: &str, client: &Client) -> Result<Grant> {
    let refresh = j["refresh_token"].as_str().ok_or_else(|| {
        auth_error("Google returned no refresh token; sign out and sign in again")
    })?;
    Ok(Grant {
        refresh_token: refresh.to_owned(),
        scopes: j["scope"]
            .as_str()
            .unwrap_or_default()
            .split(' ')
            .map(str::to_owned)
            .collect(),
        client: kind.to_owned(),
        client_id: Some(client.id.clone()),
    })
}

fn oauth_error(j: &J) -> String {
    let e = j["error"].as_str().unwrap_or("unknown_error");
    match j["error_description"].as_str() {
        Some(d) => format!("{e}: {d}"),
        None => e.to_owned(),
    }
}

/// Signs in through the browser: a loopback redirect to 127.0.0.1 with PKCE.
///
/// `show_url` gets the sign-in URL; it should open it in a browser or print it.
pub async fn loopback_login(
    client: &Client,
    scopes: &[&str],
    show_url: impl FnOnce(&str),
) -> Result<Grant> {
    let io = |what: &'static str| move |e| Error::new(ErrorKind::Internal, what).with_source(e);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(io("cannot listen on 127.0.0.1"))?;
    let port = listener.local_addr().map_err(io("cannot listen"))?.port();
    let redirect = format!("http://127.0.0.1:{port}");
    let verifier = random_b64(32);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_b64(16);
    let mut url = reqwest::Url::parse(AUTH_URL).expect("valid URL");
    url.query_pairs_mut()
        .append_pair("client_id", &client.id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("response_type", "code")
        .append_pair("scope", &scopes.join(" "))
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    show_url(url.as_str());

    let code = tokio::time::timeout(Duration::from_secs(300), async {
        loop {
            let (mut sock, _) = listener.accept().await.map_err(io("sign-in redirect"))?;
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let Some(target) = req.lines().next().and_then(|l| l.split(' ').nth(1)) else {
                continue;
            };
            let params: std::collections::HashMap<String, String> =
                reqwest::Url::parse(&format!("http://localhost{target}"))
                    .map(|u| u.query_pairs().into_owned().collect())
                    .unwrap_or_default();
            if params.is_empty() {
                // Browsers also ask for /favicon.ico.
                let _ = sock
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .await;
                continue;
            }
            let ok = params.get("state") == Some(&state) && params.contains_key("code");
            let page = if ok {
                "Bothy is signed in. You can close this tab."
            } else {
                "Sign-in failed. Return to the app."
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                page.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            if let Some(err) = params.get("error") {
                return Err(auth_error(format!("sign-in was not completed: {err}")));
            }
            if !ok {
                return Err(auth_error("sign-in redirect had a wrong state; try again"));
            }
            return Ok(params["code"].clone());
        }
    })
    .await
    .map_err(|_| auth_error("sign-in timed out after 5 minutes"))??;

    let http = http()?;
    let (status, j) = post_form(
        &http,
        TOKEN_URL,
        &[
            ("code", &code),
            ("client_id", &client.id),
            ("client_secret", &client.secret),
            ("redirect_uri", &redirect),
            ("grant_type", "authorization_code"),
            ("code_verifier", &verifier),
        ],
    )
    .await?;
    if status != 200 {
        return Err(auth_error(format!(
            "token exchange failed: {}",
            oauth_error(&j)
        )));
    }
    grant_from(&j, "desktop", client)
}

/// What the user must do to approve a device sign-in.
#[derive(Clone, Debug)]
pub struct DeviceCode {
    /// The page to open on a phone or computer.
    pub verification_url: String,
    /// The code to enter there.
    pub user_code: String,
    /// How long the code stays valid.
    pub expires_in: Duration,
}

/// Signs in on a machine without a browser: the user enters a code on another device.
/// Google allows only `drive.file` here (it rejects `drive.appdata` in this flow).
///
/// `show` gets the code to display; the call then waits until the user approves.
pub async fn device_login(client: &Client, show: impl FnOnce(&DeviceCode)) -> Result<Grant> {
    let http = http()?;
    let (status, j) = post_form(
        &http,
        DEVICE_URL,
        &[("client_id", &client.id), ("scope", SCOPE_FILE)],
    )
    .await?;
    if status != 200 {
        let mut msg = format!("device sign-in failed: {}", oauth_error(&j));
        if j["error"] == "invalid_client" {
            msg.push_str(
                "\nhint: the device client must be of type \"TVs and Limited Input devices\"",
            );
        }
        return Err(auth_error(msg));
    }
    let device_code = j["device_code"]
        .as_str()
        .ok_or_else(|| auth_error("no device code"))?
        .to_owned();
    let mut interval = j["interval"].as_u64().unwrap_or(5).max(1);
    let expires = j["expires_in"].as_u64().unwrap_or(1800);
    show(&DeviceCode {
        verification_url: j["verification_url"]
            .as_str()
            .or(j["verification_uri"].as_str())
            .unwrap_or("https://www.google.com/device")
            .to_owned(),
        user_code: j["user_code"].as_str().unwrap_or_default().to_owned(),
        expires_in: Duration::from_secs(expires),
    });

    let expired = || auth_error("the code expired; start the device sign-in again");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(expires);
    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        if tokio::time::Instant::now() >= deadline {
            return Err(expired());
        }
        let (status, j) = post_form(
            &http,
            TOKEN_URL,
            &[
                ("client_id", &client.id),
                ("client_secret", &client.secret),
                ("device_code", &device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ],
        )
        .await?;
        if status == 200 {
            return grant_from(&j, "device", client);
        }
        match j["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some("access_denied") => return Err(auth_error("sign-in was denied")),
            Some("expired_token") => return Err(expired()),
            _ => {
                return Err(auth_error(format!(
                    "device sign-in failed: {}",
                    oauth_error(&j)
                )));
            }
        }
    }
}

/// Exchanges a refresh token for an access token: `(token, lifetime)`.
pub async fn refresh(
    http: &reqwest::Client,
    client: &Client,
    refresh_token: &str,
) -> Result<(String, Duration)> {
    let resp = http
        .post(TOKEN_URL)
        .form(&[
            ("client_id", client.id.as_str()),
            ("client_secret", client.secret.as_str()),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await
        .map_err(|e| offline("cannot reach Google to refresh the token").with_source(e))?;
    let status = resp.status();
    let j: J = match resp.bytes().await {
        Ok(b) => serde_json::from_slice(&b).unwrap_or(J::Null),
        Err(_) => J::Null,
    };
    if status.is_success()
        && let Some(t) = j["access_token"].as_str()
    {
        let ttl = j["expires_in"].as_u64().unwrap_or(3600);
        return Ok((t.to_owned(), Duration::from_secs(ttl)));
    }
    let reason = oauth_error(&j);
    if status.is_server_error() {
        return Err(offline(format!("token refresh failed: {reason}")));
    }
    // invalid_grant: revoked, expired (7 days in "Testing" mode) or rotated.
    Err(auth_error(format!(
        "the sign-in is no longer valid ({reason})"
    )))
}

/// Revokes a refresh token at Google. Google revokes per Cloud project: every device of the
/// user signed in with this project is signed out, so apps should usually only forget the
/// token locally ([`TokenStore::clear`]).
pub async fn revoke(refresh_token: &str) -> Result<()> {
    let http = http()?;
    let (status, j) = post_form(&http, REVOKE_URL, &[("token", refresh_token)]).await?;
    // 400 invalid_token means it was already revoked or expired.
    if status == 200 || j["error"] == "invalid_token" {
        Ok(())
    } else {
        Err(Error::new(
            ErrorKind::Internal,
            format!("revocation failed: {}", oauth_error(&j)),
        ))
    }
}

/// Where [`TokenStore::save`] put the grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    /// The OS keyring.
    Keyring,
    /// A file readable only by the user.
    File(PathBuf),
}

/// Keeps a grant in the OS keyring, or in a `0600` file where there is none (headless Linux
/// without a Secret Service). `BOTHY_TOKEN_STORE=file`, or [`TokenStore::file_only`], uses only
/// the file.
#[derive(Clone, Debug)]
// Without the keyring feature only `file` is used.
#[cfg_attr(not(feature = "keyring"), allow(dead_code))]
pub struct TokenStore {
    service: String,
    account: String,
    file: PathBuf,
    keyring: bool,
}

impl TokenStore {
    /// A store under the keyring entry `service`/`account`, with `file` as the fallback.
    pub fn new(service: &str, account: &str, file: impl Into<PathBuf>) -> Self {
        Self {
            service: service.to_owned(),
            account: account.to_owned(),
            file: file.into(),
            keyring: cfg!(feature = "keyring")
                && !std::env::var("BOTHY_TOKEN_STORE").is_ok_and(|v| v == "file"),
        }
    }

    /// A store that only uses `file`.
    pub fn file_only(file: impl Into<PathBuf>) -> Self {
        Self {
            keyring: false,
            ..Self::new("", "", file)
        }
    }

    /// The fallback file.
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// Stores the grant and removes any copy left in the other place.
    pub fn save(&self, grant: &Grant) -> Result<Location> {
        #[cfg(feature = "keyring")]
        if self.keyring
            && let Ok(entry) = keyring::Entry::new(&self.service, &self.account)
            && entry.set_password(&grant.to_json()).is_ok()
        {
            let _ = std::fs::remove_file(&self.file);
            return Ok(Location::Keyring);
        }
        write_private(&self.file, grant.to_json().as_bytes())?;
        Ok(Location::File(self.file.clone()))
    }

    /// The stored grant, if any.
    pub fn load(&self) -> Option<Grant> {
        #[cfg(feature = "keyring")]
        if self.keyring
            && let Ok(entry) = keyring::Entry::new(&self.service, &self.account)
            && let Ok(s) = entry.get_password()
        {
            return Grant::from_json(&s);
        }
        std::fs::read_to_string(&self.file)
            .ok()
            .and_then(|s| Grant::from_json(&s))
    }

    /// Forgets the grant on this machine (it stays valid at Google; see [`revoke`]).
    pub fn clear(&self) {
        #[cfg(feature = "keyring")]
        if self.keyring
            && let Ok(entry) = keyring::Entry::new(&self.service, &self.account)
        {
            let _ = entry.delete_credential();
        }
        let _ = std::fs::remove_file(&self.file);
    }
}

/// Writes `bytes` to `path` atomically, readable only by the user (mode `0600` on Unix).
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let io = |e| {
        Error::new(
            ErrorKind::Internal,
            format!("cannot write {}", path.display()),
        )
        .with_source(e)
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let tmp = path.with_extension("tmp");
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).map_err(io)?;
        f.write_all(bytes).map_err(io)?;
        f.sync_all().map_err(io)?;
    }
    std::fs::rename(&tmp, path).map_err(io)
}

/// Fresh access tokens from a grant, cached until a minute before they expire.
pub struct RefreshingTokens {
    client: Client,
    refresh_token: String,
    http: reqwest::Client,
    cached: Mutex<Option<(String, Instant)>>,
}

impl std::fmt::Debug for RefreshingTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshingTokens")
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

impl RefreshingTokens {
    /// A provider for `grant`, which `client` issued.
    pub fn new(client: Client, grant: &Grant) -> Result<Self> {
        Ok(Self {
            client,
            refresh_token: grant.refresh_token.clone(),
            http: http()?,
            cached: Mutex::new(None),
        })
    }
}

#[async_trait]
impl AccessTokenProvider for RefreshingTokens {
    async fn access_token(&self) -> Result<String> {
        if let Some((t, until)) = self
            .cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            && Instant::now() < until
        {
            return Ok(t);
        }
        let (token, ttl) = refresh(&self.http, &self.client, &self.refresh_token).await?;
        // Refresh a minute early.
        let until = Instant::now() + ttl.saturating_sub(Duration::from_secs(60));
        *self.cached.lock().unwrap_or_else(|e| e.into_inner()) = Some((token.clone(), until));
        Ok(token)
    }

    async fn invalidate(&self, rejected: &str) {
        let mut c = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if c.as_ref().is_some_and(|(t, _)| t == rejected) {
            *c = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_files_from_the_console() {
        let c = Client::from_json(
            br#"{"installed":{"client_id":"abc.apps.googleusercontent.com","client_secret":"s"}}"#,
        )
        .unwrap();
        assert_eq!(c.id, "abc.apps.googleusercontent.com");
        assert_eq!(
            Client::from_json(b"{}").unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
        assert!(Client::from_json(b"nope").is_err());
    }

    #[test]
    fn debug_output_never_contains_secrets() {
        let c = Client::new("id", "TOP-SECRET");
        let g = Grant {
            refresh_token: "REFRESH".into(),
            scopes: vec![SCOPE_FILE.into()],
            client: "device".into(),
            client_id: Some("id".into()),
        };
        let t = RefreshingTokens::new(c.clone(), &g).unwrap();
        let all = format!("{c:?}{g:?}{t:?}");
        assert!(
            !all.contains("TOP-SECRET") && !all.contains("REFRESH"),
            "{all}"
        );
    }

    #[test]
    fn grant_json_round_trip_and_file_store() {
        let g = Grant {
            refresh_token: "r".into(),
            scopes: vec!["a".into(), "b".into()],
            client: "desktop".into(),
            client_id: Some("1-d.apps.googleusercontent.com".into()),
        };
        let back = Grant::from_json(&g.to_json()).unwrap();
        assert_eq!(
            (back.refresh_token, back.scopes, back.client, back.client_id),
            (
                g.refresh_token.clone(),
                g.scopes.clone(),
                g.client.clone(),
                g.client_id.clone()
            )
        );
        // Grants stored before 0.5 have no client id.
        let old = Grant::from_json(r#"{"refresh_token":"r","scopes":[],"client":"device"}"#);
        assert_eq!(old.unwrap().client_id, None);

        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::file_only(dir.path().join("t/token.json"));
        assert!(store.load().is_none());
        assert_eq!(
            store.save(&g).unwrap(),
            Location::File(store.file().to_owned())
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.file())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(store.load().unwrap().has("b"));
        store.clear();
        assert!(store.load().is_none());
    }
}
