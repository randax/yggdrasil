//! Fuzzy node addressing: parse a bare symbol name plus an explicit repo,
//! resolve it through the indexed FTS segment, and make ambiguity legible
//! using ADR 0006's spread-confidence convention.

use serde::{Deserialize, Serialize};

use crate::{RepoQualifier, ResponseNodeKind, SearchNodeName, SearchPath, VerbId};

/// The client-spelled fuzzy address. Exact node ids never enter this type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FuzzyNodeAddress {
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
/// descending confidence and then canonical node id.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AmbiguousNodeAddress {
    pub resolution: AmbiguousResolution,
    pub address: FuzzyNodeAddress,
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

const SYNTACTIC_MATCH: f64 = 0.9;

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
            candidates: Vec::new(),
        };
        assert_eq!(
            serde_json::to_value(ambiguous).expect("ambiguity serializes")["resolution"],
            "ambiguous"
        );
    }
}
