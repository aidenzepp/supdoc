//! Naming symbols from `tags.scm` captures ([`tag_symbols`], 03 R4) and resolving a symbol path
//! against a file's [`SymbolTable`] (03 Symbols and resolution).
//!
//! # The naming rule
//!
//! A symbol's path is the segments of every enclosing `@definition.*` and `@supdoc.scope`,
//! outermost first, then any `@supdoc.scope.name` captured inside the definition's own pattern,
//! then the definition's `@name`. A definition inside a `@supdoc.scope` that has no name can't be
//! named, so it isn't a symbol.
//!
//! When a scope segment also captures its type arguments (`@supdoc.scope.arguments`,
//! `@supdoc.scope.qualifier.arguments`), the symbol additionally answers to the path with those
//! arguments written out, so `impl Foo<u8>` and `impl Foo<u16>` stay distinguishable (03 R5).

use std::collections::{HashMap, HashSet};

use tree_sitter::{Node, QueryCursor, StreamingIterator};

use crate::link::SymbolPath;
use crate::path::AbsolutePath;
use crate::read::cartridge::{SourceFile, Symbol, SymbolTable};
use crate::read::error::ResolveError;

/// How many near-misses a `NotFound` suggests.
const NEAREST: usize = 3;

/// The symbols `file` exposes, named by its cartridge's `tags.scm` (03 R4).
///
/// Symbols come back in file order: by start offset, outer before inner.
#[must_use]
pub fn tag_symbols(file: &SourceFile) -> SymbolTable {
    let query = file.language().queries().tags();
    let roles: Vec<Role> = query
        .capture_names()
        .iter()
        .map(|name| Role::of(name))
        .collect();
    let text = file.text();

    let mut definitions: Vec<Definition> = Vec::new();
    let mut seen_definitions = HashSet::new();
    let mut scopes: HashMap<usize, Scope> = HashMap::new();

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, file.tree().root_node(), text.as_bytes());
    while let Some(found) = matches.next() {
        let mut captured = Captured::default();
        for capture in found.captures() {
            let role = usize::try_from(capture.index)
                .ok()
                .and_then(|index| roles.get(index))
                .copied()
                .unwrap_or(Role::Other);
            captured.record(role, capture.node, text);
        }
        if let Some(node) = captured.definition {
            let Some(name) = captured.name else { continue };
            if seen_definitions.insert(node.id()) {
                definitions.push(Definition {
                    node,
                    parents: captured.scope.segments(),
                    name,
                });
            }
        } else if let Some(node) = captured.scope_node {
            scopes.entry(node.id()).or_default().merge(captured.scope);
        }
    }

    let own_segments: HashMap<usize, Vec<Segment>> = definitions
        .iter()
        .map(|definition| (definition.node.id(), definition.segments()))
        .collect();
    let mut symbols: Vec<Symbol> = definitions
        .iter()
        .filter_map(|definition| definition.symbol(&own_segments, &scopes))
        .collect();
    symbols.sort_by_key(|symbol| (symbol.range.start, std::cmp::Reverse(symbol.range.end)));
    SymbolTable::new(symbols)
}

/// What a capture name means to supdoc. Every other capture is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// `@definition.<kind>`: an addressable symbol's node.
    Definition,
    /// `@name`: the definition's own segment.
    Name,
    /// `@supdoc.scope`: a container that isn't addressable itself.
    Scope,
    /// `@supdoc.scope.name`: the container's segment, or a parent segment inside a definition.
    ScopeName,
    /// `@supdoc.scope.arguments`: type arguments written after the scope name.
    ScopeArguments,
    /// `@supdoc.scope.qualifier`: a second segment after the name (a trait).
    Qualifier,
    /// `@supdoc.scope.qualifier.arguments`: type arguments written after the qualifier.
    QualifierArguments,
    /// Anything else (`@reference.*`, `@doc`, …).
    Other,
}

impl Role {
    /// The role of the capture called `name`.
    fn of(name: &str) -> Self {
        match name {
            "name" => Self::Name,
            "supdoc.scope" => Self::Scope,
            "supdoc.scope.name" => Self::ScopeName,
            "supdoc.scope.arguments" => Self::ScopeArguments,
            "supdoc.scope.qualifier" => Self::Qualifier,
            "supdoc.scope.qualifier.arguments" => Self::QualifierArguments,
            _ if name.starts_with("definition.") => Self::Definition,
            _ => Self::Other,
        }
    }
}

/// One path segment, with the type arguments it was written with, if the query captured them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Segment {
    /// The bare name: `Foo`.
    name: String,
    /// The arguments with whitespace removed: `<u8,u16>`.
    arguments: Option<String>,
}

impl Segment {
    /// A segment with no arguments.
    fn plain(name: String) -> Self {
        Self {
            name,
            arguments: None,
        }
    }

    /// The segment as written in full: `Foo<u8>`.
    fn full(&self) -> String {
        match &self.arguments {
            Some(arguments) => format!("{}{arguments}", self.name),
            None => self.name.clone(),
        }
    }
}

/// The scope captures of one match, or of every match on one `@supdoc.scope` node merged.
#[derive(Debug, Clone, Default)]
struct Scope {
    /// `@supdoc.scope.name`.
    name: Option<String>,
    /// `@supdoc.scope.arguments`.
    arguments: Option<String>,
    /// `@supdoc.scope.qualifier`.
    qualifier: Option<String>,
    /// `@supdoc.scope.qualifier.arguments`.
    qualifier_arguments: Option<String>,
}

impl Scope {
    /// Fills in whatever `other` captured that `self` hasn't. Several patterns may match one node
    /// (one for its type, one for its trait).
    fn merge(&mut self, other: Self) {
        self.name = self.name.take().or(other.name);
        self.arguments = self.arguments.take().or(other.arguments);
        self.qualifier = self.qualifier.take().or(other.qualifier);
        self.qualifier_arguments = self
            .qualifier_arguments
            .take()
            .or(other.qualifier_arguments);
    }

    /// The segments this scope adds: its name, then its qualifier. Empty when it has no name.
    fn segments(self) -> Vec<Segment> {
        let Some(name) = self.name else {
            return Vec::new();
        };
        let mut segments = vec![Segment {
            name,
            arguments: self.arguments,
        }];
        if let Some(qualifier) = self.qualifier {
            segments.push(Segment {
                name: qualifier,
                arguments: self.qualifier_arguments,
            });
        }
        segments
    }
}

/// The captures of one query match, sorted by role.
#[derive(Debug, Default)]
struct Captured<'tree> {
    /// The `@definition.*` node.
    definition: Option<Node<'tree>>,
    /// The `@name` text.
    name: Option<String>,
    /// The `@supdoc.scope` node.
    scope_node: Option<Node<'tree>>,
    /// The `@supdoc.scope.*` texts.
    scope: Scope,
}

impl<'tree> Captured<'tree> {
    /// Files one capture under its role. Text that isn't valid at the node's range is skipped.
    fn record(&mut self, role: Role, node: Node<'tree>, text: &str) {
        let content = || text.get(node.byte_range()).map(str::to_owned);
        let compact = || {
            text.get(node.byte_range())
                .map(|raw| raw.split_whitespace().collect::<String>())
        };
        match role {
            Role::Definition => self.definition = Some(node),
            Role::Name => self.name = content(),
            Role::Scope => self.scope_node = Some(node),
            Role::ScopeName => self.scope.name = content(),
            Role::ScopeArguments => self.scope.arguments = compact(),
            Role::Qualifier => self.scope.qualifier = content(),
            Role::QualifierArguments => self.scope.qualifier_arguments = compact(),
            Role::Other => {}
        }
    }
}

/// A `@definition.*` match: its node and the segments its own pattern captured.
#[derive(Debug)]
struct Definition<'tree> {
    /// The node whose bytes are the symbol.
    node: Node<'tree>,
    /// `@supdoc.scope.*` segments captured inside the definition's pattern (a Go receiver).
    parents: Vec<Segment>,
    /// `@name`.
    name: String,
}

impl Definition<'_> {
    /// The segments this definition contributes to its own path and to anything nested in it.
    fn segments(&self) -> Vec<Segment> {
        let mut segments = self.parents.clone();
        segments.push(Segment::plain(self.name.clone()));
        segments
    }

    /// The symbol, named by containment. `None` when an enclosing scope has no name or a segment
    /// isn't a valid path segment.
    fn symbol(
        &self,
        definitions: &HashMap<usize, Vec<Segment>>,
        scopes: &HashMap<usize, Scope>,
    ) -> Option<Symbol> {
        let mut enclosing: Vec<Vec<Segment>> = Vec::new();
        let mut ancestor = self.node.parent();
        while let Some(node) = ancestor {
            if let Some(scope) = scopes.get(&node.id()) {
                // A nameless scope can't contribute a segment, so nothing inside it is nameable.
                let _named = scope.name.as_ref()?;
                enclosing.push(scope.clone().segments());
            }
            if let Some(segments) = definitions.get(&node.id()) {
                enclosing.push(segments.clone());
            }
            ancestor = node.parent();
        }
        let segments: Vec<Segment> = enclosing
            .into_iter()
            .rev()
            .flatten()
            .chain(self.segments())
            .collect();

        let path =
            SymbolPath::from_segments(segments.iter().map(|s| s.name.clone()).collect()).ok()?;
        let aliases = if segments.iter().any(|segment| segment.arguments.is_some()) {
            SymbolPath::from_segments(segments.iter().map(Segment::full).collect())
                .ok()
                .into_iter()
                .collect()
        } else {
            Vec::new()
        };
        Some(Symbol {
            path,
            aliases,
            range: self.node.byte_range(),
        })
    }
}

impl SymbolTable {
    /// The one symbol `query` names in this table, which was built from `file`.
    ///
    /// A path that equals a symbol's full path or one of its aliases names that symbol. A
    /// one-segment path that matches no full path names every symbol whose last segment it is
    /// (docref's `#add` shorthand), so it resolves only when that is unique (03 R5a).
    ///
    /// # Errors
    ///
    /// - [`ResolveError::Ambiguous`] when several symbols match, listing every full path.
    /// - [`ResolveError::NotFound`] when none does, with the [`SymbolTable::nearest`] paths.
    pub fn resolve(
        &self,
        file: &AbsolutePath,
        query: &SymbolPath,
    ) -> Result<&Symbol, ResolveError> {
        let exact: Vec<&Symbol> = self
            .iter()
            .filter(|symbol| symbol.path == *query || symbol.aliases.contains(query))
            .collect();
        let matches = if exact.is_empty() && query.segments().len() == 1 {
            self.iter()
                .filter(|symbol| {
                    symbol.path.last() == query.last()
                        || symbol
                            .aliases
                            .iter()
                            .any(|alias| alias.last() == query.last())
                })
                .collect()
        } else {
            exact
        };

        match matches.as_slice() {
            [symbol] => Ok(symbol),
            [] => Err(ResolveError::NotFound {
                file: file.clone(),
                query: query.clone(),
                nearest: self.nearest(query),
            }),
            several => Err(ResolveError::Ambiguous {
                file: file.clone(),
                query: query.clone(),
                candidates: several.iter().map(|symbol| symbol.path.clone()).collect(),
            }),
        }
    }

    /// Up to three paths (full paths or aliases) closest to `query` by edit distance, closest
    /// first. Ties keep file order.
    #[must_use]
    pub fn nearest(&self, query: &SymbolPath) -> Vec<SymbolPath> {
        let wanted = query.to_string();
        let mut seen = HashSet::new();
        let mut scored: Vec<(usize, &SymbolPath)> = self
            .iter()
            .flat_map(|symbol| std::iter::once(&symbol.path).chain(&symbol.aliases))
            .filter(|path| seen.insert(*path))
            .map(|path| (strsim::levenshtein(&wanted, &path.to_string()), path))
            .collect();
        scored.sort_by_key(|(distance, _)| *distance);
        scored
            .into_iter()
            .take(NEAREST)
            .map(|(_, path)| path.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn absolute(raw: &str) -> AbsolutePath {
        AbsolutePath::new(Path::new(raw)).expect("the fixture path should be absolute")
    }

    fn path(raw: &str) -> SymbolPath {
        raw.parse().expect("the fixture symbol should be valid")
    }

    fn table(paths: &[&str]) -> SymbolTable {
        SymbolTable::new(
            paths
                .iter()
                .zip(0..)
                .map(|(raw, start)| Symbol {
                    path: path(raw),
                    aliases: Vec::new(),
                    range: start..start + 1,
                })
                .collect(),
        )
    }

    fn resolved(table: &SymbolTable, query: &str) -> Result<String, ResolveError> {
        table
            .resolve(&absolute("/repo/src/lib.rs"), &path(query))
            .map(|symbol| symbol.path.to_string())
    }

    #[test]
    fn bare_name_resolves_when_unique() {
        let table = table(&["Config", "Config.validate", "add"]);

        assert_eq!(
            resolved(&table, "validate"),
            Ok("Config.validate".to_owned())
        );
    }

    #[test]
    fn bare_name_is_ambiguous_listing_full_paths() {
        let table = table(&["Foo.new", "Bar.new", "add"]);

        assert_eq!(
            resolved(&table, "new"),
            Err(ResolveError::Ambiguous {
                file: absolute("/repo/src/lib.rs"),
                query: path("new"),
                candidates: vec![path("Foo.new"), path("Bar.new")],
            }),
        );
    }

    #[test]
    fn bare_name_equal_to_a_top_level_path_resolves_to_it() {
        let table = table(&["add", "Calc.add"]);

        assert_eq!(
            resolved(&table, "add"),
            Ok("add".to_owned()),
            "the tree tells a top-level `add` from `Calc.add`, so `#add` names the top-level one",
        );
    }

    #[test]
    fn several_segments_match_only_the_full_path() {
        let table = table(&["Config.Display.fmt"]);

        assert!(
            matches!(
                resolved(&table, "Display.fmt"),
                Err(ResolveError::NotFound { .. })
            ),
            "a multi-segment path is never a suffix match",
        );
    }

    #[test]
    fn identical_full_paths_are_ambiguous() {
        let table = table(&["Foo.new", "Foo.new"]);

        assert!(
            matches!(
                resolved(&table, "Foo.new"),
                Err(ResolveError::Ambiguous { ref candidates, .. }) if candidates.len() == 2
            ),
            "two symbols with one path must not be silently picked between",
        );
    }

    #[test]
    fn alias_resolves_to_its_symbol() {
        let table = SymbolTable::new(vec![Symbol {
            path: path("news.whats-new"),
            aliases: vec![path("whats-new-1")],
            range: 0..1,
        }]);

        assert_eq!(
            resolved(&table, "whats-new-1"),
            Ok("news.whats-new".to_owned())
        );
    }

    #[test]
    fn not_found_suggests_nearest_three() {
        let table = table(&[
            "Config",
            "Config.port",
            "Config.validate",
            "Config.valid",
            "add",
        ]);

        assert_eq!(
            resolved(&table, "Config.validat"),
            Err(ResolveError::NotFound {
                file: absolute("/repo/src/lib.rs"),
                query: path("Config.validat"),
                nearest: vec![
                    path("Config.validate"),
                    path("Config.valid"),
                    path("Config.port")
                ],
            }),
        );
    }

    #[test]
    fn nearest_is_empty_for_an_empty_table() {
        assert_eq!(SymbolTable::default().nearest(&path("add")), Vec::new());
    }
}
