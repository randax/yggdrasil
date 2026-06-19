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

/// One shipped Verb's public tool metadata. The schema is stored with the
/// Verb engine so REST, CLI, and MCP can point at the same Verb names and
/// request shapes instead of maintaining separate tool inventories.
pub struct VerbTool {
    pub verb: Verb,
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Node,
    Neighbors,
    Search,
    History,
}

impl Verb {
    pub fn tool(self) -> &'static VerbTool {
        VERB_TOOLS
            .iter()
            .find(|tool| tool.verb == self)
            .expect("every Verb enum variant has tool metadata")
    }
}

pub fn verb_tool(name: &str) -> Option<&'static VerbTool> {
    VERB_TOOLS.iter().find(|tool| tool.name == name)
}

/// The shipped Verb catalog. MCP tool listing and calling both resolve
/// through this catalog; REST and CLI still own their transport-specific
/// parsing and rendering around these shared request shapes.
pub const VERB_TOOLS: &[VerbTool] = &[
    VerbTool {
        verb: Verb::Node,
        name: "node",
        description: "Return one Knowledge Graph node with inbound and outbound edge summaries.",
        input_schema: r#"{
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Node id, e.g. sym:github.com/acme/widgets:main.go#Hello"
                }
            },
            "required": ["id"]
        }"#,
    },
    VerbTool {
        verb: Verb::Neighbors,
        name: "neighbors",
        description: "Return a node's neighboring subgraph with edge details and pagination.",
        input_schema: r#"{
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Node id to traverse from"
                },
                "direction": {
                    "type": "string",
                    "enum": ["in", "out", "both"],
                    "description": "Which edge direction to follow"
                },
                "edge_kinds": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Edge kinds to follow, e.g. DEFINES or CALLS"
                },
                "depth": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 3,
                    "description": "Traversal depth"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 1000,
                    "description": "Nodes per page"
                },
                "cursor": {
                    "type": "string",
                    "description": "Opaque cursor returned by a previous neighbors call"
                }
            },
            "required": ["id"]
        }"#,
    },
    VerbTool {
        verb: Verb::Search,
        name: "search",
        description: "Search indexed repos for symbols, files, and docs with ranked node ids.",
        input_schema: r#"{
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query"
                },
                "kinds": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Node kinds to search, e.g. Symbol or File"
                },
                "repos": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Repo qualifiers to search"
                },
                "mode": {
                    "type": "string",
                    "enum": ["lexical", "semantic", "hybrid"],
                    "description": "Search mode; lexical is available in this release"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100,
                    "description": "Hits per page"
                },
                "cursor": {
                    "type": "string",
                    "description": "Opaque cursor returned by a previous search call"
                }
            },
            "anyOf": [
                {"required": ["query"]},
                {"required": ["cursor"]}
            ]
        }"#,
    },
    VerbTool {
        verb: Verb::History,
        name: "history",
        description: "Return commits touching a file node or a symbol's defining file.",
        input_schema: r#"{
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "id": {
                    "type": "string",
                    "description": "File or Symbol node id"
                },
                "since": {
                    "type": "string",
                    "description": "RFC3339 timestamp or YYYY-MM-DD lower bound"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 1000,
                    "description": "Commits per page"
                },
                "cursor": {
                    "type": "string",
                    "description": "Opaque cursor returned by a previous history call"
                }
            },
            "required": ["id"]
        }"#,
    },
];

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
                 file:<repo>:<path>, sym:<repo>:<path>#<name>, \
                 pkg:<repo>:<import-path>, commit:<repo>:<sha>, or \
                 contributor:<repo>:<email>"
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
            "file" | "sym" | "pkg" | "commit" | "contributor" => {
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
/// the Shard knows about the node itself. A Commit's committer date is
/// deliberately not here — it is surfaced by the `history` Verb, the one
/// place commit dates are answered; `node`/`neighbors` describe structure.
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
    /// Where the edge was witnessed (`<path>:<line>:<col>`,
    /// repo-relative, 1-based; `col` is a byte offset within the line),
    /// for edges that have a site — a CALLS edge's call site.
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
pub const KNOWN_EDGE_KINDS: &[&str] = &[
    "AUTHORED",
    "CALLS",
    "DEFINES",
    "EXTENDS",
    "IMPLEMENTS",
    "IMPORTS",
    "TOUCHES",
];

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

/// One commit in the `history` Verb's answer: the commit, when it landed,
/// what it was, and who wrote it (RFC 0001 §7).
#[derive(Debug, Serialize)]
pub struct HistoryCommit {
    /// External commit id (`commit:<repo>:<sha>`).
    pub commit: String,
    /// The bare commit sha.
    pub sha: String,
    /// First line of the commit message, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Committer date in unix seconds — the newest-first ordering key. The
    /// wire layer renders it RFC3339; the cursor carries it raw.
    pub committed_at: i64,
    /// The author, when the commit carried an attributable email.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<HistoryAuthor>,
}

/// A commit's author, as a Contributor reference (CONTEXT.md): deduped by
/// email within this repo's Shard — cross-Forge identity merge is M2.
#[derive(Debug, Serialize)]
pub struct HistoryAuthor {
    /// External contributor id (`contributor:<repo>:<email>`).
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub email: String,
}

/// One page of the `history` Verb's answer: commits newest-first.
///
/// `next` is the resume position — `(committed_at, sha)` of the last
/// commit here — for the caller to wrap into its opaque cursor; `None`
/// means the history is exhausted. The sha (not the external id) is
/// carried because it is repo-independent and orders identically to the
/// Shard-internal `commit:<sha>` id the keyset resumes on.
#[derive(Debug, Serialize)]
pub struct HistoryPage {
    pub commits: Vec<HistoryCommit>,
    #[serde(skip)]
    pub next: Option<(i64, String)>,
}

/// The `history` Verb's filters and pagination, validated by the caller
/// (the page size and `since` spelling are wire concerns).
#[derive(Debug, Clone)]
pub struct HistoryOptions {
    /// Page size, in commits.
    pub limit: usize,
    /// Only commits whose committer date is at or after this (unix
    /// seconds); `None` imposes no lower bound.
    pub since: Option<i64>,
    /// Resume after this `(committed_at, sha)` position — the previous
    /// page's last commit, in the `(committer date desc, id asc)` order.
    pub after: Option<(i64, String)>,
}

/// The `history` Verb: commits that touched a File — or a Symbol's
/// defining file — newest-first, or `None` when the Shard holds no such
/// node (the caller's 404).
///
/// Commits are ordered by committer date descending, the external commit
/// id breaking ties — a total, Shard-stable order, so a cursor resumed
/// against the same immutable revision sees exactly the page a single
/// uninterrupted read would have.
pub fn history(
    conn: &rusqlite::Connection,
    id: &VerbId,
    options: &HistoryOptions,
) -> anyhow::Result<Option<HistoryPage>> {
    let Some(local) = &id.local else {
        // The Repo node has no file to take a history of (M0).
        return Ok(None);
    };
    let Some(file_local) = resolve_history_file(conn, local)? else {
        return Ok(None);
    };

    // Assembled with `?` placeholders bound in push order: the `since`
    // floor and the keyset resume clause are optional, so the SQL grows
    // to match. Fetch one past the page so the presence of a next page is
    // known without a second query. AUTHORED is a left join: an
    // unattributable commit (no author email) still appears, authorless.
    let mut sql = String::from(
        "SELECT c.id, c.name, c.committed_at, a.src, who.name
         FROM edges t
         JOIN nodes c ON c.id = t.src
         LEFT JOIN edges a ON a.dst = c.id AND a.kind = 'AUTHORED'
         LEFT JOIN nodes who ON who.id = a.src
         WHERE t.kind = 'TOUCHES' AND t.dst = ?",
    );
    // The keyset resumes on the Shard-internal `commit:<sha>` id, which
    // orders identically to the bare sha the cursor carries.
    let after_id = options
        .after
        .as_ref()
        .map(|(_, sha)| format!("commit:{sha}"));
    let after_at = options.after.as_ref().map(|(at, _)| *at);
    // The caller bounds `limit`, but this is a public API: a huge `usize`
    // from another caller must neither overflow the +1 nor wrap negative on
    // the i64 cast (SQLite reads a negative LIMIT as unlimited). Saturate,
    // then clamp into i64 — a limit no real page ever reaches.
    let limit_plus_one = i64::try_from(options.limit.saturating_add(1)).unwrap_or(i64::MAX);
    let mut params: Vec<&dyn rusqlite::ToSql> = vec![&file_local];
    if let Some(since) = options.since.as_ref() {
        sql.push_str(" AND c.committed_at >= ?");
        params.push(since);
    }
    // A row falls after `(at, id)` in the (date desc, id asc) order when
    // it is strictly older, or same-date with a later id.
    if let (Some(at), Some(id)) = (after_at.as_ref(), after_id.as_ref()) {
        sql.push_str(" AND (c.committed_at < ? OR (c.committed_at = ? AND c.id > ?))");
        params.push(at);
        params.push(at);
        params.push(id);
    }
    sql.push_str(" ORDER BY c.committed_at DESC, c.id ASC LIMIT ?");
    params.push(&limit_plus_one);

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()
        .context("reading a file's commit history")?;

    let mut commits: Vec<HistoryCommit> = rows
        .into_iter()
        .map(
            |(commit_local, subject, committed_at, author_local, author_name)| {
                history_commit(
                    id,
                    &commit_local,
                    subject,
                    committed_at,
                    author_local,
                    author_name,
                )
            },
        )
        .collect();
    // A full extra row means there is another page; trim to it and hand
    // back the last commit's (date, sha) as the resume point.
    let next = if commits.len() > options.limit {
        commits.truncate(options.limit);
        commits
            .last()
            .map(|last| (last.committed_at, last.sha.clone()))
    } else {
        None
    };
    Ok(Some(HistoryPage { commits, next }))
}

/// The Shard-internal File id whose history answers for `local`: the File
/// itself, or a Symbol's defining file (RFC 0001 §7) — the Symbol carries
/// that file's path, so its history is the file's. A node the Shard
/// doesn't hold, or one that is neither (a Package, Commit, Contributor),
/// yields `None` — the caller's 404, since M0 history is file-anchored.
fn resolve_history_file(
    conn: &rusqlite::Connection,
    local: &str,
) -> anyhow::Result<Option<String>> {
    let found = conn
        .query_row(
            "SELECT kind, path FROM nodes WHERE id = ?1",
            [local],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            e => Err(e),
        })
        .context("looking up the history target")?;
    let Some((kind, path)) = found else {
        return Ok(None);
    };
    match kind.as_str() {
        "File" => Ok(Some(local.to_string())),
        // The Symbol's path is its defining file; map it to that File id.
        // A Symbol without a path (which the writer never mints) has no
        // file history to give.
        "Symbol" => Ok(path.map(|path| format!("file:{path}"))),
        _ => Ok(None),
    }
}

/// Assemble one [`HistoryCommit`] from its stored row, qualifying the
/// commit and contributor ids into their external form and splitting the
/// sha and email back out of the Shard-internal ids.
fn history_commit(
    id: &VerbId,
    commit_local: &str,
    subject: Option<String>,
    committed_at: i64,
    author_local: Option<String>,
    author_name: Option<String>,
) -> HistoryCommit {
    let sha = commit_local
        .strip_prefix("commit:")
        .unwrap_or(commit_local)
        .to_string();
    let author = author_local.map(|local| HistoryAuthor {
        email: local
            .strip_prefix("contributor:")
            .unwrap_or(&local)
            .to_string(),
        id: id.qualify(&local),
        name: author_name,
    });
    HistoryCommit {
        commit: id.qualify(commit_local),
        sha,
        subject,
        committed_at,
        author,
    }
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
    fn contributor_ids_round_trip_with_colons_in_the_email() {
        // A Contributor id carries the author's email as its local part,
        // and an email can contain a colon. The repo qualifier always holds
        // a '/', so the repo/local boundary lands at the first colon and the
        // whole email survives — even one that itself looks port-like.
        for email in [
            "alice@example.com",
            "weird:user@example.com",
            "9999/bot@x.io",
        ] {
            let id = format!("contributor:github.com/acme/widgets:{email}");
            let parsed = VerbId::parse(&id).unwrap_or_else(|e| panic!("{id:?}: {e}"));
            assert_eq!(parsed.repo, "github.com/acme/widgets");
            assert_eq!(
                parsed.local.as_deref(),
                Some(format!("contributor:{email}").as_str())
            );
            assert_eq!(parsed.external(), id, "{id:?} must round-trip");
        }
    }

    #[test]
    fn commit_ids_round_trip() {
        let id = "commit:github.com/acme/widgets:1a2b3c4d5e6f";
        let parsed = VerbId::parse(id).unwrap();
        assert_eq!(parsed.repo, "github.com/acme/widgets");
        assert_eq!(parsed.local.as_deref(), Some("commit:1a2b3c4d5e6f"));
        assert_eq!(parsed.external(), id);
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
