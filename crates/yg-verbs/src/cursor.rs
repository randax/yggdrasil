//! Opaque pagination cursors: one codec for every Verb (ADR 0003).
//!
//! A cursor is a versioned, URL-safe payload authenticated with
//! HMAC-SHA256. The codec exists exactly once — every Verb's cursor,
//! including search, encodes and decodes through here — so
//! the "opaque, replay-exactly" contract cannot drift between Verbs.

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::Sha256;
use thiserror::Error;

use crate::search::SearchTarget;
use crate::{Direction, RepoQualifier, SearchPath, SearchRequest, TraversalShape};

/// The longest encoded cursor the codec will decode. The largest
/// legitimate cursor this server mints is a search cursor pinning
/// `MAX_SEARCH_TARGETS` (1000) repos — a few hundred KB encoded — so a
/// megabyte leaves generous headroom while stopping a multi-megabyte
/// forgery before any base64 or JSON work is spent on it.
const MAX_ENCODED_CURSOR_LEN: usize = 1024 * 1024;
const VERSION: &str = "v1";
const MAC_DOMAIN: &[u8] = b"yg.cursor.v1\0";
pub const MIN_CURSOR_SECRET_LEN: usize = 32;

/// Secret key used only to authenticate pagination cursors.
///
/// It deliberately has no `Debug` or `Display` implementation so config
/// reports cannot accidentally expose it.
#[derive(Clone, PartialEq, Eq)]
pub struct CursorSecret(Vec<u8>);

impl CursorSecret {
    pub fn new(secret: impl Into<Vec<u8>>) -> Result<Self, CursorSecretError> {
        let secret = secret.into();
        if secret.len() < MIN_CURSOR_SECRET_LEN {
            return Err(CursorSecretError {
                actual_len: secret.len(),
            });
        }
        Ok(Self(secret))
    }
}

impl TryFrom<String> for CursorSecret {
    type Error = CursorSecretError;

    fn try_from(secret: String) -> Result<Self, Self::Error> {
        Self::new(secret.into_bytes())
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("cursor secret must contain at least {MIN_CURSOR_SECRET_LEN} bytes, got {actual_len}")]
pub struct CursorSecretError {
    actual_len: usize,
}

/// A cursor failure kept typed until the transport maps it to its 400-class
/// error. Signature failures intentionally disclose no payload details.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CursorError {
    #[error(
        "invalid cursor: signature verification failed; pass back next_cursor from a previous response, unmodified"
    )]
    InvalidSignature,
    #[error(
        "invalid cursor: authenticated payload is malformed; pass back next_cursor from a previous response, unmodified"
    )]
    MalformedPayload,
    #[error(
        "this cursor belongs to a different request (id, repo, path, direction, edge_kinds, and depth must match the page it came from); start a fresh traversal or pass the cursor with the original parameters"
    )]
    NeighborsAddressMismatch,
    #[error(
        "this cursor belongs to a different request (id, direction, edge_kinds, and depth must match the page it came from); start a fresh traversal or pass the cursor with the original parameters"
    )]
    NeighborsShapeMismatch,
    #[error(
        "this cursor belongs to a different request (id, repo, path, and since must match the page it came from); start a fresh history or pass the cursor with the original parameters"
    )]
    HistoryShapeMismatch,
    #[error(
        "this cursor belongs to a different query; start a fresh search or pass the cursor without a query"
    )]
    SearchQueryMismatch,
    #[error(
        "this cursor belongs to a different search mode; page it without a mode, or start a fresh search"
    )]
    SearchModeMismatch,
    #[error(
        "this cursor belongs to a different kinds filter; page it without kinds, or start a fresh search"
    )]
    SearchKindsMismatch,
    #[error(
        "this cursor belongs to a different repos filter; page it without repos, or start a fresh search"
    )]
    SearchReposMismatch,
    #[error("invalid cursor: it names too many repositories")]
    SearchTargetCap,
}

/// The only cursor encoder/decoder. Owning the secret prevents transports
/// from accidentally minting unsigned continuations.
#[derive(Clone)]
pub struct CursorCodec {
    secret: CursorSecret,
}

impl CursorCodec {
    pub fn new(secret: CursorSecret) -> Self {
        Self { secret }
    }

    /// Encode JSON payload bytes and authenticate their exact representation.
    pub fn encode<T: Serialize>(&self, cursor: &T) -> String {
        use base64::Engine;

        let payload = serde_json::to_vec(cursor).expect("a cursor serializes");
        let signature = self.sign(&payload);
        let base64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{VERSION}.{}.{}",
            base64.encode(payload),
            base64.encode(signature)
        )
    }

    /// Verify the signature before parsing any client-controlled payload.
    pub fn decode<T: DeserializeOwned>(&self, cursor: &str) -> Result<T, CursorError> {
        use base64::Engine;

        if cursor.len() > MAX_ENCODED_CURSOR_LEN {
            return Err(CursorError::InvalidSignature);
        }
        let mut parts = cursor.split('.');
        let (Some(version), Some(payload), Some(signature), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(CursorError::InvalidSignature);
        };
        if version != VERSION {
            return Err(CursorError::InvalidSignature);
        }
        let base64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let payload = base64
            .decode(payload)
            .map_err(|_| CursorError::InvalidSignature)?;
        let signature = base64
            .decode(signature)
            .map_err(|_| CursorError::InvalidSignature)?;
        let mut mac = self.mac();
        mac.update(MAC_DOMAIN);
        mac.update(&payload);
        mac.verify_slice(&signature)
            .map_err(|_| CursorError::InvalidSignature)?;
        serde_json::from_slice(&payload).map_err(|_| CursorError::MalformedPayload)
    }

    fn sign(&self, payload: &[u8]) -> Vec<u8> {
        let mut mac = self.mac();
        mac.update(MAC_DOMAIN);
        mac.update(payload);
        mac.finalize().into_bytes().to_vec()
    }

    fn mac(&self) -> Hmac<Sha256> {
        Hmac::<Sha256>::new_from_slice(&self.secret.0)
            .expect("HMAC accepts every non-empty key length")
    }
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
/// silently served from a different traversal. The truncation flag is
/// page-local, so callers must OR it across all pages when deciding
/// whether that union is complete.
#[derive(Serialize, Deserialize)]
pub(crate) struct NeighborsCursor {
    pub repo_id: i64,
    pub rev: crate::ShardRevision,
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
    pub fn agrees_with(&self, req: &TraversalShape) -> Result<(), CursorError> {
        fn direction(spelled: &Option<String>) -> Result<Direction, CursorError> {
            spelled
                .as_deref()
                .map_or(Ok(Direction::default()), Direction::parse)
                .map_err(|_| CursorError::NeighborsShapeMismatch)
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
            return Err(CursorError::NeighborsAddressMismatch);
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
            return Err(CursorError::NeighborsShapeMismatch);
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
    pub repo_id: i64,
    pub rev: crate::ShardRevision,
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

impl SearchCursor {
    /// A resumed search may repeat its pinned query shape in an equivalent
    /// spelling or omit it. Only page size may actually change.
    pub(crate) fn agrees_with(
        &self,
        req: &SearchRequest,
        max_targets: usize,
    ) -> Result<(), CursorError> {
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
            return Err(CursorError::SearchQueryMismatch);
        }
        if req
            .mode
            .as_deref()
            .is_some_and(|mode| mode != self.mode.as_str())
        {
            return Err(CursorError::SearchModeMismatch);
        }
        if req.kinds.as_ref().is_some_and(|kinds| {
            str_set(kinds) != kind_set(self.kinds.as_deref().unwrap_or_default())
        }) {
            return Err(CursorError::SearchKindsMismatch);
        }
        if req.repos.as_ref().is_some_and(|repos| {
            str_set(repos)
                != self
                    .targets
                    .iter()
                    .map(|target| target.qualifier().as_str())
                    .collect()
        }) {
            return Err(CursorError::SearchReposMismatch);
        }
        if self.targets.len() > max_targets {
            return Err(CursorError::SearchTargetCap);
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
    ) -> Result<(), CursorError> {
        if req_id != self.id
            || req_repo.is_some_and(|repo| Some(repo) != self.repo.as_ref())
            || req_path.is_some_and(|path| Some(path) != self.path.as_ref())
            || (req_since.is_some() && req_since != self.since)
        {
            return Err(CursorError::HistoryShapeMismatch);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SECRET: &[u8] = b"issue-63-test-cursor-secret-32-bytes";

    fn codec() -> CursorCodec {
        CursorCodec::new(CursorSecret::new(TEST_SECRET).expect("test secret is non-empty"))
    }

    #[test]
    fn cursor_secrets_require_cryptographic_key_length() {
        assert!(matches!(
            CursorSecret::new(b"guessable".to_vec()),
            Err(CursorSecretError { actual_len: 9 })
        ));
        assert!(CursorSecret::new(vec![0; MIN_CURSOR_SECRET_LEN]).is_ok());
    }

    #[test]
    fn cursors_round_trip_and_reject_tampering() {
        let cursor = HistoryCursor {
            repo_id: 1,
            rev: crate::ShardRevision::new("rev-1".to_string()),
            id: "file:github.com/acme/widgets:main.go".to_string(),
            repo: None,
            path: None,
            since: Some(1_700_000_000),
            after_committed_at: 1_700_000_100,
            after_sha: "abc123".to_string(),
        };
        let codec = codec();
        let wire = codec.encode(&cursor);
        let back: HistoryCursor = codec.decode(&wire).expect("round-trips");
        assert_eq!(back.rev, cursor.rev);
        assert_eq!(back.since, cursor.since);

        let mut tampered = wire.into_bytes();
        let signature_start = tampered
            .iter()
            .rposition(|byte| *byte == b'.')
            .expect("signed cursor has a signature")
            + 1;
        tampered[signature_start] = if tampered[signature_start] == b'A' {
            b'B'
        } else {
            b'A'
        };
        let tampered = String::from_utf8(tampered).expect("cursor remains ASCII");
        assert!(matches!(
            codec.decode::<HistoryCursor>(&tampered),
            Err(CursorError::InvalidSignature)
        ));

        for bad in ["", "!!!", "bm90IGpzb24"] {
            assert!(
                codec.decode::<HistoryCursor>(bad).is_err(),
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
        let codec = codec();
        let err = match codec.decode::<HistoryCursor>(&huge) {
            Err(err) => err,
            Ok(_) => panic!("an oversized cursor must be rejected"),
        };
        assert!(
            err.to_string().contains("invalid cursor"),
            "uniform message: {err}"
        );
        // The bound is on the encoded form: a legitimate cursor well
        // under it still decodes.
        let cursor = HistoryCursor {
            repo_id: 1,
            rev: crate::ShardRevision::new("rev-1".to_string()),
            id: "file:github.com/acme/widgets:main.go".to_string(),
            repo: None,
            path: None,
            since: None,
            after_committed_at: 1,
            after_sha: "abc".to_string(),
        };
        assert!(
            codec
                .decode::<HistoryCursor>(&codec.encode(&cursor))
                .is_ok()
        );
    }

    #[test]
    fn neighbors_cursor_agreement_normalizes_spellings() {
        let cursor = NeighborsCursor {
            repo_id: 1,
            rev: crate::ShardRevision::new("rev-1".to_string()),
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
            repo_id: 1,
            rev: crate::ShardRevision::new("rev-1".to_string()),
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
        assert_eq!(err, CursorError::NeighborsAddressMismatch);
        assert_eq!(
            err.to_string(),
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
            repo_id: 1,
            rev: crate::ShardRevision::new("rev-1".to_string()),
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
                .expect_err("a different depth must be rejected")
                .to_string(),
            "this cursor belongs to a different request (id, direction, edge_kinds, \
             and depth must match the page it came from); start a fresh traversal \
             or pass the cursor with the original parameters"
        );
    }

    #[test]
    fn history_cursor_agreement_pins_id_and_since() {
        let cursor = HistoryCursor {
            repo_id: 1,
            rev: crate::ShardRevision::new("rev-1".to_string()),
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
            repo_id: 1,
            rev: crate::ShardRevision::new("rev-1".to_string()),
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
    fn search_cursor_round_trips_as_a_structurally_valid_signed_payload() {
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
        let codec = codec();
        let encoded = codec.encode(&cursor);
        let mut parts = encoded.split('.');
        assert_eq!(parts.next(), Some("v1"));
        let payload = {
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(parts.next().expect("payload segment"))
                .expect("payload is base64url")
        };
        let signature = {
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(parts.next().expect("signature segment"))
                .expect("signature is base64url")
        };
        assert!(
            parts.next().is_none(),
            "the wire has exactly three segments"
        );
        let value: serde_json::Value = serde_json::from_slice(&payload).expect("payload JSON");
        assert_eq!(value["query"], "rate limit");
        assert_eq!(value["mode"], "lexical");
        assert_eq!(value["targets"][0]["repo_id"], 7);
        assert_eq!(value["targets"][0]["qualifier"], "github.com/acme/widgets");
        assert_eq!(value["targets"][0]["revision"], "abc-syntactic-v4");
        assert_eq!(value["offset"], 20);
        let mut mac = Hmac::<Sha256>::new_from_slice(TEST_SECRET).expect("valid HMAC key");
        mac.update(MAC_DOMAIN);
        mac.update(&payload);
        mac.verify_slice(&signature)
            .expect("signature verifies under the test secret");

        let decoded: SearchCursor = codec.decode(&encoded).expect("round-trips");
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
            Err(CursorError::SearchQueryMismatch)
        );
        different.query = None;
        different.mode = Some("semantic".to_string());
        assert_eq!(
            cursor.agrees_with(&different, 1_000),
            Err(CursorError::SearchModeMismatch)
        );
    }

    #[test]
    fn unsigned_legacy_search_cursor_is_a_typed_signature_error() {
        let legacy_json =
            r#"{"query":"q","kinds":["FutureKind"],"mode":"future-mode","targets":[],"offset":0}"#;
        let wire = {
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(legacy_json)
        };
        assert!(matches!(
            codec().decode::<SearchCursor>(&wire),
            Err(CursorError::InvalidSignature)
        ));
    }

    #[test]
    fn search_cursor_agreement_errors_keep_the_wire_messages() {
        let errors = [
            (
                CursorError::SearchQueryMismatch,
                "this cursor belongs to a different query; start a fresh search or pass the cursor without a query",
            ),
            (
                CursorError::SearchModeMismatch,
                "this cursor belongs to a different search mode; page it without a mode, or start a fresh search",
            ),
            (
                CursorError::SearchKindsMismatch,
                "this cursor belongs to a different kinds filter; page it without kinds, or start a fresh search",
            ),
            (
                CursorError::SearchReposMismatch,
                "this cursor belongs to a different repos filter; page it without repos, or start a fresh search",
            ),
            (
                CursorError::SearchTargetCap,
                "invalid cursor: it names too many repositories",
            ),
        ];

        for (error, expected) in errors {
            assert_eq!(error.to_string(), expected);
        }
    }
}
