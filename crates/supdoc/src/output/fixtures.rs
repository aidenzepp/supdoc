//! Constructed values the output tests render. The real reports come with their commands.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use schemars::JsonSchema;
use serde::Serialize;

use crate::finding::{Finding, Location, Severity, Snippet};
use crate::link::SourcePath;
use crate::output::format::{OutputFormat, TextContext, Verbosity};
use crate::output::json::FindingJson;
use crate::output::paths::PathStyle;
use crate::output::report::Report;
use crate::output::sink::{Output, OutputSettings};
use crate::output::summary::{Summary, render_summary};
use crate::output::text::render_finding;
use crate::path::AbsolutePath;
use crate::state::LinkState;

/// An absolute path from a literal.
pub(super) fn absolute(raw: &str) -> AbsolutePath {
    AbsolutePath::new(std::path::Path::new(raw)).expect("the fixture path should be absolute")
}

/// A source path from a literal.
pub(super) fn source(raw: &str) -> SourcePath {
    raw.parse()
        .expect("the fixture source path should be valid")
}

/// Everything at `/repo`, run from `/repo`, with no GitHub workspace.
pub(super) fn paths() -> PathStyle {
    PathStyle::new(absolute("/repo"), absolute("/repo"), None)
}

/// Plain, ASCII text settings at 80 columns.
pub(super) fn context(verbosity: Verbosity) -> TextContext {
    TextContext {
        color: false,
        unicode: false,
        width: 80,
        paths: paths(),
        verbosity,
    }
}

/// Output settings for `format`, plain and ASCII.
pub(super) fn settings(format: OutputFormat, verbosity: Verbosity) -> OutputSettings {
    OutputSettings {
        format,
        verbosity,
        color: false,
        unicode: false,
        width: 80,
        paths: paths(),
    }
}

/// A buffer two writers can share, so a test can read what `Output` wrote.
#[derive(Clone, Default)]
pub(super) struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    /// Everything written so far, as text.
    pub(super) fn text(&self) -> String {
        let bytes = self
            .0
            .lock()
            .expect("the capture lock should not be poisoned");
        String::from_utf8(bytes.clone()).expect("output should be UTF-8")
    }
}

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("the capture lock should not be poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// An `Output` writing into two captures: stdout first, stderr second.
pub(super) fn captured(settings: OutputSettings) -> (Output, Capture, Capture) {
    let stdout = Capture::default();
    let stderr = Capture::default();
    let output = Output::new(settings, Box::new(stdout.clone()), Box::new(stderr.clone()));
    (output, stdout, stderr)
}

/// A finding on `needle` in `text`, which is the contents of `file`.
pub(super) fn finding_on(
    file: &str,
    text: &str,
    needle: &str,
    code: &'static str,
    severity: Severity,
    message: &str,
    help: &str,
) -> Finding {
    let start = text.find(needle).expect("the needle should be in the text");
    let range = start..start + needle.len();
    Finding::new(code, severity, message)
        .with_location(
            Location::in_text(source(file), text, range.clone())
                .expect("the range should be locatable"),
        )
        .with_snippet(Snippet::capture(text, range).expect("the range should be capturable"))
        .with_help(help)
}

/// The `check` example from 07: one failed, one broken, one obsolete link, one loose finding.
pub(super) fn check_report() -> CheckFixture {
    let guide = "# Guide\n\nThe [`add`](../src/lib.rs#add) function applies the offset.\n";
    let api = "# API\n\n- [login](../src/auth.rs#login) starts a session.\n";
    let failed = finding_on(
        "docs/guide.md",
        guide,
        "[`add`](../src/lib.rs#add)",
        "link-failed",
        Severity::Error,
        "`src/lib.rs#add` changed since this doc was confirmed",
        "reread this, and if it's still accurate run `supdoc update docs/guide.md --yes`",
    );
    let broken = finding_on(
        "docs/api.md",
        api,
        "[login](../src/auth.rs#login)",
        "resolve-not-found",
        Severity::Error,
        "no symbol `login` in src/auth.rs",
        "did you mean `log_in`? Run `supdoc fix docs/api.md --yes`",
    );
    let obsolete = Finding::new(
        "link-obsolete",
        Severity::Warning,
        "supdoc.lock has an entry for a link that no longer exists",
    )
    .with_help("run `supdoc update --state obsolete --yes`");
    let loose = Finding::new(
        "scan-unreadable-dir",
        Severity::Warning,
        "skipped private: permission denied",
    )
    .with_help("fix the directory's permissions, or add it to `exclude` in supdoc.config.toml");
    CheckFixture {
        command: "check",
        summary: Summary {
            passed: 38,
            failed: 1,
            broken: 1,
            new: 0,
            obsolete: 1,
        },
        links: vec![
            LinkFixture::new(
                LinkState::Failed,
                "docs/guide.md",
                "src/lib.rs",
                Some(failed),
            ),
            LinkFixture::new(
                LinkState::Broken,
                "docs/api.md",
                "src/auth.rs",
                Some(broken),
            ),
            LinkFixture::new(
                LinkState::Obsolete,
                "docs/old.md",
                "src/old.rs",
                Some(obsolete),
            ),
            LinkFixture::new(LinkState::Passed, "docs/setup.md", "src/config.rs", None),
        ],
        loose: vec![loose],
    }
}

/// A report shaped like `check`'s: counts, links with nested findings, and loose findings.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct CheckFixture {
    #[serde(skip)]
    pub(super) command: &'static str,
    pub(super) summary: Summary,
    pub(super) links: Vec<LinkFixture>,
    #[serde(skip)]
    pub(super) loose: Vec<Finding>,
}

/// One link in a [`CheckFixture`].
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct LinkFixture {
    pub(super) state: String,
    pub(super) source: String,
    pub(super) target: String,
    pub(super) finding: Option<FindingJson>,
    #[serde(skip)]
    pub(super) raw: Option<Finding>,
    #[serde(skip)]
    pub(super) link_state: Option<LinkState>,
}

impl LinkFixture {
    fn new(state: LinkState, source: &str, target: &str, raw: Option<Finding>) -> Self {
        Self {
            state: state.as_str().to_owned(),
            source: source.to_owned(),
            target: target.to_owned(),
            finding: raw.as_ref().map(FindingJson::nested),
            raw,
            link_state: Some(state),
        }
    }
}

impl Report for CheckFixture {
    fn command(&self) -> &'static str {
        self.command
    }

    fn summary(&self) -> Option<&Summary> {
        Some(&self.summary)
    }

    fn findings(&self) -> &[Finding] {
        &self.loose
    }

    fn annotated_findings(&self) -> Vec<&Finding> {
        self.links
            .iter()
            .filter_map(|link| link.raw.as_ref())
            .chain(&self.loose)
            .collect()
    }

    fn render_text(&self, context: &TextContext, out: &mut dyn Write) -> io::Result<()> {
        for link in &self.links {
            match (&link.raw, link.link_state) {
                (Some(finding), _) => render_finding(context, finding, out)?,
                (None, Some(LinkState::Passed)) if context.verbosity.lists_passed() => {
                    writeln!(out, "passed  {} -> {}\n", link.source, link.target)?;
                }
                (None, _) => {}
            }
        }
        for finding in &self.loose {
            render_finding(context, finding, out)?;
        }
        render_summary(context, &self.summary, out)
    }
}
