//! The sender: makes each pending delivery's one attempt and records what
//! came of it (spec/usage.md, "What a world sends").
//!
//! Deliveries go out one at a time, oldest first, so a receiver gets them in
//! the order their events were logged. A delivery goes out as soon as it is
//! pending, whatever the world's clock mode: an attempt takes real time, and
//! the world's clock is only what its result is dated with.

use std::sync::Arc;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use orrery_outbound::{Client, Failure, Request};
use orrery_world::{Delivery, DeliveryFailure, DeliveryOutcome, Signal, World, WorldError};

/// How many pending deliveries one read takes. The loop reads again until
/// none is pending, so this bounds a read, not what is sent.
const BATCH: usize = 32;

/// How long the loop waits after a result it could not record.
const RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(1);

/// Send the world's deliveries until `stopping` turns true. Without a
/// runtime to run on there is nothing to send with, and the deliveries stay
/// pending.
pub(crate) fn start(
    world: Arc<World>,
    client: Client,
    stopping: tokio::sync::watch::Receiver<bool>,
) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::warn!("no runtime to send deliveries on: they stay pending");
        return;
    };
    let wake = Arc::new(tokio::sync::Notify::new());
    world.listen({
        let wake = wake.clone();
        // A listener must not block: this stores a wake-up and returns.
        move |signal| {
            if matches!(
                signal,
                Signal::LogMayHaveGrown | Signal::StateReplaced { .. }
            ) {
                wake.notify_one();
            }
        }
    });
    runtime.spawn(run(world, client, wake, stopping));
}

async fn run(
    world: Arc<World>,
    client: Client,
    wake: Arc<tokio::sync::Notify>,
    mut stopping: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        loop {
            let pending = match world.pending_deliveries(BATCH) {
                Ok(pending) => pending,
                Err(e) => {
                    tracing::error!(error = %e, "reading pending deliveries failed");
                    break;
                }
            };
            if pending.is_empty() {
                break;
            }
            let mut recorded = true;
            for delivery in pending {
                if *stopping.borrow() {
                    return;
                }
                recorded &= send(&world, &client, delivery).await;
            }
            // A result that could not be recorded leaves its delivery
            // pending. Reading it again at once would send it again at
            // once, so the loop waits before it does.
            if !recorded {
                tokio::select! {
                    () = tokio::time::sleep(RETRY_AFTER) => {}
                    _ = stopping.wait_for(|stopping| *stopping) => return,
                }
            }
        }
        tokio::select! {
            () = wake.notified() => {}
            _ = stopping.wait_for(|stopping| *stopping) => return,
        }
    }
}

/// Make one delivery's attempt and record its result, answering whether the
/// delivery is no longer pending. A delivery a restore dropped or discarded
/// while its attempt was in flight has nowhere to record one.
async fn send(world: &World, client: &Client, delivery: Delivery) -> bool {
    let secret = match world.app_hook_secret(delivery.app_id) {
        Ok(secret) => secret,
        Err(e) => {
            tracing::error!(error = %e, delivery = delivery.id, "reading the hook's secret failed");
            return false;
        }
    };
    let mut headers = HeaderMap::new();
    for (name, value) in
        orrery_face_github::webhooks::delivery_headers(&delivery, secret.as_deref())
    {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            headers.append(name, value);
        }
    }
    let attempt = client
        .post(Request {
            url: delivery.url.clone(),
            headers,
            body: delivery.body.clone().into_bytes(),
        })
        .await;
    let outcome = match attempt.outcome {
        Ok(response) => DeliveryOutcome::Answered {
            status_code: i64::from(response.status.as_u16()),
            headers: response
                .headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_string(),
                        String::from_utf8_lossy(value.as_bytes()).into_owned(),
                    )
                })
                .collect(),
            body: String::from_utf8_lossy(&response.body).into_owned(),
            truncated: response.body_truncated,
        },
        Err(failure) => DeliveryOutcome::Failed {
            step: match &failure {
                Failure::Url(_) => DeliveryFailure::Url,
                Failure::Connect(_) => DeliveryFailure::Connect,
                Failure::Proxy(_) => DeliveryFailure::Proxy,
                Failure::Tls(_) => DeliveryFailure::Tls,
                Failure::Timeout => DeliveryFailure::Timeout,
                Failure::Exchange(_) => DeliveryFailure::Exchange,
            },
            detail: failure.to_string(),
        },
    };
    let duration_ms = i64::try_from(attempt.elapsed.as_millis()).unwrap_or(i64::MAX);
    match world.record_delivery_attempt(delivery.id, duration_ms, &outcome) {
        Ok(_) | Err(WorldError::NoSuchDelivery(_) | WorldError::DeliveryNotPending(_)) => true,
        Err(e) => {
            tracing::error!(error = %e, delivery = delivery.id, "recording an attempt failed");
            false
        }
    }
}
