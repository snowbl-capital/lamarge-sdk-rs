//! The event WebSocket connector: `ws` over TCP (or the host's stream) for an `http` API, `wss`
//! over the client's own rustls configuration for an `https` one, at `<API base>/v1/stream` (the
//! base path kept).

use std::sync::Arc;

use futures_util::SinkExt as _;
use reqwest::Url;
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{self, Message, protocol::WebSocketConfig},
};

use crate::{
    Error, HostStream, Result,
    error::TransportError,
    transport::{Inner, MAX_ANSWER_BYTES, ops},
};

/// An open event WebSocket.
pub(super) type Socket = WebSocketStream<Box<dyn HostStream>>;

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
    let tcp: Box<dyn HostStream> = match &inner.stream_connector {
        Some(connector) => connector
            .connect(&endpoint.host, endpoint.port)
            .await
            .map_err(|error| Error::Transport(TransportError::caller(error)))?,
        None => {
            let tcp = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
                .await
                .map_err(|error| websocket(tungstenite::Error::Io(error)))?;
            // Frames are small and latency matters more than packet count.
            let _ = tcp.set_nodelay(true);
            Box::new(tcp)
        }
    };
    let io: Box<dyn HostStream> = if endpoint.tls {
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
    use futures_util::StreamExt as _;

    use super::*;
    use crate::{Client, Environment, StreamConnector, StreamFuture};

    /// Hands the SDK one end of an in-memory pipe and serves a WebSocket on the other.
    struct Pipe(std::sync::Mutex<Option<tokio::io::DuplexStream>>, std::sync::Mutex<Vec<String>>);

    impl StreamConnector for Pipe {
        fn connect<'a>(&'a self, host: &'a str, port: u16) -> StreamFuture<'a> {
            self.1.lock().unwrap().push(format!("{host}:{port}"));
            let end = self.0.lock().unwrap().take();
            Box::pin(async move {
                let stream: Box<dyn HostStream> = Box::new(end.ok_or("connected once")?);
                Ok(stream)
            })
        }
    }

    #[tokio::test]
    async fn a_host_connector_carries_the_event_websocket() {
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let pipe = std::sync::Arc::new(Pipe(Some(ours).into(), Vec::new().into()));
        // Nothing listens there: only the pipe can carry the connection.
        let (api_url, app_origin) =
            ("http://127.0.0.1:9/base".to_owned(), "http://127.0.0.1:9".to_owned());
        let client = Client::builder(Environment::Custom { api_url, app_origin })
            .stream_connector(std::sync::Arc::clone(&pipe))
            .build()
            .unwrap();
        let server = tokio::spawn(async move {
            let mut path = String::new();
            let callback = |request: &tungstenite::handshake::server::Request, response| {
                path = request.uri().path().to_owned();
                Ok(response)
            };
            let mut socket = tokio_tungstenite::accept_hdr_async(theirs, callback).await.unwrap();
            let first = socket.next().await.unwrap().unwrap();
            (path, first.into_text().unwrap().to_string())
        });
        let _socket =
            connect(client.inner(), r#"{"subscribe":"public"}"#.to_owned()).await.unwrap();
        let (path, first) = server.await.unwrap();
        assert_eq!(path, "/base/v1/stream");
        assert_eq!(first, r#"{"subscribe":"public"}"#);
        assert_eq!(*pipe.1.lock().unwrap(), ["127.0.0.1:9"]);

        let Err(error) = connect(client.inner(), String::new()).await else {
            panic!("the pipe connects once")
        };
        assert!(
            matches!(error, Error::Transport(_)),
            "a host failure is a lost connection: {error:?}"
        );
    }

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
