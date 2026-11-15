//! The API client: environment, configuration and the public resource handles.

use std::{borrow::Cow, fmt, future::Future, pin::Pin, sync::Arc, time::Duration};

use reqwest::{Url, header::HeaderValue};

use alloy_signer::Signer;

use crate::{
    Error, HttpExchange, MakerCredential, Quoter, Result, SavedSession, Session, StreamConnector,
    api,
    model::Venue,
    protocol::{checks::deployment::check_deployment, signing::SigningKey},
    store::Journal,
    transport::{self, Clock, Inner},
    types::EscrowEnvironment,
};

const USER_AGENT: &str = concat!("lamarge-rust/", env!("CARGO_PKG_VERSION"));

/// A LaMarge deployment: where the API is and which app origin sign-in challenges name.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Environment {
    /// API `https://api.lamarge.trade`, sign-in origin `https://app.lamarge.trade`, production
    /// escrows.
    Production,
    /// API `https://api.dev.lamarge.trade` (tailnet), sign-in origin
    /// `https://app.dev.lamarge.trade`, development escrows.
    Development,
    /// Any API, its escrow environment resolved from its venues. `https` is required except for
    /// `localhost`, `127.0.0.1` and `[::1]`.
    Custom {
        /// The API base URL; it may carry a path.
        api_url: String,
        /// The app origin its sign-in challenges name (`scheme://host[:port]`).
        app_origin: String,
    },
}

impl Environment {
    fn urls(&self) -> (&str, &str) {
        match self {
            Self::Production => ("https://api.lamarge.trade", "https://app.lamarge.trade"),
            Self::Development => ("https://api.dev.lamarge.trade", "https://app.dev.lamarge.trade"),
            Self::Custom { api_url, app_origin } => (api_url, app_origin),
        }
    }

    /// The escrow environment a named environment requires; `None` for a custom one.
    fn escrow_environment(&self) -> Option<EscrowEnvironment> {
        match self {
            Self::Production => Some(EscrowEnvironment::Production),
            Self::Development => Some(EscrowEnvironment::Development),
            Self::Custom { .. } => None,
        }
    }
}

/// How transient failures of retryable requests are retried: up to `max_attempts` attempts,
/// waiting the server's `Retry-After` or a jittered exponential backoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    max_attempts: u32,
    base_delay: Duration,
    max_delay: Duration,
}

impl RetryPolicy {
    /// `max_attempts` counts the first attempt (0 is taken as 1). Backoff waits are uniform in
    /// `[0, min(base_delay · 2^n, max_delay))` after the `n`-th failure (from 0); a `Retry-After`
    /// above `max_delay` returns the error instead of waiting.
    pub fn new(max_attempts: u32, base_delay: Duration, max_delay: Duration) -> Self {
        Self { max_attempts: max_attempts.max(1), base_delay, max_delay }
    }

    /// No retries.
    pub const fn none() -> Self {
        Self { max_attempts: 1, base_delay: Duration::ZERO, max_delay: Duration::ZERO }
    }

    pub(crate) const fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    pub(crate) const fn base_delay(&self) -> Duration {
        self.base_delay
    }

    pub(crate) const fn max_delay(&self) -> Duration {
        self.max_delay
    }
}

impl Default for RetryPolicy {
    /// 3 attempts, 500 ms base, 5 s cap.
    fn default() -> Self {
        Self::new(3, Duration::from_millis(500), Duration::from_secs(5))
    }
}

/// A LaMarge API client. Cheap to clone; clones share connections and configuration.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl Client {
    /// A client with the default configuration.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the TLS setup fails, or for a custom environment's invalid URL.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn run() -> lamarge::Result<()> {
    /// let client = lamarge::Client::new(lamarge::Environment::Production)?;
    /// let venues = client.market().venues().await?;
    /// # Ok(()) }
    /// ```
    pub fn new(environment: Environment) -> Result<Self> {
        Self::builder(environment).build()
    }

    /// A builder to configure timeouts, retries and the user agent.
    pub fn builder(environment: Environment) -> ClientBuilder {
        ClientBuilder {
            environment,
            timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            retry: RetryPolicy::default(),
            user_agent_suffix: None,
            clock: None,
            lighter_api: None,
            lighter_reads: None,
            http_exchange: None,
            stream_connector: None,
        }
    }

    /// The environment the client was built for.
    pub fn environment(&self) -> &Environment {
        &self.inner.environment
    }

    /// Public market data.
    pub fn market(&self) -> api::Market<'_> {
        api::Market { inner: &self.inner, auth: None }
    }

    /// The public explorer: every user's transfers and trades.
    pub fn explorer(&self) -> api::Explorer<'_> {
        api::Explorer { inner: &self.inner, auth: None }
    }

    /// The public event journal.
    pub fn events(&self) -> api::PublicEvents<'_> {
        api::PublicEvents { inner: &self.inner, auth: None }
    }

    /// Reads the venues (`GET /v1/venues`) and checks every chain they list against the
    /// deployments this release pins; returns the escrow environment they belong to.
    ///
    /// Fails with [`Error::Verification`] (kind `Deployment`) when a chain differs from the pins,
    /// the escrows span environments or are not the named environment's, or no escrow venue is
    /// enabled.
    pub async fn check_deployment(&self) -> Result<EscrowEnvironment> {
        let venues: Vec<Venue> = self.market().venues().await?;
        Ok(check_deployment(&venues, self.inner.environment.escrow_environment())?)
    }

    /// Signs in with Sign-In with Ethereum: the deployment check and the challenge are asked
    /// together and the deployment decides first; the challenge must name this environment's
    /// app origin, the signer's address and a current window before it is signed
    /// (`personal_sign`).
    ///
    /// `journal` is where the session's flows save signatures before submitting them: a
    /// [`Journal`], or any [`Store`](crate::store::Store) (wrapped in a new journal). Sessions
    /// that share persistence must share one journal (clone it), so they share its locks.
    ///
    /// # Errors
    ///
    /// - [`Error::Verification`]: the deployment differs from the pins, or the challenge names
    ///   another origin, address or window; nothing was signed.
    /// - [`Error::Signer`]: the signer failed or signed with another key.
    /// - [`Error::Api`], [`Error::Transport`], [`Error::Decode`]: the API refused or was not
    ///   reached.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn run(signer: alloy_signer_local::PrivateKeySigner) -> lamarge::Result<()> {
    /// use lamarge::{Client, Environment, store::FileStore};
    ///
    /// let client = Client::new(Environment::Production)?;
    /// let session = client.sign_in(signer, FileStore::open("lamarge-journal")?).await?;
    /// let saved = session.export(); // persist it to resume later
    /// # Ok(()) }
    /// ```
    pub async fn sign_in<S, J>(&self, signer: S, journal: J) -> Result<Session>
    where
        S: Signer + Send + Sync + 'static,
        J: Into<Journal>,
    {
        Session::sign_in(self, SigningKey::new(signer), journal.into()).await
    }

    /// Resumes a saved session after the deployment check. `journal` as in
    /// [`sign_in`](Self::sign_in).
    ///
    /// # Errors
    ///
    /// - [`Error::Verification`]: the deployment differs from the pins, or `GET /v1/me` names
    ///   another address than the signer's.
    /// - [`Error::Api`] (`401` for an expired or revoked session), [`Error::Transport`],
    ///   [`Error::Decode`].
    pub async fn resume_session<S, J>(
        &self,
        saved: SavedSession,
        signer: S,
        journal: J,
    ) -> Result<Session>
    where
        S: Signer + Send + Sync + 'static,
        J: Into<Journal>,
    {
        Session::resume(self, saved, SigningKey::new(signer), journal.into()).await
    }

    /// A quoter for `credential`, signing with `quote_signer`; the deployment is checked.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidRequest`], before any request: `quote_signer` is not the credential's
    ///   quote signer.
    /// - [`Error::Verification`]: the deployment differs from the pins.
    /// - [`Error::Api`], [`Error::Transport`], [`Error::Decode`].
    pub async fn quoter<S>(&self, credential: MakerCredential, quote_signer: S) -> Result<Quoter>
    where
        S: Signer + Send + Sync + 'static,
    {
        Quoter::start(self, credential, SigningKey::new(quote_signer)).await
    }

    pub(crate) fn inner(&self) -> &Arc<Inner> {
        &self.inner
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("environment", &self.inner.environment)
            .field("api", &self.inner.api)
            .finish_non_exhaustive()
    }
}

/// Performs the client's reads of Lighter's public API in place of its own HTTP client, so they
/// leave from the caller's address and count in the caller's rate budget there.
///
/// The SDK reads Lighter directly to learn which address owns an account index before it signs
/// a take delivering to, or paying from, Lighter. Set with [`ClientBuilder::lighter_reads`].
///
/// ```
/// use lamarge::{LighterRead, LighterReads};
///
/// /// Lighter reads from the HTTP client bound to this account's address.
/// struct FromAddress(reqwest::Client);
///
/// impl LighterReads for FromAddress {
///     fn get<'a>(&'a self, url: &'a str) -> LighterRead<'a> {
///         Box::pin(async move {
///             let answer = self.0.get(url).send().await?;
///             let status = answer.status().as_u16();
///             Ok((status, answer.bytes().await?.to_vec()))
///         })
///     }
/// }
/// ```
pub trait LighterReads: Send + Sync {
    /// Sends `GET url` (Lighter's public API, query included; the SDK expects JSON) and returns
    /// the answer's status and body. An `Err` means no answer was received.
    ///
    /// The SDK applies its own 10-second deadline, never retries, refuses any status other than
    /// 2xx and reads at most 16 MiB of the body.
    fn get<'a>(&'a self, url: &'a str) -> LighterRead<'a>;
}

/// One carrier shared by several clients.
impl<T: LighterReads + ?Sized> LighterReads for Arc<T> {
    fn get<'a>(&'a self, url: &'a str) -> LighterRead<'a> {
        (**self).get(url)
    }
}

/// The future [`LighterReads::get`] returns: the status and body of the answer.
pub type LighterRead<'a> = Pin<
    Box<
        dyn Future<
                Output = std::result::Result<
                    (u16, Vec<u8>),
                    Box<dyn std::error::Error + Send + Sync>,
                >,
            > + Send
            + 'a,
    >,
>;

/// Configures a [`Client`].
#[must_use = "a builder does nothing until built"]
pub struct ClientBuilder {
    environment: Environment,
    timeout: Duration,
    connect_timeout: Duration,
    retry: RetryPolicy,
    user_agent_suffix: Option<String>,
    clock: Option<Clock>,
    lighter_api: Option<String>,
    lighter_reads: Option<Arc<dyn LighterReads>>,
    http_exchange: Option<Arc<dyn HttpExchange>>,
    stream_connector: Option<Arc<dyn StreamConnector>>,
}

impl ClientBuilder {
    /// Deadline of each attempt, the answer included (default 30 s).
    pub fn timeout(mut self, per_attempt: Duration) -> Self {
        self.timeout = per_attempt;
        self
    }

    /// Deadline to establish a connection (default 10 s).
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// The retry policy of retryable requests (default [`RetryPolicy::default`]).
    pub fn retry(mut self, policy: RetryPolicy) -> Self {
        self.retry = policy;
        self
    }

    /// Appended to the user agent: `lamarge-rust/<version> <suffix>`.
    pub fn user_agent_suffix(mut self, suffix: impl Into<String>) -> Self {
        self.user_agent_suffix = Some(suffix.into());
        self
    }

    /// Sends the client's reads of Lighter's public API through `reads` (default: the client's own
    /// HTTP client).
    pub fn lighter_reads(mut self, reads: impl LighterReads + 'static) -> Self {
        self.lighter_reads = Some(Arc::new(reads));
        self
    }

    /// Sends every request to the API through `exchange` (default: the client's own HTTP
    /// client). Lighter's reads keep their own carrier ([`Self::lighter_reads`]).
    pub fn http_exchange(mut self, exchange: impl HttpExchange + 'static) -> Self {
        self.http_exchange = Some(Arc::new(exchange));
        self
    }

    /// Opens the event WebSocket's connection through `connector` (default: a TCP connection of
    /// the client's own).
    pub fn stream_connector(mut self, connector: impl StreamConnector + 'static) -> Self {
        self.stream_connector = Some(Arc::new(connector));
        self
    }

    /// Replaces the wall clock.
    #[cfg(test)]
    pub(crate) fn clock(mut self, clock: Clock) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Replaces Lighter's public API base.
    #[cfg(test)]
    pub(crate) fn lighter_api(mut self, base: impl Into<String>) -> Self {
        self.lighter_api = Some(base.into());
        self
    }

    /// Builds the client.
    ///
    /// Fails with [`Error::Config`] for an invalid or non-HTTPS URL (outside localhost), an
    /// invalid user agent, or a TLS setup failure.
    pub fn build(self) -> Result<Client> {
        let (api_url, app_origin) = self.environment.urls();
        let api = api_base(api_url)?;
        let origin = app_origin_of(app_origin)?;
        let user_agent = match &self.user_agent_suffix {
            Some(suffix) => HeaderValue::try_from(format!("{USER_AGENT} {suffix}")),
            None => Ok(HeaderValue::from_static(USER_AGENT)),
        }
        .map_err(|_| Error::Config("user agent suffix is not a valid header value".into()))?;
        let (http_tls, ws_tls) = transport::tls::configs()?;
        let user_agent_text =
            user_agent.to_str().map_err(|_| Error::Config("user agent".into()))?.to_owned();
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured(http_tls)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(self.timeout)
            .connect_timeout(self.connect_timeout)
            .user_agent(user_agent)
            .build()
            .map_err(|error| Error::Config(format!("http client: {error}").into()))?;
        let clock = self.clock.unwrap_or_else(|| Arc::new(chrono::Utc::now));
        Ok(Client {
            inner: Arc::new(Inner {
                http,
                api,
                origin,
                lighter_api: self.lighter_api.unwrap_or_else(|| transport::LIGHTER_API.to_owned()),
                lighter_reads: self.lighter_reads,
                http_exchange: self.http_exchange,
                stream_connector: self.stream_connector,
                timeout: self.timeout,
                user_agent: user_agent_text,
                ws_tls,
                retry: self.retry,
                environment: self.environment,
                clock,
            }),
        })
    }
}

impl fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientBuilder")
            .field("environment", &self.environment)
            .field("timeout", &self.timeout)
            .field("connect_timeout", &self.connect_timeout)
            .field("retry", &self.retry)
            .field("user_agent_suffix", &self.user_agent_suffix)
            .field("lighter_reads", &self.lighter_reads.is_some())
            .field("http_exchange", &self.http_exchange.is_some())
            .field("stream_connector", &self.stream_connector.is_some())
            .finish_non_exhaustive()
    }
}

/// Parses `text` as an `http(s)` URL allowed by the HTTPS guard.
fn guarded_url(text: &str, what: &str) -> Result<Url> {
    let invalid = |reason: &str| Error::Config(Cow::Owned(format!("{what} {text:?}: {reason}")));
    let url = Url::parse(text).map_err(|error| invalid(&error.to_string()))?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    match url.scheme() {
        "https" => {}
        "http" if local => {}
        "http" => return Err(invalid("https is required outside localhost")),
        _ => return Err(invalid("not an http(s) URL")),
    }
    if url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(invalid("credentials, query and fragment are not allowed"));
    }
    Ok(url)
}

/// The API base without a trailing slash.
fn api_base(text: &str) -> Result<String> {
    let url = guarded_url(text, "api url")?;
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// The origin (`scheme://host[:port]`) of an app URL that names no path.
fn app_origin_of(text: &str) -> Result<String> {
    let url = guarded_url(text, "app origin")?;
    if url.path() != "/" {
        return Err(Error::Config(format!("app origin {text:?}: an origin has no path").into()));
    }
    Ok(url.origin().ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom(api_url: &str, app_origin: &str) -> Result<Client> {
        Client::new(Environment::Custom { api_url: api_url.into(), app_origin: app_origin.into() })
    }

    #[test]
    fn named_environments_build() {
        let client = Client::new(Environment::Production).unwrap();
        assert_eq!(client.inner().api, "https://api.lamarge.trade");
        assert_eq!(client.inner().origin, "https://app.lamarge.trade");
        assert_eq!(client.environment(), &Environment::Production);
        let client = Client::new(Environment::Development).unwrap();
        assert_eq!(client.inner().api, "https://api.dev.lamarge.trade");
        assert_eq!(client.inner().origin, "https://app.dev.lamarge.trade");
        assert_eq!(client.inner().lighter_api, transport::LIGHTER_API);
    }

    #[test]
    fn https_guard() {
        assert!(custom("https://api.example.com/base/", "https://app.example.com").is_ok());
        for local in ["http://localhost:8080", "http://127.0.0.1:1", "http://[::1]:9"] {
            assert!(custom(local, local).is_ok(), "{local}");
        }
        for (api, origin) in [
            ("http://api.example.com", "https://app.example.com"),
            ("https://api.example.com", "http://app.example.com"),
            ("http://localhost.example.com", "https://app.example.com"),
            ("ftp://api.example.com", "https://app.example.com"),
            ("api.example.com", "https://app.example.com"),
            ("https://user:pass@api.example.com", "https://app.example.com"),
            ("https://api.example.com?x=1", "https://app.example.com"),
            ("https://api.example.com", "https://app.example.com/login"),
        ] {
            assert!(matches!(custom(api, origin), Err(Error::Config(_))), "{api} {origin}");
        }
    }

    #[test]
    fn api_base_keeps_its_path_without_trailing_slash() {
        let client = custom("https://api.example.com/base/", "https://app.example.com/").unwrap();
        assert_eq!(client.inner().api, "https://api.example.com/base");
        assert_eq!(client.inner().origin, "https://app.example.com");
        let client =
            custom("https://api.example.com:8443", "https://app.example.com:8443").unwrap();
        assert_eq!(client.inner().api, "https://api.example.com:8443");
        assert_eq!(client.inner().origin, "https://app.example.com:8443");
    }

    #[test]
    fn an_invalid_user_agent_suffix_is_refused() {
        let error =
            Client::builder(Environment::Production).user_agent_suffix("bad\nsuffix").build();
        assert!(matches!(error, Err(Error::Config(_))));
    }

    #[test]
    fn retry_policy_counts_the_first_attempt() {
        assert_eq!(RetryPolicy::new(0, Duration::ZERO, Duration::ZERO).max_attempts(), 1);
        assert_eq!(RetryPolicy::none().max_attempts(), 1);
        let default = RetryPolicy::default();
        assert_eq!(default.max_attempts(), 3);
        assert_eq!(default.base_delay(), Duration::from_millis(500));
        assert_eq!(default.max_delay(), Duration::from_secs(5));
    }

    #[test]
    fn injected_clock_and_websocket_tls_reach_the_transport() {
        let fixed = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let client = Client::builder(Environment::Production)
            .clock(Arc::new(move || fixed))
            .build()
            .unwrap();
        assert_eq!(client.inner().now(), fixed);
        assert_eq!(client.inner().ws_tls.alpn_protocols, [b"http/1.1".to_vec()]);
    }

    #[test]
    fn client_is_send_sync_and_debug_shows_no_internals() {
        fn check<T: Send + Sync + 'static>() {}
        check::<Client>();
        let client = Client::new(Environment::Production).unwrap();
        assert!(format!("{client:?}").contains("api.lamarge.trade"));
    }
}
