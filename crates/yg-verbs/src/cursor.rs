//! Opaque pagination cursors: one codec for every Verb (ADR 0003).
//!
//! A cursor is URL-safe base64 over the JSON of a cursor struct. The
//! codec exists exactly once — every Verb's cursor, including search,
//! encodes and decodes through here — so
//! the "opaque, replay-exactly" contract cannot drift between Verbs.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

use crate::search::SearchTarget;
use crate::{Direction, RepoQualifier, SearchPath, SearchRequest, TraversalShape};

/// The longest encoded cursor the codec will decode. The largest
/// legitimate cursor this server mints is a search cursor pinning
/// `MAX_SEARCH_TARGETS` (1000) repos — a few hundred KB encoded — so a
/// megabyte leaves generous headroom while stopping a multi-megabyte
/// forgery before any base64 or JSON work is spent on it.
const MAX_ENCODED_CURSOR_LEN: usize = 1024 * 1024;

/// Encode a cursor struct into its opaque wire form.
pub fn encode<T: Serialize>(cursor: &T) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(cursor).expect("a cursor serializes"))
}

/// Decode an opaque wire cursor. The error is client-facing and the
/// same for every way a cursor can be malformed — a tampered cursor
/// learns nothing about which byte offended, and an oversized one is
/// rejected before any decode work happens.
pub fn decode<T: DeserializeOwned>(cursor: &str) -> Result<T, String> {
    use base64::Engine;
    let invalid =
        || "invalid cursor: pass back next_cursor from a previous response, unmodified".to_string();
    if cursor.len() > MAX_ENCODED_CURSOR_LEN {
        return Err(invalid());
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or_else(invalid)
}

/// What a `neighbors` `next_cursor` opaquely carries: the traversal
/// position, the Shard revision it was read from, and the request shape
/// it belongs to. Later pages stay on the pinned revision — Shards are
/// immutable, so a paginated walk sees one consistent graph even across
/// a pointer swap; only a *fresh* query picks up the new Shard. The
/// request shape rides along because the page contract (unless marked
/// truncated, pages of one traversal union to the full induced subgraph)
/// only holds when every page is computed with identical origin and
/// filters: a replay that contradicts its cursor is rejected, never
/// silently served from a different traversal.
#[derive(Serialize, Deserialize)]
pub(crate) struct NeighborsCursor {
    pub rev: String,
    #[serde(flatten)]
    pub shape: TraversalShape,
    pub after_depth: u32,
    pub after: String,
}

impl NeighborsCursor {
    /// The cursor remembers what the first page was asked; a follow-up
    /// may repeat those fields (in any equivalent spelling) or omit
    /// them, nothing else. Spellings are compared normalized: an
    /// omitted direction and an explicit `"both"` mean the same
    /// traversal, and edge-kind order carries no meaning.
    pub fn agrees_with(&self, req: &TraversalShape) -> Result<(), String> {
        fn direction(spelled: &Option<String>) -> Result<Direction, String> {
            spelled
                .as_deref()
                .map_or(Ok(Direction::default()), Direction::parse)
        }
        fn kind_set(kinds: &Option<Vec<String>>) -> Option<Vec<String>> {
            kinds.clone().map(|mut kinds| {
                kinds.sort_unstable();
                kinds.dedup();
                kinds
            })
        }
        let address_contradicts = (req.repo.is_some() && req.repo != self.shape.repo)
            || (req.path.is_some() && req.path != self.shape.path);
        if address_contradicts {
            return Err(
                "this cursor belongs to a different request (id, repo, path, direction, \
                 edge_kinds, and depth must match the page it came from); start a fresh \
                 traversal or pass the cursor with the original parameters"
                    .to_string(),
            );
        }
        let traversal_contradicts = req.id != self.shape.id
            || (req.direction.is_some()
                && direction(&req.direction)? != direction(&self.shape.direction)?)
            || (req.edge_kinds.is_some()
                && kind_set(&req.edge_kinds) != kind_set(&self.shape.edge_kinds))
            || req
                .depth
                .is_some_and(|d| d != self.shape.depth.unwrap_or(1));
        if traversal_contradicts {
            return Err(
                "this cursor belongs to a different request (id, direction, edge_kinds, \
                 and depth must match the page it came from); start a fresh traversal \
                 or pass the cursor with the original parameters"
                    .to_string(),
            );
        }
        Ok(())
    }
}

/// What a `history` `next_cursor` opaquely carries: the resume position,
/// the Shard revision it was read from, and the request it belongs to.
/// Later pages stay on the pinned revision — Shards are immutable, so a
/// paginated history is one consistent walk even across a pointer swap;
/// only a fresh request picks up a newer Shard. The id and `since` ride
/// along because the page contract only holds when every page is the
/// same query: a replay that contradicts its cursor is rejected, never
/// served from a different history.
#[derive(Serialize, Deserialize)]
pub(crate) struct HistoryCursor {
    pub rev: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<RepoQualifier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<SearchPath>,
    /// The normalized `since` floor (unix seconds) the first page used.
    pub since: Option<i64>,
    pub after_committed_at: i64,
    pub after_sha: String,
}

/// What a search `next_cursor` opaquely carries: the query state and the
/// pinned fan-out set, plus how many hits have already been returned.
/// The field names and types are the compatibility contract for cursors
/// minted before search orchestration moved into the engine.
#[derive(Debug, Serialize, Deserialize)]
pub struct SearchCursor {
    pub(crate) query: String,
    pub(crate) kinds: Option<Vec<SearchKind>>,
    pub(crate) mode: SearchMode,
    pub(crate) targets: Vec<SearchTarget>,
    pub(crate) offset: usize,
}

/// A search mode recorded in an opaque continuation cursor.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum SearchMode {
    Lexical,
    Semantic,
    Hybrid,
    // Unknown values remain representable for cursor wire compatibility: they
    // must reach the existing mode gate instead of becoming decode failures.
    Unknown(String),
}

impl SearchMode {
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Lexical => "lexical",
            Self::Semantic => "semantic",
            Self::Hybrid => "hybrid",
            Self::Unknown(value) => value,
        }
    }

    pub(crate) fn into_string(self) -> String {
        match self {
            Self::Lexical => "lexical".to_string(),
            Self::Semantic => "semantic".to_string(),
            Self::Hybrid => "hybrid".to_string(),
            Self::Unknown(value) => value,
        }
    }
}

impl From<String> for SearchMode {
    fn from(value: String) -> Self {
        match value.as_str() {
            "lexical" => Self::Lexical,
            "semantic" => Self::Semantic,
            "hybrid" => Self::Hybrid,
            _ => Self::Unknown(value),
        }
    }
}

impl From<SearchMode> for String {
    fn from(mode: SearchMode) -> Self {
        mode.into_string()
    }
}

impl PartialEq<&str> for SearchMode {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl std::fmt::Debug for SearchMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_str().fmt(formatter)
    }
}

impl Serialize for SearchMode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SearchMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::from)
    }
}

/// A node-kind spelling recorded in a cursor and validated by the search gate.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct SearchKind(String);

impl SearchKind {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn into_string(self) -> String {
        self.0
    }
}

impl From<String> for SearchKind {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<SearchKind> for String {
    fn from(kind: SearchKind) -> Self {
        kind.into_string()
    }
}

/// Why a continuation cursor cannot be used with a search request.
#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum SearchCursorAgreementError {
    #[error(
        "this cursor belongs to a different query; start a fresh search or pass the cursor without a query"
    )]
    Query,
    #[error(
        "this cursor belongs to a different search mode; page it without a mode, or start a fresh search"
    )]
    Mode,
    #[error(
        "this cursor belongs to a different kinds filter; page it without kinds, or start a fresh search"
    )]
    Kinds,
    #[error(
        "this cursor belongs to a different repos filter; page it without repos, or start a fresh search"
    )]
    Repos,
    #[error("invalid cursor: it names too many repositories")]
    TargetCap,
}

impl SearchCursor {
    /// A resumed search may repeat its pinned query shape in an equivalent
    /// spelling or omit it. Only page size may actually change.
    pub(crate) fn agrees_with(
        &self,
        req: &SearchRequest,
        max_targets: usize,
    ) -> Result<(), SearchCursorAgreementError> {
        fn str_set(items: &[String]) -> std::collections::HashSet<&str> {
            items.iter().map(String::as_str).collect()
        }

        fn kind_set(items: &[SearchKind]) -> std::collections::HashSet<&str> {
            items.iter().map(SearchKind::as_str).collect()
        }

        if req
            .query
            .as_deref()
            .is_some_and(|query| query.trim() != self.query)
        {
            return Err(SearchCursorAgreementError::Query);
        }
        if req
            .mode
            .as_deref()
            .is_some_and(|mode| mode != self.mode.as_str())
        {
            return Err(SearchCursorAgreementError::Mode);
        }
        if req.kinds.as_ref().is_some_and(|kinds| {
            str_set(kinds) != kind_set(self.kinds.as_deref().unwrap_or_default())
        }) {
            return Err(SearchCursorAgreementError::Kinds);
        }
        if req.repos.as_ref().is_some_and(|repos| {
            str_set(repos)
                != self
                    .targets
                    .iter()
                    .map(|target| target.qualifier().as_str())
                    .collect()
        }) {
            return Err(SearchCursorAgreementError::Repos);
        }
        if self.targets.len() > max_targets {
            return Err(SearchCursorAgreementError::TargetCap);
        }
        Ok(())
    }
}

impl HistoryCursor {
    /// A follow-up may repeat the original id and `since` or omit
    /// `since`, nothing else. The `since` is compared already-normalized,
    /// so two spellings of the same instant agree.
    pub fn agrees_with(
        &self,
        req_id: &str,
        req_repo: Option<&RepoQualifier>,
        req_path: Option<&SearchPath>,
        req_since: Option<i64>,
    ) -> Result<(), String> {
        if req_id != self.id
            || req_repo.is_some_and(|repo| Some(repo) != self.repo.as_ref())
            || req_path.is_some_and(|path| Some(path) != self.path.as_ref())
            || (req_since.is_some() && req_since != self.since)
        {
            return Err(
                "this cursor belongs to a different request (id, repo, path, and since must match the \
                 page it came from); start a fresh history or pass the cursor with the \
                 original parameters"
                    .to_string(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_and_reject_tampering() {
        let cursor = HistoryCursor {
            rev: "rev-1".to_string(),
            id: "file:github.com/acme/widgets:main.go".to_string(),
            repo: None,
            path: None,
            since: Some(1_700_000_000),
            after_committed_at: 1_700_000_100,
            after_sha: "abc123".to_string(),
        };
        let wire = encode(&cursor);
        let back: HistoryCursor = decode(&wire).expect("round-trips");
        assert_eq!(back.rev, cursor.rev);
        assert_eq!(back.since, cursor.since);

        for bad in ["", "!!!", "bm90IGpzb24"] {
            assert!(
                decode::<HistoryCursor>(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    /// An oversized cursor is rejected up front — with the same message
    /// as any other malformed cursor — instead of paying for a
    /// multi-megabyte base64 + JSON decode an attacker controls.
    #[test]
    fn oversized_cursors_are_rejected_before_decoding() {
        let huge = "A".repeat(MAX_ENCODED_CURSOR_LEN + 1);
        let err = match decode::<HistoryCursor>(&huge) {
            Err(err) => err,
            Ok(_) => panic!("an oversized cursor must be rejected"),
        };
        assert!(err.contains("invalid cursor"), "uniform message: {err}");
        // The bound is on the encoded form: a legitimate cursor well
        // under it still decodes.
        let cursor = HistoryCursor {
            rev: "rev-1".to_string(),
            id: "file:github.com/acme/widgets:main.go".to_string(),
            repo: None,
            path: None,
            since: None,
            after_committed_at: 1,
            after_sha: "abc".to_string(),
        };
        assert!(decode::<HistoryCursor>(&encode(&cursor)).is_ok());
    }

    #[test]
    fn neighbors_cursor_agreement_normalizes_spellings() {
        let cursor = NeighborsCursor {
            rev: "rev-1".to_string(),
            shape: TraversalShape {
                id: "sym:github.com/acme/widgets:main.go#Hello".to_string(),
                repo: None,
                path: None,
                direction: None,
                edge_kinds: Some(vec!["CALLS".to_string(), "DEFINES".to_string()]),
                depth: None,
            },
            after_depth: 1,
            after: "x".to_string(),
        };
        // Omitted fields, an equivalent direction spelling, and
        // reordered edge kinds all agree.
        let mut req = cursor.shape.clone();
        assert!(cursor.agrees_with(&req).is_ok());
        req.direction = Some("both".to_string());
        req.edge_kinds = Some(vec!["DEFINES".to_string(), "CALLS".to_string()]);
        req.depth = Some(1);
        assert!(cursor.agrees_with(&req).is_ok());
        // A contradicting depth is rejected.
        req.depth = Some(2);
        assert!(cursor.agrees_with(&req).is_err());

        let fuzzy = NeighborsCursor {
            rev: "rev-1".to_string(),
            shape: TraversalShape {
                id: "Hello".to_string(),
                repo: Some(RepoQualifier::new("github.com/acme/widgets".to_string())),
                path: Some(SearchPath::new("src/".to_string())),
                direction: None,
                edge_kinds: None,
                depth: None,
            },
            after_depth: 1,
            after: "x".to_string(),
        };
        let mut resume = fuzzy.shape.clone();
        resume.repo = None;
        resume.path = None;
        assert!(
            fuzzy.agrees_with(&resume).is_ok(),
            "optional address fields may be omitted"
        );
        resume.repo = Some(RepoQualifier::new("github.com/other/repo".to_string()));
        let err = fuzzy
            .agrees_with(&resume)
            .expect_err("an explicit repo must agree");
        assert_eq!(
            err,
            "this cursor belongs to a different request (id, repo, path, direction, \
             edge_kinds, and depth must match the page it came from); start a fresh \
             traversal or pass the cursor with the original parameters"
        );

        resume.repo = fuzzy.shape.repo.clone();
        resume.path = Some(SearchPath::new("tests/".to_string()));
        resume.direction = Some("sideways".to_string());
        assert_eq!(
            fuzzy
                .agrees_with(&resume)
                .expect_err("an explicit path must disagree before direction parsing"),
            err,
            "repo and path disagreements use the fuzzy-address message even with another invalid field"
        );
    }

    #[test]
    fn exact_neighbors_cursor_disagreement_keeps_legacy_message() {
        let cursor = NeighborsCursor {
            rev: "rev-1".to_string(),
            shape: TraversalShape {
                id: "sym:github.com/acme/widgets:main.go#Hello".to_string(),
                repo: None,
                path: None,
                direction: None,
                edge_kinds: None,
                depth: None,
            },
            after_depth: 1,
            after: "x".to_string(),
        };
        let mut request = cursor.shape.clone();
        request.depth = Some(2);

        assert_eq!(
            cursor
                .agrees_with(&request)
                .expect_err("a different depth must be rejected"),
            "this cursor belongs to a different request (id, direction, edge_kinds, \
             and depth must match the page it came from); start a fresh traversal \
             or pass the cursor with the original parameters"
        );
    }

    #[test]
    fn history_cursor_agreement_pins_id_and_since() {
        let cursor = HistoryCursor {
            rev: "rev-1".to_string(),
            id: "file:github.com/acme/widgets:main.go".to_string(),
            repo: None,
            path: None,
            since: Some(100),
            after_committed_at: 200,
            after_sha: "abc".to_string(),
        };
        assert!(
            cursor.agrees_with(&cursor.id, None, None, None).is_ok(),
            "omitted"
        );
        assert!(
            cursor
                .agrees_with(&cursor.id, None, None, Some(100))
                .is_ok(),
            "same"
        );
        assert!(
            cursor
                .agrees_with(&cursor.id, None, None, Some(101))
                .is_err()
        );
        assert!(
            cursor
                .agrees_with("file:other:x", None, None, None)
                .is_err()
        );

        let fuzzy = HistoryCursor {
            rev: "rev-1".to_string(),
            id: "Hello".to_string(),
            repo: Some(RepoQualifier::new("github.com/acme/widgets".to_string())),
            path: Some(SearchPath::new("src/".to_string())),
            since: None,
            after_committed_at: 200,
            after_sha: "abc".to_string(),
        };
        assert!(fuzzy.agrees_with("Hello", None, None, None).is_ok());
        assert!(
            fuzzy
                .agrees_with("Hello", fuzzy.repo.as_ref(), fuzzy.path.as_ref(), None)
                .is_ok()
        );
        let other = RepoQualifier::new("github.com/other/repo".to_string());
        assert!(
            fuzzy
                .agrees_with("Hello", Some(&other), None, None)
                .is_err()
        );
    }

    #[test]
    fn search_cursor_round_trips_the_legacy_shape() {
        let cursor = SearchCursor {
            query: "rate limit".to_string(),
            kinds: Some(vec![SearchKind::from("Symbol".to_string())]),
            mode: SearchMode::Lexical,
            targets: vec![SearchTarget::new(
                7,
                crate::RepoQualifier::new("github.com/acme/widgets".to_string()),
                crate::ShardRevision::new("abc-syntactic-v4".to_string()),
            )],
            offset: 20,
        };
        let encoded = encode(&cursor);
        assert_eq!(
            encoded,
            "eyJxdWVyeSI6InJhdGUgbGltaXQiLCJraW5kcyI6WyJTeW1ib2wiXSwibW9kZSI6ImxleGljYWwiLCJ0YXJnZXRzIjpbeyJyZXBvX2lkIjo3LCJxdWFsaWZpZXIiOiJnaXRodWIuY29tL2FjbWUvd2lkZ2V0cyIsInJldmlzaW9uIjoiYWJjLXN5bnRhY3RpYy12NCJ9XSwib2Zmc2V0IjoyMH0"
        );
        let decoded: SearchCursor = decode(&encoded).expect("round-trips");
        assert_eq!(decoded.query, cursor.query);
        assert_eq!(decoded.kinds, cursor.kinds);
        assert_eq!(decoded.mode, SearchMode::Lexical);
        assert_eq!(decoded.targets[0].repo_id(), 7);
        assert_eq!(
            decoded.targets[0].qualifier().as_str(),
            "github.com/acme/widgets"
        );
        assert_eq!(decoded.offset, 20);
    }

    #[test]
    fn search_cursor_agreement_normalizes_query_and_filter_sets() {
        let cursor = SearchCursor {
            query: "rate limit".to_string(),
            kinds: Some(vec![
                SearchKind::from("File".to_string()),
                SearchKind::from("Symbol".to_string()),
            ]),
            mode: SearchMode::Lexical,
            targets: vec![
                SearchTarget::new(
                    1,
                    crate::RepoQualifier::new("a".to_string()),
                    crate::ShardRevision::new("ra".to_string()),
                ),
                SearchTarget::new(
                    2,
                    crate::RepoQualifier::new("b".to_string()),
                    crate::ShardRevision::new("rb".to_string()),
                ),
            ],
            offset: 20,
        };
        let equivalent = SearchRequest {
            query: Some("  rate limit  ".to_string()),
            kinds: Some(vec!["Symbol".to_string(), "File".to_string()]),
            repos: Some(vec!["b".to_string(), "a".to_string(), "a".to_string()]),
            mode: Some("lexical".to_string()),
            limit: Some(5),
            cursor: None,
        };
        assert!(cursor.agrees_with(&equivalent, 1_000).is_ok());

        let mut different = equivalent;
        different.query = Some("other".to_string());
        assert_eq!(
            cursor.agrees_with(&different, 1_000),
            Err(SearchCursorAgreementError::Query)
        );
        different.query = None;
        different.mode = Some("semantic".to_string());
        assert_eq!(
            cursor.agrees_with(&different, 1_000),
            Err(SearchCursorAgreementError::Mode)
        );
    }

    #[test]
    fn search_cursor_unknown_values_preserve_the_legacy_validation_path() {
        let legacy_json =
            r#"{"query":"q","kinds":["FutureKind"],"mode":"future-mode","targets":[],"offset":0}"#;
        let wire = {
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(legacy_json)
        };
        let cursor: SearchCursor = decode(&wire).expect("unknown values still decode");

        assert_eq!(cursor.mode, SearchMode::Unknown("future-mode".to_string()));
        assert_eq!(
            cursor.kinds.as_deref().expect("kinds")[0].as_str(),
            "FutureKind"
        );
        assert_eq!(
            encode(&cursor),
            wire,
            "unknown spellings serialize unchanged"
        );
        assert_eq!(format!("{:?}", cursor.mode), "\"future-mode\"");
    }

    #[test]
    fn search_cursor_agreement_errors_keep_the_wire_messages() {
        let errors = [
            (
                SearchCursorAgreementError::Query,
                "this cursor belongs to a different query; start a fresh search or pass the cursor without a query",
            ),
            (
                SearchCursorAgreementError::Mode,
                "this cursor belongs to a different search mode; page it without a mode, or start a fresh search",
            ),
            (
                SearchCursorAgreementError::Kinds,
                "this cursor belongs to a different kinds filter; page it without kinds, or start a fresh search",
            ),
            (
                SearchCursorAgreementError::Repos,
                "this cursor belongs to a different repos filter; page it without repos, or start a fresh search",
            ),
            (
                SearchCursorAgreementError::TargetCap,
                "invalid cursor: it names too many repositories",
            ),
        ];

        for (error, expected) in errors {
            assert_eq!(error.to_string(), expected);
        }
    }
}
