//! Access tokens for remote backends. OAuth flows live outside the core (CLI, app, binding);
//! the core only asks for a token and reports a rejected one.

use async_trait::async_trait;

use crate::error::Result;

/// Supplies OAuth access tokens to a backend.
#[async_trait]
pub trait AccessTokenProvider: Send + Sync + 'static {
    /// A valid access token. Fails with `AuthRequired` if the user must sign in again.
    async fn access_token(&self) -> Result<String>;

    /// Called once when the backend rejected `token` (HTTP 401); the provider should refresh it.
    async fn invalidate(&self, rejected: &str);
}

/// A fixed token: for tests and short-lived tools.
#[derive(Clone)]
pub struct StaticToken(String);

impl StaticToken {
    /// Wraps a token.
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }
}

impl std::fmt::Debug for StaticToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the token itself.
        f.write_str("StaticToken(..)")
    }
}

#[async_trait]
impl AccessTokenProvider for StaticToken {
    async fn access_token(&self) -> Result<String> {
        Ok(self.0.clone())
    }

    async fn invalidate(&self, _rejected: &str) {}
}
