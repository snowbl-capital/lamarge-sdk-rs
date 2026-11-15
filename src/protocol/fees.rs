//! Taker fee rules: the fee a swap id carries, the fee check, and the exact fee, price and
//! ratio arithmetic the checks and book math share.

use alloy_primitives::{B256, U256, U512, hex, ruint::UintTryFrom};

use super::checks::details::{self, differs};
use crate::{deployments, error::VerificationError, types::Bps, types::VenueId};

/// The escrow's tags: a swap id starting with one carries the fee in its next ten bytes.
const FEE_ON_PAYMENT: [u8; 6] = hex!("de65f326692b");
const FEE_ON_DELIVERY: [u8; 6] = hex!("cc909c08081a");
/// Their wallet twins, which a wallet delivery's swap id always carries, fee zero included.
const WALLET_FEE_ON_PAYMENT: [u8; 6] = hex!("1b4c4374729e");
const WALLET_FEE_ON_DELIVERY: [u8; 6] = hex!("fcb0d9ddb285");

/// The tags under the names `pins.json` gives them.
#[cfg(test)]
pub(crate) const TAGS_FOR_PINS: [(&str, [u8; 6]); 4] = [
    ("fee_on_payment", FEE_ON_PAYMENT),
    ("fee_on_delivery", FEE_ON_DELIVERY),
    ("wallet_fee_on_payment", WALLET_FEE_ON_PAYMENT),
    ("wallet_fee_on_delivery", WALLET_FEE_ON_DELIVERY),
];

/// A take's taker fee: charged on top of the payment, or out of what the maker delivers when
/// the taker pays inside a venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Fee {
    pub(crate) bps: Bps,
    pub(crate) on_delivery: bool,
}

/// The fee a take from `from` pays at its pair's `pair_bps`: none without the taker's consent,
/// `max_fee_bps`.
pub(crate) fn take_fee(from: &VenueId, pair_bps: Bps, max_fee_bps: Option<Bps>) -> Fee {
    match max_fee_bps {
        None => Fee::default(),
        Some(_) => Fee { bps: pair_bps, on_delivery: deployments::is_venue_settled(from) },
    }
}

/// `⌊amount × bps / 10 000⌋`, exactly and without overflow (`bps ≤ 10 000`).
pub(crate) fn fee_on(amount: U256, bps: Bps) -> U256 {
    let scale = U256::from(10_000u16);
    let bps = U256::from(bps.get());
    amount / scale * bps + amount % scale * bps / scale
}

/// The largest price whose fee on top fits `budget`: `P + ⌊P × bps / 10 000⌋ ≤ budget`, as the
/// server plans it.
pub(crate) fn price_within(budget: U256, bps: Bps) -> Option<U256> {
    let scale = U512::from(10_000u16);
    let numerator = (wide(budget) + U512::from(1u8)) * scale - U512::from(1u8);
    narrow(numerator / (scale + U512::from(bps.get())))
}

/// `⌊a × b / c⌋`; `None` for `c = 0` or a result above `2^256 - 1`.
pub(crate) fn mul_div(a: U256, b: U256, c: U256) -> Option<U256> {
    let product: U512 = a.widening_mul(b);
    product.checked_div(wide(c)).and_then(narrow)
}

/// `⌈a × b / c⌉`; `None` for `c = 0` or a result above `2^256 - 1`.
pub(crate) fn mul_div_ceil(a: U256, b: U256, c: U256) -> Option<U256> {
    let product: U512 = a.widening_mul(b);
    let divisor = wide(c);
    let quotient = product.checked_div(divisor)?;
    let rounded = if (product % divisor).is_zero() { quotient } else { quotient + U512::from(1u8) };
    narrow(rounded)
}

/// `a × b < c × d`, exactly.
pub(crate) fn product_lt(a: U256, b: U256, c: U256, d: U256) -> bool {
    a.widening_mul::<256, 4, 512, 8>(b) < c.widening_mul::<256, 4, 512, 8>(d)
}

fn wide(value: U256) -> U512 {
    value.widening_mul(U256::from(1u8))
}

fn narrow(value: U512) -> Option<U256> {
    U256::uint_try_from(value).ok()
}

/// The fee a swap id carries and where the escrow takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SwapFee {
    pub(crate) fee: U256,
    pub(crate) on_delivery: bool,
    /// A wallet delivery's swap id: the escrow pays it by plain transfer, never into a venue.
    pub(crate) wallet: bool,
}

/// The fee `swap_id` carries; zero on the payment, into a venue, without a tag.
pub(crate) fn swap_fee(swap_id: &B256) -> SwapFee {
    let (tag, rest) = swap_id.split_at(6);
    let (on_delivery, wallet) = match <[u8; 6]>::try_from(tag) {
        Ok(FEE_ON_PAYMENT) => (false, false),
        Ok(FEE_ON_DELIVERY) => (true, false),
        Ok(WALLET_FEE_ON_PAYMENT) => (false, true),
        Ok(WALLET_FEE_ON_DELIVERY) => (true, true),
        _ => return SwapFee { fee: U256::ZERO, on_delivery: false, wallet: false },
    };
    SwapFee { fee: U256::from_be_slice(&rest[..10]), on_delivery, wallet }
}

/// That a fill's fee is the one its swap id carries, taken where its payment says, within what
/// the taker accepted: on the maker's price from an escrow payment (`on_delivery` false), on
/// what the offer delivers when the taker pays inside a venue. Without consent, no fee.
pub(crate) fn check_fee(
    swap_id: &B256,
    taker_fee: U256,
    amount_in: U256,
    amount_out: U256,
    on_delivery: bool,
    max_fee_bps: Option<Bps>,
) -> Result<(), VerificationError> {
    let carried = swap_fee(swap_id);
    if carried.fee != taker_fee {
        return Err(differs(details::FILLS_TAKER_FEE));
    }
    if carried.fee.is_zero() {
        return Ok(());
    }
    if carried.on_delivery != on_delivery {
        return Err(differs(details::FILLS_SWAP_ID));
    }
    let charged = if on_delivery {
        amount_out.checked_add(carried.fee)
    } else {
        amount_in.checked_sub(carried.fee)
    }
    .ok_or(differs(details::FILLS_TAKER_FEE))?;
    let accepted = max_fee_bps.is_some_and(|max| {
        !product_lt(U256::from(max.get()), charged, carried.fee, U256::from(10_000u16))
    });
    if accepted { Ok(()) } else { Err(differs(details::FEE_ABOVE_ACCEPTED)) }
}

/// The least a wallet delivery of `amount_out` to `to` pays its recipient, as the escrow checks
/// it: on Polygon `⌈amount_out × 9 995 / 10 000⌉`, elsewhere all of it.
pub(crate) fn minimum_received(to: &VenueId, amount_out: U256) -> Option<U256> {
    match to {
        VenueId::Polymarket => mul_div_ceil(
            amount_out,
            U256::from(deployments::WALLET_SWAP_MIN_BPS),
            U256::from(10_000u16),
        ),
        _ => Some(amount_out),
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::keccak256;

    use super::*;
    use crate::error::VerificationKind;

    fn bps(value: u16) -> Bps {
        Bps::new(value).unwrap()
    }

    fn id(text: &str) -> B256 {
        text.parse().unwrap()
    }

    /// `contracts/test/TakerFees.t.sol`'s vector: 3 000 units on the payment.
    const CHARGED: &str = "0xde65f326692b00000000000000000bb8290decd9548b62a8d60345a988386fc8";

    fn swap(tag: &str, fee: &str) -> B256 {
        id(&format!("0x{tag}{fee:0>20}{}", "ab".repeat(16)))
    }

    #[test]
    fn a_swap_id_carries_its_fee_after_the_escrows_tag() {
        let fee = |id: B256| swap_fee(&id);
        assert_eq!(
            fee(id(CHARGED)),
            SwapFee { fee: U256::from(3_000u16), on_delivery: false, wallet: false }
        );
        assert_eq!(
            fee(swap("cc909c08081a", "0258")),
            SwapFee { fee: U256::from(600u16), on_delivery: true, wallet: false }
        );
        assert_eq!(
            fee(id(&format!("0x{}", "aa".repeat(32)))),
            SwapFee { fee: U256::ZERO, on_delivery: false, wallet: false }
        );
        assert_eq!(
            fee(swap("1b4c4374729e", "0bb8")),
            SwapFee { fee: U256::from(3_000u16), on_delivery: false, wallet: true }
        );
        assert_eq!(
            fee(swap("fcb0d9ddb285", "0")),
            SwapFee { fee: U256::ZERO, on_delivery: true, wallet: true }
        );
        // The largest fee ten bytes hold.
        assert_eq!(
            fee(swap("de65f326692b", &"ff".repeat(10))).fee,
            (U256::from(1u8) << 80) - U256::from(1u8)
        );
    }

    /// `contracts/test/WalletDelivery.t.sol`: the tags are the first six bytes of these hashes.
    #[test]
    fn the_tags_are_the_escrows() {
        for (text, tag) in [
            ("LaMarge fee on payment", FEE_ON_PAYMENT),
            ("LaMarge fee on delivery", FEE_ON_DELIVERY),
            ("LaMarge wallet, fee on payment", WALLET_FEE_ON_PAYMENT),
            ("LaMarge wallet, fee on delivery", WALLET_FEE_ON_DELIVERY),
        ] {
            assert_eq!(keccak256(text)[..6], tag, "{text}");
        }
    }

    fn check(
        swap_id: B256,
        taker_fee: u64,
        amounts: (u64, u64),
        on_delivery: bool,
        max: Option<u16>,
    ) -> Result<(), &'static str> {
        check_fee(
            &swap_id,
            U256::from(taker_fee),
            U256::from(amounts.0),
            U256::from(amounts.1),
            on_delivery,
            max.map(bps),
        )
        .map_err(|error| {
            assert_eq!(error.kind, VerificationKind::Differs);
            error.detail
        })
    }

    #[test]
    fn a_fee_is_the_swap_ids_where_the_payment_says_within_the_takers_consent() {
        let amounts = (10_003_000, 10_000_000);
        assert_eq!(check(id(CHARGED), 3_000, amounts, false, Some(3)), Ok(()));
        assert_eq!(
            check(id(CHARGED), 3_000, amounts, false, Some(2)),
            Err(details::FEE_ABOVE_ACCEPTED)
        );
        assert_eq!(
            check(id(CHARGED), 3_000, amounts, false, None),
            Err(details::FEE_ABOVE_ACCEPTED)
        );
        assert_eq!(check(id(CHARGED), 3_000, amounts, true, Some(3)), Err(details::FILLS_SWAP_ID));
        assert_eq!(
            check(id(CHARGED), 2_999, amounts, false, Some(3)),
            Err(details::FILLS_TAKER_FEE)
        );
        // A fee larger than the payment it is part of.
        assert_eq!(
            check(id(CHARGED), 3_000, (2_999, 1), false, Some(100)),
            Err(details::FILLS_TAKER_FEE)
        );
        let plain = id(&format!("0x{}", "aa".repeat(32)));
        assert_eq!(check(plain, 0, amounts, false, None), Ok(()));
        assert_eq!(check(plain, 1, amounts, false, Some(3)), Err(details::FILLS_TAKER_FEE));
    }

    #[test]
    fn a_wallet_delivery_fee_on_delivery_is_bounded_by_what_it_delivers() {
        let wallet = swap("fcb0d9ddb285", "0258");
        assert_eq!(check(wallet, 600, (2_001_300, 2_000_000), true, Some(3)), Ok(()));
        assert_eq!(
            check(wallet, 600, (2_001_300, 2_000_000), false, Some(3)),
            Err(details::FILLS_SWAP_ID)
        );
        assert_eq!(
            check(wallet, 600, (2_001_300, 2_000_000), true, Some(2)),
            Err(details::FEE_ABOVE_ACCEPTED)
        );
        // On delivery the bound is on what the offer delivers, fee included: 600 of 1 999 400 +
        // 600 is exactly 3 bps; one unit less delivered is above.
        assert_eq!(check(wallet, 600, (0, 1_999_400), true, Some(3)), Ok(()));
        assert_eq!(
            check(wallet, 600, (0, 1_999_399), true, Some(3)),
            Err(details::FEE_ABOVE_ACCEPTED)
        );
    }

    #[test]
    fn a_wallet_deliverys_least_received_is_the_escrows_floor_on_polygon() {
        let least =
            |venue: &VenueId, amount: u64| minimum_received(venue, U256::from(amount)).unwrap();
        assert_eq!(least(&VenueId::Polymarket, 1_005_000), U256::from(1_004_498u64));
        assert_eq!(least(&VenueId::Polymarket, 10_000), U256::from(9_995u64));
        for amount in 0..3_000u64 {
            let paid = least(&VenueId::Polymarket, amount);
            // The escrow reverts below it: `paid × 10 000 < amount × 9 995`.
            let floor = U256::from(amount) * U256::from(9_995u64);
            assert!(paid * U256::from(10_000u64) >= floor);
            assert!(paid.is_zero() || (paid - U256::from(1u8)) * U256::from(10_000u64) < floor);
        }
        assert_eq!(least(&VenueId::Hyperliquid, 1_005_000), U256::from(1_005_000u64));
        assert_eq!(least(&VenueId::Gmx, 1), U256::from(1u8));
        assert_eq!(
            minimum_received(&VenueId::Polymarket, U256::MAX),
            Some(U256::MAX - U256::MAX / U256::from(2_000u16))
        );
    }

    #[test]
    fn the_price_within_a_budget_leaves_no_unit_its_fee_does_not_take() {
        for value in [0, 1, 3, 100] {
            let rate = bps(value);
            for budget in 0..3_000u64 {
                let budget = U256::from(budget);
                let price = price_within(budget, rate).unwrap();
                let next = price + U256::from(1u8);
                assert!(price + fee_on(price, rate) <= budget);
                assert!(next + fee_on(next, rate) > budget);
            }
        }
        assert_eq!(price_within(U256::from(2_001_900u64), bps(3)), Some(U256::from(2_001_300u64)));
        // No overflow at the top of the range.
        assert_eq!(price_within(U256::MAX, bps(0)), Some(U256::MAX));
        assert!(price_within(U256::MAX, bps(100)).unwrap() < U256::MAX);
    }

    #[test]
    fn fees_round_down_exactly_without_overflow() {
        assert_eq!(fee_on(U256::from(2_001_300u64), bps(3)), U256::from(600u16));
        assert_eq!(fee_on(U256::from(9_999u64), bps(1)), U256::ZERO);
        assert_eq!(fee_on(U256::MAX, bps(10_000)), U256::MAX);
        assert_eq!(fee_on(U256::MAX, bps(100)), U256::MAX / U256::from(100u8));
    }

    #[test]
    fn ratios_widen_then_narrow() {
        let max = U256::MAX;
        assert_eq!(mul_div(max, max, max), Some(max));
        assert_eq!(mul_div_ceil(max, max, max), Some(max));
        assert_eq!(
            mul_div_ceil(max, U256::from(1u8), U256::from(2u8)),
            Some(U256::from(1u8) << 255)
        );
        assert_eq!(mul_div(max, U256::from(2u8), U256::from(1u8)), None);
        assert_eq!(mul_div(U256::from(1u8), U256::from(1u8), U256::ZERO), None);
        assert_eq!(mul_div_ceil(U256::from(1u8), U256::from(1u8), U256::ZERO), None);
        assert_eq!(
            mul_div(U256::from(7u8), U256::from(3u8), U256::from(2u8)),
            Some(U256::from(10u8))
        );
        assert_eq!(
            mul_div_ceil(U256::from(7u8), U256::from(3u8), U256::from(2u8)),
            Some(U256::from(11u8))
        );
        assert_eq!(
            mul_div_ceil(U256::from(6u8), U256::from(3u8), U256::from(2u8)),
            Some(U256::from(9u8))
        );
        assert!(product_lt(U256::from(1u8), max, U256::from(2u8), max));
        assert!(!product_lt(max, max, max, max));
    }

    #[test]
    fn a_fee_needs_the_takers_consent_and_is_on_delivery_from_a_venue() {
        assert_eq!(take_fee(&VenueId::Polymarket, bps(3), None), Fee::default());
        assert_eq!(
            take_fee(&VenueId::Polymarket, bps(3), Some(bps(3))),
            Fee { bps: bps(3), on_delivery: false }
        );
        assert_eq!(
            take_fee(&VenueId::Lighter, bps(3), Some(bps(5))),
            Fee { bps: bps(3), on_delivery: true }
        );
    }
}
