//! OpenAPI conformance of the models: each registered model's fields equal its schemas'
//! properties, its fixture decodes with no ignored field, removing any required key fails, every
//! spec enum value is a known variant, and every object schema a mapped operation reaches is
//! registered or listed as unmodelled.
//!
//! Models are registered in [`registry`], enum sites in [`enums`]. A schema is named by its
//! component name or, for an inline object, by a site `Schema.property[.property…]`.

use std::{collections::BTreeSet, fmt, sync::OnceLock};

use serde::{
    Deserializer,
    de::{self, DeserializeOwned, Visitor},
    forward_to_deserialize_any,
};
use serde_json::Value;

use super::*;
use crate::{testing::json::fixture, types::SetupStepKind};

/// The public spec, read once (`tests/vectors/openapi.json`; JSON parses as YAML).
pub(crate) fn spec() -> &'static Value {
    static SPEC: OnceLock<Value> = OnceLock::new();
    SPEC.get_or_init(|| {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/openapi.json");
        let text = std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{path}: {error}"));
        serde_saphyr::from_str(&text).unwrap_or_else(|error| panic!("{path}: {error}"))
    })
}

/// Implemented by open enums: whether a parsed value is a known variant.
pub(crate) trait Variant {
    fn is_known(&self) -> bool;
}

/// A model with the schemas it implements and a fixture setting every property.
pub(crate) struct Case {
    rust: &'static str,
    schemas: &'static [&'static str],
    fields: fn() -> Vec<&'static str>,
    /// Fields next to a flattened model, whose fields `fields` lists.
    own: &'static [&'static str],
    /// Decodes a value, returning the paths serde ignored.
    decode: fn(Value) -> Result<Vec<String>, String>,
    fixture: Value,
}

fn case<T: DeserializeOwned>(schemas: &'static [&'static str], fixture: Value) -> Case {
    flat::<T, T>(schemas, fixture, &[])
}

/// A model `T` that flattens `Inner` (an `allOf`) and adds the fields `own`.
fn flat<T: DeserializeOwned, Inner: DeserializeOwned>(
    schemas: &'static [&'static str],
    fixture: Value,
    own: &'static [&'static str],
) -> Case {
    Case {
        rust: std::any::type_name::<T>(),
        schemas,
        fields: fields::<Inner>,
        own,
        decode: decode::<T>,
        fixture,
    }
}

/// An enum site, `"Schema"` or `"Schema.property"`, with the Rust type that parses its values.
pub(crate) struct EnumCase {
    site: &'static str,
    known: fn(&str) -> bool,
}

fn enum_case<T: DeserializeOwned + Variant>(site: &'static str) -> EnumCase {
    EnumCase {
        site,
        known: |value| {
            serde_json::from_value::<T>(Value::String(value.to_owned())).is_ok_and(|v| v.is_known())
        },
    }
}

/// Registers `$type` for `$schemas` with the fixture `$name` (a file, then an optional JSON
/// pointer into it: [`fixture`]).
macro_rules! cases {
    ($( $type:ty: [$($schema:literal),+] $name:literal, )+) => {
        vec![$( case::<$type>(&[$($schema),+], fixture($name)), )+]
    };
}

/// Every registered model.
fn registry() -> Vec<Case> {
    cases! {
        Book: ["Book"] "book",
        Level: ["Level"] "book/levels/0",
        // Market data.
        Venue: ["Venue"] "venue",
        EscrowChain: ["EscrowChain"] "venue/chain",
        WalletToken: ["EscrowChain.wallet"] "venue/chain/wallet",
        PermitDomain: ["EscrowChain.permit"] "venue/chain/permit",
        MarginMoveSpec: ["MarginMoveSpec"] "venue/margin_move",
        Markets: ["Markets"] "markets",
        MarketVenue: ["Markets.venues"] "markets/venues/0",
        Volume: ["Volume"] "volume",
        TradeProof: ["TradeProof"] "trade_proof",
        SignedAgreement: ["SignedAgreement"] "trade_proof/maker",
        VenueDelivery: ["TradeProof.delivery"] "trade_proof/delivery",
        VenuePayment: ["TradeProof.payment"] "trade_proof/payment",
        Transaction: ["Transaction"] "trade_proof/transactions/0",
        // Info and health.
        Info: ["Info"] "info",
        AccessPolicy: ["Info.access"] "info/access",
        ApiLimits: ["Limits"] "info/limits",
        SettlementTiming: ["SettlementTiming"] "info/settlement",
        DeliveryTiming: ["SettlementTiming.delivery"] "info/settlement/delivery",
        DeploymentChain: ["DeploymentChain"] "info/chains/0",
        Health: ["Health"] "health",
        ChainHealth: ["ChainHealth"] "health/chains/0",
        // Sign-in.
        crate::session::Challenge: ["Challenge"] "challenge",
        crate::SavedSession: ["Session"] "session",
        // Account.
        Me: ["Me"] "me",
        LinkedAccount: ["LinkedAccount"] "me/accounts/0",
        Balance: ["Balance"] "balance",
        XyzBalance: ["Balance.xyz"] "balance/xyz",
        Holdings: ["Holdings"] "balance/holdings",
        PortfolioObservation: ["PortfolioObservation"] "balance/portfolio",
        Stats: ["Stats"] "stats",
        MakerResults: ["MakerResults"] "stats/maker",
        PairResult: ["MakerResults.pairs"] "stats/maker/pairs/0",
        DailyResult: ["MakerResults.daily"] "stats/maker/daily/0",
        StatsAsset: ["StatsAsset"] "stats/assets/0",
        Exposure: ["Exposure"] "exposure",
        ExposureAsset: ["ExposureAsset"] "exposure/assets/0",
        ChainObservation: ["ChainObservation"] "exposure/assets/0/chain_observation",
        EscrowOrder: ["EscrowOrder"] "escrow_order",
        // Venue setup.
        SetupStatus: ["SetupStatus"] "balance/setup/0",
        SetupStep: ["SetupStep"] "venue_setup/steps/0",
        SetupRequest: ["SetupStep.to_sign"] "venue_setup/steps/0/to_sign",
        SetupAttestation: ["SetupAttestation"] "venue_setup/steps/0/to_sign/attestation",
        VenueSetup: ["VenueSetup"] "venue_setup",
        // Wallets.
        WalletFundingCapability: ["WalletFundingCapability"] "venue/chain/wallet_funding",
        WalletConversionCapability: ["WalletConversionCapability"] "venue/chain/wallet_funding/conversion",
        WalletFunding: ["WalletFunding"] "wallet_funding",
        ConversionBalance: ["WalletFunding.conversion"] "wallet_funding/conversion",
        WalletPermission: ["WalletPermission"] "wallet_funding/payment_setup",
        WalletPermit: ["WalletPermission.permit"] "wallet_funding/payment_setup/permit",
        PermitRequest: ["WalletPermission.permit.to_sign"] "wallet_funding/payment_setup/permit/to_sign",
        WalletReceipt: ["WalletReceipt"] "explorer_transfer_page/data/0/executions/0/received",
        TokenAmount: ["TokenAmount"] "explorer_transfer_page/data/0/received",
        // Events.
        Event: ["BookInvalidationEvent"] "event_page/events/0",
        Event: ["OrderPreviewEvent"] "event_page/events/1",
        Event: ["FillPreviewEvent"] "event_page/events/2",
        Event: ["LeasePreviewEvent"] "event_page/events/3",
        Event: ["WithdrawalPreviewEvent"] "event_page/events/4",
        Event: ["BalancePreviewEvent"] "event_page/events/5",
        Event: ["TradeEventEvent"] "event_page/events/6",
        EventPage: ["EventPage"] "event_page",
        BookInvalidation: ["BookInvalidation"] "event_page/events/0/data",
        OrderPreview: ["OrderPreview"] "event_page/events/1/data",
        FillPreview: ["FillPreview"] "event_page/events/2/data",
        LeasePreview: ["LeasePreview"] "event_page/events/3/data",
        WithdrawalPreview: ["WithdrawalPreview"] "event_page/events/4/data",
        BalancePreview: ["BalancePreview"] "event_page/events/5/data",
        TradeEvent: ["TradeEvent"] "event_page/events/6/data",
        PublicSnapshot: ["Snapshot"] "snapshot",
        PrivateSnapshot: ["PrivateSnapshot"] "private_snapshot",
        SnapshotBook: ["SnapshotBook"] "snapshot/books/0",
        SnapshotQuote: ["Quote", "SnapshotBook.quotes"] "snapshot/books/0/quotes/0",
        SnapshotIdentity: ["SnapshotIdentity"] "private_snapshot/quote_identities/0",
        // Explorer and trade history.
        ExplorerTransfer: ["ExplorerTransfer"] "explorer_transfer_page/data/0",
        ExplorerExecution: ["ExplorerExecution"] "explorer_transfer_page/data/0/executions/0",
        Page<ExplorerTransfer>: ["ExplorerTransferPage"] "explorer_transfer_page",
        PendingTrade: ["PendingTrade"] "pending_trade_list/data/0",
        TradeHistoryRow: ["TradeHistoryRow"] "trade_history_page/data/0",
        Page<TradeHistoryRow>: ["TradeHistoryPage"] "trade_history_page",
    }
    .into_iter()
    .chain(taker_cases())
    .chain(maker_cases())
    .chain(flattened_cases())
    .collect()
}

/// Models flattening an `allOf` part.
fn flattened_cases() -> Vec<Case> {
    vec![
        flat::<SnapshotOrder, OrderPreview>(
            &["SnapshotOrder"],
            fixture("private_snapshot/orders/0"),
            &["quote_identity_id", "revision", "amount"],
        ),
        flat::<SnapshotFill, FillPreview>(
            &["SnapshotFill"],
            fixture("private_snapshot/fills/0"),
            &["revision"],
        ),
        flat::<WithdrawalHistoryRow, Withdrawal>(
            &["WithdrawalHistoryRow"],
            fixture("withdrawal_history_page/data/0"),
            &["revision"],
        ),
    ]
}

/// Transfers, takes, fills and withdrawals.
fn taker_cases() -> Vec<Case> {
    cases! {
        // Transfers and routes.
        Page<TransferIntent>: ["TransferPage"] "transfer_page",
        TransferIntent: ["TransferIntent"] "transfer_page/data/0",
        PreparedWithdrawal: ["TransferIntent.withdrawal_preparation"] "transfer_page/data/0/withdrawal_preparation",
        FundingPlanError: ["TransferIntent.funding_plan_error"] "transfer_page/data/0/funding_plan_error",
        Route: ["Route"] "route",
        RouteLeg: ["Route.legs"] "route/legs/0",
        FundingPlan: ["FundingPlan"] "transfer_page/data/0/funding_plan",
        FundingRequest: ["FundingPlan.requests"] "transfer_page/data/0/funding_plan/requests/0",
        GroupedRoute: ["GroupedRoute"] "transfer_page/data/0/grouped",
        TransferThen: ["TransferThen"] "transfer_page/data/0/then",
        WalletConversion: ["WalletConversion"] "transfer_page/data/0/conversion",
        // Takes.
        TakePreparation: ["TakePreparation"] "take_preparation",
        Take: ["Take"] "take_preparation/take",
        ToSign: ["ToSign"] "to_sign",
        LighterTransferItem: ["LighterTransferItem"] "lighter_transfer_item",
        LighterTransferTerms: ["LighterTransferTerms"] "lighter_transfer_item/typed_data",
        AsterTransferItem: ["AsterTransferItem"] "take_preparation/to_sign/0",
        TakeAttestation: ["TakeAttestation"] "lighter_transfer_item/attestation",
        TakeTerms: ["TakeTerms"] "lighter_transfer_item/attestation/terms",
        WalletTakeTerms: ["WalletTakeTerms"] "wallet_take_terms",
        Page<TakeHistoryRow>: ["TakeHistoryPage"] "take_history_page",
        TakeHistoryRow: ["TakeHistoryPage.data"] "take_history_page/data/0",
        // Fills.
        Fill: ["Fill"] "take_preparation/take/fills/0",
        FillHistoryRow: ["FillHistoryRow"] "fill_history_page/data/0",
        Page<FillHistoryRow>: ["FillHistoryPage"] "fill_history_page",
        // Withdrawals.
        Withdrawal: ["Withdrawal"] "transfer_page/data/0/withdrawal",
        Page<WithdrawalHistoryRow>: ["WithdrawalHistoryPage"] "withdrawal_history_page",
        // Moves and bridges.
        MoveQuote: ["MoveQuote"] "move_quote",
        LamargePart: ["MoveQuote.lamarge"] "move_quote/lamarge",
        BridgeQuote: ["BridgeQuote"] "move_quote/bridge",
        Move: ["Move"] "move",
        MoveEndpoint: ["MoveEndpoint"] "move/to",
        MovePart: ["MovePart"] "move/transfers/0",
        Bridge: ["Bridge"] "move/bridges/0",
        BridgeOrder: ["Bridge.order"] "move/bridges/0/order",
        BridgeRefund: ["Bridge.refund"] "move/bridges/0/refund",
        DirectOperation: ["DirectOperation"] "direct_operation",
        // Request bodies.
        crate::api::DirectRequest: ["DirectRequest"] "direct_request",
        crate::flows::TakeInput: ["TakeInput"] "take_input",
        crate::api::MoveRequest: ["MoveRequest"] "move_request",
        crate::api::BridgeRequest: ["BridgeRequest"] "bridge_request",
        crate::api::WalletSource: ["BridgeRequest.from_wallet"] "bridge_request/from_wallet",
    }
}

/// Orders, quotes, maker preparations and credentials.
fn maker_cases() -> Vec<Case> {
    cases! {
        // Orders and quotes.
        Order: ["Order"] "maker_preparation/order",
        VenueAmount: ["Order.received_by_venue"] "maker_preparation/order/received_by_venue/0",
        OrderHistoryRow: ["OrderHistoryRow"] "order_history_page/data/0",
        Page<OrderHistoryRow>: ["OrderHistoryPage"] "order_history_page",
        MarginMove: ["MarginMove"] "maker_preparation/order/margin_move",
        QuoteCancellation: ["QuoteCancellation"] "quote_cancellation",
        Quote: ["Quote"] "quote_lines_preparation/drafts/0/quotes/0",
        QuoteSet: ["QuoteSet"] "maker_preparation/order/quote_set",
        OfferedQuote: ["QuoteSet.quotes"] "maker_preparation/order/quote_set/quotes/0",
        QuoteLine: ["QuoteLine"] "quote_lines/lines/0",
        QuoteLines: ["QuoteLines"] "quote_lines",
        QuoteLineDraft: ["QuoteLineDraft"] "quote_lines_preparation/drafts/0",
        QuoteLinesPreparation: ["QuoteLinesPreparation"] "quote_lines_preparation",
        Lease: ["Lease"] "lease",
        IdentityCancellation: ["IdentityCancellation"] "identity_cancellation",
        // Maker preparations and credentials.
        MakerPreparation: ["MakerPreparation"] "maker_preparation",
        MakerIntent: ["MakerPreparation.intent"] "maker_preparation/intent",
        MakerFunding: ["MakerPreparation.funding"] "maker_preparation/funding",
        CredentialCreated: ["CredentialCreated"] "credential_created",
        CredentialRotated: ["CredentialRotated"] "credential_rotated",
        CredentialInput: ["CredentialInput"] "credential_input",
        RotationBody: ["CredentialRotationInput"] "credential_rotation_input",
        // Maker and quoting request bodies.
        crate::flows::MakerInput: ["MakerInput"] "maker_input",
        crate::flows::LeaseInput: ["LeaseInput"] "lease_input",
        crate::flows::QuoteLinesInput: ["QuoteLinesInput"] "quote_lines_input",
        crate::flows::QuoteLinesPublication: ["QuoteLinesPublication"] "quote_lines_publication",
        crate::flows::DraftSignature: ["QuoteLinesPublication.signatures"] "quote_lines_publication/signatures/0",
    }
}

/// Every enum site the models parse.
fn enums() -> Vec<EnumCase> {
    vec![
        enum_case::<SetupStepKind>("Error.error.step"),
        enum_case::<AccessState>("AccessState"),
        enum_case::<Destination>("Destination"),
        enum_case::<Funding>("Funding"),
        enum_case::<FillState>("FillState"),
        enum_case::<FillRole>("FillRole"),
        enum_case::<PaymentLeg>("PaymentLeg"),
        enum_case::<MakerLeg>("MakerLeg"),
        enum_case::<OrderState>("OrderState"),
        enum_case::<HeldIn>("HeldIn"),
        enum_case::<WithdrawalState>("WithdrawalState"),
        enum_case::<PaymentDestination>("FillPreview.payment_destination"),
        enum_case::<MakerDestination>("FillPreview.maker_destination"),
        enum_case::<PaymentDestination>("SnapshotFill.payment_destination"),
        enum_case::<MakerDestination>("SnapshotFill.maker_destination"),
        enum_case::<SetupStepKind>("SetupStatus.step"),
        enum_case::<StepStatus>("SetupStatus.status"),
        enum_case::<SetupUse>("SetupStatus.needed_for"),
        enum_case::<SetupStepKind>("SetupStep.step"),
        enum_case::<StepStatus>("SetupStep.status"),
        enum_case::<SetupUse>("SetupStep.needed_for"),
        enum_case::<SignRequestKind>("SetupStep.to_sign.kind"),
        enum_case::<SignRequestKind>("WalletPermission.permit.to_sign.kind"),
        enum_case::<Settlement>("Venue.settlement"),
        enum_case::<AmountFormat>("MarginMoveSpec.amount"),
        enum_case::<MarginDestination>("MarginMoveSpec.destinations"),
        enum_case::<HealthStatus>("Health.status"),
        enum_case::<AccessMode>("Info.access.mode"),
        enum_case::<TransactionKind>("Transaction.kind"),
        enum_case::<TransactionStatus>("Transaction.status"),
        enum_case::<ExplorerTransferStatus>("ExplorerTransfer.status"),
        enum_case::<PaymentDestination>("Fill.payment_destination"),
        enum_case::<MakerDestination>("Fill.maker_destination"),
        enum_case::<WithdrawalKind>("Withdrawal.kind"),
        enum_case::<WithdrawalKind>("WithdrawalHistoryRow.kind"),
        enum_case::<Dex>("Withdrawal.from_dex"),
        enum_case::<Dex>("WithdrawalHistoryRow.from_dex"),
        enum_case::<Dex>("TransferIntent.from_dex"),
        enum_case::<ToSignKind>("ToSign.kind"),
        enum_case::<ToSignKind>("LighterTransferItem.kind"),
        enum_case::<ToSignKind>("AsterTransferItem.kind"),
        enum_case::<Destination>("WalletTakeTerms.destination"),
        enum_case::<WithdrawalDestination>("FundingPlan.withdrawal_to"),
        enum_case::<FundingRequestKind>("FundingPlan.requests.kind"),
        enum_case::<SetupStepKind>("TransferIntent.funding_plan_error.step"),
        enum_case::<PaymentSetup>("FundingPlan.payment_setup"),
        enum_case::<GroupedRouteState>("GroupedRoute.state"),
        enum_case::<ConversionState>("WalletConversion.state"),
        enum_case::<MakerPreparationState>("MakerPreparation.state"),
        enum_case::<FundingSource>("FundingSource"),
        enum_case::<CredentialScope>("CredentialScope"),
        enum_case::<MoveVenue>("MoveVenue"),
        enum_case::<LamargeUnavailable>("MoveQuote.lamarge_unavailable"),
        enum_case::<BridgeUnavailable>("MoveQuote.bridge_unavailable"),
        enum_case::<BridgeProvider>("BridgeQuote.provider"),
        enum_case::<BridgeProvider>("Bridge.provider"),
        enum_case::<MoveState>("Move.state"),
        enum_case::<BridgeState>("Bridge.state"),
        enum_case::<DirectKind>("DirectRequest.kind"),
        enum_case::<crate::types::VenueId>("DirectRequest.venue"),
        enum_case::<DirectState>("DirectOperation.state"),
    ]
}

/// Properties a registered model leaves out on purpose, with the reason.
const UNREAD: &[(&str, &str, &str)] =
    &[("Challenge", "typed_data", "never served on sign-in; the schema is shared with Aster")];

/// Whether `property` of `schemas` is in [`UNREAD`].
fn unread(schemas: &[&str], property: &str) -> bool {
    UNREAD.iter().any(|(schema, unread, _)| *unread == property && schemas.contains(schema))
}

/// Object schemas reachable from mapped operations that no model implements, with the reason.
const UNMODELLED: &[(&str, &str)] = &[
    ("TypedData", "kept as served (serde_json::Value): only the protocol checks interpret it"),
    ("Eip712Domain", "part of typed data kept as served"),
    ("Eip712Types", "part of typed data kept as served"),
    ("OpTypedData", "typed data kept as served"),
    (
        "Signatures",
        "request body each flow writes from the items it signs; its envelope is Aster's (0.2)",
    ),
    ("AsterEnvelope", "Aster margin moves: active market making, 0.2"),
    (
        "TransferInput",
        "request body the transfer and route flows write from their requests; \
         `previous_transfer_id` and `from_dex` are deferred",
    ),
    ("WalletConversionInput", "request body the transfer flow writes from `ConversionTerms`"),
    ("GroupedPreparation", "read by the route flow: its route and two take preparations"),
    ("PendingTradeList", "list envelope: `Explorer::pending_trades` returns its `data`"),
];

// --- schema reading --------------------------------------------------------------------------

fn schemas() -> &'static serde_json::Map<String, Value> {
    spec()["components"]["schemas"].as_object().expect("components.schemas")
}

fn schema(name: &str) -> &'static Value {
    schemas().get(name).unwrap_or_else(|| panic!("no schema {name}"))
}

/// The schema name of a `#/components/schemas/<name>` reference.
fn ref_name(value: &Value) -> Option<&str> {
    value.get("$ref")?.as_str()?.strip_prefix("#/components/schemas/")
}

fn resolve(value: &Value) -> &Value {
    ref_name(value).map_or(value, |name| resolve(schema(name)))
}

/// Steps into array items, a nullable `oneOf`'s other branch and a single-part `allOf`, without
/// resolving references.
fn peel(value: &Value) -> &Value {
    if let Some(items) = value.get("items") {
        return peel(items);
    }
    for key in ["oneOf", "allOf"] {
        let parts: Vec<&Value> = value
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|part| part.get("type").and_then(Value::as_str) != Some("null"))
            .collect();
        if let [part] = parts[..] {
            return peel(part);
        }
    }
    value
}

/// [`peel`] through references.
fn inner(value: &Value) -> &Value {
    let peeled = peel(value);
    ref_name(peeled).map_or(peeled, |name| inner(schema(name)))
}

/// The schema at a site: a component name, then one step per `.property`.
fn site(site: &str) -> Option<Value> {
    let mut parts = site.split('.');
    let mut target = schemas().get(parts.next()?)?.clone();
    for property in parts {
        target = inner(object(&target).properties.get(property)?).clone();
    }
    Some(target)
}

/// Properties and required keys of an object schema, `allOf` parts merged.
#[derive(Debug, Default)]
struct Object {
    properties: serde_json::Map<String, Value>,
    required: BTreeSet<String>,
}

fn object(value: &Value) -> Object {
    let value = resolve(value);
    let mut object = Object::default();
    for part in value.get("allOf").and_then(Value::as_array).into_iter().flatten() {
        let merged = self::object(part);
        object.properties.extend(merged.properties);
        object.required.extend(merged.required);
    }
    if let Some(properties) = value.get("properties").and_then(Value::as_object) {
        object.properties.extend(properties.clone());
    }
    for key in value.get("required").and_then(Value::as_array).into_iter().flatten() {
        object.required.insert(key.as_str().expect("required key").to_owned());
    }
    object
}

fn merged(schemas: &[&str]) -> Object {
    let mut all = Object::default();
    for name in schemas {
        let part = object(&site(name).unwrap_or_else(|| panic!("no schema {name}")));
        all.properties.extend(part.properties);
        all.required.extend(part.required);
    }
    all
}

/// The string enum values at a site, or `None` if the site has no enum.
fn enum_values(name: &str) -> Option<Vec<String>> {
    let target = site(name)?;
    let values = inner(&target).get("enum")?.as_array()?;
    Some(values.iter().filter_map(Value::as_str).map(str::to_owned).collect())
}

/// Whether a schema is an enum of strings (integer enums are modelled as integers).
fn is_string_enum(value: &Value) -> bool {
    value
        .get("enum")
        .and_then(Value::as_array)
        .is_some_and(|values| values.iter().any(Value::is_string))
}

/// The site an enum property is registered under: its enum schema's name, or `Schema.property`.
fn enum_site(schema_name: &str, property: &str, value: &Value) -> Option<String> {
    let peeled = peel(value);
    if let Some(name) = ref_name(peeled) {
        return is_string_enum(inner(peeled)).then(|| name.to_owned());
    }
    is_string_enum(peeled).then(|| format!("{schema_name}.{property}"))
}

/// The sites of the inline objects among a registered schema's properties.
fn inline_object_sites(schema_name: &str) -> Vec<String> {
    merged(&[schema_name])
        .properties
        .iter()
        .filter(|(_, value)| {
            let peeled = peel(value);
            ref_name(peeled).is_none()
                && (peeled.get("properties").is_some() || peeled.get("allOf").is_some())
        })
        .map(|(property, _)| format!("{schema_name}.{property}"))
        .collect()
}

// --- model reading ---------------------------------------------------------------------------

/// The wire field names of a `#[derive(Deserialize)]` struct.
fn fields<T: DeserializeOwned>() -> Vec<&'static str> {
    let mut recorded = None;
    let _ = T::deserialize(FieldRecorder(&mut recorded));
    recorded.unwrap_or_else(|| panic!("{} is not a struct", std::any::type_name::<T>())).to_vec()
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<Vec<String>, String> {
    let mut ignored = Vec::new();
    serde_ignored::deserialize::<_, _, T>(value, |path| ignored.push(path.to_string()))
        .map(|_| ignored)
        .map_err(|error| error.to_string())
}

/// A deserializer that records the field list a struct asks for, then stops.
struct FieldRecorder<'a>(&'a mut Option<&'static [&'static str]>);

#[derive(Debug)]
struct Stop;

impl fmt::Display for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("recorded")
    }
}

impl std::error::Error for Stop {}

impl de::Error for Stop {
    fn custom<T: fmt::Display>(_: T) -> Self {
        Self
    }
}

impl<'de> Deserializer<'de> for FieldRecorder<'_> {
    type Error = Stop;

    fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Stop> {
        Err(Stop)
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        fields: &'static [&'static str],
        _: V,
    ) -> Result<V::Value, Stop> {
        *self.0 = Some(fields);
        Err(Stop)
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
        option unit unit_struct newtype_struct seq tuple tuple_struct map enum identifier
        ignored_any
    }
}

// --- checks ----------------------------------------------------------------------------------

fn check_fields(case: &Case) -> Result<(), String> {
    let rust: BTreeSet<&str> =
        (case.fields)().into_iter().chain(case.own.iter().copied()).collect();
    let object = merged(case.schemas);
    let spec: BTreeSet<&str> = object
        .properties
        .keys()
        .map(String::as_str)
        .filter(|property| !unread(case.schemas, property))
        .collect();
    if rust == spec {
        return Ok(());
    }
    Err(format!(
        "{}: only in Rust {:?}, only in {:?} {:?}",
        case.rust,
        rust.difference(&spec).collect::<Vec<_>>(),
        case.schemas,
        spec.difference(&rust).collect::<Vec<_>>()
    ))
}

fn fixture_object(case: &Case) -> serde_json::Map<String, Value> {
    match &case.fixture {
        Value::Object(fixture) => fixture.clone(),
        other => panic!("{}: fixture is not a JSON object: {other:?}", case.rust),
    }
}

fn check_fixture(case: &Case) -> Result<(), String> {
    let fixture = fixture_object(case);
    let object = merged(case.schemas);
    let unset: Vec<_> = object
        .properties
        .keys()
        .filter(|key| !fixture.contains_key(*key) && !unread(case.schemas, key))
        .collect();
    if !unset.is_empty() {
        return Err(format!("{}: fixture leaves {unset:?} unset", case.rust));
    }
    let ignored =
        (case.decode)(Value::Object(fixture)).map_err(|error| format!("{}: {error}", case.rust))?;
    if ignored.is_empty() { Ok(()) } else { Err(format!("{}: ignored {ignored:?}", case.rust)) }
}

fn check_required(case: &Case) -> Result<(), String> {
    let fixture = fixture_object(case);
    let accepted: Vec<_> = merged(case.schemas)
        .required
        .into_iter()
        .filter(|key| {
            let mut without = fixture.clone();
            without.remove(key);
            (case.decode)(Value::Object(without)).is_ok()
        })
        .collect();
    if accepted.is_empty() {
        Ok(())
    } else {
        Err(format!("{}: decodes without required {accepted:?}", case.rust))
    }
}

fn check_enum(case: &EnumCase) -> Result<(), String> {
    let values = enum_values(case.site).ok_or_else(|| format!("{}: no enum there", case.site))?;
    let unknown: Vec<_> = values.iter().filter(|value| !(case.known)(value)).collect();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(format!("{}: unknown values {unknown:?}", case.site))
    }
}

/// Object schemas reachable from the request and answer bodies of the mapped operations.
fn reachable_objects() -> BTreeSet<String> {
    let mut pending: Vec<&'static Value> = Vec::new();
    for op in crate::transport::ops::ALL {
        let operation = &spec()["paths"][op.path][op.method.as_str().to_lowercase()];
        pending.push(&operation["requestBody"]["content"]["application/json"]["schema"]);
        for (status, answer) in operation["responses"].as_object().into_iter().flatten() {
            let status: u16 = status.parse().unwrap_or(0);
            if (200..300).contains(&status) || op.answers.contains(&status) {
                pending.push(&answer["content"]["application/json"]["schema"]);
            }
        }
    }
    let mut seen = BTreeSet::new();
    let mut objects = BTreeSet::new();
    while let Some(value) = pending.pop() {
        match value {
            Value::Object(map) => {
                if let Some(name) = ref_name(value) {
                    if seen.insert(name.to_owned()) {
                        let target = schema(name);
                        if target.get("properties").is_some() || target.get("allOf").is_some() {
                            objects.insert(name.to_owned());
                        }
                        pending.push(target);
                    }
                } else {
                    pending.extend(map.values());
                }
            }
            Value::Array(items) => pending.extend(items),
            _ => {}
        }
    }
    objects
}

// --- tests -----------------------------------------------------------------------------------

fn failures<T>(items: &[T], check: impl Fn(&T) -> Result<(), String>) -> Vec<String> {
    items.iter().filter_map(|item| check(item).err()).collect()
}

#[test]
fn model_fields_match_their_schemas() {
    let errors = failures(&registry(), check_fields);
    assert!(errors.is_empty(), "{errors:#?}");
}

#[test]
fn fixtures_set_every_property_and_decode_without_ignored_fields() {
    let errors = failures(&registry(), check_fixture);
    assert!(errors.is_empty(), "{errors:#?}");
}

#[test]
fn required_keys_cannot_be_missing() {
    let errors = failures(&registry(), check_required);
    assert!(errors.is_empty(), "{errors:#?}");
}

#[test]
fn spec_enum_values_are_known_variants() {
    let errors = failures(&enums(), check_enum);
    assert!(errors.is_empty(), "{errors:#?}");
}

#[test]
fn enum_properties_of_registered_models_have_cases() {
    let sites: BTreeSet<&str> = enums().iter().map(|case| case.site).collect();
    let mut missing = Vec::new();
    for case in registry() {
        for name in case.schemas {
            for (property, value) in &merged(&[name]).properties {
                if let Some(site) = enum_site(name, property, value)
                    && !sites.contains(site.as_str())
                {
                    missing.push(site);
                }
            }
        }
    }
    assert!(missing.is_empty(), "enum sites without a case: {missing:?}");
}

#[test]
fn inline_objects_of_registered_models_are_registered() {
    let registered: BTreeSet<&str> =
        registry().iter().flat_map(|case| case.schemas.iter().copied()).collect();
    let missing: Vec<_> = registered
        .iter()
        .flat_map(|name| inline_object_sites(name))
        .filter(|site| !registered.contains(site.as_str()))
        .collect();
    assert!(missing.is_empty(), "inline objects without a model: {missing:?}");
}

#[test]
fn reachable_object_schemas_are_registered() {
    let registered: BTreeSet<&str> = registry()
        .iter()
        .flat_map(|case| case.schemas.iter().copied())
        .chain(UNMODELLED.iter().map(|(name, _)| *name))
        .collect();
    let missing: Vec<_> = reachable_objects()
        .into_iter()
        .filter(|name| !registered.contains(name.as_str()))
        .collect();
    assert!(missing.is_empty(), "reachable schemas neither registered nor unmodelled: {missing:?}");
}

/// An inline answer schema, which no component names: its model's fields equal its properties,
/// its fixture sets each and decodes without ignored fields, and no required key may be missing.
#[test]
fn inline_answers_match_their_models() {
    // A bridge's and a direct operation's funds share one shape, read as `BridgeFunding`.
    for path in ["/v1/bridges/{id}/funding", "/v1/direct/{id}/funding"] {
        let answer = &spec()["paths"][path]["get"]["responses"]["200"];
        let object = object(&answer["content"]["application/json"]["schema"]);
        let spec_fields: BTreeSet<&str> = object.properties.keys().map(String::as_str).collect();
        let rust: BTreeSet<&str> = fields::<BridgeFunding>().into_iter().collect();
        assert_eq!(rust, spec_fields, "{path}");
        let fixture = fixture("bridge_funding");
        assert_eq!(decode::<BridgeFunding>(fixture.clone()), Ok(Vec::new()));
        assert_eq!(fixture.as_object().unwrap().len(), spec_fields.len());
        for key in &object.required {
            let mut without = fixture.clone();
            without.as_object_mut().unwrap().remove(key);
            assert!(decode::<BridgeFunding>(without).is_err(), "{key}");
        }
    }
}

/// The checks themselves catch what they claim to.
mod harness {
    use serde::Deserialize;

    use super::*;
    use crate::types::{Amount, Bps, VenueId};

    #[derive(Deserialize)]
    #[expect(dead_code, reason = "only decoded")]
    struct LooseBook {
        from: VenueId,
        to: VenueId,
        #[serde(default)]
        max_fill: Option<Amount>,
        taker_fee_bps: Bps,
        levels: Vec<Level>,
    }

    #[test]
    fn field_check_catches_a_mismatch() {
        let wrong = case::<Level>(&["Book"], fixture("book"));
        let error = check_fields(&wrong).unwrap_err();
        assert!(error.contains("max_fill") && error.contains("order_id"), "{error}");
    }

    #[test]
    fn required_check_catches_a_defaulted_field() {
        let loose = case::<LooseBook>(&["Book"], fixture("book"));
        let error = check_required(&loose).unwrap_err();
        assert!(error.contains("max_fill"), "{error}");
    }

    #[test]
    fn fixture_check_catches_ignored_and_unset_fields() {
        let extra = r#"{"from":"gmx","to":"aster","max_fill":"1","taker_fee_bps":0,"levels":[],"new_field":1}"#;
        let error = check_fixture(&case::<Book>(&["Book"], serde_json::from_str(extra).unwrap()))
            .unwrap_err();
        assert!(error.contains("new_field"), "{error}");
        let unset = r#"{"from":"gmx","to":"aster","max_fill":"1","taker_fee_bps":0}"#;
        let error = check_fixture(&case::<Book>(&["Book"], serde_json::from_str(unset).unwrap()))
            .unwrap_err();
        assert!(error.contains("levels"), "{error}");
    }

    #[test]
    fn enum_check_reads_sites_and_catches_unknown_values() {
        assert_eq!(enum_values("Error.error.step").unwrap(), ["permit", "agent", "api_key"]);
        let wrong = enum_case::<crate::types::VenueId>("Error.error.step");
        assert!(check_enum(&wrong).unwrap_err().contains("permit"));
        assert!(enum_values("Book.from").is_none());
    }

    #[test]
    fn sites_step_through_nullable_objects_arrays_and_all_of() {
        assert_eq!(
            enum_values("SetupStep.to_sign.kind").unwrap(),
            ["permit", "aster_approval", "lighter_api_key"]
        );
        assert_eq!(
            enum_values("MarginMoveSpec.destinations").unwrap(),
            ["wallet", "signer", "escrow"]
        );
        let quote = merged(&["SnapshotBook.quotes"]);
        assert!(quote.properties.contains_key("recipient") && quote.required.contains("available"));
        assert!(site("Balance.nothing").is_none());
    }

    #[test]
    fn enum_sites_are_found_through_one_of_and_all_of() {
        let property =
            |schema: &str, name: &str| object(&site(schema).unwrap()).properties[name].clone();
        let state = property("ExplorerTransfer", "state");
        assert_eq!(enum_site("ExplorerTransfer", "state", &state).as_deref(), Some("FillState"));
        let state = property("PendingTrade", "state");
        assert_eq!(enum_site("PendingTrade", "state", &state).as_deref(), Some("FillState"));
        let settlement = property("Venue", "settlement");
        assert_eq!(
            enum_site("Venue", "settlement", &settlement).as_deref(),
            Some("Venue.settlement")
        );
        let version = property("TradeProof", "version");
        assert_eq!(enum_site("TradeProof", "version", &version), None);
    }

    #[test]
    fn inline_objects_are_found() {
        assert_eq!(inline_object_sites("Balance"), ["Balance.xyz"]);
        assert_eq!(inline_object_sites("SnapshotBook"), ["SnapshotBook.quotes"]);
        assert!(inline_object_sites("Venue").is_empty());
    }
}
