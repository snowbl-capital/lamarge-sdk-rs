//! Relay (relay.link), the external bridge LaMarge plans and tracks: its pins, its order's id,
//! the check of an order against the caller's terms, and the payloads a bridge signs or sends.
//!
//! Follows `sdk/src/protocol/relay.ts`: `tests/vectors/pins.json` holds the pins equal, and
//! `tests/vectors/relay.json` holds orders of live quotes. Every address is pinned, never taken
//! from an answer: an order is accepted only if its hash, contracts, solver, recipients and
//! amounts match what the pins and the caller's terms rebuild.

use alloy_dyn_abi::TypedData;
use alloy_primitives::{Address, B256, Bytes, U256, address, hex};
use alloy_sol_types::{SolCall, SolStruct};
use serde::{Deserialize, Deserializer, de};
use serde_json::Value;

use super::{
    checks::details::{self, at, differs, stale, unsupported},
    eip712::{self, address_text},
    hyperliquid::SendAsset,
    signing::Verified,
};
use crate::{
    deployments,
    error::VerificationError,
    model::{MoveEndpoint, MoveVenue},
};

/// The EVM depository, the same address on every EVM chain.
pub(crate) const DEPOSITORY: Address = address!("4cd00e387622c35bddb9b4c962c136462338bc31");
/// `HyperCore`'s depository account, which receives the payer's `SendAsset`.
pub(crate) const HYPERLIQUID_DEPOSITORY: Address =
    address!("66cf0aace1b4e562593bec10ec7868fba9932224");
/// The router that runs destination calls and executes EVM payouts, on every EVM chain.
pub(crate) const ROUTER: Address = address!("0729e2a4716d9f7dd17858d5d504426d228697f7");
/// The solver every order names: it is credited the deposit and owes the fill or the refund.
pub(crate) const SOLVER: Address = address!("f70da97812cb96acdf810712aa562db8dfa3dbef");
/// The solver's chain.
pub(crate) const SOLVER_CHAIN_ID: &str = "base";

/// Polymarket Perps' deposit contract on Polygon (the escrow's `margin()`).
pub(crate) const POLYMARKET_DEPOSIT: Address = deployments::POLYMARKET_WITHDRAW_CONTRACT;
/// GMX's `MultichainTransferRouter` on Arbitrum.
pub(crate) const GMX_ROUTER: Address = address!("3f6772b95423fc03264adf90efb8a9922b6c8c6e");
/// GMX's multichain vault on Arbitrum.
pub(crate) const GMX_VAULT: Address = address!("ceaadfaf6a8c489b250e407987877c5fdfcdbe6e");
/// Aster's vault on BSC.
pub(crate) const ASTER_VAULT: Address = address!("128463a60784c4d3f46c23af3f65ed859ba87974");

/// How a Relay chain names accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Vm {
    Evm,
    Hyperliquid,
    /// Accounts and currencies are indexes.
    Lighter,
}

/// A Relay endpoint: Relay's chain name and id, the currency as orders write it, and the
/// decimals on each side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RelayEndpoint {
    pub(crate) chain: &'static str,
    pub(crate) chain_id: u64,
    pub(crate) currency: &'static str,
    /// Relay's decimals.
    pub(crate) decimals: u8,
    /// The endpoint's own decimals: LaMarge counts `HyperCore` USDC in 6, Relay in 8.
    pub(crate) units: u8,
    pub(crate) vm: Vm,
}

impl RelayEndpoint {
    /// An EVM endpoint's token.
    pub(crate) fn token(&self) -> Option<Address> {
        if self.vm == Vm::Evm { address_text(self.currency) } else { None }
    }
}

const fn evm(
    chain: &'static str,
    chain_id: u64,
    currency: &'static str,
    decimals: u8,
) -> RelayEndpoint {
    RelayEndpoint { chain, chain_id, currency, decimals, units: decimals, vm: Vm::Evm }
}

/// Wallet tokens a bridge can start from or deliver to.
pub(crate) const WALLETS: [RelayEndpoint; 5] = [
    evm("hyperevm", 999, "0xb88339cb7199b77e23db6e890353e22632ba630f", 6),
    evm("polygon", 137, "0xc011a7e12a19f7b1f670d46f03b03f3342e82dfb", 6),
    evm("polygon", 137, "0x3c499c542cef5e3811e1192ce70d8cc03d5c3359", 6),
    evm("arbitrum", 42161, "0xaf88d065e77c8cc2239327c5edb3a432268e5831", 6),
    evm("bnb", 56, "0x55d398326f99059ff775485246999027b3197955", 18),
];

/// `HyperCore` perps USDC: 16 zero bytes, 8 decimals.
const HYPERCORE: RelayEndpoint = RelayEndpoint {
    chain: "hyperliquid",
    chain_id: 1337,
    currency: "0x00000000000000000000000000000000",
    decimals: 8,
    units: 6,
    vm: Vm::Hyperliquid,
};
/// Lighter USDC: asset index 0.
const LIGHTER: RelayEndpoint = RelayEndpoint {
    chain: "lighter",
    chain_id: 3_586_256,
    currency: "0",
    decimals: 6,
    units: 6,
    vm: Vm::Lighter,
};

/// Each venue account as a bridge reaches it: `HyperCore` and Lighter natively, the others
/// through destination calls on their chain.
pub(crate) fn account(venue: &MoveVenue) -> Option<&'static RelayEndpoint> {
    match venue {
        MoveVenue::Hyperliquid => Some(&HYPERCORE),
        MoveVenue::Lighter => Some(&LIGHTER),
        MoveVenue::Polymarket => Some(&WALLETS[1]),
        MoveVenue::Gmx => Some(&WALLETS[3]),
        MoveVenue::Aster => Some(&WALLETS[4]),
        _ => None,
    }
}

/// The least a call-credited account receives: Polymarket's deposit minimum, 5 pUSD; GMX and
/// Aster have none.
pub(crate) fn account_minimum(venue: &MoveVenue) -> U256 {
    match venue {
        MoveVenue::Polymarket => U256::from(5_000_000u32),
        _ => U256::ZERO,
    }
}

/// Venue accounts a bridge debits: `HyperCore` only (Polymarket and GMX first move to the
/// payer's wallet).
pub(crate) fn is_origin(venue: &MoveVenue) -> bool {
    *venue == MoveVenue::Hyperliquid
}

/// Venue accounts credited through calls the router runs, rather than a plain payout.
pub(crate) fn is_call_venue(venue: &MoveVenue) -> bool {
    matches!(venue, MoveVenue::Polymarket | MoveVenue::Gmx | MoveVenue::Aster)
}

/// The Relay endpoint of `endpoint` as an origin (`origin`) or a destination; refused for
/// anything LaMarge does not bridge.
pub(crate) fn relay_endpoint(
    endpoint: &MoveEndpoint,
    origin: bool,
) -> Result<&'static RelayEndpoint, VerificationError> {
    if endpoint.venue == MoveVenue::Wallet {
        return WALLETS
            .iter()
            .find(|wallet| {
                Some(wallet.chain_id) == endpoint.chain_id && endpoint.token == wallet.token()
            })
            .ok_or(unsupported(details::BRIDGE_WALLET));
    }
    if origin && !is_origin(&endpoint.venue) {
        return Err(unsupported(details::BRIDGE_ORIGIN));
    }
    account(&endpoint.venue).ok_or(unsupported(details::BRIDGE_VENUE))
}

fn scale(endpoint: &RelayEndpoint) -> U256 {
    U256::from(10u8).pow(U256::from(endpoint.decimals - endpoint.units))
}

/// `amount` in the endpoint's own units, in Relay's (`HyperCore`: × 100, exact).
pub(crate) fn to_relay_units(amount: U256, endpoint: &RelayEndpoint) -> Option<U256> {
    amount.checked_mul(scale(endpoint))
}

/// A Relay amount in the endpoint's own units, rounded down.
pub(crate) fn from_relay_units(amount: U256, endpoint: &RelayEndpoint) -> U256 {
    amount / scale(endpoint)
}

/// Relay's order and calls, as its settlement hashes and its contracts decode them.
mod typed {
    alloy_sol_types::sol! {
        struct Order {
            string version;
            string solverChainId;
            address solver;
            uint256 salt;
            Input[] inputs;
            Output output;
            Fee[] fees;
        }
        struct Input {
            InputPayment payment;
            InputRefund[] refunds;
        }
        struct InputPayment {
            string chainId;
            bytes currency;
            uint256 amount;
            uint256 weight;
        }
        struct InputRefund {
            string chainId;
            bytes recipient;
            bytes currency;
            uint256 minimumAmount;
            uint32 deadline;
            bytes extraData;
        }
        struct Output {
            string chainId;
            OutputPayment[] payments;
            uint32 deadline;
            bytes[] calls;
            bytes extraData;
        }
        struct OutputPayment {
            bytes recipient;
            bytes currency;
            uint256 minimumAmount;
            uint256 expectedAmount;
        }
        struct Fee {
            string recipientChainId;
            bytes recipient;
            string currencyChainId;
            bytes currency;
            uint256 amount;
        }

        /// Relay's nonce mapping: ties a `HyperCore` nonce to an order.
        #[derive(serde::Serialize)]
        struct NonceMapping {
            string chainId;
            address wallet;
            address depositor;
            bytes32 id;
            uint256 nonce;
        }

        function approve(address spender, uint256 amount);
        function transfer(address to, uint256 amount);
        function depositErc20(address depositor, address currency, uint256 amount, bytes32 id);
        function deposit(address token, uint256 amount, address recipient);
        function bridgeIn(address account, address token);
        function depositFor(address token, address recipient, uint256 amount, uint256 referral);
        function cleanupErc20s(address[] tokens, address[] recipients, uint256[] amounts, bytes data);
    }
}

/// An order as Relay serves it (`protocol.v2.orderData`): amounts and deadlines in decimal, the
/// salt in hex, bytes in hex except Lighter's indexes.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Order {
    version: String,
    solver_chain_id: String,
    solver: String,
    #[serde(deserialize_with = "uint")]
    salt: U256,
    inputs: Vec<Input>,
    output: Output,
    fees: Vec<Fee>,
}

#[derive(Debug, Deserialize)]
struct Input {
    payment: InputPayment,
    refunds: Vec<Refund>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InputPayment {
    chain_id: String,
    currency: String,
    #[serde(deserialize_with = "uint")]
    amount: U256,
    #[serde(deserialize_with = "uint")]
    weight: U256,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Refund {
    chain_id: String,
    recipient: String,
    currency: String,
    #[serde(deserialize_with = "uint")]
    minimum_amount: U256,
    #[serde(deserialize_with = "uint")]
    deadline: U256,
    extra_data: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Output {
    chain_id: String,
    payments: Vec<Payment>,
    #[serde(deserialize_with = "uint")]
    deadline: U256,
    calls: Vec<String>,
    extra_data: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Payment {
    recipient: String,
    currency: String,
    #[serde(deserialize_with = "uint")]
    minimum_amount: U256,
    #[serde(deserialize_with = "uint")]
    expected_amount: U256,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fee {
    recipient_chain_id: String,
    recipient: String,
    currency_chain_id: String,
    currency: String,
    #[serde(deserialize_with = "uint")]
    amount: U256,
}

/// A JSON integer, a decimal string, or a `0x` hex string of at most 32 bytes.
fn uint<'de, D: Deserializer<'de>>(deserializer: D) -> Result<U256, D::Error> {
    let parsed = match Value::deserialize(deserializer)? {
        Value::Number(number) => number.as_u64().map(U256::from),
        Value::String(text) => match text.strip_prefix("0x") {
            Some(digits) if (1..=64).contains(&digits.len()) => {
                U256::from_str_radix(digits, 16).ok()
            }
            Some(_) => None,
            None => decimal(&text),
        },
        _ => None,
    };
    parsed.ok_or_else(|| de::Error::custom("invalid integer"))
}

fn decimal(text: &str) -> Option<U256> {
    let digits = (1..=78).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit());
    if digits { U256::from_str_radix(text, 10).ok() } else { None }
}

/// `0x` and an even number of hexadecimal digits.
fn bytes(text: &str) -> Option<Bytes> {
    let digits = text.strip_prefix("0x")?;
    if digits.len() % 2 != 0 { None } else { hex::decode(digits).ok().map(Bytes::from) }
}

/// Bytes as orders hash them on `chain`: Lighter's accounts and currencies are decimal
/// indexes, hashed as 16 big-endian bytes.
fn bytes_on(chain: &str, text: &str) -> Option<Bytes> {
    if chain == LIGHTER.chain {
        let index = u128::try_from(decimal(text)?).ok()?;
        Some(Bytes::copy_from_slice(&index.to_be_bytes()))
    } else {
        bytes(text)
    }
}

impl Order {
    fn read(value: &Value) -> Result<Self, VerificationError> {
        Self::deserialize(value).map_err(|_| differs(details::BRIDGE_ORDER))
    }

    /// Relay's EIP-712 hash of the order: its id.
    fn id(&self) -> Option<B256> {
        let deadline = |value: U256| u32::try_from(value).ok();
        let inputs = self
            .inputs
            .iter()
            .map(|input| {
                let payment = &input.payment;
                let refunds = input
                    .refunds
                    .iter()
                    .map(|refund| {
                        Some(typed::InputRefund {
                            chainId: refund.chain_id.clone(),
                            recipient: bytes_on(&refund.chain_id, &refund.recipient)?,
                            currency: bytes_on(&refund.chain_id, &refund.currency)?,
                            minimumAmount: refund.minimum_amount,
                            deadline: deadline(refund.deadline)?,
                            extraData: bytes(&refund.extra_data)?,
                        })
                    })
                    .collect::<Option<_>>()?;
                Some(typed::Input {
                    payment: typed::InputPayment {
                        chainId: payment.chain_id.clone(),
                        currency: bytes_on(&payment.chain_id, &payment.currency)?,
                        amount: payment.amount,
                        weight: payment.weight,
                    },
                    refunds,
                })
            })
            .collect::<Option<_>>()?;
        let output = &self.output;
        let payments = output
            .payments
            .iter()
            .map(|payment| {
                Some(typed::OutputPayment {
                    recipient: bytes_on(&output.chain_id, &payment.recipient)?,
                    currency: bytes_on(&output.chain_id, &payment.currency)?,
                    minimumAmount: payment.minimum_amount,
                    expectedAmount: payment.expected_amount,
                })
            })
            .collect::<Option<_>>()?;
        let fees = self
            .fees
            .iter()
            .map(|fee| {
                Some(typed::Fee {
                    recipientChainId: fee.recipient_chain_id.clone(),
                    recipient: bytes(&fee.recipient)?,
                    currencyChainId: fee.currency_chain_id.clone(),
                    currency: bytes(&fee.currency)?,
                    amount: fee.amount,
                })
            })
            .collect::<Option<_>>()?;
        let order = typed::Order {
            version: self.version.clone(),
            solverChainId: self.solver_chain_id.clone(),
            solver: address_text(&self.solver)?,
            salt: self.salt,
            inputs,
            output: typed::Output {
                chainId: output.chain_id.clone(),
                payments,
                deadline: deadline(output.deadline)?,
                calls: output.calls.iter().map(|call| bytes(call)).collect::<Option<_>>()?,
                extraData: bytes(&output.extra_data)?,
            },
            fees,
        };
        Some(order.eip712_hash_struct())
    }
}

/// The order's id: Relay's EIP-712 hash of `order`.
#[cfg(test)]
pub(crate) fn relay_order_id(order: &Value) -> Result<B256, VerificationError> {
    Order::read(order)?.id().ok_or(differs(details::BRIDGE_ORDER))
}

/// The `extraData` of a payout or refund on `chain`: on an EVM chain, the router that executes
/// it. Anything else could name another execution contract.
fn canonical_extra(chain: &str) -> Bytes {
    if chain == HYPERCORE.chain || chain == LIGHTER.chain {
        Bytes::new()
    } else {
        Bytes::copy_from_slice(ROUTER.into_word().as_slice())
    }
}

/// One router call as Relay's orders encode it: the target, then the calldata in Relay's own
/// layout, value zero.
fn call(to: Address, data: &[u8]) -> Bytes {
    let word = |n: usize| U256::from(n).to_be_bytes::<32>();
    let mut out = Vec::with_capacity(160 + data.len().next_multiple_of(32));
    out.extend_from_slice(to.into_word().as_slice());
    for n in [0x60, 0, 0x20, data.len()] {
        out.extend_from_slice(&word(n));
    }
    out.extend_from_slice(data);
    out.resize(160 + data.len().next_multiple_of(32), 0);
    out.into()
}

/// The calls the router runs to credit `recipient`'s account on `venue` (Polymarket, GMX,
/// Aster) with exactly `amount`, then sweep the rest of the output to `recipient`'s wallet.
pub(crate) fn destination_calls(
    venue: &MoveVenue,
    recipient: Address,
    amount: U256,
) -> Option<Vec<Bytes>> {
    if amount.is_zero() {
        return None;
    }
    let token = account(venue)?.token()?;
    let approve = |spender| typed::approveCall { spender, amount }.abi_encode();
    let credit = match venue {
        MoveVenue::Polymarket => [
            call(token, &approve(POLYMARKET_DEPOSIT)),
            call(POLYMARKET_DEPOSIT, &typed::depositCall { token, amount, recipient }.abi_encode()),
        ],
        MoveVenue::Gmx => [
            call(token, &typed::transferCall { to: GMX_VAULT, amount }.abi_encode()),
            call(GMX_ROUTER, &typed::bridgeInCall { account: recipient, token }.abi_encode()),
        ],
        MoveVenue::Aster => {
            let deposit = typed::depositForCall { token, recipient, amount, referral: U256::ZERO };
            [call(token, &approve(ASTER_VAULT)), call(ASTER_VAULT, &deposit.abi_encode())]
        }
        _ => return None,
    };
    // The router's whole remaining balance of `token`.
    let sweep = typed::cleanupErc20sCall {
        tokens: vec![token],
        recipients: vec![recipient],
        amounts: vec![U256::ZERO],
        data: Bytes::new(),
    };
    Some(credit.into_iter().chain([call(ROUTER, &sweep.abi_encode())]).collect())
}

/// Who a bridge delivers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recipient {
    Address(Address),
    Lighter(u64),
}

impl Recipient {
    /// Whether an order's `value` names this recipient on `endpoint`.
    fn is(self, value: &str, endpoint: &RelayEndpoint) -> bool {
        match (self, endpoint.vm) {
            (Self::Lighter(index), Vm::Lighter) => {
                value.bytes().all(|b| b.is_ascii_digit()) && value == index.to_string()
            }
            (Self::Address(address), Vm::Evm | Vm::Hyperliquid) => {
                address_text(value) == Some(address)
            }
            _ => false,
        }
    }
}

/// What a bridge must do, from the caller's own terms: the order must match it exactly.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Expectation<'a> {
    pub(crate) from: &'a MoveEndpoint,
    pub(crate) to: &'a MoveEndpoint,
    pub(crate) payer: Address,
    pub(crate) recipient: Recipient,
    /// Origin units.
    pub(crate) amount_in: U256,
    /// The part's floor, destination units.
    pub(crate) min_out: U256,
    /// Polymarket, GMX, Aster accounts: the exact account credit.
    pub(crate) credit: Option<U256>,
}

/// An order the checks accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Checked {
    pub(crate) id: B256,
    /// Unix seconds.
    pub(crate) deadline: u32,
}

fn ensure(ok: bool, detail: &'static str) -> Result<(), VerificationError> {
    if ok { Ok(()) } else { Err(differs(detail)) }
}

fn same_text(value: &str, pinned: &str) -> bool {
    value.eq_ignore_ascii_case(pinned)
}

/// Checks Relay's `order` against `expect` at `now` (Unix seconds) and returns its id.
pub(crate) fn check_bridge_order(
    order: &Value,
    expect: &Expectation<'_>,
    now: i64,
) -> Result<Checked, VerificationError> {
    let origin = relay_endpoint(expect.from, true)?;
    let destination = relay_endpoint(expect.to, false)?;
    let order = Order::read(order)?;
    ensure(order.version == "v1", details::ORDER_VERSION)?;
    ensure(
        order.solver_chain_id == SOLVER_CHAIN_ID && address_text(&order.solver) == Some(SOLVER),
        details::ORDER_SOLVER,
    )?;
    ensure(order.fees.is_empty(), details::ORDER_FEES)?;
    let [input] = order.inputs.as_slice() else { return Err(differs(details::ORDER_INPUTS)) };
    let payment = &input.payment;
    ensure(
        payment.chain_id == origin.chain && same_text(&payment.currency, origin.currency),
        details::ORDER_INPUT,
    )?;
    ensure(
        Some(payment.amount) == to_relay_units(expect.amount_in, origin)
            && payment.weight == U256::from(1u8),
        details::ORDER_AMOUNT,
    )?;
    // Relay always offers a `HyperCore` refund besides the move's own chains.
    let chains = [origin.chain, destination.chain, HYPERCORE.chain];
    for (index, refund) in input.refunds.iter().enumerate() {
        let to_payer = refund.chain_id != LIGHTER.chain
            && address_text(&refund.recipient) == Some(expect.payer);
        let to_recipient = refund.chain_id == destination.chain
            && expect.recipient.is(&refund.recipient, destination);
        let checks = ensure(
            chains.contains(&refund.chain_id.as_str()) && (to_payer || to_recipient),
            details::ORDER_REFUND,
        )
        .and_then(|()| {
            ensure(
                bytes(&refund.extra_data) == Some(canonical_extra(&refund.chain_id)),
                details::ORDER_REFUND_EXTRA,
            )
        });
        checks.map_err(|error| at(error, index))?;
    }
    let output = &order.output;
    ensure(output.chain_id == destination.chain, details::ORDER_OUTPUT_CHAIN)?;
    ensure(
        bytes(&output.extra_data) == Some(canonical_extra(&output.chain_id)),
        details::ORDER_OUTPUT_EXTRA,
    )?;
    let deadline = u32::try_from(output.deadline).map_err(|_| differs(details::ORDER_DEADLINE))?;
    if i64::from(deadline) <= now {
        return Err(stale(details::ORDER_DEADLINE));
    }
    let [paid] = output.payments.as_slice() else {
        return Err(differs(details::ORDER_PAYMENTS));
    };
    ensure(same_text(&paid.currency, destination.currency), details::ORDER_CURRENCY)?;
    let minimum = from_relay_units(paid.minimum_amount, destination);
    ensure(minimum >= expect.min_out, details::ORDER_MINIMUM)?;
    let calls: Option<Vec<Bytes>> = output.calls.iter().map(|call| bytes(call)).collect();
    if is_call_venue(&expect.to.venue) {
        let credit = expect.credit.ok_or(differs(details::ACCOUNT_CREDIT))?;
        ensure(credit >= expect.min_out && minimum >= credit, details::ACCOUNT_CREDIT)?;
        ensure(address_text(&paid.recipient) == Some(ROUTER), details::ORDER_RECIPIENT)?;
        let Recipient::Address(recipient) = expect.recipient else {
            return Err(differs(details::ORDER_RECIPIENT));
        };
        let expected = destination_calls(&expect.to.venue, recipient, credit);
        ensure(calls.is_some() && calls == expected, details::ORDER_CALLS)?;
    } else {
        ensure(calls.is_some_and(|calls| calls.is_empty()), details::ORDER_CALLS)?;
        ensure(expect.recipient.is(&paid.recipient, destination), details::ORDER_RECIPIENT)?;
    }
    let id = order.id().ok_or(differs(details::BRIDGE_ORDER))?;
    Ok(Checked { id, deadline })
}

/// A wallet-origin bridge's transactions: an `approve` of `amount` to the depository, needed
/// only when the allowance is short, then the deposit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Transactions {
    pub(crate) chain_id: u64,
    pub(crate) token: Address,
    /// To `token`.
    pub(crate) approve: Bytes,
    /// To [`DEPOSITORY`].
    pub(crate) deposit: Bytes,
}

/// The deposit of `amount` (origin units) for order `id` from `payer`'s wallet `origin`.
pub(crate) fn bridge_transactions(
    id: B256,
    origin: &RelayEndpoint,
    payer: Address,
    amount: U256,
) -> Option<Transactions> {
    let token = origin.token()?;
    Some(Transactions {
        chain_id: origin.chain_id,
        token,
        approve: typed::approveCall { spender: DEPOSITORY, amount }.abi_encode().into(),
        deposit: typed::depositErc20Call { depositor: payer, currency: token, amount, id }
            .abi_encode()
            .into(),
    })
}

/// A part's floor, in destination units: its share of the move's `min_out` at the move's rate,
/// rounded up. `None` for an empty move or above `2^256 - 1`.
pub(crate) fn part_floor(part: U256, min_out: U256, amount_in: U256) -> Option<U256> {
    if amount_in.is_zero() {
        return None;
    }
    Some(part.checked_mul(min_out)?.div_ceil(amount_in))
}

/// A `HyperCore` bridge's nonce, which the payer's send spends: a millisecond timestamp under
/// 30 minutes old and at most 5 minutes ahead of `now_ms`.
pub(crate) fn check_bridge_nonce(nonce: u64, now_ms: i64) -> Result<u64, VerificationError> {
    let nonce_ms = i64::try_from(nonce).map_err(|_| differs(details::NONCE))?;
    let fresh = nonce_ms > now_ms - 1_800_000 && nonce_ms < now_ms + 300_000;
    if fresh { Ok(nonce) } else { Err(differs(details::NONCE)) }
}

/// Relay's nonce mapping the `HyperCore` payer signs: it ties `nonce` to the order before the
/// `SendAsset` with that nonce reaches Relay.
pub(crate) fn bridge_authorization(id: B256, payer: Address, nonce: u64) -> TypedData {
    let mapping = typed::NonceMapping {
        chainId: HYPERCORE.chain.to_owned(),
        wallet: payer,
        depositor: payer,
        id,
        nonce: U256::from(nonce),
    };
    eip712::typed(&mapping, eip712::domain("RelayNonceMapping", "2", 1, Some(Address::ZERO)))
}

/// The `HyperCore` `SendAsset` of `amount` (6 decimals) from the payer's perps to Relay's
/// depository, its amount written with six decimals as Relay does.
pub(crate) fn bridge_send_asset(amount: U256, nonce: u64) -> SendAsset {
    let million = U256::from(1_000_000u32);
    let fraction = u32::try_from(amount % million).unwrap_or_default();
    SendAsset {
        hyperliquid_chain: "Mainnet".to_owned(),
        destination: format!("{HYPERLIQUID_DEPOSITORY:#x}"),
        source_dex: String::new(),
        destination_dex: String::new(),
        token: deployments::HYPERLIQUID_USDC.to_owned(),
        amount: format!("{}.{fraction:06}", amount / million),
        from_sub_account: String::new(),
        nonce,
    }
}

/// Both payloads a `HyperCore`-origin bridge signs, for `expect.payer`, until 30 s before the
/// order's deadline.
pub(crate) fn hypercore_payloads(
    checked: Checked,
    expect: &Expectation<'_>,
    nonce: u64,
) -> Result<[Verified; 2], VerificationError> {
    if relay_endpoint(expect.from, true)?.vm != Vm::Hyperliquid {
        return Err(unsupported(details::BRIDGE_ORIGIN));
    }
    if expect.amount_in.is_zero() || nonce == 0 {
        return Err(differs(details::NONCE));
    }
    let not_after = Some(i64::from(checked.deadline) - 30);
    let verified = |typed: TypedData| {
        let digest = typed.eip712_signing_hash().map_err(|_| differs(details::DIGEST))?;
        Verified::typed(typed, digest, expect.payer, not_after)
    };
    Ok([
        verified(bridge_authorization(checked.id, expect.payer, nonce))?,
        verified(bridge_send_asset(expect.amount_in, nonce).typed_data())?,
    ])
}

#[cfg(test)]
mod tests;
