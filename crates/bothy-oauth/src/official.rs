//! The OAuth clients of Bothy's own Google Cloud project, for apps that bring none: the CLI,
//! the Python binding and the example agents use them unless given a client of their own.
//!
//! The client ids are public. The secrets of installed-app clients are not confidential either
//! (they ship inside every copy of an app), but they stay out of the repository: release builds
//! get them from `BOTHY_OFFICIAL_DESKTOP_SECRET` and `BOTHY_OFFICIAL_DEVICE_SECRET` at compile
//! time. A build without them has no official clients, and apps must bring their own.
//!
//! The consent screen shows "Bothy", and everyone using these clients shares one Drive API
//! quota. Apps of your own should use a Cloud project of their own (`bothy setup google`). All
//! devices of one database must sign in through clients of the same project.

use crate::Client;

/// The Cloud project the clients belong to.
pub const PROJECT: &str = "bothy-512819";

/// "Desktop app" client, for browser sign-in ([`crate::loopback_login`]).
pub const DESKTOP_CLIENT_ID: &str =
    "983923837909-c1rflgiktq04kghhdo9jedc3h54p8huo.apps.googleusercontent.com";

/// "TVs and Limited Input devices" client, for device sign-in ([`crate::device_login`]).
pub const DEVICE_CLIENT_ID: &str =
    "983923837909-5772i5stqa1jstj7di2rv5r75jq6ijif.apps.googleusercontent.com";

const DESKTOP_CLIENT_SECRET: Option<&str> = option_env!("BOTHY_OFFICIAL_DESKTOP_SECRET");
const DEVICE_CLIENT_SECRET: Option<&str> = option_env!("BOTHY_OFFICIAL_DEVICE_SECRET");

/// The official "Desktop app" client, if this build has it.
pub fn desktop() -> Option<Client> {
    DESKTOP_CLIENT_SECRET
        .filter(|s| !s.is_empty())
        .map(|s| Client::new(DESKTOP_CLIENT_ID, s))
}

/// The official "TVs and Limited Input devices" client, if this build has it.
pub fn device() -> Option<Client> {
    DEVICE_CLIENT_SECRET
        .filter(|s| !s.is_empty())
        .map(|s| Client::new(DEVICE_CLIENT_ID, s))
}

/// The official client of a kind, `desktop` or `device` (as in [`crate::Grant::client`]), if
/// this build has it.
pub fn client(kind: &str) -> Option<Client> {
    match kind {
        "desktop" => desktop(),
        "device" => device(),
        _ => None,
    }
}

/// Whether `client_id` is one of the official clients.
pub fn is_official(client_id: &str) -> bool {
    client_id == DESKTOP_CLIENT_ID || client_id == DEVICE_CLIENT_ID
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_belong_to_the_project_and_secrets_stay_hidden() {
        assert!(is_official(DESKTOP_CLIENT_ID) && is_official(DEVICE_CLIENT_ID));
        assert!(!is_official("123-abc.apps.googleusercontent.com"));
        assert!(client("web").is_none());
        if let Some(c) = desktop() {
            assert_eq!(c.id, DESKTOP_CLIENT_ID);
            assert!(!format!("{c:?}").contains(DESKTOP_CLIENT_SECRET.unwrap()));
        }
    }
}
