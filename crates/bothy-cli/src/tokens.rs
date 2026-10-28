//! Where the CLI keeps its Google grant: the OS keyring, or a `0600` file where there is none
//! (headless Linux without a Secret Service). `BOTHY_TOKEN_STORE=file` forces the file (tests,
//! containers).

use bothy_oauth::{Location, TokenStore};

use crate::config::Paths;
use crate::fail::{CliError, CliResult};
use crate::oauth::{Client, Grant};

/// Gives the core fresh access tokens from the stored grant.
pub use bothy_oauth::RefreshingTokens as CliTokens;

const SERVICE: &str = "dev.bothy.cli";
const ACCOUNT: &str = "google";

fn store(paths: &Paths) -> TokenStore {
    TokenStore::new(SERVICE, &account(paths), paths.token_file())
}

/// The keyring account of the grant: `google` for the default home (as before), and
/// `google:<config dir>` for any other, so that signing in from one home leaves the others alone.
pub fn account(paths: &Paths) -> String {
    match paths.keyring_scope() {
        None => ACCOUNT.to_owned(),
        Some(scope) => format!("{ACCOUNT}:{scope}"),
    }
}

fn file_only() -> bool {
    std::env::var("BOTHY_TOKEN_STORE").is_ok_and(|v| v == "file")
}

/// Stores the grant. Returns where it went (`the system keyring` or the file path).
pub fn save(paths: &Paths, grant: &Grant) -> CliResult<String> {
    Ok(match store(paths).save(grant)? {
        Location::Keyring => "the system keyring".into(),
        Location::File(file) => {
            if !file_only() {
                eprintln!(
                    "warning: no system keyring available; the sign-in is stored in {} (readable only by you)",
                    file.display()
                );
            }
            file.display().to_string()
        }
    })
}

/// Loads the grant, if signed in.
pub fn load(paths: &Paths) -> Option<Grant> {
    store(paths).load()
}

/// Deletes the stored grant everywhere.
pub fn clear(paths: &Paths) {
    store(paths).clear();
}

/// The client of a kind (`desktop` or `device`): the one stored with `auth setup` or
/// `setup google`, else Bothy's official client when this build has it.
pub fn client(paths: &Paths, kind: &str) -> CliResult<Client> {
    match std::fs::read(paths.client_file(kind)) {
        Ok(bytes) => Ok(Client::from_json(&bytes)?),
        Err(_) => bothy_oauth::official::client(kind).ok_or_else(|| {
            let flag = if kind == "device" {
                "--device-client"
            } else {
                "--client"
            };
            CliError::auth(format!(
                "this build has no official Bothy OAuth client and no {kind} client is configured; \
                 run `bothy setup google`, or `bothy auth setup {flag} <client.json>`"
            ))
        }),
    }
}

/// The client that issued `grant`, checked against the current one: after switching between
/// the official client and one's own, the old sign-in cannot be refreshed.
pub fn client_for(paths: &Paths, grant: &Grant) -> CliResult<Client> {
    let client = client(paths, &grant.client)?;
    match &grant.client_id {
        Some(id) if *id != client.id => Err(CliError::auth(format!(
            "the sign-in was made with another OAuth client ({id}); run `bothy auth login` again"
        ))),
        _ => Ok(client),
    }
}
