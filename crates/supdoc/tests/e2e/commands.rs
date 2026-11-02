//! Every command this build defines runs: none answers with a usage error when given its required
//! arguments. The commands and schema names come from the clap definition, so a new one is covered
//! too.

use crate::support::Project;

/// The arguments a leaf command can't run without, for the `namespaced` fixture.
fn required_arguments(command: &str) -> &'static [&'static str] {
    match command {
        "resolve" => &["src/lib.rs"],
        "completions" => &["zsh"],
        "namespace rename" => &["auth", "authn"],
        _ => &[],
    }
}

/// Every runnable command path under `command`, such as `check` or `namespace rename`.
fn leaf_commands(command: &clap::Command, prefix: &str, found: &mut Vec<String>) {
    for subcommand in command.get_subcommands() {
        if subcommand.get_name() == "help" {
            continue;
        }
        let path = format!("{prefix}{}", subcommand.get_name());
        if subcommand.has_subcommands() {
            leaf_commands(subcommand, &format!("{path} "), found);
        } else {
            found.push(path);
        }
    }
}

/// Fails listing every run that was a usage error.
fn assert_none_is_a_usage_error(project: &Project, invocations: &[Vec<&str>]) {
    let mut failures = Vec::new();
    for argv in invocations {
        let run = project.supdoc(argv);
        let rejected = [
            i32::from(supdoc::testing::usage_exit_code()),
            i32::from(supdoc::testing::bug_exit_code()),
            101,
        ];
        if run.code.is_none_or(|code| rejected.contains(&code)) {
            failures.push(format!(
                "`supdoc {}` ended with {:?} (usage error, bug, panic or signal), so this test \
                 can't tell whether it is built; fix the command or add its required arguments \
                 to `required_arguments`: {}",
                run.args, run.code, run.stderr
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "every command should run:\n{}",
        failures.join("\n")
    );
}

#[test]
fn no_command_is_a_usage_error() {
    let project = Project::fixture("namespaced");
    let mut commands = Vec::new();
    leaf_commands(&supdoc::testing::command(), "", &mut commands);

    let invocations: Vec<Vec<&str>> = commands
        .iter()
        .map(|command| {
            command
                .split(' ')
                .chain(required_arguments(command).iter().copied())
                .collect()
        })
        .collect();

    assert!(
        invocations.len() >= 12,
        "the clap definition should yield every command: {commands:?}"
    );
    assert_none_is_a_usage_error(&project, &invocations);
}

#[test]
fn no_command_schema_is_a_usage_error() {
    let project = Project::fixture("namespaced");
    let cli = supdoc::testing::command();
    let names: Vec<String> = cli
        .find_subcommand("schema")
        .and_then(|schema| {
            schema
                .get_arguments()
                .find(|argument| argument.get_id() == "command")
        })
        .expect("`supdoc schema` should take a COMMAND argument")
        .get_possible_values()
        .iter()
        .map(|value| value.get_name().to_owned())
        .collect();

    let invocations: Vec<Vec<&str>> = names
        .iter()
        .map(|name| vec!["schema", name.as_str()])
        .collect();

    assert!(
        !invocations.is_empty(),
        "`supdoc schema` should name the commands it describes"
    );
    assert_none_is_a_usage_error(&project, &invocations);
}
