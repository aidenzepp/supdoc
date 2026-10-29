//! The check report: every link in scope with its state, plus findings not tied to one link (05).

use crate::finding::{Finding, Location};
use crate::link::LinkKey;
use crate::state::LinkState;

/// The result of comparing a scan with the lockfile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// One per [`LinkKey`] in scope, sorted.
    pub links: Vec<LinkReport>,
    /// Findings not tied to one link: from the scan, plus moved-link notes and syntax-error
    /// warnings.
    pub findings: Vec<Finding>,
}

/// The JSON shape of a [`Report`]: `summary` is computed from `links` (07 G8).
#[derive(serde::Serialize, schemars::JsonSchema)]
struct ReportView<'a> {
    /// How many links are in each state.
    summary: Counts,
    /// One per link in scope.
    links: &'a [LinkReport],
    /// Findings not tied to one link.
    findings: &'a [Finding],
}

impl serde::Serialize for Report {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ReportView {
            summary: self.counts(),
            links: &self.links,
            findings: &self.findings,
        }
        .serialize(serializer)
    }
}

impl schemars::JsonSchema for Report {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Report".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        <ReportView<'static>>::json_schema(generator)
    }
}

impl Report {
    /// How many links are in each state. Always computed from `links`, so it can't disagree with
    /// them.
    #[must_use]
    pub fn counts(&self) -> Counts {
        self.links.iter().map(|link| link.state).collect()
    }
}

/// One link and its state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct LinkReport {
    /// Which link. Serialized as `source`, `target` and `symbol` beside the other fields.
    #[serde(flatten)]
    pub key: LinkKey,
    /// Its state.
    pub state: LinkState,
    /// Every place it appears. Empty only for `obsolete`, since that link no longer exists.
    pub locations: Vec<Location>,
    /// The finding that explains this state (none for `passed`, and then left out of JSON).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finding: Option<Finding>,
    /// Set when a hash was carried from an old key (05 C6).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moved_from: Option<LinkKey>,
}

/// How many links are in each state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct Counts {
    /// Links whose hash matches.
    pub passed: usize,
    /// Links whose code changed.
    pub failed: usize,
    /// Links whose target can't be found.
    pub broken: usize,
    /// Links without a lock entry.
    pub new: usize,
    /// Lock entries without a link.
    pub obsolete: usize,
}

impl Counts {
    /// The count for one state.
    #[must_use]
    pub const fn get(&self, state: LinkState) -> usize {
        match state {
            LinkState::Passed => self.passed,
            LinkState::Failed => self.failed,
            LinkState::Broken => self.broken,
            LinkState::New => self.new,
            LinkState::Obsolete => self.obsolete,
        }
    }

    /// The number of links counted.
    #[must_use]
    pub const fn total(&self) -> usize {
        self.passed + self.failed + self.broken + self.new + self.obsolete
    }
}

impl FromIterator<LinkState> for Counts {
    fn from_iter<I: IntoIterator<Item = LinkState>>(states: I) -> Self {
        let mut counts = Self::default();
        for state in states {
            let count = match state {
                LinkState::Passed => &mut counts.passed,
                LinkState::Failed => &mut counts.failed,
                LinkState::Broken => &mut counts.broken,
                LinkState::New => &mut counts.new,
                LinkState::Obsolete => &mut counts.obsolete,
            };
            *count += 1;
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::{Fix, Severity};
    use crate::link::SourcePath;

    fn link(source: &str, state: LinkState) -> LinkReport {
        LinkReport {
            key: LinkKey {
                source: source.parse().expect("the fixture source should be valid"),
                target: "src/lib.rs"
                    .parse()
                    .expect("the fixture target should be valid"),
                symbol: None,
            },
            state,
            locations: Vec::new(),
            finding: None,
            moved_from: None,
        }
    }

    #[test]
    fn counts_tally_each_state_from_the_links() {
        let report = Report {
            links: vec![
                link("a.md", LinkState::Passed),
                link("b.md", LinkState::Passed),
                link("c.md", LinkState::Failed),
                link("d.md", LinkState::Obsolete),
            ],
            findings: Vec::new(),
        };

        let counts = report.counts();

        assert_eq!(
            LinkState::ALL.map(|state| counts.get(state)),
            [2, 1, 0, 0, 1],
            "counts in order passed, failed, broken, new, obsolete",
        );
        assert_eq!(counts.total(), report.links.len());
    }

    /// A report shaped like the `check --json` example in 07.
    fn sample_report() -> Report {
        let source: SourcePath = "docs/api.md"
            .parse()
            .expect("the fixture source should be valid");
        let location = Location {
            file: source.clone(),
            range: 1022..1051,
            line: 40,
            column: 3,
        };
        let fix = Fix {
            file: source,
            range: 1046..1051,
            replacement: "log_in".to_owned(),
        };
        let finding = Finding::new("resolve/not-found", Severity::Error, "no symbol `login`")
            .with_location(location.clone())
            .with_help("did you mean `log_in`?")
            .with_fix(fix);
        let mut broken = link("docs/api.md", LinkState::Broken);
        broken.key.symbol = Some("login".parse().expect("the fixture symbol should be valid"));
        broken.locations = vec![location];
        broken.finding = Some(finding);
        Report {
            links: vec![link("docs/a.md", LinkState::Passed), broken],
            findings: vec![Finding::new(
                "scan/unreadable-dir",
                Severity::Warning,
                "skipped /repo/private",
            )],
        }
    }

    #[test]
    fn report_serializes_to_the_documented_json_shape() {
        let json = serde_json::to_value(sample_report()).expect("a report should serialize");

        assert_eq!(
            json,
            serde_json::json!({
                "summary": { "passed": 1, "failed": 0, "broken": 1, "new": 0, "obsolete": 0 },
                "links": [
                    {
                        "source": "docs/a.md", "target": "src/lib.rs",
                        "state": "passed", "locations": [],
                    },
                    {
                        "source": "docs/api.md", "target": "src/lib.rs", "symbol": "login",
                        "state": "broken",
                        "locations": [{
                            "file": "docs/api.md", "start": 1022, "end": 1051,
                            "line": 40, "column": 3,
                        }],
                        "finding": {
                            "kind": "resolve/not-found", "severity": "error",
                            "message": "no symbol `login`",
                            "location": {
                                "file": "docs/api.md", "start": 1022, "end": 1051,
                                "line": 40, "column": 3,
                            },
                            "help": "did you mean `log_in`?",
                            "fix": {
                                "file": "docs/api.md", "start": 1046, "end": 1051,
                                "replacement": "log_in",
                            },
                        },
                    },
                ],
                "findings": [{
                    "kind": "scan/unreadable-dir", "severity": "warning",
                    "message": "skipped /repo/private",
                }],
            }),
        );
    }

    #[test]
    fn serialized_report_validates_against_its_schema() {
        let schema =
            serde_json::to_value(schemars::schema_for!(Report)).expect("a schema should serialize");
        let validator = jsonschema::validator_for(&schema)
            .expect("the generated schema should be a valid JSON Schema");
        let instance = serde_json::to_value(sample_report()).expect("a report should serialize");

        let errors: Vec<String> = validator
            .iter_errors(&instance)
            .map(|error| error.to_string())
            .collect();

        assert!(errors.is_empty(), "schema violations: {errors:?}");
        assert!(
            validator.is_valid(&instance),
            "the sample report should be valid"
        );
    }

    #[test]
    fn schema_rejects_a_report_with_a_wrong_state_name() {
        let schema =
            serde_json::to_value(schemars::schema_for!(Report)).expect("a schema should serialize");
        let validator = jsonschema::validator_for(&schema)
            .expect("the generated schema should be a valid JSON Schema");
        let instance = serde_json::json!({
            "summary": { "passed": 1, "failed": 0, "broken": 0, "new": 0, "obsolete": 0 },
            "links": [{
                "source": "docs/a.md", "target": "src/lib.rs",
                "state": "Passed", "locations": [],
            }],
            "findings": [],
        });

        assert!(
            !validator.is_valid(&instance),
            "an uppercase state should not validate"
        );
    }

    #[test]
    fn empty_report_counts_zero() {
        assert_eq!(Report::default().counts(), Counts::default());
    }
}
