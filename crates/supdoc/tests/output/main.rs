//! Runs the built `supdoc` binary in a fresh project and checks what a user sees: a golden
//! transcript per command (07 G13), and the binary-level stream, color, pipe and write
//! contracts (06 X3–X14, 07 G2/G8/G12).

#![cfg_attr(
    test,
    allow(
        clippy::missing_errors_doc,
        clippy::missing_panics_doc,
        reason = "tests report failures rather than document callable contracts"
    )
)]

#[cfg(test)]
mod contracts;
#[cfg(test)]
mod goldens;
#[cfg(test)]
mod support;
