//! CLI errors and exit codes: 0 ok, 1 error, 2 usage, 3 sign-in needed, 4 offline,
//! 5 remote inconsistent or format too new.

use std::fmt;
use std::process::ExitCode;

use bothy::ErrorKind;

#[derive(Debug)]
pub struct CliError {
    pub code: u8,
    pub message: String,
}

pub type CliResult<T> = Result<T, CliError>;

impl CliError {
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            code: 2,
            message: message.into(),
        }
    }

    pub fn auth(message: impl Into<String>) -> Self {
        Self {
            code: 3,
            message: message.into(),
        }
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: message.into(),
        }
    }

    pub fn io(what: &str, e: std::io::Error) -> Self {
        Self::other(format!("{what}: {e}"))
    }

    pub fn exit_code(&self) -> ExitCode {
        ExitCode::from(self.code)
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<bothy::Error> for CliError {
    fn from(e: bothy::Error) -> Self {
        let code = match e.kind() {
            ErrorKind::InvalidInput => 2,
            ErrorKind::AuthRequired => 3,
            ErrorKind::Offline => 4,
            ErrorKind::RemoteInconsistent | ErrorKind::FormatTooNew => 5,
            _ => 1,
        };
        let mut message = e.to_string();
        if e.kind() == ErrorKind::AuthRequired {
            message.push_str("\nhint: run `bothy auth login` (or `bothy auth login --device` on a headless machine)");
        }
        Self { code, message }
    }
}
