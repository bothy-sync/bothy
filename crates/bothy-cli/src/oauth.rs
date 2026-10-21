//! Google OAuth for the CLI: the user's own client, loopback sign-in with PKCE, the device flow,
//! token refresh and revocation. Tokens and secrets are never printed.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value as J;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::fail::{CliError, CliResult};

pub const SCOPE_APPDATA: &str = "https://www.googleapis.com/auth/drive.appdata";
pub const SCOPE_FILE: &str = "https://www.googleapis.com/auth/drive.file";

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const DEVICE_URL: &str = "https://oauth2.googleapis.com/device/code";
const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";

/// An OAuth client from the user's own Google Cloud project.
#[derive(Clone)]
pub struct Client {
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
    /// Parses the JSON file downloaded from the Cloud console (`installed` or `web`).
    pub fn from_json(bytes: &[u8]) -> CliResult<Self> {
        let j: J = serde_json::from_slice(bytes)
            .map_err(|_| CliError::usage("the client file is not JSON"))?;
        let c = j.get("installed").or_else(|| j.get("web")).unwrap_or(&j);
        let id = c["client_id"].as_str().filter(|s| !s.is_empty());
        let secret = c["client_secret"].as_str().unwrap_or_default();
        match id {
            Some(id) => Ok(Self {
                id: id.to_owned(),
                secret: secret.to_owned(),
            }),
            None => Err(CliError::usage(
                "the client file has no client_id; download it from Google Cloud console → APIs & Services → Credentials",
            )),
        }
    }
}

/// A long-lived grant: the refresh token and the scopes it covers.
#[derive(Clone)]
pub struct Grant {
    pub refresh_token: String,
    pub scopes: Vec<String>,
    /// `desktop` or `device`: which client issued it.
    pub client: String,
}

impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grant")
            .field("scopes", &self.scopes)
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

impl Grant {
    pub fn has(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

pub fn http() -> CliResult<reqwest::Client> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| CliError::other(format!("cannot build HTTP client: {e}")))
}

fn random_b64(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("system random generator");
    URL_SAFE_NO_PAD.encode(b)
}

async fn post_form(
    http: &reqwest::Client,
    url: &str,
    form: &[(&str, &str)],
) -> CliResult<(u16, J)> {
    let resp = http
        .post(url)
        .form(form)
        .send()
        .await
        .map_err(|e| CliError {
            code: 4,
            message: format!("cannot reach Google: {e}"),
        })?;
    let status = resp.status().as_u16();
    let body = resp.bytes().await.map_err(|e| CliError {
        code: 4,
        message: format!("Google: {e}"),
    })?;
    Ok((status, serde_json::from_slice(&body).unwrap_or(J::Null)))
}

fn grant_from(j: &J, client: &str) -> CliResult<Grant> {
    let refresh = j["refresh_token"].as_str().ok_or_else(|| {
        CliError::auth(
            "Google returned no refresh token; run `bothy auth logout` and sign in again",
        )
    })?;
    Ok(Grant {
        refresh_token: refresh.to_owned(),
        scopes: j["scope"]
            .as_str()
            .unwrap_or_default()
            .split(' ')
            .map(str::to_owned)
            .collect(),
        client: client.to_owned(),
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
pub async fn loopback_login(client: &Client, scopes: &[&str]) -> CliResult<Grant> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| CliError::io("cannot listen on 127.0.0.1", e))?;
    let port = listener
        .local_addr()
        .map_err(|e| CliError::io("cannot listen", e))?
        .port();
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
    eprintln!("Opening the browser to sign in. If it does not open, visit:\n\n  {url}\n");
    let _ = open::that_detached(url.as_str());

    let code = tokio::time::timeout(Duration::from_secs(300), async {
        loop {
            let (mut sock, _) = listener.accept().await.map_err(|e| CliError::io("sign-in redirect", e))?;
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let Some(target) = req.lines().next().and_then(|l| l.split(' ').nth(1)) else { continue };
            let params: std::collections::HashMap<String, String> =
                reqwest::Url::parse(&format!("http://localhost{target}"))
                    .map(|u| u.query_pairs().into_owned().collect())
                    .unwrap_or_default();
            if params.is_empty() {
                // Browsers also ask for /favicon.ico.
                let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
                continue;
            }
            let ok = params.get("state") == Some(&state) && params.contains_key("code");
            let page = if ok {
                "Bothy is signed in. You can close this tab."
            } else {
                "Sign-in failed. Return to the terminal."
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                page.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            if let Some(err) = params.get("error") {
                return Err(CliError::auth(format!("sign-in was not completed: {err}")));
            }
            if !ok {
                return Err(CliError::auth("sign-in redirect had a wrong state; try again"));
            }
            return Ok(params["code"].clone());
        }
    })
    .await
    .map_err(|_| CliError::auth("sign-in timed out after 5 minutes"))??;

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
        return Err(CliError::auth(format!(
            "token exchange failed: {}",
            oauth_error(&j)
        )));
    }
    grant_from(&j, "desktop")
}

/// Signs in on a machine without a browser: show a code, the user approves on another device.
/// Google allows only `drive.file` here (it rejects `drive.appdata` in this flow).
pub async fn device_login(client: &Client) -> CliResult<Grant> {
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
        return Err(CliError::auth(msg));
    }
    let device_code = j["device_code"]
        .as_str()
        .ok_or_else(|| CliError::auth("no device code"))?
        .to_owned();
    let user_code = j["user_code"].as_str().unwrap_or_default();
    let verify = j["verification_url"]
        .as_str()
        .or(j["verification_uri"].as_str())
        .unwrap_or("https://www.google.com/device");
    let mut interval = j["interval"].as_u64().unwrap_or(5).max(1);
    let expires = j["expires_in"].as_u64().unwrap_or(1800);
    eprintln!(
        "On a phone or computer, open {verify} and enter the code:\n\n  {user_code}\n\nWaiting for approval..."
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(expires);
    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        if tokio::time::Instant::now() >= deadline {
            return Err(CliError::auth(
                "the code expired; run `bothy auth login --device` again",
            ));
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
            return grant_from(&j, "device");
        }
        match j["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some("access_denied") => return Err(CliError::auth("sign-in was denied")),
            Some("expired_token") => {
                return Err(CliError::auth(
                    "the code expired; run `bothy auth login --device` again",
                ));
            }
            _ => {
                return Err(CliError::auth(format!(
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
) -> bothy::Result<(String, Duration)> {
    use bothy::{Error, ErrorKind};
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
        .map_err(|e| {
            Error::new(
                ErrorKind::Offline,
                "cannot reach Google to refresh the token",
            )
            .with_source(e)
        })?;
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
        return Err(Error::new(
            ErrorKind::Offline,
            format!("token refresh failed: {reason}"),
        ));
    }
    // invalid_grant: revoked, expired (7 days in "Testing" mode) or rotated.
    Err(Error::new(
        ErrorKind::AuthRequired,
        format!("the sign-in is no longer valid ({reason})"),
    ))
}

/// Revokes a refresh token. Revoking one token ends every token of this client for the user.
pub async fn revoke(refresh_token: &str) -> CliResult<()> {
    let http = http()?;
    let (status, j) = post_form(&http, REVOKE_URL, &[("token", refresh_token)]).await?;
    // 400 invalid_token means it was already revoked or expired.
    if status == 200 || j["error"] == "invalid_token" {
        Ok(())
    } else {
        Err(CliError::other(format!(
            "revocation failed: {}",
            oauth_error(&j)
        )))
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
        assert!(Client::from_json(b"{}").is_err());
        assert!(Client::from_json(b"nope").is_err());
    }

    #[test]
    fn debug_output_never_contains_secrets() {
        let c = Client {
            id: "id".into(),
            secret: "TOP-SECRET".into(),
        };
        let g = Grant {
            refresh_token: "REFRESH".into(),
            scopes: vec![SCOPE_FILE.into()],
            client: "device".into(),
        };
        assert!(!format!("{c:?}{g:?}").contains("TOP-SECRET"));
        assert!(!format!("{c:?}{g:?}").contains("REFRESH"));
    }
}
