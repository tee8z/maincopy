use std::{future::Future, time::Duration};

use tokio_util::sync::CancellationToken;

use super::{
    LightningProvider, NextPaymentUpdatesRequest, PaymentIdentityError, PaymentModelError,
    ProviderPaymentUpdate, ProviderPaymentUpdatePoll, ProviderUpdateCursor,
};

/// Retry policy for provider disconnects and durable-handler failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaymentUpdateRetryPolicy {
    initial_delay: Duration,
    maximum_delay: Duration,
}

/// Redacted subscriber health signal for application diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaymentUpdateSubscriberEvent {
    Healthy,
    RetryScheduled {
        cause: PaymentUpdateRetryCause,
        delay: Duration,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaymentUpdateRetryCause {
    ProviderFailure,
    DurableHandlerFailure,
}

impl PaymentUpdateRetryPolicy {
    pub fn new(
        initial_delay: Duration,
        maximum_delay: Duration,
    ) -> Result<Self, PaymentModelError> {
        if initial_delay.is_zero() || maximum_delay.is_zero() {
            return Err(PaymentModelError::ZeroPaymentUpdateRetryDelay);
        }
        if maximum_delay < initial_delay {
            return Err(PaymentModelError::PaymentUpdateMaximumRetryDelayTooShort);
        }
        Ok(Self {
            initial_delay,
            maximum_delay,
        })
    }
}

/// Single application-owned supervisor for provider payment updates.
///
/// Application startup must construct exactly one subscriber after loading its
/// durable cursor. The handler runs updates sequentially. Returning the exact
/// update cursor is an acknowledgement that the ledger decision (or audit
/// disposition) and cursor were committed atomically by the database writer.
/// Returning `Err(_)` leaves the cursor unchanged and replays from that cursor
/// after backoff. The database-backed handler is a later composition step; the
/// subscriber itself neither stores state nor creates another work queue.
/// The handler must be idempotent for the case where its commit succeeds but
/// its completion signal is lost, and no other task may advance this cursor.
pub struct PaymentUpdateSubscriber {
    provider: LightningProvider,
    cursor: Option<ProviderUpdateCursor>,
    retry_policy: PaymentUpdateRetryPolicy,
}

impl PaymentUpdateSubscriber {
    pub fn new(
        provider: LightningProvider,
        persisted_cursor: Option<ProviderUpdateCursor>,
        retry_policy: PaymentUpdateRetryPolicy,
    ) -> Result<Self, PaymentIdentityError> {
        if let Some(cursor) = &persisted_cursor {
            let expected = provider.kind();
            let actual = cursor.provider();
            if actual != expected {
                return Err(PaymentIdentityError::ProviderMismatch { expected, actual });
            }
        }
        Ok(Self {
            provider,
            cursor: persisted_cursor,
            retry_policy,
        })
    }

    /// Runs until cancellation while preserving at-least-once update delivery.
    ///
    /// Provider and handler failures retry from the last acknowledged cursor.
    /// An idle long poll is healthy: it resets failure backoff and immediately
    /// starts the next finite poll.
    pub async fn run_until_stop<Handler, HandlerFuture, HandlerError, Observer>(
        mut self,
        cancellation: CancellationToken,
        mut handler: Handler,
        mut observe: Observer,
    ) where
        Handler: FnMut(ProviderPaymentUpdate) -> HandlerFuture + Send,
        HandlerFuture: Future<Output = Result<ProviderUpdateCursor, HandlerError>> + Send,
        Observer: FnMut(PaymentUpdateSubscriberEvent) + Send,
    {
        let mut backoff = RetryBackoff::new(self.retry_policy);

        loop {
            if cancellation.is_cancelled() {
                return;
            }

            let request = NextPaymentUpdatesRequest::new(self.cursor.clone());
            let response = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return,
                response = self.provider.next_payment_updates(request) => response,
            };

            let retry_cause = match response {
                Err(_) => Some(PaymentUpdateRetryCause::ProviderFailure),
                Ok(ProviderPaymentUpdatePoll::Idle) => {
                    backoff.reset();
                    observe(PaymentUpdateSubscriberEvent::Healthy);
                    None
                }
                Ok(ProviderPaymentUpdatePoll::Updates(batch)) => {
                    let mut handler_failed = false;
                    for update in batch.into_updates() {
                        if cancellation.is_cancelled() {
                            return;
                        }
                        let next_cursor = update.next_cursor().clone();
                        match handler(update).await {
                            Ok(acknowledged_cursor) if acknowledged_cursor == next_cursor => {
                                self.cursor = Some(next_cursor);
                                backoff.reset();
                                observe(PaymentUpdateSubscriberEvent::Healthy);
                            }
                            Ok(_) | Err(_) => {
                                handler_failed = true;
                                break;
                            }
                        }
                        if cancellation.is_cancelled() {
                            return;
                        }
                    }

                    if handler_failed && cancellation.is_cancelled() {
                        return;
                    }

                    handler_failed.then_some(PaymentUpdateRetryCause::DurableHandlerFailure)
                }
            };

            if let Some(cause) = retry_cause {
                let delay = backoff.next_delay();
                observe(PaymentUpdateSubscriberEvent::RetryScheduled { cause, delay });
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return,
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

impl std::fmt::Debug for PaymentUpdateSubscriber {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PaymentUpdateSubscriber")
            .field("provider", &self.provider.kind())
            .field("has_persisted_cursor", &self.cursor.is_some())
            .field("retry_policy", &self.retry_policy)
            .finish_non_exhaustive()
    }
}

struct RetryBackoff {
    policy: PaymentUpdateRetryPolicy,
    next: Duration,
}

impl RetryBackoff {
    const fn new(policy: PaymentUpdateRetryPolicy) -> Self {
        Self {
            next: policy.initial_delay,
            policy,
        }
    }

    fn next_delay(&mut self) -> Duration {
        let current = self.next;
        self.next = self
            .next
            .checked_mul(2)
            .unwrap_or(self.policy.maximum_delay)
            .min(self.policy.maximum_delay);
        current
    }

    fn reset(&mut self) {
        self.next = self.policy.initial_delay;
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use tokio::sync::Notify;

    use super::*;
    use crate::payments::{
        CreateTipInvoiceError, IgnoredPaymentUpdateReason, IgnoredProviderPaymentUpdate,
        InvoiceCreationReconciliation, PaymentOperationError, ProviderPaymentUpdateBatch,
        ProviderSubstitute, SubstituteCall, SubstituteResponses,
    };

    #[test]
    fn retry_policy_rejects_zero_and_reversed_bounds() {
        assert_eq!(
            PaymentUpdateRetryPolicy::new(Duration::ZERO, Duration::from_secs(1)),
            Err(PaymentModelError::ZeroPaymentUpdateRetryDelay)
        );
        assert_eq!(
            PaymentUpdateRetryPolicy::new(Duration::from_secs(2), Duration::from_secs(1)),
            Err(PaymentModelError::PaymentUpdateMaximumRetryDelayTooShort)
        );
    }

    #[tokio::test]
    async fn prompt_update_is_acknowledged_without_retry_delay() {
        let (provider, substitute) = substitute_provider();
        substitute.enqueue_next_payment_updates(Ok(update_batch([1])));
        let cancellation = CancellationToken::new();
        let handler_cancellation = cancellation.clone();
        let handled = Arc::new(Mutex::new(Vec::new()));
        let handler_observed = Arc::clone(&handled);
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed_events = Arc::clone(&events);
        subscriber(provider)
            .run_until_stop(
                cancellation,
                move |update| {
                    let ack = update.next_cursor().clone();
                    handler_observed
                        .lock()
                        .unwrap()
                        .push(update.next_cursor().as_str().to_owned());
                    handler_cancellation.cancel();
                    future::ready(Ok::<ProviderUpdateCursor, ()>(ack))
                },
                move |event| observed_events.lock().unwrap().push(event),
            )
            .await;

        assert_eq!(*handled.lock().unwrap(), vec![update_cursor_value(1)]);
        assert_eq!(requested_cursors(&substitute), vec![None]);
        assert_eq!(
            *events.lock().unwrap(),
            vec![PaymentUpdateSubscriberEvent::Healthy]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn failed_ack_replays_from_the_latest_durable_cursor() {
        let (provider, substitute) = substitute_provider();
        substitute.enqueue_next_payment_updates(Ok(update_batch([1, 2])));
        substitute.enqueue_next_payment_updates(Ok(update_batch([2])));
        let cancellation = CancellationToken::new();
        let handler_cancellation = cancellation.clone();
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let handler_attempts = Arc::clone(&attempts);
        let second_attempts = Arc::new(AtomicUsize::new(0));
        let handler_second_attempts = Arc::clone(&second_attempts);
        let events = Arc::new(EventLog::default());
        let observed_events = Arc::clone(&events);

        let task = tokio::spawn(subscriber(provider).run_until_stop(
            cancellation,
            move |update| {
                let cursor = update.next_cursor().as_str().to_owned();
                let ack = update.next_cursor().clone();
                handler_attempts.lock().unwrap().push(cursor.clone());
                let should_fail = cursor == update_cursor_value(2)
                    && handler_second_attempts.fetch_add(1, Ordering::SeqCst) == 0;
                if !should_fail && cursor == update_cursor_value(2) {
                    handler_cancellation.cancel();
                }
                future::ready(if should_fail { Err(()) } else { Ok(ack) })
            },
            move |event| observed_events.push(event),
        ));

        events.wait_for_len(2).await;
        tokio::task::yield_now().await;
        assert_eq!(requested_cursors(&substitute), vec![None]);
        tokio::time::advance(Duration::from_millis(9)).await;
        assert_eq!(requested_cursors(&substitute), vec![None]);
        tokio::time::advance(Duration::from_millis(1)).await;
        task.await.unwrap();

        assert_eq!(
            *attempts.lock().unwrap(),
            vec![
                update_cursor_value(1),
                update_cursor_value(2),
                update_cursor_value(2),
            ]
        );
        assert_eq!(
            requested_cursors(&substitute),
            vec![None, Some(update_cursor_value(1))]
        );
        assert_eq!(
            events.snapshot(),
            vec![
                PaymentUpdateSubscriberEvent::Healthy,
                PaymentUpdateSubscriberEvent::RetryScheduled {
                    cause: PaymentUpdateRetryCause::DurableHandlerFailure,
                    delay: Duration::from_millis(10),
                },
                PaymentUpdateSubscriberEvent::Healthy,
            ]
        );
    }

    #[tokio::test]
    async fn idle_poll_is_healthy_and_reissued_without_backoff() {
        let (provider, substitute) = substitute_provider();
        substitute.enqueue_next_payment_updates(Ok(ProviderPaymentUpdatePoll::Idle));
        substitute.enqueue_next_payment_updates(Ok(update_batch([1])));
        let cancellation = CancellationToken::new();
        let handler_cancellation = cancellation.clone();
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed_events = Arc::clone(&events);

        subscriber(provider)
            .run_until_stop(
                cancellation,
                move |update| {
                    let ack = update.next_cursor().clone();
                    handler_cancellation.cancel();
                    future::ready(Ok::<ProviderUpdateCursor, ()>(ack))
                },
                move |event| observed_events.lock().unwrap().push(event),
            )
            .await;

        assert_eq!(requested_cursors(&substitute), vec![None, None]);
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                PaymentUpdateSubscriberEvent::Healthy,
                PaymentUpdateSubscriberEvent::Healthy,
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn mismatched_durable_ack_is_replayed_as_a_handler_failure() {
        let (provider, substitute) = substitute_provider();
        substitute.enqueue_next_payment_updates(Ok(update_batch([1])));
        substitute.enqueue_next_payment_updates(Ok(update_batch([1])));
        let cancellation = CancellationToken::new();
        let handler_cancellation = cancellation.clone();
        let attempts = Arc::new(AtomicUsize::new(0));
        let handler_attempts = Arc::clone(&attempts);
        let events = Arc::new(EventLog::default());
        let observed_events = Arc::clone(&events);

        let task = tokio::spawn(subscriber(provider).run_until_stop(
            cancellation,
            move |update| {
                let attempt = handler_attempts.fetch_add(1, Ordering::SeqCst);
                let ack = if attempt == 0 {
                    update_cursor(2)
                } else {
                    handler_cancellation.cancel();
                    update.next_cursor().clone()
                };
                future::ready(Ok::<ProviderUpdateCursor, ()>(ack))
            },
            move |event| observed_events.push(event),
        ));

        events.wait_for_len(1).await;
        tokio::task::yield_now().await;
        assert_eq!(requested_cursors(&substitute), vec![None]);
        tokio::time::advance(Duration::from_millis(9)).await;
        assert_eq!(requested_cursors(&substitute), vec![None]);
        tokio::time::advance(Duration::from_millis(1)).await;
        task.await.unwrap();

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(requested_cursors(&substitute), vec![None, None]);
        assert_eq!(
            events.snapshot(),
            vec![
                PaymentUpdateSubscriberEvent::RetryScheduled {
                    cause: PaymentUpdateRetryCause::DurableHandlerFailure,
                    delay: Duration::from_millis(10),
                },
                PaymentUpdateSubscriberEvent::Healthy,
            ]
        );
    }

    #[tokio::test]
    async fn cancellation_after_ack_stops_before_the_next_batch_item() {
        let (provider, substitute) = substitute_provider();
        substitute.enqueue_next_payment_updates(Ok(update_batch([1, 2])));
        let cancellation = CancellationToken::new();
        let handler_cancellation = cancellation.clone();
        let handled = Arc::new(Mutex::new(Vec::new()));
        let handler_observed = Arc::clone(&handled);

        subscriber(provider)
            .run_until_stop(
                cancellation,
                move |update| {
                    let ack = update.next_cursor().clone();
                    handler_observed
                        .lock()
                        .unwrap()
                        .push(update.next_cursor().as_str().to_owned());
                    handler_cancellation.cancel();
                    future::ready(Ok::<ProviderUpdateCursor, ()>(ack))
                },
                |_| {},
            )
            .await;

        assert_eq!(*handled.lock().unwrap(), vec![update_cursor_value(1)]);
        assert_eq!(requested_cursors(&substitute), vec![None]);
    }

    #[tokio::test(start_paused = true)]
    async fn provider_failures_use_capped_exponential_backoff() {
        let (provider, substitute) = substitute_provider();
        for _ in 0..3 {
            substitute.enqueue_next_payment_updates(Err(
                PaymentOperationError::TemporarilyUnavailable.into(),
            ));
        }
        substitute.enqueue_next_payment_updates(Ok(update_batch([1])));
        let cancellation = CancellationToken::new();
        let handler_cancellation = cancellation.clone();
        let events = Arc::new(EventLog::default());
        let observed_events = Arc::clone(&events);

        let task = tokio::spawn(subscriber(provider).run_until_stop(
            cancellation,
            move |update| {
                let ack = update.next_cursor().clone();
                handler_cancellation.cancel();
                future::ready(Ok::<ProviderUpdateCursor, ()>(ack))
            },
            move |event| observed_events.push(event),
        ));

        for (index, delay) in [10, 20, 25].into_iter().enumerate() {
            events.wait_for_len(index + 1).await;
            tokio::task::yield_now().await;
            assert_eq!(requested_cursors(&substitute).len(), index + 1);
            tokio::time::advance(Duration::from_millis(delay - 1)).await;
            assert_eq!(requested_cursors(&substitute).len(), index + 1);
            tokio::time::advance(Duration::from_millis(1)).await;
        }
        task.await.unwrap();

        assert_eq!(requested_cursors(&substitute), vec![None; 4]);
        assert_eq!(
            events.snapshot(),
            vec![
                PaymentUpdateSubscriberEvent::RetryScheduled {
                    cause: PaymentUpdateRetryCause::ProviderFailure,
                    delay: Duration::from_millis(10),
                },
                PaymentUpdateSubscriberEvent::RetryScheduled {
                    cause: PaymentUpdateRetryCause::ProviderFailure,
                    delay: Duration::from_millis(20),
                },
                PaymentUpdateSubscriberEvent::RetryScheduled {
                    cause: PaymentUpdateRetryCause::ProviderFailure,
                    delay: Duration::from_millis(25),
                },
                PaymentUpdateSubscriberEvent::Healthy,
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_interrupts_a_retry_sleep() {
        let (provider, substitute) = substitute_provider();
        substitute.enqueue_next_payment_updates(Err(
            PaymentOperationError::TemporarilyUnavailable.into()
        ));
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let events = Arc::new(EventLog::default());
        let observed_events = Arc::clone(&events);
        let subscriber = subscriber(provider);
        let task = tokio::spawn(async move {
            subscriber
                .run_until_stop(
                    task_cancellation,
                    |update| {
                        future::ready(Ok::<ProviderUpdateCursor, ()>(update.next_cursor().clone()))
                    },
                    move |event| observed_events.push(event),
                )
                .await;
        });

        events.wait_for_len(1).await;
        tokio::task::yield_now().await;
        let cancelled_at = tokio::time::Instant::now();
        assert!(!task.is_finished());
        cancellation.cancel();
        tokio::time::timeout(Duration::from_millis(50), task)
            .await
            .expect("cancellation should interrupt the retry sleep")
            .unwrap();
        assert_eq!(tokio::time::Instant::now(), cancelled_at);
        assert_eq!(
            events.snapshot(),
            vec![PaymentUpdateSubscriberEvent::RetryScheduled {
                cause: PaymentUpdateRetryCause::ProviderFailure,
                delay: Duration::from_millis(10),
            }]
        );
    }

    fn subscriber(provider: LightningProvider) -> PaymentUpdateSubscriber {
        PaymentUpdateSubscriber::new(
            provider,
            None,
            PaymentUpdateRetryPolicy::new(Duration::from_millis(10), Duration::from_millis(25))
                .unwrap(),
        )
        .unwrap()
    }

    fn substitute_provider() -> (LightningProvider, Arc<ProviderSubstitute>) {
        let substitute = Arc::new(ProviderSubstitute::new(SubstituteResponses {
            create_tip_invoice: Err(CreateTipInvoiceError::NotCreated),
            reconcile_invoice_creation: Ok(InvoiceCreationReconciliation::Missing),
            reconcile_payment: Err(PaymentOperationError::PaymentNotFound.into()),
            next_payment_updates: Ok(ProviderPaymentUpdatePoll::Idle),
        }));
        (
            LightningProvider::Substitute(Arc::clone(&substitute)),
            substitute,
        )
    }

    fn update_batch<const COUNT: usize>(cursors: [i64; COUNT]) -> ProviderPaymentUpdatePoll {
        let updates = cursors
            .into_iter()
            .map(|cursor| {
                ProviderPaymentUpdate::Ignored(IgnoredProviderPaymentUpdate::new(
                    update_cursor(cursor),
                    IgnoredPaymentUpdateReason::MissingMarker,
                ))
            })
            .collect();
        ProviderPaymentUpdatePoll::Updates(ProviderPaymentUpdateBatch::new(updates).unwrap())
    }

    fn update_cursor(sequence: i64) -> ProviderUpdateCursor {
        ProviderUpdateCursor::lexe(update_cursor_value(sequence)).unwrap()
    }

    fn update_cursor_value(sequence: i64) -> String {
        let timestamp = 1_700_000_000_000_i64 + sequence;
        format!("u{timestamp:019}-ln_{}", "01".repeat(32))
    }

    fn requested_cursors(substitute: &ProviderSubstitute) -> Vec<Option<String>> {
        substitute
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                SubstituteCall::NextPaymentUpdates(request) => Some(
                    request
                        .cursor
                        .as_ref()
                        .map(|cursor| cursor.as_str().to_owned()),
                ),
                _ => None,
            })
            .collect()
    }

    #[derive(Default)]
    struct EventLog {
        events: Mutex<Vec<PaymentUpdateSubscriberEvent>>,
        changed: Notify,
    }

    impl EventLog {
        fn push(&self, event: PaymentUpdateSubscriberEvent) {
            self.events.lock().unwrap().push(event);
            self.changed.notify_one();
        }

        fn snapshot(&self) -> Vec<PaymentUpdateSubscriberEvent> {
            self.events.lock().unwrap().clone()
        }

        async fn wait_for_len(&self, expected: usize) {
            while self.events.lock().unwrap().len() < expected {
                self.changed.notified().await;
            }
        }
    }
}
