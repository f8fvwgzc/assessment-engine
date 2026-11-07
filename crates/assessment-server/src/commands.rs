//! The offline subcommands: they run the core engine directly and print JSON
//! to stdout.

use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use assessment_core::Engine;
use assessment_core::model::AssessmentRequest;

/// `assess <file|->`: reads an `AssessmentRequest` and prints the `Assessment`.
pub(crate) fn assess(engine: &Engine, input: &Path) -> Result<()> {
    let (text, source) = if input == Path::new("-") {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .context("failed to read the request from stdin")?;
        (text, "stdin".to_owned())
    } else {
        let text = std::fs::read_to_string(input)
            .with_context(|| format!("failed to read the request `{}`", input.display()))?;
        (text, format!("`{}`", input.display()))
    };
    let request: AssessmentRequest = serde_json::from_str(&text).with_context(|| {
        format!("failed to parse the request in {source} as an AssessmentRequest")
    })?;
    let assessment = engine
        .assess(&request)
        .with_context(|| format!("failed to assess the request in {source}"))?;
    print_json(&assessment)
}

/// `policy`: prints the effective policy.
pub(crate) fn policy(engine: &Engine) -> Result<()> {
    print_json(engine.policy())
}

/// `compare-text`: prints the `TextDiff` of two texts.
pub(crate) fn compare_text(engine: &Engine, before: &str, after: &str) -> Result<()> {
    let diff = engine
        .compare_text(before, after, None)
        .context("failed to compare the texts")?;
    print_json(&diff)
}

/// Prints `value` as pretty JSON. A reader that goes away early (`| head`)
/// is not an error.
fn print_json<T: serde::Serialize>(value: &T) -> Result<()> {
    let mut out = std::io::stdout().lock();
    let written = serde_json::to_writer_pretty(&mut out, value)
        .map_err(std::io::Error::from)
        .and_then(|()| writeln!(out))
        .and_then(|()| out.flush());
    match written {
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other.context("failed to write the JSON output"),
    }
}
