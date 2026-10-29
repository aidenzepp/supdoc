//! Contract tests for `Config::load` / `Config::load_file`, against real files in a temp dir.

use std::fs;
use std::path::Path;

use tempfile::TempDir;

use super::*;
use crate::path::RelativePath;

/// Writes `files` (relative path, contents) under a fresh temp dir.
fn tree(files: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().expect("a temp dir should be creatable");
    for (relative, contents) in files {
        let path = dir.path().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("the fixture directory should be creatable");
        }
        fs::write(&path, contents).expect("the fixture file should be writable");
    }
    dir
}

/// `relative` beneath the temp dir, as an absolute path.
fn at(dir: &TempDir, relative: &str) -> AbsolutePath {
    AbsolutePath::new(&dir.path().join(relative)).expect("a temp dir path should be absolute")
}

/// A namespace name that the fixture knows is valid.
fn name(raw: &str) -> NamespaceName {
    raw.parse().expect("the fixture name should be valid")
}

/// Loads the config in `dir/relative_dir`, searching upward from there.
fn load_from(dir: &TempDir, relative_dir: &str) -> Config {
    Config::load(&at(dir, relative_dir)).expect("the fixture config should load")
}

/// The error from loading the config at `dir/file`.
fn load_error(dir: &TempDir, file: &str) -> ConfigError {
    Config::load_file(&at(dir, file)).expect_err("the fixture config should be rejected")
}

/// The text a parse error's span points at.
fn parse_span_text<'a>(error: &ConfigError, text: &'a str) -> &'a str {
    let ConfigError::Parse { error, .. } = error else {
        panic!("expected a parse error, got {error}");
    };
    let span = error.span().expect("a parse error should carry a span");
    text.get(span).expect("the span should lie inside the file")
}

// Medium: Config::load against real files

#[test]
fn empty_config_scans_everything_under_root() {
    let dir = tree(&[("supdoc.config.toml", "")]);
    let config = load_from(&dir, ".");

    assert_eq!(config.root(), &at(&dir, "."));
    assert_eq!(config.files(), [at(&dir, "supdoc.config.toml")]);
    assert!(config.scans(&at(&dir, "docs/a.md")));
    assert!(config.scans(&at(&dir, "README.md")));
    assert!(!config.scans(&AbsolutePath::new(Path::new("/elsewhere/a.md")).expect("absolute")));
}

#[test]
fn no_config_anywhere_uses_defaults_rooted_at_start() {
    let dir = tree(&[("docs/a.md", "")]);
    let config = load_from(&dir, "docs");

    assert_eq!(config.root(), &at(&dir, "docs"));
    assert_eq!(config.files(), []);
    assert!(config.scans(&at(&dir, "docs/a.md")));
    assert!(config.namespaces().is_empty());
}

#[test]
fn nearest_config_wins_when_walking_up() {
    let dir = tree(&[
        ("supdoc.config.toml", r#"include = ["outer"]"#),
        ("apps/web/supdoc.config.toml", r#"include = ["inner"]"#),
        ("apps/web/docs/a.md", ""),
    ]);
    let config = load_from(&dir, "apps/web/docs");

    assert_eq!(config.root(), &at(&dir, "apps/web"));
    assert!(config.scans(&at(&dir, "apps/web/inner/a.md")));
    assert!(!config.scans(&at(&dir, "outer/a.md")));
}

#[test]
fn load_file_with_missing_path_is_not_found_not_defaults() {
    let dir = tree(&[]);
    let error = load_error(&dir, "supdoc.config.toml");

    assert_eq!(error.code(), "config/not-found");
    assert!(
        matches!(&error, ConfigError::NotFound { path } if *path == at(&dir, "supdoc.config.toml")),
        "the error should name the requested file: {error:?}",
    );
}

#[test]
fn directory_pattern_matches_files_beneath_it() {
    for pattern in ["docs", "docs/"] {
        let dir = tree(&[("supdoc.config.toml", &format!(r#"include = ["{pattern}"]"#))]);
        let config = load_from(&dir, ".");

        assert!(config.scans(&at(&dir, "docs/a.md")), "`{pattern}`");
        assert!(config.scans(&at(&dir, "docs/x/b.md")), "`{pattern}`");
    }
}

#[test]
fn directory_pattern_does_not_match_sibling_with_same_prefix() {
    let dir = tree(&[("supdoc.config.toml", r#"include = ["docs"]"#)]);
    let config = load_from(&dir, ".");

    assert!(!config.scans(&at(&dir, "docsy/a.md")));
}

#[test]
fn single_star_does_not_cross_directories() {
    let dir = tree(&[("supdoc.config.toml", r#"include = ["docs/*.md"]"#)]);
    let config = load_from(&dir, ".");

    assert!(config.scans(&at(&dir, "docs/a.md")));
    assert!(!config.scans(&at(&dir, "docs/x/b.md")));
}

#[test]
fn double_star_crosses_directories_and_root_files_need_exact_names() {
    let dir = tree(&[(
        "supdoc.config.toml",
        r#"include = ["docs/**/*.md", "README.md", "**/NOTES.md"]"#,
    )]);
    let config = load_from(&dir, ".");

    assert!(config.scans(&at(&dir, "docs/a.md")));
    assert!(config.scans(&at(&dir, "docs/x/b.md")));
    assert!(!config.scans(&at(&dir, "docs/a.txt")));
    assert!(config.scans(&at(&dir, "README.md")));
    assert!(!config.scans(&at(&dir, "x/README.md")));
    assert!(config.scans(&at(&dir, "NOTES.md")));
    assert!(config.scans(&at(&dir, "x/NOTES.md")));
}

#[test]
fn pattern_in_bracketed_directory_matches_literally() {
    let dir = tree(&[("[id]/supdoc.config.toml", r#"include = ["docs"]"#)]);
    let config = load_from(&dir, "[id]");

    assert!(config.scans(&at(&dir, "[id]/docs/a.md")));
    assert!(!config.scans(&at(&dir, "[id]/other/a.md")));
}

#[test]
fn exclude_wins_over_include() {
    let dir = tree(&[(
        "supdoc.config.toml",
        r#"
include = ["docs"]
exclude = ["docs/archive"]
"#,
    )]);
    let config = load_from(&dir, ".");

    assert!(config.scans(&at(&dir, "docs/a.md")));
    assert!(!config.scans(&at(&dir, "docs/archive/old.md")));
}

#[test]
fn child_include_replaces_parent_include() {
    let dir = tree(&[
        ("supdoc.config.toml", r#"include = ["parent"]"#),
        (
            "apps/web/supdoc.config.toml",
            "extends = \"../../supdoc.config.toml\"\ninclude = [\"child\"]",
        ),
    ]);
    let config = load_from(&dir, "apps/web");

    assert!(config.scans(&at(&dir, "apps/web/child/a.md")));
    assert!(!config.scans(&at(&dir, "parent/a.md")));
    assert!(!config.scans(&at(&dir, "apps/web/parent/a.md")));
}

#[test]
fn unset_child_key_inherits_parent_value() {
    let dir = tree(&[
        ("supdoc.config.toml", r#"exclude = ["skip"]"#),
        (
            "supdoc.child.toml",
            "extends = \"supdoc.config.toml\"\ninclude = [\"docs\"]",
        ),
    ]);
    let config = Config::load_file(&at(&dir, "supdoc.child.toml")).expect("the chain should load");

    assert!(config.scans(&at(&dir, "docs/a.md")));
    assert!(!config.scans(&at(&dir, "skip/a.md")));
    assert_eq!(
        config.files(),
        [
            at(&dir, "supdoc.child.toml"),
            at(&dir, "supdoc.config.toml")
        ],
    );
}

#[test]
fn empty_child_include_scans_nothing() {
    let dir = tree(&[
        ("supdoc.config.toml", r#"include = ["docs"]"#),
        (
            "supdoc.child.toml",
            "extends = \"supdoc.config.toml\"\ninclude = []",
        ),
    ]);
    let config = Config::load_file(&at(&dir, "supdoc.child.toml")).expect("the chain should load");

    assert!(!config.scans(&at(&dir, "docs/a.md")));
    assert!(!config.scans(&at(&dir, "anything.md")));
}

#[test]
fn child_namespace_overrides_parent_namespace() {
    let dir = tree(&[
        (
            "supdoc.config.toml",
            "[namespaces]\nauth = \"parent/auth\"\nweb = \"parent/web\"",
        ),
        (
            "supdoc.child.toml",
            "extends = \"supdoc.config.toml\"\n[namespaces]\nauth = \"child/auth\"",
        ),
    ]);
    let config = Config::load_file(&at(&dir, "supdoc.child.toml")).expect("the chain should load");

    assert_eq!(
        config.namespace(&name("auth")),
        Some(&at(&dir, "child/auth"))
    );
    assert_eq!(
        config.namespace(&name("web")),
        Some(&at(&dir, "parent/web"))
    );
    assert_eq!(config.namespace(&name("other")), None);
}

#[test]
fn inherited_namespace_resolves_from_defining_config() {
    let dir = tree(&[
        (
            "supdoc.config.toml",
            "[namespaces]\nauth = \"services/auth\"",
        ),
        (
            "apps/web/supdoc.config.toml",
            "extends = \"../../supdoc.config.toml\"\ninclude = [\"docs\"]",
        ),
    ]);
    let config = load_from(&dir, "apps/web");

    assert_eq!(config.root(), &at(&dir, "apps/web"));
    assert_eq!(
        config.namespace(&name("auth")),
        Some(&at(&dir, "services/auth"))
    );
    assert!(config.scans(&at(&dir, "apps/web/docs/guide.md")));
}

#[test]
fn inherited_include_resolves_from_defining_config() {
    let dir = tree(&[
        ("supdoc.config.toml", r#"include = ["docs"]"#),
        (
            "apps/web/supdoc.config.toml",
            r#"extends = "../../supdoc.config.toml""#,
        ),
    ]);
    let config = load_from(&dir, "apps/web");

    assert!(config.scans(&at(&dir, "docs/a.md")));
    assert!(!config.scans(&at(&dir, "apps/web/docs/a.md")));
}

#[test]
fn missing_namespace_directory_still_loads() {
    let dir = tree(&[("supdoc.config.toml", "[namespaces]\nghost = \"nowhere\"")]);
    let config = load_from(&dir, ".");

    assert_eq!(config.namespace(&name("ghost")), Some(&at(&dir, "nowhere")));
}

// Medium: errors, checked by code() and facts, not exact wording

#[test]
fn unknown_key_is_parse_error_naming_key_and_valid_keys() {
    let text = "exlude = [\"x\"]\n";
    let dir = tree(&[("supdoc.config.toml", text)]);
    let error = load_error(&dir, "supdoc.config.toml");
    let message = error.to_string();

    assert_eq!(error.code(), "config/parse");
    assert!(message.contains("exlude"), "{message}");
    assert!(message.contains("exclude"), "{message}");
    assert_eq!(parse_span_text(&error, text), "exlude");
    assert_eq!(
        error.help().as_deref(),
        Some("Rename or remove the unknown key")
    );
}

#[test]
fn invalid_glob_is_parse_error_with_line() {
    let text = "include = [\"ok\"]\nexclude = [\"docs/[\"]\n";
    let dir = tree(&[("supdoc.config.toml", text)]);
    let error = load_error(&dir, "supdoc.config.toml");

    assert_eq!(error.code(), "config/parse");
    assert!(error.to_string().contains("docs/["), "{error}");
    assert!(parse_span_text(&error, text).contains("docs/["));
    assert_eq!(error.help(), None, "the glob message is already the fix");
}

#[test]
fn pattern_with_parent_segment_is_rejected() {
    let text = "include = [\"../shared\"]\n";
    let dir = tree(&[("supdoc.config.toml", text)]);
    let error = load_error(&dir, "supdoc.config.toml");

    assert_eq!(error.code(), "config/parse");
    assert!(error.to_string().contains("../shared"), "{error}");
    assert!(error.help().is_some());
}

#[test]
fn pattern_with_leading_slash_is_rejected() {
    let dir = tree(&[("supdoc.config.toml", "include = [\"/docs\"]\n")]);
    let error = load_error(&dir, "supdoc.config.toml");

    assert_eq!(error.code(), "config/parse");
    assert!(error.to_string().contains("/docs"), "{error}");
}

#[test]
fn single_letter_namespace_is_parse_error_with_line() {
    let text = "[namespaces]\nc = \"src\"\n";
    let dir = tree(&[("supdoc.config.toml", text)]);
    let error = load_error(&dir, "supdoc.config.toml");

    assert_eq!(error.code(), "config/parse");
    assert_eq!(parse_span_text(&error, text), "c");
    assert!(error.help().is_some());
}

#[test]
fn absolute_namespace_path_is_parse_error() {
    let dir = tree(&[("supdoc.config.toml", "[namespaces]\nauth = \"/srv/auth\"\n")]);
    let error = load_error(&dir, "supdoc.config.toml");

    assert_eq!(error.code(), "config/parse");
    assert!(error.to_string().contains("/srv/auth"), "{error}");
    assert!(error.help().is_some());
}

#[test]
fn malformed_toml_is_parse_error_without_help() {
    let dir = tree(&[("supdoc.config.toml", "include = [\n")]);
    let error = load_error(&dir, "supdoc.config.toml");

    assert_eq!(error.code(), "config/parse");
    assert_eq!(error.exit_code(), 78);
}

#[test]
fn extends_missing_names_file_value_and_resolved_path() {
    let text = "extends = \"../../supdoc.confg.toml\"\n";
    let dir = tree(&[("apps/web/supdoc.config.toml", text)]);
    let error = load_error(&dir, "apps/web/supdoc.config.toml");

    assert_eq!(error.code(), "config/extends-missing");
    let ConfigError::ExtendsMissing {
        file,
        extends,
        span,
        resolved,
    } = &error
    else {
        panic!("expected extends-missing, got {error:?}");
    };
    assert_eq!(file, &at(&dir, "apps/web/supdoc.config.toml"));
    assert_eq!(extends.as_path(), Path::new("../../supdoc.confg.toml"));
    assert_eq!(resolved, &at(&dir, "supdoc.confg.toml"));
    assert_eq!(text.get(span.clone()), Some("\"../../supdoc.confg.toml\""));
}

#[test]
fn extends_directory_suggests_config_inside_it() {
    let dir = tree(&[
        ("supdoc.config.toml", ""),
        ("apps/web/supdoc.config.toml", "extends = \"../..\"\n"),
    ]);
    let error = load_error(&dir, "apps/web/supdoc.config.toml");

    assert_eq!(error.code(), "config/extends-directory");
    let ConfigError::ExtendsDirectory { suggestion, .. } = &error else {
        panic!("expected extends-directory, got {error:?}");
    };
    assert_eq!(
        suggestion.as_ref().map(RelativePath::as_path),
        Some(Path::new("../../supdoc.config.toml")),
    );
    assert_eq!(
        error.help().as_deref(),
        Some("Use `extends = \"../../supdoc.config.toml\"`"),
    );
}

#[test]
fn extends_directory_without_config_has_no_suggestion() {
    let dir = tree(&[("apps/web/supdoc.config.toml", "extends = \"../..\"\n")]);
    let error = load_error(&dir, "apps/web/supdoc.config.toml");

    let ConfigError::ExtendsDirectory { suggestion, .. } = &error else {
        panic!("expected extends-directory, got {error:?}");
    };
    assert_eq!(suggestion, &None);
    assert!(
        !error
            .help()
            .unwrap_or_default()
            .contains("supdoc.config.toml\"`"),
        "no made-up fix: {:?}",
        error.help(),
    );
}

#[test]
fn extends_loop_reports_every_file_in_order() {
    let dir = tree(&[
        (
            "a/supdoc.config.toml",
            "extends = \"../b/supdoc.config.toml\"\n",
        ),
        (
            "b/supdoc.config.toml",
            "extends = \"../c/supdoc.config.toml\"\n",
        ),
        (
            "c/supdoc.config.toml",
            "extends = \"../a/supdoc.config.toml\"\n",
        ),
    ]);
    let error = load_error(&dir, "a/supdoc.config.toml");

    assert_eq!(error.code(), "config/extends-loop");
    let ConfigError::ExtendsLoop { chain, .. } = &error else {
        panic!("expected extends-loop, got {error:?}");
    };
    assert_eq!(
        chain,
        &[
            at(&dir, "a/supdoc.config.toml"),
            at(&dir, "b/supdoc.config.toml"),
            at(&dir, "c/supdoc.config.toml"),
            at(&dir, "a/supdoc.config.toml"),
        ],
    );
}

#[test]
fn extends_loop_that_does_not_include_the_start_reports_only_the_loop() {
    let dir = tree(&[
        ("start.toml", "extends = \"b.toml\"\n"),
        ("b.toml", "extends = \"c.toml\"\n"),
        ("c.toml", "extends = \"b.toml\"\n"),
    ]);
    let error = load_error(&dir, "start.toml");

    let ConfigError::ExtendsLoop { chain, .. } = &error else {
        panic!("expected extends-loop, got {error:?}");
    };
    assert_eq!(
        chain,
        &[at(&dir, "b.toml"), at(&dir, "c.toml"), at(&dir, "b.toml")]
    );
}

#[test]
fn file_extending_itself_is_a_loop() {
    let dir = tree(&[("supdoc.config.toml", "extends = \"supdoc.config.toml\"\n")]);
    let error = load_error(&dir, "supdoc.config.toml");

    assert_eq!(error.code(), "config/extends-loop");
}

#[cfg(unix)]
#[test]
fn extends_loop_through_a_symlink_is_caught() {
    let dir = tree(&[("real.toml", "extends = \"link.toml\"\n")]);
    std::os::unix::fs::symlink(dir.path().join("real.toml"), dir.path().join("link.toml"))
        .expect("the symlink should be creatable");
    let error = load_error(&dir, "real.toml");

    assert_eq!(error.code(), "config/extends-loop");
}

#[cfg(unix)]
#[test]
fn unreadable_config_is_read_error() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tree(&[("supdoc.config.toml", "")]);
    let path = dir.path().join("supdoc.config.toml");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000))
        .expect("the fixture permissions should be settable");
    // A privileged user reads the file anyway, which would make this test vacuous.
    if fs::read_to_string(&path).is_ok() {
        return;
    }
    let error = load_error(&dir, "supdoc.config.toml");

    assert_eq!(error.code(), "config/read");
    assert_eq!(error.exit_code(), 74);
}

#[test]
fn every_error_has_a_code_and_message_names_a_file() {
    let file = at(&tree(&[]), "supdoc.config.toml");
    let relative = RelativePath::try_from("../x.toml".to_owned()).expect("relative");
    let parse_error =
        toml::from_str::<toml::Table>("= bad").expect_err("the fixture TOML should be malformed");
    let errors = [
        ConfigError::NotFound { path: file.clone() },
        ConfigError::Read {
            path: file.clone(),
            error: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        },
        ConfigError::Parse {
            path: file.clone(),
            error: parse_error,
        },
        ConfigError::ExtendsMissing {
            file: file.clone(),
            extends: relative.clone(),
            span: 0..1,
            resolved: file.clone(),
        },
        ConfigError::ExtendsDirectory {
            file: file.clone(),
            extends: relative,
            span: 0..1,
            suggestion: None,
        },
        ConfigError::ExtendsLoop {
            chain: vec![file.clone(), file.clone()],
            span: 0..1,
        },
    ];
    let mut codes = Vec::new();
    for error in &errors {
        assert!(error.code().starts_with("config/"), "{error:?}");
        assert!(
            error.to_string().contains(&file.display().to_string()),
            "message should name the file: {error}",
        );
        codes.push(error.code());
    }
    codes.sort_unstable();
    codes.dedup();
    assert_eq!(codes.len(), errors.len(), "codes must be unique");
}

// Provenance: which file defined each key

/// A two-level chain: `child` extends `mid` extends `root`. `root` sets include, exclude and
/// namespaces `shared` and `deep`; `mid` overrides `exclude` and `shared`; `child` sets only `own`.
fn provenance_chain() -> TempDir {
    tree(&[
        (
            "root.toml",
            "include = [\"docs\", \"notes/**\"]\nexclude = [\"old\"]\n[namespaces]\nshared = \"from-root\"\ndeep = \"deep\"\n",
        ),
        (
            "mid/mid.toml",
            "extends = \"../root.toml\"\nexclude = [\"drafts\", \"tmp/\"]\n[namespaces]\nshared = \"from-mid\"\n",
        ),
        (
            "child/supdoc.config.toml",
            "extends = \"../mid/mid.toml\"\n[namespaces]\nown = \"own\"\n",
        ),
    ])
}

#[test]
fn include_definition_names_the_grandparent_that_set_it_with_raw_patterns() {
    let dir = provenance_chain();
    let config = load_from(&dir, "child");

    let definition = config
        .include_definition()
        .expect("a file in the chain sets include");
    assert_eq!(definition.patterns(), ["docs", "notes/**"]);
    assert_eq!(definition.file(), &at(&dir, "root.toml"));
}

#[test]
fn exclude_definition_names_the_nearest_file_that_set_it_with_raw_patterns() {
    let dir = provenance_chain();
    let config = load_from(&dir, "child");

    let definition = config
        .exclude_definition()
        .expect("a file in the chain sets exclude");
    assert_eq!(definition.patterns(), ["drafts", "tmp/"]);
    assert_eq!(definition.file(), &at(&dir, "mid/mid.toml"));
}

#[test]
fn namespace_defined_in_names_the_nearest_file_defining_each_namespace() {
    let dir = provenance_chain();
    let config = load_from(&dir, "child");

    let defined_in = |raw: &str| config.namespace_defined_in(&name(raw)).cloned();
    assert_eq!(
        defined_in("own"),
        Some(at(&dir, "child/supdoc.config.toml"))
    );
    assert_eq!(defined_in("shared"), Some(at(&dir, "mid/mid.toml")));
    assert_eq!(defined_in("deep"), Some(at(&dir, "root.toml")));
    assert_eq!(defined_in("absent"), None);
}

#[test]
fn unset_keys_have_no_definition() {
    let dir = tree(&[("supdoc.config.toml", "")]);
    let config = load_from(&dir, ".");

    assert_eq!(config.include_definition(), None);
    assert_eq!(config.exclude_definition(), None);

    let defaults = load_from(&tree(&[]), ".");
    assert_eq!(defaults.include_definition(), None);
}
