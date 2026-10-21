//! Error type shared by every Bothy API.

use std::borrow::Cow;
use std::fmt;

/// Broad error category. Callers branch on this; the message is for humans.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// No network; retried automatically.
    Offline,
    /// No usable access token; the user has to sign in again.
    AuthRequired,
    /// Rate limit still exceeded after backing off.
    RateLimited,
    /// The remote storage quota is full.
    QuotaExceeded,
    /// The remote is in an unexpected state (missing files, forked device).
    RemoteInconsistent,
    /// Data was written by a newer format version; update the client.
    FormatTooNew,
    /// Data uses a newer application schema version.
    SchemaTooNew,
    /// The password does not unlock the database key.
    WrongPassword,
    /// Data failed an integrity or validity check.
    Corrupt,
    /// Another process has the local database open.
    DatabaseLocked,
    /// The caller passed an invalid argument.
    InvalidInput,
    /// A bug or an unexpected local failure.
    Internal,
}

impl ErrorKind {
    fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Offline => "offline",
            ErrorKind::AuthRequired => "authentication required",
            ErrorKind::RateLimited => "rate limited",
            ErrorKind::QuotaExceeded => "storage quota exceeded",
            ErrorKind::RemoteInconsistent => "remote inconsistent",
            ErrorKind::FormatTooNew => "format too new",
            ErrorKind::SchemaTooNew => "schema too new",
            ErrorKind::WrongPassword => "wrong password",
            ErrorKind::Corrupt => "corrupt data",
            ErrorKind::DatabaseLocked => "database locked",
            ErrorKind::InvalidInput => "invalid input",
            ErrorKind::Internal => "internal error",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

type Source = Box<dyn std::error::Error + Send + Sync + 'static>;

/// A Bothy error: a [`ErrorKind`], a message and an optional underlying cause.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    message: Cow<'static, str>,
    source: Option<Source>,
}

impl Error {
    /// Creates an error of the given kind.
    pub fn new(kind: ErrorKind, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    /// Attaches an underlying cause.
    pub fn with_source(mut self, source: impl Into<Source>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// The error category.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The human-readable message, without the kind prefix.
    pub fn message(&self) -> &str {
        &self.message
    }

    pub(crate) fn invalid_input(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(ErrorKind::InvalidInput, message)
    }

    pub(crate) fn corrupt(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(ErrorKind::Corrupt, message)
    }

    pub(crate) fn format_too_new(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(ErrorKind::FormatTooNew, message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|s| s as &(dyn std::error::Error + 'static))
    }
}

/// Result type used throughout Bothy.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_includes_kind_and_message() {
        let e = Error::corrupt("bad trailer");
        assert_eq!(e.kind(), ErrorKind::Corrupt);
        assert_eq!(e.to_string(), "corrupt data: bad trailer");
    }

    #[test]
    fn source_is_exposed() {
        let io = std::io::Error::other("disk");
        let e = Error::new(ErrorKind::Internal, "write failed").with_source(io);
        assert_eq!(std::error::Error::source(&e).unwrap().to_string(), "disk");
    }
}
