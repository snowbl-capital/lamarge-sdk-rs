//! A move's budget as the journal counts it: every part from before it is created until its final
//! state shows nothing debited, against the move's `amount_in` and what the move put in the
//! payer's wallet.

use alloy_primitives::U256;
use serde::{Deserialize, Serialize};

use crate::{Error, Result, Session, api::MoveRequest, flows::records, types::Amount};

/// What a part takes its funds from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Source {
    /// The move's origin.
    Origin,
    /// The payer's wallet on the origin chain, which the move's withdrawals credited.
    Wallet,
}

/// One part of a move: what it takes, from where, and what it put in the wallet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Part {
    name: String,
    amount: Amount,
    from: Source,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credit: Option<Amount>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    released: bool,
}

impl Part {
    pub(super) fn new(name: &str, amount: Amount, from: Source) -> Self {
        Self { name: name.to_owned(), amount, from, credit: None, released: false }
    }
}

/// What a part's state shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Amend {
    /// Never created: it counts no more.
    Forget,
    /// Final with nothing debited.
    Release,
    /// It put `amount` in the wallet.
    Credit(Amount),
    /// It ended having taken only `amount` from the origin, all of it now in the wallet.
    Freed(Amount),
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Budget {
    parts: Vec<Part>,
}

fn keys(session: &Session, request: &MoveRequest) -> (String, String) {
    let id = &request.client_move_id;
    (
        session.record_key(&format!("move-budget:{id}")),
        session.record_key(&format!("move:{id}:parts")),
    )
}

/// Counts a new part against the move's `amount_in` and what the move put in the wallet; one
/// already counted is left as is.
///
/// # Errors
///
/// [`Error::InvalidRequest`] when the parts would exceed either; nothing is counted.
pub(super) async fn reserve(session: &Session, request: &MoveRequest, part: Part) -> Result<()> {
    let (lock, key) = keys(session, request);
    let journal = session.journal();
    let _turn = journal.lock(&lock).await;
    let mut budget = records::load::<Budget>(journal, &key).await?.unwrap_or_default();
    if budget.parts.iter().any(|known| known.name == part.name) {
        return Ok(());
    }
    budget.parts.push(part);
    let total = |from: Source| {
        budget
            .parts
            .iter()
            .filter(|part| part.from == from && !part.released)
            .try_fold(U256::ZERO, |sum, part| sum.checked_add(part.amount.get()))
    };
    let credit = budget
        .parts
        .iter()
        .try_fold(U256::ZERO, |sum, part| sum.checked_add(part.credit.unwrap_or_default().get()));
    let within = total(Source::Origin).is_some_and(|origin| origin <= request.amount_in.get())
        && total(Source::Wallet).zip(credit).is_some_and(|(wallet, credit)| wallet <= credit);
    if !within {
        return Err(Error::InvalidRequest("the move's parts would exceed its amount_in".into()));
    }
    records::save(journal, &key, &budget, false).await
}

/// Records what part `name`'s state shows.
pub(super) async fn amend(
    session: &Session,
    request: &MoveRequest,
    name: &str,
    amend: Amend,
) -> Result<()> {
    let (lock, key) = keys(session, request);
    let journal = session.journal();
    let _turn = journal.lock(&lock).await;
    let mut budget = records::load::<Budget>(journal, &key).await?.unwrap_or_default();
    if amend == Amend::Forget {
        budget.parts.retain(|part| part.name != name);
    }
    for part in budget.parts.iter_mut().filter(|part| part.name == name) {
        match amend {
            Amend::Forget => {}
            Amend::Release => part.released = true,
            Amend::Credit(amount) => part.credit = Some(amount),
            Amend::Freed(amount) => {
                part.amount = amount;
                part.credit = Some(amount);
            }
        }
    }
    records::save(journal, &key, &budget, false).await
}
