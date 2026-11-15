//! Cursor pagination shared by every history: one page, or every item as an owned stream.

use std::sync::Arc;

use futures_core::Stream;
use futures_util::{TryStreamExt as _, stream};
use serde::de::DeserializeOwned;

use super::{PAGE_LIMIT_MAX, PageRequest};
use crate::{
    Error, Result,
    model::Page,
    transport::{Auth, Call, Inner, ops::Op},
};

/// The `after` and `limit` query of a page.
pub(super) fn page_query<'a>(call: Call<'a>, page: &PageRequest) -> Call<'a> {
    let call = match page.cursor() {
        Some(cursor) => call.query("after", cursor),
        None => call,
    };
    match page.limit() {
        Some(limit) => call.query("limit", limit),
        None => call,
    }
}

/// The `limit` query of a list, clamped to `1..=500`.
pub(super) fn limit_query(call: Call<'_>, limit: Option<u16>) -> Call<'_> {
    match limit {
        Some(limit) => call.query("limit", limit.clamp(1, PAGE_LIMIT_MAX)),
        None => call,
    }
}

/// One page of `op`.
pub(super) async fn history<T: DeserializeOwned>(
    inner: &Inner,
    auth: Auth<'_>,
    op: &Op,
    page: &PageRequest,
) -> Result<Page<T>> {
    inner.send(page_query(Call::new(op), page), auth).await
}

/// Every item from `first` on, one page read at a time as items are consumed; ends after the
/// page without a next cursor or with the first error. `base` builds each page's call before
/// its cursor and limit are added. The stream owns the client state and the credential.
pub(super) fn every<T>(
    inner: &Arc<Inner>,
    auth: Auth<'_>,
    first: PageRequest,
    base: impl Fn() -> Call<'static> + Send + 'static,
) -> impl Stream<Item = Result<T>> + Send + Unpin + 'static
where
    T: DeserializeOwned + Send + 'static,
{
    let (inner, auth) = (Arc::clone(inner), auth.cloned());
    let pages = stream::try_unfold(Some(first), move |next| {
        let read = next.map(|request| (page_query(base(), &request), request));
        let (inner, auth) = (Arc::clone(&inner), auth.clone());
        async move {
            let Some((call, request)) = read else { return Ok::<_, Error>(None) };
            let page: Page<T> = inner.send(call, auth.as_ref()).await?;
            let next =
                page.next_cursor.filter(|_| page.has_more).map(|cursor| request.after(cursor));
            Ok(Some((stream::iter(page.data.into_iter().map(Ok)), next)))
        }
    });
    Box::pin(pages.try_flatten())
}
