//! Access tokens. Providers ask a [`TokenSource`] for a token per request
//! and tell it when the provider rejected one, so it can refresh.

use async_trait::async_trait;
use mail_domain::Redacted;

use crate::ProviderResult;

pub type AccessToken = Redacted<String>;

#[async_trait]
pub trait TokenSource: Send + Sync {
    /// A currently valid access token, refreshing if it is near expiry.
    async fn access_token(&self) -> ProviderResult<AccessToken>;
    /// The provider rejected `token` (HTTP 401); the next call must refresh.
    async fn invalidate(&self, token: &AccessToken);
}

/// A fixed token, for tests and fakes.
pub struct StaticToken(pub String);

#[async_trait]
impl TokenSource for StaticToken {
    async fn access_token(&self) -> ProviderResult<AccessToken> {
        Ok(Redacted::new(self.0.clone()))
    }
    async fn invalidate(&self, _token: &AccessToken) {}
}
