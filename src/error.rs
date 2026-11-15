//! Errors: one [`Error`] for every operation, with the server's refusal in [`ApiError`].

use std::{borrow::Cow, fmt, time::Duration};

use alloy_primitives::Address;

use crate::{
    model::MakerPreparationState,
    types::{ClientId, OrderId, PreparationId, SetupStepKind, VenueId, open_enum},
};

/// `std::result::Result` with [`Error`] as the default error.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Any failure of an SDK operation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The server refused the request.
    #[error("api error {0}")]
    Api(Box<ApiError>),
    /// The request did not complete: connect, TLS, reset or timeout.
    #[error("transport error")]
    Transport(#[source] TransportError),
    /// The answer could not be read as the expected shape.
    #[error("unreadable response")]
    Decode(#[source] Box<DecodeError>),
    /// A server payload failed the checks run before signing; nothing was signed. Never retried.
    #[error("refused to sign: {0}")]
    Verification(VerificationError),
    /// The signer failed, or produced a signature from another key.
    #[error("signer error")]
    Signer(#[source] Box<SignerError>),
    /// The journal's store failed; see [`StoreError::submitted`] before retrying.
    #[error("store error")]
    Store(#[source] Box<StoreError>),
    /// A venue's own public API, read to check a request, was unreachable or unreadable.
    #[error("venue api error")]
    VenueApi(#[source] VenueApiError),
    /// No quote can serve the request; nothing was reserved or signed.
    #[error("quote unavailable: {0}")]
    QuoteUnavailable(&'static str),
    /// The caller's input is invalid; nothing was sent.
    #[error("invalid request: {0}")]
    InvalidRequest(Cow<'static, str>),
    /// The session was signed out; nothing was sent. Sign in again.
    #[error("signed out")]
    SignedOut,
    /// Aster's answer to a payment request re-issue was not definitive; run the take again.
    #[error("aster request outcome unknown")]
    AsterRequestUncertain,
    /// The venue refused the withdrawal for good; a new attempt needs a new id.
    #[error("withdrawal {id} was refused ({code})")]
    WithdrawalRefused {
        /// The withdrawal's client id.
        id: ClientId,
        /// The refusal's code.
        code: ErrorCode,
    },
    /// An order the flow waited on failed.
    #[error("order {order} failed: {reason}")]
    OrderFailed {
        /// The order.
        order: OrderId,
        /// The server's reason.
        reason: String,
    },
    /// A maker preparation the flow waited on was cancelled or failed.
    #[error("funding {preparation} {state}: {reason}")]
    FundingFailed {
        /// The preparation.
        preparation: PreparationId,
        /// Its final state.
        state: MakerPreparationState,
        /// The server's reason.
        reason: String,
    },
    /// A venue setup step did not complete in time.
    #[error("{venue} setup incomplete at step {step}")]
    SetupIncomplete {
        /// The venue.
        venue: VenueId,
        /// The first step not done.
        step: SetupStepKind,
    },
    /// A wait ran out; recover by running the same request (same client id) again.
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
    /// The client configuration is invalid.
    #[error("invalid configuration: {0}")]
    Config(Cow<'static, str>),
}

impl From<VerificationError> for Error {
    fn from(error: VerificationError) -> Self {
        Self::Verification(error)
    }
}

impl Error {
    /// The server's refusal, if this is one.
    pub fn api(&self) -> Option<&ApiError> {
        match self {
            Self::Api(error) => Some(error),
            _ => None,
        }
    }

    /// The HTTP status of the answer, if one arrived.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Api(error) => Some(error.status),
            Self::Decode(error) => error.status,
            _ => None,
        }
    }

    /// The server's error code, for a refusal.
    pub fn code(&self) -> Option<&ErrorCode> {
        self.api().map(ApiError::code)
    }

    /// The server's request id, when an answer named one.
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Self::Api(error) => error.request_id(),
            Self::Decode(error) => error.request_id(),
            _ => None,
        }
    }

    /// How long the server asked to wait before retrying.
    pub fn retry_after(&self) -> Option<Duration> {
        self.api().and_then(ApiError::retry_after)
    }

    /// Whether the same request may succeed if sent again: a transport failure, or a refusal the
    /// server marks retryable.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::Api(error) => error.retryable,
            _ => false,
        }
    }

    /// Whether the session or credential was refused (`401 UNAUTHENTICATED`).
    pub fn is_unauthenticated(&self) -> bool {
        self.api()
            .is_some_and(|error| error.status == 401 || error.code == ErrorCode::Unauthenticated)
    }
}

/// A refusal from the LaMarge API.
///
/// `Display`: `<status> <CODE>: <message> (request <id>)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    status: u16,
    code: ErrorCode,
    message: String,
    request_id: Option<String>,
    retryable: bool,
    retry_after: Option<Duration>,
    reason: Option<Reason>,
    venue: Option<VenueId>,
    step: Option<SetupStepKind>,
}

impl ApiError {
    /// Reads a refusal from its answer.
    ///
    /// Without the API's error envelope (`{"error":{"code","message",…}}`, as from a proxy), the
    /// code is `Unknown("")`, the message `HTTP <status>`, and only a 429 or 5xx is retryable.
    pub(crate) fn from_answer(
        status: u16,
        body: &[u8],
        request_id: Option<String>,
        retry_after: Option<Duration>,
    ) -> Self {
        #[derive(serde::Deserialize)]
        struct Envelope {
            error: Body,
        }
        #[derive(serde::Deserialize)]
        struct Body {
            code: ErrorCode,
            message: String,
            request_id: Option<String>,
            retryable: Option<bool>,
            reason: Option<Reason>,
            venue: Option<VenueId>,
            step: Option<SetupStepKind>,
        }
        let default_retryable = status == 429 || status >= 500;
        match serde_json::from_slice::<Envelope>(body) {
            Ok(Envelope { error }) => Self {
                status,
                code: error.code,
                message: error.message,
                request_id: error.request_id.or(request_id),
                retryable: error.retryable.unwrap_or(default_retryable),
                retry_after,
                reason: error.reason,
                venue: error.venue,
                step: error.step,
            },
            Err(_) => Self {
                status,
                code: ErrorCode::Unknown(crate::types::UnknownValue::new(String::new())),
                message: format!("HTTP {status}"),
                request_id,
                retryable: default_retryable,
                retry_after,
                reason: None,
                venue: None,
                step: None,
            },
        }
    }

    /// The HTTP status.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The stable error code.
    pub fn code(&self) -> &ErrorCode {
        &self.code
    }

    /// The human-readable message; do not parse it.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The server's request id, for support.
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    /// Whether the server says the same request may be retried.
    pub fn is_retryable(&self) -> bool {
        self.retryable
    }

    /// The `Retry-After` delay, when the answer had one.
    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    /// The stable sub-case of the code, where the route lists one.
    pub fn reason(&self) -> Option<&Reason> {
        self.reason.as_ref()
    }

    /// With `SETUP_REQUIRED`: the venue whose account needs setup.
    pub fn venue(&self) -> Option<&VenueId> {
        self.venue.as_ref()
    }

    /// With `SETUP_REQUIRED`: the setup step to do first.
    pub fn step(&self) -> Option<&SetupStepKind> {
        self.step.as_ref()
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: {}", self.status, self.code, self.message)?;
        if let Some(id) = &self.request_id {
            write!(f, " (request {id})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ApiError {}

open_enum! {
    /// The stable code of an [`ApiError`].
    pub enum ErrorCode {
        ValidationError = "VALIDATION_ERROR",
        BadSignature = "BAD_SIGNATURE",
        WithdrawalExpired = "WITHDRAWAL_EXPIRED",
        CaptchaFailed = "CAPTCHA_FAILED",
        Unauthenticated = "UNAUTHENTICATED",
        Forbidden = "FORBIDDEN",
        AccessRequired = "ACCESS_REQUIRED",
        MakerNotApproved = "MAKER_NOT_APPROVED",
        NotFound = "NOT_FOUND",
        MethodNotAllowed = "METHOD_NOT_ALLOWED",
        StateConflict = "STATE_CONFLICT",
        AccountInUse = "ACCOUNT_IN_USE",
        PrefundingRequired = "PREFUNDING_REQUIRED",
        StaleSequence = "STALE_SEQUENCE",
        StaleQuote = "STALE_QUOTE",
        InsufficientLiquidity = "INSUFFICIENT_LIQUIDITY",
        Slippage = "SLIPPAGE",
        FillInProgress = "FILL_IN_PROGRESS",
        TakeExpired = "TAKE_EXPIRED",
        ResyncRequired = "RESYNC_REQUIRED",
        PayloadTooLarge = "PAYLOAD_TOO_LARGE",
        VenueDisabled = "VENUE_DISABLED",
        AccountNotLinked = "ACCOUNT_NOT_LINKED",
        WalletNotFound = "WALLET_NOT_FOUND",
        SetupRequired = "SETUP_REQUIRED",
        DeliveryPending = "DELIVERY_PENDING",
        RateLimited = "RATE_LIMITED",
        VenueRejected = "VENUE_REJECTED",
        VenueUnavailable = "VENUE_UNAVAILABLE",
        Internal = "INTERNAL",
        OperatorUnavailable = "OPERATOR_UNAVAILABLE",
        SnapshotTooLarge = "SNAPSHOT_TOO_LARGE",
    }
}

open_enum! {
    /// The stable sub-case of an [`ApiError`]'s code.
    pub enum Reason {
        FeeAboveMax = "fee_above_max",
        TooMany = "too_many",
        FundsChanged = "funds_changed",
        GroupedUnavailable = "grouped_unavailable",
        GroupedRoute = "grouped_route",
        FundsArriving = "funds_arriving",
        WithdrawalSubmitted = "withdrawal_submitted",
        WithdrawalUnsigned = "withdrawal_unsigned",
        VenuePayment = "venue_payment",
        AsterRequest = "aster_request",
        NoDeposit = "no_deposit",
        NotDue = "not_due",
        RefundAuthorized = "refund_authorized",
        ReportWindowClosed = "report_window_closed",
        TransferUsed = "transfer_used",
        NotATransfer = "not_a_transfer",
        UnknownTransaction = "unknown_transaction",
        SendUnresolved = "send_unresolved",
        AmountOver = "amount_over",
        BeforeFill = "before_fill",
        AmountShort = "amount_short",
        ExecutedLate = "executed_late",
    }
}

/// A request that did not complete. Opaque: ask [`is_timeout`](Self::is_timeout) or
/// [`is_connect`](Self::is_connect); the cause is its [`source`](std::error::Error::source).
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct TransportError(TransportKind);

#[derive(Debug, thiserror::Error)]
enum TransportKind {
    #[error("http request failed")]
    Http(#[source] reqwest::Error),
    #[error("websocket failed")]
    WebSocket(#[source] Box<tokio_tungstenite::tungstenite::Error>),
    #[error("timed out")]
    Timeout,
    #[error("the caller's request failed")]
    Caller(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl TransportError {
    pub(crate) fn http(error: reqwest::Error) -> Self {
        // reqwest's error carries the URL; a URL never holds a secret here, but keep it short.
        Self(TransportKind::Http(error.without_url()))
    }

    pub(crate) fn websocket(error: tokio_tungstenite::tungstenite::Error) -> Self {
        Self(TransportKind::WebSocket(Box::new(error)))
    }

    pub(crate) const fn timeout() -> Self {
        Self(TransportKind::Timeout)
    }

    /// A failure of a request the caller sent for the SDK ([`crate::LighterReads`]).
    pub(crate) fn caller(error: Box<dyn std::error::Error + Send + Sync>) -> Self {
        Self(TransportKind::Caller(error))
    }

    /// Whether a connect or response deadline ran out.
    pub fn is_timeout(&self) -> bool {
        match &self.0 {
            TransportKind::Http(error) => error.is_timeout(),
            TransportKind::WebSocket(_) | TransportKind::Caller(_) => false,
            TransportKind::Timeout => true,
        }
    }

    /// Whether the connection could not be established.
    pub fn is_connect(&self) -> bool {
        match &self.0 {
            TransportKind::Http(error) => error.is_connect(),
            TransportKind::WebSocket(error) => {
                matches!(**error, tokio_tungstenite::tungstenite::Error::Io(_))
            }
            TransportKind::Timeout | TransportKind::Caller(_) => false,
        }
    }
}

/// An answer that could not be read as the expected shape.
///
/// For operations whose answers carry secrets (sign-in verification, credential creation and
/// rotation) it keeps only the status, the request id and the position of the failure.
#[derive(Debug)]
pub struct DecodeError {
    status: Option<u16>,
    request_id: Option<String>,
    detail: DecodeDetail,
    excerpt: Option<String>,
}

#[derive(Debug)]
enum DecodeDetail {
    Json { message: Option<String>, line: usize, column: usize },
    TooLarge { limit: usize },
    Invalid(&'static str),
}

/// The longest body excerpt a [`DecodeError`] keeps.
const EXCERPT_BYTES: usize = 1024;

impl DecodeError {
    /// A body that is not the expected JSON. `secret`: the body may hold a secret, so neither
    /// the excerpt nor serde's message (which quotes values) is kept.
    pub(crate) fn json(
        status: Option<u16>,
        request_id: Option<String>,
        body: &[u8],
        error: &serde_json::Error,
        secret: bool,
    ) -> Self {
        let excerpt = (!secret).then(|| {
            let end = body.len().min(EXCERPT_BYTES);
            String::from_utf8_lossy(&body[..end]).into_owned()
        });
        Self {
            status,
            request_id,
            detail: DecodeDetail::Json {
                message: (!secret).then(|| error.to_string()),
                line: error.line(),
                column: error.column(),
            },
            excerpt,
        }
    }

    /// A well-formed answer that contradicts an earlier one.
    pub(crate) fn invalid(what: &'static str) -> Self {
        Self { status: None, request_id: None, detail: DecodeDetail::Invalid(what), excerpt: None }
    }

    /// A body longer than `limit` bytes.
    pub(crate) fn too_large(status: Option<u16>, request_id: Option<String>, limit: usize) -> Self {
        Self { status, request_id, detail: DecodeDetail::TooLarge { limit }, excerpt: None }
    }

    /// The HTTP status of the answer (none for a WebSocket frame).
    pub fn status(&self) -> Option<u16> {
        self.status
    }

    /// The server's request id.
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    /// The start of the body (at most 1 KiB), except for answers that may hold secrets.
    pub fn excerpt(&self) -> Option<&str> {
        self.excerpt.as_deref()
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(status) = self.status {
            write!(f, "HTTP {status}: ")?;
        }
        match &self.detail {
            DecodeDetail::Json { message: Some(message), .. } => write!(f, "{message}")?,
            DecodeDetail::Json { message: None, line, column } => {
                write!(f, "unexpected JSON at line {line} column {column}")?;
            }
            DecodeDetail::TooLarge { limit } => write!(f, "body over {limit} bytes")?,
            DecodeDetail::Invalid(what) => f.write_str(what)?,
        }
        if let Some(id) = &self.request_id {
            write!(f, " (request {id})")?;
        }
        Ok(())
    }
}

impl std::error::Error for DecodeError {}

/// Why a server payload was refused before signing.
///
/// `Display`: `<kind>: <detail>[<index>]`, e.g. `differs: payments.receiveEscrow[1]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct VerificationError {
    /// The class of the failed check.
    pub kind: VerificationKind,
    /// The field or rule that failed, stable across releases.
    pub detail: &'static str,
    /// The position of the failing item in a list, if any.
    pub index: Option<usize>,
}

impl fmt::Display for VerificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.detail)?;
        if let Some(index) = self.index {
            write!(f, "[{index}]")?;
        }
        Ok(())
    }
}

impl std::error::Error for VerificationError {}

/// The class of a failed pre-signing check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum VerificationKind {
    /// The server's deployment is not one this SDK pins for the environment.
    Deployment,
    /// A term differs from the request or the pinned deployment.
    Differs,
    /// A window, nonce or deadline is out of range.
    Stale,
    /// State moved since the caller decided (a linked account, maker capital); review again.
    Changed,
    /// A venue or path this SDK version does not support.
    Unsupported,
}

impl fmt::Display for VerificationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Deployment => "deployment",
            Self::Differs => "differs",
            Self::Stale => "stale",
            Self::Changed => "changed",
            Self::Unsupported => "unsupported",
        })
    }
}

/// A signer failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SignerError {
    /// The signer returned an error.
    #[error("signer failed")]
    Failed(#[source] alloy_signer::Error),
    /// The signature recovers to another address than the expected signer.
    #[error("signature from {recovered}, expected {expected}")]
    Mismatch {
        /// The signer's address.
        expected: Address,
        /// The address the signature recovers to.
        recovered: Address,
    },
}

/// A store failure, with whether the operation it served already reached the server.
#[derive(Debug, thiserror::Error)]
#[error("store key {key:?}{}", if *.submitted { " (after submission)" } else { "" })]
pub struct StoreError {
    key: String,
    submitted: bool,
    #[source]
    source: std::io::Error,
}

impl StoreError {
    pub(crate) fn new(key: impl Into<String>, submitted: bool, source: std::io::Error) -> Self {
        Self { key: key.into(), submitted, source }
    }

    /// The store key.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// `false`: nothing of this operation was sent by this call. `true`: the operation reached
    /// the server and only bookkeeping failed; rerun with the same client id, never a new one.
    pub fn submitted(&self) -> bool {
        self.submitted
    }
}

/// A text that is not a maker credential token (`mk_` and at least one more character).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a credential token starts with mk_")]
pub struct InvalidCredentialToken;

/// A venue's public API (read to check a request) was unreachable or unreadable.
#[derive(Debug)]
pub struct VenueApiError {
    venue: VenueId,
    kind: VenueApiKind,
}

#[derive(Debug)]
enum VenueApiKind {
    Unreachable(TransportError),
    Status(u16),
    Unreadable,
}

impl VenueApiError {
    pub(crate) fn unreachable(venue: VenueId, error: TransportError) -> Self {
        Self { venue, kind: VenueApiKind::Unreachable(error) }
    }

    pub(crate) fn answered(venue: VenueId, status: u16) -> Self {
        Self { venue, kind: VenueApiKind::Status(status) }
    }

    pub(crate) fn unreadable(venue: VenueId) -> Self {
        Self { venue, kind: VenueApiKind::Unreadable }
    }

    /// The venue.
    pub fn venue(&self) -> &VenueId {
        &self.venue
    }

    /// The HTTP status, when the venue answered with an error.
    pub fn status(&self) -> Option<u16> {
        match self.kind {
            VenueApiKind::Status(status) => Some(status),
            _ => None,
        }
    }

    /// Whether the venue did not answer in time.
    pub fn is_timeout(&self) -> bool {
        matches!(&self.kind, VenueApiKind::Unreachable(error) if error.is_timeout())
    }
}

impl fmt::Display for VenueApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            VenueApiKind::Unreachable(_) => write!(f, "{} api unreachable", self.venue),
            VenueApiKind::Status(status) => write!(f, "{} api answered HTTP {status}", self.venue),
            VenueApiKind::Unreadable => write!(f, "{} api answer unreadable", self.venue),
        }
    }
}

impl std::error::Error for VenueApiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            VenueApiKind::Unreachable(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_stays_small() {
        assert!(size_of::<Error>() <= 128, "Error is {} bytes", size_of::<Error>());
    }

    #[test]
    fn error_is_send_sync_static() {
        fn check<T: Send + Sync + 'static>() {}
        check::<Error>();
        check::<ApiError>();
    }

    #[test]
    fn reads_the_api_envelope() {
        let body = br#"{"error":{"code":"SETUP_REQUIRED","message":"set up first","request_id":"r-1","retryable":false,"reason":"no_deposit","venue":"aster","step":"agent","extra":1}}"#;
        let error = ApiError::from_answer(409, body, Some("header-id".into()), None);
        assert_eq!(error.status(), 409);
        assert_eq!(error.code(), &ErrorCode::SetupRequired);
        assert_eq!(error.message(), "set up first");
        assert_eq!(error.request_id(), Some("r-1"), "the envelope's id wins");
        assert!(!error.is_retryable());
        assert_eq!(error.reason(), Some(&Reason::NoDeposit));
        assert_eq!(error.venue(), Some(&VenueId::Aster));
        assert_eq!(error.step(), Some(&SetupStepKind::Agent));
        assert_eq!(error.to_string(), "409 SETUP_REQUIRED: set up first (request r-1)");
    }

    #[test]
    fn envelope_retryable_overrides_status_default() {
        let internal =
            br#"{"error":{"code":"INTERNAL","message":"x","request_id":"r","retryable":false}}"#;
        assert!(!ApiError::from_answer(500, internal, None, None).is_retryable());
        let early = br#"{"error":{"code":"VENUE_UNAVAILABLE","message":"x","request_id":"r","retryable":true}}"#;
        assert!(ApiError::from_answer(425, early, None, None).is_retryable());
    }

    #[test]
    fn answer_without_envelope_uses_status() {
        let error = ApiError::from_answer(502, b"<html>bad gateway</html>", Some("h".into()), None);
        assert!(error.code().is_unknown());
        assert_eq!(error.code().as_str(), "");
        assert_eq!(error.message(), "HTTP 502");
        assert_eq!(error.request_id(), Some("h"));
        assert!(error.is_retryable());
        assert!(ApiError::from_answer(429, b"", None, None).is_retryable());
        assert!(!ApiError::from_answer(404, b"{}", None, None).is_retryable());
        assert!(!ApiError::from_answer(302, b"", None, None).is_retryable());
    }

    #[test]
    fn unknown_codes_are_kept() {
        let body = br#"{"error":{"code":"NEW_CODE","message":"m","request_id":"r","retryable":false,"reason":"new_reason"}}"#;
        let error = ApiError::from_answer(400, body, None, None);
        assert_eq!(error.code().as_str(), "NEW_CODE");
        assert!(error.reason().unwrap().is_unknown());
    }

    #[test]
    fn error_helpers() {
        let body = br#"{"error":{"code":"UNAUTHENTICATED","message":"m","request_id":"r","retryable":false}}"#;
        let error = Error::Api(Box::new(ApiError::from_answer(
            401,
            body,
            None,
            Some(Duration::from_secs(2)),
        )));
        assert!(error.is_unauthenticated());
        assert!(!error.is_retryable());
        assert_eq!(error.status(), Some(401));
        assert_eq!(error.request_id(), Some("r"));
        assert_eq!(error.retry_after(), Some(Duration::from_secs(2)));
        assert_eq!(error.code(), Some(&ErrorCode::Unauthenticated));
        let invalid = Error::InvalidRequest("x".into());
        assert!(!invalid.is_unauthenticated() && invalid.status().is_none());
    }

    #[test]
    fn verification_display() {
        let error = VerificationError {
            kind: VerificationKind::Differs,
            detail: "payments.receiveEscrow",
            index: Some(1),
        };
        assert_eq!(error.to_string(), "differs: payments.receiveEscrow[1]");
        let error = Error::from(VerificationError { index: None, ..error });
        assert_eq!(error.to_string(), "refused to sign: differs: payments.receiveEscrow");
        assert!(std::error::Error::source(&error).is_none(), "shown once, not as a source too");
    }

    #[test]
    fn secret_decode_errors_keep_no_body_text() {
        let body = br#"{"token":"mk_SENTINEL"}"#;
        let serde_error = serde_json::from_slice::<u64>(body).unwrap_err();
        let error = DecodeError::json(Some(200), Some("r".into()), body, &serde_error, true);
        assert_eq!(error.excerpt(), None);
        assert!(!format!("{error} {error:?}").contains("SENTINEL"));
        let error = DecodeError::json(Some(200), None, body, &serde_error, false);
        assert_eq!(error.excerpt(), Some(r#"{"token":"mk_SENTINEL"}"#));
    }

    #[test]
    fn excerpt_is_bounded() {
        let body = vec![b'x'; 5000];
        let serde_error = serde_json::from_slice::<u64>(&body).unwrap_err();
        let error = DecodeError::json(None, None, &body, &serde_error, false);
        assert_eq!(error.excerpt().unwrap().len(), EXCERPT_BYTES);
    }

    #[test]
    fn transport_error_classification() {
        assert!(TransportError::timeout().is_timeout());
        assert!(!TransportError::timeout().is_connect());
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        let websocket =
            TransportError::websocket(tokio_tungstenite::tungstenite::Error::Io(refused));
        assert!(websocket.is_connect() && !websocket.is_timeout());
        assert!(std::error::Error::source(&websocket).is_some());
        assert!(Error::Transport(websocket).is_retryable());
    }

    #[test]
    fn store_error_says_whether_submitted() {
        let error = StoreError::new("k", true, std::io::Error::other("disk"));
        assert!(error.submitted());
        assert_eq!(error.key(), "k");
        assert_eq!(error.to_string(), r#"store key "k" (after submission)"#);
        assert_eq!(
            StoreError::new("k", false, std::io::Error::other("d")).to_string(),
            r#"store key "k""#
        );
    }
}
