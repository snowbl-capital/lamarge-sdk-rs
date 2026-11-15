//! Stable details of every check failure: the field or rule a [`VerificationError`] names.
//!
//! Every detail a check reports is a constant here, so the set is reviewable and each one is
//! covered by a tamper test.

use crate::error::{VerificationError, VerificationKind};

/// The typed data is not an object with exactly `types`, `primaryType`, `domain` and `message`.
pub(crate) const TYPED_DATA: &str = "typed_data";
/// The domain's keys or values are not the expected ones.
pub(crate) const DOMAIN: &str = "domain";
/// The primary type is not the expected struct.
pub(crate) const PRIMARY_TYPE: &str = "primaryType";
/// The types differ from the expected schema: names, fields, field types or order.
pub(crate) const TYPES: &str = "types";
/// The message's keys or values do not fit the schema.
pub(crate) const MESSAGE: &str = "message";
/// A Polymarket operation's `op` is missing or not an internal transfer.
pub(crate) const OP: &str = "op";
/// The payload names another signer than the signing key.
pub(crate) const SIGNER: &str = "signer";
/// The payload rebuilt from the checked terms hashes differently from the server's.
pub(crate) const DIGEST: &str = "digest";
/// The clock is past the latest moment the checks allowed signing.
pub(crate) const SIGNING_WINDOW: &str = "signing window";
/// The sign-in challenge is not the expected text for this origin, address and nonce.
pub(crate) const CHALLENGE: &str = "challenge";
/// The sign-in challenge is issued too late, expired or valid too long.
pub(crate) const CHALLENGE_WINDOW: &str = "challenge window";
/// A fill's fee is not the one its swap id carries.
pub(crate) const FILLS_TAKER_FEE: &str = "fills.taker_fee";
/// A fill's swap id is another fill's, is of another kind than its delivery (wallet or venue), or
/// takes its fee on the other side than its payment says.
pub(crate) const FILLS_SWAP_ID: &str = "fills.swap_id";
/// The book is unusable for book math: a zero price term or an overflow.
pub(crate) const BOOK: &str = "book";
/// A fill's fee is above what the taker accepted.
pub(crate) const FEE_ABOVE_ACCEPTED: &str = "fee above accepted";

// Deployment and registry.
/// A listed chain differs from the pinned one: id, token, decimals, permit domain or wallet token.
pub(crate) const REGISTRY: &str = "registry";
/// A listed or signed escrow is not the environment's pinned escrow on that chain.
pub(crate) const ESCROW: &str = "escrow";
/// A venue is listed as settled otherwise than pinned.
pub(crate) const SETTLEMENT: &str = "settlement";
/// The listed escrows belong to several environments, or not to the one the client names.
pub(crate) const ENVIRONMENT: &str = "environment";
/// No escrow venue is enabled.
pub(crate) const NO_ESCROW_VENUE: &str = "no escrow venue";
/// The venue is not one this SDK version trades, or not for this request.
pub(crate) const VENUE: &str = "venue";

// Requests to sign.
/// The requests to sign are not the expected ones: count, kinds, or a missing request.
pub(crate) const TO_SIGN: &str = "to_sign";
/// A request's kind is not the one its payment or step signs.
pub(crate) const TO_SIGN_KIND: &str = "to_sign.kind";
/// A request names another signer than the one that must sign it.
pub(crate) const TO_SIGN_SIGNER: &str = "to_sign.signer";
/// A request's kind is not the one this check signs.
pub(crate) const KIND: &str = "kind";

// Setup.
/// The setup step is not one this SDK version signs, or not on this venue.
pub(crate) const SETUP_STEP: &str = "step";
/// The setup step asks another kind of signature than the step's.
pub(crate) const SETUP_KIND: &str = "step.kind";
/// The signed owner is not the linked address.
pub(crate) const OWNER: &str = "owner";
/// The permit's spender is not the environment's escrow.
pub(crate) const SPENDER: &str = "spender";
/// The permit's allowance is not the unlimited one.
pub(crate) const VALUE: &str = "value";
/// A deadline is past, or further ahead than the request allows.
pub(crate) const DEADLINE: &str = "deadline";
/// The agent approval is not exactly the expected query string.
pub(crate) const AGENT_APPROVAL: &str = "agent approval";
/// The agent approval names another account.
pub(crate) const AGENT_USER: &str = "agent approval.user";
/// The agent approval's nonce is not the prepared one or not within a minute.
pub(crate) const AGENT_NONCE: &str = "agent approval.nonce";
/// The approved agent is not the environment's LaMarge agent at another address.
pub(crate) const AGENT: &str = "agent approval.agent";
/// The agent approval does not expire a year after its nonce.
pub(crate) const AGENT_EXPIRY: &str = "agent approval.expired";
/// The agent may do more than trade perps.
pub(crate) const AGENT_PERMISSIONS: &str = "agent approval.permissions";
/// The agent may be used from too many addresses or too wide a range.
pub(crate) const AGENT_ADDRESSES: &str = "agent approval.ipWhitelist";
/// The Lighter key registration is not Lighter's message for LaMarge's key on the account.
pub(crate) const LIGHTER_REGISTRATION: &str = "lighter registration";
/// Lighter does not show the signer as the owner of the account the key registers on.
pub(crate) const LIGHTER_OWNER: &str = "lighter registration.owner";
/// No account is linked where the request needs one, or not the one the request names.
pub(crate) const LINKED_ACCOUNT: &str = "linked account";

// Takes.
/// The take's or a fill's funding is not the asked one.
pub(crate) const FUNDING: &str = "funding";
/// The destination asked has no wallet token on the receiving venue.
pub(crate) const DESTINATION: &str = "destination";
/// A wallet take's least received is not the escrow's floor for all it delivers, or a venue
/// take names one.
pub(crate) const MINIMUM_RECEIVED: &str = "minimum_received";
/// Lighter does not show the recipient as the owner of the account a take delivers into.
pub(crate) const DESTINATION_OWNER: &str = "delivery_to.owner";
/// The wallet funds are not the payer's on the pinned escrow, or do not cover the take.
pub(crate) const WALLET_FUNDS: &str = "wallet funds";
/// The wallet funds were not observed within two minutes.
pub(crate) const WALLET_FUNDS_OBSERVED_AT: &str = "wallet funds.observed_at";
/// The fills are none or more parts than asked.
pub(crate) const FILLS: &str = "fills";
/// Two fills share an id.
pub(crate) const FILLS_ID: &str = "fills.id";
/// A fill is not the taker's.
pub(crate) const FILLS_ROLE: &str = "fills.role";
/// A fill pays on another venue.
pub(crate) const FILLS_FROM_VENUE: &str = "fills.from_venue";
/// A fill delivers on another venue.
pub(crate) const FILLS_TO_VENUE: &str = "fills.to_venue";
/// A fill names another payer.
pub(crate) const FILLS_PAYER: &str = "fills.payer";
/// A fill delivers to another recipient.
pub(crate) const FILLS_TAKER_RECIPIENT: &str = "fills.taker_recipient";
/// A fill delivers to another kind of destination.
pub(crate) const FILLS_DESTINATION: &str = "fills.destination";
/// A fill pays nothing, or more than asked.
pub(crate) const FILLS_AMOUNT_IN: &str = "fills.amount_in";
/// A fill delivers nothing, or less than asked.
pub(crate) const FILLS_AMOUNT_OUT: &str = "fills.amount_out";
/// A fill delivers inside a venue to another account than the recipient's, or through an
/// escrow to an account.
pub(crate) const FILLS_DELIVERY_TO: &str = "fills.delivery_to";
/// A fill's payment deadline is past or further than the pay window.
pub(crate) const FILLS_PAY_BY: &str = "fills.pay_by";
/// A fill's maker deadline is not after its payment deadline within the maker's timeout.
pub(crate) const FILLS_T1: &str = "fills.t1";
/// A fill's refund time is not after its maker deadline within the escrow's longest lock.
pub(crate) const FILLS_T2: &str = "fills.t2";
/// The fills pay nothing, or more than asked in all.
pub(crate) const AMOUNT_IN: &str = "amount_in";
/// The fills deliver less than asked in all.
pub(crate) const AMOUNT_OUT: &str = "amount_out";
/// The payment names another payer.
pub(crate) const PAYER: &str = "payer";
/// The request names no fill of the take.
pub(crate) const FILL_ID: &str = "fill_id";
/// The payment pays another number of fills.
pub(crate) const PAYMENTS: &str = "payments";
/// A fill has no payment, or several.
pub(crate) const PAYMENTS_SWAP_ID: &str = "payments.swapId";
/// A payment names another payer.
pub(crate) const PAYMENTS_PAYER: &str = "payments.payer";
/// A payment pays another recipient than the fill's maker.
pub(crate) const PAYMENTS_RECIPIENT: &str = "payments.recipient";
/// A payment locks under another hashlock than the fill's.
pub(crate) const PAYMENTS_HASHLOCK: &str = "payments.hashlock";
/// A payment pays another amount than the fill's.
pub(crate) const PAYMENTS_AMOUNT: &str = "payments.amount";
/// A payment refunds at another time than the fill's `t2`.
pub(crate) const PAYMENTS_REFUND_AT: &str = "payments.refundAt";
/// A payment's deadline is not the fill's `pay_by`.
pub(crate) const PAYMENTS_DEADLINE: &str = "payments.deadline";
/// A payment leaves another balance than the venue's perps (or Hyperliquid's spot).
pub(crate) const PAYMENTS_DEX: &str = "payments.dex";
/// A payment names another order than the fill's.
pub(crate) const PAYMENTS_ORDER_ID: &str = "payments.orderId";
/// A payment names another quote sequence than the fill's.
pub(crate) const PAYMENTS_SEQUENCE: &str = "payments.sequence";
/// A payment's delivery is not the fill's.
pub(crate) const PAYMENTS_AMOUNT_OUT: &str = "payments.amountOut";
/// A payment delivers to another recipient.
pub(crate) const PAYMENTS_RECEIVE_RECIPIENT: &str = "payments.receiveRecipient";
/// A payment delivers on another chain, token or escrow than the receiving venue's pinned one.
pub(crate) const PAYMENTS_RECEIVE: &str = "payments.receive";
/// The payment deposits more than it pays.
pub(crate) const DEPOSIT: &str = "deposit";
/// The Lighter transfer is not the fill's: account, amount or memo.
pub(crate) const LIGHTER_TRANSFER: &str = "lighter transfer";
/// The Lighter transfer is not sent by the environment's LaMarge key.
pub(crate) const LIGHTER_KEY: &str = "lighter transfer.api_key_index";
/// Lighter's fee on the transfer is above 1 USDC.
pub(crate) const LIGHTER_FEE: &str = "lighter transfer.usdc_fee";
/// Lighter does not show the maker's recipient as the owner of the account paid.
pub(crate) const LIGHTER_MAKER_ACCOUNT: &str = "lighter transfer.to_account_index";
/// Lighter does not show the payer as the owner of the account paying.
pub(crate) const LIGHTER_PAYER_ACCOUNT: &str = "lighter transfer.from_account_index";
/// The Aster transfer is not the expected request, or not the fill's payment.
pub(crate) const ASTER_TRANSFER: &str = "aster transfer";
/// The Aster transfer's nonce is not within the window LaMarge relays it in.
pub(crate) const ASTER_NONCE: &str = "aster transfer.nonce";

// Transfers.
/// The funding plan is missing, or names another venue, payer or amount.
pub(crate) const FUNDING_PLAN: &str = "funding_plan";
/// The funding plan withdraws otherwise than its amount says, or from a wallet-funded transfer.
pub(crate) const FUNDING_WITHDRAWAL: &str = "funding_plan.withdrawal_to";
/// The funding plan was not observed within two minutes.
pub(crate) const FUNDING_OBSERVED_AT: &str = "funding_plan.observed_at";
/// The funding plan's requests are not a permit, a withdrawal and the payment as planned.
pub(crate) const FUNDING_REQUESTS: &str = "funding_plan.requests";
/// The venue lists no wallet funding in its pinned escrow token.
pub(crate) const WALLET_FUNDING: &str = "wallet_funding";

// Withdrawals and sends.
/// The amount is zero, or not the asked amount.
pub(crate) const AMOUNT: &str = "amount";
/// The send goes to the signer itself.
pub(crate) const RECIPIENT: &str = "recipient";
/// The transfer is not on Hyperliquid's mainnet.
pub(crate) const HYPERLIQUID_CHAIN: &str = "hyperliquidChain";
/// The transfer moves another token than the venue's.
pub(crate) const TOKEN: &str = "token";
/// The transfer leaves a subaccount.
pub(crate) const FROM_SUB_ACCOUNT: &str = "fromSubAccount";
/// The transfer leaves another balance than perps or spot.
pub(crate) const SOURCE_DEX: &str = "sourceDex";
/// The transfer arrives in another balance than allowed.
pub(crate) const DESTINATION_DEX: &str = "destinationDex";
/// A nonce is not within a minute of the clock.
pub(crate) const NONCE: &str = "nonce";
/// A timestamp is not within a minute of the clock.
pub(crate) const TS: &str = "ts";
/// The withdrawal or operation leaves another account than the signer's.
pub(crate) const ACCOUNT: &str = "account";
/// The withdrawal or send goes to another address than asked.
pub(crate) const TO: &str = "to";
/// The withdrawal charges a fee.
pub(crate) const FEE: &str = "fee";
/// The operation's hash is not its `op`'s.
pub(crate) const DATA: &str = "data";
/// The Aster send leaves another account than the signer's.
pub(crate) const USER: &str = "user";
/// The Aster send goes to another account than asked.
pub(crate) const TO_ADDRESS: &str = "toAddress";
/// The Aster send does not carry a send's own id.
pub(crate) const CLIENT_TRAN_ID: &str = "clientTranId";

// Maker.
/// The preparation is not awaiting its signatures.
pub(crate) const STATE: &str = "state";
/// The preparation answers another request.
pub(crate) const CLIENT_REQUEST_ID: &str = "client_request_id";
/// The order belongs to another owner.
pub(crate) const ORDER_OWNER_ADDRESS: &str = "order.owner_address";
/// The order has another quote signer.
pub(crate) const ORDER_QUOTE_SIGNER: &str = "order.quote_signer";
/// The order quotes for another identity.
pub(crate) const ORDER_QUOTE_IDENTITY_ID: &str = "order.quote_identity_id";
/// The preparation adds another amount.
pub(crate) const INTENT_AMOUNT: &str = "intent.amount";
/// The preparation adds for another identity.
pub(crate) const INTENT_QUOTE_IDENTITY_ID: &str = "intent.quote_identity_id";
/// The preparation funds from another source.
pub(crate) const INTENT_FUNDING_SOURCE: &str = "intent.funding_source";
/// The preparation expects another revision than its order's or the published lines'.
pub(crate) const EXPECTED_REVISION: &str = "expected_revision";
/// An expiry is past, too far ahead, or not the one the terms give.
pub(crate) const EXPIRES_AT: &str = "expires_at";
/// The preparation funds another order than the existing pool.
pub(crate) const ORDER_ID: &str = "order.id";
/// The existing pool is not this identity's on the venue.
pub(crate) const EXISTING_ORDER: &str = "existing order";
/// The pool's capital or quotes changed since it was read.
pub(crate) const MAKER_CAPITAL: &str = "maker capital";
/// A new pool already has quotes or capital.
pub(crate) const ORDER: &str = "order";
/// The pool's available balance is below what was read.
pub(crate) const FUNDING_EXISTING_AVAILABLE: &str = "funding.existing_available";
/// The idle and deposited amounts do not make up the capital asked.
pub(crate) const FUNDING_IDLE_AMOUNT: &str = "funding.idle_amount";
/// Venue funding takes from the wallet.
pub(crate) const FUNDING_WALLET_AMOUNT: &str = "funding.wallet_amount";
/// Wallet funding takes from the venue.
pub(crate) const FUNDING_VENUE_AMOUNT: &str = "funding.venue_amount";
/// The deposit is not what the available balance leaves to cover.
pub(crate) const FUNDING_DEPOSIT: &str = "funding.deposit";
/// A sequence is not the expected one, or not above the pool's last.
pub(crate) const SEQUENCE: &str = "sequence";
/// The margin move is signed by another account than the owner.
pub(crate) const MARGIN_MOVE_SIGNER: &str = "margin_move.signer";
/// The margin move moves another amount than the venue deposit.
pub(crate) const MARGIN_MOVE_AMOUNT: &str = "margin_move.amount";
/// The funding is signed by another account than the owner.
pub(crate) const FUND_SIGNER: &str = "fund.signer";
/// The funding names another owner.
pub(crate) const FUND_OWNER: &str = "fund.owner";
/// The funding funds another order.
pub(crate) const FUND_OFFER_ID: &str = "fund.offerId";
/// The funding adds another amount.
pub(crate) const FUND_AMOUNT: &str = "fund.amount";
/// The funding deposits another amount.
pub(crate) const FUND_DEPOSIT: &str = "fund.deposit";
/// The funding authorizes another quote signer.
pub(crate) const FUND_QUOTE_SIGNER: &str = "fund.quoteSigner";
/// The funding authorizes another operator.
pub(crate) const FUND_OPERATOR: &str = "fund.operator";
/// The funding's quote floor is not above the order's last sequence.
pub(crate) const FUND_MIN_QUOTE_SEQUENCE: &str = "fund.minQuoteSequence";
/// The quote set names another order.
pub(crate) const OFFER_ID: &str = "offerId";
/// The quote set has no quotes, or another number than asked.
pub(crate) const QUOTES: &str = "quotes";
/// A quote receives on another venue, or on the pool's own.
pub(crate) const QUOTES_VENUE: &str = "quotes.venue";
/// A quote names another chain than its venue's.
pub(crate) const QUOTES_CHAIN_ID: &str = "quotes.chainId";
/// A quote names another token than its venue's.
pub(crate) const QUOTES_TOKEN: &str = "quotes.token";
/// A quote pays another recipient.
pub(crate) const QUOTES_RECIPIENT: &str = "quotes.recipient";
/// A quote's supply is zero or not the one asked.
pub(crate) const QUOTES_SUPPLY: &str = "quotes.supply";
/// A quote's receive is zero or not the one asked.
pub(crate) const QUOTES_RECEIVE: &str = "quotes.receive";
/// A quote's minimum fill is not the one asked, or above its supply.
pub(crate) const QUOTES_MIN_FILL: &str = "quotes.minFill";
/// The close names no order, an order twice, or one not asked.
pub(crate) const ORDERS: &str = "orders";
/// The created credential names another quote signer than asked.
pub(crate) const CREDENTIAL_QUOTE_SIGNER: &str = "credential.quote_signer";
/// The created credential carries other scopes than asked.
pub(crate) const CREDENTIAL_SCOPES: &str = "credential.scopes";

// Quote lines.
/// The preparation is for another quote identity.
pub(crate) const QUOTE_IDENTITY_ID: &str = "quote_identity_id";
/// The preparation's last admission time is not the asked one, or already past.
pub(crate) const VALID_UNTIL: &str = "valid_until";
/// The prepared lines differ from the caller's, or the lines are inconsistent.
pub(crate) const LINES: &str = "lines";
/// The drafts are not one per live pool with stock.
pub(crate) const DRAFTS: &str = "drafts";
/// A draft names another pool.
pub(crate) const DRAFTS_ORDER_ID: &str = "drafts.order_id";
/// A draft is for another venue than its pool's.
pub(crate) const DRAFTS_VENUE: &str = "drafts.venue";
/// A draft has no quotes, a venue twice, or another number than its set.
pub(crate) const DRAFTS_QUOTES: &str = "drafts.quotes";
/// A draft's quote receives where the caller does not buy, or as no direct pair.
pub(crate) const DRAFTS_QUOTES_VENUE: &str = "drafts.quotes.venue";
/// A draft's quote pays another recipient than the caller's on its venue.
pub(crate) const DRAFTS_QUOTES_RECIPIENT: &str = "drafts.quotes.recipient";
/// A draft's quote supplies another amount than the line's stock.
pub(crate) const DRAFTS_QUOTES_SUPPLY: &str = "drafts.quotes.supply";
/// A draft's quote receives another amount than the line's prices give.
pub(crate) const DRAFTS_QUOTES_RECEIVE: &str = "drafts.quotes.receive";
/// A draft's quote has another minimum fill than the line's.
pub(crate) const DRAFTS_QUOTES_MIN_FILL: &str = "drafts.quotes.min_fill";
/// A draft's budget is not the line's stock.
pub(crate) const QUOTE_SIZE: &str = "quote_size";
/// The pool is not live.
pub(crate) const POOL_STATE: &str = "pool.state";
/// The pool has another quote signer.
pub(crate) const POOL_QUOTE_SIGNER: &str = "pool.quote_signer";
/// A cancellation names a pool with stock, or none of the identity's.
pub(crate) const CANCELS: &str = "cancels";

// Moves and bridges.
/// The transfer belongs to another move than the request names.
pub(crate) const MOVE_ID: &str = "move_id";
/// The bridge's wallet is not a token a bridge starts from or delivers to.
pub(crate) const BRIDGE_WALLET: &str = "bridge wallet";
/// The bridge's origin is not one a bridge debits (`HyperCore` or a wallet), or not the one the
/// call signs or sends from.
pub(crate) const BRIDGE_ORIGIN: &str = "bridge origin";
/// The bridge's destination is not a venue a bridge credits.
pub(crate) const BRIDGE_VENUE: &str = "bridge venue";
/// The bridge's funds are read for another owner, token or spender than the payer, the origin
/// token and Relay's pinned depository.
pub(crate) const BRIDGE_FUNDING: &str = "bridge funding";
/// The bridge's order is unreadable, or does not hash to its id.
pub(crate) const BRIDGE_ORDER: &str = "order.data";
/// The order is of another version than `v1`.
pub(crate) const ORDER_VERSION: &str = "order.version";
/// The order names another solver than Relay's pinned one, or on another chain.
pub(crate) const ORDER_SOLVER: &str = "order.solver";
/// The order charges fees.
pub(crate) const ORDER_FEES: &str = "order.fees";
/// The order has more or fewer inputs than one.
pub(crate) const ORDER_INPUTS: &str = "order.inputs";
/// The input is on another chain or in another currency than the origin.
pub(crate) const ORDER_INPUT: &str = "order.inputs.payment";
/// The input's amount or weight is not the bridge's `amount_in`.
pub(crate) const ORDER_AMOUNT: &str = "order.inputs.payment.amount";
/// A refund goes to someone else than the payer (or the recipient, on the destination chain),
/// or to another chain than the move's.
pub(crate) const ORDER_REFUND: &str = "order.inputs.refunds";
/// A refund names another execution contract than Relay's router.
pub(crate) const ORDER_REFUND_EXTRA: &str = "order.inputs.refunds.extraData";
/// The output is on another chain than the destination.
pub(crate) const ORDER_OUTPUT_CHAIN: &str = "order.output.chainId";
/// The output names another execution contract than Relay's router.
pub(crate) const ORDER_OUTPUT_EXTRA: &str = "order.output.extraData";
/// The order's deadline is past.
pub(crate) const ORDER_DEADLINE: &str = "order.output.deadline";
/// The output has more or fewer payments than one.
pub(crate) const ORDER_PAYMENTS: &str = "order.output.payments";
/// The payment is in another currency than the destination's.
pub(crate) const ORDER_CURRENCY: &str = "order.output.payments.currency";
/// The guaranteed output is below the part's floor.
pub(crate) const ORDER_MINIMUM: &str = "order.output.payments.minimumAmount";
/// The payment goes to someone else than the recipient (or the router, for an account credited
/// through calls).
pub(crate) const ORDER_RECIPIENT: &str = "order.output.payments.recipient";
/// The account credit is missing, below the floor or above the guaranteed output.
pub(crate) const ACCOUNT_CREDIT: &str = "account credit";
/// The router's calls are not exactly the account credit and sweep the terms rebuild, or a plain
/// payout has calls.
pub(crate) const ORDER_CALLS: &str = "order.output.calls";

/// A [`VerificationKind::Differs`] failure of `detail`.
pub(crate) const fn differs(detail: &'static str) -> VerificationError {
    VerificationError { kind: VerificationKind::Differs, detail, index: None }
}

/// A [`VerificationKind::Stale`] failure of `detail`.
pub(crate) const fn stale(detail: &'static str) -> VerificationError {
    VerificationError { kind: VerificationKind::Stale, detail, index: None }
}

/// A [`VerificationKind::Changed`] failure of `detail`.
pub(crate) const fn changed(detail: &'static str) -> VerificationError {
    VerificationError { kind: VerificationKind::Changed, detail, index: None }
}

/// A [`VerificationKind::Deployment`] failure of `detail`.
pub(crate) const fn deployment(detail: &'static str) -> VerificationError {
    VerificationError { kind: VerificationKind::Deployment, detail, index: None }
}

/// A [`VerificationKind::Unsupported`] failure of `detail`.
pub(crate) const fn unsupported(detail: &'static str) -> VerificationError {
    VerificationError { kind: VerificationKind::Unsupported, detail, index: None }
}

/// `error` about the item at `index` of a list; an inner index is kept.
pub(crate) const fn at(error: VerificationError, index: usize) -> VerificationError {
    match error.index {
        Some(_) => error,
        None => VerificationError { index: Some(index), ..error },
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// Every detail is one of these constants: no check in `src/protocol` or `src/book.rs` builds
    /// a failure from a literal.
    #[test]
    fn checks_report_no_literal_detail() {
        fn visit(path: &Path, offenders: &mut Vec<String>) {
            if path.is_dir() {
                for entry in std::fs::read_dir(path).unwrap() {
                    visit(&entry.unwrap().path(), offenders);
                }
                return;
            }
            let text = std::fs::read_to_string(path).unwrap();
            let literals = [
                "differs(\"",
                "stale(\"",
                "changed(\"",
                "deployment(\"",
                "unsupported(\"",
                "detail: \"",
            ];
            offenders.extend(
                literals
                    .iter()
                    .filter(|literal| text.contains(*literal))
                    .map(|literal| format!("{}: {literal}", path.display())),
            );
        }
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        visit(&src.join("protocol"), &mut offenders);
        visit(&src.join("book.rs"), &mut offenders);
        assert!(offenders.is_empty(), "literal details: {offenders:?}");
    }
}
