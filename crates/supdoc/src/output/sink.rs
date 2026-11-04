//! `Output`: the one value that writes to stdout and stderr.

use std::io::{self, Write};
use std::sync::atomic::AtomicI32;

use crate::finding::Finding;
use crate::output::format::{OutputFormat, TextContext, Verbosity};
#[cfg(test)]
use crate::output::json::FindingJson;
use crate::output::json::{Envelope, FatalJson};
use crate::output::paths::PathStyle;
use crate::output::report::Report;
use crate::output::{github, jsonl, text};
use crate::path::AbsolutePath;
use crate::process;

/// What `main` decides about output before any command runs.
#[derive(Debug, Clone)]
pub(crate) struct OutputSettings {
    /// The format data is written in.
    pub(crate) format: OutputFormat,
    /// How much to say.
    pub(crate) verbosity: Verbosity,
    /// Whether text output carries ANSI color.
    pub(crate) color: bool,
    /// Whether text output draws with Unicode instead of ASCII.
    pub(crate) unicode: bool,
    /// The terminal width in columns.
    pub(crate) width: usize,
    /// How paths are shown.
    pub(crate) paths: PathStyle,
}

impl OutputSettings {
    /// What text rendering needs from these settings.
    fn text_context(&self) -> TextContext {
        TextContext {
            color: self.color,
            unicode: self.unicode,
            width: self.width,
            paths: self.paths.clone(),
            verbosity: self.verbosity,
        }
    }
}

/// Why output could not be written.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OutputError {
    /// Writing to stdout or stderr failed for a reason other than a closed pipe.
    #[error("could not write supdoc's output: {0}")]
    Write(#[from] io::Error),
    /// A report could not be turned into JSON.
    #[error("could not turn supdoc's output into JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Every byte supdoc prints goes through here.
///
/// A closed pipe, as in `supdoc list | head`, is not an error: the first write that meets one
/// marks that stream closed, and later writes to it are dropped quietly. The other
/// stream keeps working, and the run keeps its own exit code.
///
/// Once a termination signal has arrived, every write blocks until the signal ends the process,
/// so a run killed mid-flight never prints a report built from killed tools.
pub(crate) struct Output {
    /// How to write.
    settings: OutputSettings,
    /// Where data goes.
    stdout: Box<dyn Write>,
    /// Where notes about the run, diagnostics and fatal errors go.
    stderr: Box<dyn Write>,
    /// Whether the reader of stdout hung up.
    stdout_closed: bool,
    /// Whether the reader of stderr hung up.
    stderr_closed: bool,
    /// The flag a termination signal sets; while it is set, writes are held.
    terminating: &'static AtomicI32,
}

impl std::fmt::Debug for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Output")
            .field("settings", &self.settings)
            .field("stdout_closed", &self.stdout_closed)
            .field("stderr_closed", &self.stderr_closed)
            .finish_non_exhaustive()
    }
}

impl Output {
    /// Output that writes to `stdout` and `stderr`.
    #[must_use]
    pub(crate) fn new(
        settings: OutputSettings,
        stdout: Box<dyn Write>,
        stderr: Box<dyn Write>,
    ) -> Self {
        Self {
            settings,
            stdout,
            stderr,
            stdout_closed: false,
            stderr_closed: false,
            terminating: &process::TERMINATING,
        }
    }

    /// The same output, holding its writes on `flag` instead of the process's termination flag.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn with_termination_flag(mut self, flag: &'static AtomicI32) -> Self {
        self.terminating = flag;
        self
    }

    /// Whether a write now would be held until a termination signal ends the process.
    #[must_use]
    pub(crate) fn holds_writes(&self) -> bool {
        process::ends_by_signal(self.terminating)
    }

    /// Output on the process's own stdout and stderr, with ANSI codes stripped wherever
    /// `choice` and the terminal say color is off.
    #[must_use]
    pub(crate) fn stdio(settings: OutputSettings, choice: anstream::ColorChoice) -> Self {
        Self::new(
            settings,
            Box::new(anstream::AutoStream::new(io::stdout(), choice)),
            Box::new(anstream::AutoStream::new(io::stderr(), choice)),
        )
    }

    /// Sets the project root once `Config::load` has found it. Everything written after this
    /// call, including the JSON `root`, uses it; text paths stay relative to the current
    /// directory.
    pub(crate) fn set_root(&mut self, root: AbsolutePath) {
        self.settings.paths.set_root(root);
    }

    /// Whether text output draws with Unicode instead of ASCII.
    #[must_use]
    pub(crate) const fn unicode(&self) -> bool {
        self.settings.unicode
    }

    /// How much to say, as `main` decided it from the flags.
    #[must_use]
    pub(crate) const fn verbosity(&self) -> Verbosity {
        self.settings.verbosity
    }

    /// Whether the reader of stdout closed the pipe. The run's exit code does not change.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn is_closed(&self) -> bool {
        self.stdout_closed
    }

    /// Writes `report` to stdout in the chosen format. `ok` must be true exactly when the process
    /// will exit 0.
    ///
    /// # Errors
    ///
    /// Fails if the report can't be turned into JSON or a write fails for any reason but a
    /// closed pipe.
    pub(crate) fn report<R: Report>(&mut self, report: &R, ok: bool) -> Result<(), OutputError> {
        let mut bytes = Vec::new();
        match self.settings.format {
            OutputFormat::Text => report.render_text(&self.settings.text_context(), &mut bytes)?,
            OutputFormat::Json => {
                let envelope = Envelope::new(report, ok, self.settings.paths.root());
                serde_json::to_writer(&mut bytes, &envelope)?;
                bytes.push(b'\n');
            }
            OutputFormat::Jsonl => {
                for line in jsonl::lines(report, ok) {
                    serde_json::to_writer(&mut bytes, &line)?;
                    bytes.push(b'\n');
                }
            }
            OutputFormat::Github => {
                github::render_annotations(
                    &self.settings.paths,
                    self.settings.verbosity,
                    &report.annotated_findings(),
                    &mut bytes,
                )?;
                if let Some(summary) = report.summary()
                    && self.settings.verbosity.shows_summary()
                {
                    writeln!(bytes, "{}", summary.line(false, self.settings.unicode))?;
                }
            }
        }
        self.write(Stream::Stdout, &bytes)
    }

    /// Writes an artifact, a script or a schema. Text and GitHub output print `raw` exactly as
    /// given, because the artifact is the file the user asked for. The JSON formats wrap it by
    /// writing `report`, which carries the same artifact as data.
    ///
    /// # Errors
    ///
    /// Fails if the report can't be turned into JSON or a write fails for any reason but a
    /// closed pipe.
    pub(crate) fn artifact<R: Report>(
        &mut self,
        report: &R,
        raw: &str,
        ok: bool,
    ) -> Result<(), OutputError> {
        match self.settings.format {
            OutputFormat::Text | OutputFormat::Github => self.write(Stream::Stdout, raw.as_bytes()),
            OutputFormat::Json | OutputFormat::Jsonl => self.report(report, ok),
        }
    }

    /// Writes a finding about the run itself to stderr. Quiet hides everything but errors.
    ///
    /// # Errors
    ///
    /// Fails if a write fails for any reason but a closed pipe.
    #[cfg(test)]
    pub(crate) fn diag(&mut self, finding: &Finding) -> Result<(), OutputError> {
        if !self.settings.verbosity.shows(finding.severity) {
            return Ok(());
        }
        let mut bytes = Vec::new();
        if self.settings.format.is_json() {
            serde_json::to_writer(&mut bytes, &FindingJson::from(finding))?;
            bytes.push(b'\n');
        } else if self.settings.format == OutputFormat::Github {
            writeln!(
                bytes,
                "{}",
                github::annotation(&self.settings.paths, finding)
            )?;
        } else {
            text::render_finding(&self.settings.text_context(), finding, &mut bytes)?;
        }
        self.write(Stream::Stderr, &bytes)
    }

    /// Writes a closing note about the run, like `Nothing written. Run the same command with
    /// --yes to apply.`, to stderr. Quiet and the JSON formats hide it.
    ///
    /// # Errors
    ///
    /// Fails if a write fails for any reason but a closed pipe.
    pub(crate) fn note(&mut self, note: &str) -> Result<(), OutputError> {
        if !self.settings.verbosity.shows_summary() || self.settings.format.is_json() {
            return Ok(());
        }
        let note = if self.settings.format == OutputFormat::Github {
            github::escape_data(&text::visible(note))
        } else {
            text::visible(note)
        };
        self.write(Stream::Stderr, format!("{note}\n").as_bytes())
    }

    /// Writes a fatal error to stderr: a JSON object under the JSON formats, otherwise the same
    /// text as any finding. Shown even under quiet.
    ///
    /// # Errors
    ///
    /// Fails if a write fails for any reason but a closed pipe.
    pub(crate) fn fatal(&mut self, finding: &Finding, exit_code: u8) -> Result<(), OutputError> {
        let mut bytes = Vec::new();
        if self.settings.format.is_json() {
            serde_json::to_writer(&mut bytes, &FatalJson::new(finding, exit_code))?;
            bytes.push(b'\n');
        } else if self.settings.format == OutputFormat::Github {
            writeln!(
                bytes,
                "{}",
                github::annotation(&self.settings.paths, finding)
            )?;
        } else {
            let context = TextContext {
                verbosity: Verbosity::Normal,
                ..self.settings.text_context()
            };
            text::render_finding(&context, finding, &mut bytes)?;
        }
        self.write(Stream::Stderr, &bytes)
    }

    /// Writes `bytes` to `stream`, turning a closed pipe into a closed stream.
    ///
    /// # Errors
    ///
    /// Fails if the write fails for any reason but a closed pipe.
    fn write(&mut self, stream: Stream, bytes: &[u8]) -> Result<(), OutputError> {
        if self.holds_writes() {
            process::await_termination(self.terminating);
        }
        let (target, closed) = match stream {
            Stream::Stdout => (&mut self.stdout, &mut self.stdout_closed),
            Stream::Stderr => (&mut self.stderr, &mut self.stderr_closed),
        };
        if *closed {
            return Ok(());
        }
        match target.write_all(bytes).and_then(|()| target.flush()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                *closed = true;
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }
}

/// Which of the two output streams to write to.
#[derive(Debug, Clone, Copy)]
enum Stream {
    /// Data: what the command was asked for.
    Stdout,
    /// Everything about the run itself.
    Stderr,
}
