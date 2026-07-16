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

use std::path::Path;

use anyhow::Context;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions, Value,
};
use tantivy::snippet::SnippetGenerator;
use tantivy::tokenizer::{
    LowerCaser, MAX_TOKEN_LEN, SimpleTokenizer, TextAnalyzer, Token, TokenStream, Tokenizer,
};
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

/// A stored symbol name that cannot be safely addressed through its exact
/// index term.
///
/// Exact-name lookup is bounded before stored documents are read. A name shared
/// by more than `MAX_ADDRESS_SCAN_DOCS` declarations is therefore
/// unaddressable rather than silently truncated into a false unique match.
#[derive(Debug)]
pub struct UnaddressableSymbolName;

impl std::fmt::Display for UnaddressableSymbolName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("too many declarations share the exact symbol name")
    }
}

impl std::error::Error for UnaddressableSymbolName {}

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

const CODE_TOKENIZER: &str = "code";
const WHOLE_TOKENIZER: &str = "whole";

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
    /// A Symbol's byte-exact, case-sensitive name, indexed with Tantivy's raw
    /// tokenizer for bounded fuzzy-address lookup. Not stored: `name` is the
    /// sole display value.
    raw_name: Field,
    /// The matchable name text, split by the same code tokenizer as queries
    /// and boosted but not stored.
    terms: Field,
    /// Original name text analyzed without camel splitting, preserving whole
    /// identifier queries while retaining the name-field boost.
    terms_whole: Field,
    /// The stored path, also indexed with the code tokenizer so directory and
    /// filename components are searchable without spelling separators.
    path: Field,
    /// Path text analyzed without camel splitting.
    path_whole: Field,
    /// The content, indexed and stored — searched, and the source the
    /// snippet generator reads fragments from. Matched as natural text
    /// with code-aware camel/snake splitting.
    body: Field,
    /// Body text analyzed without camel splitting. It is not stored; snippet
    /// fallback reuses the original value stored in `body`.
    body_whole: Field,
}

impl Fields {
    /// Build the schema and its field handles. `terms`, `path`, and `body` use
    /// the registered code tokenizer; the parallel `*_whole` fields use an
    /// unfiltered lowercase word tokenizer. `kind`, `node_id`, and `raw_name`
    /// are raw exact terms. `body` remains stored because snippet generation
    /// reads the original field value.
    fn schema() -> (Schema, Self) {
        let mut builder = Schema::builder();
        let code_indexing = TextFieldIndexing::default()
            .set_tokenizer(CODE_TOKENIZER)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions);
        let code_indexed: TextOptions =
            TextOptions::default().set_indexing_options(code_indexing.clone());
        let whole_indexed: TextOptions = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(WHOLE_TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
        let code_stored: TextOptions = TextOptions::default()
            .set_indexing_options(code_indexing)
            .set_stored();
        let fields = Self {
            // Indexed (exact) as well as stored: the snippet-hydration
            // pass looks a hit's document up by its node id.
            node_id: builder.add_text_field("node_id", STRING | STORED),
            kind: builder.add_text_field("kind", STRING | STORED),
            name: builder.add_text_field("name", STORED),
            raw_name: builder.add_text_field("raw_name", STRING),
            terms: builder.add_text_field("terms", code_indexed),
            terms_whole: builder.add_text_field("terms_whole", whole_indexed.clone()),
            path: builder.add_text_field("path", code_stored.clone()),
            path_whole: builder.add_text_field("path_whole", whole_indexed.clone()),
            body: builder.add_text_field("body", code_stored),
            body_whole: builder.add_text_field("body_whole", whole_indexed),
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
            raw_name: field("raw_name")?,
            terms: field("terms")?,
            terms_whole: field("terms_whole")?,
            path: field("path")?,
            path_whole: field("path_whole")?,
            body: field("body")?,
            body_whole: field("body_whole")?,
        })
    }
}

/// Tokenizer for source text and repository-relative paths. It splits on
/// punctuation/path separators and at camel-case boundaries, lowercases each
/// word, and chunks pathological words at Tantivy's hard term-size limit.
/// Unlike Tantivy's default analyzer it has no 40-byte `RemoveLongFilter`, so
/// long identifiers are indexed instead of silently disappearing.
#[derive(Clone, Default)]
struct CodeTokenizer;

struct CodeTokenStream<'a> {
    text: &'a str,
    cursor: usize,
    current: Token,
}

impl Tokenizer for CodeTokenizer {
    type TokenStream<'a> = CodeTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        CodeTokenStream {
            text,
            cursor: 0,
            current: Token::default(),
        }
    }
}

impl TokenStream for CodeTokenStream<'_> {
    fn advance(&mut self) -> bool {
        self.current.text.clear();
        while self.cursor < self.text.len() {
            let character = self.text[self.cursor..]
                .chars()
                .next()
                .expect("cursor stays on a character boundary");
            if character.is_alphanumeric() {
                break;
            }
            self.cursor += character.len_utf8();
        }
        if self.cursor == self.text.len() {
            return false;
        }

        self.current.position = self.current.position.wrapping_add(1);
        self.current.position_length = 1;
        self.current.offset_from = self.cursor;
        let mut previous = None;
        while self.cursor < self.text.len() {
            let character = self.text[self.cursor..]
                .chars()
                .next()
                .expect("cursor stays on a character boundary");
            if !character.is_alphanumeric() {
                break;
            }
            let after = self.cursor + character.len_utf8();
            let next = self.text[after..].chars().next();
            let boundary = previous.is_some_and(|previous: char| {
                (!previous.is_uppercase() && character.is_uppercase())
                    || (previous.is_uppercase()
                        && character.is_uppercase()
                        && next.is_some_and(char::is_lowercase))
            });
            let lowercase = character.to_lowercase().collect::<String>();
            if boundary
                || (!self.current.text.is_empty()
                    && self.current.text.len() + lowercase.len() > MAX_TOKEN_LEN)
            {
                break;
            }
            self.current.text.push_str(&lowercase);
            self.cursor = after;
            previous = Some(character);
        }
        self.current.offset_to = self.cursor;
        true
    }

    fn token(&self) -> &Token {
        &self.current
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.current
    }
}

#[cfg(test)]
fn code_tokens(text: &str) -> Vec<Token> {
    let mut tokenizer = CodeTokenizer;
    let mut stream = tokenizer.token_stream(text);
    let mut tokens = Vec::new();
    while stream.advance() {
        tokens.push(stream.token().clone());
    }
    tokens
}

fn register_tokenizers(index: &Index) {
    index.tokenizers().register(CODE_TOKENIZER, CodeTokenizer);
    index.tokenizers().register(
        WHOLE_TOKENIZER,
        TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .build(),
    );
}

/// An opened full-text segment, ready to answer [`search`]. Holds the
/// index, a reader, and the schema's field handles.
pub struct FtsIndex {
    index: Index,
    reader: IndexReader,
    fields: Fields,
}

/// Build a full-text segment over `docs` and return the packed bytes —
/// the tantivy index directory rolled into one tar artifact, ready to put
/// into object storage beside the graph segment.
pub fn build_fts(docs: &[SearchDoc]) -> anyhow::Result<Vec<u8>> {
    let dir = tempfile::tempdir().context("creating a scratch dir for the fts segment")?;
    let (schema, fields) = Fields::schema();
    let index = Index::create_in_dir(dir.path(), schema).context("creating the fts index")?;
    register_tokenizers(&index);
    let mut writer: IndexWriter = index
        .writer(50_000_000)
        .context("opening the fts index writer")?;
    for doc in docs {
        let mut td = TantivyDocument::default();
        td.add_text(fields.node_id, &doc.node_id);
        td.add_text(fields.kind, doc.kind.as_str());
        // The raw name is stored for display and analyzed only in the
        // matchable `terms` field, never in what the API returns.
        if let Some(name) = &doc.name {
            td.add_text(fields.name, name);
            td.add_text(fields.terms, name);
            td.add_text(fields.terms_whole, name);
            if doc.kind == NodeKind::Symbol {
                td.add_text(fields.raw_name, name);
            }
        }
        if let Some(path) = &doc.path {
            td.add_text(fields.path, path);
            td.add_text(fields.path_whole, path);
        }
        td.add_text(fields.body, &doc.content);
        td.add_text(fields.body_whole, &doc.content);
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
    unpack_fts_reader(std::io::Cursor::new(bytes), dest)
}

/// Unpack a full-text segment directly from its cached archive without
/// loading the archive or its entries into a process-sized byte buffer.
pub(crate) fn unpack_fts_file(archive: &Path, dest: &Path) -> anyhow::Result<()> {
    let file = std::fs::File::open(archive).context("opening the cached fts segment archive")?;
    unpack_fts_reader(file, dest)
}

fn unpack_fts_reader(reader: impl std::io::Read, dest: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dest).context("creating the fts segment directory")?;
    let mut archive = tar::Archive::new(reader);
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
        let mut output =
            std::fs::File::create(dest.join(&name)).context("creating an fts segment file")?;
        std::io::copy(&mut entry, &mut output).context("writing an fts segment file")?;
    }
    Ok(())
}

/// Open an unpacked full-text segment directory for reading.
pub fn open_fts(dir: &Path) -> anyhow::Result<FtsIndex> {
    let index = Index::open_in_dir(dir).context("opening the fts segment")?;
    register_tokenizers(&index);
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

/// Parse a user query against the code-aware matchable fields (`terms` boosted
/// over `path` and `body`) plus a whole-word fallback that preserves collapsed
/// identifier queries. A query that won't parse is a client error, surfaced as
/// [`QueryMalformed`]. Shared by ranking and snippet hydration so both
/// interpret the query identically.
fn parse_user_query(index: &FtsIndex, query: &str) -> anyhow::Result<Box<dyn Query>> {
    guard_query_complexity(query).map_err(anyhow::Error::new)?;
    let mut parser = QueryParser::for_index(
        &index.index,
        vec![
            index.fields.terms,
            index.fields.terms_whole,
            index.fields.path,
            index.fields.path_whole,
            index.fields.body,
            index.fields.body_whole,
        ],
    );
    // A name hit (a Symbol called RateLimit) should outrank prose that
    // merely mentions the words.
    parser.set_field_boost(index.fields.terms, 3.0);
    parser.set_field_boost(index.fields.terms_whole, 3.0);
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

/// Find every Symbol whose indexed raw name exactly equals `name`, optionally
/// narrowed to declarations whose repository-relative path contains
/// `path_fragment`.
///
/// Candidate discovery is a case-sensitive raw-term query. Candidate sets
/// beyond `MAX_ADDRESS_SCAN_DOCS` are rejected as unaddressable before stored
/// documents are read, preserving the guarantee that a truncated scan can
/// never produce a false unique resolution.
pub fn symbols_named(
    index: &FtsIndex,
    name: &str,
    path_fragment: Option<&str>,
) -> anyhow::Result<Vec<LocalSymbol>> {
    let raw_name = Term::from_field_text(index.fields.raw_name, name);
    let symbol = Term::from_field_text(index.fields.kind, NodeKind::Symbol.as_str());
    let query = BooleanQuery::new(vec![
        (
            Occur::Must,
            Box::new(TermQuery::new(raw_name, IndexRecordOption::Basic)) as Box<dyn Query>,
        ),
        (
            Occur::Must,
            Box::new(TermQuery::new(symbol, IndexRecordOption::Basic)),
        ),
    ]);
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
    let code_generator = SnippetGenerator::create(&searcher, &*user_query, index.fields.body)
        .context("preparing the snippet generator")?;
    let whole_generator =
        SnippetGenerator::create(&searcher, &*user_query, index.fields.body_whole)
            .context("preparing the whole-identifier snippet generator")?;
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
        let snippet = code_generator.snippet_from_doc(&doc);
        if !snippet.fragment().is_empty() {
            out.insert(node_id.clone(), snippet.to_html());
            continue;
        }
        let Some(body) = doc
            .get_first(index.fields.body)
            .and_then(|value| value.as_str())
        else {
            continue;
        };
        let mut whole_doc = TantivyDocument::default();
        whole_doc.add_text(index.fields.body_whole, body);
        let snippet = whole_generator.snippet_from_doc(&whole_doc);
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
        MAX_ADDRESS_SCAN_DOCS, MAX_QUERY_BYTES, MAX_QUERY_DEPTH, MAX_TOKEN_LEN, SearchDoc,
        SearchParams, UnaddressableSymbolName, build_fts, code_tokens, guard_query_complexity,
        open_fts, search, snippets_for, symbols_named, unpack_fts,
    };
    use crate::NodeKind;

    #[test]
    fn code_tokenizer_splits_identifiers_and_keeps_long_words() {
        let long = "identifier".repeat(8);
        let source = format!("rateLimit HTTPServer snake_case {long}");
        let tokens = code_tokens(&source)
            .into_iter()
            .map(|token| token.text)
            .collect::<Vec<_>>();

        assert_eq!(
            tokens,
            ["rate", "limit", "http", "server", "snake", "case", &long,]
        );

        let pathological = "x".repeat(MAX_TOKEN_LEN * 3);
        let chunks = code_tokens(&pathological);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|token| token.text.len() == MAX_TOKEN_LEN));
    }

    #[test]
    fn file_bodies_and_paths_use_code_aware_terms() {
        let bytes = build_fts(&[
            SearchDoc {
                node_id: "file:src/http/rate_limiter.rs".to_string(),
                kind: NodeKind::File,
                name: Some("opaque".to_string()),
                path: Some("src/http/rate_limiter.rs".to_string()),
                content: format!("fn applyRateLimit() {{}} {}", "x".repeat(80)),
            },
            SearchDoc {
                node_id: "file:docs/guide.md".to_string(),
                kind: NodeKind::File,
                name: Some("guide.md".to_string()),
                path: Some("docs/guide.md".to_string()),
                content: "ordinary prose".to_string(),
            },
        ])
        .expect("build fixture index");
        let packed = tempfile::tempdir().expect("packed fixture dir");
        unpack_fts(&bytes, packed.path()).expect("unpack fixture index");
        let index = open_fts(packed.path()).expect("open fixture index");

        let long_query = "x".repeat(80);
        for query in [
            "apply rate limit",
            "applyratelimit",
            "http",
            "limiter",
            &long_query,
        ] {
            let hits = search(
                &index,
                &SearchParams {
                    query,
                    kinds: Some(&[NodeKind::File]),
                    limit: 10,
                },
            )
            .expect("search fixture");
            assert_eq!(
                hits.first().map(|hit| hit.node_id.as_str()),
                Some("file:src/http/rate_limiter.rs"),
                "{query:?} matches code or path components: {hits:?}"
            );
        }
    }

    #[test]
    fn whole_and_split_identifier_queries_cross_match_without_phrase_gaps() {
        let bytes = build_fts(&[
            SearchDoc {
                node_id: "file:camel.txt".to_string(),
                kind: NodeKind::File,
                name: Some("opaque-camel".to_string()),
                path: Some("camel.txt".to_string()),
                content: "foo rateLimit".to_string(),
            },
            SearchDoc {
                node_id: "file:snake.txt".to_string(),
                kind: NodeKind::File,
                name: Some("opaque-snake".to_string()),
                path: Some("snake.txt".to_string()),
                content: "foo rate_limit".to_string(),
            },
            SearchDoc {
                node_id: "file:prose.txt".to_string(),
                kind: NodeKind::File,
                name: Some("opaque-prose".to_string()),
                path: Some("prose.txt".to_string()),
                content: "foo rate limit".to_string(),
            },
        ])
        .expect("build fixture index");
        let packed = tempfile::tempdir().expect("packed fixture dir");
        unpack_fts(&bytes, packed.path()).expect("unpack fixture index");
        let index = open_fts(packed.path()).expect("open fixture index");

        let ids_for = |query| {
            search(
                &index,
                &SearchParams {
                    query,
                    kinds: Some(&[NodeKind::File]),
                    limit: 10,
                },
            )
            .expect("search fixture")
            .into_iter()
            .map(|hit| hit.node_id)
            .collect::<std::collections::HashSet<_>>()
        };
        let camel = ["file:camel.txt".to_string()]
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids_for("ratelimit"), camel);
        let all = [
            "file:camel.txt".to_string(),
            "file:snake.txt".to_string(),
            "file:prose.txt".to_string(),
        ]
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids_for("rateLimit"), all);
        assert_eq!(ids_for("\"foo rate\""), all);
        let snippets = snippets_for(&index, "ratelimit", &["file:camel.txt".to_string()])
            .expect("hydrate whole-identifier snippet");
        assert!(
            snippets
                .get("file:camel.txt")
                .is_some_and(|snippet| snippet.contains("<b>rateLimit</b>")),
            "whole-identifier fallback still highlights the body: {snippets:?}"
        );
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
    fn punctuation_only_and_dollar_symbol_names_are_addressable() {
        let bytes = build_fts(&[
            SearchDoc {
                node_id: "sym:operators.js#::".to_string(),
                kind: NodeKind::Symbol,
                name: Some("::".to_string()),
                path: Some("operators.js".to_string()),
                content: String::new(),
            },
            SearchDoc {
                node_id: "sym:operators.js#$".to_string(),
                kind: NodeKind::Symbol,
                name: Some("$".to_string()),
                path: Some("operators.js".to_string()),
                content: String::new(),
            },
        ])
        .expect("build fixture index");
        let packed = tempfile::tempdir().expect("packed fixture dir");
        unpack_fts(&bytes, packed.path()).expect("unpack fixture index");
        let index = open_fts(packed.path()).expect("open fixture index");

        for name in ["::", "$"] {
            let symbols = symbols_named(&index, name, None).expect("raw name lookup");
            assert_eq!(symbols.len(), 1);
            assert_eq!(
                symbols[0].node_id.as_str(),
                format!("sym:operators.js#{name}")
            );
        }
    }

    #[test]
    fn names_longer_than_the_default_token_limit_resolve_exactly() {
        let name = "LongSymbolName".repeat(4);
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

        let symbols = symbols_named(&index, &name, None).expect("lookup long raw name");

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].node_id.as_str(), "sym:service.go#ResolveResolve");
    }

    #[test]
    fn candidate_overflow_is_unaddressable_instead_of_false_unique() {
        let documents = (0..=MAX_ADDRESS_SCAN_DOCS)
            .map(|index| SearchDoc {
                node_id: format!("sym:service{index}.go#Resolve"),
                kind: NodeKind::Symbol,
                name: Some("Resolve".to_string()),
                path: Some(format!("service{index}.go")),
                content: String::new(),
            })
            .collect::<Vec<_>>();
        let bytes = build_fts(&documents).expect("build overflowing fixture index");
        let packed = tempfile::tempdir().expect("packed fixture dir");
        unpack_fts(&bytes, packed.path()).expect("unpack fixture index");
        let index = open_fts(packed.path()).expect("open fixture index");

        let error = symbols_named(&index, "Resolve", None)
            .expect_err("an overflowing candidate set must not return its one exact match");

        assert!(error.downcast_ref::<UnaddressableSymbolName>().is_some());
    }
}
