//! The tool cartridges supdoc knows how to call on a block, the environment they get, and
//! finding each one's binary on `PATH`.
//!
//! A tool is chosen only by its name in [`CARTRIDGES`]; no path or command ever comes from
//! config.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::config::code_blocks::{CodeBlocks, Dialect, Edition, Language};

use super::format::ParseError;
use super::lint;
use super::runner::{Failure, Ran};
use super::{python, rust, shell};

/// The variables a tool gets from supdoc's own environment, and no others.
const PASSED: [&str; 5] = [
    "PATH",
    "HOME",
    "RUSTUP_HOME",
    "CARGO_HOME",
    "XDG_CACHE_HOME",
];

/// The environment tools get, read once by `main` and passed down like the skill directories.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ToolEnv {
    /// Each passed variable that is set, with its value, in [`PASSED`] order.
    vars: Vec<(&'static str, OsString)>,
}

impl ToolEnv {
    /// Reads `PATH`, `HOME`, `RUSTUP_HOME`, `CARGO_HOME` and `XDG_CACHE_HOME` through `var`.
    pub(crate) fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Self {
        Self {
            vars: PASSED
                .into_iter()
                .filter_map(|name| var(name).map(|value| (name, value)))
                .collect(),
        }
    }

    /// Each passed variable that is set, with its value: the whole environment a tool gets.
    pub(crate) fn vars(&self) -> impl Iterator<Item = (&'static str, &OsStr)> {
        self.vars
            .iter()
            .map(|(name, value)| (*name, value.as_os_str()))
    }

    /// `PATH`, when it is set.
    fn path(&self) -> Option<&OsStr> {
        self.var("PATH")
    }

    /// The passed variable `name`, when it is set.
    pub(crate) fn var(&self, name: &str) -> Option<&OsStr> {
        self.vars
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_os_str())
    }
}

/// Which check a tool does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// Rewrites the block into its formatted form.
    Format,
    /// Reports diagnostics on the block, never rewriting it.
    Lint,
}

/// What a tool loads, which decides whether it may run in a checkout supdoc doesn't trust. The
/// tiers are ordered: a tool above [`Tier::Static`] never runs until trust exists (see `supdoc trust`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Tier {
    /// Reads only the block on stdin and plain-data config: runs in any checkout.
    Static,
    /// Compiles the block, so it needs a trusted project.
    Project,
}

/// What a cartridge's argv is built from: the block's place in its doc and its language's
/// settings.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Invocation<'run> {
    /// The doc's absolute path.
    pub(crate) doc: &'run Path,
    /// The block's 1-based position among the blocks of its doc.
    pub(crate) index: usize,
    /// The Rust edition the block is read as.
    pub(crate) edition: Edition,
    /// The shell dialect the block is read as.
    pub(crate) dialect: Dialect,
    /// The flags the project contributes, each found inside the root: rustfmt's
    /// `--config-path`, clippy's `[lints]` levels. Empty for every other tool.
    pub(crate) project: &'run [OsString],
    /// Where clippy writes the block's metadata. `None` names no path, which is how a cache key
    /// names every run of the same block alike.
    pub(crate) emit: Option<&'run Path>,
    /// What clippy links the block against: the project build's `--extern` and `-L` flags,
    /// placed right after `--error-format=json`. Empty when the block compiles alone, and for
    /// every other tool.
    pub(crate) externs: &'run [OsString],
}

/// How a cartridge reads a finished run, which also says which step it does.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Reader {
    /// A formatter: its syntax-error report in stderr, `None` for any other stderr. The format
    /// step compares its stdout with the block (see [`super::format::judge`]).
    Format(fn(&str) -> Option<ParseError>),
    /// A linter: its verdict on a run over the checked text.
    Lint(fn(&str, Result<Ran, Failure>) -> lint::Verdict),
}

/// One tool supdoc knows how to call safely: everything the pipeline needs to look it up, run it
/// on a block and read the result.
///
/// A struct of fn pointers rather than a trait: the seven cartridges are constant data in one
/// table, chosen by name, with no state of their own, so a trait would add a unit type and an
/// `impl` per tool and `dyn` dispatch while giving nothing a fn pointer doesn't.
#[derive(Debug)]
pub(crate) struct ToolCartridge {
    /// The name config uses for it.
    pub(crate) name: &'static str,
    /// The language whose blocks it checks.
    pub(crate) language: Language,
    /// The binary looked up on `PATH`.
    pub(crate) binary: &'static str,
    /// What it loads.
    pub(crate) tier: Tier,
    /// The command that installs it.
    pub(crate) install: &'static str,
    /// The config files it reads, by name at the project root: each one's content is part of
    /// its cached results' key.
    pub(crate) config_files: &'static [&'static str],
    /// The values its step's mode accepts, in config and in a block attribute.
    pub(crate) modes: &'static [&'static str],
    /// Its arguments for a block.
    argv: fn(&Invocation<'_>) -> Vec<OsString>,
    /// How a run is read.
    reader: Reader,
}

impl ToolCartridge {
    /// The check it does.
    #[must_use]
    pub(crate) const fn step(&self) -> Step {
        match self.reader {
            Reader::Format(_) => Step::Format,
            Reader::Lint(_) => Step::Lint,
        }
    }

    /// Its arguments for the block `invocation` describes.
    #[must_use]
    pub(crate) fn args(&self, invocation: &Invocation<'_>) -> Vec<OsString> {
        (self.argv)(invocation)
    }

    /// Its syntax-error report in `stderr`. `None` for any other stderr, and always for a
    /// linter.
    #[must_use]
    pub(crate) fn parse_error(&self, stderr: &str) -> Option<ParseError> {
        match self.reader {
            Reader::Format(read) => read(stderr),
            Reader::Lint(_) => None,
        }
    }

    /// Its lint verdict on `result`, a run over `input`. A formatter reports no diagnostics, so
    /// it is judged on its exit alone.
    #[must_use]
    pub(crate) fn lint_verdict(&self, input: &str, result: Result<Ran, Failure>) -> lint::Verdict {
        match self.reader {
            Reader::Lint(read) => read(input, result),
            Reader::Format(_) => lint::judge(result, |_| None),
        }
    }
}

/// The values every cartridge's mode accepts today.
const FULL_OR_OFF: &[&str] = &["full", "off"];

/// The known tools. A language's first cartridge for a step is its default for that step.
pub(crate) static CARTRIDGES: [ToolCartridge; 7] = [
    ToolCartridge {
        name: "rustfmt",
        language: Language::Rust,
        binary: "rustfmt",
        tier: Tier::Static,
        install: "rustup component add rustfmt",
        config_files: &["rustfmt.toml", ".rustfmt.toml"],
        modes: FULL_OR_OFF,
        argv: rustfmt_args,
        reader: Reader::Format(rust::parse_error),
    },
    ToolCartridge {
        name: "shfmt",
        language: Language::Shell,
        binary: "shfmt",
        tier: Tier::Static,
        install: "go install mvdan.cc/sh/v3/cmd/shfmt@latest",
        config_files: &[".editorconfig"],
        modes: FULL_OR_OFF,
        argv: shfmt_args,
        reader: Reader::Format(shell::parse_error),
    },
    ToolCartridge {
        name: "ruff-format",
        language: Language::Python,
        binary: "ruff",
        tier: Tier::Static,
        install: "pipx install ruff",
        config_files: &["ruff.toml", ".ruff.toml", "pyproject.toml"],
        modes: FULL_OR_OFF,
        argv: ruff_format_args,
        reader: Reader::Format(python::ruff_parse_error),
    },
    ToolCartridge {
        name: "black",
        language: Language::Python,
        binary: "black",
        tier: Tier::Static,
        install: "pipx install black",
        config_files: &["pyproject.toml"],
        modes: FULL_OR_OFF,
        argv: black_args,
        reader: Reader::Format(python::black_parse_error),
    },
    ToolCartridge {
        name: "ruff",
        language: Language::Python,
        binary: "ruff",
        tier: Tier::Static,
        install: "pipx install ruff",
        config_files: &["ruff.toml", ".ruff.toml", "pyproject.toml"],
        modes: FULL_OR_OFF,
        argv: ruff_args,
        reader: Reader::Lint(python::lint_verdict),
    },
    ToolCartridge {
        name: "shellcheck",
        language: Language::Shell,
        binary: "shellcheck",
        tier: Tier::Static,
        install: "cabal install ShellCheck",
        config_files: &[".shellcheckrc"],
        modes: FULL_OR_OFF,
        argv: shellcheck_args,
        reader: Reader::Lint(shell::lint_verdict),
    },
    ToolCartridge {
        name: "clippy",
        language: Language::Rust,
        binary: "clippy-driver",
        tier: Tier::Project,
        install: "rustup component add clippy",
        config_files: &["clippy.toml", ".clippy.toml", "Cargo.toml"],
        modes: FULL_OR_OFF,
        argv: clippy_args,
        reader: Reader::Lint(rust::lint_verdict),
    },
];

/// `language`'s default formatter: its first format cartridge, whatever its tier.
#[must_use]
pub(crate) fn formatter(language: Language) -> Option<&'static ToolCartridge> {
    default_for(language, Step::Format)
}

/// `language`'s first cartridge for `step`: its default for the step.
#[must_use]
pub(crate) fn default_for(language: Language, step: Step) -> Option<&'static ToolCartridge> {
    CARTRIDGES
        .iter()
        .find(|tool| tool.language == language && tool.step() == step)
}

/// The cartridge `code_blocks` selects for `language`'s `step`: the one its `formatter` or
/// `linter` names, else the default. `None` when the language has none.
#[must_use]
pub(crate) fn selected(
    code_blocks: &CodeBlocks,
    language: Language,
    step: Step,
) -> Option<&'static ToolCartridge> {
    let written = match step {
        Step::Format => code_blocks.formatter(language),
        Step::Lint => code_blocks.linter(language),
    };
    written.map_or_else(
        || default_for(language, step),
        |name| named(language, step, name),
    )
}

/// The cartridge named `name` for `language`'s `step`, as config selects it.
#[must_use]
pub(crate) fn named(language: Language, step: Step, name: &str) -> Option<&'static ToolCartridge> {
    CARTRIDGES
        .iter()
        .find(|tool| tool.language == language && tool.step() == step && tool.name == name)
}

/// The `index`th block (1-based) of `doc`, the doc's absolute path, as a tool sees it:
/// `<doc dir>/<doc file name>.<index>.<extension>`, so the tool finds its config walking up from
/// the doc's real directory while the block itself arrives on stdin.
fn virtual_path(doc: &Path, index: usize, extension: &str) -> OsString {
    let mut name = doc.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{index}.{extension}"));
    doc.with_file_name(name).into_os_string()
}

/// `rustfmt`'s arguments: `--edition <year> --emit stdout`, then the project's `--config-path`.
/// The block arrives on stdin as written; rustfmt takes no stdin file name, so the doc and the
/// block's index don't appear.
fn rustfmt_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    ["--edition", invocation.edition.year(), "--emit", "stdout"]
        .into_iter()
        .map(OsString::from)
        .chain(invocation.project.iter().cloned())
        .collect()
}

/// `shfmt`'s arguments: `--filename <doc>.<index>.sh` (see [`virtual_path`]), then `-ln
/// <dialect>`.
fn shfmt_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    vec![
        OsString::from("--filename"),
        virtual_path(invocation.doc, invocation.index, "sh"),
        OsString::from("-ln"),
        OsString::from(invocation.dialect.language_name()),
    ]
}

/// `ruff format`'s arguments: the block on stdin under its virtual path, no cache.
fn ruff_format_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    vec![
        OsString::from("format"),
        OsString::from("--no-cache"),
        OsString::from("--stdin-filename"),
        virtual_path(invocation.doc, invocation.index, "py"),
        OsString::from("-"),
    ]
}

/// `black`'s arguments: quiet, the block on stdin under its virtual path.
fn black_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    vec![
        OsString::from("--quiet"),
        OsString::from("--stdin-filename"),
        virtual_path(invocation.doc, invocation.index, "py"),
        OsString::from("-"),
    ]
}

/// The rules `ruff check` leaves off for a block, which a snippet breaks for lack of context:
/// undefined names, unused imports and unused variables.
const RUFF_RELAXED: &str = "F821,F401,F841";

/// `ruff check`'s arguments: the block on stdin under its virtual path, JSON diagnostics, no
/// cache, the snippet relaxations ignored, and never `--fix`.
fn ruff_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    vec![
        OsString::from("check"),
        OsString::from("--no-cache"),
        OsString::from("--output-format=json"),
        OsString::from("--stdin-filename"),
        virtual_path(invocation.doc, invocation.index, "py"),
        OsString::from("--ignore"),
        OsString::from(RUFF_RELAXED),
        OsString::from("-"),
    ]
}

/// The checks `shellcheck` leaves off for a block, which a snippet breaks for lack of context:
/// a variable used but never assigned (SC2154), one assigned but never used (SC2034), and a
/// sourced file it can't follow (SC1091).
const SHELLCHECK_RELAXED: &str = "SC2154,SC2034,SC1091";

/// `shellcheck`'s arguments: `-s <shell>` for the dialect (POSIX is `sh`, mksh is `ksh`), no rc
/// file, json1 diagnostics, the snippet relaxations excluded, and the block on stdin. `-x` and
/// `--external-sources` are never passed (see [`shell`]).
fn shellcheck_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    let shell = match invocation.dialect {
        Dialect::Bash => "bash",
        Dialect::Posix => "sh",
        Dialect::Mksh => "ksh",
    };
    vec![
        OsString::from("-s"),
        OsString::from(shell),
        OsString::from("--norc"),
        OsString::from("--format=json1"),
        OsString::from(format!("--exclude={SHELLCHECK_RELAXED}")),
        OsString::from("-"),
    ]
}

/// The lints clippy leaves off for a block, which a snippet breaks for lack of context, as
/// `-A` flags after the project's own so they win.
const CLIPPY_RELAXED: [&str; 4] = [
    "unused",
    "dead_code",
    "missing_docs",
    "clippy::missing_docs_in_private_items",
];

/// `clippy-driver`'s arguments: `--edition <E> --crate-type lib --emit=metadata=<emit>
/// --error-format=json`, the project build's `--extern` and `-L` flags when the block is linked
/// against it, the project's `lints` flags, the snippet relaxations, and `-` for the block on
/// stdin. Nothing else from the project: no `--cfg`.
fn clippy_args(invocation: &Invocation<'_>) -> Vec<OsString> {
    let mut metadata = OsString::from("--emit=metadata");
    if let Some(emit) = invocation.emit {
        metadata.push("=");
        metadata.push(emit);
    }
    let mut args: Vec<OsString> = vec![
        "--edition".into(),
        invocation.edition.year().into(),
        "--crate-type".into(),
        "lib".into(),
        metadata,
        "--error-format=json".into(),
    ];
    args.extend(invocation.externs.iter().cloned());
    args.extend(invocation.project.iter().cloned());
    for lint in CLIPPY_RELAXED {
        args.extend(["-A".into(), lint.into()]);
    }
    args.push("-".into());
    args
}

/// Where a tool's binary was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Found {
    /// On `PATH`, outside the project root, at the real path held: the path a tool is run by.
    OnPath(PathBuf),
    /// On `PATH`, but its real path is inside the project root, so the checkout chose it.
    InsideRoot(PathBuf),
    /// Not on `PATH`.
    Missing,
}

/// Looks `binary` up on `env`'s `PATH`, judging the first match's real path against `root`.
///
/// Effect: reads file metadata along `PATH` and resolves symlinks.
#[must_use]
pub(crate) fn find(binary: &str, env: &ToolEnv, root: &Path) -> Found {
    let Some(real) = env.path().and_then(|path| first_on_path(binary, path)) else {
        return Found::Missing;
    };
    if inside(&real, root) {
        Found::InsideRoot(real)
    } else {
        Found::OnPath(real)
    }
}

/// The `PATH` entries a lookup searches, in order: the absolute ones. An empty entry (the
/// current directory to a shell) and every relative one are skipped.
fn search_dirs(path: &OsStr) -> impl Iterator<Item = PathBuf> {
    std::env::split_paths(path).filter(|dir| dir.is_absolute())
}

/// The real path of the first executable regular file named `binary` in [`search_dirs`].
///
/// Effect: reads file metadata and resolves symlinks.
fn first_on_path(binary: &str, path: &OsStr) -> Option<PathBuf> {
    search_dirs(path)
        .map(|dir| dir.join(binary))
        .filter(|candidate| is_executable_file(candidate))
        .find_map(|candidate| candidate.canonicalize().ok())
}

/// Whether `real`, a resolved path, is inside `root`: under the root's real path by whole
/// components, or under a directory that is the root's own file however it is spelled.
///
/// Effect: reads the metadata of `root` and of each ancestor of `real`.
fn inside(real: &Path, root: &Path) -> bool {
    let real_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    real.starts_with(&real_root) || under_same_file(real, root)
}

/// Whether some ancestor of `real` has the same device and inode as `root`, so a firmlink or
/// bind-mount spelling of the root counts as the root.
///
/// Effect: reads the metadata of `root` and of each ancestor of `real`.
#[cfg(unix)]
fn under_same_file(real: &Path, root: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let Ok(root) = std::fs::metadata(root) else {
        return false;
    };
    real.ancestors()
        .filter_map(|ancestor| std::fs::metadata(ancestor).ok())
        .any(|meta| meta.dev() == root.dev() && meta.ino() == root.ino())
}

/// Whether some ancestor of `real` is `root` by file identity: never known off Unix.
#[cfg(not(unix))]
fn under_same_file(_real: &Path, _root: &Path) -> bool {
    false
}

/// Whether `path`, following symlinks, is a regular file someone may execute.
///
/// Effect: reads the file's metadata.
fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && executable(&meta))
}

/// Whether `meta`'s permissions let someone execute the file.
#[cfg(unix)]
fn executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o111 != 0
}

/// Whether `meta`'s permissions let someone execute the file: any regular file off Unix.
#[cfg(not(unix))]
fn executable(_meta: &std::fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use std::path::Path;

    use super::{
        CARTRIDGES, Found, Invocation, Step, Tier, ToolCartridge, ToolEnv, find, search_dirs,
        under_same_file,
    };
    use crate::config::code_blocks::{Dialect, Edition, Language};

    /// The cartridge named `name`.
    fn cartridge(name: &str) -> &'static ToolCartridge {
        CARTRIDGES
            .iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("the cartridges should include {name}"))
    }

    /// `name`'s arguments for the `index`th block of `/p/docs/a.md`, a 2024 bash block, with
    /// `project`'s flags and no metadata path.
    fn args_of(name: &str, index: usize, project: &[OsString]) -> Vec<OsString> {
        cartridge(name).args(&Invocation {
            doc: Path::new("/p/docs/a.md"),
            index,
            edition: Edition::E2024,
            dialect: Dialect::Bash,
            project,
            emit: None,
            externs: &[],
        })
    }

    #[test]
    fn each_cartridge_declares_its_language_step_binary_and_tier() {
        let declared: Vec<_> = CARTRIDGES
            .iter()
            .map(|tool| {
                (
                    tool.name,
                    tool.language,
                    tool.step(),
                    tool.binary,
                    tool.tier,
                )
            })
            .collect();

        assert_eq!(
            declared,
            [
                (
                    "rustfmt",
                    Language::Rust,
                    Step::Format,
                    "rustfmt",
                    Tier::Static
                ),
                (
                    "shfmt",
                    Language::Shell,
                    Step::Format,
                    "shfmt",
                    Tier::Static
                ),
                (
                    "ruff-format",
                    Language::Python,
                    Step::Format,
                    "ruff",
                    Tier::Static
                ),
                (
                    "black",
                    Language::Python,
                    Step::Format,
                    "black",
                    Tier::Static
                ),
                ("ruff", Language::Python, Step::Lint, "ruff", Tier::Static),
                (
                    "shellcheck",
                    Language::Shell,
                    Step::Lint,
                    "shellcheck",
                    Tier::Static
                ),
                (
                    "clippy",
                    Language::Rust,
                    Step::Lint,
                    "clippy-driver",
                    Tier::Project
                ),
            ]
        );
    }

    #[test]
    fn every_cartridge_accepts_full_and_off() {
        for tool in &CARTRIDGES {
            assert_eq!(tool.modes, ["full", "off"], "{}", tool.name);
        }
    }

    #[test]
    fn a_formatter_reads_a_parse_error_and_a_linter_none() {
        let stderr = "error: expected one of `;`\n --> <stdin>:3:9\n";

        assert!(cartridge("rustfmt").parse_error(stderr).is_some());
        assert_eq!(cartridge("clippy").parse_error(stderr), None);
    }

    #[test]
    fn rustfmt_reads_the_block_as_its_edition_then_the_projects_config_path() {
        assert_eq!(
            args_of(
                "rustfmt",
                1,
                &["--config-path".into(), "/p/rustfmt.toml".into()]
            ),
            [
                "--edition",
                "2024",
                "--emit",
                "stdout",
                "--config-path",
                "/p/rustfmt.toml"
            ]
        );
    }

    #[test]
    fn shfmt_names_the_block_by_its_index_then_its_dialect() {
        let posix = cartridge("shfmt").args(&Invocation {
            doc: Path::new("/p/docs/a.md"),
            index: 3,
            edition: Edition::E2024,
            dialect: Dialect::Posix,
            project: &[],
            emit: None,
            externs: &[],
        });

        assert_eq!(posix, ["--filename", "/p/docs/a.md.3.sh", "-ln", "posix"]);
    }

    #[test]
    fn ruff_formats_stdin_under_the_blocks_virtual_path_without_a_cache() {
        assert_eq!(
            args_of("ruff-format", 2, &[]),
            [
                "format",
                "--no-cache",
                "--stdin-filename",
                "/p/docs/a.md.2.py",
                "-"
            ]
        );
    }

    #[test]
    fn black_formats_stdin_quietly_under_the_blocks_virtual_path() {
        assert_eq!(
            args_of("black", 1, &[]),
            ["--quiet", "--stdin-filename", "/p/docs/a.md.1.py", "-"]
        );
    }

    #[test]
    fn ruff_checks_stdin_under_the_virtual_path_with_the_relaxations_and_no_fix() {
        let args = args_of("ruff", 2, &[]);

        assert_eq!(
            args,
            [
                "check",
                "--no-cache",
                "--output-format=json",
                "--stdin-filename",
                "/p/docs/a.md.2.py",
                "--ignore",
                "F821,F401,F841",
                "-"
            ]
        );
        assert!(!args.iter().any(|arg| arg == "--fix"), "{args:?}");
    }

    #[test]
    fn shellcheck_reads_stdin_in_json1_with_no_rc_and_the_relaxations() {
        let args = args_of("shellcheck", 2, &[]);

        assert_eq!(
            args,
            [
                "-s",
                "bash",
                "--norc",
                "--format=json1",
                "--exclude=SC2154,SC2034,SC1091",
                "-"
            ]
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg == "-x" || arg == "--external-sources"),
            "{args:?}"
        );
    }

    #[test]
    fn each_dialect_becomes_shellchecks_shell_flag() {
        let shell = |dialect| {
            cartridge("shellcheck")
                .args(&Invocation {
                    doc: Path::new("/p/a.md"),
                    index: 1,
                    edition: Edition::E2024,
                    dialect,
                    project: &[],
                    emit: None,
                    externs: &[],
                })
                .into_iter()
                .take(2)
                .collect::<Vec<_>>()
        };

        assert_eq!(shell(Dialect::Bash), ["-s", "bash"]);
        assert_eq!(shell(Dialect::Posix), ["-s", "sh"]);
        assert_eq!(shell(Dialect::Mksh), ["-s", "ksh"]);
    }

    #[test]
    fn clippy_gets_the_block_on_stdin_with_its_edition_metadata_path_and_relaxations_last() {
        let lints = ["-W".into(), "clippy::pedantic".into()];
        let invocation = Invocation {
            doc: Path::new("/p/docs/a.md"),
            index: 1,
            edition: Edition::E2021,
            dialect: Dialect::Bash,
            project: &lints,
            emit: Some(Path::new("/c/emit/1-0/block.rmeta")),
            externs: &[],
        };

        let args = cartridge("clippy").args(&invocation);

        assert_eq!(
            args,
            [
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--emit=metadata=/c/emit/1-0/block.rmeta",
                "--error-format=json",
                "-W",
                "clippy::pedantic",
                "-A",
                "unused",
                "-A",
                "dead_code",
                "-A",
                "missing_docs",
                "-A",
                "clippy::missing_docs_in_private_items",
                "-",
            ]
        );
        for refused in ["--extern", "-L", "--cfg"] {
            assert!(!args.iter().any(|arg| arg == refused), "{args:?}");
        }
        let key = cartridge("clippy").args(&Invocation {
            emit: None,
            ..invocation
        });
        assert!(
            key.contains(&"--emit=metadata".into()),
            "a cache key names no metadata path"
        );
    }

    #[test]
    fn clippy_gets_the_project_builds_externs_after_the_error_format_and_before_the_lints() {
        let lints = ["-W".into(), "clippy::pedantic".into()];
        let externs = [
            "--extern".into(),
            "proj=/t/debug/deps/libproj-1.rmeta".into(),
            "-L".into(),
            "dependency=/t/debug/deps".into(),
        ];

        let args = cartridge("clippy").args(&Invocation {
            doc: Path::new("/p/docs/a.md"),
            index: 1,
            edition: Edition::E2021,
            dialect: Dialect::Bash,
            project: &lints,
            emit: None,
            externs: &externs,
        });

        let at = args
            .iter()
            .position(|arg| arg == "--error-format=json")
            .expect("clippy should get its error format");
        assert_eq!(
            args.get(at + 1..at + 7),
            Some(
                [
                    "--extern",
                    "proj=/t/debug/deps/libproj-1.rmeta",
                    "-L",
                    "dependency=/t/debug/deps",
                    "-W",
                    "clippy::pedantic",
                ]
                .map(OsString::from)
                .as_slice()
            ),
            "{args:?}"
        );
        assert_eq!(args.last(), Some(&OsString::from("-")), "{args:?}");
    }

    /// Writes an executable `#!/bin/sh` stub named `name` in `dir`.
    fn stub(dir: &Path, name: &str) {
        fs::create_dir_all(dir).expect("the stub directory should be creatable");
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\n").expect("the stub should be writable");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .expect("the stub should be made executable");
    }

    /// A [`ToolEnv`] whose `PATH` is `entries` joined.
    fn env_with_path(entries: &[&Path]) -> ToolEnv {
        let path = std::env::join_paths(entries).expect("the fixture PATH should join");
        ToolEnv::from_vars(|name| (name == "PATH").then(|| path.clone()))
    }

    /// A fresh directory holding a `root` project directory and an `outside` one.
    fn dirs() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&root).expect("the root should be creatable");
        fs::create_dir_all(&outside).expect("the outside directory should be creatable");
        (dir, root, outside)
    }

    #[test]
    fn a_relative_entry_is_skipped() {
        let (_dir, root, _outside) = dirs();
        stub(&root.join("bin"), "shfmt");
        let env = ToolEnv::from_vars(|name| (name == "PATH").then(|| OsString::from("bin")));

        assert_eq!(find("shfmt", &env, &root), Found::Missing);
    }

    #[test]
    fn a_file_that_is_not_executable_is_skipped() {
        let (_dir, root, outside) = dirs();
        fs::write(outside.join("shfmt"), "#!/bin/sh\n").expect("the file should be writable");

        assert_eq!(
            find("shfmt", &env_with_path(&[&outside]), &root),
            Found::Missing
        );
    }

    #[test]
    fn a_symlink_outside_whose_real_path_is_in_the_root_is_refused() {
        let (_dir, root, outside) = dirs();
        stub(&root.join("bin"), "shfmt");
        symlink(root.join("bin/shfmt"), outside.join("shfmt"))
            .expect("the fixture symlink should be creatable");

        let found = find("shfmt", &env_with_path(&[&outside]), &root);

        assert!(matches!(found, Found::InsideRoot(_)), "{found:?}");
    }

    #[test]
    fn a_tool_on_no_entry_is_missing() {
        let (_dir, root, outside) = dirs();

        assert_eq!(
            find("shfmt", &env_with_path(&[&outside]), &root),
            Found::Missing
        );
        assert_eq!(
            find("shfmt", &ToolEnv::from_vars(|_| None), &root),
            Found::Missing
        );
    }

    #[test]
    fn the_first_match_on_path_wins() {
        let (_dir, root, outside) = dirs();
        stub(&outside, "shfmt");
        stub(&root.join("bin"), "shfmt");
        let real = outside
            .join("shfmt")
            .canonicalize()
            .expect("the stub should resolve");

        assert_eq!(
            find(
                "shfmt",
                &env_with_path(&[&outside, &root.join("bin")]),
                &root
            ),
            Found::OnPath(real)
        );
        assert!(matches!(
            find(
                "shfmt",
                &env_with_path(&[&root.join("bin"), &outside]),
                &root
            ),
            Found::InsideRoot(_)
        ));
    }

    #[test]
    fn an_empty_entry_and_dot_are_not_searched() {
        let dirs: Vec<_> = search_dirs(OsStr::new(":.::./bin:/usr/bin:")).collect();

        assert_eq!(dirs, [Path::new("/usr/bin")]);
    }

    #[test]
    fn a_path_directory_that_is_a_symlink_into_the_root_is_refused() {
        let (_dir, root, outside) = dirs();
        stub(&root.join("bin"), "shfmt");
        symlink(root.join("bin"), outside.join("bin"))
            .expect("the fixture symlink should be creatable");

        let found = find("shfmt", &env_with_path(&[&outside.join("bin")]), &root);

        assert!(matches!(found, Found::InsideRoot(_)), "{found:?}");
    }

    #[test]
    fn a_root_reached_through_a_symlink_is_judged_by_its_real_path() {
        let (dir, root, outside) = dirs();
        stub(&root.join("bin"), "shfmt");
        stub(&outside, "ruff");
        let alias = dir.path().join("alias");
        symlink(&root, &alias).expect("the fixture symlink should be creatable");

        let inside = find("shfmt", &env_with_path(&[&root.join("bin")]), &alias);
        let elsewhere = find("ruff", &env_with_path(&[&outside]), &alias);

        assert!(matches!(inside, Found::InsideRoot(_)), "{inside:?}");
        assert!(matches!(elsewhere, Found::OnPath(_)), "{elsewhere:?}");
    }

    #[test]
    fn a_sibling_root_sharing_the_name_prefix_is_not_inside() {
        let (dir, root, _outside) = dirs();
        let sibling = dir.path().join("root2");
        stub(&sibling.join("bin"), "shfmt");

        let found = find("shfmt", &env_with_path(&[&sibling.join("bin")]), &root);

        assert!(matches!(found, Found::OnPath(_)), "{found:?}");
    }

    #[test]
    fn a_same_inode_alias_of_the_root_is_inside() {
        let (dir, root, _outside) = dirs();
        stub(&root.join("bin"), "shfmt");
        let alias = dir.path().join("alias");
        symlink(&root, &alias).expect("the fixture symlink should be creatable");
        let real = root
            .join("bin/shfmt")
            .canonicalize()
            .expect("the stub should resolve");

        assert!(
            !real.starts_with(&alias),
            "the alias should not be a path prefix"
        );
        assert!(under_same_file(&real, &alias));
    }
}
