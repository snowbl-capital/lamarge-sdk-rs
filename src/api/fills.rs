//! Fill reads: current fills and fill history.

use futures_core::Stream;

use super::{
    Fills, PageRequest,
    page::{every, history},
};
use crate::{
    Result,
    model::{Fill, FillHistoryRow, Page},
    transport::{Call, ops},
};

impl Fills<'_> {
    /// Nonfinal fills plus the latest 50 final ones, newest first: a session's taker and maker
    /// fills, or a quoter identity's maker fills. Over 1000 the server answers
    /// `SNAPSHOT_TOO_LARGE`: page with [`history`](Self::history).
    pub async fn list(&self) -> Result<Vec<Fill>> {
        self.inner.send(Call::new(&ops::GET_FILLS), self.auth).await
    }

    /// One page of fill history, newest first.
    pub async fn history(&self, page: &PageRequest) -> Result<Page<FillHistoryRow>> {
        history(self.inner, self.auth, &ops::GET_FILL_HISTORY, page).await
    }

    /// Every fill, newest first, reading pages of `page_size` (clamped to `1..=500`) as
    /// consumed. The stream owns what it needs, so it outlives the handle.
    pub fn history_stream(
        &self,
        page_size: u16,
    ) -> impl Stream<Item = Result<FillHistoryRow>> + Send + Unpin + 'static {
        every(self.inner, self.auth, PageRequest::first(page_size), || {
            Call::new(&ops::GET_FILL_HISTORY)
        })
    }
}

#[cfg(test)]
mod tests {
    use wiremock::MockServer;

    use super::*;
    use crate::{
        Error,
        testing::{
            http::{Expect, client, collect_spawned, served, session},
            json::{fixture, list, page},
        },
        types::PageCursor,
    };

    #[tokio::test]
    async fn every_read_carries_the_credential() {
        let server = MockServer::start().await;
        let client = client(&server);
        let fills = Fills { inner: client.inner(), auth: Some(session()) };
        served(
            &server,
            Expect::get("/v1/fills", list("take_preparation/take/fills/0")).bearer(),
            fills.list(),
        )
        .await
        .unwrap();
        let row = fixture("fill_history_page/data/0");
        let request = PageRequest::default().after(PageCursor::new("f1"));
        let history =
            Expect::get("/v1/fills/history", page(std::slice::from_ref(&row), Some("f2")))
                .query(&[("after", "f1")])
                .bearer();
        served(&server, history, fills.history(&request)).await.unwrap();
        let rows =
            Expect::get("/v1/fills/history", page(&[row], None)).query(&[("limit", "50")]).bearer();
        served(&server, rows, collect_spawned(fills.history_stream(50))).await.unwrap();
    }

    #[tokio::test]
    async fn a_malformed_fill_answer_keeps_no_excerpt() {
        let server = MockServer::start().await;
        let client = client(&server);
        let fills = Fills { inner: client.inner(), auth: Some(session()) };
        let mut fill = fixture("take_preparation/take/fills/0");
        fill["created_at"] = "not a time".into();
        let malformed = Expect::get("/v1/fills", serde_json::json!([fill.clone()])).bearer();
        let error = served(&server, malformed, fills.list()).await.unwrap_err();
        let Error::Decode(decode) = &error else { panic!("{error:?}") };
        assert!(decode.excerpt().is_none());
        let secret = fill["secret"].as_str().unwrap().trim_start_matches("0x");
        assert!(!format!("{error} {error:?}").contains(&secret[..16]));
    }
}
