//! Reconciliation for Shard objects whose control-plane row was lost.

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use anyhow::Context;
use futures::TryStreamExt;
use futures::stream::BoxStream;
use object_store::ObjectStore;
use object_store::path::Path;
use yg_control::ShardOperationFence;

/// Object age required before a rowless revision may be reclaimed. This is
/// deliberately much longer than the publication critical section so object
/// storage remains the primary fence even if database locking is unavailable
/// to a publisher that crashed.
pub(crate) const ORPHAN_RECONCILE_GRACE: Duration = Duration::from_secs(6 * 60 * 60);

/// Maximum number of orphan prefixes deleted by one sweep.
pub(crate) const ORPHAN_RECLAIM_CAP: u64 = 100;

const SHARDS_ROOT: &str = "shards";
const REVISION_DEDUP_CAP: usize = 4_096;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ReconcileReport {
    pub(crate) reclaimed: u64,
    pub(crate) cap_reached: bool,
}

trait OrphanControl {
    type Fence: ShardOperationFence;

    async fn try_lock_revision(
        &self,
        repo_id: i64,
        revision: &str,
    ) -> anyhow::Result<Option<Self::Fence>>;

    async fn shard_exists(&self, repo_id: i64, revision: &str) -> anyhow::Result<bool>;
}

impl OrphanControl for yg_control::ControlPlane {
    type Fence = yg_control::ShardOperationGuard;

    async fn try_lock_revision(
        &self,
        repo_id: i64,
        revision: &str,
    ) -> anyhow::Result<Option<Self::Fence>> {
        self.try_lock_shard_operation(repo_id, revision).await
    }

    async fn shard_exists(&self, repo_id: i64, revision: &str) -> anyhow::Result<bool> {
        Ok(self.shard_state(repo_id, revision).await?.is_some())
    }
}

trait ReconcileEvents {
    fn reclaimed(&self, repo_id: i64, revision: &str, prefix: &Path);
    fn cap_reached(&self, cap: u64);
}

struct TracingEvents;

impl ReconcileEvents for TracingEvents {
    fn reclaimed(&self, repo_id: i64, revision: &str, prefix: &Path) {
        tracing::info!(
            repo_id,
            revision,
            prefix = %prefix,
            "reclaimed orphaned Shard object prefix"
        );
    }

    fn cap_reached(&self, cap: u64) {
        tracing::warn!(
            reclaim_cap = cap,
            "orphaned Shard reconciliation hit its per-sweep reclaim cap; additional prefixes will be retried"
        );
    }
}

pub(crate) async fn reconcile_orphans(
    control: &yg_control::ControlPlane,
    store: &dyn ObjectStore,
) -> anyhow::Result<ReconcileReport> {
    reconcile_orphans_at(
        control,
        store,
        SystemTime::now(),
        ORPHAN_RECONCILE_GRACE,
        ORPHAN_RECLAIM_CAP,
        &TracingEvents,
    )
    .await
}

async fn reconcile_orphans_at<C: OrphanControl, E: ReconcileEvents>(
    control: &C,
    store: &dyn ObjectStore,
    now: SystemTime,
    grace: Duration,
    reclaim_cap: u64,
    events: &E,
) -> anyhow::Result<ReconcileReport> {
    let root = Path::from(SHARDS_ROOT);
    reconcile_listed_objects(
        control,
        store,
        store.list(Some(&root)),
        now,
        grace,
        reclaim_cap,
        events,
    )
    .await
}

async fn reconcile_listed_objects<C: OrphanControl, E: ReconcileEvents>(
    control: &C,
    store: &dyn ObjectStore,
    mut objects: BoxStream<'static, object_store::Result<object_store::ObjectMeta>>,
    now: SystemTime,
    grace: Duration,
    reclaim_cap: u64,
    events: &E,
) -> anyhow::Result<ReconcileReport> {
    let mut reclaimed = HashSet::new();
    let mut examined = HashSet::new();

    while let Some(object) = objects.try_next().await? {
        let Some(revision) = ShardRevision::from_location(&object.location) else {
            continue;
        };
        if reclaimed.contains(&revision) {
            continue;
        }
        // Listings are unordered, so a full-sweep dedup set would grow with
        // the bucket. A bounded window removes the normal three-object
        // duplication without surrendering bounded memory.
        if examined.len() == REVISION_DEDUP_CAP {
            examined.clear();
        }
        if !examined.insert(revision.clone()) {
            continue;
        }
        let outcome = match reconcile_candidate(control, store, &revision, now, grace).await {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::warn!(
                    repo_id = revision.repo_id,
                    revision = %revision.revision,
                    prefix = %revision.prefix(),
                    error = format!("{error:#}"),
                    "could not reconcile orphaned Shard candidate; a later sweep will retry"
                );
                continue;
            }
        };

        if outcome {
            let prefix = revision.prefix();
            events.reclaimed(revision.repo_id, &revision.revision, &prefix);
            reclaimed.insert(revision);
            if reclaimed.len() as u64 == reclaim_cap {
                events.cap_reached(reclaim_cap);
                return Ok(ReconcileReport {
                    reclaimed: reclaim_cap,
                    cap_reached: true,
                });
            }
        }
    }

    Ok(ReconcileReport {
        reclaimed: reclaimed.len() as u64,
        cap_reached: false,
    })
}

async fn reconcile_candidate<C: OrphanControl>(
    control: &C,
    store: &dyn ObjectStore,
    revision: &ShardRevision,
    now: SystemTime,
    grace: Duration,
) -> anyhow::Result<bool> {
    let prefix = revision.prefix();
    if !prefix_is_past_grace(store, &prefix, now, grace).await? {
        return Ok(false);
    }

    let Some(operation) = control
        .try_lock_revision(revision.repo_id, &revision.revision)
        .await
        .with_context(|| format!("trying to lock orphan candidate {prefix}"))?
    else {
        return Ok(false);
    };

    yg_control::finish_shard_operation(operation, async {
        // Re-list under the lock: the preliminary age check is only an
        // optimization, and a publisher may have written another object
        // before its operation lock became visible to this sweep.
        if !prefix_is_past_grace(store, &prefix, now, grace).await? {
            return Ok(false);
        }
        if control
            .shard_exists(revision.repo_id, &revision.revision)
            .await?
        {
            return Ok(false);
        }
        yg_shard::delete_shard(store, revision.repo_id, &revision.revision)
            .await
            .with_context(|| format!("deleting orphaned Shard prefix {prefix}"))?;
        Ok(true)
    })
    .await
}

async fn prefix_is_past_grace(
    store: &dyn ObjectStore,
    prefix: &Path,
    now: SystemTime,
    grace: Duration,
) -> anyhow::Result<bool> {
    let newest = store
        .list(Some(prefix))
        .map_ok(|object| SystemTime::from(object.last_modified))
        .try_fold(None, |newest, modified| async move {
            Ok(Some(newest.map_or(modified, |current: SystemTime| {
                current.max(modified)
            })))
        })
        .await?;
    Ok(newest.is_some_and(|modified| now.duration_since(modified).is_ok_and(|age| age >= grace)))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ShardRevision {
    repo_id: i64,
    revision: String,
}

impl ShardRevision {
    fn from_location(location: &Path) -> Option<Self> {
        let mut parts = location.as_ref().split('/');
        if parts.next()? != SHARDS_ROOT {
            return None;
        }
        let repo_id = parts.next()?.parse().ok()?;
        let revision = parts.next()?;
        if revision.is_empty() || parts.next().is_none() {
            return None;
        }
        Some(Self {
            repo_id,
            revision: revision.to_owned(),
        })
    }

    fn prefix(&self) -> Path {
        Path::from(format!("{SHARDS_ROOT}/{}/{}/", self.repo_id, self.revision))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use futures::StreamExt;
    use object_store::ObjectStoreExt;
    use object_store::memory::InMemory;

    use super::*;

    struct TestFence;

    impl ShardOperationFence for TestFence {
        async fn release(self) {}
    }

    struct TestControl {
        lock_available: AtomicBool,
        shard_exists: bool,
        lock_attempts: AtomicU64,
    }

    impl TestControl {
        fn orphan() -> Self {
            Self {
                lock_available: AtomicBool::new(true),
                shard_exists: false,
                lock_attempts: AtomicU64::new(0),
            }
        }
    }

    impl OrphanControl for TestControl {
        type Fence = TestFence;

        async fn try_lock_revision(
            &self,
            _repo_id: i64,
            _revision: &str,
        ) -> anyhow::Result<Option<Self::Fence>> {
            self.lock_attempts.fetch_add(1, Ordering::Relaxed);
            Ok(self
                .lock_available
                .load(Ordering::Relaxed)
                .then_some(TestFence))
        }

        async fn shard_exists(&self, _repo_id: i64, _revision: &str) -> anyhow::Result<bool> {
            Ok(self.shard_exists)
        }
    }

    #[derive(Default)]
    struct TestEvents {
        reclaimed: Mutex<Vec<(i64, String, String)>>,
        cap_warnings: AtomicU64,
    }

    impl ReconcileEvents for TestEvents {
        fn reclaimed(&self, repo_id: i64, revision: &str, prefix: &Path) {
            self.reclaimed
                .lock()
                .unwrap()
                .push((repo_id, revision.to_owned(), prefix.to_string()));
        }

        fn cap_reached(&self, _cap: u64) {
            self.cap_warnings.fetch_add(1, Ordering::Relaxed);
        }
    }

    type CapturedEvent = (tracing::Level, Vec<&'static str>);
    type CapturedEvents = Arc<Mutex<Vec<CapturedEvent>>>;

    #[derive(Clone, Default)]
    struct CapturingSubscriber(CapturedEvents);

    impl tracing::Subscriber for CapturingSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let metadata = event.metadata();
            let fields = metadata.fields().iter().map(|field| field.name()).collect();
            self.0.lock().unwrap().push((*metadata.level(), fields));
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    #[test]
    fn production_events_log_reclaims_and_cap_warning() {
        let subscriber = CapturingSubscriber::default();
        let captured = subscriber.0.clone();
        tracing::subscriber::with_default(subscriber, || {
            TracingEvents.reclaimed(21, "orphan", &Path::from("shards/21/orphan/"));
            TracingEvents.cap_reached(2);
        });

        let events = captured.lock().unwrap();
        assert!(events.iter().any(|(level, fields)| {
            *level == tracing::Level::INFO
                && ["repo_id", "revision", "prefix"]
                    .iter()
                    .all(|required| fields.contains(required))
        }));
        assert!(events.iter().any(|(level, fields)| {
            *level == tracing::Level::WARN && fields.contains(&"reclaim_cap")
        }));
    }

    async fn put_revision(store: &InMemory, repo_id: i64, revision: &str) {
        for file in ["graph.sqlite", "fts.tar", "manifest.json"] {
            store
                .put(
                    &Path::from(format!("shards/{repo_id}/{revision}/{file}")),
                    "segment".to_owned().into(),
                )
                .await
                .unwrap();
        }
    }

    async fn revision_exists(store: &InMemory, repo_id: i64, revision: &str) -> bool {
        store
            .list(Some(&Path::from(format!("shards/{repo_id}/{revision}/"))))
            .try_next()
            .await
            .unwrap()
            .is_some()
    }

    #[tokio::test]
    async fn old_rowless_prefix_is_reclaimed() {
        let store = InMemory::new();
        put_revision(&store, 7, "old").await;
        let events = TestEvents::default();

        let report = reconcile_orphans_at(
            &TestControl::orphan(),
            &store,
            SystemTime::now() + Duration::from_secs(7 * 60 * 60),
            ORPHAN_RECONCILE_GRACE,
            ORPHAN_RECLAIM_CAP,
            &events,
        )
        .await
        .unwrap();

        assert_eq!(report.reclaimed, 1);
        assert!(!revision_exists(&store, 7, "old").await);
        assert_eq!(events.reclaimed.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn fresh_prefix_is_not_reclaimed() {
        let store = InMemory::new();
        put_revision(&store, 8, "publishing").await;
        let control = TestControl::orphan();

        let report = reconcile_orphans_at(
            &control,
            &store,
            SystemTime::now(),
            ORPHAN_RECONCILE_GRACE,
            ORPHAN_RECLAIM_CAP,
            &TestEvents::default(),
        )
        .await
        .unwrap();

        assert_eq!(report.reclaimed, 0);
        assert_eq!(control.lock_attempts.load(Ordering::Relaxed), 0);
        assert!(revision_exists(&store, 8, "publishing").await);
    }

    #[tokio::test]
    async fn newest_object_anchors_the_prefix_grace_window() {
        let store = InMemory::new();
        let old_path = Path::from("shards/8/staggered/graph.sqlite");
        let fresh_path = Path::from("shards/8/staggered/manifest.json");
        store
            .put(&old_path, "old segment".to_owned().into())
            .await
            .unwrap();
        let old_modified = SystemTime::from(store.head(&old_path).await.unwrap().last_modified);
        std::thread::sleep(Duration::from_millis(2));
        store
            .put(&fresh_path, "fresh manifest".to_owned().into())
            .await
            .unwrap();
        let fresh_modified = SystemTime::from(store.head(&fresh_path).await.unwrap().last_modified);
        let write_gap = fresh_modified.duration_since(old_modified).unwrap();

        let report = reconcile_orphans_at(
            &TestControl::orphan(),
            &store,
            fresh_modified + write_gap / 2,
            write_gap,
            ORPHAN_RECLAIM_CAP,
            &TestEvents::default(),
        )
        .await
        .unwrap();

        assert_eq!(report.reclaimed, 0);
        assert!(revision_exists(&store, 8, "staggered").await);
    }

    #[tokio::test]
    async fn held_operation_lock_skips_old_prefix() {
        let store = InMemory::new();
        put_revision(&store, 9, "locked").await;
        let control = TestControl::orphan();
        control.lock_available.store(false, Ordering::Relaxed);

        let report = reconcile_orphans_at(
            &control,
            &store,
            SystemTime::now(),
            Duration::ZERO,
            ORPHAN_RECLAIM_CAP,
            &TestEvents::default(),
        )
        .await
        .unwrap();

        assert_eq!(report.reclaimed, 0);
        assert!(control.lock_attempts.load(Ordering::Relaxed) > 0);
        assert!(revision_exists(&store, 9, "locked").await);
    }

    #[tokio::test]
    async fn matching_control_plane_row_skips_old_prefix_under_lock() {
        let store = InMemory::new();
        put_revision(&store, 10, "recorded").await;
        let control = TestControl {
            lock_available: AtomicBool::new(true),
            shard_exists: true,
            lock_attempts: AtomicU64::new(0),
        };

        let report = reconcile_orphans_at(
            &control,
            &store,
            SystemTime::now(),
            Duration::ZERO,
            ORPHAN_RECLAIM_CAP,
            &TestEvents::default(),
        )
        .await
        .unwrap();

        assert_eq!(report.reclaimed, 0);
        assert!(control.lock_attempts.load(Ordering::Relaxed) > 0);
        assert!(revision_exists(&store, 10, "recorded").await);
    }

    #[tokio::test]
    async fn listing_stream_is_consumed_beyond_its_first_page() {
        let store = InMemory::new();
        let page_one_path = Path::from("shards/not-a-repo/ignored/object");
        store
            .put(&page_one_path, "ignored".to_owned().into())
            .await
            .unwrap();
        put_revision(&store, 11, "second-page").await;
        let page_two_path = Path::from("shards/11/second-page/manifest.json");
        let first_page = futures::stream::iter([Ok(store.head(&page_one_path).await.unwrap())]);
        let second_page = futures::stream::iter([Ok(store.head(&page_two_path).await.unwrap())]);
        let paged_listing = first_page.chain(second_page).boxed();

        let report = reconcile_listed_objects(
            &TestControl::orphan(),
            &store,
            paged_listing,
            SystemTime::now(),
            Duration::ZERO,
            ORPHAN_RECLAIM_CAP,
            &TestEvents::default(),
        )
        .await
        .unwrap();

        assert_eq!(report.reclaimed, 1);
        assert!(!revision_exists(&store, 11, "second-page").await);
    }

    /// A publisher that finishes writing an object in the window between the
    /// sweep's preliminary grace check and its lock acquisition.
    struct FreshWriteControl {
        store: Arc<InMemory>,
    }

    impl OrphanControl for FreshWriteControl {
        type Fence = TestFence;

        async fn try_lock_revision(
            &self,
            repo_id: i64,
            revision: &str,
        ) -> anyhow::Result<Option<Self::Fence>> {
            self.store
                .put(
                    &Path::from(format!("shards/{repo_id}/{revision}/manifest.json")),
                    "late publish".to_owned().into(),
                )
                .await?;
            Ok(Some(TestFence))
        }

        async fn shard_exists(&self, _repo_id: i64, _revision: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
    }

    #[tokio::test]
    async fn object_written_before_lock_acquisition_aborts_the_reclaim() {
        let store = Arc::new(InMemory::new());
        put_revision(&store, 14, "racing").await;
        let old_modified = SystemTime::from(
            store
                .head(&Path::from("shards/14/racing/manifest.json"))
                .await
                .unwrap()
                .last_modified,
        );
        // Distinct object-store timestamps for the pre-existing objects and
        // the racing write issued inside `try_lock_revision`.
        std::thread::sleep(Duration::from_millis(2));
        let control = FreshWriteControl {
            store: store.clone(),
        };
        let events = TestEvents::default();

        // The preliminary grace check passes exactly (age == grace), but the
        // fresh object written before the lock was granted must make the
        // under-lock re-list veto the deletion.
        let report = reconcile_orphans_at(
            &control,
            store.as_ref(),
            old_modified + ORPHAN_RECONCILE_GRACE,
            ORPHAN_RECONCILE_GRACE,
            ORPHAN_RECLAIM_CAP,
            &events,
        )
        .await
        .unwrap();

        assert_eq!(report.reclaimed, 0);
        assert!(revision_exists(&store, 14, "racing").await);
        assert!(events.reclaimed.lock().unwrap().is_empty());
    }

    struct PoisonFirstControl;

    impl OrphanControl for PoisonFirstControl {
        type Fence = TestFence;

        async fn try_lock_revision(
            &self,
            repo_id: i64,
            _revision: &str,
        ) -> anyhow::Result<Option<Self::Fence>> {
            if repo_id == 12 {
                anyhow::bail!("persistent lock failure")
            }
            Ok(Some(TestFence))
        }

        async fn shard_exists(&self, _repo_id: i64, _revision: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
    }

    #[tokio::test]
    async fn one_poison_prefix_does_not_starve_later_orphans() {
        let store = InMemory::new();
        put_revision(&store, 12, "poison").await;
        put_revision(&store, 13, "healthy").await;
        let poison = store
            .head(&Path::from("shards/12/poison/manifest.json"))
            .await
            .unwrap();
        let healthy = store
            .head(&Path::from("shards/13/healthy/manifest.json"))
            .await
            .unwrap();
        let listing = futures::stream::iter([Ok(poison), Ok(healthy)]).boxed();

        let report = reconcile_listed_objects(
            &PoisonFirstControl,
            &store,
            listing,
            SystemTime::now(),
            Duration::ZERO,
            ORPHAN_RECLAIM_CAP,
            &TestEvents::default(),
        )
        .await
        .unwrap();

        assert_eq!(report.reclaimed, 1);
        assert!(revision_exists(&store, 12, "poison").await);
        assert!(!revision_exists(&store, 13, "healthy").await);
    }

    #[tokio::test]
    async fn reclaim_cap_is_reported_and_logged() {
        let store = InMemory::new();
        for repo_id in 1..=3 {
            put_revision(&store, repo_id, "orphan").await;
        }
        let events = TestEvents::default();

        let report = reconcile_orphans_at(
            &TestControl::orphan(),
            &store,
            SystemTime::now(),
            Duration::ZERO,
            2,
            &events,
        )
        .await
        .unwrap();

        assert_eq!(
            report,
            ReconcileReport {
                reclaimed: 2,
                cap_reached: true,
            }
        );
        assert_eq!(events.reclaimed.lock().unwrap().len(), 2);
        assert_eq!(events.cap_warnings.load(Ordering::Relaxed), 1);
        let mut remaining = 0;
        for repo_id in 1..=3 {
            if revision_exists(&store, repo_id, "orphan").await {
                remaining += 1;
            }
        }
        assert_eq!(remaining, 1);
    }
}
