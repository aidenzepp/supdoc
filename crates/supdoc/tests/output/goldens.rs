//! One golden transcript per command, colors off, so a change to what a person reads shows up
//! as a reviewed snapshot diff (07 G13).
//!
//! `schema` and `completions` print generated documents (JSON Schema, a shell script), which
//! their own tests and `schema/*.json` already pin, so they have no golden here. `skill install`
//! has no golden yet either.

use crate::support::Project;

/// Runs `args` in `project` and pins the transcript as the snapshot `name`.
fn golden(name: &str, project: &Project, args: &[&str]) {
    insta::assert_snapshot!(name, project.run(args).transcript());
}

#[test]
fn check_text_matches_golden() {
    golden("check", &Project::dirty(), &["check"]);
}

#[test]
fn check_clean_text_matches_golden() {
    golden("check_clean", &Project::clean(), &["check"]);
}

#[test]
fn check_moved_text_matches_golden() {
    golden("check_moved", &Project::moved(), &["check"]);
}

#[test]
fn check_verbose_text_matches_golden() {
    golden(
        "check_verbose",
        &Project::failing(),
        &["check", "--verbose"],
    );
}

#[test]
fn check_quiet_text_matches_golden() {
    golden("check_quiet", &Project::dirty(), &["check", "--quiet"]);
}

#[test]
fn check_github_matches_golden() {
    golden(
        "check_github",
        &Project::dirty(),
        &["check", "--output", "github"],
    );
}

#[test]
fn update_preview_text_matches_golden() {
    golden("update_preview", &Project::dirty(), &["update"]);
}

#[test]
fn update_applied_text_matches_golden() {
    golden("update_applied", &Project::dirty(), &["update", "--yes"]);
}

#[test]
fn fix_preview_text_matches_golden() {
    golden("fix_preview", &Project::dirty(), &["fix"]);
}

#[test]
fn fix_applied_text_matches_golden() {
    golden("fix_applied", &Project::dirty(), &["fix", "--yes"]);
}

#[test]
fn list_text_matches_golden() {
    golden("list", &Project::dirty(), &["list"]);
}

#[test]
fn list_one_target_text_matches_golden() {
    golden(
        "list_target",
        &Project::dirty(),
        &["list", "src/lib.rs#add"],
    );
}

#[test]
fn resolve_symbols_text_matches_golden() {
    golden(
        "resolve_symbols",
        &Project::clean(),
        &["resolve", "src/lib.rs"],
    );
}

#[test]
fn resolve_missing_symbol_text_matches_golden() {
    golden(
        "resolve_missing",
        &Project::clean(),
        &["resolve", "src/lib.rs", "ad"],
    );
}

#[test]
fn config_show_text_matches_golden() {
    golden("config_show", &Project::clean(), &["config", "show"]);
}

#[test]
fn namespace_list_text_matches_golden() {
    golden("namespace_list", &Project::clean(), &["namespace", "list"]);
}

#[test]
fn namespace_rename_preview_text_matches_golden() {
    golden(
        "namespace_rename_preview",
        &Project::clean(),
        &["namespace", "rename", "core", "base"],
    );
}

#[test]
fn init_dry_run_text_matches_golden() {
    golden("init_dry_run", &Project::empty(), &["init", "--dry-run"]);
}

#[test]
fn init_refusal_text_matches_golden() {
    golden("init_exists", &Project::clean(), &["init"]);
}

#[test]
fn fatal_error_text_matches_golden() {
    golden(
        "fatal_missing_config",
        &Project::empty(),
        &["check", "--config", "missing.toml"],
    );
}

#[test]
fn usage_error_text_matches_golden() {
    golden("usage_misspelled_command", &Project::empty(), &["chek"]);
}

#[test]
fn usage_error_json_matches_golden() {
    golden("usage_json", &Project::empty(), &["--json", "chek"]);
}
