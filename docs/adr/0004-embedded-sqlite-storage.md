Status: superseded by ADR-0005

# Embedded SQLite storage, sharded per repo

The Knowledge Graph lives in embedded SQLite (graph tables + FTS5 full-text + sqlite-vec vectors), one shard per repo plus an org-level shard for cross-repo edges, all behind an internal storage interface. Because the public contract is bounded verbs (ADR 0003), we don't need a graph engine — we need fast indexed traversals and a single-binary deployment story for self-hosting Admins. Single-writer SQLite is acceptable because the sync pipeline is the only writer, and per-repo sharding makes indexing parallel and bounds re-index blast radius.

Rejected: Postgres (second moving part in every deployment; revisit if multi-node indexing workers arrive), Kuzu (company shut down mid-2025, community-fork risk, and we'd be buying a Cypher engine ADR 0003 forbids exposing), custom RocksDB/badger (storage-layer engineering before the first verb works).
