// Copyright © 2026 Jalapeno Labs

//! Collecting threads, whether by expiry, by request, or on completion.
//!
//! Every thread must declare a lifetime when it is created. That requirement is
//! only worth anything if something acts on it, and this is the something.
//!
//! Three paths reach the same place. The idle TTL elapses, the SDK destroys a
//! thread, or a thread marked `delete_on_complete` finishes its last turn. All
//! three remove the workspace subtree and leave a tombstone, differing only in
//! the reason they report.

use crate::store::{Store, StoreError};
use crate::stream::{EventBus, control_event};
use crate::workspace::{self, WorkspaceError};
use arsox_sdk::proto::event::v1::control_event::Payload;
use arsox_sdk::proto::event::v1::{ThreadDestroyed, ThreadEndReason};
use arsox_sdk::proto::thread::v1::{Thread, ThreadState};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// How many threads one sweep will collect.
///
/// A satellite that has been down for a week comes back to a large backlog of
/// expired threads. Removing them all in one pass would hold the runtime on
/// filesystem work while the API waits, so a pass takes a bounded bite and the
/// next one takes the rest.
const COLLECT_BATCH: u32 = 32;

/// Removes threads and the workspaces they own.
#[derive(Debug, Clone)]
pub struct Collector {
    store: Store,
    workspace_root: PathBuf,
    bus: EventBus,

    /// The exec shim directories, which go with the workspace they gated.
    broker: crate::broker::Broker,
}

impl Collector {
    #[must_use]
    pub fn new(
        store: Store,
        workspace_root: PathBuf,
        bus: EventBus,
        broker: crate::broker::Broker,
    ) -> Self {
        Self {
            store,
            workspace_root,
            bus,
            broker,
        }
    }

    /// Sweeps expired threads until the satellite stops.
    pub async fn sweep_forever(self: Arc<Self>, interval: Duration) {
        let mut ticker = tokio::time::interval(interval);

        // A sweep that runs long must not queue up the ticks it missed and then
        // fire them back to back against the same backlog.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;
            self.sweep_once().await;
        }
    }

    /// Collects one batch of expired threads, returning how many it took.
    ///
    /// One thread failing never stops the sweep. A single workspace that will
    /// not delete, because a file is held open or a permission was lost, would
    /// otherwise block every expired thread behind it forever.
    pub async fn sweep_once(&self) -> usize {
        let expired = match self.store.expired_threads(COLLECT_BATCH).await {
            Ok(expired) => expired,
            Err(error) => {
                tracing::error!(
                    event.name = "satellite.collector.query_failed",
                    "could not list expired threads: {error}",
                );
                return 0;
            }
        };

        let mut collected = 0;
        for thread_id in expired {
            match self.collect(&thread_id, ThreadEndReason::Expired).await {
                Ok(_thread) => collected += 1,
                Err(error) => {
                    tracing::error!(
                        event.name = "satellite.collector.failed",
                        thread.id = thread_id,
                        "leaving this thread for the next sweep: {error}",
                    );
                }
            }
        }

        if collected > 0 {
            tracing::info!(
                event.name = "satellite.collector.swept",
                thread.count = collected,
                "collected {collected} expired threads",
            );
        }

        collected
    }

    /// Removes a thread's workspace, tombstones it, and announces it.
    ///
    /// The workspace goes first. Reversed, a process that died between the two
    /// steps would leave a thread reading as collected while its files remained,
    /// and nothing would ever look at it again: a leak no later sweep can find.
    /// This order fails the other way, leaving a workspace whose thread is still
    /// expired, which the next sweep picks straight back up.
    ///
    /// # Errors
    ///
    /// Returns [`CollectError::Workspace`] when the subtree cannot be removed,
    /// and [`CollectError::Store`] when the thread is unknown or the tombstone
    /// cannot be written.
    pub async fn collect(
        &self,
        thread_id: &str,
        reason: ThreadEndReason,
    ) -> Result<Thread, CollectError> {
        workspace::remove_thread_directory(&self.workspace_root, thread_id).await?;

        // Reported rather than raised. A shim directory left behind is a few
        // thousand inodes on a path nothing will look at again, and refusing to
        // collect the thread over it would leave the workspace it gated in place
        // too.
        if let Err(error) = self.broker.remove(thread_id).await {
            tracing::error!(
                event.name = "broker.remove.failed",
                thread.id = thread_id,
                "could not remove a collected thread's exec shim directory: {error}",
            );
        }

        let state = match reason {
            ThreadEndReason::Expired => ThreadState::Expired,
            _requested => ThreadState::Destroyed,
        };
        let thread = self.store.collect_thread(thread_id, state).await?;

        self.bus.publish_control(control_event(
            "thread.destroyed",
            Payload::ThreadDestroyed(ThreadDestroyed {
                thread_id: thread_id.to_owned(),
                reason: reason.into(),
            }),
        ));

        Ok(thread)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CollectError {
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),

    #[error(transparent)]
    Store(#[from] StoreError),
}
