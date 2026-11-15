//! Quoting flows: leases and quote line publication.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    sync::PoisonError,
    time::Duration,
};

use alloy_primitives::Address;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use tracing::Instrument as _;
use uuid::Uuid;

use crate::{
    Error, Quoter, Result, deployments,
    model::{IdentityCancellation, Lease, QuoteLine, QuoteLines, QuoteLinesPreparation},
    protocol::checks::{
        details::{self, unsupported},
        lines::{LinesAuthority, LinesRequest, check_prepared_lines},
    },
    transport::{Call, ops},
    types::{Amount, Price, QuoteIdentityId, Revision, SignatureBytes, VenueId},
};

/// The lease lengths the API accepts.
const LEASE_TTL: std::ops::RangeInclusive<Duration> =
    Duration::from_secs(5)..=Duration::from_secs(120);
/// The latest last admission time of a publication, from now.
const MAX_VALIDITY_SECONDS: i64 = 300;
/// The longest round trip a lease reports.
const MAX_RTT_MS: u64 = 60_000;
/// The oldest round trip a lease reports.
const MAX_RTT_AGE_MS: u64 = 120_000;

/// A maker's prices and stock on one venue: what it pays to receive one unit there (`buy`),
/// what it asks to deliver one unit from its pool there (`sell`), in USDC from 0.90 to 1.10.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceLine {
    venue: VenueId,
    buy: Option<Price>,
    sell: Option<Price>,
    stock: Amount,
    min_fill: Option<Amount>,
}

impl PriceLine {
    /// A line offering `stock` (in `venue` base units) of the identity's pool on `venue`, without
    /// prices yet. Stock needs a sell price; zero stock delivers nothing.
    pub fn new(venue: VenueId, stock: Amount) -> Self {
        Self { venue, buy: None, sell: None, stock, min_fill: None }
    }

    /// Receives on this venue, paying `price` per unit.
    #[must_use]
    pub fn buy(self, price: Price) -> Self {
        Self { buy: Some(price), ..self }
    }

    /// Delivers from this venue's pool, asking `price` per unit.
    #[must_use]
    pub fn sell(self, price: Price) -> Self {
        Self { sell: Some(price), ..self }
    }

    /// The smallest fill of the pool's quotes, in `venue` base units (one token when unset).
    #[must_use]
    pub fn min_fill(self, amount: Amount) -> Self {
        Self { min_fill: Some(amount), ..self }
    }

    fn to_wire(&self) -> QuoteLine {
        QuoteLine {
            venue: self.venue.clone(),
            buy: self.buy,
            sell: self.sell,
            stock: self.stock,
            min_fill: self.min_fill,
        }
    }
}

/// A complete replacement of a quote identity's lines: a venue left out is removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteLinesRequest {
    lines: Vec<PriceLine>,
    recipients: BTreeMap<VenueId, Address>,
    valid_until: DateTime<Utc>,
}

impl QuoteLinesRequest {
    /// `lines`, one per venue; `recipients`, the maker's address paid on each venue it buys on;
    /// `valid_until`, the last admission time, within the next five minutes.
    pub fn new(
        lines: Vec<PriceLine>,
        recipients: BTreeMap<VenueId, Address>,
        valid_until: DateTime<Utc>,
    ) -> Self {
        Self { lines, recipients, valid_until }
    }
}

/// `PUT /v1/quote-identities/{id}/lease` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LeaseInput {
    pub(crate) ttl_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) api_rtt_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) api_rtt_age_ms: Option<u64>,
}

/// `POST /v1/quote-lines/prepare` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct QuoteLinesInput {
    pub(crate) quote_identity_id: QuoteIdentityId,
    pub(crate) expected_revision: Revision,
    /// Unix seconds.
    pub(crate) valid_until: u64,
    pub(crate) lines: Vec<QuoteLine>,
}

/// `POST /v1/quote-lines` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct QuoteLinesPublication {
    pub(crate) preparation_id: Uuid,
    pub(crate) signatures: Vec<DraftSignature>,
}

/// One draft's quote set signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DraftSignature {
    pub(crate) draft_id: Uuid,
    pub(crate) signature: SignatureBytes,
}

impl Quoter {
    /// Renews the identity's lease for `ttl` (5 to 120 seconds, whole seconds): its quotes
    /// admit takes only while a lease runs. Reports the round trip of the quoter's latest
    /// answered request (lease, lines, publication or cancellation) when it took at most a minute
    /// and completed at most two minutes ago.
    ///
    /// A `ttl` out of range fails with [`Error::InvalidRequest`] before any request.
    pub async fn lease(&self, ttl: Duration) -> Result<Lease> {
        if !LEASE_TTL.contains(&ttl) {
            return Err(Error::InvalidRequest("a lease lasts 5 to 120 seconds".into()));
        }
        let sample = *self.last_rtt().lock().unwrap_or_else(PoisonError::into_inner);
        let rtt = telemetry(sample, Instant::now());
        let body = LeaseInput {
            ttl_seconds: ttl.as_secs(),
            api_rtt_ms: rtt.map(|(ms, _)| ms),
            api_rtt_age_ms: rtt.map(|(_, age)| age),
        };
        let call = Call::new(&ops::RENEW_LEASE).segment(self.identity().id).json(&body)?;
        self.timed(self.transport().send(call, self.auth())).await
    }

    /// The identity's published lines and their revision (`0` before any).
    pub async fn lines(&self) -> Result<QuoteLines> {
        self.timed(self.transport().send(Call::new(&ops::GET_QUOTE_LINES), self.auth())).await
    }

    /// Replaces the identity's lines: every pool switches at once, or none. The lines are
    /// prepared against the published revision, and each pool's quote set is rebuilt from the
    /// request (the caller's prices, stock, minimum fill and recipients, a sequence above the
    /// pool's last, settling an hour after `valid_until`, in the pool's escrow domain) before
    /// the quote key signs it; only pools without stock may be cancelled.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidRequest`] before any request: a venue twice, a buy price above its sell
    ///   price, stock without a sell price, a venue bought on without a recipient, or
    ///   `valid_until` not within the next five minutes; `Verification(Unsupported)` for a venue
    ///   this release does not pin.
    /// - `Verification(..)` when the preparation differs from the request; nothing is signed.
    /// - `STATE_CONFLICT` (the lines changed since they were read) or `STALE_QUOTE`: publish
    ///   again.
    ///
    /// # Cancellation
    ///
    /// Dropping the future is safe: an unpublished preparation expires in five minutes.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn run(quoter: &lamarge::Quoter) -> Result<(), Box<dyn std::error::Error>> {
    /// use std::collections::BTreeMap;
    ///
    /// use lamarge::flows::{PriceLine, QuoteLinesRequest};
    /// use lamarge::types::{Amount, Price, VenueId};
    ///
    /// let stock = Amount::parse_venue(&VenueId::Polymarket, "500")?;
    /// let line = PriceLine::new(VenueId::Polymarket, stock)
    ///     .buy("0.998".parse::<Price>()?)
    ///     .sell("1.002".parse::<Price>()?);
    /// let recipients = BTreeMap::from([(VenueId::Polymarket, quoter.identity().quote_signer)]);
    /// let valid_until = chrono::Utc::now() + chrono::Duration::seconds(60);
    /// quoter.publish_lines(&QuoteLinesRequest::new(vec![line], recipients, valid_until)).await?;
    /// # Ok(()) }
    /// ```
    pub async fn publish_lines(&self, request: &QuoteLinesRequest) -> Result<QuoteLines> {
        let span = tracing::info_span!("lamarge.publish_lines", lines = request.lines.len());
        self.publish(request).instrument(span).await
    }

    async fn publish(&self, request: &QuoteLinesRequest) -> Result<QuoteLines> {
        // Not journalled: each preparation's drafts take new sequences and the server refuses a
        // stale revision or sequence, so lines signed again only supersede the earlier ones.
        let transport = self.transport();
        let valid_until = validate(request, transport.now())?;
        let lines: Vec<QuoteLine> = request.lines.iter().map(PriceLine::to_wire).collect();
        let (current, pools) =
            futures_util::future::try_join(self.lines(), self.timed(self.orders().list(None)))
                .await?;
        let identity = self.identity();
        let body = QuoteLinesInput {
            quote_identity_id: identity.id,
            expected_revision: current.revision,
            valid_until,
            lines,
        };
        let call = Call::new(&ops::PREPARE_QUOTE_LINES).json(&body)?;
        let prepared: QuoteLinesPreparation = self.timed(transport.send(call, self.auth())).await?;
        let asked =
            LinesRequest { lines: &body.lines, valid_until, expected_revision: current.revision };
        let authority = LinesAuthority {
            identity: identity.id,
            quote_signer: identity.quote_signer,
            pools: &pools,
            recipients: &request.recipients,
        };
        let verified = check_prepared_lines(
            &prepared,
            &asked,
            &authority,
            self.escrow_environment(),
            transport.now(),
        )?;
        let mut signatures = Vec::with_capacity(verified.len());
        for (draft, verified) in prepared.drafts.iter().zip(&verified) {
            let signature = self.sign(verified).await?;
            signatures.push(DraftSignature { draft_id: draft.draft_id, signature });
        }
        let publication = QuoteLinesPublication { preparation_id: prepared.id, signatures };
        let call = Call::new(&ops::PUBLISH_QUOTE_LINES).json(&publication)?;
        let published: QuoteLines = self.timed(transport.send(call, self.auth())).await?;
        tracing::info!(revision = %published.revision, drafts = prepared.drafts.len(), "lines published");
        Ok(published)
    }

    /// Pulls every quote of the identity at once: its generation and each of its orders'
    /// `cancel_epoch` advance; reserved fills continue. Never retried.
    pub async fn cancel_all(&self) -> Result<IdentityCancellation> {
        let call = Call::new(&ops::CANCEL_IDENTITY_QUOTES).segment(self.identity().id);
        self.timed(self.transport().send(call, self.auth())).await
    }

    /// Runs `request`, keeping its round trip when it is answered.
    async fn timed<T>(&self, request: impl Future<Output = Result<T>>) -> Result<T> {
        let started = Instant::now();
        let answer = request.await?;
        let completed = Instant::now();
        *self.last_rtt().lock().unwrap_or_else(PoisonError::into_inner) =
            Some((completed - started, completed));
        Ok(answer)
    }
}

/// The lease's `(api_rtt_ms, api_rtt_age_ms)` at `now` from the latest round trip and its
/// completion: only a round trip of at most a minute, completed at most two minutes ago.
fn telemetry(sample: Option<(Duration, Instant)>, now: Instant) -> Option<(u64, u64)> {
    let (rtt, completed) = sample?;
    let rtt = u64::try_from(rtt.as_millis()).ok()?;
    let age = u64::try_from(now.saturating_duration_since(completed).as_millis()).ok()?;
    (rtt <= MAX_RTT_MS && age <= MAX_RTT_AGE_MS).then_some((rtt, age))
}

/// The request's last admission time in Unix seconds, once its lines and recipients are
/// consistent.
fn validate(request: &QuoteLinesRequest, now: DateTime<Utc>) -> Result<u64> {
    let invalid = |message: &'static str| Err(Error::InvalidRequest(message.into()));
    let mut venues = BTreeSet::new();
    for line in &request.lines {
        if deployments::chain(&line.venue).is_none() {
            return Err(unsupported(details::VENUE).into());
        }
        if !venues.insert(&line.venue) {
            return invalid("a venue appears twice");
        }
        if line.buy.zip(line.sell).is_some_and(|(buy, sell)| buy > sell) {
            return invalid("a buy price is above its sell price");
        }
        if line.sell.is_none() && !line.stock.get().is_zero() {
            return invalid("stock needs a sell price");
        }
        if line.buy.is_some() && !request.recipients.contains_key(&line.venue) {
            return invalid("a venue bought on needs a recipient");
        }
    }
    let ahead = request.valid_until.timestamp() - now.timestamp();
    if ahead <= 0 || ahead > MAX_VALIDITY_SECONDS {
        return invalid("valid_until must be within the next five minutes");
    }
    u64::try_from(request.valid_until.timestamp())
        .map_err(|_| Error::InvalidRequest("valid_until is before 1970".into()))
}

#[cfg(test)]
mod tests;
