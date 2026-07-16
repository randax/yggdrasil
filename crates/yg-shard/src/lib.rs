//! Shard read/write, cache tier, formats.
//!
//! A Shard is the immutable per-repo index artifact (RFC 0001 §6): a
//! graph segment, a full-text segment, and a manifest, written to object
//! storage under `shards/<repo-id>/<revision>/` and never mutated
//! afterwards.

mod cache;
pub mod metrics;

pub use cache::{CacheCapacity, CacheLease, LeasedPath};
pub use metrics::Metrics;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::Context;
use cache::{CacheKey, DiskLru, Flights, MemoryLru, Pins};
use futures::StreamExt;
use object_store::aws::AmazonS3Builder;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

mod fts;
pub mod graph_schema;
pub use fts::{
    FTS_SEGMENT_FILE, FtsIndex, LocalHit, LocalSymbol, LocalSymbolId, LocalSymbolPath,
    QueryMalformed, SearchDoc, SearchParams, UnaddressableSymbolName, build_fts, open_fts, search,
    snippets_for, symbols_named, unpack_fts,
};

/// Version of the Shard layout (graph tables + manifest shape). Part of
/// every revision id: bumping it re-indexes the world rather than mixing
/// layouts under one revision.
///
/// It also stands in for the pass implementation: a change to what the
/// syntactic pass extracts must bump it, or already-published revisions
/// keep their old artifacts (re-publishing an existing revision is a
/// no-op by design).
///
/// v2: indexes on edges(src) and edges(dst) — the read path's neighbor
/// and summary lookups are index seeks instead of table scans.
/// v3: edges carry a nullable location (`<path>:<line>:<col>`, 1-based)
/// — the call site of a CALLS edge, the import spec of an IMPORTS edge
/// (RFC 0001 §5: "locations where applicable").
/// v4: a Shard carries a full-text segment ([`FTS_SEGMENT_FILE`]) beside
/// the graph — the tantivy index the lexical `search` Verb reads.
/// v5: the git-history layer — Commit and Contributor node kinds, TOUCHES
/// (Commit→File) and AUTHORED (Contributor→Commit) edges, and a nullable
/// `committed_at` on nodes (set on Commits, for the `history` Verb's
/// newest-first ordering and `since` filter).
/// v6: the syntactic grammar pack adds TypeScript, JavaScript, Python,
/// Rust, and Java Symbols plus DEFINES/IMPORTS/CALLS facts.
/// v7: code-aware body and path tokenization, exact raw Symbol names,
/// per-repo normalized search ranking, and graph edge integrity constraints.
///
/// Bumping this changes every revision id (see
/// [`syntactic_revision_suffix`]): readers refuse artifacts from other
/// schema versions ([`SchemaOutdated`]), and worker boot queues a
/// re-index for every repo still pointing at an outdated revision.
pub const SCHEMA_VERSION: u32 = 7;

/// Name of the M0 indexing pass, as recorded in revision ids, manifests,
/// and the control plane's `provenance_level`. The precise pass (M1)
/// adds its own.
pub const SYNTACTIC_PASS: &str = "syntactic";

/// Confidence ceiling for one heuristic syntactic name resolution.
///
/// ADR 0006 defines this value and its N-way spread for syntactic-pass
/// edges. Fuzzy node addressing borrows the same convention.
pub const SYNTACTIC_MATCH: f64 = 0.9;

/// How a graph edge was derived (ADR 0002, CONTEXT.md). Carried by every
/// edge from day one, even while only syntactic values exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provenance {
    /// Compiler-grade indexer (SCIP) — arrives with the M1 precise pass.
    Precise,
    /// Heuristic parsing (tree-sitter).
    Syntactic,
    /// Deterministically derived from non-code sources.
    Extracted,
    /// Heuristic guess, reversible.
    Inferred,
}

impl Provenance {
    /// Every provenance level, in one place, so tests can assert the
    /// control plane's CHECK constraint mirrors exactly this vocabulary.
    pub const ALL: [Provenance; 4] = [
        Provenance::Precise,
        Provenance::Syntactic,
        Provenance::Extracted,
        Provenance::Inferred,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Provenance::Precise => "precise",
            Provenance::Syntactic => "syntactic",
            Provenance::Extracted => "extracted",
            Provenance::Inferred => "inferred",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|item| item.as_str() == value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Symbol,
    /// An imported package, named by import path (RFC 0001 §5) — the
    /// target IMPORTS edges point at whether or not the package's source
    /// is in this repo.
    Package,
    /// A commit on the default branch (RFC 0001 §5), keyed by its sha;
    /// carries the commit subject and committer date. TOUCHES edges fan
    /// out to the Files it changed.
    Commit,
    /// A person appearing in the repo's history (CONTEXT.md), keyed by
    /// email within this repo's Shard — cross-Forge identity merge is M2.
    /// AUTHORED edges point at the Commits they wrote.
    Contributor,
}

/// Compile-time backstop for `NodeKind::ALL`: a new variant makes this
/// match non-exhaustive (build error), forcing whoever adds it to come
/// here — next to `ALL` and its length assert — rather than silently
/// leaving `ALL` short. The assert pins the count so a forgotten `ALL`
/// entry fails the build too. (Set *equality* with the variants — e.g.
/// catching a duplicate that masks a drop — is enforced by the
/// cross-crate drift test in yg-api, which compares against the writer.)
const _: () = {
    fn count(kind: NodeKind) -> usize {
        match kind {
            NodeKind::File => 1,
            NodeKind::Symbol => 1,
            NodeKind::Package => 1,
            NodeKind::Commit => 1,
            NodeKind::Contributor => 1,
        }
    }
    let _ = count;
    assert!(NodeKind::ALL.len() == 5);
};

impl NodeKind {
    /// Every node kind, for exhaustive checks (a cross-crate test holds
    /// the id grammar in yg-verbs to this list; the `const _` block
    /// above pins its length, the yg-api drift test its contents).
    pub const ALL: &'static [NodeKind] = &[
        NodeKind::File,
        NodeKind::Symbol,
        NodeKind::Package,
        NodeKind::Commit,
        NodeKind::Contributor,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::File => "File",
            NodeKind::Symbol => "Symbol",
            NodeKind::Package => "Package",
            NodeKind::Commit => "Commit",
            NodeKind::Contributor => "Contributor",
        }
    }

    /// Parse the wire spelling (the [`Self::as_str`] form) back to a kind
    /// — the `search` Verb's `kinds` filter validates against this.
    pub fn parse(kind: &str) -> Option<NodeKind> {
        NodeKind::ALL.iter().copied().find(|k| k.as_str() == kind)
    }

    /// The id prefix a node of this kind carries (`file:…`, `sym:…`,
    /// `pkg:…`) — the one place the wire prefix is defined, used by the
    /// `Node` constructors and matched by yg-verbs' id grammar.
    pub fn id_prefix(self) -> &'static str {
        match self {
            NodeKind::File => "file",
            NodeKind::Symbol => "sym",
            NodeKind::Package => "pkg",
            NodeKind::Commit => "commit",
            NodeKind::Contributor => "contributor",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EdgeKind {
    Defines,
    Calls,
    Imports,
    Extends,
    Implements,
    /// Commit → File: the commit changed this file (RFC 0001 §5).
    Touches,
    /// Contributor → Commit: this contributor authored the commit.
    Authored,
}

/// Compile-time backstop for `EdgeKind::ALL` — see the guard above
/// [`NodeKind::ALL`] for the mechanism and its limits.
const _: () = {
    fn count(kind: EdgeKind) -> usize {
        match kind {
            EdgeKind::Defines => 1,
            EdgeKind::Calls => 1,
            EdgeKind::Imports => 1,
            EdgeKind::Extends => 1,
            EdgeKind::Implements => 1,
            EdgeKind::Touches => 1,
            EdgeKind::Authored => 1,
        }
    }
    let _ = count;
    assert!(EdgeKind::ALL.len() == 7);
};

impl EdgeKind {
    /// Every edge kind, for exhaustive checks (yg-verbs reads its
    /// `edge_kinds` filter vocabulary straight from this list; the
    /// `const _` block above pins its length).
    pub const ALL: &'static [EdgeKind] = &[
        EdgeKind::Defines,
        EdgeKind::Calls,
        EdgeKind::Imports,
        EdgeKind::Extends,
        EdgeKind::Implements,
        EdgeKind::Touches,
        EdgeKind::Authored,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Defines => "DEFINES",
            EdgeKind::Calls => "CALLS",
            EdgeKind::Imports => "IMPORTS",
            EdgeKind::Extends => "EXTENDS",
            EdgeKind::Implements => "IMPLEMENTS",
            EdgeKind::Touches => "TOUCHES",
            EdgeKind::Authored => "AUTHORED",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|item| item.as_str() == value)
    }
}

/// A graph node. IDs are stable content-derived strings relative to the
/// repo (`file:<path>`, `sym:<path>#<name>`) so Shard rebuilds don't
/// invalidate references held by agents mid-session (RFC 0001 §5).
#[derive(Debug, Clone)]
pub struct Node {
    pub id: String,
    pub kind: NodeKind,
    /// Human name: a Symbol's name, a Package's import path, a Commit's
    /// subject line, a Contributor's display name. None for nodes whose
    /// id says it all (Files).
    pub name: Option<String>,
    /// Repo-relative path for file-anchored nodes (File, Symbol).
    pub path: Option<String>,
    /// Committer date in unix seconds — set on Commit nodes, the key the
    /// `history` Verb orders by and filters `since` against. None for
    /// every other kind.
    pub committed_at: Option<i64>,
}

impl Node {
    /// The File node for a repo-relative path: id `file:<path>`.
    pub fn file(path: &str) -> Self {
        Self {
            id: format!("{}:{path}", NodeKind::File.id_prefix()),
            kind: NodeKind::File,
            name: None,
            path: Some(path.to_string()),
            committed_at: None,
        }
    }

    /// The Package node for an import path: id `pkg:<import-path>`.
    pub fn package(import_path: &str) -> Self {
        Self {
            id: format!("{}:{import_path}", NodeKind::Package.id_prefix()),
            kind: NodeKind::Package,
            name: Some(import_path.to_string()),
            path: None,
            committed_at: None,
        }
    }

    /// The Commit node for a commit sha: id `commit:<sha>`, name the
    /// commit subject (first line of the message), `committed_at` the
    /// committer date in unix seconds.
    pub fn commit(sha: &str, subject: &str, committed_at: i64) -> Self {
        Self {
            id: format!("{}:{sha}", NodeKind::Commit.id_prefix()),
            kind: NodeKind::Commit,
            name: Some(subject.to_string()),
            path: None,
            committed_at: Some(committed_at),
        }
    }

    /// The Contributor node for an email: id `contributor:<email>` (the
    /// dedup key within this repo's Shard), name the display name. A blank
    /// or whitespace-only name is stored as `None`, not `Some("")`, so the
    /// optional field's absence is honest and consumers (the `history`
    /// response, the CLI) fall back to the email instead of a blank name.
    pub fn contributor(email: &str, name: &str) -> Self {
        let name = name.trim();
        Self {
            id: format!("{}:{email}", NodeKind::Contributor.id_prefix()),
            kind: NodeKind::Contributor,
            name: (!name.is_empty()).then(|| name.to_string()),
            path: None,
            committed_at: None,
        }
    }

    /// A Symbol declared in `path`: id `sym:<path>#<name>`. `ordinal`
    /// disambiguates same-named declarations in one file (multiple
    /// `func init()`): the first is 1 (no suffix), later ones get `~n`.
    pub fn symbol(path: &str, name: &str, ordinal: u32) -> Self {
        let prefix = NodeKind::Symbol.id_prefix();
        let id = if ordinal <= 1 {
            format!("{prefix}:{path}#{name}")
        } else {
            format!("{prefix}:{path}#{name}~{ordinal}")
        };
        Self {
            id,
            kind: NodeKind::Symbol,
            name: Some(name.to_string()),
            path: Some(path.to_string()),
            committed_at: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Edge {
    pub src: String,
    pub dst: String,
    pub kind: EdgeKind,
    pub provenance: Provenance,
    /// In [0, 1]: how sure the producing pass is (ADR 0002). f64
    /// because the artifact stores REAL and the wire serves f64: an f32
    /// here would put float noise (0.8999999…) on every response.
    pub confidence: f64,
    /// Where in the source this edge was witnessed
    /// (`<path>:<line>:<col>`, 1-based; `col` is a byte offset within
    /// the line), for edges that have a site — a CALLS edge's call
    /// site. RFC 0001 §5: "locations where applicable".
    pub location: Option<String>,
}

/// The graph segment of one Shard, as produced by an indexing pass.
#[derive(Debug, Clone, Default)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// The manifest written beside every Shard's segments: enough to verify
/// and describe the artifact without opening it.
#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    /// The commit this Shard indexes.
    pub commit: String,
    /// Indexing pass that produced it: `syntactic` (M0) or `precise` (M1).
    pub pass: String,
    pub counts: Counts,
    /// Checksums and sizes per segment file, keyed by file name.
    pub segments: BTreeMap<String, Segment>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Counts {
    pub nodes: i64,
    pub edges: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Segment {
    pub sha256: String,
    pub bytes: u64,
}

/// A Shard that made it to object storage, ready to be recorded in the
/// control plane. Commit and pass aren't carried: both entry points
/// fail closed unless the manifest matches the commit the caller asked
/// about and [`SYNTACTIC_PASS`], so the caller already holds them.
#[derive(Debug, Clone)]
pub struct PublishedShard {
    pub revision: String,
    pub manifest_key: String,
    pub node_count: i64,
    pub edge_count: i64,
}

/// The revision id the syntactic pass publishes for a commit.
/// Deterministic on purpose: re-indexing the same commit derives the same
/// revision, which is what makes re-indexing idempotent.
pub fn syntactic_revision(commit: &str) -> String {
    format!("{commit}{}", syntactic_revision_suffix())
}

/// The pass+schema suffix every current syntactic revision id ends with
/// — what worker boot hands the control plane to find repos whose
/// current Shard predates this binary's schema.
pub fn syntactic_revision_suffix() -> String {
    format!("-{SYNTACTIC_PASS}-v{SCHEMA_VERSION}")
}

/// Object key of a revision's manifest — the one definition of the
/// Shard layout (RFC 0001 §6), shared by the writer, the reader, and
/// anything that needs to plant or inspect a Shard.
pub fn manifest_key(repo_id: i64, revision: &str) -> String {
    format!("shards/{repo_id}/{revision}/manifest.json")
}

/// File name of the graph segment inside a Shard — the key under
/// [`Manifest::segments`] and the last component of
/// [`graph_segment_key`].
pub const GRAPH_SEGMENT_FILE: &str = "graph.sqlite";

/// Object key of a revision's graph segment, beside its manifest.
pub fn graph_segment_key(repo_id: i64, revision: &str) -> String {
    format!("shards/{repo_id}/{revision}/{GRAPH_SEGMENT_FILE}")
}

/// Object key of a revision's full-text segment, beside its manifest —
/// the packed tantivy index the lexical `search` Verb reads.
pub fn fts_segment_key(repo_id: i64, revision: &str) -> String {
    format!("shards/{repo_id}/{revision}/{FTS_SEGMENT_FILE}")
}

/// Delete every object of a superseded Shard revision — the whole
/// `shards/<repo_id>/<revision>/` prefix (manifest + segments) — when the
/// GC sweep reclaims it (issue #9). Prefix matching is path-segment
/// aware, so one revision's directory never sweeps another's. Idempotent:
/// an object already gone is success, so a sweep retried after a partial
/// delete finishes cleanly. Listing surfaces no objects for a revision
/// already fully deleted, which is also success.
pub async fn delete_shard(
    store: &dyn ObjectStore,
    repo_id: i64,
    revision: &str,
) -> anyhow::Result<()> {
    use futures::{StreamExt, TryStreamExt};
    let prefix = object_store::path::Path::from(format!("shards/{repo_id}/{revision}/"));
    let manifest = object_store::path::Path::from(manifest_key(repo_id, revision));

    delete_if_present(store, &manifest)
        .await
        .context("deleting the Shard manifest before its segments")?;
    let manifest_for_filter = manifest.clone();
    let locations = store
        .list(Some(&prefix))
        .map_err(|error| object_store::Error::Generic {
            store: "Shard object listing",
            source: Box::new(error),
        })
        .map_ok(|object| object.location)
        .try_filter(move |location| futures::future::ready(location != &manifest_for_filter))
        .boxed();
    store
        .delete_stream(locations)
        .filter_map(|deleted| async {
            match deleted {
                Ok(path) => Some(Ok(path)),
                Err(object_store::Error::NotFound { .. }) => None,
                Err(e) => Some(Err(e)),
            }
        })
        .try_for_each(|_| futures::future::ready(Ok(())))
        .await
        .with_context(|| format!("deleting Shard objects under {prefix}"))?;

    // A publisher that started before the reclaiming control-plane row
    // became visible may have recreated the manifest while segment
    // deletion was in flight. Remove it once more before the row is
    // reaped: either the racing completion is requeued, or it begins
    // after this fence and publishes a complete artifact after cleanup.
    delete_if_present(store, &manifest)
        .await
        .context("fencing a racing Shard manifest after segment deletion")?;
    Ok(())
}

async fn delete_if_present(
    store: &dyn ObjectStore,
    location: &object_store::path::Path,
) -> object_store::Result<()> {
    use object_store::ObjectStoreExt;
    match store.delete(location).await {
        Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
        Err(error) => Err(error),
    }
}

/// S3-compatible object storage holding the Shards (ADR 0005).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectStoreConfig {
    pub endpoint: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub region: String,
    /// Key prefix every object lands under — empty means the bucket
    /// root. Lets deployments share a bucket, and gives each e2e test
    /// database its own key namespace so identical fixture commits can
    /// never read each other's Shards.
    pub key_prefix: String,
}

impl ObjectStoreConfig {
    pub fn connect(&self) -> anyhow::Result<Arc<dyn ObjectStore>> {
        let s3 = AmazonS3Builder::new()
            .with_endpoint(&self.endpoint)
            .with_bucket_name(&self.bucket)
            .with_access_key_id(&self.access_key)
            .with_secret_access_key(&self.secret_key)
            .with_region(&self.region)
            .with_allow_http(true)
            .build()
            .context("configuring object store client")?;
        Ok(if self.key_prefix.is_empty() {
            Arc::new(s3)
        } else {
            Arc::new(object_store::prefix::PrefixStore::new(
                s3,
                self.key_prefix.as_str(),
            ))
        })
    }
}

/// Cheap reachability check that distinguishes "bucket missing or
/// unreachable" from "bucket empty": a delimited list succeeds on an
/// empty bucket but errors when the bucket doesn't exist. Every process
/// that touches Shards probes at boot — [`ObjectStoreConfig::connect`]
/// never goes near the network, so a misconfigured endpoint otherwise
/// boots cleanly and fails every job instead.
pub async fn probe_object_store(store: &dyn ObjectStore) -> anyhow::Result<()> {
    store.list_with_delimiter(None).await?;
    Ok(())
}

/// The Shard already published for `commit` at the current schema
/// version, if any — read back from its manifest, so the returned counts
/// describe the artifact actually in storage, not whatever a fresh run
/// of the pass would produce.
///
/// Syntactic-pass only, like [`write_shard`]: M1's precise pass brings
/// its own entry points rather than a pass parameter guessed at now.
pub async fn published_shard(
    store: &dyn ObjectStore,
    repo_id: i64,
    commit: &str,
) -> anyhow::Result<Option<PublishedShard>> {
    let revision = syntactic_revision(commit);
    let manifest_key = manifest_key(repo_id, &revision);
    let bytes = match store.get(&manifest_key.as_str().into()).await {
        Ok(get) => get
            .bytes()
            .await
            .context("reading the published manifest")?,
        Err(object_store::Error::NotFound { .. }) => return Ok(None),
        Err(e) => return Err(e).context("checking for an already-published Shard"),
    };
    let manifest: Manifest =
        serde_json::from_slice(&bytes).context("parsing the published manifest")?;
    // Fail closed: the revision id asserts (commit, pass, schema), and a
    // manifest that disagrees — bucket aliasing across deployments, a
    // manual repair gone wrong — must not have its contents recorded as
    // this revision's.
    if manifest.commit != commit
        || manifest.pass != SYNTACTIC_PASS
        || manifest.schema_version != SCHEMA_VERSION
    {
        anyhow::bail!(
            "the published manifest at {manifest_key} does not describe this revision \
             (it says commit {}, pass {}, schema v{}); refusing to trust it",
            manifest.commit,
            manifest.pass,
            manifest.schema_version
        );
    }
    // Same fail-closed stance for shape: write_shard always records the
    // graph segment, so a manifest without it describes an artifact the
    // layout says cannot exist (a half-copied bucket, a manual repair).
    // Trusting it would surface as a missing segment at first read,
    // far from the cause — and the deterministic revision plus
    // create-only puts mean nothing would ever repair it.
    for required in [GRAPH_SEGMENT_FILE, FTS_SEGMENT_FILE] {
        if !manifest.segments.contains_key(required) {
            anyhow::bail!(
                "the published manifest at {manifest_key} records no {required} segment; \
                 refusing to trust it"
            );
        }
    }
    for file in manifest.segments.keys() {
        let key = object_store::path::Path::from(format!("shards/{repo_id}/{revision}/{file}"));
        match object_store::ObjectStoreExt::head(store, &key).await {
            Ok(_) => {}
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("checking published segment {file}"));
            }
        }
    }
    Ok(Some(PublishedShard {
        revision,
        manifest_key,
        node_count: manifest.counts.nodes,
        edge_count: manifest.counts.edges,
    }))
}

/// Fully built Shard segments ready for object-store publication. Its bytes
/// stay private so callers cannot publish a partially prepared artifact.
pub struct PreparedShard {
    graph_bytes: Vec<u8>,
    graph_segment: Segment,
    fts_bytes: Vec<u8>,
    fts_segment: Segment,
    counts: Counts,
}

/// Build and digest a syntactic Shard's graph and full-text segments without
/// touching object storage. Publishers can do this expensive CPU work before
/// entering their short publication critical section.
pub async fn prepare_shard(
    mut graph: Graph,
    search_docs: Vec<SearchDoc>,
) -> anyhow::Result<PreparedShard> {
    // Build and digest both segments off the runtime threads: building a
    // tantivy index and hashing a large artifact are as blocking as the
    // graph build.
    tokio::task::spawn_blocking(move || -> anyhow::Result<PreparedShard> {
        deduplicate_identical_edges(&mut graph);
        let counts = Counts {
            nodes: graph.nodes.len() as i64,
            edges: graph.edges.len() as i64,
        };
        let graph_bytes = build_graph_sqlite(&graph)?;
        let graph_segment = Segment {
            sha256: sha256_hex(&graph_bytes),
            bytes: graph_bytes.len() as u64,
        };
        let fts_bytes = build_fts(&search_docs)?;
        let fts_segment = Segment {
            sha256: sha256_hex(&fts_bytes),
            bytes: fts_bytes.len() as u64,
        };
        Ok(PreparedShard {
            graph_bytes,
            graph_segment,
            fts_bytes,
            fts_segment,
            counts,
        })
    })
    .await
    .context("shard segment build task panicked")?
}

/// Drop facts repeated byte-for-byte by an extractor before SQLite insertion.
///
/// The artifact's unique indexes intentionally identify an edge without its
/// provenance or confidence. Keeping those fields in this deduplication key
/// means conflicting derivations still reach SQLite and fail loudly instead of
/// being mistaken for harmless extractor repetition.
fn deduplicate_identical_edges(graph: &mut Graph) {
    let mut seen = BTreeSet::new();
    graph.edges.retain(|edge| {
        seen.insert((
            edge.src.clone(),
            edge.dst.clone(),
            edge.kind.as_str(),
            edge.provenance.as_str(),
            edge.confidence.to_bits(),
            edge.location.clone(),
        ))
    });
}

/// Publish prepared segments for `commit` of repo `repo_id`: the graph
/// segment, the full-text segment, then the manifest. The manifest goes last —
/// its presence marks a complete Shard, so a write that dies half-way leaves
/// garbage, never a torn artifact. Published Shards are immutable: every put
/// is create-only (If-None-Match), so even a stale lease holder racing a fresh
/// one can never overwrite published objects, and an already-published
/// revision is returned as-is.
pub async fn publish_shard(
    store: &dyn ObjectStore,
    repo_id: i64,
    commit: &str,
    prepared: PreparedShard,
) -> anyhow::Result<PublishedShard> {
    let revision = syntactic_revision(commit);
    let manifest_key = manifest_key(repo_id, &revision);
    let graph_key = object_store::path::Path::from(graph_segment_key(repo_id, &revision));
    let fts_key = object_store::path::Path::from(fts_segment_key(repo_id, &revision));
    let PreparedShard {
        graph_bytes,
        graph_segment: local_segment,
        fts_bytes,
        fts_segment: local_fts_segment,
        counts: local_counts,
    } = prepared;
    let create_only = PutOptions {
        mode: PutMode::Create,
        ..Default::default()
    };
    let (segment, counts) = match store
        .put_opts(
            &graph_key,
            PutPayload::from(graph_bytes),
            create_only.clone(),
        )
        .await
    {
        Ok(_) => (local_segment, local_counts),
        // Another publisher beat us to the segment. When its manifest
        // landed too, the published Shard is the whole answer — done,
        // without re-reading the artifact. Otherwise the manifest is
        // ours to write, and it must describe what storage actually
        // holds — a racing worker on a different build (mixed images
        // mid-deploy) may have written subtly different bytes — so
        // digest AND counts come from the stored artifact.
        Err(object_store::Error::AlreadyExists { .. }) => {
            if let Some(published) = published_shard(store, repo_id, commit).await? {
                return Ok(published);
            }
            let stored = store
                .get(&graph_key)
                .await
                .context("reading the segment another publisher just wrote")?
                .bytes()
                .await
                .context("reading the segment another publisher just wrote")?;
            tokio::task::spawn_blocking(move || -> anyhow::Result<(Segment, Counts)> {
                let segment = Segment {
                    sha256: sha256_hex(&stored),
                    bytes: stored.len() as u64,
                };
                let counts = read_graph_counts(&stored)
                    .context("counting the segment another publisher just wrote")?;
                Ok((segment, counts))
            })
            .await
            .context("segment inspection task panicked")??
        }
        Err(e) => return Err(e).context("uploading the graph segment"),
    };

    // The full-text segment, same create-only race handling: a publisher
    // that lost the put defers to the published Shard, or — manifest not
    // yet written — digests the bytes storage actually holds (a racing
    // worker on a different build writes a different, but equally valid,
    // tantivy index).
    let fts_segment = match store
        .put_opts(&fts_key, PutPayload::from(fts_bytes), create_only.clone())
        .await
    {
        Ok(_) => local_fts_segment,
        Err(object_store::Error::AlreadyExists { .. }) => {
            if let Some(published) = published_shard(store, repo_id, commit).await? {
                return Ok(published);
            }
            let stored = store
                .get(&fts_key)
                .await
                .context("reading the full-text segment another publisher just wrote")?
                .bytes()
                .await
                .context("reading the full-text segment another publisher just wrote")?;
            let bytes = stored.len() as u64;
            let (_, sha256) = digest_off_thread(stored).await?;
            Segment { sha256, bytes }
        }
        Err(e) => return Err(e).context("uploading the full-text segment"),
    };

    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        commit: commit.to_string(),
        pass: SYNTACTIC_PASS.to_string(),
        counts,
        segments: BTreeMap::from([
            (GRAPH_SEGMENT_FILE.to_string(), segment),
            (FTS_SEGMENT_FILE.to_string(), fts_segment),
        ]),
    };
    match store
        .put_opts(
            &manifest_key.as_str().into(),
            PutPayload::from(serde_json::to_vec_pretty(&manifest)?),
            create_only,
        )
        .await
    {
        Ok(_) => {}
        // A racing publisher committed the manifest first: defer to the
        // published artifact.
        Err(object_store::Error::AlreadyExists { .. }) => {
            return published_shard(store, repo_id, commit)
                .await?
                .context("the manifest vanished or one of its listed segments is missing");
        }
        Err(e) => return Err(e).context("uploading the manifest"),
    }

    Ok(PublishedShard {
        revision,
        manifest_key,
        node_count: counts.nodes,
        edge_count: counts.edges,
    })
}

/// Build and publish a syntactic-pass Shard. Callers that serialize only the
/// publication phase should use [`prepare_shard`] before taking their fence,
/// then call [`publish_shard`] while it is held.
pub async fn write_shard(
    store: &dyn ObjectStore,
    repo_id: i64,
    commit: &str,
    graph: Graph,
    search_docs: Vec<SearchDoc>,
) -> anyhow::Result<PublishedShard> {
    let prepared = prepare_shard(graph, search_docs).await?;
    publish_shard(store, repo_id, commit, prepared).await
}

/// The query side's local Shard tier (RFC 0001 §6): graph segments are
/// materialized once into `dir` under their checksum and reused for
/// every later query, so warm queries never touch object storage.
///
/// Segments are immutable by checksum. Manifests normally stay cached by
/// key, but same-revision repair may replace one; a missing or mismatched
/// cold, not-yet-process-verified segment evicts the cached manifest and
/// retries once against storage.
/// Pointer swaps are picked up because the *caller* resolves the current
/// revision per query and only then asks this cache.
pub struct ShardCache {
    store: Arc<dyn ObjectStore>,
    dir: std::path::PathBuf,
    /// Manifests by manifest key. A std (not tokio) Mutex: these locks
    /// only ever guard map lookups and inserts, never I/O or `.await`.
    manifests: Arc<std::sync::Mutex<MemoryLru<String, Arc<Manifest>>>>,
    /// Checksums whose on-disk file this process has already verified.
    verified: Arc<std::sync::Mutex<std::collections::HashSet<CacheKey>>>,
    lru: Arc<std::sync::Mutex<DiskLru>>,
    flights: Arc<Flights>,
    pins: Arc<Pins>,
    maintenance: Arc<tokio::sync::Mutex<()>>,
    capacity: CacheCapacity,
    metrics: Metrics,
}

#[cfg(test)]
struct FtsUnpackTestSeam {
    repo_id: i64,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[cfg(test)]
static FTS_UNPACK_TEST_SEAM: std::sync::Mutex<Option<Arc<FtsUnpackTestSeam>>> =
    std::sync::Mutex::new(None);

/// A single cache artifact cannot fit under the configured byte ceiling.
#[derive(Debug)]
pub struct CacheArtifactTooLarge {
    pub artifact_bytes: u64,
    pub capacity: CacheCapacity,
}

impl std::fmt::Display for CacheArtifactTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Shard cache artifact is {} bytes, exceeding the configured {}-byte capacity",
            self.artifact_bytes,
            self.capacity.bytes()
        )
    }
}

impl std::error::Error for CacheArtifactTooLarge {}

/// Pinned query artifacts leave insufficient unpinned space for a new one.
#[derive(Debug)]
pub struct CacheCapacityUnavailable {
    pub artifact_bytes: u64,
    pub capacity: CacheCapacity,
    release_generation: u64,
}

impl std::fmt::Display for CacheCapacityUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Shard cache cannot retain a {}-byte artifact within its {}-byte capacity while query artifacts are in use",
            self.artifact_bytes,
            self.capacity.bytes()
        )
    }
}

impl std::error::Error for CacheCapacityUnavailable {}

/// A revision whose manifest is not in object storage — distinct from
/// transport or corruption errors because callers holding the revision
/// from a client (a pagination cursor) must answer "your cursor
/// expired", not "the server broke". Detect with
/// `err.downcast_ref::<RevisionMissing>()`.
#[derive(Debug)]
pub struct RevisionMissing {
    pub revision: String,
}

impl std::fmt::Display for RevisionMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no Shard published for revision {}", self.revision)
    }
}

impl std::error::Error for RevisionMissing {}

/// A revision published under a different [`SCHEMA_VERSION`] than this
/// binary reads — a pre-deploy Shard met post-deploy code. Distinct
/// from corruption errors because callers can say something useful: a
/// pinned pagination cursor has expired, and a current pointer is
/// re-indexed at worker boot (the queue is populated from the same
/// revision-suffix check), so "try again shortly" is true. Detect with
/// `err.downcast_ref::<SchemaOutdated>()`.
#[derive(Debug)]
pub struct SchemaOutdated {
    pub revision: String,
    pub schema_version: u32,
}

impl std::fmt::Display for SchemaOutdated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "revision {} was published under schema v{}; this server reads v{SCHEMA_VERSION}",
            self.revision, self.schema_version
        )
    }
}

impl std::error::Error for SchemaOutdated {}

#[derive(Debug)]
struct SegmentNeedsManifestRefresh {
    revision: String,
    message: String,
    missing: bool,
}

impl std::fmt::Display for SegmentNeedsManifestRefresh {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SegmentNeedsManifestRefresh {}

impl ShardCache {
    pub fn new(store: Arc<dyn ObjectStore>, dir: impl Into<std::path::PathBuf>) -> Self {
        Self::with_capacity(store, dir, CacheCapacity::DEFAULT)
    }

    /// Build a cache with an explicit non-zero on-disk byte capacity.
    pub fn with_capacity(
        store: Arc<dyn ObjectStore>,
        dir: impl Into<std::path::PathBuf>,
        capacity: CacheCapacity,
    ) -> Self {
        Self::with_metrics_and_capacity(store, dir, Metrics::unregistered(), capacity)
    }

    /// Build a cache using the supplied hit, miss, and eviction collectors.
    pub fn with_metrics(
        store: Arc<dyn ObjectStore>,
        dir: impl Into<std::path::PathBuf>,
        metrics: Metrics,
    ) -> Self {
        Self::with_metrics_and_capacity(store, dir, metrics, CacheCapacity::DEFAULT)
    }

    /// Build a metrics-instrumented cache with an explicit byte capacity.
    pub fn with_metrics_and_capacity(
        store: Arc<dyn ObjectStore>,
        dir: impl Into<std::path::PathBuf>,
        metrics: Metrics,
        capacity: CacheCapacity,
    ) -> Self {
        let dir = dir.into();
        let (mut lru, startup_evictions, stale_paths) = DiskLru::scan(&dir, capacity);
        for (path, artifact) in stale_paths {
            if let Err(error) = cache::remove_cached_path(&path) {
                let remaining = error.remaining_path().to_path_buf();
                let bytes =
                    cache::measure_paths(std::slice::from_ref(&remaining)).unwrap_or(u64::MAX);
                lru.track_stale(remaining, bytes, artifact);
            }
        }
        for mut eviction in startup_evictions {
            let removed = remove_eviction_paths(&mut eviction).is_ok();
            if removed {
                metrics.capacity_eviction(eviction.key.artifact);
            } else {
                lru.restore_failed_eviction(eviction);
            }
        }
        Self {
            store,
            dir,
            manifests: Arc::new(std::sync::Mutex::new(MemoryLru::new(
                capacity.manifest_entries(),
            ))),
            verified: Default::default(),
            lru: Arc::new(std::sync::Mutex::new(lru)),
            flights: Default::default(),
            pins: Default::default(),
            maintenance: Arc::new(tokio::sync::Mutex::new(())),
            capacity,
            metrics,
        }
    }

    /// Local path of the verified graph segment for a revision, fetching
    /// from object storage only when the local tier can't answer: a warm
    /// revision costs no storage round-trips at all, and a cached file
    /// whose checksum no longer matches its name is refetched, not
    /// trusted.
    ///
    /// A revision that was never published (or has been GC'd) fails
    /// with [`RevisionMissing`].
    pub async fn graph_path(
        &self,
        repo_id: i64,
        revision: &str,
    ) -> anyhow::Result<std::path::PathBuf> {
        match self.graph_path_once(repo_id, revision).await {
            Err(error)
                if error
                    .downcast_ref::<SegmentNeedsManifestRefresh>()
                    .is_some() =>
            {
                self.forget_manifest(repo_id, revision);
                self.graph_path_once(repo_id, revision)
                    .await
                    .map_err(public_segment_error)
            }
            result => result,
        }
    }

    /// Resolve and pin a graph segment until its consumer has opened it.
    pub async fn leased_graph_path(
        &self,
        repo_id: i64,
        revision: &str,
    ) -> anyhow::Result<LeasedPath> {
        loop {
            let path = match self.graph_path(repo_id, revision).await {
                Ok(path) => path,
                Err(error) if error.downcast_ref::<CacheCapacityUnavailable>().is_some() => {
                    let generation = error
                        .downcast_ref::<CacheCapacityUnavailable>()
                        .expect("the capacity error was just matched")
                        .release_generation;
                    self.pins.wait_for_lease_release(generation).await;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let key = CacheKey {
                sha: checksum_from_cache_path(&path, ".sqlite")?,
                artifact: metrics::Artifact::Graph,
            };
            let lease = match self.pins.try_pin(key) {
                Ok(lease) => lease,
                Err(generation) => {
                    self.pins.wait_for_eviction(generation).await;
                    continue;
                }
            };
            if tokio::fs::try_exists(&path)
                .await
                .context("checking a leased graph segment")?
            {
                return Ok(LeasedPath { path, lease });
            }
            drop(lease);
        }
    }

    async fn graph_path_once(
        &self,
        repo_id: i64,
        revision: &str,
    ) -> anyhow::Result<std::path::PathBuf> {
        let manifest = self.manifest(repo_id, revision).await?;
        let sha = checked_segment_sha(&manifest, revision, GRAPH_SEGMENT_FILE)?;
        let path = self.dir.join(format!("{sha}.sqlite"));
        let key = CacheKey {
            sha: sha.clone(),
            artifact: metrics::Artifact::Graph,
        };
        if self.is_verified(&key, &path).await? {
            self.metrics.hit(metrics::Artifact::Graph);
            return Ok(path);
        }
        let gate = self.flights.gate(&sha);
        let _flight = gate.lock().await;
        if self.is_verified(&key, &path).await? {
            self.metrics.hit(metrics::Artifact::Graph);
            return Ok(path);
        }
        self.fetch_verify_into(
            revision,
            &sha,
            &path,
            graph_segment_key(repo_id, revision),
            metrics::Artifact::Graph,
        )
        .await?;
        self.record_verified(key, vec![path.clone()]).await?;
        Ok(path)
    }

    /// Process verification is only a hashing shortcut. The filesystem
    /// remains authoritative because operators and external cleaners may
    /// remove cache files while the server is running.
    async fn is_verified(
        &self,
        key: &CacheKey,
        required_path: &std::path::Path,
    ) -> anyhow::Result<bool> {
        let remembered = self
            .verified
            .lock()
            .expect("shard cache lock poisoned")
            .contains(key);
        if !remembered {
            return Ok(false);
        }
        match tokio::fs::try_exists(required_path).await {
            Ok(true) => {
                self.lru
                    .lock()
                    .expect("shard cache lock poisoned")
                    .touch(key);
                Ok(true)
            }
            Ok(false) => {
                self.forget_verified(key);
                Ok(false)
            }
            Err(error) => Err(error).context("checking a verified Shard-cache artifact"),
        }
    }

    fn forget_verified(&self, key: &CacheKey) {
        self.verified
            .lock()
            .expect("shard cache lock poisoned")
            .remove(key);
    }

    async fn record_verified(
        &self,
        key: CacheKey,
        paths: Vec<std::path::PathBuf>,
    ) -> anyhow::Result<()> {
        let maintenance = self.maintenance.clone();
        let context = CommitContext {
            lru: self.lru.clone(),
            verified: self.verified.clone(),
            manifests: self.manifests.clone(),
            pins: self.pins.clone(),
            metrics: self.metrics.clone(),
            capacity: self.capacity,
        };
        tokio::spawn(async move {
            let maintenance = maintenance.lock_owned().await;
            tokio::task::spawn_blocking(move || {
                let _maintenance = maintenance;
                let artifact_bytes =
                    cache::measure_paths(&paths).context("measuring a Shard-cache artifact")?;
                commit_record(&context, key, paths, artifact_bytes)
            })
            .await
            .context("Shard-cache commit task panicked")?
        })
        .await
        .context("Shard-cache record task panicked")?
    }

    /// Ensure the content-addressed cache file `local` holds the bytes
    /// whose sha256 is `sha`, fetching `object_key` from storage and
    /// verifying it whenever the local copy is absent or no longer matches.
    /// A segment storage no longer holds surfaces as [`RevisionMissing`].
    /// Fetch chunks are hashed and written directly to a staging file. The
    /// per-checksum single-flight normally leaves one writer; the atomic
    /// rename and destination verification remain the final correctness
    /// fence across processes.
    async fn fetch_verify_into(
        &self,
        revision: &str,
        sha: &str,
        local: &std::path::Path,
        object_key: String,
        artifact: metrics::Artifact,
    ) -> anyhow::Result<()> {
        let what = artifact.description();
        let on_disk = match digest_file(local).await {
            Ok(digest) => {
                if digest == sha {
                    self.metrics.hit(artifact);
                    true
                } else {
                    self.metrics.miss(artifact);
                    self.metrics.eviction(artifact);
                    false
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.metrics.miss(artifact);
                false
            }
            Err(e) => return Err(e).with_context(|| format!("reading the cached {what}")),
        };
        if on_disk {
            return Ok(());
        }
        let fetched = match self.store.get(&object_key.as_str().into()).await {
            Ok(get) => get,
            // A manifest without its segment: the revision is being (or
            // was partially) GC'd — gone for the caller's purposes, same
            // as a missing manifest.
            Err(object_store::Error::NotFound { .. }) => {
                return Err(anyhow::Error::new(SegmentNeedsManifestRefresh {
                    revision: revision.to_string(),
                    message: format!("the {what} for {revision} is missing from object storage"),
                    missing: true,
                }));
            }
            Err(e) => return Err(e).with_context(|| format!("fetching the {what}")),
        };
        tokio::fs::create_dir_all(&self.dir)
            .await
            .context("creating the shard cache directory")?;
        // Write-then-rename so a crash mid-write leaves a stray temp file,
        // never a checksum-named file with wrong contents. The temp name
        // is unique per attempt — pid alone would let two tasks racing the
        // same cold revision share (and tear) one staging file.
        static ATTEMPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let file_name = local
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("segment");
        let tmp = self.dir.join(format!(
            "{file_name}.tmp-{}-{}",
            std::process::id(),
            ATTEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut cleanup = cache::CleanupPath::new(tmp.clone());
        let staged = async {
            use tokio::io::AsyncWriteExt;

            let mut file = tokio::fs::File::create(&tmp)
                .await
                .with_context(|| format!("staging the {what} into the cache"))?;
            let mut digest = Sha256::new();
            let mut stream = fetched.into_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.with_context(|| format!("fetching the {what}"))?;
                digest.update(&chunk);
                file.write_all(&chunk)
                    .await
                    .with_context(|| format!("staging the {what} into the cache"))?;
            }
            file.flush()
                .await
                .with_context(|| format!("staging the {what} into the cache"))?;
            drop(file);
            Ok::<String, anyhow::Error>(hex::encode(digest.finalize()))
        }
        .await;
        let digest = match staged {
            Ok(digest) => digest,
            Err(error) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(error);
            }
        };
        if digest != sha {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(anyhow::Error::new(SegmentNeedsManifestRefresh {
                revision: revision.to_string(),
                message: format!(
                    "the {what} for {revision} does not match its manifest \
                     (manifest says sha256 {sha}, storage holds {digest}); refusing to serve it"
                ),
                missing: false,
            }));
        }
        if let Err(e) = tokio::fs::rename(&tmp, local).await {
            // On platforms where rename cannot replace (Windows), the
            // loser of a cold-fetch race lands here with the winner's
            // identical, verified bytes already committed. That is only
            // success if the destination actually holds the expected
            // bytes — a stale mismatched file (the very thing this fetch
            // replaces) must not be blessed by a failed rename.
            let committed = match digest_file(local).await {
                Ok(digest) => digest == sha,
                Err(_) => false,
            };
            let _ = tokio::fs::remove_file(&tmp).await;
            if !committed {
                return Err(e).with_context(|| format!("committing the {what} into the cache"));
            }
        } else {
            cleanup.disarm();
        }
        Ok(())
    }

    /// Local path of the unpacked full-text segment directory for a
    /// revision — the tantivy index the `search` Verb opens. Like
    /// [`Self::graph_path`], the packed artifact is fetched only when the
    /// local tier can't answer and is verified against its manifest
    /// checksum before use; here it is additionally unpacked into a
    /// checksum-named directory, derived fresh from the verified archive.
    ///
    /// A revision that was never published (or has been GC'd) fails with
    /// [`RevisionMissing`].
    pub async fn fts_path(
        &self,
        repo_id: i64,
        revision: &str,
    ) -> anyhow::Result<std::path::PathBuf> {
        match self.fts_path_once(repo_id, revision).await {
            Err(error)
                if error
                    .downcast_ref::<SegmentNeedsManifestRefresh>()
                    .is_some() =>
            {
                self.forget_manifest(repo_id, revision);
                self.fts_path_once(repo_id, revision)
                    .await
                    .map_err(public_segment_error)
            }
            result => result,
        }
    }

    /// Resolve and pin an unpacked FTS segment until its consumer opens it.
    pub async fn leased_fts_path(
        &self,
        repo_id: i64,
        revision: &str,
    ) -> anyhow::Result<LeasedPath> {
        self.leased_fts_path_inner(repo_id, revision, true).await
    }

    /// Resolve FTS without waiting for another artifact pinned by this same
    /// operation; used when a caller already holds a graph lease.
    pub async fn leased_fts_path_without_wait(
        &self,
        repo_id: i64,
        revision: &str,
    ) -> anyhow::Result<LeasedPath> {
        self.leased_fts_path_inner(repo_id, revision, false).await
    }

    async fn leased_fts_path_inner(
        &self,
        repo_id: i64,
        revision: &str,
        wait_for_capacity: bool,
    ) -> anyhow::Result<LeasedPath> {
        loop {
            let path = match self.fts_path(repo_id, revision).await {
                Ok(path) => path,
                Err(error)
                    if wait_for_capacity
                        && error.downcast_ref::<CacheCapacityUnavailable>().is_some() =>
                {
                    let generation = error
                        .downcast_ref::<CacheCapacityUnavailable>()
                        .expect("the capacity error was just matched")
                        .release_generation;
                    self.pins.wait_for_lease_release(generation).await;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let key = CacheKey {
                sha: checksum_from_cache_path(&path, ".fts")?,
                artifact: metrics::Artifact::Fts,
            };
            let lease = match self.pins.try_pin(key) {
                Ok(lease) => lease,
                Err(generation) => {
                    self.pins.wait_for_eviction(generation).await;
                    continue;
                }
            };
            if tokio::fs::try_exists(&path)
                .await
                .context("checking a leased fts segment")?
            {
                return Ok(LeasedPath { path, lease });
            }
            drop(lease);
        }
    }

    async fn fts_path_once(
        &self,
        repo_id: i64,
        revision: &str,
    ) -> anyhow::Result<std::path::PathBuf> {
        let manifest = self.manifest(repo_id, revision).await?;
        let sha = checked_segment_sha(&manifest, revision, FTS_SEGMENT_FILE)?;
        let archive_path = self.dir.join(format!("{sha}.tar"));
        let unpacked = self.dir.join(format!("{sha}.fts"));
        let key = CacheKey {
            sha: sha.clone(),
            artifact: metrics::Artifact::Fts,
        };

        // Warm: this process already verified the archive and unpacked it.
        if self.is_verified(&key, &archive_path).await?
            && tokio::fs::try_exists(&unpacked)
                .await
                .context("checking the unpacked fts segment")?
        {
            self.metrics.hit(metrics::Artifact::Fts);
            return Ok(unpacked);
        }
        let task_cache = self.detached_task_handle();
        let task_revision = revision.to_string();
        tokio::spawn(async move {
            task_cache
                .materialize_fts(repo_id, task_revision, sha, key, archive_path, unpacked)
                .await
        })
        .await
        .context("FTS materialization task panicked")?
    }

    /// Own all work after the first possible cache-file commit. The caller
    /// awaits this detached task, so dropping a cold request cannot strand a
    /// committed archive or unpacked directory outside LRU accounting.
    async fn materialize_fts(
        &self,
        repo_id: i64,
        revision: String,
        sha: String,
        key: CacheKey,
        archive_path: std::path::PathBuf,
        unpacked: std::path::PathBuf,
    ) -> anyhow::Result<std::path::PathBuf> {
        let gate = self.flights.gate(&sha);
        let _flight = gate.lock().await;
        if self.is_verified(&key, &archive_path).await?
            && tokio::fs::try_exists(&unpacked)
                .await
                .context("checking the unpacked fts segment")?
        {
            self.metrics.hit(metrics::Artifact::Fts);
            return Ok(unpacked);
        }

        // The packed archive, fetched and checksum-verified through the
        // shared dance. The unpacked directory is content-addressed by the
        // same sha (it is only ever created by an atomic rename of a
        // fully-unpacked temp dir), so an existing one is by definition the
        // correct content — there is nothing to invalidate on a refetch,
        // and deleting it would yank it out from under a concurrent reader.
        self.fetch_verify_into(
            &revision,
            &sha,
            &archive_path,
            fts_segment_key(repo_id, &revision),
            metrics::Artifact::Fts,
        )
        .await?;

        #[cfg(test)]
        let unpack_test_seam = {
            FTS_UNPACK_TEST_SEAM
                .lock()
                .expect("FTS unpack test seam lock poisoned")
                .clone()
        };
        #[cfg(test)]
        if let Some(seam) = unpack_test_seam.filter(|seam| seam.repo_id == repo_id) {
            seam.entered.notify_one();
            seam.release.notified().await;
        }

        // Unpack the verified archive into its directory if absent.
        let unpack_result = match tokio::fs::try_exists(&unpacked)
            .await
            .context("checking the unpacked fts segment")
        {
            Ok(true) => Ok(()),
            Ok(false) => {
                let dir_root = self.dir.clone();
                let target = unpacked.clone();
                let sha_for_tmp = sha.clone();
                let archive = archive_path.clone();
                // Untarring is blocking fs work; keep it off the runtime threads.
                tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    static ATTEMPT: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let tmp = dir_root.join(format!(
                        "{sha_for_tmp}.fts.tmp-{}-{}",
                        std::process::id(),
                        ATTEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    ));
                    let mut cleanup = cache::CleanupPath::new(tmp.clone());
                    // A previous crashed unpack may have left this temp dir.
                    let _ = std::fs::remove_dir_all(&tmp);
                    // Clean the temp dir on any failure (a partial unpack, a
                    // rename race) so a failed attempt never leaks it.
                    let unpack_and_commit = || -> anyhow::Result<()> {
                        fts::unpack_fts_file(&archive, &tmp)?;
                        match std::fs::rename(&tmp, &target) {
                            Ok(()) => Ok(()),
                            // Another task unpacked the same revision first —
                            // its directory is the complete one; ours is
                            // redundant.
                            Err(_) if target.exists() => Ok(()),
                            Err(e) => {
                                Err(e).context("committing the unpacked fts segment into the cache")
                            }
                        }
                    };
                    let result = unpack_and_commit();
                    if result.is_err() || tmp.exists() {
                        let _ = std::fs::remove_dir_all(&tmp);
                    } else {
                        cleanup.disarm();
                    }
                    result
                })
                .await
                .context("fts segment unpack task panicked")
                .and_then(|result| result)
            }
            Err(error) => Err(error),
        };

        let mut committed_paths = vec![archive_path];
        if unpacked.is_dir() {
            committed_paths.push(unpacked.clone());
        }
        self.record_verified(key, committed_paths).await?;
        unpack_result?;
        Ok(unpacked)
    }

    fn detached_task_handle(&self) -> Self {
        Self {
            store: self.store.clone(),
            dir: self.dir.clone(),
            manifests: self.manifests.clone(),
            verified: self.verified.clone(),
            lru: self.lru.clone(),
            flights: self.flights.clone(),
            pins: self.pins.clone(),
            maintenance: self.maintenance.clone(),
            capacity: self.capacity,
            metrics: self.metrics.clone(),
        }
    }

    fn forget_manifest(&self, repo_id: i64, revision: &str) {
        self.manifests
            .lock()
            .expect("shard cache lock poisoned")
            .remove(&manifest_key(repo_id, revision));
    }

    /// A revision's manifest, normally fetched once per process. A
    /// same-revision repair can replace it, so segment retrieval evicts
    /// and refetches this entry once when stored bytes are missing or no
    /// longer match. Fetched manifests get the same fail-closed scrutiny
    /// as [`published_shard`]:
    /// the revision id asserts (commit, pass, schema), and a manifest
    /// that disagrees — bucket aliasing across deployments, a manual
    /// repair gone wrong — must not be served as this revision.
    ///
    /// A manifest from another schema version is refused with a typed
    /// [`SchemaOutdated`] — but only after it is cached: it honestly
    /// describes its (older) revision and is immutable, so caching it
    /// means a stale revision read during a deploy rollout costs one
    /// object-store fetch total, not one per query. The gate runs on
    /// both the cache-hit and fresh-fetch paths, so a warm entry can
    /// never smuggle an unreadable artifact past it.
    async fn manifest(&self, repo_id: i64, revision: &str) -> anyhow::Result<Arc<Manifest>> {
        let key = manifest_key(repo_id, revision);
        let cached = self
            .manifests
            .lock()
            .expect("shard cache lock poisoned")
            .get(&key);
        let manifest = match cached {
            Some(manifest) => {
                self.metrics.hit(metrics::Artifact::Manifest);
                manifest
            }
            None => {
                self.metrics.miss(metrics::Artifact::Manifest);
                self.fetch_and_cache_manifest(&key, revision).await?
            }
        };
        // Gate after the cache lookup so cache hits are screened too:
        // this binary cannot read another schema version's segment, and
        // saying so here beats the v-mismatched SQL inside the segment
        // saying it cryptically.
        if manifest.schema_version != SCHEMA_VERSION {
            return Err(anyhow::Error::new(SchemaOutdated {
                revision: revision.to_string(),
                schema_version: manifest.schema_version,
            }));
        }
        Ok(manifest)
    }

    /// Fetch a manifest from object storage, validate it agrees with its
    /// revision id, and cache it. A disagreeing manifest (corruption,
    /// bucket aliasing) is never cached — it bails. An honest manifest
    /// is cached even if its schema is outdated; the schema gate lives
    /// in [`Self::manifest`], applied to every read.
    async fn fetch_and_cache_manifest(
        &self,
        key: &str,
        revision: &str,
    ) -> anyhow::Result<Arc<Manifest>> {
        // Fetched outside the lock: a slow fetch of one revision must
        // not stall cached lookups of others. Racing duplicate fetches
        // of one immutable manifest are harmless.
        let bytes = match self.store.get(&key.into()).await {
            Ok(get) => get
                .bytes()
                .await
                .with_context(|| format!("fetching the manifest for revision {revision}"))?,
            Err(object_store::Error::NotFound { .. }) => {
                return Err(anyhow::Error::new(RevisionMissing {
                    revision: revision.to_string(),
                }));
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("fetching the manifest for revision {revision}"));
            }
        };
        let manifest: Manifest =
            serde_json::from_slice(&bytes).context("parsing the fetched manifest")?;
        if manifest_disagrees_with_revision(&manifest, revision) {
            anyhow::bail!(
                "the manifest at {key} does not describe revision {revision} \
                 (it says commit {}, pass {}, schema v{}); refusing to trust it",
                manifest.commit,
                manifest.pass,
                manifest.schema_version
            );
        }
        let manifest = Arc::new(manifest);
        self.manifests
            .lock()
            .expect("shard cache lock poisoned")
            .insert(key.to_string(), manifest.clone());
        Ok(manifest)
    }
}

struct EvictionMarker {
    pins: Arc<Pins>,
    key: CacheKey,
}

impl EvictionMarker {
    fn begin(pins: Arc<Pins>, key: &CacheKey) -> Option<Self> {
        pins.begin_eviction(key).then(|| Self {
            pins,
            key: key.clone(),
        })
    }
}

impl Drop for EvictionMarker {
    fn drop(&mut self) {
        self.pins.finish_eviction(&self.key);
    }
}

struct CommitContext {
    lru: Arc<std::sync::Mutex<DiskLru>>,
    verified: Arc<std::sync::Mutex<std::collections::HashSet<CacheKey>>>,
    manifests: Arc<std::sync::Mutex<MemoryLru<String, Arc<Manifest>>>>,
    pins: Arc<Pins>,
    metrics: Metrics,
    capacity: CacheCapacity,
}

fn commit_record(
    context: &CommitContext,
    key: CacheKey,
    paths: Vec<std::path::PathBuf>,
    artifact_bytes: u64,
) -> anyhow::Result<()> {
    if artifact_bytes > context.capacity.bytes() {
        let eviction = cache::Evicted::new(key, paths, artifact_bytes);
        remove_eviction(
            &context.lru,
            &context.verified,
            &context.manifests,
            &context.pins,
            &context.metrics,
            eviction,
        )?;
        return Err(anyhow::Error::new(CacheArtifactTooLarge {
            artifact_bytes,
            capacity: context.capacity,
        }));
    }

    let (pinned, release_generation) = context.pins.pinned();
    let evictions = context
        .lru
        .lock()
        .expect("shard cache lock poisoned")
        .record(key.clone(), paths, artifact_bytes, &pinned);
    let evicted_current = evictions.iter().any(|eviction| eviction.key == key);
    let mut capacity_unavailable = false;
    let mut first_error = None;

    for eviction in evictions {
        let Some(_marker) = EvictionMarker::begin(context.pins.clone(), &eviction.key) else {
            context
                .lru
                .lock()
                .expect("shard cache lock poisoned")
                .restore(eviction);
            capacity_unavailable = true;
            continue;
        };
        if let Err(error) = remove_marked_eviction(
            &context.lru,
            &context.verified,
            &context.manifests,
            &context.metrics,
            eviction,
        ) {
            first_error.get_or_insert(error);
        }
    }

    if first_error.is_some() || capacity_unavailable {
        if let Some(current) = context
            .lru
            .lock()
            .expect("shard cache lock poisoned")
            .take(&key)
            && let Err(error) = remove_eviction(
                &context.lru,
                &context.verified,
                &context.manifests,
                &context.pins,
                &context.metrics,
                current,
            )
        {
            first_error.get_or_insert(error);
        }
        if let Some(error) = first_error {
            return Err(error.context("evicting a capacity-limited Shard-cache artifact"));
        }
        return Err(anyhow::Error::new(CacheCapacityUnavailable {
            artifact_bytes,
            capacity: context.capacity,
            release_generation,
        }));
    }

    if evicted_current {
        return Err(anyhow::Error::new(CacheCapacityUnavailable {
            artifact_bytes,
            capacity: context.capacity,
            release_generation,
        }));
    }
    context
        .verified
        .lock()
        .expect("shard cache lock poisoned")
        .insert(key);
    Ok(())
}

fn remove_eviction(
    lru: &std::sync::Mutex<DiskLru>,
    verified: &std::sync::Mutex<std::collections::HashSet<CacheKey>>,
    manifests: &std::sync::Mutex<MemoryLru<String, Arc<Manifest>>>,
    pins: &Arc<Pins>,
    metrics: &Metrics,
    eviction: cache::Evicted,
) -> anyhow::Result<()> {
    let Some(_marker) = EvictionMarker::begin(pins.clone(), &eviction.key) else {
        lru.lock()
            .expect("shard cache lock poisoned")
            .restore(eviction);
        anyhow::bail!("a Shard-cache artifact became pinned while it was being evicted");
    };
    remove_marked_eviction(lru, verified, manifests, metrics, eviction)
}

fn remove_marked_eviction(
    lru: &std::sync::Mutex<DiskLru>,
    verified: &std::sync::Mutex<std::collections::HashSet<CacheKey>>,
    manifests: &std::sync::Mutex<MemoryLru<String, Arc<Manifest>>>,
    metrics: &Metrics,
    mut eviction: cache::Evicted,
) -> anyhow::Result<()> {
    let removal = remove_eviction_paths(&mut eviction);
    verified
        .lock()
        .expect("shard cache lock poisoned")
        .remove(&eviction.key);
    manifests
        .lock()
        .expect("shard cache lock poisoned")
        .retain(|_, manifest| {
            !manifest
                .segments
                .values()
                .any(|segment| segment.sha256 == eviction.key.sha)
        });
    if let Err(error) = removal {
        lru.lock()
            .expect("shard cache lock poisoned")
            .restore_failed_eviction(eviction);
        return Err(error);
    }
    if eviction.displaced_cached_entry() {
        metrics.capacity_eviction(eviction.key.artifact);
    }
    Ok(())
}

fn remove_eviction_paths(eviction: &mut cache::Evicted) -> anyhow::Result<()> {
    let mut first_error = None;
    for path in &mut eviction.paths {
        if let Err(error) = cache::remove_cached_path(path) {
            *path = error.remaining_path().to_path_buf();
            first_error.get_or_insert(error);
        }
    }
    match first_error {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}

fn public_segment_error(error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<SegmentNeedsManifestRefresh>() {
        Some(refresh) if refresh.missing => anyhow::Error::new(RevisionMissing {
            revision: refresh.revision.clone(),
        }),
        _ => error,
    }
}

/// Whether a fetched manifest contradicts what its revision id asserts.
/// Checked by *recomposing* — minting the revision a manifest with these
/// fields would get and comparing — never by parsing the revision apart,
/// so the check can't drift from the minting grammar (a pass name
/// containing `-` parses ambiguously but recomposes exactly).
fn manifest_disagrees_with_revision(manifest: &Manifest, revision: &str) -> bool {
    let recomposed = format!(
        "{}-{}-v{}",
        manifest.commit, manifest.pass, manifest.schema_version
    );
    recomposed != revision
}

/// The validated sha256 of a manifest's named segment — confirmed to be a
/// checksum (64 hex chars) before it is ever joined onto a cache path, so
/// a doctored manifest (`../`, an absolute path) can't escape the cache
/// directory through the join.
fn checked_segment_sha(manifest: &Manifest, revision: &str, file: &str) -> anyhow::Result<String> {
    let segment = manifest
        .segments
        .get(file)
        .with_context(|| format!("the manifest for {revision} records no {file} segment"))?;
    let sha = segment.sha256.clone();
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!(
            "the manifest for {revision} records {sha:?} as a segment checksum, \
             which is not a sha256 digest; refusing to trust it"
        );
    }
    Ok(sha)
}

/// The one digest the Shard layout uses, writer and reader alike:
/// lowercase hex SHA-256.
fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Hash a whole segment off the runtime threads — as blocking as
/// building one — handing the bytes back beside their digest: the one
/// verification every cache path runs.
async fn digest_off_thread<B: AsRef<[u8]> + Send + 'static>(
    bytes: B,
) -> anyhow::Result<(B, String)> {
    tokio::task::spawn_blocking(move || {
        let digest = sha256_hex(bytes.as_ref());
        (bytes, digest)
    })
    .await
    .context("segment verification task panicked")
}

async fn digest_file(path: &std::path::Path) -> std::io::Result<String> {
    use tokio::io::AsyncReadExt;

    let mut file = tokio::fs::File::open(path).await?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn checksum_from_cache_path(path: &std::path::Path, suffix: &str) -> anyhow::Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(suffix))
        .filter(|sha| sha.len() == 64 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_string)
        .context("a materialized Shard-cache path is not content-addressed")
}

/// Node/edge counts of a serialized graph segment, read back from its
/// bytes — used when another publisher's artifact, not ours, is the one
/// in storage.
fn read_graph_counts(bytes: &[u8]) -> anyhow::Result<Counts> {
    let dir = tempfile::tempdir().context("creating a scratch dir to inspect a graph segment")?;
    let path = dir.path().join("graph.sqlite");
    std::fs::write(&path, bytes).context("staging the graph segment for inspection")?;
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let counts = conn.query_row(
        &format!(
            "SELECT (SELECT count(*) FROM {}), (SELECT count(*) FROM {})",
            graph_schema::NODES,
            graph_schema::EDGES
        ),
        [],
        |row| {
            Ok(Counts {
                nodes: row.get(0)?,
                edges: row.get(1)?,
            })
        },
    )?;
    Ok(counts)
}

/// Serialize a graph into a single-file SQLite database (SQLite as
/// *artifact format*, RFC 0001 §6) and return its bytes.
fn build_graph_sqlite(graph: &Graph) -> anyhow::Result<Vec<u8>> {
    // SQLite writes to a real file; a tempdir gives it one without ever
    // racing another build.
    let dir = tempfile::tempdir().context("creating a scratch dir for the graph segment")?;
    let path = dir.path().join("graph.sqlite");
    let conn = rusqlite::Connection::open(&path)?;
    // Write-once artifact: no journal to clean up, nothing to recover.
    conn.pragma_update(None, "journal_mode", "OFF")?;
    {
        use graph_schema::*;
        conn.execute_batch(&format!(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE {NODES} (
                 {NODE_ID}           TEXT PRIMARY KEY,
                 {NODE_KIND}         TEXT NOT NULL,
                 {NODE_NAME}         TEXT,
                 {NODE_PATH}         TEXT,
                 {NODE_COMMITTED_AT} INTEGER
             );
             CREATE TABLE {EDGES} (
                 {EDGE_SRC}        TEXT NOT NULL,
                 {EDGE_DST}        TEXT NOT NULL,
                 {EDGE_KIND}       TEXT NOT NULL,
                 {EDGE_PROVENANCE} TEXT NOT NULL,
                 {EDGE_CONFIDENCE} REAL NOT NULL
                     CHECK ({EDGE_CONFIDENCE} >= 0.0 AND {EDGE_CONFIDENCE} <= 1.0),
                 {EDGE_LOCATION}   TEXT
             );
             -- An edge's derivation and confidence describe it; its
             -- endpoints, kind, and optional witnessed site identify it.
             -- Split NULL and non-NULL sites because SQLite treats NULLs
             -- as distinct in an ordinary UNIQUE constraint.
             CREATE UNIQUE INDEX edges_unique_without_location
                 ON {EDGES} ({EDGE_SRC}, {EDGE_DST}, {EDGE_KIND})
                 WHERE {EDGE_LOCATION} IS NULL;
             CREATE UNIQUE INDEX edges_unique_with_location
                 ON {EDGES} ({EDGE_SRC}, {EDGE_DST}, {EDGE_KIND}, {EDGE_LOCATION})
                 WHERE {EDGE_LOCATION} IS NOT NULL;
             -- Two single-column indexes, not one composite: the read
             -- path's incident-edge lookup is (src = ? OR dst = ?), which
             -- SQLite turns into two index seeks only when each side has
             -- its own index.
             CREATE INDEX edges_src ON {EDGES} ({EDGE_SRC});
             CREATE INDEX edges_dst ON {EDGES} ({EDGE_DST});"
        ))?;
    }
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
        [SCHEMA_VERSION.to_string()],
    )?;
    {
        let tx = conn.unchecked_transaction()?;
        {
            use graph_schema::*;
            let mut insert_node = tx.prepare(&format!(
                "INSERT INTO {NODES} ({NODE_ID}, {NODE_KIND}, {NODE_NAME}, {NODE_PATH}, \
                 {NODE_COMMITTED_AT}) VALUES (?1, ?2, ?3, ?4, ?5)"
            ))?;
            for node in &graph.nodes {
                insert_node.execute(rusqlite::params![
                    node.id,
                    node.kind.as_str(),
                    node.name,
                    node.path,
                    node.committed_at
                ])?;
            }
            let mut insert_edge = tx.prepare(&format!(
                "INSERT INTO {EDGES} ({EDGE_SRC}, {EDGE_DST}, {EDGE_KIND}, {EDGE_PROVENANCE}, \
                 {EDGE_CONFIDENCE}, {EDGE_LOCATION}) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
            ))?;
            for edge in &graph.edges {
                insert_edge.execute(rusqlite::params![
                    edge.src,
                    edge.dst,
                    edge.kind.as_str(),
                    edge.provenance.as_str(),
                    edge.confidence,
                    edge.location
                ])?;
            }
        }
        tx.commit()?;
    }
    conn.close().map_err(|(_, e)| e)?;
    std::fs::read(&path).context("reading the built graph segment")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_edge(kind: EdgeKind, confidence: f64, location: Option<&str>) -> Edge {
        Edge {
            src: "sym:src.rs#source".into(),
            dst: "sym:dst.rs#target".into(),
            kind,
            provenance: Provenance::Syntactic,
            confidence,
            location: location.map(str::to_string),
        }
    }

    #[test]
    fn graph_build_rejects_conflicting_duplicate_edges_without_locations() {
        let mut graph = Graph {
            nodes: Vec::new(),
            edges: vec![
                test_edge(EdgeKind::Authored, 1.0, None),
                test_edge(EdgeKind::Authored, 0.9, None),
            ],
        };
        deduplicate_identical_edges(&mut graph);

        assert_eq!(
            graph.edges.len(),
            2,
            "conflicting facts are not deduplicated"
        );
        build_graph_sqlite(&graph)
            .expect_err("conflicting duplicate AUTHORED edges must fail the build");
    }

    #[test]
    fn writer_deduplicates_only_identical_edges() {
        let edge = test_edge(EdgeKind::Imports, 0.9, Some("app.py:1:1"));
        let mut graph = Graph {
            nodes: Vec::new(),
            edges: vec![edge.clone(), edge],
        };

        deduplicate_identical_edges(&mut graph);

        assert_eq!(graph.edges.len(), 1);
    }

    #[test]
    fn graph_build_distinguishes_edges_witnessed_at_different_locations() {
        let graph = Graph {
            nodes: Vec::new(),
            edges: vec![
                test_edge(EdgeKind::Calls, 0.9, Some("src.rs:1:1")),
                test_edge(EdgeKind::Calls, 0.9, Some("src.rs:2:1")),
            ],
        };

        build_graph_sqlite(&graph)
            .expect("repeated relationships at distinct source sites are unique edges");
    }

    #[test]
    fn graph_build_enforces_the_documented_confidence_range() {
        for confidence in [-f64::EPSILON, 1.0 + f64::EPSILON, f64::NAN] {
            let graph = Graph {
                nodes: Vec::new(),
                edges: vec![test_edge(EdgeKind::Calls, confidence, None)],
            };
            assert!(
                build_graph_sqlite(&graph).is_err(),
                "confidence {confidence:?} must fail the build"
            );
        }

        for confidence in [0.0, 1.0] {
            let graph = Graph {
                nodes: Vec::new(),
                edges: vec![test_edge(EdgeKind::Calls, confidence, None)],
            };
            build_graph_sqlite(&graph)
                .unwrap_or_else(|error| panic!("confidence {confidence} must be valid: {error:#}"));
        }
    }

    #[test]
    fn contributor_blank_name_is_none_so_the_email_can_stand_in() {
        // A real, non-blank name is kept (and surrounding whitespace trimmed).
        assert_eq!(
            Node::contributor("alice@example.com", "Alice")
                .name
                .as_deref(),
            Some("Alice")
        );
        assert_eq!(
            Node::contributor("bob@example.com", "  Bob  ")
                .name
                .as_deref(),
            Some("Bob")
        );
        // A blank or whitespace-only name becomes None, not Some("") — so
        // the `history` response omits it and the CLI falls back to email
        // rather than printing a blank author.
        assert_eq!(Node::contributor("x@example.com", "").name, None);
        assert_eq!(Node::contributor("y@example.com", "   ").name, None);
    }

    #[tokio::test]
    async fn cancelled_fts_unpack_finishes_accounting_committed_paths() {
        let store = Arc::new(object_store::memory::InMemory::new());
        let published = write_shard(
            store.clone().as_ref(),
            71,
            "cancelled-unpack",
            Graph {
                nodes: vec![Node::file("cancelled.rs")],
                edges: Vec::new(),
            },
            vec![SearchDoc {
                node_id: "file:cancelled.rs".into(),
                kind: NodeKind::File,
                name: Some("cancelled.rs".into()),
                path: Some("cancelled.rs".into()),
                content: "fn materialize_even_if_the_request_goes_away() {}".into(),
            }],
        )
        .await
        .expect("publish cancellation fixture");
        let directory = tempfile::tempdir().expect("cache directory");
        let cache = Arc::new(ShardCache::new(store, directory.path()));
        let seam = Arc::new(FtsUnpackTestSeam {
            repo_id: 71,
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        *FTS_UNPACK_TEST_SEAM
            .lock()
            .expect("FTS unpack test seam lock poisoned") = Some(seam.clone());

        let task_cache = cache.clone();
        let revision = published.revision;
        let request = tokio::spawn(async move { task_cache.fts_path(71, &revision).await });
        seam.entered.notified().await;
        let archive = std::fs::read_dir(directory.path())
            .expect("read cache directory")
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|extension| extension == "tar"))
            .expect("the archive is committed before unpack begins");
        let sha = checksum_from_cache_path(&archive, ".tar").expect("content-addressed archive");

        request.abort();
        let _ = request.await;
        *FTS_UNPACK_TEST_SEAM
            .lock()
            .expect("FTS unpack test seam lock poisoned") = None;
        seam.release.notify_one();

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let accounted = cache
                    .verified
                    .lock()
                    .expect("shard cache lock poisoned")
                    .contains(&CacheKey {
                        sha: sha.clone(),
                        artifact: metrics::Artifact::Fts,
                    });
                let removed = !archive.exists();
                if accounted || removed {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached FTS materialization must finish accounting or cleanup");

        if archive.exists() {
            assert!(
                directory.path().join(format!("{sha}.fts")).is_dir(),
                "an accounted successful materialization includes the unpacked index"
            );
        }
    }

    #[tokio::test]
    async fn failed_fts_unpack_accounts_the_committed_archive() {
        let store = Arc::new(object_store::memory::InMemory::new());
        let repo_id = 72;
        let commit = "invalid-tar";
        let revision = syntactic_revision(commit);
        let invalid_archive = b"checksum-valid bytes that are not a tar archive".to_vec();
        let sha = hex::encode(Sha256::digest(&invalid_archive));
        let manifest = Manifest {
            schema_version: SCHEMA_VERSION,
            commit: commit.into(),
            pass: SYNTACTIC_PASS.into(),
            counts: Counts { nodes: 0, edges: 0 },
            segments: BTreeMap::from([(
                FTS_SEGMENT_FILE.into(),
                Segment {
                    sha256: sha.clone(),
                    bytes: invalid_archive.len() as u64,
                },
            )]),
        };
        store
            .put(
                &fts_segment_key(repo_id, &revision).into(),
                invalid_archive.into(),
            )
            .await
            .expect("put invalid FTS fixture");
        store
            .put(
                &manifest_key(repo_id, &revision).into(),
                serde_json::to_vec(&manifest)
                    .expect("serialize manifest")
                    .into(),
            )
            .await
            .expect("put manifest fixture");

        let directory = tempfile::tempdir().expect("cache directory");
        let cache = ShardCache::new(store, directory.path());
        cache
            .fts_path(repo_id, &revision)
            .await
            .expect_err("an invalid tar cannot materialize an FTS index");

        let archive = directory.path().join(format!("{sha}.tar"));
        assert!(archive.is_file(), "the committed archive remains on disk");
        assert!(
            cache
                .verified
                .lock()
                .expect("shard cache lock poisoned")
                .contains(&CacheKey {
                    sha: sha.clone(),
                    artifact: metrics::Artifact::Fts,
                }),
            "the failed materialization's committed archive is LRU-accounted"
        );
        assert!(!directory.path().join(format!("{sha}.fts")).exists());
        assert!(
            std::fs::read_dir(directory.path())
                .expect("read cache directory")
                .flatten()
                .all(|entry| !entry.file_name().to_string_lossy().contains(".fts.tmp-")),
            "failed unpack staging is removed"
        );
    }
}
