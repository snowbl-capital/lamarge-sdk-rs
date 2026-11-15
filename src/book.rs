//! Book math: sweeps, minimum takes, limits and slippage floors, as the server matches a take.
//!
//! Pure and deterministic: `now` is explicit, the fee comes from the book itself
//! ([`Book::taker_fee_bps`]) and the taker's consent ([`SweepTerms::new`]), and every amount is
//! computed exactly in base units, failing rather than wrapping on overflow.

use std::collections::HashSet;

use alloy_primitives::U256;
use chrono::{DateTime, Utc};

use crate::{
    Error, deployments,
    model::{Book, Destination, Level, Venue},
    protocol::{
        checks::details::{self, differs},
        fees::{self, Fee},
    },
    types::{Amount, Bps, MaxParts, OrderId, VenueId},
};

/// The least a take delivers: `total` across its parts (a venue's minimum deposit) and `part`
/// in each, fee deducted (a wallet's minimum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Floor {
    /// The least all parts deliver together, in `to` base units.
    pub total: Amount,
    /// The least each part delivers, in `to` base units.
    pub part: Amount,
}

impl Floor {
    /// A floor of `total` across all parts and `part` in each.
    pub const fn new(total: Amount, part: Amount) -> Self {
        Self { total, part }
    }
}

/// What a sweep takes: `out` received on `to`, `pay` paid on `from`, in `parts` fills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Sweep {
    /// Received on `to`, fee on delivery deducted, in `to` base units.
    pub out: Amount,
    /// Paid on `from`, fee on payment included, in `from` base units.
    pub pay: Amount,
    /// The number of fills.
    pub parts: u8,
}

impl Sweep {
    const NONE: Self = Self { out: Amount::ZERO, pay: Amount::ZERO, parts: 0 };
}

/// A book's executable endpoints: its smallest take and its take at capacity (or budget).
/// Amounts between them can fall in minimum-fill gaps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// The smallest executable take.
    pub minimum: Sweep,
    /// The largest take within the budget.
    pub maximum: Sweep,
}

/// Why book math gave no answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BookError {
    /// No quote can serve the request: the fee is above what it accepts, no price executes, or
    /// none meets its minimum.
    #[error("quote unavailable: {0}")]
    Unavailable(&'static str),
    /// The book itself is unusable: a zero supply or receive, or amounts beyond `2^256 - 1`.
    #[error("invalid book: {0}")]
    Invalid(&'static str),
}

impl From<BookError> for Error {
    fn from(error: BookError) -> Self {
        match error {
            BookError::Unavailable(reason) => Error::QuoteUnavailable(reason),
            BookError::Invalid(_) => Error::Verification(differs(details::BOOK)),
        }
    }
}

const FEE_ABOVE_MAX: BookError = BookError::Unavailable("the fee is above what this take accepts");
const NO_PRICE: BookError = BookError::Unavailable("no executable price");
const BELOW_MINIMUM: BookError = BookError::Unavailable("no price meets the minimum");
const ZERO_PRICE: BookError = BookError::Invalid("zero supply or receive");
const OVERFLOW: BookError = BookError::Invalid("amount beyond 2^256 - 1");
const WHOLE_FEE: BookError = BookError::Invalid("a fee of the whole output");

/// The terms a sweep follows, as the take or transfer request states them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepTerms {
    max_fee_bps: Option<Bps>,
    max_parts: MaxParts,
    floor: Floor,
    own: HashSet<OrderId>,
}

impl SweepTerms {
    /// Terms accepting the book's fee up to `max_fee_bps`, the same value as the request's;
    /// `None` accepts no fee. Up to 8 parts, no floor, no own orders.
    pub fn new(max_fee_bps: Option<Bps>) -> Self {
        Self {
            max_fee_bps,
            max_parts: MaxParts::default(),
            floor: Floor::default(),
            own: HashSet::new(),
        }
    }

    /// The most fills; a payment from a venue settled inside itself (Lighter, Aster) is always
    /// one fill.
    #[must_use]
    pub fn max_parts(mut self, parts: MaxParts) -> Self {
        self.max_parts = parts;
        self
    }

    /// The least the take delivers.
    #[must_use]
    pub fn floor(mut self, floor: Floor) -> Self {
        self.floor = floor;
        self
    }

    /// The caller's own orders, which a take never fills.
    #[must_use]
    pub fn own(mut self, orders: HashSet<OrderId>) -> Self {
        self.own = orders;
        self
    }
}

/// One level's terms in base units.
struct Quote {
    supply: U256,
    receive: U256,
    available: U256,
    min_fill: U256,
}

impl Quote {
    fn of(level: &Level) -> Result<Self, BookError> {
        let (supply, receive) = (level.supply.get(), level.receive.get());
        if supply.is_zero() || receive.is_zero() {
            return Err(ZERO_PRICE);
        }
        Ok(Self {
            supply,
            receive,
            available: level.available.get(),
            min_fill: level.min_fill.get(),
        })
    }

    /// The payment for `out`, rounded up.
    fn cost(&self, out: U256) -> Result<U256, BookError> {
        fees::mul_div_ceil(out, self.receive, self.supply).ok_or(OVERFLOW)
    }
}

fn add(a: U256, b: U256) -> Result<U256, BookError> {
    a.checked_add(b).ok_or(OVERFLOW)
}

fn sub(a: U256, b: U256) -> Result<U256, BookError> {
    a.checked_sub(b).ok_or(OVERFLOW)
}

/// A book with its fee and terms resolved for one computation.
struct Matcher<'a> {
    book: &'a Book,
    terms: &'a SweepTerms,
    fee: Fee,
    max_parts: u8,
    now: DateTime<Utc>,
}

impl<'a> Matcher<'a> {
    fn new(book: &'a Book, terms: &'a SweepTerms, now: DateTime<Utc>) -> Result<Self, BookError> {
        if terms.max_fee_bps.is_some_and(|max| book.taker_fee_bps > max) {
            return Err(FEE_ABOVE_MAX);
        }
        let fee = fees::take_fee(&book.from, book.taker_fee_bps, terms.max_fee_bps);
        // The server pays from a venue settled inside itself with one transfer: one fill.
        let max_parts =
            if deployments::is_venue_settled(&book.from) { 1 } else { terms.max_parts.get() };
        Ok(Self { book, terms, fee, max_parts, now })
    }

    /// Levels a take can fill now, in book order: live and not the caller's own.
    fn levels(&self) -> impl Iterator<Item = &'a Level> + '_ {
        self.book.levels.iter().filter(|level| {
            level.valid_until > self.now && !self.terms.own.contains(&level.order_id)
        })
    }

    fn fee_on(&self, amount: U256) -> U256 {
        fees::fee_on(amount, self.fee.bps)
    }

    /// What every level could take at once, fees included: an upper bound of useful payments.
    fn budget(&self) -> Result<U256, BookError> {
        let max_fill = self.book.max_fill.get();
        self.levels().try_fold(U256::ZERO, |sum, level| {
            let quote = Quote::of(level)?;
            let paid = quote.cost(quote.available.min(max_fill))?;
            let fee = if self.fee.on_delivery { U256::ZERO } else { self.fee_on(paid) };
            add(sum, add(paid, fee)?)
        })
    }

    /// Matches `amount` in book order as the server does: each part's price rounded up, its
    /// output rounded down; a payment fee comes on top, a delivery fee out of the output.
    /// Nothing executes below the floor.
    fn sweep(&self, amount: U256) -> Result<Sweep, BookError> {
        let max_fill = self.book.max_fill.get();
        let floor = self.terms.floor;
        let (mut left, mut out, mut parts) = (amount, U256::ZERO, 0u8);
        for level in self.levels() {
            if parts == self.max_parts || left.is_zero() {
                break;
            }
            let quote = Quote::of(level)?;
            let priced = if self.fee.on_delivery {
                left
            } else {
                fees::price_within(left, self.fee.bps).ok_or(OVERFLOW)?
            };
            let capacity = quote.available.min(max_fill);
            // Beyond 2^256 - 1, what `priced` buys is above any capacity.
            let got = fees::mul_div(priced, quote.supply, quote.receive)
                .map_or(capacity, |affordable| affordable.min(capacity));
            if got.is_zero() || got < quote.min_fill {
                continue;
            }
            let delivered = if self.fee.on_delivery { sub(got, self.fee_on(got))? } else { got };
            if delivered < floor.part.get() {
                continue;
            }
            let paid = quote.cost(got)?;
            let charged = if self.fee.on_delivery { paid } else { add(paid, self.fee_on(paid))? };
            out = add(out, delivered)?;
            left = sub(left, charged)?;
            parts += 1;
        }
        if parts == 0 || out < floor.total.get() {
            return Ok(Sweep::NONE);
        }
        Ok(Sweep { out: Amount::new(out), pay: Amount::new(sub(amount, left)?), parts })
    }

    /// The smallest executable payment, by bisection over the (monotonic) sweep.
    fn minimum_take(&self) -> Result<Option<U256>, BookError> {
        let mut high = self.budget()?;
        if self.sweep(high)?.parts == 0 {
            return Ok(None);
        }
        let mut low = U256::ZERO;
        while high - low > U256::from(1u8) {
            let middle = low + (high - low) / U256::from(2u8);
            if self.sweep(middle)?.parts > 0 {
                high = middle;
            } else {
                low = middle;
            }
        }
        Ok(Some(high))
    }

    /// Whether what `swept` leaves of `amount` cannot buy one more part: an output unit, or the
    /// per-part floor with its delivery fee added back, fee rounding included.
    fn pays_all(&self, amount: U256, swept: &Sweep) -> Result<bool, BookError> {
        let mut best: Option<Quote> = None;
        for level in self.levels() {
            let quote = Quote::of(level)?;
            let better = best.as_ref().is_none_or(|kept| {
                fees::product_lt(quote.receive, kept.supply, kept.receive, quote.supply)
            });
            if better {
                best = Some(quote);
            }
        }
        let Some(best) = best else { return Ok(true) };
        let least = self.terms.floor.part.get();
        let scale = U256::from(10_000u16);
        let bps = U256::from(self.fee.bps.get());
        // The least output g with g − ⌊g × bps / 10 000⌋ ≥ the per-part floor.
        let part = if self.fee.on_delivery && !least.is_zero() {
            let kept = scale.checked_sub(bps).filter(|kept| !kept.is_zero()).ok_or(WHOLE_FEE)?;
            add(
                fees::mul_div(least - U256::from(1u8), scale, kept).ok_or(OVERFLOW)?,
                U256::from(1u8),
            )?
        } else {
            least
        };
        let unit = best.cost(part.max(U256::from(1u8)))?;
        // Rounding the total fee down can make the next output unit cost one more payment unit.
        let cost = if self.fee.on_delivery || bps.is_zero() {
            unit
        } else {
            add(add(unit, self.fee_on(unit))?, U256::from(1u8))?
        };
        Ok(sub(amount, swept.pay.get())? < cost)
    }
}

impl Book {
    /// What paying `amount` (in `from` base units) takes from this book now.
    ///
    /// A sweep that executes nothing (below the floor, no fillable level) has zero parts.
    pub fn sweep(
        &self,
        amount: Amount,
        terms: &SweepTerms,
        now: DateTime<Utc>,
    ) -> Result<Sweep, BookError> {
        Matcher::new(self, terms, now)?.sweep(amount.get())
    }

    /// The smallest payment that executes now; `None` if none does.
    pub fn minimum_take(
        &self,
        terms: &SweepTerms,
        now: DateTime<Utc>,
    ) -> Result<Option<Amount>, BookError> {
        Ok(Matcher::new(self, terms, now)?.minimum_take()?.map(Amount::new))
    }

    /// The smallest take and the take at capacity, or at `budget` (in `from` base units, after
    /// withdrawal fees and reserves) if smaller; `None` if nothing executes within it. Reserves
    /// nothing; ignores account funding.
    pub fn limits(
        &self,
        budget: Option<Amount>,
        terms: &SweepTerms,
        now: DateTime<Utc>,
    ) -> Result<Option<Limits>, BookError> {
        let matcher = Matcher::new(self, terms, now)?;
        let budget = match budget {
            Some(budget) => budget.get(),
            None => matcher.budget()?,
        };
        let maximum = matcher.sweep(budget)?;
        if maximum.parts == 0 {
            return Ok(None);
        }
        let Some(least) = matcher.minimum_take()? else { return Ok(None) };
        Ok(Some(Limits { minimum: matcher.sweep(least)?, maximum }))
    }

    /// That this book pays all of `amount_in` but for rounding, for at least `min_out`, within
    /// the accepted fee, above the floor: the reprice a transfer runs before money moves.
    pub fn check(
        &self,
        amount_in: Amount,
        min_out: Amount,
        terms: &SweepTerms,
        now: DateTime<Utc>,
    ) -> Result<(), BookError> {
        let matcher = Matcher::new(self, terms, now)?;
        let amount = amount_in.get();
        let swept = matcher.sweep(amount)?;
        if swept.parts == 0 {
            return Err(NO_PRICE);
        }
        if !matcher.pays_all(amount, &swept)? || swept.out < min_out {
            return Err(BELOW_MINIMUM);
        }
        Ok(())
    }
}

/// The least output to accept for a quoted `amount_out` with `slippage`, rounded down; not a
/// liquidity or delivery guarantee.
pub fn slippage_minimum(amount_out: Amount, slippage: Bps) -> Amount {
    // ⌊a × (10 000 − s) / 10 000⌋ = a − ⌈a × s / 10 000⌉, computed without overflow; the
    // subtrahend is at most `a` since `s ≤ 10 000`.
    let (units, scale, bps) = (amount_out.get(), U256::from(10_000u16), U256::from(slippage.get()));
    let slipped = units / scale * bps + (units % scale * bps).div_ceil(scale);
    Amount::new(units - slipped)
}

/// The least a take to `to` delivers, as `GET /v1/venues` lists it: the venue's release minimum
/// in all, or for a [`Destination::Wallet`] delivery the chain's wallet minimum in each part.
/// Nothing for a venue the list lacks.
pub fn delivery_floor(venues: &[Venue], to: &VenueId, destination: &Destination) -> Floor {
    let venue = venues.iter().find(|venue| venue.id == *to);
    if *destination == Destination::Wallet {
        let part = venue.and_then(|venue| venue.chain.as_ref()?.wallet.as_ref()).map(|w| w.minimum);
        Floor::new(Amount::ZERO, part.unwrap_or_default())
    } else {
        Floor::new(venue.and_then(|venue| venue.release_minimum).unwrap_or_default(), Amount::ZERO)
    }
}

#[cfg(test)]
mod tests;
