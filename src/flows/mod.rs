//! Multi-step operations run by a `Session`: takes, transfers, withdrawals, venue setup and maker
//! capital, with their request types. Each checks every server payload before signing. A signature
//! whose second copy could move funds or an allowance twice (takes, transfers, withdrawals, maker
//! capital, wallet permits) is journalled before it is submitted and reused for a matching
//! request. Venue setup steps (setup permits included), closes and quote publications are not
//! journalled and may sign again after an interruption: a second copy has no further effect.

use std::{future::Future, time::Duration};

use alloy_primitives::{Address, B256, keccak256};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::Instant;
use uuid::Uuid;

use crate::{
    Result, Session, deployments,
    model::{Balance, ToSignKind},
    protocol::{
        checks::details::{self, changed, unsupported},
        lighter,
        signing::Verified,
    },
    transport::Inner,
    types::{SignatureBytes, VenueId},
};

mod bridge;
mod maker;
mod quoter;
mod records;
mod setup;
mod take;
mod transfer;
mod withdraw;

pub use bridge::*;
pub use maker::*;
pub use quoter::*;
pub use take::*;
pub use transfer::*;
pub use withdraw::*;

/// The wait between reads of something the server is still doing.
const POLL: Duration = if cfg!(test) { Duration::from_millis(1) } else { Duration::from_secs(2) };

/// One signature as the signature endpoints take it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SignedItem {
    kind: ToSignKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fill_id: Option<Uuid>,
    signature: SignatureBytes,
}

/// A signature endpoint's body.
#[derive(Serialize)]
struct Signatures<'a> {
    signatures: &'a [SignedItem],
}

/// Signatures saved before they are submitted, with the digests of the requests they sign and of
/// the context they were checked in: submitted again only for the same requests and context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SignedRecord {
    digests: Vec<B256>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context: Option<B256>,
    items: Vec<SignedItem>,
}

impl SignedRecord {
    /// The saved signatures, if they sign `digests` in `context`; another record is a change.
    fn reuse(self, digests: &[B256], context: Option<B256>) -> Result<Vec<SignedItem>> {
        if self.digests == digests && self.context == context {
            tracing::info!("submitting the saved signatures");
            Ok(self.items)
        } else {
            Err(changed(details::DIGEST).into())
        }
    }
}

impl Session {
    /// Signs `verified` with the session's key at the client's clock.
    async fn sign(&self, verified: &Verified) -> Result<SignatureBytes> {
        self.key().sign(verified, &self.transport().clock).await
    }
}

/// What identifies each signing request: the keccak-256 of its canonical JSON, without LaMarge's
/// attestation.
fn request_digests<T: Serialize>(to_sign: &[T]) -> Vec<B256> {
    to_sign
        .iter()
        .map(|item| {
            let mut value = serde_json::to_value(item).unwrap_or(Value::Null);
            if let Value::Object(fields) = &mut value {
                fields.remove("attestation");
            }
            canonical_digest(&value)
        })
        .collect()
}

/// The keccak-256 of `value`'s canonical JSON (keys sorted).
fn canonical_digest(value: &Value) -> B256 {
    let mut bytes = Vec::new();
    canonical(value, &mut bytes);
    keccak256(bytes)
}

fn canonical(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Object(fields) => {
            let mut names: Vec<&String> = fields.keys().collect();
            names.sort();
            out.push(b'{');
            for (n, name) in names.into_iter().enumerate() {
                if n > 0 {
                    out.push(b',');
                }
                canonical(&Value::String(name.clone()), out);
                out.push(b':');
                canonical(&fields[name], out);
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (n, item) in items.iter().enumerate() {
                if n > 0 {
                    out.push(b',');
                }
                canonical(item, out);
            }
            out.push(b']');
        }
        scalar => out.extend_from_slice(scalar.to_string().as_bytes()),
    }
}

/// Reads with `read` every `interval` until it answers `Some`; `None` once `deadline` passed.
async fn poll_until<T, F>(
    deadline: Instant,
    interval: Duration,
    mut read: impl FnMut() -> F,
) -> Result<Option<T>>
where
    F: Future<Output = Result<Option<T>>>,
{
    loop {
        if let Some(value) = read().await? {
            return Ok(Some(value));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(interval).await;
    }
}

/// A venue this release pins; anything else is refused before any request.
fn supported(venue: &VenueId) -> Result<()> {
    match deployments::chain(venue) {
        Some(_) => Ok(()),
        None => Err(unsupported(details::VENUE).into()),
    }
}

/// The balance row of the account `address` has linked on `venue`; any other is a change.
fn linked<'a>(balances: &'a [Balance], venue: &VenueId, address: Address) -> Result<&'a Balance> {
    balances
        .iter()
        .find(|row| row.venue == *venue)
        .filter(|row| row.address == address)
        .ok_or_else(|| changed(details::LINKED_ACCOUNT).into())
}

/// The owner Lighter's public API reports for account `index`, if it answers for it.
async fn lighter_owner(transport: &Inner, index: u64) -> Result<Option<Address>> {
    let answer: Value = transport.lighter_get(&lighter::account_path(index)).await?;
    Ok(lighter::account_owner(&answer, index))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{MakerCredential, QuoteIdentity, Quoter, model::Order, types::PreparationId};

    #[test]
    fn flow_requests_are_shared_and_flow_futures_send() {
        fn send<T: Send>(_: T) {}
        fn shared<T: Send + Sync + 'static>() {}
        #[expect(dead_code, reason = "compile-time check only")]
        fn futures(
            session: &Session,
            quoter: &Quoter,
            requests: (&TakeRequest, &TransferRequest, &WithdrawalRequest, &CapitalRequest),
            lines: &QuoteLinesRequest,
            order: &Order,
            preparation: PreparationId,
        ) {
            let (take, transfer, withdrawal, capital) = requests;
            let venue = VenueId::Hyperliquid;
            send(session.take(take));
            send(session.transfer(transfer));
            send(session.withdraw(withdrawal));
            send(session.link_account(&venue));
            send(session.set_up_venue(&venue, None));
            send(session.authorize_wallet(&venue));
            let maker = session.maker(Address::ZERO);
            send(
                maker.add_capital(
                    capital,
                    &QuoteIdentity::new(order.quote_identity_id, Address::ZERO),
                ),
            );
            send(maker.wait_for_funding(order, Duration::ZERO));
            send(maker.wait_for_order(order.id, &[], Duration::ZERO));
            send(maker.close_orders(&[]));
            send(maker.preparation(preparation));
            send(maker.cancel_preparation(preparation));
            send(quoter.lease(Duration::from_secs(30)));
            send(quoter.lines());
            send(quoter.publish_lines(lines));
            send(quoter.cancel_all());
        }
        shared::<TakeRequest>();
        shared::<TransferRequest>();
        shared::<WithdrawalRequest>();
        shared::<CapitalRequest>();
        shared::<QuoteLinesRequest>();
        shared::<Quoter>();
        shared::<MakerCredential>();
    }

    #[test]
    fn request_digests_ignore_key_order_and_attestations() {
        let item = json!({"kind": "pay_take", "typed_data": {"b": 1, "a": [true, null, "x"]}});
        let reordered: Value =
            serde_json::from_str(r#"{"typed_data":{"a":[true,null,"x"],"b":1},"kind":"pay_take"}"#)
                .unwrap();
        let mut attested = reordered.clone();
        attested["attestation"] = json!({"signature": "0x11"});
        let digests = request_digests(std::slice::from_ref(&item));
        assert_eq!(digests, request_digests(&[reordered]));
        assert_eq!(digests, request_digests(&[attested]));
        let mut moved = item;
        moved["typed_data"]["b"] = json!(2);
        assert_ne!(digests, request_digests(&[moved]));
        let mut bytes = Vec::new();
        canonical(&json!({"b": 1, "a": [true, null, "x"]}), &mut bytes);
        assert_eq!(String::from_utf8(bytes).unwrap(), r#"{"a":[true,null,"x"],"b":1}"#);
    }
}
