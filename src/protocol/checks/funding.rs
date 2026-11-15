//! Funding plan and wallet funding checks.

use alloy_primitives::{Address, U256};
use chrono::{DateTime, Duration, Utc};

use super::{
    deployment,
    details::{self, changed, differs, stale, unsupported},
};
use crate::{
    deployments,
    error::VerificationError,
    model::{
        FundingPlan, FundingRequestKind, PaymentSetup, Venue, WalletFunding, WithdrawalDestination,
    },
    types::{Amount, EscrowEnvironment, VenueId},
};

/// The caller's terms a transfer's funding plan is checked against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanTerms {
    pub(crate) from: VenueId,
    /// The session address.
    pub(crate) payer: Address,
    /// The transfer's amount, in `from` base units.
    pub(crate) amount_in: Amount,
    /// Paid from the escrow-chain wallet: nothing may leave the venue for it.
    pub(crate) wallet_funding: bool,
}

/// Checks a transfer's funding plan (`TransferIntent.funding_plan`, `None` when the server could
/// not plan: its `funding_plan_error` says why) at `now`: what the transfer will ask the payer to
/// sign is at most a permit, a margin withdrawal and the payment, in that order, on `from`'s
/// chain, for `terms`' amount, observed within two minutes.
pub(crate) fn check_funding_plan<'a>(
    plan: Option<&'a FundingPlan>,
    terms: &PlanTerms,
    environment: EscrowEnvironment,
    now: DateTime<Utc>,
) -> Result<&'a FundingPlan, VerificationError> {
    let plan = plan.ok_or(changed(details::FUNDING_PLAN))?;
    let chain = deployments::chain(&terms.from).ok_or(unsupported(details::VENUE))?;
    deployments::escrow(chain.id, environment).ok_or(unsupported(details::VENUE))?;
    let (amount, deposit, withdrawal) =
        (plan.amount.get(), plan.deposit.get(), plan.withdrawal_amount.get());
    if plan.venue != terms.from
        || plan.owner != terms.payer
        || plan.amount != terms.amount_in
        || deposit > amount
        || withdrawal > amount
    {
        return Err(changed(details::FUNDING_PLAN));
    }
    let withdraws = !withdrawal.is_zero();
    if terms.wallet_funding && (withdraws || plan.withdrawal_to.is_some()) {
        return Err(differs(details::FUNDING_WITHDRAWAL));
    }
    let age = now - plan.observed_at;
    if age < -Duration::seconds(5) || age >= Duration::seconds(120) {
        return Err(stale(details::FUNDING_OBSERVED_AT));
    }
    let destination = withdraws.then_some(WithdrawalDestination::Signer);
    if plan.withdrawal_to != destination {
        return Err(differs(details::FUNDING_WITHDRAWAL));
    }
    // From GMX the withdrawal is a `transfer_out`, a transaction the payer sends itself.
    let withdrawal_kind = if terms.from == VenueId::Gmx {
        FundingRequestKind::TransferOut
    } else {
        FundingRequestKind::MarginMove
    };
    let kinds: Vec<&FundingRequestKind> =
        plan.requests.iter().map(|request| &request.kind).collect();
    let count = |kind: &FundingRequestKind| kinds.iter().filter(|each| **each == kind).count();
    let permit = plan.payment_setup == PaymentSetup::Required;
    let requests = plan
        .requests
        .iter()
        .all(|request| request.signer == terms.payer && request.chain_id == chain.id)
        && count(&FundingRequestKind::PayTake) == 1
        && kinds.iter().all(|kind| {
            [&FundingRequestKind::Permit, &withdrawal_kind, &FundingRequestKind::PayTake]
                .contains(kind)
                && count(kind) == 1
        })
        && (count(&withdrawal_kind) == 1) == withdraws
        && (count(&FundingRequestKind::Permit) == 1) == permit
        && (!permit || kinds.first() == Some(&&FundingRequestKind::Permit) && !deposit.is_zero());
    if requests { Ok(plan) } else { Err(differs(details::FUNDING_REQUESTS)) }
}

/// Checks that `venue` is enabled with wallet funding in its pinned escrow token
/// (`GET /v1/venues`), in `environment`'s escrow.
pub(crate) fn check_wallet_capability(
    venues: &[Venue],
    venue: &VenueId,
    environment: EscrowEnvironment,
) -> Result<(), VerificationError> {
    let chain = venues
        .iter()
        .find(|row| row.id == *venue && row.enabled)
        .and_then(|row| row.chain.as_ref())
        .ok_or(unsupported(details::VENUE))?;
    match deployment::check_registry(venue, chain)? {
        Some(escrow) if escrow == environment => {}
        Some(_) => return Err(differs(details::ESCROW)),
        // A venue settled inside itself pays from its account.
        None => return Err(unsupported(details::WALLET_FUNDING)),
    }
    let capability = chain.wallet_funding.as_ref().ok_or(unsupported(details::WALLET_FUNDING))?;
    if capability.token == chain.token && capability.decimals == chain.decimals {
        Ok(())
    } else {
        Err(differs(details::WALLET_FUNDING))
    }
}

/// Checks a fresh read of `payer`'s wallet funds on `venue` (`GET /v1/wallets/{venue}`) at `now`
/// against the pins and returns what a new take may spend of them: the pinned chain, escrow and
/// token, the payer's own balances and permission, observed within two minutes.
pub(crate) fn check_wallet_funding(
    funds: &WalletFunding,
    venue: &VenueId,
    payer: Address,
    environment: EscrowEnvironment,
    now: DateTime<Utc>,
) -> Result<U256, VerificationError> {
    let chain = deployments::chain(venue).ok_or(unsupported(details::VENUE))?;
    if funds.venue != *venue
        || funds.chain_id != chain.id
        || Some(funds.escrow) != deployments::escrow(chain.id, environment)
        || funds.token != chain.token
        || funds.decimals != chain.decimals
    {
        return Err(differs(details::WALLET_FUNDS));
    }
    if funds.address != payer || funds.payment_setup.address != payer {
        return Err(changed(details::WALLET_FUNDS));
    }
    let age = now - funds.observed_at;
    if age < -Duration::seconds(15) || age >= Duration::seconds(120) {
        return Err(stale(details::WALLET_FUNDS_OBSERVED_AT));
    }
    if funds.spendable_wallet > funds.wallet || funds.spendable_available > funds.available {
        return Err(differs(details::WALLET_FUNDS));
    }
    funds
        .spendable_wallet
        .get()
        .checked_add(funds.spendable_available.get())
        .ok_or(differs(details::WALLET_FUNDS))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::{
        error::VerificationKind,
        testing::{self, OTHER, OWNER, at, edited, fixtures},
    };

    fn plan_check(plan: &Value, terms: &PlanTerms) -> Result<(), VerificationError> {
        let plan: FundingPlan = fixtures::model(plan);
        check_funding_plan(Some(&plan), terms, EscrowEnvironment::Production, fixtures::now())
            .map(|_| ())
    }

    fn requests(kinds: &[&str]) -> Value {
        kinds.iter().map(|kind| json!({"kind": kind, "signer": OWNER, "chain_id": 999})).collect()
    }

    /// `consents.test.ts` "a funding plan lists the permit first, only when a wallet deposit needs
    /// it, then the withdrawal and the payment".
    #[test]
    fn a_funding_plan_lists_the_permit_first_then_the_withdrawal_and_the_payment() {
        let terms = fixtures::plan_terms();
        assert!(plan_check(&fixtures::funding_plan(), &terms).is_ok());
        let cases: Vec<Value> = vec![
            edited(fixtures::funding_plan(), |p| {
                p["requests"] = requests(&["margin_move", "permit", "pay_take"]);
            }),
            edited(fixtures::funding_plan(), |p| p["payment_setup"] = json!("done")),
            edited(fixtures::funding_plan(), |p| p["payment_setup"] = json!("pending")),
            edited(fixtures::funding_plan(), |p| {
                p["requests"] = requests(&["margin_move", "pay_take"]);
            }),
            edited(fixtures::funding_plan(), |p| {
                p["deposit"] = json!("0");
                p["withdrawal_amount"] = json!("0");
                p["withdrawal_to"] = Value::Null;
                p["requests"] = requests(&["permit", "pay_take"]);
            }),
            edited(fixtures::funding_plan(), |p| {
                p["requests"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"kind": "move", "signer": OWNER, "chain_id": 999}));
            }),
            edited(fixtures::funding_plan(), |p| {
                p["requests"] = requests(&["permit", "margin_move", "pay_take", "pay_take"]);
            }),
            edited(fixtures::funding_plan(), |p| {
                p["requests"] = requests(&["permit", "transfer_out", "pay_take"]);
            }),
            edited(fixtures::funding_plan(), |p| p["requests"][2]["signer"] = json!(OTHER)),
            edited(fixtures::funding_plan(), |p| p["requests"][2]["chain_id"] = json!(137)),
        ];
        for (index, plan) in cases.iter().enumerate() {
            assert_eq!(
                plan_check(plan, &terms).unwrap_err().detail,
                details::FUNDING_REQUESTS,
                "case {index}"
            );
        }
        let done = edited(fixtures::funding_plan(), |p| {
            p["payment_setup"] = json!("done");
            p["requests"] = requests(&["margin_move", "pay_take"]);
        });
        assert!(plan_check(&done, &terms).is_ok());
        // GMX withdraws by the payer's own `transfer_out`.
        let gmx = edited(done, |p| {
            p["venue"] = json!("gmx");
            p["requests"] = json!([
                {"kind": "transfer_out", "signer": OWNER, "chain_id": 42161},
                {"kind": "pay_take", "signer": OWNER, "chain_id": 42161},
            ]);
        });
        assert!(plan_check(&gmx, &PlanTerms { from: VenueId::Gmx, ..terms }).is_ok());
    }

    #[test]
    fn a_funding_plan_is_the_transfers_fresh_and_withdraws_only_to_the_signer() {
        let terms = fixtures::plan_terms();
        let cases = [
            (
                edited(fixtures::funding_plan(), |p| p["venue"] = json!("polymarket")),
                details::FUNDING_PLAN,
            ),
            (
                edited(fixtures::funding_plan(), |p| p["owner"] = json!(OTHER)),
                details::FUNDING_PLAN,
            ),
            (
                edited(fixtures::funding_plan(), |p| p["amount"] = json!("10000001")),
                details::FUNDING_PLAN,
            ),
            (
                edited(fixtures::funding_plan(), |p| p["deposit"] = json!("10000001")),
                details::FUNDING_PLAN,
            ),
            (
                edited(fixtures::funding_plan(), |p| p["withdrawal_amount"] = json!("10000001")),
                details::FUNDING_PLAN,
            ),
            (
                edited(fixtures::funding_plan(), |p| p["withdrawal_to"] = json!("escrow")),
                details::FUNDING_WITHDRAWAL,
            ),
            (
                edited(fixtures::funding_plan(), |p| p["withdrawal_to"] = Value::Null),
                details::FUNDING_WITHDRAWAL,
            ),
            (
                edited(fixtures::funding_plan(), |p| {
                    p["observed_at"] = json!(testing::time(-120));
                }),
                details::FUNDING_OBSERVED_AT,
            ),
            (
                edited(fixtures::funding_plan(), |p| p["observed_at"] = json!(testing::time(6))),
                details::FUNDING_OBSERVED_AT,
            ),
        ];
        for (index, (plan, detail)) in cases.iter().enumerate() {
            assert_eq!(plan_check(plan, &terms).unwrap_err().detail, *detail, "case {index}");
        }
        assert!(
            plan_check(
                &edited(fixtures::funding_plan(), |p| p["observed_at"] = json!(testing::time(5))),
                &terms
            )
            .is_ok()
        );
        // Wallet funding withdraws nothing from a venue.
        let wallet = PlanTerms { wallet_funding: true, ..terms.clone() };
        assert_eq!(
            plan_check(&fixtures::funding_plan(), &wallet).unwrap_err().detail,
            details::FUNDING_WITHDRAWAL
        );
        let from_wallet = edited(fixtures::funding_plan(), |p| {
            p["withdrawal_amount"] = json!("0");
            p["withdrawal_to"] = Value::Null;
            p["requests"] = requests(&["permit", "pay_take"]);
        });
        assert!(plan_check(&from_wallet, &wallet).is_ok());
        // A missing plan asks for the server's reason (`funding_plan_error`).
        let missing =
            check_funding_plan(None, &terms, EscrowEnvironment::Production, fixtures::now());
        assert_eq!(missing.unwrap_err().kind, VerificationKind::Changed);
        let unknown = PlanTerms { from: "dydx".parse().unwrap(), ..terms };
        let plan: FundingPlan = fixtures::model(&fixtures::funding_plan());
        let error = check_funding_plan(
            Some(&plan),
            &unknown,
            EscrowEnvironment::Production,
            fixtures::now(),
        );
        assert_eq!(error.unwrap_err().kind, VerificationKind::Unsupported);
    }

    fn funds_check(funds: &Value, now: i64) -> Result<U256, VerificationError> {
        let funds: WalletFunding = fixtures::model(funds);
        check_wallet_funding(
            &funds,
            &VenueId::Hyperliquid,
            OWNER,
            EscrowEnvironment::Production,
            at(now),
        )
    }

    /// `wallet.test.ts` "wallet reads bind the payer, token, deployment and fresh observation".
    #[test]
    fn wallet_reads_bind_the_payer_token_deployment_and_fresh_observation() {
        assert_eq!(funds_check(&fixtures::wallet_funds(), 0).unwrap(), U256::from(5_000_000u64));
        let cases = [
            ("venue", json!("polymarket"), details::WALLET_FUNDS),
            ("chain_id", json!(137), details::WALLET_FUNDS),
            ("escrow", json!("0x948e65c40442d935d4ee990bdbd01bdd825c4873"), details::WALLET_FUNDS),
            ("token", json!(OTHER), details::WALLET_FUNDS),
            ("decimals", json!(18), details::WALLET_FUNDS),
            ("address", json!(OTHER), details::WALLET_FUNDS),
            ("spendable_wallet", json!("5000001"), details::WALLET_FUNDS),
            ("spendable_available", json!("1000001"), details::WALLET_FUNDS),
            ("observed_at", json!(testing::time(-130)), details::WALLET_FUNDS_OBSERVED_AT),
            ("observed_at", json!(testing::time(16)), details::WALLET_FUNDS_OBSERVED_AT),
        ];
        for (field, value, detail) in cases {
            let funds = edited(fixtures::wallet_funds(), |f| f[field] = value.clone());
            assert_eq!(funds_check(&funds, 0).unwrap_err().detail, detail, "{field}");
        }
        let permission =
            edited(fixtures::wallet_funds(), |f| f["payment_setup"]["address"] = json!(OTHER));
        assert_eq!(funds_check(&permission, 0).unwrap_err().kind, VerificationKind::Changed);
        assert!(funds_check(&fixtures::wallet_funds(), 109).is_ok());
        assert!(funds_check(&fixtures::wallet_funds(), 110).is_err());
        // Spendable amounts near the largest integer refuse rather than wrap.
        let max = U256::MAX.to_string();
        let huge = edited(fixtures::wallet_funds(), |f| {
            for field in ["wallet", "available", "spendable_wallet", "spendable_available"] {
                f[field] = json!(max);
            }
        });
        assert_eq!(funds_check(&huge, 0).unwrap_err().detail, details::WALLET_FUNDS);
    }

    /// `wallet.test.ts` "new wallet funding fails closed when the server does not advertise
    /// support".
    #[test]
    fn wallet_funding_fails_closed_without_the_pinned_capability() {
        let venues = fixtures::venues();
        let check = |venues: &[Venue], venue: VenueId| {
            check_wallet_capability(venues, &venue, EscrowEnvironment::Production)
        };
        assert!(check(&venues, VenueId::Hyperliquid).is_ok());
        assert_eq!(check(&venues, VenueId::Lighter).unwrap_err().detail, details::WALLET_FUNDING);
        let edit = |edit: fn(&mut Value)| {
            fixtures::venues_with(|list| {
                edit(list.iter_mut().find(|row| row["id"] == "hyperliquid").unwrap());
            })
        };
        let missing = edit(|row| row["chain"]["wallet_funding"] = Value::Null);
        assert_eq!(
            check(&missing, VenueId::Hyperliquid).unwrap_err().detail,
            details::WALLET_FUNDING
        );
        let token = edit(|row| row["chain"]["wallet_funding"]["token"] = json!(OTHER));
        assert_eq!(
            check(&token, VenueId::Hyperliquid).unwrap_err().detail,
            details::WALLET_FUNDING
        );
        let decimals = edit(|row| row["chain"]["wallet_funding"]["decimals"] = json!(18));
        assert_eq!(
            check(&decimals, VenueId::Hyperliquid).unwrap_err().detail,
            details::WALLET_FUNDING
        );
        let disabled = edit(|row| row["enabled"] = json!(false));
        assert_eq!(check(&disabled, VenueId::Hyperliquid).unwrap_err().detail, details::VENUE);
        let development = fixtures::venues_in(EscrowEnvironment::Development);
        assert_eq!(check(&development, VenueId::Hyperliquid).unwrap_err().detail, details::ESCROW);
    }
}
