//! Route tests, ported from `sdk/src/grouped.test.ts`, `route.test.ts` and
//! `route-grouped.test.ts` (each test names its source), on a stateful mock of the routes a route
//! reads and writes. Every continuation answers with a take already paid, which the transfer flow
//! recovers without signing: the take flow's own tests cover signing it.
#![expect(
    clippy::too_many_lines,
    clippy::unnecessary_wraps,
    reason = "the mock's routes; ids as the outcomes hold them"
)]

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use wiremock::{
    Mock, Request, ResponseTemplate,
    matchers::{method, path, path_regex},
};

use super::*;
use crate::{
    store::{Journal, MemoryStore},
    testing::{FILL, MAKER, OWNER, edited, fixtures, harness::*, json::fixture, time},
};

/// The route's first transfer, its second, and its continuations.
const FIRST: &str = "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a10";
const SECOND: &str = "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a11";
const CONTINUATION: &str = "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a12";
const SUCCESSOR: &str = "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a13";
/// The second take, its fill, and the continuations' takes.
const SECOND_TAKE: &str = "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6b01";
const SECOND_FILL: &str = "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6b02";
const CONTINUATION_TAKE: &str = "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6b03";
const SUCCESSOR_TAKE: &str = "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6b04";
const ROUTE: &str = "route-1";

fn amount(units: u64) -> Amount {
    Amount::new(U256::from(units))
}

/// Lighter to Polymarket through Hyperliquid: 2.0013 USDC for 2 on Hyperliquid, then 2 for 1.9.
fn request(owner: Address) -> RouteRequest {
    RouteRequest::new(
        ClientId::new(ROUTE).unwrap(),
        (VenueId::Lighter, VenueId::Hyperliquid, VenueId::Polymarket),
        amount(2_001_300),
        amount(2_000_000),
        RouteLeg::new(amount(2_000_000), amount(1_900_000)),
        Recipient::Account(owner),
    )
    .poll_interval(Duration::ZERO)
    .arrival_timeout(Duration::ZERO)
}

/// `value` with the fixtures' [`OWNER`] replaced by `owner`.
fn owned_by(value: &Value, owner: Address) -> Value {
    let text = value
        .to_string()
        .replace(&OWNER.to_checksum(None), &owner.to_checksum(None))
        .replace(&format!("{OWNER:#x}"), &format!("{owner:#x}"));
    serde_json::from_str(&text).unwrap()
}

/// A transfer intent: the fixture's, cleared of what a route's transfers don't carry.
fn intent(patch: &Value) -> Value {
    let mut intent = fixture("transfer_page")["data"][0].clone();
    for field in [
        "move_id",
        "conversion",
        "funding_plan",
        "funding_plan_error",
        "withdrawal",
        "withdrawal_preparation",
        "then",
        "grouped",
        "continuable",
        "from_dex",
        "previous_transfer_id",
        "cancelled_at",
        "take_id",
        "max_fee_bps",
    ] {
        intent[field] = Value::Null;
    }
    intent["funding"] = json!("venue");
    intent["destination"] = json!("venue");
    intent["max_parts"] = json!(8);
    for (field, value) in patch.as_object().unwrap() {
        intent[field] = value.clone();
    }
    intent
}

/// A settled fill `id` of take `take`, claimed into `destination` on `to`.
fn settled(take: &str, id: &str, (from, to): (&str, &str), out: &str, destination: &str) -> Value {
    let mut fill = fixtures::fill(from, to, out, out);
    fill["id"] = json!(id);
    fill["take_id"] = json!(take);
    fill["state"] = json!("SETTLED");
    fill["maker_leg"] = json!("CLAIMED");
    fill["payment_leg"] = json!("PAID");
    fill["maker_destination"] = json!(destination);
    fill["maker_claimed_at"] = json!(time(-10));
    fill["signed_at"] = json!(time(-60));
    fill
}

/// What the mock serves, changed by what it is sent.
struct State {
    owner: Address,
    first: Value,
    second: Value,
    continuations: Vec<Value>,
    route: Value,
    prepare_refusal: Option<ResponseTemplate>,
    commit_refusals: Vec<ResponseTemplate>,
    first_take: Value,
    second_take: Value,
    /// Fills by take id.
    fills: Vec<(String, Vec<Value>)>,
    aborts: usize,
    commits: Vec<Value>,
    balances: Vec<Value>,
}

impl State {
    fn transfer(&self, id: &str) -> Option<Value> {
        [&self.first, &self.second]
            .into_iter()
            .chain(self.continuations.iter())
            .find(|intent| intent["id"] == id)
            .cloned()
    }

    /// Opens the continuation `body` asks for, its take already paid.
    fn open(&mut self, body: &Value) -> Value {
        let id = body["client_transfer_id"].as_str().unwrap().to_owned();
        let (transfer, take) = if self.continuations.is_empty() {
            (CONTINUATION, CONTINUATION_TAKE)
        } else {
            (SUCCESSOR, SUCCESSOR_TAKE)
        };
        let funding = body.get("funding").cloned().unwrap_or(json!("venue"));
        let continuation = intent(&json!({
            "id": transfer, "client_transfer_id": id,
            "client_take_id": format!("transfer:{transfer}"),
            "from": body["from"], "to": body["to"], "from_chain_id": 999, "to_chain_id": 137,
            "amount_in": body["amount_in"], "min_out": body["min_out"], "payer": self.owner,
            "recipient": body["recipient"], "previous_transfer_id": body["previous_transfer_id"],
            "funding": funding, "take_id": take,
        }));
        let out = body["min_out"].as_str().unwrap();
        let fill = settled(take, take, ("hyperliquid", "polymarket"), out, "venue");
        self.fills.push((take.to_owned(), vec![owned_by(&fill, self.owner)]));
        self.continuations.push(continuation.clone());
        continuation
    }
}

struct Mocked {
    harness: Harness,
    session: Session,
    state: Arc<Mutex<State>>,
}

impl Mocked {
    async fn new() -> Self {
        Self::with(|_| {}).await
    }

    async fn with(edit: impl FnOnce(&mut State)) -> Self {
        let harness = Harness::new().await;
        let owner = harness.address();
        harness.lighter_owner(42, owner).await;
        harness.lighter_owner(731_033, MAKER).await;
        let then = json!({
            "to": "polymarket", "recipient": owner, "amount_in": "2000000", "min_out": "1900000",
            "chain_id": 137,
        });
        let first = intent(&json!({
            "id": FIRST, "client_transfer_id": ROUTE, "client_take_id": format!("transfer:{FIRST}"),
            "from": "lighter", "to": "hyperliquid", "from_chain_id": 304, "to_chain_id": 999,
            "amount_in": "2001300", "min_out": "2000000", "payer": owner, "recipient": owner,
            "then": then, "take_id": FILL,
        }));
        let second = intent(&json!({
            "id": SECOND, "client_transfer_id": format!("grouped:{FIRST}"),
            "client_take_id": format!("transfer:{SECOND}"), "from": "hyperliquid",
            "to": "polymarket", "from_chain_id": 999, "to_chain_id": 137,
            "amount_in": "2000000", "min_out": "1900000", "payer": owner, "recipient": owner,
            "previous_transfer_id": FIRST, "take_id": SECOND_TAKE,
        }));
        let route = json!({
            "state": "prepared", "second_transfer_id": SECOND, "first_take_id": FILL,
            "second_take_id": SECOND_TAKE, "commit_by": time(40),
        });
        let first_take = harness.owned(&fixtures::lighter_take());
        let second_take = harness.owned(&edited(fixtures::escrow_take(), |p| {
            p["take"]["id"] = json!(SECOND_TAKE);
            p["take"]["fills"][0]["id"] = json!(SECOND_FILL);
            p["take"]["fills"][0]["take_id"] = json!(SECOND_TAKE);
            p["take"]["fills"][0]["amount_out"] = json!("1990000");
            p["take"]["amount_out"] = json!("1990000");
            p["to_sign"][0]["fill_id"] = json!(SECOND_FILL);
            fixtures::payment(p)["amountOut"] = json!("1990000");
        }));
        let fresh = time(5);
        let mut via = balance("hyperliquid", owner, None);
        via["read_at"] = json!(fresh);
        via["chain_read_at"] = json!(fresh);
        via["chain_balance"] = json!("0");
        via["escrow_balance"] = json!("3000000");
        via["withdrawable"] = json!("3000000");
        let first_fill = settled(FILL, FILL, ("lighter", "hyperliquid"), "2000000", "escrow");
        let second_fill =
            settled(SECOND_TAKE, SECOND_FILL, ("hyperliquid", "polymarket"), "1990000", "venue");
        let mut state = State {
            owner,
            first,
            second,
            continuations: Vec::new(),
            route,
            prepare_refusal: None,
            commit_refusals: Vec::new(),
            first_take,
            second_take,
            fills: vec![
                (FILL.to_owned(), vec![owned_by(&first_fill, owner)]),
                (SECOND_TAKE.to_owned(), vec![owned_by(&second_fill, owner)]),
            ],
            aborts: 0,
            commits: Vec::new(),
            balances: vec![
                balance("lighter", owner, Some("42")),
                via,
                balance("polymarket", owner, None),
            ],
        };
        edit(&mut state);
        let state = Arc::new(Mutex::new(state));
        serve(&harness, &state).await;
        let session = harness.session_with(Journal::new(MemoryStore::new())).await;
        Self { harness, session, state }
    }

    fn signed(&self) -> usize {
        self.harness.flow_signatures()
    }

    async fn posted(&self) -> Vec<Value> {
        self.harness.bodies("POST", "/v1/transfers").await
    }

    /// The continuations' bodies, once each: the transfer flow posts again what the route did.
    async fn continuation_bodies(&self) -> Vec<Value> {
        let mut bodies: Vec<Value> = Vec::new();
        for body in self.posted().await {
            if body["client_transfer_id"] != ROUTE && !bodies.contains(&body) {
                bodies.push(body);
            }
        }
        bodies
    }
}

async fn serve(harness: &Harness, state: &Arc<Mutex<State>>) {
    let s = Arc::clone(state);
    Mock::given(method("POST"))
        .and(path("/v1/transfers"))
        .respond_with(move |request: &Request| {
            let body: Value = request.body_json().unwrap();
            let mut state = s.lock().unwrap();
            if body["client_transfer_id"] == ROUTE {
                return ok(state.first.clone());
            }
            let id = &body["client_transfer_id"];
            if let Some(found) = state.continuations.iter().find(|c| c["client_transfer_id"] == *id)
            {
                return ok(found.clone());
            }
            ok(state.open(&body))
        })
        .mount(&harness.server)
        .await;
    let s = Arc::clone(state);
    Mock::given(method("GET"))
        .and(path("/v1/transfers"))
        .respond_with(move |_: &Request| {
            let state = s.lock().unwrap();
            let mut data: Vec<Value> = state.continuations.iter().rev().cloned().collect();
            data.push(state.second.clone());
            data.push(state.first.clone());
            ok(json!({"data": data, "next_cursor": null, "has_more": false}))
        })
        .mount(&harness.server)
        .await;
    let s = Arc::clone(state);
    Mock::given(method("GET"))
        .and(path_regex(r"^/v1/transfers/[0-9a-f-]+$"))
        .respond_with(move |request: &Request| {
            let id = request.url.path().rsplit('/').next().unwrap().to_owned();
            s.lock().unwrap().transfer(&id).map_or_else(|| api_error(404, "NOT_FOUND"), ok)
        })
        .mount(&harness.server)
        .await;
    let s = Arc::clone(state);
    Mock::given(method("POST"))
        .and(path(format!("/v1/transfers/{FIRST}/grouped")))
        .respond_with(move |_: &Request| {
            let state = s.lock().unwrap();
            match &state.prepare_refusal {
                Some(refusal) => refusal.clone(),
                None => ok(json!({
                    "route": state.route, "first": state.first_take, "second": state.second_take,
                })),
            }
        })
        .mount(&harness.server)
        .await;
    let s = Arc::clone(state);
    Mock::given(method("POST"))
        .and(path(format!("/v1/transfers/{FIRST}/grouped/signatures")))
        .respond_with(move |request: &Request| {
            let mut state = s.lock().unwrap();
            state.commits.push(request.body_json().unwrap());
            if !state.commit_refusals.is_empty() {
                return state.commit_refusals.remove(0);
            }
            state.route["state"] = json!("committed");
            // The operator runs it to its end at once.
            state.first["grouped"] = with(&state.route, "state", json!("closed"));
            state.first["continuable"] = json!("0");
            ok(state.route.clone())
        })
        .mount(&harness.server)
        .await;
    let s = Arc::clone(state);
    Mock::given(method("DELETE"))
        .and(path(format!("/v1/transfers/{FIRST}/grouped")))
        .respond_with(move |_: &Request| {
            let mut state = s.lock().unwrap();
            state.aborts += 1;
            state.route["state"] = json!("aborted");
            ResponseTemplate::new(204)
        })
        .mount(&harness.server)
        .await;
    let s = Arc::clone(state);
    Mock::given(method("GET"))
        .and(path("/v1/balances"))
        .respond_with(move |_: &Request| ok(json!(s.lock().unwrap().balances)))
        .mount(&harness.server)
        .await;
    let s = Arc::clone(state);
    Mock::given(method("GET"))
        .and(path_regex(r"^/v1/takes/[0-9a-f-]+$"))
        .respond_with(move |request: &Request| {
            let id = request.url.path().rsplit('/').next().unwrap().to_owned();
            let state = s.lock().unwrap();
            let fills = state.fills.iter().find(|(take, _)| *take == id).map(|(_, f)| f.clone());
            fills.map_or_else(
                || api_error(404, "NOT_FOUND"),
                |fills| {
                    let prepared = fixtures::preparation(fills, vec![]);
                    ok(edited(prepared, |p| p["take"]["id"] = json!(id)))
                },
            )
        })
        .mount(&harness.server)
        .await;
}

fn with(value: &Value, field: &str, replaced: Value) -> Value {
    edited(value.clone(), |v| v[field] = replaced)
}

/// The mock's route, already in `state`.
fn held(state: &mut State, route_state: &str) {
    state.first["grouped"] = with(&state.route, "state", json!(route_state));
}

fn not_committed(error: &Error) -> RouteNotCommitted {
    match error {
        Error::RouteNotCommitted { reason, .. } => *reason,
        other => panic!("expected RouteNotCommitted, got {other:?}"),
    }
}

fn transfer_id(id: &str) -> Option<TransferId> {
    Some(id.parse().unwrap())
}

/// `grouped.test.ts` "a route signed at once signs its two requests back to back, checked as any
/// take's, and commits them together".
#[tokio::test]
async fn a_grouped_route_signs_both_takes_back_to_back_and_commits_them_together() {
    let mocked = Mocked::new().await;
    let request = request(mocked.harness.address()).grouped(true);
    let outcome = mocked.session.route(&request).await.unwrap();
    let RouteOutcome::Settled { first_fills, second_fills, second_transfer, .. } = outcome else {
        panic!("settled")
    };
    assert_eq!(mocked.signed(), 2, "the Lighter transfer and the pay_take");
    let state = mocked.state.lock().unwrap();
    let [commit] = state.commits.clone().try_into().unwrap();
    let kinds: Vec<&str> = commit["signatures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["lighter_transfer", "pay_take"]);
    assert_eq!((first_fills.len(), second_fills.len()), (1, 1));
    assert_eq!((second_transfer, state.aborts), (transfer_id(SECOND), 0));
}

/// "an unavailable grouped route throws with nothing signed".
#[tokio::test]
async fn an_unavailable_grouped_route_ends_with_nothing_signed() {
    let refusal = ResponseTemplate::new(409).set_body_json(json!({"error": {
        "code": "STATE_CONFLICT", "message": "m", "reason": "grouped_unavailable",
        "retryable": false,
    }}));
    let mocked = Mocked::with(|state| state.prepare_refusal = Some(refusal)).await;
    let request = request(mocked.harness.address()).grouped(true);
    let error = mocked.session.route(&request).await.unwrap_err();
    assert_eq!(not_committed(&error), RouteNotCommitted::Unavailable);
    assert_eq!(mocked.signed(), 0);
    assert!(mocked.state.lock().unwrap().commits.is_empty());
}

/// "a second take paying more than the first delivers ... is refused and the route aborted".
#[tokio::test]
async fn a_second_take_paying_more_than_the_first_delivers_aborts_the_route() {
    let mocked = Mocked::with(|state| state.second["amount_in"] = json!("2000001")).await;
    let request = request(mocked.harness.address()).grouped(true);
    let error = mocked.session.route(&request).await.unwrap_err();
    assert!(matches!(error, Error::Verification(_)), "{error:?}");
    let (aborts, commits) = {
        let state = mocked.state.lock().unwrap();
        (state.aborts, state.commits.len())
    };
    assert_eq!((aborts, commits, mocked.signed()), (1, 0, 0));
}

/// "too little time left aborts the route and throws, nothing signed".
#[tokio::test]
async fn too_little_time_left_aborts_with_nothing_signed() {
    let mocked = Mocked::with(|state| state.route["commit_by"] = json!(time(3))).await;
    let request = request(mocked.harness.address()).grouped(true);
    let error = mocked.session.route(&request).await.unwrap_err();
    assert_eq!(not_committed(&error), RouteNotCommitted::Expired);
    assert_eq!(mocked.signed(), 0);
    assert_eq!(mocked.state.lock().unwrap().aborts, 1);
}

/// "a refused commit aborts the route, forgets signatures and exposes its deadline".
#[tokio::test]
async fn a_refused_commit_aborts_the_route_and_forgets_its_signatures() {
    let refusal = api_error(409, "TAKE_EXPIRED");
    let mocked = Mocked::with(|state| state.commit_refusals = vec![refusal]).await;
    let request = request(mocked.harness.address()).grouped(true);
    let error = mocked.session.route(&request).await.unwrap_err();
    let Error::RouteNotCommitted { reason, commit_by, .. } = &error else { panic!("{error:?}") };
    assert_eq!((*reason, commit_by.is_some()), (RouteNotCommitted::Expired, true));
    assert_eq!(mocked.state.lock().unwrap().aborts, 1);
    assert!(!take::saved(&mocked.session, FILL.parse().unwrap()).await.unwrap(), "forgotten");
}

/// "an uncertain commit keeps its signatures and resumes without another wallet request".
#[tokio::test]
async fn an_uncertain_commit_keeps_its_signatures_and_resumes_without_asking_again() {
    let lost = api_error(500, "INTERNAL_ERROR");
    let mocked = Mocked::with(|state| state.commit_refusals = vec![lost]).await;
    let request = request(mocked.harness.address()).grouped(true);
    mocked.session.route(&request).await.unwrap_err();
    assert_eq!(mocked.state.lock().unwrap().aborts, 0, "an uncertain commit is never aborted");
    assert_eq!(mocked.signed(), 2);
    let outcome = mocked.session.route(&request).await.unwrap();
    assert!(matches!(outcome, RouteOutcome::Settled { .. }));
    assert_eq!(mocked.signed(), 2, "the kept signatures are committed");
    let state = mocked.state.lock().unwrap();
    assert_eq!(state.commits.len(), 2);
    assert_eq!(state.commits[0], state.commits[1]);
}

/// `route-grouped.test.ts` "a committed route still open when the wait ends is pending".
#[tokio::test]
async fn a_committed_route_still_open_is_pending_with_both_transfers() {
    let mocked = Mocked::with(|state| held(state, "committed")).await;
    let request = request(mocked.harness.address()).grouped(true);
    let outcome = mocked.session.route(&request).await.unwrap();
    let RouteOutcome::Pending { second_transfer, reason, .. } = outcome else { panic!("pending") };
    assert_eq!((second_transfer, reason), (transfer_id(SECOND), PendingReason::ArrivalUnconfirmed));
    assert_eq!(mocked.signed(), 0, "a held route is followed, never signed again");
}

/// "resuming an aborted grouped route stops before any reservation or signature".
#[tokio::test]
async fn an_aborted_grouped_route_stops_before_any_signature() {
    let mocked = Mocked::with(|state| held(state, "aborted")).await;
    let request = request(mocked.harness.address()).grouped(true);
    let error = mocked.session.route(&request).await.unwrap_err();
    assert_eq!(not_committed(&error), RouteNotCommitted::Aborted);
    assert_eq!(mocked.signed(), 0);
    let prepare = format!("/v1/transfers/{FIRST}/grouped");
    assert!(mocked.harness.requests("POST", &prepare).await.is_empty());
}

/// "what a closed route leaves continues through a successor of its second transfer, never the
/// second itself": `renew:<second>`, for its share of `then.min_out`.
#[tokio::test]
async fn what_a_closed_route_leaves_continues_from_the_escrow_for_its_share() {
    let mocked = Mocked::with(|state| {
        held(state, "closed");
        state.first["continuable"] = json!("1000000");
    })
    .await;
    let request = request(mocked.harness.address()).grouped(true);
    let outcome = mocked.session.route(&request).await.unwrap();
    let RouteOutcome::Settled { second_fills, second_transfer, .. } = outcome else {
        panic!("settled")
    };
    assert_eq!(second_transfer, transfer_id(CONTINUATION));
    assert_eq!(second_fills.len(), 2, "the route's own second take, then the continuation's");
    let [continuation] = mocked.continuation_bodies().await.try_into().unwrap();
    assert_eq!(continuation["client_transfer_id"], format!("renew:{SECOND}"));
    assert_eq!(continuation["amount_in"], "1000000");
    assert_eq!(continuation["min_out"], "950000", "1.9 × 1 / 2");
    assert_eq!(continuation["previous_transfer_id"], FIRST);

    // Run again: followed, never opened again.
    mocked.session.route(&request).await.unwrap();
    assert_eq!(mocked.state.lock().unwrap().continuations.len(), 1);
}

/// `route.test.ts` "with keepFloor, a continuation of less than planned still asks the whole
/// floor; without, its share".
#[tokio::test]
async fn with_keep_floor_a_continuation_asks_the_whole_floor() {
    let mocked = Mocked::with(|state| {
        held(state, "closed");
        state.first["continuable"] = json!("1000000");
    })
    .await;
    let request = request(mocked.harness.address()).grouped(true).keep_floor(true);
    mocked.session.route(&request).await.unwrap();
    let [continuation] = mocked.continuation_bodies().await.try_into().unwrap();
    assert_eq!(continuation["min_out"], "1900000");
}

/// "what is left under a hundredth of the second take waits on the venue between".
#[tokio::test]
async fn what_is_left_under_a_hundredth_waits_on_via() {
    let mocked = Mocked::with(|state| {
        held(state, "closed");
        state.first["continuable"] = json!("19999");
    })
    .await;
    let request = request(mocked.harness.address()).grouped(true);
    let outcome = mocked.session.route(&request).await.unwrap();
    let RouteOutcome::Settled { left, second_transfer, .. } = outcome else { panic!("settled") };
    assert_eq!((left, second_transfer), (Some(amount(19_999)), transfer_id(SECOND)));
    assert!(mocked.state.lock().unwrap().continuations.is_empty());
}

/// "what LaMarge leaves to continue with never goes past what the first take delivered".
#[tokio::test]
async fn what_is_left_never_goes_past_the_delivery() {
    let mocked = Mocked::with(|state| {
        held(state, "closed");
        state.first["continuable"] = json!("2000001");
    })
    .await;
    let request = request(mocked.harness.address()).grouped(true);
    let error = mocked.session.route(&request).await.unwrap_err();
    assert!(matches!(error, Error::Verification(_)), "{error:?}");
    assert!(mocked.state.lock().unwrap().continuations.is_empty());
}

/// `route.test.ts` "a continuation cancelled before it was paid is followed, from the history,
/// by one successor with what is left now".
#[tokio::test]
async fn a_cancelled_continuation_is_followed_by_one_successor() {
    let mocked = Mocked::with(|state| {
        held(state, "closed");
        state.first["continuable"] = json!("1000000");
        let body = json!({
            "client_transfer_id": format!("{ROUTE}.2"), "from": "hyperliquid", "to": "polymarket",
            "amount_in": "1000000", "min_out": "950000", "recipient": state.owner,
            "previous_transfer_id": FIRST,
        });
        state.open(&body);
        state.continuations[0]["cancelled_at"] = json!(time(-5));
    })
    .await;
    let request = request(mocked.harness.address()).grouped(true);
    let outcome = mocked.session.route(&request).await.unwrap();
    let RouteOutcome::Settled { second_transfer, .. } = outcome else { panic!("settled") };
    assert_eq!(second_transfer, transfer_id(SUCCESSOR));
    let [successor] = mocked.continuation_bodies().await.try_into().unwrap();
    assert_eq!(successor["client_transfer_id"], format!("renew:{CONTINUATION}"));
}

/// `route-grouped.test.ts` "without grouped, a route is signed leg by leg": every post of the
/// first transfer names the route's end; delivered into the venue, the continuation pays from it.
#[tokio::test]
async fn without_grouped_a_route_runs_leg_by_leg() {
    let mocked = Mocked::with(|state| {
        let fill = settled(FILL, FILL, ("lighter", "hyperliquid"), "2000000", "venue");
        state.fills[0].1 = vec![owned_by(&fill, state.owner)];
        state.first["continuable"] = json!("2000000");
    })
    .await;
    let outcome = mocked.session.route(&request(mocked.harness.address())).await.unwrap();
    let RouteOutcome::Settled { second_transfer, .. } = outcome else { panic!("settled") };
    assert_eq!(second_transfer, transfer_id(CONTINUATION));
    let prepare = format!("/v1/transfers/{FIRST}/grouped");
    assert!(mocked.harness.requests("POST", &prepare).await.is_empty());
    let posted = mocked.posted().await;
    let firsts: Vec<&Value> =
        posted.iter().filter(|body| body["client_transfer_id"] == ROUTE).collect();
    assert_eq!(firsts.len(), 2, "the route's, then the transfer flow's");
    assert!(firsts.iter().all(|body| body["then"]["to"] == "polymarket"));
    let [continuation] = mocked.continuation_bodies().await.try_into().unwrap();
    assert_eq!(continuation["min_out"], "1900000");
    assert!(continuation.get("funding").is_none(), "delivered into the venue: paid from it");
}

/// `route-grouped.test.ts` "an unavailable grouped ... route never ... starts a sequential
/// transfer; the caller can explicitly retry it leg by leg".
#[tokio::test]
async fn a_route_asked_grouped_that_lamarge_cannot_group_is_never_run_leg_by_leg() {
    let mocked = Mocked::with(|state| {
        state.first["from"] = json!("hyperliquid");
        state.first["to"] = json!("lighter");
        state.first["from_chain_id"] = json!(999);
        state.first["to_chain_id"] = json!(304);
        state.first["amount_in"] = json!("2000000");
    })
    .await;
    let owner = mocked.harness.address();
    let request = RouteRequest::new(
        ClientId::new(ROUTE).unwrap(),
        (VenueId::Hyperliquid, VenueId::Lighter, VenueId::Polymarket),
        amount(2_000_000),
        amount(2_000_000),
        RouteLeg::new(amount(2_000_000), amount(1_900_000)),
        Recipient::Account(owner),
    )
    .grouped(true);
    let error = mocked.session.route(&request).await.unwrap_err();
    assert_eq!(not_committed(&error), RouteNotCommitted::Unavailable);
    assert_eq!(mocked.signed(), 0);
    assert_eq!(mocked.posted().await.len(), 1, "no sequential transfer");
    assert!(!groupable(&VenueId::Hyperliquid, &VenueId::Lighter));
    assert!(groupable(&VenueId::Aster, &VenueId::Polymarket));
}

#[tokio::test]
async fn a_route_through_its_own_end_is_refused_before_any_request() {
    let mocked = Mocked::new().await;
    let owner = mocked.harness.address();
    let request = RouteRequest::new(
        ClientId::new(ROUTE).unwrap(),
        (VenueId::Lighter, VenueId::Polymarket, VenueId::Polymarket),
        amount(1),
        amount(1),
        RouteLeg::new(amount(1), amount(1)),
        Recipient::Account(owner),
    );
    let error = mocked.session.route(&request).await.unwrap_err();
    assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    assert!(mocked.harness.received_only_auth().await);
}
