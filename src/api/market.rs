//! Market data reads: venues, books, markets, volume, info, health and trade proofs.

use super::Market;
use crate::{
    Result,
    model::{Book, Health, Info, Markets, TradeProof, Venue, Volume},
    transport::{Call, ops},
    types::{FillId, VenueId},
};

impl Market<'_> {
    /// The venues of this deployment, enabled or not (`GET /v1/venues`).
    pub async fn venues(&self) -> Result<Vec<Venue>> {
        self.inner.send(Call::new(&ops::GET_VENUES), self.auth).await
    }

    /// The executable quotes for paying on `from` and receiving on `to` (`GET /v1/books`).
    pub async fn book(&self, from: &VenueId, to: &VenueId) -> Result<Book> {
        let call = Call::new(&ops::GET_BOOK).query("from", from).query("to", to);
        self.inner.send(call, self.auth).await
    }

    /// Prices, liquidity and volume per venue (`GET /v1/markets`).
    pub async fn markets(&self) -> Result<Markets> {
        self.inner.send(Call::new(&ops::GET_MARKETS), self.auth).await
    }

    /// The trading volume between `a` and `b` over the last 24 hours, in `b` base units
    /// (`GET /v1/volume`).
    pub async fn volume(&self, a: &VenueId, b: &VenueId) -> Result<Volume> {
        let call = Call::new(&ops::GET_VOLUME).query("a", a).query("b", b);
        self.inner.send(call, self.auth).await
    }

    /// The deployment's version, operator, limits, timing and chains (`GET /v1/info`).
    pub async fn info(&self) -> Result<Info> {
        self.inner.send(Call::new(&ops::GET_INFO), self.auth).await
    }

    /// The server's health (`GET /health`). A degraded server answers 503 with the same body,
    /// returned here as `Ok`.
    pub async fn health(&self) -> Result<Health> {
        self.inner.send(Call::new(&ops::GET_HEALTH), self.auth).await
    }

    /// A settled trade with everything signed for it (`GET /v1/trades/{id}/proof`).
    pub async fn trade_proof(&self, id: FillId) -> Result<TradeProof> {
        self.inner.send(Call::new(&ops::GET_TRADE_PROOF).segment(id), self.auth).await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::MockServer;

    use crate::{
        model::HealthStatus,
        testing::{
            http::{Expect, client, served},
            json::{fixture, list},
        },
        types::{FillId, VenueId},
    };

    #[tokio::test]
    async fn every_read_is_anonymous_and_names_its_terms() {
        let server = MockServer::start().await;
        let client = client(&server);
        let market = client.market();
        let (hl, pm) = (VenueId::Hyperliquid, VenueId::Polymarket);
        let venues = served(&server, Expect::get("/v1/venues", list("venue")), market.venues());
        assert_eq!(venues.await.unwrap()[0].id, VenueId::Polymarket);
        let books = Expect::get("/v1/books", fixture("book"))
            .query(&[("from", "hyperliquid"), ("to", "polymarket")]);
        served(&server, books, market.book(&hl, &pm)).await.unwrap();
        served(&server, Expect::get("/v1/markets", fixture("markets")), market.markets())
            .await
            .unwrap();
        let volume = Expect::get("/v1/volume", fixture("volume"))
            .query(&[("a", "hyperliquid"), ("b", "polymarket")]);
        served(&server, volume, market.volume(&hl, &pm)).await.unwrap();
        served(&server, Expect::get("/v1/info", fixture("info")), market.info()).await.unwrap();
        let id: FillId = fixture("trade_proof")["id"].as_str().unwrap().parse().unwrap();
        let proof = Expect::get(format!("/v1/trades/{id}/proof"), fixture("trade_proof"));
        assert_eq!(served(&server, proof, market.trade_proof(id)).await.unwrap().id, id);
    }

    #[tokio::test]
    async fn health_reads_a_503_as_an_answer() {
        let server = MockServer::start().await;
        let client = client(&server);
        let mut body = fixture("health");
        body["status"] = json!("degraded");
        let degraded = Expect::get("/health", body).status(503);
        let health = served(&server, degraded, client.market().health()).await.unwrap();
        assert_eq!(health.status, HealthStatus::Degraded);
    }
}
