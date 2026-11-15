//! Bridges of a move: the order checked against the caller's own terms, then the `HyperCore`
//! payloads signed or the wallet transactions returned. The crate sends no transaction and runs
//! no move: the caller keeps the move's budget (see [`Session::sign_bridge`]).

use alloy_primitives::{Address, Bytes};
use serde::{Deserialize, Serialize};
use tracing::Instrument as _;

use super::lighter_owner;
use crate::{
    Error, Result, Session,
    api::{MoveRecipient, MoveRequest},
    error::{ApiError, Reason},
    model::{Bridge, BridgeFunding, MoveEndpoint},
    protocol::{
        checks::details::{self, changed, differs},
        relay::{self, Checked, Expectation, Recipient},
    },
    transport::{Call, ops},
    types::{Amount, SignatureBytes},
};

/// What a bridge must do, from the caller's own records, never from the API: its order must
/// match exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeTerms {
    from: MoveEndpoint,
    to: MoveEndpoint,
    payer: Address,
    recipient: MoveRecipient,
    amount_in: Amount,
    min_out: Amount,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_credit: Option<Amount>,
}

impl BridgeTerms {
    /// A bridge of `amount_in` (origin base units) for the move `request`, as the caller
    /// created it: its floor is its share of the move's `min_out`, rounded up; a Polymarket, GMX
    /// or Aster account receives exactly the floor, at least the venue's minimum (Polymarket:
    /// 5 pUSD). `recipient` is the move's.
    pub fn for_move(
        request: &MoveRequest,
        recipient: MoveRecipient,
        amount_in: Amount,
    ) -> Result<Self> {
        if recipient.wire() != request.recipient {
            return Err(Error::InvalidRequest("recipient is not the move's".into()));
        }
        let invalid = || Error::InvalidRequest("invalid bridge amount".into());
        if amount_in.get().is_zero() {
            return Err(invalid());
        }
        let floor =
            relay::part_floor(amount_in.get(), request.min_out.get(), request.amount_in.get())
                .ok_or_else(invalid)?;
        let to = request.to.clone();
        let credit = relay::is_call_venue(&to.venue)
            .then(|| Amount::new(floor.max(relay::account_minimum(&to.venue))));
        Ok(Self {
            from: request.from.clone(),
            to,
            payer: request.payer,
            recipient,
            amount_in,
            min_out: Amount::new(floor),
            account_credit: credit,
        })
    }

    /// From the payer's `token` wallet on `chain_id` instead of the move's origin.
    #[must_use]
    pub fn from_wallet(mut self, chain_id: u64, token: Address) -> Self {
        self.from = MoveEndpoint::wallet(chain_id, token);
        self
    }

    fn expectation(&self) -> Expectation<'_> {
        Expectation {
            from: &self.from,
            to: &self.to,
            payer: self.payer,
            recipient: match self.recipient {
                MoveRecipient::Address(address) => Recipient::Address(address),
                MoveRecipient::LighterAccount { index, .. } => Recipient::Lighter(index),
            },
            amount_in: self.amount_in.get(),
            min_out: self.min_out.get(),
            credit: self.account_credit.map(Amount::get),
        }
    }
}

/// A transaction to send: `data` to `to`, value zero.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ContractCall {
    /// The contract.
    pub to: Address,
    /// The calldata.
    pub data: Bytes,
}

/// A wallet-origin bridge's checked transactions, from the payer's wallet.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WalletDeposit {
    /// The origin chain.
    pub chain_id: u64,
    /// The token deposited.
    pub token: Address,
    /// To send first, for exactly `amount_in`: the allowance toward the depository is below it,
    /// or unknown.
    pub approve: Option<ContractCall>,
    /// The deposit.
    pub deposit: ContractCall,
    /// The payer's funds as the server read them; `None` when it cannot read the origin chain
    /// (BSC): the balance is then unchecked.
    pub funding: Option<BridgeFunding>,
}

#[derive(Serialize)]
struct SignaturesBody {
    authorization: SignatureBytes,
    send_asset: SignatureBytes,
}

impl Session {
    /// Signs a `HyperCore`-origin bridge: checks its order against `terms`, signs Relay's nonce
    /// mapping and the `SendAsset` to Relay's pinned depository with the session's key (the
    /// payer), and submits both. Signing again signs the same payloads: their nonce makes one
    /// send at most.
    ///
    /// The crate runs no move: before each bridge, the caller checks that the move's parts not
    /// final without a debit (bridges not `failed` or `expired`, transfers at their take's
    /// `amount_in`, withdrawals feeding the bridge) stay within the move's `amount_in`, from its
    /// own records, and that the part's floor is at least its share of the move's `min_out`.
    ///
    /// # Errors
    ///
    /// `Verification(..)` naming the field when the order differs from `terms` or the pins, or
    /// when a Lighter recipient's owner on Lighter's API is not the one `terms` names; nothing is
    /// signed.
    pub async fn sign_bridge(&self, bridge: &Bridge, terms: &BridgeTerms) -> Result<Bridge> {
        let span = tracing::info_span!("lamarge.bridge", bridge = %bridge.id);
        async {
            let checked = check(self, bridge, terms).await?;
            let nonce = bridge.nonce.ok_or(differs(details::NONCE))?.get();
            let now_ms = self.transport().now().timestamp_millis();
            let nonce = relay::check_bridge_nonce(nonce, now_ms)?;
            let [authorization, send] =
                relay::hypercore_payloads(checked, &terms.expectation(), nonce)?;
            let body = SignaturesBody {
                authorization: self.sign(&authorization).await?,
                send_asset: self.sign(&send).await?,
            };
            let call = Call::new(&ops::SIGN_BRIDGE).segment(bridge.id).json(&body)?;
            self.transport().send(call, self.auth()).await
        }
        .instrument(span)
        .await
    }

    /// A wallet-origin bridge's transactions, after its order is checked against `terms` and
    /// the payer's funds are read ([`Bridges::funding`](crate::api::Bridges::funding), for the
    /// payer, the origin token and Relay's pinned depository): the `approve` only when the
    /// allowance is below `amount_in`, then the deposit. Where the server has no reader for the
    /// origin chain (BSC, `chain_unread`), the `approve` always comes and the balance is unchecked;
    /// any other failure of the read is returned, to retry later. Send them yourself, fencing first with
    /// [`Bridges::fence`](crate::api::Bridges::fence) and reporting each broadcast hash. The
    /// caller keeps the move's budget as for [`sign_bridge`](Self::sign_bridge).
    ///
    /// # Errors
    ///
    /// As [`sign_bridge`](Self::sign_bridge); `Verification(Unsupported)` for a `HyperCore`
    /// origin; `Verification(Changed)` while the balance is below `amount_in` (run it again once
    /// the funds arrived).
    pub async fn bridge_deposit(
        &self,
        bridge: &Bridge,
        terms: &BridgeTerms,
    ) -> Result<WalletDeposit> {
        let checked = check(self, bridge, terms).await?;
        let origin = relay::relay_endpoint(&terms.from, true)?;
        let deposit =
            relay::bridge_transactions(checked.id, origin, terms.payer, terms.amount_in.get())
                .ok_or(details::unsupported(details::BRIDGE_ORIGIN))?;
        let funding = match self.bridges().funding(bridge.id).await {
            Ok(funds) => Some(funds),
            Err(error) if error.api().and_then(ApiError::reason) == Some(&Reason::ChainUnread) => {
                None
            }
            Err(error) => return Err(error),
        };
        if let Some(funds) = &funding {
            if (funds.owner, funds.token, funds.spender)
                != (terms.payer, deposit.token, relay::DEPOSITORY)
            {
                return Err(differs(details::BRIDGE_FUNDING).into());
            }
            if funds.balance < terms.amount_in {
                return Err(changed(details::WALLET_FUNDS).into());
            }
        }
        let short = funding.as_ref().is_none_or(|funds| funds.allowance < terms.amount_in);
        Ok(WalletDeposit {
            chain_id: deposit.chain_id,
            token: deposit.token,
            approve: short.then(|| ContractCall { to: deposit.token, data: deposit.approve }),
            deposit: ContractCall { to: relay::DEPOSITORY, data: deposit.deposit },
            funding,
        })
    }
}

/// The bridge's order, checked against `terms` and its own id; a Lighter recipient's owner read
/// on Lighter's API.
async fn check(session: &Session, bridge: &Bridge, terms: &BridgeTerms) -> Result<Checked> {
    let now = session.transport().now().timestamp();
    let checked = relay::check_bridge_order(&bridge.order.data, &terms.expectation(), now)?;
    if checked.id != bridge.order.id {
        return Err(differs(details::ORDER_ID).into());
    }
    if let MoveRecipient::LighterAccount { index, owner } = terms.recipient
        && lighter_owner(session.transport(), index).await? != Some(owner)
    {
        return Err(changed(details::DESTINATION_OWNER).into());
    }
    Ok(checked)
}

#[cfg(test)]
mod tests;
