//! Passwords and remembered data keys for encrypted databases.
//!
//! A password comes from `BOTHY_PASSWORD` (`BOTHY_NEW_PASSWORD` for the new one in `passwd`),
//! from stdin with `--password-stdin` (one per line, in the order the command asks), or from a
//! prompt without echo. `unlock --remember` keeps the data key in the OS keyring, or in a `0600`
//! file where there is none (`BOTHY_TOKEN_STORE=file` forces the file). Neither is ever printed.

use std::collections::VecDeque;
use std::io::{BufRead, IsTerminal};
use std::sync::Mutex;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use bothy::DbId;
use bothy::crypto::DataKey;
use zeroize::Zeroizing;

use crate::config::{Paths, write_private};
use crate::fail::{CliError, CliResult};

const SERVICE: &str = "dev.bothy.cli";

pub type Password = Zeroizing<String>;

/// Which password a command asks for.
#[derive(Clone, Copy)]
pub enum Ask {
    /// The current password (`BOTHY_PASSWORD`).
    Current,
    /// A new password, typed twice at a prompt (`BOTHY_PASSWORD` for `init`).
    Create,
    /// The new password in `passwd` (`BOTHY_NEW_PASSWORD`).
    Change,
}

static STDIN_LINES: Mutex<Option<VecDeque<Password>>> = Mutex::new(None);

fn next_stdin_line() -> CliResult<Password> {
    let mut lines = STDIN_LINES.lock().unwrap_or_else(|e| e.into_inner());
    let lines = lines.get_or_insert_with(|| {
        std::io::stdin()
            .lock()
            .lines()
            .map_while(Result::ok)
            .map(|l| Zeroizing::new(l.trim_end_matches('\r').to_owned()))
            .collect()
    });
    lines
        .pop_front()
        .ok_or_else(|| CliError::usage("--password-stdin: not enough lines on stdin"))
}

/// Whether a password can be had without a prompt.
pub fn available_without_prompt(password_stdin: bool, ask: Ask) -> bool {
    password_stdin || env_var(ask).is_some()
}

fn env_var(ask: Ask) -> Option<Password> {
    let name = match ask {
        Ask::Current | Ask::Create => "BOTHY_PASSWORD",
        Ask::Change => "BOTHY_NEW_PASSWORD",
    };
    std::env::var(name).ok().map(Zeroizing::new)
}

/// Reads a password from the environment, stdin or a prompt.
pub fn read(password_stdin: bool, ask: Ask) -> CliResult<Password> {
    let pw = if let Some(p) = env_var(ask) {
        p
    } else if password_stdin {
        next_stdin_line()?
    } else if std::io::stdin().is_terminal() {
        let prompt = match ask {
            Ask::Current => "password: ",
            Ask::Create => "new password: ",
            Ask::Change => "new password: ",
        };
        let first = prompt_hidden(prompt)?;
        if !matches!(ask, Ask::Current) && *prompt_hidden("repeat it: ")? != *first {
            return Err(CliError::usage("the passwords do not match"));
        }
        first
    } else {
        return Err(CliError::usage(
            "a password is needed: set BOTHY_PASSWORD, pass --password-stdin, or run in a terminal",
        ));
    };
    if pw.is_empty() {
        return Err(CliError::usage("the password is empty"));
    }
    Ok(pw)
}

fn prompt_hidden(prompt: &str) -> CliResult<Password> {
    rpassword::prompt_password(prompt)
        .map(Zeroizing::new)
        .map_err(|e| CliError::io("cannot read the password", e))
}

fn file_only() -> bool {
    std::env::var("BOTHY_TOKEN_STORE").is_ok_and(|v| v == "file")
}

fn account(db_id: DbId) -> String {
    format!("key:{db_id}")
}

/// Keeps the data key of `db_id` on this machine. Returns where it went.
pub fn remember(paths: &Paths, db_id: DbId, key: &DataKey) -> CliResult<String> {
    let encoded = Zeroizing::new(B64.encode(key.to_bytes().as_slice()));
    if !file_only()
        && let Ok(entry) = keyring::Entry::new(SERVICE, &account(db_id))
        && entry.set_password(&encoded).is_ok()
    {
        let _ = std::fs::remove_file(paths.key_file(db_id));
        return Ok("the system keyring".into());
    }
    let file = paths.key_file(db_id);
    write_private(&file, encoded.as_bytes())?;
    if !file_only() {
        eprintln!(
            "warning: no system keyring available; the data key is stored in {} (readable only by you)",
            file.display()
        );
    }
    Ok(file.display().to_string())
}

/// The remembered data key of `db_id`, if any.
pub fn remembered(paths: &Paths, db_id: DbId) -> Option<DataKey> {
    let text = if !file_only()
        && let Ok(entry) = keyring::Entry::new(SERVICE, &account(db_id))
        && let Ok(s) = entry.get_password()
    {
        Zeroizing::new(s)
    } else {
        Zeroizing::new(std::fs::read_to_string(paths.key_file(db_id)).ok()?)
    };
    let bytes = Zeroizing::new(B64.decode(text.trim()).ok()?);
    let bytes: [u8; 32] = bytes.as_slice().try_into().ok()?;
    Some(DataKey::from_bytes(bytes))
}

/// Forgets the data key of `db_id`. Returns whether one was stored.
pub fn forget(paths: &Paths, db_id: DbId) -> bool {
    let mut found = false;
    if !file_only()
        && let Ok(entry) = keyring::Entry::new(SERVICE, &account(db_id))
    {
        found |= entry.delete_credential().is_ok();
    }
    found |= std::fs::remove_file(paths.key_file(db_id)).is_ok();
    found
}
