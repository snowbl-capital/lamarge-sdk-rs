# Changelog

All notable changes to this crate are documented here. The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html): before 1.0, a minor version may break.

## [0.1.0] - 2026-11-15

### Added

- `Client` for `Environment::Production`, `Development` or `Custom`: public reads (`market`, `explorer`, `events`), `check_deployment` against the pinned escrow deployments, retries with `RetryPolicy`.
- Sign-in with Ethereum through any `alloy_signer::Signer`: `Client::sign_in`, `resume_session` from a `SavedSession`, `Session::renew`, `sign_out` (`Error::SignedOut` afterwards); session reads (`account`, `transfers`, `takes`, `fills`, `orders`, `withdrawals`, `explorer`, `events`) with `history` pages and `history_stream`.
- Venue setup: `Session::link_account`, `set_up_venue` (permit, agent and API key steps), `authorize_wallet`.
- `Session::take`: takes at market from Hyperliquid, Polymarket and GMX (escrow payment), Lighter and Aster (in-venue payment), into an account or a wallet, funded by the venue or the wallet; journalled and resumable.
- `Session::transfer`: a take funded first by a margin withdrawal from Hyperliquid or Polymarket (sequential, or signed at once with the take), by wallet funds with the escrow's permit, or paid directly from Lighter or Aster; stale preparations and expired takes replaced; `TransferOutcome::Filled` or `Pending`.
- Moves and bridges, the manual path: `Session::moves` (`quote`, `create`, `get`, `close`) and `Session::bridges` (`create`, `get`, `fence`, `report_transaction`, `report_unsent`); `Session::sign_bridge` signs a HyperCore-origin Relay bridge and `Session::bridge_deposit` returns a wallet-origin bridge's `approve` and deposit calldata, each after checking the order against the caller's `flows::BridgeTerms` and the pinned Relay contracts (`tests/vectors/pins.json`, live orders in `tests/vectors/relay.json`); `TransferRequest::move_id` attaches a transfer to a move. The caller keeps the move's budget.
- `Session::withdraw`: withdrawals out of Hyperliquid and Polymarket (to the signer, or to the linked venue wallet the caller names), sends to another account on Hyperliquid, Polymarket and Aster; the outcome is kept under the request's id.
- Request types (`TakeRequest`, `TransferRequest`, `WithdrawalRequest`, `CapitalRequest`, `QuoteLinesRequest` and their parts) are `Serialize`/`Deserialize`: persist the whole request to resume it; their JSON changes only in a breaking release.
- Maker capital on Hyperliquid, Polymarket and GMX pools for a delegated quote identity: `credentials().create`, `rotate`, `delete`; `Session::maker(operator)` with `add_capital`, `wait_for_funding`, `wait_for_order`, `preparation`, `cancel_preparation`, `close_orders`.
- `Quoter` for a maker credential and its quote key: `lease`, `publish_lines`, `lines`, `cancel_all`, `exposure`, the identity's orders, fills and private events.
- Event feed as a `Stream` (`events::EventStream`: snapshot, WebSocket, HTTP replay, resync; follows session renewals) and `events::FillTracker`.
- Book math on `model::Book` (`sweep`, `minimum_take`, `limits`, `check`), `book::slippage_minimum`, `book::delivery_floor`.
- Typed amounts and IDs: `Amount`, `SignedAmount`, `Price`, `Bps`, `Decimal`, `ClientId` and per-resource ID newtypes; open enums decode unknown server values as `Unknown`.
- Signature persistence: `store::Journal` over `MemoryStore`, `FileStore` or your own `Store`.
- Features `aws-lc-rs` (default) and `ring` select the rustls provider.
- `ClientBuilder::lighter_reads`: the SDK's reads of Lighter's public API go through the caller's `LighterReads`, from its address and within its rate budget.

[0.1.0]: https://github.com/snowbl-capital/lamarge-sdk-rs/releases/tag/v0.1.0
