//! Resource handles: cheap borrows of a [`Client`](crate::Client), `Session` or `Quoter`,
//! one per API area. A handle is reachable only from the credentials allowed to call it, so an
//! unauthorised call does not compile.

mod account;
mod bridges;
mod credentials;
mod direct;
mod events;
mod explorer;
mod fills;
mod market;
mod moves;
mod orders;
mod page;
mod takes;
mod transfers;
mod withdrawals;

use std::{fmt, sync::Arc};

pub use bridges::{BridgeRequest, WalletSource};
pub use credentials::CredentialRequest;
pub use direct::DirectRequest;
pub use moves::{MoveQuery, MoveRecipient, MoveRequest};
pub use transfers::RouteQuery;

use crate::{
    transport::{Auth, Inner},
    types::PageCursor,
};

/// The most items a history page returns.
const PAGE_LIMIT_MAX: u16 = 500;

/// Declares a handle: the client state and the credential its requests carry. The state is
/// borrowed as an `Arc` so a handle can hand owned copies to streams that outlive it.
macro_rules! handle {
    ($( $(#[$meta:meta])* $name:ident; )+) => {$(
        $(#[$meta])*
        #[derive(Clone, Copy)]
        pub struct $name<'a> {
            pub(crate) inner: &'a Arc<Inner>,
            pub(crate) auth: Auth<'a>,
        }

        impl fmt::Debug for $name<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($name))
                    .field("api", &self.inner.api)
                    .field("authenticated", &self.auth.is_some())
                    .finish()
            }
        }
    )+};
}

handle! {
    /// Public market data: venues, books, markets, volume, health. From
    /// [`Client::market`](crate::Client::market).
    Market;
    /// Transfers and trades: every user's from a `Client`, the session's own from a `Session`.
    Explorer;
    /// The public event journal, from [`Client::events`](crate::Client::events).
    PublicEvents;
    /// The reader's event journal, from a `Session` or `Quoter`.
    Events;
    /// The session's account: profile, balances, statistics, venue setup and links.
    Account;
    /// The session's transfers.
    Transfers;
    /// The session's takes.
    Takes;
    /// Fills of the session or of the quoter's identity.
    Fills;
    /// Orders of the session or of the quoter's identity.
    Orders;
    /// The session's withdrawals.
    Withdrawals;
    /// The session's moves: plans and the records their parts attach to.
    Moves;
    /// The session's external bridges.
    Bridges;
    /// The session's direct operations: GMX withdrawals its owner sends itself.
    Directs;
    /// The session's maker credentials.
    Credentials;
}

/// Which page of a history to read: the newest by default, then `after` a `next_cursor`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageRequest {
    after: Option<PageCursor>,
    limit: Option<u16>,
}

impl PageRequest {
    /// The newest page with at most `limit` items, clamped to `1..=500`.
    pub fn first(limit: u16) -> Self {
        Self { after: None, limit: Some(limit.clamp(1, PAGE_LIMIT_MAX)) }
    }

    /// The page after `cursor` (a previous page's `next_cursor`).
    #[must_use]
    pub fn after(mut self, cursor: PageCursor) -> Self {
        self.after = Some(cursor);
        self
    }

    /// The cursor the page follows, if any.
    pub(crate) fn cursor(&self) -> Option<&PageCursor> {
        self.after.as_ref()
    }

    /// The item limit; `None` leaves the server's default (100).
    pub(crate) fn limit(&self) -> Option<u16> {
        self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Client, Environment, testing::http::session};

    #[test]
    fn page_limit_is_clamped() {
        assert_eq!(PageRequest::first(0).limit(), Some(1));
        assert_eq!(PageRequest::first(501).limit(), Some(500));
        assert_eq!(PageRequest::first(50).limit(), Some(50));
        assert_eq!(PageRequest::default().limit(), None);
        let next = PageRequest::first(10).after(PageCursor::new("c1"));
        assert_eq!(next.cursor().map(PageCursor::as_str), Some("c1"));
        assert_eq!(next.limit(), Some(10));
    }

    #[test]
    fn handles_debug_without_the_token_and_their_futures_and_streams_are_send() {
        fn send<T: Send>(_: T) {}
        let client = Client::new(Environment::Production).unwrap();
        let debug = format!("{:?}", client.market());
        assert_eq!(debug, "Market { api: \"https://api.lamarge.trade\", authenticated: false }");
        let fills = Fills { inner: client.inner(), auth: Some(session()) };
        assert!(format!("{fills:?}").contains("authenticated: true"));
        send(client.market().venues());
        send(client.explorer().transfers_stream(10));
        send(client.events().snapshot());
        send(fills.list());
        send(fills.history_stream(10));
        send(
            Credentials { inner: client.inner(), auth: Some(session()) }
                .delete(uuid::Uuid::nil().into()),
        );
    }
}
