//! The event WebSocket connector: `ws` over TCP for an `http` API, `wss` over the client's own
//! rustls configuration for an `https` one, at `<API base>/v1/stream` (the base path kept).

use std::sync::Arc;

use futures_util::SinkExt as _;
use reqwest::Url;
use rustls::pki_types::ServerName;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tokio_rustls::TlsConnector;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{self, Message, protocol::WebSocketConfig},
};

use crate::{
    Error, Result,
    error::TransportError,
    transport::{Inner, MAX_ANSWER_BYTES, ops},
};

/// A byte stream the WebSocket runs over: plain TCP or TLS.
pub(super) trait Io: AsyncRead + AsyncWrite + Send + Sync + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Sync + Unpin> Io for T {}

/// An open event WebSocket.
pub(super) type Socket = WebSocketStream<Box<dyn Io>>;

/// Where the event WebSocket of an API base is.
#[derive(Debug, PartialEq, Eq)]
struct Endpoint {
    url: String,
    host: String,
    port: u16,
    tls: bool,
}

/// The endpoint of `api`, an `http(s)` base URL without a trailing slash.
fn endpoint(api: &str) -> Result<Endpoint> {
    let invalid = || Error::Config(format!("api url {api:?}: no websocket endpoint").into());
    let (scheme, tls, rest) = if let Some(rest) = api.strip_prefix("https://") {
        ("wss", true, rest)
    } else if let Some(rest) = api.strip_prefix("http://") {
        ("ws", false, rest)
    } else {
        return Err(invalid());
    };
    let parsed = Url::parse(api).map_err(|_| invalid())?;
    // An IPv6 host is bracketed in a URL, not in a socket address or TLS name.
    let host = parsed.host_str().ok_or_else(invalid)?.trim_start_matches('[').trim_end_matches(']');
    let port = parsed.port_or_known_default().ok_or_else(invalid)?;
    Ok(Endpoint {
        url: format!("{scheme}://{rest}{}", ops::STREAM.path),
        host: host.to_owned(),
        port,
        tls,
    })
}

/// Opens the event WebSocket and sends `subscription` as its first frame.
pub(super) async fn connect(inner: &Inner, subscription: String) -> Result<Socket> {
    let endpoint = endpoint(&inner.api)?;
    let websocket = |error| Error::Transport(TransportError::websocket(error));
    let tcp = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
        .await
        .map_err(|error| websocket(tungstenite::Error::Io(error)))?;
    // Frames are small and latency matters more than packet count.
    let _ = tcp.set_nodelay(true);
    let io: Box<dyn Io> = if endpoint.tls {
        let name = ServerName::try_from(endpoint.host.clone()).map_err(|_| {
            Error::Config(format!("api host {:?}: not a TLS name", endpoint.host).into())
        })?;
        let tls = TlsConnector::from(Arc::clone(&inner.ws_tls))
            .connect(name, tcp)
            .await
            .map_err(|error| websocket(tungstenite::Error::Io(error)))?;
        Box::new(tls)
    } else {
        Box::new(tcp)
    };
    let config = WebSocketConfig::default().max_message_size(Some(MAX_ANSWER_BYTES));
    let (mut socket, _) =
        tokio_tungstenite::client_async_with_config(endpoint.url, io, Some(config))
            .await
            .map_err(websocket)?;
    socket.send(Message::text(subscription)).await.map_err(websocket)?;
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_follow_the_api_scheme_and_keep_its_path() {
        let cases = [
            (
                "https://api.lamarge.trade",
                "wss://api.lamarge.trade/v1/stream",
                "api.lamarge.trade",
                443,
                true,
            ),
            (
                "https://api.example.com:8443/base",
                "wss://api.example.com:8443/base/v1/stream",
                "api.example.com",
                8443,
                true,
            ),
            (
                "http://127.0.0.1:8080/api",
                "ws://127.0.0.1:8080/api/v1/stream",
                "127.0.0.1",
                8080,
                false,
            ),
            ("http://localhost", "ws://localhost/v1/stream", "localhost", 80, false),
            ("http://[::1]:9", "ws://[::1]:9/v1/stream", "::1", 9, false),
        ];
        for (api, url, host, port, tls) in cases {
            let expected = Endpoint { url: url.to_owned(), host: host.to_owned(), port, tls };
            assert_eq!(endpoint(api).unwrap(), expected, "{api}");
        }
        assert!(matches!(endpoint("ftp://api.example.com"), Err(Error::Config(_))));
    }
}
