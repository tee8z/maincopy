use std::{collections::VecDeque, sync::Mutex};

use super::super::{
    CreateTipInvoiceRequest, CreateTipInvoiceResult, InvoiceCreationReconciliation,
    NextPaymentUpdatesRequest, PaymentProviderResult, ProviderPaymentStatus,
    ProviderPaymentUpdatePoll, ReconcilePaymentRequest,
};

pub struct ProviderSubstitute {
    responses: SubstituteResponses,
    queued_payment_updates: Mutex<VecDeque<PaymentProviderResult<ProviderPaymentUpdatePoll>>>,
    calls: Mutex<Vec<SubstituteCall>>,
}

impl ProviderSubstitute {
    pub fn new(responses: SubstituteResponses) -> Self {
        Self {
            responses,
            queued_payment_updates: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Adds a one-shot scripted response for subscriber tests. Scripted
    /// responses are returned in FIFO order before the configured fallback.
    pub fn enqueue_next_payment_updates(
        &self,
        response: PaymentProviderResult<ProviderPaymentUpdatePoll>,
    ) {
        self.queued_payment_updates
            .lock()
            .unwrap()
            .push_back(response);
    }

    pub fn calls(&self) -> Vec<SubstituteCall> {
        self.calls.lock().unwrap().clone()
    }

    pub(super) async fn create_tip_invoice(
        &self,
        request: CreateTipInvoiceRequest,
    ) -> CreateTipInvoiceResult {
        self.record(SubstituteCall::CreateTipInvoice(request));
        self.responses.create_tip_invoice.clone()
    }

    pub(super) async fn reconcile_invoice_creation(
        &self,
        request: CreateTipInvoiceRequest,
    ) -> PaymentProviderResult<InvoiceCreationReconciliation> {
        self.record(SubstituteCall::ReconcileInvoiceCreation(request));
        self.responses.reconcile_invoice_creation.clone()
    }

    pub(super) async fn reconcile_payment(
        &self,
        request: ReconcilePaymentRequest,
    ) -> PaymentProviderResult<ProviderPaymentStatus> {
        self.record(SubstituteCall::ReconcilePayment(request));
        self.responses.reconcile_payment.clone()
    }

    pub(super) async fn next_payment_updates(
        &self,
        request: NextPaymentUpdatesRequest,
    ) -> PaymentProviderResult<ProviderPaymentUpdatePoll> {
        self.record(SubstituteCall::NextPaymentUpdates(request));
        self.queued_payment_updates
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| self.responses.next_payment_updates.clone())
    }

    fn record(&self, call: SubstituteCall) {
        self.calls.lock().unwrap().push(call);
    }
}

#[derive(Clone)]
pub struct SubstituteResponses {
    pub create_tip_invoice: CreateTipInvoiceResult,
    pub reconcile_invoice_creation: PaymentProviderResult<InvoiceCreationReconciliation>,
    pub reconcile_payment: PaymentProviderResult<ProviderPaymentStatus>,
    pub next_payment_updates: PaymentProviderResult<ProviderPaymentUpdatePoll>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubstituteCall {
    CreateTipInvoice(CreateTipInvoiceRequest),
    ReconcileInvoiceCreation(CreateTipInvoiceRequest),
    ReconcilePayment(ReconcilePaymentRequest),
    NextPaymentUpdates(NextPaymentUpdatesRequest),
}
