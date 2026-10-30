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

    assert_eq!(error.code(), "config-not-found");
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
            "[namespaces]\nauth = \"apps/web/services/auth\"",
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
        Some(&at(&dir, "apps/web/services/auth"))
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

    assert_eq!(error.code(), "config-parse");
    assert!(message.contains("exlude"), "{message}");
    assert!(message.contains("exclude"), "{message}");
    assert_eq!(parse_span_text(&error, text), "exlude");
    assert_eq!(
        error.help().as_deref(),
        Some("Remove `exlude`, or rename it to one of the keys the message lists")
    );
}

#[test]
fn invalid_glob_is_parse_error_with_line() {
    let text = "include = [\"ok\"]\nexclude = [\"docs/[\"]\n";
    let dir = tree(&[("supdoc.config.toml", text)]);
    let error = load_error(&dir, "supdoc.config.toml");

    assert_eq!(error.code(), "config-parse");
    assert!(error.to_string().contains("docs/["), "{error}");
    assert!(parse_span_text(&error, text).contains("docs/["));
    assert_eq!(error.help(), None, "the glob message is already the fix");
}

#[test]
fn pattern_with_parent_segment_is_rejected() {
    let text = "include = [\"../shared\"]\n";
    let dir = tree(&[("supdoc.config.toml", text)]);
    let error = load_error(&dir, "supdoc.config.toml");

    assert_eq!(error.code(), "config-parse");
    assert!(error.to_string().contains("../shared"), "{error}");
    assert!(error.help().is_some());
}

#[test]
fn pattern_with_leading_slash_is_rejected() {
    let dir = tree(&[("supdoc.config.toml", "include = [\"/docs\"]\n")]);
    let error = load_error(&dir, "supdoc.config.toml");

    assert_eq!(error.code(), "config-parse");
    assert!(error.to_string().contains("/docs"), "{error}");
}

#[test]
fn single_letter_namespace_is_parse_error_with_line() {
    let text = "[namespaces]\nc = \"src\"\n";
    let dir = tree(&[("supdoc.config.toml", text)]);
    let error = load_error(&dir, "supdoc.config.toml");

    assert_eq!(error.code(), "config-parse");
    assert_eq!(parse_span_text(&error, text), "c");
    assert!(error.help().is_some());
}

#[test]
fn absolute_namespace_path_is_parse_error() {
    let dir = tree(&[("supdoc.config.toml", "[namespaces]\nauth = \"/srv/auth\"\n")]);
    let error = load_error(&dir, "supdoc.config.toml");

    assert_eq!(error.code(), "config-parse");
    assert!(error.to_string().contains("/srv/auth"), "{error}");
    assert!(error.help().is_some());
}

#[test]
fn malformed_toml_is_parse_error_without_help() {
    let dir = tree(&[("supdoc.config.toml", "include = [\n")]);
    let error = load_error(&dir, "supdoc.config.toml");

    assert_eq!(error.code(), "config-parse");
    assert_eq!(error.exit_code(), 78);
}

#[test]
fn extends_missing_names_file_value_and_resolved_path() {
    let text = "extends = \"../../shared/supdoc.config.toml\"\n";
    let dir = tree(&[("apps/web/supdoc.config.toml", text)]);
    let error = load_error(&dir, "apps/web/supdoc.config.toml");

    assert_eq!(error.code(), "config-extends-missing");
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
    assert_eq!(
        extends.as_path(),
        Path::new("../../shared/supdoc.config.toml")
    );
    assert_eq!(resolved, &at(&dir, "shared/supdoc.config.toml"));
    assert_eq!(
        text.get(span.clone()),
        Some("\"../../shared/supdoc.config.toml\"")
    );
}

#[test]
fn extends_directory_suggests_config_inside_it() {
    let dir = tree(&[
        ("supdoc.config.toml", ""),
        ("apps/web/supdoc.config.toml", "extends = \"../..\"\n"),
    ]);
    let error = load_error(&dir, "apps/web/supdoc.config.toml");

    assert_eq!(error.code(), "config-extends-directory");
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

    assert_eq!(error.code(), "config-extends-loop");
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
        ("start.toml", "extends = \"b/supdoc.config.toml\"\n"),
        (
            "b/supdoc.config.toml",
            "extends = \"../c/supdoc.config.toml\"\n",
        ),
        (
            "c/supdoc.config.toml",
            "extends = \"../b/supdoc.config.toml\"\n",
        ),
    ]);
    let error = load_error(&dir, "start.toml");

    let ConfigError::ExtendsLoop { chain, .. } = &error else {
        panic!("expected extends-loop, got {error:?}");
    };
    assert_eq!(
        chain,
        &[
            at(&dir, "b/supdoc.config.toml"),
            at(&dir, "c/supdoc.config.toml"),
            at(&dir, "b/supdoc.config.toml"),
        ]
    );
}

#[test]
fn file_extending_itself_is_a_loop() {
    let dir = tree(&[("supdoc.config.toml", "extends = \"supdoc.config.toml\"\n")]);
    let error = load_error(&dir, "supdoc.config.toml");

    assert_eq!(error.code(), "config-extends-loop");
}

#[cfg(unix)]
#[test]
fn extends_loop_through_a_symlink_is_caught() {
    let dir = tree(&[(
        "real/supdoc.config.toml",
        "extends = \"../link/supdoc.config.toml\"\n",
    )]);
    fs::create_dir(dir.path().join("link")).expect("the fixture directory should be creatable");
    std::os::unix::fs::symlink(
        dir.path().join("real/supdoc.config.toml"),
        dir.path().join("link/supdoc.config.toml"),
    )
    .expect("the symlink should be creatable");
    let error = load_error(&dir, "real/supdoc.config.toml");

    assert_eq!(error.code(), "config-extends-loop");
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

    assert_eq!(error.code(), "config-read");
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
        ConfigError::ExtendsNotConfig {
            file: file.clone(),
            extends: RelativePath::try_from("../.env".to_owned()).expect("relative"),
            span: 0..1,
        },
        ConfigError::SymlinkNotConfig { path: file.clone() },
        ConfigError::NamespaceOutsideRoot {
            file: file.clone(),
            name: name("shared"),
            directory: file.clone(),
            root: file.clone(),
        },
    ];
    let mut codes = Vec::new();
    for error in &errors {
        assert!(error.code().starts_with("config-"), "{error:?}");
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

/// A two-level chain: `child` extends `mid` extends the top config. The top sets include, exclude
/// and namespaces `shared` and `deep`; `mid` overrides `exclude` and `shared`; `child` sets only
/// `own`. Every namespace names a directory inside `child`, the root.
fn provenance_chain() -> TempDir {
    tree(&[
        (
            "supdoc.config.toml",
            "include = [\"docs\", \"notes/**\"]\nexclude = [\"old\"]\n[namespaces]\nshared = \"child/from-root\"\ndeep = \"child/deep\"\n",
        ),
        (
            "mid/supdoc.config.toml",
            "extends = \"../supdoc.config.toml\"\nexclude = [\"drafts\", \"tmp/\"]\n[namespaces]\nshared = \"../child/from-mid\"\n",
        ),
        (
            "child/supdoc.config.toml",
            "extends = \"../mid/supdoc.config.toml\"\n[namespaces]\nown = \"own\"\n",
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
    assert_eq!(definition.file(), &at(&dir, "supdoc.config.toml"));
}

#[test]
fn exclude_definition_names_the_nearest_file_that_set_it_with_raw_patterns() {
    let dir = provenance_chain();
    let config = load_from(&dir, "child");

    let definition = config
        .exclude_definition()
        .expect("a file in the chain sets exclude");
    assert_eq!(definition.patterns(), ["drafts", "tmp/"]);
    assert_eq!(definition.file(), &at(&dir, "mid/supdoc.config.toml"));
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
    assert_eq!(
        defined_in("shared"),
        Some(at(&dir, "mid/supdoc.config.toml"))
    );
    assert_eq!(defined_in("deep"), Some(at(&dir, "supdoc.config.toml")));
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

// Containment: a hostile config can't make supdoc read or name files outside the project

#[test]
fn extends_naming_another_file_is_refused_without_printing_it() {
    let secret = "DB_PASSWORD=hunter2";
    let dir = tree(&[
        ("home/.env", secret),
        ("project/supdoc.config.toml", "extends = \"../home/.env\"\n"),
    ]);
    let error = load_error(&dir, "project/supdoc.config.toml");

    assert_eq!(error.code(), "config-extends-not-config", "got {error:?}");
    assert_eq!(error.exit_code(), 78);
    let shown = format!("{error} {}", error.help().unwrap_or_default());
    assert!(
        !shown.contains("hunter2"),
        "the secret must not be shown: {shown}"
    );
}

#[test]
fn extends_naming_a_missing_file_of_another_name_is_refused_the_same_way() {
    let dir = tree(&[("project/supdoc.config.toml", "extends = \"../home/.env\"\n")]);
    let error = load_error(&dir, "project/supdoc.config.toml");

    assert_eq!(
        error.code(),
        "config-extends-not-config",
        "a missing file and a present one must look the same: {error:?}",
    );
}

#[cfg(unix)]
#[test]
fn extends_through_a_symlink_named_like_a_config_is_judged_by_the_real_file() {
    let dir = tree(&[
        ("home/.env", "DB_PASSWORD=hunter2"),
        (
            "project/supdoc.config.toml",
            "extends = \"base/supdoc.config.toml\"\n",
        ),
    ]);
    fs::create_dir(dir.path().join("project/base"))
        .expect("the fixture directory should be creatable");
    std::os::unix::fs::symlink(
        dir.path().join("home/.env"),
        dir.path().join("project/base/supdoc.config.toml"),
    )
    .expect("the symlink should be creatable");
    let error = load_error(&dir, "project/supdoc.config.toml");

    assert_eq!(error.code(), "config-extends-not-config", "got {error:?}");
    assert!(!error.to_string().contains("hunter2"), "{error}");
}

#[cfg(unix)]
#[test]
fn found_config_symlinked_to_another_name_is_refused_without_reading_it() {
    let dir = tree(&[
        ("home/.env", "DB_PASSWORD=hunter2"),
        ("project/docs/a.md", ""),
    ]);
    std::os::unix::fs::symlink(
        dir.path().join("home/.env"),
        dir.path().join("project/supdoc.config.toml"),
    )
    .expect("the symlink should be creatable");

    let error = Config::load(&at(&dir, "project/docs")).expect_err("the symlink should be refused");

    assert_eq!(error.code(), "config-symlink-not-config", "got {error:?}");
    assert!(!error.to_string().contains("hunter2"), "{error}");
}

#[cfg(unix)]
#[test]
fn found_config_symlinked_to_a_directory_or_nowhere_is_refused_like_one_to_another_name() {
    let dir = tree(&[
        ("supdoc.config.toml", "include = [\"docs\"]\n"),
        ("home/.env", "DB_PASSWORD=hunter2"),
        ("project/docs/a.md", ""),
    ]);
    fs::create_dir(dir.path().join("home/secrets"))
        .expect("the fixture directory should be creatable");
    let refusal = |target: &str| {
        let link = format!("project-{target}");
        fs::create_dir_all(dir.path().join(&link).join("docs"))
            .expect("the fixture project should be creatable");
        std::os::unix::fs::symlink(
            dir.path().join("home").join(target),
            dir.path().join(&link).join("supdoc.config.toml"),
        )
        .expect("the symlink should be creatable");
        let error = Config::load(&at(&dir, &format!("{link}/docs")))
            .expect_err("a config symlink that doesn't lead to a config should be refused");
        (error.code(), error.exit_code(), error.help())
    };

    let to_file = refusal(".env");

    assert_eq!(to_file.0, "config-symlink-not-config", "got {to_file:?}");
    assert_eq!(refusal("secrets"), to_file, "a link to a directory");
    assert_eq!(refusal("absent.env"), to_file, "a dangling link");
}

#[cfg(unix)]
#[test]
fn found_config_symlink_loops_are_refused_like_any_other_non_config_target() {
    let dir = tree(&[
        ("home/.env", "DB_PASSWORD=hunter2"),
        ("project-file/docs/a.md", ""),
        ("self-loop/docs/a.md", ""),
        ("two-loop/docs/a.md", ""),
    ]);
    let link = |from: std::path::PathBuf, to: std::path::PathBuf| {
        std::os::unix::fs::symlink(to, from).expect("the symlink should be creatable");
    };
    let config_of = |project: &str| dir.path().join(project).join("supdoc.config.toml");
    link(config_of("project-file"), dir.path().join("home/.env"));
    link(config_of("self-loop"), config_of("self-loop"));
    link(config_of("two-loop"), dir.path().join("two-loop/other"));
    link(dir.path().join("two-loop/other"), config_of("two-loop"));
    let refusal = |project: &str| {
        let error = Config::load(&at(&dir, &format!("{project}/docs")))
            .expect_err("a looping config symlink should be refused");
        (
            error.code(),
            error.exit_code(),
            error.help(),
            error.to_string().replace(project, "P"),
        )
    };

    let to_file = refusal("project-file");

    assert_eq!(to_file.0, "config-symlink-not-config", "got {to_file:?}");
    assert_eq!(refusal("self-loop"), to_file, "a self-loop");
    assert_eq!(refusal("two-loop"), to_file, "a two-link loop");
}

#[cfg(unix)]
#[test]
fn found_config_symlinked_to_another_config_file_loads_it() {
    let dir = tree(&[
        ("shared/supdoc.config.toml", "include = [\"docs\"]\n"),
        ("project/docs/a.md", ""),
    ]);
    std::os::unix::fs::symlink(
        dir.path().join("shared/supdoc.config.toml"),
        dir.path().join("project/supdoc.config.toml"),
    )
    .expect("the symlink should be creatable");

    let config = load_from(&dir, "project");

    assert_eq!(config.root(), &at(&dir, "project"));
    assert!(config.scans(&at(&dir, "project/docs/a.md")));
}

#[cfg(unix)]
#[test]
fn config_flag_loads_a_symlink_to_any_file_name() {
    let dir = tree(&[("home/base.toml", "include = [\"docs\"]\n")]);
    std::os::unix::fs::symlink(
        dir.path().join("home/base.toml"),
        dir.path().join("supdoc.config.toml"),
    )
    .expect("the symlink should be creatable");

    let loaded = Config::load_file(&at(&dir, "supdoc.config.toml"));

    assert!(
        loaded.is_ok(),
        "--config is trusted with any file: {loaded:?}"
    );
}

#[test]
fn namespace_outside_the_root_is_a_config_error() {
    let dir = tree(&[(
        "project/supdoc.config.toml",
        "[namespaces]\nup = \"../secrets\"",
    )]);
    let error = load_error(&dir, "project/supdoc.config.toml");

    assert_eq!(
        error.code(),
        "config-namespace-outside-root",
        "got {error:?}"
    );
    assert_eq!(error.exit_code(), 78);
    assert!(error.to_string().contains("`up`"), "{error}");
}

#[test]
fn inherited_namespace_outside_the_child_root_is_a_config_error() {
    let dir = tree(&[
        (
            "supdoc.config.toml",
            "[namespaces]\nshared = \"packages/shared\"",
        ),
        (
            "apps/web/supdoc.config.toml",
            "extends = \"../../supdoc.config.toml\"\n",
        ),
    ]);
    let error = Config::load(&at(&dir, "apps/web"))
        .expect_err("a parent's namespace outside the child's root should be refused");

    assert_eq!(
        error.code(),
        "config-namespace-outside-root",
        "got {error:?}"
    );
}

#[cfg(unix)]
#[test]
fn namespace_symlinked_outside_the_root_is_a_config_error() {
    let dir = tree(&[
        ("outside/lib.rs", "fn f() {}"),
        (
            "project/supdoc.config.toml",
            "[namespaces]\nlib = \"vendor\"",
        ),
    ]);
    std::os::unix::fs::symlink(
        dir.path().join("outside"),
        dir.path().join("project/vendor"),
    )
    .expect("the symlink should be creatable");
    let error = load_error(&dir, "project/supdoc.config.toml");

    assert_eq!(
        error.code(),
        "config-namespace-outside-root",
        "got {error:?}"
    );
}

#[cfg(unix)]
#[test]
fn contains_judges_a_dangling_symlink_by_where_it_points() {
    let dir = tree(&[("project/supdoc.config.toml", "")]);
    let root = dir.path().join("project");
    std::os::unix::fs::symlink(dir.path().join("gone.rs"), root.join("out.rs"))
        .expect("the symlink should be creatable");
    std::os::unix::fs::symlink(root.join("gone.rs"), root.join("in.rs"))
        .expect("the symlink should be creatable");
    let config = load_from(&dir, "project");

    assert!(
        !config.contains(&root.join("out.rs")),
        "points outside the root"
    );
    assert!(
        config.contains(&root.join("in.rs")),
        "points at a missing file in the root"
    );
    assert!(
        config.contains(&root.join("src/missing.rs")),
        "a missing file in the root"
    );
}

// Features: [features.hash] line-endings

use crate::hash::LineEndings;

#[test]
fn line_endings_defaults_to_normalize() {
    let dir = tree(&[("supdoc.config.toml", "")]);

    assert_eq!(
        load_from(&dir, ".").hash_settings().line_endings,
        LineEndings::Normalize
    );
}

#[test]
fn line_endings_exact_is_read() {
    let dir = tree(&[(
        "supdoc.config.toml",
        "[features.hash]\nline-endings = \"exact\"\n",
    )]);

    assert_eq!(
        load_from(&dir, ".").hash_settings().line_endings,
        LineEndings::Exact
    );
}

#[test]
fn child_line_endings_overrides_parent() {
    let dir = tree(&[
        (
            "supdoc.config.toml",
            "[features.hash]\nline-endings = \"exact\"\n",
        ),
        (
            "child/supdoc.config.toml",
            "extends = \"../supdoc.config.toml\"\n[features.hash]\nline-endings = \"normalize\"\n",
        ),
    ]);

    assert_eq!(
        load_from(&dir, "child").hash_settings().line_endings,
        LineEndings::Normalize
    );
}

#[test]
fn unset_child_line_endings_inherits_parent() {
    let dir = tree(&[
        (
            "supdoc.config.toml",
            "[features.hash]\nline-endings = \"exact\"\n",
        ),
        (
            "child/supdoc.config.toml",
            "extends = \"../supdoc.config.toml\"\n",
        ),
    ]);

    assert_eq!(
        load_from(&dir, "child").hash_settings().line_endings,
        LineEndings::Exact
    );
}

#[test]
fn empty_features_hash_table_inherits_parent() {
    let dir = tree(&[
        (
            "supdoc.config.toml",
            "[features.hash]\nline-endings = \"exact\"\n",
        ),
        (
            "child/supdoc.config.toml",
            "extends = \"../supdoc.config.toml\"\n[features.hash]\n",
        ),
    ]);

    assert_eq!(
        load_from(&dir, "child").hash_settings().line_endings,
        LineEndings::Exact
    );
}

#[test]
fn unknown_line_endings_value_is_parse_error_listing_both_values() {
    let dir = tree(&[(
        "supdoc.config.toml",
        "[features.hash]\nline-endings = \"crlf\"\n",
    )]);
    let error = load_error(&dir, "supdoc.config.toml");

    assert_eq!(error.code(), "config-parse", "got {error:?}");
    let message = error.to_string();
    assert!(
        message.contains("unknown variant `crlf`")
            && message.contains("`normalize`")
            && message.contains("`exact`"),
        "the message should name the bad value and both choices, got {message}"
    );
}

#[test]
fn unknown_features_key_is_parse_error() {
    for text in [
        "[features]\ncache = true\n",
        "[features.hash]\neol = \"x\"\n",
    ] {
        let dir = tree(&[("supdoc.config.toml", text)]);
        let error = load_error(&dir, "supdoc.config.toml");

        assert_eq!(error.code(), "config-parse", "{text:?} gave {error:?}");
    }
}

#[test]
fn line_endings_provenance_names_the_defining_file() {
    let dir = tree(&[
        (
            "supdoc.config.toml",
            "[features.hash]\nline-endings = \"exact\"\n",
        ),
        (
            "child/supdoc.config.toml",
            "extends = \"../supdoc.config.toml\"\n",
        ),
    ]);
    let inherited = load_from(&dir, "child");
    let default = tree(&[("supdoc.config.toml", "")]);

    assert_eq!(
        inherited.line_endings_defined_in(),
        Some(&at(&dir, "supdoc.config.toml"))
    );
    assert_eq!(load_from(&default, ".").line_endings_defined_in(), None);
}
