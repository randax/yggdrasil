use std::collections::BTreeSet;
use std::path::PathBuf;

use yg_shard::{Edge, EdgeKind, Graph, NodeKind};

use super::{FileDegradation, MAX_SOURCE_FILE_BYTES, syntactic_pass, syntactic_pass_with_io};

struct LanguageFixture {
    name: &'static str,
    extension: &'static str,
    declaration: &'static str,
    caller: &'static str,
    caller_symbol: &'static str,
    nested: &'static str,
    inner_symbol: &'static str,
    outer_symbol: &'static str,
    nested_call_count: usize,
}

impl LanguageFixture {
    fn is_ecmascript(&self) -> bool {
        matches!(self.extension, "ts" | "tsx" | "js")
    }
}

const SIMPLE_LANGUAGES: &[LanguageFixture] = &[
    LanguageFixture {
        name: "TypeScript",
        extension: "ts",
        declaration: "export function target() {}\n",
        caller: "export function caller() { target(); }\n",
        caller_symbol: "caller",
        nested: "function target() {}\nfunction outer() { function inner() { target(); new target(); } }\n",
        inner_symbol: "inner",
        outer_symbol: "outer",
        nested_call_count: 2,
    },
    LanguageFixture {
        name: "TSX",
        extension: "tsx",
        declaration: "export function target() { return null; }\n",
        caller: "export function caller() { target(); return null; }\n",
        caller_symbol: "caller",
        nested: "function target() { return null; }\nfunction outer() { function inner() { target(); new target(); } }\n",
        inner_symbol: "inner",
        outer_symbol: "outer",
        nested_call_count: 2,
    },
    LanguageFixture {
        name: "JavaScript",
        extension: "js",
        declaration: "export function target() {}\n",
        caller: "export function caller() { target(); }\n",
        caller_symbol: "caller",
        nested: "function target() {}\nfunction outer() { function inner() { target(); new target(); } }\n",
        inner_symbol: "inner",
        outer_symbol: "outer",
        nested_call_count: 2,
    },
    LanguageFixture {
        name: "Python",
        extension: "py",
        declaration: "def target():\n    pass\n",
        caller: "def caller():\n    target()\n",
        caller_symbol: "caller",
        nested: "def target():\n    pass\n\ndef outer():\n    def inner():\n        target()\n",
        inner_symbol: "inner",
        outer_symbol: "outer",
        nested_call_count: 1,
    },
    LanguageFixture {
        name: "Rust",
        extension: "rs",
        declaration: "fn target() {}\n",
        caller: "fn caller() { target(); }\n",
        caller_symbol: "caller",
        nested: "fn target() {}\nfn outer() { fn inner() { target(); } }\n",
        inner_symbol: "inner",
        outer_symbol: "outer",
        nested_call_count: 1,
    },
    LanguageFixture {
        name: "Java",
        extension: "java",
        declaration: "class Target { static void target() {} }\n",
        caller: "class Caller { void caller() { target(); } }\n",
        caller_symbol: "Caller.caller",
        nested: "class Target { static void target() {} }\nclass Outer { void outer() { class Inner { void inner() { target(); new Target(); } } } }\n",
        inner_symbol: "Inner.inner",
        outer_symbol: "Outer.outer",
        nested_call_count: 2,
    },
];

fn pass_over(files: &[(String, String)]) -> Graph {
    let dir = tempfile::tempdir().unwrap();
    for (path, contents) in files {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
    }
    syntactic_pass(dir.path()).unwrap().0
}

fn calls_from<'a>(graph: &'a Graph, path: &str, symbol_name: &str) -> Vec<&'a Edge> {
    let caller = graph
        .nodes
        .iter()
        .find(|node| {
            node.kind == NodeKind::Symbol
                && node.path.as_deref() == Some(path)
                && node.name.as_deref() == Some(symbol_name)
        })
        .unwrap_or_else(|| panic!("missing caller {symbol_name} in {path}"));
    graph
        .edges
        .iter()
        .filter(|edge| edge.kind == EdgeKind::Calls && edge.src == caller.id)
        .collect()
}

fn target_paths<'a>(graph: &'a Graph, calls: &[&Edge]) -> Vec<&'a str> {
    let mut paths = calls
        .iter()
        .map(|edge| {
            graph
                .nodes
                .iter()
                .find(|node| node.id == edge.dst)
                .and_then(|node| node.path.as_deref())
                .unwrap_or_else(|| panic!("missing CALLS target {}", edge.dst))
        })
        .collect::<Vec<_>>();
    paths.sort_unstable();
    paths
}

#[test]
fn calls_cross_only_within_the_ecmascript_family() {
    for caller_language in SIMPLE_LANGUAGES {
        for target_language in SIMPLE_LANGUAGES {
            if caller_language.name == target_language.name
                || (caller_language.is_ecmascript() && target_language.is_ecmascript())
            {
                continue;
            }
            let caller_path = format!("caller.{}", caller_language.extension);
            let target_path = format!("target.{}", target_language.extension);
            let graph = pass_over(&[
                (caller_path.clone(), caller_language.caller.to_string()),
                (target_path, target_language.declaration.to_string()),
            ]);
            assert!(
                calls_from(&graph, &caller_path, caller_language.caller_symbol).is_empty(),
                "{} caller resolved into {}",
                caller_language.name,
                target_language.name
            );
        }
    }
}

#[test]
fn ecmascript_grammars_share_directory_and_repository_resolution_tiers() {
    let cases = [("tsx", "ts"), ("js", "ts"), ("jsx", "js")];
    for (caller_extension, target_extension) in cases {
        let caller_path = format!("scope/caller.{caller_extension}");
        let caller = "export function caller() { target(); return null; }\n";
        let declaration = "export function target() { return null; }\n";

        let same_directory = pass_over(&[
            (caller_path.clone(), caller.to_string()),
            (
                format!("scope/target.{target_extension}"),
                declaration.to_string(),
            ),
            (
                format!("remote/target.{target_extension}"),
                declaration.to_string(),
            ),
        ]);
        let calls = calls_from(&same_directory, &caller_path, "caller");
        assert_eq!(
            target_paths(&same_directory, &calls),
            vec![format!("scope/target.{target_extension}")],
            "{caller_extension} must resolve into {target_extension} and prefer its directory"
        );

        let repository = pass_over(&[
            (caller_path.clone(), caller.to_string()),
            (
                format!("blue/target.{target_extension}"),
                declaration.to_string(),
            ),
            (
                format!("red/target.{target_extension}"),
                declaration.to_string(),
            ),
        ]);
        let calls = calls_from(&repository, &caller_path, "caller");
        assert_eq!(
            target_paths(&repository, &calls),
            vec![
                format!("blue/target.{target_extension}"),
                format!("red/target.{target_extension}"),
            ],
            "{caller_extension} must retain repository-tier {target_extension} ambiguity"
        );
    }
}

#[test]
fn simple_language_resolution_uses_file_directory_then_repository_tiers() {
    for language in SIMPLE_LANGUAGES {
        let caller_path = format!("scope/caller.{}", language.extension);
        let same_file = pass_over(&[
            (
                caller_path.clone(),
                format!(
                    "{}{}{}",
                    language.declaration, language.declaration, language.caller
                ),
            ),
            (
                format!("scope/sibling.{}", language.extension),
                language.declaration.to_string(),
            ),
            (
                format!("remote/target.{}", language.extension),
                language.declaration.to_string(),
            ),
        ]);
        let calls = calls_from(&same_file, &caller_path, language.caller_symbol);
        assert_eq!(
            target_paths(&same_file, &calls),
            vec![caller_path.as_str(), caller_path.as_str()],
            "{} same-file tier",
            language.name
        );
        assert!(
            calls
                .iter()
                .all(|call| (call.confidence - 0.45).abs() < 1e-6),
            "{} same-file ambiguity must spread ADR 0006 confidence: {calls:?}",
            language.name
        );

        let same_directory = pass_over(&[
            (caller_path.clone(), language.caller.to_string()),
            (
                format!("scope/sibling_a.{}", language.extension),
                language.declaration.to_string(),
            ),
            (
                format!("scope/sibling_b.{}", language.extension),
                language.declaration.to_string(),
            ),
            (
                format!("remote/target.{}", language.extension),
                language.declaration.to_string(),
            ),
        ]);
        let calls = calls_from(&same_directory, &caller_path, language.caller_symbol);
        assert_eq!(
            target_paths(&same_directory, &calls),
            vec![
                format!("scope/sibling_a.{}", language.extension),
                format!("scope/sibling_b.{}", language.extension),
            ],
            "{} same-directory tier",
            language.name
        );
        assert!(
            calls
                .iter()
                .all(|call| (call.confidence - 0.45).abs() < 1e-6),
            "{} same-directory ambiguity must spread ADR 0006 confidence: {calls:?}",
            language.name
        );

        let repository = pass_over(&[
            (caller_path.clone(), language.caller.to_string()),
            (
                format!("blue/target.{}", language.extension),
                language.declaration.to_string(),
            ),
            (
                format!("red/target.{}", language.extension),
                language.declaration.to_string(),
            ),
        ]);
        let calls = calls_from(&repository, &caller_path, language.caller_symbol);
        assert_eq!(
            target_paths(&repository, &calls),
            vec![
                format!("blue/target.{}", language.extension),
                format!("red/target.{}", language.extension),
            ],
            "{} repository tier retains residual ambiguity",
            language.name
        );
        assert!(
            calls
                .iter()
                .all(|call| (call.confidence - 0.45).abs() < 1e-6),
            "{} repository ambiguity must spread ADR 0006 confidence: {calls:?}",
            language.name
        );
    }
}

#[test]
fn nested_invocations_belong_only_to_the_innermost_declaration() {
    for language in SIMPLE_LANGUAGES {
        let path = format!("nested.{}", language.extension);
        let graph = pass_over(&[(path.clone(), language.nested.to_string())]);

        let inner_calls = calls_from(&graph, &path, language.inner_symbol);
        assert_eq!(
            inner_calls.len(),
            language.nested_call_count,
            "{} inner calls must be retained: {inner_calls:?}",
            language.name
        );
        assert!(
            target_paths(&graph, &inner_calls)
                .iter()
                .all(|target| *target == path),
            "{} nested call targets: {inner_calls:?}",
            language.name
        );
        assert!(
            calls_from(&graph, &path, language.outer_symbol).is_empty(),
            "{} enclosing declaration must not also claim nested invocations",
            language.name
        );
    }
}

#[test]
fn ecmascript_nested_boundaries_preserve_exact_call_ownership() {
    for language in &SIMPLE_LANGUAGES[..3] {
        let path = format!("destructure.{}", language.extension);
        let graph = pass_over(&[(
            path.clone(),
            "function target() {}\nfunction outer() { const { value } = target(); }\n".to_string(),
        )]);
        assert_eq!(
            calls_from(&graph, &path, "outer").len(),
            1,
            "{} destructuring has no inner symbol, so outer retains the call",
            language.name
        );
    }

    for language in &SIMPLE_LANGUAGES[..2] {
        let path = format!("enum.{}", language.extension);
        let graph = pass_over(&[(
            path.clone(),
            "function target() {}\nfunction outer() { enum E { Value = target() } }\n".to_string(),
        )]);
        assert_eq!(
            calls_from(&graph, &path, "E").len(),
            1,
            "{} nested enum retains its initializer call",
            language.name
        );
        assert!(
            calls_from(&graph, &path, "outer").is_empty(),
            "{} enclosing function must not also claim the enum initializer",
            language.name
        );
    }
}

#[test]
fn oversized_and_unreadable_files_warn_and_retain_file_nodes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("good.rs"), "fn good() {}\n").unwrap();
    std::fs::write(
        dir.path().join("oversized.rs"),
        vec![b' '; (MAX_SOURCE_FILE_BYTES + 1) as usize],
    )
    .unwrap();
    let unreadable = dir.path().join("unreadable.rs");
    std::fs::write(&unreadable, "fn hidden() {}\n").unwrap();

    let mut warnings = Vec::new();
    let mut reads = Vec::new();
    let (graph, search_docs) = syntactic_pass_with_io(
        dir.path(),
        |path, degradation| {
            let kind = match degradation {
                FileDegradation::Stat(_) => "stat",
                FileDegradation::Oversized { .. } => "oversized",
                FileDegradation::Read(_) => "read",
            };
            warnings.push((path.to_path_buf(), kind));
        },
        |path: &std::path::Path| std::fs::metadata(path),
        |path| {
            reads.push(path.file_name().unwrap().to_os_string());
            assert_ne!(
                path.file_name().and_then(|name| name.to_str()),
                Some("oversized.rs"),
                "the size cap must prevent reading the oversized file"
            );
            if path.file_name().and_then(|name| name.to_str()) == Some("unreadable.rs") {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "fixture read failure",
                ))
            } else {
                std::fs::read(path)
            }
        },
    )
    .expect("per-file degradation must not abort the index job");

    let files = graph
        .nodes
        .iter()
        .filter(|node| node.kind == NodeKind::File)
        .map(|node| node.path.as_deref().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        files,
        BTreeSet::from(["good.rs", "oversized.rs", "unreadable.rs"])
    );
    assert_eq!(
        warnings,
        vec![
            (PathBuf::from("oversized.rs"), "oversized"),
            (PathBuf::from("unreadable.rs"), "read"),
        ]
    );
    assert_eq!(reads, vec!["good.rs", "unreadable.rs"]);
    for path in ["oversized.rs", "unreadable.rs"] {
        let document = search_docs
            .iter()
            .find(|document| document.path.as_deref() == Some(path))
            .unwrap_or_else(|| panic!("missing search document for {path}"));
        assert!(document.content.is_empty());
    }
    assert!(
        graph
            .nodes
            .iter()
            .any(|node| node.name.as_deref() == Some("good"))
    );
}
