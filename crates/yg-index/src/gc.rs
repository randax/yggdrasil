//! Superseded-shard and terminal-job garbage-collection implementation.

use std::time::Duration;

use anyhow::Context;

use crate::worker::IndexWorker;

pub(crate) async fn collect_superseded(
    worker: &IndexWorker,
    grace: Duration,
) -> anyhow::Result<u64> {
    let stale = worker.control.superseded_shards_past_grace(grace).await?;
    let mut collected = 0;
    for shard in &stale {
        match collect_shard(worker, shard).await {
            Ok(true) => collected += 1,
            Ok(false) => {}
            Err(error) => tracing::warn!(
                repo_id = shard.repo_id,
                revision = %shard.revision,
                error = format!("{error:#}"),
                "could not delete object-storage segments for superseded Shard; these segments are now orphaned"
            ),
        }
    }
    if collected > 0 {
        tracing::info!(shards = collected, "garbage-collected superseded Shards");
    }
    Ok(collected)
}

pub(crate) async fn retire_terminal_jobs(
    worker: &IndexWorker,
    retention: Duration,
) -> anyhow::Result<u64> {
    let deleted = worker
        .control
        .delete_terminal_jobs_past_retention(retention)
        .await?;
    if deleted > 0 {
        tracing::info!(jobs = deleted, "removed terminal jobs past retention");
    }
    Ok(deleted)
}

async fn collect_shard(
    worker: &IndexWorker,
    shard: &yg_control::SupersededShard,
) -> anyhow::Result<bool> {
    if !worker
        .control
        .delete_superseded_shard(shard.shard_id)
        .await?
    {
        return Ok(false);
    }
    yg_shard::delete_shard(worker.store.as_ref(), shard.repo_id, &shard.revision)
        .await
        .context("deleting the Shard's object-storage segments")?;
    Ok(true)
}
