//! HTTP with auth, rate limiting and retries (spec §7.1).
//!
//! - Every request takes quota from the [`RateLimiter`] first.
//! - 401: invalidate the token, refresh, retry once; then `Unauthorized`.
//! - 429, 5xx, network errors and Google's 403 `rateLimitExceeded`: retry
//!   with exponential backoff and full jitter, honoring `Retry-After`.
//! - 404 and other 4xx: fail immediately, classified.

use std::sync::Arc;
use std::time::Duration;

use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;

use crate::rate_limit::{Priority, RateLimiter};
use crate::token::TokenSource;
use crate::{ProviderError, ProviderResult};

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 5, base_delay: Duration::from_millis(500), max_delay: Duration::from_secs(60) }
    }
}

impl RetryPolicy {
    fn backoff(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        if let Some(after) = retry_after {
            return after.min(self.max_delay);
        }
        let exp = self.base_delay.saturating_mul(1u32 << attempt.min(16));
        let capped = exp.min(self.max_delay);
        // Full jitter: uniform in [0, capped].
        capped.mul_f64(fastrand::f64())
    }
}

#[derive(Clone)]
pub struct HttpClient {
    client: Client,
    tokens: Arc<dyn TokenSource>,
    limiter: Arc<RateLimiter>,
    retry: RetryPolicy,
}

impl HttpClient {
    pub fn new(tokens: Arc<dyn TokenSource>, limiter: Arc<RateLimiter>, retry: RetryPolicy) -> ProviderResult<Self> {
        let client = Client::builder()
            .user_agent(concat!("OpenAGC/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .gzip(true)
            .https_only(false) // tests talk to a local mock; providers pass https URLs
            .build()
            .map_err(|e| ProviderError::Network(e.to_string()))?;
        Ok(Self { client, tokens, limiter, retry })
    }

    pub fn limiter(&self) -> &RateLimiter {
        &self.limiter
    }

    /// Send and decode a JSON response.
    pub async fn json<T: DeserializeOwned>(
        &self,
        cost: u32,
        priority: Priority,
        build: impl Fn(&Client) -> RequestBuilder,
    ) -> ProviderResult<T> {
        let response = self.execute(cost, priority, build).await?;
        response.json::<T>().await.map_err(|e| ProviderError::Decode(e.to_string()))
    }

    /// Send and return the raw body with its content type.
    pub async fn bytes(
        &self,
        cost: u32,
        priority: Priority,
        build: impl Fn(&Client) -> RequestBuilder,
    ) -> ProviderResult<(Vec<u8>, Option<String>)> {
        let response = self.execute(cost, priority, build).await?;
        let content_type =
            response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let body = response.bytes().await.map_err(|e| ProviderError::Network(e.to_string()))?;
        Ok((body.to_vec(), content_type))
    }

    /// Send, expecting no useful body.
    pub async fn empty(
        &self,
        cost: u32,
        priority: Priority,
        build: impl Fn(&Client) -> RequestBuilder,
    ) -> ProviderResult<()> {
        self.execute(cost, priority, build).await.map(|_| ())
    }

    async fn execute(
        &self,
        cost: u32,
        priority: Priority,
        build: impl Fn(&Client) -> RequestBuilder,
    ) -> ProviderResult<Response> {
        let mut attempt = 0;
        let mut refreshed = false;
        loop {
            self.limiter.acquire(cost, priority).await;
            let token = self.tokens.access_token().await?;
            let request = build(&self.client).bearer_auth(token.expose());
            tracing::debug!(cost, attempt, "provider request");
            let result = request.send().await;
            tracing::debug!(ok = result.is_ok(), "provider response");
            let error = match result {
                Err(e) => ProviderError::Network(e.to_string()),
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) if response.status() == StatusCode::UNAUTHORIZED => {
                    if refreshed {
                        return Err(ProviderError::Unauthorized);
                    }
                    self.tokens.invalidate(&token).await;
                    refreshed = true;
                    continue;
                }
                Ok(response) => classify(response).await,
            };
            if !error.is_transient() || attempt + 1 >= self.retry.max_attempts {
                return Err(error);
            }
            attempt += 1;
            if let ProviderError::RateLimited { retry_after } = &error {
                // Every caller pauses, not just this one; acquire() waits.
                tracing::warn!(attempt, ?retry_after, "provider rate limit; pausing all requests");
                self.limiter.report_rate_limited(*retry_after).await;
                continue;
            }
            let delay = self.retry.backoff(attempt - 1, None);
            tracing::warn!(attempt, ?delay, %error, "retrying provider request");
            tokio::time::sleep(delay).await;
        }
    }
}

/// Turn a non-success response into a classified error, reading Google's
/// JSON error body when present.
async fn classify(response: Response) -> ProviderError {
    let status = response.status();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let body = response.text().await.unwrap_or_default();
    let (message, reasons) = google_error(&body);
    if status.as_u16() == 429 || (status.as_u16() == 403 && reasons.iter().any(|r| r.contains("ateLimit"))) {
        tracing::warn!(status = status.as_u16(), ?retry_after, ?reasons, %message, "rate limited by provider");
    }
    match status.as_u16() {
        429 => ProviderError::RateLimited { retry_after },
        403 if reasons.iter().any(|r| r.contains("RateLimitExceeded") || r == "rateLimitExceeded") => {
            ProviderError::RateLimited { retry_after: retry_after.or(Some(Duration::from_secs(60))) }
        }
        403 => ProviderError::Forbidden(message),
        404 => ProviderError::NotFound(message),
        s if s >= 500 => ProviderError::Server { status: s, message },
        _ => ProviderError::Invalid(format!("HTTP {}: {message}", status.as_u16())),
    }
}

/// `{"error": {"message": "...", "errors": [{"reason": "..."}]}}`
fn google_error(body: &str) -> (String, Vec<String>) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return (body.chars().take(200).collect(), vec![]);
    };
    let err = &v["error"];
    let message = err["message"].as_str().unwrap_or_default().to_owned();
    let reasons = err["errors"]
        .as_array()
        .map(|a| a.iter().filter_map(|e| e["reason"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    (message, reasons)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use async_trait::async_trait;
    use mail_domain::Redacted;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::token::{AccessToken, StaticToken};

    fn fast() -> RetryPolicy {
        RetryPolicy { max_attempts: 4, base_delay: Duration::from_millis(1), max_delay: Duration::from_millis(20) }
    }

    fn client(tokens: Arc<dyn TokenSource>) -> HttpClient {
        HttpClient::new(tokens, Arc::new(RateLimiter::new(60_000, 0)), fast()).unwrap()
    }

    #[derive(serde::Deserialize, Debug, PartialEq)]
    struct Profile {
        email: String,
    }

    #[tokio::test]
    async fn sends_the_bearer_token_and_decodes_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/profile"))
            .and(header("authorization", "Bearer abc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"email": "me@example.com"})))
            .mount(&server)
            .await;
        let http = client(Arc::new(StaticToken("abc".into())));
        let p: Profile =
            http.json(1, Priority::Interactive, |c| c.get(format!("{}/profile", server.uri()))).await.unwrap();
        assert_eq!(p.email, "me@example.com");
    }

    /// Hands out t1 until invalidated, then t2.
    struct Rotating {
        current: Mutex<String>,
        invalidations: AtomicU32,
    }

    #[async_trait]
    impl TokenSource for Rotating {
        async fn access_token(&self) -> ProviderResult<AccessToken> {
            Ok(Redacted::new(self.current.lock().unwrap().clone()))
        }
        async fn invalidate(&self, _token: &AccessToken) {
            self.invalidations.fetch_add(1, Ordering::SeqCst);
            *self.current.lock().unwrap() = "t2".into();
        }
    }

    #[tokio::test]
    async fn a_401_refreshes_the_token_once_and_retries() {
        let server = MockServer::start().await;
        Mock::given(header("authorization", "Bearer t1")).respond_with(ResponseTemplate::new(401)).mount(&server).await;
        Mock::given(header("authorization", "Bearer t2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"email": "x@example.com"})))
            .mount(&server)
            .await;
        let tokens = Arc::new(Rotating { current: Mutex::new("t1".into()), invalidations: AtomicU32::new(0) });
        let http = client(tokens.clone());
        let p: Profile = http.json(1, Priority::Interactive, |c| c.get(server.uri())).await.unwrap();
        assert_eq!(p.email, "x@example.com");
        assert_eq!(tokens.invalidations.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_second_401_is_unauthorized() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(401)).mount(&server).await;
        let tokens = Arc::new(Rotating { current: Mutex::new("t1".into()), invalidations: AtomicU32::new(0) });
        let err = client(tokens).empty(1, Priority::Interactive, |c| c.get(server.uri())).await.unwrap_err();
        assert_eq!(err, ProviderError::Unauthorized);
    }

    #[tokio::test]
    async fn rate_limits_and_server_errors_are_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "error": {"code": 403, "message": "Rate", "errors": [{"reason": "userRateLimitExceeded"}]}
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(503)).up_to_n_times(1).mount(&server).await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"email": "ok@example.com"})))
            .mount(&server)
            .await;
        let http = client(Arc::new(StaticToken("t".into())));
        let p: Profile = http.json(1, Priority::Background, |c| c.get(server.uri())).await.unwrap();
        assert_eq!(p.email, "ok@example.com");
        assert_eq!(server.received_requests().await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn persistent_server_errors_give_up_after_max_attempts() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
        let err = client(Arc::new(StaticToken("t".into())))
            .empty(1, Priority::Background, |c| c.get(server.uri()))
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Server { status: 500, .. }), "{err:?}");
        assert_eq!(server.received_requests().await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn not_found_and_permission_errors_are_not_retried() {
        let server = MockServer::start().await;
        Mock::given(path("/missing"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(serde_json::json!({"error": {"message": "Requested entity was not found."}})),
            )
            .mount(&server)
            .await;
        Mock::given(path("/forbidden"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({"error": {"message": "Insufficient Permission", "errors": [{"reason": "insufficientPermissions"}]}})))
            .mount(&server)
            .await;
        let http = client(Arc::new(StaticToken("t".into())));
        let err =
            http.empty(1, Priority::Interactive, |c| c.get(format!("{}/missing", server.uri()))).await.unwrap_err();
        assert_eq!(err, ProviderError::NotFound("Requested entity was not found.".into()));
        let err =
            http.empty(1, Priority::Interactive, |c| c.get(format!("{}/forbidden", server.uri()))).await.unwrap_err();
        assert_eq!(err, ProviderError::Forbidden("Insufficient Permission".into()));
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[test]
    fn backoff_is_capped_and_honors_retry_after() {
        let p =
            RetryPolicy { max_attempts: 5, base_delay: Duration::from_millis(500), max_delay: Duration::from_secs(8) };
        for attempt in 0..10 {
            assert!(p.backoff(attempt, None) <= Duration::from_secs(8));
        }
        assert_eq!(p.backoff(0, Some(Duration::from_secs(3))), Duration::from_secs(3));
        assert_eq!(p.backoff(0, Some(Duration::from_secs(300))), Duration::from_secs(8));
    }
}
