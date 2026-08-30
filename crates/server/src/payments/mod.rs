//! Provider-neutral Lightning receive contracts.
//!
//! [`LightningProvider`] is the only cloneable runtime handle. Its Lexe variant
//! sends work to one bounded, application-owned concurrency queue. A
//! higher-level tip service can commit database state before and after these
//! calls, but no database transaction crosses this boundary.

mod error;
mod models;
mod provider;
mod subscriber;
#[cfg(test)]
mod test_support;

pub use error::{
    CommandNotAcceptedReason, CreateTipInvoiceError, CreateTipInvoiceResult,
    InvoiceCreationUnknownReason, PaymentIdentityError, PaymentOperationError,
    PaymentProviderError, PaymentProviderResult, PaymentTransportError,
};
pub use models::{
    Bolt11Invoice, CreateTipInvoiceRequest, DEFAULT_TIP_INVOICE_DESCRIPTION,
    IgnoredPaymentUpdateReason, IgnoredProviderPaymentUpdate, InvoiceCreationReconciliation,
    LightningNetwork, MAX_BOLT11_INVOICE_BYTES, MAX_PROVIDER_PAYMENT_LOCATOR_BYTES,
    MAX_PROVIDER_UPDATE_CURSOR_BYTES, MAX_TIP_INVOICE_DESCRIPTION_BYTES,
    MIN_PAYMENT_CONCURRENCY_LIMIT, NextPaymentUpdatesRequest, ObservedTipPaymentUpdate,
    ObservedTipRecoveryUpdate, PaymentConcurrencyLimit, PaymentHash, PaymentModelError,
    PaymentQueueCapacity, PaymentResponseDeadline, PaymentStringField, ProviderKind,
    ProviderPaymentLocator, ProviderPaymentReference, ProviderPaymentState, ProviderPaymentStatus,
    ProviderPaymentUpdate, ProviderPaymentUpdateBatch, ProviderPaymentUpdatePoll,
    ProviderUpdateCursor, ReconcilePaymentRequest, SatoshiAmount, TipIntentId, TipInvoice,
    TipInvoiceDescription, TipInvoiceView, TipRecoveryReason, TipSettlement,
};
pub use provider::{LexeProvider, LightningProvider};
pub use subscriber::{
    PaymentUpdateRetryCause, PaymentUpdateRetryPolicy, PaymentUpdateSubscriber,
    PaymentUpdateSubscriberEvent,
};

#[cfg(any(test, feature = "test-utils"))]
pub use provider::{ProviderSubstitute, SubstituteCall, SubstituteResponses};
