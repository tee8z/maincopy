mod lexe;
#[cfg(any(test, feature = "test-utils"))]
mod substitute;

use std::sync::Arc;

pub use lexe::{LexeProvider, LexeProviderRuntime, LexeProviderRuntimeError};
#[cfg(any(test, feature = "test-utils"))]
pub use substitute::{ProviderSubstitute, SubstituteCall, SubstituteResponses};

use super::{
    CreateTipInvoiceError, CreateTipInvoiceRequest, CreateTipInvoiceResult,
    InvoiceCreationReconciliation, InvoiceCreationUnknownReason, NextPaymentUpdatesRequest,
    PaymentIdentityError, PaymentOperationError, PaymentProviderError, PaymentProviderResult,
    ProviderKind, ProviderPaymentReference, ProviderPaymentStatus, ProviderPaymentUpdatePoll,
    ReconcilePaymentRequest, TipInvoice,
};

/// Closed runtime choice for Lightning receive implementations.
///
/// Adding another production variant is an intentional exhaustive compiler
/// change. Maincopy runs one provider and does not need a runtime registry.
#[derive(Clone)]
pub enum LightningProvider {
    Lexe(Arc<LexeProvider>),

    #[cfg(any(test, feature = "test-utils"))]
    Substitute(Arc<ProviderSubstitute>),
}

impl LightningProvider {
    pub fn kind(&self) -> ProviderKind {
        match self {
            Self::Lexe(provider) => provider.kind(),
            #[cfg(any(test, feature = "test-utils"))]
            Self::Substitute(provider) => provider.kind(),
        }
    }

    pub async fn create_tip_invoice(
        &self,
        request: CreateTipInvoiceRequest,
    ) -> CreateTipInvoiceResult {
        let expected = request.clone();
        let invoice = match self {
            Self::Lexe(provider) => provider.create_tip_invoice(request).await,
            #[cfg(any(test, feature = "test-utils"))]
            Self::Substitute(provider) => provider.create_tip_invoice(request).await,
        }?;

        if !self.invoice_matches_request(&expected, &invoice) {
            return Err(CreateTipInvoiceError::OutcomeUnknown(
                InvoiceCreationUnknownReason::InvalidProviderResponse,
            ));
        }

        Ok(invoice)
    }

    pub async fn reconcile_invoice_creation(
        &self,
        request: CreateTipInvoiceRequest,
    ) -> PaymentProviderResult<InvoiceCreationReconciliation> {
        let expected = request.clone();
        let result = match self {
            Self::Lexe(provider) => provider.reconcile_invoice_creation(request).await,
            #[cfg(any(test, feature = "test-utils"))]
            Self::Substitute(provider) => provider.reconcile_invoice_creation(request).await,
        }?;

        match &result {
            InvoiceCreationReconciliation::Found(invoice)
                if !self.invoice_matches_request(&expected, invoice) =>
            {
                Err(PaymentOperationError::InvalidProviderResponse.into())
            }
            _ => Ok(result),
        }
    }

    pub async fn reconcile_payment(
        &self,
        request: ReconcilePaymentRequest,
    ) -> PaymentProviderResult<ProviderPaymentStatus> {
        self.require_matching_provider(request.payment())?;
        let expected_payment = request.payment().clone();
        let status = match self {
            Self::Lexe(provider) => provider.reconcile_payment(request).await,
            #[cfg(any(test, feature = "test-utils"))]
            Self::Substitute(provider) => provider.reconcile_payment(request).await,
        }?;

        self.require_matching_response(&expected_payment, &status)?;
        Ok(status)
    }

    pub async fn next_payment_updates(
        &self,
        request: NextPaymentUpdatesRequest,
    ) -> PaymentProviderResult<ProviderPaymentUpdatePoll> {
        let requested_cursor = request.cursor().cloned();
        if let Some(cursor) = request.cursor() {
            let expected = self.kind();
            let actual = cursor.provider();
            if actual != expected {
                return Err(PaymentProviderError::Identity(
                    PaymentIdentityError::ProviderMismatch { expected, actual },
                ));
            }
        }

        match self {
            Self::Lexe(provider) => {
                let response = provider.next_payment_updates(request).await?;
                lexe::validate_provider_update_sequence(requested_cursor.as_ref(), response)
            }
            #[cfg(any(test, feature = "test-utils"))]
            Self::Substitute(provider) => {
                // The v1 substitute explicitly emulates the Lexe variant, so
                // it uses the same opaque cursor grammar and validation. A
                // future production variant must validate in its own arm.
                let response = provider.next_payment_updates(request).await?;
                lexe::validate_provider_update_sequence(requested_cursor.as_ref(), response)
            }
        }
    }

    fn require_matching_provider(
        &self,
        payment: &ProviderPaymentReference,
    ) -> PaymentProviderResult<()> {
        let expected = self.kind();
        let actual = payment.provider();
        if actual == expected {
            Ok(())
        } else {
            Err(PaymentProviderError::Identity(
                PaymentIdentityError::ProviderMismatch { expected, actual },
            ))
        }
    }

    fn require_matching_response(
        &self,
        expected: &ProviderPaymentReference,
        status: &ProviderPaymentStatus,
    ) -> PaymentProviderResult<()> {
        if status.payment() == expected {
            Ok(())
        } else {
            Err(PaymentOperationError::InvalidProviderResponse.into())
        }
    }

    fn invoice_matches_request(
        &self,
        request: &CreateTipInvoiceRequest,
        invoice: &TipInvoice,
    ) -> bool {
        invoice.matches_request(request) && invoice.payment().provider() == self.kind()
    }
}

impl std::fmt::Debug for LightningProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LightningProvider")
            .field("kind", &self.kind())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
