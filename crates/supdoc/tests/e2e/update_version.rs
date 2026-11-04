//! `supdoc update` given something that reads as a version: a usage error pointing at `upgrade`.

use crate::support::Project;

#[test]
fn update_with_a_version_argument_suggests_upgrade() {
    let project = Project::fixture("basic");
    project.write("supdoc.config.toml", "");

    let run = project.supdoc(&["update", "0.3.1"]);

    let _ = run.assert_exit(i32::from(supdoc::testing::usage_exit_code()));
    assert!(
        run.stderr.contains("supdoc upgrade 0.3.1"),
        "stderr should suggest `supdoc upgrade 0.3.1`, got: {}",
        run.stderr
    );
}
