//! Fuzzy node addressing: parse a bare symbol name plus an explicit repo,
//! resolve it byte-exactly through the FTS raw-name index, and make ambiguity
//! legible by borrowing the syntactic pass's ADR 0006 spread-confidence
//! convention.

use serde::{Deserialize, Serialize};
use yg_shard::SYNTACTIC_MATCH;

use crate::{RepoQualifier, ResponseNodeKind, SearchNodeName, SearchPath, VerbId};

/// The client-spelled fuzzy address. Exact node ids never enter this type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FuzzyNodeAddress {
    /// Stored symbol name matched byte-for-byte and case-sensitively.
    pub name: SearchNodeName,
    pub repo: RepoQualifier,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<SearchPath>,
}

/// One ranked declaration offered for an ambiguous fuzzy address.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeCandidate {
    pub id: VerbId,
    pub kind: ResponseNodeKind,
    pub path: SearchPath,
    pub confidence: f64,
}

/// A fuzzy address matched several declarations. Candidates are ordered by
/// descending confidence and then canonical node id, and may be a bounded
/// prefix of the full match set.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AmbiguousNodeAddress {
    pub resolution: AmbiguousResolution,
    pub address: FuzzyNodeAddress,
    /// Exact number of declarations matched before rendering was capped.
    pub total_matches: usize,
    pub candidates: Vec<NodeCandidate>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AmbiguousResolution {
    Ambiguous,
}

/// The typed payload carried by a fuzzy-address 404.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoSuchSymbol {
    pub kind: NoSuchSymbolKind,
    pub address: FuzzyNodeAddress,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoSuchSymbolKind {
    NoSuchSymbol,
    /// More declarations share the exact name than the bounded address lookup
    /// may safely inspect.
    UnaddressableSymbol,
}

/// A node-addressed Verb either resolved normally or returns the ranked set
/// the caller must choose from. Untagged serialization is load-bearing:
/// `Resolved` is byte-identical to the pre-fuzzy response.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AddressedResponse<T> {
    Resolved(T),
    Ambiguous(AmbiguousNodeAddress),
}

pub(crate) enum NodeAddress {
    Exact(VerbId),
    Fuzzy(FuzzyNodeAddress),
}

/// Maximum UTF-8 byte length accepted for a fuzzy symbol name.
///
/// This mirrors the lexical-search query guard and bounds tokenization work
/// before a request can cause any Shard to be resolved or opened.
pub(crate) const MAX_ADDRESS_NAME_BYTES: usize = 1024;

pub(crate) fn parse_address(
    id: &str,
    repo: Option<RepoQualifier>,
    path: Option<SearchPath>,
) -> Result<NodeAddress, String> {
    if let Ok(id) = VerbId::parse(id) {
        if repo.is_some() || path.is_some() {
            return Err("repo and path are only valid with a bare symbol name".to_string());
        }
        return Ok(NodeAddress::Exact(id));
    }
    let name = id.trim();
    if name.is_empty() || name != id {
        return Err("a fuzzy node address needs a non-empty bare symbol name".to_string());
    }
    if name.len() > MAX_ADDRESS_NAME_BYTES {
        return Err(format!(
            "fuzzy symbol name is {} bytes; the limit is {MAX_ADDRESS_NAME_BYTES}",
            name.len()
        ));
    }
    let repo = repo
        .filter(|repo| !repo.as_str().is_empty())
        .ok_or_else(|| format!("a bare symbol name {id:?} needs the repo field"))?;
    let path = path
        .map(|path| {
            if path.as_str().is_empty() {
                Err("a fuzzy path fragment must not be empty".to_string())
            } else {
                Ok(path)
            }
        })
        .transpose()?;
    Ok(NodeAddress::Fuzzy(FuzzyNodeAddress {
        name: SearchNodeName::new(name.to_string()),
        repo,
        path,
    }))
}

/// Maximum number of ambiguous address candidates rendered on the wire.
pub const MAX_ADDRESS_CANDIDATES: usize = 25;

pub(crate) fn rank_candidates(
    address: &FuzzyNodeAddress,
    symbols: Vec<yg_shard::LocalSymbol>,
) -> Vec<NodeCandidate> {
    let confidence = SYNTACTIC_MATCH / symbols.len().max(1) as f64;
    let mut candidates = symbols
        .into_iter()
        .map(|symbol| NodeCandidate {
            id: VerbId {
                repo: address.repo.as_str().to_string(),
                local: Some(symbol.node_id.as_str().to_string()),
            },
            kind: ResponseNodeKind::Symbol,
            path: SearchPath::new(symbol.path.as_str().to_string()),
            confidence,
        })
        .collect::<Vec<_>>();
    candidates.sort_unstable_by_key(|candidate| candidate.id.external());
    candidates.truncate(MAX_ADDRESS_CANDIDATES);
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ambiguity_spreads_syntactic_confidence_and_orders_ids() {
        let address = FuzzyNodeAddress {
            name: SearchNodeName::new("Resolve".to_string()),
            repo: RepoQualifier::new("github.com/acme/widgets".to_string()),
            path: None,
        };
        let candidates = rank_candidates(
            &address,
            vec![
                yg_shard::LocalSymbol {
                    node_id: yg_shard::LocalSymbolId::new("sym:z.go#Resolve".to_string()),
                    path: yg_shard::LocalSymbolPath::new("z.go".to_string()),
                },
                yg_shard::LocalSymbol {
                    node_id: yg_shard::LocalSymbolId::new("sym:a.go#Resolve".to_string()),
                    path: yg_shard::LocalSymbolPath::new("a.go".to_string()),
                },
            ],
        );
        assert_eq!(candidates.len(), 2);
        assert_eq!(
            candidates[0].id.external(),
            "sym:github.com/acme/widgets:a.go#Resolve"
        );
        for candidate in candidates {
            assert!((candidate.confidence - 0.45).abs() < 1e-9);
        }
    }

    #[test]
    fn rendered_candidates_are_capped_without_changing_confidence() {
        let address = FuzzyNodeAddress {
            name: SearchNodeName::new("Resolve".to_string()),
            repo: RepoQualifier::new("github.com/acme/widgets".to_string()),
            path: None,
        };
        let symbols = (0..=MAX_ADDRESS_CANDIDATES)
            .rev()
            .map(|index| yg_shard::LocalSymbol {
                node_id: yg_shard::LocalSymbolId::new(format!("sym:{index:02}.go#Resolve")),
                path: yg_shard::LocalSymbolPath::new(format!("{index:02}.go")),
            })
            .collect::<Vec<_>>();

        let candidates = rank_candidates(&address, symbols);

        assert_eq!(candidates.len(), MAX_ADDRESS_CANDIDATES);
        assert_eq!(
            candidates.first().expect("first candidate").id.external(),
            "sym:github.com/acme/widgets:00.go#Resolve"
        );
        assert_eq!(
            candidates
                .last()
                .expect("last rendered candidate")
                .id
                .external(),
            "sym:github.com/acme/widgets:24.go#Resolve"
        );
        let expected = SYNTACTIC_MATCH / (MAX_ADDRESS_CANDIDATES + 1) as f64;
        assert!(
            candidates
                .iter()
                .all(|candidate| (candidate.confidence - expected).abs() < 1e-9),
            "confidence uses the exact match count, not the rendered cap"
        );
    }

    #[test]
    fn exact_ids_win_before_fuzzy_parsing() {
        let address = parse_address("sym:github.com/acme/widgets:main.go#Hello", None, None)
            .expect("exact id parses");
        assert!(matches!(address, NodeAddress::Exact(_)));
    }

    #[test]
    fn resolved_outcomes_do_not_add_a_wire_wrapper() {
        let resolved = AddressedResponse::Resolved(serde_json::json!({
            "node": {"id": "sym:github.com/acme/widgets:a.go#Resolve"}
        }));
        assert_eq!(
            serde_json::to_string(&resolved).expect("resolved outcome serializes"),
            r#"{"node":{"id":"sym:github.com/acme/widgets:a.go#Resolve"}}"#
        );
    }

    #[test]
    fn fuzzy_failures_have_distinct_typed_wire_discriminators() {
        let address = FuzzyNodeAddress {
            name: SearchNodeName::new("Resolve".to_string()),
            repo: RepoQualifier::new("github.com/acme/widgets".to_string()),
            path: None,
        };
        let missing = NoSuchSymbol {
            kind: NoSuchSymbolKind::NoSuchSymbol,
            address: address.clone(),
        };
        assert_eq!(
            serde_json::to_value(missing).expect("missing payload serializes")["kind"],
            "no_such_symbol"
        );
        let ambiguous = AmbiguousNodeAddress {
            resolution: AmbiguousResolution::Ambiguous,
            address,
            total_matches: 2,
            candidates: Vec::new(),
        };
        let ambiguous = serde_json::to_value(ambiguous).expect("ambiguity serializes");
        assert_eq!(ambiguous["resolution"], "ambiguous");
        assert_eq!(ambiguous["total_matches"], 2);
    }
}
