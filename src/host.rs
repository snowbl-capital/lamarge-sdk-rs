//! What a host supplies in place of the client's own: every request to the API through
//! [`HttpExchange`], the event WebSocket's connection through [`StreamConnector`], and the
//! transactions a flow sends through [`TransactionSender`].

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

/// A transaction the SDK asks the host to send from the session's address.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Transaction {
    /// Stable across runs and replacement transfers: the same `id` is the same transaction.
    /// Key the host's own record on it, so [`TransactionSender::status`] can answer for it.
    pub id: String,
    /// The chain.
    pub chain_id: u64,
    /// The contract called.
    pub to: alloy_primitives::Address,
    /// The calldata.
    pub data: Vec<u8>,
    /// The native value sent; zero for every transaction the SDK asks today.
    pub value: alloy_primitives::U256,
}

/// Why a [`TransactionSender`] did not return a hash.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SendError {
    /// Nothing was broadcast, and nothing will be: the owner declined, the signature request
    /// timed out, the address lacks gas. The SDK may ask for the same transaction again.
    #[error("transaction not sent")]
    NotSent(#[source] HostError),
    /// It may have been broadcast. The SDK never asks for it again until
    /// [`TransactionSender::status`] says otherwise.
    #[error("transaction outcome unknown")]
    Unknown(#[source] HostError),
}

/// What the host knows of a transaction it was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SendStatus {
    /// Broadcast, with this hash.
    Sent(alloy_primitives::B256),
    /// Never broadcast, and never will be under this request: it may be asked again.
    NotSent,
    /// Not known: the SDK waits and never asks again.
    Unknown,
}

/// Sends the transactions a flow needs from the session's address, which pays their gas: today
/// the Polygon native-USDC conversion of a wallet-funded transfer (an `approve`, then the
/// convert call). Passed to [`Session::transfer_with`](crate::Session::transfer_with).
///
/// The host owns gas, fees, nonces and signing; the SDK sets none. It journals each request
/// before asking and each hash once returned, so a run that restarts never asks for a
/// transaction it may have sent: it asks [`Self::status`] instead.
pub trait TransactionSender: Send + Sync {
    /// Sends `transaction` and returns its hash once broadcast, without waiting for its receipt:
    /// the SDK follows the outcome on LaMarge. Waiting for the owner's signature may take long;
    /// the SDK sets no deadline on it.
    fn send(&self, transaction: Transaction) -> SendFuture<'_>;

    /// What became of transaction `id`, asked earlier with no answer recorded (the run stopped
    /// meanwhile). By default [`SendStatus::Unknown`].
    fn status<'a>(&'a self, id: &'a str) -> StatusFuture<'a> {
        let _ = id;
        Box::pin(async { Ok(SendStatus::Unknown) })
    }
}

/// One sender shared by several runs.
impl<T: TransactionSender + ?Sized> TransactionSender for Arc<T> {
    fn send(&self, transaction: Transaction) -> SendFuture<'_> {
        (**self).send(transaction)
    }

    fn status<'a>(&'a self, id: &'a str) -> StatusFuture<'a> {
        (**self).status(id)
    }
}

/// The future [`TransactionSender::send`] returns.
pub type SendFuture<'a> =
    Pin<Box<dyn Future<Output = Result<alloy_primitives::B256, SendError>> + Send + 'a>>;

/// The future [`TransactionSender::status`] returns; an `Err` is read as [`SendStatus::Unknown`].
pub type StatusFuture<'a> =
    Pin<Box<dyn Future<Output = Result<SendStatus, HostError>> + Send + 'a>>;
