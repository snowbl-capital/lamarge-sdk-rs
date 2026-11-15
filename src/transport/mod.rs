//! HTTP transport: one request path for every API operation, with the SDK's retry policy,
//! timeouts, response cap, error decoding and tracing.

pub(crate) mod ops;
pub(crate) mod tls;

#[cfg(test)]
mod tests;

use std::{fmt, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use reqwest::header::{self, HeaderMap, HeaderValue};
use serde::{Serialize, de::DeserializeOwned};
use tracing::Instrument as _;

use crate::{
    Environment, Error, LighterReads, Result, RetryPolicy,
    error::{ApiError, DecodeError, TransportError, VenueApiError},
    types::VenueId,
};
use ops::{AuthTier, Op};

/// The most of an answer that is read.
pub(crate) const MAX_ANSWER_BYTES: usize = 16 << 20;
/// Lighter's public API, read directly so account data does not depend on LaMarge.
pub(crate) const LIGHTER_API: &str = "https://mainnet.zklighter.elliot.ai";
/// Deadline of a Lighter public API read.
const LIGHTER_TIMEOUT: Duration = Duration::from_secs(10);

/// The SDK's clock; tests inject a fixed one.
pub(crate) type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// State shared by every handle of one `Client`.
pub(crate) struct Inner {
    pub(crate) http: reqwest::Client,
    /// API base URL without a trailing slash; may carry a path.
    pub(crate) api: String,
    /// The app origin SIWE challenges must name.
    pub(crate) origin: String,
    /// Lighter's public API base; only crate tests change it.
    pub(crate) lighter_api: String,
    /// The caller's carrier of Lighter reads, if any.
    pub(crate) lighter_reads: Option<Arc<dyn LighterReads>>,
    /// TLS for the event WebSocket (ALPN `http/1.1`), same roots and provider as HTTP.
    pub(crate) ws_tls: Arc<rustls::ClientConfig>,
    pub(crate) retry: RetryPolicy,
    pub(crate) environment: Environment,
    pub(crate) clock: Clock,
}

impl Inner {
    /// The current time on the client's clock.
    pub(crate) fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }
}

impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inner")
            .field("api", &self.api)
            .field("environment", &self.environment)
            .finish_non_exhaustive()
    }
}

/// A source of bearer tokens, read at every attempt so a renewed session applies to requests
/// already in flight.
pub(crate) trait BearerToken: Send + Sync {
    /// The token to send now; an error stops the request before anything is sent (for example
    /// a signed-out session).
    fn bearer(&self) -> Result<String>;

    /// Wakes when the token is renewed or signed out, so live streams reconnect or end; `None`
    /// for a token that never changes.
    fn changes(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        None
    }
}

/// The credential a request carries: `None` for a public request. Streams that outlive their
/// handle clone the `Arc`.
pub(crate) type Auth<'a> = Option<&'a Arc<dyn BearerToken>>;

/// The `authorization` header of `auth`, marked sensitive.
fn bearer_header(auth: Auth<'_>) -> Result<Option<HeaderValue>> {
    let Some(source) = auth else { return Ok(None) };
    let mut value = HeaderValue::try_from(format!("Bearer {}", source.bearer()?))
        .map_err(|_| Error::InvalidRequest("token is not a valid header value".into()))?;
    value.set_sensitive(true);
    Ok(Some(value))
}

/// One API request: an operation with its path segments, query and body.
#[derive(Debug)]
pub(crate) struct Call<'a> {
    op: &'a Op,
    segments: Vec<String>,
    query: Vec<(&'static str, String)>,
    body: Option<Vec<u8>>,
}

impl<'a> Call<'a> {
    pub(crate) fn new(op: &'a Op) -> Self {
        Self { op, segments: Vec::new(), query: Vec::new(), body: None }
    }

    /// Fills the next `{…}` placeholder of the path; percent-encoded when sent.
    pub(crate) fn segment(mut self, value: impl fmt::Display) -> Self {
        self.segments.push(value.to_string());
        self
    }

    /// Adds a query parameter.
    pub(crate) fn query(mut self, name: &'static str, value: impl fmt::Display) -> Self {
        self.query.push((name, value.to_string()));
        self
    }

    /// Sets the JSON body, serialized once and resent unchanged on retries.
    pub(crate) fn json(mut self, body: &impl Serialize) -> Result<Self> {
        let bytes = serde_json::to_vec(body)
            .map_err(|error| Error::InvalidRequest(format!("request body: {error}").into()))?;
        self.body = Some(bytes);
        Ok(self)
    }

    /// The path with its placeholders filled.
    fn path(&self) -> Result<String> {
        let mut path = String::with_capacity(self.op.path.len());
        let mut segments = self.segments.iter();
        let mut rest = self.op.path;
        while let Some(start) = rest.find('{') {
            let end = rest[start..].find('}').map_or(rest.len(), |end| start + end + 1);
            let segment = segments.next().ok_or_else(|| {
                Error::InvalidRequest(format!("{}: missing path parameter", self.op.id).into())
            })?;
            path.push_str(&rest[..start]);
            percent_encode(segment, &mut path);
            rest = &rest[end..];
        }
        if segments.next().is_some() {
            return Err(Error::InvalidRequest(
                format!("{}: extra path parameter", self.op.id).into(),
            ));
        }
        path.push_str(rest);
        Ok(path)
    }
}

/// Appends `segment` with every byte outside RFC 3986's unreserved set percent-encoded.
fn percent_encode(segment: &str, out: &mut String) {
    use fmt::Write as _;
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
}

/// An answer, read up to [`MAX_ANSWER_BYTES`].
struct Answer {
    status: u16,
    request_id: Option<String>,
    retry_after: Option<Duration>,
    body: Vec<u8>,
}

impl Inner {
    /// Sends `call`, retrying per the client's policy when the operation allows it, and decodes
    /// a 2xx answer (or a status the operation lists as an answer) as `T`; a 204 decodes as `()`.
    pub(crate) async fn send<T: DeserializeOwned>(
        &self,
        call: Call<'_>,
        auth: Auth<'_>,
    ) -> Result<T> {
        let op = call.op;
        debug_assert!(
            auth.is_some() || matches!(op.auth, AuthTier::Public | AuthTier::OptionalSession),
            "{} needs a credential",
            op.id
        );
        let url = format!("{}{}", self.api, call.path()?);
        let span = tracing::info_span!(
            "lamarge.http",
            otel.name = op.id,
            http.request.method = op.method.as_str(),
            url.path = op.path,
            http.response.status_code = tracing::field::Empty,
            lamarge.request_id = tracing::field::Empty,
            lamarge.attempt = tracing::field::Empty,
        );
        async {
            let attempts = if op.retry { self.retry.max_attempts() } else { 1 };
            let mut attempt = 0;
            loop {
                tracing::Span::current().record("lamarge.attempt", attempt + 1);
                let error = match self.attempt(&call, &url, auth).await {
                    Ok(answer) => match decode_answer(op, answer) {
                        Ok(value) => return Ok(value),
                        Err(error) => error,
                    },
                    Err(error) => error,
                };
                attempt += 1;
                let Some(delay) = (attempt < attempts)
                    .then(|| retry_delay(&self.retry, attempt - 1, &error))
                    .flatten()
                else {
                    return Err(error);
                };
                tracing::debug!(
                    delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                    "retrying"
                );
                tokio::time::sleep(delay).await;
            }
        }
        .instrument(span)
        .await
    }

    /// One attempt: the request, then the answer read up to the cap. `Err` only for transport
    /// failures (retryable), a refused bearer source or an oversized answer.
    async fn attempt(&self, call: &Call<'_>, url: &str, auth: Auth<'_>) -> Result<Answer> {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        if let Some(value) = bearer_header(auth)? {
            headers.insert(header::AUTHORIZATION, value);
        }
        let mut request = self.http.request(call.op.method.as_reqwest(), url).headers(headers);
        if !call.query.is_empty() {
            request = request.query(&call.query);
        }
        if let Some(body) = &call.body {
            request = request.header(header::CONTENT_TYPE, "application/json").body(body.clone());
        }
        let response =
            request.send().await.map_err(|error| Error::Transport(TransportError::http(error)))?;
        let status = response.status().as_u16();
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let span = tracing::Span::current();
        span.record("http.response.status_code", status);
        if let Some(id) = &request_id {
            span.record("lamarge.request_id", id.as_str());
        }
        let retry_after = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = read_capped(response).await.map_err(|error| match error {
            ReadError::Transport(error) => Error::Transport(TransportError::http(error)),
            ReadError::TooLarge => Error::Decode(Box::new(DecodeError::too_large(
                Some(status),
                request_id.clone(),
                MAX_ANSWER_BYTES,
            ))),
        })?;
        Ok(Answer { status, request_id, retry_after, body })
    }

    /// GET on Lighter's public API (`path` with its query), within ten seconds, never retried;
    /// any failure is an [`Error::VenueApi`].
    pub(crate) async fn lighter_get<T: DeserializeOwned>(&self, path_and_query: &str) -> Result<T> {
        let url = format!("{}{path_and_query}", self.lighter_api);
        let body = match &self.lighter_reads {
            Some(reads) => self.lighter_read_by(reads.as_ref(), &url).await,
            None => self.lighter_read(&url).await,
        }
        .map_err(Error::VenueApi)?;
        serde_json::from_slice(&body)
            .map_err(|_| Error::VenueApi(VenueApiError::unreadable(VenueId::Lighter)))
    }

    async fn lighter_read(&self, url: &str) -> std::result::Result<Vec<u8>, VenueApiError> {
        let unreachable =
            |error| VenueApiError::unreachable(VenueId::Lighter, TransportError::http(error));
        let response = self
            .http
            .get(url)
            .header(header::ACCEPT, "application/json")
            .timeout(LIGHTER_TIMEOUT)
            .send()
            .await
            .map_err(unreachable)?;
        let status = response.status();
        if !status.is_success() {
            return Err(VenueApiError::answered(VenueId::Lighter, status.as_u16()));
        }
        read_capped(response).await.map_err(|error| match error {
            ReadError::Transport(error) => unreachable(error),
            ReadError::TooLarge => VenueApiError::unreadable(VenueId::Lighter),
        })
    }

    async fn lighter_read_by(
        &self,
        reads: &dyn LighterReads,
        url: &str,
    ) -> std::result::Result<Vec<u8>, VenueApiError> {
        let unreachable = |error| VenueApiError::unreachable(VenueId::Lighter, error);
        let (status, body) = tokio::time::timeout(LIGHTER_TIMEOUT, reads.get(url))
            .await
            .map_err(|_| unreachable(TransportError::timeout()))?
            .map_err(|error| unreachable(TransportError::caller(error)))?;
        if !(200..300).contains(&status) {
            return Err(VenueApiError::answered(VenueId::Lighter, status));
        }
        if body.len() > MAX_ANSWER_BYTES {
            return Err(VenueApiError::unreadable(VenueId::Lighter));
        }
        Ok(body)
    }
}

/// Decodes a 2xx answer (or one the operation lists) as `T`, any other as an [`ApiError`].
fn decode_answer<T: DeserializeOwned>(op: &Op, answer: Answer) -> Result<T> {
    let Answer { status, request_id, retry_after, body } = answer;
    if (200..300).contains(&status) || op.answers.contains(&status) {
        let json: &[u8] = if body.is_empty() { b"null" } else { &body };
        return serde_json::from_slice(json).map_err(|error| {
            Error::Decode(Box::new(DecodeError::json(
                Some(status),
                request_id,
                &body,
                &error,
                op.secret,
            )))
        });
    }
    Err(Error::Api(Box::new(ApiError::from_answer(status, &body, request_id, retry_after))))
}

/// The wait before retrying after failed attempt `failed` (0 for the first), or `None` when
/// `error` is not retryable or the server asks for a longer wait than the policy allows.
///
/// Without `Retry-After`: full jitter below `min(base · 2^failed, max)`, so clients that failed
/// together come back apart.
fn retry_delay(policy: &RetryPolicy, failed: u32, error: &Error) -> Option<Duration> {
    if !error.is_retryable() {
        return None;
    }
    if let Some(wait) = error.retry_after() {
        return (wait <= policy.max_delay()).then_some(wait);
    }
    let ceiling =
        policy.base_delay().saturating_mul(2u32.saturating_pow(failed)).min(policy.max_delay());
    Some(jitter(ceiling))
}

/// A wait uniform in `[0, below)`; zero when `below` is.
pub(crate) fn jitter(below: Duration) -> Duration {
    let nanos = u64::try_from(below.as_nanos()).unwrap_or(u64::MAX);
    Duration::from_nanos(if nanos == 0 { 0 } else { fastrand::u64(0..nanos) })
}

enum ReadError {
    Transport(reqwest::Error),
    TooLarge,
}

/// Reads the body chunk by chunk, refusing more than [`MAX_ANSWER_BYTES`].
async fn read_capped(mut response: reqwest::Response) -> Result<Vec<u8>, ReadError> {
    if response.content_length().is_some_and(|length| length > MAX_ANSWER_BYTES as u64) {
        return Err(ReadError::TooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(ReadError::Transport)? {
        if body.len() + chunk.len() > MAX_ANSWER_BYTES {
            return Err(ReadError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
