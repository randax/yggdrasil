//! Heuristic relationship extraction (issue #6, ADR 0002): CALLS,
//! IMPORTS, and EXTENDS/IMPLEMENTS edges resolved by name/scope
//! heuristics, every edge tagged Provenance::Syntactic with a
//! confidence that encodes how ambiguous the resolution was.

use yg_shard::{Edge, EdgeKind, Graph, NodeKind, Provenance};

/// Run the pass over an in-memory tree laid out in a tempdir.
fn pass_over(files: &[(&str, &str)]) -> Graph {
    let dir = tempfile::tempdir().unwrap();
    for (path, contents) in files {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
    }
    yg_index::syntactic_pass(dir.path())
        .expect("the pass must handle a plain tree")
        .0
}

fn edges_of_kind(graph: &Graph, kind: EdgeKind) -> Vec<&Edge> {
    graph.edges.iter().filter(|e| e.kind == kind).collect()
}

#[test]
fn a_direct_call_produces_a_calls_edge_with_its_site() {
    let graph = pass_over(&[(
        "widgets.go",
        r#"package gadgets

func helper() {}

func Caller() {
	helper()
}
"#,
    )]);

    let calls = edges_of_kind(&graph, EdgeKind::Calls);
    assert_eq!(calls.len(), 1, "exactly one call site, got {calls:?}");
    let call = calls[0];
    assert_eq!(call.src, "sym:widgets.go#Caller", "the enclosing function");
    assert_eq!(call.dst, "sym:widgets.go#helper", "the called function");
    assert_eq!(call.provenance, Provenance::Syntactic);
    assert!(
        (call.confidence - 0.9).abs() < 1e-6,
        "a unique syntactic match is 0.9, got {}",
        call.confidence
    );
    assert_eq!(
        call.location.as_deref(),
        Some("widgets.go:6:2"),
        "the call site: <path>:<line>:<col>, 1-based"
    );
}

#[test]
fn every_call_site_is_its_own_edge() {
    let graph = pass_over(&[(
        "retry.go",
        r#"package gadgets

func attempt() {}

func Retry() {
	attempt()
	attempt()
}
"#,
    )]);

    let sites: Vec<Option<&str>> = edges_of_kind(&graph, EdgeKind::Calls)
        .iter()
        .map(|e| e.location.as_deref())
        .collect();
    assert_eq!(
        sites,
        vec![Some("retry.go:6:2"), Some("retry.go:7:2")],
        "two call sites, two CALLS edges, each locating its own line"
    );
}

#[test]
fn calls_resolve_across_files_of_the_same_package() {
    let graph = pass_over(&[
        ("gadgets/render.go", "package gadgets\n\nfunc render() {}\n"),
        (
            "gadgets/widget.go",
            "package gadgets\n\nfunc Show() {\n\trender()\n}\n",
        ),
    ]);

    let calls = edges_of_kind(&graph, EdgeKind::Calls);
    assert_eq!(calls.len(), 1, "got {calls:?}");
    assert_eq!(calls[0].src, "sym:gadgets/widget.go#Show");
    assert_eq!(calls[0].dst, "sym:gadgets/render.go#render");
}

#[test]
fn an_ambiguous_call_keeps_every_candidate_at_split_confidence() {
    // Two same-named functions, both plausible targets from a third
    // package: ambiguity is recorded as spread confidence, not dropped
    // (issue #6).
    let graph = pass_over(&[
        ("red/setup.go", "package red\n\nfunc Setup() {}\n"),
        ("blue/setup.go", "package blue\n\nfunc Setup() {}\n"),
        (
            "cmd/main.go",
            "package main\n\nfunc main() {\n\tSetup()\n}\n",
        ),
    ]);

    let calls = edges_of_kind(&graph, EdgeKind::Calls);
    let mut targets: Vec<&str> = calls.iter().map(|e| e.dst.as_str()).collect();
    targets.sort_unstable();
    assert_eq!(
        targets,
        vec!["sym:blue/setup.go#Setup", "sym:red/setup.go#Setup"],
        "both candidates are kept"
    );
    for call in calls {
        assert!(
            (call.confidence - 0.45).abs() < 1e-6,
            "0.9 split across two candidates, got {}",
            call.confidence
        );
    }
}

#[test]
fn a_same_package_candidate_shadows_other_packages() {
    // Go scoping: an unqualified call can only reach its own package, so
    // a candidate in the caller's package resolves alone at full
    // confidence — the same-named function elsewhere is not a candidate.
    let graph = pass_over(&[
        ("gadgets/log.go", "package gadgets\n\nfunc log() {}\n"),
        (
            "gadgets/widget.go",
            "package gadgets\n\nfunc Show() {\n\tlog()\n}\n",
        ),
        ("util/log.go", "package util\n\nfunc log() {}\n"),
    ]);

    let calls = edges_of_kind(&graph, EdgeKind::Calls);
    assert_eq!(calls.len(), 1, "the same-package candidate wins: {calls:?}");
    assert_eq!(calls[0].dst, "sym:gadgets/log.go#log");
    assert!(
        (calls[0].confidence - 0.9).abs() < 1e-6,
        "shadowing makes the match unique again, got {}",
        calls[0].confidence
    );
}

#[test]
fn a_method_call_resolves_to_every_receivers_method() {
    // The receiver's type is invisible to a syntactic pass: x.Render()
    // could be either Render method, so both are candidates.
    let graph = pass_over(&[(
        "render.go",
        r#"package gadgets

type Widget struct{}

func (w Widget) Render() string { return "w" }

type Gadget struct{}

func (g Gadget) Render() string { return "g" }

func Show(x Widget) {
	x.Render()
}
"#,
    )]);

    let calls = edges_of_kind(&graph, EdgeKind::Calls);
    let mut targets: Vec<&str> = calls.iter().map(|e| e.dst.as_str()).collect();
    targets.sort_unstable();
    assert_eq!(
        targets,
        vec!["sym:render.go#Gadget.Render", "sym:render.go#Widget.Render"],
        "either receiver's method could be the target"
    );
    for call in calls {
        assert_eq!(call.src, "sym:render.go#Show");
        assert!(
            (call.confidence - 0.45).abs() < 1e-6,
            "0.9 split across two method candidates, got {}",
            call.confidence
        );
        assert_eq!(call.location.as_deref(), Some("render.go:12:2"));
    }
}

#[test]
fn an_import_qualified_call_resolves_through_the_import() {
    // util.Reset() names a package, not a receiver: it must resolve to
    // the imported package's function (located via go.mod's module
    // path) and never fan out to same-named methods.
    let graph = pass_over(&[
        ("go.mod", "module example.com/mod\n\ngo 1.22\n"),
        ("util/reset.go", "package util\n\nfunc Reset() {}\n"),
        (
            "gadgets/widget.go",
            "package gadgets\n\ntype Widget struct{}\n\nfunc (w Widget) Reset() {}\n",
        ),
        (
            "app/main.go",
            "package main\n\nimport \"example.com/mod/util\"\n\nfunc main() {\n\tutil.Reset()\n}\n",
        ),
    ]);

    let calls = edges_of_kind(&graph, EdgeKind::Calls);
    assert_eq!(
        calls.len(),
        1,
        "one target: the imported package's function, got {calls:?}"
    );
    assert_eq!(calls[0].src, "sym:app/main.go#main");
    assert_eq!(calls[0].dst, "sym:util/reset.go#Reset");
    assert!(
        (calls[0].confidence - 0.9).abs() < 1e-6,
        "a unique qualified match, got {}",
        calls[0].confidence
    );
}

#[test]
fn calls_to_names_the_repo_does_not_declare_yield_no_edges() {
    // Builtins, the standard library, external modules: the graph links
    // only Symbols it holds — it must not invent targets.
    let graph = pass_over(&[
        ("go.mod", "module example.com/mod\n\ngo 1.22\n"),
        (
            "app/main.go",
            r#"package main

import "fmt"

func main() {
	println("builtin")
	fmt.Println("stdlib")
}
"#,
        ),
    ]);

    let calls = edges_of_kind(&graph, EdgeKind::Calls);
    assert!(
        calls.is_empty(),
        "nothing the repo declares is called, got {calls:?}"
    );
}

#[test]
fn every_import_connects_the_file_to_a_package_node() {
    let graph = pass_over(&[(
        "app/main.go",
        r#"package main

import (
	"fmt"
	"golang.org/x/net/html"
)

func main() {}
"#,
    )]);

    let mut packages: Vec<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Package)
        .map(|n| n.id.as_str())
        .collect();
    packages.sort_unstable();
    assert_eq!(
        packages,
        vec!["pkg:fmt", "pkg:golang.org/x/net/html"],
        "each import path is a Package node"
    );

    let imports = edges_of_kind(&graph, EdgeKind::Imports);
    assert_eq!(imports.len(), 2, "got {imports:?}");
    for edge in &imports {
        assert_eq!(edge.src, "file:app/main.go");
        assert_eq!(edge.provenance, Provenance::Syntactic);
        assert!(
            (edge.confidence - 1.0).abs() < 1e-6,
            "the import statement is witnessed, not guessed: {}",
            edge.confidence
        );
    }
    assert_eq!(imports[0].dst, "pkg:fmt");
    assert_eq!(
        imports[0].location.as_deref(),
        Some("app/main.go:4:2"),
        "the import spec's site"
    );
    assert_eq!(imports[1].dst, "pkg:golang.org/x/net/html");
    assert_eq!(imports[1].location.as_deref(), Some("app/main.go:5:2"));
}

#[test]
fn a_package_imported_twice_is_one_node_with_two_edges() {
    let graph = pass_over(&[
        ("a.go", "package gadgets\n\nimport \"fmt\"\n"),
        ("b.go", "package gadgets\n\nimport \"fmt\"\n"),
    ]);

    let packages: Vec<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Package)
        .map(|n| n.id.as_str())
        .collect();
    assert_eq!(packages, vec!["pkg:fmt"], "node ids are a primary key");

    let mut sources: Vec<&str> = edges_of_kind(&graph, EdgeKind::Imports)
        .iter()
        .map(|e| e.src.as_str())
        .collect();
    sources.sort_unstable();
    assert_eq!(sources, vec!["file:a.go", "file:b.go"]);
}

#[test]
fn an_in_repo_import_also_reaches_the_packages_files() {
    // go.mod places example.com/mod/util inside this repo: besides the
    // Package node, the importing file links to the package's Go files
    // (RFC 0001 §5: IMPORTS is File → File/Package). The directory
    // resolution is heuristic — 0.9, not the Package edge's 1.0 — and
    // non-Go files in the directory are not the package.
    let graph = pass_over(&[
        ("go.mod", "module example.com/mod\n\ngo 1.22\n"),
        ("util/a.go", "package util\n\nfunc A() {}\n"),
        ("util/b.go", "package util\n\nfunc B() {}\n"),
        ("util/README.md", "# util\n"),
        (
            "app/main.go",
            "package main\n\nimport \"example.com/mod/util\"\n\nfunc main() {\n\tutil.A()\n}\n",
        ),
    ]);

    let imports = edges_of_kind(&graph, EdgeKind::Imports);
    let mut to_files: Vec<(&str, f64)> = imports
        .iter()
        .filter(|e| e.dst.starts_with("file:"))
        .map(|e| (e.dst.as_str(), e.confidence))
        .collect();
    to_files.sort_by(|a, b| a.0.cmp(b.0));
    assert_eq!(
        to_files,
        vec![("file:util/a.go", 0.9), ("file:util/b.go", 0.9)],
        "the in-repo package's Go files, at heuristic confidence"
    );
    for edge in imports.iter().filter(|e| e.dst.starts_with("file:")) {
        assert_eq!(edge.src, "file:app/main.go");
        assert_eq!(
            edge.location.as_deref(),
            Some("app/main.go:3:8"),
            "file edges still locate the import spec"
        );
    }
    assert!(
        imports.iter().any(|e| e.dst == "pkg:example.com/mod/util"),
        "the Package edge is still there: {imports:?}"
    );
}

#[test]
fn embedded_structs_and_interfaces_produce_extends_edges() {
    let graph = pass_over(&[(
        "shapes.go",
        r#"package gadgets

type Base struct{}

type Widget struct {
	*Base
	Name string
}

type Reader interface {
	Read()
}

type ReadCloser interface {
	Reader
	Close()
}
"#,
    )]);

    let mut extends: Vec<(&str, &str, Option<&str>)> = edges_of_kind(&graph, EdgeKind::Extends)
        .iter()
        .map(|e| (e.src.as_str(), e.dst.as_str(), e.location.as_deref()))
        .collect();
    extends.sort_unstable();
    assert_eq!(
        extends,
        vec![
            (
                "sym:shapes.go#ReadCloser",
                "sym:shapes.go#Reader",
                Some("shapes.go:15:2"),
            ),
            (
                "sym:shapes.go#Widget",
                "sym:shapes.go#Base",
                Some("shapes.go:6:2"),
            ),
        ],
        "each embedding is an EXTENDS edge located at the embedded field"
    );
    for edge in edges_of_kind(&graph, EdgeKind::Extends) {
        assert_eq!(edge.provenance, Provenance::Syntactic);
        assert!(
            (edge.confidence - 0.9).abs() < 1e-6,
            "unique syntactic matches, got {}",
            edge.confidence
        );
    }
}

#[test]
fn a_type_covering_an_interfaces_methods_implements_it() {
    // Method-set matching by name only (signatures are invisible to a
    // reasonable syntactic pass): capped at 0.5, and only complete
    // coverage counts. Repo-wide — Go interfaces are satisfied across
    // package boundaries.
    let graph = pass_over(&[
        (
            "draw/renderer.go",
            r#"package draw

type Renderer interface {
	Render() string
	Reset()
}
"#,
        ),
        (
            "gadgets/widget.go",
            r#"package gadgets

type Widget struct{}

func (w *Widget) Render() string { return "" }

func (w *Widget) Reset() {}

type Gadget struct{}

func (g Gadget) Render() string { return "" }
"#,
        ),
    ]);

    let implements = edges_of_kind(&graph, EdgeKind::Implements);
    assert_eq!(
        implements.len(),
        1,
        "Widget covers Renderer; Gadget (no Reset) does not: {implements:?}"
    );
    assert_eq!(implements[0].src, "sym:gadgets/widget.go#Widget");
    assert_eq!(implements[0].dst, "sym:draw/renderer.go#Renderer");
    assert_eq!(implements[0].provenance, Provenance::Syntactic);
    assert!(
        (implements[0].confidence - 0.5).abs() < 1e-6,
        "name-only matching is capped at 0.5, got {}",
        implements[0].confidence
    );
}

#[test]
fn empty_interfaces_are_implemented_by_nothing() {
    // Everything satisfies interface{} / any: edges to it would be pure
    // noise, so an interface with no direct methods matches nothing.
    let graph = pass_over(&[(
        "any.go",
        r#"package gadgets

type Anything interface{}

type Widget struct{}

func (w Widget) Render() {}
"#,
    )]);

    let implements = edges_of_kind(&graph, EdgeKind::Implements);
    assert!(implements.is_empty(), "got {implements:?}");
}

#[test]
fn two_calls_on_one_line_stay_distinct_edges() {
    // Without the column, both edges would be byte-identical rows —
    // indistinguishable, and silently collapsible by any consumer that
    // dedups.
    let graph = pass_over(&[(
        "pair.go",
        "package gadgets\n\nfunc a() {}\n\nfunc B() { a(); a() }\n",
    )]);

    let sites: Vec<Option<&str>> = edges_of_kind(&graph, EdgeKind::Calls)
        .iter()
        .map(|e| e.location.as_deref())
        .collect();
    assert_eq!(
        sites,
        vec![Some("pair.go:5:12"), Some("pair.go:5:17")],
        "same line, two columns"
    );
}

#[test]
fn blank_and_dot_imports_still_connect_the_file_to_their_packages() {
    // `import _ "…"` (side-effect registration) and `import . "…"` are
    // witnessed import statements like any other: they must produce the
    // Package node and the 1.0 IMPORTS edge even though neither
    // introduces a name call sites could qualify with.
    let graph = pass_over(&[(
        "db.go",
        r#"package db

import _ "github.com/lib/pq"
import . "math"
"#,
    )]);

    let mut packages: Vec<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Package)
        .map(|n| n.id.as_str())
        .collect();
    packages.sort_unstable();
    assert_eq!(
        packages,
        vec!["pkg:github.com/lib/pq", "pkg:math"],
        "blank and dot imports are still imports"
    );
    let imports = edges_of_kind(&graph, EdgeKind::Imports);
    assert_eq!(imports.len(), 2, "got {imports:?}");
    for edge in imports {
        assert!(
            (edge.confidence - 1.0).abs() < 1e-6,
            "witnessed statements stay 1.0: {edge:?}"
        );
    }
}

#[test]
fn an_empty_import_path_mints_no_unaddressable_package() {
    // `import ""` parses (tree-sitter) but is illegal Go — mid-edit
    // garbage. A `pkg:` node with an empty path could never round-trip
    // through the external id grammar, so the spec is skipped whole.
    let graph = pass_over(&[("broken.go", "package gadgets\n\nimport \"\"\n")]);

    assert!(
        !graph.nodes.iter().any(|n| n.kind == NodeKind::Package),
        "no Package node for an empty import path"
    );
    assert!(
        edges_of_kind(&graph, EdgeKind::Imports).is_empty(),
        "no IMPORTS edge either"
    );
}

#[test]
fn a_go_mod_module_directive_tolerates_trailing_comments() {
    // `module example.com/mod // renamed in v2` is legal go.mod syntax;
    // the comment must not poison the module path, or every in-repo
    // resolution for the whole repo silently goes quiet.
    let graph = pass_over(&[
        (
            "go.mod",
            "module example.com/mod // renamed in v2\n\ngo 1.22\n",
        ),
        ("util/reset.go", "package util\n\nfunc Reset() {}\n"),
        (
            "app/main.go",
            "package main\n\nimport \"example.com/mod/util\"\n\nfunc main() {\n\tutil.Reset()\n}\n",
        ),
    ]);

    let calls = edges_of_kind(&graph, EdgeKind::Calls);
    assert_eq!(calls.len(), 1, "the qualified call must resolve: {calls:?}");
    assert_eq!(calls[0].dst, "sym:util/reset.go#Reset");
}

#[test]
fn a_go_mod_that_is_not_utf8_never_fails_the_pass() {
    // Synced repos are arbitrary: anything can be named go.mod. A file
    // the module parser can't read costs module resolution, never the
    // whole index job (which would retry forever, identically).
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("junk")).unwrap();
    std::fs::write(dir.path().join("junk/go.mod"), [0xFF, 0xFE, 0x00, 0xD8]).unwrap();
    std::fs::write(
        dir.path().join("main.go"),
        "package main\n\nfunc main() {}\n",
    )
    .unwrap();

    let graph = yg_index::syntactic_pass(dir.path())
        .expect("a junk go.mod must not fail the whole pass")
        .0;
    assert!(
        graph.nodes.iter().any(|n| n.id == "sym:main.go#main"),
        "the rest of the repo still indexes"
    );
}

#[test]
fn a_nested_module_is_resolved_through_its_own_go_mod_only() {
    // util/ has its own go.mod: per Go semantics the nested module owns
    // its subtree, so an import of it must resolve exactly once — not
    // once via the parent's prefix and again via the nested module.
    let graph = pass_over(&[
        ("go.mod", "module example.com/mod\n\ngo 1.22\n"),
        ("util/go.mod", "module example.com/mod/util\n\ngo 1.22\n"),
        ("util/reset.go", "package util\n\nfunc Reset() {}\n"),
        (
            "app/main.go",
            "package main\n\nimport \"example.com/mod/util\"\n\nfunc main() {\n\tutil.Reset()\n}\n",
        ),
    ]);

    let file_imports: Vec<&Edge> = edges_of_kind(&graph, EdgeKind::Imports)
        .into_iter()
        .filter(|e| e.dst == "file:util/reset.go")
        .collect();
    assert_eq!(
        file_imports.len(),
        1,
        "one import spec, one edge per target file: {file_imports:?}"
    );
    let calls = edges_of_kind(&graph, EdgeKind::Calls);
    assert_eq!(calls.len(), 1, "and one resolved call: {calls:?}");
    assert!(
        (calls[0].confidence - 0.9).abs() < 1e-6,
        "a single candidate, not a doubled one: {}",
        calls[0].confidence
    );
}

#[test]
fn in_repo_import_edges_never_point_at_the_importing_file_itself() {
    // An external test package (util_test) lives in the same directory
    // as the package it imports: the importing file must not appear as
    // its own import target.
    let graph = pass_over(&[
        ("go.mod", "module example.com/mod\n\ngo 1.22\n"),
        ("util/util.go", "package util\n\nfunc Reset() {}\n"),
        (
            "util/util_test.go",
            "package util_test\n\nimport \"example.com/mod/util\"\n\nfunc check() {\n\tutil.Reset()\n}\n",
        ),
    ]);

    let from_test: Vec<&str> = edges_of_kind(&graph, EdgeKind::Imports)
        .into_iter()
        .filter(|e| e.src == "file:util/util_test.go" && e.dst.starts_with("file:"))
        .map(|e| e.dst.as_str())
        .collect();
    assert_eq!(
        from_test,
        vec!["file:util/util.go"],
        "the sibling, never the file itself"
    );
}

#[test]
fn a_union_constraint_interface_is_not_misread_as_embedding() {
    // `Number interface { Int | Float }` is a generic constraint, not
    // an embedding: it must not emit an EXTENDS edge to Int (silently
    // dropping Float). Embedding stays reserved for `interface { Base }`.
    let graph = pass_over(&[(
        "constraints.go",
        r#"package gadgets

type Int int

type Float float64

type Number interface {
	Int | Float
}
"#,
    )]);

    assert!(
        edges_of_kind(&graph, EdgeKind::Extends).is_empty(),
        "a union constraint embeds nothing: {:?}",
        edges_of_kind(&graph, EdgeKind::Extends)
    );
}

#[test]
fn an_embedded_interface_still_extends_the_named_interface() {
    // The flip side: a lone embedded interface is still an embedding.
    let graph = pass_over(&[(
        "io.go",
        r#"package gadgets

type Reader interface {
	Read()
}

type ReadCloser interface {
	Reader
	Close()
}
"#,
    )]);

    let extends: Vec<(&str, &str)> = edges_of_kind(&graph, EdgeKind::Extends)
        .iter()
        .map(|e| (e.src.as_str(), e.dst.as_str()))
        .collect();
    assert_eq!(
        extends,
        vec![("sym:io.go#ReadCloser", "sym:io.go#Reader")],
        "the embedded interface is an EXTENDS edge"
    );
}

#[test]
fn an_in_repo_interface_embed_chain_matches_its_transitive_method_set() {
    let graph = pass_over(&[(
        "impl.go",
        r#"package gadgets

type Reader interface {
	Read()
}

type ReadAlias interface {
	Reader
}

type ReadWriter interface {
	ReadAlias
	Write()
}

type ReadWriteCloser interface {
	ReadWriter
	Close()
}

type CloseOnly struct{}

func (c CloseOnly) Close() {}

type MissingRead struct{}

func (m MissingRead) Write() {}

func (m MissingRead) Close() {}

type All struct{}

func (a All) Read() {}

func (a All) Write() {}

func (a All) Close() {}
"#,
    )]);

    let implements: Vec<(&str, &str)> = edges_of_kind(&graph, EdgeKind::Implements)
        .iter()
        .map(|e| (e.src.as_str(), e.dst.as_str()))
        .collect();
    assert_eq!(
        implements,
        vec![
            ("sym:impl.go#All", "sym:impl.go#Reader"),
            ("sym:impl.go#All", "sym:impl.go#ReadAlias"),
            ("sym:impl.go#All", "sym:impl.go#ReadWriter"),
            ("sym:impl.go#All", "sym:impl.go#ReadWriteCloser"),
        ],
        "All covers every interface in the transitive chain"
    );
    for edge in edges_of_kind(&graph, EdgeKind::Implements) {
        assert_eq!(edge.confidence, 0.5, "name-only confidence stays capped");
        assert_eq!(edge.location, None);
    }
}

#[test]
fn an_interface_with_an_external_embed_stays_unmatchable() {
    let graph = pass_over(&[(
        "external.go",
        r#"package gadgets

import "io"

type ReadCloser interface {
	io.Reader
	Close()
}

type Candidate struct{}

func (c Candidate) Read(p []byte) (int, error) { return 0, nil }

func (c Candidate) Close() error { return nil }
"#,
    )]);

    assert!(
        edges_of_kind(&graph, EdgeKind::Implements).is_empty(),
        "an external method set must not be guessed"
    );
}

#[test]
fn mutually_embedding_interfaces_terminate_without_partial_matches() {
    let graph = pass_over(&[(
        "cycle.go",
        r#"package gadgets

type A interface {
	B
	F()
}

type B interface {
	A
	G()
}

type Both struct{}

func (b Both) F() {}

func (b Both) G() {}
"#,
    )]);

    assert!(
        edges_of_kind(&graph, EdgeKind::Implements).is_empty(),
        "a cyclic closure must be skipped instead of matching a partial set"
    );
}

#[test]
fn an_ambiguous_interface_embed_stays_unmatchable() {
    let graph = pass_over(&[
        ("a.go", "package gadgets\n\ntype Base interface { F() }\n"),
        ("b.go", "package gadgets\n\ntype Base interface { G() }\n"),
        (
            "consumer.go",
            r#"package gadgets

type Consumer interface {
	Base
	Close()
}

type All struct{}

func (a All) F() {}

func (a All) G() {}

func (a All) Close() {}
"#,
        ),
    ]);

    let implements = edges_of_kind(&graph, EdgeKind::Implements);
    assert!(
        !implements
            .iter()
            .any(|edge| edge.dst == "sym:consumer.go#Consumer"),
        "an ambiguous embedded name must not fold either candidate: {implements:?}"
    );
}

#[test]
fn a_single_type_constraint_does_not_extend_a_concrete_type() {
    // `interface { MyInt }` is a single-type generic constraint, not an
    // embedded interface (you cannot embed a concrete type in an
    // interface). It must not emit EXTENDS → MyInt.
    let graph = pass_over(&[(
        "constraint.go",
        r#"package gadgets

type MyInt int

type Number interface {
	MyInt
}
"#,
    )]);

    assert!(
        edges_of_kind(&graph, EdgeKind::Extends).is_empty(),
        "an interface extends only interfaces, not concrete types: {:?}",
        edges_of_kind(&graph, EdgeKind::Extends)
    );
}

#[test]
fn an_interface_embedding_another_interface_still_extends_it() {
    // The legitimate case must survive the concrete-type filter.
    let graph = pass_over(&[(
        "x.go",
        "package g\n\ntype A interface { F() }\n\ntype B interface {\n\tA\n\tG()\n}\n",
    )]);
    let extends: Vec<(&str, &str)> = edges_of_kind(&graph, EdgeKind::Extends)
        .iter()
        .map(|e| (e.src.as_str(), e.dst.as_str()))
        .collect();
    assert_eq!(extends, vec![("sym:x.go#B", "sym:x.go#A")]);
}

#[test]
fn a_dot_import_suppresses_repo_wide_resolution_of_an_unqualified_call() {
    // `import . "fmt"` brings Println into scope unqualified. A bare
    // `Println()` is fmt's (external) — it must NOT resolve to an
    // unrelated repo package's func of the same name.
    let graph = pass_over(&[
        ("go.mod", "module example.com/mod\n\ngo 1.22\n"),
        ("other/p.go", "package other\n\nfunc Println() {}\n"),
        (
            "app/main.go",
            "package main\n\nimport . \"fmt\"\n\nfunc main() {\n\tPrintln(\"hi\")\n}\n",
        ),
    ]);

    let calls = edges_of_kind(&graph, EdgeKind::Calls);
    assert!(
        calls.is_empty(),
        "a dot-imported name must not resolve to a far-off repo function: {calls:?}"
    );
}

#[test]
fn a_dot_import_still_resolves_a_same_package_call() {
    // The suppression only drops the repo-wide fallback; a genuinely
    // same-package callee still resolves.
    let graph = pass_over(&[(
        "app/main.go",
        "package main\n\nimport . \"fmt\"\n\nfunc helper() {}\n\nfunc main() {\n\thelper()\n}\n",
    )]);

    let calls = edges_of_kind(&graph, EdgeKind::Calls);
    assert_eq!(calls.len(), 1, "got {calls:?}");
    assert_eq!(calls[0].dst, "sym:app/main.go#helper");
}

#[test]
fn an_ambiguous_in_repo_import_spreads_confidence_across_candidate_dirs() {
    // Two go.mod files declaring the SAME module path (a broken/mid-edit
    // repo) make an import resolve to two candidate directories. Those
    // directories are alternatives, so per ADR 0006 the File→File edges
    // spread confidence across them (0.9/2), rather than asserting 0.9
    // for each as if both were certain.
    let graph = pass_over(&[
        ("a/go.mod", "module example.com/dup\n\ngo 1.22\n"),
        ("a/a.go", "package a\n\nfunc A() {}\n"),
        ("b/go.mod", "module example.com/dup\n\ngo 1.22\n"),
        ("b/b.go", "package b\n\nfunc B() {}\n"),
        (
            "app/main.go",
            "package main\n\nimport \"example.com/dup\"\n\nfunc main() {}\n",
        ),
    ]);

    let mut file_edges: Vec<(&str, f64)> = edges_of_kind(&graph, EdgeKind::Imports)
        .iter()
        .filter(|e| e.dst.starts_with("file:"))
        .map(|e| (e.dst.as_str(), e.confidence))
        .collect();
    file_edges.sort_by(|x, y| x.0.cmp(y.0));
    assert_eq!(
        file_edges,
        vec![("file:a/a.go", 0.45), ("file:b/b.go", 0.45)],
        "two candidate directories split 0.9"
    );
}

#[test]
fn an_unambiguous_in_repo_import_keeps_full_confidence_across_its_files() {
    // The common case: one resolved directory. All its Go files are
    // genuinely imported together (not alternatives), so each keeps the
    // full SYNTACTIC_MATCH — the spread is across directories, never
    // across files within one package.
    let graph = pass_over(&[
        ("go.mod", "module example.com/mod\n\ngo 1.22\n"),
        ("util/a.go", "package util\n\nfunc A() {}\n"),
        ("util/b.go", "package util\n\nfunc B() {}\n"),
        (
            "app/main.go",
            "package main\n\nimport \"example.com/mod/util\"\n\nfunc main() {}\n",
        ),
    ]);

    let confidences: Vec<f64> = edges_of_kind(&graph, EdgeKind::Imports)
        .iter()
        .filter(|e| e.dst.starts_with("file:"))
        .map(|e| e.confidence)
        .collect();
    assert_eq!(confidences.len(), 2, "both package files are linked");
    for c in confidences {
        assert!(
            (c - 0.9).abs() < 1e-6,
            "one directory, full confidence per file, got {c}"
        );
    }
}
