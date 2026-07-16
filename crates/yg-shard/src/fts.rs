//! The full-text segment (RFC 0001 §6): a tantivy index over a repo's
//! Symbol and File nodes, packed into a single checksummed artifact so it
//! lives in a Shard exactly like `graph.sqlite` — one segment file under
//! the revision's key, one integrity checksum the cache tier verifies on
//! materialize, read in-process. (The checksum guards integrity, not
//! reproducibility: tantivy stamps each build with random segment ids, so
//! the bytes are not stable across rebuilds of identical content. The
//! Shard is addressed by repo+revision, never by this checksum.)
//!
//! The lexical search Verb reads it: a query returns ranked hits whose
//! node ids feed straight into `node`/`neighbors`. Writer and reader share
//! this one schema definition, so they cannot drift.

use std::collections::HashSet;
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
/// fan-out and cursor pagination are the Verb engine's concern; one segment
/// answers one ranked page.
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

/// One symbol declaration found by an exact-name lookup in the FTS
/// segment. The graph reader uses the node id; the remaining fields are
/// enough to present an ambiguous address without opening or scanning the
/// graph segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSymbol {
    pub node_id: LocalSymbolId,
    pub path: LocalSymbolPath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSymbolId(String);

impl LocalSymbolId {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSymbolPath(String);

impl LocalSymbolPath {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A stored symbol name that cannot be safely addressed through searchable
/// terms.
///
/// The raw display-name field is stored but not indexed, so resolving a name
/// with no terms, too many distinct terms, or too many candidate documents
/// would require either scanning the Shard or constructing an excessive
/// Boolean query.
#[derive(Debug)]
pub struct UnaddressableSymbolName;

impl std::fmt::Display for UnaddressableSymbolName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the symbol name cannot be safely addressed through searchable terms")
    }
}

impl std::error::Error for UnaddressableSymbolName {}

/// Maximum number of distinct analyzed terms in one fuzzy symbol lookup.
///
/// Exact-name filtering happens only after Tantivy selects candidates, so
/// bounding the Boolean query prevents a crafted name from producing an
/// excessive number of MUST clauses.
const MAX_SYMBOL_NAME_TERMS: usize = 64;

/// Maximum candidate documents an exact-name fuzzy address may inspect.
///
/// 256 permits substantial overload sets while bounding stored-document reads
/// for common analyzed terms. Lookup collects one additional address as an
/// overflow sentinel, so exceeding this limit is rejected rather than
/// truncating a potentially ambiguous result into a false unique match.
const MAX_ADDRESS_SCAN_DOCS: usize = 256;

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
    // A Shard segment is immutable, so the reader never reloads. Manual
    // avoids the background watch thread the default (`OnCommit`) policy
    // spawns per reader — which would otherwise leak as searches reopen
    // segments.
    let reader: IndexReader = index
        .reader_builder()
        .reload_policy(tantivy::ReloadPolicy::Manual)
        .try_into()
        .context("opening the fts segment reader")?;
    Ok(FtsIndex {
        index,
        reader,
        fields,
    })
}

/// A query longer than this many bytes is refused before parsing. Real
/// lexical queries are short; the cap also bounds the cursor that carries
/// the query and the CPU one query can spend fanning out across repos.
const MAX_QUERY_BYTES: usize = 1024;

/// The deepest run of nested `(` a query may contain. tantivy's query
/// grammar parses a parenthesised group by backtracking recursion, so parse
/// time grows ~4x per nesting level (measured: depth 8 ≈ 5 ms, depth 14 ≈
/// 0.2 s, depth 18 ≈ 3.5 s) on the way to an outright stack-overflow abort
/// far deeper. Either way a tiny query — about one byte per level — would
/// pin a worker thread, so the cap sits well below the exponential knee. No
/// real query nests more than a handful.
const MAX_QUERY_DEPTH: usize = 8;

/// Refuse queries that would crash or pin tantivy's parser before it ever
/// sees them, surfaced as the same client-facing [`QueryMalformed`] (400)
/// as any other unparseable query. Every parse path funnels through
/// [`parse_user_query`] — ranking, snippet hydration, and the query a
/// cursor carries — so guarding here covers all of them.
fn guard_query_complexity(query: &str) -> Result<(), QueryMalformed> {
    if query.len() > MAX_QUERY_BYTES {
        return Err(QueryMalformed(format!(
            "query is {} bytes; the limit is {MAX_QUERY_BYTES}",
            query.len()
        )));
    }
    let mut depth = 0usize;
    let mut deepest = 0usize;
    for byte in query.bytes() {
        match byte {
            b'(' => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            b')' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if deepest > MAX_QUERY_DEPTH {
        return Err(QueryMalformed(format!(
            "query nests parentheses {deepest} deep; the limit is {MAX_QUERY_DEPTH}"
        )));
    }
    // A tantivy range query streams every term in the range out of the
    // dictionary and unions all their postings *before* the page limit
    // applies — an unbounded per-repo cost (measured ~200x a term query at 80k
    // docs, and climbing with corpus size) from a tiny query, and a range is
    // meaningless for lexical search. tantivy spells a range three ways, all of
    // which must be refused:
    //
    //   1. bracket form  `field:[a TO z]` / `field:{a TO *}` — note tantivy
    //      accepts *any* whitespace around `TO` (tab, newline), not just the
    //      ASCII space, so a `" TO "` substring test misses `[a\tTO\tz]`.
    //   2. elastic form  `field:>a`, `field:<z`, `field:>=a`, `field:<=z` —
    //      no bracket and no `TO` at all (tantivy's grammar enters a range on
    //      `peek(one_of("{[><"))`).
    //
    // Detect the entry tokens structurally rather than by the `TO` keyword: a
    // `{` (only ranges use it), a `[` that is not an `IN [` set, or a `<`/`>`
    // in leaf position (query start, or after `(`/whitespace/`:`). `<`/`>`
    // mid-word (`a->b`, `Vec<T>`) is an ordinary term and is left alone.
    if let Some(reason) = range_query_reason(query) {
        return Err(QueryMalformed(reason.to_string()));
    }
    Ok(())
}

/// Why a query is a (refused) range query, or `None` if it is not one. Mirrors
/// tantivy's range-entry dispatch so no spelling — bracket with exotic
/// whitespace around `TO`, or the bracketless elastic `>`/`<` comparison —
/// slips through to the unbounded term-dictionary scan.
fn range_query_reason(query: &str) -> Option<&'static str> {
    let bytes = query.as_bytes();
    // `{` is used by nothing but a range. A `[` is a range unless it opens an
    // `IN [...]` set, which prior analysis found bounded by its listed
    // elements — allow only that one bracket use.
    if bytes.contains(&b'{') {
        return Some("range queries (`{a TO z}`) are not supported");
    }
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'[' if !is_in_set_bracket(query, i) => {
                return Some("range queries (`[a TO z]`) are not supported");
            }
            // Elastic comparison range (`>a`, `<=z`): a `<`/`>` only enters a
            // range in leaf position — at the query start or right after a
            // clause opener (`(`), a field colon (`:`), or whitespace. The
            // same byte mid-token (`a->b`) is an ordinary term character.
            b'<' | b'>' if at_leaf_start(bytes, i) => {
                return Some("range queries (`field:>a`) are not supported");
            }
            _ => {}
        }
    }
    None
}

/// Whether the `[` at byte `i` opens an `IN [` set rather than a range — i.e.
/// it is preceded by `IN` and optional whitespace (tantivy: `tag("IN"),
/// multispace1, char('[')`, with leading `multispace0`).
fn is_in_set_bracket(query: &str, i: usize) -> bool {
    let prefix = query[..i].trim_end();
    // The set keyword is `IN`; tantivy requires whitespace before the `[`, so a
    // trimmed prefix ending in `IN` (and `IN` standing alone as a token) marks
    // the set form. `field:IN [..]` and bare `IN [..]` both qualify.
    let stripped = prefix.strip_suffix("IN").filter(|before| {
        before
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_')
    });
    // Require that whitespace actually separated `IN` from `[` (the trim above
    // removed it), so `IN[` — not a set — is treated as a range bracket.
    stripped.is_some() && query[..i].ends_with(char::is_whitespace)
}

/// Whether byte `i` sits in leaf-start position: the start of the query, or
/// immediately after a clause opener (`(`), a field colon (`:`), or
/// whitespace — the positions where tantivy's grammar may begin a range.
fn at_leaf_start(bytes: &[u8], i: usize) -> bool {
    match i.checked_sub(1).map(|p| bytes[p]) {
        None => true,
        Some(p) => p == b'(' || p == b':' || p.is_ascii_whitespace(),
    }
}

/// Parse a user query against the matchable fields (`terms` boosted over
/// `body`). A query that won't parse is a client error, surfaced as
/// [`QueryMalformed`]. Shared by ranking and snippet hydration so both
/// interpret the query identically.
fn parse_user_query(index: &FtsIndex, query: &str) -> anyhow::Result<Box<dyn Query>> {
    guard_query_complexity(query).map_err(anyhow::Error::new)?;
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

/// Find every Symbol whose stored name exactly equals `name`, optionally
/// narrowed to declarations whose repository-relative path contains
/// `path_fragment`.
///
/// Candidate discovery is index-backed and bounded: the terms generated by the
/// FTS tokenizer select candidate documents, then the stored raw name is
/// compared exactly so identifier sub-word matches never become fuzzy
/// addresses. Candidate sets beyond `MAX_ADDRESS_SCAN_DOCS` are rejected as
/// unaddressable before stored documents are read, preserving the guarantee
/// that a truncated scan can never produce a false unique resolution.
pub fn symbols_named(
    index: &FtsIndex,
    name: &str,
    path_fragment: Option<&str>,
) -> anyhow::Result<Vec<LocalSymbol>> {
    let mut analyzer = index
        .index
        .tokenizers()
        .get("default")
        .context("the FTS index has no default tokenizer")?;
    let mut stream = analyzer.token_stream(name);
    let mut tokens = Vec::new();
    let mut seen = HashSet::new();
    while stream.advance() {
        let token = stream.token().text.clone();
        if seen.insert(token.clone()) {
            tokens.push(token);
            if tokens.len() > MAX_SYMBOL_NAME_TERMS {
                return Err(UnaddressableSymbolName.into());
            }
        }
    }
    if tokens.is_empty() {
        return Err(UnaddressableSymbolName.into());
    }
    let mut clauses = tokens
        .into_iter()
        .map(|token| {
            let term = Term::from_field_text(index.fields.terms, &token);
            (
                Occur::Must,
                Box::new(TermQuery::new(term, IndexRecordOption::Basic)) as Box<dyn Query>,
            )
        })
        .collect::<Vec<_>>();
    let symbol = Term::from_field_text(index.fields.kind, NodeKind::Symbol.as_str());
    clauses.push((
        Occur::Must,
        Box::new(TermQuery::new(symbol, IndexRecordOption::Basic)),
    ));
    let query = BooleanQuery::new(clauses);
    let searcher = index.reader.searcher();
    let addresses = searcher
        .search(
            &query,
            &TopDocs::with_limit(MAX_ADDRESS_SCAN_DOCS + 1).order_by_score(),
        )
        .context("looking up symbol declarations by name")?;
    if addresses.len() > MAX_ADDRESS_SCAN_DOCS {
        return Err(UnaddressableSymbolName.into());
    }

    let mut symbols = Vec::new();
    for (_, address) in addresses {
        let doc: TantivyDocument = searcher
            .doc(address)
            .context("reading a symbol lookup document")?;
        let stored = |field| doc.get_first(field).and_then(|value| value.as_str());
        let Some(stored_name) = stored(index.fields.name) else {
            continue;
        };
        if stored_name != name {
            continue;
        }
        let Some(path) = stored(index.fields.path) else {
            continue;
        };
        if path_fragment.is_some_and(|fragment| !path.contains(fragment)) {
            continue;
        }
        let node_id =
            stored(index.fields.node_id).context("a symbol lookup document has no node_id")?;
        symbols.push(LocalSymbol {
            node_id: LocalSymbolId::new(node_id.to_string()),
            path: LocalSymbolPath::new(path.to_string()),
        });
    }
    symbols.sort_unstable_by(|left, right| left.node_id.0.cmp(&right.node_id.0));
    Ok(symbols)
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
    use super::{
        MAX_ADDRESS_SCAN_DOCS, MAX_QUERY_BYTES, MAX_QUERY_DEPTH, MAX_SYMBOL_NAME_TERMS, SearchDoc,
        UnaddressableSymbolName, build_fts, guard_query_complexity, identifier_words, open_fts,
        symbols_named, unpack_fts,
    };
    use crate::NodeKind;

    #[test]
    fn identifier_words_splits_camel_snake_and_acronyms() {
        assert_eq!(identifier_words("RateLimit"), ["rate", "limit"]);
        assert_eq!(identifier_words("rate_limit"), ["rate", "limit"]);
        assert_eq!(identifier_words("HTTPServer"), ["http", "server"]);
        assert_eq!(identifier_words("parseURL"), ["parse", "url"]);
        assert_eq!(identifier_words("main.go"), ["main", "go"]);
        // An acronym trailed by a short lowercase suffix splits at the last
        // capital before the suffix (the documented approximate behavior).
        assert_eq!(identifier_words("URLs"), ["ur", "ls"]);
    }

    #[test]
    fn guard_allows_queries_at_the_limits_and_rejects_past_them() {
        // The length cap is inclusive: exactly at the limit is fine.
        assert!(guard_query_complexity(&"a".repeat(MAX_QUERY_BYTES)).is_ok());
        assert!(guard_query_complexity(&"a".repeat(MAX_QUERY_BYTES + 1)).is_err());
        // Nesting is counted by depth, not by count: many shallow groups
        // pass, but one level past the cap is refused.
        assert!(guard_query_complexity(&"(".repeat(MAX_QUERY_DEPTH)).is_ok());
        assert!(guard_query_complexity(&"(".repeat(MAX_QUERY_DEPTH + 1)).is_err());
        assert!(guard_query_complexity(&"() ".repeat(200)).is_ok());
        // Range queries are refused in every spelling tantivy accepts:
        // bracket (with any whitespace around `TO`, not just spaces), the
        // exclusive `{}` form, and the bracketless elastic `>`/`<`/`>=`.
        assert!(guard_query_complexity("body:[a TO z]").is_err());
        assert!(guard_query_complexity("terms:{a TO *}").is_err());
        assert!(guard_query_complexity("body:[a\tTO\tz]").is_err());
        assert!(guard_query_complexity("body:[a\nTO\nz]").is_err());
        assert!(guard_query_complexity("body:>a").is_err());
        assert!(guard_query_complexity("terms:<=z").is_err());
        assert!(guard_query_complexity(">a").is_err());
        // ...but ordinary code queries that merely contain `<`/`>`/`TO`, and
        // the bounded `IN [...]` set, are left alone.
        assert!(guard_query_complexity("convert TO json").is_ok());
        assert!(guard_query_complexity("rate limit").is_ok());
        assert!(guard_query_complexity("a->b").is_ok());
        assert!(guard_query_complexity("Vec<T>").is_ok());
        assert!(guard_query_complexity("foo IN [a b c]").is_ok());
    }

    #[test]
    fn unpack_refuses_an_entry_that_escapes_its_directory() {
        // A doctored segment whose entry carries a path component (not a
        // bare file name) must be refused, never written outside `dest`.
        let mut builder = tar::Builder::new(Vec::new());
        let payload = b"pwned";
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, "sub/escape", &payload[..])
            .unwrap();
        let bytes = builder.into_inner().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let err =
            unpack_fts(&bytes, dir.path()).expect_err("an entry with a path component is refused");
        assert!(
            err.to_string().contains("unexpected path"),
            "the escape is rejected by path: {err:#}"
        );
        assert!(
            !dir.path().join("sub").exists() && !dir.path().join("escape").exists(),
            "nothing was written outside a bare file name"
        );
    }

    #[test]
    fn exact_symbol_lookup_is_exhaustive_and_path_narrowable() {
        let bytes = build_fts(&[
            SearchDoc {
                node_id: "sym:a/service.go#Resolve".to_string(),
                kind: NodeKind::Symbol,
                name: Some("Resolve".to_string()),
                path: Some("a/service.go".to_string()),
                content: String::new(),
            },
            SearchDoc {
                node_id: "sym:b/service.go#Resolve".to_string(),
                kind: NodeKind::Symbol,
                name: Some("Resolve".to_string()),
                path: Some("b/service.go".to_string()),
                content: String::new(),
            },
            SearchDoc {
                node_id: "sym:a/service.go#Resolver".to_string(),
                kind: NodeKind::Symbol,
                name: Some("Resolver".to_string()),
                path: Some("a/service.go".to_string()),
                content: String::new(),
            },
        ])
        .expect("build fixture index");
        let packed = tempfile::tempdir().expect("packed fixture dir");
        unpack_fts(&bytes, packed.path()).expect("unpack fixture index");
        let index = open_fts(packed.path()).expect("open fixture index");

        let all = symbols_named(&index, "Resolve", None).expect("lookup all declarations");
        assert_eq!(all.len(), 2, "the prefix-like Resolver is exact-filtered");
        assert_eq!(all[0].path.as_str(), "a/service.go");
        assert_eq!(all[1].path.as_str(), "b/service.go");

        let narrowed =
            symbols_named(&index, "Resolve", Some("b/")).expect("lookup under path fragment");
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].node_id.as_str(), "sym:b/service.go#Resolve");
        assert!(symbols_named(&index, "Missing", None).unwrap().is_empty());
    }

    #[test]
    fn zero_term_symbol_names_are_distinct_from_absent_names() {
        let bytes = build_fts(&[SearchDoc {
            node_id: "sym:operators.go#::".to_string(),
            kind: NodeKind::Symbol,
            name: Some("::".to_string()),
            path: Some("operators.go".to_string()),
            content: String::new(),
        }])
        .expect("build fixture index");
        let packed = tempfile::tempdir().expect("packed fixture dir");
        unpack_fts(&bytes, packed.path()).expect("unpack fixture index");
        let index = open_fts(packed.path()).expect("open fixture index");

        let error = symbols_named(&index, "::", None)
            .expect_err("a zero-term name is un-addressable, not absent");

        assert!(error.downcast_ref::<UnaddressableSymbolName>().is_some());
    }

    #[test]
    fn duplicate_name_tokens_still_resolve_exactly() {
        let name = vec!["Resolve"; MAX_SYMBOL_NAME_TERMS + 1].join(" ");
        let bytes = build_fts(&[SearchDoc {
            node_id: "sym:service.go#ResolveResolve".to_string(),
            kind: NodeKind::Symbol,
            name: Some(name.clone()),
            path: Some("service.go".to_string()),
            content: String::new(),
        }])
        .expect("build fixture index");
        let packed = tempfile::tempdir().expect("packed fixture dir");
        unpack_fts(&bytes, packed.path()).expect("unpack fixture index");
        let index = open_fts(packed.path()).expect("open fixture index");

        let symbols = symbols_named(&index, &name, None).expect("lookup duplicate-token name");

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].node_id.as_str(), "sym:service.go#ResolveResolve");
    }

    #[test]
    fn too_many_distinct_name_tokens_are_unaddressable() {
        let bytes = build_fts(&[]).expect("build empty fixture index");
        let packed = tempfile::tempdir().expect("packed fixture dir");
        unpack_fts(&bytes, packed.path()).expect("unpack fixture index");
        let index = open_fts(packed.path()).expect("open fixture index");
        let name = (0..=MAX_SYMBOL_NAME_TERMS)
            .map(|index| format!("token{index}"))
            .collect::<Vec<_>>()
            .join(" ");

        let error = symbols_named(&index, &name, None)
            .expect_err("too many distinct tokens must be unaddressable");

        assert!(error.downcast_ref::<UnaddressableSymbolName>().is_some());
    }

    #[test]
    fn candidate_overflow_is_unaddressable_instead_of_false_unique() {
        let mut documents = (0..MAX_ADDRESS_SCAN_DOCS)
            .map(|index| SearchDoc {
                node_id: format!("sym:service{index}.go#ResolveVariant{index}"),
                kind: NodeKind::Symbol,
                name: Some(format!("ResolveVariant{index}")),
                path: Some(format!("service{index}.go")),
                content: String::new(),
            })
            .collect::<Vec<_>>();
        documents.push(SearchDoc {
            node_id: "sym:service.go#Resolve".to_string(),
            kind: NodeKind::Symbol,
            name: Some("Resolve".to_string()),
            path: Some("service.go".to_string()),
            content: String::new(),
        });
        let bytes = build_fts(&documents).expect("build overflowing fixture index");
        let packed = tempfile::tempdir().expect("packed fixture dir");
        unpack_fts(&bytes, packed.path()).expect("unpack fixture index");
        let index = open_fts(packed.path()).expect("open fixture index");

        let error = symbols_named(&index, "Resolve", None)
            .expect_err("an overflowing candidate set must not return its one exact match");

        assert!(error.downcast_ref::<UnaddressableSymbolName>().is_some());
    }
}
