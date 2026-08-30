#![cfg(feature = "test-utils")]

use std::sync::Arc;

use maincopy_server::payments::{
    Bolt11Invoice, CreateTipInvoiceError, CreateTipInvoiceRequest, IgnoredPaymentUpdateReason,
    IgnoredProviderPaymentUpdate, LightningProvider, NextPaymentUpdatesRequest, PaymentHash,
    PaymentOperationError, PaymentProviderError, ProviderKind, ProviderPaymentLocator,
    ProviderPaymentReference, ProviderPaymentUpdate, ProviderPaymentUpdateBatch,
    ProviderPaymentUpdatePoll, ProviderSubstitute, ProviderUpdateCursor, ReconcilePaymentRequest,
    SatoshiAmount, SubstituteCall, SubstituteResponses, TipIntentId, TipInvoiceDescription,
};

#[tokio::test]
async fn external_tests_can_use_the_provider_substitute() {
    let create_request = CreateTipInvoiceRequest {
        intent_id: TipIntentId::parse("2e776d7d-7d5f-4ab7-8c63-434c66a262aa").unwrap(),
        amount: SatoshiAmount::new(21).unwrap(),
        description: TipInvoiceDescription::tip(),
    };
    let reconcile_request = ReconcilePaymentRequest {
        payment: ProviderPaymentReference::lexe(
            ProviderPaymentLocator::new("opaque-lexe-index").unwrap(),
        ),
        intent_id: TipIntentId::parse("2e776d7d-7d5f-4ab7-8c63-434c66a262aa").unwrap(),
        invoice: Bolt11Invoice::parse("lnbc2500u1pvjluezsp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygspp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdq5xysxxatsyp3k7enxv4jsxqzpu9qrsgquk0rl77nj30yxdy8j9vdx85fkpmdla2087ne0xh8nhedh8w27kyke0lp53ut353s06fv3qfegext0eh0ymjpf39tuven09sam30g4vgpfna3rh").unwrap(),
        amount: SatoshiAmount::new(21).unwrap(),
        payment_hash: PaymentHash::from_bytes([1; 32]),
    };
    let update_request = NextPaymentUpdatesRequest {
        cursor: Some(update_cursor(1)),
    };
    let update_response = ProviderPaymentUpdatePoll::Updates(
        ProviderPaymentUpdateBatch::new(vec![ProviderPaymentUpdate::Ignored(
            IgnoredProviderPaymentUpdate {
                next_cursor: update_cursor(2),
                reason: IgnoredPaymentUpdateReason::MissingMarker,
            },
        )])
        .unwrap(),
    );
    let substitute = Arc::new(ProviderSubstitute::new(SubstituteResponses {
        create_tip_invoice: Err(CreateTipInvoiceError::NotCreated),
        reconcile_invoice_creation: Err(PaymentOperationError::TemporarilyUnavailable.into()),
        reconcile_payment: Err(PaymentOperationError::PaymentNotFound.into()),
        next_payment_updates: Ok(update_response.clone()),
    }));
    let provider = LightningProvider::Substitute(Arc::clone(&substitute));

    assert_eq!(provider.kind(), ProviderKind::Lexe);
    assert_eq!(
        provider
            .create_tip_invoice(create_request.clone())
            .await
            .unwrap_err(),
        CreateTipInvoiceError::NotCreated
    );
    assert_eq!(
        provider
            .reconcile_invoice_creation(create_request.clone())
            .await
            .unwrap_err(),
        PaymentProviderError::Operation(PaymentOperationError::TemporarilyUnavailable)
    );
    assert_eq!(
        provider
            .reconcile_payment(reconcile_request.clone())
            .await
            .unwrap_err(),
        PaymentProviderError::Operation(PaymentOperationError::PaymentNotFound)
    );
    assert_eq!(
        provider
            .next_payment_updates(update_request.clone())
            .await
            .unwrap(),
        update_response
    );
    assert_eq!(
        substitute.calls(),
        vec![
            SubstituteCall::CreateTipInvoice(create_request.clone()),
            SubstituteCall::ReconcileInvoiceCreation(create_request),
            SubstituteCall::ReconcilePayment(reconcile_request),
            SubstituteCall::NextPaymentUpdates(update_request),
        ]
    );
}

fn update_cursor(sequence: i64) -> ProviderUpdateCursor {
    let timestamp = 1_700_000_000_000_i64 + sequence;
    ProviderUpdateCursor::lexe(format!("u{timestamp:019}-ln_{}", "01".repeat(32))).unwrap()
}
