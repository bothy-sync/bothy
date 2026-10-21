//! Where the refresh token lives, and the access-token provider the core uses.
//!
//! The grant goes to the OS keyring. Where there is none (headless Linux without a Secret
//! Service) it falls back to a `0600` file, with a warning. `BOTHY_TOKEN_STORE=file` forces the
//! file (tests, containers).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value as J, json};

use crate::config::{Paths, write_private};
use crate::fail::{CliError, CliResult};
use crate::oauth::{self, Client, Grant};

const SERVICE: &str = "dev.bothy.cli";
const ACCOUNT: &str = "google";

fn to_json(g: &Grant) -> String {
    json!({"refresh_token": g.refresh_token, "scopes": g.scopes, "client": g.client}).to_string()
}

fn from_json(s: &str) -> Option<Grant> {
    let j: J = serde_json::from_str(s).ok()?;
    Some(Grant {
        refresh_token: j["refresh_token"].as_str()?.to_owned(),
        scopes: j["scopes"]
            .as_array()?
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        client: j["client"].as_str()?.to_owned(),
    })
}

fn file_only() -> bool {
    std::env::var("BOTHY_TOKEN_STORE").is_ok_and(|v| v == "file")
}

/// Stores the grant. Returns where it went (`keyring` or the file path).
pub fn save(paths: &Paths, grant: &Grant) -> CliResult<String> {
    if !file_only()
        && let Ok(entry) = keyring::Entry::new(SERVICE, ACCOUNT)
        && entry.set_password(&to_json(grant)).is_ok()
    {
        let _ = std::fs::remove_file(paths.token_file());
        return Ok("the system keyring".into());
    }
    let file = paths.token_file();
    write_private(&file, to_json(grant).as_bytes())?;
    if !file_only() {
        eprintln!(
            "warning: no system keyring available; the sign-in is stored in {} (readable only by you)",
            file.display()
        );
    }
    Ok(file.display().to_string())
}

/// Loads the grant, if signed in.
pub fn load(paths: &Paths) -> Option<Grant> {
    if !file_only()
        && let Ok(entry) = keyring::Entry::new(SERVICE, ACCOUNT)
        && let Ok(s) = entry.get_password()
    {
        return from_json(&s);
    }
    std::fs::read_to_string(paths.token_file())
        .ok()
        .and_then(|s| from_json(&s))
}

/// Deletes the stored grant everywhere.
pub fn clear(paths: &Paths) {
    if !file_only()
        && let Ok(entry) = keyring::Entry::new(SERVICE, ACCOUNT)
    {
        let _ = entry.delete_credential();
    }
    let _ = std::fs::remove_file(paths.token_file());
}

/// Gives the core fresh access tokens from the stored grant.
pub struct CliTokens {
    client: Client,
    refresh_token: String,
    http: reqwest::Client,
    cached: Mutex<Option<(String, Instant)>>,
}

impl CliTokens {
    pub fn new(client: Client, grant: &Grant) -> CliResult<Self> {
        Ok(Self {
            client,
            refresh_token: grant.refresh_token.clone(),
            http: oauth::http()?,
            cached: Mutex::new(None),
        })
    }
}

#[async_trait]
impl bothy::auth::AccessTokenProvider for CliTokens {
    async fn access_token(&self) -> bothy::Result<String> {
        if let Some((t, until)) = self
            .cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            && Instant::now() < until
        {
            return Ok(t);
        }
        let (token, ttl) = oauth::refresh(&self.http, &self.client, &self.refresh_token).await?;
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

/// The stored client of a kind (`desktop` or `device`).
pub fn client(paths: &Paths, kind: &str) -> CliResult<Client> {
    let file = paths.client_file(kind);
    let bytes = std::fs::read(&file).map_err(|_| {
        let flag = if kind == "device" { "--device-client" } else { "--client" };
        CliError::auth(format!(
            "no {kind} OAuth client configured; run `bothy auth setup {flag} <client.json>` (see `bothy-cli` README)"
        ))
    })?;
    Client::from_json(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_json_round_trip() {
        let g = Grant {
            refresh_token: "r".into(),
            scopes: vec!["a".into(), "b".into()],
            client: "desktop".into(),
        };
        let back = from_json(&to_json(&g)).unwrap();
        assert_eq!(
            (back.refresh_token, back.scopes, back.client),
            (g.refresh_token, g.scopes, g.client)
        );
    }
}
