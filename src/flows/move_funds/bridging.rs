//! One bridge of a move, journalled: created or found again by its client id, its order checked
//! against the move's own terms, then signed (`HyperCore`) or sent through the host (a wallet),
//! each step journalled first. Stored signatures are submitted again, never signed anew; a
//! deposit of unknown outcome is never asked again.

use std::time::Duration;

use alloy_primitives::{Address, B256, U256};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use super::{
    MoveWait,
    budget::{self, Amend, Part, Source},
    same_endpoint, same_recipient,
};
use crate::{
    Error, Result, SendError, SendStatus, Session, Transaction, TransactionSender,
    api::{BridgeRequest, MoveRecipient, MoveRequest},
    error::{ErrorCode, Reason},
    flows::{BridgeTerms, lighter_owner, records, transfer::FRESH_SECONDS},
    model::{Bridge, BridgeState, BridgeUnavailable, MoveVenue},
    protocol::{
        checks::details::{self, changed, differs, stale, unsupported},
        relay::{self, Checked, Transactions},
    },
    transport::{Call, ops},
    types::{Amount, BridgeId, ClientId, HyperliquidNonce, MoveId, SignatureBytes},
};

/// A bridge's order is signed or sent only while its deadline is further away than this.
const SIGNING_MARGIN_SECONDS: i64 = 30;

/// One bridge of a move.
pub(super) struct BridgePart<'a> {
    pub(super) move_id: MoveId,
    pub(super) request: &'a MoveRequest,
    pub(super) recipient: MoveRecipient,
    pub(super) id: ClientId,
    pub(super) amount_in: Amount,
    /// The payer's wallet on the origin chain, instead of the origin.
    pub(super) from_wallet: Option<(u64, Address)>,
    pub(super) poll_interval: Duration,
    pub(super) arrival_timeout: Duration,
}

/// Where a bridge run stopped.
pub(super) enum Placed {
    /// As last read: final, prepared and past signing, or under way.
    Bridge(Box<Bridge>),
    /// Refused at creation for a reason no later run changes.
    Refused(String),
    /// Nothing failed: run again later.
    Waiting(MoveWait),
}

/// What the part was created for: a stored record answers only the same part.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Pinned {
    move_id: MoveId,
    client_move_id: ClientId,
    amount_in: Amount,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from_wallet: Option<(u64, Address)>,
}

/// The bridge first created for the part, and its order.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    bridge: BridgeId,
    order: B256,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    nonce: Option<HyperliquidNonce>,
}

/// A `HyperCore` bridge's signatures, each kept as soon as given.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Signed {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    authorization: Option<SignatureBytes>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    send_asset: Option<SignatureBytes>,
}

#[derive(Serialize)]
struct SignaturesBody {
    authorization: SignatureBytes,
    send_asset: SignatureBytes,
}

/// A transaction asked of the sender: saved before asking, with its hash once returned;
/// `unsent` once the sender said it sent nothing; for a deposit, `fenced` once this store holds
/// the bridge's fence, before the sender is asked.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
struct Asked {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hash: Option<B256>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    unsent: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    fenced: bool,
}

/// Final states: nothing more debits or credits.
pub(super) fn is_final(state: &BridgeState) -> bool {
    matches!(
        state,
        BridgeState::Completed
            | BridgeState::Refunded
            | BridgeState::Stuck
            | BridgeState::Failed
            | BridgeState::Expired
    )
}

/// Final with nothing debited.
pub(super) fn is_dead(state: &BridgeState) -> bool {
    matches!(state, BridgeState::Failed | BridgeState::Expired)
}

/// Prepared and still signable at `now`: its order's deadline more than 30 s away, as the server
/// sends a `HyperCore` one until then.
pub(super) fn in_time(bridge: &Bridge, now: i64) -> bool {
    let deadline =
        bridge.order.data.pointer("/output/deadline").and_then(serde_json::Value::as_i64);
    bridge.state == BridgeState::Prepared
        && deadline.is_some_and(|deadline| deadline > now + SIGNING_MARGIN_SECONDS)
}

/// Bridge refusals no later run changes: what they leave is unallocated.
pub(super) fn is_lasting(reason: &BridgeUnavailable) -> bool {
    matches!(
        reason,
        BridgeUnavailable::InsufficientLiquidity
            | BridgeUnavailable::BelowMinimum
            | BridgeUnavailable::Unsupported
            | BridgeUnavailable::Slippage
            | BridgeUnavailable::HyperliquidMode
            | BridgeUnavailable::Disabled
    )
}

/// A refusal the API answered: nothing was created.
pub(super) fn answered(error: &Error) -> bool {
    error.api().is_some_and(|api| !api.code().is_unknown())
}

/// A lasting refusal at creation, by its reason.
fn lasting(error: &Error) -> Option<String> {
    let api = error.api()?;
    let reason: BridgeUnavailable = match api.reason() {
        Some(reason) => {
            let Ok(reason) = reason.as_str().parse();
            reason
        }
        None if *api.code() == ErrorCode::Slippage => BridgeUnavailable::Slippage,
        None => return None,
    };
    is_lasting(&reason).then(|| reason.as_str().to_owned())
}

/// Runs the bridge `part`: see the module.
pub(super) async fn run(
    session: &Session,
    part: &BridgePart<'_>,
    sender: Option<&dyn TransactionSender>,
) -> Result<Placed> {
    let mut terms = BridgeTerms::for_move(part.request, part.recipient, part.amount_in)?;
    if let Some((chain_id, token)) = part.from_wallet {
        terms = terms.from_wallet(chain_id, token);
    }
    let journal = session.journal();
    let key =
        |record: &str| session.record_key(&format!("bridge:{}:{}{record}", part.move_id, part.id));
    let _turn = journal.lock(&key("")).await;
    let pinned = Pinned {
        move_id: part.move_id,
        client_move_id: part.request.client_move_id.clone(),
        amount_in: part.amount_in,
        from_wallet: part.from_wallet,
    };
    match records::load::<Pinned>(journal, &key(":request")).await? {
        Some(saved) if saved != pinned => {
            let message = format!("bridge {} names another bridge", part.id);
            return Err(Error::InvalidRequest(message.into()));
        }
        Some(_) => {}
        None => records::save(journal, &key(":request"), &pinned, false).await?,
    }
    let source = if part.from_wallet.is_some() { Source::Wallet } else { Source::Origin };
    let name = part.id.as_str();
    budget::reserve(session, part.request, Part::new(name, part.amount_in, source)).await?;

    let identity = records::load::<Identity>(journal, &key(":identity")).await?;
    let bridge = if let Some(identity) = &identity {
        session.bridges().get(identity.bridge).await?
    } else {
        let mut body = BridgeRequest::new(part.id.clone(), part.amount_in);
        if let Some((chain_id, token)) = part.from_wallet {
            body = body.from_wallet(chain_id, token);
        }
        match session.bridges().create(part.move_id, &body).await {
            Ok(bridge) => bridge,
            Err(error) => {
                if answered(&error) {
                    budget::amend(session, part.request, name, Amend::Forget).await?;
                }
                if let Some(reason) = lasting(&error) {
                    return Ok(Placed::Refused(reason));
                }
                if error.code() == Some(&ErrorCode::VenueUnavailable) {
                    tracing::info!(%error, "bridge unavailable");
                    return Ok(Placed::Waiting(MoveWait::Bridge));
                }
                return Err(error);
            }
        }
    };
    let run = Run { session, part, terms: &terms, sender, identity: key(":identity") };
    let checked = run.check(&bridge).await?;
    if is_final(&bridge.state) {
        return Ok(Placed::Bridge(Box::new(bridge)));
    }
    if terms.from.venue == MoveVenue::Hyperliquid {
        run.hypercore(bridge, checked, &key(":signatures")).await
    } else {
        run.wallet(bridge, checked, &key(":deposit"), &key(":approval")).await
    }
}

struct Run<'a> {
    session: &'a Session,
    part: &'a BridgePart<'a>,
    terms: &'a BridgeTerms,
    sender: Option<&'a dyn TransactionSender>,
    identity: String,
}

impl Run<'_> {
    /// The bridge is the part's: its terms, its order (at any time: a final one is read after
    /// its deadline) and the identity first journalled. A dead one frees its budget.
    async fn check(&self, bridge: &Bridge) -> Result<Checked> {
        let (part, terms) = (self.part, self.terms);
        let ok = bridge.move_id == part.move_id
            && bridge.client_bridge_id == part.id
            && bridge.amount_in == part.amount_in
            && same_endpoint(&bridge.from, &terms.from)
            && same_endpoint(&bridge.to, &terms.to)
            && bridge.payer == self.session.address()
            && same_recipient(&bridge.recipient, &part.request.recipient)
            && bridge.min_out >= terms.min_out;
        if !ok {
            return Err(changed(details::BRIDGE).into());
        }
        let checked = relay::check_bridge_order(&bridge.order.data, &terms.expectation(), 0)?;
        if checked.id != bridge.order.id {
            return Err(differs(details::ORDER_ID).into());
        }
        let identity = Identity { bridge: bridge.id, order: checked.id, nonce: bridge.nonce };
        let journal = self.session.journal();
        match records::load::<Identity>(journal, &self.identity).await? {
            Some(saved) if saved != identity => return Err(changed(details::BRIDGE).into()),
            Some(_) => {}
            None => records::save(journal, &self.identity, &identity, false).await?,
        }
        if is_dead(&bridge.state) {
            let name = part.id.as_str();
            budget::amend(self.session, part.request, name, Amend::Release).await?;
        }
        Ok(checked)
    }

    async fn read(&self, id: BridgeId) -> Result<Placed> {
        let bridge = self.session.bridges().get(id).await?;
        self.check(&bridge).await?;
        Ok(Placed::Bridge(Box::new(bridge)))
    }

    fn now(&self) -> i64 {
        self.session.transport().now().timestamp()
    }

    /// A Lighter recipient is still owned by the address the move names.
    async fn owned(&self) -> Result<()> {
        if let MoveRecipient::LighterAccount { index, owner } = self.terms.recipient
            && lighter_owner(self.session.transport(), index).await? != Some(owner)
        {
            return Err(changed(details::DESTINATION_OWNER).into());
        }
        Ok(())
    }

    /// Signs the nonce mapping and the send, each journalled once given, then submits both.
    async fn hypercore(&self, bridge: Bridge, checked: Checked, key: &str) -> Result<Placed> {
        let journal = self.session.journal();
        let mut signed = records::load::<Signed>(journal, key).await?.unwrap_or_default();
        if signed.send_asset.is_none() {
            if !in_time(&bridge, self.now()) {
                return Ok(Placed::Bridge(Box::new(bridge)));
            }
            self.owned().await?;
            let nonce = bridge.nonce.ok_or(differs(details::NONCE))?.get();
            let now_ms = self.session.transport().now().timestamp_millis();
            let nonce = relay::check_bridge_nonce(nonce, now_ms)?;
            let [authorization, send] = relay::hypercore_payloads(
                checked,
                &self.terms.expectation(),
                nonce,
                bridge.source_dex.as_deref(),
            )?;
            if signed.authorization.is_none() {
                signed.authorization = Some(self.session.sign(&authorization).await?);
                records::save(journal, key, &signed, false).await?;
            }
            signed.send_asset = Some(self.session.sign(&send).await?);
            records::save(journal, key, &signed, false).await?;
        }
        let (Some(authorization), Some(send_asset)) = (signed.authorization, signed.send_asset)
        else {
            return Err(changed(details::BRIDGE).into());
        };
        if bridge.state != BridgeState::Prepared {
            return Ok(Placed::Bridge(Box::new(bridge)));
        }
        let body = SignaturesBody { authorization, send_asset };
        let call = Call::new(&ops::SIGN_BRIDGE).segment(bridge.id).json(&body)?;
        // Once signed, the bridge may still debit: a failure leaves it pending, never refused.
        match self.session.transport().send::<Bridge>(call, self.session.auth()).await {
            Ok(signed) => {
                self.check(&signed).await?;
                Ok(Placed::Bridge(Box::new(signed)))
            }
            Err(error) => {
                tracing::warn!(%error, "bridge signatures not accepted yet");
                Ok(Placed::Waiting(MoveWait::Bridge))
            }
        }
    }

    /// Sends a wallet bridge's `approve` when the allowance is short, fences, then sends its
    /// deposit through the host and reports it.
    async fn wallet(
        &self,
        bridge: Bridge,
        checked: Checked,
        deposit_key: &str,
        approval_key: &str,
    ) -> Result<Placed> {
        let journal = self.session.journal();
        let deposit_id = format!("lamarge.bridge.{}.deposit", bridge.id);
        if let Some(mut asked) = records::load::<Asked>(journal, deposit_key).await? {
            // Journalled: the deposit is never asked again, only its outcome reported. Only a store
            // holding the fence asked the sender; one that lost it, or its answer, never says unsent.
            if asked.fenced && asked.hash.is_none() && !asked.unsent {
                match self.status(&deposit_id).await {
                    SendStatus::Sent(hash) => asked.hash = Some(hash),
                    SendStatus::NotSent => {
                        asked.unsent = true;
                        records::save(journal, deposit_key, &asked, true).await?;
                        return self.refused(&bridge, &asked).await;
                    }
                    _ => return Ok(Placed::Waiting(MoveWait::Bridge)),
                }
                records::save(journal, deposit_key, &asked, true).await?;
            }
            return self.report(&bridge, &asked).await;
        }
        if !in_time(&bridge, self.now()) {
            return Ok(Placed::Bridge(Box::new(bridge)));
        }
        let sender = self.sender.ok_or_else(|| {
            Error::InvalidRequest("a wallet bridge needs a transaction sender".into())
        })?;
        self.owned().await?;
        let amount = self.terms.amount_in.get();
        let origin = relay::relay_endpoint(&self.terms.from, true)?;
        let tx = relay::bridge_transactions(checked.id, origin, self.terms.payer, amount)
            .ok_or(unsupported(details::BRIDGE_ORIGIN))?;
        let allowance = match self.funds(bridge.id, &tx).await? {
            Ok(allowance) => allowance,
            Err(wait) => return Ok(Placed::Waiting(wait)),
        };
        let approved = records::load::<Asked>(journal, approval_key).await?;
        // Where the chain is unread (BSC), an approval sent once counts: the sender sends the
        // deposit after it from the same address.
        let short = match allowance {
            Some(allowance) => allowance < amount,
            None => approved.as_ref().is_none_or(|asked| asked.hash.is_none()),
        };
        if short
            && let Some(wait) = self.approve(sender, &bridge, &tx, approval_key, approved).await?
        {
            return Ok(Placed::Waiting(wait));
        }

        // Journalled, then fenced, before the prompt: only the first fence succeeds, and a lost
        // answer or a fence another store holds never prompts again.
        records::save(journal, deposit_key, &Asked::default(), false).await?;
        if let Err(error) = self.session.bridges().fence(bridge.id).await {
            tracing::warn!(%error, "bridge deposit not fenced");
            return Ok(Placed::Waiting(MoveWait::Bridge));
        }
        let fenced = Asked { fenced: true, ..Asked::default() };
        records::save(journal, deposit_key, &fenced, true).await?;
        let transaction = transaction(deposit_id, tx.chain_id, relay::DEPOSITORY, &tx.deposit);
        match sender.send(transaction).await {
            Ok(hash) => {
                let asked = Asked { hash: Some(hash), ..fenced };
                records::save(journal, deposit_key, &asked, true).await?;
                self.report(&bridge, &asked).await
            }
            Err(SendError::NotSent(error)) => {
                tracing::info!(%error, "bridge deposit not sent");
                let asked = Asked { unsent: true, ..fenced };
                records::save(journal, deposit_key, &asked, true).await?;
                self.refused(&bridge, &asked).await
            }
            Err(error) => {
                tracing::warn!(%error, "bridge deposit outcome unknown");
                Ok(Placed::Waiting(MoveWait::Bridge))
            }
        }
    }

    /// The sender sent no deposit: reported, which fails the bridge, and the run ends there, as
    /// the owner's refusal does; running again asks for a new bridge.
    async fn refused(&self, bridge: &Bridge, asked: &Asked) -> Result<Placed> {
        // As fenced: the bridge read before the fence is still `prepared`.
        let fenced = self.session.bridges().get(bridge.id).await?;
        self.check(&fenced).await?;
        match self.report(&fenced, asked).await? {
            Placed::Bridge(_) => Ok(Placed::Waiting(MoveWait::NotSent)),
            other => Ok(other),
        }
    }

    /// Reports the deposit's hash, or that none was sent, then reads the bridge.
    async fn report(&self, bridge: &Bridge, asked: &Asked) -> Result<Placed> {
        let bridges = self.session.bridges();
        let reported = match asked.hash {
            Some(hash) if !bridge.origin_txs.iter().any(|seen| seen.parse() == Ok(hash)) => {
                Some(bridges.report_transaction(bridge.id, hash).await)
            }
            None if asked.unsent
                && bridge.state == BridgeState::Submitting
                && bridge.origin_txs.is_empty() =>
            {
                Some(bridges.report_unsent(bridge.id).await)
            }
            _ => None,
        };
        if let Some(Err(error)) = reported {
            tracing::warn!(%error, "bridge deposit not reported yet");
            return Ok(Placed::Waiting(MoveWait::Bridge));
        }
        self.read(bridge.id).await
    }

    /// The allowance toward the depository, `None` where the server reads no such chain (BSC);
    /// a wait while the funds are short of the deposit, or the chain did not answer.
    async fn funds(
        &self,
        id: BridgeId,
        tx: &Transactions,
    ) -> Result<std::result::Result<Option<U256>, MoveWait>> {
        let funds = match self.session.bridges().funding(id).await {
            Ok(funds) => funds,
            Err(error) => {
                let reason = error.api().and_then(|api| api.reason());
                return match reason {
                    Some(Reason::ChainUnread) => Ok(Ok(None)),
                    Some(Reason::ChainUnavailable) => Ok(Err(MoveWait::Bridge)),
                    _ => Err(error),
                };
            }
        };
        if (funds.owner, funds.token, funds.spender)
            != (self.terms.payer, tx.token, relay::DEPOSITORY)
        {
            return Err(differs(details::BRIDGE_FUNDING).into());
        }
        let age = self.session.transport().now() - funds.observed_at;
        if age.num_seconds() >= FRESH_SECONDS {
            return Err(stale(details::BRIDGE_FUNDING).into());
        }
        if funds.balance < self.terms.amount_in {
            return Ok(Err(MoveWait::Withdrawal));
        }
        Ok(Ok(Some(funds.allowance.get())))
    }

    /// Asks for the `approve` once, then waits until the allowance shows it; `None` once it
    /// does (or the chain is unread). One asked of unknown outcome is never asked again.
    async fn approve(
        &self,
        sender: &dyn TransactionSender,
        bridge: &Bridge,
        tx: &Transactions,
        key: &str,
        approved: Option<Asked>,
    ) -> Result<Option<MoveWait>> {
        let journal = self.session.journal();
        let id = format!("lamarge.bridge.{}.approve", bridge.id);
        let mut asked = approved;
        if let Some(known) = &mut asked
            && known.hash.is_none()
        {
            match self.status(&id).await {
                SendStatus::Sent(hash) => {
                    known.hash = Some(hash);
                    records::save(journal, key, known, true).await?;
                }
                SendStatus::NotSent => asked = None,
                _ => return Ok(Some(MoveWait::Bridge)),
            }
        }
        if asked.is_none() {
            records::save(journal, key, &Asked::default(), false).await?;
            let transaction = transaction(id, tx.chain_id, tx.token, &tx.approve);
            match sender.send(transaction).await {
                Ok(hash) => {
                    let asked = Asked { hash: Some(hash), ..Asked::default() };
                    records::save(journal, key, &asked, true).await?;
                }
                Err(SendError::NotSent(error)) => {
                    tracing::info!(%error, "bridge approval not sent");
                    records::delete(journal, key, true).await?;
                    return Ok(Some(MoveWait::NotSent));
                }
                Err(error) => {
                    tracing::warn!(%error, "bridge approval outcome unknown");
                    return Ok(Some(MoveWait::Bridge));
                }
            }
        }
        let deadline = Instant::now() + self.part.arrival_timeout;
        loop {
            match self.funds(bridge.id, tx).await? {
                Ok(None) => return Ok(None),
                Ok(Some(allowance)) if allowance >= self.terms.amount_in.get() => return Ok(None),
                Err(wait) => return Ok(Some(wait)),
                Ok(Some(_)) if Instant::now() >= deadline => return Ok(Some(MoveWait::Bridge)),
                Ok(Some(_)) => tokio::time::sleep(self.part.poll_interval).await,
            }
        }
    }

    /// What the sender knows of `id`; without a sender or a lookup, nothing.
    async fn status(&self, id: &str) -> SendStatus {
        match self.sender {
            Some(sender) => sender.status(id).await.unwrap_or(SendStatus::Unknown),
            None => SendStatus::Unknown,
        }
    }
}

fn transaction(id: String, chain_id: u64, to: Address, data: &[u8]) -> Transaction {
    Transaction { id, chain_id, to, data: data.to_vec(), value: U256::ZERO }
}
