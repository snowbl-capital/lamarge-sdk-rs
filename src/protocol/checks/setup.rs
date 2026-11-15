//! Venue setup checks: permits, Aster agent approvals, Lighter API key registrations.

use alloy_primitives::{Address, hex};
use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{
    LighterOwners, SKEW_SECONDS, deadline_bound, deadline_within,
    details::{self, changed, differs, stale, unsupported},
    signed,
};
use crate::{
    deployments,
    error::VerificationError,
    model::{SetupStep, SignRequestKind, WalletPermit},
    protocol::{
        aster,
        eip712::{self, Message, Permit},
        lighter,
        signing::Verified,
    },
    types::{EscrowEnvironment, SetupStepKind, VenueId},
};

/// The longest a permit may stay signable.
const PERMIT_SECONDS: i64 = 600;
/// An agent approval lasts a year, in milliseconds.
const APPROVAL_MILLIS: u64 = 365 * 86_400_000;
/// The most addresses an agent may be used from.
const MAX_ADDRESSES: usize = 8;

/// Checks a prepared setup step of `venue` (`POST /v1/accounts/{venue}/setup/{step}`) for
/// `owner`, the linked address that signs it, and returns what it signs: the step's one kind of
/// request (`permit`, `agent`, `api_key`), for `owner`, on the venue it belongs to. A Lighter key
/// registration names `lighter_account`, the linked account index, whose owner `owners` must
/// show to be `owner`.
pub(crate) fn check_setup_step(
    step: &SetupStep,
    venue: &VenueId,
    owner: Address,
    lighter_account: Option<u64>,
    owners: &LighterOwners,
    environment: EscrowEnvironment,
    now: DateTime<Utc>,
) -> Result<Verified, VerificationError> {
    let request = step.to_sign.as_ref().ok_or(differs(details::TO_SIGN))?;
    let (kind, settled) = match step.step {
        SetupStepKind::Permit => (SignRequestKind::Permit, None),
        SetupStepKind::Agent => (SignRequestKind::AsterApproval, Some(VenueId::Aster)),
        SetupStepKind::ApiKey => (SignRequestKind::LighterApiKey, Some(VenueId::Lighter)),
        SetupStepKind::Unknown(_) => return Err(unsupported(details::SETUP_STEP)),
    };
    if request.kind != kind {
        return Err(differs(details::SETUP_KIND));
    }
    if settled.as_ref().is_some_and(|settled| settled != venue) {
        return Err(unsupported(details::SETUP_STEP));
    }
    if request.signer != owner {
        return Err(differs(details::TO_SIGN_SIGNER));
    }
    let typed_data = || request.typed_data.as_ref().ok_or(differs(details::TYPED_DATA));
    match kind {
        SignRequestKind::Permit => check_permit(typed_data()?, venue, owner, environment, now),
        SignRequestKind::AsterApproval => {
            check_aster_approval(typed_data()?, &request.reference, owner, environment, now)
        }
        _ => {
            let message = request.message.as_deref().ok_or(differs(details::MESSAGE))?;
            let account = lighter_account.ok_or(changed(details::LINKED_ACCOUNT))?;
            check_lighter_registration(
                message,
                &request.reference,
                account,
                owners,
                owner,
                environment,
            )
        }
    }
}

/// Checks the wallet permit a transfer from wallet funds asks for
/// (`POST /v1/wallets/{venue}/permit`): the unlimited permit of `venue`'s token to `environment`'s
/// escrow, by `owner`.
pub(crate) fn check_wallet_permit(
    permit: &WalletPermit,
    venue: &VenueId,
    owner: Address,
    environment: EscrowEnvironment,
    now: DateTime<Utc>,
) -> Result<Verified, VerificationError> {
    let request = &permit.to_sign;
    if request.kind != SignRequestKind::Permit {
        return Err(differs(details::SETUP_KIND));
    }
    if request.signer != owner {
        return Err(differs(details::TO_SIGN_SIGNER));
    }
    check_permit(&request.typed_data, venue, owner, environment, now)
}

/// An EIP-2612 permit in the pinned domain of `venue`'s token, by `owner`, of an unlimited
/// allowance to `environment`'s escrow on that chain, its deadline within ten minutes.
fn check_permit(
    raw: &Value,
    venue: &VenueId,
    owner: Address,
    environment: EscrowEnvironment,
    now: DateTime<Utc>,
) -> Result<Verified, VerificationError> {
    let chain = deployments::chain(venue).ok_or(unsupported(details::VENUE))?;
    let domain = eip712::permit_domain(chain).ok_or(unsupported(details::VENUE))?;
    let escrow = deployments::escrow(chain.id, environment).ok_or(unsupported(details::VENUE))?;
    let parsed = eip712::parse_in::<Permit>(raw, &domain)?;
    let permit = &parsed.message;
    if permit.owner != owner {
        return Err(differs(details::OWNER));
    }
    if permit.spender != escrow {
        return Err(differs(details::SPENDER));
    }
    if permit.value != deployments::UNLIMITED_ALLOWANCE {
        return Err(differs(details::VALUE));
    }
    if !deadline_within(permit.deadline, now.timestamp(), PERMIT_SECONDS) {
        return Err(stale(details::DEADLINE));
    }
    let not_after = Some(deadline_bound(permit.deadline));
    Verified::typed(eip712::typed(permit, domain), parsed.digest, owner, not_after)
}

/// Aster's agent approval: `Message{msg}` at BSC's chain id approving `environment`'s LaMarge
/// agent for `owner`, for a year, trading perps only (no spot, no withdrawal), from a few narrow
/// public addresses if it lists any, under the prepared nonce (`reference`), within a minute of
/// `now`.
fn check_aster_approval(
    raw: &Value,
    reference: &str,
    owner: Address,
    environment: EscrowEnvironment,
    now: DateTime<Utc>,
) -> Result<Verified, VerificationError> {
    const NAMES: [&str; 10] = [
        "user",
        "nonce",
        "agentName",
        "agentAddress",
        "expired",
        "signatureChainId",
        "canSpotTrade",
        "canPerpTrade",
        "canWithdraw",
        "ipWhitelist",
    ];
    let domain = aster::domain(aster::CHAIN_ID);
    let parsed = eip712::parse_in::<Message>(raw, &domain)?;
    let msg = parsed.message.msg.as_str();
    let listed = msg.split('&').count() > NAMES.len() - 1;
    let names = if listed { &NAMES[..] } else { &NAMES[..NAMES.len() - 1] };
    let values = aster::fields(msg, names).ok_or(differs(details::AGENT_APPROVAL))?;
    let [user, nonce, name, agent, expired, chain, spot, perp, withdraw] = values[..9] else {
        return Err(differs(details::AGENT_APPROVAL));
    };
    if eip712::address_text(user) != Some(owner) {
        return Err(differs(details::AGENT_USER));
    }
    let nonce = aster::parse_nonce(nonce)
        .filter(|_| nonce == reference)
        .ok_or(differs(details::AGENT_NONCE))?;
    if !aster::nonce_near(nonce, now.timestamp(), SKEW_SECONDS) {
        return Err(stale(details::AGENT_NONCE));
    }
    let agent = eip712::address_text(agent).filter(|agent| *agent != owner);
    if name != deployments::slots(environment).aster_agent_name || agent.is_none() {
        return Err(differs(details::AGENT));
    }
    if expired.parse::<u64>().ok() != (nonce / 1000).checked_add(APPROVAL_MILLIS) {
        return Err(differs(details::AGENT_EXPIRY));
    }
    let ips = values.get(9).copied();
    if chain != aster::CHAIN_ID.to_string()
        || spot != "false"
        || perp != "true"
        || withdraw != "false"
        || ips == Some("")
    {
        return Err(differs(details::AGENT_PERMISSIONS));
    }
    if ips.is_some_and(|ips| !narrow_addresses(ips)) {
        return Err(differs(details::AGENT_ADDRESSES));
    }
    let not_after = Some(signed(nonce / 1_000_000).saturating_add(SKEW_SECONDS));
    let message = Message { msg: msg.to_owned() };
    Verified::typed(eip712::typed(&message, domain), parsed.digest, owner, not_after)
}

/// Public addresses only: at most eight, IPv4 ranges no wider than /24 and IPv6 no wider than
/// /64, no aliases, special ranges or embedded addresses.
fn narrow_addresses(list: &str) -> bool {
    let entries: Vec<&str> = list.split([' ', ',']).collect();
    entries.len() <= MAX_ADDRESSES
        && entries.iter().all(|entry| {
            let mut parts = entry.split('/');
            let host = parts.next().unwrap_or_default();
            let prefix = parts.next();
            if parts.next().is_some() {
                return false;
            }
            let width = match prefix {
                None => None,
                Some(text) if decimal(text, 3) => text.parse::<u16>().ok(),
                Some(_) => return false,
            };
            if let Some([first, ..]) = ipv4(host) {
                (1..=223).contains(&first) && width.is_none_or(|width| (24..=32).contains(&width))
            } else {
                public_ipv6(host) && width.is_none_or(|width| (64..=128).contains(&width))
            }
        })
}

/// A decimal of at most `digits` digits without leading zero.
fn decimal(text: &str, digits: usize) -> bool {
    (1..=digits).contains(&text.len())
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'))
}

/// A dotted IPv4 address: four decimals of 0 to 255 without leading zeros.
fn ipv4(host: &str) -> Option<[u8; 4]> {
    let octets: Vec<u8> = host
        .split('.')
        .map(|part| if decimal(part, 3) { part.parse().ok() } else { None })
        .collect::<Option<_>>()?;
    octets.try_into().ok()
}

/// A full or `::`-compressed IPv6 address in the global unicast range `2000::/3`.
fn public_ipv6(host: &str) -> bool {
    let halves: Vec<&str> = host.split("::").collect();
    let groups: Vec<&str> =
        halves.iter().filter(|half| !half.is_empty()).flat_map(|half| half.split(':')).collect();
    let count = if halves.len() == 2 { groups.len() < 8 } else { groups.len() == 8 };
    let first = host.split(':').next().and_then(|group| u16::from_str_radix(group, 16).ok());
    halves.len() <= 2
        && count
        && groups.iter().all(|group| {
            (1..=4).contains(&group.len()) && group.bytes().all(|b| b.is_ascii_hexdigit())
        })
        && first.is_some_and(|first| (0x2000..=0x3fff).contains(&first))
}

/// Lighter's own owner message registering `environment`'s LaMarge key index on `account` at
/// the key's nonce (`reference`), exactly; `owners` must show Lighter's owner of `account` to be
/// `owner`.
fn check_lighter_registration(
    message: &str,
    reference: &str,
    account: u64,
    owners: &LighterOwners,
    owner: Address,
    environment: EscrowEnvironment,
) -> Result<Verified, VerificationError> {
    const PREFIX: &str = "Register Lighter Account\n\npubkey: 0x";
    let key = message
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.get(..80))
        .filter(|key| key.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        .and_then(|key| <[u8; 40]>::try_from(hex::decode(key).ok()?).ok())
        .ok_or(differs(details::LIGHTER_REGISTRATION))?;
    let nonce = reference
        .parse::<u64>()
        .ok()
        .filter(|_| reference.bytes().all(|b| b.is_ascii_digit()))
        .ok_or(differs(details::LIGHTER_REGISTRATION))?;
    let index = deployments::slots(environment).lighter_key_index;
    if message != lighter::registration_message(account, index, &key, nonce) {
        return Err(differs(details::LIGHTER_REGISTRATION));
    }
    if owners.get(&account) != Some(&owner) {
        return Err(differs(details::LIGHTER_OWNER));
    }
    Ok(Verified::text(message.to_owned(), owner, None))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::{
        error::VerificationKind,
        testing::{MAKER, NOW, OTHER, OWNER, at, edited, fixtures, replaced},
    };

    fn step(value: &Value) -> SetupStep {
        fixtures::model(value)
    }

    fn check_at(
        value: &Value,
        venue: &str,
        environment: EscrowEnvironment,
        now: i64,
    ) -> Result<Verified, VerificationError> {
        let owners = fixtures::lighter_owners();
        check_setup_step(
            &step(value),
            &venue.parse().unwrap(),
            OWNER,
            Some(42),
            &owners,
            environment,
            at(now),
        )
    }

    fn check(value: &Value, venue: &str) -> Result<Verified, VerificationError> {
        check_at(value, venue, EscrowEnvironment::Production, 0)
    }

    /// `consents.test.ts` "a setup step asks the one kind of signature it is signed with".
    #[test]
    fn a_setup_step_asks_the_one_kind_of_signature_it_is_signed_with() {
        assert!(check(&fixtures::permit_step(), "hyperliquid").is_ok());
        assert!(check(&fixtures::approval_step(), "aster").is_ok());
        assert!(check(&fixtures::registration_step(), "lighter").is_ok());
        for (base, step) in [
            (fixtures::permit_step(), "agent"),
            (fixtures::approval_step(), "permit"),
            (fixtures::registration_step(), "permit"),
        ] {
            let other = edited(base, |s| s["step"] = json!(step));
            assert_eq!(check(&other, "aster").unwrap_err().detail, details::SETUP_KIND);
        }
        let unknown = edited(fixtures::permit_step(), |s| s["step"] = json!("other"));
        assert_eq!(check(&unknown, "hyperliquid").unwrap_err().kind, VerificationKind::Unsupported);
        // An agent belongs to Aster, a key to Lighter.
        assert_eq!(
            check(&fixtures::approval_step(), "lighter").unwrap_err().detail,
            details::SETUP_STEP
        );
        assert_eq!(
            check(&fixtures::registration_step(), "aster").unwrap_err().detail,
            details::SETUP_STEP
        );
        let signer = edited(fixtures::permit_step(), |s| s["to_sign"]["signer"] = json!(OTHER));
        assert_eq!(check(&signer, "hyperliquid").unwrap_err().detail, details::TO_SIGN_SIGNER);
        let missing = edited(fixtures::permit_step(), |s| s["to_sign"] = Value::Null);
        assert_eq!(check(&missing, "hyperliquid").unwrap_err().detail, details::TO_SIGN);
        let untyped = edited(fixtures::permit_step(), |s| {
            fixtures::object(&mut s["to_sign"]).remove("typed_data");
        });
        assert_eq!(check(&untyped, "hyperliquid").unwrap_err().detail, details::TYPED_DATA);
        let unmessaged = edited(fixtures::registration_step(), |s| {
            fixtures::object(&mut s["to_sign"]).remove("message");
        });
        assert_eq!(check(&unmessaged, "lighter").unwrap_err().detail, details::MESSAGE);
    }

    /// `intents.test.ts` "only the unlimited permit for the pinned escrow is signed"; `wallet.test.ts`
    /// "a wallet permit cannot authorize another spender".
    #[test]
    fn only_the_unlimited_permit_to_the_environments_escrow_is_signed() {
        let verified = check(&fixtures::permit_step(), "hyperliquid").unwrap();
        // The vector's deadline is ten minutes ahead: signable until 30 seconds before it.
        assert_eq!(verified.not_after(), Some(NOW + 570));
        let message = |name: &str, value: Value| {
            edited(fixtures::permit_step(), |s| {
                s["to_sign"]["typed_data"]["message"][name] = value.clone();
            })
        };
        let cases = [
            (message("value", json!("10000000")), details::VALUE),
            (message("spender", json!(OTHER)), details::SPENDER),
            (
                message("spender", json!("0x948e65c40442d935d4ee990bdbd01bdd825c4873")),
                details::SPENDER,
            ),
            (message("owner", json!(OTHER)), details::OWNER),
            (message("deadline", json!((NOW + 631).to_string())), details::DEADLINE),
            (message("deadline", json!(NOW.to_string())), details::DEADLINE),
        ];
        for (index, (value, detail)) in cases.iter().enumerate() {
            assert_eq!(check(value, "hyperliquid").unwrap_err().detail, *detail, "case {index}");
        }
        assert!(check(&message("deadline", json!((NOW + 630).to_string())), "hyperliquid").is_ok());
        // Another token's domain, or the permit asked on another chain.
        let domain = edited(fixtures::permit_step(), |s| {
            s["to_sign"]["typed_data"]["domain"]["version"] = json!("1");
        });
        assert_eq!(check(&domain, "hyperliquid").unwrap_err().detail, details::DOMAIN);
        assert_eq!(
            check(&fixtures::permit_step(), "polymarket").unwrap_err().detail,
            details::DOMAIN
        );
        assert_eq!(
            check(&fixtures::permit_step(), "lighter").unwrap_err().kind,
            VerificationKind::Unsupported
        );
        // Development permits development's escrow only.
        let development =
            |raw: &Value| check_at(raw, "hyperliquid", EscrowEnvironment::Development, 0);
        assert_eq!(development(&fixtures::permit_step()).unwrap_err().detail, details::SPENDER);
        assert!(
            development(&message("spender", json!("0x948e65c40442d935d4ee990bdbd01bdd825c4873")))
                .is_ok()
        );
        // The wallet permit is the same permit.
        let wallet = |value: &Value| {
            let permit: WalletPermit = fixtures::model(value);
            check_wallet_permit(
                &permit,
                &VenueId::Hyperliquid,
                OWNER,
                EscrowEnvironment::Production,
                fixtures::now(),
            )
        };
        assert!(wallet(&fixtures::wallet_permit()).is_ok());
        let spender = edited(fixtures::wallet_permit(), |p| {
            p["to_sign"]["typed_data"]["message"]["spender"] = json!(MAKER);
        });
        assert_eq!(wallet(&spender).unwrap_err().detail, details::SPENDER);
        let signer = edited(fixtures::wallet_permit(), |p| p["to_sign"]["signer"] = json!(OTHER));
        assert_eq!(wallet(&signer).unwrap_err().detail, details::TO_SIGN_SIGNER);
        let kind =
            edited(fixtures::wallet_permit(), |p| p["to_sign"]["kind"] = json!("aster_approval"));
        assert_eq!(wallet(&kind).unwrap_err().detail, details::SETUP_KIND);
    }

    fn approval(fields: &[(&str, String)]) -> Value {
        fixtures::approval_step_of(&fixtures::approval_message(fields))
    }

    fn with(name: &str, value: &str) -> Vec<(&'static str, String)> {
        replaced(fixtures::approval_fields(), name, value)
    }

    /// `aster.test.ts` "an agent approval is LaMarge's, for this address, only moving margin to
    /// Spot, for a year".
    #[test]
    fn an_agent_approval_is_the_environments_for_this_address_for_a_year() {
        let verified = check(&fixtures::approval_step(), "aster").unwrap();
        assert_eq!(verified.not_after(), Some(NOW + 60));
        let without_ips: Vec<_> = fixtures::approval_fields().into_iter().take(9).collect();
        assert!(check(&approval(&without_ips), "aster").is_ok());
        let nonce = fixtures::approval_fields()[1].1.parse::<u64>().unwrap();
        let expired = (nonce / 1000 + APPROVAL_MILLIS).to_string();
        let cases = [
            (with("canWithdraw", "true"), details::AGENT_PERMISSIONS),
            (with("canPerpTrade", "false"), details::AGENT_PERMISSIONS),
            (with("canSpotTrade", "true"), details::AGENT_PERMISSIONS),
            (with("ipWhitelist", ""), details::AGENT_PERMISSIONS),
            (with("signatureChainId", "42161"), details::AGENT_PERMISSIONS),
            (with("user", &format!("{OTHER:#x}")), details::AGENT_USER),
            (with("agentName", "Other"), details::AGENT),
            (with("agentName", "@aster-desktop"), details::AGENT),
            (with("agentName", "LaMargeProd"), details::AGENT),
            (with("agentName", "LaMargeWithdrawal"), details::AGENT),
            // The development agent, approved in production, would revoke development's.
            (with("agentName", "LaMargeDev"), details::AGENT),
            (with("agentAddress", &format!("{OWNER:#x}")), details::AGENT),
            (with("agentAddress", "0x1234"), details::AGENT),
            (
                with("expired", &(expired.parse::<u64>().unwrap() + 1).to_string()),
                details::AGENT_EXPIRY,
            ),
            (with("nonce", &(nonce + 61_000_000).to_string()), details::AGENT_NONCE),
        ];
        for (index, (fields, detail)) in cases.iter().enumerate() {
            assert_eq!(
                check(&approval(fields), "aster").unwrap_err().detail,
                *detail,
                "case {index}"
            );
        }
        let mut extra = fixtures::approval_fields();
        extra.push(("agentCode", "ref".to_owned()));
        assert_eq!(check(&approval(&extra), "aster").unwrap_err().detail, details::AGENT_APPROVAL);
        let mut reordered = fixtures::approval_fields();
        reordered.swap(0, 1);
        assert_eq!(
            check(&approval(&reordered), "aster").unwrap_err().detail,
            details::AGENT_APPROVAL
        );
        // The nonce signed is the one prepared, and within a minute.
        let reference =
            edited(fixtures::approval_step(), |s| s["to_sign"]["ref"] = json!("1790000000000001"));
        assert_eq!(check(&reference, "aster").unwrap_err().detail, details::AGENT_NONCE);
        let late = check_at(&fixtures::approval_step(), "aster", EscrowEnvironment::Production, 61);
        assert_eq!(late.unwrap_err().kind, VerificationKind::Stale);
        let elsewhere = edited(fixtures::approval_step(), |s| {
            s["to_sign"]["typed_data"]["domain"]["chainId"] = json!(42161);
        });
        assert_eq!(check(&elsewhere, "aster").unwrap_err().detail, details::DOMAIN);
        // Development approves its own agent only.
        let development =
            |value: &Value| check_at(value, "aster", EscrowEnvironment::Development, 0);
        assert!(development(&approval(&with("agentName", "LaMargeDev"))).is_ok());
        assert_eq!(development(&fixtures::approval_step()).unwrap_err().detail, details::AGENT);
    }

    /// `aster.test.ts` "an agent approval that lists addresses lists a few, narrow, public ones".
    #[test]
    fn listed_addresses_are_few_narrow_and_public() {
        let eight = ["203.0.113.7"; 8].join(",");
        for ips in [
            "203.0.113.7",
            "203.0.113.7,198.51.100.0/24",
            "1.2.3.4 10.0.0.0/24",
            "2001:db8::1",
            "2001:db8:1:2::/64",
            "2001:db8:0:0:0:0:0:1/128",
            &eight,
        ] {
            assert!(narrow_addresses(ips), "{ips}");
            assert!(check(&approval(&with("ipWhitelist", ips)), "aster").is_ok(), "{ips}");
        }
        let nine = ["203.0.113.7"; 9].join(",");
        for ips in [
            "0.0.0.0/0",
            "203.0.113.0/23",
            "0.1.2.3",
            "224.0.0.1",
            "255.255.255.255",
            "::/0",
            "::1",
            "::ffff:1.2.3.4",
            "fe80::1",
            "2001:db8::/63",
            "2001:db8::1%eth0",
            "1.2.3",
            "203.0.113.7/024",
            "203.0.113.7/24/1",
            &nine,
            "2001:db8::1::2",
            "4001::1",
            "01.2.3.4",
            "1.2.3.4,",
            "2001:db8:0:0:0:0:0:0:1",
        ] {
            assert!(!narrow_addresses(ips), "{ips}");
        }
        let wide = approval(&with("ipWhitelist", "0.0.0.0/0"));
        assert_eq!(check(&wide, "aster").unwrap_err().detail, details::AGENT_ADDRESSES);
    }

    /// `account.test.ts` "Lighter's key is registered by the owner's signature of Lighter's own
    /// message", "… at LaMarge's development index, never at another slot of the account, nor at
    /// the other environment's", "a Lighter account that Lighter says is someone else's gets no
    /// key registration".
    #[test]
    fn a_lighter_key_is_registered_by_lighters_own_message_on_the_owners_account() {
        let verified = check(&fixtures::registration_step(), "lighter").unwrap();
        let message = lighter::registration_message(42, 254, &fixtures::LIGHTER_KEY, 9);
        assert_eq!(verified.digest(), alloy_primitives::eip191_hash_message(message.as_bytes()));
        assert_eq!(verified.not_after(), None);
        let registering = |account: u64, key: u8, nonce: u64, reference: &str| {
            let message =
                lighter::registration_message(account, key, &fixtures::LIGHTER_KEY, nonce);
            edited(fixtures::registration_step(), |s| {
                s["to_sign"]["message"] = json!(message);
                s["to_sign"]["ref"] = json!(reference);
            })
        };
        for (value, detail) in [
            (registering(42, 253, 9, "9"), details::LIGHTER_REGISTRATION),
            (registering(42, 4, 9, "9"), details::LIGHTER_REGISTRATION),
            (registering(43, 254, 9, "9"), details::LIGHTER_REGISTRATION),
            (registering(42, 254, 9, "10"), details::LIGHTER_REGISTRATION),
            (registering(42, 254, 9, "+9"), details::LIGHTER_REGISTRATION),
        ] {
            assert_eq!(check(&value, "lighter").unwrap_err().detail, detail);
        }
        let upper = edited(fixtures::registration_step(), |s| {
            let message = s["to_sign"]["message"].as_str().unwrap().replace("0707", "0A07");
            s["to_sign"]["message"] = json!(message);
        });
        assert_eq!(check(&upper, "lighter").unwrap_err().detail, details::LIGHTER_REGISTRATION);
        let development =
            check_at(&registering(42, 253, 9, "9"), "lighter", EscrowEnvironment::Development, 0);
        assert!(development.is_ok());
        // Lighter's word on the account, never LaMarge's record of the link.
        let someone = |owners: &LighterOwners| {
            check_setup_step(
                &step(&fixtures::registration_step()),
                &VenueId::Lighter,
                OWNER,
                Some(42),
                owners,
                EscrowEnvironment::Production,
                fixtures::now(),
            )
        };
        assert_eq!(
            someone(&LighterOwners::from([(42, MAKER)])).unwrap_err().detail,
            details::LIGHTER_OWNER
        );
        assert_eq!(someone(&LighterOwners::new()).unwrap_err().detail, details::LIGHTER_OWNER);
        let unlinked = check_setup_step(
            &step(&fixtures::registration_step()),
            &VenueId::Lighter,
            OWNER,
            None,
            &fixtures::lighter_owners(),
            EscrowEnvironment::Production,
            fixtures::now(),
        );
        let error = unlinked.unwrap_err();
        assert_eq!(
            (error.kind, error.detail),
            (VerificationKind::Changed, details::LINKED_ACCOUNT)
        );
    }
}
