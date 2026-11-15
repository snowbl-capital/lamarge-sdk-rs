//! Prepared quote line checks: every pool's quote set is rebuilt from the caller's lines before
//! it is signed.

use std::collections::{BTreeMap, HashSet};

use alloy_primitives::{Address, U256};
use chrono::{DateTime, Utc};

use super::{
    details::{self, at, changed, differs, stale},
    differs_unless,
    maker::{self, QuoteTerms},
    signed,
};
use crate::{
    deployments,
    error::VerificationError,
    model::{Order, OrderState, QuoteLine, QuoteLineDraft, QuoteLinesPreparation},
    protocol::{fees, signing::Verified},
    types::{Amount, EscrowEnvironment, Price, QuoteIdentityId, Revision, VenueId},
};

/// The caller's lines a preparation is checked against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinesRequest<'a> {
    pub(crate) lines: &'a [QuoteLine],
    /// The last admission time, Unix seconds.
    pub(crate) valid_until: u64,
    /// The published lines' revision the publication replaces.
    pub(crate) expected_revision: Revision,
}

/// The quote identity publishing, its quote signer, its pools as last read, and the maker's
/// payment recipient on each venue it buys on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LinesAuthority<'a> {
    pub(crate) identity: QuoteIdentityId,
    pub(crate) quote_signer: Address,
    pub(crate) pools: &'a [Order],
    pub(crate) recipients: &'a BTreeMap<VenueId, Address>,
}

/// What `stock` base units sold on `supply` at `sell` cost on `paying` bought at `buy`, rounded
/// up for the maker; `None` for an unpinned venue or beyond `2^256 - 1`.
pub(crate) fn line_receive(
    stock: U256,
    sell: Price,
    buy: Price,
    paying: &VenueId,
    supply: &VenueId,
) -> Option<U256> {
    let scale = |price: Price, venue: &VenueId| {
        let decimals = deployments::chain(venue)?.decimals;
        U256::from(price.units()).checked_mul(U256::from(10u8).pow(U256::from(decimals)))
    };
    fees::mul_div_ceil(stock, scale(sell, paying)?, scale(buy, supply)?)
}

/// Whether a pool on `supply` may quote takers paying on `paying`: another venue, and never
/// between the two venues settled inside themselves.
pub(crate) fn direct_pair(paying: &VenueId, supply: &VenueId) -> bool {
    paying != supply
        && !(deployments::is_venue_settled(paying) && deployments::is_venue_settled(supply))
}

/// Checks a lines preparation (`POST /v1/quotes/lines/prepare`) and returns one payload per draft,
/// in draft order. The echoed lines are the caller's, each live pool of the identity with stock
/// signs exactly the set its line builds (receiving where the caller buys, at the caller's
/// prices, to the caller's recipients), and only pools without stock are cancelled. Omitting a
/// receiving venue is allowed; changing a price, stock, minimum or recipient is not. Each payload
/// is signable until a second before `valid_until`, refused once `now` reaches it.
pub(crate) fn check_prepared_lines(
    prepared: &QuoteLinesPreparation,
    request: &LinesRequest<'_>,
    authority: &LinesAuthority<'_>,
    environment: EscrowEnvironment,
    now: DateTime<Utc>,
) -> Result<Vec<Verified>, VerificationError> {
    if signed(request.valid_until) <= now.timestamp() {
        return Err(stale(details::VALID_UNTIL));
    }
    let asked = lines(request.lines)?;
    differs_unless([
        (prepared.quote_identity_id == authority.identity, details::QUOTE_IDENTITY_ID),
        (prepared.expected_revision == request.expected_revision, details::EXPECTED_REVISION),
        (prepared.valid_until == request.valid_until, details::VALID_UNTIL),
        (lines(&prepared.lines)? == asked, details::LINES),
    ])?;
    let pools: Vec<&Order> = authority
        .pools
        .iter()
        .filter(|pool| {
            pool.capital_pool
                && pool.quote_identity_id == authority.identity
                && !matches!(pool.state, OrderState::Closed | OrderState::Failed)
        })
        .collect();
    let stocked =
        |pool: &&Order| asked.get(&pool.venue).is_some_and(|line| !line.stock.get().is_zero());
    let (with_stock, idle): (Vec<&Order>, Vec<&Order>) = pools.into_iter().partition(stocked);
    let drafted: HashSet<_> = prepared.drafts.iter().map(|draft| draft.order_id).collect();
    if prepared.drafts.len() != with_stock.len() || drafted.len() != with_stock.len() {
        return Err(differs(details::DRAFTS));
    }
    let verified = prepared
        .drafts
        .iter()
        .enumerate()
        .map(|(index, draft)| {
            check_draft(draft, &with_stock, &asked, request, authority, environment)
                .map_err(|error| at(error, index))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !prepared.cancels.iter().all(|id| idle.iter().any(|pool| pool.id == *id)) {
        return Err(differs(details::CANCELS));
    }
    Ok(verified)
}

/// One pool's draft: the quote set its line builds for each venue the caller buys on.
fn check_draft(
    draft: &QuoteLineDraft,
    pools: &[&Order],
    asked: &BTreeMap<&VenueId, &QuoteLine>,
    request: &LinesRequest<'_>,
    authority: &LinesAuthority<'_>,
    environment: EscrowEnvironment,
) -> Result<Verified, VerificationError> {
    let pool = pools
        .iter()
        .find(|pool| pool.id == draft.order_id)
        .ok_or(differs(details::DRAFTS_ORDER_ID))?;
    let line = asked.get(&pool.venue).ok_or(differs(details::DRAFTS_ORDER_ID))?;
    let sell = line.sell.ok_or(differs(details::LINES))?;
    let stock = line.stock.get();
    let one_token = deployments::chain(&pool.venue)
        .map(|chain| U256::from(10u8).pow(U256::from(chain.decimals)))
        .ok_or(differs(details::DRAFTS_VENUE))?;
    let min_fill = line.min_fill.map_or(one_token, Amount::get);
    if draft.venue != pool.venue {
        return Err(differs(details::DRAFTS_VENUE));
    }
    if pool.state != OrderState::Live {
        return Err(changed(details::POOL_STATE));
    }
    if pool.quote_signer != authority.quote_signer {
        return Err(changed(details::POOL_QUOTE_SIGNER));
    }
    let venues: HashSet<_> = draft.quotes.iter().map(|quote| &quote.venue).collect();
    if draft.quotes.is_empty() || venues.len() != draft.quotes.len() {
        return Err(differs(details::DRAFTS_QUOTES));
    }
    let quotes = draft
        .quotes
        .iter()
        .map(|quote| {
            let buy = asked
                .get(&quote.venue)
                .and_then(|line| line.buy)
                .filter(|_| direct_pair(&quote.venue, &pool.venue))
                .ok_or(differs(details::DRAFTS_QUOTES_VENUE))?;
            let recipient = *authority
                .recipients
                .get(&quote.venue)
                .ok_or(differs(details::DRAFTS_QUOTES_RECIPIENT))?;
            let receive = line_receive(stock, sell, buy, &quote.venue, &pool.venue)
                .ok_or(differs(details::DRAFTS_QUOTES_RECEIVE))?;
            Ok(QuoteTerms {
                venue: quote.venue.clone(),
                recipient,
                supply: stock,
                receive,
                min_fill,
            })
        })
        .collect::<Result<Vec<_>, VerificationError>>()?;
    check_prepared_quote(draft, pool, &quotes, request.valid_until, line.stock, environment)
}

/// A draft's terms and its one quote set: `quotes` (built from the draft's own quotes, one each)
/// exactly, `size` as its budget, valid until `valid_until` and settling an hour and a second
/// later, at a sequence above the pool's last and at most [`maker::MAX_SEQUENCE`].
fn check_prepared_quote(
    draft: &QuoteLineDraft,
    pool: &Order,
    quotes: &[QuoteTerms],
    valid_until: u64,
    size: Amount,
    environment: EscrowEnvironment,
) -> Result<Verified, VerificationError> {
    let expires_at = valid_until.checked_add(deployments::MAKER_TIMEOUT_SECONDS + 1);
    differs_unless([
        (draft.valid_until == valid_until, details::VALID_UNTIL),
        (draft.quote_size == size, details::QUOTE_SIZE),
        (Some(draft.expires_at) == expires_at, details::EXPIRES_AT),
        (
            pool.last_sequence < draft.sequence && draft.sequence <= maker::MAX_SEQUENCE,
            details::SEQUENCE,
        ),
    ])?;
    let [item] = &draft.to_sign[..] else {
        return Err(differs(details::TO_SIGN));
    };
    for (quote, expected) in draft.quotes.iter().zip(quotes) {
        differs_unless([
            (quote.recipient == expected.recipient, details::DRAFTS_QUOTES_RECIPIENT),
            (quote.supply.get() == expected.supply, details::DRAFTS_QUOTES_SUPPLY),
            (quote.receive.get() == expected.receive, details::DRAFTS_QUOTES_RECEIVE),
            (quote.min_fill.get() == expected.min_fill, details::DRAFTS_QUOTES_MIN_FILL),
        ])?;
    }
    maker::check_signed_quote(
        item,
        pool,
        quotes,
        draft.sequence,
        valid_until,
        draft.expires_at,
        environment,
    )
}

/// Lines by venue: each venue once, a buy price at most its sell price, stock only with a sell
/// price.
fn lines(lines: &[QuoteLine]) -> Result<BTreeMap<&VenueId, &QuoteLine>, VerificationError> {
    let mut by_venue = BTreeMap::new();
    for line in lines {
        let priced = match (line.buy, line.sell) {
            (Some(buy), Some(sell)) => buy <= sell,
            _ => true,
        };
        let stocked = line.stock.get().is_zero() || line.sell.is_some();
        if !priced || !stocked || by_venue.insert(&line.venue, line).is_some() {
            return Err(differs(details::LINES));
        }
    }
    Ok(by_venue)
}

#[cfg(test)]
mod tests {
    use alloy_signer_local::PrivateKeySigner;
    use serde_json::{Value, json};

    use super::*;
    use crate::{
        error::VerificationKind,
        testing::{
            self, MAKER, OTHER,
            fixtures::{self, nico},
        },
    };

    fn price(text: &str) -> Price {
        text.parse().unwrap()
    }

    fn check(prepared: &Value, lines: &[Value]) -> Result<Vec<Verified>, VerificationError> {
        fixtures::lines_check(prepared, lines, &fixtures::recipients())
    }

    fn unchanged(_: &str, _: &mut Vec<Value>) {}

    /// `lines.test.ts` "the compiled terms match the server's vectors".
    #[test]
    fn the_compiled_terms_match_the_servers_vectors() {
        let receive = |stock: U256, sell: &str, buy: &str, paying: VenueId, supply: VenueId| {
            line_receive(stock, price(sell), price(buy), &paying, &supply).unwrap()
        };
        assert_eq!(
            receive(
                U256::from(100_000_000u64),
                "1.0014",
                "0.9994",
                VenueId::Hyperliquid,
                VenueId::Polymarket
            ),
            U256::from(100_200_121u64)
        );
        assert_eq!(
            receive(
                U256::from(10u8).pow(U256::from(20u8)),
                "1.0012",
                "1",
                VenueId::Hyperliquid,
                VenueId::Aster
            ),
            U256::from(100_120_000u64)
        );
        assert_eq!(
            receive(
                U256::from(100_000_000u64),
                "1.0012",
                "0.9995",
                VenueId::Aster,
                VenueId::Polymarket
            ),
            "100170085042521260631".parse::<U256>().unwrap()
        );
        // Near the largest integer the price refuses rather than wraps.
        assert_eq!(
            line_receive(
                U256::MAX,
                price("1.1"),
                price("0.9"),
                &VenueId::Aster,
                &VenueId::Hyperliquid
            ),
            None
        );
        assert!(
            line_receive(
                U256::MAX,
                price("1"),
                price("1"),
                &VenueId::Hyperliquid,
                &VenueId::Polymarket
            )
            .is_some()
        );
        assert_eq!(
            line_receive(
                U256::ONE,
                price("1"),
                price("1"),
                &"dydx".parse().unwrap(),
                &VenueId::Polymarket
            ),
            None
        );
    }

    #[test]
    fn lighter_and_aster_never_pair_directly() {
        assert!(direct_pair(&VenueId::Lighter, &VenueId::Hyperliquid));
        assert!(direct_pair(&VenueId::Hyperliquid, &VenueId::Aster));
        assert!(!direct_pair(&VenueId::Lighter, &VenueId::Aster));
        assert!(!direct_pair(&VenueId::Polymarket, &VenueId::Polymarket));
    }

    /// `lines.test.ts` "every pool's set is rebuilt from the lines before signing".
    #[test]
    fn every_pools_set_is_rebuilt_from_the_lines() {
        let verified = check(&fixtures::lines_preparation(&nico(), unchanged), &nico()).unwrap();
        assert_eq!(verified.len(), 2);
        let empty = fixtures::lines_preparation(&nico(), |_, quotes| quotes.clear());
        assert_eq!(check(&empty, &nico()).unwrap_err().detail, details::DRAFTS_QUOTES);
        // A receiving venue without a pool, its quotes omitted.
        let mut lighter = nico();
        lighter.push(json!({"venue": "lighter", "buy": "0.9997", "sell": null, "stock": "0"}));
        let omitted = fixtures::lines_preparation(&lighter, |_, quotes| {
            quotes.retain(|quote| quote["venue"] != "lighter");
        });
        assert!(check(&omitted, &lighter).is_ok());
        assert!(check(&fixtures::lines_preparation(&lighter, unchanged), &lighter).is_ok());
        // Without a recipient there, nothing is quoted to receive there.
        let recipients: BTreeMap<VenueId, Address> = fixtures::recipients()
            .into_iter()
            .filter(|(venue, _)| *venue != VenueId::Lighter)
            .collect();
        let error = fixtures::lines_check(
            &fixtures::lines_preparation(&lighter, unchanged),
            &lighter,
            &recipients,
        )
        .unwrap_err();
        assert_eq!(error.detail, details::DRAFTS_QUOTES_RECIPIENT);
    }

    /// `lines.test.ts` "a price, recipient, budget or pool other than the lines' is refused".
    #[test]
    fn a_price_recipient_budget_or_pool_other_than_the_lines_is_refused() {
        let edit = |field: &'static str, value: fn(&Value) -> Value| {
            fixtures::lines_preparation(&nico(), move |_, quotes| {
                for quote in quotes.iter_mut() {
                    quote[field] = value(&quote[field]);
                }
            })
        };
        let less = |value: &Value| {
            json!((value.as_str().unwrap().parse::<U256>().unwrap() - U256::ONE).to_string())
        };
        let twice = |value: &Value| {
            json!((value.as_str().unwrap().parse::<U256>().unwrap() * U256::from(2u8)).to_string())
        };
        let cases = [
            (edit("receive", less), details::DRAFTS_QUOTES_RECEIVE),
            (edit("recipient", |_| json!(MAKER)), details::DRAFTS_QUOTES_RECIPIENT),
            (edit("supply", twice), details::DRAFTS_QUOTES_SUPPLY),
            (edit("min_fill", twice), details::DRAFTS_QUOTES_MIN_FILL),
            (edit("venue", |_| json!("gmx")), details::DRAFTS_QUOTES_VENUE),
        ];
        for (index, (prepared, detail)) in cases.iter().enumerate() {
            let error = check(prepared, &nico()).unwrap_err();
            assert_eq!((error.detail, error.index), (*detail, Some(0)), "case {index}");
        }
        let mut changed = fixtures::lines_preparation(&nico(), unchanged);
        changed["lines"][0]["sell"] = json!("1.0001");
        assert_eq!(check(&changed, &nico()).unwrap_err().detail, details::LINES);
        let unstocked: Vec<Value> = nico()
            .into_iter()
            .map(|mut line| {
                if line["venue"] == "polymarket" {
                    line["stock"] = json!("0");
                }
                line
            })
            .collect();
        let mut echoed = fixtures::lines_preparation(&nico(), unchanged);
        echoed["lines"] = json!(unstocked);
        assert_eq!(check(&echoed, &unstocked).unwrap_err().detail, details::DRAFTS);
        let mut cancelled = fixtures::lines_preparation(&nico(), unchanged);
        cancelled["cancels"] = json!([fixtures::line_pools()[0]["id"]]);
        assert_eq!(check(&cancelled, &nico()).unwrap_err().detail, details::CANCELS);
        // A pool without stock may be cancelled.
        let mut idle = fixtures::lines_preparation(&unstocked, unchanged);
        idle["cancels"] = json!([fixtures::line_pools()[1]["id"]]);
        assert!(check(&idle, &unstocked).is_ok());
    }

    /// `lines.test.ts` "a line's minimum fill is signed as asked, one token by default".
    #[test]
    fn a_lines_minimum_fill_is_signed_as_asked() {
        let larger: Vec<Value> = nico()
            .into_iter()
            .map(|mut line| {
                if line["venue"] == "hyperliquid" {
                    line["min_fill"] = json!("25000000");
                }
                line
            })
            .collect();
        let asked = fixtures::lines_preparation(&larger, |venue, quotes| {
            if venue == "hyperliquid" {
                for quote in quotes.iter_mut() {
                    quote["min_fill"] = json!("25000000");
                }
            }
        });
        // The signed set carries the draft's minimum: rebuild it from the draft's quotes.
        let asked = resign(asked);
        assert!(check(&asked, &larger).is_ok());
        assert_eq!(
            check(&fixtures::lines_preparation(&larger, unchanged), &larger).unwrap_err().detail,
            details::DRAFTS_QUOTES_MIN_FILL
        );
    }

    /// Copies each draft's quotes into its signed set, as the server signs what it drafts.
    fn resign(mut prepared: Value) -> Value {
        for draft in prepared["drafts"].as_array_mut().unwrap() {
            let quotes = draft["quotes"].clone();
            for (signed, quote) in draft["to_sign"][0]["typed_data"]["message"]["quotes"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .zip(quotes.as_array().unwrap())
            {
                signed["minFill"] = quote["min_fill"].clone();
            }
        }
        prepared
    }

    #[test]
    fn the_preparation_answers_this_identity_revision_and_validity() {
        let edited = |edit: fn(&mut Value)| {
            let mut prepared = fixtures::lines_preparation(&nico(), unchanged);
            edit(&mut prepared);
            prepared
        };
        let cases = [
            (edited(|p| p["quote_identity_id"] = json!(testing::FILL)), details::QUOTE_IDENTITY_ID),
            (edited(|p| p["expected_revision"] = json!("4")), details::EXPECTED_REVISION),
            (edited(|p| p["valid_until"] = json!(1)), details::VALID_UNTIL),
            (edited(|p| p["drafts"][0]["valid_until"] = json!(1)), details::VALID_UNTIL),
            (edited(|p| p["drafts"][0]["quote_size"] = json!("1")), details::QUOTE_SIZE),
            (
                edited(|p| p["drafts"][0]["expires_at"] = json!(fixtures::VALID_UNTIL + 3600)),
                details::EXPIRES_AT,
            ),
            (edited(|p| p["drafts"][0]["sequence"] = json!(1)), details::SEQUENCE),
            (edited(|p| p["drafts"][0]["venue"] = json!("gmx")), details::DRAFTS_VENUE),
            (
                edited(|p| p["drafts"][0]["order_id"] = json!(testing::ORDER)),
                details::DRAFTS_ORDER_ID,
            ),
            (edited(|p| p["drafts"][0]["to_sign"] = json!([])), details::TO_SIGN),
            (
                edited(|p| {
                    p["drafts"][0]["to_sign"][0]["typed_data"]["message"]["sequence"] = json!(3);
                }),
                details::SEQUENCE,
            ),
            (
                edited(|p| p["drafts"][0]["to_sign"][0]["signer"] = json!(OTHER)),
                details::TO_SIGN_SIGNER,
            ),
            (
                edited(|p| {
                    let quote = p["drafts"][0]["quotes"][0].clone();
                    p["drafts"][0]["quotes"].as_array_mut().unwrap().push(quote);
                }),
                details::DRAFTS_QUOTES,
            ),
            (
                edited(|p| {
                    let draft = p["drafts"][0].clone();
                    p["drafts"].as_array_mut().unwrap().push(draft);
                }),
                details::DRAFTS,
            ),
        ];
        for (index, (prepared, detail)) in cases.iter().enumerate() {
            assert_eq!(check(prepared, &nico()).unwrap_err().detail, *detail, "case {index}");
        }
        // Inconsistent lines, asked or echoed.
        let mut twice = nico();
        twice.push(nico()[0].clone());
        assert_eq!(
            check(&fixtures::lines_preparation(&nico(), unchanged), &twice).unwrap_err().detail,
            details::LINES
        );
        let mut inverted = nico();
        inverted[0]["buy"] = json!("1.0003");
        assert_eq!(
            check(&fixtures::lines_preparation(&nico(), unchanged), &inverted).unwrap_err().detail,
            details::LINES
        );
        let mut unpriced = nico();
        unpriced[1]["sell"] = Value::Null;
        assert_eq!(
            check(&fixtures::lines_preparation(&nico(), unchanged), &unpriced).unwrap_err().detail,
            details::LINES
        );
    }

    #[test]
    fn a_drafts_sequence_stays_within_the_protocol_ceiling() {
        let at_sequence = |sequence: u64| {
            let mut prepared = fixtures::lines_preparation(&nico(), unchanged);
            for draft in prepared["drafts"].as_array_mut().unwrap() {
                draft["sequence"] = json!(sequence);
                draft["to_sign"][0]["typed_data"]["message"]["sequence"] = json!(sequence);
            }
            check(&prepared, &nico())
        };
        assert!(at_sequence(maker::MAX_SEQUENCE).is_ok());
        for sequence in [maker::MAX_SEQUENCE + 1, u64::MAX] {
            assert_eq!(at_sequence(sequence).unwrap_err().detail, details::SEQUENCE, "{sequence}");
        }
    }

    #[test]
    fn quote_sets_are_signed_before_the_last_admission_time() {
        let key = PrivateKeySigner::random();
        let signer = key.address();
        let mut prepared = fixtures::lines_preparation(&nico(), unchanged);
        for draft in prepared["drafts"].as_array_mut().unwrap() {
            draft["to_sign"][0]["signer"] = json!(signer);
        }
        let prepared: QuoteLinesPreparation = fixtures::model(&prepared);
        let lines: Vec<QuoteLine> = fixtures::model(&Value::Array(nico()));
        let mut pools = fixtures::line_pools();
        for pool in &mut pools {
            pool["quote_signer"] = json!(signer);
        }
        let pools: Vec<Order> = fixtures::model(&Value::Array(pools));
        let recipients = fixtures::recipients();
        let request = LinesRequest {
            lines: &lines,
            valid_until: fixtures::VALID_UNTIL,
            expected_revision: Revision::new(3),
        };
        let authority = LinesAuthority {
            identity: testing::IDENTITY.parse().unwrap(),
            quote_signer: signer,
            pools: &pools,
            recipients: &recipients,
        };
        let check_at = |offset: i64| {
            let now = testing::at(offset);
            check_prepared_lines(
                &prepared,
                &request,
                &authority,
                EscrowEnvironment::Production,
                now,
            )
        };
        let expiry = i64::try_from(fixtures::VALID_UNTIL).unwrap() - testing::NOW;
        // Expired before signing.
        let error = check_at(expiry).unwrap_err();
        assert_eq!((error.kind, error.detail), (VerificationKind::Stale, details::VALID_UNTIL));
        let verified = check_at(expiry - 1).unwrap();
        assert_eq!(verified.len(), 2);
        for payload in &verified {
            assert_eq!(payload.not_after(), Some(expiry + testing::NOW - 1));
            // Expired while signing: a signer returning at `valid_until` gives no signature.
            assert!(fixtures::sign_until(key.clone(), payload, expiry - 1).is_ok());
            let late = fixtures::sign_until(key.clone(), payload, expiry).unwrap_err();
            assert_eq!(
                (late.kind, late.detail),
                (VerificationKind::Stale, details::SIGNING_WINDOW)
            );
        }
    }

    #[test]
    fn only_the_identitys_live_pools_with_its_quote_signer_are_drafted() {
        let prepared: QuoteLinesPreparation =
            fixtures::model(&fixtures::lines_preparation(&nico(), unchanged));
        let lines: Vec<QuoteLine> = fixtures::model(&Value::Array(nico()));
        let recipients = fixtures::recipients();
        let with_pools = |edit: fn(&mut Value)| {
            let mut pools = fixtures::line_pools();
            edit(&mut pools[0]);
            let pools: Vec<Order> = fixtures::model(&Value::Array(pools));
            let request = LinesRequest {
                lines: &lines,
                valid_until: fixtures::VALID_UNTIL,
                expected_revision: Revision::new(3),
            };
            let authority = LinesAuthority {
                identity: testing::IDENTITY.parse().unwrap(),
                quote_signer: MAKER,
                pools: &pools,
                recipients: &recipients,
            };
            check_prepared_lines(
                &prepared,
                &request,
                &authority,
                EscrowEnvironment::Production,
                fixtures::now(),
            )
        };
        assert!(with_pools(|_| {}).is_ok());
        let error = with_pools(|pool| pool["state"] = json!("funding")).unwrap_err();
        assert_eq!((error.kind, error.detail), (VerificationKind::Changed, details::POOL_STATE));
        assert_eq!(
            with_pools(|pool| pool["quote_signer"] = json!(OTHER)).unwrap_err().detail,
            details::POOL_QUOTE_SIGNER
        );
        // Another identity's, a closed one or not the current pool: not drafted.
        for edit in [
            (|pool: &mut Value| pool["quote_identity_id"] = json!(testing::FILL)) as fn(&mut Value),
            |pool| pool["state"] = json!("closed"),
            |pool| pool["capital_pool"] = json!(false),
        ] {
            assert_eq!(with_pools(edit).unwrap_err().detail, details::DRAFTS);
        }
    }
}
