//! The `supdoc` command: a thin binary over the library's one entry point, [`supdoc::main`].

#![cfg_attr(
    test,
    allow(
        clippy::missing_errors_doc,
        clippy::missing_panics_doc,
        reason = "tests report failures rather than document callable contracts"
    )
)]

fn main() -> std::process::ExitCode {
    supdoc::main()
}
