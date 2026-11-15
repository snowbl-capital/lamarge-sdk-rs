//! The event feed through the production connector, against an in-process server answering
//! HTTP and the WebSocket (plain `ws`) on one port, under a base path.

use std::{
    collections::VecDeque,
    io::ErrorKind,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::watch,
};
use tokio_tungstenite::tungstenite::Message;

use super::{Change, EventStream, FillTracker, Snapshot, stream::Timing};
use crate::{
    Client, Environment, Error, Result, RetryPolicy,
    error::ErrorCode,
    model::{EventData, EventPage, FillState},
    testing::json::fixture,
    transport::BearerToken,
    types::{Cursor, FillId},
};

const FAST: Timing = Timing {
    connect: Duration::from_secs(2),
    idle: Duration::from_millis(300),
    pause: Duration::from_millis(20),
    max_pause: Duration::from_millis(100),
};

const FILL: &str = "0190f3a4-7b2c-7d10-8e4f-000000000004";

// --- scripted server ---------------------------------------------------------------------------

/// What one WebSocket connection does after reading the subscription; it then closes.
enum Act {
    Send(Value),
    Wait(Duration),
    /// Stay open and silent until the client goes.
    Hold,
}

enum Connection {
    /// Answer the upgrade with 429.
    Reject,
    Accept(Vec<Act>),
}

/// What the server received.
#[derive(Debug, Clone, PartialEq)]
enum Seen {
    Get {
        path: String,
        query: String,
        authorization: Option<String>,
    },
    Subscribed(Value),
    /// A held connection the client closed.
    Closed,
}

/// Answers in order; an unscripted replay is an empty last page at `after`, an unscripted
/// connection holds.
#[derive(Default)]
struct Script {
    snapshots: VecDeque<(u16, Value)>,
    replays: VecDeque<(u16, Value)>,
    connections: VecDeque<Connection>,
    seen: Vec<Seen>,
}

impl Script {
    fn snapshot(mut self, status: u16, body: Value) -> Self {
        self.snapshots.push_back((status, body));
        self
    }

    fn replay(mut self, status: u16, body: Value) -> Self {
        self.replays.push_back((status, body));
        self
    }

    fn connection(mut self, connection: Connection) -> Self {
        self.connections.push_back(connection);
        self
    }
}

struct Server {
    origin: String,
    script: Arc<Mutex<Script>>,
}

impl Server {
    async fn start(script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let script = Arc::new(Mutex::new(script));
        let shared = Arc::clone(&script);
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(serve(stream, Arc::clone(&shared)));
            }
        });
        Self { origin, script }
    }

    /// A client of the API at `<origin>/api`, without HTTP retries.
    fn client(&self) -> Client {
        Client::builder(Environment::Custom {
            api_url: format!("{}/api", self.origin),
            app_origin: self.origin.clone(),
        })
        .retry(RetryPolicy::none())
        .build()
        .unwrap()
    }

    fn seen(&self) -> Vec<Seen> {
        self.script.lock().unwrap().seen.clone()
    }

    fn subscriptions(&self) -> Vec<Value> {
        let seen = self.seen().into_iter();
        seen.filter_map(|seen| if let Seen::Subscribed(value) = seen { Some(value) } else { None })
            .collect()
    }

    fn gets(&self) -> Vec<(String, String, Option<String>)> {
        self.seen()
            .into_iter()
            .filter_map(|seen| match seen {
                Seen::Get { path, query, authorization } => Some((path, query, authorization)),
                _ => None,
            })
            .collect()
    }

    async fn eventually(&self, what: &str, done: impl Fn(&[Seen]) -> bool) {
        let wait = async {
            while !done(&self.seen()) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .unwrap_or_else(|_| panic!("{what}: {:?}", self.seen()));
    }
}

async fn serve(stream: TcpStream, script: Arc<Mutex<Script>>) {
    let line = request_line(&stream).await;
    let target = line.split(' ').nth(1).unwrap_or_default().to_owned();
    if target == "/api/v1/stream" {
        websocket(stream, &script).await;
    } else {
        http(stream, &target, &script).await;
    }
}

fn record(script: &Mutex<Script>, seen: Seen) {
    script.lock().unwrap().seen.push(seen);
}

async fn websocket(stream: TcpStream, script: &Mutex<Script>) {
    let connection = script.lock().unwrap().connections.pop_front();
    let acts = match connection.unwrap_or(Connection::Accept(vec![Act::Hold])) {
        Connection::Reject => {
            read_head(&stream).await;
            respond(&stream, 429, &envelope("RATE_LIMITED")).await;
            return;
        }
        Connection::Accept(acts) => acts,
    };
    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
    let Some(Ok(Message::Text(text))) = ws.next().await else { return };
    record(script, Seen::Subscribed(serde_json::from_str(&text).unwrap()));
    for act in acts {
        match act {
            Act::Send(frame) => {
                if ws.send(Message::text(frame.to_string())).await.is_err() {
                    return;
                }
            }
            Act::Wait(duration) => tokio::time::sleep(duration).await,
            Act::Hold => {
                while let Some(Ok(_)) = ws.next().await {}
                record(script, Seen::Closed);
                return;
            }
        }
    }
    let _ = ws.close(None).await;
}

async fn http(stream: TcpStream, target: &str, script: &Mutex<Script>) {
    let head = read_head(&stream).await;
    let authorization = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("authorization").then(|| value.trim().to_owned())
    });
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let path = path.strip_prefix("/api").unwrap_or("not under the base path").to_owned();
    let after = query.split('&').find_map(|pair| pair.strip_prefix("after=")).unwrap_or("0");
    let after: u64 = after.parse().unwrap();
    let answer = {
        let mut script = script.lock().unwrap();
        script.seen.push(Seen::Get { path: path.clone(), query: query.to_owned(), authorization });
        match path.as_str() {
            "/v1/snapshot" | "/v1/me/snapshot" => {
                script.snapshots.pop_front().unwrap_or((503, envelope("INTERNAL")))
            }
            "/v1/events" | "/v1/me/events" => {
                script.replays.pop_front().unwrap_or_else(|| (200, page(&[], after, false)))
            }
            _ => (404, envelope("NOT_FOUND")),
        }
    };
    respond(&stream, answer.0, &answer.1).await;
}

/// The request line, peeked so a WebSocket handshake can still read it.
async fn request_line(stream: &TcpStream) -> String {
    let mut buffer = [0u8; 512];
    loop {
        let read = stream.peek(&mut buffer).await.unwrap();
        if let Some(end) = buffer[..read].iter().position(|byte| *byte == b'\n') {
            return String::from_utf8_lossy(&buffer[..end]).into_owned();
        }
        if read == 0 {
            return String::new();
        }
        tokio::task::yield_now().await;
    }
}

/// Reads a request head (requests carry no body here).
async fn read_head(stream: &TcpStream) -> String {
    let mut head = Vec::new();
    let mut buffer = [0u8; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        stream.readable().await.unwrap();
        match stream.try_read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => head.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) => panic!("{error}"),
        }
    }
    String::from_utf8(head).unwrap()
}

async fn respond(stream: &TcpStream, status: u16, body: &Value) {
    let body = body.to_string();
    let answer = format!(
        "HTTP/1.1 {status} Scripted\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut bytes = answer.as_bytes();
    while !bytes.is_empty() {
        stream.writable().await.unwrap();
        match stream.try_write(bytes) {
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) => panic!("{error}"),
        }
    }
}

// --- payloads ----------------------------------------------------------------------------------

fn envelope(code: &str) -> Value {
    // The server always states `retryable`, false on a plain 500 (server/src/http.rs).
    json!({"error": {"code": code, "message": "scripted", "request_id": "req-1", "retryable": false}})
}

fn page(events: &[Value], next: u64, has_more: bool) -> Value {
    json!({
        "events": events,
        "next_cursor": next.to_string(),
        "has_more": has_more,
        "oldest_cursor": "1",
        "server_time": "2026-11-14T12:00:00Z",
    })
}

fn book_event(cursor: u64) -> Value {
    json!({
        "cursor": cursor.to_string(),
        "resource_id": "hyperliquid",
        "revision": "1",
        "occurred_at": "2026-11-14T12:00:00Z",
        "kind": "book.changed",
        "data": {"from": "hyperliquid"},
    })
}

fn fill_event(cursor: u64, revision: u64, state: &str) -> Value {
    let mut event: Value = fixture("event_page/events/2");
    event["cursor"] = json!(cursor.to_string());
    event["revision"] = json!(revision.to_string());
    event["data"]["state"] = json!(state);
    event
}

fn public_snapshot(cursor: u64) -> Value {
    let mut snapshot: Value = fixture("snapshot");
    snapshot["cursor"] = json!(cursor.to_string());
    snapshot
}

/// The fixture's private snapshot: one fill `FILL` at revision 9, `SETTLED`.
fn private_snapshot(cursor: u64) -> Value {
    let mut snapshot: Value = fixture("private_snapshot");
    snapshot["cursor"] = json!(cursor.to_string());
    snapshot
}

fn control(kind: &str) -> Value {
    json!({"type": kind, "reason": "scripted"})
}

// --- feed helpers ------------------------------------------------------------------------------

/// A session token that can be renewed (`Some`) or signed out (`None`), waking live streams as
/// a session does; clones share it.
#[derive(Clone)]
struct Rotating(Arc<(Mutex<Option<String>>, watch::Sender<()>)>);

impl Rotating {
    fn new(token: &str) -> Self {
        Self(Arc::new((Mutex::new(Some(token.to_owned())), watch::Sender::new(()))))
    }

    fn set(&self, token: Option<&str>) {
        *self.0.0.lock().unwrap() = token.map(str::to_owned);
        self.0.1.send_replace(());
    }
}

impl BearerToken for Rotating {
    fn bearer(&self) -> Result<String> {
        self.0.0.lock().unwrap().clone().ok_or(Error::SignedOut)
    }

    fn changes(&self) -> Option<watch::Receiver<()>> {
        Some(self.0.1.subscribe())
    }
}

/// A public feed with short deadlines; the client is dropped at once, the feed owns its state.
fn public_feed(server: &Server, timing: Timing) -> EventStream {
    EventStream::with_timing(Arc::clone(server.client().inner()), None, timing)
}

fn private_feed(server: &Server, token: &Rotating) -> EventStream {
    let auth: Arc<dyn BearerToken> = Arc::new(token.clone());
    EventStream::with_timing(Arc::clone(server.client().inner()), Some(auth), FAST)
}

async fn next(feed: &mut EventStream) -> Option<Result<Change>> {
    tokio::time::timeout(Duration::from_secs(10), feed.next()).await.expect("the feed stalled")
}

async fn change(feed: &mut EventStream) -> Change {
    next(feed).await.expect("the feed ended").expect("the feed failed")
}

async fn events(feed: &mut EventStream) -> EventPage {
    match change(feed).await {
        Change::Events(page) => page,
        other => panic!("expected events, got {other:?}"),
    }
}

async fn resync(feed: &mut EventStream) -> Snapshot {
    match change(feed).await {
        Change::Resync(snapshot) => snapshot,
        other => panic!("expected a resync, got {other:?}"),
    }
}

async fn fatal(feed: &mut EventStream) -> Error {
    let error = next(feed).await.expect("the feed ended").expect_err("expected an error");
    assert!(next(feed).await.is_none(), "the stream ends after a fatal error");
    error
}

fn afters(subscriptions: &[Value]) -> Vec<&str> {
    subscriptions.iter().map(|value| value["after"].as_str().unwrap()).collect()
}

// --- tests -------------------------------------------------------------------------------------

#[tokio::test]
async fn public_feed_skips_heartbeats_and_replays_after_a_close() {
    let unknown = json!({
        "cursor": "1009", "resource_id": "x", "revision": "1",
        "occurred_at": "2026-11-14T12:00:00Z", "kind": "maker.renamed", "data": {"name": "n"},
    });
    let server = Server::start(
        Script::default()
            .snapshot(200, public_snapshot(1007))
            .connection(Connection::Accept(vec![
                Act::Send(page(&[book_event(1007)], 1008, false)),
                Act::Send(page(&[], 1009, false)),
            ]))
            .replay(200, page(&[unknown], 1010, false)),
    )
    .await;
    let mut feed = server.client().events().subscribe();

    let Snapshot::Public(snapshot) = resync(&mut feed).await else { panic!("public expected") };
    assert_eq!(snapshot.cursor, Cursor::new(1007));
    let live = events(&mut feed).await;
    assert_eq!(live.next_cursor, Cursor::new(1008));
    assert!(matches!(live.events[0].data, EventData::BookChanged(_)));
    let replayed = events(&mut feed).await;
    assert_eq!(replayed.next_cursor, Cursor::new(1010), "the heartbeat page was not yielded");
    let EventData::Unknown { kind, data } = &replayed.events[0].data else {
        panic!("{replayed:?}")
    };
    assert_eq!((kind.as_str(), data), ("maker.renamed", &json!({"name": "n"})));

    assert_eq!(server.subscriptions(), [json!({"after": "1007"})], "public: no token key");
    assert_eq!(
        server.gets(),
        [
            ("/v1/snapshot".to_owned(), String::new(), None),
            ("/v1/events".to_owned(), "after=1009&limit=500".to_owned(), None),
        ],
        "the heartbeat advanced the cursor to 1009"
    );
}

#[tokio::test]
async fn an_idle_connection_falls_back_to_replay_then_reconnects_from_the_new_cursor() {
    let server = Server::start(
        Script::default()
            .snapshot(200, public_snapshot(1007))
            .connection(Connection::Accept(vec![
                Act::Send(page(&[book_event(1007)], 1008, false)),
                Act::Hold,
            ]))
            .replay(200, page(&[book_event(1008)], 1009, false))
            .connection(Connection::Accept(vec![
                Act::Send(page(&[book_event(1009)], 1010, false)),
                Act::Hold,
            ])),
    )
    .await;
    let mut feed = public_feed(&server, FAST);

    resync(&mut feed).await;
    assert_eq!(events(&mut feed).await.next_cursor, Cursor::new(1008));
    assert_eq!(events(&mut feed).await.next_cursor, Cursor::new(1009), "replayed after idling");
    assert_eq!(events(&mut feed).await.next_cursor, Cursor::new(1010), "live again");

    assert_eq!(afters(&server.subscriptions()), ["1007", "1009"]);
    assert_eq!(server.gets()[1].1, "after=1008&limit=500");
    server.eventually("the idle socket is closed", |seen| seen.contains(&Seen::Closed)).await;
}

#[tokio::test]
async fn replay_follows_has_more_and_skips_empty_pages() {
    let server = Server::start(
        Script::default()
            .snapshot(200, public_snapshot(1007))
            .connection(Connection::Accept(vec![]))
            .replay(200, page(&[], 2000, true))
            .replay(200, page(&[book_event(2000)], 2001, true))
            .replay(200, page(&[book_event(2001)], 2002, false)),
    )
    .await;
    let mut feed = public_feed(&server, FAST);

    resync(&mut feed).await;
    assert_eq!(events(&mut feed).await.next_cursor, Cursor::new(2001));
    assert_eq!(events(&mut feed).await.next_cursor, Cursor::new(2002));
    let queries: Vec<String> = server.gets().into_iter().skip(1).map(|get| get.1).collect();
    assert_eq!(queries, ["after=1007&limit=500", "after=2000&limit=500", "after=2001&limit=500"]);
}

#[tokio::test]
async fn resync_required_takes_a_new_snapshot() {
    let server = Server::start(
        Script::default()
            .snapshot(200, public_snapshot(1007))
            .snapshot(200, public_snapshot(2000))
            .connection(Connection::Accept(vec![Act::Send(control("resync_required")), Act::Hold]))
            .connection(Connection::Accept(vec![
                Act::Send(page(&[book_event(2000)], 2001, false)),
                Act::Hold,
            ])),
    )
    .await;
    let mut feed = public_feed(&server, FAST);

    assert_eq!(resync(&mut feed).await.cursor(), Cursor::new(1007));
    assert_eq!(resync(&mut feed).await.cursor(), Cursor::new(2000));
    assert_eq!(events(&mut feed).await.next_cursor, Cursor::new(2001));
    assert_eq!(afters(&server.subscriptions()), ["1007", "2000"]);
}

#[tokio::test]
async fn a_cursor_gone_from_the_journal_takes_a_new_snapshot() {
    let server = Server::start(
        Script::default()
            .snapshot(200, public_snapshot(1007))
            .snapshot(200, public_snapshot(3000))
            .connection(Connection::Accept(vec![]))
            .replay(410, envelope("RESYNC_REQUIRED")),
    )
    .await;
    let mut feed = public_feed(&server, FAST);

    assert_eq!(resync(&mut feed).await.cursor(), Cursor::new(1007));
    assert_eq!(resync(&mut feed).await.cursor(), Cursor::new(3000));
    let paths: Vec<String> = server.gets().into_iter().map(|get| get.0).collect();
    assert_eq!(paths, ["/v1/snapshot", "/v1/events", "/v1/snapshot"]);
}

#[tokio::test]
async fn transient_failures_never_end_the_stream() {
    let server = Server::start(
        Script::default()
            .snapshot(503, envelope("INTERNAL"))
            .snapshot(200, public_snapshot(1007))
            .connection(Connection::Reject)
            .replay(500, envelope("INTERNAL"))
            .replay(429, envelope("RATE_LIMITED"))
            .connection(Connection::Accept(vec![]))
            .connection(Connection::Accept(vec![
                Act::Send(page(&[book_event(1007)], 1008, false)),
                Act::Hold,
            ])),
    )
    .await;
    let mut feed = public_feed(&server, FAST);

    assert_eq!(resync(&mut feed).await.cursor(), Cursor::new(1007));
    assert_eq!(events(&mut feed).await.next_cursor, Cursor::new(1008));
    let paths: Vec<String> = server.gets().into_iter().map(|get| get.0).collect();
    assert_eq!(paths, ["/v1/snapshot", "/v1/snapshot", "/v1/events", "/v1/events"]);
    assert_eq!(afters(&server.subscriptions()), ["1007", "1007"], "the rejected upgrade sent none");
}

#[tokio::test]
async fn authentication_required_ends_the_stream() {
    let server = Server::start(Script::default().snapshot(200, private_snapshot(1007)).connection(
        Connection::Accept(vec![Act::Send(control("authentication_required")), Act::Hold]),
    ))
    .await;
    let token = Rotating::new("first");
    let mut feed = private_feed(&server, &token);

    assert!(matches!(resync(&mut feed).await, Snapshot::Private(_)));
    let error = fatal(&mut feed).await;
    assert!(error.is_unauthenticated(), "{error:?}");
    assert_eq!(error.code(), Some(&ErrorCode::Unauthenticated));
    assert_eq!(
        server.gets(),
        [("/v1/me/snapshot".to_owned(), String::new(), Some("Bearer first".to_owned()))]
    );
}

#[tokio::test]
async fn a_refused_replay_ends_the_stream() {
    let server = Server::start(
        Script::default()
            .snapshot(200, public_snapshot(1007))
            .connection(Connection::Accept(vec![]))
            .replay(403, envelope("FORBIDDEN")),
    )
    .await;
    let mut feed = public_feed(&server, FAST);

    resync(&mut feed).await;
    let error = fatal(&mut feed).await;
    assert_eq!(error.status(), Some(403));
}

#[tokio::test]
async fn a_cursor_moving_backwards_ends_the_stream() {
    let server = Server::start(Script::default().snapshot(200, public_snapshot(1007)).connection(
        Connection::Accept(vec![Act::Send(page(&[book_event(1000)], 1001, false)), Act::Hold]),
    ))
    .await;
    let mut feed = public_feed(&server, FAST);

    resync(&mut feed).await;
    let Error::Decode(error) = fatal(&mut feed).await else { panic!("decode error expected") };
    assert_eq!(error.to_string(), "event cursor moved backwards");
}

#[tokio::test]
async fn an_unreadable_frame_ends_the_stream() {
    let server = Server::start(
        Script::default()
            .snapshot(200, public_snapshot(1007))
            .connection(Connection::Accept(vec![Act::Send(json!({"events": "none"})), Act::Hold])),
    )
    .await;
    let mut feed = public_feed(&server, FAST);

    resync(&mut feed).await;
    assert!(matches!(fatal(&mut feed).await, Error::Decode(_)));
}

#[tokio::test]
async fn the_token_is_read_again_at_each_reconnection() {
    let server = Server::start(
        Script::default()
            .snapshot(200, private_snapshot(1007))
            .connection(Connection::Accept(vec![Act::Send(page(&[book_event(1007)], 1008, false))]))
            .replay(200, page(&[book_event(1008)], 1009, false))
            .connection(Connection::Accept(vec![
                Act::Send(page(&[book_event(1009)], 1010, false)),
                Act::Hold,
            ])),
    )
    .await;
    let token = Rotating::new("first");
    let mut feed = private_feed(&server, &token);

    resync(&mut feed).await;
    assert_eq!(events(&mut feed).await.next_cursor, Cursor::new(1008));
    // The close is read before the change: the token is read again at the reconnection.
    tokio::time::sleep(Duration::from_millis(100)).await;
    token.set(Some("second"));
    assert_eq!(events(&mut feed).await.next_cursor, Cursor::new(1009));
    assert_eq!(events(&mut feed).await.next_cursor, Cursor::new(1010));

    assert_eq!(
        server.subscriptions(),
        [json!({"after": "1007", "token": "first"}), json!({"after": "1009", "token": "second"})]
    );
    let authorizations: Vec<Option<String>> = server.gets().into_iter().map(|get| get.2).collect();
    assert_eq!(authorizations, [Some("Bearer first".to_owned()), Some("Bearer second".to_owned())]);
}

#[tokio::test]
async fn a_renewal_reconnects_a_live_stream_with_the_new_token() {
    let server = Server::start(
        Script::default()
            .snapshot(200, private_snapshot(1007))
            .connection(Connection::Accept(vec![
                Act::Send(page(&[book_event(1007)], 1008, false)),
                Act::Hold,
            ]))
            .connection(Connection::Accept(vec![
                Act::Send(page(&[book_event(1008)], 1009, false)),
                Act::Hold,
            ])),
    )
    .await;
    let token = Rotating::new("first");
    let mut feed = private_feed(&server, &token);

    resync(&mut feed).await;
    assert_eq!(events(&mut feed).await.next_cursor, Cursor::new(1008));
    token.set(Some("second"));
    assert_eq!(events(&mut feed).await.next_cursor, Cursor::new(1009));
    assert_eq!(
        server.subscriptions(),
        [json!({"after": "1007", "token": "first"}), json!({"after": "1008", "token": "second"})]
    );
    server.eventually("the old connection is closed", |seen| seen.contains(&Seen::Closed)).await;
}

#[tokio::test]
async fn a_refusal_of_the_revoked_token_after_a_renewal_reconnects() {
    let server = Server::start(
        Script::default()
            .snapshot(200, private_snapshot(1007))
            .connection(Connection::Accept(vec![
                Act::Send(page(&[book_event(1007)], 1008, false)),
                Act::Send(control("authentication_required")),
                Act::Hold,
            ]))
            .connection(Connection::Accept(vec![
                Act::Send(page(&[book_event(1008)], 1009, false)),
                Act::Hold,
            ])),
    )
    .await;
    let token = Rotating::new("first");
    let mut feed = private_feed(&server, &token);

    resync(&mut feed).await;
    assert_eq!(events(&mut feed).await.next_cursor, Cursor::new(1008));
    // The refusal is buffered before the renewal is noticed: it concerns the old token.
    tokio::time::sleep(Duration::from_millis(100)).await;
    token.set(Some("second"));
    assert_eq!(events(&mut feed).await.next_cursor, Cursor::new(1009));
    assert_eq!(afters(&server.subscriptions()), ["1007", "1008"]);
}

#[tokio::test]
async fn a_sign_out_ends_a_live_stream() {
    let server = Server::start(Script::default().snapshot(200, private_snapshot(1007)).connection(
        Connection::Accept(vec![Act::Send(page(&[book_event(1007)], 1008, false)), Act::Hold]),
    ))
    .await;
    let token = Rotating::new("first");
    let mut feed = private_feed(&server, &token);

    resync(&mut feed).await;
    events(&mut feed).await;
    token.set(None);
    assert!(matches!(fatal(&mut feed).await, Error::SignedOut));
    server.eventually("the connection is closed", |seen| seen.contains(&Seen::Closed)).await;
    assert_eq!(server.subscriptions().len(), 1);
}

#[tokio::test]
async fn a_malformed_private_resync_leaves_the_tracker_unchanged() {
    let mut missing_fills = private_snapshot(2000);
    missing_fills.as_object_mut().unwrap().remove("fills");
    let mut missing_revision = private_snapshot(2000);
    missing_revision["fills"][0].as_object_mut().unwrap().remove("revision");
    for malformed in [missing_fills, missing_revision] {
        let server = Server::start(
            Script::default()
                .snapshot(200, private_snapshot(1007))
                .snapshot(200, malformed)
                .connection(Connection::Accept(vec![
                    Act::Send(page(&[fill_event(1007, 10, "REFUNDED")], 1008, false)),
                    Act::Send(control("resync_required")),
                    Act::Hold,
                ])),
        )
        .await;
        let mut feed = private_feed(&server, &Rotating::new("t"));
        let mut tracker = FillTracker::default();
        let error = loop {
            match next(&mut feed).await.expect("an error ends the feed") {
                Ok(change) => tracker.apply(&change),
                Err(error) => break error,
            }
        };
        assert!(matches!(error, Error::Decode(_)), "{error:?}");
        assert!(next(&mut feed).await.is_none());
        assert!(tracker.is_synced());
        let fill = tracker.get(FILL.parse::<FillId>().unwrap()).unwrap();
        assert_eq!(fill.state, FillState::Refunded, "the event's state survives the bad resync");
        assert_eq!(tracker.iter().count(), 1);
    }
}

#[tokio::test]
async fn cancelled_polls_lose_nothing_and_dropping_closes_the_connection() {
    let server = Server::start(Script::default().snapshot(200, public_snapshot(1007)).connection(
        Connection::Accept(vec![
            Act::Wait(Duration::from_millis(300)),
            Act::Send(page(&[book_event(1007)], 1008, false)),
            Act::Hold,
        ]),
    ))
    .await;
    let mut feed = public_feed(&server, Timing { idle: Duration::from_secs(5), ..FAST });

    resync(&mut feed).await;
    let cancelled = tokio::time::timeout(Duration::from_millis(50), feed.next()).await;
    assert!(cancelled.is_err(), "nothing yet");
    assert_eq!(events(&mut feed).await.next_cursor, Cursor::new(1008));
    assert_eq!(server.subscriptions().len(), 1, "the cancelled poll kept its connection");

    drop(feed);
    server
        .eventually("the dropped stream closes its socket", |seen| seen.contains(&Seen::Closed))
        .await;
}

#[tokio::test]
async fn subscribe_is_lazy_and_outlives_its_client() {
    fn owned<T: Send + Unpin + 'static>(_: &T) {}
    let server = Server::start(Script::default().snapshot(200, public_snapshot(1007))).await;
    let mut feed = {
        let client = server.client();
        client.events().subscribe()
    };
    owned(&feed);
    assert_eq!(format!("{feed:?}"), "EventStream { .. }");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(server.seen().is_empty(), "nothing is sent before the first poll");
    assert_eq!(resync(&mut feed).await.cursor(), Cursor::new(1007));
}
