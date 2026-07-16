use anyhow::Context;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::VerbId;

/// A typed repository qualifier on the search resolver seam.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct RepoQualifier(String);

impl RepoQualifier {
    /// Wrap a qualifier parsed by the deployment's control-plane boundary.
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// The qualifier's canonical wire spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A typed immutable Shard revision on the search resolver seam.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ShardRevision(String);

impl ShardRevision {
    /// Wrap an immutable revision parsed by the deployment boundary.
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// The revision's canonical storage spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One repo in a search's pinned fan-out set.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SearchTarget {
    pub(super) repo_id: i64,
    pub(super) qualifier: RepoQualifier,
    pub(super) revision: ShardRevision,
}

/// How a search target entered the current request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchTargetProvenance {
    /// The control plane enumerated the target during this request.
    FreshlyEnumerated,
    /// A signed cursor restored the target from an earlier request.
    ResumedFromCursor,
}

impl SearchTarget {
    /// Build one pinned search target from typed resolver values.
    pub fn new(repo_id: i64, qualifier: RepoQualifier, revision: ShardRevision) -> Self {
        Self {
            repo_id,
            qualifier,
            revision,
        }
    }

    /// The control-plane repository identifier used by the segment cache.
    pub fn repo_id(&self) -> i64 {
        self.repo_id
    }

    /// The qualifier prepended to this repository's local node ids.
    pub fn qualifier(&self) -> &RepoQualifier {
        &self.qualifier
    }

    /// The immutable Shard revision pinned by the search cursor.
    pub fn revision(&self) -> &ShardRevision {
        &self.revision
    }
}

/// A node display name returned by search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SearchNodeName(String);

impl SearchNodeName {
    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }
    /// The node name as indexed.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A repository-relative node path returned by search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct SearchPath(String);

impl SearchPath {
    pub fn new(value: String) -> Self {
        Self(value)
    }
    /// The repository-relative path spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A highlighted search excerpt returned for a content hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SearchSnippet(String);

impl SearchSnippet {
    pub(super) fn new(value: String) -> Self {
        Self(value)
    }
    /// The highlighted HTML excerpt.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One ranked search hit.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchHit {
    #[serde(
        serialize_with = "serialize_verb_id",
        deserialize_with = "deserialize_verb_id"
    )]
    pub id: VerbId,
    #[serde(
        serialize_with = "serialize_node_kind",
        deserialize_with = "deserialize_node_kind"
    )]
    pub kind: yg_shard::NodeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<SearchNodeName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<SearchPath>,
    pub repo: RepoQualifier,
    #[serde(serialize_with = "f32_shortest")]
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<SearchSnippet>,
}

/// The search answer as it appears on every transport boundary.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchWireResponse {
    pub hits: Vec<SearchHit>,
    pub next_cursor: Option<String>,
}

impl super::SearchResponse {
    /// The one place the typed continuation becomes its encoded wire
    /// form, shared by every transport.
    pub(crate) fn into_wire(self, cursors: &crate::cursor::CursorCodec) -> SearchWireResponse {
        let next_cursor = self.next.as_ref().map(|cursor| cursors.encode(cursor));
        SearchWireResponse {
            hits: self.hits,
            next_cursor,
        }
    }
}

#[derive(Debug)]
pub struct SearchResponse {
    /// The requested page in deterministic merged rank order.
    pub hits: Vec<SearchHit>,
    /// Typed continuation for the next page, or `None` when exhausted.
    pub next: Option<crate::cursor::SearchCursor>,
}

pub(super) fn qualify_hit(qualifier: &str, hit: yg_shard::LocalHit) -> anyhow::Result<SearchHit> {
    let kind = yg_shard::NodeKind::parse(&hit.kind)
        .with_context(|| format!("an FTS hit has unknown node kind {:?}", hit.kind))?;
    Ok(SearchHit {
        id: VerbId {
            repo: qualifier.to_string(),
            local: Some(hit.node_id),
        },
        kind,
        name: hit.name.map(SearchNodeName::new),
        path: hit.path.map(SearchPath::new),
        repo: RepoQualifier::new(qualifier.to_string()),
        score: hit.score,
        snippet: hit.snippet.map(SearchSnippet::new),
    })
}

fn serialize_verb_id<S: serde::Serializer>(id: &VerbId, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&id.external())
}

fn deserialize_verb_id<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<VerbId, D::Error> {
    let id = String::deserialize(deserializer)?;
    VerbId::parse(&id).map_err(serde::de::Error::custom)
}

fn serialize_node_kind<S: serde::Serializer>(
    kind: &yg_shard::NodeKind,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(kind.as_str())
}

fn deserialize_node_kind<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<yg_shard::NodeKind, D::Error> {
    let kind = String::deserialize(deserializer)?;
    yg_shard::NodeKind::parse(&kind)
        .ok_or_else(|| serde::de::Error::custom(format!("unknown node kind {kind:?}")))
}

/// Preserve the existing shortest-f32 wire representation when canonical JSON first converts this response through `serde_json::Value`.
fn f32_shortest<S: serde::Serializer>(value: &f32, serializer: S) -> Result<S::Ok, S::Error> {
    let shortest: f64 = value
        .to_string()
        .parse()
        .unwrap_or_else(|_| f64::from(*value));
    serializer.serialize_f64(shortest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_hits_keep_the_existing_wire_scalars() {
        let hit = qualify_hit(
            "github.com/acme/widgets",
            yg_shard::LocalHit {
                node_id: "sym:main.go#RateLimit".to_string(),
                kind: "Symbol".to_string(),
                name: Some("RateLimit".to_string()),
                path: Some("main.go".to_string()),
                score: 5.480_152,
                snippet: Some("<b>RateLimit</b>".to_string()),
            },
        )
        .expect("fixture hit qualifies");
        assert_eq!(
            serde_json::to_string(&hit).expect("serializes"),
            r#"{"id":"sym:github.com/acme/widgets:main.go#RateLimit","kind":"Symbol","name":"RateLimit","path":"main.go","repo":"github.com/acme/widgets","score":5.480152,"snippet":"<b>RateLimit</b>"}"#
        );
    }
}
