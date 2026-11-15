//! Withdrawal and send checks: Hyperliquid `SendAsset`, Polymarket `Withdraw` and internal
//! transfers, Aster transfers between accounts.

use alloy_primitives::{Address, U256};
use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{
    SKEW_SECONDS,
    details::{self, differs, stale, unsupported},
    differs_unless, near_clock, signed,
};
use crate::{
    deployments,
    error::VerificationError,
    protocol::{
        aster,
        eip712::{self, Message, Withdraw},
        hyperliquid::{self, SendAsset},
        polymarket,
        signing::Verified,
    },
    types::{Amount, VenueId},
};

/// Checks a withdrawal (`POST /v1/withdrawals/prepare`) of exactly `amount` from `signer`'s
/// account on `venue` to `destination` at `at`: on Hyperliquid to the signer's own HyperEVM
/// address through USDC's bridge, on Polymarket to `destination`, without fee. Aster's
/// withdrawals (`Action`) are not supported.
pub(crate) fn check_withdrawal(
    raw: &Value,
    venue: &VenueId,
    signer: Address,
    destination: Address,
    amount: Amount,
    at: DateTime<Utc>,
) -> Result<Verified, VerificationError> {
    let amount = positive(amount)?;
    match venue {
        VenueId::Hyperliquid => {
            let (parsed, not_after) = check_send_asset(raw, amount, at)?;
            let send = &parsed.message;
            if destination != signer
                || !send
                    .destination
                    .eq_ignore_ascii_case(&format!("{:#x}", deployments::HYPERLIQUID_BRIDGE))
            {
                return Err(differs(details::DESTINATION));
            }
            if !["", "spot"].contains(&send.source_dex.as_str()) {
                return Err(differs(details::SOURCE_DEX));
            }
            if send.destination_dex != "spot" {
                return Err(differs(details::DESTINATION_DEX));
            }
            Verified::typed(send.typed_data(), parsed.digest, signer, not_after)
        }
        VenueId::Polymarket => {
            let domain = polymarket::withdraw_domain();
            let parsed = eip712::parse_in::<Withdraw>(raw, &domain)?;
            let withdraw = &parsed.message;
            differs_unless([
                (withdraw.account == signer, details::ACCOUNT),
                (withdraw.token == deployments::POLYMARKET.token, details::TOKEN),
                (withdraw.to == destination, details::TO),
                (withdraw.amount == amount, details::AMOUNT),
                (withdraw.fee.is_zero(), details::FEE),
            ])?;
            let not_after = near_clock(signed(withdraw.ts), at.timestamp(), details::TS)?;
            Verified::typed(eip712::typed(withdraw, domain), parsed.digest, signer, Some(not_after))
        }
        _ => Err(unsupported(details::VENUE)),
    }
}

/// Checks a send (`POST /v1/withdrawals/prepare` with a recipient) of exactly `amount` from
/// `signer`'s account on `venue` to `recipient`, another account of the same venue, at `at`. A
/// Polymarket wallet shows only the operation's hash: it is rebuilt from the typed data's `op`,
/// whose terms are checked.
pub(crate) fn check_send(
    raw: &Value,
    venue: &VenueId,
    signer: Address,
    recipient: Address,
    amount: Amount,
    at: DateTime<Utc>,
) -> Result<Verified, VerificationError> {
    let amount = positive(amount)?;
    if recipient == signer {
        return Err(differs(details::RECIPIENT));
    }
    match venue {
        VenueId::Polymarket => check_polymarket_send(raw, signer, recipient, amount, at),
        VenueId::Hyperliquid => {
            let (parsed, not_after) = check_send_asset(raw, amount, at)?;
            let send = &parsed.message;
            if send.destination != format!("{recipient:#x}")
                || hyperliquid::is_system_address(&send.destination)
            {
                return Err(differs(details::DESTINATION));
            }
            if !["", "spot"].contains(&send.source_dex.as_str()) {
                return Err(differs(details::SOURCE_DEX));
            }
            if !["", "spot"].contains(&send.destination_dex.as_str()) {
                return Err(differs(details::DESTINATION_DEX));
            }
            Verified::typed(send.typed_data(), parsed.digest, signer, not_after)
        }
        VenueId::Aster => {
            let domain = aster::domain(aster::TRANSFER_CHAIN_ID);
            let parsed = eip712::parse_in::<Message>(raw, &domain)?;
            let transfer = aster::parse_transfer(&parsed.message.msg)
                .ok_or(differs(details::ASTER_TRANSFER))?;
            // A send's own id: an `s` and 32 lowercase hex digits, never a fill's or a margin
            // move's.
            let id = transfer.client_id.strip_prefix('s').is_some_and(|hex| {
                hex.len() == 32
                    && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            });
            differs_unless([
                (transfer.user == signer, details::USER),
                (transfer.to == recipient, details::TO_ADDRESS),
                (id, details::CLIENT_TRAN_ID),
                (Some(transfer.amount) == deployments::venue_units(amount, venue), details::AMOUNT),
            ])?;
            // Microsecond-exact, unlike `near_clock`'s whole seconds.
            if !aster::nonce_near(transfer.nonce, at.timestamp(), SKEW_SECONDS) {
                return Err(stale(details::NONCE));
            }
            let not_after = Some(signed(transfer.nonce / 1_000_000).saturating_add(SKEW_SECONDS));
            Verified::typed(
                eip712::typed(&parsed.message, domain),
                parsed.digest,
                signer,
                not_after,
            )
        }
        _ => Err(unsupported(details::VENUE)),
    }
}

/// When a withdrawal or send was signed, from each venue's own unit: what a stored signature is
/// checked again at before it is submitted again.
pub(crate) fn signed_at(raw: &Value) -> Result<DateTime<Utc>, VerificationError> {
    let message = &raw["message"];
    let integer = |name: &str| {
        let value = &message[name];
        value.as_u64().or_else(|| value.as_str().and_then(|text| text.parse().ok()))
    };
    let seconds = match raw["primaryType"].as_str() {
        Some("Op") => integer("ts").map(|ms| ms / 1000),
        Some("HyperliquidTransaction:SendAsset") => integer("nonce").map(|ms| ms / 1000),
        Some("Message") => message["msg"]
            .as_str()
            .and_then(aster::parse_transfer)
            .map(|transfer| transfer.nonce / 1_000_000),
        Some("Withdraw") => integer("ts"),
        _ => None,
    };
    seconds
        .and_then(|seconds| DateTime::from_timestamp(signed(seconds), 0))
        .ok_or(differs(details::TYPED_DATA))
}

/// What a margin withdrawal of `venue` moves out of the account: Hyperliquid and Polymarket move
/// exactly the signed amount; any other venue is refused.
pub(crate) fn withdrawal_cost(raw: &Value, venue: &VenueId) -> Result<Amount, VerificationError> {
    let units = match venue {
        VenueId::Hyperliquid => {
            let parsed = eip712::parse::<SendAsset>(raw)?;
            deployments::decimal_units(&parsed.message.amount, deployments::HYPERLIQUID.decimals)
                .ok_or(differs(details::AMOUNT))?
        }
        VenueId::Polymarket => eip712::parse::<Withdraw>(raw)?.message.amount,
        _ => return Err(unsupported(details::VENUE)),
    };
    Ok(Amount::new(units))
}

fn positive(amount: Amount) -> Result<U256, VerificationError> {
    let units = amount.get();
    if units.is_zero() { Err(differs(details::AMOUNT)) } else { Ok(units) }
}

/// What every Hyperliquid `SendAsset` the SDK signs shares: USDC from the main account on
/// mainnet, exactly `amount`, its nonce within a minute of `at`. Returns it with its signing
/// bound.
fn check_send_asset(
    raw: &Value,
    amount: U256,
    at: DateTime<Utc>,
) -> Result<(eip712::Parsed<SendAsset>, Option<i64>), VerificationError> {
    let parsed = eip712::parse_in::<SendAsset>(raw, &hyperliquid::domain())?;
    let send = &parsed.message;
    let decimals = deployments::HYPERLIQUID.decimals;
    differs_unless([
        (send.hyperliquid_chain == "Mainnet", details::HYPERLIQUID_CHAIN),
        (send.token == deployments::HYPERLIQUID_USDC, details::TOKEN),
        (send.from_sub_account.is_empty(), details::FROM_SUB_ACCOUNT),
        (deployments::decimal_units(&send.amount, decimals) == Some(amount), details::AMOUNT),
    ])?;
    let not_after = near_clock(signed(send.nonce / 1000), at.timestamp(), details::NONCE)?;
    Ok((parsed, Some(not_after)))
}

/// A Polymarket internal transfer: the operation's `op` names the signer's account, pUSD, the
/// recipient and the amount, and its `data` is their hash.
fn check_polymarket_send(
    raw: &Value,
    signer: Address,
    recipient: Address,
    amount: U256,
    at: DateTime<Utc>,
) -> Result<Verified, VerificationError> {
    let domain = polymarket::operation_domain();
    let (parsed, op) = eip712::parse_operation(raw, &domain)?;
    let transfer = polymarket::InternalTransfer::read(op).ok_or(differs(details::OP))?;
    let decimals = deployments::POLYMARKET.decimals;
    differs_unless([
        (eip712::address_text(&transfer.account) == Some(signer), details::ACCOUNT),
        (
            eip712::address_text(&transfer.token) == Some(deployments::POLYMARKET.token),
            details::TOKEN,
        ),
        (eip712::address_text(&transfer.to) == Some(recipient), details::TO),
        (deployments::decimal_units(&transfer.amount, decimals) == Some(amount), details::AMOUNT),
        (transfer.hash() == Some(parsed.message.data), details::DATA),
    ])?;
    let not_after = near_clock(signed(parsed.message.ts / 1000), at.timestamp(), details::TS)?;
    Verified::typed(eip712::typed(&parsed.message, domain), parsed.digest, signer, Some(not_after))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        error::VerificationKind,
        protocol::polymarket::InternalTransfer,
        testing::{MAKER, NOW, OTHER, OWNER, at, edited, fixtures, sample},
    };

    fn units(amount: u128) -> Amount {
        Amount::from(amount)
    }

    fn withdraw(
        raw: &Value,
        venue: &VenueId,
        destination: Address,
        amount: u128,
    ) -> Result<Verified, VerificationError> {
        check_withdrawal(raw, venue, OWNER, destination, units(amount), fixtures::now())
    }

    /// `intents.test.ts` "withdrawal pins exact schema and prevents external destinations or fee
    /// changes", for Polymarket and Hyperliquid.
    #[test]
    fn a_withdrawal_pins_its_destination_amount_and_fee() {
        let pm = |raw: &Value, amount: u128| withdraw(raw, &VenueId::Polymarket, OWNER, amount);
        let verified = pm(&sample("polymarket_withdraw"), 10_000_000).unwrap();
        assert_eq!(verified.not_after(), Some(NOW + 60));
        let message = |name: &str, value: Value| {
            edited(sample("polymarket_withdraw"), |raw| raw["message"][name] = value.clone())
        };
        let cases = [
            (message("to", json!(OTHER)), details::TO),
            (message("fee", json!("1")), details::FEE),
            (message("account", json!(OTHER)), details::ACCOUNT),
            (message("token", json!(OTHER)), details::TOKEN),
            (message("amount", json!("10000001")), details::AMOUNT),
            (message("ts", json!(NOW - 61)), details::TS),
        ];
        for (index, (raw, detail)) in cases.iter().enumerate() {
            assert_eq!(pm(raw, 10_000_000).unwrap_err().detail, *detail, "case {index}");
        }
        assert_eq!(pm(&sample("polymarket_withdraw"), 0).unwrap_err().detail, details::AMOUNT);
        let domain = edited(sample("polymarket_withdraw"), |raw| {
            raw["domain"]["verifyingContract"] = json!(OTHER);
        });
        assert_eq!(pm(&domain, 10_000_000).unwrap_err().detail, details::DOMAIN);
        // A venue wallet destination is the caller's, never the API's.
        assert_eq!(
            withdraw(&sample("polymarket_withdraw"), &VenueId::Polymarket, MAKER, 10_000_000)
                .unwrap_err()
                .detail,
            details::TO
        );

        let hl = |raw: &Value| withdraw(raw, &VenueId::Hyperliquid, OWNER, 12_500_000);
        assert!(hl(&sample("send_asset_withdrawal")).is_ok());
        let message = |name: &str, value: Value| {
            edited(sample("send_asset_withdrawal"), |raw| raw["message"][name] = value.clone())
        };
        let cases = [
            (message("destination", json!(format!("{OWNER:#x}"))), details::DESTINATION),
            (message("sourceDex", json!("xyz")), details::SOURCE_DEX),
            (message("destinationDex", json!("")), details::DESTINATION_DEX),
            (message("hyperliquidChain", json!("Testnet")), details::HYPERLIQUID_CHAIN),
            (message("token", json!("USDC:0x00")), details::TOKEN),
            (message("fromSubAccount", json!(format!("{OTHER:#x}"))), details::FROM_SUB_ACCOUNT),
            (message("amount", json!("12.6")), details::AMOUNT),
            (message("amount", json!("12.5000001")), details::AMOUNT),
            (message("nonce", json!((NOW - 61) * 1000)), details::NONCE),
        ];
        for (index, (raw, detail)) in cases.iter().enumerate() {
            assert_eq!(hl(raw).unwrap_err().detail, *detail, "case {index}");
        }
        // Spot pays too.
        assert!(hl(&message("sourceDex", json!("spot"))).is_ok());
        // A Hyperliquid withdrawal goes to the signer only.
        assert_eq!(
            withdraw(&sample("send_asset_withdrawal"), &VenueId::Hyperliquid, MAKER, 12_500_000)
                .unwrap_err()
                .detail,
            details::DESTINATION
        );
    }

    /// `aster.test.ts` "an Aster withdrawal is pinned to …" and `withdraw.test.ts` "an Aster
    /// withdrawal is refused before any request": Aster's `Action` is deferred.
    #[test]
    fn aster_and_gmx_withdrawals_are_unsupported() {
        for venue in [VenueId::Aster, VenueId::Gmx, VenueId::Lighter] {
            let error = withdraw(&sample("send_asset_withdrawal"), &venue, OWNER, 1).unwrap_err();
            assert_eq!(error.kind, VerificationKind::Unsupported, "{venue}");
            assert_eq!(
                withdrawal_cost(&sample("send_asset_withdrawal"), &venue).unwrap_err().kind,
                VerificationKind::Unsupported
            );
        }
    }

    fn send(
        raw: &Value,
        venue: &VenueId,
        recipient: Address,
        amount: u128,
    ) -> Result<Verified, VerificationError> {
        check_send(raw, venue, OWNER, recipient, units(amount), fixtures::now())
    }

    /// The vector's internal transfer with its `op` arguments replaced and its hash rebuilt.
    fn operation(account: &str, amount: &str, to: &str) -> Value {
        edited(sample("polymarket_internal_transfer"), |raw| {
            raw["op"]["args"]["account"] = json!(account);
            raw["op"]["args"]["amount"] = json!(amount);
            raw["op"]["args"]["to"] = json!(to);
            let transfer = InternalTransfer::read(&raw["op"]).unwrap();
            raw["message"]["data"] = json!(transfer.hash().unwrap());
        })
    }

    /// `send.test.ts` "a Polymarket send is checked by rebuilding the hash its wallet is shown".
    #[test]
    fn a_polymarket_send_is_checked_by_rebuilding_its_hash() {
        let pm = |raw: &Value, amount: u128| send(raw, &VenueId::Polymarket, MAKER, amount);
        let verified = pm(&sample("polymarket_internal_transfer"), 250_500_000).unwrap();
        let raw = sample("polymarket_internal_transfer");
        let served = eip712::parse_operation(&raw, &polymarket::operation_domain()).unwrap().0;
        assert_eq!(verified.digest(), served.digest);
        let owner = "0x84026D3BefF123AAb7DFF957574D1362d6F880A5";
        let maker = "0x669ED91296e0098eb827D85eAE6F2A60c5f14f8d";
        let other = "0x3333333333333333333333333333333333333333";
        assert!(pm(&operation(&owner.to_lowercase(), "250.5", maker), 250_500_000).is_ok());
        let cases = [
            (operation(owner, "250.5", other), 250_500_000, details::TO),
            (operation(owner, "250.5", maker), 250_000_000, details::AMOUNT),
            (operation(maker, "250.5", maker), 250_500_000, details::ACCOUNT),
            (operation(owner, "250.5000001", maker), 250_500_000, details::AMOUNT),
        ];
        for (index, (raw, amount, detail)) in cases.iter().enumerate() {
            assert_eq!(pm(raw, *amount).unwrap_err().detail, *detail, "case {index}");
        }
        // The arguments shown differ from the ones the signed hash covers.
        let swapped = edited(sample("polymarket_internal_transfer"), |raw| {
            raw["op"]["args"]["to"] = json!(other);
        });
        assert_eq!(
            send(&swapped, &VenueId::Polymarket, other.parse().unwrap(), 250_500_000)
                .unwrap_err()
                .detail,
            details::DATA
        );
        let forged = edited(sample("polymarket_internal_transfer"), |raw| {
            raw["message"]["data"] = operation(owner, "250.5", other)["message"]["data"].clone();
        });
        assert_eq!(pm(&forged, 250_500_000).unwrap_err().detail, details::DATA);
        let withdraw_op = edited(sample("polymarket_internal_transfer"), |raw| {
            raw["op"]["type"] = json!("withdraw");
        });
        assert_eq!(pm(&withdraw_op, 250_500_000).unwrap_err().detail, details::OP);
        let token = edited(sample("polymarket_internal_transfer"), |raw| {
            raw["op"]["args"]["token"] = json!(other);
            raw["message"]["data"] =
                json!(InternalTransfer::read(&raw["op"]).unwrap().hash().unwrap());
        });
        assert_eq!(pm(&token, 250_500_000).unwrap_err().detail, details::TOKEN);
        let contract = edited(sample("polymarket_internal_transfer"), |raw| {
            raw["domain"]["verifyingContract"] =
                json!("0xdca4af75705dbb50f62437045aff9921947917d2");
        });
        assert_eq!(pm(&contract, 250_500_000).unwrap_err().detail, details::DOMAIN);
        let stale = edited(sample("polymarket_internal_transfer"), |raw| {
            raw["message"]["ts"] = json!((NOW - 120) * 1000);
        });
        assert_eq!(pm(&stale, 250_500_000).unwrap_err().detail, details::TS);
        assert_eq!(
            send(&sample("polymarket_internal_transfer"), &VenueId::Polymarket, OWNER, 250_500_000)
                .unwrap_err()
                .detail,
            details::RECIPIENT
        );
    }

    /// `send.test.ts` "a Hyperliquid send names its recipient, mainnet and the two dexes" and
    /// "trade.xyz pays a Hyperliquid send only when asked" (trade.xyz is deferred here).
    #[test]
    fn a_hyperliquid_send_names_its_recipient_mainnet_and_the_two_dexes() {
        let hl = |raw: &Value, recipient: Address| {
            send(raw, &VenueId::Hyperliquid, recipient, 5_250_000)
        };
        assert!(hl(&sample("send_asset_send"), MAKER).is_ok());
        let message = |name: &str, value: Value| {
            edited(sample("send_asset_send"), |raw| raw["message"][name] = value.clone())
        };
        let cases = [
            (message("destination", json!(format!("0x{}", "33".repeat(20)))), details::DESTINATION),
            (
                message("destination", json!("0x2000000000000000000000000000000000000000")),
                details::DESTINATION,
            ),
            // The recipient exactly as written: lowercase.
            (
                message("destination", json!("0x669ED91296e0098eb827D85eAE6F2A60c5f14f8d")),
                details::DESTINATION,
            ),
            (message("hyperliquidChain", json!("Testnet")), details::HYPERLIQUID_CHAIN),
            (message("destinationDex", json!("xyz")), details::DESTINATION_DEX),
            (message("sourceDex", json!("xyz")), details::SOURCE_DEX),
            (message("amount", json!("5.3")), details::AMOUNT),
            (message("nonce", json!((NOW - 120) * 1000)), details::NONCE),
        ];
        for (index, (raw, detail)) in cases.iter().enumerate() {
            assert_eq!(hl(raw, MAKER).unwrap_err().detail, *detail, "case {index}");
        }
        assert!(hl(&message("destinationDex", json!("spot")), MAKER).is_ok());
        // A system address is never a recipient, even when asked.
        let index = "0x2000000000000000000000000000000000010000";
        assert_eq!(
            hl(&message("destination", json!(index)), index.parse().unwrap()).unwrap_err().detail,
            details::DESTINATION
        );
        let own = message("destination", json!(format!("{OWNER:#x}")));
        assert_eq!(hl(&own, OWNER).unwrap_err().detail, details::RECIPIENT);
    }

    fn aster_send(id: &str, to: Address, amount: &str) -> Value {
        let fields = [
            ("asset", "USDT".to_owned()),
            ("amount", amount.to_owned()),
            ("toAddress", format!("{to:#x}")),
            ("clientTranId", id.to_owned()),
            ("nonce", (NOW * 1_000_000).to_string()),
            ("user", format!("{OWNER:#x}")),
        ];
        fixtures::aster_request(&fields, 1666)
    }

    /// `send.test.ts` "an Aster send carries a send's own id, never a fill's or a margin move's".
    #[test]
    fn an_aster_send_carries_a_sends_own_id() {
        let id = format!("s{}", "ab".repeat(16));
        let five = 5_000_000_000_000_000_000;
        let aster = |raw: &Value, amount: u128| send(raw, &VenueId::Aster, MAKER, amount);
        assert!(aster(&aster_send(&id, MAKER, "5"), five).is_ok());
        let cases = [
            (aster_send(&"ab".repeat(16), MAKER, "5"), details::CLIENT_TRAN_ID),
            (aster_send(&format!("m{}", "ab".repeat(16)), MAKER, "5"), details::CLIENT_TRAN_ID),
            (aster_send(&format!("s{}", "AB".repeat(16)), MAKER, "5"), details::CLIENT_TRAN_ID),
            (aster_send(&id, OTHER, "5"), details::TO_ADDRESS),
            (aster_send(&id, MAKER, "6"), details::AMOUNT),
        ];
        for (index, (raw, detail)) in cases.iter().enumerate() {
            assert_eq!(aster(raw, five).unwrap_err().detail, *detail, "case {index}");
        }
        // Rounded up to whole millionths, as Aster moves it.
        assert!(aster(&aster_send(&id, MAKER, "5.000001"), five + 1).is_ok());
        let user = edited(sample("aster_transfer"), |raw| {
            let msg = aster_send(&id, MAKER, "5")["message"]["msg"]
                .as_str()
                .unwrap()
                .replace(&format!("{OWNER:#x}"), &format!("{OTHER:#x}"));
            raw["message"]["msg"] = json!(msg);
        });
        assert_eq!(aster(&user, five).unwrap_err().detail, details::USER);
        let stale = edited(sample("aster_transfer"), |raw| {
            let msg = aster_send(&id, MAKER, "5")["message"]["msg"]
                .as_str()
                .unwrap()
                .replace(&(NOW * 1_000_000).to_string(), &((NOW - 61) * 1_000_000).to_string());
            raw["message"]["msg"] = json!(msg);
        });
        assert_eq!(aster(&stale, five).unwrap_err().detail, details::NONCE);
        let garbled =
            edited(sample("aster_transfer"), |raw| raw["message"]["msg"] = json!("asset=USDT"));
        assert_eq!(aster(&garbled, five).unwrap_err().detail, details::ASTER_TRANSFER);
        let approval = aster_send(&id, MAKER, "5");
        let mut bsc = approval.clone();
        bsc["domain"]["chainId"] = json!(56);
        assert_eq!(aster(&bsc, five).unwrap_err().detail, details::DOMAIN);
    }

    /// `send.test.ts` "signing times read each venue's own unit".
    #[test]
    fn signing_times_read_each_venues_own_unit() {
        let aster = aster_send(&format!("s{}", "ab".repeat(16)), MAKER, "5");
        for raw in [
            sample("polymarket_internal_transfer"),
            sample("send_asset_send"),
            sample("polymarket_withdraw"),
            aster,
        ] {
            assert_eq!(signed_at(&raw).unwrap(), at(0), "{}", raw["primaryType"]);
        }
        assert_eq!(signed_at(&sample("fund_offer")).unwrap_err().detail, details::TYPED_DATA);
        // A stored signature is checked again at its own signing time.
        let stored = sample("send_asset_withdrawal");
        let later = check_withdrawal(
            &stored,
            &VenueId::Hyperliquid,
            OWNER,
            OWNER,
            units(12_500_000),
            at(3600),
        );
        assert_eq!(later.unwrap_err().kind, VerificationKind::Stale);
        assert!(
            check_withdrawal(
                &stored,
                &VenueId::Hyperliquid,
                OWNER,
                OWNER,
                units(12_500_000),
                signed_at(&stored).unwrap()
            )
            .is_ok()
        );
    }

    #[test]
    fn a_withdrawals_cost_is_its_signed_amount() {
        assert_eq!(
            withdrawal_cost(&sample("send_asset_withdrawal"), &VenueId::Hyperliquid).unwrap(),
            units(12_500_000)
        );
        assert_eq!(
            withdrawal_cost(&sample("polymarket_withdraw"), &VenueId::Polymarket).unwrap(),
            units(10_000_000)
        );
        let garbled = edited(sample("send_asset_withdrawal"), |raw| {
            raw["message"]["amount"] = json!("12.5000001");
        });
        assert_eq!(
            withdrawal_cost(&garbled, &VenueId::Hyperliquid).unwrap_err().detail,
            details::AMOUNT
        );
        assert_eq!(
            withdrawal_cost(&sample("polymarket_withdraw"), &VenueId::Hyperliquid)
                .unwrap_err()
                .detail,
            details::PRIMARY_TYPE
        );
    }
}
