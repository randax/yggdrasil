//! The full-text segment (RFC 0001 §6): a tantivy index over a repo's
//! Symbol and File nodes, packed into a single content-addressed artifact
//! so it lives in a Shard exactly like `graph.sqlite` — one segment file,
//! one checksum, materialized by the cache tier and read in-process.
//!
//! The lexical search Verb reads it: a query returns ranked hits whose
//! node ids feed straight into `node`/`neighbors`. Writer and reader share
//! this one schema definition, so they cannot drift.

use std::io::Read;
use std::path::Path;

use anyhow::Context;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions, Value,
};
use tantivy::snippet::SnippetGenerator;
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument, Term};

use crate::NodeKind;

/// One searchable node handed to the segment builder: a Symbol (matched by
/// name) or a File (matched by content). `content` is the file text for a
/// File and empty for a Symbol (the M0 syntactic pass has no declaration
/// text to carry); `name` is the symbol name or the file name.
pub struct SearchDoc {
    /// The Shard-internal node id this hit resolves to (`sym:…`/`file:…`).
    pub node_id: String,
    pub kind: NodeKind,
    pub name: Option<String>,
    pub path: Option<String>,
    pub content: String,
}

/// The filters and page size a lexical search runs under. Cross-repo
/// fan-out and cursor pagination are the caller's (yg-api's) concern; one
/// segment answers one ranked page.
pub struct SearchParams<'a> {
    /// The user's natural-language query, in tantivy query syntax.
    pub query: &'a str,
    /// Restrict hits to these node kinds; `None` searches every kind.
    pub kinds: Option<&'a [NodeKind]>,
    /// How many hits to return, ranked by score.
    pub limit: usize,
}

/// One ranked hit, with the Shard-internal node id (the caller qualifies
/// it for the wire), the matched node's kind/name/path, and the relevance
/// score. `snippet` is `None` from ranking ([`search`]) and filled in only
/// for the page that survives the cross-repo merge (via [`snippets_for`]).
#[derive(Debug, Clone)]
pub struct LocalHit {
    pub node_id: String,
    pub kind: String,
    pub name: Option<String>,
    pub path: Option<String>,
    pub score: f32,
    pub snippet: Option<String>,
}

/// File name of the full-text segment inside a Shard — the packed tantivy
/// index, recorded under [`crate::Manifest::segments`] like the graph
/// segment.
pub const FTS_SEGMENT_FILE: &str = "fts.tar";

/// The schema's field handles, resolved once so neither the writer nor the
/// reader spells a field name twice.
#[derive(Clone, Copy)]
struct Fields {
    node_id: Field,
    kind: Field,
    /// The raw display name, stored only — read back verbatim for a hit's
    /// `name`. Kept separate from `terms` so the index-time split words
    /// (`rate limit`) never leak into what the API shows the user.
    name: Field,
    /// The matchable name text: the raw name plus its split words,
    /// indexed and boosted but not stored.
    terms: Field,
    path: Field,
    /// The content, indexed and stored — searched, and the source the
    /// snippet generator reads fragments from. Matched as natural text
    /// (grep-like): a camelCase identifier inside file content is one
    /// token, so sub-word matching of code identifiers is the `terms`
    /// field's job (for Symbols), not the body's.
    body: Field,
}

impl Fields {
    /// Build the schema and its field handles. `terms` and `body` are
    /// full-text (the default tokenizer: split on punctuation, lowercase);
    /// `kind` is a raw exact term for filtering; `node_id`/`name`/`path`
    /// are stored-only for read-back. `body` is stored because the snippet
    /// generator reads the field value off the matched document; `terms`
    /// is not — it exists only to be matched.
    fn schema() -> (Schema, Self) {
        let mut builder = Schema::builder();
        let indexing = TextFieldIndexing::default()
            .set_tokenizer("default")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions);
        let text_indexed: TextOptions =
            TextOptions::default().set_indexing_options(indexing.clone());
        let text_stored: TextOptions = TextOptions::default()
            .set_indexing_options(indexing)
            .set_stored();
        let fields = Self {
            // Indexed (exact) as well as stored: the snippet-hydration
            // pass looks a hit's document up by its node id.
            node_id: builder.add_text_field("node_id", STRING | STORED),
            kind: builder.add_text_field("kind", STRING | STORED),
            name: builder.add_text_field("name", STORED),
            terms: builder.add_text_field("terms", text_indexed),
            path: builder.add_text_field("path", STORED),
            body: builder.add_text_field("body", text_stored),
        };
        (builder.build(), fields)
    }

    /// Resolve the field handles of an already-built schema (the reader's
    /// path: the schema rode along inside the segment).
    fn of(schema: &Schema) -> anyhow::Result<Self> {
        let field = |name: &str| {
            schema
                .get_field(name)
                .with_context(|| format!("the fts segment's schema lacks a {name:?} field"))
        };
        Ok(Self {
            node_id: field("node_id")?,
            kind: field("kind")?,
            name: field("name")?,
            terms: field("terms")?,
            path: field("path")?,
            body: field("body")?,
        })
    }
}

/// An opened full-text segment, ready to answer [`search`]. Holds the
/// index, a reader, and the schema's field handles.
pub struct FtsIndex {
    index: Index,
    reader: IndexReader,
    fields: Fields,
}

/// Split a code identifier into its lowercased words, so a natural query
/// (`rate limit`) matches a camelCase or snake_case name (`RateLimit`,
/// `rate_limit`). Splits on non-alphanumeric runs and on camelCase humps,
/// keeping acronym boundaries (`HTTPServer` → `http`, `server`).
///
/// An acronym followed by a short lowercase suffix splits approximately
/// (`URLs` → `ur`, `ls`) — genuinely ambiguous, and standard splitters
/// disagree. This costs only sub-word recall on such names: the raw name
/// is *also* indexed (see [`term_text`]) and tokenized the same way the
/// query is, so a case-insensitive query for the whole name (`urls`) still
/// matches, even though a sub-word query (`url`) may not.
fn identifier_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = name.chars().collect();
    let flush = |current: &mut String, words: &mut Vec<String>| {
        if !current.is_empty() {
            words.push(std::mem::take(current).to_lowercase());
        }
    };
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            flush(&mut current, &mut words);
            continue;
        }
        // A hump starts here when a lowercase/digit gives way to an
        // uppercase (`rateLimit`), or an acronym gives way to a word
        // (`HTTPServer`: the S before `erver`).
        let prev = if i > 0 { Some(chars[i - 1]) } else { None };
        let next = chars.get(i + 1).copied();
        let lower_to_upper = prev.is_some_and(|p| !p.is_uppercase()) && c.is_uppercase();
        let acronym_to_word = prev.is_some_and(|p| p.is_uppercase())
            && c.is_uppercase()
            && next.is_some_and(|n| n.is_lowercase());
        if lower_to_upper || acronym_to_word {
            flush(&mut current, &mut words);
        }
        current.push(c);
    }
    flush(&mut current, &mut words);
    words
}

/// The text indexed in the `terms` field for matching and boosting: the
/// name plus its split words, so `RateLimit` matches both `RateLimit` and
/// `rate limit`. Never stored — the raw name is stored separately for
/// display.
fn term_text(doc: &SearchDoc) -> String {
    match &doc.name {
        Some(name) => {
            let mut text = name.clone();
            for word in identifier_words(name) {
                text.push(' ');
                text.push_str(&word);
            }
            text
        }
        None => String::new(),
    }
}

/// Build a full-text segment over `docs` and return the packed bytes —
/// the tantivy index directory rolled into one tar artifact, ready to put
/// into object storage beside the graph segment.
pub fn build_fts(docs: &[SearchDoc]) -> anyhow::Result<Vec<u8>> {
    let dir = tempfile::tempdir().context("creating a scratch dir for the fts segment")?;
    let (schema, fields) = Fields::schema();
    let index = Index::create_in_dir(dir.path(), schema).context("creating the fts index")?;
    let mut writer: IndexWriter = index
        .writer(50_000_000)
        .context("opening the fts index writer")?;
    for doc in docs {
        let mut td = TantivyDocument::default();
        td.add_text(fields.node_id, &doc.node_id);
        td.add_text(fields.kind, doc.kind.as_str());
        // The raw name is stored for display; its split words go only into
        // the matchable `terms` field, never into what the API returns.
        if let Some(name) = &doc.name {
            td.add_text(fields.name, name);
        }
        let terms = term_text(doc);
        if !terms.is_empty() {
            td.add_text(fields.terms, &terms);
        }
        if let Some(path) = &doc.path {
            td.add_text(fields.path, path);
        }
        td.add_text(fields.body, &doc.content);
        writer
            .add_document(td)
            .context("adding a document to the fts index")?;
    }
    writer.commit().context("committing the fts index")?;
    writer
        .wait_merging_threads()
        .context("finishing fts index merges")?;
    // Drop the index so its directory handles close before we read the
    // files back to pack them.
    drop(index);
    pack_dir(dir.path())
}

/// Roll a built tantivy index directory into a deterministic-order tar
/// archive, skipping the transient lock files (advisory write locks the
/// read path never needs).
fn pack_dir(dir: &Path) -> anyhow::Result<Vec<u8>> {
    let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .context("listing the built fts index")?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()
        .context("listing the built fts index")?;
    entries.sort();
    let mut builder = tar::Builder::new(Vec::new());
    for path in entries {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .context("an fts index file has a non-UTF-8 name")?;
        if name.ends_with(".lock") {
            continue;
        }
        let mut file = std::fs::File::open(&path)
            .with_context(|| format!("reading the fts index file {name}"))?;
        builder
            .append_file(name, &mut file)
            .with_context(|| format!("packing the fts index file {name}"))?;
    }
    builder
        .into_inner()
        .context("finishing the fts segment archive")
}

/// Unpack a packed full-text segment into `dest` (created if absent) — the
/// inverse of [`build_fts`]'s packing, run by the cache tier once per
/// checksum.
pub fn unpack_fts(bytes: &[u8], dest: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dest).context("creating the fts segment directory")?;
    let mut archive = tar::Archive::new(std::io::Cursor::new(bytes));
    for entry in archive
        .entries()
        .context("reading the fts segment archive")?
    {
        let mut entry = entry.context("reading an fts segment entry")?;
        // Every entry was packed by file name alone (no directories): a
        // path that escapes `dest` means a doctored segment, refuse it.
        let path = entry.path().context("an fts segment entry has no path")?;
        let name = path
            .file_name()
            .filter(|n| Path::new(n) == path.as_ref())
            .context("an fts segment entry has an unexpected path")?
            .to_owned();
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .context("reading an fts segment entry")?;
        std::fs::write(dest.join(&name), &bytes).context("writing an fts segment file")?;
    }
    Ok(())
}

/// Open an unpacked full-text segment directory for reading.
pub fn open_fts(dir: &Path) -> anyhow::Result<FtsIndex> {
    let index = Index::open_in_dir(dir).context("opening the fts segment")?;
    let fields = Fields::of(&index.schema())?;
    let reader = index.reader().context("opening the fts segment reader")?;
    Ok(FtsIndex {
        index,
        reader,
        fields,
    })
}

/// Parse a user query against the matchable fields (`terms` boosted over
/// `body`). A query that won't parse is a client error, surfaced as
/// [`QueryMalformed`]. Shared by ranking and snippet hydration so both
/// interpret the query identically.
fn parse_user_query(index: &FtsIndex, query: &str) -> anyhow::Result<Box<dyn Query>> {
    let mut parser =
        QueryParser::for_index(&index.index, vec![index.fields.terms, index.fields.body]);
    // A name hit (a Symbol called RateLimit) should outrank prose that
    // merely mentions the words.
    parser.set_field_boost(index.fields.terms, 3.0);
    parser
        .parse_query(query)
        .map_err(|e| anyhow::Error::new(QueryMalformed(e.to_string())))
}

/// Rank a lexical search over one segment, returning up to `params.limit`
/// hits ordered by relevance — **without** snippets, which are hydrated
/// separately ([`snippets_for`]) for only the page that survives the
/// cross-repo merge, so ranking never pays to snippet candidates it
/// discards. A query that won't parse surfaces as [`QueryMalformed`].
pub fn search(index: &FtsIndex, params: &SearchParams) -> anyhow::Result<Vec<LocalHit>> {
    // tantivy's TopDocs asserts a non-zero limit; "no hits wanted" is an
    // empty result, not a panic.
    if params.limit == 0 {
        return Ok(Vec::new());
    }
    let searcher = index.reader.searcher();
    let user_query = parse_user_query(index, params.query)?;

    let query: Box<dyn Query> = match params.kinds {
        Some(kinds) if !kinds.is_empty() => {
            let any_kind = BooleanQuery::new(
                kinds
                    .iter()
                    .map(|kind| {
                        let term = Term::from_field_text(index.fields.kind, kind.as_str());
                        (
                            Occur::Should,
                            Box::new(TermQuery::new(term, IndexRecordOption::Basic))
                                as Box<dyn Query>,
                        )
                    })
                    .collect(),
            );
            Box::new(BooleanQuery::new(vec![
                (Occur::Must, user_query),
                (Occur::Must, Box::new(any_kind)),
            ]))
        }
        _ => user_query,
    };

    let top = searcher
        .search(&query, &TopDocs::with_limit(params.limit).order_by_score())
        .context("running the fts query")?;

    let mut hits = Vec::with_capacity(top.len());
    for (score, address) in top {
        let doc: TantivyDocument = searcher
            .doc(address)
            .context("reading a matched document")?;
        let stored = |field| {
            doc.get_first(field)
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        let node_id =
            stored(index.fields.node_id).context("a matched fts document has no node_id")?;
        hits.push(LocalHit {
            node_id,
            kind: stored(index.fields.kind).unwrap_or_default(),
            name: stored(index.fields.name),
            path: stored(index.fields.path),
            score,
            snippet: None,
        });
    }
    Ok(hits)
}

/// Highlighted snippets for specific hits, keyed by node id — the
/// hydration pass run over the final page after the cross-repo merge.
/// `query` is the same user query the ranking used, so the highlight
/// tracks what matched; a node id with no snippet (an empty body, e.g. a
/// Symbol) is simply absent from the map. Unknown node ids are skipped.
pub fn snippets_for(
    index: &FtsIndex,
    query: &str,
    node_ids: &[String],
) -> anyhow::Result<std::collections::HashMap<String, String>> {
    let mut out = std::collections::HashMap::new();
    if node_ids.is_empty() {
        return Ok(out);
    }
    let searcher = index.reader.searcher();
    let user_query = parse_user_query(index, query)?;
    let generator = SnippetGenerator::create(&searcher, &*user_query, index.fields.body)
        .context("preparing the snippet generator")?;
    for node_id in node_ids {
        let term = Term::from_field_text(index.fields.node_id, node_id);
        let found = searcher
            .search(
                &TermQuery::new(term, IndexRecordOption::Basic),
                &TopDocs::with_limit(1).order_by_score(),
            )
            .context("looking up a hit's document for snippet hydration")?;
        let Some((_, address)) = found.first() else {
            continue;
        };
        let doc: TantivyDocument = searcher
            .doc(*address)
            .context("reading a hit's document for snippet hydration")?;
        let snippet = generator.snippet_from_doc(&doc);
        if !snippet.fragment().is_empty() {
            out.insert(node_id.clone(), snippet.to_html());
        }
    }
    Ok(out)
}

/// A search query the client spelled in a way tantivy can't parse
/// (unbalanced quotes, a dangling operator). Distinct so the transport can
/// answer 400 with the reason instead of a 500. Detect with
/// `err.downcast_ref::<QueryMalformed>()`.
#[derive(Debug)]
pub struct QueryMalformed(pub String);

impl std::fmt::Display for QueryMalformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "malformed search query: {}", self.0)
    }
}

impl std::error::Error for QueryMalformed {}

#[cfg(test)]
mod tests {
    use super::identifier_words;

    #[test]
    fn identifier_words_splits_camel_snake_and_acronyms() {
        assert_eq!(identifier_words("RateLimit"), ["rate", "limit"]);
        assert_eq!(identifier_words("rate_limit"), ["rate", "limit"]);
        assert_eq!(identifier_words("HTTPServer"), ["http", "server"]);
        assert_eq!(identifier_words("parseURL"), ["parse", "url"]);
        assert_eq!(identifier_words("main.go"), ["main", "go"]);
    }
}
