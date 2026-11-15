//! The session's account: profile, balances, statistics, exposure, venue setup, wallet funding,
//! escrow orders and links.

use super::Account;
use crate::{
    Result,
    model::{Balance, EscrowOrder, Exposure, Me, Stats, VenueSetup, WalletFunding},
    transport::{Call, ops},
    types::{TakeId, VenueId},
};

impl Account<'_> {
    /// The signed-in user and its linked accounts (`GET /v1/me`).
    pub async fn me(&self) -> Result<Me> {
        self.inner.send(Call::new(&ops::GET_ME), self.auth).await
    }

    /// The linked accounts' funds, one per venue (`GET /v1/balances`).
    pub async fn balances(&self) -> Result<Vec<Balance>> {
        self.inner.send(Call::new(&ops::GET_BALANCES), self.auth).await
    }

    /// Trading statistics (`GET /v1/me/stats`).
    pub async fn stats(&self) -> Result<Stats> {
        self.inner.send(Call::new(&ops::GET_STATS), self.auth).await
    }

    /// Funds committed to trading (`GET /v1/me/exposure`).
    pub async fn exposure(&self) -> Result<Exposure> {
        self.inner.send(Call::new(&ops::GET_EXPOSURE), self.auth).await
    }

    /// The setup of the account linked on `venue` (`GET /v1/accounts/{venue}/setup`).
    pub async fn venue_setup(&self, venue: &VenueId) -> Result<VenueSetup> {
        self.inner.send(Call::new(&ops::GET_VENUE_SETUP).segment(venue), self.auth).await
    }

    /// The session address's wallet funds on `venue`'s escrow chain; with `take`, as that
    /// unpaid take may spend them (`GET /v1/wallets/{venue}`).
    pub async fn wallet_funding(
        &self,
        venue: &VenueId,
        take: Option<TakeId>,
    ) -> Result<WalletFunding> {
        let mut call = Call::new(&ops::GET_WALLET_FUNDING).segment(venue);
        if let Some(take) = take {
            call = call.query("take_id", take);
        }
        self.inner.send(call, self.auth).await
    }

    /// The linked addresses' escrow orders with unlocked funds (`GET /v1/escrow`).
    pub async fn escrow(&self) -> Result<Vec<EscrowOrder>> {
        self.inner.send(Call::new(&ops::GET_ESCROW), self.auth).await
    }

    /// Unlinks the account on `venue` (`DELETE /v1/accounts/{venue}`).
    pub async fn unlink(&self, venue: &VenueId) -> Result<()> {
        self.inner.send(Call::new(&ops::DELETE_LINK).segment(venue), self.auth).await
    }
}

#[cfg(test)]
mod tests {
    use wiremock::MockServer;

    use super::*;
    use crate::testing::{
        http::{Expect, client, served, session},
        json::{fixture, list},
    };

    #[tokio::test]
    async fn every_call_carries_the_session_and_its_terms() {
        let server = MockServer::start().await;
        let client = client(&server);
        let account = Account { inner: client.inner(), auth: Some(session()) };
        served(&server, Expect::get("/v1/me", fixture("me")).bearer(), account.me()).await.unwrap();
        let balances = Expect::get("/v1/balances", list("balance")).bearer();
        served(&server, balances, account.balances()).await.unwrap();
        let stats = Expect::get("/v1/me/stats", fixture("stats")).bearer();
        served(&server, stats, account.stats()).await.unwrap();
        let exposure = Expect::get("/v1/me/exposure", fixture("exposure")).bearer();
        served(&server, exposure, account.exposure()).await.unwrap();
        let setup = Expect::get("/v1/accounts/aster/setup", fixture("venue_setup")).bearer();
        served(&server, setup, account.venue_setup(&VenueId::Aster)).await.unwrap();
        let take: TakeId = "0190f3a4-7b2c-7d10-8e4f-000000000005".parse().unwrap();
        let funding = Expect::get("/v1/wallets/polymarket", fixture("wallet_funding"))
            .query(&[("take_id", "0190f3a4-7b2c-7d10-8e4f-000000000005")])
            .bearer();
        served(&server, funding, account.wallet_funding(&VenueId::Polymarket, Some(take)))
            .await
            .unwrap();
        let funding = Expect::get("/v1/wallets/gmx", fixture("wallet_funding")).bearer();
        served(&server, funding, account.wallet_funding(&VenueId::Gmx, None)).await.unwrap();
        let escrow = Expect::get("/v1/escrow", list("escrow_order")).bearer();
        served(&server, escrow, account.escrow()).await.unwrap();
        let unlink = Expect::delete("/v1/accounts/lighter", None).bearer();
        served(&server, unlink, account.unlink(&VenueId::Lighter)).await.unwrap();
    }
}
