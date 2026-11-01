//! Behaviour every `RevisionControl` adapter must exhibit. Runs against the
//! in-memory adapter in-crate, and against Postgres in the `yg-cli` e2e suite,
//! so the fake cannot drift from the SQL without a test failing.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use object_store::ObjectStore;
use object_store::path::Path;
use yg_control::{ShardOperationFence, ShardState, SupersededShard};
use yg_shard::{Edge, Graph, Node, PreparedShard, SearchDoc};

use super::{
    PublicationSettlement, RevisionControl, RevisionLocks, publish_revision, reclaim_orphan,
    reclaim_superseded, settle_publication,
};

const COMMIT_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const COMMIT_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// One empty repo with an adapter, an object store and a job queue.
pub trait Harness {
    type Control: RevisionControl;

    fn control(&self) -> &Self::Control;
    fn store(&self) -> &dyn ObjectStore;
    fn repo_id(&self) -> i64;

    /// Sync the repo to `commit` and claim the index job that queues.
    fn claim_index(
        &self,
        commit: &str,
    ) -> impl Future<Output = <Self::Control as RevisionControl>::Job>;

    /// As [`Self::claim_index`], but another worker has since reclaimed the job.
    fn claim_index_then_lose_lease(
        &self,
        commit: &str,
    ) -> impl Future<Output = <Self::Control as RevisionControl>::Job>;

    /// The revision the repo's current-Shard pointer holds.
    fn current_revision(&self) -> impl Future<Output = Option<String>>;

    /// What a GC sweep would find with no grace window.
    fn reclaimable(&self) -> impl Future<Output = Vec<SupersededShard>>;
}

/// Run every case, each against a fresh harness.
pub async fn verify<H: Harness>(harness: impl AsyncFn() -> H) {
    publishing_a_new_revision_makes_it_current(&harness().await).await;
    preparation_never_holds_the_revision_lock(&harness().await).await;
    republishing_reuses_the_artifact_without_preparing(&harness().await).await;
    a_superseded_revision_is_reclaimed_and_the_current_one_kept(&harness().await).await;
    a_revision_current_again_is_not_reclaimed(&harness().await).await;
    publishing_a_reclaiming_revision_defers(&harness().await).await;
    an_interrupted_reclamation_resumes(&harness().await).await;
    a_result_after_the_lease_lapsed_is_not_applied(&harness().await).await;
    publishing_during_reclamation_republishes_after_it(&harness().await).await;
    a_settled_job_cannot_settle_again(&harness().await).await;
    a_deferred_job_cannot_settle_again(&harness().await).await;
    only_an_old_rowless_prefix_is_reclaimed_as_an_orphan(&harness().await).await;
}

fn revision(commit: &str) -> String {
    yg_shard::syntactic_revision(commit)
}

async fn prepared(commit: &str) -> anyhow::Result<PreparedShard> {
    let graph = Graph {
        nodes: vec![Node::file(&format!("{commit}.rs"))],
        edges: Vec::<Edge>::new(),
    };
    yg_shard::prepare_shard(graph, Vec::<SearchDoc>::new()).await
}

async fn publish_and_settle<H: Harness>(harness: &H, commit: &str) -> PublicationSettlement {
    let job = harness.claim_index(commit).await;
    let publication = publish_revision(
        harness.control(),
        harness.store(),
        harness.repo_id(),
        commit,
        || prepared(commit),
    )
    .await
    .expect("publish");
    settle_publication(harness.control(), &job, commit, publication)
        .await
        .expect("settle")
}

async fn state<H: Harness>(harness: &H, commit: &str) -> Option<ShardState> {
    harness
        .control()
        .revision_state(harness.repo_id(), &revision(commit))
        .await
        .expect("revision state")
}

async fn artifact_exists<H: Harness>(harness: &H, commit: &str) -> bool {
    yg_shard::published_shard(harness.store(), harness.repo_id(), commit)
        .await
        .expect("published probe")
        .is_some()
}

/// `COMMIT_A` published, then superseded by `COMMIT_B`; returns A's GC record.
async fn supersede_a_with_b<H: Harness>(harness: &H) -> SupersededShard {
    publish_and_settle(harness, COMMIT_A).await;
    publish_and_settle(harness, COMMIT_B).await;
    let mut reclaimable = harness.reclaimable().await;
    assert_eq!(reclaimable.len(), 1, "only A is superseded");
    let shard = reclaimable.remove(0);
    assert_eq!(shard.revision, revision(COMMIT_A));
    shard
}

async fn publishing_a_new_revision_makes_it_current<H: Harness>(harness: &H) {
    let settled = publish_and_settle(harness, COMMIT_A).await;

    assert!(matches!(settled, PublicationSettlement::Applied { .. }));
    assert_eq!(harness.current_revision().await, Some(revision(COMMIT_A)));
    assert_eq!(state(harness, COMMIT_A).await, Some(ShardState::Published));
    assert!(artifact_exists(harness, COMMIT_A).await);
}

async fn preparation_never_holds_the_revision_lock<H: Harness>(harness: &H) {
    let job = harness.claim_index(COMMIT_A).await;
    let publication = publish_revision(
        harness.control(),
        harness.store(),
        harness.repo_id(),
        COMMIT_A,
        || async {
            let fence = harness
                .control()
                .try_lock_revision(harness.repo_id(), &revision(COMMIT_A))
                .await
                .expect("try lock")
                .expect("preparing must not hold the revision lock");
            fence.release().await;
            prepared(COMMIT_A).await
        },
    )
    .await
    .expect("publish");
    settle_publication(harness.control(), &job, COMMIT_A, publication)
        .await
        .expect("settle");
}

async fn republishing_reuses_the_artifact_without_preparing<H: Harness>(harness: &H) {
    publish_and_settle(harness, COMMIT_A).await;

    let job = harness.claim_index(COMMIT_A).await;
    let publication = publish_revision(
        harness.control(),
        harness.store(),
        harness.repo_id(),
        COMMIT_A,
        || async { panic!("a published artifact must not be prepared again") },
    )
    .await
    .expect("publish");
    let settled = settle_publication(harness.control(), &job, COMMIT_A, publication)
        .await
        .expect("settle");

    assert!(matches!(settled, PublicationSettlement::Applied { .. }));
    assert_eq!(harness.current_revision().await, Some(revision(COMMIT_A)));
}

async fn a_superseded_revision_is_reclaimed_and_the_current_one_kept<H: Harness>(harness: &H) {
    let superseded = supersede_a_with_b(harness).await;

    let reclaimed = reclaim_superseded(harness.control(), harness.store(), &superseded)
        .await
        .expect("reclaim");

    assert!(reclaimed);
    assert_eq!(state(harness, COMMIT_A).await, None);
    assert!(!artifact_exists(harness, COMMIT_A).await);
    assert_eq!(harness.current_revision().await, Some(revision(COMMIT_B)));
    assert!(artifact_exists(harness, COMMIT_B).await);
}

async fn a_revision_current_again_is_not_reclaimed<H: Harness>(harness: &H) {
    let stale_record = supersede_a_with_b(harness).await;
    publish_and_settle(harness, COMMIT_A).await;

    let reclaimed = reclaim_superseded(harness.control(), harness.store(), &stale_record)
        .await
        .expect("reclaim");

    assert!(!reclaimed, "a current revision must be left alone");
    assert_eq!(harness.current_revision().await, Some(revision(COMMIT_A)));
    assert_eq!(state(harness, COMMIT_A).await, Some(ShardState::Published));
    assert!(artifact_exists(harness, COMMIT_A).await);
}

async fn publishing_a_reclaiming_revision_defers<H: Harness>(harness: &H) {
    let superseded = supersede_a_with_b(harness).await;
    assert!(
        harness
            .control()
            .begin_reclaiming(superseded.shard_id)
            .await
            .expect("begin reclaiming")
    );

    let job = harness.claim_index(COMMIT_A).await;
    let publication = publish_revision(
        harness.control(),
        harness.store(),
        harness.repo_id(),
        COMMIT_A,
        || prepared(COMMIT_A),
    )
    .await
    .expect("publish");
    let settled = settle_publication(harness.control(), &job, COMMIT_A, publication)
        .await
        .expect("settle");

    assert!(matches!(settled, PublicationSettlement::Deferred));
    assert_eq!(state(harness, COMMIT_A).await, Some(ShardState::Reclaiming));
    assert_eq!(harness.current_revision().await, Some(revision(COMMIT_B)));
}

async fn an_interrupted_reclamation_resumes<H: Harness>(harness: &H) {
    let superseded = supersede_a_with_b(harness).await;
    assert!(
        harness
            .control()
            .begin_reclaiming(superseded.shard_id)
            .await
            .expect("begin reclaiming")
    );

    let mut found = harness.reclaimable().await;
    assert_eq!(found.len(), 1, "the next sweep finds the reclaiming row");
    let resumed = found.remove(0);
    assert_eq!(resumed.state, ShardState::Reclaiming);
    let reclaimed = reclaim_superseded(harness.control(), harness.store(), &resumed)
        .await
        .expect("reclaim");

    assert!(reclaimed);
    assert_eq!(state(harness, COMMIT_A).await, None);
    assert!(!artifact_exists(harness, COMMIT_A).await);
}

async fn a_result_after_the_lease_lapsed_is_not_applied<H: Harness>(harness: &H) {
    let job = harness.claim_index_then_lose_lease(COMMIT_A).await;
    let publication = publish_revision(
        harness.control(),
        harness.store(),
        harness.repo_id(),
        COMMIT_A,
        || prepared(COMMIT_A),
    )
    .await
    .expect("publish");

    let settled = settle_publication(harness.control(), &job, COMMIT_A, publication)
        .await
        .expect("settle");

    assert!(matches!(settled, PublicationSettlement::NotApplied));
    assert_eq!(harness.current_revision().await, None);
}

/// Reclamation holds the fence while a publisher of the same revision runs.
/// Whichever way they interleave, the publisher must not advance the pointer
/// to objects reclamation deleted, so it ends with a complete artifact.
async fn publishing_during_reclamation_republishes_after_it<H: Harness>(harness: &H) {
    let superseded = supersede_a_with_b(harness).await;
    let job = harness.claim_index(COMMIT_A).await;
    let fence = harness
        .control()
        .lock_revision(harness.repo_id(), &revision(COMMIT_A))
        .await
        .expect("lock");

    let reclaim = async {
        assert!(
            harness
                .control()
                .begin_reclaiming(superseded.shard_id)
                .await
                .expect("begin reclaiming")
        );
        yg_shard::delete_shard(harness.store(), harness.repo_id(), &revision(COMMIT_A))
            .await
            .expect("delete objects");
        assert!(
            harness
                .control()
                .finish_reclaiming(superseded.shard_id)
                .await
                .expect("finish reclaiming")
        );
        fence.release().await;
    };
    let prepares = AtomicUsize::new(0);
    let publish = publish_revision(
        harness.control(),
        harness.store(),
        harness.repo_id(),
        COMMIT_A,
        || {
            prepares.fetch_add(1, Ordering::SeqCst);
            prepared(COMMIT_A)
        },
    );
    // Polling the publisher first parks it on the held fence wherever the
    // adapter's earlier steps complete without yielding, as in-memory ones do.
    let (publication, ()) = futures::join!(publish, reclaim);
    let settled = settle_publication(
        harness.control(),
        &job,
        COMMIT_A,
        publication.expect("publish"),
    )
    .await
    .expect("settle");

    assert_eq!(
        prepares.load(Ordering::SeqCst),
        1,
        "reclamation deleted the objects, so they must be built again"
    );
    assert!(matches!(settled, PublicationSettlement::Applied { .. }));
    assert_eq!(harness.current_revision().await, Some(revision(COMMIT_A)));
    assert_eq!(state(harness, COMMIT_A).await, Some(ShardState::Published));
    assert!(artifact_exists(harness, COMMIT_A).await);
}

async fn a_settled_job_cannot_settle_again<H: Harness>(harness: &H) {
    let job = harness.claim_index(COMMIT_A).await;
    for expected_applied in [true, false] {
        let publication = publish_revision(
            harness.control(),
            harness.store(),
            harness.repo_id(),
            COMMIT_A,
            || prepared(COMMIT_A),
        )
        .await
        .expect("publish");
        let settled = settle_publication(harness.control(), &job, COMMIT_A, publication)
            .await
            .expect("settle");
        assert_eq!(
            matches!(settled, PublicationSettlement::Applied { .. }),
            expected_applied,
            "settling consumes the job's lease"
        );
    }
}

async fn a_deferred_job_cannot_settle_again<H: Harness>(harness: &H) {
    let superseded = supersede_a_with_b(harness).await;
    assert!(
        harness
            .control()
            .begin_reclaiming(superseded.shard_id)
            .await
            .expect("begin reclaiming")
    );
    let job = harness.claim_index(COMMIT_A).await;
    for expected in [true, false] {
        let deferred = harness
            .control()
            .defer_publication(&job)
            .await
            .expect("defer");
        assert_eq!(deferred, expected, "deferring consumes the job's lease");
    }
}

async fn only_an_old_rowless_prefix_is_reclaimed_as_an_orphan<H: Harness>(harness: &H) {
    publish_and_settle(harness, COMMIT_A).await;
    let rowless = yg_shard::publish_shard(
        harness.store(),
        harness.repo_id(),
        COMMIT_B,
        prepared(COMMIT_B).await.expect("prepare"),
    )
    .await
    .expect("write rowless objects")
    .revision;
    let later = SystemTime::now() + Duration::from_secs(3600);
    let grace = Duration::from_secs(60);
    let prefix = |revision: &str| Path::from(format!("shards/{}/{revision}/", harness.repo_id()));
    let orphan = async |revision: &str, now, grace| {
        reclaim_orphan(
            harness.control(),
            harness.store(),
            harness.repo_id(),
            revision,
            &prefix(revision),
            now,
            grace,
        )
        .await
        .expect("reclaim orphan")
    };

    assert!(
        !orphan(&revision(COMMIT_A), later, grace).await,
        "a revision with a row is live"
    );
    assert!(artifact_exists(harness, COMMIT_A).await);
    assert!(
        !orphan(&rowless, SystemTime::now(), grace).await,
        "a young prefix may be a publisher's write in progress"
    );
    assert!(artifact_exists(harness, COMMIT_B).await);
    assert!(orphan(&rowless, later, grace).await);
    assert!(!artifact_exists(harness, COMMIT_B).await);
}
