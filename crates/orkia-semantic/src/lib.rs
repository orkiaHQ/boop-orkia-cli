//! Tree-sitter backed extraction of intra-commit review atoms.

use orkia_model::{
    AtomDependency, AtomId, AtomKind, ChangeAtom, DependencyKind, EventId, Hash, SemanticBranch,
    SemanticLeaf, SemanticNodeId, SemanticNodeState, SemanticTrunk, TrunkState,
};
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
    extract_atoms_in_ranges(file, &[(file.changed_start, file.changed_end)])
}

/// Extracts semantic boundaries that overlap at least one concrete changed
/// line range.  Git's compact `changed_start..changed_end` envelope is useful
/// for compatibility, but it can include an unchanged symbol between two
/// distant hunks; callers with a real diff should use this function so that
/// such symbols never become review units accidentally.
pub fn extract_atoms_in_ranges(
    file: &ChangedFile,
    changed_ranges: &[(u32, u32)],
) -> Vec<ChangeAtom> {
    if let Some(kind) = path_atom_kind(&file.path) {
        return vec![hunk_atom_kind(file, kind)];
    }
    let language = Language::from_path(&file.path);
    let mut parser = Parser::new();
    let Some(grammar) = language.grammar() else {
        return vec![hunk_atom_kind(file, AtomKind::Hunk)];
    };
    if parser.set_language(&grammar).is_err() {
        return vec![hunk_atom_kind(file, AtomKind::Hunk)];
    }
    let Some(tree) = parser.parse(&file.content, None) else {
        return vec![hunk_atom_kind(file, AtomKind::Hunk)];
    };
    let mut nodes = Vec::new();
    collect_named(&mut tree.walk(), &mut nodes);
    let block_node = nodes
        .iter()
        .find(|node| {
            let range = node.range();
            let start = range.start_point.row as u32 + 1;
            let end = range.end_point.row as u32 + 1;
            is_block_boundary(node.kind())
                && changed_ranges.iter().any(|(changed_start, changed_end)| {
                    end >= *changed_start && start <= *changed_end
                })
        })
        .copied();
    let mut atoms: Vec<_> = nodes
        .into_iter()
        .filter_map(|node| {
            let range = node.range();
            let start = range.start_point.row as u32 + 1;
            let end = range.end_point.row as u32 + 1;
            if !changed_ranges
                .iter()
                .any(|(changed_start, changed_end)| end >= *changed_start && start <= *changed_end)
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
        if let Some(node) = block_node {
            let range = node.range();
            let start = range.start_point.row as u32 + 1;
            let end = range.end_point.row as u32 + 1;
            let text = node.utf8_text(file.content.as_bytes()).unwrap_or_default();
            atoms.push(atom(
                file,
                AtomKind::Block,
                symbol_name(text, node.kind()),
                start,
                end,
                text,
            ));
        } else {
            atoms.push(hunk_atom_kind(file, AtomKind::Hunk));
        }
    }
    atoms.sort_by(|a, b| {
        (a.start_line, &a.path, &a.content_hash).cmp(&(b.start_line, &b.path, &b.content_hash))
    });
    stabilize_symbol_atom_ids(&mut atoms);
    atoms
}

/// Returns the new-file line ranges touched by a line diff.  It is deliberately
/// conservative for deletions: a deletion with no surviving new line is
/// anchored at the nearest surviving line so a surrounding semantic symbol is
/// still reviewable.  The ranges are deterministic and disjoint.
pub fn changed_line_ranges(old: &str, new: &str) -> Vec<(u32, u32)> {
    let old = old.split_inclusive('\n').collect::<Vec<_>>();
    let new = new.split_inclusive('\n').collect::<Vec<_>>();
    if old == new {
        return Vec::new();
    }
    if old.is_empty() {
        return if new.is_empty() {
            Vec::new()
        } else {
            vec![(1, new.len() as u32)]
        };
    }
    if new.is_empty() {
        return vec![(1, 1)];
    }
    let mut lcs = vec![vec![0_usize; new.len() + 1]; old.len() + 1];
    for old_index in (0..old.len()).rev() {
        for new_index in (0..new.len()).rev() {
            lcs[old_index][new_index] = if old[old_index] == new[new_index] {
                lcs[old_index + 1][new_index + 1] + 1
            } else {
                lcs[old_index + 1][new_index].max(lcs[old_index][new_index + 1])
            };
        }
    }
    let (mut old_index, mut new_index) = (0_usize, 0_usize);
    let mut ranges = Vec::<(u32, u32)>::new();
    while old_index < old.len() || new_index < new.len() {
        if old_index < old.len() && new_index < new.len() && old[old_index] == new[new_index] {
            old_index += 1;
            new_index += 1;
            continue;
        }
        let start_new = new_index;
        while old_index < old.len() && new_index < new.len() && old[old_index] != new[new_index] {
            // Prefer consuming a new line on a tie. This makes an insertion
            // between two unchanged lines stay a one-line range instead of
            // swallowing the unchanged successor as well; deletions still
            // win when the LCS score proves that the old line is unmatched.
            if lcs[old_index + 1][new_index] > lcs[old_index][new_index + 1] {
                old_index += 1;
            } else {
                new_index += 1;
            }
        }
        if old_index == old.len() {
            new_index = new.len();
        } else if new_index == new.len() {
            old_index = old.len();
        }
        let end_new = new_index;
        let anchor = start_new.min(new.len().saturating_sub(1));
        let range = if end_new > start_new {
            (start_new as u32 + 1, end_new as u32)
        } else {
            (anchor as u32 + 1, anchor as u32 + 1)
        };
        if let Some((_, previous_end)) = ranges.last_mut()
            && range.0 <= *previous_end + 1
        {
            *previous_end = (*previous_end).max(range.1);
        } else {
            ranges.push(range);
        }
    }
    ranges
}

/// Builds the Git-native Trunk → Branch → Leaf overlay for one analysed file.
///
/// `previous` is supplied by the caller after Git has identified a file
/// continuation (same path, rename, or explicit reconciliation). Its Trunk
/// identity is retained and its vanished nodes remain as tombstones.
pub fn extract_trunk(
    repository: &str,
    blob: &str,
    file: &ChangedFile,
    previous: Option<&SemanticTrunk>,
) -> SemanticTrunk {
    let id = previous.map(|trunk| trunk.id.clone()).unwrap_or_else(|| {
        SemanticNodeId::from_stable_parts(&[
            b"orkia:trunk:v1",
            repository.as_bytes(),
            file.path.as_bytes(),
            blob.as_bytes(),
        ])
    });
    let mut paths = previous
        .map(|trunk| trunk.paths.clone())
        .unwrap_or_default();
    paths.insert(file.path.clone());

    let mut branches = Vec::new();
    // The occurrence is scoped to a semantic key rather than the absolute
    // source position, so inserting an unrelated symbol before this one does
    // not renumber its stable Branch identity.
    let mut occurrences: BTreeMap<(String, String), u64> = BTreeMap::new();
    for atom in extract_atoms(file) {
        let symbol = atom.symbol.clone().unwrap_or_default();
        let occurrence = occurrences
            .entry((atom.kind.stable_tag().into(), symbol.clone()))
            .or_default();
        let ordinal = (*occurrence).to_be_bytes();
        *occurrence += 1;
        let branch_id = SemanticNodeId::from_stable_parts(&[
            b"orkia:branch:v1",
            id.0.as_bytes(),
            atom.kind.stable_tag().as_bytes(),
            symbol.as_bytes(),
            &ordinal,
        ]);
        let source = source_for_atom(file, &atom);
        let mut leaves = source
            .split_whitespace()
            .enumerate()
            .map(|(index, token)| {
                let index = (index as u64).to_be_bytes();
                let text_hash = hex::encode(Sha256::digest(token.as_bytes()));
                SemanticLeaf {
                    id: SemanticNodeId::from_stable_parts(&[
                        b"orkia:leaf:v1",
                        id.0.as_bytes(),
                        branch_id.0.as_bytes(),
                        &index,
                        text_hash.as_bytes(),
                    ]),
                    state: SemanticNodeState::Alive,
                    text_hash,
                }
            })
            .collect::<Vec<_>>();
        if let Some(old_branch) =
            previous.and_then(|trunk| trunk.branches.iter().find(|branch| branch.id == branch_id))
        {
            let current_leaf_ids = leaves
                .iter()
                .map(|leaf| leaf.id.clone())
                .collect::<BTreeSet<_>>();
            leaves.extend(
                old_branch
                    .leaves
                    .iter()
                    .filter(|leaf| !current_leaf_ids.contains(&leaf.id))
                    .map(|leaf| SemanticLeaf {
                        id: leaf.id.clone(),
                        state: SemanticNodeState::Deleted,
                        text_hash: leaf.text_hash.clone(),
                    }),
            );
        }
        branches.push(SemanticBranch {
            id: branch_id,
            state: SemanticNodeState::Alive,
            source_hash: atom.content_hash,
            leaves,
        });
    }
    let current_branch_ids = branches
        .iter()
        .map(|branch| branch.id.clone())
        .collect::<BTreeSet<_>>();
    if let Some(previous) = previous {
        branches.extend(
            previous
                .branches
                .iter()
                .filter(|branch| !current_branch_ids.contains(&branch.id))
                .map(|branch| SemanticBranch {
                    id: branch.id.clone(),
                    state: SemanticNodeState::Deleted,
                    source_hash: branch.source_hash.clone(),
                    leaves: branch
                        .leaves
                        .iter()
                        .map(|leaf| SemanticLeaf {
                            id: leaf.id.clone(),
                            state: SemanticNodeState::Deleted,
                            text_hash: leaf.text_hash.clone(),
                        })
                        .collect(),
                }),
        );
    }
    branches.sort_by(|left, right| left.id.cmp(&right.id));
    SemanticTrunk {
        id,
        state: TrunkState::Alive,
        paths,
        branches,
    }
}

/// Retains a removed file as a tombstone; callers set `still_required` when a
/// live operation or resolution refers to this Trunk, yielding Atomic's zombie
/// state without replacing Git's file deletion.
pub fn delete_trunk(trunk: &SemanticTrunk, still_required: bool) -> SemanticTrunk {
    let mut deleted = trunk.clone();
    deleted.state = if still_required {
        TrunkState::Zombie
    } else {
        TrunkState::Deleted
    };
    for branch in &mut deleted.branches {
        branch.state = SemanticNodeState::Deleted;
        for leaf in &mut branch.leaves {
            leaf.state = SemanticNodeState::Deleted;
        }
    }
    deleted
}

fn source_for_atom(file: &ChangedFile, atom: &ChangeAtom) -> String {
    file.content
        .lines()
        .skip(atom.start_line.saturating_sub(1) as usize)
        .take((atom.end_line.saturating_sub(atom.start_line) + 1) as usize)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Produces conservative edges only. Every changed file is a closed Git
/// projection boundary; test atoms then depend on implementation atoms.
pub fn infer_dependencies(atoms: &[ChangeAtom]) -> Vec<AtomDependency> {
    let mut output = Vec::new();
    // A single captured action can legitimately touch several semantic atoms
    // (for example one apply_patch call edits two files). Keep that causal
    // signal soft: it informs confidence without fabricating Git parentage or
    // forcing independent atoms into one hard component.
    for (index, left) in atoms.iter().enumerate() {
        for right in atoms.iter().skip(index + 1) {
            if left
                .source_events
                .intersection(&right.source_events)
                .next()
                .is_some()
            {
                output.push(AtomDependency {
                    from: left.id.clone(),
                    to: right.id.clone(),
                    kind: DependencyKind::Causal,
                    confidence_milli: 700,
                });
            }
        }
    }
    let mut by_path: BTreeMap<&str, Vec<&ChangeAtom>> = BTreeMap::new();
    for atom in atoms {
        by_path.entry(&atom.path).or_default().push(atom);
    }
    for group in by_path.values() {
        for (index, left) in group.iter().enumerate() {
            for right in group.iter().skip(index + 1) {
                if left.start_line <= right.end_line && right.start_line <= left.end_line {
                    // Nested AST boundaries (for example an impl and a
                    // method) must stay together because their fragments
                    // overlap. Disjoint sibling symbols are intentionally
                    // independent and may become parallel StackPullRequests.
                    output.push(AtomDependency {
                        from: left.id.clone(),
                        to: right.id.clone(),
                        kind: DependencyKind::Hard,
                        confidence_milli: 1000,
                    });
                } else if left.kind == AtomKind::Import && right.kind != AtomKind::Import {
                    output.push(AtomDependency {
                        from: right.id.clone(),
                        to: left.id.clone(),
                        kind: DependencyKind::Import,
                        confidence_milli: 950,
                    });
                } else if right.kind == AtomKind::Import && left.kind != AtomKind::Import {
                    output.push(AtomDependency {
                        from: left.id.clone(),
                        to: right.id.clone(),
                        kind: DependencyKind::Import,
                        confidence_milli: 950,
                    });
                }
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
        (&left.from, &left.to)
            .cmp(&(&right.from, &right.to))
            .then_with(|| right.confidence_milli.cmp(&left.confidence_milli))
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
fn is_block_boundary(kind: &str) -> bool {
    matches!(
        kind,
        "block" | "compound_statement" | "block_statement" | "statement_block"
    )
}
fn path_atom_kind(path: &str) -> Option<AtomKind> {
    let lower = path.to_ascii_lowercase();
    if lower.contains("migration") || lower.ends_with(".sql") {
        return Some(AtomKind::Migration);
    }
    let configuration = [
        ".toml", ".yaml", ".yml", ".json", ".xml", ".ini", ".cfg", ".conf", ".env",
    ];
    configuration
        .iter()
        .any(|extension| lower.ends_with(extension))
        .then_some(AtomKind::Configuration)
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
    let kind_tag = kind.stable_tag();
    let symbol = symbol.unwrap_or_default();
    ChangeAtom {
        // Keep the review atom stable across ordinary body edits. Content is
        // recorded separately in `content_hash`; identity names the logical
        // review boundary, not one particular blob revision. Trunk manifests
        // retain continuity across recognized Git renames.
        id: AtomId::from_stable_parts(&[
            b"orkia:atom:v2",
            file.path.as_bytes(),
            kind_tag.as_bytes(),
            symbol.as_bytes(),
            &start_line.to_be_bytes(),
            &end_line.to_be_bytes(),
        ]),
        kind,
        path: file.path.clone(),
        symbol: (!symbol.is_empty()).then_some(symbol),
        start_line,
        end_line,
        content_hash,
        source_events: file.source_events.clone(),
    }
}

fn stabilize_symbol_atom_ids(atoms: &mut [ChangeAtom]) {
    let mut occurrences: BTreeMap<(String, String, String), u64> = BTreeMap::new();
    for atom in atoms.iter_mut().filter(|atom| atom.symbol.is_some()) {
        let symbol = atom.symbol.as_deref().unwrap_or_default();
        let occurrence = occurrences
            .entry((
                atom.path.clone(),
                atom.kind.stable_tag().into(),
                symbol.into(),
            ))
            .or_default();
        let ordinal = (*occurrence).to_be_bytes();
        *occurrence += 1;
        atom.id = AtomId::from_stable_parts(&[
            b"orkia:atom:v3",
            atom.path.as_bytes(),
            atom.kind.stable_tag().as_bytes(),
            symbol.as_bytes(),
            &ordinal,
        ]);
    }
}
fn hunk_atom_kind(file: &ChangedFile, kind: AtomKind) -> ChangeAtom {
    let lines: Vec<_> = file
        .content
        .lines()
        .skip(file.changed_start.saturating_sub(1) as usize)
        .take((file.changed_end.saturating_sub(file.changed_start) + 1) as usize)
        .collect();
    atom(
        file,
        kind,
        None,
        file.changed_start,
        file.changed_end,
        &lines.join("\n"),
    )
}

/// Conservatively merges two text revisions relative to a common base at a
/// token boundary. It succeeds only when their edit spans are disjoint, or
/// when both sides made the exact same edit. Whitespace and punctuation are
/// retained as tokens, so the returned text is byte-for-byte deterministic.
///
/// `None` is a proof of *insufficient* information, not a best effort: the
/// caller must retain the ordinary Git conflict in that case.
pub fn merge_token_text(base: &str, left: &str, right: &str) -> Option<String> {
    if left == right {
        return Some(left.into());
    }
    if left == base {
        return Some(right.into());
    }
    if right == base {
        return Some(left.into());
    }
    let base = lexical_tokens(base);
    let left = lexical_tokens(left);
    let right = lexical_tokens(right);
    let left_edits = token_edits(&base, &left);
    let right_edits = token_edits(&base, &right);
    merge_token_edits(&base, &left_edits, &right_edits).map(|tokens| tokens.concat())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TokenEdit {
    start: usize,
    end: usize,
    replacement: Vec<String>,
}

fn lexical_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut word = None;
    for character in text.chars() {
        let is_word = character.is_alphanumeric() || character == '_';
        if word == Some(is_word) {
            current.push(character);
        } else {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            current.push(character);
            word = Some(is_word);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn token_edits(base: &[String], side: &[String]) -> Vec<TokenEdit> {
    let mut lcs = vec![vec![0usize; side.len() + 1]; base.len() + 1];
    for base_index in (0..base.len()).rev() {
        for side_index in (0..side.len()).rev() {
            lcs[base_index][side_index] = if base[base_index] == side[side_index] {
                lcs[base_index + 1][side_index + 1] + 1
            } else {
                lcs[base_index + 1][side_index].max(lcs[base_index][side_index + 1])
            };
        }
    }
    let mut edits = Vec::new();
    let mut base_index = 0;
    let mut side_index = 0;
    let mut active: Option<TokenEdit> = None;
    while base_index < base.len() || side_index < side.len() {
        if base_index < base.len()
            && side_index < side.len()
            && base[base_index] == side[side_index]
        {
            if let Some(edit) = active.take() {
                edits.push(edit);
            }
            base_index += 1;
            side_index += 1;
        } else if base_index < base.len()
            && (side_index == side.len()
                || lcs[base_index + 1][side_index] >= lcs[base_index][side_index + 1])
        {
            let edit = active.get_or_insert_with(|| TokenEdit {
                start: base_index,
                end: base_index,
                replacement: Vec::new(),
            });
            edit.end += 1;
            base_index += 1;
        } else {
            let edit = active.get_or_insert_with(|| TokenEdit {
                start: base_index,
                end: base_index,
                replacement: Vec::new(),
            });
            edit.replacement.push(side[side_index].clone());
            side_index += 1;
        }
    }
    if let Some(edit) = active {
        edits.push(edit);
    }
    edits
}

fn merge_token_edits(
    base: &[String],
    left: &[TokenEdit],
    right: &[TokenEdit],
) -> Option<Vec<String>> {
    let mut output = Vec::new();
    let mut position = 0;
    let mut left_index = 0;
    let mut right_index = 0;
    while left_index < left.len() || right_index < right.len() {
        match (left.get(left_index), right.get(right_index)) {
            (Some(left), Some(right)) if left.start == right.start => {
                if left == right {
                    output.extend_from_slice(&base[position..left.start]);
                    output.extend(left.replacement.clone());
                    position = left.end;
                    left_index += 1;
                    right_index += 1;
                } else {
                    return None;
                }
            }
            (Some(left), Some(right)) if left.start < right.start => {
                if right.start < left.end {
                    return None;
                }
                output.extend_from_slice(&base[position..left.start]);
                output.extend(left.replacement.clone());
                position = left.end;
                left_index += 1;
            }
            (Some(_), Some(right)) => {
                let left = &left[left_index];
                if left.start < right.end {
                    return None;
                }
                output.extend_from_slice(&base[position..right.start]);
                output.extend(right.replacement.clone());
                position = right.end;
                right_index += 1;
            }
            (Some(left), None) => {
                output.extend_from_slice(&base[position..left.start]);
                output.extend(left.replacement.clone());
                position = left.end;
                left_index += 1;
            }
            (None, Some(right)) => {
                output.extend_from_slice(&base[position..right.start]);
                output.extend(right.replacement.clone());
                position = right.end;
                right_index += 1;
            }
            (None, None) => break,
        }
    }
    output.extend_from_slice(&base[position..]);
    Some(output)
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
    fn configuration_and_migration_files_have_explicit_atom_kinds() {
        let configuration = extract_atoms(&ChangedFile {
            path: "orkia.toml".into(),
            content: "minimum_confidence_milli = 800\n".into(),
            changed_start: 1,
            changed_end: 1,
            source_events: BTreeSet::new(),
        });
        assert_eq!(configuration[0].kind, AtomKind::Configuration);
        let migration = extract_atoms(&ChangedFile {
            path: "db/migrations/001_add_users.sql".into(),
            content: "CREATE TABLE users (id INTEGER);\n".into(),
            changed_start: 1,
            changed_end: 1,
            source_events: BTreeSet::new(),
        });
        assert_eq!(migration[0].kind, AtomKind::Migration);
    }

    #[test]
    fn nested_code_blocks_are_retained_as_block_atoms() {
        let atoms = extract_atoms(&ChangedFile {
            path: "script.js".into(),
            content: "if (true) { console.log(\"x\"); }\n".into(),
            changed_start: 1,
            changed_end: 1,
            source_events: BTreeSet::new(),
        });
        assert!(atoms.iter().any(|atom| atom.kind == AtomKind::Block));
    }
    #[test]
    fn disjoint_symbols_in_same_file_can_be_parallel_projections() {
        let atoms = extract_atoms(&ChangedFile {
            path: "lib.rs".into(),
            content: "fn one() {}\nfn two() {}\n".into(),
            changed_start: 1,
            changed_end: 2,
            source_events: BTreeSet::new(),
        });
        assert!(infer_dependencies(&atoms).is_empty());
    }

    #[test]
    fn concrete_diff_ranges_exclude_an_unchanged_symbol_between_hunks() {
        let old = "fn one() {}\nfn untouched() {}\nfn three() {}\n";
        let new = "fn one() { 1 }\nfn untouched() {}\nfn three() { 3 }\n";
        let ranges = changed_line_ranges(old, new);
        let atoms = extract_atoms_in_ranges(
            &ChangedFile {
                path: "lib.rs".into(),
                content: new.into(),
                changed_start: 1,
                changed_end: 3,
                source_events: BTreeSet::new(),
            },
            &ranges,
        );
        let symbols = atoms
            .iter()
            .filter_map(|atom| atom.symbol.as_deref())
            .collect::<BTreeSet<_>>();
        assert!(symbols.contains("one"));
        assert!(symbols.contains("three"));
        assert!(!symbols.contains("untouched"));
    }

    #[test]
    fn line_ranges_keep_insertions_and_deletions_local() {
        assert_eq!(changed_line_ranges("a\nb\n", "a\nx\nb\n"), vec![(2, 2)]);
        assert_eq!(changed_line_ranges("a\nx\nb\n", "a\nb\n"), vec![(2, 2)]);
    }

    #[test]
    fn overlapping_symbols_in_same_file_remain_a_hard_dependency() {
        let atoms = vec![
            ChangeAtom {
                id: AtomId::new(),
                kind: AtomKind::Symbol,
                path: "lib.rs".into(),
                symbol: Some("outer".into()),
                start_line: 1,
                end_line: 4,
                content_hash: "outer".into(),
                source_events: BTreeSet::new(),
            },
            ChangeAtom {
                id: AtomId::new(),
                kind: AtomKind::Symbol,
                path: "lib.rs".into(),
                symbol: Some("inner".into()),
                start_line: 2,
                end_line: 3,
                content_hash: "inner".into(),
                source_events: BTreeSet::new(),
            },
        ];
        assert!(
            infer_dependencies(&atoms)
                .iter()
                .any(|dependency| dependency.kind == DependencyKind::Hard)
        );
    }

    #[test]
    fn shared_captured_action_is_a_soft_causal_dependency() {
        let event = EventId::new();
        let left = ChangeAtom {
            id: AtomId::new(),
            kind: AtomKind::Symbol,
            path: "a.rs".into(),
            symbol: Some("left".into()),
            start_line: 1,
            end_line: 1,
            content_hash: "left".into(),
            source_events: BTreeSet::from([event.clone()]),
        };
        let right = ChangeAtom {
            id: AtomId::new(),
            kind: AtomKind::Symbol,
            path: "b.rs".into(),
            symbol: Some("right".into()),
            start_line: 1,
            end_line: 1,
            content_hash: "right".into(),
            source_events: BTreeSet::from([event]),
        };
        assert!(infer_dependencies(&[left, right]).iter().any(|dependency| {
            dependency.kind == DependencyKind::Causal && dependency.confidence_milli == 700
        }));
    }

    #[test]
    fn unchanged_content_produces_stable_atom_ids() {
        let file = ChangedFile {
            path: "lib.rs".into(),
            content: "fn stable() {}\n".into(),
            changed_start: 1,
            changed_end: 1,
            source_events: BTreeSet::new(),
        };
        assert_eq!(extract_atoms(&file), extract_atoms(&file));
    }

    #[test]
    fn editing_a_symbol_retains_its_review_atom_identity() {
        let original = ChangedFile {
            path: "lib.rs".into(),
            content: "fn stable() { 1 }\n".into(),
            changed_start: 1,
            changed_end: 1,
            source_events: BTreeSet::new(),
        };
        let edited = ChangedFile {
            content: "fn stable() { 2 }\n".into(),
            ..original.clone()
        };
        let first = extract_atoms(&original);
        let next = extract_atoms(&edited);
        assert_eq!(first[0].id, next[0].id);
        assert_ne!(first[0].content_hash, next[0].content_hash);
    }

    #[test]
    fn inserting_another_symbol_before_it_retains_review_atom_identity() {
        let original = ChangedFile {
            path: "lib.rs".into(),
            content: "fn stable() {}\n".into(),
            changed_start: 1,
            changed_end: 1,
            source_events: BTreeSet::new(),
        };
        let inserted = ChangedFile {
            content: "fn added() {}\nfn stable() {}\n".into(),
            changed_start: 1,
            changed_end: 2,
            ..original.clone()
        };
        let first = extract_atoms(&original);
        let next = extract_atoms(&inserted);
        let stable = next
            .iter()
            .find(|atom| atom.symbol.as_deref() == Some("stable"))
            .unwrap();
        assert_eq!(first[0].id, stable.id);
    }

    #[test]
    fn rename_retains_trunk_identity_and_historical_path() {
        let original = ChangedFile {
            path: "src/old.rs".into(),
            content: "fn stable() {}\n".into(),
            changed_start: 1,
            changed_end: 1,
            source_events: BTreeSet::new(),
        };
        let first = extract_trunk("repo-1", "blob-1", &original, None);
        let renamed = ChangedFile {
            path: "src/new.rs".into(),
            ..original
        };
        let continued = extract_trunk("repo-1", "blob-1", &renamed, Some(&first));
        assert_eq!(continued.id, first.id);
        assert!(continued.paths.contains("src/old.rs"));
        assert!(continued.paths.contains("src/new.rs"));
    }

    #[test]
    fn editing_a_symbol_retains_its_branch_and_tombstones_replaced_leaves() {
        let original = ChangedFile {
            path: "src/lib.rs".into(),
            content: "fn stable() { 1 }\n".into(),
            changed_start: 1,
            changed_end: 1,
            source_events: BTreeSet::new(),
        };
        let first = extract_trunk("repo-1", "blob-1", &original, None);
        let edited = ChangedFile {
            content: "fn stable() { 2 }\n".into(),
            ..original
        };
        let next = extract_trunk("repo-1", "blob-2", &edited, Some(&first));
        assert_eq!(next.branches[0].id, first.branches[0].id);
        assert!(
            next.branches[0]
                .leaves
                .iter()
                .any(|leaf| leaf.state == SemanticNodeState::Deleted)
        );
    }

    #[test]
    fn inserting_an_unrelated_symbol_does_not_renumber_existing_branches() {
        let original = ChangedFile {
            path: "src/lib.rs".into(),
            content: "fn stable() {}\n".into(),
            changed_start: 1,
            changed_end: 1,
            source_events: BTreeSet::new(),
        };
        let first = extract_trunk("repo-1", "blob-1", &original, None);
        let inserted = ChangedFile {
            content: "fn added() {}\nfn stable() {}\n".into(),
            changed_start: 1,
            changed_end: 2,
            ..original
        };
        let next = extract_trunk("repo-1", "blob-2", &inserted, Some(&first));
        let stable_hash = hex::encode(Sha256::digest(b"fn stable() {}"));
        let stable = next
            .branches
            .iter()
            .find(|branch| branch.source_hash == stable_hash)
            .unwrap();
        assert_eq!(stable.id, first.branches[0].id);
    }

    #[test]
    fn deleting_a_required_trunk_yields_a_zombie_tombstone() {
        let file = ChangedFile {
            path: "src/lib.rs".into(),
            content: "fn stable() {}\n".into(),
            changed_start: 1,
            changed_end: 1,
            source_events: BTreeSet::new(),
        };
        let trunk = extract_trunk("repo-1", "blob-1", &file, None);
        let zombie = delete_trunk(&trunk, true);
        assert_eq!(zombie.state, TrunkState::Zombie);
        assert!(
            zombie
                .branches
                .iter()
                .all(|branch| branch.state == SemanticNodeState::Deleted)
        );
    }

    #[test]
    fn token_merge_combines_disjoint_edits_on_the_same_line() {
        assert_eq!(
            merge_token_text(
                "let left = 1; let right = 2;\n",
                "let left = 10; let right = 2;\n",
                "let left = 1; let right = 20;\n",
            ),
            Some("let left = 10; let right = 20;\n".into())
        );
    }

    #[test]
    fn token_merge_refuses_competing_edits_of_the_same_token() {
        assert_eq!(
            merge_token_text("let value = 1;\n", "let value = 2;\n", "let value = 3;\n"),
            None
        );
    }
}
