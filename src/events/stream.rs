//! The event stream: snapshot, WebSocket, HTTP replay and resync.

use std::{
    fmt,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use futures_core::Stream;
use futures_util::{
    StreamExt as _,
    future::{self, Either},
    stream,
};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

use super::socket::{self, Socket};
use crate::{
    Error, Result,
    api::{Events, PublicEvents},
    error::{ApiError, DecodeError, ErrorCode, TransportError},
    model::{self, EventPage},
    transport::{BearerToken, Inner, jitter},
    types::Cursor,
};

/// A snapshot of the event journal: public from a `Client`, private from a `Session` or
/// `Quoter`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Snapshot {
    /// The public books.
    Public(model::PublicSnapshot),
    /// The reader's orders, fills and quote identities.
    Private(model::PrivateSnapshot),
}

impl Snapshot {
    /// The journal position the snapshot is at; replay from it.
    pub fn cursor(&self) -> Cursor {
        match self {
            Self::Public(snapshot) => snapshot.cursor,
            Self::Private(snapshot) => snapshot.cursor,
        }
    }
}

/// One item of an [`EventStream`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Change {
    /// A new snapshot: replace all state derived from earlier changes.
    Resync(Snapshot),
    /// Events after the previous change. Empty heartbeat pages advance the cursor and are not
    /// yielded.
    Events(EventPage),
}

/// The event journal as a stream of [`Change`]s: a snapshot first, then pages pushed over the
/// WebSocket; HTTP replay catches up between connections, and a cursor the journal no longer
/// holds triggers a new snapshot.
///
/// Retryable failures (network, 429, 5xx) never end the stream: it reconnects with backoff. A
/// session renewal reconnects with the new token; a sign-out ends the stream with
/// [`Error::SignedOut`]. A fatal error (authentication, another 4xx, an unreadable payload, a
/// cursor moving backwards) is yielded once, then the stream ends. It runs no background task: it
/// makes progress only while polled, and dropping it closes its connection.
#[must_use = "streams do nothing unless polled"]
pub struct EventStream {
    inner: Pin<Box<dyn Stream<Item = Result<Change>> + Send>>,
}

impl EventStream {
    pub(crate) fn new(inner: Arc<Inner>, auth: Option<Arc<dyn BearerToken>>) -> Self {
        Self::with_timing(inner, auth, Timing::DEFAULT)
    }

    pub(super) fn with_timing(
        inner: Arc<Inner>,
        auth: Option<Arc<dyn BearerToken>>,
        timing: Timing,
    ) -> Self {
        let feed = Feed {
            inner,
            changes: auth.as_ref().and_then(|auth| auth.changes()),
            auth,
            timing,
            cursor: Cursor::default(),
            step: Step::Snapshot,
            failures: 0,
        };
        let changes = stream::unfold(feed, |mut feed| async move {
            let change = feed.next().await?;
            Some((change, feed))
        });
        Self { inner: Box::pin(changes.fuse()) }
    }
}

impl Stream for EventStream {
    type Item = Result<Change>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl fmt::Debug for EventStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventStream").finish_non_exhaustive()
    }
}

/// The feed's deadlines and waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Timing {
    /// To open the WebSocket and send the subscription.
    pub(super) connect: Duration,
    /// Without a frame before the connection counts as lost (the server sends one at least
    /// every 500 ms).
    pub(super) idle: Duration,
    /// Before reconnecting after a healthy connection; the backoff floor.
    pub(super) pause: Duration,
    /// The backoff ceiling.
    pub(super) max_pause: Duration,
}

impl Timing {
    const DEFAULT: Self = Self {
        connect: Duration::from_secs(10),
        idle: Duration::from_secs(5),
        pause: Duration::from_millis(500),
        max_pause: Duration::from_secs(5),
    };

    /// The wait before the next attempt after `failures` consecutive failed ones: `pause`
    /// after none, else uniform in `[pause, min(pause · 2^failures, max_pause))`.
    fn backoff(&self, failures: u32) -> Duration {
        let ceiling = self.pause.saturating_mul(2u32.saturating_pow(failures)).min(self.max_pause);
        self.pause + jitter(ceiling.saturating_sub(self.pause))
    }
}

/// The most events a replay request asks for.
const REPLAY_LIMIT: u16 = 500;

/// The feed's next action.
enum Step {
    Snapshot,
    Connect,
    Live(Box<Socket>),
    Replay,
    Pause,
    Done,
}

/// The state behind an [`EventStream`]; owned by the stream, so dropping the stream drops it.
struct Feed {
    inner: Arc<Inner>,
    auth: Option<Arc<dyn BearerToken>>,
    /// Credential changes; marked seen before each use of the token.
    changes: Option<watch::Receiver<()>>,
    timing: Timing,
    /// The next cursor of the last page processed.
    cursor: Cursor,
    step: Step,
    /// Consecutive reconnections without a WebSocket page; reset by a page or a snapshot.
    failures: u32,
}

/// Whether the feed outlasts `error`: the server marks some recoverable answers (a 500 after a
/// database failure) not retryable for writes, but a read of the feed can always be repeated.
fn transient(error: &Error) -> bool {
    error.is_retryable() || error.status().is_some_and(|status| status == 429 || status >= 500)
}

impl Feed {
    /// The journal's snapshot: the reader's with a credential, else the public one.
    async fn snapshot(&self) -> Result<Snapshot> {
        let inner = &self.inner;
        match &self.auth {
            Some(auth) => {
                Events { inner, auth: Some(auth) }.snapshot().await.map(Snapshot::Private)
            }
            None => PublicEvents { inner, auth: None }.snapshot().await.map(Snapshot::Public),
        }
    }

    /// The journal's events after the cursor: the reader's with a credential, else public.
    async fn page(&self) -> Result<EventPage> {
        let (inner, after) = (&self.inner, self.cursor);
        match &self.auth {
            Some(auth) => Events { inner, auth: Some(auth) }.page(after, Some(REPLAY_LIMIT)).await,
            None => PublicEvents { inner, auth: None }.page(after, Some(REPLAY_LIMIT)).await,
        }
    }

    /// The next change, or `None` once ended. A fatal error is returned once, then `None`.
    async fn next(&mut self) -> Option<Result<Change>> {
        loop {
            // `Done` until a step decides otherwise, so an early return ends the stream.
            match std::mem::replace(&mut self.step, Step::Done) {
                Step::Done => return None,
                Step::Snapshot => {
                    self.mark();
                    match self.snapshot().await {
                        Ok(snapshot) => {
                            self.cursor = snapshot.cursor();
                            self.failures = 0;
                            self.step = Step::Connect;
                            return Some(Ok(Change::Resync(snapshot)));
                        }
                        Err(error) if self.renewed(&error) => self.step = Step::Snapshot,
                        Err(error) if transient(&error) => {
                            tracing::warn!(%error, "event stream: snapshot failed, retrying");
                            self.pause().await;
                            self.step = Step::Snapshot;
                        }
                        Err(error) => return Some(Err(error)),
                    }
                }
                Step::Connect => match self.connect().await {
                    Ok(socket) => self.step = Step::Live(Box::new(socket)),
                    Err(error) if transient(&error) => {
                        tracing::warn!(%error, "event stream: connection failed, replaying");
                        self.step = Step::Replay;
                    }
                    Err(error) => return Some(Err(error)),
                },
                Step::Live(socket) => {
                    if let Some(item) = self.live(socket).await {
                        return Some(item);
                    }
                }
                Step::Replay => {
                    if let Some(item) = self.replay().await {
                        return Some(item);
                    }
                }
                Step::Pause => {
                    self.pause().await;
                    self.step = Step::Connect;
                }
            }
        }
    }

    /// Marks the current credentials as the ones in use.
    fn mark(&mut self) {
        if let Some(changes) = &mut self.changes {
            changes.borrow_and_update();
        }
    }

    /// Whether `error` refused credentials that have changed since [`mark`](Self::mark): a
    /// renewal raced the request, which is then made again.
    fn renewed(&self, error: &Error) -> bool {
        error.is_unauthenticated()
            && self.changes.as_ref().is_some_and(|changes| changes.has_changed().unwrap_or(false))
    }

    /// Reads one frame: a non-empty page or a fatal error to yield, else `None` with the next
    /// step set. A credential change reconnects, or ends the stream once signed out.
    async fn live(&mut self, mut socket: Box<Socket>) -> Option<Result<Change>> {
        let frame = {
            let read = std::pin::pin!(tokio::time::timeout(self.timing.idle, socket.next()));
            let changed = std::pin::pin!(changed(&mut self.changes));
            match future::select(read, changed).await {
                Either::Left((frame, _)) => Some(frame),
                Either::Right(((), _)) => None,
            }
        };
        let Some(frame) = frame else {
            if let Some(Err(error)) = self.auth.as_ref().map(|auth| auth.bearer()) {
                return Some(Err(error));
            }
            tracing::debug!("event stream: credentials changed, reconnecting");
            self.step = Step::Connect;
            return None;
        };
        let text = match frame {
            Ok(Some(Ok(Message::Text(text)))) => text,
            // Pings are answered by the socket; the server sends nothing else.
            Ok(Some(Ok(
                Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_),
            ))) => {
                self.step = Step::Live(socket);
                return None;
            }
            Ok(Some(Ok(Message::Close(_))) | None) => {
                tracing::warn!("event stream: connection closed, replaying");
                self.step = Step::Replay;
                return None;
            }
            Ok(Some(Err(error))) => {
                let error = TransportError::websocket(error);
                tracing::warn!(%error, "event stream: connection lost, replaying");
                self.step = Step::Replay;
                return None;
            }
            Err(_) => {
                tracing::warn!("event stream: no frame within the idle deadline, replaying");
                self.step = Step::Replay;
                return None;
            }
        };
        match decode_frame(text.as_str()) {
            Ok(Frame::Page(page)) => {
                self.failures = 0;
                let page = match self.advance(page) {
                    Ok(page) => page,
                    Err(error) => return Some(Err(error)),
                };
                self.step = Step::Live(socket);
                page.map(|page| Ok(Change::Events(page)))
            }
            Ok(Frame::Control { kind: ControlKind::ResyncRequired, .. }) => {
                self.step = Step::Snapshot;
                None
            }
            Ok(Frame::Control { kind: ControlKind::AuthenticationRequired, reason }) => {
                let error = unauthenticated(&reason);
                if self.renewed(&error) {
                    self.step = Step::Connect;
                    return None;
                }
                Some(Err(error))
            }
            Ok(Frame::Control { kind: ControlKind::Unavailable | ControlKind::Other, reason }) => {
                tracing::warn!(reason, "event stream: server unavailable, replaying");
                self.step = Step::Replay;
                None
            }
            Err(error) => {
                let error = DecodeError::json(None, None, text.as_bytes(), &error, false);
                Some(Err(Error::Decode(Box::new(error))))
            }
        }
    }

    /// Reads one replay page: a non-empty page or a fatal error to yield, else `None` with the
    /// next step set.
    async fn replay(&mut self) -> Option<Result<Change>> {
        self.mark();
        match self.page().await {
            Ok(page) => {
                let more = page.has_more;
                let page = match self.advance(page) {
                    Ok(page) => page,
                    Err(error) => return Some(Err(error)),
                };
                self.step = if more { Step::Replay } else { Step::Pause };
                page.map(|page| Ok(Change::Events(page)))
            }
            Err(error) if error.code() == Some(&ErrorCode::ResyncRequired) => {
                self.step = Step::Snapshot;
                None
            }
            Err(error) if self.renewed(&error) => {
                self.step = Step::Replay;
                None
            }
            Err(error) if transient(&error) => {
                tracing::warn!(%error, "event stream: replay failed, reconnecting");
                self.step = Step::Pause;
                None
            }
            Err(error) => Some(Err(error)),
        }
    }

    /// Opens the WebSocket from the current cursor with the current token.
    async fn connect(&mut self) -> Result<Socket> {
        self.mark();
        let token = self.auth.as_ref().map(|source| source.bearer()).transpose()?;
        let subscription =
            serde_json::to_string(&Subscription { after: self.cursor, token: token.as_deref() })
                .map_err(|error| Error::InvalidRequest(format!("subscription: {error}").into()))?;
        tokio::time::timeout(self.timing.connect, socket::connect(&self.inner, subscription))
            .await
            .unwrap_or(Err(Error::Transport(TransportError::timeout())))
    }

    /// Moves the cursor to `page`'s next cursor; the page itself unless it is empty.
    fn advance(&mut self, page: EventPage) -> Result<Option<EventPage>> {
        if page.next_cursor < self.cursor {
            let error = DecodeError::invalid("event cursor moved backwards");
            return Err(Error::Decode(Box::new(error)));
        }
        self.cursor = page.next_cursor;
        Ok((!page.events.is_empty()).then_some(page))
    }

    async fn pause(&mut self) {
        let delay = self.timing.backoff(self.failures);
        self.failures = self.failures.saturating_add(1);
        tokio::time::sleep(delay).await;
    }
}

/// Resolves when the credentials change; never for credentials that cannot.
async fn changed(changes: &mut Option<watch::Receiver<()>>) {
    if let Some(changes) = changes
        && changes.changed().await.is_ok()
    {
        return;
    }
    future::pending::<()>().await;
}

/// The error an `authentication_required` control frame ends the stream with.
fn unauthenticated(reason: &str) -> Error {
    let body = serde_json::json!({
        "error": {"code": "UNAUTHENTICATED", "message": reason, "retryable": false}
    });
    Error::Api(Box::new(ApiError::from_answer(401, body.to_string().as_bytes(), None, None)))
}

/// The first frame the client sends: `StreamSubscription`.
#[derive(Serialize)]
struct Subscription<'a> {
    after: Cursor,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<&'a str>,
}

/// A frame the server sends: an `EventPage` or a `StreamControl`.
#[derive(Debug)]
enum Frame {
    Page(EventPage),
    Control { kind: ControlKind, reason: String },
}

/// `StreamControl.type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ControlKind {
    ResyncRequired,
    AuthenticationRequired,
    Unavailable,
    /// A later kind: handled as `unavailable`.
    #[serde(other)]
    Other,
}

fn decode_frame(text: &str) -> Result<Frame, serde_json::Error> {
    #[derive(Deserialize)]
    struct Control {
        #[serde(rename = "type")]
        kind: Option<ControlKind>,
        reason: Option<String>,
    }
    match serde_json::from_str::<Control>(text)? {
        Control { kind: Some(kind), reason } => {
            Ok(Frame::Control { kind, reason: reason.unwrap_or_default() })
        }
        Control { kind: None, .. } => serde_json::from_str(text).map(Frame::Page),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::model::conformance::spec;

    #[test]
    fn backoff_starts_at_the_pause_and_stays_under_the_ceiling() {
        let timing = Timing::DEFAULT;
        assert_eq!(timing.backoff(0), Duration::from_millis(500));
        for _ in 0..200 {
            let first = timing.backoff(1);
            assert!(
                (Duration::from_millis(500)..=Duration::from_secs(1)).contains(&first),
                "{first:?}"
            );
            let late = timing.backoff(40);
            assert!(
                (Duration::from_millis(500)..=Duration::from_secs(5)).contains(&late),
                "{late:?}"
            );
        }
    }

    #[test]
    fn subscription_names_the_cursor_and_the_token_only_when_private() {
        let public =
            serde_json::to_value(Subscription { after: Cursor::new(42), token: None }).unwrap();
        assert_eq!(public, json!({"after": "42"}));
        let private =
            serde_json::to_value(Subscription { after: Cursor::new(7), token: Some("t") }).unwrap();
        assert_eq!(private, json!({"after": "7", "token": "t"}));
    }

    #[test]
    fn frames_decode_as_controls_or_pages() {
        let frame = decode_frame(r#"{"type":"resync_required","reason":"gone"}"#).unwrap();
        assert!(
            matches!(frame, Frame::Control { kind: ControlKind::ResyncRequired, ref reason } if reason == "gone")
        );
        let frame = decode_frame(r#"{"type":"later_kind","reason":"r"}"#).unwrap();
        assert!(matches!(frame, Frame::Control { kind: ControlKind::Other, .. }));
        let frame = decode_frame(&crate::testing::json::fixture("event_page").to_string()).unwrap();
        let Frame::Page(page) = frame else { panic!("{frame:?}") };
        assert_eq!(page.next_cursor, Cursor::new(1007));
        assert!(decode_frame(r#"{"events":[]}"#).is_err());
        assert!(decode_frame("not json").is_err());
    }

    #[test]
    fn wire_types_match_the_spec() {
        let schemas = &spec()["components"]["schemas"];
        let properties = |name: &str| -> Vec<String> {
            let mut keys: Vec<String> =
                schemas[name]["properties"].as_object().unwrap().keys().cloned().collect();
            keys.sort();
            keys
        };
        assert_eq!(properties("StreamSubscription"), ["after", "token"]);
        assert_eq!(properties("StreamControl"), ["reason", "type"]);
        for kind in schemas["StreamControl"]["properties"]["type"]["enum"].as_array().unwrap() {
            let decoded: ControlKind = serde_json::from_value(kind.clone()).unwrap();
            assert_ne!(decoded, ControlKind::Other, "{kind}");
        }
        let operation = &spec()["paths"][crate::transport::ops::STREAM.path]["get"]["x-websocket"];
        assert_eq!(
            operation["clientMessage"]["$ref"],
            Value::from("#/components/schemas/StreamSubscription")
        );
    }

    #[test]
    fn authentication_control_maps_to_an_unauthenticated_api_error() {
        let error = unauthenticated("revoked");
        assert!(error.is_unauthenticated());
        assert_eq!(error.status(), Some(401));
        assert_eq!(error.code(), Some(&ErrorCode::Unauthenticated));
        assert_eq!(error.api().unwrap().message(), "revoked");
        assert!(!error.is_retryable());
    }
}
