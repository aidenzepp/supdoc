//! `Output`: the one value that writes to stdout and stderr (06 "main and the output module").

use std::io::{self, Write};

use crate::finding::Finding;
use crate::output::format::{OutputFormat, TextContext, Verbosity};
use crate::output::json::{Envelope, FatalJson, FindingJson};
use crate::output::paths::PathStyle;
use crate::output::report::Report;
use crate::output::{github, jsonl, text};

/// What `main` decides about output before any command runs.
#[derive(Debug, Clone)]
pub struct OutputSettings {
    /// The format data is written in.
    pub format: OutputFormat,
    /// How much to say.
    pub verbosity: Verbosity,
    /// Whether text output carries ANSI color.
    pub color: bool,
    /// Whether text output draws with Unicode instead of ASCII (07 G12).
    pub unicode: bool,
    /// The terminal width in columns.
    pub width: usize,
    /// How paths are shown.
    pub paths: PathStyle,
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
pub enum OutputError {
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
/// marks the output closed, and everything after it is dropped quietly (06 X13). Callers can ask
/// [`Output::is_closed`] and exit 0.
pub struct Output {
    /// How to write.
    settings: OutputSettings,
    /// Where data goes.
    stdout: Box<dyn Write>,
    /// Where notes about the run, diagnostics and fatal errors go.
    stderr: Box<dyn Write>,
    /// Whether a reader hung up.
    closed: bool,
}

impl std::fmt::Debug for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Output")
            .field("settings", &self.settings)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl Output {
    /// Output that writes to `stdout` and `stderr`.
    #[must_use]
    pub fn new(settings: OutputSettings, stdout: Box<dyn Write>, stderr: Box<dyn Write>) -> Self {
        Self {
            settings,
            stdout,
            stderr,
            closed: false,
        }
    }

    /// Output on the process's own stdout and stderr, with ANSI codes stripped wherever
    /// `choice` and the terminal say color is off.
    #[must_use]
    pub fn stdio(settings: OutputSettings, choice: anstream::ColorChoice) -> Self {
        Self::new(
            settings,
            Box::new(anstream::AutoStream::new(io::stdout(), choice)),
            Box::new(anstream::AutoStream::new(io::stderr(), choice)),
        )
    }

    /// Whether a reader closed the pipe. The caller should exit 0.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Writes `report` to stdout in the chosen format. `ok` must be true exactly when the process
    /// will exit 0.
    ///
    /// # Errors
    ///
    /// Fails if the report can't be turned into JSON or a write fails for any reason but a
    /// closed pipe.
    pub fn report<R: Report>(&mut self, report: &R, ok: bool) -> Result<(), OutputError> {
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
                    writeln!(bytes, "{}", summary.line(false))?;
                }
            }
        }
        self.write(Stream::Stdout, &bytes)
    }

    /// Writes a finding about the run itself to stderr. Quiet hides everything but errors.
    ///
    /// # Errors
    ///
    /// Fails if a write fails for any reason but a closed pipe.
    pub fn diag(&mut self, finding: &Finding) -> Result<(), OutputError> {
        if !self.settings.verbosity.shows(finding.severity) {
            return Ok(());
        }
        let mut bytes = Vec::new();
        if self.settings.format.is_json() {
            serde_json::to_writer(&mut bytes, &FindingJson::from(finding))?;
            bytes.push(b'\n');
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
    pub fn note(&mut self, note: &str) -> Result<(), OutputError> {
        if !self.settings.verbosity.shows_summary() || self.settings.format.is_json() {
            return Ok(());
        }
        self.write(Stream::Stderr, format!("{note}\n").as_bytes())
    }

    /// Writes a fatal error to stderr: a JSON object under the JSON formats, otherwise the same
    /// text as any finding. Shown even under quiet.
    ///
    /// # Errors
    ///
    /// Fails if a write fails for any reason but a closed pipe.
    pub fn fatal(&mut self, finding: &Finding, exit_code: u8) -> Result<(), OutputError> {
        let mut bytes = Vec::new();
        if self.settings.format.is_json() {
            serde_json::to_writer(&mut bytes, &FatalJson::new(finding, exit_code))?;
            bytes.push(b'\n');
        } else {
            let context = TextContext {
                verbosity: Verbosity::Normal,
                ..self.settings.text_context()
            };
            text::render_finding(&context, finding, &mut bytes)?;
        }
        self.write(Stream::Stderr, &bytes)
    }

    /// Writes `bytes` to `stream`, turning a closed pipe into a closed output.
    ///
    /// # Errors
    ///
    /// Fails if the write fails for any reason but a closed pipe.
    fn write(&mut self, stream: Stream, bytes: &[u8]) -> Result<(), OutputError> {
        if self.closed {
            return Ok(());
        }
        let target = match stream {
            Stream::Stdout => &mut self.stdout,
            Stream::Stderr => &mut self.stderr,
        };
        match target.write_all(bytes).and_then(|()| target.flush()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                self.closed = true;
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
