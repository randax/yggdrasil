# Object-storage-native shards with a Postgres control plane

Supersedes ADR-0004. Designing for 5000+ repos and 500+ Members made the single-box premise untenable: one machine can't run SCIP builds for 5000 repos overnight, and cold per-query fan-out across thousands of embedded FTS shards can't meet search latency. The per-repo shard insight survives; the box doesn't.

Each repo's index is an immutable shard artifact (graph + tantivy full-text + vector ANN) written to S3-compatible object storage by stateless indexing workers; re-indexing writes a new shard and atomically swaps a pointer. Stateless query nodes serve verbs by mmapping hot shards through an NVMe/memory cache tier (the turbopuffer/Zoekt pattern). Postgres is the control plane: repo registry, sync state, job queue (SKIP LOCKED), Members/tokens, and the cross-repo edge index that bounds query fan-out.

Consequences: minimum deployment grows from one binary to binary + Postgres + S3-compatible store (MinIO/Garage self-hostable); all server roles (API/query, worker, all-in-one) remain modes of one binary. Rejected: everything-in-Postgres (billions-row occurrence tables plus an inevitable search sidecar), specialist cluster zoo (graph DB + OpenSearch + qdrant + Redis — four systems to operate).
