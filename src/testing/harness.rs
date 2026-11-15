//! Flow test harness: a wiremock API serving production's venues and SIWE, a movable test clock,
//! a signer that counts and records its signatures, and a store whose writes fail on demand.

use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    },
};

use alloy_dyn_abi::TypedData;
use alloy_primitives::{Address, B256, ChainId, Signature};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path, query_param},
};

use super::{NOW, OWNER, json::fixture, stamp};
use crate::{
    Client, Environment, RetryPolicy, Session, deployments,
    store::{Journal, MemoryStore, Store, StoreFuture},
    types::{EscrowEnvironment, VenueId},
};

/// What every token the harness issues contains, to search logs for.
pub(crate) const TOKEN_SENTINEL: &str = "SENTINEL-SESSION-TOKEN";

/// The token of the `n`-th sign-in (from 1).
pub(crate) fn token(n: usize) -> String {
    format!("tok{n}-{TOKEN_SENTINEL}")
}

/// The session id of the `n`-th sign-in (from 1).
pub(crate) fn session_id(n: usize) -> String {
    format!("0192f0a1-0000-7000-8000-{n:012}")
}

/// A `200` answer of `body`.
pub(crate) fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

/// The API's refusal `code` with `status`.
pub(crate) fn api_error(status: u16, code: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({"error": {
        "code": code, "message": code, "retryable": status >= 500,
    }}))
}

/// Answers matching requests with `answers` in turn, repeating the last.
pub(crate) fn script(answers: Vec<ResponseTemplate>) -> impl Fn(&Request) -> ResponseTemplate {
    let answers = Mutex::new(VecDeque::from(answers));
    move |_: &Request| {
        let mut answers = answers.lock().unwrap();
        if answers.len() > 1 { answers.pop_front().unwrap() } else { answers[0].clone() }
    }
}

/// `owner`'s balance row on `venue`, with its venue wallet (Lighter's account index, a
/// Polymarket wallet).
pub(crate) fn balance(venue: &str, owner: Address, venue_wallet: Option<&str>) -> Value {
    let mut row = fixture("balance");
    row["venue"] = json!(venue);
    row["address"] = json!(owner);
    row["venue_wallet"] = json!(venue_wallet);
    row
}

/// The EIP-712 digest of `typed` (without a Polymarket operation's `op`).
pub(crate) fn digest(typed: &Value) -> B256 {
    let mut typed = typed.clone();
    if let Some(fields) = typed.as_object_mut() {
        fields.remove("op");
    }
    serde_json::from_value::<TypedData>(typed).unwrap().eip712_signing_hash().unwrap()
}

/// Who signed `digest` with the signature `item` carries.
pub(crate) fn signer_of(item: &Value, digest: &B256) -> Address {
    let signature: Signature = item["signature"].as_str().unwrap().parse().unwrap();
    signature.recover_address_from_prehash(digest).unwrap()
}

/// A signer that counts its signatures, records the kind of each payload the mock served
/// ([`Harness::serve`]), and declines or slows a kind on request.
#[derive(Clone)]
pub(crate) struct TestSigner {
    key: PrivateKeySigner,
    calls: Arc<AtomicUsize>,
    served: Arc<Mutex<HashMap<B256, &'static str>>>,
    signed: Arc<Mutex<Vec<&'static str>>>,
    /// Kinds declined, with how many more times.
    declined: Arc<Mutex<HashMap<&'static str, usize>>>,
    /// A kind after whose signature the clock moves by the seconds given, as a slow wallet's.
    slow: Arc<Mutex<Option<(&'static str, i64)>>>,
    clock: Arc<AtomicI64>,
}

impl TestSigner {
    /// Records that `typed` stands for `kind`: the signer reports and declines by kind.
    pub(crate) fn serve(&self, typed: &Value, kind: &'static str) {
        self.served.lock().unwrap().insert(digest(typed), kind);
    }
}

impl Signer for TestSigner {
    fn sign_hash<'life0, 'life1, 'async_trait>(
        &'life0 self,
        hash: &'life1 B256,
    ) -> Pin<Box<dyn Future<Output = alloy_signer::Result<Signature>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let kind = self.served.lock().unwrap().get(hash).copied();
            if let Some(kind) = kind
                && let Some(left) =
                    self.declined.lock().unwrap().get_mut(kind).filter(|left| **left > 0)
            {
                *left -= 1;
                return Err(alloy_signer::Error::other("User rejected the request"));
            }
            // A test signer: the key it wraps signs; the SDK reaches it only through
            // `SigningKey`.
            let signature = Signer::sign_hash(&self.key, hash).await?;
            if let Some(kind) = kind {
                self.signed.lock().unwrap().push(kind);
                if let Some((slow, seconds)) = *self.slow.lock().unwrap()
                    && slow == kind
                {
                    self.clock.fetch_add(seconds, Ordering::SeqCst);
                }
            }
            Ok(signature)
        })
    }

    fn address(&self) -> Address {
        self.key.address()
    }

    fn chain_id(&self) -> Option<ChainId> {
        None
    }

    fn set_chain_id(&mut self, _: Option<ChainId>) {}
}

/// A mock API, a client of it on a movable test clock, and a [`TestSigner`].
pub(crate) struct Harness {
    pub(crate) server: MockServer,
    pub(crate) client: Client,
    clock: Arc<AtomicI64>,
    signer: TestSigner,
    /// Sign-in verifications received: each carries one signature.
    verifications: Arc<AtomicUsize>,
    /// Sign-in texts issued, by nonce.
    issued: Arc<Mutex<HashMap<String, String>>>,
    /// Sessions issued, also across mocks served again.
    sessions: Arc<AtomicUsize>,
}

impl Harness {
    /// Serves production's venues and SIWE.
    pub(crate) async fn new() -> Self {
        let harness = Self::bare().await;
        harness.serve_venues(venues(EscrowEnvironment::Production)).await;
        harness.serve_sign_in().await;
        harness
    }

    /// Serves nothing yet.
    pub(crate) async fn bare() -> Self {
        let server = MockServer::start().await;
        let clock = Arc::new(AtomicI64::new(NOW));
        let signer = TestSigner {
            key: PrivateKeySigner::random(),
            calls: Arc::default(),
            served: Arc::default(),
            signed: Arc::default(),
            declined: Arc::default(),
            slow: Arc::default(),
            clock: Arc::clone(&clock),
        };
        let client = client(&server, &clock, RetryPolicy::none());
        Self {
            server,
            client,
            clock,
            signer,
            verifications: Arc::default(),
            issued: Arc::default(),
            sessions: Arc::default(),
        }
    }

    /// Another client of the mock API on the same clock, retrying under `retry`.
    pub(crate) fn client_with(&self, retry: RetryPolicy) -> Client {
        client(&self.server, &self.clock, retry)
    }

    /// The signer (clones share its state).
    pub(crate) fn signer(&self) -> TestSigner {
        self.signer.clone()
    }

    /// The signer's address.
    pub(crate) fn address(&self) -> Address {
        self.signer.address()
    }

    /// Signer calls so far, sign-ins included.
    pub(crate) fn signatures(&self) -> usize {
        self.signer.calls.load(Ordering::SeqCst)
    }

    /// Signer calls beyond the sign-ins'.
    pub(crate) fn flow_signatures(&self) -> usize {
        self.signatures() - self.verifications.load(Ordering::SeqCst)
    }

    /// What the signer signed of what the mock served, by kind, in order.
    pub(crate) fn kinds(&self) -> Vec<&'static str> {
        self.signer.signed.lock().unwrap().clone()
    }

    /// The signer declines the next `times` payloads of `kind`.
    pub(crate) fn decline(&self, kind: &'static str, times: usize) {
        self.signer.declined.lock().unwrap().insert(kind, times);
    }

    /// The clock moves `seconds` after each signature of `kind`.
    pub(crate) fn slow(&self, kind: &'static str, seconds: i64) {
        *self.signer.slow.lock().unwrap() = Some((kind, seconds));
    }

    /// Moves the test clock by `seconds`.
    pub(crate) fn later(&self, seconds: i64) {
        self.clock.fetch_add(seconds, Ordering::SeqCst);
    }

    /// The test clock now.
    pub(crate) fn now(&self) -> DateTime<Utc> {
        at(&self.clock)
    }

    /// The test clock, for a mock that stamps its answers.
    pub(crate) fn clock(&self) -> Arc<AtomicI64> {
        Arc::clone(&self.clock)
    }

    /// A session signed in with a fresh memory journal.
    pub(crate) async fn session(&self) -> Session {
        self.session_with(Journal::new(MemoryStore::new())).await
    }

    /// A session signed in with `journal`.
    pub(crate) async fn session_with(&self, journal: Journal) -> Session {
        self.client.sign_in(self.signer(), journal).await.unwrap()
    }

    /// `value` with the fixtures' [`OWNER`] replaced by the signer's address.
    pub(crate) fn owned(&self, value: &Value) -> Value {
        let address = self.address();
        let text = value
            .to_string()
            .replace(&OWNER.to_checksum(None), &address.to_checksum(None))
            .replace(&format!("{OWNER:#x}"), &format!("{address:#x}"));
        serde_json::from_str(&text).unwrap()
    }

    /// Answers `verb` `route` with `answers` in turn, repeating the last.
    pub(crate) async fn mount(&self, verb: &str, route: &str, answers: Vec<ResponseTemplate>) {
        Mock::given(method(verb))
            .and(path(route))
            .respond_with(script(answers))
            .mount(&self.server)
            .await;
    }

    /// Lighter's public API naming `owner` for account `index`.
    pub(crate) async fn lighter_owner(&self, index: u64, owner: Address) {
        Mock::given(method("GET"))
            .and(path("/api/v1/account"))
            .and(query_param("by", "index"))
            .and(query_param("value", index.to_string()))
            .respond_with(ok(json!({"accounts": [{"index": index, "l1_address": owner}]})))
            .mount(&self.server)
            .await;
    }

    /// Requests the server received for `verb` `route`.
    pub(crate) async fn requests(&self, verb: &str, route: &str) -> Vec<Request> {
        let received = self.server.received_requests().await.unwrap();
        received
            .into_iter()
            .filter(|r| r.method.as_str() == verb && r.url.path() == route)
            .collect()
    }

    /// The JSON bodies of the requests the server received for `verb` `route`.
    pub(crate) async fn bodies(&self, verb: &str, route: &str) -> Vec<Value> {
        self.requests(verb, route).await.iter().map(|r| r.body_json().unwrap()).collect()
    }

    /// Whether the server received only sign-in and deployment requests.
    pub(crate) async fn received_only_auth(&self) -> bool {
        let received = self.server.received_requests().await.unwrap();
        received
            .iter()
            .all(|r| r.url.path().starts_with("/v1/auth") || r.url.path() == "/v1/venues")
    }

    /// Answers `GET /v1/venues` with `venues`.
    pub(crate) async fn serve_venues(&self, venues: Value) {
        Mock::given(method("GET"))
            .and(path("/v1/venues"))
            .respond_with(ok(venues))
            .mount(&self.server)
            .await;
    }

    /// Answers sign-in challenges with the server's EIP-4361 text for this app origin, and
    /// verifications whose signature recovers to the address over the text issued for the nonce
    /// with the `n`-th session ([`token`], [`session_id`], expiring in a day).
    pub(crate) async fn serve_sign_in(&self) {
        let origin = self.server.uri();
        self.serve_challenges(move |address, nonce, now| challenge(&origin, address, nonce, now))
            .await;
        let sessions = Arc::clone(&self.sessions);
        let clock = Arc::clone(&self.clock);
        let issued = Arc::clone(&self.issued);
        let verifications = Arc::clone(&self.verifications);
        Mock::given(method("POST"))
            .and(path("/v1/auth/verify"))
            .respond_with(move |request: &Request| {
                verifications.fetch_add(1, Ordering::SeqCst);
                let body: Value = request.body_json().unwrap();
                let address: Address = body["address"].as_str().unwrap().parse().unwrap();
                let message = issued.lock().unwrap().remove(body["nonce"].as_str().unwrap());
                let signature: Signature = body["signature"].as_str().unwrap().parse().unwrap();
                let signed_by = |message: &str| signature.recover_address_from_msg(message).ok();
                if message.is_none_or(|message| signed_by(&message) != Some(address)) {
                    return ResponseTemplate::new(400).set_body_json(json!({"error": {
                        "code": "BAD_SIGNATURE", "message": "bad signature", "retryable": false,
                    }}));
                }
                let n = sessions.fetch_add(1, Ordering::SeqCst) + 1;
                ok(json!({
                    "token": token(n),
                    "session_id": session_id(n),
                    "expires_at": stamp(at(&clock) + chrono::Duration::days(1)),
                }))
            })
            .mount(&self.server)
            .await;
    }

    /// Answers sign-in challenges with `make(address, nonce, now)`; nonces count from 1.
    pub(crate) async fn serve_challenges(
        &self,
        make: impl Fn(Address, &str, DateTime<Utc>) -> Value + Send + Sync + 'static,
    ) {
        let asked = AtomicUsize::new(0);
        let clock = Arc::clone(&self.clock);
        let issued = Arc::clone(&self.issued);
        Mock::given(method("POST"))
            .and(path("/v1/auth/challenge"))
            .respond_with(move |request: &Request| {
                let body: Value = request.body_json().unwrap();
                let address: Address = body["address"].as_str().unwrap().parse().unwrap();
                let nonce = format!("nonce{}", asked.fetch_add(1, Ordering::SeqCst) + 1);
                let challenge = make(address, &nonce, at(&clock));
                if let Some(message) = challenge["message"].as_str() {
                    issued.lock().unwrap().insert(nonce, message.to_owned());
                }
                ok(challenge)
            })
            .mount(&self.server)
            .await;
    }
}

fn at(clock: &AtomicI64) -> DateTime<Utc> {
    DateTime::from_timestamp(clock.load(Ordering::SeqCst), 0).unwrap()
}

fn client(server: &MockServer, clock: &Arc<AtomicI64>, retry: RetryPolicy) -> Client {
    let uri = server.uri();
    let clock = Arc::clone(clock);
    Client::builder(Environment::Custom { api_url: uri.clone(), app_origin: uri.clone() })
        .retry(retry)
        .clock(Arc::new(move || at(&clock)))
        .lighter_api(uri)
        .build()
        .unwrap()
}

/// The server's sign-in challenge for `origin`, `address` and `nonce`, issued `now`, valid ten
/// minutes.
pub(crate) fn challenge(origin: &str, address: Address, nonce: &str, now: DateTime<Utc>) -> Value {
    let host = origin.split_once("://").unwrap().1;
    let expires = now + chrono::Duration::minutes(10);
    let message = format!(
        "{host} wants you to sign in with your Ethereum account:\n{address}\n\n\
         Sign in to LaMarge.\n\nURI: {origin}\nVersion: 1\nChain ID: 1\nNonce: {nonce}\n\
         Issued At: {}\nExpiration Time: {}",
        stamp(now),
        stamp(expires)
    );
    json!({"message": message, "nonce": nonce, "expires_at": stamp(expires)})
}

/// Every venue this release pins, listed with `environment`'s escrows, all enabled; escrow
/// venues take wallet funding in their token.
pub(crate) fn venues(environment: EscrowEnvironment) -> Value {
    let rows: Vec<Value> = [
        VenueId::Hyperliquid,
        VenueId::Polymarket,
        VenueId::Gmx,
        VenueId::Lighter,
        VenueId::Aster,
    ]
    .iter()
    .map(|venue| {
        let chain = deployments::chain(venue).unwrap();
        let escrow = deployments::contract(venue, environment).unwrap();
        let settled = deployments::is_venue_settled(venue);
        let wallet_funding = (!settled).then(|| {
            json!({"token": chain.token, "symbol": "USDC", "decimals": chain.decimals})
        });
        json!({
            "id": venue.as_str(), "name": venue.as_str(), "enabled": true,
            "settlement": if settled { "venue" } else { "escrow" },
            "chain": {
                "wallet": chain.wallet_token.map(|token| json!({
                    "token": token, "symbol": "USDC", "decimals": chain.decimals, "minimum": "10000",
                })),
                "id": chain.id, "escrow": escrow, "token": chain.token, "symbol": "USDC",
                "decimals": chain.decimals,
                "permit": chain.permit.map(|p| json!({"name": p.name, "version": p.version})),
                "wallet_funding": wallet_funding,
            },
            "margin_move": null, "release_minimum": "5000000",
        })
    })
    .collect();
    Value::Array(rows)
}

/// A memory store whose `n`-th save of a key containing a needle, or `n`-th delete, fails (from
/// 1; 0 never).
#[derive(Default)]
pub(crate) struct FlakyStore {
    values: MemoryStore,
    saves: AtomicUsize,
    deletes: AtomicUsize,
    fail_save: (&'static str, usize),
    fail_delete: usize,
}

impl FlakyStore {
    /// Fails the `n`-th save.
    pub(crate) fn failing_save(n: usize) -> Self {
        Self::failing_save_of("", n)
    }

    /// Fails the `n`-th save of a key containing `needle`.
    pub(crate) fn failing_save_of(needle: &'static str, n: usize) -> Self {
        Self { fail_save: (needle, n), ..Self::default() }
    }

    /// Fails the `n`-th delete.
    pub(crate) fn failing_delete(n: usize) -> Self {
        Self { fail_delete: n, ..Self::default() }
    }
}

impl Store for FlakyStore {
    fn load<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<Vec<u8>>> {
        self.values.load(key)
    }

    fn save<'a>(&'a self, key: &'a str, value: &'a [u8]) -> StoreFuture<'a, ()> {
        let (needle, n) = self.fail_save;
        if key.contains(needle) && self.saves.fetch_add(1, Ordering::SeqCst) + 1 == n {
            return Box::pin(std::future::ready(Err(io::Error::other("disk full"))));
        }
        self.values.save(key, value)
    }

    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        if self.deletes.fetch_add(1, Ordering::SeqCst) + 1 == self.fail_delete {
            return Box::pin(std::future::ready(Err(io::Error::other("disk gone"))));
        }
        self.values.delete(key)
    }
}
