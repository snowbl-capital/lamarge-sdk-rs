//! Transfer flow tests, ported from `sdk/src/transfer.test.ts` (each test names its source) on a
//! stateful mock of the API ported from that file's `api()`.
#![expect(
    clippy::too_many_lines,
    clippy::struct_excessive_bools,
    reason = "the mock's routes and switches"
)]

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
};

use alloy_primitives::address;
use alloy_signer::Signer;
use chrono::DateTime;
use wiremock::{Mock, Request, ResponseTemplate, matchers::any};

use super::*;
use crate::{
    error::VerificationKind,
    protocol::fees,
    store::{Journal, MemoryStore, scoped_key},
    testing::{
        FILL as TAKE, HASHLOCK, MAKER, NOW, ORDER, SWAP, harness::*, json::fixture, sample, time,
    },
    types::{EscrowEnvironment, VenueId},
};

/// The transfer, and the one replacing it.
const T1: &str = "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a10";
const T2: &str = "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a11";
const WITHDRAWAL_ID: &str = "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a20";
/// A wallet delivery's swap id: fee-tagged for a wallet, no fee.
const WALLET_SWAP: &str = "0x1b4c4374729e00000000000000000000cccccccccccccccccccccccccccccccc";
/// An exchange's deposit address, receiving by wallet transfer.
const BYBIT: Address = address!("5555555555555555555555555555555555555555");
/// The caller's id: never what the server's takes are named after.
const CLIENT_ID: &str = "move-1";

/// The take's client id the server derives for transfer `id`.
fn take_client_id(id: &str) -> String {
    format!("transfer:{id}")
}

/// What the mock serves, as `api()`'s options in `transfer.test.ts`.
#[derive(Default)]
struct Options {
    from: Option<&'static str>,
    /// The intent's funding when it already exists (a resumed transfer).
    wallet_funding: bool,
    chain_balance: Option<&'static str>,
    withdrawal_amount: Option<&'static str>,
    /// The payment's recipient, when not the transfer's.
    recipient: Option<Address>,
    linked_recipient: Option<Address>,
    /// The plan asks for a permit first.
    permit: bool,
    setup: Option<&'static str>,
    permit_reads: Vec<&'static str>,
    levels: Option<Value>,
    stale_preparation: bool,
    fresh_preparation: bool,
    /// The transfer has its take already.
    take: bool,
    answered: Option<Value>,
    funds: Vec<Value>,
    withdrawal: Option<&'static str>,
    withdrawn: Option<&'static str>,
    take_fill: Option<Value>,
    /// The take's fill states no `signed_at`.
    unproven: bool,
    live: Vec<&'static str>,
    spent: Option<&'static str>,
    withdrawn_during_permit: bool,
    at_once: bool,
    committed: bool,
    pay_window: Option<i64>,
    unlinked_source: bool,
    wallet: Option<Address>,
    release_minimum: Option<&'static str>,
    /// The send asset's amount the server prepares, when not the one asked.
    prepared_amount: Option<&'static str>,
    /// What goes wrong on a route, in turn: `cancel`, `withdrawal`, `replacement`, `take`,
    /// `take-cancel`, `submission`.
    faults: Vec<(&'static str, Fault)>,
}

/// A route's failure.
#[derive(Clone, Copy)]
enum Fault {
    /// The request takes effect, its answer is lost.
    Lost,
    /// The request is lost before it takes effect.
    Unrecorded,
    /// The answer reads as no known shape.
    Unreadable,
    /// The API refuses with `STATE_CONFLICT` and this message, changing nothing.
    Conflict(&'static str),
    /// Another run's withdrawal is recorded meanwhile; the API refuses with `STATE_CONFLICT`,
    /// this message and reason.
    Raced(&'static str, Option<&'static str>),
}

type Hook = Box<dyn FnMut(&mut State) + Send>;

/// The server: the transfer, its take and the payer's funds, changed by what it is sent.
struct State {
    o: Options,
    owner: Address,
    clock: Arc<AtomicI64>,
    /// Tells the signer what each payload served is.
    signer: TestSigner,
    faults: HashMap<&'static str, VecDeque<Fault>>,
    intent: Value,
    /// The transfer replaced, as its reads answer it.
    original: Option<Value>,
    /// A withdrawal prepared, never submitted.
    unsent: bool,
    committed: bool,
    paid: bool,
    reads: usize,
    permit_reads: usize,
    live_reads: usize,
    plan_reads: usize,
    held: u64,
    last_balance: Option<Value>,
    /// Runs on each read of the transfer with its plan from the `wait_from`-th: what changes
    /// while the flow waits.
    on_wait: Option<Hook>,
    wait_from: usize,
}

fn merge(target: &mut Value, patch: &Value) {
    if let (Some(target), Some(patch)) = (target.as_object_mut(), patch.as_object()) {
        target.extend(patch.clone());
    }
}

/// `units` of a 6-decimal token as a decimal string.
fn decimal(units: u64) -> String {
    let fraction = format!("{:06}", units % 1_000_000);
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        (units / 1_000_000).to_string()
    } else {
        format!("{}.{fraction}", units / 1_000_000)
    }
}

fn response(status: u16, body: Value) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(body)
}

fn refusal(status: u16, code: &str, message: &str, reason: Option<&str>) -> ResponseTemplate {
    response(
        status,
        json!({"error": {"code": code, "message": message, "retryable": false, "reason": reason}}),
    )
}

/// A reply lost on the way: a gateway's error without the API's envelope.
fn lost() -> ResponseTemplate {
    ResponseTemplate::new(502)
}

impl State {
    fn new(mut o: Options, signer: TestSigner, clock: Arc<AtomicI64>) -> Self {
        let committed = o.committed;
        let unsent = o.fresh_preparation && o.withdrawal.is_none();
        let mut faults: HashMap<&'static str, VecDeque<Fault>> = HashMap::new();
        for (route, fault) in o.faults.drain(..) {
            faults.entry(route).or_default().push_back(fault);
        }
        let mut state = Self {
            o,
            owner: signer.address(),
            clock,
            signer,
            faults,
            intent: Value::Null,
            original: None,
            unsent,
            committed,
            paid: false,
            reads: 0,
            permit_reads: 0,
            live_reads: 0,
            plan_reads: 0,
            held: 0,
            last_balance: None,
            on_wait: None,
            wait_from: 1,
        };
        state.intent = state.initial_intent();
        state
    }

    fn from(&self) -> &'static str {
        self.o.from.unwrap_or("hyperliquid")
    }

    fn venue(&self) -> VenueId {
        self.from().parse().unwrap()
    }

    fn chain_id(&self) -> u64 {
        deployments::chain(&self.venue()).unwrap().id
    }

    fn setup(&self) -> &'static str {
        self.o.setup.unwrap_or(if self.o.permit { "required" } else { "done" })
    }

    fn serve(&self, typed: &Value, kind: &'static str) {
        self.signer.serve(typed, kind);
    }

    /// The next failure of `route`, if any.
    fn fault(&mut self, route: &str) -> Option<Fault> {
        self.faults.get_mut(route).and_then(VecDeque::pop_front)
    }

    fn send_asset(&self, amount: u64) -> Value {
        let mut typed = sample("send_asset_withdrawal");
        let amount = self.o.prepared_amount.map_or_else(|| decimal(amount), str::to_owned);
        typed["message"]["amount"] = json!(amount);
        typed["message"]["nonce"] = json!(self.clock.load(Ordering::SeqCst) * 1000);
        self.serve(&typed, "SendAsset");
        typed
    }

    fn permit_typed(&self) -> Value {
        let mut typed = sample("permit");
        typed["message"]["owner"] = json!(self.owner);
        typed["message"]["deadline"] = json!((NOW + 300).to_string());
        self.serve(&typed, "Permit");
        typed
    }

    fn withdrawal(&self, state: &str) -> Value {
        let mut row = fixture("transfer_page/data/0/withdrawal");
        merge(
            &mut row,
            &json!({
                "id": WITHDRAWAL_ID, "venue": self.from(), "from_dex": null, "kind": "withdrawal",
                "amount": self.o.withdrawal_amount.unwrap_or("600000"), "destination": self.owner,
                "state": state, "venue_ref": null,
                "error": (state == "refused").then_some("Margin unavailable"),
                "created_at": time(0), "relayed_at": (state == "relayed").then(|| time(0)),
            }),
        );
        row
    }

    fn plan(&self) -> Value {
        let setup = self.setup();
        let chain = self.chain_id();
        let request = |kind: &str| json!({"kind": kind, "signer": self.owner, "chain_id": chain});
        let mut requests = Vec::new();
        if setup == "required" {
            requests.push(request("permit"));
        }
        if self.o.withdrawal_amount.is_some() {
            requests.push(request("margin_move"));
        }
        requests.push(request("pay_take"));
        json!({
            "observed_at": time(0), "venue": self.from(), "owner": self.owner, "amount": "1000000",
            "deposit": if setup == "required" { "1000000" } else { "0" },
            "withdrawal_amount": self.o.withdrawal_amount.unwrap_or("0"),
            "withdrawal_to": self.o.withdrawal_amount.map(|_| "signer"),
            "payment_setup": setup, "signed_at_once": self.o.at_once, "requests": requests,
        })
    }

    fn initial_intent(&self) -> Value {
        let preparation = if self.o.stale_preparation {
            json!({"amount": "600000", "created_at": time(-60), "typed_data": self.send_asset(600_000)})
        } else if self.o.fresh_preparation {
            json!({"amount": "600000", "created_at": time(0), "typed_data": self.send_asset(600_000)})
        } else {
            Value::Null
        };
        json!({
            "funding": if self.o.wallet_funding { "wallet" } else { "venue" },
            "destination": if self.o.wallet.is_some() { "wallet" } else { "venue" },
            "id": T1, "client_transfer_id": CLIENT_ID, "client_take_id": take_client_id(T1),
            "from": self.from(), "to": "polymarket", "amount_in": "1000000",
            "min_out": "1000000", "max_parts": 8, "payer": self.owner,
            "recipient": self.o.wallet.unwrap_or(self.owner), "from_chain_id": self.chain_id(),
            "to_chain_id": 137, "withdrawal_preparation": preparation,
            "withdrawal": self.o.withdrawal.map(|state| self.withdrawal(state)),
            "funding_plan": self.plan(), "funding_plan_error": null,
            "take_id": self.o.take.then_some(TAKE), "take_final": null,
            "previous_transfer_id": null, "continuable": null, "cancelled_at": null,
            "created_at": time(0), "grouped": null,
        })
    }

    fn fill(&self) -> Value {
        let mut fill = fixture("take_preparation/take/fills/0");
        let wallet = self.o.wallet;
        merge(
            &mut fill,
            &json!({
                "funding": self.intent["funding"], "id": TAKE, "take_id": TAKE, "role": "taker",
                "order_id": ORDER, "sequence": 1, "quote_index": 0,
                "swap_id": if wallet.is_some() { WALLET_SWAP } else { SWAP },
                "hashlock": HASHLOCK, "from_venue": self.from(), "to_venue": "polymarket",
                "amount_in": "1000000", "amount_out": "1005000", "taker_fee": "0",
                "taker_fee_bps": 0, "maker_owner": MAKER, "maker_recipient": MAKER,
                "payer": self.owner, "taker_recipient": wallet.unwrap_or(self.owner),
                "destination": if wallet.is_some() { "wallet" } else { "venue" },
                "state": "PREPARED", "pay_by": time(self.o.pay_window.unwrap_or(45)),
                "t1": time(3600), "t2": time(7200), "signed_at": null, "payment_leg": "NONE",
                "maker_leg": "NONE", "transactions": [], "delivery_hash": null,
                "payment_locked_at": null, "maker_claimed_at": null, "delivery_to": null,
                "received": null,
            }),
        );
        fill
    }

    /// The take's payment of `fill`, in the escrow on the paying chain.
    fn pay_take(&self, fill: &Value) -> Value {
        let chain = deployments::chain(&VenueId::Polymarket).unwrap();
        let receive = self.o.wallet.map_or(chain.token, |_| chain.wallet_token.unwrap());
        let seconds = |field: &str| {
            DateTime::parse_from_rfc3339(fill[field].as_str().unwrap()).unwrap().timestamp()
        };
        let lock = json!({
            "payer": self.owner, "swapId": fill["swap_id"], "recipient": MAKER,
            "amount": "1000000", "hashlock": HASHLOCK, "refundAt": seconds("t2"),
            "deadline": seconds("pay_by").to_string(), "orderId": ORDER, "sequence": 1,
            "amountOut": "1005000", "receiveChainId": chain.id.to_string(),
            "receiveToken": receive,
            "receiveEscrow": deployments::escrow(chain.id, EscrowEnvironment::Production).unwrap(),
            "receiveRecipient": self.o.recipient.or(self.o.wallet).unwrap_or(self.owner),
            "dex": 0,
        });
        let mut typed = sample("pay_take");
        typed["domain"]["chainId"] = json!(self.chain_id());
        typed["domain"]["verifyingContract"] = json!(
            deployments::escrow(self.chain_id(), EscrowEnvironment::Production)
                .unwrap_or(Address::ZERO)
        );
        typed["message"] = json!({"payer": self.owner, "deposit": "0", "payments": [lock]});
        self.serve(&typed, "PayTake");
        typed
    }

    fn prepared(&self) -> Value {
        let mut fill = self.fill();
        if self.committed {
            fill["signed_at"] = json!(time(0));
        }
        if self.paid {
            merge(
                &mut fill,
                &json!({"state": "PAID", "payment_leg": "LOCKED", "signed_at": time(0)}),
            );
        }
        if let Some(patch) = &self.o.take_fill {
            merge(&mut fill, patch);
        }
        let typed = self.pay_take(&fill);
        if self.o.unproven {
            fill.as_object_mut().unwrap().remove("signed_at");
        }
        let least = self.o.wallet.map(|_| {
            let chain = deployments::chain(&VenueId::Polymarket).unwrap();
            let amount =
                fees::minimum_received(&VenueId::Polymarket, U256::from(1_005_000u64)).unwrap();
            json!({"token": chain.wallet_token.unwrap(), "amount": amount.to_string()})
        });
        let to_sign = if self.committed || self.paid {
            json!([])
        } else {
            json!([{"kind": "pay_take", "fill_id": TAKE, "signer": self.owner, "typed_data": typed}])
        };
        json!({
            "take": {
                "funding": self.intent["funding"], "minimum_received": least, "id": TAKE,
                "client_take_id": self.intent["client_take_id"], "amount_in": "1000000",
                "amount_out": "1005000", "taker_fee": "0", "fills": [fill],
            },
            "to_sign": to_sign,
        })
    }

    fn permit_step(&self, status: &str) -> Value {
        let to_sign = (status == "required").then(|| {
            json!({
                "kind": "permit", "signer": self.owner, "ref": "permit-1",
                "typed_data": self.permit_typed(), "message": null, "expires_at": null,
            })
        });
        json!({
            "step": "permit", "status": status, "reason": null, "expires_at": null,
            "needed_for": ["pay"], "hash": null, "to_sign": to_sign,
        })
    }

    fn payer_balance(&mut self) -> Value {
        let account = (self.from() == "lighter").then_some("42");
        let mut row = balance(self.from(), self.owner, account);
        merge(
            &mut row,
            &json!({
                "chain_balance": self.o.chain_balance.unwrap_or("1000000"), "chain_error": null,
                "chain_read_at": time(0), "escrow_balance": "0", "deposit_balance": "0",
            }),
        );
        if !self.o.funds.is_empty() {
            let patch = self.o.funds[self.reads.min(self.o.funds.len() - 1)].clone();
            merge(&mut row, &patch);
        }
        self.reads += 1;
        let units = |field: &str| row[field].as_str().unwrap().parse::<u64>().unwrap();
        self.held = units("chain_balance") + units("escrow_balance");
        self.last_balance = Some(row.clone());
        row
    }

    /// The transfer as the server answers it: its plan only when asked, after a relayed
    /// withdrawal only once the funds arrived.
    fn answer(&mut self, plan: bool) -> Value {
        let mut out = self.intent.clone();
        if !plan || !out["take_id"].is_null() || !out["cancelled_at"].is_null() {
            out["funding_plan"] = Value::Null;
            return out;
        }
        if out["withdrawal"].is_null() {
            if let Some(spent) = self.o.spent {
                out["funding_plan"]["withdrawal_amount"] = json!(spent);
            }
            return out;
        }
        let live = if self.o.live.is_empty() {
            self.held
        } else {
            let read = self.o.live[self.live_reads.min(self.o.live.len() - 1)];
            self.live_reads += 1;
            read.parse().unwrap()
        };
        if out["withdrawal"]["state"] != "relayed" || live < 1_000_000 {
            out["funding_plan"] = Value::Null;
        } else {
            let plan = &mut out["funding_plan"];
            plan["withdrawal_amount"] = json!("0");
            plan["withdrawal_to"] = Value::Null;
            let requests = plan["requests"].as_array().unwrap().clone();
            plan["requests"] =
                requests.into_iter().filter(|request| request["kind"] != "margin_move").collect();
        }
        out
    }

    fn handle(&mut self, request: &Request) -> ResponseTemplate {
        let path = request.url.path().to_owned();
        let body: Value = request.body_json().unwrap_or(Value::Null);
        let plan = request.url.query_pairs().any(|(k, v)| k == "funding_plan" && v == "true");
        let from = self.from();
        let route = format!("{} {path}", request.method.as_str());
        let wallets = format!("GET /v1/wallets/{from}");
        let setup_permit = format!("POST /v1/accounts/{from}/setup/permit");
        let setup = format!("GET /v1/accounts/{from}/setup");
        match route.as_str() {
            "GET /v1/venues" => {
                let mut rows = venues(EscrowEnvironment::Production);
                for row in rows.as_array_mut().unwrap() {
                    row["release_minimum"] = json!(self.o.release_minimum);
                }
                response(200, rows)
            }
            "GET /v1/orders" => response(200, json!([])),
            "GET /v1/books" => {
                let level = json!({
                    "order_id": ORDER, "quote_index": 0, "supply": "1005000", "receive": "1000000",
                    "available": "100000000", "min_fill": "1", "expires_at": time(3600),
                    "quoted_at": time(0), "sequence": 1, "valid_until": time(60),
                });
                let levels = self.o.levels.clone().unwrap_or_else(|| json!([level]));
                response(
                    200,
                    json!({"from": from, "to": "polymarket", "max_fill": "100000000",
                           "taker_fee_bps": 0, "levels": levels}),
                )
            }
            "POST /v1/transfers" => {
                self.intent["funding"] = body.get("funding").cloned().unwrap_or(json!("venue"));
                let id = body["client_transfer_id"].as_str().unwrap().to_owned();
                if self.original.is_none() && id.contains(':') {
                    // A renewal cancelled the original first; a retry leaves it with its take.
                    let mut original = self.intent.clone();
                    if id.starts_with("renew:") {
                        original["cancelled_at"] = json!(time(0));
                    }
                    self.original = Some(original);
                }
                if id.starts_with("renew:") {
                    merge(
                        &mut self.intent,
                        &json!({"id": T2, "client_transfer_id": id, "cancelled_at": null,
                                "client_take_id": take_client_id(T2),
                                "withdrawal_preparation": null}),
                    );
                    self.unsent = false;
                }
                if id.starts_with("retry:") {
                    if let Some(Fault::Unrecorded) = self.fault("replacement") {
                        return lost();
                    }
                    merge(
                        &mut self.intent,
                        &json!({"id": T2, "client_transfer_id": id,
                                "client_take_id": take_client_id(T2), "take_id": null,
                                "withdrawal": null, "withdrawal_preparation": null}),
                    );
                    self.unsent = false;
                    self.committed = false;
                    self.paid = false;
                }
                let mut answer = self.intent.clone();
                if let Some(patch) = &self.o.answered {
                    merge(&mut answer, patch);
                }
                response(201, answer)
            }
            _ if route == format!("GET /v1/transfers/{T1}")
                || route == format!("GET /v1/transfers/{T2}") =>
            {
                if plan {
                    self.plan_reads += 1;
                    if self.plan_reads >= self.wait_from
                        && let Some(mut hook) = self.on_wait.take()
                    {
                        hook(self);
                        self.on_wait = Some(hook);
                    }
                }
                if path.ends_with(T1)
                    && let Some(original) = &self.original
                {
                    let mut original = original.clone();
                    original["funding_plan"] = Value::Null;
                    return response(200, original);
                }
                let answer = self.answer(plan);
                response(200, answer)
            }
            _ if route == format!("DELETE /v1/transfers/{T1}") => {
                let fault = self.fault("cancel");
                if let Some(Fault::Conflict(message)) = fault {
                    return refusal(409, "STATE_CONFLICT", message, None);
                }
                self.intent["cancelled_at"] = json!(time(0));
                if let Some(Fault::Lost) = fault {
                    return lost();
                }
                ResponseTemplate::new(204)
            }
            _ if route == format!("POST /v1/transfers/{T1}/withdrawal/prepare") => {
                self.unsent = true;
                let amount: u64 = body["amount"].as_str().unwrap().parse().unwrap();
                let typed = self.send_asset(amount);
                if self.o.at_once {
                    self.intent["withdrawal_preparation"] = json!({
                        "amount": body["amount"], "created_at": time(0), "typed_data": typed,
                    });
                }
                response(200, json!({"typed_data": typed}))
            }
            _ if route == format!("POST /v1/transfers/{T1}/withdrawal") => {
                self.unsent = false;
                let fault = self.fault("withdrawal");
                if let Some(Fault::Unrecorded) = fault {
                    return lost();
                }
                self.intent["withdrawal"] = self.withdrawal(self.o.withdrawn.unwrap_or("relayed"));
                if let Some(Fault::Lost) = fault {
                    return lost();
                }
                response(200, self.intent["withdrawal"].clone())
            }
            _ if route == setup_permit => {
                if self.o.withdrawn_during_permit {
                    self.intent["withdrawal"] = self.withdrawal("relayed");
                }
                if body.get("signature").is_none() {
                    return response(200, self.permit_step("required"));
                }
                let plan = &mut self.intent["funding_plan"];
                plan["payment_setup"] = json!("pending");
                let requests = plan["requests"].as_array().unwrap().clone();
                plan["requests"] =
                    requests.into_iter().filter(|request| request["kind"] != "permit").collect();
                response(200, self.permit_step("pending"))
            }
            _ if route == setup => {
                let reads = &self.o.permit_reads;
                let status = if reads.is_empty() {
                    "done"
                } else {
                    reads[self.permit_reads.min(reads.len() - 1)]
                };
                self.permit_reads += 1;
                response(
                    200,
                    json!({"venue": from, "address": self.owner, "ready": false,
                           "steps": [self.permit_step(status)]}),
                )
            }
            _ if route == wallets => {
                let chain = deployments::chain(&self.venue()).unwrap();
                let last = self.last_balance.clone().unwrap_or(Value::Null);
                let wallet =
                    last["chain_balance"].as_str().or(self.o.chain_balance).unwrap_or("1000000");
                let available = last["escrow_balance"].as_str().unwrap_or("0");
                let observed =
                    last["chain_read_at"].as_str().map_or_else(|| time(0), str::to_owned);
                response(
                    200,
                    json!({
                        "venue": from, "chain_id": chain.id,
                        "escrow": deployments::escrow(chain.id, EscrowEnvironment::Production),
                        "token": chain.token, "symbol": "USDC", "decimals": chain.decimals,
                        "address": self.owner, "wallet": wallet, "available": available,
                        "spendable_wallet": wallet, "spendable_available": available,
                        "committed_wallet": "0", "committed_available": "0", "block": 1,
                        "observed_at": observed,
                        "payment_setup": {"address": self.owner, "authorized": true,
                                          "pending": false, "hash": null, "permit": null},
                    }),
                )
            }
            "GET /v1/balances" => {
                let mut rows = Vec::new();
                if !self.o.unlinked_source {
                    rows.push(self.payer_balance());
                }
                if self.o.wallet.is_none() {
                    let owner = self.o.linked_recipient.unwrap_or(self.owner);
                    let mut row = balance("polymarket", owner, None);
                    merge(
                        &mut row,
                        &json!({"chain_balance": "0", "chain_error": null,
                                            "chain_read_at": time(0), "escrow_balance": "0",
                                            "deposit_balance": "0"}),
                    );
                    rows.push(row);
                }
                response(200, Value::Array(rows))
            }
            "POST /v1/takes" => {
                if let Some(Fault::Raced(message, reason)) = self.fault("take") {
                    self.intent["withdrawal"] = self.withdrawal("relayed");
                    self.unsent = false;
                    return refusal(409, "STATE_CONFLICT", message, reason);
                }
                if self.unsent && !self.o.at_once {
                    return refusal(409, "STATE_CONFLICT", "resolve the withdrawal first", None);
                }
                self.intent["take_id"] = json!(TAKE);
                self.o.take_fill = None;
                self.o.unproven = false;
                response(201, self.prepared())
            }
            _ if route == format!("GET /v1/takes/{TAKE}") => response(200, self.prepared()),
            _ if route == format!("DELETE /v1/takes/{TAKE}") => {
                if let Some(Fault::Conflict(message)) = self.fault("take-cancel") {
                    return refusal(409, "STATE_CONFLICT", message, None);
                }
                let mut expired = self.o.take_fill.take().unwrap_or(json!({}));
                merge(&mut expired, &json!({"state": "EXPIRED", "maker_leg": "ABANDONED"}));
                self.o.take_fill = Some(expired);
                ResponseTemplate::new(204)
            }
            _ if route == format!("POST /v1/takes/{TAKE}/signatures") => {
                match self.fault("submission") {
                    Some(Fault::Unrecorded) => return lost(),
                    Some(Fault::Unreadable) => return response(200, json!({})),
                    _ => {}
                }
                let moves = body["signatures"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|item| item["kind"] == "margin_move")
                    .count();
                let mut fill = self.fill();
                fill["signed_at"] = json!(time(0));
                if self.unsent {
                    // Signed at once: the withdrawal is sent now, the payment once funds arrive.
                    if moves != 1 {
                        return refusal(400, "VALIDATION_ERROR", "one withdrawal signature", None);
                    }
                    self.intent["withdrawal"] =
                        self.withdrawal(self.o.withdrawn.unwrap_or("relayed"));
                    self.unsent = false;
                    self.committed = true;
                    return response(200, json!({"fills": [fill]}));
                }
                if moves > 0 {
                    return refusal(400, "VALIDATION_ERROR", "only the requested signatures", None);
                }
                self.paid = true;
                merge(&mut fill, &json!({"state": "PAID", "payment_leg": "LOCKED"}));
                response(200, json!({"fills": [fill]}))
            }
            _ => refusal(404, "NOT_FOUND", "no route", None),
        }
    }
}

/// A taker on the mock API, with its journal.
struct Taker {
    harness: Harness,
    state: Arc<Mutex<State>>,
    journal: Journal,
}

impl Taker {
    async fn new(options: Options) -> Self {
        Self::with(options, Journal::new(MemoryStore::new())).await
    }

    async fn with(options: Options, journal: Journal) -> Self {
        let harness = Harness::bare().await;
        harness.serve_sign_in().await;
        let state = State::new(options, harness.signer(), harness.clock());
        let state = Arc::new(Mutex::new(state));
        let shared = Arc::clone(&state);
        Mock::given(any())
            .respond_with(move |request: &Request| shared.lock().unwrap().handle(request))
            .with_priority(10)
            .mount(&harness.server)
            .await;
        Self { harness, state, journal }
    }

    fn owner(&self) -> Address {
        self.harness.address()
    }

    /// Moves the caller's clock.
    fn later(&self, seconds: i64) {
        self.harness.later(seconds);
    }

    fn decline(&self, kind: &'static str, times: usize) {
        self.harness.decline(kind, times);
    }

    fn on_wait(&self, from: usize, hook: impl FnMut(&mut State) + Send + 'static) {
        let mut state = self.state.lock().unwrap();
        state.wait_from = from;
        state.on_wait = Some(Box::new(hook));
    }

    fn edit(&self, edit: impl FnOnce(&mut State)) {
        edit(&mut self.state.lock().unwrap());
    }

    /// The request already ran once: the journal names transfer `T1`.
    async fn resumes(&self) {
        let key = scoped_key(EscrowEnvironment::Production, self.owner(), "transfer:move-1");
        let pointer = TransferPointer { intent: T1.parse().unwrap() };
        records::save(&self.journal, &key, &pointer, false).await.unwrap();
    }

    fn key(&self, record: &str) -> String {
        scoped_key(EscrowEnvironment::Production, self.owner(), record)
    }

    async fn holds(&self, record: &str) -> bool {
        self.journal.load(&self.key(record)).await.unwrap().is_some()
    }

    /// Signs in and runs `request`.
    async fn run(&self, request: &TransferRequest) -> Result<TransferOutcome> {
        self.harness.session_with(self.journal.clone()).await.transfer(request).await
    }

    /// The flow's requests, as `"VERB /path"`, sign-in and venue reads left out.
    async fn calls(&self) -> Vec<String> {
        let received = self.harness.server.received_requests().await.unwrap();
        received
            .iter()
            .filter(|r| !r.url.path().starts_with("/v1/auth/") && r.url.path() != "/v1/venues")
            .map(|r| format!("{} {}", r.method.as_str(), r.url.path()))
            .collect()
    }

    async fn count(&self, route: &str) -> usize {
        self.calls().await.iter().filter(|call| *call == route).count()
    }

    async fn bodies(&self, route: &str) -> Vec<Value> {
        let received = self.harness.server.received_requests().await.unwrap();
        received
            .iter()
            .filter(|r| format!("{} {}", r.method.as_str(), r.url.path()) == route)
            .map(|r| r.body_json().unwrap())
            .collect()
    }

    async fn withdrawals(&self) -> Vec<String> {
        self.calls().await.into_iter().filter(|call| call.contains("/withdrawal")).collect()
    }

    /// What was signed beyond the sign-ins, by kind.
    fn kinds(&self) -> Vec<&'static str> {
        self.harness.kinds()
    }

    /// Signer calls beyond the sign-ins.
    fn signatures(&self) -> usize {
        self.harness.flow_signatures()
    }
}

fn terms(owner: Address, funding: Funding) -> TradeTerms {
    TradeTerms::new(
        VenueId::Hyperliquid,
        VenueId::Polymarket,
        Amount::new(U256::from(1_000_000u64)),
        Amount::new(U256::from(1_000_000u64)),
        Recipient::Account(owner),
    )
    .funding(funding)
}

fn transfer(terms: TradeTerms) -> TransferRequest {
    TransferRequest::new(ClientId::new(CLIENT_ID).unwrap(), terms)
        .poll_interval(Duration::from_millis(1))
        .arrival_timeout(Duration::from_secs(10))
}

/// `transfer.test.ts`'s `request`: from wallet funds.
fn wallet(owner: Address) -> TransferRequest {
    transfer(terms(owner, Funding::Wallet))
}

/// `transfer.test.ts`'s `venue`: from the venue account.
fn venue(owner: Address) -> TransferRequest {
    transfer(terms(owner, Funding::Venue))
}

/// From `from`'s account.
fn venue_from(owner: Address, from: VenueId) -> TransferRequest {
    let terms = TradeTerms::new(
        from,
        VenueId::Polymarket,
        Amount::new(U256::from(1_000_000u64)),
        Amount::new(U256::from(1_000_000u64)),
        Recipient::Account(owner),
    );
    transfer(terms)
}

/// To `BYBIT`'s wallet, from `funding`.
fn to_wallet(funding: Funding) -> TransferRequest {
    let terms = TradeTerms::new(
        VenueId::Hyperliquid,
        VenueId::Polymarket,
        Amount::new(U256::from(1_000_000u64)),
        Amount::new(U256::from(1_000_000u64)),
        Recipient::Wallet(BYBIT),
    )
    .funding(funding);
    transfer(terms)
}

/// The fills' states of a filled outcome, and its transfer.
fn filled(outcome: Result<TransferOutcome>) -> (TransferIntent, Vec<String>) {
    match outcome {
        Ok(TransferOutcome::Filled { intent, fills }) => {
            (intent, fills.iter().map(|fill| fill.state.to_string()).collect())
        }
        other => panic!("not filled: {other:?}"),
    }
}

fn states(outcome: Result<TransferOutcome>) -> Vec<String> {
    filled(outcome).1
}

fn pending(outcome: Result<TransferOutcome>) -> PendingReason {
    match outcome {
        Ok(TransferOutcome::Pending { reason, .. }) => reason,
        other => panic!("not pending: {other:?}"),
    }
}

/// The refusal's kind and detail.
fn refused(outcome: Result<TransferOutcome>) -> (VerificationKind, &'static str) {
    match outcome {
        Err(Error::Verification(error)) => (error.kind, error.detail),
        other => panic!("not a refusal: {other:?}"),
    }
}

fn paid() -> Vec<String> {
    vec!["PAID".to_owned()]
}

fn short() -> Vec<Value> {
    vec![
        json!({"chain_balance": "400000"}),
        json!({"chain_balance": "400000"}),
        json!({"chain_balance": "1000000"}),
    ]
}

const SEND: &str = "SendAsset";
const PAY: &str = "PayTake";

fn route(verb: &str, path: &str) -> String {
    format!("{verb} {path}")
}

fn takes() -> String {
    route("POST", "/v1/takes")
}

fn submissions() -> String {
    route("POST", &format!("/v1/takes/{TAKE}/signatures"))
}

fn withdrawal_submission() -> String {
    route("POST", &format!("/v1/transfers/{T1}/withdrawal"))
}

fn withdrawal_preparation() -> String {
    route("POST", &format!("/v1/transfers/{T1}/withdrawal/prepare"))
}

// Wallet funding, recipients and resumption.

/// "wallet funding never withdraws: a shortfall stops before anything is signed, reserved or
/// permitted, even when the plan shows no withdrawal".
#[tokio::test]
async fn wallet_funding_never_withdraws_and_a_shortfall_signs_nothing() {
    for plan in [
        Options { withdrawal_amount: Some("600000"), ..Options::default() },
        Options { permit: true, ..Options::default() },
    ] {
        let t = Taker::new(Options { chain_balance: Some("400000"), ..plan }).await;
        let outcome = t.run(&wallet(t.owner())).await;
        assert_eq!(refused(outcome), (VerificationKind::Changed, details::WALLET_FUNDS));
        assert_eq!(t.signatures(), 0);
        let calls = t.calls().await;
        assert!(
            !calls.iter().any(|call| call.starts_with("POST /v1/takes")
                || call.contains("withdrawal")
                || call.contains("/setup/")),
            "{calls:?}"
        );
    }
}

/// "a payment to anyone but the requested recipient is never signed".
#[tokio::test]
async fn a_payment_to_another_recipient_is_never_signed() {
    let t = Taker::new(Options { recipient: Some(MAKER), ..Options::default() }).await;
    let (kind, detail) = refused(t.run(&wallet(t.owner())).await);
    assert_eq!((kind, detail), (VerificationKind::Differs, details::PAYMENTS_RECEIVE_RECIPIENT));
    assert_eq!(t.signatures(), 0);
    assert_eq!(t.count(&submissions()).await, 0);
    assert!(!t.holds(&format!("take:{TAKE}")).await);
}

/// "after a lost submission, running it again submits the stored signature instead of signing
/// another".
#[tokio::test]
async fn a_lost_submission_is_submitted_again_with_the_stored_signature() {
    let t = Taker::new(Options {
        faults: vec![("submission", Fault::Unrecorded)],
        ..Options::default()
    })
    .await;
    assert!(t.run(&wallet(t.owner())).await.is_err());
    assert_eq!(t.signatures(), 1);
    assert_eq!(states(t.run(&wallet(t.owner())).await), paid());
    assert_eq!(t.signatures(), 1);
    let submitted = t.bodies(&submissions()).await;
    assert_eq!(submitted.len(), 2);
    assert_eq!(submitted[0], submitted[1], "the same signature bytes");
}

/// "an unreadable answer to the payment keeps its signature, submitted again as signed".
#[tokio::test]
async fn an_unreadable_answer_keeps_the_signature() {
    let t = Taker::new(Options {
        faults: vec![("submission", Fault::Unreadable)],
        ..Options::default()
    })
    .await;
    assert!(matches!(t.run(&wallet(t.owner())).await, Err(Error::Decode(_))));
    assert!(t.holds(&format!("take:{TAKE}")).await);
    assert_eq!(states(t.run(&wallet(t.owner())).await), paid());
    assert_eq!(t.signatures(), 1);
}

/// "a transfer attached to a move sends its id, and one answered under another move is never
/// signed".
#[tokio::test]
async fn a_transfer_of_a_move_carries_its_id() {
    const MOVE: &str = "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a60";
    let request = |t: &Taker| wallet(t.owner()).move_id(MOVE.parse().unwrap());
    let t = Taker::new(Options::default()).await;
    let outcome = t.run(&request(&t)).await;
    assert_eq!(refused(outcome), (VerificationKind::Changed, details::MOVE_ID));
    assert_eq!(t.signatures(), 0);
    assert_eq!(t.bodies(&route("POST", "/v1/transfers")).await[0]["move_id"], MOVE);
    t.edit(|state| state.intent["move_id"] = json!(MOVE));
    assert_eq!(states(t.run(&request(&t)).await), paid());
    let outcome = t.run(&wallet(t.owner())).await;
    assert_eq!(refused(outcome), (VerificationKind::Changed, details::MOVE_ID));
}

/// "a resumed take never pays an account other than the transfer's recipient".
#[tokio::test]
async fn a_resumed_take_never_pays_another_account() {
    let t = Taker::new(Options {
        take: true,
        recipient: Some(MAKER),
        linked_recipient: Some(MAKER),
        wallet_funding: true,
        ..Options::default()
    })
    .await;
    t.resumes().await;
    let outcome = t.run(&wallet(t.owner())).await;
    assert_eq!(refused(outcome), (VerificationKind::Changed, details::LINKED_ACCOUNT));
    assert_eq!(t.signatures(), 0);
}

/// "wallet funding is refused for a venue whose account pays, before any request"; GMX venue
/// funding (deferred) likewise.
#[tokio::test]
async fn unsupported_requests_fail_before_any_request() {
    let t = Taker::new(Options::default()).await;
    let terms = terms(t.owner(), Funding::Wallet);
    let aster = TradeTerms::new(
        VenueId::Aster,
        VenueId::Polymarket,
        terms.amount_in,
        terms.min_out,
        terms.recipient,
    )
    .funding(Funding::Wallet);
    assert!(matches!(t.run(&transfer(aster)).await, Err(Error::InvalidRequest(_))));
    let gmx = venue_from(t.owner(), VenueId::Gmx);
    assert_eq!(refused(t.run(&gmx).await), (VerificationKind::Unsupported, details::VENUE));
    let lighter_wallet = TradeTerms::new(
        VenueId::Hyperliquid,
        VenueId::Lighter,
        terms.amount_in,
        terms.min_out,
        Recipient::Wallet(BYBIT),
    );
    assert!(matches!(t.run(&transfer(lighter_wallet)).await, Err(Error::InvalidRequest(_))));
    assert_eq!(t.calls().await, Vec::<String>::new());
    assert_eq!(t.signatures(), 0);
}

/// "a plan the server could not compute fails as its refusal, with when to ask again".
#[tokio::test]
async fn a_plan_the_server_could_not_compute_fails_as_its_refusal() {
    let t = Taker::new(Options {
        withdrawal_amount: Some("600000"),
        answered: Some(json!({"funding_plan": null, "funding_plan_error": {
            "code": "STATE_CONFLICT", "message": "an earlier transfer is being sent",
            "status": 409, "retry_after": 9,
        }})),
        ..Options::default()
    })
    .await;
    let error = t.run(&venue(t.owner())).await.unwrap_err();
    assert_eq!(error.status(), Some(409));
    assert_eq!(error.code(), Some(&ErrorCode::StateConflict));
    assert_eq!(error.retry_after(), Some(Duration::from_secs(9)));
    assert_eq!(t.signatures(), 0);
    assert_eq!(t.withdrawals().await, Vec::<String>::new());
}

/// "margin leaves the venue only for a take the book can execute".
#[tokio::test]
async fn margin_leaves_only_for_an_executable_book() {
    let t = Taker::new(Options {
        chain_balance: Some("400000"),
        withdrawal_amount: Some("600000"),
        levels: Some(json!([])),
        ..Options::default()
    })
    .await;
    assert!(matches!(t.run(&venue(t.owner())).await, Err(Error::QuoteUnavailable(_))));
    assert_eq!(t.signatures(), 0);
    assert_eq!(t.withdrawals().await, Vec::<String>::new());
}

/// "the book is priced at the caller's clock: a level past its validity there funds nothing".
#[tokio::test]
async fn the_book_is_priced_at_the_callers_clock() {
    let t = Taker::new(Options {
        chain_balance: Some("400000"),
        withdrawal_amount: Some("600000"),
        ..Options::default()
    })
    .await;
    t.later(90);
    assert!(matches!(t.run(&venue(t.owner())).await, Err(Error::QuoteUnavailable(_))));
    assert_eq!(t.signatures(), 0);
    assert_eq!(t.withdrawals().await, Vec::<String>::new());
}

// The plan decides the withdrawal.

/// "the plan decides the withdrawal: a balances read predating a payment that spent the funds
/// withdraws them all the same".
#[tokio::test]
async fn the_plan_decides_the_withdrawal_over_a_stale_balance() {
    let t = Taker::new(Options {
        withdrawal_amount: Some("600000"),
        live: vec!["400000", "400000", "1000000"],
        ..Options::default()
    })
    .await;
    assert_eq!(states(t.run(&venue(t.owner())).await), paid());
    assert_eq!(t.bodies(&withdrawal_preparation()).await, vec![json!({"amount": "600000"})]);
    assert_eq!(t.count(&withdrawal_submission()).await, 1);
}

/// "the plan decides the withdrawal: funds the balances read has not seen yet, or still at the
/// deposit address, are waited for, not withdrawn again".
#[tokio::test]
async fn unseen_or_sweeping_funds_are_waited_for() {
    let unseen = json!({"chain_balance": "400000"});
    let deposited = json!({"chain_balance": "0", "deposit_balance": "1000000"});
    for funds in [
        vec![unseen.clone(), unseen, json!({"chain_balance": "1000000"})],
        vec![
            deposited.clone(),
            deposited,
            json!({"chain_balance": "0", "deposit_balance": "0", "escrow_balance": "1000000"}),
        ],
    ] {
        let t = Taker::new(Options { funds, ..Options::default() }).await;
        assert_eq!(states(t.run(&venue(t.owner())).await), paid());
        assert_eq!(t.withdrawals().await, Vec::<String>::new());
    }
}

/// "the plan decides the withdrawal: a plan asking for a withdrawal while the take waits stops
/// before anything is withdrawn or signed".
#[tokio::test]
async fn a_plan_asking_for_a_withdrawal_while_waiting_stops() {
    for (funding, detail) in
        [(Funding::Venue, details::FUNDING_PLAN), (Funding::Wallet, details::WALLET_FUNDS)]
    {
        let t = Taker::new(Options {
            funds: vec![json!({"chain_balance": "0", "deposit_balance": "1000000"})],
            spent: Some("600000"),
            wallet_funding: funding == Funding::Wallet,
            ..Options::default()
        })
        .await;
        let outcome = t.run(&transfer(terms(t.owner(), funding))).await;
        assert_eq!(refused(outcome), (VerificationKind::Changed, detail));
        assert_eq!(t.signatures(), 0);
        let calls = t.calls().await;
        assert!(
            !calls
                .iter()
                .any(|call| call.contains("withdrawal") || call.starts_with("POST /v1/takes")),
            "{calls:?}"
        );
    }
}

/// "the plan decides the withdrawal: after its permit is signed, the plan is read again before
/// withdrawing".
#[tokio::test]
async fn after_the_permit_the_plan_is_read_again_before_withdrawing() {
    let before = json!({"chain_balance": "400000"});
    let t = Taker::new(Options {
        permit: true,
        withdrawal_amount: Some("600000"),
        permit_reads: vec!["done"],
        funds: vec![
            before.clone(),
            before,
            json!({"chain_balance": "1000000", "chain_read_at": time(1)}),
        ],
        ..Options::default()
    })
    .await;
    assert_eq!(states(t.run(&venue(t.owner())).await), paid());
    let calls = t.calls().await;
    let permit = calls.iter().position(|c| *c == "POST /v1/accounts/hyperliquid/setup/permit");
    let withdrawal = calls.iter().position(|c| *c == withdrawal_preparation());
    let between = &calls[permit.unwrap()..withdrawal.unwrap()];
    assert!(between.contains(&route("GET", &format!("/v1/transfers/{T1}"))), "{calls:?}");
}

/// "the plan decides the withdrawal: one recorded while the permit waited is not withdrawn
/// again".
#[tokio::test]
async fn a_withdrawal_recorded_while_the_permit_waited_is_not_sent_again() {
    for (live, filled_now) in [("400000", false), ("1000000", true)] {
        let t = Taker::new(Options {
            permit: true,
            withdrawal_amount: Some("600000"),
            permit_reads: vec!["done"],
            withdrawn_during_permit: true,
            live: vec![live],
            ..Options::default()
        })
        .await;
        let request = venue(t.owner()).arrival_timeout(Duration::from_millis(50));
        let outcome = t.run(&request).await;
        if filled_now {
            assert_eq!(states(outcome), paid());
        } else {
            assert_eq!(pending(outcome), PendingReason::ArrivalUnconfirmed);
        }
        assert_eq!(t.withdrawals().await, Vec::<String>::new());
        let expected = if filled_now { vec!["Permit", PAY] } else { vec!["Permit"] };
        assert_eq!(t.kinds(), expected);
    }
}

// Replacements.

/// "a replacement decided before a lost cancellation answer is completed on the next run".
#[tokio::test]
async fn a_replacement_decided_before_a_lost_cancellation_is_completed_next_run() {
    let t = Taker::new(Options {
        stale_preparation: true,
        faults: vec![("cancel", Fault::Lost)],
        ..Options::default()
    })
    .await;
    t.resumes().await;
    assert!(t.run(&venue(t.owner())).await.is_err());
    assert!(t.holds(&format!("renewal:{T1}")).await);
    let (intent, fills) = filled(t.run(&venue(t.owner())).await);
    assert_eq!(intent.id.to_string(), T2);
    assert_eq!(fills, paid());
    assert!(!t.holds(&format!("renewal:{T1}")).await);
}

/// "a refused cancellation leaves no replacement owed".
#[tokio::test]
async fn a_refused_cancellation_owes_no_replacement() {
    let t = Taker::new(Options {
        stale_preparation: true,
        faults: vec![("cancel", Fault::Conflict("withdrawal accepted"))],
        ..Options::default()
    })
    .await;
    t.resumes().await;
    let error = t.run(&venue(t.owner())).await.unwrap_err();
    assert_eq!(error.code(), Some(&ErrorCode::StateConflict));
    assert!(!t.holds(&format!("renewal:{T1}")).await);
    assert_eq!(t.count(&route("POST", "/v1/transfers")).await, 0);
}

/// "a recovered take from wallet funds signs only after a fresh read covers it".
#[tokio::test]
async fn a_recovered_wallet_take_signs_only_after_a_covering_read() {
    let t = Taker::new(Options {
        take: true,
        chain_balance: Some("0"),
        wallet_funding: true,
        ..Options::default()
    })
    .await;
    t.resumes().await;
    let outcome = t.run(&wallet(t.owner())).await;
    assert_eq!(refused(outcome), (VerificationKind::Changed, details::WALLET_FUNDS));
    assert_eq!(t.signatures(), 0);
    assert_eq!(t.count(&submissions()).await, 0);
}

/// "a replacement recorded as funded is never funded again, even when the balance looks short".
#[tokio::test]
async fn a_replacement_recorded_as_funded_is_never_funded_again() {
    let t = Taker::new(Options {
        chain_balance: Some("400000"),
        withdrawal_amount: Some("600000"),
        ..Options::default()
    })
    .await;
    t.resumes().await;
    records::save(&t.journal, &t.key(&format!("funded:{T1}")), &Marker {}, false).await.unwrap();
    assert_eq!(states(t.run(&venue(t.owner())).await), paid());
    assert_eq!(t.withdrawals().await, Vec::<String>::new());
    assert_eq!(t.count(&takes()).await, 1);
}

/// "a transfer answered under another client id, or between other chains, is never paid"; and
/// every other term the request states (signer never called, nothing reserved or withdrawn).
#[tokio::test]
async fn a_transfer_answered_with_other_terms_is_never_paid() {
    let cases = [
        (
            json!({"client_transfer_id": "move-2"}),
            VerificationKind::Differs,
            details::CLIENT_REQUEST_ID,
        ),
        (json!({"to_chain_id": 56}), VerificationKind::Changed, details::VENUE),
        (json!({"from_chain_id": 137}), VerificationKind::Changed, details::VENUE),
        (json!({"cancelled_at": time(0)}), VerificationKind::Changed, details::STATE),
        (json!({"amount_in": "1000001"}), VerificationKind::Changed, details::AMOUNT_IN),
        (json!({"min_out": "1"}), VerificationKind::Changed, details::AMOUNT_OUT),
        (json!({"max_parts": 2}), VerificationKind::Changed, details::FILLS),
        (json!({"max_fee_bps": 5}), VerificationKind::Changed, details::FEE_ABOVE_ACCEPTED),
        (json!({"payer": MAKER}), VerificationKind::Changed, details::PAYER),
        (json!({"recipient": MAKER}), VerificationKind::Changed, details::RECIPIENT),
        (json!({"destination": "wallet"}), VerificationKind::Changed, details::DESTINATION),
        (
            json!({"then": {"to": "aster", "recipient": MAKER, "amount_in": "1", "min_out": "1"}}),
            VerificationKind::Changed,
            details::DESTINATION,
        ),
        (json!({"funding": "venue"}), VerificationKind::Changed, details::FUNDING),
        (
            json!({"conversion": {"amount_in": "1000000", "min_out": "1000000"}}),
            VerificationKind::Changed,
            details::FUNDING,
        ),
        (json!({"from_dex": "xyz"}), VerificationKind::Changed, details::SOURCE_DEX),
        (
            json!({"previous_transfer_id": T2}),
            VerificationKind::Changed,
            details::CLIENT_REQUEST_ID,
        ),
    ];
    for (answered, kind, detail) in cases {
        let t =
            Taker::new(Options { answered: Some(answered.clone()), ..Options::default() }).await;
        let outcome = t.run(&wallet(t.owner())).await;
        assert_eq!(refused(outcome), (kind, detail), "{answered}");
        assert_eq!(t.signatures(), 0);
        let calls = t.calls().await;
        assert!(
            !calls
                .iter()
                .any(|call| call.starts_with("POST /v1/takes") || call.contains("withdrawal")),
            "{answered}: {calls:?}"
        );
    }
}

// Venue funding.

/// "one venue-funded transfer withdraws only what the address lacks, waits for its arrival,
/// then takes".
#[tokio::test]
async fn a_venue_funded_transfer_withdraws_the_shortfall_waits_then_takes() {
    let t = Taker::new(Options {
        withdrawal_amount: Some("600000"),
        funds: short(),
        ..Options::default()
    })
    .await;
    assert_eq!(states(t.run(&venue(t.owner())).await), paid());
    assert_eq!(t.bodies(&withdrawal_preparation()).await, vec![json!({"amount": "600000"})]);
    assert_eq!(t.kinds(), vec![SEND, PAY]);
    assert_eq!(t.count(&takes()).await, 1);
    assert!(!t.holds(&format!("transfer-withdrawal:{T1}")).await);
}

/// "without payment setup, the permit is signed before the withdrawal and the take waits until
/// it is final".
#[tokio::test]
async fn the_permit_is_signed_before_the_withdrawal_and_waited_for() {
    let t = Taker::new(Options {
        setup: Some("required"),
        withdrawal_amount: Some("600000"),
        funds: short(),
        permit_reads: vec!["pending", "pending", "done"],
        ..Options::default()
    })
    .await;
    assert_eq!(states(t.run(&venue(t.owner())).await), paid());
    let calls = t.calls().await;
    let at = |call: &str| calls.iter().position(|c| c == call).unwrap();
    let last = |call: &str| calls.iter().rposition(|c| c == call).unwrap();
    let (permit, setup) =
        ("POST /v1/accounts/hyperliquid/setup/permit", "GET /v1/accounts/hyperliquid/setup");
    // The permit is submitted before the withdrawal; the take waits until it is final.
    assert!(last(permit) < at(&withdrawal_submission()), "{calls:?}");
    assert!(at(&withdrawal_submission()) < at(setup) && last(setup) < at(&takes()), "{calls:?}");
    assert_eq!(t.kinds(), vec!["Permit", SEND, PAY]);
}

/// "a refused permit never withdraws or takes".
#[tokio::test]
async fn a_refused_permit_never_withdraws_or_takes() {
    let t = Taker::new(Options {
        setup: Some("required"),
        withdrawal_amount: Some("600000"),
        funds: short(),
        ..Options::default()
    })
    .await;
    t.decline("Permit", usize::MAX);
    assert!(matches!(t.run(&venue(t.owner())).await, Err(Error::Signer(_))));
    assert_eq!(t.withdrawals().await, Vec::<String>::new());
    assert_eq!(t.count(&takes()).await, 0);
}

/// "a submitted permit is waited for without another signature; still pending after the wait,
/// the transfer stays resumable and takes nothing".
#[tokio::test]
async fn a_submitted_permit_is_waited_for_then_pending() {
    let waited = Taker::new(Options {
        setup: Some("pending"),
        permit_reads: vec!["pending", "done"],
        ..Options::default()
    })
    .await;
    assert_eq!(states(waited.run(&venue(waited.owner())).await), paid());
    let setup: Vec<String> =
        waited.calls().await.into_iter().filter(|call| call.contains("/setup")).collect();
    assert_eq!(setup, vec!["GET /v1/accounts/hyperliquid/setup"; 2]);
    assert_eq!(waited.kinds(), vec![PAY]);
    let still = Taker::new(Options {
        setup: Some("pending"),
        permit_reads: vec!["pending"],
        ..Options::default()
    })
    .await;
    let request = venue(still.owner()).arrival_timeout(Duration::from_millis(30));
    assert_eq!(pending(still.run(&request).await), PendingReason::AuthorizationUnconfirmed);
    assert_eq!(still.count(&takes()).await, 0);
}

/// "a final or absent payment setup is not waited for".
#[tokio::test]
async fn a_final_or_absent_payment_setup_is_not_waited_for() {
    for (options, resume) in [
        (Options { setup: Some("done"), ..Options::default() }, false),
        (Options { withdrawal: Some("relayed"), ..Options::default() }, true),
    ] {
        let t = Taker::new(options).await;
        if resume {
            t.resumes().await;
        }
        assert_eq!(states(t.run(&venue(t.owner())).await), paid());
        assert!(!t.calls().await.iter().any(|call| call.contains("/setup")));
    }
}

/// "dust at the deposit address is not counted: the shortfall is withdrawn in full".
#[tokio::test]
async fn dust_at_the_deposit_address_is_not_counted() {
    let dusty = json!({"chain_balance": "400000", "deposit_balance": "500000"});
    let t = Taker::new(Options {
        withdrawal_amount: Some("600000"),
        funds: vec![
            dusty.clone(),
            dusty,
            json!({"chain_balance": "1000000", "deposit_balance": "0"}),
        ],
        ..Options::default()
    })
    .await;
    assert_eq!(states(t.run(&venue(t.owner())).await), paid());
    assert_eq!(t.bodies(&withdrawal_preparation()).await, vec![json!({"amount": "600000"})]);
}

/// "a take paid inside a venue funds nothing first and goes straight to its reservation".
#[tokio::test]
async fn a_take_paid_inside_a_venue_funds_nothing_first() {
    for (from, name) in [(VenueId::Aster, "aster"), (VenueId::Lighter, "lighter")] {
        let t = Taker::new(Options { from: Some(name), ..Options::default() }).await;
        let outcome = t.run(&venue_from(t.owner(), from)).await;
        assert_eq!(refused(outcome), (VerificationKind::Differs, details::TO_SIGN_KIND));
        assert_eq!(t.count(&takes()).await, 1);
        assert_eq!(t.withdrawals().await, Vec::<String>::new());
        assert_eq!(t.signatures(), 0);
    }
}

/// "funds on the address that cover the amount go straight to the take without a withdrawal:
/// only its payment is signed and submitted".
#[tokio::test]
async fn covering_funds_go_straight_to_the_take() {
    for funding in [Funding::Wallet, Funding::Venue] {
        let t = Taker::new(Options::default()).await;
        assert_eq!(states(t.run(&transfer(terms(t.owner(), funding))).await), paid());
        assert_eq!(t.signatures(), 1);
        assert_eq!(t.withdrawals().await, Vec::<String>::new());
    }
}

// Withdrawal outcomes.

/// "a refused or unknown withdrawal outcome stops before taking; a queued one read later as
/// refused ends the wait with its error".
#[tokio::test]
async fn a_refused_or_unknown_withdrawal_is_pending() {
    for (withdrawn, reason) in [
        ("refused", PendingReason::WithdrawalRefused),
        ("unknown", PendingReason::WithdrawalUnknown),
    ] {
        let t = Taker::new(Options {
            withdrawal_amount: Some("600000"),
            chain_balance: Some("400000"),
            withdrawn: Some(withdrawn),
            ..Options::default()
        })
        .await;
        assert_eq!(pending(t.run(&venue(t.owner())).await), reason);
        assert_eq!(t.count(&takes()).await, 0);
        // An unknown outcome keeps the signature: never another for it.
        assert_eq!(t.holds(&format!("transfer-withdrawal:{T1}")).await, withdrawn == "unknown");
    }
    let t = Taker::new(Options {
        withdrawal_amount: Some("600000"),
        chain_balance: Some("400000"),
        withdrawn: Some("queued"),
        ..Options::default()
    })
    .await;
    t.on_wait(2, |state| state.intent["withdrawal"]["state"] = json!("refused"));
    assert_eq!(pending(t.run(&venue(t.owner())).await), PendingReason::WithdrawalRefused);
    assert_eq!(t.count(&takes()).await, 0);
}

/// "a balance that covers the amount does not skip reconciling a queued withdrawal: the take
/// waits until it is relayed".
#[tokio::test]
async fn a_queued_withdrawal_is_reconciled_before_taking() {
    let t = Taker::new(Options {
        withdrawal: Some("queued"),
        withdrawal_amount: Some("600000"),
        ..Options::default()
    })
    .await;
    t.resumes().await;
    t.on_wait(2, |state| {
        assert!(state.intent["take_id"].is_null(), "nothing taken before the relay");
        state.intent["withdrawal"]["state"] = json!("relayed");
    });
    assert_eq!(states(t.run(&venue(t.owner())).await), paid());
    assert_eq!(t.withdrawals().await, Vec::<String>::new());
}

/// "a linked account changed during the arrival wait prevents the take".
#[tokio::test]
async fn a_linked_account_changed_during_the_wait_prevents_the_take() {
    let t = Taker::new(Options {
        withdrawal_amount: Some("600000"),
        chain_balance: Some("400000"),
        ..Options::default()
    })
    .await;
    t.on_wait(1, |state| state.o.linked_recipient = Some(MAKER));
    let outcome = t.run(&venue(t.owner())).await;
    assert_eq!(refused(outcome), (VerificationKind::Changed, details::LINKED_ACCOUNT));
    assert_eq!(t.count(&takes()).await, 0);
}

/// "a stale balance read is refused even when it covers the amount: nothing is withdrawn or
/// taken".
#[tokio::test]
async fn a_stale_balance_read_is_refused() {
    let t = Taker::new(Options {
        funds: vec![json!({"chain_read_at": "2020-01-01T00:00:00Z"})],
        ..Options::default()
    })
    .await;
    let outcome = t.run(&venue(t.owner())).await;
    assert_eq!(refused(outcome), (VerificationKind::Stale, details::WALLET_FUNDS_OBSERVED_AT));
    assert_eq!(t.withdrawals().await, Vec::<String>::new());
    assert_eq!(t.count(&takes()).await, 0);
    assert_eq!(t.signatures(), 0);
}

/// "an accepted take is recovered before any balance is read, and one accepted elsewhere
/// during the arrival wait is never taken again".
#[tokio::test]
async fn an_accepted_take_is_recovered_never_taken_again() {
    let accepted = json!({"state": "PAID", "payment_leg": "LOCKED", "signed_at": time(-5)});
    let resumed = Taker::new(Options {
        take: true,
        withdrawal: Some("relayed"),
        chain_balance: Some("0"),
        take_fill: Some(accepted.clone()),
        ..Options::default()
    })
    .await;
    resumed.resumes().await;
    assert_eq!(states(resumed.run(&venue(resumed.owner())).await), paid());
    assert_eq!(resumed.count("GET /v1/balances").await, 0);
    assert_eq!(resumed.signatures(), 0);
    let t = Taker::new(Options {
        withdrawal_amount: Some("600000"),
        chain_balance: Some("400000"),
        ..Options::default()
    })
    .await;
    t.on_wait(1, move |state| {
        state.intent["take_id"] = json!(TAKE);
        state.o.take_fill = Some(accepted.clone());
    });
    assert_eq!(states(t.run(&venue(t.owner())).await), paid());
    assert_eq!(t.count(&withdrawal_submission()).await, 1);
    assert_eq!(t.count(&takes()).await, 0);
}

/// "a withdrawal answer lost after the server recorded it resumes from the transfer, never
/// submitted or signed again".
#[tokio::test]
async fn a_lost_withdrawal_answer_resumes_from_the_transfer() {
    let t = Taker::new(Options {
        withdrawal_amount: Some("600000"),
        funds: vec![json!({"chain_balance": "400000"}), json!({"chain_balance": "1000000"})],
        faults: vec![("withdrawal", Fault::Lost)],
        ..Options::default()
    })
    .await;
    assert!(t.run(&venue(t.owner())).await.is_err());
    let sent = t.count(&withdrawal_submission()).await;
    assert_eq!(states(t.run(&venue(t.owner())).await), paid());
    assert_eq!(t.count(&withdrawal_submission()).await, sent);
    assert_eq!(t.count(&withdrawal_preparation()).await, 1);
    assert_eq!(t.kinds(), vec![SEND, PAY]);
}

/// "a withdrawal the server never recorded is submitted again with its stored signature, never
/// signed twice".
#[tokio::test]
async fn an_unrecorded_withdrawal_is_submitted_again_with_its_signature() {
    let mut funds = short()[..2].to_vec();
    funds.extend(short());
    let t = Taker::new(Options {
        withdrawal_amount: Some("600000"),
        funds,
        faults: vec![("withdrawal", Fault::Unrecorded)],
        ..Options::default()
    })
    .await;
    assert!(t.run(&venue(t.owner())).await.is_err());
    assert!(t.holds(&format!("transfer-withdrawal:{T1}")).await);
    assert_eq!(states(t.run(&venue(t.owner())).await), paid());
    assert_eq!(t.count(&withdrawal_preparation()).await, 1);
    let submitted = t.bodies(&withdrawal_submission()).await;
    assert_eq!(submitted.len(), 2);
    assert_eq!(submitted[0], submitted[1], "the same signature");
    assert_eq!(t.kinds(), vec![SEND, PAY]);
    assert!(!t.holds(&format!("transfer-withdrawal:{T1}")).await);
}

/// A saved withdrawal goes for its own amount after the shortfall moved, never above what the
/// transfer pays.
#[tokio::test]
async fn a_saved_withdrawal_is_submitted_for_its_own_amount() {
    let mut funds = short()[..2].to_vec();
    funds.extend(short());
    let t = Taker::new(Options {
        withdrawal_amount: Some("600000"),
        funds,
        faults: vec![("withdrawal", Fault::Unrecorded), ("withdrawal", Fault::Unrecorded)],
        ..Options::default()
    })
    .await;
    assert!(t.run(&venue(t.owner())).await.is_err());
    t.edit(|s| s.intent["funding_plan"]["withdrawal_amount"] = json!("500000"));
    assert!(t.run(&venue(t.owner())).await.is_err());
    let submitted = t.bodies(&withdrawal_submission()).await;
    assert_eq!((submitted.len(), &submitted[0]), (2, &submitted[1]), "the same signature");
    // A record for more than the transfer pays is never submitted.
    let key = t.key(&format!("transfer-withdrawal:{T1}"));
    let mut record: Value =
        serde_json::from_slice(&t.journal.load(&key).await.unwrap().unwrap()).unwrap();
    record["amount"] = json!("1000001");
    record["typed_data"]["message"]["amount"] = json!("1.000001");
    t.journal.save(&key, &serde_json::to_vec(&record).unwrap()).await.unwrap();
    let Err(Error::Verification(error)) = t.run(&venue(t.owner())).await else { panic!() };
    assert_eq!((error.kind, error.detail), (VerificationKind::Differs, details::AMOUNT));
    assert_eq!(t.count(&withdrawal_submission()).await, 2);
    assert_eq!((t.count(&withdrawal_preparation()).await, t.kinds()), (1, vec![SEND]));
}

/// "funds never confirmed leave the transfer pending, and running it again withdraws nothing
/// more".
#[tokio::test]
async fn unconfirmed_funds_are_pending_and_never_withdrawn_again() {
    let t = Taker::new(Options {
        withdrawal_amount: Some("600000"),
        chain_balance: Some("400000"),
        ..Options::default()
    })
    .await;
    let request = venue(t.owner()).arrival_timeout(Duration::from_millis(30));
    for _ in 0..2 {
        assert_eq!(pending(t.run(&request).await), PendingReason::ArrivalUnconfirmed);
    }
    assert_eq!(t.count(&withdrawal_submission()).await, 1);
    assert_eq!(t.count(&takes()).await, 0);
}

/// "a resumed transfer with other terms is refused, and one changed or cancelled during the
/// wait opens no other signature".
#[tokio::test]
async fn a_transfer_changed_or_cancelled_opens_no_other_signature() {
    let resumed = Taker::new(Options::default()).await;
    resumed.resumes().await;
    let terms = terms(resumed.owner(), Funding::Venue);
    let other = TradeTerms::new(
        VenueId::Hyperliquid,
        VenueId::Polymarket,
        terms.amount_in,
        Amount::new(U256::from(999_999u64)),
        terms.recipient,
    );
    let outcome = resumed.run(&transfer(other)).await;
    assert_eq!(refused(outcome), (VerificationKind::Changed, details::AMOUNT_OUT));
    assert_eq!(resumed.withdrawals().await, Vec::<String>::new());
    assert_eq!(resumed.signatures(), 0);
    for (patch, detail) in [
        (json!({"min_out": "1"}), details::AMOUNT_OUT),
        (json!({"cancelled_at": time(0)}), details::STATE),
    ] {
        let t = Taker::new(Options {
            withdrawal_amount: Some("600000"),
            chain_balance: Some("400000"),
            ..Options::default()
        })
        .await;
        t.on_wait(1, move |state| merge(&mut state.intent, &patch));
        assert_eq!(refused(t.run(&venue(t.owner())).await), (VerificationKind::Changed, detail));
        assert_eq!(t.count(&takes()).await, 0);
        assert_eq!(t.kinds(), vec![SEND]);
    }
}

/// "the price is read again after the funds arrive; running it again reconciles the same
/// withdrawal and takes".
#[tokio::test]
async fn the_price_is_read_again_after_arrival() {
    let t = Taker::new(Options {
        withdrawal_amount: Some("600000"),
        funds: short(),
        ..Options::default()
    })
    .await;
    t.on_wait(1, |state| state.o.levels = Some(json!([])));
    assert!(matches!(t.run(&venue(t.owner())).await, Err(Error::QuoteUnavailable(_))));
    assert_eq!(t.count(&takes()).await, 0);
    t.edit(|state| {
        state.on_wait = None;
        state.o.levels = None;
    });
    assert_eq!(states(t.run(&venue(t.owner())).await), paid());
    assert_eq!(t.count(&withdrawal_submission()).await, 1);
    assert_eq!(t.kinds(), vec![SEND, PAY]);
}

/// "a recent withdrawal preparation is signed and submitted before any new price check".
#[tokio::test]
async fn a_recent_preparation_is_signed_before_any_price_check() {
    let t = Taker::new(Options {
        fresh_preparation: true,
        withdrawal_amount: Some("600000"),
        funds: short(),
        levels: Some(json!([])),
        ..Options::default()
    })
    .await;
    t.resumes().await;
    assert!(matches!(t.run(&venue(t.owner())).await, Err(Error::QuoteUnavailable(_))));
    let calls = t.calls().await;
    assert!(!calls.contains(&withdrawal_preparation()));
    let submitted = calls.iter().position(|call| *call == withdrawal_submission()).unwrap();
    let priced = calls.iter().position(|call| call == "GET /v1/books").unwrap();
    assert!(submitted < priced, "{calls:?}");
    assert_eq!(t.count(&takes()).await, 0);
    assert_eq!(t.kinds(), vec![SEND]);
}

/// "a stale withdrawal preparation cancels its transfer before the replacement, which the take
/// pays"; the take uses the replacement's server ids, never the caller's.
#[tokio::test]
async fn a_stale_preparation_is_cancelled_and_replaced() {
    let t = Taker::new(Options { stale_preparation: true, ..Options::default() }).await;
    t.resumes().await;
    let (intent, fills) = filled(t.run(&venue(t.owner())).await);
    assert_eq!((intent.id.to_string(), fills), (T2.to_owned(), paid()));
    let calls = t.calls().await;
    let cancelled =
        calls.iter().position(|c| *c == route("DELETE", &format!("/v1/transfers/{T1}")));
    let replaced = calls.iter().position(|c| *c == route("POST", "/v1/transfers"));
    assert!(cancelled.unwrap() < replaced.unwrap(), "{calls:?}");
    let replacement = t.bodies(&route("POST", "/v1/transfers")).await;
    assert_eq!(replacement[0]["client_transfer_id"], format!("renew:{T1}"));
    let taken = t.bodies(&takes()).await;
    assert_eq!(taken[0]["transfer_intent_id"], T2);
    assert_eq!(taken[0]["client_take_id"], take_client_id(T2));
    assert_ne!(taken[0]["client_take_id"], CLIENT_ID);
    assert_eq!(t.withdrawals().await, Vec::<String>::new());
}

/// R2-2: renewal, the replacement fills, a restart: the same request returns the same
/// completion from the journal's pointer, without any signature.
#[tokio::test]
async fn a_filled_replacement_is_found_again_after_a_restart() {
    let t = Taker::new(Options { stale_preparation: true, ..Options::default() }).await;
    t.resumes().await;
    let (first, fills) = filled(t.run(&venue(t.owner())).await);
    assert_eq!((first.id.to_string(), fills), (T2.to_owned(), paid()));
    let signed = t.signatures();
    let posted = t.count(&route("POST", "/v1/transfers")).await;
    // A restart: a new session on the same journal.
    let (again, fills) = filled(t.run(&venue(t.owner())).await);
    assert_eq!(again.id, first.id);
    assert_eq!(fills, paid());
    assert_eq!(t.signatures(), signed);
    assert_eq!(t.count(&route("POST", "/v1/transfers")).await, posted);
    assert_eq!(t.count(&takes()).await, 1);
}

/// "a reservation past its payment window is cancelled before any replacement: a refused
/// cancellation stops everything".
#[tokio::test]
async fn a_late_reservation_is_cancelled_before_any_replacement() {
    let refused_cancel = Taker::new(Options {
        take: true,
        faults: vec![("take-cancel", Fault::Conflict("take signed"))],
        ..Options::default()
    })
    .await;
    refused_cancel.resumes().await;
    refused_cancel.later(60);
    let error = refused_cancel.run(&venue(refused_cancel.owner())).await.unwrap_err();
    assert_eq!(error.code(), Some(&ErrorCode::StateConflict));
    assert_eq!(refused_cancel.count(&route("DELETE", &format!("/v1/takes/{TAKE}"))).await, 1);
    assert_eq!(refused_cancel.count(&route("POST", "/v1/transfers")).await, 0);
    assert_eq!(refused_cancel.signatures(), 0);
    let t = Taker::new(Options { take: true, ..Options::default() }).await;
    t.resumes().await;
    t.later(60);
    assert!(t.run(&venue(t.owner())).await.is_err());
    let calls = t.calls().await;
    let cancelled = calls.iter().position(|c| *c == route("DELETE", &format!("/v1/takes/{TAKE}")));
    let replaced = calls.iter().position(|c| *c == route("POST", "/v1/transfers"));
    assert!(cancelled.unwrap() < replaced.unwrap(), "{calls:?}");
    let replacement = t.bodies(&route("POST", "/v1/transfers")).await;
    assert_eq!(replacement[0]["client_transfer_id"], format!("retry:{T1}"));
}

/// "an expired take that was signed, or whose signature is unproven, is returned as it is,
/// never replaced".
#[tokio::test]
async fn an_expired_signed_or_unproven_take_is_returned_as_it_is() {
    for unproven in [false, true] {
        let t = Taker::new(Options {
            take: true,
            take_fill: Some(
                json!({"state": "EXPIRED", "maker_leg": "ABANDONED", "signed_at": time(-10)}),
            ),
            unproven,
            ..Options::default()
        })
        .await;
        t.resumes().await;
        assert_eq!(states(t.run(&venue(t.owner())).await), vec!["EXPIRED".to_owned()]);
        let calls = t.calls().await;
        assert!(!calls.iter().any(|c| *c == route("POST", "/v1/transfers") || *c == takes()));
        assert_eq!(t.signatures(), 0);
    }
}

/// "an unsigned expired take is replaced under retry:<id> with its route's terms, and the
/// replacement is never funded again" (routes are deferred: the request's terms).
#[tokio::test]
async fn an_unsigned_expired_take_is_replaced_and_never_funded_again() {
    let t = Taker::new(Options {
        take: true,
        withdrawal: Some("relayed"),
        withdrawal_amount: Some("600000"),
        chain_balance: Some("0"),
        take_fill: Some(json!({"state": "EXPIRED", "maker_leg": "ABANDONED"})),
        ..Options::default()
    })
    .await;
    t.resumes().await;
    let (intent, fills) = filled(t.run(&venue(t.owner())).await);
    assert_eq!((intent.id.to_string(), fills), (T2.to_owned(), paid()));
    let owner = t.owner();
    assert_eq!(
        t.bodies(&route("POST", "/v1/transfers")).await,
        vec![json!({
            "client_transfer_id": format!("retry:{T1}"), "from": "hyperliquid", "to": "polymarket",
            "amount_in": "1000000", "min_out": "1000000", "max_parts": 8, "payer": owner,
            "recipient": owner,
        })]
    );
    let taken = t.bodies(&takes()).await;
    assert_eq!(taken[0]["client_take_id"], take_client_id(T2));
    assert_eq!(taken[0]["transfer_intent_id"], T2);
    assert!(t.holds(&format!("funded:{T2}")).await);
    assert_eq!(t.withdrawals().await, Vec::<String>::new());
    // The request now names the replacement.
    let pointer: TransferPointer =
        records::load(&t.journal, &t.key("transfer:move-1")).await.unwrap().unwrap();
    assert_eq!(pointer.intent.to_string(), T2);
}

/// A crash between recording a funded replacement and pointing the request at it never funds
/// the replacement again: the funded mark is written first.
#[tokio::test]
async fn a_replacement_interrupted_between_its_records_is_never_funded_again() {
    for store in
        [FlakyStore::failing_save_of("funded:", 1), FlakyStore::failing_save_of(":move-1", 2)]
    {
        let t = Taker::with(
            Options {
                take: true,
                withdrawal: Some("relayed"),
                withdrawal_amount: Some("600000"),
                chain_balance: Some("0"),
                take_fill: Some(json!({"state": "EXPIRED", "maker_leg": "ABANDONED"})),
                ..Options::default()
            },
            Journal::new(store),
        )
        .await;
        t.resumes().await;
        let Err(Error::Store(error)) = t.run(&venue(t.owner())).await else {
            panic!("a record of the replacement fails");
        };
        assert!(error.submitted());
        let (intent, fills) = filled(t.run(&venue(t.owner())).await);
        assert_eq!((intent.id.to_string(), fills), (T2.to_owned(), paid()));
        assert_eq!(t.withdrawals().await, Vec::<String>::new());
        assert_eq!(t.count(&takes()).await, 1);
    }
}

/// "a lost replacement answer is asked again under the same retry:<id> key, funding nothing".
#[tokio::test]
async fn a_lost_replacement_is_asked_again_under_the_same_key() {
    let t = Taker::new(Options {
        take: true,
        take_fill: Some(json!({"state": "EXPIRED", "maker_leg": "ABANDONED"})),
        faults: vec![("replacement", Fault::Unrecorded)],
        ..Options::default()
    })
    .await;
    t.resumes().await;
    assert!(t.run(&venue(t.owner())).await.is_err());
    assert_eq!(states(t.run(&venue(t.owner())).await), paid());
    let ids: Vec<Value> = t
        .bodies(&route("POST", "/v1/transfers"))
        .await
        .into_iter()
        .map(|body| body["client_transfer_id"].clone())
        .collect();
    assert_eq!(ids, vec![json!(format!("retry:{T1}")); 2]);
    assert_eq!(t.count(&takes()).await, 1);
    assert_eq!(t.withdrawals().await, Vec::<String>::new());
}

/// "a declined take signature after funding is signed again on the same take when run again,
/// never withdrawn twice".
#[tokio::test]
async fn a_declined_take_signature_is_signed_again_on_the_same_take() {
    let t = Taker::new(Options {
        withdrawal_amount: Some("600000"),
        funds: short(),
        ..Options::default()
    })
    .await;
    t.decline(PAY, 1);
    assert!(matches!(t.run(&venue(t.owner())).await, Err(Error::Signer(_))));
    assert_eq!(states(t.run(&venue(t.owner())).await), paid());
    assert_eq!(t.count(&withdrawal_submission()).await, 1);
    assert_eq!(t.count(&takes()).await, 1);
    assert_eq!(t.kinds(), vec![SEND, PAY]);
}

// Signed at once.

/// Signed at once, by default from 0.4 USDC on the address, paid within two minutes.
fn at_once(mut options: Options) -> Options {
    options.chain_balance.get_or_insert("400000");
    options.withdrawal_amount.get_or_insert("600000");
    options.pay_window.get_or_insert(120);
    options.at_once = true;
    options
}

/// "a venue-funded transfer is signed at once: the take reserved against its prepared
/// withdrawal, both committed together, no arrival waited for".
#[tokio::test]
async fn a_venue_funded_transfer_is_signed_at_once() {
    let t = Taker::new(at_once(Options::default())).await;
    // Nothing is waited for: a wait would outlast the timeout.
    let request = venue(t.owner()).poll_interval(Duration::from_secs(3600));
    let outcome = tokio::time::timeout(Duration::from_secs(5), t.run(&request)).await.unwrap();
    assert_eq!(states(outcome), vec!["PREPARED".to_owned()]);
    assert_eq!(t.kinds(), vec![SEND, PAY]);
    let calls = t.calls().await;
    assert!(!calls.contains(&withdrawal_submission()));
    let prepared = calls.iter().position(|c| *c == withdrawal_preparation()).unwrap();
    let reserved = calls.iter().position(|c| *c == takes()).unwrap();
    assert!(prepared < reserved);
    let submitted = t.bodies(&submissions()).await;
    let kinds: Vec<&Value> =
        submitted[0]["signatures"].as_array().unwrap().iter().map(|i| &i["kind"]).collect();
    assert_eq!(kinds, vec!["pay_take", "margin_move"]);
}

/// "a take signed at once whose commit was lost is committed again with the same signatures,
/// never signed twice, even once its withdrawal could no longer be signed".
#[tokio::test]
async fn a_lost_at_once_commit_is_committed_again_with_the_same_signatures() {
    let t = Taker::new(at_once(Options {
        faults: vec![("submission", Fault::Unrecorded)],
        ..Options::default()
    }))
    .await;
    assert!(t.run(&venue(t.owner())).await.is_err());
    assert_eq!(t.signatures(), 2);
    // Seventy seconds later the withdrawal could not be signed again, but it was signed already.
    t.later(70);
    assert_eq!(states(t.run(&venue(t.owner())).await), vec!["PREPARED".to_owned()]);
    assert_eq!(t.signatures(), 2);
    assert_eq!(t.count(&takes()).await, 1);
    assert!(!t.calls().await.contains(&withdrawal_submission()));
    let submitted = t.bodies(&submissions()).await;
    assert_eq!(submitted[0], submitted[1], "the same signatures");
}

/// "a committed take signed at once waits for its funds, even past its `pay_by`: running it again
/// signs, deletes and withdraws nothing".
#[tokio::test]
async fn a_committed_at_once_take_is_returned_without_any_write() {
    for take_fill in [json!({}), json!({"pay_by": time(-1)})] {
        let t = Taker::new(at_once(Options {
            take: true,
            withdrawal: Some("relayed"),
            committed: true,
            take_fill: Some(take_fill),
            ..Options::default()
        }))
        .await;
        t.resumes().await;
        assert_eq!(states(t.run(&venue(t.owner())).await), vec!["PREPARED".to_owned()]);
        assert_eq!(t.signatures(), 0);
        let writes: Vec<String> = t
            .calls()
            .await
            .into_iter()
            .filter(|call| call.starts_with("POST") || call.starts_with("DELETE"))
            .collect();
        assert_eq!(writes, Vec::<String>::new());
    }
}

/// "a take signed at once whose withdrawal is refused reports the refusal, on every run, and
/// signs nothing more".
#[tokio::test]
async fn a_refused_at_once_withdrawal_is_pending_on_every_run() {
    let t = Taker::new(at_once(Options { withdrawn: Some("refused"), ..Options::default() })).await;
    for _ in 0..2 {
        assert_eq!(pending(t.run(&venue(t.owner())).await), PendingReason::WithdrawalRefused);
    }
    assert_eq!(t.signatures(), 2);
    assert_eq!(t.count(&submissions()).await, 1);
}

/// "a take signed at once whose signature is refused releases its reservation, and one with too
/// little time left asks for nothing more".
#[tokio::test]
async fn a_failed_at_once_signature_releases_the_reservation() {
    let declined = Taker::new(at_once(Options::default())).await;
    declined.decline(PAY, 1);
    assert!(matches!(declined.run(&venue(declined.owner())).await, Err(Error::Signer(_))));
    assert_eq!(declined.count(&route("DELETE", &format!("/v1/takes/{TAKE}"))).await, 1);
    assert_eq!(declined.count(&submissions()).await, 0);
    // The wallet held the withdrawal's prompt until too little time was left for the payment.
    let slow = Taker::new(at_once(Options::default())).await;
    slow.harness.slow(SEND, 65);
    let outcome = slow.run(&venue(slow.owner())).await;
    assert_eq!(refused(outcome), (VerificationKind::Stale, details::SIGNING_WINDOW));
    assert_eq!(slow.kinds(), vec![SEND]);
    assert_eq!(slow.count(&route("DELETE", &format!("/v1/takes/{TAKE}"))).await, 1);
    assert!(!slow.holds(&format!("take:{TAKE}")).await);
    let short = Taker::new(at_once(Options { pay_window: Some(55), ..Options::default() })).await;
    let outcome = short.run(&venue(short.owner())).await;
    assert_eq!(refused(outcome), (VerificationKind::Stale, details::SIGNING_WINDOW));
    assert_eq!(short.signatures(), 0);
    assert_eq!(short.count(&route("DELETE", &format!("/v1/takes/{TAKE}"))).await, 1);
}

/// "a take signed at once whose withdrawal another run submitted meanwhile follows that
/// withdrawal and signs only its payment".
#[tokio::test]
async fn an_at_once_withdrawal_submitted_meanwhile_is_followed() {
    let t = Taker::new(at_once(Options {
        chain_balance: Some("1000000"),
        pay_window: Some(45),
        faults: vec![(
            "take",
            Fault::Raced(
                "the withdrawal was submitted meanwhile: take again",
                Some("withdrawal_submitted"),
            ),
        )],
        ..Options::default()
    }))
    .await;
    assert_eq!(states(t.run(&venue(t.owner())).await), paid());
    assert_eq!(t.kinds(), vec![PAY]);
    assert!(!t.calls().await.contains(&withdrawal_submission()));
    let other = Taker::new(at_once(Options {
        chain_balance: Some("1000000"),
        faults: vec![("take", Fault::Raced("another financial preparation is open", None))],
        ..Options::default()
    }))
    .await;
    let error = other.run(&venue(other.owner())).await.unwrap_err();
    assert_eq!(error.api().unwrap().message(), "another financial preparation is open");
}

/// The withdrawal signed with the take is checked first: a tampered one signs nothing and
/// reserves nothing.
#[tokio::test]
async fn a_tampered_at_once_withdrawal_signs_nothing() {
    let t =
        Taker::new(at_once(Options { prepared_amount: Some("0.7"), ..Options::default() })).await;
    let outcome = t.run(&venue(t.owner())).await;
    assert_eq!(refused(outcome).0, VerificationKind::Differs);
    assert_eq!(t.signatures(), 0);
    assert_eq!(t.count(&takes()).await, 0);
}

// Withdrawal, plan and permit checks.

/// A withdrawal of another amount than the plan's shortfall is never signed, saved or sent.
#[tokio::test]
async fn a_tampered_withdrawal_is_never_signed() {
    let t = Taker::new(Options {
        withdrawal_amount: Some("600000"),
        funds: short(),
        prepared_amount: Some("0.7"),
        ..Options::default()
    })
    .await;
    let outcome = t.run(&venue(t.owner())).await;
    assert_eq!(refused(outcome), (VerificationKind::Differs, details::AMOUNT));
    assert_eq!(t.signatures(), 0);
    assert_eq!(t.count(&withdrawal_submission()).await, 0);
    assert!(!t.holds(&format!("transfer-withdrawal:{T1}")).await);
}

/// A plan withdrawing elsewhere than to the signer, or a tampered permit, signs nothing.
#[tokio::test]
async fn a_tampered_plan_or_permit_signs_nothing() {
    let elsewhere = Taker::new(Options {
        withdrawal_amount: Some("600000"),
        funds: short(),
        answered: Some(json!({"funding_plan": {
            "observed_at": time(0), "venue": "hyperliquid", "owner": Address::ZERO,
            "amount": "1000000", "deposit": "0", "withdrawal_amount": "600000",
            "withdrawal_to": "escrow", "payment_setup": "done", "signed_at_once": false,
            "requests": [],
        }})),
        ..Options::default()
    })
    .await;
    let owner = elsewhere.owner();
    elsewhere.edit(|state| {
        state.o.answered.as_mut().unwrap()["funding_plan"]["owner"] = json!(owner);
    });
    let outcome = elsewhere.run(&venue(owner)).await;
    assert_eq!(refused(outcome), (VerificationKind::Differs, details::FUNDING_WITHDRAWAL));
    assert_eq!(elsewhere.signatures(), 0);
    assert_eq!(elsewhere.withdrawals().await, Vec::<String>::new());
    let permit = Taker::new(Options {
        setup: Some("required"),
        withdrawal_amount: Some("600000"),
        funds: short(),
        ..Options::default()
    })
    .await;
    // The permit's spender is another contract than the escrow.
    let owner = permit.owner();
    let mut tampered = sample("permit");
    tampered["message"]["owner"] = json!(owner);
    tampered["message"]["deadline"] = json!((NOW + 300).to_string());
    tampered["message"]["spender"] = json!(MAKER);
    Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/accounts/hyperliquid/setup/permit"))
        .respond_with(response(
            200,
            json!({
                "step": "permit", "status": "required", "reason": null, "expires_at": null,
                "needed_for": ["pay"], "hash": null,
                "to_sign": {"kind": "permit", "signer": owner, "ref": "permit-1",
                            "typed_data": tampered, "message": null, "expires_at": null},
            }),
        ))
        .mount(&permit.harness.server)
        .await;
    let outcome = permit.run(&venue(owner)).await;
    assert_eq!(refused(outcome), (VerificationKind::Differs, details::SPENDER));
    assert_eq!(permit.signatures(), 0);
    assert_eq!(permit.count("POST /v1/accounts/hyperliquid/setup/permit").await, 1);
}

// Wallet destinations and wallet funding.

/// "a transfer to a wallet asks for it, pays the address it names, and needs no account nor
/// deposit minimum where it ends".
#[tokio::test]
async fn a_transfer_to_a_wallet_pays_the_address_it_names() {
    let t = Taker::new(Options {
        wallet: Some(BYBIT),
        release_minimum: Some("5000000"),
        ..Options::default()
    })
    .await;
    assert_eq!(states(t.run(&to_wallet(Funding::Wallet)).await), paid());
    assert_eq!(t.kinds(), vec![PAY]);
    let created = &t.bodies(&route("POST", "/v1/transfers")).await[0];
    assert_eq!((&created["destination"], &created["recipient"]), (&json!("wallet"), &json!(BYBIT)));
    let taken = &t.bodies(&takes()).await[0];
    assert_eq!(taken["transfer_intent_id"], T1);
    assert_eq!((&taken["destination"], &taken["recipient"]), (&json!("wallet"), &json!(BYBIT)));
    // Into the venue, Polymarket's minimum deposit stops the same transfer.
    let into = Taker::new(Options { release_minimum: Some("5000000"), ..Options::default() }).await;
    assert!(matches!(into.run(&wallet(into.owner())).await, Err(Error::QuoteUnavailable(_))));
    // A request into the venue names no destination.
    let plain = Taker::new(Options::default()).await;
    assert_eq!(states(plain.run(&wallet(plain.owner())).await), paid());
    for route in [route("POST", "/v1/transfers"), takes()] {
        assert!(plain.bodies(&route).await[0].get("destination").is_none());
    }
    assert!(plain.bodies(&takes()).await[0].get("recipient").is_none());
}

/// "a transfer whose destination alone differs from the request is never paid, opened or
/// resumed".
#[tokio::test]
async fn a_transfer_whose_destination_differs_is_never_paid() {
    let into = Taker::new(Options {
        answered: Some(json!({"destination": "wallet"})),
        ..Options::default()
    })
    .await;
    let outcome = into.run(&wallet(into.owner())).await;
    assert_eq!(refused(outcome), (VerificationKind::Changed, details::DESTINATION));
    let out = Taker::new(Options {
        wallet: Some(BYBIT),
        answered: Some(json!({"destination": "venue"})),
        ..Options::default()
    })
    .await;
    let outcome = out.run(&to_wallet(Funding::Wallet)).await;
    assert_eq!(refused(outcome), (VerificationKind::Changed, details::DESTINATION));
    for t in [&into, &out] {
        assert_eq!(t.signatures(), 0);
        assert_eq!(t.count(&takes()).await, 0);
    }
    let resumed =
        Taker::new(Options { wallet: Some(BYBIT), wallet_funding: true, ..Options::default() })
            .await;
    resumed.resumes().await;
    let outcome = resumed.run(&wallet(resumed.owner())).await;
    assert_eq!(refused(outcome).0, VerificationKind::Changed);
    assert_eq!(resumed.calls().await, vec![route("GET", &format!("/v1/transfers/{T1}"))]);
}

/// "a transfer to a wallet stays one through its replacements and its resumption".
#[tokio::test]
async fn a_wallet_transfer_stays_one_through_replacements_and_resumption() {
    let expired = json!({"state": "EXPIRED", "maker_leg": "ABANDONED"});
    for (options, key) in [
        (Options { take: true, take_fill: Some(expired), ..Options::default() }, "retry"),
        (Options { stale_preparation: true, ..Options::default() }, "renew"),
    ] {
        let t = Taker::new(Options { wallet: Some(BYBIT), ..options }).await;
        t.resumes().await;
        assert_eq!(states(t.run(&to_wallet(Funding::Venue)).await), paid());
        let created = &t.bodies(&route("POST", "/v1/transfers")).await[0];
        assert_eq!(created["client_transfer_id"], format!("{key}:{T1}"));
        assert_eq!(
            (&created["destination"], &created["recipient"]),
            (&json!("wallet"), &json!(BYBIT))
        );
        let taken = &t.bodies(&takes()).await[0];
        assert_eq!(taken["transfer_intent_id"], T2);
        assert_eq!((&taken["destination"], &taken["recipient"]), (&json!("wallet"), &json!(BYBIT)));
    }
    let t = Taker::new(Options {
        wallet: Some(BYBIT),
        faults: vec![("submission", Fault::Unrecorded)],
        ..Options::default()
    })
    .await;
    assert!(t.run(&to_wallet(Funding::Wallet)).await.is_err());
    assert_eq!(states(t.run(&to_wallet(Funding::Wallet)).await), paid());
    assert_eq!(t.signatures(), 1);
}

/// "wallet funding pays without a source account, to a venue or a wallet, and never requests a
/// venue withdrawal".
#[tokio::test]
async fn wallet_funding_pays_without_a_source_account() {
    for to_a_wallet in [false, true] {
        let t = Taker::new(Options {
            unlinked_source: true,
            wallet: to_a_wallet.then_some(BYBIT),
            ..Options::default()
        })
        .await;
        let owner = t.owner();
        let request = if to_a_wallet { to_wallet(Funding::Wallet) } else { wallet(owner) };
        assert_eq!(states(t.run(&request).await), paid());
        let created = &t.bodies(&route("POST", "/v1/transfers")).await[0];
        assert_eq!((&created["funding"], &created["payer"]), (&json!("wallet"), &json!(owner)));
        assert_eq!(t.bodies(&takes()).await[0]["funding"], "wallet");
        let calls = t.calls().await;
        assert!(
            !calls.iter().any(|c| c.contains("/withdrawal") || c.contains("/accounts/")),
            "{calls:?}"
        );
    }
}

/// A wallet-funded transfer needing the escrow's permit signs it after a covering read, by
/// `authorize_wallet`, then pays.
#[tokio::test]
async fn a_wallet_funded_transfer_authorizes_the_wallet_first() {
    let t = Taker::new(Options { permit: true, ..Options::default() }).await;
    let owner = t.owner();
    let mut permit = sample("permit");
    permit["message"]["owner"] = json!(owner);
    permit["message"]["deadline"] = json!((NOW + 300).to_string());
    t.edit(|state| state.serve(&permit, "Permit"));
    let wallet_permit = json!({
        "id": "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a30",
        "to_sign": {"kind": "permit", "signer": owner, "ref": "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a30",
                    "typed_data": permit, "message": null, "expires_at": null},
    });
    Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/wallets/hyperliquid/permit"))
        .respond_with(move |request: &Request| {
            let body: Value = request.body_json().unwrap();
            let submitted = body.get("signature").is_some();
            response(
                200,
                json!({
                    "address": owner, "authorized": false, "pending": submitted, "hash": null,
                    "permit": if submitted { Value::Null } else { wallet_permit.clone() },
                }),
            )
        })
        .mount(&t.harness.server)
        .await;
    let outcome = t.run(&wallet(owner)).await;
    assert_eq!(states(outcome), paid());
    assert_eq!(t.kinds(), vec!["Permit", PAY]);
}

// Store failures.

/// A take signature that cannot be saved is never submitted (`submitted == false`); a pointer
/// that cannot be saved after the transfer exists says it reached the server.
#[tokio::test]
async fn a_signature_that_cannot_be_saved_is_never_submitted() {
    let t = Taker::with(Options::default(), Journal::new(FlakyStore::failing_save(2))).await;
    let Err(Error::Store(error)) = t.run(&wallet(t.owner())).await else {
        panic!("the take record's save fails");
    };
    assert!(!error.submitted());
    assert_eq!(t.count(&submissions()).await, 0);
    let t = Taker::with(Options::default(), Journal::new(FlakyStore::failing_save(1))).await;
    let Err(Error::Store(error)) = t.run(&wallet(t.owner())).await else {
        panic!("the pointer's save fails");
    };
    assert!(error.submitted());
    assert_eq!(t.count(&takes()).await, 0);
    // The same request finds the same transfer.
    assert_eq!(states(t.run(&wallet(t.owner())).await), paid());
    let ids: Vec<Value> = t
        .bodies(&route("POST", "/v1/transfers"))
        .await
        .into_iter()
        .map(|body| body["client_transfer_id"].clone())
        .collect();
    assert_eq!(ids, vec![json!(CLIENT_ID); 2]);
}

/// A withdrawal signature that cannot be saved is never sent.
#[tokio::test]
async fn a_withdrawal_that_cannot_be_saved_is_never_sent() {
    let t = Taker::with(
        Options { withdrawal_amount: Some("600000"), funds: short(), ..Options::default() },
        Journal::new(FlakyStore::failing_save(2)),
    )
    .await;
    let Err(Error::Store(error)) = t.run(&venue(t.owner())).await else {
        panic!("the withdrawal record's save fails");
    };
    assert!(!error.submitted());
    assert_eq!(t.count(&withdrawal_submission()).await, 0);
}

/// Callers persist requests to resume them: this JSON is a public format, changed only with a
/// breaking release.
#[test]
fn a_persisted_request_keeps_its_json() {
    let terms = TradeTerms::new(
        "hyperliquid".parse().unwrap(),
        "polymarket".parse().unwrap(),
        Amount::new(U256::from(250_000_000_u64)),
        Amount::new(U256::from(249_500_000_u64)),
        Recipient::Account(Address::repeat_byte(0x11)),
    )
    .max_fee_bps(Bps::new(5).unwrap())
    .funding(Funding::Wallet);
    let request = TransferRequest::new("rebalance-1".parse().unwrap(), terms);
    let pinned = json!({
        "id": "rebalance-1",
        "terms": {
            "from": "hyperliquid",
            "to": "polymarket",
            "amount_in": "250000000",
            "min_out": "249500000",
            "recipient": {"account": "0x1111111111111111111111111111111111111111"},
            "max_parts": 8,
            "max_fee_bps": 5,
            "funding": "wallet",
        },
        "poll_interval": {"secs": 2, "nanos": 0},
        "arrival_timeout": {"secs": 600, "nanos": 0},
    });

    assert_eq!(serde_json::to_value(&request).unwrap(), pinned);
    assert_eq!(serde_json::from_value::<TransferRequest>(pinned).unwrap(), request);
}
