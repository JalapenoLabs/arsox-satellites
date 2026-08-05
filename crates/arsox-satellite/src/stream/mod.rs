// Copyright © 2026 Jalapeno Labs

//! Live delivery of events to connected consumers.
//!
//! The event log is the source of truth and this is a view onto it. Every event
//! is written to the database before it is published here, so a consumer that
//! reconnects can always replay what it missed.
//!
//! # Many consumers, one broadcast
//!
//! A single channel carries every thread's events and subscribers filter. A
//! channel per thread would avoid waking uninterested subscribers, at the cost
//! of a map that has to be created, reference counted, and torn down in step
//! with thread lifetimes. With a per-satellite concurrency cap in the single
//! digits, that bookkeeping costs more than the wakeups it saves. Revisit if a
//! satellite ever runs hundreds of threads at once.
//!
//! # Lagging is loud
//!
//! The channel is bounded. A consumer that falls too far behind is dropped by
//! the broadcast itself, which surfaces as an error rather than as silence, and
//! the socket is closed telling it exactly that. Reconnecting with
//! `from_sequence` costs it nothing. Slow consumers degrade loudly, never
//! quietly.

pub mod sockets;

use arsox_sdk::proto::event::v1::{ControlEvent, ThreadEvent};
use std::sync::Arc;
use tokio::sync::broadcast;

/// Events buffered per consumer before it is considered hopeless.
///
/// Large enough that a consumer doing ordinary work never notices, small enough
/// that one stuck consumer cannot hold a turn's worth of events in memory on
/// everyone else's behalf.
const CHANNEL_CAPACITY: usize = 1024;

/// Publishes events to whoever is listening.
///
/// Cheap to clone: senders share one channel.
#[derive(Debug, Clone)]
pub struct EventBus {
    thread_events: broadcast::Sender<Arc<ThreadEvent>>,
    control_events: broadcast::Sender<Arc<ControlEvent>>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    #[must_use]
    pub fn new() -> Self {
        Self {
            thread_events: broadcast::channel(CHANNEL_CAPACITY).0,
            control_events: broadcast::channel(CHANNEL_CAPACITY).0,
        }
    }

    /// Publishes a thread event.
    ///
    /// Sending to a channel with no subscribers is not a failure. A satellite
    /// doing work nobody is watching is the normal case, not an error.
    pub fn publish(&self, event: Arc<ThreadEvent>) {
        drop(self.thread_events.send(event));
    }

    /// Publishes a satellite lifecycle event.
    pub fn publish_control(&self, event: ControlEvent) {
        drop(self.control_events.send(Arc::new(event)));
    }

    /// Subscribes to every thread's events.
    ///
    /// **Subscribe before replaying.** A consumer that reads history first and
    /// subscribes afterwards loses everything published in between, and the gap
    /// is invisible: the sequence numbers it receives are contiguous with what
    /// it read, just missing the middle. Subscribing first and discarding what
    /// the replay already covered is the only ordering without that hole.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<ThreadEvent>> {
        self.thread_events.subscribe()
    }

    /// Subscribes to satellite lifecycle events.
    #[must_use]
    pub fn subscribe_control(&self) -> broadcast::Receiver<Arc<ControlEvent>> {
        self.control_events.subscribe()
    }
}

/// Builds a control event ready to publish.
///
/// The sequence is left at zero: control events are satellite lifecycle rather
/// than thread history, so they are never persisted and never replayed, and
/// there is no per-thread counter for them to draw from.
#[must_use]
pub fn control_event(
    type_name: &str,
    payload: arsox_sdk::proto::event::v1::control_event::Payload,
) -> arsox_sdk::proto::event::v1::ControlEvent {
    arsox_sdk::proto::event::v1::ControlEvent {
        sequence: 0,
        occurred_at: Some(arsox_sdk::proto::common::v1::Timestamp::now()),
        r#type: type_name.to_owned(),
        payload: Some(payload),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(thread_id: &str, sequence: u64) -> Arc<ThreadEvent> {
        Arc::new(ThreadEvent {
            sequence,
            thread_id: thread_id.to_owned(),
            r#type: "agent.message".to_owned(),
            ..ThreadEvent::default()
        })
    }

    #[tokio::test]
    async fn every_subscriber_receives_every_event() {
        // A horizontally scaled host application has several replicas watching
        // one thread, and each must receive all of it. A design that delivered
        // to one subscriber would force a designated replica and internal
        // fan-out.
        let bus = EventBus::new();
        let mut first = bus.subscribe();
        let mut second = bus.subscribe();

        bus.publish(event("thread-a", 1));

        assert_eq!(first.recv().await.expect("first").sequence, 1);
        assert_eq!(second.recv().await.expect("second").sequence, 1);
    }

    #[tokio::test]
    async fn publishing_with_nobody_listening_is_not_a_failure() {
        // The normal case: a satellite doing work nobody is watching.
        let bus = EventBus::new();
        bus.publish(event("thread-a", 1));
    }

    #[tokio::test]
    async fn a_consumer_that_falls_behind_is_told_rather_than_silently_skipped() {
        let bus = EventBus::new();
        let mut slow = bus.subscribe();

        for sequence in 1..=(CHANNEL_CAPACITY as u64 + 10) {
            bus.publish(event("thread-a", sequence));
        }

        // The broadcast reports the overflow instead of quietly dropping the
        // oldest events, which is what lets the socket close with a reason the
        // consumer can act on.
        let error = slow.recv().await.expect_err("should report lagging");
        assert!(matches!(error, broadcast::error::RecvError::Lagged(_)));
    }
}
