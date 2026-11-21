//! Ported from `sdk-ts/src/book.test.ts` and `sdk-ts/src/book-limits.test.ts`, plus overflow and
//! invalid-book cases.

use super::*;
use crate::types::VenueId;

const NO_CAP: u128 = 1_000_000_000_000_000_000;

fn now() -> DateTime<Utc> {
    "2026-11-09T12:00:00Z".parse().unwrap()
}

fn order(name: &str) -> OrderId {
    OrderId::new(alloy_primitives::keccak256(name))
}

fn level(name: &str, supply: u128, receive: u128, available: u128, min_fill: u128) -> Level {
    Level {
        order_id: order(name),
        quote_index: 0,
        supply: Amount::from(supply),
        receive: Amount::from(receive),
        available: Amount::from(available),
        min_fill: Amount::from(min_fill),
        expires_at: "2099-01-01T01:00:00Z".parse().unwrap(),
        quoted_at: "2026-11-09T11:00:00Z".parse().unwrap(),
        sequence: 1,
        valid_until: "2099-01-01T00:00:00Z".parse().unwrap(),
    }
}

fn book(from: VenueId, max_fill: u128, fee: u16, levels: Vec<Level>) -> Book {
    Book {
        from,
        to: VenueId::Hyperliquid,
        max_fill: Amount::from(max_fill),
        taker_fee_bps: Bps::new(fee).unwrap(),
        levels,
    }
}

/// A book paid from Polymarket (a fee would be on the payment), no fee.
fn plain(max_fill: u128, levels: Vec<Level>) -> Book {
    book(VenueId::Polymarket, max_fill, 0, levels)
}

fn bps(value: u16) -> Bps {
    Bps::new(value).unwrap()
}

fn parts(value: u8) -> MaxParts {
    MaxParts::new(value).unwrap()
}

fn floor(total: u128, part: u128) -> Floor {
    Floor::new(Amount::from(total), Amount::from(part))
}

fn none() -> SweepTerms {
    SweepTerms::new(None)
}

fn swept(out: u128, pay: u128, parts: u8) -> Sweep {
    Sweep { out: Amount::from(out), pay: Amount::from(pay), parts }
}

fn sweep(book: &Book, amount: u128, terms: &SweepTerms) -> Sweep {
    book.sweep(Amount::from(amount), terms, now()).unwrap()
}

fn minimum(book: &Book, terms: &SweepTerms) -> Option<u128> {
    book.minimum_take(terms, now()).unwrap().map(|amount| u128::try_from(amount.get()).unwrap())
}

fn check(book: &Book, amount_in: u128, min_out: u128, terms: &SweepTerms) -> Result<(), BookError> {
    book.check(Amount::from(amount_in), Amount::from(min_out), terms, now())
}

fn pays_all(book: &Book, amount: u128, swept: &Sweep, terms: &SweepTerms) -> bool {
    Matcher::new(book, terms, now()).unwrap().pays_all(U256::from(amount), swept).unwrap()
}

#[test]
fn a_books_least_take_counts_no_more_parts_than_the_take_buys() {
    // Two levels of 3 each, and a venue that takes 5 at least: two parts reach it, one never does.
    let quotes = plain(
        1_000_000_000_000,
        vec![
            level("a", 1_000_000, 1_000_000, 3_000_000, 1),
            level("b", 1_000_000, 1_000_000, 3_000_000, 1),
        ],
    );
    let terms = none().floor(floor(5_000_000, 0));
    assert_eq!(minimum(&quotes, &terms), Some(5_000_000));
    assert_eq!(minimum(&quotes, &terms.max_parts(parts(1))), None);
}

#[test]
fn the_makers_5_usdc_minimum_needs_5_00256_pusd_at_the_quoted_price() {
    let quotes = plain(NO_CAP, vec![level("maker", 1_000_000, 1_000_512, 98_000_000, 5_000_000)]);
    assert_eq!(minimum(&quotes, &none()), Some(5_002_560));
    assert_eq!(sweep(&quotes, 5_000_000, &none()).parts, 0);
    assert_eq!(sweep(&quotes, 5_002_559, &none()).parts, 0);
    assert_eq!(sweep(&quotes, 5_002_560, &none()), swept(5_000_000, 5_002_560, 1));
}

#[test]
fn minimum_payment_rounds_up_and_excludes_expired_owned_or_unfillable_quotes() {
    let mut expired = level("expired", 1, 1, 50, 1);
    expired.valid_until = "2020-01-01T00:00:00Z".parse().unwrap();
    let levels = vec![
        level("mine", 1, 1, 50, 1),
        expired,
        level("empty", 1, 1, 0, 1),
        level("cap", 1, 1, 50, 20),
        level("valid", 3, 7, 50, 5),
    ];
    let own = none().own([order("mine")].into());
    assert_eq!(minimum(&plain(10, levels.clone()), &own), Some(12));
    assert_eq!(minimum(&plain(10, levels[..4].to_vec()), &own), None);
    assert_eq!(minimum(&plain(NO_CAP, vec![]), &none()), None);
}

#[test]
fn a_level_valid_until_now_is_expired() {
    let mut ending = level("a", 1, 1, 50, 1);
    ending.valid_until = now();
    assert_eq!(sweep(&plain(NO_CAP, vec![ending]), 10, &none()).parts, 0);
}

#[test]
fn sweep_pays_each_parts_price_rounded_up_and_receives_it_rounded_down() {
    // 10 USDC at 1.001 USDC per pUSD: 9.990009 pUSD, costing 9.999999009 rounded up.
    let quotes = plain(NO_CAP, vec![level("a", 25_000_000, 25_025_000, 25_000_000, 1)]);
    assert_eq!(sweep(&quotes, 10_000_000, &none()), swept(9_990_009, 10_000_000, 1));
}

#[test]
fn sweep_goes_through_the_book_best_first_the_last_order_in_part() {
    let quotes = plain(
        NO_CAP,
        vec![
            level("a", 1_000_000, 1_000_000, 3_000_000, 1),
            level("b", 1_000_000, 2_000_000, 5_000_000, 1),
        ],
    );
    assert_eq!(sweep(&quotes, 5_000_000, &none()), swept(4_000_000, 5_000_000, 2));
}

#[test]
fn sweep_takes_at_most_max_fill_from_one_level() {
    let quotes = plain(
        1_000_000_000,
        vec![level("a", 1, 1, 2_000_000_000, 1), level("b", 1, 1, 1_000_000_000, 1)],
    );
    assert_eq!(sweep(&quotes, 1_500_000_000, &none()), swept(1_500_000_000, 1_500_000_000, 2));
}

#[test]
fn sweep_skips_an_order_whose_share_falls_below_its_min_fill() {
    let quotes = plain(
        NO_CAP,
        vec![level("a", 1, 1, 50_000_000, 5_000_000), level("b", 1, 2, 50_000_000, 1_000_000)],
    );
    assert_eq!(sweep(&quotes, 2_000_000, &none()), swept(1_000_000, 2_000_000, 1));
}

#[test]
fn sweep_leaves_ones_own_orders_out() {
    let quotes =
        plain(NO_CAP, vec![level("mine", 1, 1, 10_000_000, 1), level("b", 1, 2, 10_000_000, 1)]);
    let own = none().own([order("mine")].into());
    assert_eq!(sweep(&quotes, 2_000_000, &own), swept(1_000_000, 2_000_000, 1));
}

#[test]
fn sweep_goes_through_at_most_max_parts_levels() {
    let levels = (0..10).map(|index| level(&format!("o{index}"), 1, 1, 1_000_000, 1)).collect();
    assert_eq!(sweep(&plain(NO_CAP, levels), 20_000_000, &none()), swept(8_000_000, 8_000_000, 8));
}

#[test]
fn the_receiving_venues_minimum_applies_to_all_parts_together() {
    // Three makers with 2 pUSD left each, as Polymarket's 5 pUSD minimum leaves them.
    let quotes =
        plain(NO_CAP, ["a", "b", "c"].map(|id| level(id, 1, 1, 2_000_000, 1_000_000)).to_vec());
    let venue = none().floor(floor(5_000_000, 0));
    assert_eq!(sweep(&quotes, 5_000_000, &venue), swept(5_000_000, 5_000_000, 3));
    assert_eq!(sweep(&quotes, 4_999_999, &venue), swept(0, 0, 0));
    assert_eq!(sweep(&quotes, 5_000_000, &venue.clone().max_parts(parts(2))).parts, 0);
    assert_eq!(minimum(&quotes, &venue), Some(5_000_000));
    assert_eq!(minimum(&quotes, &none().floor(floor(7_000_000, 0))), None);
    assert_eq!(minimum(&quotes, &none()), Some(1_000_000));
}

#[test]
fn a_wallets_minimum_applies_to_each_part_fee_deducted() {
    let wallet = none().floor(floor(0, 10_000));
    let short = ["a", "b"].map(|id| level(id, 1, 1, 9_999, 1)).to_vec();
    let mut with_c = short.clone();
    with_c.push(level("c", 1, 1, 10_000, 1));
    assert_eq!(sweep(&plain(NO_CAP, with_c), 30_000, &wallet), swept(10_000, 10_000, 1));
    assert_eq!(sweep(&plain(NO_CAP, short), 30_000, &wallet).parts, 0);
    let lighter = book(VenueId::Lighter, NO_CAP, 3, vec![level("c", 1, 1, 10_000, 1)]);
    let wallet_fee = SweepTerms::new(Some(bps(3))).floor(floor(0, 10_000));
    assert_eq!(sweep(&lighter, 10_000, &wallet_fee).parts, 0);

    // A remainder under the per-part minimum cannot form a part: the take pays all the rest.
    let levels = vec![level("a", 1, 1, 100_000, 1), level("b", 1, 1, 1_000_000, 1)];
    let quotes = plain(NO_CAP, levels.clone());
    let taken = sweep(&quotes, 105_000, &wallet);
    assert_eq!(taken, swept(100_000, 100_000, 1));
    assert!(pays_all(&quotes, 105_000, &taken, &wallet));
    assert_eq!(sweep(&quotes, 110_000, &wallet).parts, 2);
    assert!(!pays_all(&quotes, 105_000, &taken, &none()));
    assert_eq!(check(&quotes, 105_000, 100_000, &wallet), Ok(()));

    // With the fee on delivery, a part needs 10 003 out to deliver 10 000: 10 002 left is paid,
    // 10 003 left could buy a part the one fill of a Lighter payment cannot take.
    let from_lighter = book(VenueId::Lighter, NO_CAP, 3, levels);
    let short = sweep(&from_lighter, 110_002, &wallet_fee);
    assert_eq!(short.parts, 1);
    assert!(pays_all(&from_lighter, 110_002, &short, &wallet_fee));
    let single = sweep(&from_lighter, 110_003, &wallet_fee);
    assert_eq!(single.parts, 1);
    assert!(!pays_all(&from_lighter, 110_003, &single, &wallet_fee));
}

#[test]
fn a_payment_from_a_venue_settled_inside_itself_is_one_fill_whatever_the_fee() {
    // Two live 1:1 levels of 5 USDC each.
    let levels = vec![level("a", 1, 1, 5_000_000, 1), level("b", 1, 1, 5_000_000, 1)];
    for from in [VenueId::Lighter, VenueId::Aster] {
        let quotes = book(from.clone(), NO_CAP, 0, levels.clone());
        for terms in [none(), SweepTerms::new(Some(Bps::ZERO))] {
            let case = format!("{from:?} {terms:?}");
            assert_eq!(
                sweep(&quotes, 10_000_000, &terms),
                swept(5_000_000, 5_000_000, 1),
                "{case}"
            );
            assert_eq!(
                check(&quotes, 10_000_000, 10_000_000, &terms),
                Err(BELOW_MINIMUM),
                "{case}"
            );
            assert_eq!(check(&quotes, 5_000_000, 5_000_000, &terms), Ok(()), "{case}");
            // Two parts would reach a 6 USDC floor; one never does.
            assert_eq!(minimum(&quotes, &terms.clone().floor(floor(6_000_000, 0))), None, "{case}");
            let limits = quotes.limits(None, &terms, now()).unwrap().unwrap();
            assert_eq!(limits.maximum, swept(5_000_000, 5_000_000, 1), "{case}");
        }
    }
    // Paid from an escrow venue, the same book fills both levels.
    let escrowed = plain(NO_CAP, levels);
    assert_eq!(sweep(&escrowed, 10_000_000, &none()), swept(10_000_000, 10_000_000, 2));
    assert_eq!(check(&escrowed, 10_000_000, 10_000_000, &none()), Ok(()));
    assert_eq!(minimum(&escrowed, &none().floor(floor(6_000_000, 0))), Some(6_000_000));
    let limits = escrowed.limits(None, &none(), now()).unwrap().unwrap();
    assert_eq!(limits.maximum, swept(10_000_000, 10_000_000, 2));
}

#[test]
fn an_empty_book_or_no_amount_takes_nothing() {
    assert_eq!(sweep(&plain(NO_CAP, vec![]), 5_000_000, &none()), swept(0, 0, 0));
    assert_eq!(
        sweep(&plain(NO_CAP, vec![level("a", 1, 1, 1_000_000, 1)]), 0, &none()),
        swept(0, 0, 0)
    );
}

#[test]
fn fresh_quote_validation_respects_the_reviewed_amount_minimum_and_part_limit() {
    let quoted = level("order", 1_000_000, 1_000_000, 5_000_000, 1_000_000);
    let book_of = |levels: Vec<Level>| plain(100_000_000, levels);
    let two = none().max_parts(parts(2));
    let with = |edit: fn(&mut Level)| {
        let mut changed = quoted.clone();
        edit(&mut changed);
        book_of(vec![changed])
    };
    assert_eq!(check(&book_of(vec![quoted.clone()]), 5_000_000, 4_900_000, &two), Ok(()));
    assert_eq!(
        check(&with(|l| l.receive = Amount::from(1_010_000u64)), 5_000_000, 4_900_000, &two),
        Ok(())
    );
    let cheaper = with(|l| {
        l.supply = Amount::from(990_000u64);
        l.available = Amount::from(6_000_000u64);
    });
    assert_eq!(check(&cheaper, 5_000_000, 4_900_000, &two), Ok(()));
    assert_eq!(
        check(&with(|l| l.receive = Amount::from(1_100_000u64)), 5_000_000, 4_900_000, &two),
        Err(BELOW_MINIMUM)
    );
    let mut first = quoted.clone();
    first.available = Amount::from(3_000_000u64);
    let mut second = quoted.clone();
    second.order_id = order("other");
    second.available = Amount::from(2_000_000u64);
    let split = book_of(vec![first, second]);
    assert_eq!(check(&split, 5_000_000, 4_900_000, &two), Ok(()));
    assert_eq!(
        check(&split, 5_000_000, 4_900_000, &none().max_parts(parts(1))),
        Err(BELOW_MINIMUM)
    );
    assert_eq!(
        check(&with(|l| l.available = Amount::from(4_000_000u64)), 5_000_000, 4_900_000, &two),
        Err(BELOW_MINIMUM)
    );
    assert_eq!(check(&book_of(vec![]), 5_000_000, 4_900_000, &two), Err(NO_PRICE));
    assert_eq!(
        check(&split, 5_000_000, 4_900_000, &two.clone().floor(floor(5_000_000, 0))),
        Ok(())
    );
    assert_eq!(check(&split, 5_000_000, 4_900_000, &two.floor(floor(5_000_001, 0))), Err(NO_PRICE));
}

#[test]
fn a_payment_fee_comes_on_top_and_a_delivery_fee_out_of_the_output() {
    let levels = vec![level("maker", 5_000_000, 5_003_250, 5_000_000, 1)];
    let consent = SweepTerms::new(Some(bps(3)));
    // 2 001 300 pays 2 000 000 at the price, and ⌊2 001 300 × 3 / 10 000⌋ = 600 on top.
    let polymarket = book(VenueId::Polymarket, NO_CAP, 3, levels.clone());
    assert_eq!(sweep(&polymarket, 2_001_900, &consent), swept(2_000_000, 2_001_900, 1));
    // From Lighter, the offer delivers 2 000 000 for 2 001 300, 600 of it the fee.
    let lighter = book(VenueId::Lighter, NO_CAP, 3, levels);
    assert_eq!(sweep(&lighter, 2_001_300, &consent), swept(1_999_400, 2_001_300, 1));
    // The fee lowers what the same payment buys.
    assert!(
        sweep(&polymarket, 2_001_900, &consent).out < sweep(&polymarket, 2_001_900, &none()).out
    );
    // Without consent, no fee.
    assert_eq!(sweep(&polymarket, 2_001_300, &none()), swept(2_000_000, 2_001_300, 1));

    assert_eq!(check(&polymarket, 2_001_900, 2_000_000, &consent), Ok(()));
    assert!(check(&polymarket, 2_001_300, 2_000_000, &consent).is_err());
    let lower = SweepTerms::new(Some(bps(2)));
    assert_eq!(check(&polymarket, 2_001_900, 2_000_000, &lower), Err(FEE_ABOVE_MAX));
    assert_eq!(polymarket.sweep(Amount::from(2_001_900u64), &lower, now()), Err(FEE_ABOVE_MAX));
    assert_eq!(polymarket.limits(None, &lower, now()), Err(FEE_ABOVE_MAX));
}

#[test]
fn a_remainder_the_fees_rounding_leaves_is_no_shortfall() {
    // 1:1 at 100 bps: 102 000 000 buys 100 990 099, the one unit left buying nothing more.
    let consent = SweepTerms::new(Some(bps(100)));
    let quotes =
        book(VenueId::Polymarket, NO_CAP, 100, vec![level("maker", 1, 1, 1_000_000_000, 1)]);
    assert_eq!(sweep(&quotes, 102_000_000, &consent), swept(100_990_099, 101_999_999, 1));
    assert_eq!(check(&quotes, 102_000_000, 100_900_000, &consent), Ok(()));
    let thin = book(VenueId::Polymarket, NO_CAP, 100, vec![level("maker", 1, 1, 50_000_000, 1)]);
    assert!(check(&thin, 102_000_000, 0, &consent).is_err());
}

mod limits {
    use super::*;

    fn quoted(edit: impl FnOnce(&mut Level)) -> Level {
        let mut quote = level("maker", 100_000_000, 100_000_000, 100_000_000, 1_000_000);
        quote.valid_until = "2026-11-09T13:00:00Z".parse().unwrap();
        edit(&mut quote);
        quote
    }

    fn limits(book: &Book, budget: Option<u128>, terms: &SweepTerms) -> Option<Limits> {
        book.limits(budget.map(Amount::from), terms, now()).unwrap()
    }

    #[test]
    fn expose_executable_endpoints_capped_by_the_budget() {
        let quotes = plain(100_000_000, vec![quoted(|_| ())]);
        let venue = none().floor(floor(5_000_000, 0));
        let found = limits(&quotes, Some(7_500_000), &venue).unwrap();
        assert_eq!(
            found,
            Limits {
                minimum: swept(5_000_000, 5_000_000, 1),
                maximum: swept(7_500_000, 7_500_000, 1)
            }
        );
        assert_eq!(
            limits(&quotes, None, &none()).unwrap().maximum.pay,
            Amount::from(100_000_000u64)
        );
        assert_eq!(limits(&quotes, Some(4_999_999), &venue), None);
        for endpoint in [found.minimum, found.maximum] {
            assert_eq!(quotes.check(endpoint.pay, endpoint.out, &venue, now()), Ok(()));
        }
    }

    #[test]
    fn exclude_owned_expired_empty_and_undersized_liquidity() {
        let levels = vec![
            quoted(|l| l.order_id = order("mine")),
            quoted(|l| {
                l.order_id = order("ending");
                l.valid_until = now();
            }),
            quoted(|l| {
                l.order_id = order("empty");
                l.available = Amount::ZERO;
            }),
            quoted(|l| {
                l.order_id = order("large");
                l.min_fill = Amount::from(50_000_001u64);
            }),
            quoted(|l| l.order_id = order("a")),
            quoted(|l| l.order_id = order("b")),
            quoted(|l| l.order_id = order("c")),
        ];
        let terms = none().own([order("mine")].into()).max_parts(parts(2));
        assert_eq!(
            limits(&plain(50_000_000, levels), None, &terms).unwrap().maximum,
            swept(100_000_000, 100_000_000, 2)
        );
        assert_eq!(limits(&plain(100_000_000, vec![]), None, &none()), None);
        assert_eq!(limits(&plain(100_000_000, vec![quoted(|_| ())]), Some(0), &none()), None);
        let later: DateTime<Utc> = "2026-11-09T13:00:00Z".parse().unwrap();
        assert_eq!(plain(100_000_000, vec![quoted(|_| ())]).limits(None, &none(), later), Ok(None));
    }

    #[test]
    fn payment_fees_fit_the_budget_and_wallet_floors_apply_to_every_part() {
        let quotes = book(
            VenueId::Hyperliquid,
            100_000_000,
            100,
            vec![
                quoted(|l| {
                    l.available = Amount::from(9_999u64);
                    l.min_fill = Amount::from(1u64);
                }),
                quoted(|l| {
                    l.order_id = order("next");
                    l.min_fill = Amount::from(1u64);
                }),
            ],
        );
        let terms = SweepTerms::new(Some(bps(100))).floor(floor(0, 10_000));
        assert_eq!(
            limits(&quotes, Some(20_200), &terms),
            Some(Limits { minimum: swept(10_000, 10_100, 1), maximum: swept(20_000, 20_200, 1) })
        );
        assert_eq!(limits(&quotes, Some(10_099), &terms), None);
    }

    #[test]
    fn venue_payments_buy_one_fill_and_deduct_the_fee_before_the_minimum() {
        let quotes = book(
            VenueId::Lighter,
            100_000_000,
            100,
            vec![
                quoted(|l| l.available = Amount::from(2_000_000u64)),
                quoted(|l| l.order_id = order("next")),
            ],
        );
        let terms = SweepTerms::new(Some(bps(100))).floor(floor(1_000_000, 0));
        let found = limits(&quotes, None, &terms).unwrap();
        assert_eq!(found.maximum, swept(1_980_000, 2_000_000, 1));
        assert_eq!(found.minimum, swept(1_000_000, 1_010_101, 1));
    }

    #[test]
    fn six_and_eighteen_decimal_endpoints_keep_their_precision() {
        let quotes = plain(
            100_000_000,
            vec![quoted(|l| l.receive = Amount::from(100_010_000_000_000_000_000u128))],
        );
        assert_eq!(
            limits(&quotes, Some(6_000_000_000_000_000_000), &none()).unwrap().maximum,
            swept(5_999_400, 5_999_999_940_000_000_000, 1)
        );
        assert_eq!(
            limits(&quotes, None, &none()).unwrap().minimum.pay,
            Amount::from(1_000_100_000_000_000_000u128)
        );
    }
}

#[test]
fn slippage_is_an_output_floor_rounded_down() {
    assert_eq!(slippage_minimum(Amount::from(5_999_500u64), bps(10)), Amount::from(5_993_500u64));
    let large = Amount::from(10_000_000_000_000_000_001u128);
    assert_eq!(slippage_minimum(large, bps(0)), large);
    assert_eq!(slippage_minimum(Amount::from(123u64), bps(10_000)), Amount::ZERO);
    assert_eq!(slippage_minimum(Amount::from(9_999u64), bps(1)), Amount::from(9_998u64));
    let max = Amount::new(U256::MAX);
    assert_eq!(slippage_minimum(max, bps(0)), max);
    assert_eq!(slippage_minimum(max, bps(10_000)), Amount::ZERO);
    let half = slippage_minimum(max, bps(5_000)).get();
    assert_eq!(half, U256::MAX / U256::from(2u8));
}

#[test]
fn a_zero_supply_or_receive_is_an_invalid_book() {
    for quote in [level("a", 0, 1, 10, 1), level("a", 1, 0, 10, 1)] {
        let quotes = plain(NO_CAP, vec![quote]);
        assert_eq!(quotes.sweep(Amount::from(10u64), &none(), now()), Err(ZERO_PRICE));
        assert_eq!(quotes.minimum_take(&none(), now()), Err(ZERO_PRICE));
        assert_eq!(quotes.limits(None, &none(), now()), Err(ZERO_PRICE));
        assert_eq!(
            quotes.check(Amount::from(10u64), Amount::ZERO, &none(), now()),
            Err(ZERO_PRICE)
        );
    }
}

#[test]
fn amounts_near_two_to_the_256_never_wrap() {
    let max = U256::MAX;
    let huge = |name: &str, supply: U256, receive: U256| Level {
        supply: Amount::new(supply),
        receive: Amount::new(receive),
        available: Amount::new(max),
        ..level(name, 1, 1, 1, 1)
    };
    let top = |levels: Vec<Level>| Book { max_fill: Amount::new(max), ..plain(1, levels) };

    // One level at 1:1 takes everything exactly.
    let single = top(vec![huge("a", max, max)]);
    assert_eq!(
        single.sweep(Amount::new(max), &none(), now()),
        Ok(Sweep { out: Amount::new(max), pay: Amount::new(max), parts: 1 })
    );

    // Two such levels: their capacity together is beyond 2^256 - 1.
    let double = top(vec![
        huge("a", U256::from(1u8), U256::from(1u8)),
        huge("b", U256::from(1u8), U256::from(1u8)),
    ]);
    assert_eq!(double.minimum_take(&none(), now()), Err(OVERFLOW));
    assert_eq!(double.limits(None, &none(), now()), Err(OVERFLOW));
    assert!(double.limits(Some(Amount::new(max)), &none(), now()).is_err());

    // Levels delivering twice what they cost: the outputs of two parts overflow.
    let generous = top(vec![
        huge("a", U256::from(2u8), U256::from(1u8)),
        huge("b", U256::from(2u8), U256::from(1u8)),
    ]);
    assert_eq!(generous.sweep(Amount::new(max), &none(), now()), Err(OVERFLOW));

    // What a huge payment buys is bounded by the capacity, not an overflow.
    let cheap =
        top(vec![Level { available: Amount::from(5u64), ..huge("a", max, U256::from(1u8)) }]);
    assert_eq!(cheap.sweep(Amount::new(max), &none(), now()).unwrap().out, Amount::from(5u64));

    // A fee on the payment at the top of the range.
    let fee = Book { taker_fee_bps: bps(100), ..top(vec![huge("a", max, max)]) };
    let taken = fee.sweep(Amount::new(max), &SweepTerms::new(Some(bps(100))), now()).unwrap();
    assert_eq!(taken.parts, 1);
    assert!(taken.out.get() < max);
}

#[test]
fn a_fee_of_the_whole_output_is_an_invalid_book() {
    let quotes = book(VenueId::Lighter, NO_CAP, 10_000, vec![level("a", 1, 1, 1_000_000, 1)]);
    let terms = SweepTerms::new(Some(bps(10_000))).floor(floor(0, 10));
    assert_eq!(check(&quotes, 1_000_000, 0, &terms), Err(NO_PRICE));
    let matcher = Matcher::new(&quotes, &terms, now()).unwrap();
    assert_eq!(matcher.pays_all(U256::from(10u8), &Sweep::NONE), Err(WHOLE_FEE));
}

#[test]
fn book_errors_map_to_sdk_errors() {
    assert!(matches!(Error::from(NO_PRICE), Error::QuoteUnavailable("no executable price")));
    match Error::from(OVERFLOW) {
        Error::Verification(error) => assert_eq!(error.detail, details::BOOK),
        other => unreachable!("{other:?}"),
    }
    assert_eq!(OVERFLOW.to_string(), "invalid book: amount beyond 2^256 - 1");
}

/// `book.test.ts` "… a wallet's minimum per part": the floor of a take, as the venues list it.
#[test]
fn a_takes_floor_is_the_venues_release_minimum_or_the_wallets_per_part() {
    let mut polymarket = crate::testing::json::fixture("venue");
    polymarket["release_minimum"] = serde_json::json!("5000000");
    polymarket["chain"]["wallet"]["minimum"] = serde_json::json!("10000");
    let mut lighter = polymarket.clone();
    lighter["id"] = serde_json::json!("lighter");
    lighter["release_minimum"] = serde_json::Value::Null;
    lighter["chain"]["wallet"] = serde_json::Value::Null;
    let venues: Vec<Venue> =
        serde_json::from_value(serde_json::json!([polymarket, lighter])).unwrap();
    let floor = |to: VenueId, wallet: bool| {
        let destination = if wallet { Destination::Wallet } else { Destination::Venue };
        delivery_floor(&venues, &to, &destination)
    };
    assert_eq!(
        floor(VenueId::Polymarket, false),
        Floor::new(Amount::from(5_000_000u64), Amount::ZERO)
    );
    assert_eq!(floor(VenueId::Polymarket, true), Floor::new(Amount::ZERO, Amount::from(10_000u64)));
    assert_eq!(floor(VenueId::Lighter, false), Floor::default());
    assert_eq!(floor(VenueId::Lighter, true), Floor::default());
    assert_eq!(floor(VenueId::Gmx, false), Floor::default());
}
