//! Tree-sitter backed extraction of intra-commit review atoms.

use orkia_model::{AtomDependency, AtomId, AtomKind, ChangeAtom, DependencyKind, EventId, Hash};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::{Language as TsLanguage, Parser, TreeCursor};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Language {
    Rust,
    TypeScript,
    JavaScript,
    Python,
    Go,
    Java,
    C,
    Cpp,
    Swift,
    Unknown,
}

impl Language {
    pub fn from_path(path: &str) -> Self {
        match path.rsplit('.').next().unwrap_or_default() {
            "rs" => Self::Rust,
            "ts" | "tsx" => Self::TypeScript,
            "js" | "jsx" | "mjs" | "cjs" => Self::JavaScript,
            "py" => Self::Python,
            "go" => Self::Go,
            "java" => Self::Java,
            "c" | "h" => Self::C,
            "cc" | "cpp" | "cxx" | "hpp" | "hh" => Self::Cpp,
            "swift" => Self::Swift,
            _ => Self::Unknown,
        }
    }
    fn grammar(self) -> Option<TsLanguage> {
        match self {
            Self::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
            Self::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
            Self::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
            Self::Python => Some(tree_sitter_python::LANGUAGE.into()),
            Self::Go => Some(tree_sitter_go::LANGUAGE.into()),
            Self::Java => Some(tree_sitter_java::LANGUAGE.into()),
            Self::C => Some(tree_sitter_c::LANGUAGE.into()),
            Self::Cpp => Some(tree_sitter_cpp::LANGUAGE.into()),
            Self::Swift => Some(tree_sitter_swift::LANGUAGE.into()),
            Self::Unknown => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChangedFile {
    pub path: String,
    pub content: String,
    pub changed_start: u32,
    pub changed_end: u32,
    pub source_events: BTreeSet<EventId>,
}

pub fn extract_atoms(file: &ChangedFile) -> Vec<ChangeAtom> {
    let language = Language::from_path(&file.path);
    let mut parser = Parser::new();
    let Some(grammar) = language.grammar() else {
        return vec![hunk_atom(file)];
    };
    if parser.set_language(&grammar).is_err() {
        return vec![hunk_atom(file)];
    }
    let Some(tree) = parser.parse(&file.content, None) else {
        return vec![hunk_atom(file)];
    };
    let mut nodes = Vec::new();
    collect_named(&mut tree.walk(), &mut nodes);
    let mut atoms: Vec<_> = nodes
        .into_iter()
        .filter_map(|node| {
            let range = node.range();
            let start = range.start_point.row as u32 + 1;
            let end = range.end_point.row as u32 + 1;
            if end < file.changed_start
                || start > file.changed_end
                || !is_review_boundary(node.kind())
            {
                return None;
            }
            let kind = if node.kind().contains("import") || node.kind().contains("use_") {
                AtomKind::Import
            } else if node.kind().contains("test") {
                AtomKind::Test
            } else {
                AtomKind::Symbol
            };
            let text = node.utf8_text(file.content.as_bytes()).unwrap_or_default();
            Some(atom(
                file,
                kind,
                symbol_name(text, node.kind()),
                start,
                end,
                text,
            ))
        })
        .collect();
    if atoms.is_empty() {
        atoms.push(hunk_atom(file));
    }
    atoms.sort_by(|a, b| {
        (a.start_line, &a.path, &a.content_hash).cmp(&(b.start_line, &b.path, &b.content_hash))
    });
    atoms
}

/// Produces conservative edges only. Every changed file is a closed Git
/// projection boundary; test atoms then depend on implementation atoms.
pub fn infer_dependencies(atoms: &[ChangeAtom]) -> Vec<AtomDependency> {
    let mut output = Vec::new();
    let mut by_path: BTreeMap<&str, Vec<&ChangeAtom>> = BTreeMap::new();
    for atom in atoms {
        by_path.entry(&atom.path).or_default().push(atom);
    }
    for group in by_path.values() {
        for (index, left) in group.iter().enumerate() {
            for right in group.iter().skip(index + 1) {
                output.push(AtomDependency {
                    from: left.id.clone(),
                    to: right.id.clone(),
                    kind: DependencyKind::Hard,
                    confidence_milli: 1000,
                });
            }
        }
    }
    let tests: Vec<_> = atoms
        .iter()
        .filter(|atom| {
            atom.kind == AtomKind::Test
                || atom
                    .symbol
                    .as_deref()
                    .is_some_and(|symbol| symbol.contains("test"))
        })
        .collect();
    let implementations: Vec<_> = atoms
        .iter()
        .filter(|atom| !tests.iter().any(|test| test.id == atom.id))
        .collect();
    for test in tests {
        for implementation in &implementations {
            if test.path != implementation.path {
                output.push(AtomDependency {
                    from: test.id.clone(),
                    to: implementation.id.clone(),
                    kind: DependencyKind::Test,
                    confidence_milli: 900,
                });
            }
        }
    }
    output.sort_by(|left, right| {
        (&left.from, &left.to, left.confidence_milli).cmp(&(
            &right.from,
            &right.to,
            right.confidence_milli,
        ))
    });
    output.dedup_by(|left, right| left.from == right.from && left.to == right.to);
    output
}

fn collect_named<'a>(cursor: &mut TreeCursor<'a>, output: &mut Vec<tree_sitter::Node<'a>>) {
    loop {
        let node = cursor.node();
        if node.is_named() {
            output.push(node);
        }
        if cursor.goto_first_child() {
            collect_named(cursor, output);
            cursor.goto_parent();
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}
fn is_review_boundary(kind: &str) -> bool {
    matches!(
        kind,
        "function_item"
            | "function_definition"
            | "function_declaration"
            | "method_definition"
            | "method_declaration"
            | "class_definition"
            | "class_declaration"
            | "struct_item"
            | "enum_item"
            | "interface_declaration"
            | "impl_item"
            | "import_statement"
            | "import_declaration"
            | "use_declaration"
            | "lexical_declaration"
    )
}
fn symbol_name(text: &str, fallback: &str) -> Option<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .find(|word| {
            !word.is_empty()
                && !matches!(
                    *word,
                    "fn" | "func"
                        | "function"
                        | "class"
                        | "struct"
                        | "enum"
                        | "import"
                        | "use"
                        | "pub"
                        | "def"
                )
        })
        .map(str::to_owned)
        .or_else(|| Some(fallback.into()))
}
fn atom(
    file: &ChangedFile,
    kind: AtomKind,
    symbol: Option<String>,
    start_line: u32,
    end_line: u32,
    content: &str,
) -> ChangeAtom {
    let content_hash: Hash = hex::encode(Sha256::digest(content.as_bytes()));
    ChangeAtom {
        id: AtomId::new(),
        kind,
        path: file.path.clone(),
        symbol,
        start_line,
        end_line,
        content_hash,
        source_events: file.source_events.clone(),
    }
}
fn hunk_atom(file: &ChangedFile) -> ChangeAtom {
    let lines: Vec<_> = file
        .content
        .lines()
        .skip(file.changed_start.saturating_sub(1) as usize)
        .take((file.changed_end.saturating_sub(file.changed_start) + 1) as usize)
        .collect();
    atom(
        file,
        AtomKind::Hunk,
        None,
        file.changed_start,
        file.changed_end,
        &lines.join("\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rust_function_becomes_symbol_atom() {
        let atoms = extract_atoms(&ChangedFile {
            path: "lib.rs".into(),
            content: "pub fn hello() {}\n".into(),
            changed_start: 1,
            changed_end: 1,
            source_events: BTreeSet::new(),
        });
        assert!(atoms.iter().any(|a| a.kind == AtomKind::Symbol));
    }
    #[test]
    fn symbols_in_same_file_are_one_closed_projection() {
        let atoms = extract_atoms(&ChangedFile {
            path: "lib.rs".into(),
            content: "fn one() {}\nfn two() {}\n".into(),
            changed_start: 1,
            changed_end: 2,
            source_events: BTreeSet::new(),
        });
        assert!(!infer_dependencies(&atoms).is_empty());
    }
}
