//! A GMX withdrawal its owner sends itself, journalled: created or found again by its client id,
//! checked against the request and the pinned GMX deployment, its `transferOut` rebuilt from the
//! amount alone, fenced before the host is asked, and never asked again once fenced: a GMX
//! withdrawal has no nonce, only its first fenced prompt may send it.

use alloy_primitives::{Address, B256, U256};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::MoveWait;
use crate::{
    Error, Result, SendError, SendStatus, Session, Transaction, TransactionSender,
    api::DirectRequest,
    deployments,
    flows::records,
    model::{DirectKind, DirectOperation, DirectState},
    protocol::{
        checks::{
            deployment,
            details::{self, changed, differs, stale, unsupported},
        },
        gmx,
    },
    types::{Amount, VenueId},
};

/// The server's window for a direct operation's deadline, with its skew.
const DEADLINE_WINDOW_SECONDS: i64 = 1815;
/// How far a funds read may be from the client's clock.
const FUNDS_AHEAD_SECONDS: i64 = 15;
const FUNDS_AGE_SECONDS: i64 = 120;

/// Where a direct run stopped.
pub(super) enum Withdrawn {
    /// As last read.
    Operation(Box<DirectOperation>),
    /// Nothing failed: run again later.
    Waiting(MoveWait),
}

/// The operation first created for the request.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    id: Uuid,
    nonce: Amount,
    deadline: Amount,
    escrow: Address,
}

/// The withdrawal asked of the sender: journalled before the fence, `fenced` once this store holds
/// it, with its hash once returned, `unsent` once the sender said it sent nothing.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
struct Asked {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hash: Option<B256>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    unsent: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    fenced: bool,
}

/// Final: nothing more moves.
pub(super) fn is_final(state: &DirectState) -> bool {
    matches!(state, DirectState::Confirmed | DirectState::Failed | DirectState::Expired)
}

/// Runs the GMX withdrawal `request`: see the module.
#[expect(clippy::too_many_lines, reason = "the TypeScript directTransfer(), step by step")]
pub(super) async fn run(
    session: &Session,
    request: &DirectRequest,
    sender: Option<&dyn TransactionSender>,
) -> Result<Withdrawn> {
    if request.owner != session.address() || request.kind != DirectKind::Withdrawal {
        return Err(Error::InvalidRequest("a GMX withdrawal of the session's own account".into()));
    }
    let journal = session.journal();
    let key = |record: &str| {
        session.record_key(&format!("direct:{}{record}", request.client_operation_id))
    };
    let _turn = journal.lock(&key("")).await;
    match records::load::<DirectRequest>(journal, &key(":request")).await? {
        Some(saved) if saved != *request => {
            let message = format!("direct {} names another operation", request.client_operation_id);
            return Err(Error::InvalidRequest(message.into()));
        }
        Some(_) => {}
        None => records::save(journal, &key(":request"), request, false).await?,
    }
    let directs = session.directs();
    let created = directs.create(request).await?;
    check(session, &created, request)?;
    let identity = Identity {
        id: created.id,
        nonce: created.nonce,
        deadline: created.deadline,
        escrow: created.escrow,
    };
    match records::load::<Identity>(journal, &key(":identity")).await? {
        Some(saved) if saved != identity => return Err(changed(details::DIRECT).into()),
        Some(_) => {}
        None => records::save(journal, &key(":identity"), &identity, false).await?,
    }
    let read = || async {
        let operation = directs.get(identity.id).await?;
        check(session, &operation, request)?;
        let same = (operation.id, operation.nonce, operation.deadline, operation.escrow)
            == (identity.id, identity.nonce, identity.deadline, identity.escrow);
        if same { Ok(operation) } else { Err(Error::from(changed(details::DIRECT))) }
    };

    let sent_key = key(":sent");
    let mut sent = records::load::<Asked>(journal, &sent_key).await?;
    if let Some(asked) = &mut sent {
        // Fenced by this store with no answer recorded: only the sender knows whether it went.
        if asked.fenced && asked.hash.is_none() && !asked.unsent {
            let id = transaction_id(identity.id);
            match status(sender, &id).await {
                SendStatus::Sent(hash) => asked.hash = Some(hash),
                SendStatus::NotSent => asked.unsent = true,
                _ => {}
            }
            records::save(journal, &sent_key, asked, true).await?;
        }
        if let Some(hash) = asked.hash
            && !created.attempts.iter().any(|seen| seen.parse() == Ok(hash))
        {
            directs.report_transaction(identity.id, hash).await?;
        }
    }
    let operation = read().await?;
    if is_final(&operation.state) || operation.state != DirectState::Prepared || sent.is_some() {
        return Ok(Withdrawn::Operation(Box::new(operation)));
    }

    let now = session.transport().now().timestamp();
    let deadline = i64::try_from(operation.deadline.get()).unwrap_or(i64::MAX);
    if deadline <= now || deadline > now + DEADLINE_WINDOW_SECONDS {
        return Err(stale(details::DEADLINE).into());
    }
    let venues = session.client().market().venues().await?;
    let chain = venues
        .iter()
        .find(|row| row.id == VenueId::Gmx && row.enabled)
        .and_then(|row| row.chain.as_ref())
        .ok_or(unsupported(details::VENUE))?;
    if deployment::check_registry(&VenueId::Gmx, chain)? != Some(session.escrow_environment())
        || chain.escrow != operation.escrow
    {
        return Err(differs(details::ESCROW).into());
    }
    if chain.direct_withdrawal != Some(true) {
        return Err(unsupported(details::VENUE).into());
    }
    let sender = sender.ok_or(Error::TransactionSenderRequired)?;
    let funds = directs.funding(identity.id).await?;
    let age = session.transport().now() - funds.observed_at;
    let fresh = age.num_seconds() >= -FUNDS_AHEAD_SECONDS && age.num_seconds() < FUNDS_AGE_SECONDS;
    if (funds.owner, funds.token, funds.spender)
        != (session.address(), request.token, operation.escrow)
        || !fresh
        || funds.balance < request.amount
    {
        return Err(changed(details::DIRECT_FUNDING).into());
    }

    // Journalled, then fenced, before the prompt: a lost fence answer never prompts again.
    records::save(journal, &sent_key, &Asked::default(), false).await?;
    directs.fence(identity.id).await?;
    let fenced = Asked { fenced: true, ..Asked::default() };
    records::save(journal, &sent_key, &fenced, true).await?;
    let transaction = Transaction {
        id: transaction_id(identity.id),
        chain_id: operation.chain_id,
        to: gmx::TRANSFER_ROUTER,
        data: gmx::transfer_out(request.amount.get()).to_vec(),
        value: U256::ZERO,
    };
    match sender.send(transaction).await {
        Ok(hash) => {
            records::save(journal, &sent_key, &Asked { hash: Some(hash), ..fenced }, true).await?;
            directs.report_transaction(identity.id, hash).await?;
        }
        Err(SendError::NotSent(error)) => {
            tracing::info!(%error, "GMX withdrawal not sent");
            records::save(journal, &sent_key, &Asked { unsent: true, ..fenced }, true).await?;
            return Ok(Withdrawn::Waiting(MoveWait::NotSent));
        }
        Err(error) => {
            tracing::warn!(%error, "GMX withdrawal outcome unknown");
            return Ok(Withdrawn::Waiting(MoveWait::Withdrawal));
        }
    }
    Ok(Withdrawn::Operation(Box::new(read().await?)))
}

/// The operation is the request's, on the pinned GMX deployment, of the session's own account to
/// itself; a confirmed one received at least its floor.
fn check(session: &Session, operation: &DirectOperation, request: &DirectRequest) -> Result<()> {
    let chain = deployments::chain(&VenueId::Gmx).ok_or(unsupported(details::VENUE))?;
    let pinned = deployments::escrow(chain.id, session.escrow_environment());
    let owner = session.address();
    let ok = operation.chain_id == chain.id
        && pinned == Some(operation.escrow)
        && operation.client_operation_id == request.client_operation_id
        && operation.venue == VenueId::Gmx
        && operation.kind == DirectKind::Withdrawal
        && operation.owner == owner
        && operation.recipient == owner
        && operation.token == chain.token
        && operation.token == request.token
        && operation.amount == request.amount
        && operation.min_out == request.min_out
        && request.amount == request.min_out
        && !request.amount.get().is_zero();
    if !ok {
        return Err(changed(details::DIRECT).into());
    }
    if operation.state == DirectState::Confirmed
        && (operation.tx_hash.is_none()
            || operation.received.is_none_or(|received| received < request.min_out))
    {
        return Err(differs(details::DIRECT).into());
    }
    Ok(())
}

fn transaction_id(id: Uuid) -> String {
    format!("lamarge.direct.{id}.withdrawal")
}

/// What the sender knows of `id`; without a sender or a lookup, nothing.
async fn status(sender: Option<&dyn TransactionSender>, id: &str) -> SendStatus {
    match sender {
        Some(sender) => sender.status(id).await.unwrap_or(SendStatus::Unknown),
        None => SendStatus::Unknown,
    }
}
