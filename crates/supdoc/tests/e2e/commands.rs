//! Every command this build defines is built: none answers with `cli/not-built` (operator ruling on
//! H1). The commands and schema names come from the clap definition, so a new one is covered too.

use clap::CommandFactory;
use supdoc::cli::CliError;
use supdoc::cli::args::Cli;

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

/// Fails listing every run that was a usage error or answered `cli/not-built`.
fn assert_all_built(project: &Project, invocations: &[Vec<&str>]) {
    let mut unbuilt = Vec::new();
    for argv in invocations {
        let run = project.supdoc(argv);
        if run.code == Some(i32::from(CliError::USAGE_EXIT_CODE)) {
            unbuilt.push(format!(
                "`supdoc {}` was a usage error, so this test can't tell whether it is built; add \
                 its required arguments to `required_arguments`: {}",
                run.args, run.stderr
            ));
        } else if run.stderr.contains(CliError::NOT_BUILT) {
            unbuilt.push(format!("`supdoc {}`: {}", run.args, run.stderr.trim_end()));
        }
    }
    assert!(
        unbuilt.is_empty(),
        "every command should be built:\n{}",
        unbuilt.join("\n")
    );
}

#[test]
fn no_command_answers_with_the_not_built_error() {
    let project = Project::fixture("namespaced");
    let mut commands = Vec::new();
    leaf_commands(&Cli::command(), "", &mut commands);

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
    assert_all_built(&project, &invocations);
}

#[test]
fn no_command_schema_answers_with_the_not_built_error() {
    let project = Project::fixture("namespaced");
    let cli = Cli::command();
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
    assert_all_built(&project, &invocations);
}
