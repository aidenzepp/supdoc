//! The key-path symbol model the data cartridges (JSON, YAML, TOML) share: how a key becomes one
//! path segment, and a builder that turns a file's written structure into a [`SymbolTable`].
//!
//! A symbol is a key or array element of the file as written. Its path is the keys from the
//! document root, outermost first, joined by `.`. A key is escaped so it is always one segment:
//! `~` is written `~0` and `.` is written `~1`, RFC 6901's escape with `.` in place of `/`. An
//! array element is named by its 0-based index. An empty key, or a key that is not a scalar, is
//! not a symbol, and nothing under it is. Two keys with the same path are both kept, so a link to
//! that path is ambiguous rather than silently resolved to one of them.
//!
//! The builder bounds hostile input three ways (SEC-1). The walk keeps its own stack, so nesting
//! depth never reaches the call stack. Every symbol checks the read's [`Deadline`]. Every symbol's
//! segments are charged to a budget before its path is allocated, so a deep or wide file fails
//! closed instead of exhausting memory. A build that fails returns no table.

use std::collections::HashMap;
use std::ops::Range;

use crate::link::{SymbolPath, SymbolSegment};
use crate::read::cartridge::{Deadline, Stopped, Symbol, SymbolTable};

/// The path segment naming `key`: `~` written `~0`, then `.` written `~1`.
///
/// `None` for the empty key, which names nothing.
#[must_use]
pub(crate) fn key_segment(key: &str) -> Option<SymbolSegment> {
    // Escaping `~` first keeps the escape a bijection: `~1` as written becomes `~01`, never `.`.
    SymbolSegment::new(key.replace('~', "~0").replace('.', "~1")).ok()
}

/// The path segment naming the array element at 0-based `index`.
///
/// A decimal number is never empty and holds no `.`, so this is never `None`.
#[must_use]
fn index_segment(index: usize) -> Option<SymbolSegment> {
    SymbolSegment::new(index.to_string()).ok()
}

/// A symbol a [`KeyPaths`] builder made, named as the parent of the symbols under it.
///
/// Meaningful only to the builder that returned it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct KeyId(usize);

/// Why building a data file's symbol table stopped. No partial table survives either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyPathError {
    /// The read's deadline passed at the symbol starting at the recorded byte.
    Timeout(Stopped),
    /// The table would hold more than `limit` path segments in total.
    TooManySegments {
        /// The budget the build was given.
        limit: usize,
    },
}

/// What one node of a data file is, as [`KeyPaths::walk`] sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Shape<N> {
    /// A mapping: its members in file order.
    Mapping(Vec<Member<N>>),
    /// A sequence: its elements in file order, each named by its index.
    Sequence(Vec<Element<N>>),
    /// A scalar, an alias, or anything else nothing is named under.
    Leaf,
}

/// One key of a mapping and its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Member<N> {
    /// The key's text, decoded from its quoting; `None` when the key is not a scalar.
    pub(crate) key: Option<String>,
    /// The symbol's byte range in the file.
    pub(crate) span: Range<usize>,
    /// The value to walk into; `None` when the key has no value.
    pub(crate) value: Option<N>,
}

/// One element of a sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Element<N> {
    /// The symbol's byte range in the file.
    pub(crate) span: Range<usize>,
    /// The value to walk into; `None` when the element is empty.
    pub(crate) value: Option<N>,
}

/// A symbol named but not yet made, waiting on the walk's stack.
struct Pending<N> {
    /// The symbol it goes under, or `None` at the root.
    parent: Option<KeyId>,
    /// Its own escaped name.
    segment: SymbolSegment,
    /// Its byte range.
    span: Range<usize>,
    /// What to walk into once it is made.
    value: Option<N>,
}

/// Builds one data file's symbol table under a segment budget and the read's deadline.
#[derive(Debug)]
pub(crate) struct KeyPaths {
    /// The symbols made so far, in the order they were made.
    symbols: Vec<Symbol>,
    /// Tables made by [`KeyPaths::table`], by parent and name, so a later piece widens the
    /// first instead of making a twin.
    tables: HashMap<(Option<KeyId>, SymbolSegment), KeyId>,
    /// Path segments held by `symbols`.
    spent: usize,
    /// The most path segments `symbols` may hold.
    max_segments: usize,
    /// When the read of the file must stop.
    deadline: Deadline,
    /// The first error, which every later call returns, so a caller can't build past it.
    failed: Option<KeyPathError>,
}

impl KeyPaths {
    /// The table `fill` builds, holding at most `max_segments` path segments in total.
    ///
    /// # Errors
    ///
    /// The first [`KeyPathError`] any call inside `fill` met, even when `fill` dropped it: a
    /// failed build never returns a partial table.
    pub(crate) fn build(
        max_segments: usize,
        deadline: Deadline,
        fill: impl FnOnce(&mut Self) -> Result<(), KeyPathError>,
    ) -> Result<SymbolTable, KeyPathError> {
        let mut paths = Self {
            symbols: Vec::new(),
            tables: HashMap::new(),
            spent: 0,
            max_segments,
            deadline,
            failed: None,
        };
        let filled = fill(&mut paths);
        match paths.failed {
            Some(error) => Err(error),
            None => filled.map(|()| SymbolTable::new(paths.symbols)),
        }
    }

    /// A new symbol named `segment` under `parent` (`None` for the root), covering `span`.
    ///
    /// Always a new symbol: a duplicate key becomes a twin with the same path, which resolves as
    /// ambiguous.
    ///
    /// # Errors
    ///
    /// [`KeyPathError`] when the deadline has passed, the budget would be exceeded, or an earlier
    /// call already failed.
    pub(crate) fn key(
        &mut self,
        parent: Option<KeyId>,
        segment: SymbolSegment,
        span: Range<usize>,
    ) -> Result<KeyId, KeyPathError> {
        self.ensure_live(span.start)?;
        let path = self.charge(parent, segment)?;
        let id = KeyId(self.symbols.len());
        self.symbols.push(Symbol::new(path, Vec::new(), span));
        Ok(id)
    }

    /// The table named `segment` under `parent`, widened to cover `span`, for a table written in
    /// several pieces (TOML's `[a]` and `[a.b]`): the first piece makes it, every later piece
    /// widens it, so its span runs from its first piece through its last.
    ///
    /// # Errors
    ///
    /// As [`KeyPaths::key`]. Widening a table charges nothing.
    pub(crate) fn table(
        &mut self,
        parent: Option<KeyId>,
        segment: SymbolSegment,
        span: Range<usize>,
    ) -> Result<KeyId, KeyPathError> {
        self.ensure_live(span.start)?;
        let name = (parent, segment);
        if let Some(&id) = self.tables.get(&name) {
            if let Some(table) = self.symbols.get_mut(id.0) {
                table.cover(&span);
            }
            return Ok(id);
        }
        let id = self.key(name.0, name.1.clone(), span)?;
        let _new = self.tables.insert(name, id);
        Ok(id)
    }

    /// Makes a symbol for every member and element under `root`, depth first in file order, each
    /// under `parent`'s path. `shape` says what a node is. `root` itself names nothing.
    ///
    /// The walk keeps its own stack, so any nesting depth is safe. A member whose key is empty
    /// or not a scalar is skipped with everything under it.
    ///
    /// # Errors
    ///
    /// As [`KeyPaths::key`], at the first symbol that fails.
    pub(crate) fn walk<N>(
        &mut self,
        parent: Option<KeyId>,
        root: &N,
        mut shape: impl FnMut(&N) -> Shape<N>,
    ) -> Result<(), KeyPathError> {
        let mut pending = Vec::new();
        push_children(&mut pending, parent, shape(root));
        while let Some(next) = pending.pop() {
            let id = self.key(next.parent, next.segment, next.span)?;
            if let Some(value) = next.value {
                push_children(&mut pending, Some(id), shape(&value));
            }
        }
        Ok(())
    }

    /// Checks the build may go on, at the symbol starting at byte `offset`.
    ///
    /// # Errors
    ///
    /// The earlier error if a call already failed, else [`KeyPathError::Timeout`] at `offset`
    /// once the deadline has passed.
    fn ensure_live(&mut self, offset: usize) -> Result<(), KeyPathError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.deadline.passed() {
            return Err(self.fail(KeyPathError::Timeout(Stopped { offset })));
        }
        Ok(())
    }

    /// `parent`'s path extended by `segment`, after charging its segments to the budget.
    ///
    /// # Errors
    ///
    /// [`KeyPathError::TooManySegments`], before the path is allocated, when the budget would be
    /// exceeded.
    fn charge(
        &mut self,
        parent: Option<KeyId>,
        segment: SymbolSegment,
    ) -> Result<SymbolPath, KeyPathError> {
        let parent = parent
            .and_then(|id| self.symbols.get(id.0))
            .map(Symbol::path);
        let cost = parent.map_or(1, |path| path.segments().len().saturating_add(1));
        let spent = self.spent.saturating_add(cost);
        if spent > self.max_segments {
            let limit = self.max_segments;
            return Err(self.fail(KeyPathError::TooManySegments { limit }));
        }
        let path = match parent {
            Some(path) => path.child(segment),
            None => SymbolPath::root(segment),
        };
        self.spent = spent;
        Ok(path)
    }

    /// Records `error` as the build's outcome and returns it.
    fn fail(&mut self, error: KeyPathError) -> KeyPathError {
        self.failed = Some(error);
        error
    }
}

/// Puts the named members or elements of `shape` on `pending`, under `parent`, so they pop in
/// file order. Members whose key names nothing are dropped with their values.
fn push_children<N>(pending: &mut Vec<Pending<N>>, parent: Option<KeyId>, shape: Shape<N>) {
    match shape {
        Shape::Mapping(members) => pending.extend(members.into_iter().rev().filter_map(|member| {
            Some(Pending {
                parent,
                segment: key_segment(member.key.as_deref()?)?,
                span: member.span,
                value: member.value,
            })
        })),
        Shape::Sequence(elements) => {
            pending.extend(elements.into_iter().enumerate().rev().filter_map(
                |(index, element)| {
                    Some(Pending {
                        parent,
                        segment: index_segment(index)?,
                        span: element.span,
                        value: element.value,
                    })
                },
            ));
        }
        Shape::Leaf => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{Element, KeyPathError, KeyPaths, Member, Shape, key_segment};
    use crate::limits::Limits;
    use crate::link::SymbolPath;
    use crate::read::cartridge::{Deadline, SymbolTable};

    /// The default `[limits] max-key-segments` the design names.
    const BUDGET: usize = 1_000_000;

    /// A data document in memory: what a cartridge would read from its tree.
    #[derive(Debug, Clone)]
    enum Doc {
        /// Keys (`None` for a non-scalar key) and their values.
        Map(Vec<(Option<&'static str>, Doc)>),
        /// Elements.
        Seq(Vec<Doc>),
        /// A scalar.
        Scalar,
    }

    /// `doc`'s shape, every span `0..1`: the walk under test never reads spans.
    fn shape(doc: &Doc) -> Shape<Doc> {
        match doc {
            Doc::Map(members) => Shape::Mapping(
                members
                    .iter()
                    .map(|(key, value)| Member {
                        key: key.map(str::to_owned),
                        span: 0..1,
                        value: Some(value.clone()),
                    })
                    .collect(),
            ),
            Doc::Seq(elements) => Shape::Sequence(
                elements
                    .iter()
                    .map(|value| Element {
                        span: 0..1,
                        value: Some(value.clone()),
                    })
                    .collect(),
            ),
            Doc::Scalar => Shape::Leaf,
        }
    }

    /// A deadline that will not pass during a test.
    fn deadline() -> Deadline {
        Deadline::start(Limits::PARSE_TIMEOUT)
    }

    /// The table `doc` builds under `budget`.
    fn walk(doc: &Doc, budget: usize) -> Result<SymbolTable, KeyPathError> {
        KeyPaths::build(budget, deadline(), |paths| paths.walk(None, doc, shape))
    }

    /// Every symbol's path, in table order.
    fn paths(table: &SymbolTable) -> Vec<String> {
        table
            .iter()
            .map(|symbol| symbol.path().to_string())
            .collect()
    }

    /// Reverses [`key_segment`], to prove the escape loses nothing.
    fn unescape(segment: &str) -> Option<String> {
        let mut key = String::new();
        let mut chars = segment.chars();
        while let Some(c) = chars.next() {
            if c != '~' {
                key.push(c);
                continue;
            }
            match chars.next()? {
                '0' => key.push('~'),
                '1' => key.push('.'),
                _ => return None,
            }
        }
        Some(key)
    }

    /// A chain of one-key mappings `depth` deep, as a node holding its own depth.
    fn chain(depth: usize) -> impl FnMut(&usize) -> Shape<usize> {
        move |&level| {
            if level < depth {
                Shape::Mapping(vec![Member {
                    key: Some("a".to_owned()),
                    span: level..level + 1,
                    value: Some(level + 1),
                }])
            } else {
                Shape::Leaf
            }
        }
    }

    /// Runs `body` on a thread with a 128 KiB stack, far too small for a recursive walk of
    /// thousands of levels.
    fn on_small_stack<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(body)
            .expect("the test thread should start")
            .join()
            .expect("the walk should not overflow its stack")
    }

    #[test]
    fn a_dot_or_tilde_in_a_key_is_escaped_and_round_trips() {
        for (key, escaped) in [
            ("version", "version"),
            ("./utils", "~1/utils"),
            ("app.kubernetes.io/name", "app~1kubernetes~1io/name"),
            ("~", "~0"),
            ("~1", "~01"),
            ("~0.~1", "~00~1~01"),
            (":~:", ":~0:"),
            ("1.5", "1~15"),
        ] {
            let segment = key_segment(key).expect("a non-empty key should name a segment");
            let text = SymbolPath::root(segment).to_string();

            assert_eq!(text, escaped, "the escaped segment for key {key:?}");
            assert_eq!(
                unescape(&text).as_deref(),
                Some(key),
                "unescaping {text:?} should give back key {key:?}",
            );
        }
    }

    #[test]
    fn every_escaped_key_parses_back_as_exactly_one_segment() {
        for key in ["a.b", "...", "~", "~1", ":~:", "a~0.b~1", "x/y#z [w]"] {
            let text =
                SymbolPath::root(key_segment(key).expect("key should name a segment")).to_string();

            let parsed: SymbolPath = text
                .parse()
                .unwrap_or_else(|error| panic!("{text:?} should parse as a path: {error}"));

            assert_eq!(
                parsed.segments(),
                std::slice::from_ref(&text),
                "key {key:?} as {text:?}"
            );
            assert!(
                !text.contains(":~:"),
                "{text:?} must not hold a text fragment marker"
            );
        }
    }

    #[test]
    fn the_empty_key_names_nothing() {
        assert_eq!(key_segment(""), None);
    }

    #[test]
    fn keys_join_with_dots_and_elements_are_named_by_index_in_file_order() {
        let doc = Doc::Map(vec![
            (
                Some("package"),
                Doc::Map(vec![(Some("version"), Doc::Scalar)]),
            ),
            (
                Some("bin"),
                Doc::Seq(vec![
                    Doc::Map(vec![(Some("name"), Doc::Scalar)]),
                    Doc::Scalar,
                ]),
            ),
            (Some("a.b"), Doc::Scalar),
        ]);

        let table = walk(&doc, BUDGET).expect("a small document should build");

        assert_eq!(
            paths(&table),
            [
                "package",
                "package.version",
                "bin",
                "bin.0",
                "bin.0.name",
                "bin.1",
                "a~1b",
            ],
        );
    }

    #[test]
    fn empty_and_non_scalar_keys_are_skipped_with_everything_under_them() {
        let doc = Doc::Map(vec![
            (Some(""), Doc::Map(vec![(Some("under_empty"), Doc::Scalar)])),
            (None, Doc::Map(vec![(Some("under_complex"), Doc::Scalar)])),
            (Some("kept"), Doc::Scalar),
        ]);

        let table = walk(&doc, BUDGET).expect("a small document should build");

        assert_eq!(paths(&table), ["kept"]);
    }

    #[test]
    fn duplicate_keys_both_stay_as_twins() {
        let doc = Doc::Map(vec![(Some("a"), Doc::Scalar), (Some("a"), Doc::Scalar)]);

        let table = walk(&doc, BUDGET).expect("a small document should build");

        assert_eq!(
            paths(&table),
            ["a", "a"],
            "both twins, so a link to `a` is ambiguous"
        );
    }

    #[test]
    fn a_table_in_several_pieces_is_one_symbol_covering_first_through_last() {
        let segment = |key| key_segment(key).expect("key should name a segment");

        // `[a]` at 0..10, `[b]` at 10..20, `[a.c]` at 20..30.
        let table = KeyPaths::build(BUDGET, deadline(), |paths| {
            let a = paths.table(None, segment("a"), 0..10)?;
            let _b = paths.table(None, segment("b"), 10..20)?;
            let again = paths.table(None, segment("a"), 20..30)?;
            assert_eq!(a, again, "the second piece of `a` should widen the first");
            let _c = paths.table(Some(a), segment("c"), 20..30)?;
            Ok(())
        })
        .expect("three tables should build");

        let spans: Vec<(String, std::ops::Range<usize>)> = table
            .iter()
            .map(|symbol| (symbol.path().to_string(), symbol.range()))
            .collect();
        assert_eq!(
            spans,
            [
                ("a".to_owned(), 0..30),
                ("b".to_owned(), 10..20),
                ("a.c".to_owned(), 20..30),
            ],
        );
    }

    #[test]
    fn exceeding_the_budget_fails_with_no_table() {
        // Ten keys at depth 2 hold 1 + 10 × 2 = 21 segments.
        let doc = Doc::Map(vec![(
            Some("top"),
            Doc::Map(vec![(Some("k"), Doc::Scalar); 10]),
        )]);

        assert!(
            walk(&doc, 21).is_ok(),
            "21 segments should fit a budget of 21"
        );
        assert_eq!(
            walk(&doc, 20),
            Err(KeyPathError::TooManySegments { limit: 20 }),
        );
    }

    #[test]
    fn a_dropped_error_still_fails_the_build() {
        let segment = key_segment("k").expect("key should name a segment");

        let built = KeyPaths::build(1, deadline(), |paths| {
            let _first = paths.key(None, segment.clone(), 0..1)?;
            let _dropped = paths.key(None, segment.clone(), 1..2);
            let _after = paths.key(None, segment.clone(), 2..3);
            Ok(())
        });

        assert_eq!(built, Err(KeyPathError::TooManySegments { limit: 1 }));
    }

    #[test]
    fn a_passed_deadline_stops_the_walk_with_no_table() {
        let deadline = Deadline::start("1ms".parse().expect("1ms should be a time limit"));
        while !deadline.passed() {
            std::hint::spin_loop();
        }

        let built = KeyPaths::build(BUDGET, deadline, |paths| paths.walk(None, &0, chain(3)));

        assert!(
            matches!(built, Err(KeyPathError::Timeout(stopped)) if stopped.offset == 0),
            "{built:?}",
        );
    }

    #[test]
    fn nesting_one_hundred_thousand_deep_fails_closed_without_recursing() {
        let built = on_small_stack(|| {
            KeyPaths::build(BUDGET, deadline(), |paths| {
                paths.walk(None, &0, chain(100_000))
            })
        });

        assert_eq!(built, Err(KeyPathError::TooManySegments { limit: BUDGET }));
    }

    #[test]
    fn nesting_as_deep_as_the_budget_allows_walks_without_recursing() {
        // 1,413 levels hold 1,413 × 1,414 / 2 = 999,091 segments.
        let table = on_small_stack(|| {
            KeyPaths::build(BUDGET, deadline(), |paths| {
                paths.walk(None, &0, chain(1_413))
            })
        })
        .expect("a chain within the budget should build");

        let deepest = table.iter().last().expect("the chain should name symbols");
        assert_eq!(deepest.path().segments().len(), 1_413);
    }
}
