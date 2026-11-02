//! Git-ignore parity: nested repositories, submodules, global-excludes precedence, whitelists and
//! work-tree top discovery decide which files supdoc scans.

use std::fs;

use crate::containment::web_project;
use crate::support::{Project, array_at, at};

#[cfg(unix)]
#[test]
fn a_named_path_beside_a_gitignore_symlinked_out_of_the_root_is_not_quoted() {
    let project = web_project();
    // `gui*.md` matches the guide; no path the run names contains it.
    fs::write(project.path("rules"), "gui*.md\n").expect("the outside rules should be writable");
    std::os::unix::fs::symlink(
        project.path("rules"),
        project.path("services/web/.gitignore"),
    )
    .expect("the symlink should be creatable");

    let run = project.supdoc_in("services/web", &["check", "docs/guide.md"]);
    let list = project.supdoc_in("services/web", &["--json", "list"]);

    let _ = run.assert_exit(1);
    let _ = list.assert_exit(0);
    assert!(
        listed_sources(&list).contains(&"docs/guide.md".to_owned()),
        "the guide is not hidden by the symlinked rules\n{}",
        list.stdout
    );
    for stream in [&run.stdout, &run.stderr] {
        assert!(
            !stream.contains("gui*.md"),
            "`supdoc {}` printed a line from a file outside the root\nstdout:\n{}\nstderr:\n{}",
            run.args,
            run.stdout,
            run.stderr
        );
    }
}

/// The number of findings of `kind` in a `--json` run.
fn count_of(run: &crate::support::Run, kind: &str) -> usize {
    array_at(&run.json(), "/findings")
        .iter()
        .filter(|finding| finding["kind"].as_str() == Some(kind))
        .count()
}

/// The sources of every link a `--json list` run reports.
fn listed_sources(run: &crate::support::Run) -> Vec<String> {
    array_at(&run.json(), "/data/links")
        .iter()
        .map(|link| at(link, "/source").as_str().unwrap_or_default().to_owned())
        .collect()
}

#[cfg(unix)]
#[test]
fn a_directory_whose_gitignore_is_symlinked_out_is_checked_without_its_rules() {
    let project = web_project();
    let _ = project
        .supdoc_in("services/web", &["update", "--yes"])
        .assert_exit(0);
    // As in git, the symlink is treated as absent, so this rule hides nothing.
    fs::write(project.path("rules"), "gui*.md\n").expect("the outside rules should be writable");
    std::os::unix::fs::symlink(
        project.path("rules"),
        project.path("services/web/docs/.gitignore"),
    )
    .expect("the symlink should be creatable");

    let run = project.supdoc_in("services/web", &["--json", "check"]);
    let list = project.supdoc_in("services/web", &["--json", "list"]);

    assert_eq!(
        count_of(&run, "scan-ignore-file-symlink"),
        1,
        "{}",
        run.stdout
    );
    assert_eq!(count_of(&run, "link-obsolete"), 0, "{}", run.stdout);
    assert!(
        listed_sources(&list).contains(&"docs/guide.md".to_owned()),
        "the guide is walked and its links counted\n{}",
        list.stdout
    );
    assert!(!run.stdout.contains("gui*.md"), "{}", run.stdout);
}

#[cfg(unix)]
#[test]
fn a_root_gitignore_symlinked_out_of_the_root_is_not_read_and_the_scan_goes_on() {
    let project = web_project();
    let _ = project
        .supdoc_in("services/web", &["update", "--yes"])
        .assert_exit(0);
    let locked = project.read("services/web/supdoc.lock");
    fs::write(project.path("rules"), "gui*.md\n").expect("the outside rules should be writable");
    std::os::unix::fs::symlink(
        project.path("rules"),
        project.path("services/web/.gitignore"),
    )
    .expect("the symlink should be creatable");

    let check = project.supdoc_in("services/web", &["check"]);
    let update = project.supdoc_in("services/web", &["update", "--yes"]);

    // The fixture's guide has a link into an undefined namespace, so `check` fails on that.
    let _ = check.assert_exit(1);
    let _ = update.assert_exit(0);
    for run in [&check, &update] {
        let shown = format!("{}{}", run.stdout, run.stderr);
        assert_eq!(
            shown.matches("scan-ignore-file-symlink").count(),
            1,
            "`supdoc {}` should warn once about the symlink\n{shown}",
            run.args
        );
        assert!(
            !shown.contains("gui*.md"),
            "`supdoc {}` printed a line from outside the root\n{shown}",
            run.args
        );
    }
    assert_eq!(
        project.read("services/web/supdoc.lock"),
        locked,
        "the guide is still walked, so its entries stay"
    );
}

#[cfg(unix)]
#[test]
fn a_package_whose_gitignore_links_to_the_monorepos_scans_with_one_warning() {
    let project = monorepo_with_git();
    // The monorepo's own `.gitignore` sits above the package root and still applies.
    fs::write(project.path(".gitignore"), "scr*.md\n").expect("the gitignore should be writable");
    std::os::unix::fs::symlink("../../.gitignore", project.path("services/web/.gitignore"))
        .expect("the symlink should be creatable");
    project.write("services/web/docs/scratch.md", SCRATCH);

    let run = project.supdoc_in("services/web", &["--json", "update", "--yes"]);
    let list = project.supdoc_in("services/web", &["--json", "list"]);

    let _ = run.assert_exit(0);
    assert_eq!(
        count_of(&run, "scan-ignore-file-symlink"),
        1,
        "{}",
        run.stdout
    );
    let sources = listed_sources(&list);
    assert!(
        sources.contains(&"docs/guide.md".to_owned())
            && !sources.contains(&"docs/scratch.md".to_owned()),
        "the guide is counted, and the monorepo's rule above the root still hides scratch.md\n{}",
        list.stdout
    );
}

#[cfg(unix)]
#[test]
fn a_gitignore_symlinked_within_the_root_is_not_honoured() {
    let project = web_project();
    project.write("services/web/rules", "gui*.md\n");
    std::os::unix::fs::symlink("../rules", project.path("services/web/docs/.gitignore"))
        .expect("the symlink should be creatable");

    let run = project.supdoc_in("services/web", &["--json", "update", "--yes"]);
    let list = project.supdoc_in("services/web", &["--json", "list"]);

    let _ = run.assert_exit(0);
    assert_eq!(
        count_of(&run, "scan-ignore-file-symlink"),
        1,
        "{}",
        run.stdout
    );
    assert!(
        listed_sources(&list).contains(&"docs/guide.md".to_owned()),
        "git doesn't honour the symlink, so neither does supdoc\n{}",
        list.stdout
    );
}

/// Makes `relative` in `project` a git directory git accepts as a repository, without running
/// git: a `HEAD` naming a branch, and empty `objects` and `refs` directories.
fn make_git_dir(project: &Project, relative: &str) {
    project.write(&format!("{relative}/HEAD"), "ref: refs/heads/main\n");
    for dir in ["objects", "refs"] {
        fs::create_dir_all(project.path(&format!("{relative}/{dir}")))
            .expect("the fixture git directory should be creatable");
    }
}

/// A doc with one link, for a file the tests hide with an ignore rule.
const SCRATCH: &str = "The [`handler`](../src/lib.rs#handler) function.\n";

#[test]
fn a_gitignore_whitelist_overrides_a_git_exclude_rule_and_keeps_the_lock_entry() {
    let project = web_project();
    project.write("services/web/docs/keep.md", SCRATCH);
    let _ = project
        .supdoc_in("services/web", &["update", "--yes"])
        .assert_exit(0);
    let kept = "source = \"docs/keep.md\"";
    assert!(project.read("services/web/supdoc.lock").contains(kept));
    // As in git, the `.gitignore` whitelist outranks the exclude file's rule.
    make_git_dir(&project, "services/web/.git");
    project.write("services/web/.git/info/exclude", "docs/*.md\n");
    project.write("services/web/.gitignore", "!docs/keep.md\n");

    let _ = project
        .supdoc_in("services/web", &["update", "--yes"])
        .assert_exit(0);

    let locked = project.read("services/web/supdoc.lock");
    assert!(
        locked.contains(kept),
        "keep.md is walked, so its entry stays:\n{locked}"
    );
}

#[test]
fn a_git_exclude_whitelist_overrides_the_global_excludes_file() {
    let project = web_project();
    let home = tempfile::tempdir().expect("a home directory should be creatable");
    fs::create_dir_all(home.path().join(".config/git"))
        .expect("the git config directory should be creatable");
    fs::write(home.path().join(".config/git/ignore"), "docs/*.md\n")
        .expect("the global excludes file should be writable");
    project.write("services/web/docs/keep.md", SCRATCH);
    make_git_dir(&project, "services/web/.git");
    project.write("services/web/.git/info/exclude", "!docs/keep.md\n");

    let run = project.supdoc_in_with(
        "services/web",
        &["--json", "list"],
        &[
            ("HOME", home.path()),
            ("XDG_CONFIG_HOME", &home.path().join(".config")),
        ],
    );

    let sources: Vec<_> = array_at(&run.json(), "/data/links")
        .iter()
        .map(|link| at(link, "/source").as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        sources.contains(&"docs/keep.md".to_owned()),
        "the exclude whitelist outranks the global rule, as in git\n{}\n{}",
        run.stdout,
        run.stderr
    );
}

/// Runs `supdoc update --yes` from `dir` with a global excludes file of `pattern` in an isolated
/// home, and returns the web project's lockfile.
fn lock_with_global_pattern(project: &Project, dir: &str, pattern: &str) -> String {
    let home = tempfile::tempdir().expect("a home directory should be creatable");
    fs::create_dir_all(home.path().join(".config/git"))
        .expect("the git config directory should be creatable");
    fs::write(
        home.path().join(".config/git/ignore"),
        format!("{pattern}\n"),
    )
    .expect("the global excludes file should be writable");
    project.write("services/web/docs/keep.md", SCRATCH);

    let run = project.supdoc_in_with(
        dir,
        &["update", "--yes"],
        &[
            ("HOME", home.path()),
            ("XDG_CONFIG_HOME", &home.path().join(".config")),
        ],
    );

    let _ = run.assert_exit(0);
    lock_of_web(project)
}

#[test]
fn an_anchored_global_pattern_counts_from_the_work_tree_top_whatever_the_working_directory() {
    for dir in ["services/web", "services/web/docs"] {
        let project = monorepo_with_git();

        let locked = lock_with_global_pattern(&project, dir, "services/web/docs/gui*.md");

        assert!(
            locked.contains("docs/keep.md") && !locked.contains("docs/guide.md"),
            "git anchors a global pattern at the work tree top, so it hides the guide from `{dir}`:\n{locked}"
        );
    }
}

#[test]
fn an_anchored_global_pattern_counts_from_the_root_outside_a_repository() {
    for dir in ["services/web", "services/web/docs"] {
        let project = web_project();

        let locked = lock_with_global_pattern(&project, dir, "docs/gui*.md");

        assert!(
            locked.contains("docs/keep.md") && !locked.contains("docs/guide.md"),
            "with no repository the pattern counts from the root, so it hides the guide from `{dir}`:\n{locked}"
        );
    }
}

#[test]
fn a_separate_git_dir_brings_its_info_exclude_file_unquoted() {
    let project = web_project();
    // A submodule or `--separate-git-dir` checkout: a `.git` file naming the git directory, which
    // has no `commondir`, so it is the shared directory itself.
    project.write("outside.git/info/exclude", "gui*.md\n");
    project.write(
        "services/web/.git",
        &format!("gitdir: {}\n", project.path("outside.git").display()),
    );

    assert_followed_and_never_quoted(&project);
}

#[test]
fn a_worktrees_shared_exclude_file_applies_and_is_never_quoted() {
    let project = web_project();
    let _ = project
        .supdoc_in("services/web", &["update", "--yes"])
        .assert_exit(0);
    // `git worktree add` leaves a `.git` file naming the worktree's git directory, whose
    // `commondir` leads to the main repository's, outside the root.
    project.write(
        "services/web/.git",
        &format!(
            "gitdir: {}\n",
            project.path("main.git/worktrees/web").display()
        ),
    );
    project.write("main.git/worktrees/web/commondir", "../..\n");
    project.write("main.git/info/exclude", "scr*.md\n");
    project.write("services/web/docs/scratch.md", SCRATCH);

    let check = project.supdoc_in("services/web", &["check"]);
    let named = project.supdoc_in("services/web", &["check", "docs/scratch.md"]);

    let _ = check.assert_exit(1);
    assert!(!check.stdout.contains("scratch.md"), "{}", check.stdout);
    assert!(
        check.stdout.contains("docs/guide.md"),
        "the guide is still checked\n{}",
        check.stdout
    );
    assert!(
        named.stdout.contains("ignored by an ignore file"),
        "{}",
        named.stdout
    );
    for run in [&check, &named] {
        assert!(
            !run.stdout.contains("scr*.md") && !run.stderr.contains("scr*.md"),
            "`supdoc {}` printed a line from outside the root\n{}\n{}",
            run.args,
            run.stdout,
            run.stderr
        );
    }
}

#[test]
fn a_parent_repositorys_exclude_file_applies_to_a_root_in_a_subdirectory() {
    let project = web_project();
    let _ = project
        .supdoc_in("services/web", &["update", "--yes"])
        .assert_exit(0);
    make_git_dir(&project, ".git");
    project.write(".git/info/exclude", "scratch.md\n");
    project.write("services/web/docs/scratch.md", SCRATCH);

    let run = project.supdoc_in("services/web", &["check"]);

    let _ = run.assert_exit(1);
    assert!(
        run.stdout.contains("docs/guide.md") && !run.stdout.contains("scratch.md"),
        "the parent's exclude rule hides scratch.md and nothing else\n{}",
        run.stdout
    );
}

/// Asserts that the web project scans as git would have it with an ignore file it doesn't own
/// hiding the guide: `check` and `update --yes` succeed without the guide, the named guide is
/// blamed on "an ignore file", and no line of that file is printed.
fn assert_followed_and_never_quoted(project: &Project) {
    project.write("services/web/docs/keep.md", SCRATCH);
    let update = project.supdoc_in("services/web", &["update", "--yes"]);
    let check = project.supdoc_in("services/web", &["check"]);
    let named = project.supdoc_in("services/web", &["check", "docs/guide.md"]);

    for run in [&update, &check, &named] {
        let _ = run.assert_exit(0);
        assert!(
            !run.stdout.contains("gui*.md") && !run.stderr.contains("gui*.md"),
            "`supdoc {}` printed a line from outside the root\n{}\n{}",
            run.args,
            run.stdout,
            run.stderr
        );
        assert!(
            !run.stdout.contains("scan-ignore-file-symlink")
                && !run.stderr.contains("scan-ignore-file-symlink"),
            "`supdoc {}` warned about an ignore file it doesn't own\n{}\n{}",
            run.args,
            run.stdout,
            run.stderr
        );
    }
    let locked = project.read("services/web/supdoc.lock");
    assert!(
        locked.contains("docs/keep.md") && !locked.contains("docs/guide.md"),
        "the rule hides the guide, as in git, and nothing else:\n{locked}"
    );
    assert!(
        named.stdout.contains("ignored by an ignore file"),
        "{}",
        named.stdout
    );
}

/// Makes `link` in `project` a symlink to a git directory outside the root whose exclude file
/// hides the guide.
#[cfg(unix)]
fn git_dir_symlinked_out(project: &Project, link: &str) {
    make_git_dir(project, "outside.git");
    project.write("outside.git/info/exclude", "gui*.md\n");
    std::os::unix::fs::symlink(project.path("outside.git"), project.path(link))
        .expect("the symlink should be creatable");
}

#[cfg(unix)]
#[test]
fn a_subdirectory_whose_git_dir_is_symlinked_out_applies_its_exclude_file_unquoted() {
    let project = web_project();
    // Git refuses to check out a `.git` path, so a checkout can't plant this one.
    git_dir_symlinked_out(&project, "services/web/docs/.git");

    assert_followed_and_never_quoted(&project);
}

#[cfg(unix)]
#[test]
fn a_root_whose_git_dir_is_symlinked_out_applies_its_exclude_file_unquoted() {
    let project = web_project();
    git_dir_symlinked_out(&project, "services/web/.git");

    assert_followed_and_never_quoted(&project);
}

#[cfg(unix)]
#[test]
fn a_root_whose_git_info_is_symlinked_out_applies_its_exclude_file_unquoted() {
    let project = web_project();
    project.write("outside.git/info/exclude", "gui*.md\n");
    make_git_dir(&project, "services/web/.git");
    std::os::unix::fs::symlink(
        project.path("outside.git/info"),
        project.path("services/web/.git/info"),
    )
    .expect("the symlink should be creatable");

    assert_followed_and_never_quoted(&project);
}

#[cfg(unix)]
#[test]
fn a_dotfile_gitignore_above_the_root_symlinked_elsewhere_is_not_read() {
    let project = monorepo_with_git();
    // A dotfiles manager such as stow links the monorepo's `.gitignore` into a directory of its own.
    let dotfiles = tempfile::tempdir().expect("a dotfiles directory should be creatable");
    fs::write(dotfiles.path().join("gitignore"), "gui*.md\n")
        .expect("the dotfile should be writable");
    std::os::unix::fs::symlink(
        dotfiles.path().join("gitignore"),
        project.path(".gitignore"),
    )
    .expect("the symlink should be creatable");

    assert_guide_recorded(
        &project,
        "git doesn't read a symlinked ignore file in its work tree, so its rule hides nothing",
    );
    let check = project.supdoc_in("services/web", &["check"]);
    assert!(
        !check.stdout.contains("scan-ignore-file-symlink")
            && !check.stderr.contains("scan-ignore-file-symlink"),
        "a symlinked ignore file above the root is passed over without a warning\n{}\n{}",
        check.stdout,
        check.stderr
    );
}

#[cfg(unix)]
#[test]
fn a_shared_ignore_file_above_the_root_symlinked_within_the_checkout_is_not_read() {
    let project = monorepo_with_git();
    // A monorepo whose root `.ignore` links to a shared file outside the subproject's root.
    project.write("tools/ignore", "gui*.md\n");
    std::os::unix::fs::symlink(project.path("tools/ignore"), project.path(".ignore"))
        .expect("the symlink should be creatable");

    assert_guide_recorded(
        &project,
        "git doesn't read a symlinked ignore file in its work tree, so its rule hides nothing",
    );
    let check = project.supdoc_in("services/web", &["check"]);
    assert!(
        !check.stdout.contains("scan-ignore-file-symlink")
            && !check.stderr.contains("scan-ignore-file-symlink"),
        "a symlinked ignore file above the root is passed over without a warning\n{}\n{}",
        check.stdout,
        check.stderr
    );
}

#[cfg(unix)]
#[test]
fn a_parent_repository_whose_git_dir_is_symlinked_elsewhere_applies_its_exclude_unquoted() {
    let project = web_project();
    git_dir_symlinked_out(&project, ".git");

    assert_followed_and_never_quoted(&project);
}

#[test]
fn a_named_path_hidden_by_a_regular_ignore_file_above_the_root_is_blamed_unquoted() {
    for file in [".gitignore", ".ignore"] {
        let project = monorepo_with_git();
        // A regular file the work tree holds above the root: it applies, but lies outside.
        project.write(file, "gui*.md\n");

        let run = project.supdoc_in("services/web", &["--json", "check", "docs/guide.md"]);

        let messages: Vec<_> = array_at(&run.json(), "/findings")
            .iter()
            .filter(|finding| finding["kind"].as_str() == Some("scan-explicit-path-ignored"))
            .map(|finding| {
                at(finding, "/message")
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            messages,
            ["docs/guide.md was named on the command line but is ignored by an ignore file"],
            "a rule from {file} above the root is blamed without its line or its path\n{}",
            run.stdout
        );
        let shown = format!("{}{}", run.stdout, run.stderr);
        assert!(
            !shown.contains("gui*.md") && !shown.contains(&format!("/{file}")),
            "`supdoc {}` printed a line or path from {file} above the root\n{shown}",
            run.args
        );
    }
}

/// A doc with one link, for a file two directories below the web project.
const NESTED_DOC: &str = "The [`handler`](../../src/lib.rs#handler) function.\n";

/// The web project as the top of its own repository, whose `.gitignore` hides `*.gen.md`, with an
/// untracked repository `docs/nested` inside it whose exclude file hides `ne.md`.
/// `docs/nested` holds `a.gen.md`, `ne.md` and `k.md`; `docs` holds `top.gen.md` and `top.md`.
fn web_with_nested_repo() -> Project {
    let project = web_project();
    make_git_dir(&project, "services/web/.git");
    project.write("services/web/.gitignore", "*.gen.md\n");
    make_git_dir(&project, "services/web/docs/nested/.git");
    project.write("services/web/docs/nested/.git/info/exclude", "ne.md\n");
    for doc in ["nested/a.gen.md", "nested/ne.md", "nested/k.md"] {
        project.write(&format!("services/web/docs/{doc}"), NESTED_DOC);
    }
    for doc in ["top.gen.md", "top.md"] {
        project.write(&format!("services/web/docs/{doc}"), SCRATCH);
    }
    project
}

/// The sorted sources of the web project's `--json list`.
fn web_sources(project: &Project) -> Vec<String> {
    let mut sources = listed_sources(&project.supdoc_in("services/web", &["--json", "list"]));
    sources.sort();
    sources.dedup();
    sources
}

#[test]
fn a_nested_repository_applies_its_own_rules_and_not_the_outer_gitignore() {
    let project = web_with_nested_repo();

    assert_eq!(
        web_sources(&project),
        [
            "docs/guide.md",
            "docs/nested/a.gen.md",
            "docs/nested/k.md",
            "docs/top.md"
        ],
        "as git run inside docs/nested: its exclude file hides ne.md and the outer `*.gen.md` \
         stops at it, while still hiding docs/top.gen.md"
    );
}

#[test]
fn a_submodule_applies_its_own_exclude_file_and_not_the_superprojects_gitignore() {
    let project = monorepo_with_git();
    // The superproject is the whole fixture; `docs/mod` is a submodule whose `.git` file names
    // its git directory under the superproject's `.git/modules`.
    project.write(".gitignore", "*.sp.md\n");
    make_git_dir(&project, ".git/modules/mod");
    project.write(".git/modules/mod/info/exclude", "sm.md\n");
    project.write(
        "services/web/docs/mod/.git",
        "gitdir: ../../../../.git/modules/mod\n",
    );
    for doc in ["mod/a.sp.md", "mod/sm.md", "mod/k.md"] {
        project.write(&format!("services/web/docs/{doc}"), NESTED_DOC);
    }
    for doc in ["top.sp.md", "top.md"] {
        project.write(&format!("services/web/docs/{doc}"), SCRATCH);
    }

    assert_eq!(
        web_sources(&project),
        [
            "docs/guide.md",
            "docs/mod/a.sp.md",
            "docs/mod/k.md",
            "docs/top.md"
        ],
        "as git run inside the submodule: its exclude file hides sm.md and the superproject's \
         `*.sp.md` stops at it, while still hiding docs/top.sp.md"
    );
}

#[test]
fn an_outer_ignore_file_still_hides_files_inside_a_nested_repository() {
    let project = web_with_nested_repo();
    project.write("services/web/.ignore", "ig*.md\n");
    project.write("services/web/docs/nested/ig.md", NESTED_DOC);

    assert!(
        !web_sources(&project).contains(&"docs/nested/ig.md".to_owned()),
        "a `.ignore` is supdoc's own, so it crosses into a nested repository, as in the walker"
    );
}

#[test]
fn an_outer_rule_naming_a_nested_repository_still_skips_it() {
    let project = web_with_nested_repo();
    project.write("services/web/.gitignore", "/docs/nested/\n");

    let sources = web_sources(&project);

    assert!(
        !sources
            .iter()
            .any(|source| source.starts_with("docs/nested/")),
        "the directory itself is judged by the outer rules, so nothing beneath it is walked\n\
         {sources:?}"
    );
}

#[test]
fn a_named_path_hidden_only_by_an_outer_gitignore_inside_a_nested_repository_is_not_warned() {
    let project = web_with_nested_repo();

    let run = project.supdoc_in("services/web", &["--json", "check", "docs/nested/a.gen.md"]);

    assert_eq!(
        count_of(&run, "scan-explicit-path-ignored"),
        0,
        "the outer `*.gen.md` stops at the nested repository, so the named file is checked\n{}",
        run.stdout
    );
}

/// The web project inside a git repository whose work tree is the whole `monorepo` fixture, so
/// the ignore files of the directories above the project apply.
fn monorepo_with_git() -> Project {
    let project = web_project();
    make_git_dir(&project, ".git");
    project
}

/// The web project's `supdoc.lock`, or nothing when `update` found no link to write one for.
fn lock_of_web(project: &Project) -> String {
    fs::read_to_string(project.path("services/web/supdoc.lock")).unwrap_or_default()
}

/// Asserts that `update --yes` in the web project records the guide, so no rule hides it.
fn assert_guide_recorded(project: &Project, why: &str) {
    let run = project.supdoc_in("services/web", &["update", "--yes"]);

    let _ = run.assert_exit(0);
    assert!(
        lock_of_web(project).contains("docs/guide.md"),
        "{why}\n{}\n{}",
        run.stdout,
        run.stderr
    );
}

#[test]
fn a_gitignore_above_a_repository_root_does_not_apply() {
    let project = web_project();
    fs::write(project.path(".gitignore"), "*\n").expect("the gitignore should be writable");
    make_git_dir(&project, "services/web/.git");

    assert_guide_recorded(
        &project,
        "git reads no `.gitignore` above the work tree, so the `*` above the repository hides nothing",
    );
}

#[test]
fn a_gitignore_above_a_project_outside_any_repository_does_not_apply() {
    let project = web_project();
    fs::write(project.path(".gitignore"), "*\n").expect("the gitignore should be writable");

    assert_guide_recorded(
        &project,
        "with no `.git` in the root or above it, no `.gitignore` above the root applies",
    );
}

#[test]
fn a_gitignore_above_the_work_tree_top_does_not_apply_but_one_at_the_top_does() {
    let project = web_project();
    make_git_dir(&project, "services/.git");
    fs::write(project.path(".gitignore"), "gui*.md\n").expect("the gitignore should be writable");
    fs::write(project.path("services/.gitignore"), "scr*.md\n")
        .expect("the gitignore should be writable");
    project.write("services/web/docs/scratch.md", SCRATCH);

    let run = project.supdoc_in("services/web", &["update", "--yes"]);

    let _ = run.assert_exit(0);
    let locked = lock_of_web(&project);
    assert!(
        locked.contains("docs/guide.md") && !locked.contains("docs/scratch.md"),
        "the work tree's top `.gitignore` hides scratch.md, and the one above the top hides \
         nothing:\n{locked}"
    );
}

#[cfg(unix)]
#[test]
fn a_dangling_gitignore_above_the_root_changes_nothing() {
    let project = web_project();
    make_git_dir(&project, ".git");
    std::os::unix::fs::symlink(project.path("gone"), project.path(".gitignore"))
        .expect("the symlink should be creatable");

    let run = project.supdoc_in("services/web", &["update", "--yes"]);

    let _ = run.assert_exit(0);
    assert!(
        project
            .read("services/web/supdoc.lock")
            .contains("docs/guide.md"),
        "nothing hides the guide\n{}",
        run.stderr
    );
}

/// A `.git` entry git can't use, which it passes over in its search for the work tree top.
#[cfg(unix)]
#[derive(Debug, Clone, Copy)]
enum UnusableGit {
    /// An empty directory.
    EmptyDir,
    /// A symlink to nothing.
    Dangling,
}

/// Every [`UnusableGit`].
#[cfg(unix)]
const UNUSABLE_GITS: [UnusableGit; 2] = [UnusableGit::EmptyDir, UnusableGit::Dangling];

/// The web project, whose own `.git` is `unusable`, inside a repository whose work tree is the
/// whole `monorepo` fixture, the top git finds.
#[cfg(unix)]
fn monorepo_with_unusable_web_git(unusable: UnusableGit) -> Project {
    let project = monorepo_with_git();
    let dot_git = project.path("services/web/.git");
    match unusable {
        UnusableGit::EmptyDir => {
            fs::create_dir(&dot_git).expect("the empty git directory should be creatable");
        }
        UnusableGit::Dangling => std::os::unix::fs::symlink(project.path("gone"), &dot_git)
            .expect("the dangling symlink should be creatable"),
    }
    project
}

#[cfg(unix)]
#[test]
fn a_gitignore_above_a_git_entry_git_cannot_use_applies_up_to_the_real_top() {
    for unusable in UNUSABLE_GITS {
        let project = monorepo_with_unusable_web_git(unusable);
        fs::write(project.path(".gitignore"), "gui*.md\n")
            .expect("the gitignore should be writable");
        project.write("services/web/docs/keep.md", SCRATCH);

        let run = project.supdoc_in("services/web", &["update", "--yes"]);

        let _ = run.assert_exit(0);
        let locked = lock_of_web(&project);
        assert!(
            locked.contains("docs/keep.md") && !locked.contains("docs/guide.md"),
            "git passes over a {unusable:?} `.git` to the monorepo's top, whose `.gitignore` hides \
             the guide:\n{locked}"
        );
    }
}

#[cfg(unix)]
#[test]
fn an_anchored_global_pattern_counts_from_the_real_top_past_a_git_entry_git_cannot_use() {
    for unusable in UNUSABLE_GITS {
        let project = monorepo_with_unusable_web_git(unusable);

        let locked =
            lock_with_global_pattern(&project, "services/web", "services/web/docs/gui*.md");

        assert!(
            locked.contains("docs/keep.md") && !locked.contains("docs/guide.md"),
            "git passes over a {unusable:?} `.git` and anchors the pattern at the monorepo's top, \
             so it hides the guide:\n{locked}"
        );
    }
}

#[test]
fn a_git_directory_holding_only_an_exclude_file_hides_nothing() {
    let project = monorepo_with_git();
    // Git rejects a `.git` without `HEAD`, `objects` and `refs`, so never reads its exclude file.
    project.write("services/web/.git/info/exclude", "gui*.md\n");

    assert_guide_recorded(
        &project,
        "git passes over a `.git` it rejects, so the exclude file inside it hides nothing",
    );
}

/// The web project with a directory, not a file, named `supdoc.config.toml` inside `sub`, and a
/// doc there linking to a source that does not exist.
fn web_project_with_a_config_named_directory_in_sub() -> Project {
    let project = web_project();
    fs::write(project.path("services/web/supdoc.config.toml"), "")
        .expect("the config should be writable");
    fs::create_dir_all(project.path("services/web/sub/supdoc.config.toml"))
        .expect("the directory should be creatable");
    fs::create_dir(project.path("services/web/sub/docs"))
        .expect("the directory should be creatable");
    fs::write(
        project.path("services/web/sub/docs/p.md"),
        "# P\n\n[gone](nothere.rs)\n",
    )
    .expect("the doc should be writable");
    project
}

/// The `source: kind` of each link finding in `run`'s `--json` output.
fn link_findings(run: &crate::support::Run) -> Vec<String> {
    array_at(&run.json(), "/data/links")
        .iter()
        .map(|link| {
            format!(
                "{}: {}",
                link["source"].as_str().unwrap_or_default(),
                link.pointer("/finding/kind")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
            )
        })
        .collect()
}

#[test]
fn a_directory_named_like_the_config_is_not_a_nested_project_from_the_root() {
    let project = web_project_with_a_config_named_directory_in_sub();

    let run = project.supdoc_in("services/web", &["--json", "check"]);

    assert!(
        run.stdout.contains("sub/docs/nothere.rs"),
        "the doc under `sub` should be scanned by the root project\n{}",
        run.stdout
    );
    assert!(
        link_findings(&run).contains(&"sub/docs/p.md: read-missing".to_owned()),
        "{}",
        run.stdout
    );
    let _ = run.assert_exit(1);
}

#[test]
fn a_directory_named_like_the_config_reports_the_same_from_inside_it() {
    let project = web_project_with_a_config_named_directory_in_sub();

    let from_root = project.supdoc_in("services/web", &["--json", "check"]);
    let from_sub = project.supdoc_in("services/web/sub", &["--json", "check"]);

    let _ = from_sub.assert_exit(1);
    assert_eq!(
        (
            at(&from_sub.json(), "/root").clone(),
            link_findings(&from_sub)
        ),
        (
            at(&from_root.json(), "/root").clone(),
            link_findings(&from_root)
        ),
        "both runs should use the root project's config and find the same things\nfrom sub:\n{}\nfrom root:\n{}",
        from_sub.stdout,
        from_root.stdout
    );
}

#[test]
fn an_outer_rule_ignoring_the_root_or_a_directory_on_the_way_to_it_hides_nothing_beneath_it() {
    for outer_rule in ["web/", "/services/"] {
        let project = monorepo_with_git();
        fs::write(project.path(".gitignore"), format!("{outer_rule}\n"))
            .expect("the gitignore should be writable");

        let sources = web_sources(&project);
        let named = project.supdoc_in("services/web", &["--json", "check", "docs/guide.md"]);

        assert!(
            sources.contains(&"docs/guide.md".to_owned()),
            "git ignores everything beneath `{outer_rule}`, but supdoc judges only what is beneath \
             the root, so the guide is still a source\n{sources:?}"
        );
        assert_eq!(
            count_of(&named, "scan-explicit-path-ignored"),
            0,
            "the root and the directories above it are never judged, so a named file beneath the \
             root draws no warning under `{outer_rule}`\n{}",
            named.stdout
        );
    }
}

/// The sources of every link a `--json list` run from `services/web` reports, with `home` as the
/// only environment.
fn sources_listed_with_home(project: &Project, home: &std::path::Path) -> Vec<String> {
    let run = project.supdoc_in_with("services/web", &["--json", "list"], &[("HOME", home)]);
    let _ = run.assert_exit(0);
    listed_sources(&run)
}

#[test]
fn rules_match_case_sensitively_even_when_the_repository_sets_core_ignorecase() {
    let project = web_project();
    let home = tempfile::tempdir().expect("a home directory should be creatable");
    make_git_dir(&project, "services/web/.git");
    // Git would hide `docs/guide.md` with `GUI*.md` under `core.ignorecase`; supdoc does not.
    project.write("services/web/.git/config", "[core]\n\tignorecase = true\n");
    project.write("services/web/.gitignore", "docs/GUI*.md\n");

    let sources = sources_listed_with_home(&project, home.path());

    assert!(
        sources.contains(&"docs/guide.md".to_owned()),
        "a differently cased rule hides nothing, whatever `core.ignorecase` says\n{sources:?}"
    );
}

#[test]
fn core_excludes_file_counts_from_the_user_config_and_not_from_the_repository_config() {
    let project = web_project();
    let home = tempfile::tempdir().expect("a home directory should be creatable");
    project.write("rules-user", "docs/gui*.md\n");
    project.write("rules-repo", "docs/gui*.md\n");
    make_git_dir(&project, "services/web/.git");
    project.write(
        "services/web/.git/config",
        &format!(
            "[core]\n\texcludesFile = {}\n",
            project.path("rules-repo").display()
        ),
    );

    let from_repo_config = sources_listed_with_home(&project, home.path());
    fs::write(
        home.path().join(".gitconfig"),
        format!(
            "[core]\n\texcludesFile = {}\n",
            project.path("rules-user").display()
        ),
    )
    .expect("the user git config should be writable");
    let from_user_config = sources_listed_with_home(&project, home.path());

    assert!(
        from_repo_config.contains(&"docs/guide.md".to_owned()),
        "a repository's own `core.excludesFile` is not read\n{from_repo_config:?}"
    );
    assert!(
        !from_user_config.contains(&"docs/guide.md".to_owned()),
        "the user config's `core.excludesFile` hides the guide, as in git\n{from_user_config:?}"
    );
}

#[test]
fn a_global_excludes_rule_from_the_xdg_config_home_hides_a_file() {
    let project = Project::fixture("basic");
    let home = project.path("home");
    let xdg = home.join("xdg");
    fs::create_dir_all(xdg.join("git")).expect("the fixture git directory should be creatable");
    fs::write(xdg.join("git/ignore"), "/docs/guide.md\n")
        .expect("the global excludes file should be writable");
    let system = home.join("no-system-gitconfig");

    let run = project.supdoc_in_with(
        ".",
        &["--json", "list"],
        &[
            ("HOME", &home),
            ("XDG_CONFIG_HOME", &xdg),
            ("GIT_CONFIG_SYSTEM", &system),
        ],
    );

    let _ = run.assert_exit(0);
    let sources = listed_sources(&run);
    assert!(
        !sources.contains(&"docs/guide.md".to_owned()) && !sources.is_empty(),
        "the global rule hides the guide and nothing else\n{sources:?}"
    );
}
