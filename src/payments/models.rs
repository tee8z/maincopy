use std::{fmt, num::NonZeroU64, num::NonZeroUsize, time::Duration};

use lightning_invoice::{
    Bolt11Invoice as ParsedBolt11Invoice, Bolt11InvoiceDescriptionRef, Currency,
};
use serde::{Deserialize, Serialize, de};
use thiserror::Error;
use time::{OffsetDateTime, UtcOffset};
use uuid::Uuid;

pub const MAX_BOLT11_INVOICE_BYTES: usize = 8_192;
pub const MAX_PROVIDER_PAYMENT_LOCATOR_BYTES: usize = 512;
pub const MAX_PROVIDER_UPDATE_CURSOR_BYTES: usize = 512;
pub const DEFAULT_TIP_INVOICE_DESCRIPTION: &str = "Tip";
pub const MAX_TIP_INVOICE_DESCRIPTION_BYTES: usize = 200;
pub const MIN_PAYMENT_CONCURRENCY_LIMIT: usize = 2;
pub(crate) const TIP_CORRELATION_MARKER_PREFIX: &str = "maincopy-tip:";
pub(crate) const MAX_TIP_CORRELATION_MARKER_BYTES: usize = TIP_CORRELATION_MARKER_PREFIX.len() + 36;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Lexe,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LightningNetwork {
    Mainnet,
    Testnet,
    Regtest,
    Signet,
    Simnet,
}

impl LightningNetwork {
    fn from_invoice(invoice: &ParsedBolt11Invoice) -> Self {
        match invoice.currency() {
            Currency::Bitcoin => Self::Mainnet,
            Currency::BitcoinTestnet => Self::Testnet,
            Currency::Regtest => Self::Regtest,
            Currency::Signet => Self::Signet,
            Currency::Simnet => Self::Simnet,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TipIntentId(Uuid);

impl TipIntentId {
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }

    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    pub fn parse(value: &str) -> Result<Self, PaymentModelError> {
        let parsed = Uuid::parse_str(value).map_err(|_| PaymentModelError::InvalidTipIntentId)?;
        if parsed.hyphenated().to_string() != value {
            return Err(PaymentModelError::InvalidTipIntentId);
        }
        Ok(Self(parsed))
    }

    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Serialize for TipIntentId {
    fn serialize<Serializer>(
        &self,
        serializer: Serializer,
    ) -> Result<Serializer::Ok, Serializer::Error>
    where
        Serializer: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for TipIntentId {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

impl fmt::Display for TipIntentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.hyphenated().fmt(formatter)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TipInvoiceDescription(String);

impl TipInvoiceDescription {
    pub fn new(value: impl Into<String>) -> Result<Self, PaymentModelError> {
        let value = value.into();
        ensure_non_empty(&value, PaymentStringField::TipInvoiceDescription)?;
        ensure_maximum_length(
            &value,
            PaymentStringField::TipInvoiceDescription,
            MAX_TIP_INVOICE_DESCRIPTION_BYTES,
        )?;
        Ok(Self(value))
    }

    pub fn tip() -> Self {
        Self(DEFAULT_TIP_INVOICE_DESCRIPTION.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl Serialize for TipInvoiceDescription {
    fn serialize<Serializer>(
        &self,
        serializer: Serializer,
    ) -> Result<Serializer::Ok, Serializer::Error>
    where
        Serializer: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for TipInvoiceDescription {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

impl fmt::Display for TipInvoiceDescription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct TipCorrelationMarker(String);

impl TipCorrelationMarker {
    fn for_intent(intent_id: TipIntentId) -> Self {
        let value = format!("{TIP_CORRELATION_MARKER_PREFIX}{intent_id}");
        debug_assert!(value.len() <= MAX_TIP_CORRELATION_MARKER_BYTES);
        Self(value)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for TipCorrelationMarker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("TipCorrelationMarker")
            .field(&"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct Bolt11Invoice(ParsedBolt11Invoice);

impl Bolt11Invoice {
    pub fn parse(value: &str) -> Result<Self, PaymentModelError> {
        ensure_maximum_length(
            value,
            PaymentStringField::Bolt11Invoice,
            MAX_BOLT11_INVOICE_BYTES,
        )?;
        value
            .parse()
            .map(Self)
            .map_err(|_| PaymentModelError::InvalidBolt11Invoice)
    }

    pub fn encoded(&self) -> String {
        self.0.to_string()
    }

    pub const fn as_inner(&self) -> &ParsedBolt11Invoice {
        &self.0
    }
}

impl fmt::Debug for Bolt11Invoice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("Bolt11Invoice")
            .field(&"[REDACTED]")
            .finish()
    }
}

impl Serialize for Bolt11Invoice {
    fn serialize<Serializer>(
        &self,
        serializer: Serializer,
    ) -> Result<Serializer::Ok, Serializer::Error>
    where
        Serializer: serde::Serializer,
    {
        serializer.serialize_str(&self.encoded())
    }
}

impl<'de> Deserialize<'de> for Bolt11Invoice {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct PaymentHash([u8; 32]);

impl PaymentHash {
    pub const fn from_bytes(value: [u8; 32]) -> Self {
        Self(value)
    }

    pub fn parse(value: &str) -> Result<Self, PaymentModelError> {
        if value.len() != 64 {
            return Err(PaymentModelError::InvalidPaymentHash);
        }

        let mut decoded = [0_u8; 32];
        let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
        debug_assert!(remainder.is_empty());
        for (index, pair) in pairs.iter().enumerate() {
            let high = decode_hex_digit(pair[0]).ok_or(PaymentModelError::InvalidPaymentHash)?;
            let low = decode_hex_digit(pair[1]).ok_or(PaymentModelError::InvalidPaymentHash)?;
            decoded[index] = (high << 4) | low;
        }
        Ok(Self(decoded))
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn encoded(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(64);
        for byte in self.0 {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        encoded
    }
}

impl fmt::Debug for PaymentHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("PaymentHash")
            .field(&"[REDACTED]")
            .finish()
    }
}

impl Serialize for PaymentHash {
    fn serialize<Serializer>(
        &self,
        serializer: Serializer,
    ) -> Result<Serializer::Ok, Serializer::Error>
    where
        Serializer: serde::Serializer,
    {
        serializer.serialize_str(&self.encoded())
    }
}

impl<'de> Deserialize<'de> for PaymentHash {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

fn decode_hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[derive(Clone, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ProviderPaymentLocator(String);

impl ProviderPaymentLocator {
    pub fn new(value: impl Into<String>) -> Result<Self, PaymentModelError> {
        let value = value.into();
        ensure_non_empty(&value, PaymentStringField::ProviderPaymentLocator)?;
        ensure_maximum_length(
            &value,
            PaymentStringField::ProviderPaymentLocator,
            MAX_PROVIDER_PAYMENT_LOCATOR_BYTES,
        )?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ProviderPaymentLocator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ProviderPaymentLocator")
            .field(&"[REDACTED]")
            .finish()
    }
}

impl<'de> Deserialize<'de> for ProviderPaymentLocator {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ProviderUpdateCursor {
    provider: ProviderKind,
    cursor: String,
}

impl ProviderUpdateCursor {
    pub fn lexe(value: impl Into<String>) -> Result<Self, PaymentModelError> {
        let cursor = value.into();
        ensure_non_empty(&cursor, PaymentStringField::ProviderUpdateCursor)?;
        ensure_maximum_length(
            &cursor,
            PaymentStringField::ProviderUpdateCursor,
            MAX_PROVIDER_UPDATE_CURSOR_BYTES,
        )?;
        Ok(Self {
            provider: ProviderKind::Lexe,
            cursor,
        })
    }

    pub const fn provider(&self) -> ProviderKind {
        self.provider
    }

    pub fn as_str(&self) -> &str {
        &self.cursor
    }
}

impl fmt::Debug for ProviderUpdateCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderUpdateCursor")
            .field("provider", &self.provider)
            .field("cursor", &"[REDACTED]")
            .finish()
    }
}

impl<'de> Deserialize<'de> for ProviderUpdateCursor {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireCursor {
            provider: ProviderKind,
            cursor: String,
        }

        let wire = WireCursor::deserialize(deserializer)?;
        ensure_non_empty(&wire.cursor, PaymentStringField::ProviderUpdateCursor)
            .map_err(de::Error::custom)?;
        ensure_maximum_length(
            &wire.cursor,
            PaymentStringField::ProviderUpdateCursor,
            MAX_PROVIDER_UPDATE_CURSOR_BYTES,
        )
        .map_err(de::Error::custom)?;
        Ok(Self {
            provider: wire.provider,
            cursor: wire.cursor,
        })
    }
}

fn ensure_non_empty(value: &str, field: PaymentStringField) -> Result<(), PaymentModelError> {
    if value.trim().is_empty() {
        Err(PaymentModelError::EmptyString { field })
    } else {
        Ok(())
    }
}

fn ensure_maximum_length(
    value: &str,
    field: PaymentStringField,
    maximum_bytes: usize,
) -> Result<(), PaymentModelError> {
    if value.len() > maximum_bytes {
        Err(PaymentModelError::StringTooLong {
            field,
            maximum_bytes,
        })
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SatoshiAmount(NonZeroU64);

impl SatoshiAmount {
    pub fn new(value: u64) -> Result<Self, PaymentModelError> {
        NonZeroU64::new(value)
            .map(Self)
            .ok_or(PaymentModelError::ZeroSatoshiAmount)
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl<'de> Deserialize<'de> for SatoshiAmount {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        Self::new(u64::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaymentResponseDeadline(Duration);

impl PaymentResponseDeadline {
    pub fn new(value: Duration) -> Result<Self, PaymentModelError> {
        if value.is_zero() {
            Err(PaymentModelError::ZeroResponseDeadline)
        } else {
            Ok(Self(value))
        }
    }

    pub const fn get(self) -> Duration {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaymentConcurrencyLimit(NonZeroUsize);

impl PaymentConcurrencyLimit {
    pub fn new(value: usize) -> Result<Self, PaymentModelError> {
        let Some(value) = NonZeroUsize::new(value) else {
            return Err(PaymentModelError::ConcurrencyLimitTooLow {
                minimum: MIN_PAYMENT_CONCURRENCY_LIMIT,
            });
        };
        if value.get() < MIN_PAYMENT_CONCURRENCY_LIMIT {
            Err(PaymentModelError::ConcurrencyLimitTooLow {
                minimum: MIN_PAYMENT_CONCURRENCY_LIMIT,
            })
        } else {
            Ok(Self(value))
        }
    }

    pub const fn get(self) -> usize {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaymentQueueCapacity(NonZeroUsize);

impl PaymentQueueCapacity {
    pub fn new(value: usize) -> Result<Self, PaymentModelError> {
        NonZeroUsize::new(value)
            .map(Self)
            .ok_or(PaymentModelError::ZeroQueueCapacity)
    }

    pub const fn get(self) -> usize {
        self.0.get()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderPaymentReference {
    provider: ProviderKind,
    locator: ProviderPaymentLocator,
}

impl ProviderPaymentReference {
    pub fn lexe(locator: ProviderPaymentLocator) -> Self {
        Self {
            provider: ProviderKind::Lexe,
            locator,
        }
    }

    pub const fn provider(&self) -> ProviderKind {
        self.provider
    }

    pub const fn locator(&self) -> &ProviderPaymentLocator {
        &self.locator
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CreateTipInvoiceRequest {
    intent_id: TipIntentId,
    #[serde(rename = "amount_sats")]
    amount: SatoshiAmount,
    description: TipInvoiceDescription,
}

impl CreateTipInvoiceRequest {
    pub fn new(
        intent_id: TipIntentId,
        amount: SatoshiAmount,
        description: TipInvoiceDescription,
    ) -> Self {
        Self {
            intent_id,
            amount,
            description,
        }
    }

    pub const fn intent_id(&self) -> TipIntentId {
        self.intent_id
    }

    pub const fn amount(&self) -> SatoshiAmount {
        self.amount
    }

    pub const fn description(&self) -> &TipInvoiceDescription {
        &self.description
    }

    pub(crate) fn correlation_marker(&self) -> TipCorrelationMarker {
        TipCorrelationMarker::for_intent(self.intent_id)
    }
}

/// Internal provider result and persistence representation.
///
/// HTTP handlers must convert this record with [`Self::view`] so the provider
/// identity and opaque locator never enter a public response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TipInvoice {
    invoice: Bolt11Invoice,
    intent_id: TipIntentId,
    network: LightningNetwork,
    #[serde(rename = "amount_sats")]
    amount: SatoshiAmount,
    payment_hash: PaymentHash,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
    payment: ProviderPaymentReference,
}

impl TipInvoice {
    pub fn try_from_invoice(
        invoice: Bolt11Invoice,
        request: CreateTipInvoiceRequest,
        expected_network: LightningNetwork,
        now: OffsetDateTime,
        payment: ProviderPaymentReference,
    ) -> Result<Self, PaymentModelError> {
        let invoice = Self::from_invoice_fields(invoice, request, payment)?;
        if invoice.network != expected_network {
            return Err(PaymentModelError::InvoiceNetworkMismatch {
                expected: expected_network,
                actual: invoice.network,
            });
        }
        if invoice.expires_at <= now.to_offset(UtcOffset::UTC) {
            return Err(PaymentModelError::InvoiceExpired);
        }
        Ok(invoice)
    }

    pub(crate) fn try_from_reconciled_invoice(
        invoice: Bolt11Invoice,
        request: CreateTipInvoiceRequest,
        expected_network: LightningNetwork,
        payment: ProviderPaymentReference,
    ) -> Result<Self, PaymentModelError> {
        let invoice = Self::from_invoice_fields(invoice, request, payment)?;
        if invoice.network != expected_network {
            return Err(PaymentModelError::InvoiceNetworkMismatch {
                expected: expected_network,
                actual: invoice.network,
            });
        }
        Ok(invoice)
    }

    fn from_invoice_fields(
        invoice: Bolt11Invoice,
        request: CreateTipInvoiceRequest,
        payment: ProviderPaymentReference,
    ) -> Result<Self, PaymentModelError> {
        let amount_msats = invoice
            .as_inner()
            .amount_milli_satoshis()
            .ok_or(PaymentModelError::InvoiceAmountMissing)?;
        if amount_msats % 1_000 != 0 {
            return Err(PaymentModelError::InvoiceAmountNotWholeSatoshis);
        }
        let amount = SatoshiAmount::new(amount_msats / 1_000)?;
        if amount != request.amount() {
            return Err(PaymentModelError::InvoiceAmountMismatch);
        }

        let actual_description = invoice_description(&invoice)?;
        if actual_description != *request.description() {
            return Err(PaymentModelError::InvoiceDescriptionMismatch);
        }

        let mut payment_hash_bytes = [0_u8; 32];
        payment_hash_bytes.copy_from_slice(invoice.as_inner().payment_hash().as_ref());
        let payment_hash = PaymentHash::from_bytes(payment_hash_bytes);
        let expires_at = invoice
            .as_inner()
            .expires_at()
            .ok_or(PaymentModelError::InvoiceExpiryOutOfRange)
            .and_then(expiry_timestamp)?;
        let network = LightningNetwork::from_invoice(invoice.as_inner());

        Ok(Self {
            invoice,
            intent_id: request.intent_id(),
            network,
            amount,
            payment_hash,
            expires_at,
            payment,
        })
    }

    pub const fn invoice(&self) -> &Bolt11Invoice {
        &self.invoice
    }

    pub const fn intent_id(&self) -> TipIntentId {
        self.intent_id
    }

    pub const fn network(&self) -> LightningNetwork {
        self.network
    }

    pub const fn amount(&self) -> SatoshiAmount {
        self.amount
    }

    pub const fn payment_hash(&self) -> &PaymentHash {
        &self.payment_hash
    }

    pub const fn expires_at(&self) -> OffsetDateTime {
        self.expires_at
    }

    pub const fn payment(&self) -> &ProviderPaymentReference {
        &self.payment
    }

    pub(crate) fn matches_request(&self, request: &CreateTipInvoiceRequest) -> bool {
        self.intent_id == request.intent_id()
            && self.amount == request.amount()
            && invoice_description(&self.invoice).as_ref() == Ok(request.description())
    }

    pub fn view(&self) -> TipInvoiceView {
        TipInvoiceView {
            invoice: self.invoice.clone(),
            amount: self.amount,
            expires_at: self.expires_at,
        }
    }
}

fn invoice_description(
    invoice: &Bolt11Invoice,
) -> Result<TipInvoiceDescription, PaymentModelError> {
    match invoice.as_inner().description() {
        Bolt11InvoiceDescriptionRef::Direct(description) => {
            TipInvoiceDescription::new(description.to_string())
        }
        Bolt11InvoiceDescriptionRef::Hash(_) => Err(PaymentModelError::InvoiceDescriptionNotDirect),
    }
}

impl<'de> Deserialize<'de> for TipInvoice {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireInvoice {
            invoice: Bolt11Invoice,
            intent_id: TipIntentId,
            network: LightningNetwork,
            #[serde(rename = "amount_sats")]
            amount: SatoshiAmount,
            payment_hash: PaymentHash,
            #[serde(with = "time::serde::rfc3339")]
            expires_at: OffsetDateTime,
            payment: ProviderPaymentReference,
        }

        let wire = WireInvoice::deserialize(deserializer)?;
        let description = invoice_description(&wire.invoice).map_err(de::Error::custom)?;
        let request = CreateTipInvoiceRequest::new(wire.intent_id, wire.amount, description);
        let invoice = Self::from_invoice_fields(wire.invoice, request, wire.payment)
            .map_err(de::Error::custom)?;
        if invoice.network != wire.network
            || invoice.payment_hash != wire.payment_hash
            || invoice.expires_at != wire.expires_at
        {
            return Err(de::Error::custom(PaymentModelError::InvoiceFieldsMismatch));
        }
        Ok(invoice)
    }
}

fn expiry_timestamp(expiry: std::time::Duration) -> Result<OffsetDateTime, PaymentModelError> {
    let nanos = i128::try_from(expiry.as_nanos())
        .map_err(|_| PaymentModelError::InvoiceExpiryOutOfRange)?;
    OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .map(|timestamp| timestamp.to_offset(UtcOffset::UTC))
        .map_err(|_| PaymentModelError::InvoiceExpiryOutOfRange)
}

/// Public representation of a tip invoice.
///
/// This view intentionally excludes provider identity and its opaque locator.
/// HTTP handlers must serialize this type rather than [`TipInvoice`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TipInvoiceView {
    invoice: Bolt11Invoice,
    #[serde(rename = "amount_sats")]
    amount: SatoshiAmount,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
}

impl TipInvoiceView {
    pub const fn invoice(&self) -> &Bolt11Invoice {
        &self.invoice
    }

    pub const fn amount(&self) -> SatoshiAmount {
        self.amount
    }

    pub const fn expires_at(&self) -> OffsetDateTime {
        self.expires_at
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReconcilePaymentRequest {
    payment: ProviderPaymentReference,
    intent_id: TipIntentId,
    invoice: Bolt11Invoice,
    #[serde(rename = "amount_sats")]
    amount: SatoshiAmount,
    payment_hash: PaymentHash,
}

impl ReconcilePaymentRequest {
    pub const fn new(
        payment: ProviderPaymentReference,
        intent_id: TipIntentId,
        invoice: Bolt11Invoice,
        amount: SatoshiAmount,
        payment_hash: PaymentHash,
    ) -> Self {
        Self {
            payment,
            intent_id,
            invoice,
            amount,
            payment_hash,
        }
    }

    pub fn for_invoice(invoice: &TipInvoice) -> Self {
        Self::new(
            invoice.payment().clone(),
            invoice.intent_id(),
            invoice.invoice().clone(),
            invoice.amount(),
            *invoice.payment_hash(),
        )
    }

    pub const fn payment(&self) -> &ProviderPaymentReference {
        &self.payment
    }

    pub const fn intent_id(&self) -> TipIntentId {
        self.intent_id
    }

    pub const fn invoice(&self) -> &Bolt11Invoice {
        &self.invoice
    }

    pub const fn amount(&self) -> SatoshiAmount {
        self.amount
    }

    pub const fn payment_hash(&self) -> &PaymentHash {
        &self.payment_hash
    }

    pub(crate) fn correlation_marker(&self) -> TipCorrelationMarker {
        TipCorrelationMarker::for_intent(self.intent_id)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
/// Requests the next catch-up page or live provider update after a durable
/// cursor. `None` means a full oldest-first bootstrap.
pub struct NextPaymentUpdatesRequest {
    cursor: Option<ProviderUpdateCursor>,
}

impl NextPaymentUpdatesRequest {
    pub const fn new(cursor: Option<ProviderUpdateCursor>) -> Self {
        Self { cursor }
    }

    pub const fn cursor(&self) -> Option<&ProviderUpdateCursor> {
        self.cursor.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// A nonempty, cursor-ordered page of provider updates.
///
/// The application must persist the ledger decision or audit disposition and
/// the corresponding update cursor in one database-writer transaction. The
/// provider never acknowledges or advances a durable cursor on its own.
pub struct ProviderPaymentUpdateBatch {
    updates: Vec<ProviderPaymentUpdate>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderPaymentUpdatePoll {
    Updates(ProviderPaymentUpdateBatch),
    Idle,
}

impl ProviderPaymentUpdateBatch {
    pub fn new(updates: Vec<ProviderPaymentUpdate>) -> Result<Self, PaymentModelError> {
        if updates.is_empty() {
            Err(PaymentModelError::EmptyProviderPaymentUpdateBatch)
        } else {
            Ok(Self { updates })
        }
    }

    pub fn updates(&self) -> &[ProviderPaymentUpdate] {
        &self.updates
    }

    pub fn into_updates(self) -> Vec<ProviderPaymentUpdate> {
        self.updates
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderPaymentUpdate {
    Tip(ObservedTipPaymentUpdate),
    TipRecoveryRequired(ObservedTipRecoveryUpdate),
    Ignored(IgnoredProviderPaymentUpdate),
}

impl ProviderPaymentUpdate {
    pub const fn next_cursor(&self) -> &ProviderUpdateCursor {
        match self {
            Self::Tip(update) => update.next_cursor(),
            Self::TipRecoveryRequired(update) => update.next_cursor(),
            Self::Ignored(update) => update.next_cursor(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IgnoredProviderPaymentUpdate {
    next_cursor: ProviderUpdateCursor,
    reason: IgnoredPaymentUpdateReason,
}

impl IgnoredProviderPaymentUpdate {
    pub const fn new(
        next_cursor: ProviderUpdateCursor,
        reason: IgnoredPaymentUpdateReason,
    ) -> Self {
        Self {
            next_cursor,
            reason,
        }
    }

    pub const fn next_cursor(&self) -> &ProviderUpdateCursor {
        &self.next_cursor
    }

    pub const fn reason(&self) -> IgnoredPaymentUpdateReason {
        self.reason
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IgnoredPaymentUpdateReason {
    MissingMarker,
    UnrecognizedMarker,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservedTipRecoveryUpdate {
    next_cursor: ProviderUpdateCursor,
    intent_id: TipIntentId,
    observed_invoice: Option<Bolt11Invoice>,
    status: ProviderPaymentStatus,
}

impl ObservedTipRecoveryUpdate {
    pub(crate) const fn new(
        next_cursor: ProviderUpdateCursor,
        intent_id: TipIntentId,
        observed_invoice: Option<Bolt11Invoice>,
        status: ProviderPaymentStatus,
    ) -> Self {
        Self {
            next_cursor,
            intent_id,
            observed_invoice,
            status,
        }
    }

    pub const fn next_cursor(&self) -> &ProviderUpdateCursor {
        &self.next_cursor
    }

    pub const fn intent_id(&self) -> TipIntentId {
        self.intent_id
    }

    /// Exact signed invoice when the conflicting provider record contained a
    /// parseable BOLT11 value. `None` preserves evidence that the invoice was
    /// absent or malformed instead of fabricating an identity.
    pub const fn observed_invoice(&self) -> Option<&Bolt11Invoice> {
        self.observed_invoice.as_ref()
    }

    pub const fn status(&self) -> &ProviderPaymentStatus {
        &self.status
    }
}

/// Provider-observed update which still requires comparison with persisted
/// intent state before it can become a ledger transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservedTipPaymentUpdate {
    next_cursor: ProviderUpdateCursor,
    intent_id: TipIntentId,
    invoice: Bolt11Invoice,
    amount: SatoshiAmount,
    payment_hash: PaymentHash,
    status: ProviderPaymentStatus,
}

impl ObservedTipPaymentUpdate {
    pub(crate) const fn new(
        next_cursor: ProviderUpdateCursor,
        intent_id: TipIntentId,
        invoice: Bolt11Invoice,
        amount: SatoshiAmount,
        payment_hash: PaymentHash,
        status: ProviderPaymentStatus,
    ) -> Self {
        Self {
            next_cursor,
            intent_id,
            invoice,
            amount,
            payment_hash,
            status,
        }
    }

    pub const fn next_cursor(&self) -> &ProviderUpdateCursor {
        &self.next_cursor
    }

    pub const fn intent_id(&self) -> TipIntentId {
        self.intent_id
    }

    /// Exact provider-observed signed invoice. The durable handler must
    /// compare this value with the invoice persisted for the intent before it
    /// acknowledges the update cursor.
    pub const fn invoice(&self) -> &Bolt11Invoice {
        &self.invoice
    }

    pub const fn amount(&self) -> SatoshiAmount {
        self.amount
    }

    pub const fn payment_hash(&self) -> &PaymentHash {
        &self.payment_hash
    }

    pub const fn status(&self) -> &ProviderPaymentStatus {
        &self.status
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InvoiceCreationReconciliation {
    Found(Box<TipInvoice>),
    Missing,
    Ambiguous,
}

impl InvoiceCreationReconciliation {
    pub const fn requires_recovery(&self) -> bool {
        matches!(self, Self::Missing | Self::Ambiguous)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TipSettlement {
    #[serde(rename = "amount_sats")]
    amount: SatoshiAmount,
    #[serde(with = "time::serde::rfc3339")]
    settled_at: OffsetDateTime,
}

impl TipSettlement {
    pub fn new(amount: SatoshiAmount, settled_at: OffsetDateTime) -> Self {
        Self {
            amount,
            settled_at: settled_at.to_offset(UtcOffset::UTC),
        }
    }

    pub const fn amount(&self) -> SatoshiAmount {
        self.amount
    }

    pub const fn settled_at(&self) -> OffsetDateTime {
        self.settled_at
    }
}

impl<'de> Deserialize<'de> for TipSettlement {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireSettlement {
            #[serde(rename = "amount_sats")]
            amount: SatoshiAmount,
            #[serde(with = "time::serde::rfc3339")]
            settled_at: OffsetDateTime,
        }

        let wire = WireSettlement::deserialize(deserializer)?;
        Ok(Self::new(wire.amount, wire.settled_at))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TipRecoveryReason {
    SettlementIncomplete,
    ProviderConflict,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", content = "details", rename_all = "snake_case")]
pub enum ProviderPaymentState {
    InvoiceOpen,
    Received(TipSettlement),
    Expired,
    RecoveryRequired(TipRecoveryReason),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderPaymentStatus {
    payment: ProviderPaymentReference,
    status: ProviderPaymentState,
}

impl ProviderPaymentStatus {
    pub const fn new(payment: ProviderPaymentReference, status: ProviderPaymentState) -> Self {
        Self { payment, status }
    }

    pub const fn payment(&self) -> &ProviderPaymentReference {
        &self.payment
    }

    pub const fn status(&self) -> &ProviderPaymentState {
        &self.status
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum PaymentModelError {
    #[error("tip intent ID must be a valid UUID")]
    InvalidTipIntentId,

    #[error("{field} must not be empty")]
    EmptyString { field: PaymentStringField },

    #[error("{field} exceeds its {maximum_bytes}-byte limit")]
    StringTooLong {
        field: PaymentStringField,
        maximum_bytes: usize,
    },

    #[error("BOLT11 invoice is invalid")]
    InvalidBolt11Invoice,

    #[error("payment hash must contain exactly 32 bytes encoded as hexadecimal")]
    InvalidPaymentHash,

    #[error("satoshi amount must be greater than zero")]
    ZeroSatoshiAmount,

    #[error("payment provider response deadline must be greater than zero")]
    ZeroResponseDeadline,

    #[error("payment provider concurrency limit must be at least {minimum}")]
    ConcurrencyLimitTooLow { minimum: usize },

    #[error("payment provider queue capacity must be greater than zero")]
    ZeroQueueCapacity,

    #[error("payment update retry delay must be greater than zero")]
    ZeroPaymentUpdateRetryDelay,

    #[error("payment update maximum retry delay must not be shorter than the initial delay")]
    PaymentUpdateMaximumRetryDelayTooShort,

    #[error("provider payment update batches must not be empty")]
    EmptyProviderPaymentUpdateBatch,

    #[error("BOLT11 invoice does not contain an amount")]
    InvoiceAmountMissing,

    #[error("BOLT11 invoice amount is not an exact number of satoshis")]
    InvoiceAmountNotWholeSatoshis,

    #[error("BOLT11 invoice amount does not match the requested tip amount")]
    InvoiceAmountMismatch,

    #[error("BOLT11 invoice must contain a direct public description")]
    InvoiceDescriptionNotDirect,

    #[error("BOLT11 invoice description does not match the requested public description")]
    InvoiceDescriptionMismatch,

    #[error("BOLT11 invoice network is {actual:?}, expected {expected:?}")]
    InvoiceNetworkMismatch {
        expected: LightningNetwork,
        actual: LightningNetwork,
    },

    #[error("BOLT11 invoice was already expired when validated")]
    InvoiceExpired,

    #[error("BOLT11 invoice expiry is outside the supported timestamp range")]
    InvoiceExpiryOutOfRange,

    #[error("persisted invoice fields do not match the signed BOLT11 invoice")]
    InvoiceFieldsMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaymentStringField {
    Bolt11Invoice,
    ProviderPaymentLocator,
    ProviderUpdateCursor,
    TipInvoiceDescription,
}

impl fmt::Display for PaymentStringField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bolt11Invoice => formatter.write_str("BOLT11 invoice"),
            Self::ProviderPaymentLocator => formatter.write_str("provider payment locator"),
            Self::ProviderUpdateCursor => formatter.write_str("provider update cursor"),
            Self::TipInvoiceDescription => formatter.write_str("tip invoice description"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bitcoin::{
        hashes::{Hash, sha256},
        secp256k1::{Secp256k1, SecretKey},
    };
    use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
    use serde_json::json;

    use super::*;

    const TEST_INTENT_ID: &str = "2e776d7d-7d5f-4ab7-8c63-434c66a262aa";
    const TEST_INVOICE: &str = "lnbc2500u1pvjluezsp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygspp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdq5xysxxatsyp3k7enxv4jsxqzpu9qrsgquk0rl77nj30yxdy8j9vdx85fkpmdla2087ne0xh8nhedh8w27kyke0lp53ut353s06fv3qfegext0eh0ymjpf39tuven09sam30g4vgpfna3rh";

    #[test]
    fn provider_identity_wire_contract_keeps_kind_separate_from_locator() {
        let reference = payment_reference();

        assert_eq!(
            serde_json::to_value(&reference).unwrap(),
            json!({
                "provider": "lexe",
                "locator": "opaque-provider-locator"
            })
        );
        assert_eq!(
            serde_json::from_value::<ProviderPaymentReference>(json!({
                "provider": "lexe",
                "locator": "opaque-provider-locator"
            }))
            .unwrap(),
            reference
        );
    }

    #[test]
    fn update_cursor_has_a_stable_redacted_wire_contract() {
        let cursor = ProviderUpdateCursor::lexe("u0000000000000000001-ln_opaque").unwrap();

        assert_eq!(
            serde_json::to_value(&cursor).unwrap(),
            json!({
                "provider": "lexe",
                "cursor": "u0000000000000000001-ln_opaque"
            })
        );
        assert_eq!(
            serde_json::from_value::<ProviderUpdateCursor>(serde_json::to_value(&cursor).unwrap())
                .unwrap(),
            cursor
        );
        assert!(!format!("{cursor:?}").contains("ln_opaque"));
    }

    #[test]
    fn request_uses_canonical_uuid_and_exact_amount() {
        let request = create_request();

        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({
                "intent_id": TEST_INTENT_ID,
                "amount_sats": 250_000,
                "description": "Tip"
            })
        );
        assert_eq!(request.description().as_str(), "Tip");
        assert_eq!(
            request.correlation_marker().as_str(),
            "maincopy-tip:2e776d7d-7d5f-4ab7-8c63-434c66a262aa"
        );
        assert!(request.correlation_marker().as_str().len() <= MAX_TIP_CORRELATION_MARKER_BYTES);
    }

    #[test]
    fn intent_id_rejects_noncanonical_uuid_encodings() {
        assert!(TipIntentId::parse("2e776d7d7d5f4ab78c63434c66a262aa").is_err());
        assert!(TipIntentId::parse("2E776D7D-7D5F-4AB7-8C63-434C66A262AA").is_err());
        assert!(
            serde_json::from_value::<TipIntentId>(json!("2E776D7D-7D5F-4AB7-8C63-434C66A262AA"))
                .is_err()
        );
    }

    #[test]
    fn invoice_fields_are_derived_from_the_validated_bolt11_value() {
        let invoice = tip_invoice();
        let wire = serde_json::to_value(&invoice).unwrap();

        assert_eq!(wire["invoice"], invoice.invoice().encoded());
        assert_eq!(wire["intent_id"], TEST_INTENT_ID);
        assert_eq!(wire["network"], "mainnet");
        assert_eq!(wire["amount_sats"], 250_000);
        assert_eq!(
            wire["payment_hash"],
            "0101010101010101010101010101010101010101010101010101010101010101"
        );
        assert!(wire["expires_at"].as_str().unwrap().ends_with('Z'));
        assert_eq!(serde_json::from_value::<TipInvoice>(wire).unwrap(), invoice);
    }

    #[test]
    fn deserialization_rejects_invoice_metadata_that_does_not_match_signature() {
        let mut wire = serde_json::to_value(tip_invoice()).unwrap();
        wire["amount_sats"] = json!(1);

        assert!(serde_json::from_value::<TipInvoice>(wire).is_err());
    }

    #[test]
    fn public_invoice_view_never_exposes_provider_identity_or_locator() {
        let wire = serde_json::to_value(tip_invoice().view()).unwrap();

        assert_eq!(wire.as_object().unwrap().len(), 3);
        assert!(wire.get("invoice").is_some());
        assert!(wire.get("provider").is_none());
        assert!(wire.get("locator").is_none());
        assert!(wire.get("payment").is_none());
        assert!(wire.get("intent_id").is_none());
        assert!(wire.get("payment_hash").is_none());
        assert!(!wire.to_string().contains("opaque-provider-locator"));
    }

    #[test]
    fn missing_or_ambiguous_creation_reconciliation_always_requires_recovery() {
        assert!(InvoiceCreationReconciliation::Missing.requires_recovery());
        assert!(InvoiceCreationReconciliation::Ambiguous.requires_recovery());
        assert!(!InvoiceCreationReconciliation::Found(Box::new(tip_invoice())).requires_recovery());
    }

    #[test]
    fn returned_invoice_must_match_the_requested_amount() {
        let request = create_request();
        let invoice = signed_direct_invoice(request.description().as_str(), 1);

        assert_eq!(
            validate_invoice(invoice, request).unwrap_err(),
            PaymentModelError::InvoiceAmountMismatch
        );
    }

    #[test]
    fn returned_invoice_must_contain_the_exact_public_description() {
        let request = create_request();
        let invoice = signed_direct_invoice("wrong description", request.amount().get());

        assert_eq!(
            validate_invoice(invoice, request).unwrap_err(),
            PaymentModelError::InvoiceDescriptionMismatch
        );
    }

    #[test]
    fn returned_invoice_rejects_a_hashed_description() {
        let request = create_request();
        let invoice = signed_hashed_invoice(request.amount().get());

        assert_eq!(
            validate_invoice(invoice, request).unwrap_err(),
            PaymentModelError::InvoiceDescriptionNotDirect
        );
    }

    #[test]
    fn returned_invoice_must_match_the_expected_network() {
        let request = create_request();
        let invoice = signed_direct_invoice_on(
            Currency::BitcoinTestnet,
            request.description().as_str(),
            request.amount().get(),
        );

        assert_eq!(
            validate_invoice(invoice, request).unwrap_err(),
            PaymentModelError::InvoiceNetworkMismatch {
                expected: LightningNetwork::Mainnet,
                actual: LightningNetwork::Testnet,
            }
        );
    }

    #[test]
    fn returned_invoice_must_be_unexpired_at_validation_time() {
        let request = create_request();
        let invoice = signed_direct_invoice(request.description().as_str(), request.amount().get());
        let after_default_expiry = OffsetDateTime::from_unix_timestamp(1_700_003_601).unwrap();

        assert_eq!(
            TipInvoice::try_from_invoice(
                invoice,
                request,
                LightningNetwork::Mainnet,
                after_default_expiry,
                payment_reference(),
            )
            .unwrap_err(),
            PaymentModelError::InvoiceExpired
        );
    }

    #[test]
    fn reconciled_invoice_can_be_historical_after_its_expiry() {
        let request = create_request();
        let invoice = signed_direct_invoice(request.description().as_str(), request.amount().get());

        assert!(
            TipInvoice::try_from_reconciled_invoice(
                invoice,
                request,
                LightningNetwork::Mainnet,
                payment_reference(),
            )
            .is_ok()
        );
    }

    #[test]
    fn provider_payment_states_have_stable_neutral_names() {
        let states = [
            (
                ProviderPaymentState::InvoiceOpen,
                json!({ "state": "invoice_open" }),
            ),
            (
                ProviderPaymentState::Received(TipSettlement::new(
                    SatoshiAmount::new(21).unwrap(),
                    OffsetDateTime::from_unix_timestamp(1_788_013_800)
                        .unwrap()
                        .to_offset(UtcOffset::from_hms(5, 30, 0).unwrap()),
                )),
                json!({
                    "state": "received",
                    "details": {
                        "amount_sats": 21,
                        "settled_at": "2026-08-29T14:30:00Z"
                    }
                }),
            ),
            (ProviderPaymentState::Expired, json!({ "state": "expired" })),
            (
                ProviderPaymentState::RecoveryRequired(TipRecoveryReason::SettlementIncomplete),
                json!({
                    "state": "recovery_required",
                    "details": "settlement_incomplete"
                }),
            ),
        ];

        for (state, expected) in states {
            assert_eq!(serde_json::to_value(state).unwrap(), expected);
        }
    }

    #[test]
    fn invalid_or_oversized_values_fail_closed() {
        assert!(TipIntentId::parse("not-a-uuid").is_err());
        assert!(Bolt11Invoice::parse("not-an-invoice").is_err());
        assert!(Bolt11Invoice::parse(&"l".repeat(MAX_BOLT11_INVOICE_BYTES + 1)).is_err());
        assert!(PaymentHash::parse("00").is_err());
        assert!(PaymentHash::parse(&"gg".repeat(32)).is_err());
        assert!(ProviderPaymentLocator::new(" ").is_err());
        assert!(
            ProviderPaymentLocator::new("x".repeat(MAX_PROVIDER_PAYMENT_LOCATOR_BYTES + 1))
                .is_err()
        );
        assert!(serde_json::from_value::<SatoshiAmount>(json!(0)).is_err());
        assert!(ProviderUpdateCursor::lexe(" ").is_err());
        assert!(
            ProviderUpdateCursor::lexe("x".repeat(MAX_PROVIDER_UPDATE_CURSOR_BYTES + 1)).is_err()
        );
        assert!(TipInvoiceDescription::new(" ").is_err());
        assert!(
            TipInvoiceDescription::new("x".repeat(MAX_TIP_INVOICE_DESCRIPTION_BYTES + 1)).is_err()
        );
        assert_eq!(
            PaymentResponseDeadline::new(Duration::ZERO),
            Err(PaymentModelError::ZeroResponseDeadline)
        );
        assert_eq!(
            PaymentConcurrencyLimit::new(0),
            Err(PaymentModelError::ConcurrencyLimitTooLow { minimum: 2 })
        );
        assert_eq!(
            PaymentConcurrencyLimit::new(1),
            Err(PaymentModelError::ConcurrencyLimitTooLow { minimum: 2 })
        );
        assert_eq!(
            PaymentQueueCapacity::new(0),
            Err(PaymentModelError::ZeroQueueCapacity)
        );
        assert_eq!(
            ProviderPaymentUpdateBatch::new(Vec::new()),
            Err(PaymentModelError::EmptyProviderPaymentUpdateBatch)
        );
    }

    #[test]
    fn sensitive_provider_values_are_redacted_from_debug_output() {
        let invoice = Bolt11Invoice::parse(TEST_INVOICE).unwrap();
        let hash = PaymentHash::parse(&"01".repeat(32)).unwrap();
        let locator = ProviderPaymentLocator::new("secret-locator").unwrap();
        let marker = create_request().correlation_marker();

        for rendered in [
            format!("{invoice:?}"),
            format!("{hash:?}"),
            format!("{locator:?}"),
            format!("{marker:?}"),
        ] {
            assert!(rendered.contains("REDACTED"));
            assert!(!rendered.contains("secret"));
            assert!(!rendered.contains("lnbc"));
        }
    }

    fn intent_id() -> TipIntentId {
        TipIntentId::parse(TEST_INTENT_ID).unwrap()
    }

    fn create_request() -> CreateTipInvoiceRequest {
        CreateTipInvoiceRequest::new(
            intent_id(),
            SatoshiAmount::new(250_000).unwrap(),
            TipInvoiceDescription::tip(),
        )
    }

    fn payment_reference() -> ProviderPaymentReference {
        ProviderPaymentReference::lexe(
            ProviderPaymentLocator::new("opaque-provider-locator").unwrap(),
        )
    }

    fn tip_invoice() -> TipInvoice {
        let request = create_request();
        validate_invoice(
            signed_direct_invoice(request.description().as_str(), request.amount().get()),
            request,
        )
        .unwrap()
    }

    fn validate_invoice(
        invoice: Bolt11Invoice,
        request: CreateTipInvoiceRequest,
    ) -> Result<TipInvoice, PaymentModelError> {
        TipInvoice::try_from_invoice(
            invoice,
            request,
            LightningNetwork::Mainnet,
            OffsetDateTime::from_unix_timestamp(1_700_000_001).unwrap(),
            payment_reference(),
        )
    }

    fn signed_direct_invoice(description: &str, amount_sats: u64) -> Bolt11Invoice {
        signed_direct_invoice_on(Currency::Bitcoin, description, amount_sats)
    }

    fn signed_direct_invoice_on(
        currency: Currency,
        description: &str,
        amount_sats: u64,
    ) -> Bolt11Invoice {
        let payment_hash = sha256::Hash::from_byte_array([1; 32]);
        let private_key = SecretKey::from_slice(&[42; 32]).unwrap();
        let secp = Secp256k1::new();
        let invoice = InvoiceBuilder::new(currency)
            .amount_milli_satoshis(amount_sats * 1_000)
            .duration_since_epoch(Duration::from_secs(1_700_000_000))
            .description(description.to_owned())
            .payment_hash(payment_hash)
            .payment_secret(PaymentSecret([42; 32]))
            .min_final_cltv_expiry_delta(18)
            .build_signed(|message| secp.sign_ecdsa_recoverable(message, &private_key))
            .unwrap();

        Bolt11Invoice::parse(&invoice.to_string()).unwrap()
    }

    fn signed_hashed_invoice(amount_sats: u64) -> Bolt11Invoice {
        let payment_hash = sha256::Hash::from_byte_array([1; 32]);
        let description_hash = sha256::Hash::from_byte_array([2; 32]);
        let private_key = SecretKey::from_slice(&[42; 32]).unwrap();
        let secp = Secp256k1::new();
        let invoice = InvoiceBuilder::new(Currency::Bitcoin)
            .amount_milli_satoshis(amount_sats * 1_000)
            .duration_since_epoch(Duration::from_secs(1_700_000_000))
            .description_hash(description_hash)
            .payment_hash(payment_hash)
            .payment_secret(PaymentSecret([42; 32]))
            .min_final_cltv_expiry_delta(18)
            .build_signed(|message| secp.sign_ecdsa_recoverable(message, &private_key))
            .unwrap();

        Bolt11Invoice::parse(&invoice.to_string()).unwrap()
    }
}
