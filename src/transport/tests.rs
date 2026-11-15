#![expect(clippy::items_after_statements, reason = "test-local types next to their use")]

use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{body_bytes, header, header_exists, method, path},
};

use super::{
    ops::{AuthTier, Method, Op},
    *,
};
use crate::{Client, Environment, RetryPolicy, error::ErrorCode};

const GET: Op = Op {
    id: "getThing",
    method: Method::Get,
    path: "/v1/things/{id}",
    auth: AuthTier::Public,
    retry: true,
    secret: false,
    answers: &[],
};
const POST_IDEMPOTENT: Op =
    Op { id: "postThing", method: Method::Post, path: "/v1/things", retry: true, ..GET };
const POST_ONCE: Op = Op { id: "prepareThing", retry: false, ..POST_IDEMPOTENT };
const DELETE: Op = Op { id: "deleteThing", method: Method::Delete, retry: false, ..GET };
const SECRET: Op = Op { id: "postSecret", secret: true, auth: AuthTier::Session, ..POST_ONCE };
const HEALTH: Op = Op { id: "getHealth", path: "/health", answers: &[503], ..GET };

fn test_client(server: &MockServer, retry: RetryPolicy) -> Client {
    let uri = server.uri();
    Client::builder(Environment::Custom { api_url: uri.clone(), app_origin: uri })
        .retry(retry)
        .lighter_api(format!("{}/lighter", server.uri()))
        .user_agent_suffix("bot/1.2")
        .build()
        .unwrap()
}

fn fast() -> RetryPolicy {
    RetryPolicy::new(3, Duration::from_millis(1), Duration::from_millis(5))
}

fn envelope(code: &str, retryable: bool) -> Value {
    json!({"error": {"code": code, "message": "m", "request_id": "req-env", "retryable": retryable}})
}

async fn received(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap()
}

/// A token that changes at every read; clones share the count.
#[derive(Clone, Default)]
struct Token {
    reads: Arc<AtomicUsize>,
}

impl BearerToken for Token {
    fn bearer(&self) -> Result<String> {
        Ok(format!("token-{}", self.reads.fetch_add(1, Ordering::SeqCst) + 1))
    }
}

/// `token` as a request credential.
fn bearer(token: impl BearerToken + 'static) -> Arc<dyn BearerToken> {
    Arc::new(token)
}

struct SignedOut;

impl BearerToken for SignedOut {
    fn bearer(&self) -> Result<String> {
        Err(Error::SignedOut)
    }
}

#[test]
fn path_segments_are_percent_encoded_in_order() {
    let call = Call::new(&GET).segment("a b/c?d");
    assert_eq!(call.path().unwrap(), "/v1/things/a%20b%2Fc%3Fd");
    const TWO: Op = Op { path: "/v1/a/{x}/b/{y}", ..GET };
    assert_eq!(Call::new(&TWO).segment("1").segment("ok-._~").path().unwrap(), "/v1/a/1/b/ok-._~");
    assert!(matches!(Call::new(&TWO).segment("1").path(), Err(Error::InvalidRequest(_))));
    assert!(matches!(Call::new(&GET).segment(1).segment(2).path(), Err(Error::InvalidRequest(_))));
}

#[test]
fn retry_delay_classifies_errors() {
    let policy = RetryPolicy::new(5, Duration::from_millis(100), Duration::from_secs(1));
    let api = |status, body: &Value, retry_after| {
        Error::Api(Box::new(ApiError::from_answer(
            status,
            body.to_string().as_bytes(),
            None,
            retry_after,
        )))
    };
    let unavailable = api(503, &envelope("VENUE_UNAVAILABLE", true), None);
    for failed in 0..6 {
        let delay = retry_delay(&policy, failed, &unavailable).unwrap();
        let ceiling = Duration::from_millis(100 * 2u64.pow(failed)).min(Duration::from_secs(1));
        assert!(delay < ceiling, "attempt {failed}: {delay:?} >= {ceiling:?}");
    }
    let limited = api(429, &envelope("RATE_LIMITED", true), Some(Duration::from_secs(1)));
    assert_eq!(retry_delay(&policy, 0, &limited), Some(Duration::from_secs(1)));
    let too_long = api(429, &envelope("RATE_LIMITED", true), Some(Duration::from_secs(2)));
    assert_eq!(
        retry_delay(&policy, 0, &too_long),
        None,
        "Retry-After above max_delay is not waited"
    );
    assert_eq!(retry_delay(&policy, 0, &api(500, &envelope("INTERNAL", false), None)), None);
    assert_eq!(
        retry_delay(&policy, 0, &api(400, &envelope("VALIDATION_ERROR", false), None)),
        None
    );
    assert_eq!(retry_delay(&policy, 0, &Error::InvalidRequest("x".into())), None);
    let decode = serde_json::from_slice::<u8>(b"x").unwrap_err();
    let decode = Error::Decode(Box::new(DecodeError::json(Some(200), None, b"x", &decode, false)));
    assert_eq!(retry_delay(&policy, 0, &decode), None, "a decode error is never retried");
}

#[test]
fn backoff_is_jittered() {
    let policy = RetryPolicy::new(5, Duration::from_millis(100), Duration::from_secs(1));
    let error = Error::Api(Box::new(ApiError::from_answer(502, b"", None, None)));
    let delays: BTreeSet<_> = (0..20).map(|_| retry_delay(&policy, 2, &error).unwrap()).collect();
    assert!(delays.len() > 1, "full jitter varies the delay");
}

#[tokio::test]
async fn get_is_retried_after_503() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/things/7"))
        .respond_with(ResponseTemplate::new(503).set_body_json(envelope("VENUE_UNAVAILABLE", true)))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/things/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&server)
        .await;
    let client = test_client(&server, fast());
    let answer: Value = client.inner().send(Call::new(&GET).segment(7), None).await.unwrap();
    assert_eq!(answer, json!({"ok": true}));
    assert_eq!(received(&server).await.len(), 2);
}

#[tokio::test]
async fn allow_listed_post_is_retried_with_the_same_bytes() {
    let server = MockServer::start().await;
    let body = json!({"client_take_id": "t-1", "amount": "5"});
    let bytes = serde_json::to_vec(&body).unwrap();
    Mock::given(method("POST"))
        .and(path("/v1/things"))
        .and(body_bytes(bytes.clone()))
        .and(header("content-type", "application/json"))
        .respond_with(ResponseTemplate::new(502))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/things"))
        .and(body_bytes(bytes))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!(1)))
        .mount(&server)
        .await;
    let client = test_client(&server, fast());
    let answer: u8 =
        client.inner().send(Call::new(&POST_IDEMPOTENT).json(&body).unwrap(), None).await.unwrap();
    assert_eq!(answer, 1);
    let requests = received(&server).await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body, requests[1].body);
}

#[tokio::test]
async fn requests_without_idempotency_key_are_never_retried() {
    for op in [&POST_ONCE, &DELETE] {
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(
                ResponseTemplate::new(503).set_body_json(envelope("VENUE_UNAVAILABLE", true)),
            )
            .mount(&server)
            .await;
        let client = test_client(&server, fast());
        let mut call = Call::new(op);
        if op.path.contains('{') {
            call = call.segment("x");
        }
        let error = client.inner().send::<Value>(call, None).await.unwrap_err();
        assert_eq!(error.status(), Some(503), "{}", op.id);
        assert!(error.is_retryable(), "the caller may retry it with the same id");
        assert_eq!(received(&server).await.len(), 1, "{}", op.id);
    }
}

#[tokio::test]
async fn persistent_failure_makes_exactly_max_attempts() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503).set_body_json(envelope("VENUE_UNAVAILABLE", true)))
        .mount(&server)
        .await;
    let client = test_client(&server, fast());
    let error = client.inner().send::<Value>(Call::new(&GET).segment(1), None).await.unwrap_err();
    assert_eq!(error.code(), Some(&ErrorCode::VenueUnavailable));
    assert_eq!(received(&server).await.len(), 3);
    let once = test_client(&server, RetryPolicy::none());
    once.inner().send::<Value>(Call::new(&GET).segment(1), None).await.unwrap_err();
    assert_eq!(received(&server).await.len(), 4);
}

#[tokio::test]
async fn non_retryable_refusal_is_returned_at_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500).set_body_json(envelope("INTERNAL", false)))
        .mount(&server)
        .await;
    let client = test_client(&server, fast());
    let error = client.inner().send::<Value>(Call::new(&GET).segment(1), None).await.unwrap_err();
    assert_eq!(error.code(), Some(&ErrorCode::Internal));
    assert_eq!(received(&server).await.len(), 1);
}

#[tokio::test]
async fn retry_after_is_honoured() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "1")
                .set_body_json(envelope("RATE_LIMITED", true)),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(2)))
        .mount(&server)
        .await;
    let policy = RetryPolicy::new(3, Duration::from_millis(1), Duration::from_secs(2));
    let client = test_client(&server, policy);
    let started = Instant::now();
    let answer: u8 = client.inner().send(Call::new(&GET).segment(1), None).await.unwrap();
    assert_eq!(answer, 2);
    assert!(started.elapsed() >= Duration::from_secs(1), "waited {:?}", started.elapsed());
}

#[tokio::test]
async fn retry_after_above_max_delay_returns_the_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "30")
                .set_body_json(envelope("RATE_LIMITED", true)),
        )
        .mount(&server)
        .await;
    let client = test_client(&server, fast());
    let started = Instant::now();
    let error = client.inner().send::<Value>(Call::new(&GET).segment(1), None).await.unwrap_err();
    assert_eq!(error.retry_after(), Some(Duration::from_secs(30)));
    assert_eq!(received(&server).await.len(), 1);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn redirects_are_errors_and_not_followed() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/things/1"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/elsewhere"))
        .mount(&server)
        .await;
    Mock::given(path("/elsewhere"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(1)))
        .expect(0)
        .mount(&server)
        .await;
    let client = test_client(&server, fast());
    let error = client.inner().send::<Value>(Call::new(&GET).segment(1), None).await.unwrap_err();
    assert_eq!(error.status(), Some(302));
    assert!(!error.is_retryable());
    assert_eq!(received(&server).await.len(), 1);
}

#[tokio::test]
async fn attempt_timeout_is_a_transport_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!(1)).set_delay(Duration::from_secs(5)),
        )
        .mount(&server)
        .await;
    let uri = server.uri();
    let client = Client::builder(Environment::Custom { api_url: uri.clone(), app_origin: uri })
        .timeout(Duration::from_millis(200))
        .retry(RetryPolicy::none())
        .build()
        .unwrap();
    let error = client.inner().send::<Value>(Call::new(&GET).segment(1), None).await.unwrap_err();
    let Error::Transport(transport) = &error else {
        panic!("expected a transport error, got {error:?}")
    };
    assert!(transport.is_timeout());
    assert!(error.is_retryable());
}

#[tokio::test]
async fn connection_failure_is_a_transport_error() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let uri = format!("http://{address}");
    let client = Client::builder(Environment::Custom { api_url: uri.clone(), app_origin: uri })
        .retry(RetryPolicy::none())
        .build()
        .unwrap();
    let error = client.inner().send::<Value>(Call::new(&GET).segment(1), None).await.unwrap_err();
    let Error::Transport(transport) = &error else {
        panic!("expected a transport error, got {error:?}")
    };
    assert!(transport.is_connect());
}

#[tokio::test]
async fn malformed_json_is_a_decode_error_with_the_request_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-request-id", "req-7")
                .set_body_string(r#"{"token":"mk_SECRET""#),
        )
        .mount(&server)
        .await;
    let client = test_client(&server, fast());
    let error = client.inner().send::<Value>(Call::new(&POST_ONCE), None).await.unwrap_err();
    let Error::Decode(decode) = &error else { panic!("expected a decode error, got {error:?}") };
    assert_eq!(decode.request_id(), Some("req-7"));
    assert!(decode.excerpt().unwrap().contains("mk_SECRET"));
    assert_eq!(received(&server).await.len(), 1, "decode errors are not retried");

    let token = Token::default();
    let error = client
        .inner()
        .send::<Value>(Call::new(&SECRET), Some(&bearer(token.clone())))
        .await
        .unwrap_err();
    let Error::Decode(decode) = &error else { panic!("expected a decode error, got {error:?}") };
    assert_eq!(decode.request_id(), Some("req-7"));
    assert_eq!(decode.excerpt(), None);
    assert!(!format!("{error} {error:?} {decode}").contains("SECRET"));
}

#[tokio::test]
async fn wrong_shape_of_secret_answer_keeps_no_value() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "mk_SECRET"})))
        .mount(&server)
        .await;
    let client = test_client(&server, fast());
    #[derive(Debug, serde::Deserialize)]
    #[expect(dead_code, reason = "only decoded")]
    struct Created {
        token: u64,
    }
    let token = Token::default();
    let error = client
        .inner()
        .send::<Created>(Call::new(&SECRET), Some(&bearer(token.clone())))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Decode(_)));
    assert!(!format!("{error:?}").contains("SECRET"), "serde's message quotes the value");
}

#[tokio::test]
async fn refusal_without_envelope_uses_the_header_request_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(404)
                .insert_header("x-request-id", "req-h")
                .set_body_string("nope"),
        )
        .mount(&server)
        .await;
    let client = test_client(&server, fast());
    let error = client.inner().send::<Value>(Call::new(&GET).segment(1), None).await.unwrap_err();
    assert_eq!(error.request_id(), Some("req-h"));
    assert_eq!(error.api().unwrap().message(), "HTTP 404");
    assert!(error.code().unwrap().is_unknown());
}

#[tokio::test]
async fn answers_over_the_cap_are_refused() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b' '; MAX_ANSWER_BYTES + 1]))
        .mount(&server)
        .await;
    let client = test_client(&server, fast());
    let error = client.inner().send::<Value>(Call::new(&GET).segment(1), None).await.unwrap_err();
    assert!(matches!(&error, Error::Decode(_)), "{error:?}");
    assert!(error.to_string().contains("unreadable"));
    assert_eq!(received(&server).await.len(), 1);
}

#[tokio::test]
async fn no_content_decodes_as_unit_and_listed_statuses_as_answers() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(204)).mount(&server).await;
    Mock::given(path("/health"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({"ready": false})))
        .mount(&server)
        .await;
    let client = test_client(&server, fast());
    client.inner().send::<()>(Call::new(&DELETE).segment("x"), None).await.unwrap();
    let health: Value = client.inner().send(Call::new(&HEALTH), None).await.unwrap();
    assert_eq!(health, json!({"ready": false}));
    assert_eq!(received(&server).await.len(), 2, "a listed answer status is not retried");
}

#[tokio::test]
async fn headers_carry_user_agent_and_bearer_read_at_each_attempt() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(header("authorization", "Bearer token-1"))
        .respond_with(ResponseTemplate::new(503).set_body_json(envelope("VENUE_UNAVAILABLE", true)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(header("authorization", "Bearer token-2"))
        .and(header("accept", "application/json"))
        .and(header("user-agent", concat!("lamarge-rust/", env!("CARGO_PKG_VERSION"), " bot/1.2")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(1)))
        .mount(&server)
        .await;
    let client = test_client(&server, fast());
    let token = Token::default();
    let answer: u8 = client
        .inner()
        .send(Call::new(&GET).segment(1), Some(&bearer(token.clone())))
        .await
        .unwrap();
    assert_eq!(answer, 1);
    assert_eq!(token.reads.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn public_requests_carry_no_authorization_and_query_is_encoded() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(wiremock::matchers::query_param("from", "a b"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(1)))
        .mount(&server)
        .await;
    let client = test_client(&server, fast());
    let answer: u8 =
        client.inner().send(Call::new(&GET).segment(1).query("from", "a b"), None).await.unwrap();
    assert_eq!(answer, 1);
}

#[tokio::test]
async fn refused_bearer_source_sends_nothing() {
    let server = MockServer::start().await;
    let client = test_client(&server, fast());
    let error = client
        .inner()
        .send::<Value>(Call::new(&SECRET), Some(&bearer(SignedOut)))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::SignedOut));
    assert!(received(&server).await.is_empty());
}

#[tokio::test]
async fn lighter_reads_map_failures_to_venue_errors() {
    let server = MockServer::start().await;
    Mock::given(path("/lighter/api/v1/account"))
        .and(wiremock::matchers::query_param("value", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"accounts": []})))
        .mount(&server)
        .await;
    Mock::given(path("/lighter/api/v1/account"))
        .and(wiremock::matchers::query_param("value", "2"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    Mock::given(path("/lighter/api/v1/account"))
        .and(wiremock::matchers::query_param("value", "3"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;
    let client = test_client(&server, fast());
    let ok: Value = client.inner().lighter_get("/api/v1/account?by=index&value=1").await.unwrap();
    assert_eq!(ok, json!({"accounts": []}));
    let error =
        client.inner().lighter_get::<Value>("/api/v1/account?by=index&value=2").await.unwrap_err();
    let Error::VenueApi(venue) = &error else { panic!("expected a venue error, got {error:?}") };
    assert_eq!(venue.status(), Some(500));
    assert_eq!(venue.venue(), &VenueId::Lighter);
    let error =
        client.inner().lighter_get::<Value>("/api/v1/account?by=index&value=3").await.unwrap_err();
    let Error::VenueApi(venue) = &error else { panic!("expected a venue error, got {error:?}") };
    assert_eq!(venue.status(), None);
    assert_eq!(received(&server).await.len(), 3, "never retried");
}

/// The caller's Lighter carrier: records each URL; account 1 answers, 2 is rate limited, any
/// other gets no answer.
#[derive(Default)]
struct Carrier(std::sync::Mutex<Vec<String>>);

impl crate::LighterReads for Carrier {
    fn get<'a>(&'a self, url: &'a str) -> crate::LighterRead<'a> {
        self.0.lock().unwrap().push(url.to_owned());
        Box::pin(async move {
            match url.rsplit('=').next() {
                Some("1") => Ok((200, br#"{"accounts": []}"#.to_vec())),
                Some("2") => Ok((429, Vec::new())),
                _ => Err("no route from this address".into()),
            }
        })
    }
}

#[tokio::test]
async fn lighter_reads_go_through_the_callers_carrier() {
    let server = MockServer::start().await;
    let carrier = Arc::new(Carrier::default());
    let uri = server.uri();
    let client = Client::builder(Environment::Custom { api_url: uri.clone(), app_origin: uri })
        .lighter_reads(Arc::clone(&carrier))
        .build()
        .unwrap();

    let ok: Value = client.inner().lighter_get("/api/v1/account?by=index&value=1").await.unwrap();
    assert_eq!(ok, json!({"accounts": []}));
    let error =
        client.inner().lighter_get::<Value>("/api/v1/account?by=index&value=2").await.unwrap_err();
    let Error::VenueApi(venue) = &error else { panic!("expected a venue error, got {error:?}") };
    assert_eq!((venue.venue(), venue.status()), (&VenueId::Lighter, Some(429)));
    let error =
        client.inner().lighter_get::<Value>("/api/v1/account?by=index&value=3").await.unwrap_err();
    let Error::VenueApi(venue) = &error else { panic!("expected a venue error, got {error:?}") };
    assert_eq!(venue.status(), None);
    assert!(format!("{:?}", std::error::Error::source(venue)).contains("no route"), "{venue:?}");
    assert_eq!(
        *carrier.0.lock().unwrap(),
        (1..=3)
            .map(|index| format!("{LIGHTER_API}/api/v1/account?by=index&value={index}"))
            .collect::<Vec<_>>(),
        "Lighter's own base, each read once"
    );
    assert!(received(&server).await.is_empty(), "nothing sent by the SDK's own client");
}

/// A `tracing` writer into a shared buffer.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn traces_and_errors_never_show_secrets() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("x-request-id", "req-trace").set_body_json(
                json!({"token": "mk_SENTINEL_CREDENTIAL", "signature": "0xfeedfacefeedface"}),
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503).set_body_json(envelope("VENUE_UNAVAILABLE", true)))
        .mount(&server)
        .await;
    struct Sentinel;
    impl BearerToken for Sentinel {
        fn bearer(&self) -> Result<String> {
            Ok("SENTINEL_SESSION_TOKEN".to_owned())
        }
    }
    // With a single dispatcher registered, tracing computes the interest of a callsite first hit
    // on another test thread from that thread's default and caches it as disabled; a second
    // live dispatcher makes registration consult every dispatcher, this test's included.
    let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(capture.clone())
        .with_ansi(false)
        .finish();
    let _default = tracing::subscriber::set_default(subscriber);
    let client = test_client(&server, fast());
    let body = json!({"name": "bot", "signature": "0xfeedfacefeedface"});
    let created: Value = client
        .inner()
        .send(Call::new(&SECRET).json(&body).unwrap(), Some(&bearer(Sentinel)))
        .await
        .unwrap();
    assert_eq!(created["token"], "mk_SENTINEL_CREDENTIAL");
    let error =
        client.inner().send::<u64>(Call::new(&SECRET), Some(&bearer(Sentinel))).await.unwrap_err();
    let retried = client
        .inner()
        .send::<Value>(Call::new(&GET).segment(1), Some(&bearer(Sentinel)))
        .await
        .unwrap_err();
    let mut text = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(
        text.contains("lamarge.http") && text.contains("postSecret") && text.contains("req-trace"),
        "{text}"
    );
    assert!(text.contains("retrying"), "{text}");
    let rendered = [&error, &retried].map(|value| format!("{value} {value:?}"));
    text.extend(rendered);
    for secret in ["SENTINEL_SESSION_TOKEN", "mk_SENTINEL_CREDENTIAL", "feedface"] {
        assert!(!text.contains(secret), "{secret} leaked:\n{text}");
    }
}

#[test]
fn request_futures_are_send() {
    fn check<T: Send>(_: T) {}
    let client = Client::new(Environment::Production).unwrap();
    let token = Token::default();
    check(client.inner().send::<Value>(Call::new(&GET).segment(1), Some(&bearer(token.clone()))));
    check(client.inner().lighter_get::<Value>("/api/v1/account"));
}
