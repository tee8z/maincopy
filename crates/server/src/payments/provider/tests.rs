use std::sync::Arc;

use time::OffsetDateTime;

use super::{ProviderSubstitute, SubstituteCall, SubstituteResponses, *};
use crate::payments::test_support::signed_direct_invoice;
use crate::payments::{
    CreateTipInvoiceError, IgnoredPaymentUpdateReason, IgnoredProviderPaymentUpdate,
    InvoiceCreationReconciliation, InvoiceCreationUnknownReason, LightningNetwork,
    NextPaymentUpdatesRequest, PaymentHash, PaymentOperationError, PaymentProviderError,
    ProviderPaymentLocator, ProviderPaymentReference, ProviderPaymentState, ProviderPaymentStatus,
    ProviderPaymentUpdate, ProviderPaymentUpdateBatch, ProviderPaymentUpdatePoll,
    ProviderUpdateCursor, SatoshiAmount, TipIntentId, TipInvoiceDescription,
};

const TEST_INTENT_ID: &str = "2e776d7d-7d5f-4ab7-8c63-434c66a262aa";
const OTHER_INTENT_ID: &str = "b55eff03-acb8-47e4-94c7-0b7efb5ae21a";

#[tokio::test]
async fn substitute_variant_delegates_every_operation_exhaustively() {
    let create = create_request(TEST_INTENT_ID);
    let invoice = tip_invoice(&create, payment_reference("created-locator"));
    let reconcile = reconcile_request("known-locator");
    let status = payment_status(reconcile.payment.clone(), ProviderPaymentState::InvoiceOpen);
    let update_request = NextPaymentUpdatesRequest {
        cursor: Some(update_cursor("cursor-before")),
    };
    let updates = update_batch("cursor-after");
    let substitute = Arc::new(ProviderSubstitute::new(SubstituteResponses {
        create_tip_invoice: Ok(invoice.clone()),
        reconcile_invoice_creation: Ok(InvoiceCreationReconciliation::Found(Box::new(
            invoice.clone(),
        ))),
        reconcile_payment: Ok(status.clone()),
        next_payment_updates: Ok(updates.clone()),
    }));
    let provider = LightningProvider::Substitute(Arc::clone(&substitute));

    assert_eq!(provider.kind(), ProviderKind::Lexe);
    assert_eq!(
        provider.create_tip_invoice(create.clone()).await.unwrap(),
        invoice
    );
    assert_eq!(
        provider
            .reconcile_invoice_creation(create.clone())
            .await
            .unwrap(),
        InvoiceCreationReconciliation::Found(Box::new(invoice))
    );
    assert_eq!(
        provider.reconcile_payment(reconcile.clone()).await.unwrap(),
        status
    );
    assert_eq!(
        provider
            .next_payment_updates(update_request.clone())
            .await
            .unwrap(),
        updates
    );
    assert_eq!(
        substitute.calls(),
        vec![
            SubstituteCall::CreateTipInvoice(create.clone()),
            SubstituteCall::ReconcileInvoiceCreation(create),
            SubstituteCall::ReconcilePayment(reconcile),
            SubstituteCall::NextPaymentUpdates(update_request),
        ]
    );
}

#[tokio::test]
async fn create_rejects_a_valid_invoice_for_another_intent() {
    let requested = create_request(TEST_INTENT_ID);
    let other_request = create_request(OTHER_INTENT_ID);
    let provider = substitute_provider(SubstituteResponses {
        create_tip_invoice: Ok(tip_invoice(
            &other_request,
            payment_reference("other-locator"),
        )),
        reconcile_invoice_creation: Ok(InvoiceCreationReconciliation::Missing),
        reconcile_payment: Ok(payment_status(
            payment_reference("known-locator"),
            ProviderPaymentState::InvoiceOpen,
        )),
        next_payment_updates: Ok(update_batch("cursor-after")),
    });

    assert_eq!(
        provider.create_tip_invoice(requested).await.unwrap_err(),
        CreateTipInvoiceError::OutcomeUnknown(
            InvoiceCreationUnknownReason::InvalidProviderResponse
        )
    );
}

#[tokio::test]
async fn create_rejects_an_invoice_with_another_public_description() {
    let requested = create_request(TEST_INTENT_ID);
    let other_request = CreateTipInvoiceRequest {
        intent_id: TipIntentId::parse(TEST_INTENT_ID).unwrap(),
        amount: SatoshiAmount::new(21).unwrap(),
        description: TipInvoiceDescription::new("Another purpose").unwrap(),
    };
    let provider = substitute_provider(SubstituteResponses {
        create_tip_invoice: Ok(tip_invoice(
            &other_request,
            payment_reference("other-locator"),
        )),
        reconcile_invoice_creation: Ok(InvoiceCreationReconciliation::Missing),
        reconcile_payment: Ok(payment_status(
            payment_reference("known-locator"),
            ProviderPaymentState::InvoiceOpen,
        )),
        next_payment_updates: Ok(update_batch("cursor-after")),
    });

    assert_eq!(
        provider.create_tip_invoice(requested).await.unwrap_err(),
        CreateTipInvoiceError::OutcomeUnknown(
            InvoiceCreationUnknownReason::InvalidProviderResponse
        )
    );
}

#[tokio::test]
async fn creation_reconciliation_rejects_an_invoice_for_another_intent() {
    let requested = create_request(TEST_INTENT_ID);
    let other_request = create_request(OTHER_INTENT_ID);
    let provider = substitute_provider(SubstituteResponses {
        create_tip_invoice: Ok(tip_invoice(
            &requested,
            payment_reference("created-locator"),
        )),
        reconcile_invoice_creation: Ok(InvoiceCreationReconciliation::Found(Box::new(
            tip_invoice(&other_request, payment_reference("other-locator")),
        ))),
        reconcile_payment: Ok(payment_status(
            payment_reference("known-locator"),
            ProviderPaymentState::InvoiceOpen,
        )),
        next_payment_updates: Ok(update_batch("cursor-after")),
    });

    assert_eq!(
        provider
            .reconcile_invoice_creation(requested)
            .await
            .unwrap_err(),
        PaymentProviderError::Operation(PaymentOperationError::InvalidProviderResponse)
    );
}

#[tokio::test]
async fn reconcile_rejects_a_status_for_another_payment() {
    let expected = payment_reference("expected-locator");
    let provider = substitute_provider(SubstituteResponses {
        create_tip_invoice: Ok(tip_invoice(
            &create_request(TEST_INTENT_ID),
            payment_reference("created-locator"),
        )),
        reconcile_invoice_creation: Ok(InvoiceCreationReconciliation::Missing),
        reconcile_payment: Ok(payment_status(
            payment_reference("unexpected-locator"),
            ProviderPaymentState::InvoiceOpen,
        )),
        next_payment_updates: Ok(update_batch("cursor-after")),
    });

    assert_eq!(
        provider
            .reconcile_payment(reconcile_request_for(expected))
            .await
            .unwrap_err(),
        PaymentProviderError::Operation(PaymentOperationError::InvalidProviderResponse)
    );
}

#[tokio::test]
async fn provider_boundary_rejects_a_regressing_substitute_update_page() {
    let regressing = ProviderPaymentUpdatePoll::Updates(
        ProviderPaymentUpdateBatch::new(vec![
            ignored_update("cursor-after"),
            ignored_update("cursor-before"),
        ])
        .unwrap(),
    );
    let provider = substitute_provider(SubstituteResponses {
        create_tip_invoice: Ok(tip_invoice(
            &create_request(TEST_INTENT_ID),
            payment_reference("created-locator"),
        )),
        reconcile_invoice_creation: Ok(InvoiceCreationReconciliation::Missing),
        reconcile_payment: Ok(payment_status(
            payment_reference("known-locator"),
            ProviderPaymentState::InvoiceOpen,
        )),
        next_payment_updates: Ok(regressing),
    });

    assert_eq!(
        provider
            .next_payment_updates(NextPaymentUpdatesRequest { cursor: None })
            .await
            .unwrap_err(),
        PaymentProviderError::Operation(PaymentOperationError::InvalidProviderResponse)
    );
}

fn substitute_provider(responses: SubstituteResponses) -> LightningProvider {
    LightningProvider::Substitute(Arc::new(ProviderSubstitute::new(responses)))
}

fn create_request(intent_id: &str) -> CreateTipInvoiceRequest {
    CreateTipInvoiceRequest {
        intent_id: TipIntentId::parse(intent_id).unwrap(),
        amount: SatoshiAmount::new(21).unwrap(),
        description: TipInvoiceDescription::tip(),
    }
}

fn payment_reference(locator: &str) -> ProviderPaymentReference {
    ProviderPaymentReference::lexe(ProviderPaymentLocator::new(locator).unwrap())
}

fn reconcile_request(locator: &str) -> ReconcilePaymentRequest {
    reconcile_request_for(payment_reference(locator))
}

fn reconcile_request_for(payment: ProviderPaymentReference) -> ReconcilePaymentRequest {
    ReconcilePaymentRequest {
        payment,
        intent_id: TipIntentId::parse(TEST_INTENT_ID).unwrap(),
        invoice: signed_direct_invoice("Tip", 21),
        amount: SatoshiAmount::new(21).unwrap(),
        payment_hash: PaymentHash::from_bytes([1; 32]),
    }
}

fn payment_status(
    payment: ProviderPaymentReference,
    status: ProviderPaymentState,
) -> ProviderPaymentStatus {
    ProviderPaymentStatus { payment, status }
}

fn update_cursor(value: &str) -> ProviderUpdateCursor {
    let timestamp = match value {
        "cursor-before" => 1_700_000_000_001_i64,
        "cursor-after" => 1_700_000_000_002_i64,
        other => panic!("unknown test cursor label: {other}"),
    };
    ProviderUpdateCursor::lexe(format!("u{timestamp:019}-ln_{}", "01".repeat(32))).unwrap()
}

fn update_batch(cursor: &str) -> ProviderPaymentUpdatePoll {
    ProviderPaymentUpdatePoll::Updates(
        ProviderPaymentUpdateBatch::new(vec![ignored_update(cursor)]).unwrap(),
    )
}

fn ignored_update(cursor: &str) -> ProviderPaymentUpdate {
    ProviderPaymentUpdate::Ignored(IgnoredProviderPaymentUpdate {
        next_cursor: update_cursor(cursor),
        reason: IgnoredPaymentUpdateReason::MissingMarker,
    })
}

fn tip_invoice(request: &CreateTipInvoiceRequest, payment: ProviderPaymentReference) -> TipInvoice {
    TipInvoice::try_from_invoice(
        signed_direct_invoice(request.description.as_str(), request.amount.get()),
        request.clone(),
        LightningNetwork::Mainnet,
        OffsetDateTime::from_unix_timestamp(1_700_000_001).unwrap(),
        payment,
    )
    .unwrap()
}
