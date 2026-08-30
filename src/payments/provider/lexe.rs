use std::{collections::BTreeMap, future::Future, sync::Arc};

use lexe::{
    config::Network as LexeNetwork,
    types::{
        bitcoin::Amount as LexeAmount,
        command::{
            CreateInvoiceRequest as LexeCreateInvoiceRequest,
            CreateInvoiceResponse as LexeCreateInvoiceResponse, GetPaymentRequest,
            GetUpdatedPaymentsRequest, GetUpdatedPaymentsResponse, WaitForNextPaymentRequest,
        },
        payment::{
            Payment as LexePayment, PaymentCreatedIndex, PaymentDirection,
            PaymentHash as LexePaymentHash, PaymentKind, PaymentStatus, PaymentUpdatedIndex,
        },
    },
    wallet::LexeWallet,
};
use time::{OffsetDateTime, UtcOffset};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinSet,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

use crate::clock::{Clock, SystemClock};

use super::super::{
    Bolt11Invoice, CommandNotAcceptedReason, CreateTipInvoiceError, CreateTipInvoiceRequest,
    CreateTipInvoiceResult, IgnoredPaymentUpdateReason, IgnoredProviderPaymentUpdate,
    InvoiceCreationReconciliation, InvoiceCreationUnknownReason, LightningNetwork,
    NextPaymentUpdatesRequest, ObservedTipPaymentUpdate, ObservedTipRecoveryUpdate,
    PaymentConcurrencyLimit, PaymentOperationError, PaymentProviderError, PaymentProviderResult,
    PaymentQueueCapacity, PaymentResponseDeadline, PaymentTransportError, ProviderKind,
    ProviderPaymentLocator, ProviderPaymentReference, ProviderPaymentState, ProviderPaymentStatus,
    ProviderPaymentUpdate, ProviderPaymentUpdateBatch, ProviderPaymentUpdatePoll,
    ProviderUpdateCursor, ReconcilePaymentRequest, TipIntentId, TipInvoice, TipRecoveryReason,
    TipSettlement,
};

const RECONCILIATION_BATCH_SIZE: usize = 100;

/// Cloneable ingress handle for the Lexe provider runtime.
///
/// The handle owns no wallet state or background task. Calls are admitted with
/// a nonblocking send to one bounded queue. `LexeProviderRuntime` owns the
/// receiver, wallet, concurrency boundary, and shutdown lifecycle.
///
/// Production construction is private while Lexe SDK credentials do not
/// zeroize on drop.
///
/// ```compile_fail
/// use maincopy::payments::LexeProvider;
///
/// let _ = LexeProvider::bounded;
/// ```
#[derive(Clone)]
pub struct LexeProvider {
    commands: mpsc::Sender<LexeCommand>,
    network: LightningNetwork,
}

impl LexeProvider {
    #[expect(
        dead_code,
        reason = "production construction is blocked until Lexe credentials zeroize on drop"
    )]
    fn bounded(
        wallet: Arc<LexeWallet>,
        response_deadline: PaymentResponseDeadline,
        concurrency_limit: PaymentConcurrencyLimit,
        queue_capacity: PaymentQueueCapacity,
    ) -> (Self, LexeProviderRuntime) {
        Self::with_clock(
            wallet,
            response_deadline,
            concurrency_limit,
            queue_capacity,
            Arc::new(SystemClock),
        )
    }

    fn with_clock(
        wallet: Arc<LexeWallet>,
        response_deadline: PaymentResponseDeadline,
        concurrency_limit: PaymentConcurrencyLimit,
        queue_capacity: PaymentQueueCapacity,
        clock: Arc<dyn Clock>,
    ) -> (Self, LexeProviderRuntime) {
        let network = lightning_network(wallet.user_config().env_config.wallet_env.network);
        let (commands, receiver) = mpsc::channel(queue_capacity.get());
        (
            Self { commands, network },
            LexeProviderRuntime {
                receiver,
                wallet,
                network,
                response_deadline,
                concurrency_limit,
                clock,
            },
        )
    }

    pub(super) const fn kind(&self) -> ProviderKind {
        ProviderKind::Lexe
    }

    pub(super) async fn create_tip_invoice(
        &self,
        request: CreateTipInvoiceRequest,
    ) -> CreateTipInvoiceResult {
        let sdk_request = create_invoice_request(&request)?;
        let (respond_to, response) = oneshot::channel();
        self.submit(LexeCommand::CreateTipInvoice {
            request,
            sdk_request,
            respond_to,
        })
        .map_err(create_admission_error)?;

        response
            .await
            .unwrap_or(Err(CreateTipInvoiceError::OutcomeUnknown(
                InvoiceCreationUnknownReason::ProviderDidNotConfirm,
            )))
    }

    pub(super) async fn reconcile_invoice_creation(
        &self,
        request: CreateTipInvoiceRequest,
    ) -> PaymentProviderResult<InvoiceCreationReconciliation> {
        let (respond_to, response) = oneshot::channel();
        self.submit(LexeCommand::ReconcileInvoiceCreation {
            request,
            respond_to,
        })
        .map_err(operation_admission_error)?;

        response
            .await
            .unwrap_or(Err(PaymentTransportError::ResponseDropped.into()))
    }

    pub(super) async fn reconcile_payment(
        &self,
        request: ReconcilePaymentRequest,
    ) -> PaymentProviderResult<ProviderPaymentStatus> {
        let index = request
            .payment
            .locator()
            .as_str()
            .parse::<PaymentCreatedIndex>()
            .map_err(|_| PaymentOperationError::InvalidProviderResponse)?;
        let (respond_to, response) = oneshot::channel();
        self.submit(LexeCommand::ReconcilePayment {
            request,
            index,
            respond_to,
        })
        .map_err(operation_admission_error)?;

        response
            .await
            .unwrap_or(Err(PaymentTransportError::ResponseDropped.into()))
    }

    pub(super) async fn next_payment_updates(
        &self,
        request: NextPaymentUpdatesRequest,
    ) -> PaymentProviderResult<ProviderPaymentUpdatePoll> {
        // Application owns exactly one subscriber loop. The minimum provider
        // concurrency of two reserves capacity for ordinary operations while
        // this finite long poll occupies one JoinSet slot.
        let start_index = request
            .cursor
            .as_ref()
            .map(|cursor| cursor.as_str().parse::<PaymentUpdatedIndex>())
            .transpose()
            .map_err(|_| PaymentOperationError::InvalidProviderResponse)?;
        let (respond_to, response) = oneshot::channel();
        self.submit(LexeCommand::NextPaymentUpdates {
            start_index,
            respond_to,
        })
        .map_err(operation_admission_error)?;

        response
            .await
            .unwrap_or(Err(PaymentTransportError::ResponseDropped.into()))
    }

    fn submit(&self, command: LexeCommand) -> Result<(), QueueAdmissionError> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => QueueAdmissionError::Full,
                mpsc::error::TrySendError::Closed(_) => QueueAdmissionError::Closed,
            })
    }
}

impl std::fmt::Debug for LexeProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LexeProvider")
            .field("kind", &ProviderKind::Lexe)
            .field("network", &self.network)
            .field("queue_closed", &self.commands.is_closed())
            .finish_non_exhaustive()
    }
}

/// Application-owned execution half of a [`LexeProvider`].
///
/// `run_until_stop` must be running before provider calls can complete. A
/// cancellation closes ingress and gracefully drains pending and in-flight
/// work. Because the receiver closes, outstanding provider clones cannot
/// admit more commands after shutdown begins.
struct LexeProviderRuntime {
    receiver: mpsc::Receiver<LexeCommand>,
    wallet: Arc<LexeWallet>,
    network: LightningNetwork,
    response_deadline: PaymentResponseDeadline,
    concurrency_limit: PaymentConcurrencyLimit,
    clock: Arc<dyn Clock>,
}

impl LexeProviderRuntime {
    async fn run_until_stop(
        self,
        cancellation: CancellationToken,
    ) -> Result<(), LexeProviderRuntimeError> {
        let Self {
            receiver,
            wallet,
            network,
            response_deadline,
            concurrency_limit,
            clock,
        } = self;

        run_bounded_queue(
            receiver,
            concurrency_limit.get(),
            cancellation,
            move |command| {
                let wallet = Arc::clone(&wallet);
                let clock = Arc::clone(&clock);
                async move {
                    execute_command(wallet, network, response_deadline, clock, command).await;
                }
            },
        )
        .await
    }
}

impl std::fmt::Debug for LexeProviderRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LexeProviderRuntime")
            .field("kind", &ProviderKind::Lexe)
            .field("network", &self.network)
            .field("response_deadline", &self.response_deadline)
            .field("concurrency_limit", &self.concurrency_limit)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
enum LexeProviderRuntimeError {
    #[error("a Lexe provider operation task panicked")]
    OperationPanicked,
}

enum LexeCommand {
    CreateTipInvoice {
        request: CreateTipInvoiceRequest,
        sdk_request: LexeCreateInvoiceRequest,
        respond_to: oneshot::Sender<CreateTipInvoiceResult>,
    },
    ReconcileInvoiceCreation {
        request: CreateTipInvoiceRequest,
        respond_to: oneshot::Sender<PaymentProviderResult<InvoiceCreationReconciliation>>,
    },
    ReconcilePayment {
        request: ReconcilePaymentRequest,
        index: PaymentCreatedIndex,
        respond_to: oneshot::Sender<PaymentProviderResult<ProviderPaymentStatus>>,
    },
    NextPaymentUpdates {
        start_index: Option<PaymentUpdatedIndex>,
        respond_to: oneshot::Sender<PaymentProviderResult<ProviderPaymentUpdatePoll>>,
    },
}

async fn execute_command(
    wallet: Arc<LexeWallet>,
    network: LightningNetwork,
    response_deadline: PaymentResponseDeadline,
    clock: Arc<dyn Clock>,
    command: LexeCommand,
) {
    match command {
        LexeCommand::CreateTipInvoice {
            request,
            sdk_request,
            respond_to,
        } => {
            let result = execute_create(
                &wallet,
                network,
                response_deadline,
                clock,
                request,
                sdk_request,
            )
            .await;
            let _ = respond_to.send(result);
        }
        LexeCommand::ReconcileInvoiceCreation {
            request,
            respond_to,
        } => {
            let result =
                execute_creation_reconciliation(&wallet, network, response_deadline, request).await;
            let _ = respond_to.send(result);
        }
        LexeCommand::ReconcilePayment {
            request,
            index,
            respond_to,
        } => {
            let result = execute_payment_reconciliation(
                &wallet,
                network,
                response_deadline,
                clock,
                request,
                index,
            )
            .await;
            let _ = respond_to.send(result);
        }
        LexeCommand::NextPaymentUpdates {
            start_index,
            respond_to,
        } => {
            let result = execute_next_payment_updates(
                &wallet,
                network,
                response_deadline,
                clock,
                start_index,
            )
            .await;
            let _ = respond_to.send(result);
        }
    }
}

async fn execute_create(
    wallet: &LexeWallet,
    network: LightningNetwork,
    response_deadline: PaymentResponseDeadline,
    clock: Arc<dyn Clock>,
    request: CreateTipInvoiceRequest,
    sdk_request: LexeCreateInvoiceRequest,
) -> CreateTipInvoiceResult {
    let (created, confirmed) = await_creation(
        response_deadline,
        request_invoice_confirmation(wallet, sdk_request),
    )
    .await?;
    confirmed_creation(request, network, clock.now(), created, confirmed).map_err(|_| {
        CreateTipInvoiceError::OutcomeUnknown(InvoiceCreationUnknownReason::InvalidProviderResponse)
    })
}

async fn request_invoice_confirmation(
    wallet: &LexeWallet,
    request: LexeCreateInvoiceRequest,
) -> Result<(LexeCreateInvoiceResponse, Option<LexePayment>), ()> {
    let created = wallet.create_invoice(request).await.map_err(|_| ())?;
    let confirmed = wallet
        .get_payment(GetPaymentRequest {
            index: created.index,
        })
        .await
        .map_err(|_| ())?;
    Ok((created, confirmed.payment))
}

fn confirmed_creation(
    request: CreateTipInvoiceRequest,
    network: LightningNetwork,
    now: OffsetDateTime,
    created: LexeCreateInvoiceResponse,
    confirmed: Option<LexePayment>,
) -> Result<TipInvoice, PaymentOperationError> {
    let (created_index, created_invoice) = created_invoice_parts(created)?;
    let record = confirmed.ok_or(PaymentOperationError::InvalidProviderResponse)?;
    let record = LexeCreationRecord::from_payment(record);
    record.validate_fresh_confirmation(&request, created_index, &created_invoice)?;

    tip_invoice_from_parts(
        request,
        network,
        created_index,
        created_invoice,
        InvoiceValidation::Fresh(now),
    )
}

fn created_invoice_parts(
    created: LexeCreateInvoiceResponse,
) -> Result<(PaymentCreatedIndex, String), PaymentOperationError> {
    if created.index.id != created.invoice.payment_id() {
        return Err(PaymentOperationError::InvalidProviderResponse);
    }
    Ok((created.index, created.invoice.to_string()))
}

fn validate_creation_evidence(
    request: &CreateTipInvoiceRequest,
    created_index: PaymentCreatedIndex,
    confirmed_hash: Option<LexePaymentHash>,
    confirmed_amount: Option<LexeAmount>,
) -> Result<(), PaymentOperationError> {
    let indexed_hash = LexePaymentHash::try_from(created_index.id)
        .map_err(|_| PaymentOperationError::InvalidProviderResponse)?;
    let expected_msats = request
        .amount
        .get()
        .checked_mul(1_000)
        .ok_or(PaymentOperationError::InvalidProviderResponse)?;
    if confirmed_hash != Some(indexed_hash)
        || confirmed_amount.is_some_and(|amount| amount.msat() != expected_msats)
    {
        Err(PaymentOperationError::InvalidProviderResponse)
    } else {
        Ok(())
    }
}

async fn execute_creation_reconciliation(
    wallet: &LexeWallet,
    network: LightningNetwork,
    response_deadline: PaymentResponseDeadline,
    request: CreateTipInvoiceRequest,
) -> PaymentProviderResult<InvoiceCreationReconciliation> {
    let marker = request.correlation_marker();
    let selection = match timeout(
        response_deadline.get(),
        find_creation(wallet, marker.as_str()),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => return Err(PaymentTransportError::ResponseTimedOut.into()),
    };

    reconcile_creation_selection(request, network, selection)
}

fn reconcile_creation_selection(
    request: CreateTipInvoiceRequest,
    network: LightningNetwork,
    selection: MarkerSelection,
) -> PaymentProviderResult<InvoiceCreationReconciliation> {
    match selection {
        MarkerSelection::Missing => Ok(InvoiceCreationReconciliation::Missing),
        MarkerSelection::Ambiguous => Ok(InvoiceCreationReconciliation::Ambiguous),
        MarkerSelection::Found(record) => {
            let (index, encoded_invoice) = record.into_reconciled_invoice(&request)?;
            tip_invoice_from_parts(
                request,
                network,
                index,
                encoded_invoice,
                InvoiceValidation::Historical,
            )
            .map(|invoice| InvoiceCreationReconciliation::Found(Box::new(invoice)))
            .map_err(Into::into)
        }
    }
}

async fn find_creation(
    wallet: &LexeWallet,
    marker: &str,
) -> PaymentProviderResult<MarkerSelection> {
    let mut cursor = None;
    let mut matches = BTreeMap::new();

    while let ValidatedPaymentPage::Updates {
        next_cursor,
        payments,
    } = await_creation_page(wallet, cursor).await?
    {
        track_creation_payments(&mut matches, marker, payments);
        cursor = Some(next_cursor);
    }

    Ok(select_marker_matches(matches.into_values()))
}

async fn await_creation_page(
    wallet: &LexeWallet,
    cursor: Option<PaymentUpdatedIndex>,
) -> PaymentProviderResult<ValidatedPaymentPage> {
    let response = wallet
        .get_updated_payments(GetUpdatedPaymentsRequest {
            start_index: cursor,
            limit: Some(RECONCILIATION_BATCH_SIZE),
        })
        .await
        .map_err(|_| PaymentOperationError::TemporarilyUnavailable)?;
    validate_payment_page(response, cursor)
}

fn track_creation_payments(
    matches: &mut BTreeMap<PaymentCreatedIndex, LexeCreationRecord>,
    marker: &str,
    payments: Vec<(PaymentUpdatedIndex, LexePayment)>,
) {
    for (_, payment) in payments {
        track_marker_record(matches, marker, LexeCreationRecord::from_payment(payment));
    }
}

async fn execute_payment_reconciliation(
    wallet: &LexeWallet,
    network: LightningNetwork,
    response_deadline: PaymentResponseDeadline,
    clock: Arc<dyn Clock>,
    request: ReconcilePaymentRequest,
    index: PaymentCreatedIndex,
) -> PaymentProviderResult<ProviderPaymentStatus> {
    let response = match timeout(
        response_deadline.get(),
        wallet.get_payment(GetPaymentRequest { index }),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(_)) => return Err(PaymentOperationError::TemporarilyUnavailable.into()),
        Err(_) => return Err(PaymentTransportError::ResponseTimedOut.into()),
    };
    let payment = response
        .payment
        .ok_or(PaymentOperationError::PaymentNotFound)?;

    payment_status(request, index, payment, None, network, clock.now())
}

async fn execute_next_payment_updates(
    wallet: &LexeWallet,
    network: LightningNetwork,
    response_deadline: PaymentResponseDeadline,
    clock: Arc<dyn Clock>,
    start_index: Option<PaymentUpdatedIndex>,
) -> PaymentProviderResult<ProviderPaymentUpdatePoll> {
    let catch_up = await_catch_up_page(wallet, response_deadline, start_index).await?;
    let source = payment_update_source(catch_up, start_index);
    let outcome = resolve_payment_update_source(wallet, response_deadline, source).await?;
    provider_update_poll(outcome, network, clock.as_ref())
}

async fn await_catch_up_page(
    wallet: &LexeWallet,
    response_deadline: PaymentResponseDeadline,
    start_index: Option<PaymentUpdatedIndex>,
) -> PaymentProviderResult<ValidatedPaymentPage> {
    let response = match timeout(
        response_deadline.get(),
        wallet.get_updated_payments(GetUpdatedPaymentsRequest {
            start_index,
            limit: Some(RECONCILIATION_BATCH_SIZE),
        }),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(_)) => return Err(PaymentOperationError::TemporarilyUnavailable.into()),
        Err(_) => return Err(PaymentTransportError::ResponseTimedOut.into()),
    };
    validate_payment_page(response, start_index)
}

fn payment_update_source(
    page: ValidatedPaymentPage,
    start_index: Option<PaymentUpdatedIndex>,
) -> PaymentUpdateSource {
    match page {
        ValidatedPaymentPage::Empty => {
            PaymentUpdateSource::Live(start_index.unwrap_or(PaymentUpdatedIndex::MIN))
        }
        ValidatedPaymentPage::Updates { payments, .. } => PaymentUpdateSource::Ready(payments),
    }
}

async fn resolve_payment_update_source(
    wallet: &LexeWallet,
    response_deadline: PaymentResponseDeadline,
    source: PaymentUpdateSource,
) -> PaymentProviderResult<Option<Vec<(PaymentUpdatedIndex, LexePayment)>>> {
    match source {
        PaymentUpdateSource::Ready(payments) => Ok(Some(payments)),
        PaymentUpdateSource::Live(effective_start) => {
            await_live_payment_update(wallet, response_deadline, effective_start)
                .await
                .map(|update| update.map(|update| vec![update]))
        }
    }
}

async fn await_live_payment_update(
    wallet: &LexeWallet,
    response_deadline: PaymentResponseDeadline,
    effective_start: PaymentUpdatedIndex,
) -> PaymentProviderResult<Option<(PaymentUpdatedIndex, LexePayment)>> {
    let live = match timeout(
        response_deadline.get(),
        wallet.wait_for_next_payment(WaitForNextPaymentRequest {
            // `None` has cache-dependent tail semantics in Lexe. The minimum
            // cursor preserves a full bootstrap without a race between the
            // catch-up query and the long poll.
            start_index: Some(effective_start),
            timeout: None,
        }),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(_)) => return Err(PaymentOperationError::TemporarilyUnavailable.into()),
        Err(_) => return Ok(None),
    };
    validate_live_payment_update(effective_start, live.next_start_index, live.payment).map(Some)
}

fn provider_update_poll(
    updated_payments: Option<Vec<(PaymentUpdatedIndex, LexePayment)>>,
    network: LightningNetwork,
    clock: &dyn Clock,
) -> PaymentProviderResult<ProviderPaymentUpdatePoll> {
    let Some(updated_payments) = updated_payments else {
        return Ok(ProviderPaymentUpdatePoll::Idle);
    };
    let now = clock.now();
    let updates = updated_payments
        .into_iter()
        .map(|(cursor, payment)| provider_payment_update(cursor, payment, network, now))
        .collect();
    ProviderPaymentUpdateBatch::new(updates)
        .map(ProviderPaymentUpdatePoll::Updates)
        .map_err(|_| PaymentOperationError::InvalidProviderResponse.into())
}

fn validate_payment_page(
    response: GetUpdatedPaymentsResponse,
    requested_start: Option<PaymentUpdatedIndex>,
) -> PaymentProviderResult<ValidatedPaymentPage> {
    match (response.payments.is_empty(), response.updated_index) {
        (true, None) => Ok(ValidatedPaymentPage::Empty),
        (false, Some(next_cursor)) => {
            let payments =
                validate_catch_up_payment_updates(response.payments, requested_start, next_cursor)?;
            Ok(ValidatedPaymentPage::Updates {
                next_cursor,
                payments,
            })
        }
        _ => Err(PaymentOperationError::InvalidProviderResponse.into()),
    }
}

fn validate_catch_up_payment_updates(
    payments: Vec<LexePayment>,
    requested_start: Option<PaymentUpdatedIndex>,
    expected_last: PaymentUpdatedIndex,
) -> PaymentProviderResult<Vec<(PaymentUpdatedIndex, LexePayment)>> {
    let mut previous_cursor = None;
    let mut ordered = Vec::with_capacity(payments.len());
    for payment in payments {
        let cursor = payment.updated_index();
        if requested_start.is_some_and(|requested| cursor <= requested) {
            return Err(PaymentOperationError::InvalidProviderResponse.into());
        }
        if let Some(previous) = previous_cursor {
            if cursor < previous {
                return Err(PaymentOperationError::InvalidProviderResponse.into());
            }
            if cursor == previous {
                let (_, previous_payment) = ordered
                    .last()
                    .expect("a previous cursor implies a previous payment");
                if !same_validation_evidence(previous_payment, &payment) {
                    return Err(PaymentOperationError::InvalidProviderResponse.into());
                }
                continue;
            }
        }
        previous_cursor = Some(cursor);
        ordered.push((cursor, payment));
    }
    if previous_cursor != Some(expected_last) {
        return Err(PaymentOperationError::InvalidProviderResponse.into());
    }
    Ok(ordered)
}

fn same_validation_evidence(left: &LexePayment, right: &LexePayment) -> bool {
    left.index == right.index
        && left.kind == right.kind
        && left.direction == right.direction
        && left.hash == right.hash
        && left.amount == right.amount
        && left.status == right.status
        && left.finalized_at == right.finalized_at
        && left.personal_note == right.personal_note
        && left.invoice.as_deref().map(ToString::to_string)
            == right.invoice.as_deref().map(ToString::to_string)
}

fn validate_live_payment_update(
    requested_start: PaymentUpdatedIndex,
    next_start: PaymentUpdatedIndex,
    payment: LexePayment,
) -> PaymentProviderResult<(PaymentUpdatedIndex, LexePayment)> {
    if next_start <= requested_start || payment.updated_index() != next_start {
        return Err(PaymentOperationError::InvalidProviderResponse.into());
    }
    Ok((next_start, payment))
}

pub(super) fn validate_provider_update_sequence(
    requested_start: Option<&ProviderUpdateCursor>,
    response: ProviderPaymentUpdatePoll,
) -> PaymentProviderResult<ProviderPaymentUpdatePoll> {
    let requested_start = requested_start
        .map(|cursor| cursor.as_str().parse::<PaymentUpdatedIndex>())
        .transpose()
        .map_err(|_| PaymentOperationError::InvalidProviderResponse)?;
    let ProviderPaymentUpdatePoll::Updates(batch) = response else {
        return Ok(ProviderPaymentUpdatePoll::Idle);
    };

    let mut previous_cursor = None;
    let mut normalized = Vec::with_capacity(batch.updates().len());
    for update in batch.into_updates() {
        let cursor = update.next_cursor();
        if cursor.provider() != ProviderKind::Lexe {
            return Err(super::super::PaymentIdentityError::ProviderMismatch {
                expected: ProviderKind::Lexe,
                actual: cursor.provider(),
            }
            .into());
        }
        let cursor = cursor
            .as_str()
            .parse::<PaymentUpdatedIndex>()
            .map_err(|_| PaymentOperationError::InvalidProviderResponse)?;
        if requested_start.is_some_and(|requested| cursor <= requested) {
            return Err(PaymentOperationError::InvalidProviderResponse.into());
        }
        if let Some(previous) = previous_cursor {
            if cursor < previous {
                return Err(PaymentOperationError::InvalidProviderResponse.into());
            }
            if cursor == previous {
                if normalized.last() != Some(&update) {
                    return Err(PaymentOperationError::InvalidProviderResponse.into());
                }
                continue;
            }
        }
        previous_cursor = Some(cursor);
        normalized.push(update);
    }
    Ok(ProviderPaymentUpdatePoll::Updates(
        ProviderPaymentUpdateBatch::new(normalized)
            .map_err(|_| PaymentOperationError::InvalidProviderResponse)?,
    ))
}

fn provider_payment_update(
    cursor: PaymentUpdatedIndex,
    payment: LexePayment,
    network: LightningNetwork,
    now: OffsetDateTime,
) -> ProviderPaymentUpdate {
    let next_cursor = ProviderUpdateCursor::lexe(cursor.to_string())
        .expect("Lexe update cursors fit the provider cursor bound");
    let intent_id = match payment.personal_note.as_deref() {
        None => {
            return ignored_payment_update(next_cursor, IgnoredPaymentUpdateReason::MissingMarker);
        }
        Some(marker) => match parse_correlation_marker(marker) {
            Some(intent_id) => intent_id,
            None => {
                return ignored_payment_update(
                    next_cursor,
                    IgnoredPaymentUpdateReason::UnrecognizedMarker,
                );
            }
        },
    };
    let reference = ProviderPaymentReference::lexe(
        ProviderPaymentLocator::new(payment.index.to_string())
            .expect("Lexe payment indexes fit the provider locator bound"),
    );
    let Ok(observed_invoice) = provider_invoice(&payment) else {
        return conflicted_tip_update(next_cursor, intent_id, reference, None);
    };
    let Some(expected) =
        observed_payment_request(&payment, &observed_invoice, intent_id, reference.clone())
    else {
        return conflicted_tip_update(next_cursor, intent_id, reference, Some(observed_invoice));
    };
    let amount = expected.amount;
    let payment_hash = expected.payment_hash;
    let invoice = expected.invoice.clone();
    let index = payment.index;

    match payment_status(
        expected,
        index,
        payment,
        Some(&observed_invoice),
        network,
        now,
    ) {
        Ok(status) => ProviderPaymentUpdate::Tip(ObservedTipPaymentUpdate::new(
            next_cursor,
            intent_id,
            invoice,
            amount,
            payment_hash,
            status,
        )),
        Err(_) => conflicted_tip_update(next_cursor, intent_id, reference, Some(invoice)),
    }
}

fn ignored_payment_update(
    next_cursor: ProviderUpdateCursor,
    reason: IgnoredPaymentUpdateReason,
) -> ProviderPaymentUpdate {
    ProviderPaymentUpdate::Ignored(IgnoredProviderPaymentUpdate::new(next_cursor, reason))
}

fn conflicted_tip_update(
    next_cursor: ProviderUpdateCursor,
    intent_id: TipIntentId,
    payment: ProviderPaymentReference,
    observed_invoice: Option<Bolt11Invoice>,
) -> ProviderPaymentUpdate {
    ProviderPaymentUpdate::TipRecoveryRequired(ObservedTipRecoveryUpdate::new(
        next_cursor,
        intent_id,
        observed_invoice,
        ProviderPaymentStatus::new(
            payment,
            ProviderPaymentState::RecoveryRequired(TipRecoveryReason::ProviderConflict),
        ),
    ))
}

fn observed_payment_request(
    payment: &LexePayment,
    invoice: &Bolt11Invoice,
    intent_id: TipIntentId,
    reference: ProviderPaymentReference,
) -> Option<ReconcilePaymentRequest> {
    if payment.kind != PaymentKind::Invoice || payment.direction != PaymentDirection::Inbound {
        return None;
    }
    let amount = invoice.amount().ok()?;
    let payment_hash = invoice.payment_hash();
    Some(ReconcilePaymentRequest::new(
        reference,
        intent_id,
        invoice.clone(),
        amount,
        payment_hash,
    ))
}

fn parse_correlation_marker(value: &str) -> Option<TipIntentId> {
    let intent_id = value.strip_prefix(super::super::models::TIP_CORRELATION_MARKER_PREFIX)?;
    TipIntentId::parse(intent_id).ok()
}

fn payment_status(
    request: ReconcilePaymentRequest,
    expected_index: PaymentCreatedIndex,
    payment: LexePayment,
    observed_invoice: Option<&Bolt11Invoice>,
    network: LightningNetwork,
    now: OffsetDateTime,
) -> PaymentProviderResult<ProviderPaymentStatus> {
    validate_provider_payment_envelope(&request, expected_index, &payment)?;

    let parsed_invoice;
    let invoice = match observed_invoice {
        Some(invoice) => invoice,
        None => {
            parsed_invoice = provider_invoice(&payment)?;
            &parsed_invoice
        }
    };
    validate_invoice_envelope(expected_index, &payment, invoice)?;
    validate_reconciled_identity(&request, &payment, invoice, network, expected_index)?;

    let expires_at = invoice
        .expires_at()
        .map_err(|_| PaymentOperationError::InvalidProviderResponse)?;
    let status = match payment.status {
        PaymentStatus::Pending if expires_at <= now => ProviderPaymentState::Expired,
        PaymentStatus::Pending => ProviderPaymentState::InvoiceOpen,
        PaymentStatus::Failed if expires_at <= now => ProviderPaymentState::Expired,
        PaymentStatus::Failed => {
            ProviderPaymentState::RecoveryRequired(TipRecoveryReason::ProviderConflict)
        }
        PaymentStatus::Completed => completed_payment_state(&request, &payment),
    };

    Ok(ProviderPaymentStatus::new(request.payment.clone(), status))
}

fn validate_provider_payment_envelope(
    request: &ReconcilePaymentRequest,
    expected_index: PaymentCreatedIndex,
    payment: &LexePayment,
) -> PaymentProviderResult<()> {
    let marker = request.correlation_marker();
    let observed = (
        payment.index,
        payment.kind.clone(),
        payment.direction,
        payment.personal_note.as_deref(),
    );
    let expected = (
        expected_index,
        PaymentKind::Invoice,
        PaymentDirection::Inbound,
        Some(marker.as_str()),
    );
    if observed != expected {
        return Err(PaymentOperationError::ProviderConflict.into());
    }
    Ok(())
}

fn validate_invoice_envelope(
    expected_index: PaymentCreatedIndex,
    payment: &LexePayment,
    invoice: &Bolt11Invoice,
) -> PaymentProviderResult<()> {
    let provider_invoice = payment
        .invoice
        .as_deref()
        .ok_or(PaymentOperationError::InvalidProviderResponse)?;
    let envelope_matches = expected_index.id == provider_invoice.payment_id()
        && invoice.encoded() == provider_invoice.to_string();
    if !envelope_matches {
        return Err(PaymentOperationError::ProviderConflict.into());
    }
    Ok(())
}

fn validate_reconciled_identity(
    request: &ReconcilePaymentRequest,
    payment: &LexePayment,
    invoice: &Bolt11Invoice,
    network: LightningNetwork,
    expected_index: PaymentCreatedIndex,
) -> PaymentProviderResult<()> {
    validate_signed_invoice_identity(request, invoice, network)?;
    validate_provider_payment_evidence(request, payment, expected_index)
}

fn validate_signed_invoice_identity(
    request: &ReconcilePaymentRequest,
    invoice: &Bolt11Invoice,
    network: LightningNetwork,
) -> PaymentProviderResult<()> {
    let observed = (
        invoice.network(),
        invoice.encoded(),
        invoice.payment_hash(),
        invoice.amount().ok(),
    );
    let expected = (
        network,
        request.invoice.encoded(),
        request.payment_hash,
        Some(request.amount),
    );
    if observed != expected {
        return Err(PaymentOperationError::ProviderConflict.into());
    }
    Ok(())
}

fn validate_provider_payment_evidence(
    request: &ReconcilePaymentRequest,
    payment: &LexePayment,
    expected_index: PaymentCreatedIndex,
) -> PaymentProviderResult<()> {
    let indexed_hash = LexePaymentHash::try_from(expected_index.id)
        .map_err(|_| PaymentOperationError::ProviderConflict)?;
    let expected_msats = request
        .amount
        .get()
        .checked_mul(1_000)
        .ok_or(PaymentOperationError::ProviderConflict)?;
    let observed_amount_msats = payment.amount.map(|amount| amount.msat());
    let expected_amount_msats = payment.amount.map(|_| expected_msats);
    if (payment.hash, observed_amount_msats) != (Some(indexed_hash), expected_amount_msats) {
        return Err(PaymentOperationError::ProviderConflict.into());
    }
    Ok(())
}

fn provider_invoice(payment: &LexePayment) -> Result<Bolt11Invoice, PaymentOperationError> {
    let provider_invoice = payment
        .invoice
        .as_deref()
        .ok_or(PaymentOperationError::InvalidProviderResponse)?;
    let invoice = Bolt11Invoice::parse(&provider_invoice.to_string())
        .map_err(|_| PaymentOperationError::InvalidProviderResponse)?;
    Ok(invoice)
}

fn completed_payment_state(
    request: &ReconcilePaymentRequest,
    payment: &LexePayment,
) -> ProviderPaymentState {
    if payment.amount.is_none() {
        return ProviderPaymentState::RecoveryRequired(TipRecoveryReason::SettlementIncomplete);
    }
    let Some(finalized_at) = payment.finalized_at.and_then(lexe_timestamp) else {
        return ProviderPaymentState::RecoveryRequired(TipRecoveryReason::SettlementIncomplete);
    };

    ProviderPaymentState::Received(TipSettlement::new(request.amount, finalized_at))
}

fn tip_invoice_from_parts(
    request: CreateTipInvoiceRequest,
    network: LightningNetwork,
    index: PaymentCreatedIndex,
    encoded_invoice: String,
    validation: InvoiceValidation,
) -> Result<TipInvoice, PaymentOperationError> {
    let invoice = Bolt11Invoice::parse(&encoded_invoice)
        .map_err(|_| PaymentOperationError::InvalidProviderResponse)?;
    let payment = ProviderPaymentReference::lexe(
        ProviderPaymentLocator::new(index.to_string())
            .map_err(|_| PaymentOperationError::InvalidProviderResponse)?,
    );
    let invoice = match validation {
        InvoiceValidation::Fresh(now) => {
            TipInvoice::try_from_invoice(invoice, request, network, now, payment)
        }
        InvoiceValidation::Historical => {
            TipInvoice::try_from_reconciled_invoice(invoice, request, network, payment)
        }
    };
    invoice.map_err(|_| PaymentOperationError::InvalidProviderResponse)
}

fn create_invoice_request(
    request: &CreateTipInvoiceRequest,
) -> Result<LexeCreateInvoiceRequest, CreateTipInvoiceError> {
    let amount = LexeAmount::try_from_sats_u64(request.amount.get())
        .map_err(|_| CreateTipInvoiceError::NotCreated)?;
    Ok(LexeCreateInvoiceRequest {
        expiration_secs: None,
        amount: Some(amount),
        description: Some(request.description.as_str().to_owned()),
        personal_note: Some(request.correlation_marker().into_string()),
        partner_pk: None,
        partner_prop_fee: None,
        partner_base_fee: None,
    })
}

async fn await_creation<Operation, Value, Error>(
    deadline: PaymentResponseDeadline,
    operation: Operation,
) -> Result<Value, CreateTipInvoiceError>
where
    Operation: Future<Output = Result<Value, Error>>,
{
    match timeout(deadline.get(), operation).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(_)) => Err(CreateTipInvoiceError::OutcomeUnknown(
            InvoiceCreationUnknownReason::ProviderDidNotConfirm,
        )),
        Err(_) => Err(CreateTipInvoiceError::OutcomeUnknown(
            InvoiceCreationUnknownReason::ResponseTimedOut,
        )),
    }
}

fn create_admission_error(error: QueueAdmissionError) -> CreateTipInvoiceError {
    CreateTipInvoiceError::NotAccepted(command_not_accepted(error))
}

fn operation_admission_error(error: QueueAdmissionError) -> PaymentProviderError {
    PaymentTransportError::NotAccepted(command_not_accepted(error)).into()
}

fn command_not_accepted(error: QueueAdmissionError) -> CommandNotAcceptedReason {
    match error {
        QueueAdmissionError::Full => CommandNotAcceptedReason::QueueFull,
        QueueAdmissionError::Closed => CommandNotAcceptedReason::ProviderUnavailable,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueueAdmissionError {
    Full,
    Closed,
}

async fn run_bounded_queue<Message, Handler, Task>(
    mut receiver: mpsc::Receiver<Message>,
    max_concurrency: usize,
    cancellation: CancellationToken,
    mut handler: Handler,
) -> Result<(), LexeProviderRuntimeError>
where
    Message: Send + 'static,
    Handler: FnMut(Message) -> Task,
    Task: Future<Output = ()> + Send + 'static,
{
    let mut join_set = JoinSet::new();
    let mut ingress_open = true;
    let mut shutdown_started = false;
    let mut operation_panicked = false;

    loop {
        if !ingress_open && join_set.is_empty() {
            return if operation_panicked {
                Err(LexeProviderRuntimeError::OperationPanicked)
            } else {
                Ok(())
            };
        }

        tokio::select! {
            biased;

            _ = cancellation.cancelled(), if !shutdown_started => {
                receiver.close();
                shutdown_started = true;
            }

            Some(result) = join_set.join_next(), if !join_set.is_empty() => {
                if result.is_err() {
                    receiver.close();
                    shutdown_started = true;
                    operation_panicked = true;
                }
            }

            message = receiver.recv(), if ingress_open && join_set.len() < max_concurrency => {
                match message {
                    Some(message) => {
                        join_set.spawn(handler(message));
                    }
                    None => ingress_open = false,
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InvoiceValidation {
    Fresh(OffsetDateTime),
    Historical,
}

#[derive(Clone, Eq, PartialEq)]
struct LexeCreationRecord {
    index: PaymentCreatedIndex,
    hash: Option<LexePaymentHash>,
    amount: Option<LexeAmount>,
    personal_note: Option<String>,
    invoice: Option<String>,
    is_inbound_invoice: bool,
    index_matches_invoice: bool,
}

impl std::fmt::Debug for LexeCreationRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LexeCreationRecord")
            .field("index", &"[REDACTED]")
            .field("has_hash", &self.hash.is_some())
            .field("has_amount", &self.amount.is_some())
            .field("has_personal_note", &self.personal_note.is_some())
            .field("has_invoice", &self.invoice.is_some())
            .field("is_inbound_invoice", &self.is_inbound_invoice)
            .field("index_matches_invoice", &self.index_matches_invoice)
            .finish()
    }
}

impl LexeCreationRecord {
    fn from_payment(payment: LexePayment) -> Self {
        let index = payment.index;
        let invoice = payment.invoice.as_deref().map(ToString::to_string);
        let index_matches_invoice = payment
            .invoice
            .as_deref()
            .is_some_and(|invoice| index.id == invoice.payment_id());
        Self {
            index,
            hash: payment.hash,
            amount: payment.amount,
            personal_note: payment.personal_note,
            invoice,
            is_inbound_invoice: payment.kind == PaymentKind::Invoice
                && payment.direction == PaymentDirection::Inbound,
            index_matches_invoice,
        }
    }

    fn matches(&self, marker: &str) -> bool {
        self.personal_note.as_deref() == Some(marker)
    }

    fn validate_fresh_confirmation(
        &self,
        request: &CreateTipInvoiceRequest,
        created_index: PaymentCreatedIndex,
        created_invoice: &str,
    ) -> Result<(), PaymentOperationError> {
        validate_creation_evidence(request, created_index, self.hash, self.amount)?;
        let marker = request.correlation_marker();
        let observed = (
            self.index,
            self.is_inbound_invoice,
            self.index_matches_invoice,
            self.personal_note.as_deref(),
            self.invoice.as_deref(),
        );
        let expected = (
            created_index,
            true,
            true,
            Some(marker.as_str()),
            Some(created_invoice),
        );
        if observed != expected {
            return Err(PaymentOperationError::InvalidProviderResponse);
        }
        Ok(())
    }

    fn into_reconciled_invoice(
        self,
        request: &CreateTipInvoiceRequest,
    ) -> Result<(PaymentCreatedIndex, String), PaymentOperationError> {
        let marker = request.correlation_marker();
        let observed = (
            self.personal_note.as_deref(),
            self.is_inbound_invoice,
            self.index_matches_invoice,
        );
        let expected = (Some(marker.as_str()), true, true);
        if observed != expected {
            return Err(PaymentOperationError::InvalidProviderResponse);
        }
        validate_creation_evidence(request, self.index, self.hash, self.amount)?;
        let invoice = self
            .invoice
            .ok_or(PaymentOperationError::InvalidProviderResponse)?;
        Ok((self.index, invoice))
    }
}

fn select_marker_matches(records: impl IntoIterator<Item = LexeCreationRecord>) -> MarkerSelection {
    let mut records = records.into_iter();
    match (records.next(), records.next()) {
        (None, _) => MarkerSelection::Missing,
        (Some(record), None) => MarkerSelection::Found(record),
        (Some(_), Some(_)) => MarkerSelection::Ambiguous,
    }
}

fn track_marker_record(
    matches: &mut BTreeMap<PaymentCreatedIndex, LexeCreationRecord>,
    marker: &str,
    record: LexeCreationRecord,
) {
    if record.matches(marker) {
        matches.insert(record.index, record);
    } else {
        matches.remove(&record.index);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum MarkerSelection {
    Missing,
    Found(LexeCreationRecord),
    Ambiguous,
}

enum ValidatedPaymentPage {
    Empty,
    Updates {
        next_cursor: PaymentUpdatedIndex,
        payments: Vec<(PaymentUpdatedIndex, LexePayment)>,
    },
}

enum PaymentUpdateSource {
    Ready(Vec<(PaymentUpdatedIndex, LexePayment)>),
    Live(PaymentUpdatedIndex),
}

fn lexe_timestamp(timestamp: lexe::types::util::TimestampMs) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(timestamp.to_i64()) * 1_000_000)
        .ok()
        .map(|value| value.to_offset(UtcOffset::UTC))
}

fn lightning_network(network: LexeNetwork) -> LightningNetwork {
    match network {
        LexeNetwork::Mainnet => LightningNetwork::Mainnet,
        LexeNetwork::Testnet3 | LexeNetwork::Testnet4 => LightningNetwork::Testnet,
        LexeNetwork::Regtest => LightningNetwork::Regtest,
        LexeNetwork::Signet => LightningNetwork::Signet,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use lightning_invoice::Currency;

    use super::*;
    use crate::payments::{
        PaymentModelError, SatoshiAmount, TipIntentId, TipInvoiceDescription,
        test_support::{signed_direct_invoice, signed_direct_invoice_with},
    };

    #[test]
    fn invoice_request_keeps_private_marker_out_of_payer_description() {
        let request = create_request();
        let sdk_request = create_invoice_request(&request).unwrap();

        assert_eq!(sdk_request.description.as_deref(), Some("Tip"));
        assert_eq!(
            sdk_request.personal_note.as_deref(),
            Some("maincopy-tip:2e776d7d-7d5f-4ab7-8c63-434c66a262aa")
        );
        assert!(
            !sdk_request
                .description
                .unwrap()
                .contains(request.correlation_marker().as_str())
        );
        assert_eq!(sdk_request.amount.unwrap().sats_u64(), 21);
    }

    #[tokio::test]
    async fn creation_deadline_has_an_unknown_outcome() {
        let error = await_creation(
            deadline(Duration::from_millis(1)),
            future::pending::<Result<(), ()>>(),
        )
        .await
        .unwrap_err();

        assert_eq!(
            error,
            CreateTipInvoiceError::OutcomeUnknown(InvoiceCreationUnknownReason::ResponseTimedOut)
        );
    }

    #[tokio::test]
    async fn join_set_worker_enforces_concurrency_and_drains_on_close() {
        let (sender, receiver) = mpsc::channel(8);
        let current = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        for value in 0..6 {
            sender.try_send(value).unwrap();
        }
        drop(sender);

        let task_current = Arc::clone(&current);
        let task_peak = Arc::clone(&peak);
        run_bounded_queue(receiver, 2, CancellationToken::new(), move |_| {
            let current = Arc::clone(&task_current);
            let peak = Arc::clone(&task_peak);
            async move {
                let running = current.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(running, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
                current.fetch_sub(1, Ordering::SeqCst);
            }
        })
        .await
        .unwrap();

        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert_eq!(current.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn subscriber_slot_leaves_capacity_for_an_ordinary_operation() {
        type TestTask = std::pin::Pin<Box<dyn Future<Output = ()> + Send>>;

        let (sender, receiver) = mpsc::channel::<TestTask>(2);
        let (tail_started_send, tail_started) = oneshot::channel();
        let (release_tail, tail_release) = oneshot::channel();
        sender
            .try_send(Box::pin(async move {
                let _ = tail_started_send.send(());
                let _ = tail_release.await;
            }))
            .unwrap();
        let (ordinary_complete_send, ordinary_complete) = oneshot::channel();
        sender
            .try_send(Box::pin(async move {
                let _ = ordinary_complete_send.send(());
            }))
            .unwrap();

        let runtime = tokio::spawn(run_bounded_queue(
            receiver,
            PaymentConcurrencyLimit::new(2).unwrap().get(),
            CancellationToken::new(),
            |task| task,
        ));
        tail_started.await.unwrap();
        timeout(Duration::from_millis(50), ordinary_complete)
            .await
            .expect("ordinary work should run while the subscriber waits")
            .unwrap();

        release_tail.send(()).unwrap();
        drop(sender);
        assert_eq!(runtime.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn join_set_worker_preserves_fifo_start_order_at_concurrency_one() {
        let (sender, receiver) = mpsc::channel(4);
        for value in 0..4 {
            sender.try_send(value).unwrap();
        }
        drop(sender);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let task_observed = Arc::clone(&observed);

        run_bounded_queue(receiver, 1, CancellationToken::new(), move |value| {
            let observed = Arc::clone(&task_observed);
            async move { observed.lock().unwrap().push(value) }
        })
        .await
        .unwrap();

        assert_eq!(*observed.lock().unwrap(), vec![0, 1, 2, 3]);
    }

    #[tokio::test]
    async fn cancellation_closes_ingress_and_drains_running_and_pending_work() {
        let (sender, receiver) = mpsc::channel(4);
        for value in 0..3 {
            sender.try_send(value).unwrap();
        }
        let cancellation = CancellationToken::new();
        let started = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(AtomicUsize::new(0));
        let task_started = Arc::clone(&started);
        let task_completed = Arc::clone(&completed);
        let task_cancellation = cancellation.clone();

        let runtime = tokio::spawn(async move {
            run_bounded_queue(receiver, 1, task_cancellation, move |_| {
                let started = Arc::clone(&task_started);
                let completed = Arc::clone(&task_completed);
                async move {
                    started.notify_one();
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    completed.fetch_add(1, Ordering::SeqCst);
                }
            })
            .await
        });

        started.notified().await;
        cancellation.cancel();
        while !sender.is_closed() {
            tokio::task::yield_now().await;
        }

        assert!(sender.is_closed());
        assert!(matches!(
            sender.try_send(99),
            Err(mpsc::error::TrySendError::Closed(99))
        ));
        assert_eq!(runtime.await.unwrap(), Ok(()));
        assert_eq!(completed.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn worker_reaps_other_tasks_before_reporting_a_panic() {
        let (sender, receiver) = mpsc::channel(3);
        for value in 0..3 {
            sender.try_send(value).unwrap();
        }
        drop(sender);
        let completed = Arc::new(AtomicUsize::new(0));
        let task_completed = Arc::clone(&completed);

        let result = run_bounded_queue(receiver, 3, CancellationToken::new(), move |value| {
            let completed = Arc::clone(&task_completed);
            async move {
                if value == 1 {
                    panic!("deliberate queue task panic");
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
                completed.fetch_add(1, Ordering::SeqCst);
            }
        })
        .await;

        assert_eq!(result, Err(LexeProviderRuntimeError::OperationPanicked));
        assert_eq!(completed.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn bounded_ingress_reports_full_and_closed_without_waiting() {
        let (commands, receiver) = mpsc::channel(1);
        let provider = LexeProvider {
            commands,
            network: LightningNetwork::Mainnet,
        };

        assert_eq!(provider.submit(update_command()), Ok(()));
        assert_eq!(
            provider.submit(update_command()),
            Err(QueueAdmissionError::Full)
        );
        drop(receiver);
        assert_eq!(
            provider.submit(update_command()),
            Err(QueueAdmissionError::Closed)
        );
    }

    #[test]
    fn marker_selection_distinguishes_zero_one_and_multiple_distinct_matches() {
        let marker = "maincopy-tip:marker";
        let unrelated = record(1, Some("another-marker"));
        assert_eq!(
            select_records([unrelated.clone()], marker),
            MarkerSelection::Missing
        );

        let matching = record(2, Some(marker));
        assert_eq!(
            select_records([unrelated, matching.clone()], marker),
            MarkerSelection::Found(matching.clone())
        );
        assert_eq!(
            select_records([matching, record(3, Some(marker))], marker),
            MarkerSelection::Ambiguous
        );
    }

    #[test]
    fn marker_selection_deduplicates_updates_by_created_index() {
        let marker = "maincopy-tip:marker";
        let first_update = record(1, Some(marker));
        let latest_update = first_update.clone();

        assert_eq!(
            select_records([first_update, latest_update.clone()], marker),
            MarkerSelection::Found(latest_update)
        );
        assert_eq!(
            select_records([record(1, Some(marker)), record(1, None)], marker),
            MarkerSelection::Missing
        );
    }

    #[test]
    fn fresh_confirmation_rejects_hash_and_present_amount_conflicts() {
        let request = create_request();
        let mut payment = lexe_payment(Some(request.correlation_marker().as_str()));
        assert_eq!(validate_fresh_payment(&request, &payment), Ok(()));

        payment.hash = Some(LexePaymentHash::try_from(payment_index(2).id).unwrap());
        assert_eq!(
            validate_fresh_payment(&request, &payment),
            Err(PaymentOperationError::InvalidProviderResponse)
        );
        payment.hash = Some(LexePaymentHash::try_from(payment.index.id).unwrap());
        payment.amount = Some(LexeAmount::try_from_sats_u64(22).unwrap());
        assert_eq!(
            validate_fresh_payment(&request, &payment),
            Err(PaymentOperationError::InvalidProviderResponse)
        );

        payment.amount = None;
        assert_eq!(validate_fresh_payment(&request, &payment), Ok(()));
    }

    #[test]
    fn marker_recovery_rejects_hash_and_present_amount_conflicts() {
        let request = create_request();
        let payment = lexe_payment(Some(request.correlation_marker().as_str()));
        let mut record = LexeCreationRecord::from_payment(payment);
        assert_eq!(
            validate_creation_evidence(&request, record.index, record.hash, record.amount),
            Ok(())
        );

        record.hash = Some(LexePaymentHash::try_from(payment_index(2).id).unwrap());
        assert_eq!(
            validate_creation_evidence(&request, record.index, record.hash, record.amount),
            Err(PaymentOperationError::InvalidProviderResponse)
        );
        record.hash = Some(LexePaymentHash::try_from(record.index.id).unwrap());
        record.amount = Some(LexeAmount::try_from_sats_u64(22).unwrap());
        assert_eq!(
            validate_creation_evidence(&request, record.index, record.hash, record.amount),
            Err(PaymentOperationError::InvalidProviderResponse)
        );
    }

    #[test]
    fn confirmed_creation_accepts_only_consistent_provider_evidence() {
        let request = create_request();
        let payment = lexe_payment(Some(request.correlation_marker().as_str()));
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_001).unwrap();
        let invoice = confirmed_creation(
            request.clone(),
            LightningNetwork::Mainnet,
            now,
            created_response(payment.index),
            Some(payment.clone()),
        )
        .unwrap();
        assert_eq!(invoice.intent_id, request.intent_id);

        assert_eq!(
            confirmed_creation(
                request.clone(),
                LightningNetwork::Mainnet,
                now,
                created_response(payment.index),
                None,
            ),
            Err(PaymentOperationError::InvalidProviderResponse)
        );

        let mut mismatched_record = payment;
        mismatched_record.personal_note = Some("different-marker".to_owned());
        assert_eq!(
            confirmed_creation(
                request,
                LightningNetwork::Mainnet,
                now,
                created_response(mismatched_record.index),
                Some(mismatched_record),
            ),
            Err(PaymentOperationError::InvalidProviderResponse)
        );
    }

    #[test]
    fn created_invoice_envelope_must_bind_the_returned_payment_index() {
        assert_eq!(
            created_invoice_parts(created_response(payment_index(2))),
            Err(PaymentOperationError::InvalidProviderResponse)
        );
    }

    #[test]
    fn creation_reconciliation_preserves_missing_ambiguous_and_found_outcomes() {
        let request = create_request();
        assert_eq!(
            reconcile_creation_selection(
                request.clone(),
                LightningNetwork::Mainnet,
                MarkerSelection::Missing,
            ),
            Ok(InvoiceCreationReconciliation::Missing)
        );
        assert_eq!(
            reconcile_creation_selection(
                request.clone(),
                LightningNetwork::Mainnet,
                MarkerSelection::Ambiguous,
            ),
            Ok(InvoiceCreationReconciliation::Ambiguous)
        );

        let record = LexeCreationRecord::from_payment(lexe_payment(Some(
            request.correlation_marker().as_str(),
        )));
        let found = reconcile_creation_selection(
            request.clone(),
            LightningNetwork::Mainnet,
            MarkerSelection::Found(record),
        )
        .unwrap();
        let InvoiceCreationReconciliation::Found(invoice) = found else {
            panic!("expected the unique valid record to be recovered");
        };
        assert_eq!(invoice.intent_id, request.intent_id);

        let invalid_record = LexeCreationRecord::from_payment(lexe_payment(Some(
            "maincopy-tip:00000000-0000-0000-0000-000000000000",
        )));
        assert_eq!(
            reconcile_creation_selection(
                request,
                LightningNetwork::Mainnet,
                MarkerSelection::Found(invalid_record),
            ),
            Err(PaymentOperationError::InvalidProviderResponse.into())
        );
    }

    #[test]
    fn subscriber_and_reconciler_share_validated_state_mapping() {
        let payment = lexe_payment(Some("maincopy-tip:2e776d7d-7d5f-4ab7-8c63-434c66a262aa"));
        let cursor = payment.updated_index();
        let expected = expected_request(&payment);
        let reconciled = payment_status(
            expected.clone(),
            payment.index,
            payment.clone(),
            None,
            LightningNetwork::Mainnet,
            OffsetDateTime::from_unix_timestamp(1_700_000_001).unwrap(),
        )
        .unwrap();

        let ProviderPaymentUpdate::Tip(observed) = provider_payment_update(
            cursor,
            payment,
            LightningNetwork::Mainnet,
            OffsetDateTime::from_unix_timestamp(1_700_000_001).unwrap(),
        ) else {
            panic!("expected a validated tip update");
        };
        assert_eq!(observed.status(), &reconciled);
        assert_eq!(observed.invoice(), &expected.invoice);
    }

    #[test]
    fn pending_and_failed_invoices_allow_an_absent_provider_amount() {
        let mut pending = lexe_payment(Some("maincopy-tip:2e776d7d-7d5f-4ab7-8c63-434c66a262aa"));
        pending.amount = None;
        let expected = expected_request(&pending);
        assert_eq!(
            payment_status(
                expected.clone(),
                pending.index,
                pending.clone(),
                None,
                LightningNetwork::Mainnet,
                OffsetDateTime::from_unix_timestamp(1_700_000_001).unwrap(),
            )
            .unwrap()
            .status,
            ProviderPaymentState::InvoiceOpen
        );

        pending.status = PaymentStatus::Failed;
        assert_eq!(
            payment_status(
                expected,
                pending.index,
                pending,
                None,
                LightningNetwork::Mainnet,
                OffsetDateTime::from_unix_timestamp(1_700_000_001).unwrap(),
            )
            .unwrap()
            .status,
            ProviderPaymentState::RecoveryRequired(TipRecoveryReason::ProviderConflict)
        );
    }

    #[test]
    fn completed_invoice_without_provider_amount_requires_recovery() {
        let mut payment = lexe_payment(Some("maincopy-tip:2e776d7d-7d5f-4ab7-8c63-434c66a262aa"));
        payment.status = PaymentStatus::Completed;
        payment.amount = None;
        payment.finalized_at = Some("1700000000002".parse().unwrap());
        let expected = expected_request(&payment);

        assert_eq!(
            payment_status(
                expected,
                payment.index,
                payment,
                None,
                LightningNetwork::Mainnet,
                OffsetDateTime::from_unix_timestamp(1_700_000_001).unwrap(),
            )
            .unwrap()
            .status,
            ProviderPaymentState::RecoveryRequired(TipRecoveryReason::SettlementIncomplete)
        );
    }

    #[test]
    fn any_present_provider_amount_must_match_the_signed_invoice() {
        let mut payment = lexe_payment(Some("maincopy-tip:2e776d7d-7d5f-4ab7-8c63-434c66a262aa"));
        payment.amount = Some(LexeAmount::try_from_sats_u64(22).unwrap());
        let expected = expected_request(&payment);

        assert_eq!(
            payment_status(
                expected,
                payment.index,
                payment,
                None,
                LightningNetwork::Mainnet,
                OffsetDateTime::from_unix_timestamp(1_700_000_001).unwrap(),
            )
            .unwrap_err(),
            PaymentProviderError::Operation(PaymentOperationError::ProviderConflict)
        );
    }

    #[test]
    fn reconciliation_rejects_same_hash_invoice_with_a_different_description() {
        let mut payment = lexe_payment(Some("maincopy-tip:2e776d7d-7d5f-4ab7-8c63-434c66a262aa"));
        let expected = expected_request(&payment);
        payment.invoice = Some(Arc::new(
            signed_invoice_with("Different description", Duration::from_secs(3_600))
                .encoded()
                .parse()
                .unwrap(),
        ));

        assert_eq!(
            payment_status(
                expected,
                payment.index,
                payment,
                None,
                LightningNetwork::Mainnet,
                OffsetDateTime::from_unix_timestamp(1_700_000_001).unwrap(),
            )
            .unwrap_err(),
            PaymentProviderError::Operation(PaymentOperationError::ProviderConflict)
        );
    }

    #[test]
    fn reconciliation_rejects_same_hash_invoice_with_a_different_expiry() {
        let mut payment = lexe_payment(Some("maincopy-tip:2e776d7d-7d5f-4ab7-8c63-434c66a262aa"));
        let expected = expected_request(&payment);
        payment.invoice = Some(Arc::new(
            signed_invoice_with("Tip", Duration::from_secs(7_200))
                .encoded()
                .parse()
                .unwrap(),
        ));

        assert_eq!(
            payment_status(
                expected,
                payment.index,
                payment,
                None,
                LightningNetwork::Mainnet,
                OffsetDateTime::from_unix_timestamp(1_700_000_001).unwrap(),
            )
            .unwrap_err(),
            PaymentProviderError::Operation(PaymentOperationError::ProviderConflict)
        );
    }

    #[test]
    fn pre_parsed_invoice_must_belong_to_the_payment_envelope() {
        let payment = lexe_payment(Some("maincopy-tip:2e776d7d-7d5f-4ab7-8c63-434c66a262aa"));
        let observed = signed_invoice_with("Different description", Duration::from_secs(3_600));
        let expected = observed_payment_request(
            &payment,
            &observed,
            create_request().intent_id,
            ProviderPaymentReference::lexe(
                ProviderPaymentLocator::new(payment.index.to_string()).unwrap(),
            ),
        )
        .unwrap();

        assert_eq!(
            payment_status(
                expected,
                payment.index,
                payment,
                Some(&observed),
                LightningNetwork::Mainnet,
                OffsetDateTime::from_unix_timestamp(1_700_000_001).unwrap(),
            )
            .unwrap_err(),
            PaymentProviderError::Operation(PaymentOperationError::ProviderConflict)
        );
    }

    #[test]
    fn payment_envelope_is_rejected_before_missing_invoice_parsing() {
        let mut payment = lexe_payment(Some("maincopy-tip:2e776d7d-7d5f-4ab7-8c63-434c66a262aa"));
        let expected = expected_request(&payment);
        payment.kind = PaymentKind::Offer;
        payment.invoice = None;

        assert_eq!(
            payment_status(
                expected,
                payment.index,
                payment,
                None,
                LightningNetwork::Mainnet,
                OffsetDateTime::from_unix_timestamp(1_700_000_001).unwrap(),
            )
            .unwrap_err(),
            PaymentProviderError::Operation(PaymentOperationError::ProviderConflict)
        );
    }

    #[test]
    fn valid_payment_envelope_with_missing_invoice_is_an_invalid_response() {
        let mut payment = lexe_payment(Some("maincopy-tip:2e776d7d-7d5f-4ab7-8c63-434c66a262aa"));
        let expected = expected_request(&payment);
        payment.invoice = None;

        assert_eq!(
            payment_status(
                expected,
                payment.index,
                payment,
                None,
                LightningNetwork::Mainnet,
                OffsetDateTime::from_unix_timestamp(1_700_000_001).unwrap(),
            )
            .unwrap_err(),
            PaymentProviderError::Operation(PaymentOperationError::InvalidProviderResponse)
        );
    }

    #[test]
    fn signed_invoice_and_provider_evidence_are_validated_independently() {
        let mut payment = lexe_payment(Some("maincopy-tip:2e776d7d-7d5f-4ab7-8c63-434c66a262aa"));
        let expected = expected_request(&payment);
        let invoice = provider_invoice(&payment).unwrap();
        assert_eq!(
            validate_signed_invoice_identity(&expected, &invoice, LightningNetwork::Mainnet),
            Ok(())
        );
        assert_eq!(
            validate_provider_payment_evidence(&expected, &payment, payment.index),
            Ok(())
        );

        assert_eq!(
            validate_signed_invoice_identity(&expected, &invoice, LightningNetwork::Testnet),
            Err(PaymentOperationError::ProviderConflict.into())
        );
        payment.hash = Some(LexePaymentHash::try_from(payment_index(2).id).unwrap());
        assert_eq!(
            validate_provider_payment_evidence(&expected, &payment, payment.index),
            Err(PaymentOperationError::ProviderConflict.into())
        );
    }

    #[test]
    fn parseable_tip_marker_with_identity_conflict_requires_recovery() {
        let mut payment = lexe_payment(Some("maincopy-tip:2e776d7d-7d5f-4ab7-8c63-434c66a262aa"));
        let conflicting_index = payment_index(2);
        payment.hash = Some(LexePaymentHash::try_from(conflicting_index.id).unwrap());

        let ProviderPaymentUpdate::TipRecoveryRequired(update) = provider_payment_update(
            payment.updated_index(),
            payment,
            LightningNetwork::Mainnet,
            OffsetDateTime::from_unix_timestamp(1_700_000_001).unwrap(),
        ) else {
            panic!("expected a recovery-required tip update");
        };
        assert_eq!(
            &update.status().status,
            &ProviderPaymentState::RecoveryRequired(TipRecoveryReason::ProviderConflict)
        );
        assert_eq!(update.observed_invoice(), Some(&signed_invoice()));
    }

    #[test]
    fn unrelated_and_malformed_markers_remain_cursor_advanceable() {
        for (note, expected_reason) in [
            (None, IgnoredPaymentUpdateReason::MissingMarker),
            (
                Some("wallet note"),
                IgnoredPaymentUpdateReason::UnrecognizedMarker,
            ),
            (
                Some("maincopy-tip:not-a-uuid"),
                IgnoredPaymentUpdateReason::UnrecognizedMarker,
            ),
        ] {
            let payment = lexe_payment(note);
            let expected_cursor = payment.updated_index().to_string();
            let ProviderPaymentUpdate::Ignored(update) = provider_payment_update(
                payment.updated_index(),
                payment,
                LightningNetwork::Mainnet,
                OffsetDateTime::from_unix_timestamp(1_700_000_001).unwrap(),
            ) else {
                panic!("expected unrelated payment to be ignored");
            };
            assert_eq!(update.next_cursor.as_str(), expected_cursor);
            assert_eq!(update.reason, expected_reason);
        }
    }

    #[test]
    fn payment_page_decision_distinguishes_tail_from_catch_up() {
        assert!(matches!(
            validate_payment_page(
                GetUpdatedPaymentsResponse {
                    payments: Vec::new(),
                    updated_index: None,
                },
                None,
            ),
            Ok(ValidatedPaymentPage::Empty)
        ));

        let payment = lexe_payment(None);
        let next_cursor = payment.updated_index();
        let page = validate_payment_page(
            GetUpdatedPaymentsResponse {
                payments: vec![payment],
                updated_index: Some(next_cursor),
            },
            None,
        )
        .unwrap();
        let ValidatedPaymentPage::Updates {
            next_cursor: observed_cursor,
            payments,
        } = page
        else {
            panic!("expected a validated catch-up page");
        };
        assert_eq!(observed_cursor, next_cursor);
        assert_eq!(payments.len(), 1);
    }

    #[test]
    fn validated_pages_drive_creation_search_and_update_polling() {
        let request = create_request();
        let marker = request.correlation_marker();
        let payment = lexe_payment(Some(marker.as_str()));
        let cursor = payment.updated_index();
        let mut matches = BTreeMap::new();

        track_creation_payments(
            &mut matches,
            marker.as_str(),
            vec![(cursor, payment.clone())],
        );
        assert_eq!(
            select_marker_matches(matches.into_values()),
            MarkerSelection::Found(LexeCreationRecord::from_payment(payment.clone()))
        );

        assert!(matches!(
            payment_update_source(ValidatedPaymentPage::Empty, None),
            PaymentUpdateSource::Live(value) if value == PaymentUpdatedIndex::MIN
        ));
        assert!(matches!(
            payment_update_source(
                ValidatedPaymentPage::Updates {
                    next_cursor: cursor,
                    payments: vec![(cursor, payment)],
                },
                None,
            ),
            PaymentUpdateSource::Ready(payments) if payments.len() == 1
        ));
    }

    #[test]
    fn payment_page_decision_rejects_cursor_without_payments_and_vice_versa() {
        let payment = lexe_payment(None);
        let cursor = payment.updated_index();
        for response in [
            GetUpdatedPaymentsResponse {
                payments: Vec::new(),
                updated_index: Some(cursor),
            },
            GetUpdatedPaymentsResponse {
                payments: vec![payment],
                updated_index: None,
            },
        ] {
            assert!(matches!(
                validate_payment_page(response, None),
                Err(PaymentProviderError::Operation(
                    PaymentOperationError::InvalidProviderResponse
                ))
            ));
        }
    }

    #[test]
    fn catch_up_rejects_a_cursor_equal_to_the_requested_start() {
        let payment = lexe_payment(None);
        let cursor = payment.updated_index();
        assert_eq!(
            validate_catch_up_payment_updates(vec![payment], Some(cursor), cursor)
                .err()
                .unwrap(),
            PaymentProviderError::Operation(PaymentOperationError::InvalidProviderResponse)
        );
    }

    #[test]
    fn catch_up_rejects_older_regressing_and_conflicting_duplicate_cursors() {
        let older = payment_updated_at(lexe_payment(None), 1_700_000_000_001);
        let newer = payment_updated_at(lexe_payment(None), 1_700_000_000_003);
        let older_cursor = older.updated_index();
        let newer_cursor = newer.updated_index();

        assert_eq!(
            validate_catch_up_payment_updates(
                vec![older.clone()],
                Some(newer_cursor),
                older_cursor,
            )
            .err()
            .unwrap(),
            PaymentProviderError::Operation(PaymentOperationError::InvalidProviderResponse)
        );
        let mut conflicting_duplicate = older.clone();
        conflicting_duplicate.personal_note = Some("different evidence".to_owned());
        assert_eq!(
            validate_catch_up_payment_updates(
                vec![older.clone(), conflicting_duplicate],
                None,
                older_cursor,
            )
            .err()
            .unwrap(),
            PaymentProviderError::Operation(PaymentOperationError::InvalidProviderResponse)
        );
        assert_eq!(
            validate_catch_up_payment_updates(vec![newer, lexe_payment(None)], None, older_cursor)
                .err()
                .unwrap(),
            PaymentProviderError::Operation(PaymentOperationError::InvalidProviderResponse)
        );
    }

    #[test]
    fn catch_up_coalesces_an_identical_repeated_cursor() {
        let payment = lexe_payment(None);
        let cursor = payment.updated_index();
        let validated =
            validate_catch_up_payment_updates(vec![payment.clone(), payment], None, cursor)
                .unwrap();

        assert_eq!(validated.len(), 1);
        assert_eq!(validated[0].0, cursor);
    }

    #[test]
    fn catch_up_accepts_only_strictly_increasing_page_order() {
        let first = payment_updated_at(lexe_payment(None), 1_700_000_000_002);
        let second = payment_updated_at(lexe_payment(None), 1_700_000_000_003);
        let requested = payment_updated_at(lexe_payment(None), 1_700_000_000_001).updated_index();
        let expected_last = second.updated_index();
        let validated =
            validate_catch_up_payment_updates(vec![first, second], Some(requested), expected_last)
                .unwrap();

        assert_eq!(validated.len(), 2);
        assert_eq!(validated[1].0, expected_last);
    }

    #[test]
    fn live_update_rejects_nonadvancing_and_mismatched_cursors() {
        let payment = payment_updated_at(lexe_payment(None), 1_700_000_000_002);
        let payment_cursor = payment.updated_index();
        assert_eq!(
            validate_live_payment_update(payment_cursor, payment_cursor, payment.clone())
                .err()
                .unwrap(),
            PaymentProviderError::Operation(PaymentOperationError::InvalidProviderResponse)
        );

        let requested = payment_updated_at(lexe_payment(None), 1_700_000_000_001).updated_index();
        let mismatched = payment_updated_at(lexe_payment(None), 1_700_000_000_003).updated_index();
        assert_eq!(
            validate_live_payment_update(requested, mismatched, payment)
                .err()
                .unwrap(),
            PaymentProviderError::Operation(PaymentOperationError::InvalidProviderResponse)
        );
    }

    fn payment_updated_at(mut payment: LexePayment, timestamp_ms: i64) -> LexePayment {
        payment.updated_at = timestamp_ms.to_string().parse().unwrap();
        payment
    }

    #[test]
    fn provider_handle_is_send_and_sync() {
        fn assert_send_sync<Value: Send + Sync>() {}
        assert_send_sync::<LexeProvider>();
        assert_send_sync::<LexeProviderRuntime>();
    }

    #[test]
    fn queue_controls_are_strong_nonzero_types() {
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
        assert_eq!(PaymentConcurrencyLimit::new(3).unwrap().get(), 3);
        assert_eq!(PaymentQueueCapacity::new(8).unwrap().get(), 8);
    }

    fn select_records(
        records: impl IntoIterator<Item = LexeCreationRecord>,
        marker: &str,
    ) -> MarkerSelection {
        let mut matches = BTreeMap::new();
        for record in records {
            track_marker_record(&mut matches, marker, record);
        }
        select_marker_matches(matches.into_values())
    }

    fn record(index_byte: u8, personal_note: Option<&str>) -> LexeCreationRecord {
        let payment_hash = format!("{index_byte:02x}").repeat(32);
        LexeCreationRecord {
            index: format!("0000000000000000001-ln_{payment_hash}")
                .parse()
                .unwrap(),
            hash: Some(payment_hash.parse().unwrap()),
            amount: Some(LexeAmount::try_from_sats_u64(21).unwrap()),
            personal_note: personal_note.map(str::to_owned),
            invoice: Some("invoice".to_owned()),
            is_inbound_invoice: true,
            index_matches_invoice: true,
        }
    }

    fn lexe_payment(personal_note: Option<&str>) -> LexePayment {
        let index = payment_index(1);
        let invoice = signed_invoice();
        LexePayment {
            index,
            rail: lexe::types::payment::PaymentRail::Invoice,
            kind: PaymentKind::Invoice,
            direction: PaymentDirection::Inbound,
            hash: Some(LexePaymentHash::try_from(index.id).unwrap()),
            preimage: None,
            offer_id: None,
            txid: None,
            amount: Some(LexeAmount::try_from_sats_u64(21).unwrap()),
            fees: LexeAmount::ZERO,
            partner_pk: None,
            partner_prop_fee: None,
            partner_base_fee: None,
            status: PaymentStatus::Pending,
            status_msg: "invoice generated".to_owned(),
            address: None,
            invoice: Some(Arc::new(invoice.encoded().parse().unwrap())),
            tx: None,
            payer_name: None,
            message: None,
            personal_note: personal_note.map(str::to_owned),
            priority: None,
            expires_at: Some("1700003600000".parse().unwrap()),
            finalized_at: None,
            created_at: "1700000000000".parse().unwrap(),
            updated_at: "1700000000001".parse().unwrap(),
        }
    }

    fn expected_request(payment: &LexePayment) -> ReconcilePaymentRequest {
        let invoice = provider_invoice(payment).unwrap();
        observed_payment_request(
            payment,
            &invoice,
            create_request().intent_id,
            ProviderPaymentReference::lexe(
                ProviderPaymentLocator::new(payment.index.to_string()).unwrap(),
            ),
        )
        .unwrap()
    }

    fn validate_fresh_payment(
        request: &CreateTipInvoiceRequest,
        payment: &LexePayment,
    ) -> Result<(), PaymentOperationError> {
        let encoded_invoice = payment.invoice.as_deref().unwrap().to_string();
        LexeCreationRecord::from_payment(payment.clone()).validate_fresh_confirmation(
            request,
            payment.index,
            &encoded_invoice,
        )
    }

    fn created_response(index: PaymentCreatedIndex) -> LexeCreateInvoiceResponse {
        LexeCreateInvoiceResponse::new(index, signed_invoice().encoded().parse().unwrap())
    }

    fn payment_index(index_byte: u8) -> PaymentCreatedIndex {
        let payment_hash = format!("{index_byte:02x}").repeat(32);
        format!("0000001700000000000-ln_{payment_hash}")
            .parse()
            .unwrap()
    }

    fn signed_invoice() -> Bolt11Invoice {
        signed_direct_invoice("Tip", 21)
    }

    fn signed_invoice_with(description: &str, expiry: Duration) -> Bolt11Invoice {
        signed_direct_invoice_with(Currency::Bitcoin, description, 21, expiry)
    }

    fn create_request() -> CreateTipInvoiceRequest {
        CreateTipInvoiceRequest::new(
            TipIntentId::parse("2e776d7d-7d5f-4ab7-8c63-434c66a262aa").unwrap(),
            SatoshiAmount::new(21).unwrap(),
            TipInvoiceDescription::tip(),
        )
    }

    fn deadline(value: Duration) -> PaymentResponseDeadline {
        PaymentResponseDeadline::new(value).unwrap()
    }

    fn update_command() -> LexeCommand {
        let (respond_to, _response) = oneshot::channel();
        LexeCommand::NextPaymentUpdates {
            start_index: None,
            respond_to,
        }
    }
}
