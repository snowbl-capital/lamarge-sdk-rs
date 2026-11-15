//! HTTP test helpers for the resource handles: a client of a mock server, a session's
//! credential, and one-call expectations on what a handle sends.

use std::{
    future::Future,
    sync::{Arc, LazyLock},
};

use serde_json::Value;
use wiremock::{Mock, MockServer, Request, ResponseTemplate, matchers::any};

use crate::{Client, Environment, Result, RetryPolicy, transport::BearerToken};

/// The bearer token test sessions send.
pub(crate) const TOKEN: &str = "session-token";

struct Token;

impl BearerToken for Token {
    fn bearer(&self) -> Result<String> {
        Ok(TOKEN.to_owned())
    }
}

/// A session's credential, sending [`TOKEN`].
pub(crate) fn session() -> &'static Arc<dyn BearerToken> {
    static SESSION: LazyLock<Arc<dyn BearerToken>> = LazyLock::new(|| Arc::new(Token));
    &SESSION
}

/// A client of `server` on the test clock ([`NOW`](super::NOW)), without retries.
pub(crate) fn client(server: &MockServer) -> Client {
    let uri = server.uri();
    Client::builder(Environment::Custom { api_url: uri.clone(), app_origin: uri })
        .retry(RetryPolicy::none())
        .clock(Arc::new(|| super::at(0)))
        .build()
        .unwrap()
}

/// The single request `server` received.
pub(crate) async fn only_request(server: &MockServer) -> Request {
    let mut requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "{requests:?}");
    requests.pop().unwrap()
}

/// What one handle call must send, and the answer it gets.
#[derive(Debug)]
pub(crate) struct Expect {
    method: &'static str,
    path: String,
    query: Vec<(String, String)>,
    bearer: bool,
    status: u16,
    answer: Option<Value>,
}

impl Expect {
    fn new(method: &'static str, path: impl Into<String>, answer: Option<Value>) -> Self {
        let path = path.into();
        Self { method, path, query: Vec::new(), bearer: false, status: 200, answer }
    }

    /// A `GET path` answered with `answer`.
    pub(crate) fn get(path: impl Into<String>, answer: Value) -> Self {
        Self::new("GET", path, Some(answer))
    }

    /// A `POST path` answered with `answer`.
    pub(crate) fn post(path: impl Into<String>, answer: Value) -> Self {
        Self::new("POST", path, Some(answer))
    }

    /// A `DELETE path` answered with `answer`, or 204 without one.
    pub(crate) fn delete(path: impl Into<String>, answer: Option<Value>) -> Self {
        let status = if answer.is_some() { 200 } else { 204 };
        Self { status, ..Self::new("DELETE", path, answer) }
    }

    /// Exactly these query parameters, in any order.
    pub(crate) fn query(mut self, pairs: &[(&str, &str)]) -> Self {
        self.query = pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        self.query.sort();
        self
    }

    /// With the session's bearer token.
    pub(crate) fn bearer(mut self) -> Self {
        self.bearer = true;
        self
    }

    /// Answered with `status` instead of 200.
    pub(crate) fn status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }
}

/// Serves `expect`'s answer to `call` on a reset `server`, checks the one request it sent and
/// returns the call's result.
pub(crate) async fn served<T>(
    server: &MockServer,
    expect: Expect,
    call: impl Future<Output = Result<T>>,
) -> Result<T> {
    server.reset().await;
    let mut answer = ResponseTemplate::new(expect.status);
    if let Some(body) = &expect.answer {
        answer = answer.set_body_json(body);
    }
    Mock::given(any()).respond_with(answer).mount(server).await;
    let result = call.await;
    let request = only_request(server).await;
    let mut query: Vec<(String, String)> =
        request.url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    query.sort();
    let authorization = request.headers.get("authorization").map(|value| value.to_str().unwrap());
    let sent = (request.method.as_str(), request.url.path(), query, authorization);
    let bearer = format!("Bearer {TOKEN}");
    let wanted = (
        expect.method,
        expect.path.as_str(),
        expect.query.clone(),
        expect.bearer.then_some(bearer.as_str()),
    );
    assert_eq!(sent, wanted, "{expect:?}");
    result
}

/// Collects `stream` on another task; compiles only for a stream that owns its state and polls
/// without pinning.
pub(crate) async fn collect_spawned<T, S>(stream: S) -> Result<Vec<T>>
where
    T: Send + 'static,
    S: futures_core::Stream<Item = Result<T>> + Send + Unpin + 'static,
{
    use futures_util::TryStreamExt as _;
    tokio::spawn(stream.try_collect()).await.unwrap()
}
