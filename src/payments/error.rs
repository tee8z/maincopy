use thiserror::Error;

use super::{ProviderKind, TipInvoice};

pub type PaymentProviderResult<Value> = Result<Value, PaymentProviderError>;
pub type CreateTipInvoiceResult = Result<TipInvoice, CreateTipInvoiceError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum CreateTipInvoiceError {
    #[error("the invoice creation request was not accepted: {0}")]
    NotAccepted(CommandNotAcceptedReason),

    #[error(
        "the provider conclusively did not create an invoice: the provider rejected invoice creation before creating an invoice"
    )]
    NotCreated,

    #[error("invoice creation may have completed; marker reconciliation is required: {0}")]
    OutcomeUnknown(InvoiceCreationUnknownReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum InvoiceCreationUnknownReason {
    #[error("the provider did not conclusively report the creation outcome")]
    ProviderDidNotConfirm,

    #[error("the invoice creation response deadline elapsed")]
    ResponseTimedOut,

    #[error("the provider returned an invalid result after accepting creation")]
    InvalidProviderResponse,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum PaymentProviderError {
    #[error(transparent)]
    Transport(#[from] PaymentTransportError),

    #[error(transparent)]
    Identity(#[from] PaymentIdentityError),

    #[error(transparent)]
    Operation(#[from] PaymentOperationError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum PaymentTransportError {
    #[error("the payment provider request was not accepted: {0}")]
    NotAccepted(CommandNotAcceptedReason),

    #[error("the payment provider response deadline elapsed")]
    ResponseTimedOut,

    #[error("the payment provider stopped before returning a response")]
    ResponseDropped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum CommandNotAcceptedReason {
    #[error("the payment provider queue is full; retry with backoff")]
    QueueFull,

    #[error("the payment provider is unavailable")]
    ProviderUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum PaymentIdentityError {
    #[error("payment belongs to {actual:?}, but the active provider is {expected:?}")]
    ProviderMismatch {
        expected: ProviderKind,
        actual: ProviderKind,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum PaymentOperationError {
    #[error("the payment provider is temporarily unavailable")]
    TemporarilyUnavailable,

    #[error("the provider payment was not found")]
    PaymentNotFound,

    #[error("the payment provider returned an invalid response")]
    InvalidProviderResponse,

    #[error("the provider payment conflicts with current wallet state")]
    ProviderConflict,
}
