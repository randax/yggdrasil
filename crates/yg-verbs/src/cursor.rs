//! Opaque pagination cursors: one codec for every Verb (ADR 0003).
//!
//! A cursor is URL-safe base64 over the JSON of a cursor struct. The
//! codec exists exactly once — every Verb's cursor, including the
//! transport-owned search cursor, encodes and decodes through here — so
//! the "opaque, replay-exactly" contract cannot drift between Verbs.

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{Direction, TraversalShape};

/// Encode a cursor struct into its opaque wire form.
pub fn encode<T: Serialize>(cursor: &T) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(cursor).expect("a cursor serializes"))
}

/// Decode an opaque wire cursor. The error is client-facing and the
/// same for every way a cursor can be malformed — a tampered cursor
/// learns nothing about which byte offended.
pub fn decode<T: DeserializeOwned>(cursor: &str) -> Result<T, String> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or_else(|| {
            "invalid cursor: pass back next_cursor from a previous response, unmodified".to_string()
        })
}

/// What a `neighbors` `next_cursor` opaquely carries: the traversal
/// position, the Shard revision it was read from, and the request shape
/// it belongs to. Later pages stay on the pinned revision — Shards are
/// immutable, so a paginated walk sees one consistent graph even across
/// a pointer swap; only a *fresh* query picks up the new Shard. The
/// request shape rides along because the page contract ("pages of one
/// traversal union to the full induced subgraph") only holds when every
/// page is computed with identical origin and filters: a replay that
/// contradicts its cursor is rejected, never silently served from a
/// different traversal.
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
        let contradicts = req.id != self.shape.id
            || (req.direction.is_some()
                && direction(&req.direction)? != direction(&self.shape.direction)?)
            || (req.edge_kinds.is_some()
                && kind_set(&req.edge_kinds) != kind_set(&self.shape.edge_kinds))
            || req
                .depth
                .is_some_and(|d| d != self.shape.depth.unwrap_or(1));
        if contradicts {
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
    /// The normalized `since` floor (unix seconds) the first page used.
    pub since: Option<i64>,
    pub after_committed_at: i64,
    pub after_sha: String,
}

impl HistoryCursor {
    /// A follow-up may repeat the original id and `since` or omit
    /// `since`, nothing else. The `since` is compared already-normalized,
    /// so two spellings of the same instant agree.
    pub fn agrees_with(&self, req_id: &str, req_since: Option<i64>) -> Result<(), String> {
        if req_id != self.id || (req_since.is_some() && req_since != self.since) {
            return Err(
                "this cursor belongs to a different request (id and since must match the \
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

    #[test]
    fn neighbors_cursor_agreement_normalizes_spellings() {
        let cursor = NeighborsCursor {
            rev: "rev-1".to_string(),
            shape: TraversalShape {
                id: "sym:github.com/acme/widgets:main.go#Hello".to_string(),
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
    }

    #[test]
    fn history_cursor_agreement_pins_id_and_since() {
        let cursor = HistoryCursor {
            rev: "rev-1".to_string(),
            id: "file:github.com/acme/widgets:main.go".to_string(),
            since: Some(100),
            after_committed_at: 200,
            after_sha: "abc".to_string(),
        };
        assert!(cursor.agrees_with(&cursor.id, None).is_ok(), "omitted");
        assert!(cursor.agrees_with(&cursor.id, Some(100)).is_ok(), "same");
        assert!(cursor.agrees_with(&cursor.id, Some(101)).is_err());
        assert!(cursor.agrees_with("file:other:x", None).is_err());
    }
}
