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
//! path is charged to two budgets, its segments and its bytes of text, before the symbol is queued
//! and before its path is allocated, so the paths a deep, wide or long-keyed file asks for fail
//! closed instead of exhausting memory. What is left unbudgeted is linear in the file: the members
//! a cartridge hands over for one node and their escaped names. A build that fails returns no
//! table.

use std::collections::HashMap;
use std::ops::Range;

use crate::limits::Limits;
use crate::link::{SymbolPath, SymbolSegment};
use crate::read::cartridge::{Deadline, SourceFile, Stopped, Symbol, SymbolTable};
use crate::read::error::ReadError;

/// The default ceiling on the bytes of path text one data file's symbol table may hold: 32 MiB.
#[cfg(test)]
pub(crate) const DEFAULT_KEY_PATH_BYTES: usize = 32 * 1024 * 1024;

/// The ceilings one data file's symbol table is built under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyPathBudget {
    /// The most path segments all symbols together may hold.
    pub(crate) segments: usize,
    /// The most bytes of path text, as displayed with its `.` separators, all symbols together
    /// may hold.
    pub(crate) bytes: usize,
}

impl KeyPathBudget {
    /// The ceilings `limits` sets; a ceiling of `0` (no limit) is as large as a count can be.
    #[must_use]
    pub(crate) fn from_limits(limits: &Limits) -> Self {
        Self {
            segments: count(limits.max_key_segments.value.get()),
            bytes: count(limits.max_key_path_size.value.get()),
        }
    }

    /// The ceilings when nothing sets them: `max-key-segments` and `max-key-path-size` at their
    /// defaults.
    #[must_use]
    pub(crate) fn defaults() -> Self {
        Self {
            segments: count(Limits::MAX_KEY_SEGMENTS.get()),
            bytes: count(Limits::MAX_KEY_PATH_SIZE.get()),
        }
    }
}

/// `ceiling` as a count; no ceiling, or one past what a count can hold, is the largest count.
fn count(ceiling: Option<u64>) -> usize {
    ceiling.map_or(usize::MAX, |value| {
        usize::try_from(value).unwrap_or(usize::MAX)
    })
}

/// One escaped path segment and its length in bytes, so charging it costs O(1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct KeyName {
    /// The segment.
    segment: SymbolSegment,
    /// Its length in bytes.
    len: usize,
}

impl KeyName {
    /// Wraps escaped `text`. `None` when it is empty.
    fn new(text: String) -> Option<Self> {
        let len = text.len();
        SymbolSegment::new(text)
            .ok()
            .map(|segment| Self { segment, len })
    }
}

/// The path segment naming `key`: `~` written `~0`, then `.` written `~1`.
///
/// `None` for the empty key, which names nothing.
#[must_use]
pub(crate) fn key_name(key: &str) -> Option<KeyName> {
    // Escaping `~` first keeps the escape a bijection: `~1` as written becomes `~01`, never `.`.
    KeyName::new(key.replace('~', "~0").replace('.', "~1"))
}

/// The path segment naming the array element at 0-based `index`.
///
/// A decimal number is never empty and holds no `.`, so this is never `None`.
#[must_use]
pub(crate) fn index_name(index: usize) -> Option<KeyName> {
    KeyName::new(index.to_string())
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
        /// The segment budget the build was given.
        limit: usize,
    },
    /// The table would hold more than `limit` bytes of path text in total.
    TooManyBytes {
        /// The byte budget the build was given.
        limit: usize,
    },
}

impl KeyPathError {
    /// This error as the [`ReadError`] for reading `file`: a timeout names the symbol it stopped
    /// at, a budget names the ceiling and so how to raise it.
    #[must_use]
    pub(crate) fn read_error(self, file: &SourceFile) -> ReadError {
        let path = file.path().clone();
        match self {
            Self::Timeout(stopped) => file.timed_out("the key walk", stopped),
            Self::TooManySegments { limit } => ReadError::TooManyKeys {
                path,
                limit: u64::try_from(limit).unwrap_or(u64::MAX),
            },
            Self::TooManyBytes { limit } => ReadError::KeyPathsTooLarge {
                path,
                limit: u64::try_from(limit).unwrap_or(u64::MAX),
            },
        }
    }
}

/// What one node of a data file is, as [`KeyPaths::walk`] sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the YAML and JSON cartridges walk with it from P5-L10"
    )
)]
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
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the YAML and JSON cartridges walk with it from P5-L10"
    )
)]
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
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the YAML and JSON cartridges walk with it from P5-L10"
    )
)]
pub(crate) struct Element<N> {
    /// The symbol's byte range in the file.
    pub(crate) span: Range<usize>,
    /// The value to walk into; `None` when the element is empty.
    pub(crate) value: Option<N>,
}

/// A symbol named and charged but not yet made, waiting on the walk's stack.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the YAML and JSON cartridges walk with it from P5-L10"
    )
)]
struct Pending<N> {
    /// The symbol it goes under, or `None` at the root.
    parent: Option<KeyId>,
    /// Its own escaped name.
    name: KeyName,
    /// The bytes of its whole path's text, already charged.
    bytes: usize,
    /// Its byte range.
    span: Range<usize>,
    /// What to walk into once it is made.
    value: Option<N>,
}

/// Builds one data file's symbol table under a [`KeyPathBudget`] and the read's deadline.
#[derive(Debug)]
pub(crate) struct KeyPaths {
    /// The symbols made so far, in the order they were made.
    symbols: Vec<Symbol>,
    /// The bytes of each symbol's path text, by [`KeyId`], so a child's charge is O(1).
    path_bytes: Vec<usize>,
    /// Tables made by [`KeyPaths::table`], by parent and name, so a later piece widens the
    /// first instead of making a twin.
    tables: HashMap<(Option<KeyId>, KeyName), KeyId>,
    /// Path segments charged so far, made or queued.
    spent_segments: usize,
    /// Bytes of path text charged so far, made or queued.
    spent_bytes: usize,
    /// The ceilings on both.
    budget: KeyPathBudget,
    /// When the read of the file must stop.
    deadline: Deadline,
    /// The first error, which every later call returns, so a caller can't build past it.
    failed: Option<KeyPathError>,
}

impl KeyPaths {
    /// The table `fill` builds within `budget`.
    ///
    /// # Errors
    ///
    /// The first [`KeyPathError`] any call inside `fill` met, even when `fill` dropped it: a
    /// failed build never returns a partial table.
    pub(crate) fn build(
        budget: KeyPathBudget,
        deadline: Deadline,
        fill: impl FnOnce(&mut Self) -> Result<(), KeyPathError>,
    ) -> Result<SymbolTable, KeyPathError> {
        let mut paths = Self {
            symbols: Vec::new(),
            path_bytes: Vec::new(),
            tables: HashMap::new(),
            spent_segments: 0,
            spent_bytes: 0,
            budget,
            deadline,
            failed: None,
        };
        let filled = fill(&mut paths);
        match paths.failed {
            Some(error) => Err(error),
            None => filled.map(|()| SymbolTable::new(paths.symbols)),
        }
    }

    /// A new symbol named `name` under `parent` (`None` for the root), covering `span`.
    ///
    /// Always a new symbol: a duplicate key becomes a twin with the same path, which resolves as
    /// ambiguous.
    ///
    /// # Errors
    ///
    /// [`KeyPathError`] when the deadline has passed, either budget would be exceeded, or an
    /// earlier call already failed.
    pub(crate) fn key(
        &mut self,
        parent: Option<KeyId>,
        name: KeyName,
        span: Range<usize>,
    ) -> Result<KeyId, KeyPathError> {
        self.ensure_live(span.start)?;
        let bytes = self.charge(parent, &name)?;
        Ok(self.make(parent, name, bytes, span))
    }

    /// The table named `name` under `parent`, widened to cover `span`, for a table written in
    /// several pieces (TOML's `[a]` and `[a.b]`): the first piece makes it, every later piece
    /// widens it, so its span runs from its first piece through its last.
    ///
    /// # Errors
    ///
    /// As [`KeyPaths::key`]. Widening a table charges nothing.
    pub(crate) fn table(
        &mut self,
        parent: Option<KeyId>,
        name: KeyName,
        span: Range<usize>,
    ) -> Result<KeyId, KeyPathError> {
        self.ensure_live(span.start)?;
        let entry = (parent, name);
        if let Some(&id) = self.tables.get(&entry) {
            if let Some(table) = self.symbols.get_mut(id.0) {
                table.cover(&span);
            }
            return Ok(id);
        }
        let id = self.key(entry.0, entry.1.clone(), span)?;
        let _new = self.tables.insert(entry, id);
        Ok(id)
    }

    /// Makes a symbol for every member and element under `root`, depth first in file order, each
    /// under `parent`'s path. `shape` says what a node is. `root` itself names nothing.
    ///
    /// The walk keeps its own stack, so any nesting depth is safe. Each node's members are
    /// charged in file order before any is queued, so a mapping too wide for the budget stops at
    /// the first member over it. A member whose key is empty or not a scalar is skipped with
    /// everything under it.
    ///
    /// # Errors
    ///
    /// As [`KeyPaths::key`], at the first symbol that fails.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the YAML and JSON cartridges walk with it from P5-L10"
        )
    )]
    pub(crate) fn walk<N>(
        &mut self,
        parent: Option<KeyId>,
        root: &N,
        mut shape: impl FnMut(&N) -> Shape<N>,
    ) -> Result<(), KeyPathError> {
        let mut pending = Vec::new();
        self.queue(&mut pending, parent, shape(root))?;
        while let Some(next) = pending.pop() {
            self.ensure_live(next.span.start)?;
            let id = self.make(next.parent, next.name, next.bytes, next.span);
            if let Some(value) = next.value {
                self.queue(&mut pending, Some(id), shape(&value))?;
            }
        }
        Ok(())
    }

    /// Charges the named members or elements of `shape` under `parent` in file order, then puts
    /// them on `pending` so they pop in file order. Members whose key names nothing are dropped
    /// with their values.
    ///
    /// # Errors
    ///
    /// As [`KeyPaths::key`], at the first member that fails; nothing of `shape` is queued then.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the YAML and JSON cartridges walk with it from P5-L10"
        )
    )]
    fn queue<N>(
        &mut self,
        pending: &mut Vec<Pending<N>>,
        parent: Option<KeyId>,
        shape: Shape<N>,
    ) -> Result<(), KeyPathError> {
        let mut charged = Vec::new();
        match shape {
            Shape::Mapping(members) => {
                for member in members {
                    let Some(name) = member.key.as_deref().and_then(key_name) else {
                        continue;
                    };
                    charged.push(self.pending(parent, name, member.span, member.value)?);
                }
            }
            Shape::Sequence(elements) => {
                for (index, element) in elements.into_iter().enumerate() {
                    let Some(name) = index_name(index) else {
                        continue;
                    };
                    charged.push(self.pending(parent, name, element.span, element.value)?);
                }
            }
            Shape::Leaf => {}
        }
        pending.extend(charged.into_iter().rev());
        Ok(())
    }

    /// A symbol named `name` under `parent`, charged to the budget but not yet made.
    ///
    /// # Errors
    ///
    /// As [`KeyPaths::key`].
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the YAML and JSON cartridges walk with it from P5-L10"
        )
    )]
    fn pending<N>(
        &mut self,
        parent: Option<KeyId>,
        name: KeyName,
        span: Range<usize>,
        value: Option<N>,
    ) -> Result<Pending<N>, KeyPathError> {
        self.ensure_live(span.start)?;
        let bytes = self.charge(parent, &name)?;
        #[cfg(test)]
        tests::CHARGED.set(tests::CHARGED.get() + 1);
        Ok(Pending {
            parent,
            name,
            bytes,
            span,
            value,
        })
    }

    /// Checks the build may go on at byte `offset`, and that a path of `segments` segments could
    /// ever be made, so a cartridge can refuse a node before it allocates anything for its
    /// parts. A path is charged its full length when its last segment is made, so one longer
    /// than the whole segment budget never fits. Charges nothing.
    ///
    /// # Errors
    ///
    /// As [`KeyPaths::ensure_live`], then [`KeyPathError::TooManySegments`] when `segments` is
    /// over the segment budget.
    pub(crate) fn reaches(&mut self, offset: usize, segments: usize) -> Result<(), KeyPathError> {
        self.ensure_live(offset)?;
        if segments > self.budget.segments {
            let limit = self.budget.segments;
            return Err(self.fail(KeyPathError::TooManySegments { limit }));
        }
        Ok(())
    }

    /// Checks the build may go on, at the node starting at byte `offset`, so a loop over nodes
    /// that name nothing still stops at the deadline.
    ///
    /// # Errors
    ///
    /// The earlier error if a call already failed, else [`KeyPathError::Timeout`] at `offset`
    /// once the deadline has passed.
    pub(crate) fn ensure_live(&mut self, offset: usize) -> Result<(), KeyPathError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.deadline.passed() {
            return Err(self.fail(KeyPathError::Timeout(Stopped { offset })));
        }
        Ok(())
    }

    /// Charges the path of `name` under `parent` to both budgets, in O(1) and before anything is
    /// allocated for it, and returns the bytes of its text.
    ///
    /// # Errors
    ///
    /// [`KeyPathError::TooManySegments`] or [`KeyPathError::TooManyBytes`] when that budget would
    /// be exceeded; nothing is charged then.
    fn charge(&mut self, parent: Option<KeyId>, name: &KeyName) -> Result<usize, KeyPathError> {
        let parent = parent.and_then(|id| {
            Some((
                self.symbols.get(id.0)?.path().segments().len(),
                *self.path_bytes.get(id.0)?,
            ))
        });
        let (segments, bytes) = match parent {
            // The parent's segments and text, a `.`, and this segment.
            Some((segments, bytes)) => (
                segments.saturating_add(1),
                bytes.saturating_add(1).saturating_add(name.len),
            ),
            None => (1, name.len),
        };
        let spent_segments = self.spent_segments.saturating_add(segments);
        if spent_segments > self.budget.segments {
            let limit = self.budget.segments;
            return Err(self.fail(KeyPathError::TooManySegments { limit }));
        }
        let spent_bytes = self.spent_bytes.saturating_add(bytes);
        if spent_bytes > self.budget.bytes {
            let limit = self.budget.bytes;
            return Err(self.fail(KeyPathError::TooManyBytes { limit }));
        }
        self.spent_segments = spent_segments;
        self.spent_bytes = spent_bytes;
        Ok(bytes)
    }

    /// Makes the symbol named `name` under `parent`, whose `bytes` of path text
    /// [`KeyPaths::charge`] already charged.
    fn make(
        &mut self,
        parent: Option<KeyId>,
        name: KeyName,
        bytes: usize,
        span: Range<usize>,
    ) -> KeyId {
        let parent = parent
            .and_then(|id| self.symbols.get(id.0))
            .map(Symbol::path);
        let path = match parent {
            Some(path) => path.child(name.segment),
            None => SymbolPath::root(name.segment),
        };
        let id = KeyId(self.symbols.len());
        self.symbols.push(Symbol::new(path, Vec::new(), span));
        self.path_bytes.push(bytes);
        id
    }

    /// Records `error` as the build's outcome and returns it.
    fn fail(&mut self, error: KeyPathError) -> KeyPathError {
        self.failed = Some(error);
        error
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_KEY_PATH_BYTES, Element, KeyName, KeyPathBudget, KeyPathError, KeyPaths, Member,
        Shape, key_name,
    };
    use crate::limits::{CommandLineLimits, ConfigLimits, Given, Limits};
    use crate::link::SymbolPath;
    use crate::path::AbsolutePath;
    use crate::read::cartridge::{Deadline, Languages, SourceFile, Stopped, SymbolTable};
    use crate::read::error::{ReadError, ReadRole};

    thread_local! {
        /// Symbols the builder charged on this thread, made or queued.
        pub(super) static CHARGED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// A budget no fixture here reaches unless it means to: 1,000,000 segments, five times the
    /// 200,000 `max-key-segments` default, and the default 32 MiB of path text.
    const BUDGET: KeyPathBudget = KeyPathBudget {
        segments: 1_000_000,
        bytes: DEFAULT_KEY_PATH_BYTES,
    };

    /// The default budget with at most `segments` path segments.
    const fn segments(segments: usize) -> KeyPathBudget {
        KeyPathBudget {
            segments,
            bytes: DEFAULT_KEY_PATH_BYTES,
        }
    }

    /// The name `key` escapes to, for a key that names something.
    fn name(key: &str) -> KeyName {
        key_name(key).expect("a non-empty key should name a segment")
    }

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
    fn walk(doc: &Doc, budget: KeyPathBudget) -> Result<SymbolTable, KeyPathError> {
        KeyPaths::build(budget, deadline(), |paths| paths.walk(None, doc, shape))
    }

    /// Every symbol's path, in table order.
    fn paths(table: &SymbolTable) -> Vec<String> {
        table
            .iter()
            .map(|symbol| symbol.path().to_string())
            .collect()
    }

    /// Reverses [`key_name`], to prove the escape loses nothing.
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

    /// A chain of one-key mappings `depth` deep, every key `a`, as a node holding its own depth.
    fn chain(depth: usize) -> impl FnMut(&usize) -> Shape<usize> {
        chain_of(depth, "a".to_owned())
    }

    /// A chain of one-key mappings `depth` deep, every key `key`.
    fn chain_of(depth: usize, key: String) -> impl FnMut(&usize) -> Shape<usize> {
        move |&level| {
            if level < depth {
                Shape::Mapping(vec![Member {
                    key: Some(key.clone()),
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
            let text = SymbolPath::root(name(key).segment).to_string();

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
            let text = SymbolPath::root(name(key).segment).to_string();

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
        assert_eq!(key_name(""), None);
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
        // `[a]` at 0..10, `[b]` at 10..20, `[a.c]` at 20..30.
        let table = KeyPaths::build(BUDGET, deadline(), |paths| {
            let a = paths.table(None, name("a"), 0..10)?;
            let _b = paths.table(None, name("b"), 10..20)?;
            let again = paths.table(None, name("a"), 20..30)?;
            assert_eq!(a, again, "the second piece of `a` should widen the first");
            let _c = paths.table(Some(a), name("c"), 20..30)?;
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
            walk(&doc, segments(21)).is_ok(),
            "21 segments should fit a budget of 21"
        );
        assert_eq!(
            walk(&doc, segments(20)),
            Err(KeyPathError::TooManySegments { limit: 20 }),
        );
    }

    #[test]
    fn a_dropped_error_still_fails_the_build() {
        let built = KeyPaths::build(segments(1), deadline(), |paths| {
            let _first = paths.key(None, name("k"), 0..1)?;
            let _dropped = paths.key(None, name("k"), 1..2);
            let _after = paths.key(None, name("k"), 2..3);
            Ok(())
        });

        assert_eq!(built, Err(KeyPathError::TooManySegments { limit: 1 }));
    }

    #[test]
    fn every_call_after_the_first_error_returns_it_even_one_that_would_fit() {
        // `a` and `a.b` spend 3 of 4 segments; `a.b.c` would spend 3 more and fails, after which
        // a root key, costing the 1 segment still free, must fail with that first error.
        let mut after = None;

        let built = KeyPaths::build(segments(4), deadline(), |paths| {
            let a = paths.key(None, name("a"), 0..1)?;
            let b = paths.key(Some(a), name("b"), 1..2)?;
            let _dropped = paths.key(Some(b), name("c"), 2..3);
            after = Some(paths.key(None, name("x"), 3..4));
            Ok(())
        });

        let first = KeyPathError::TooManySegments { limit: 4 };
        assert_eq!(
            after,
            Some(Err(first)),
            "the later key fits but must not be made"
        );
        assert_eq!(built, Err(first));
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

        assert_eq!(
            built,
            Err(KeyPathError::TooManySegments {
                limit: BUDGET.segments
            })
        );
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

    #[test]
    fn long_keys_nested_deep_fail_on_the_byte_budget_within_the_segment_budget() {
        // 1,000 levels of 4 KiB keys: 500,500 segments, inside the budget, but about 2 GB of path
        // text. 100 levels hold about 20.7 MB and fit.
        let key = "k".repeat(4 * 1024);

        let shallow = KeyPaths::build(BUDGET, deadline(), |paths| {
            paths.walk(None, &0, chain_of(100, key.clone()))
        });
        let deep = KeyPaths::build(BUDGET, deadline(), |paths| {
            paths.walk(None, &0, chain_of(1_000, key.clone()))
        });

        assert_eq!(shallow.map(|table| table.iter().count()), Ok(100));
        assert_eq!(
            deep,
            Err(KeyPathError::TooManyBytes {
                limit: DEFAULT_KEY_PATH_BYTES
            })
        );
    }

    #[test]
    fn many_children_of_one_huge_key_fail_on_the_byte_budget() {
        // One 13 MiB key with 150 one-byte children asks for about 1.9 GiB of path text.
        let huge = "h".repeat(13 * 1024 * 1024);
        let children: Vec<(Option<&'static str>, Doc)> = vec![(Some("c"), Doc::Scalar); 150];
        let doc = Doc::Map(vec![(Some(huge.leak()), Doc::Map(children))]);

        assert_eq!(
            walk(&doc, BUDGET),
            Err(KeyPathError::TooManyBytes {
                limit: DEFAULT_KEY_PATH_BYTES
            })
        );
    }

    #[test]
    fn a_mapping_wider_than_the_budget_fails_before_queueing_all_its_members() {
        let doc = Doc::Map(vec![(Some("k"), Doc::Scalar); 2_000]);
        CHARGED.set(0);

        let built = walk(&doc, segments(100));

        assert_eq!(built, Err(KeyPathError::TooManySegments { limit: 100 }));
        assert_eq!(
            CHARGED.get(),
            100,
            "the walk should stop charging at the first member over the budget"
        );
    }

    /// The most bytes of path text and no segment limit: only the byte budget can fail.
    const fn bytes(bytes: usize) -> KeyPathBudget {
        KeyPathBudget {
            segments: usize::MAX,
            bytes,
        }
    }

    #[test]
    fn the_byte_budget_counts_each_path_with_its_dots_and_fails_one_byte_over() {
        // Keys `a`, `a.a` and `a.a.a` are 1 + 3 + 5 = 9 bytes of path text as displayed, the
        // `.` separators included. Dropping the separator charge makes it 6; `>=` for `>`
        // refuses an exact fit.
        let nested = |budget| {
            KeyPaths::build(budget, deadline(), |paths| paths.walk(None, &0, chain(3)))
                .map(|table| paths(&table))
        };

        assert_eq!(
            nested(bytes(9)),
            Ok(vec!["a".to_owned(), "a.a".to_owned(), "a.a.a".to_owned()]),
            "9 bytes of path text should fit a budget of 9",
        );
        assert_eq!(
            nested(bytes(8)),
            Err(KeyPathError::TooManyBytes { limit: 8 }),
            "9 bytes of path text should not fit a budget of 8",
        );
    }

    #[test]
    fn a_nested_path_charges_its_separator_at_the_exact_byte_boundary() {
        // `a` is 1 byte and `a.b` is 3: 4 in all.
        let doc = Doc::Map(vec![(Some("a"), Doc::Map(vec![(Some("b"), Doc::Scalar)]))]);

        assert!(
            walk(&doc, bytes(4)).is_ok(),
            "4 bytes should fit a budget of 4"
        );
        assert_eq!(
            walk(&doc, bytes(3)),
            Err(KeyPathError::TooManyBytes { limit: 3 }),
            "the `.` of `a.b` is text, so 3 bytes cannot hold `a` and `a.b`",
        );
    }

    #[test]
    fn the_budget_is_what_the_limits_in_effect_say_and_zero_lifts_it() {
        let given = |segments: &str, size: &str| {
            Limits::resolve(
                &CommandLineLimits {
                    max_key_segments: Some(Given {
                        value: segments.parse().expect("segments should be a count"),
                        from_env: false,
                    }),
                    max_key_path_size: Some(Given {
                        value: size.parse().expect("size should be a size"),
                        from_env: false,
                    }),
                    ..CommandLineLimits::default()
                },
                &ConfigLimits::default(),
            )
        };
        let defaults = Limits::resolve(&CommandLineLimits::default(), &ConfigLimits::default());

        assert_eq!(
            KeyPathBudget::from_limits(&defaults),
            KeyPathBudget {
                segments: 200_000,
                bytes: DEFAULT_KEY_PATH_BYTES,
            },
        );
        assert_eq!(
            KeyPathBudget::from_limits(&given("7", "9KiB")),
            KeyPathBudget {
                segments: 7,
                bytes: 9 * 1024,
            },
        );
        assert_eq!(
            KeyPathBudget::from_limits(&given("0", "0")),
            KeyPathBudget {
                segments: usize::MAX,
                bytes: usize::MAX,
            },
        );
    }

    #[test]
    fn each_budget_failure_becomes_its_own_read_error_naming_its_knobs() {
        let path = AbsolutePath::new(std::path::Path::new("/repo/data.json"))
            .expect("the fixture path should be absolute");
        let language = Languages::builtin()
            .expect("the built-in cartridges should load")
            .for_path(std::path::Path::new("lib.rs"))
            .expect("the built-in registry should read `.rs`");
        let file = SourceFile::from_text(path, "fn a() {}\n".to_owned(), language, deadline())
            .expect("the fixture should parse");

        let segments = KeyPathError::TooManySegments { limit: 200_000 }.read_error(&file);
        let text = KeyPathError::TooManyBytes { limit: 4096 }.read_error(&file);
        let timeout = KeyPathError::Timeout(Stopped { offset: 3 }).read_error(&file);

        assert_eq!(segments.code(), "read-too-many-keys");
        let help = segments.help(ReadRole::Target, "data.json");
        for knob in [
            "max-key-segments",
            "SUPDOC_MAX_KEY_SEGMENTS",
            "--max-key-segments",
        ] {
            assert!(help.contains(knob), "{knob} missing from {help:?}");
        }
        assert_eq!(text.code(), "read-key-paths-too-large");
        let help = text.help(ReadRole::Target, "data.json");
        for knob in [
            "max-key-path-size",
            "SUPDOC_MAX_KEY_PATH_SIZE",
            "--max-key-path-size",
        ] {
            assert!(help.contains(knob), "{knob} missing from {help:?}");
        }
        assert!(
            matches!(timeout, ReadError::Timeout { offset: 3, .. }),
            "{timeout:?}"
        );
    }
}
