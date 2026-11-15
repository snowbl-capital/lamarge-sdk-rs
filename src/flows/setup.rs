//! Account flows: linking, venue setup and wallet authorization.

use alloy_primitives::{Address, B256};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::time::Instant;
use tracing::Instrument as _;
use uuid::Uuid;

use super::{POLL, lighter_owner, poll_until, records, supported};
use crate::{
    Error, Result, Session, deployments,
    error::ErrorCode,
    model::{Balance, LinkedAccount, SetupStep, StepStatus, VenueSetup, WalletPermission},
    protocol::checks::{
        LighterOwners,
        details::{self, changed, differs, unsupported},
        setup::{check_setup_step, check_wallet_permit},
    },
    transport::{Call, ops},
    types::{SetupStepKind, SignatureBytes, VenueId},
};

/// Attempts of a setup submission the venue could not take (`VENUE_UNAVAILABLE`).
const SUBMIT_ATTEMPTS: u32 = 3;
/// How long a pending step is read before giving up on its confirmation.
const CONFIRMATION_READS: u32 = 60;

/// A wallet permit signature, saved before it is submitted; reused only for the same permit and
/// digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WalletPermitRecord {
    /// The server's permit id.
    id: Uuid,
    /// The digest signed.
    digest: B256,
    signature: SignatureBytes,
}

#[derive(Serialize)]
struct LinkBody<'a> {
    venue: &'a VenueId,
    address: Address,
}

#[derive(Serialize)]
struct StepSubmission {
    #[serde(rename = "ref")]
    reference: String,
    signature: SignatureBytes,
}

#[derive(Serialize)]
struct PermitBody {
    payer: Address,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    reference: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<SignatureBytes>,
}

impl Session {
    /// Links the session's own address on `venue` (no proof needed for the master address);
    /// relinking the same address is harmless. Its setup steps follow
    /// ([`set_up_venue`](Self::set_up_venue)).
    ///
    /// A venue this release does not pin fails with `Verification(Unsupported)` before any
    /// request; an answer naming another venue or address with `Verification(Differs)`.
    pub async fn link_account(&self, venue: &VenueId) -> Result<LinkedAccount> {
        supported(venue)?;
        let address = self.address();
        let call = Call::new(&ops::LINK).json(&LinkBody { venue, address })?;
        let linked: LinkedAccount = self.transport().send(call, self.auth()).await?;
        if linked.venue != *venue || linked.address != address {
            return Err(differs(details::ACCOUNT).into());
        }
        Ok(linked)
    }

    /// Completes the setup steps of the account linked on `venue` that are not done: each is
    /// prepared, checked (a permit of the venue token to this environment's escrow, Aster's
    /// agent approval for this environment's agent, Lighter's registration of this environment's
    /// key on an account Lighter says the session owns), signed and submitted, then awaited
    /// until the venue shows it. `steps` limits setup to those kinds; `None` runs every step.
    ///
    /// A submission the venue could not take (`VENUE_UNAVAILABLE`) is sent again up to three
    /// times; Aster's approval refused as stale is prepared and signed once more. A step still
    /// pending after two minutes, or not done, fails with [`Error::SetupIncomplete`]: run it
    /// again later, a submitted step is not signed twice.
    ///
    /// # Cancellation
    ///
    /// Dropping the future is safe: a step already submitted shows as pending or done on the
    /// next run, and an unsubmitted signature is discarded.
    pub async fn set_up_venue(
        &self,
        venue: &VenueId,
        steps: Option<&[SetupStepKind]>,
    ) -> Result<VenueSetup> {
        let span = tracing::info_span!("lamarge.setup", venue = %venue);
        self.set_up(venue, steps).instrument(span).await
    }

    async fn set_up(&self, venue: &VenueId, steps: Option<&[SetupStepKind]>) -> Result<VenueSetup> {
        supported(venue)?;
        let account = self.account();
        let (setup, lighter_account) = if *venue == VenueId::Lighter {
            let (setup, balances) =
                futures_util::future::try_join(account.venue_setup(venue), account.balances())
                    .await?;
            (setup, lighter_index(&balances))
        } else {
            (account.venue_setup(venue).await?, None)
        };
        self.check_setup_owner(&setup)?;
        let wanted = |step: &SetupStepKind| steps.is_none_or(|steps| steps.contains(step));
        let todo: Vec<SetupStepKind> = setup
            .steps
            .iter()
            .filter(|row| row.status != StepStatus::Done && wanted(&row.step))
            .map(|row| row.step.clone())
            .collect();
        for step in &todo {
            tracing::info!(step = %step, "setting up");
            let status = match self.run_step(venue, step, lighter_account).await? {
                StepStatus::Pending => {
                    let deadline = Instant::now() + POLL * CONFIRMATION_READS;
                    let confirmed = || async {
                        let status = self.step_status(venue, step).await?;
                        Ok((status != StepStatus::Pending).then_some(status))
                    };
                    poll_until(deadline, POLL, confirmed).await?.unwrap_or(StepStatus::Pending)
                }
                status => status,
            };
            if status != StepStatus::Done {
                return Err(Error::SetupIncomplete { venue: venue.clone(), step: step.clone() });
            }
            tracing::info!(step = %step, "set up");
        }
        let setup = if todo.is_empty() { setup } else { account.venue_setup(venue).await? };
        self.check_setup_owner(&setup)?;
        if let Some(row) =
            setup.steps.iter().find(|row| row.status != StepStatus::Done && wanted(&row.step))
        {
            return Err(Error::SetupIncomplete { venue: venue.clone(), step: row.step.clone() });
        }
        Ok(setup)
    }

    /// Prepares `step` and, while it is required, checks, signs and submits it; returns its
    /// status. A stale Aster approval is the only one prepared and signed again.
    ///
    /// Not journalled: a step the server took shows pending or done and is not prepared again,
    /// and one it never took is prepared again as the same permit (the token's nonce), agent or
    /// key, whose effect a second signature does not change.
    pub(super) async fn run_step(
        &self,
        venue: &VenueId,
        step: &SetupStepKind,
        lighter_account: Option<u64>,
    ) -> Result<StepStatus> {
        let mut reprepared = false;
        loop {
            let prepare =
                Call::new(&ops::SETUP_STEP).segment(venue).segment(step).json(&json!({}))?;
            let prepared: SetupStep = self.transport().send(prepare, self.auth()).await?;
            if prepared.step != *step {
                return Err(differs(details::SETUP_STEP).into());
            }
            if prepared.status != StepStatus::Required {
                return Ok(prepared.status);
            }
            let owners = self.lighter_owners(step, lighter_account).await?;
            let verified = check_setup_step(
                &prepared,
                venue,
                self.address(),
                lighter_account,
                &owners,
                self.escrow_environment(),
                self.transport().now(),
            )?;
            let reference = prepared
                .to_sign
                .map(|request| request.reference)
                .ok_or(differs(details::TO_SIGN))?;
            let signature = self.sign(&verified).await?;
            let submission = StepSubmission { reference, signature };
            match self.submit_step(venue, step, &submission).await {
                Ok(answer) if answer.step == *step => return Ok(answer.status),
                Ok(_) => return Err(differs(details::SETUP_STEP).into()),
                Err(error) if !reprepared && *step == SetupStepKind::Agent && stale(&error) => {
                    tracing::info!("agent approval stale; preparing it again");
                    reprepared = true;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Submits a signed step, again after `VENUE_UNAVAILABLE` (2 s, then 4 s).
    async fn submit_step(
        &self,
        venue: &VenueId,
        step: &SetupStepKind,
        submission: &StepSubmission,
    ) -> Result<SetupStep> {
        let mut attempt = 1;
        loop {
            let call = Call::new(&ops::SETUP_STEP).segment(venue).segment(step).json(submission)?;
            match self.transport().send(call, self.auth()).await {
                Err(error)
                    if attempt < SUBMIT_ATTEMPTS
                        && error.code() == Some(&ErrorCode::VenueUnavailable) =>
                {
                    tokio::time::sleep(POLL * attempt).await;
                    attempt += 1;
                }
                result => return result,
            }
        }
    }

    /// `step`'s status as the account's setup now shows it.
    async fn step_status(&self, venue: &VenueId, step: &SetupStepKind) -> Result<StepStatus> {
        let setup = self.account().venue_setup(venue).await?;
        self.check_setup_owner(&setup)?;
        setup
            .steps
            .into_iter()
            .find(|row| row.step == *step)
            .map(|row| row.status)
            .ok_or_else(|| differs(details::SETUP_STEP).into())
    }

    /// For Lighter's key registration, the owner of `account` as Lighter's public API answers.
    async fn lighter_owners(
        &self,
        step: &SetupStepKind,
        account: Option<u64>,
    ) -> Result<LighterOwners> {
        let mut owners = LighterOwners::new();
        if *step == SetupStepKind::ApiKey
            && let Some(index) = account
            && let Some(owner) = lighter_owner(self.transport(), index).await?
        {
            owners.insert(index, owner);
        }
        Ok(owners)
    }

    fn check_setup_owner(&self, setup: &VenueSetup) -> Result<()> {
        if setup.address == self.address() { Ok(()) } else { Err(differs(details::OWNER).into()) }
    }

    /// Authorizes the escrow on `venue` to spend the session address's wallet tokens: the
    /// token's unlimited permit to this environment's escrow, checked, signed and submitted.
    /// Nothing is signed when it is already authorized or pending.
    ///
    /// The signature is saved before it is submitted; a rerun after an unanswered submission
    /// sends the saved signature again for the same permit.
    /// A venue without wallet funding fails with `Verification(Unsupported)` before any request;
    /// a submission the server neither authorizes nor queues with [`Error::SetupIncomplete`].
    ///
    /// # Cancellation
    ///
    /// Dropping the future is safe: a saved signature is submitted by the next run.
    pub async fn authorize_wallet(&self, venue: &VenueId) -> Result<WalletPermission> {
        let span = tracing::info_span!("lamarge.authorize_wallet", venue = %venue);
        self.authorize(venue).instrument(span).await
    }

    async fn authorize(&self, venue: &VenueId) -> Result<WalletPermission> {
        if deployments::chain(venue).is_none_or(|chain| chain.permit.is_none()) {
            return Err(unsupported(details::VENUE).into());
        }
        let address = self.address();
        let key = self.record_key(&format!("wallet-permit:{venue}"));
        let journal = self.journal();
        let _section = journal.lock(&key).await;
        let body = PermitBody { payer: address, reference: None, signature: None };
        let call = Call::new(&ops::PREPARE_WALLET_PERMIT).segment(venue).json(&body)?;
        let current: WalletPermission = self.transport().send(call, self.auth()).await?;
        if current.address != address {
            return Err(differs(details::PAYER).into());
        }
        if current.authorized || current.pending {
            records::delete(journal, &key, true).await?;
            return Ok(current);
        }
        let permit = current.permit.ok_or(differs(details::TO_SIGN))?;
        let now = self.transport().now();
        let verified =
            check_wallet_permit(&permit, venue, address, self.escrow_environment(), now)?;
        let signature = match records::load::<WalletPermitRecord>(journal, &key).await? {
            Some(saved) if saved.id == permit.id => {
                if saved.digest != verified.digest() {
                    return Err(changed(details::DIGEST).into());
                }
                saved.signature
            }
            _ => {
                let signature = self.sign(&verified).await?;
                let record =
                    WalletPermitRecord { id: permit.id, digest: verified.digest(), signature };
                records::save(journal, &key, &record, false).await?;
                signature
            }
        };
        let body =
            PermitBody { payer: address, reference: Some(permit.id), signature: Some(signature) };
        let call = Call::new(&ops::SUBMIT_WALLET_PERMIT).segment(venue).json(&body)?;
        let accepted: WalletPermission = self.transport().send(call, self.auth()).await?;
        if accepted.address != address {
            return Err(differs(details::PAYER).into());
        }
        if !(accepted.authorized || accepted.pending) {
            return Err(Error::SetupIncomplete {
                venue: venue.clone(),
                step: SetupStepKind::Permit,
            });
        }
        records::delete(journal, &key, true).await?;
        tracing::info!(pending = accepted.pending, "wallet authorized");
        Ok(accepted)
    }
}

/// The Lighter account index linked, from the balances.
fn lighter_index(balances: &[Balance]) -> Option<u64> {
    balances
        .iter()
        .find(|row| row.venue == VenueId::Lighter)
        .and_then(|row| row.venue_wallet.as_deref())
        .and_then(|index| index.parse().ok())
}

/// Whether Aster refused an agent approval as stale: it says so only in its message.
fn stale(error: &Error) -> bool {
    error.api().is_some_and(|api| {
        let message = api.message().to_ascii_lowercase();
        *api.code() == ErrorCode::VenueRejected
            && (message.contains("nonce") || message.contains("expired"))
    })
}

#[cfg(test)]
mod tests;
