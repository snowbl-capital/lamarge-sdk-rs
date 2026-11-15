//! Order reads, order history and per-order quote cancellation.

use futures_core::Stream;

use super::{
    Orders, PageRequest,
    page::{every, history},
};
use crate::{
    Result,
    model::{Order, OrderHistoryRow, Page, QuoteCancellation},
    transport::{Call, ops},
    types::{ClientId, OrderId},
};

impl Orders<'_> {
    /// Capital pools, open orders and orders holding funds, plus the latest 50 empty closed or
    /// failed ones, oldest first; a quoter sees its identity's only. With `client_order_id`, the
    /// one order it names, if any. Over 1000 the server answers `SNAPSHOT_TOO_LARGE`: page with
    /// [`history`](Self::history).
    pub async fn list(&self, client_order_id: Option<&ClientId>) -> Result<Vec<Order>> {
        let mut call = Call::new(&ops::GET_ORDERS);
        if let Some(id) = client_order_id {
            call = call.query("client_order_id", id);
        }
        self.inner.send(call, self.auth).await
    }

    /// One order.
    pub async fn get(&self, id: OrderId) -> Result<Order> {
        self.inner.send(Call::new(&ops::GET_ORDER).segment(id), self.auth).await
    }

    /// Removes the order from admission; advances its `cancel_epoch` on every call.
    pub async fn cancel_quotes(&self, id: OrderId) -> Result<QuoteCancellation> {
        self.inner.send(Call::new(&ops::DELETE_QUOTES).segment(id), self.auth).await
    }

    /// One page of order history, newest first.
    pub async fn history(&self, page: &PageRequest) -> Result<Page<OrderHistoryRow>> {
        history(self.inner, self.auth, &ops::GET_ORDER_HISTORY, page).await
    }

    /// Every order, newest first, reading pages of `page_size` (clamped to `1..=500`) as
    /// consumed. The stream owns what it needs, so it outlives the handle.
    pub fn history_stream(
        &self,
        page_size: u16,
    ) -> impl Stream<Item = Result<OrderHistoryRow>> + Send + Unpin + 'static {
        every(self.inner, self.auth, PageRequest::first(page_size), || {
            Call::new(&ops::GET_ORDER_HISTORY)
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::MockServer;

    use super::*;
    use crate::testing::{
        http::{Expect, client, served, session},
        json::{fixture, list, page},
    };

    #[tokio::test]
    async fn every_call_carries_the_credential_and_its_terms() {
        let server = MockServer::start().await;
        let client = client(&server);
        let orders = Orders { inner: client.inner(), auth: Some(session()) };
        let id = ClientId::new("pool-hl-1").unwrap();
        let named = Expect::get("/v1/orders", list("maker_preparation/order"))
            .query(&[("client_order_id", "pool-hl-1")])
            .bearer();
        served(&server, named, orders.list(Some(&id))).await.unwrap();
        served(&server, Expect::get("/v1/orders", json!([])).bearer(), orders.list(None))
            .await
            .unwrap();
        let order = fixture("maker_preparation/order");
        let order_id: OrderId = order["id"].as_str().unwrap().parse().unwrap();
        let one = Expect::get(format!("/v1/orders/{order_id}"), order).bearer();
        assert_eq!(served(&server, one, orders.get(order_id)).await.unwrap().id, order_id);
        let cancelled = json!({"order_id": order_id, "cancel_epoch": "8"});
        let cancel =
            Expect::delete(format!("/v1/orders/{order_id}/quotes"), Some(cancelled)).bearer();
        served(&server, cancel, orders.cancel_quotes(order_id)).await.unwrap();
        let rows = page(&[fixture("order_history_page/data/0")], None);
        let history = Expect::get("/v1/orders/history", rows).query(&[("limit", "5")]).bearer();
        served(&server, history, orders.history(&PageRequest::first(5))).await.unwrap();
    }
}
