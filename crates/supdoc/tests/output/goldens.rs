//! One golden transcript per command, colors off, so a change to what a person reads shows up
//! as a reviewed snapshot diff (07 G13).
//!
//! `schema` and `completions` print generated documents (JSON Schema, a shell script), which
//! their own tests and `schema/*.json` already pin, so they have no golden here.

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
fn update_settings_preview_text_matches_golden() {
    // `clean` records under the default, normalize. Switching to exact with CRLF code makes every
    // link's hash change, so the migration preview has the settings row and one accept per link.
    let project = Project::clean();
    project.write(
        "supdoc.config.toml",
        "# Project settings\n[namespaces]\ncore = \"src\"\n\n[features.hash]\nline-endings = \"exact\"\n",
    );
    let lib = std::fs::read_to_string(project.path().join("src/lib.rs"))
        .expect("the fixture's src/lib.rs should be readable");
    project.write("src/lib.rs", &lib.replace('\n', "\r\n"));
    golden("update_settings_preview", &project, &["update"]);
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

#[test]
fn skill_install_preview_text_matches_golden() {
    golden(
        "skill_install_preview",
        &Project::empty(),
        &["skill", "install"],
    );
}

#[test]
fn skill_install_applied_text_matches_golden() {
    golden(
        "skill_install_applied",
        &Project::empty(),
        &["skill", "install", "--yes"],
    );
}

#[test]
fn skill_install_global_preview_text_matches_golden() {
    let project = Project::empty();
    // Outside the project, so the product prints it the same way wherever the temp dir lives.
    let home_dir = tempfile::tempdir().expect("a home directory should be created");
    let home = home_dir
        .path()
        .to_str()
        .expect("the temp directory path should be UTF-8");
    insta::assert_snapshot!(
        "skill_install_global_preview",
        project
            .run_with(&["skill", "install", "--global"], &[("HOME", home)])
            .with_home(home_dir.path())
            .transcript()
    );
}
