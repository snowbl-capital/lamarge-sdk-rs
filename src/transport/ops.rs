//! The API operations the SDK calls, one `const Op` each.
//!
//! Conformance with `docs/api/api-reference/openapi.yaml`: every public `operationId` is mapped
//! (in `ALL`), deferred or never wanted, exactly once; every `Op` matches its spec method, path
//! and security, and is marked secret exactly when its answer can carry a secret.

/// An API operation as the transport sends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Op {
    /// The spec's `operationId`.
    pub(crate) id: &'static str,
    pub(crate) method: Method,
    /// Path template with `{name}` placeholders, filled in order by `Call::segment`.
    pub(crate) path: &'static str,
    pub(crate) auth: AuthTier,
    /// Whether a transient failure is retried: reads, and writes idempotent by a client id or by
    /// content. Requests without such a key never retry.
    pub(crate) retry: bool,
    /// Whether the answer may hold a secret (a token, a fill's claim secret): decode errors then
    /// keep no body text.
    pub(crate) secret: bool,
    /// Non-2xx statuses whose body is the answer (only `/health`'s 503).
    pub(crate) answers: &'static [u16],
}

impl Op {
    /// An operation retried only when it is a read, holding no secret.
    const fn new(id: &'static str, method: Method, path: &'static str, auth: AuthTier) -> Self {
        let retry = matches!(method, Method::Get);
        Self { id, method, path, auth, retry, secret: false, answers: &[] }
    }

    const fn get(id: &'static str, path: &'static str, auth: AuthTier) -> Self {
        Self::new(id, Method::Get, path, auth)
    }

    const fn post(id: &'static str, path: &'static str, auth: AuthTier) -> Self {
        Self::new(id, Method::Post, path, auth)
    }

    const fn put(id: &'static str, path: &'static str, auth: AuthTier) -> Self {
        Self::new(id, Method::Put, path, auth)
    }

    const fn delete(id: &'static str, path: &'static str, auth: AuthTier) -> Self {
        Self::new(id, Method::Delete, path, auth)
    }

    /// The same operation, retried: a write idempotent by a client id or by content.
    const fn retried(self) -> Self {
        Self { retry: true, ..self }
    }

    /// The same operation, its answer holding a secret.
    const fn secret(self) -> Self {
        Self { secret: true, ..self }
    }

    /// The same operation, with `statuses` decoded as answers.
    const fn answers(self, statuses: &'static [u16]) -> Self {
        Self { answers: statuses, ..self }
    }
}

/// HTTP methods the API uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Get,
    Post,
    Put,
    Delete,
}

impl Method {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
        }
    }

    pub(crate) fn as_reqwest(self) -> reqwest::Method {
        match self {
            Self::Get => reqwest::Method::GET,
            Self::Post => reqwest::Method::POST,
            Self::Put => reqwest::Method::PUT,
            Self::Delete => reqwest::Method::DELETE,
        }
    }
}

/// The spec's `security` of an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthTier {
    /// `[]`
    Public,
    /// `[{}, {session}]`: public, narrowed to the session's own data with one.
    OptionalSession,
    /// `[{session}]`
    Session,
    /// `[{session}, {makerCredential}]`
    SessionOrCredential,
}

/// Declares the operations as `pub(crate)` constants, and `ALL` listing them for conformance.
macro_rules! ops {
    ($( $(#[$meta:meta])* $name:ident = $op:expr; )+) => {
        $( $(#[$meta])* pub(crate) const $name: Op = $op; )+

        /// Every mapped operation.
        #[cfg(test)]
        pub(crate) const ALL: &[Op] = &[$($name),+];
    };
}

use AuthTier::{OptionalSession, Public, Session, SessionOrCredential as Either};

ops! {
    // Market data, explorer, events and account reads.
    GET_VENUES = Op::get("getVenues", "/v1/venues", Public);
    GET_BOOK = Op::get("getBook", "/v1/books", Public);
    GET_MARKETS = Op::get("getMarkets", "/v1/markets", Public);
    GET_VOLUME = Op::get("getVolume", "/v1/volume", Public);
    GET_HEALTH = Op::get("getHealth", "/health", Public).answers(&[503]);
    GET_INFO = Op::get("getV1Info", "/v1/info", Public);
    GET_TRADE_PROOF = Op::get("getTradeProof", "/v1/trades/{id}/proof", Public);
    GET_EXPLORER_TRANSFERS =
        Op::get("getExplorerTransfers", "/v1/explorer/transfers", OptionalSession);
    GET_EXPLORER_PENDING_TRANSFERS = Op::get(
        "getExplorerPendingTransfers",
        "/v1/explorer/transfers/pending",
        OptionalSession,
    );
    GET_EXPLORER_TRANSFER =
        Op::get("getExplorerTransfer", "/v1/explorer/transfers/{id}", Public);
    GET_TRADE_HISTORY = Op::get("getV1TradesHistory", "/v1/trades/history", OptionalSession);
    GET_PENDING_TRADES = Op::get("getV1TradesPending", "/v1/trades/pending", OptionalSession);
    GET_EVENTS = Op::get("getV1Events", "/v1/events", Public);
    GET_MY_EVENTS = Op::get("getV1MeEvents", "/v1/me/events", Either);
    GET_SNAPSHOT = Op::get("getV1Snapshot", "/v1/snapshot", Public);
    GET_MY_SNAPSHOT = Op::get("getV1MeSnapshot", "/v1/me/snapshot", Either);
    /// The event WebSocket: only its path is used, by the event feed's connector.
    STREAM = Op::get("getV1Stream", "/v1/stream", Public);
    GET_ME = Op::get("getMe", "/v1/me", Session);
    GET_BALANCES = Op::get("getBalances", "/v1/balances", Session);
    GET_STATS = Op::get("getStats", "/v1/me/stats", Session);
    GET_EXPOSURE = Op::get("getV1MeExposure", "/v1/me/exposure", Either);
    GET_VENUE_SETUP = Op::get("getVenueSetup", "/v1/accounts/{venue}/setup", Session);
    GET_WALLET_FUNDING = Op::get("getWalletFunding", "/v1/wallets/{venue}", Session);
    GET_ESCROW = Op::get("getEscrow", "/v1/escrow", Session);
    DELETE_LINK = Op::delete("deleteLink", "/v1/accounts/{venue}", Session);

    // Transfers, takes, fills, orders, withdrawals and credentials. A transfer is idempotent by
    // its `client_transfer_id`, a withdrawal submission by its signed operation.
    GET_ROUTE = Op::get("getRoute", "/v1/routes", Session);
    GET_TRANSFERS = Op::get("getTransfers", "/v1/transfers", Session);
    GET_TRANSFER = Op::get("getTransfer", "/v1/transfers/{id}", Session);
    CANCEL_TRANSFER = Op::delete("cancelTransfer", "/v1/transfers/{id}", Session);
    POST_TRANSFER = Op::post("postTransfer", "/v1/transfers", Session).retried();
    PREPARE_TRANSFER_WITHDRAWAL = Op::post(
        "prepareTransferWithdrawal",
        "/v1/transfers/{id}/withdrawal/prepare",
        Session,
    );
    SUBMIT_TRANSFER_WITHDRAWAL =
        Op::post("submitTransferWithdrawal", "/v1/transfers/{id}/withdrawal", Session)
            .retried();
    GET_TAKE = Op::get("getV1TakesId", "/v1/takes/{id}", Session).secret();
    DELETE_TAKE = Op::delete("deleteTake", "/v1/takes/{id}", Session);
    GET_TAKE_HISTORY = Op::get("getTakeHistory", "/v1/takes/history", Session).secret();
    GET_FILLS = Op::get("getFills", "/v1/fills", Either).secret();
    GET_FILL_HISTORY = Op::get("getV1FillsHistory", "/v1/fills/history", Either);
    GET_ORDERS = Op::get("getOrders", "/v1/orders", Either);
    GET_ORDER = Op::get("getOrder", "/v1/orders/{id}", Either);
    GET_ORDER_HISTORY = Op::get("getV1OrdersHistory", "/v1/orders/history", Either);
    DELETE_QUOTES = Op::delete("deleteQuotes", "/v1/orders/{id}/quotes", Either);
    GET_WITHDRAWAL_HISTORY =
        Op::get("getV1WithdrawalsHistory", "/v1/withdrawals/history", Session);
    CREATE_CREDENTIAL = Op::post("postV1Credentials", "/v1/credentials", Session).secret();
    DELETE_CREDENTIAL = Op::delete("deleteV1CredentialsId", "/v1/credentials/{id}", Session);
    ROTATE_CREDENTIAL =
        Op::post("postV1CredentialsIdRotate", "/v1/credentials/{id}/rotate", Session).secret();

    // Sign-in, linking, venue setup and wallet permits. Auth and setup steps have no idempotency
    // key; linking is idempotent by content, a permit submission by its `ref`.
    AUTH_CHALLENGE = Op::post("postAuthChallenge", "/v1/auth/challenge", Public);
    AUTH_VERIFY = Op::post("postAuthVerify", "/v1/auth/verify", Public).secret();
    REVOKE_SESSION = Op::delete("deleteV1AuthSessionsId", "/v1/auth/sessions/{id}", Session);
    LINK = Op::post("postLink", "/v1/accounts/link", Session).retried();
    SETUP_STEP = Op::post("postVenueSetupStep", "/v1/accounts/{venue}/setup/{step}", Session);
    PREPARE_WALLET_PERMIT = Op::post("postWalletPermit", "/v1/wallets/{venue}/permit", Session);
    SUBMIT_WALLET_PERMIT = PREPARE_WALLET_PERMIT.retried();

    // Maker capital, closes and quote lines. Maker preparations are idempotent by
    // `client_request_id`, submissions and publications by their preparation (an accepted one is
    // answered again), a lease renewal by its identity; line preparations and close requests
    // have no key.
    PREPARE_MAKER = Op::post("prepareMaker", "/v1/maker/{venue}/prepare", Session).retried();
    SUBMIT_MAKER_SIGNATURES =
        Op::post("submitMakerSignatures", "/v1/maker/preparations/{id}/signatures", Session)
            .retried();
    GET_MAKER_PREPARATION =
        Op::get("getMakerPreparation", "/v1/maker/preparations/{id}", Session);
    CANCEL_MAKER_PREPARATION =
        Op::delete("cancelMakerPreparation", "/v1/maker/preparations/{id}", Session);
    PREPARE_CLOSE = Op::post("postCloses", "/v1/closes", Session);
    SUBMIT_CLOSE = Op::post("postCloseSignatures", "/v1/closes/{id}/signatures", Session).retried();
    GET_QUOTE_LINES = Op::get("getQuoteLines", "/v1/quote-lines", Either);
    PREPARE_QUOTE_LINES = Op::post("prepareQuoteLines", "/v1/quote-lines/prepare", Either);
    PUBLISH_QUOTE_LINES = Op::post("publishQuoteLines", "/v1/quote-lines", Either).retried();
    RENEW_LEASE =
        Op::put("putV1QuoteIdentitiesIdLease", "/v1/quote-identities/{id}/lease", Either)
            .retried();
    CANCEL_IDENTITY_QUOTES = Op::delete(
        "deleteV1QuoteIdentitiesIdQuotes",
        "/v1/quote-identities/{id}/quotes",
        Either,
    );

    // Takes and withdrawals. A take is idempotent by its `client_take_id`, its signatures by the
    // take, an Aster reissue by the held nonce (compare-and-set), a withdrawal submission by its
    // signed operation; a withdrawal preparation has no key. Take answers carry fills, whose
    // claim secret is a secret.
    POST_TAKE = Op::post("postTake", "/v1/takes", Session).retried().secret();
    SUBMIT_TAKE_SIGNATURES =
        Op::post("postTakeSignatures", "/v1/takes/{id}/signatures", Session).retried().secret();
    REISSUE_ASTER_REQUEST =
        Op::post("reissueAsterRequest", "/v1/takes/{id}/aster-request", Session)
            .retried()
            .secret();
    PREPARE_WITHDRAWAL = Op::post("postWithdrawalPrepare", "/v1/withdrawals/prepare", Session);
    SUBMIT_WITHDRAWAL = Op::post("postWithdrawal", "/v1/withdrawals", Session).retried();

    // Moves and bridges. A move is idempotent by its `client_move_id`, its close by its terms, a
    // bridge by its `client_bridge_id`, its signatures and transaction reports by content; a fence
    // has no key.
    QUOTE_MOVE = Op::get("quoteMove", "/v1/moves/quote", Session);
    CREATE_MOVE = Op::post("createMove", "/v1/moves", Session).retried();
    GET_MOVE = Op::get("getMove", "/v1/moves/{id}", Session);
    CLOSE_MOVE = Op::post("closeMove", "/v1/moves/{id}/close", Session).retried();
    CREATE_BRIDGE = Op::post("createBridge", "/v1/moves/{id}/bridges", Session).retried();
    GET_BRIDGE = Op::get("getBridge", "/v1/bridges/{id}", Session);
    GET_BRIDGE_FUNDING = Op::get("getBridgeFunding", "/v1/bridges/{id}/funding", Session);
    SIGN_BRIDGE = Op::post("signBridge", "/v1/bridges/{id}/signatures", Session).retried();
    /// The empty body fences exclusively: a replay would answer the first fence's success as
    /// another client's.
    FENCE_BRIDGE =
        Op::post("reportBridgeTransaction", "/v1/bridges/{id}/transactions", Session);
    REPORT_BRIDGE_TRANSACTION = FENCE_BRIDGE.retried();
}

/// Operations out of 0.1's scope, with the reason.
#[cfg(test)]
pub(crate) const DEFERRED: &[(&str, &str)] = &[
    ("createDirectOperation", "direct transfers send transactions: web-led"),
    ("getDirectOperation", "direct transfers send transactions: web-led"),
    ("reportDirectOperation", "direct transfers send transactions: web-led"),
    ("getDirectFunding", "direct transfers send transactions: web-led"),
    ("prepareTransferConversion", "Polygon native-USDC conversion sends transactions"),
    ("reportTransferConversion", "Polygon native-USDC conversion sends transactions"),
    ("prepareGroupedRoute", "routes and grouped routes: 0.3"),
    ("abortGroupedRoute", "routes and grouped routes: 0.3"),
    ("commitGroupedRoute", "routes and grouped routes: 0.3"),
    ("preparePaymentRefund", "refunds are an active market making obligation: 0.2"),
    ("submitPaymentRefund", "refunds are an active market making obligation: 0.2"),
    ("postEscrowWithdrawPrepare", "escrow available-balance withdrawal: web-only today"),
    ("postEscrowWithdraw", "escrow available-balance withdrawal: web-only today"),
    ("acceptFill", "active market making: 0.2"),
    ("submitDelivery", "active market making: 0.2"),
    ("postLinkChallenge", "linking another owner: one key per session in 0.1"),
];

/// Operations the SDK never maps, with the reason.
#[cfg(test)]
pub(crate) const NEVER: &[(&str, &str)] = &[
    ("postAsterSigningRpc", "browser-wallet signing stub"),
    ("getSiteShareImage", "link preview"),
    ("getTradeShareMetadata", "link preview"),
];

#[cfg(test)]
mod conformance {
    use std::collections::{BTreeMap, BTreeSet};

    use serde_json::Value;

    use super::*;
    use crate::model::conformance::spec;

    /// A public operation of the spec.
    #[derive(Debug)]
    struct SpecOp {
        method: String,
        path: String,
        auth: Option<AuthTier>,
    }

    /// Public operations by id; `x-excluded` ones are dropped.
    fn spec_ops(spec: &Value) -> BTreeMap<String, SpecOp> {
        let default_security = spec.get("security").cloned().unwrap_or(Value::Array(Vec::new()));
        let mut ops = BTreeMap::new();
        for (path, item) in spec["paths"].as_object().expect("paths") {
            for (method, operation) in item.as_object().expect("path item") {
                if !matches!(method.as_str(), "get" | "post" | "put" | "delete" | "patch") {
                    continue;
                }
                if operation.get("x-excluded").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let id = operation["operationId"].as_str().expect("operationId").to_owned();
                let security = operation.get("security").unwrap_or(&default_security);
                let op = SpecOp {
                    method: method.to_uppercase(),
                    path: path.clone(),
                    auth: tier(security),
                };
                assert!(ops.insert(id.clone(), op).is_none(), "duplicate operationId {id}");
            }
        }
        ops
    }

    fn tier(security: &Value) -> Option<AuthTier> {
        let schemes: Vec<Vec<&str>> = security
            .as_array()?
            .iter()
            .map(|requirement| {
                requirement.as_object().map(|r| r.keys().map(String::as_str).collect())
            })
            .collect::<Option<_>>()?;
        match schemes.iter().map(Vec::as_slice).collect::<Vec<_>>().as_slice() {
            [] => Some(AuthTier::Public),
            [[], ["session"]] => Some(AuthTier::OptionalSession),
            [["session"]] => Some(AuthTier::Session),
            [["session"], ["makerCredential"]] => Some(AuthTier::SessionOrCredential),
            _ => None,
        }
    }

    fn check_op(ops: &BTreeMap<String, SpecOp>, op: &Op) -> Result<(), String> {
        let spec = ops.get(op.id).ok_or_else(|| format!("{}: not a public operation", op.id))?;
        if spec.method != op.method.as_str() || spec.path != op.path || spec.auth != Some(op.auth) {
            return Err(format!(
                "{}: spec {} {} {:?}, op {} {} {:?}",
                op.id,
                spec.method,
                spec.path,
                spec.auth,
                op.method.as_str(),
                op.path,
                op.auth
            ));
        }
        Ok(())
    }

    #[test]
    fn every_public_operation_is_classified_exactly_once() {
        let ops = spec_ops(spec());
        let mut seen: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        let mapped: BTreeSet<&str> = ALL.iter().map(|op| op.id).collect();
        for id in &mapped {
            seen.entry(id).or_default().push("ALL");
        }
        for (id, _) in DEFERRED {
            seen.entry(id).or_default().push("DEFERRED");
        }
        for (id, _) in NEVER {
            seen.entry(id).or_default().push("NEVER");
        }
        let duplicated: Vec<_> = seen.iter().filter(|(_, lists)| lists.len() > 1).collect();
        assert!(duplicated.is_empty(), "classified more than once: {duplicated:?}");
        let unknown: Vec<_> = seen.keys().filter(|id| !ops.contains_key(**id)).collect();
        assert!(unknown.is_empty(), "not public operations of the spec: {unknown:?}");
        let missing: Vec<_> = ops.keys().filter(|id| !seen.contains_key(id.as_str())).collect();
        assert!(missing.is_empty(), "unclassified operations: {missing:?}");
    }

    #[test]
    fn every_op_matches_the_spec() {
        let ops = spec_ops(spec());
        let errors: Vec<_> = ALL.iter().filter_map(|op| check_op(&ops, op).err()).collect();
        assert!(errors.is_empty(), "{errors:#?}");
    }

    #[test]
    fn every_public_operation_has_a_known_security() {
        let unknown: Vec<_> = spec_ops(spec())
            .into_iter()
            .filter(|(_, op)| op.auth.is_none())
            .map(|(id, _)| id)
            .collect();
        assert!(unknown.is_empty(), "security not mapped to a tier: {unknown:?}");
    }

    #[test]
    fn op_check_catches_each_mismatch() {
        let ops = spec_ops(spec());
        let venues = Op::get("getVenues", "/v1/venues", Public);
        assert_eq!(check_op(&ops, &venues), Ok(()));
        assert!(check_op(&ops, &Op { method: Method::Post, ..venues }).is_err());
        assert!(check_op(&ops, &Op { path: "/v1/venue", ..venues }).is_err());
        assert!(check_op(&ops, &Op { auth: Session, ..venues }).is_err());
        assert!(check_op(&ops, &Op { id: "getVenue", ..venues }).is_err());
        assert_eq!(check_op(&ops, &GET_EXPOSURE), Ok(()));
        assert_eq!(check_op(&ops, &GET_EXPLORER_TRANSFERS), Ok(()));
    }

    /// Answer schemas that hold a secret: a session or credential token, a fill's claim secret.
    const SECRET_SCHEMAS: &[&str] = &["Session", "CredentialCreated", "CredentialRotated", "Fill"];

    /// Whether `value` reaches one of [`SECRET_SCHEMAS`] through its references.
    fn holds_secret(value: &Value, seen: &mut BTreeSet<String>) -> bool {
        match value {
            Value::Object(map) => match map.get("$ref").and_then(Value::as_str) {
                Some(reference) => {
                    let name = reference.rsplit('/').next().unwrap_or_default();
                    SECRET_SCHEMAS.contains(&name)
                        || (seen.insert(reference.to_owned())
                            && holds_secret(&spec()["components"]["schemas"][name], seen))
                }
                None => map.values().any(|value| holds_secret(value, seen)),
            },
            Value::Array(items) => items.iter().any(|value| holds_secret(value, seen)),
            _ => false,
        }
    }

    #[test]
    fn ops_are_secret_exactly_when_an_answer_holds_a_secret() {
        let wrong: Vec<_> = ALL
            .iter()
            .filter(|op| {
                let operation = &spec()["paths"][op.path][op.method.as_str().to_lowercase()];
                let answers = operation["responses"].as_object().expect("responses");
                let secret = answers.iter().any(|(status, answer)| {
                    let status: u16 = status.parse().unwrap_or(0);
                    ((200..300).contains(&status) || op.answers.contains(&status))
                        && holds_secret(&answer["content"], &mut BTreeSet::new())
                });
                secret != op.secret
            })
            .map(|op| op.id)
            .collect();
        assert!(wrong.is_empty(), "secret flag differs from the spec: {wrong:?}");
    }

    #[test]
    fn excluded_operations_are_dropped() {
        // The public spec has no excluded operation left to test against.
        let spec = serde_json::json!({"paths": {"/v1/a": {
            "get": {"operationId": "kept"},
            "post": {"operationId": "dropped", "x-excluded": true},
        }}});
        let ops = spec_ops(&spec);
        assert!(ops.contains_key("kept"));
        assert!(!ops.contains_key("dropped"));
    }
}
