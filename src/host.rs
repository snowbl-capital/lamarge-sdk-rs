//! Transports a host supplies in place of the client's own: every request to the API through
//! [`HttpExchange`], the event WebSocket's connection through [`StreamConnector`].

use std::{fmt, future::Future, pin::Pin, sync::Arc};

use tokio::io::{AsyncRead, AsyncWrite};

/// The error a host transport returns when no answer was received.
pub type HostError = Box<dyn std::error::Error + Send + Sync>;

/// Sends the client's requests to the API in place of its own HTTP client, so they leave through
/// the host's transport: its source address, budgets, telemetry or test fakes. Set with
/// [`ClientBuilder::http_exchange`](crate::ClientBuilder::http_exchange).
///
/// The SDK keeps its own behaviour on top: the per-attempt deadline
/// ([`ClientBuilder::timeout`](crate::ClientBuilder::timeout)), the retry policy, the 16 MiB
/// answer cap and the decoding of errors. An `Err` is a transport failure, retried like a lost
/// connection. The exchange must not follow redirects nor retry by itself.
///
/// ```
/// use lamarge::{HttpAnswer, HttpExchange, HttpExchangeFuture, HttpRequest};
///
/// /// The API's requests through the host's HTTP client.
/// struct Through(reqwest::Client);
///
/// impl HttpExchange for Through {
///     fn send(&self, request: HttpRequest) -> HttpExchangeFuture<'_> {
///         Box::pin(async move {
///             let method = reqwest::Method::from_bytes(request.method.as_bytes())?;
///             let mut outgoing = self.0.request(method, &request.url);
///             for (name, value) in &request.headers {
///                 outgoing = outgoing.header(*name, value);
///             }
///             if let Some(body) = request.body {
///                 outgoing = outgoing.body(body);
///             }
///             let answer = outgoing.send().await?;
///             let mut out = HttpAnswer::new(answer.status().as_u16(), Vec::new());
///             for (name, value) in answer.headers() {
///                 if let Ok(value) = value.to_str() {
///                     out = out.header(name.as_str(), value);
///                 }
///             }
///             Ok(out.body(answer.bytes().await?.to_vec()))
///         })
///     }
/// }
/// ```
pub trait HttpExchange: Send + Sync {
    /// Sends `request` and returns the answer, whatever its status.
    fn send(&self, request: HttpRequest) -> HttpExchangeFuture<'_>;
}

/// One carrier shared by several clients.
impl<T: HttpExchange + ?Sized> HttpExchange for Arc<T> {
    fn send(&self, request: HttpRequest) -> HttpExchangeFuture<'_> {
        (**self).send(request)
    }
}

/// The future [`HttpExchange::send`] returns.
pub type HttpExchangeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<HttpAnswer, HostError>> + Send + 'a>>;

/// A request of the client to the API.
#[non_exhaustive]
pub struct HttpRequest {
    /// `GET`, `POST`, `PUT`, `PATCH` or `DELETE`.
    pub method: &'static str,
    /// The full URL, query included and encoded.
    pub url: String,
    /// The headers to send: `accept`, `user-agent`, and `content-type` with a body. A session's
    /// request carries `authorization`, a bearer secret: keep it out of logs.
    pub headers: Vec<(&'static str, String)>,
    /// The JSON body, if any.
    pub body: Option<Vec<u8>>,
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<_> = self
            .headers
            .iter()
            .map(|(name, value)| {
                (*name, if *name == "authorization" { "<redacted>" } else { value.as_str() })
            })
            .collect();
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &headers)
            .field("body", &self.body.as_ref().map(Vec::len))
            .finish()
    }
}

/// The answer an [`HttpExchange`] received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpAnswer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpAnswer {
    /// An answer with `status` and `body`, without headers.
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self { status, headers: Vec::new(), body: body.into() }
    }

    /// Adds a header. The SDK reads `x-request-id` and `retry-after`, by any case.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Replaces the body.
    #[must_use]
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// The first value of header `name`, by any case.
    pub(crate) fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub(crate) const fn status(&self) -> u16 {
        self.status
    }

    pub(crate) fn into_body(self) -> Vec<u8> {
        self.body
    }
}

/// A byte stream the event WebSocket runs over.
pub trait HostStream: AsyncRead + AsyncWrite + Send + Sync + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Sync + Unpin> HostStream for T {}

/// Opens the event WebSocket's connection in place of the client's own TCP connection. Set with
/// [`ClientBuilder::stream_connector`](crate::ClientBuilder::stream_connector).
///
/// The host opens a byte stream to `host:port`, the API's; the SDK then runs TLS over it for an
/// `https` API (with its own roots, naming `host`) and the WebSocket handshake. An `Err` is a
/// lost connection: the event stream reconnects or falls back to HTTP replay. The SDK's connect
/// deadline applies.
///
/// ```
/// use lamarge::{HostStream, StreamConnector, StreamFuture};
///
/// /// The event feed's connection from a chosen local address.
/// struct Bound(std::net::IpAddr);
///
/// impl StreamConnector for Bound {
///     fn connect<'a>(&'a self, host: &'a str, port: u16) -> StreamFuture<'a> {
///         Box::pin(async move {
///             let remote = tokio::net::lookup_host((host, port)).await?.next().ok_or("no address")?;
///             let socket = match self.0 {
///                 std::net::IpAddr::V4(_) => tokio::net::TcpSocket::new_v4()?,
///                 std::net::IpAddr::V6(_) => tokio::net::TcpSocket::new_v6()?,
///             };
///             socket.bind((self.0, 0).into())?;
///             let stream: Box<dyn HostStream> = Box::new(socket.connect(remote).await?);
///             Ok(stream)
///         })
///     }
/// }
/// ```
pub trait StreamConnector: Send + Sync {
    /// Opens a byte stream to `host:port`.
    fn connect<'a>(&'a self, host: &'a str, port: u16) -> StreamFuture<'a>;
}

/// One carrier shared by several clients.
impl<T: StreamConnector + ?Sized> StreamConnector for Arc<T> {
    fn connect<'a>(&'a self, host: &'a str, port: u16) -> StreamFuture<'a> {
        (**self).connect(host, port)
    }
}

/// The future [`StreamConnector::connect`] returns.
pub type StreamFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Box<dyn HostStream>, HostError>> + Send + 'a>>;
