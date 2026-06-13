//! Verb engine: pure functions over Shard reads (RFC 0001 §9).
//!
//! Verbs answer over one repo's graph segment, already resolved and
//! cached by the caller (yg-api owns the control-plane lookup): functions
//! here take an open read-only connection plus a parsed [`VerbId`] and
//! never touch the network.
//!
//! External node ids are globally unique (RFC 0001 §5) by embedding the
//! repo qualifier — the Forge root sans scheme joined with the slug —
//! between the kind prefix and the Shard-internal remainder:
//!
//! ```text
//! repo:github.com/acme/widgets
//! file:github.com/acme/widgets:cmd/main.go
//! sym:github.com/acme/widgets:cmd/main.go#Hello
//! pkg:github.com/acme/widgets:golang.org/x/net/html
//! ```
//!
//! Shards store the repo-relative form (`file:cmd/main.go`,
//! `sym:cmd/main.go#Hello`); this crate owns the translation.

use std::collections::BTreeMap;

use anyhow::Context;
use serde::Serialize;

/// Where a qualifier ends inside `rest`: at the first ':', except a
/// colon introducing a port — digits then '/' — which is part of the
/// authority (`git.corp.example:8443/acme/widgets`), pushing the
/// boundary to the next colon. Returns `(qualifier, after)`.
fn split_qualifier(rest: &str) -> Option<(&str, &str)> {
    let first = rest.find(':')?;
    let after = &rest[first + 1..];
    // A port colon sits inside the authority: nothing before it may be
    // a path yet (no '/'), and digits then '/' must follow.
    let port_like = !rest[..first].contains('/')
        && matches!(after.find('/'),
            Some(slash) if slash > 0 && after[..slash].bytes().all(|b| b.is_ascii_digit()));
    let boundary = if port_like {
        first + 1 + after.find(':')?
    } else {
        first
    };
    Some((&rest[..boundary], &rest[boundary + 1..]))
}

/// Whether `qualifier` is a complete qualifier on its own (for `repo:`
/// ids, which carry no local part): colon-free, except a port.
fn whole_qualifier(qualifier: &str) -> bool {
    !qualifier.is_empty() && split_qualifier(qualifier).is_none()
}

/// Whether a repo qualifier is addressable by this grammar: it must
/// round-trip through both the bare form (`repo:<qualifier>`) and the
/// prefixed form (`file:<qualifier>:<path>`). Qualifiers with a colon
/// outside a port — an IPv6 host, a path containing `:` — are not.
/// Registration checks this up front, so an unaddressable forge URL is
/// rejected with a reason instead of indexing a repo no id can reach.
pub fn addressable_qualifier(qualifier: &str) -> bool {
    whole_qualifier(qualifier)
        && split_qualifier(&format!("{qualifier}:x"))
            .is_some_and(|(repo, local)| repo == qualifier && local == "x")
}

/// A parsed external node id: which repo's Shard to ask, and the
/// Shard-internal id within it (`None` for the Repo node itself).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerbId {
    /// Repo qualifier, e.g. `github.com/acme/widgets`.
    pub repo: String,
    /// Shard-internal id, e.g. `sym:cmd/main.go#Hello`.
    pub local: Option<String>,
}

impl VerbId {
    /// Parse an external id. Errors are client-facing: they say what a
    /// well-formed id looks like, not where parsing stopped.
    pub fn parse(id: &str) -> Result<Self, String> {
        let malformed = || {
            format!(
                "malformed node id {id:?}: expected repo:<repo>, \
                 file:<repo>:<path>, sym:<repo>:<path>#<name>, or \
                 pkg:<repo>:<import-path>"
            )
        };
        let (kind, rest) = id.split_once(':').ok_or_else(malformed)?;
        match kind {
            "repo" => {
                if !whole_qualifier(rest) {
                    return Err(malformed());
                }
                Ok(Self {
                    repo: rest.to_string(),
                    local: None,
                })
            }
            "file" | "sym" | "pkg" => {
                let (repo, local) = split_qualifier(rest).ok_or_else(malformed)?;
                if repo.is_empty() || local.is_empty() {
                    return Err(malformed());
                }
                Ok(Self {
                    repo: repo.to_string(),
                    local: Some(format!("{kind}:{local}")),
                })
            }
            _ => Err(malformed()),
        }
    }

    /// The external form of a Shard-internal id within this id's repo:
    /// the repo qualifier spliced in after the kind prefix.
    fn qualify(&self, local: &str) -> String {
        match local.split_once(':') {
            Some((kind, rest)) => format!("{kind}:{}:{rest}", self.repo),
            // Shards only hold prefixed ids; pass anything else through
            // rather than inventing a shape for it.
            None => local.to_string(),
        }
    }

    /// This id's external form.
    pub fn external(&self) -> String {
        match &self.local {
            Some(local) => self.qualify(local),
            None => format!("repo:{}", self.repo),
        }
    }
}

/// A node as Verb responses carry it: the external id plus everything
/// the Shard knows about the node itself.
#[derive(Debug, Serialize)]
pub struct NodeView {
    pub id: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// One edge kind's worth of a node's edges, with how those edges were
/// derived: `{"kind": "DEFINES", "count": 2, "provenance": {"syntactic": 2}}`.
#[derive(Debug, Serialize)]
pub struct EdgeKindSummary {
    pub kind: String,
    pub count: i64,
    /// Edge count per provenance value (CONTEXT.md: how an edge was
    /// derived).
    pub provenance: BTreeMap<String, i64>,
}

/// A node's edges grouped by direction then kind.
#[derive(Debug, Serialize)]
pub struct EdgeSummary {
    #[serde(rename = "in")]
    pub inbound: Vec<EdgeKindSummary>,
    pub out: Vec<EdgeKindSummary>,
}

/// The `node` Verb's answer: full node + edge summary (RFC 0001 §7).
#[derive(Debug, Serialize)]
pub struct NodeResponse {
    pub node: NodeView,
    pub edges: EdgeSummary,
}

/// The `node` Verb: the full node plus a summary of its edges, or `None`
/// when the Shard holds no such node.
pub fn node(conn: &rusqlite::Connection, id: &VerbId) -> anyhow::Result<Option<NodeResponse>> {
    let Some(local) = &id.local else {
        // The Repo node is control-plane reality, not a Shard row: it
        // exists by virtue of the Shard answering at all. M0 Shards hold
        // no repo-level edges.
        return Ok(Some(NodeResponse {
            node: NodeView {
                id: id.external(),
                kind: "Repo".to_string(),
                name: None,
                path: None,
            },
            edges: EdgeSummary {
                inbound: vec![],
                out: vec![],
            },
        }));
    };

    let found = conn
        .query_row(
            "SELECT kind, name, path FROM nodes WHERE id = ?1",
            [local],
            |row| {
                Ok(NodeView {
                    id: id.external(),
                    kind: row.get(0)?,
                    name: row.get(1)?,
                    path: row.get(2)?,
                })
            },
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            e => Err(e),
        })
        .context("reading the node")?;
    let Some(node) = found else { return Ok(None) };

    Ok(Some(NodeResponse {
        node,
        edges: EdgeSummary {
            inbound: edge_summary(conn, "dst", local)?,
            out: edge_summary(conn, "src", local)?,
        },
    }))
}

/// An edge as Verb responses carry it: endpoints in external form, plus
/// how the edge was derived and how sure the deriving pass was.
#[derive(Debug, Serialize)]
pub struct GraphEdge {
    pub src: String,
    pub dst: String,
    pub kind: String,
    pub provenance: String,
    pub confidence: f64,
    /// Where the edge was witnessed (`<path>:<line>`, repo-relative,
    /// 1-based), for edges that have a site — a CALLS edge's call site.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

/// One page of the `neighbors` Verb's answer: the next slice of the
/// origin's subgraph (RFC 0001 §7) in traversal order, plus every edge
/// joining this page to the subgraph already returned. The pages of one
/// traversal union to the full induced subgraph, each edge exactly once.
///
/// `next` is the resume position — `(depth, external id)` of the last
/// node here — for the caller to wrap into its opaque cursor; `None`
/// means the traversal is exhausted.
#[derive(Debug, Serialize)]
pub struct NeighborsPage {
    pub nodes: Vec<NodeView>,
    pub edges: Vec<GraphEdge>,
    #[serde(skip)]
    pub next: Option<(u32, String)>,
}

/// Which of a node's edges `neighbors` follows: those it points out
/// along, those pointing in at it, or both (the default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    In,
    Out,
    #[default]
    Both,
}

impl Direction {
    /// Parse the wire form. The error is client-facing.
    pub fn parse(direction: &str) -> Result<Self, String> {
        match direction {
            "in" => Ok(Self::In),
            "out" => Ok(Self::Out),
            "both" => Ok(Self::Both),
            other => Err(format!(
                "unknown direction {other:?}: expected \"in\", \"out\", or \"both\""
            )),
        }
    }
}

/// Every edge kind a Shard can hold, as the wire spells them — the
/// vocabulary `edge_kinds` filters validate against, so a typo
/// (`CALL`, lowercase `calls`) errors instead of silently matching
/// nothing. Must agree with yg-shard's `EdgeKind::as_str` values; a
/// drift-guard test in yg-api holds the two together (this crate
/// deliberately doesn't depend on the artifact writer).
pub const KNOWN_EDGE_KINDS: &[&str] = &["CALLS", "DEFINES", "EXTENDS", "IMPLEMENTS", "IMPORTS"];

/// The `neighbors` Verb's filters and pagination, validated by the
/// caller (limits, depth bounds, cursor decoding are wire concerns).
#[derive(Debug, Clone)]
pub struct NeighborsOptions {
    pub direction: Direction,
    /// Only follow edges of these kinds; `None` follows every kind.
    pub edge_kinds: Option<Vec<String>>,
    /// How many hops to traverse (RFC 0001 §7: ≤ 3).
    pub depth: u32,
    /// Page size, in nodes.
    pub limit: usize,
    /// Resume after this `(depth, external id)` position, from the
    /// previous page's [`NeighborsPage::next`].
    pub after: Option<(u32, String)>,
}

impl Default for NeighborsOptions {
    fn default() -> Self {
        Self {
            direction: Direction::default(),
            edge_kinds: None,
            depth: 1,
            limit: 100,
            after: None,
        }
    }
}

impl NeighborsOptions {
    /// The bounds every transport must hold `neighbors` to (RFC 0001
    /// §7 mandates identical Verbs across REST, MCP, and CLI, so the
    /// check lives with the options, not in any one transport). Errors
    /// are client-facing.
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=3).contains(&self.depth) {
            return Err(format!("depth must be between 1 and 3, got {}", self.depth));
        }
        if !(1..=1000).contains(&self.limit) {
            return Err(format!(
                "limit must be between 1 and 1000, got {}",
                self.limit
            ));
        }
        // An empty list is ambiguous ("no kinds" vs "no filter") and
        // would render as invalid SQL (`IN ()`): the client must say
        // which one they mean.
        if self
            .edge_kinds
            .as_ref()
            .is_some_and(|kinds| kinds.is_empty())
        {
            return Err(
                "edge_kinds must name at least one kind; omit it to follow every kind".to_string(),
            );
        }
        // A kind no Shard holds would silently match zero edges —
        // indistinguishable from a genuinely isolated node.
        if let Some(unknown) = self
            .edge_kinds
            .iter()
            .flatten()
            .find(|kind| !KNOWN_EDGE_KINDS.contains(&kind.as_str()))
        {
            return Err(format!(
                "unknown edge kind {unknown:?}: expected any of {}",
                KNOWN_EDGE_KINDS.join(", ")
            ));
        }
        Ok(())
    }
}

/// The `neighbors` Verb: one page of the origin's subgraph within
/// `options.depth` hops, or `None` when the Shard holds no such origin.
///
/// The traversal is breadth-first following only edges that pass the
/// direction and kind filters, with nodes ordered by `(depth, id)` — a
/// total, Shard-stable order, so a cursor resumed against the same
/// revision sees exactly the pages a single uninterrupted read would
/// have. Each edge belongs to the page of its later-ordered endpoint:
/// every edge of the induced subgraph arrives exactly once.
pub fn neighbors(
    conn: &rusqlite::Connection,
    id: &VerbId,
    options: &NeighborsOptions,
) -> anyhow::Result<Option<NeighborsPage>> {
    let empty = NeighborsPage {
        nodes: vec![],
        edges: vec![],
        next: None,
    };
    let Some(local) = &id.local else {
        // The Repo node exists (see `node`), and M0 Shards hold no
        // repo-level edges: an empty subgraph, not a missing origin.
        return Ok(Some(empty));
    };
    let origin_exists: bool = conn
        .query_row("SELECT count(*) FROM nodes WHERE id = ?1", [local], |row| {
            row.get::<_, i64>(0).map(|n| n > 0)
        })
        .context("looking up the origin node")?;
    if !origin_exists {
        return Ok(None);
    }

    // Breadth-first over the filtered edges, recomputed per page: the
    // Shard is immutable, so the traversal is deterministic and a page
    // is just a window into it. Each node's incident edges are queried
    // at most once per request (the memo serves both discovery and the
    // edge phase below); levels past the page's window are never
    // expanded — deeper nodes can only sort after it.
    let mut memo = EdgeMemo::default();
    let mut order: Vec<(u32, String, String)> = Vec::new(); // (depth, external, local)
    let mut rank: std::collections::HashMap<String, usize> =
        std::collections::HashMap::from([(local.clone(), 0)]);
    let mut frontier: Vec<String> = vec![local.clone()];
    // The window starts after the cursor position. A position that
    // isn't in the traversal (a cursor replayed with different filters,
    // say) degrades gracefully: the search's insertion point resumes at
    // the next position in order.
    let window_start = |order: &[(u32, String, String)]| match &options.after {
        Some((depth, external)) => order
            .binary_search_by(|(d, ext, _)| (d, ext.as_str()).cmp(&(depth, external.as_str())))
            .map(|i| i + 1)
            .unwrap_or_else(|i| i),
        None => 0,
    };
    for depth in 1..=options.depth {
        let mut discovered: Vec<(String, String)> = Vec::new(); // (external, local)
        for near in &frontier {
            for edge in memo.edges_of(conn, near, options.edge_kinds.as_deref())? {
                if !follows(edge, near, options.direction) {
                    continue;
                }
                let far = edge.other_end(near);
                if !rank.contains_key(far) {
                    rank.insert(far.to_string(), usize::MAX); // placed below
                    discovered.push((id.qualify(far), far.to_string()));
                }
            }
        }
        // Sorted by *external* id — the order the cursor's binary search
        // runs over; local-id order need not coincide for future kinds.
        discovered.sort_unstable();
        for (external, far) in &discovered {
            let position = order.len() + 1; // origin holds 0
            *rank.get_mut(far).expect("just inserted") = position;
            order.push((depth, external.clone(), far.clone()));
        }
        frontier = discovered.into_iter().map(|(_, local)| local).collect();
        if frontier.is_empty() {
            break;
        }
        if order.len() > window_start(&order) + options.limit {
            // The window is already full and overflowing: `next` below
            // sees more-than-a-page either way, and deeper levels only
            // append after the window.
            break;
        }
    }

    let start = window_start(&order);
    let page = &order[start.min(order.len())..(start + options.limit).min(order.len())];
    let next = if start + options.limit < order.len() {
        page.last()
            .map(|(depth, external, _)| (*depth, external.clone()))
    } else {
        None
    };

    // Page edges: every kind-filtered edge joining a page node to
    // anything earlier in the traversal (origin, earlier pages, or
    // earlier in this page), regardless of orientation — the direction
    // filter constrains which edges the traversal *crosses*, not which
    // edges of the reached subgraph are reported; an induced subgraph
    // with its back-edges hidden would silently lie about connectivity.
    // A self-loop's "other end" is the node itself, earlier than
    // nothing: it belongs to its own node's page (the origin's to the
    // first page, below).
    let mut edges = Vec::new();
    let mut push_edge = |edge: &RawEdge| {
        edges.push(GraphEdge {
            src: id.qualify(&edge.src),
            dst: id.qualify(&edge.dst),
            kind: edge.kind.clone(),
            provenance: edge.provenance.clone(),
            confidence: edge.confidence,
            location: edge.location.clone(),
        });
    };
    if start == 0 {
        for edge in memo.edges_of(conn, local, options.edge_kinds.as_deref())? {
            if edge.src == edge.dst {
                push_edge(edge);
            }
        }
    }
    for (_, _, local) in page {
        let my_rank = rank[local];
        for edge in memo.edges_of(conn, local, options.edge_kinds.as_deref())? {
            let qualifies = edge.src == edge.dst
                || rank
                    .get(edge.other_end(local))
                    .is_some_and(|&far_rank| far_rank < my_rank);
            if qualifies {
                push_edge(edge);
            }
        }
    }

    let nodes = node_views(conn, page)?;
    Ok(Some(NeighborsPage { nodes, edges, next }))
}

/// Whether the traversal may cross `edge` outward from `near`.
fn follows(edge: &RawEdge, near: &str, direction: Direction) -> bool {
    match direction {
        Direction::Out => edge.src == near,
        Direction::In => edge.dst == near,
        Direction::Both => true, // incident means one endpoint is `near`
    }
}

/// One page's nodes, hydrated in one batched query and returned in page
/// order. A page id the Shard's nodes table doesn't hold means an edge
/// points at a node that doesn't exist — fail loudly, naming it.
fn node_views(
    conn: &rusqlite::Connection,
    page: &[(u32, String, String)],
) -> anyhow::Result<Vec<NodeView>> {
    if page.is_empty() {
        return Ok(vec![]);
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT id, kind, name, path FROM nodes WHERE id IN ({})",
        vec!["?"; page.len()].join(", ")
    ))?;
    let rows = stmt
        .query_map(
            rusqlite::params_from_iter(page.iter().map(|(_, _, local)| local)),
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ),
                ))
            },
        )?
        .collect::<Result<std::collections::HashMap<_, _>, _>>()
        .context("reading the page's nodes")?;
    page.iter()
        .map(|(_, external, local)| {
            let (kind, name, path) = rows.get(local).with_context(|| {
                format!("the Shard has an edge to {local} but no such node; refusing to serve an inconsistent graph")
            })?;
            Ok(NodeView {
                id: external.clone(),
                kind: kind.clone(),
                name: name.clone(),
                path: path.clone(),
            })
        })
        .collect()
}

/// Per-request cache of [`incident_edges`] results: discovery and the
/// page-edge phase ask about overlapping nodes, and the Shard is
/// immutable for the duration.
#[derive(Default)]
struct EdgeMemo(std::collections::HashMap<String, Vec<RawEdge>>);

impl EdgeMemo {
    fn edges_of(
        &mut self,
        conn: &rusqlite::Connection,
        local: &str,
        edge_kinds: Option<&[String]>,
    ) -> anyhow::Result<&[RawEdge]> {
        if !self.0.contains_key(local) {
            let edges = incident_edges(conn, local, edge_kinds)?;
            self.0.insert(local.to_string(), edges);
        }
        Ok(self.0.get(local).expect("just inserted"))
    }
}

/// A node's edges in either direction that pass the kind filter, in
/// stored (Shard-internal) form, deterministically ordered. Direction
/// filtering happens in [`follows`], against whichever endpoint the
/// caller is standing on.
fn incident_edges(
    conn: &rusqlite::Connection,
    local: &str,
    edge_kinds: Option<&[String]>,
) -> anyhow::Result<Vec<RawEdge>> {
    // The kind filter is appended as placeholders, never spliced values.
    let mut params: Vec<&dyn rusqlite::ToSql> = vec![&local as &dyn rusqlite::ToSql];
    let kinds = match edge_kinds {
        Some(kinds) => {
            params.extend(kinds.iter().map(|kind| kind as &dyn rusqlite::ToSql));
            format!(" AND kind IN ({})", vec!["?"; kinds.len()].join(", "))
        }
        None => String::new(),
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT src, dst, kind, provenance, confidence, location FROM edges
         WHERE (src = ?1 OR dst = ?1){kinds}
         ORDER BY src, dst, kind, location"
    ))?;
    let edges = stmt
        .query_map(rusqlite::params_from_iter(params), |row| {
            Ok(RawEdge {
                src: row.get(0)?,
                dst: row.get(1)?,
                kind: row.get(2)?,
                provenance: row.get(3)?,
                confidence: row.get(4)?,
                location: row.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .context("reading a node's edges")?;
    Ok(edges)
}

/// An edge as the Shard stores it: repo-relative endpoint ids.
struct RawEdge {
    src: String,
    dst: String,
    kind: String,
    provenance: String,
    confidence: f64,
    location: Option<String>,
}

impl RawEdge {
    /// The endpoint that isn't `near` (for a self-loop, `near` itself) —
    /// the one definition both traversal phases share.
    fn other_end(&self, near: &str) -> &str {
        if self.src == near {
            &self.dst
        } else {
            &self.src
        }
    }
}

/// Summarize one direction of a node's edges: per kind, the edge count
/// and its breakdown by provenance, ordered by kind for stable output.
fn edge_summary(
    conn: &rusqlite::Connection,
    end: &str,
    local: &str,
) -> anyhow::Result<Vec<EdgeKindSummary>> {
    // `end` is one of two literals chosen above, never client input.
    let mut stmt = conn.prepare(&format!(
        "SELECT kind, provenance, count(*) FROM edges
         WHERE {end} = ?1 GROUP BY kind, provenance ORDER BY kind"
    ))?;
    let rows = stmt.query_map([local], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut summaries: Vec<EdgeKindSummary> = Vec::new();
    for row in rows {
        let (kind, provenance, count) = row.context("summarizing edges")?;
        match summaries.last_mut() {
            Some(last) if last.kind == kind => {
                last.count += count;
                last.provenance.insert(provenance, count);
            }
            _ => summaries.push(EdgeKindSummary {
                kind,
                count,
                provenance: BTreeMap::from([(provenance, count)]),
            }),
        }
    }
    Ok(summaries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_with_port_bearing_qualifiers_round_trip() {
        let id = "sym:git.corp.example:8443/acme/widgets:cmd/main.go#Hello";
        let parsed = VerbId::parse(id).unwrap();
        assert_eq!(parsed.repo, "git.corp.example:8443/acme/widgets");
        assert_eq!(parsed.local.as_deref(), Some("sym:cmd/main.go#Hello"));
        assert_eq!(parsed.external(), id);

        let repo = VerbId::parse("repo:git.corp.example:8443/acme/widgets").unwrap();
        assert_eq!(repo.repo, "git.corp.example:8443/acme/widgets");
        assert_eq!(repo.local, None);
    }

    #[test]
    fn pkg_ids_round_trip_with_slashes_in_the_import_path() {
        let id = "pkg:github.com/acme/widgets:golang.org/x/net/html";
        let parsed = VerbId::parse(id).unwrap();
        assert_eq!(parsed.repo, "github.com/acme/widgets");
        assert_eq!(parsed.local.as_deref(), Some("pkg:golang.org/x/net/html"));
        assert_eq!(parsed.external(), id);
    }

    #[test]
    fn ids_with_path_qualifiers_round_trip() {
        // file:// fixtures: the qualifier is a filesystem path, whose
        // leading '/' must not be mistaken for an authority.
        let id = "file:/tmp/fixtures/acme/widgets:main.go";
        let parsed = VerbId::parse(id).unwrap();
        assert_eq!(parsed.repo, "/tmp/fixtures/acme/widgets");
        assert_eq!(parsed.local.as_deref(), Some("file:main.go"));
        assert_eq!(parsed.external(), id);
    }

    #[test]
    fn malformed_ids_are_rejected_with_the_expected_shapes() {
        for bad in [
            "not-an-id",
            "repo:",
            "sym:no-local-part",
            "file::empty-repo",
            "sym:repo:",
            "unknown:repo:x",
            // A non-port colon inside a repo: id's qualifier.
            "repo:github.com/a:b",
        ] {
            assert!(VerbId::parse(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn neighbors_options_bounds_are_enforced_for_every_transport() {
        let ok = NeighborsOptions::default();
        assert!(ok.validate().is_ok());
        for (depth, limit) in [(0, 100), (4, 100), (1, 0), (1, 1001)] {
            let options = NeighborsOptions {
                depth,
                limit,
                ..NeighborsOptions::default()
            };
            assert!(options.validate().is_err(), "depth={depth} limit={limit}");
        }
        // An empty kind filter would render as invalid SQL (`IN ()`).
        let options = NeighborsOptions {
            edge_kinds: Some(vec![]),
            ..NeighborsOptions::default()
        };
        assert!(options.validate().is_err(), "empty edge_kinds");
    }

    #[test]
    fn unknown_edge_kinds_are_rejected_with_the_vocabulary() {
        // A typo'd kind would otherwise match zero edges silently.
        for bad in ["CALL", "calls", "DEFINED"] {
            let options = NeighborsOptions {
                edge_kinds: Some(vec![bad.to_string()]),
                ..NeighborsOptions::default()
            };
            let reason = options.validate().expect_err(bad);
            assert!(
                reason.contains(bad) && reason.contains("CALLS"),
                "names the typo and the vocabulary: {reason}"
            );
        }
        let options = NeighborsOptions {
            edge_kinds: Some(vec!["CALLS".to_string(), "IMPORTS".to_string()]),
            ..NeighborsOptions::default()
        };
        assert!(options.validate().is_ok(), "known kinds pass");
    }
}
