//! An in-memory [`RevisionControl`] following the control plane's SQL rules.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use object_store::ObjectStore;
use object_store::memory::InMemory;
use tokio::sync::{Mutex as FenceLock, OwnedMutexGuard};
use yg_control::{ShardOperationFence, ShardRecord, ShardState, SupersededShard};

use super::contract::Harness;
use super::{RevisionControl, RevisionLocks};

const REPO: i64 = 11;

pub struct MemoryFence(OwnedMutexGuard<()>);

impl ShardOperationFence for MemoryFence {
    async fn release(self) {
        drop(self.0);
    }
}

pub struct MemoryJob {
    commit: String,
    token: u64,
}

#[derive(Clone)]
struct Row {
    id: i64,
    revision: String,
    commit: String,
    state: ShardState,
}

#[derive(Default)]
struct State {
    rows: Vec<Row>,
    current: Option<i64>,
    synced: Option<String>,
    token: u64,
    next_id: i64,
}

impl State {
    fn row(&self, revision: &str) -> Option<&Row> {
        self.rows.iter().find(|row| row.revision == revision)
    }

    fn holds_lease(&self, job: &MemoryJob) -> bool {
        self.token == job.token
    }

    /// Settling clears the lease, so the same job can never settle twice.
    fn settle_lease(&mut self, job: &MemoryJob) -> bool {
        let held = self.holds_lease(job);
        if held {
            self.token += 1;
        }
        held
    }
}

#[derive(Default)]
pub struct MemoryControl {
    state: Mutex<State>,
    fences: Mutex<HashMap<String, Arc<FenceLock<()>>>>,
}

impl MemoryControl {
    fn fence(&self, revision: &str) -> Arc<FenceLock<()>> {
        self.fences
            .lock()
            .unwrap()
            .entry(revision.to_string())
            .or_default()
            .clone()
    }
}

impl RevisionLocks for MemoryControl {
    type Fence = MemoryFence;

    async fn try_lock_revision(
        &self,
        _repo_id: i64,
        revision: &str,
    ) -> anyhow::Result<Option<MemoryFence>> {
        Ok(self.fence(revision).try_lock_owned().ok().map(MemoryFence))
    }

    async fn revision_state(
        &self,
        _repo_id: i64,
        revision: &str,
    ) -> anyhow::Result<Option<ShardState>> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .row(revision)
            .map(|row| row.state))
    }
}

impl RevisionControl for MemoryControl {
    type Job = MemoryJob;

    async fn lock_revision(&self, _repo_id: i64, revision: &str) -> anyhow::Result<MemoryFence> {
        Ok(MemoryFence(self.fence(revision).lock_owned().await))
    }

    async fn begin_reclaiming(&self, shard_id: i64) -> anyhow::Result<bool> {
        let mut state = self.state.lock().unwrap();
        if state.current == Some(shard_id) {
            return Ok(false);
        }
        let Some(row) = state.rows.iter_mut().find(|row| row.id == shard_id) else {
            return Ok(false);
        };
        row.state = ShardState::Reclaiming;
        Ok(true)
    }

    async fn finish_reclaiming(&self, shard_id: i64) -> anyhow::Result<bool> {
        let mut state = self.state.lock().unwrap();
        if state.current == Some(shard_id) {
            return Ok(false);
        }
        let before = state.rows.len();
        state
            .rows
            .retain(|row| row.id != shard_id || row.state != ShardState::Reclaiming);
        Ok(state.rows.len() < before)
    }

    async fn complete_publication(
        &self,
        job: &MemoryJob,
        shard: ShardRecord<'_>,
    ) -> anyhow::Result<bool> {
        let mut state = self.state.lock().unwrap();
        let reclaiming = state
            .row(shard.revision)
            .is_some_and(|row| row.state == ShardState::Reclaiming);
        if reclaiming {
            state.settle_lease(job);
            return Ok(false);
        }
        if !state.settle_lease(job) {
            return Ok(false);
        }
        let shard_id = match state.row(shard.revision) {
            Some(row) => row.id,
            None => {
                state.next_id += 1;
                let id = state.next_id;
                state.rows.push(Row {
                    id,
                    revision: shard.revision.to_string(),
                    commit: shard.commit_sha.to_string(),
                    state: ShardState::Published,
                });
                id
            }
        };
        let up_to_date = state.synced.as_deref() == Some(job.commit.as_str());
        let exactly_current = state.current.is_some_and(|current| {
            state
                .rows
                .iter()
                .any(|row| row.id == current && Some(&row.commit) == state.synced.as_ref())
        });
        if up_to_date || !exactly_current {
            state.current = Some(shard_id);
        }
        Ok(true)
    }

    async fn defer_publication(&self, job: &MemoryJob) -> anyhow::Result<bool> {
        Ok(self.state.lock().unwrap().settle_lease(job))
    }
}

pub struct MemoryHarness {
    control: MemoryControl,
    store: InMemory,
}

impl MemoryHarness {
    pub fn new() -> Self {
        Self {
            control: MemoryControl::default(),
            store: InMemory::new(),
        }
    }
}

impl Harness for MemoryHarness {
    type Control = MemoryControl;

    fn control(&self) -> &MemoryControl {
        &self.control
    }

    fn store(&self) -> &dyn ObjectStore {
        &self.store
    }

    fn repo_id(&self) -> i64 {
        REPO
    }

    async fn claim_index(&self, commit: &str) -> MemoryJob {
        let mut state = self.control.state.lock().unwrap();
        state.synced = Some(commit.to_string());
        state.token += 1;
        MemoryJob {
            commit: commit.to_string(),
            token: state.token,
        }
    }

    async fn claim_index_then_lose_lease(&self, commit: &str) -> MemoryJob {
        let job = self.claim_index(commit).await;
        self.control.state.lock().unwrap().token += 1;
        job
    }

    async fn current_revision(&self) -> Option<String> {
        let state = self.control.state.lock().unwrap();
        let current = state.current?;
        state
            .rows
            .iter()
            .find(|row| row.id == current)
            .map(|row| row.revision.clone())
    }

    async fn reclaimable(&self) -> Vec<SupersededShard> {
        let state = self.control.state.lock().unwrap();
        state
            .rows
            .iter()
            .filter(|row| state.current != Some(row.id))
            .map(|row| SupersededShard {
                shard_id: row.id,
                repo_id: REPO,
                revision: row.revision.clone(),
                state: row.state,
            })
            .collect()
    }
}
