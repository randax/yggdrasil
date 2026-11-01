//! The Shard revision lifecycle contract against the real adapter: Postgres
//! and the dev compose object store. The same cases run in-crate against the
//! in-memory adapter, so the two cannot drift apart.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{DEV_POSTGRES, create_test_db, dev_object_store};
use object_store::ObjectStore;
use yg_control::{AddRepo, ControlPlane, LeasedIndex, SupersededShard};
use yg_index::shard_lifecycle::contract::{Harness, verify};

const LEASE: Duration = Duration::from_secs(600);

struct PostgresHarness {
    control: ControlPlane,
    store: Arc<dyn ObjectStore>,
    repo_id: i64,
}

impl PostgresHarness {
    async fn new() -> Self {
        let db_name = create_test_db().await;
        let control = ControlPlane::connect_and_migrate(&format!("{DEV_POSTGRES}/{db_name}"))
            .await
            .unwrap();
        let repo_id = register_repo(&control).await;
        Self {
            control,
            store: dev_object_store(&db_name),
            repo_id,
        }
    }

    async fn sync_to(&self, commit: &str) {
        register_repo(&self.control).await;
        let fetch = self
            .control
            .claim_due_fetch(LEASE)
            .await
            .unwrap()
            .expect("registering queues a fetch");
        assert!(self.control.complete_fetch(&fetch, commit).await.unwrap());
    }
}

async fn register_repo(control: &ControlPlane) -> i64 {
    control
        .add_repo(AddRepo {
            forge_kind: "github",
            base_url: "https://github.com",
            token_env: None,
            api_root: None,
            slug: "acme/widgets",
            fetch_depth: None,
            poll_interval_seconds: None,
        })
        .await
        .unwrap()
        .repo_id
}

impl Harness for PostgresHarness {
    type Control = ControlPlane;

    fn control(&self) -> &ControlPlane {
        &self.control
    }

    fn store(&self) -> &dyn ObjectStore {
        self.store.as_ref()
    }

    fn repo_id(&self) -> i64 {
        self.repo_id
    }

    async fn claim_index(&self, commit: &str) -> LeasedIndex {
        self.sync_to(commit).await;
        self.control
            .claim_due_index(LEASE)
            .await
            .unwrap()
            .expect("a synced commit queues an index job")
    }

    async fn claim_index_then_lose_lease(&self, commit: &str) -> LeasedIndex {
        self.sync_to(commit).await;
        let lapsing = self
            .control
            .claim_due_index(Duration::ZERO)
            .await
            .unwrap()
            .expect("a synced commit queues an index job");
        loop {
            if self.control.claim_due_index(LEASE).await.unwrap().is_some() {
                return lapsing;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn current_revision(&self) -> Option<String> {
        self.control.admin_status().await.unwrap()[0]
            .shard_revision
            .clone()
    }

    async fn reclaimable(&self) -> Vec<SupersededShard> {
        self.control
            .superseded_shards_past_grace(Duration::ZERO)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn the_postgres_adapter_honours_the_lifecycle_contract() {
    verify(PostgresHarness::new).await;
}
