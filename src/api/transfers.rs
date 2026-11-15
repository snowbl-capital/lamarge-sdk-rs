//! Transfer reads, route quotes and cancellation.

use futures_core::Stream;

use super::{
    PageRequest, Transfers,
    page::{every, history},
};
use crate::{
    Result,
    model::{Destination, Funding, Page, Route, TransferIntent},
    transport::{Call, ops},
    types::{Amount, Bps, TransferId, VenueId},
};

/// What [`Transfers::quote`] prices: paying at most `amount_in` on `from` to receive on `to`.
///
/// Reserves nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteQuery {
    from: VenueId,
    to: VenueId,
    amount_in: Amount,
    min_out: Option<Amount>,
    max_fee_bps: Option<Bps>,
    funding: Option<Funding>,
    destination: Option<Destination>,
}

impl RouteQuery {
    /// Prices paying at most `amount_in` (in `from` base units) to receive on `to`.
    pub fn new(from: VenueId, to: VenueId, amount_in: Amount) -> Self {
        Self {
            from,
            to,
            amount_in,
            min_out: None,
            max_fee_bps: None,
            funding: None,
            destination: None,
        }
    }

    /// The least received for `amount_in`, in `to` base units: the worst price taken. A route
    /// under it shrinks to the largest amount that meets it.
    #[must_use]
    pub fn min_out(mut self, amount: Amount) -> Self {
        self.min_out = Some(amount);
        self
    }

    /// The highest taker fee the takes would accept; unset, legs pay no fee.
    #[must_use]
    pub fn max_fee_bps(mut self, bps: Bps) -> Self {
        self.max_fee_bps = Some(bps);
        self
    }

    /// Where the payment comes from; the server's default is
    /// [`Funding::Venue`](crate::model::Funding::Venue).
    #[must_use]
    pub fn funding(mut self, funding: Funding) -> Self {
        self.funding = Some(funding);
        self
    }

    /// Where the route delivers; the server's default is
    /// [`Destination::Venue`](crate::model::Destination::Venue).
    #[must_use]
    pub fn destination(mut self, destination: Destination) -> Self {
        self.destination = Some(destination);
        self
    }
}

impl Transfers<'_> {
    /// The takes that would pay now on `query.to` for `query.amount_in`; reserves nothing.
    pub async fn quote(&self, query: &RouteQuery) -> Result<Route> {
        let mut call = Call::new(&ops::GET_ROUTE)
            .query("from", &query.from)
            .query("to", &query.to)
            .query("amount_in", query.amount_in);
        if let Some(min_out) = query.min_out {
            call = call.query("min_out", min_out);
        }
        if let Some(bps) = query.max_fee_bps {
            call = call.query("max_fee_bps", bps.get());
        }
        if let Some(funding) = &query.funding {
            call = call.query("funding", funding);
        }
        if let Some(destination) = &query.destination {
            call = call.query("destination", destination);
        }
        self.inner.send(call, self.auth).await
    }

    /// One page of the session's transfers, newest first, cancelled and completed included.
    pub async fn history(&self, page: &PageRequest) -> Result<Page<TransferIntent>> {
        history(self.inner, self.auth, &ops::GET_TRANSFERS, page).await
    }

    /// Every transfer, newest first, reading pages of `page_size` (clamped to `1..=500`) as
    /// consumed. The stream owns what it needs, so it outlives the handle.
    pub fn history_stream(
        &self,
        page_size: u16,
    ) -> impl Stream<Item = Result<TransferIntent>> + Send + Unpin + 'static {
        every(self.inner, self.auth, PageRequest::first(page_size), || {
            Call::new(&ops::GET_TRANSFERS)
        })
    }

    /// A transfer and its linked operations.
    pub async fn get(&self, id: TransferId) -> Result<TransferIntent> {
        self.inner.send(Call::new(&ops::GET_TRANSFER).segment(id), self.auth).await
    }

    /// A transfer with its `funding_plan` computed from fresh balances and allowances, for a
    /// wallet review; not for polling.
    pub async fn get_with_funding_plan(&self, id: TransferId) -> Result<TransferIntent> {
        let call = Call::new(&ops::GET_TRANSFER).segment(id).query("funding_plan", true);
        self.inner.send(call, self.auth).await
    }

    /// Dismisses a transfer without a take, blocking its take and prepared withdrawal. Repeats
    /// succeed; refused with a take or a withdrawal in flight.
    pub async fn cancel(&self, id: TransferId) -> Result<()> {
        self.inner.send(Call::new(&ops::CANCEL_TRANSFER).segment(id), self.auth).await
    }
}

#[cfg(test)]
mod tests {
    use futures_util::TryStreamExt as _;
    use serde_json::Value;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param, query_param_is_missing},
    };

    use super::*;
    use crate::{
        Error,
        testing::{
            http::{Expect, client, collect_spawned, served, session},
            json::{fixture, page},
        },
        types::PageCursor,
    };

    fn intent() -> Value {
        fixture("transfer_page/data/0")
    }

    #[tokio::test]
    async fn every_call_sends_its_terms_with_the_session() {
        let server = MockServer::start().await;
        let client = client(&server);
        let transfers = Transfers { inner: client.inner(), auth: Some(session()) };
        let route = RouteQuery::new(VenueId::Gmx, VenueId::Lighter, Amount::from(1u64));
        let bare = Expect::get("/v1/routes", fixture("route"))
            .query(&[("from", "gmx"), ("to", "lighter"), ("amount_in", "1")])
            .bearer();
        served(&server, bare, transfers.quote(&route)).await.unwrap();
        let route = route
            .min_out(Amount::from(4_990_000u64))
            .max_fee_bps(Bps::new(5).unwrap())
            .funding(Funding::Wallet)
            .destination(Destination::Wallet);
        let every = Expect::get("/v1/routes", fixture("route"))
            .query(&[
                ("from", "gmx"),
                ("to", "lighter"),
                ("amount_in", "1"),
                ("min_out", "4990000"),
                ("max_fee_bps", "5"),
                ("funding", "wallet"),
                ("destination", "wallet"),
            ])
            .bearer();
        served(&server, every, transfers.quote(&route)).await.unwrap();
        let request = PageRequest::first(20).after(PageCursor::new("c1"));
        let listed = Expect::get("/v1/transfers", page(&[intent()], Some("c2")))
            .query(&[("after", "c1"), ("limit", "20")])
            .bearer();
        let listed = served(&server, listed, transfers.history(&request)).await.unwrap();
        assert_eq!(listed.next_cursor, Some(PageCursor::new("c2")));
        let id: TransferId = intent()["id"].as_str().unwrap().parse().unwrap();
        let one = format!("/v1/transfers/{id}");
        served(&server, Expect::get(&one, intent()).bearer(), transfers.get(id)).await.unwrap();
        let planned = Expect::get(&one, intent()).query(&[("funding_plan", "true")]).bearer();
        served(&server, planned, transfers.get_with_funding_plan(id)).await.unwrap();
        served(&server, Expect::delete(&one, None).bearer(), transfers.cancel(id)).await.unwrap();
    }

    #[tokio::test]
    async fn history_stream_outlives_its_client_and_follows_cursors_until_the_last_page() {
        let server = MockServer::start().await;
        let first = page(&[intent(), intent()], Some("c1"));
        Mock::given(method("GET"))
            .and(path("/v1/transfers"))
            .and(query_param_is_missing("after"))
            .and(query_param("limit", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(first))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/transfers"))
            .and(query_param("after", "c1"))
            .and(query_param("limit", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(&[intent()], None)))
            .expect(1)
            .mount(&server)
            .await;
        let stream =
            Transfers { inner: client(&server).inner(), auth: Some(session()) }.history_stream(2);
        assert_eq!(collect_spawned(stream).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn history_stream_yields_the_first_error_and_ends() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(401)).mount(&server).await;
        let client = client(&server);
        let mut stream =
            Transfers { inner: client.inner(), auth: Some(session()) }.history_stream(10);
        assert!(matches!(stream.try_next().await, Err(Error::Api(_))));
        assert!(stream.try_next().await.unwrap().is_none());
    }
}
