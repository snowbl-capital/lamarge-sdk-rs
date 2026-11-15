//! Withdrawal history.

use futures_core::Stream;

use super::{
    PageRequest, Withdrawals,
    page::{every, history},
};
use crate::{
    Result,
    model::{Page, WithdrawalHistoryRow},
    transport::{Call, ops},
};

impl Withdrawals<'_> {
    /// One page of the session's withdrawals and sends, newest first.
    pub async fn history(&self, page: &PageRequest) -> Result<Page<WithdrawalHistoryRow>> {
        history(self.inner, self.auth, &ops::GET_WITHDRAWAL_HISTORY, page).await
    }

    /// Every withdrawal, newest first, reading pages of `page_size` (clamped to `1..=500`) as
    /// consumed. The stream owns what it needs, so it outlives the handle.
    pub fn history_stream(
        &self,
        page_size: u16,
    ) -> impl Stream<Item = Result<WithdrawalHistoryRow>> + Send + Unpin + 'static {
        every(self.inner, self.auth, PageRequest::first(page_size), || {
            Call::new(&ops::GET_WITHDRAWAL_HISTORY)
        })
    }
}

#[cfg(test)]
mod tests {
    use wiremock::MockServer;

    use super::*;
    use crate::{
        testing::{
            http::{Expect, client, served, session},
            json::{fixture, page},
        },
        types::PageCursor,
    };

    #[tokio::test]
    async fn history_carries_the_credential_and_the_page() {
        let server = MockServer::start().await;
        let client = client(&server);
        let withdrawals = Withdrawals { inner: client.inner(), auth: Some(session()) };
        let rows = page(&[fixture("withdrawal_history_page/data/0")], Some("w2"));
        let history = Expect::get("/v1/withdrawals/history", rows)
            .query(&[("after", "w1"), ("limit", "1")])
            .bearer();
        let request = PageRequest::first(1).after(PageCursor::new("w1"));
        served(&server, history, withdrawals.history(&request)).await.unwrap();
    }
}
