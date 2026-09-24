// Copyright © 2026 Jalapeno Labs

//! The satellite's setup script, as one row.
//!
//! Reading and writing only. When the script runs, what replaces what, and who
//! is told lives in [`crate::setup`], which is the only writer. The one rule that
//! lives here instead is the gate: `claim_next_turn` refuses work while this row
//! says `RUNNING`, so the row is not only a record of the run but the thing that
//! holds the queue.

use super::{Store, StoreError, from_nanos, to_nanos};
use arsox_sdk::proto::common::v1::Timestamp;
use arsox_sdk::proto::satellite::v1::{SetupState, SetupStatus};
use sqlx::Row as _;

/// The script the satellite holds, with the status of its last run.
#[derive(Debug, Clone)]
pub struct StoredSetup {
    pub script: String,
    pub status: SetupStatus,
}

/// How a run of the script ended.
#[derive(Debug, Clone)]
pub struct SetupOutcome {
    /// `SUCCEEDED` or `FAILED`. A run never ends `RUNNING` or `NONE`.
    pub state: SetupState,

    /// Absent when the script never exited on its own.
    pub exit_code: Option<i32>,

    pub output_tail: String,
}

impl Store {
    /// The script the satellite holds, or `None` when no script is set.
    ///
    /// # Errors
    ///
    /// Returns a database error if the read fails.
    pub async fn setup(&self) -> Result<Option<StoredSetup>, StoreError> {
        let row = sqlx::query(
            "SELECT script, script_sha256, state, exit_code, output_tail, started_at, finished_at
               FROM setup
              WHERE id = 1",
        )
        .fetch_optional(self.pool())
        .await?;

        Ok(row.map(|row| StoredSetup {
            script: row.get("script"),
            status: SetupStatus {
                state: row.get("state"),
                script_sha256: row.get("script_sha256"),
                exit_code: row.get("exit_code"),
                output_tail: row.get("output_tail"),
                started_at: Some(from_nanos(row.get("started_at"))),
                finished_at: row.get::<Option<i64>, _>("finished_at").map(from_nanos),
            },
        }))
    }

    /// Stores a script as running, replacing whatever was held before.
    ///
    /// Everything the previous run left behind is cleared in the same write, so
    /// a status read between this and the run's end can never pair the new
    /// script with the old run's exit code.
    ///
    /// # Errors
    ///
    /// Returns a database error if the write fails.
    pub async fn begin_setup(
        &self,
        script: &str,
        script_sha256: &str,
    ) -> Result<SetupStatus, StoreError> {
        let started_at = Timestamp::now();

        sqlx::query(
            "INSERT INTO setup (id, script, script_sha256, state, exit_code, output_tail, started_at, finished_at)
             VALUES (1, ?, ?, ?, NULL, '', ?, NULL)
             ON CONFLICT (id) DO UPDATE SET
                 script = excluded.script,
                 script_sha256 = excluded.script_sha256,
                 state = excluded.state,
                 exit_code = NULL,
                 output_tail = '',
                 started_at = excluded.started_at,
                 finished_at = NULL",
        )
        .bind(script)
        .bind(script_sha256)
        .bind(i32::from(SetupState::Running))
        .bind(to_nanos(&started_at))
        .execute(self.pool())
        .await?;

        Ok(SetupStatus {
            state: SetupState::Running.into(),
            script_sha256: script_sha256.to_owned(),
            exit_code: None,
            output_tail: String::new(),
            started_at: Some(started_at),
            finished_at: None,
        })
    }

    /// Records how a run ended, which releases the claim gate.
    ///
    /// # Errors
    ///
    /// Returns a database error if the write fails.
    pub async fn finish_setup(&self, outcome: &SetupOutcome) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE setup
                SET state = ?, exit_code = ?, output_tail = ?, finished_at = ?
              WHERE id = 1",
        )
        .bind(i32::from(outcome.state))
        .bind(outcome.exit_code)
        .bind(&outcome.output_tail)
        .bind(to_nanos(&Timestamp::now()))
        .execute(self.pool())
        .await?;

        Ok(())
    }

    /// Forgets the script, which also releases the claim gate.
    ///
    /// # Errors
    ///
    /// Returns a database error if the write fails.
    pub async fn clear_setup(&self) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM setup WHERE id = 1")
            .execute(self.pool())
            .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NewThread, NewTurn};
    use arsox_sdk::proto::settings::v1::ThreadSettings;
    use std::collections::BTreeMap;

    async fn store() -> Store {
        Store::open_in_memory().await.expect("should open")
    }

    /// A thread with one turn waiting in its queue.
    async fn queued_turn(store: &Store) -> String {
        let thread = store
            .create_thread(NewThread {
                settings: ThreadSettings::default(),
                metadata: BTreeMap::new(),
                idempotency_key: None,
            })
            .await
            .expect("should create")
            .thread;

        store
            .create_turn(NewTurn {
                thread_id: thread.thread_id,
                prompt: "work".to_owned(),
                metadata: BTreeMap::new(),
                idempotency_key: None,
                overrides: None,
                satellite_initiated: false,
                triggered_by_turn_id: None,
            })
            .await
            .expect("should queue")
            .turn
            .turn_id
    }

    #[tokio::test]
    async fn a_satellite_with_no_script_holds_no_row() {
        assert!(store().await.setup().await.expect("should read").is_none());
    }

    #[tokio::test]
    async fn a_running_script_holds_every_queue_until_it_finishes() {
        // The gate is the claim query itself, so it holds for whatever does the
        // claiming rather than for a runner that remembered to ask.
        let store = store().await;
        let turn_id = queued_turn(&store).await;

        store
            .begin_setup("apt-get install -y jq", "hash")
            .await
            .expect("should begin");

        assert!(
            store
                .claim_next_turn()
                .await
                .expect("should query")
                .is_none(),
            "a turn was claimed while the setup script ran"
        );

        store
            .finish_setup(&SetupOutcome {
                state: SetupState::Succeeded,
                exit_code: Some(0),
                output_tail: String::new(),
            })
            .await
            .expect("should finish");

        let claimed = store
            .claim_next_turn()
            .await
            .expect("should query")
            .expect("the finished script should release the queue");
        assert_eq!(claimed.turn.turn_id, turn_id);
    }

    #[tokio::test]
    async fn a_failed_script_does_not_hold_the_queue() {
        // Failure is degraded, not fatal: work proceeds without the tooling and
        // the incident says what is missing.
        let store = store().await;
        queued_turn(&store).await;

        store
            .begin_setup("exit 1", "hash")
            .await
            .expect("should begin");
        store
            .finish_setup(&SetupOutcome {
                state: SetupState::Failed,
                exit_code: Some(1),
                output_tail: "no such package".to_owned(),
            })
            .await
            .expect("should finish");

        assert!(
            store
                .claim_next_turn()
                .await
                .expect("should query")
                .is_some()
        );
    }

    #[tokio::test]
    async fn clearing_a_running_script_releases_the_queue() {
        let store = store().await;
        queued_turn(&store).await;

        store
            .begin_setup("sleep 600", "hash")
            .await
            .expect("should begin");
        store.clear_setup().await.expect("should clear");

        assert!(store.setup().await.expect("should read").is_none());
        assert!(
            store
                .claim_next_turn()
                .await
                .expect("should query")
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_replaced_script_carries_nothing_from_the_run_before_it() {
        // A status read between the replacement and its run's end must never
        // pair the new script with the old run's exit code and output.
        let store = store().await;

        store
            .begin_setup("exit 3", "first")
            .await
            .expect("should begin");
        store
            .finish_setup(&SetupOutcome {
                state: SetupState::Failed,
                exit_code: Some(3),
                output_tail: "the old failure".to_owned(),
            })
            .await
            .expect("should finish");

        store
            .begin_setup("exit 0", "second")
            .await
            .expect("should begin");

        let stored = store
            .setup()
            .await
            .expect("should read")
            .expect("a script is set");

        assert_eq!(stored.script, "exit 0");
        assert_eq!(stored.status.script_sha256, "second");
        assert_eq!(stored.status.state(), SetupState::Running);
        assert_eq!(stored.status.exit_code, None);
        assert!(stored.status.output_tail.is_empty());
        assert!(stored.status.finished_at.is_none());
        assert!(stored.status.started_at.is_some());
    }
}
