//! The offline subcommands: they run the core engine directly and print JSON
//! to stdout. `assess --judge` also asks a model.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use assessment_core::Engine;
use assessment_core::model::AssessmentRequest;
use assessment_grpc::flow::{self, Flow};

use crate::judge::JudgeStack;

/// Reads the `AssessmentRequest` in `input`. Also returns a name of the
/// source for error messages.
fn read_request(input: &Path) -> Result<(AssessmentRequest, String)> {
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
    Ok((request, source))
}

/// `assess <file|->`: reads an `AssessmentRequest` and prints the `Assessment`.
pub(crate) fn assess(engine: &Engine, input: &Path) -> Result<()> {
    let (request, source) = read_request(input)?;
    let assessment = engine
        .assess(&request)
        .with_context(|| format!("failed to assess the request in {source}"))?;
    print_json(&assessment)
}

/// `assess <file|-> --judge`: as [`assess`], and asks the judge about the
/// flagged text changes. A judge that fails does not fail the command: the
/// fields it did not answer are `unavailable`. One line on stderr says what
/// the judge did.
pub(crate) async fn assess_judged(engine: Engine, input: &Path, stack: JudgeStack) -> Result<()> {
    let (request, source) = read_request(input)?;
    // No deadline for the command: each judge call has the whole timeout.
    let flow = Flow::new().with_judge(stack.judge, stack.settings);
    let judged = flow::assess_judged(Arc::new(engine), request, &flow, None)
        .await
        .with_context(|| format!("failed to assess the request in {source}"))?;
    let report = &judged.report;
    eprintln!(
        "judge: {} flagged, {} judged ({}, {}/{} tokens, ~${:.4})",
        report.flagged,
        report.judged,
        stack.model,
        report.usage.input_tokens,
        report.usage.output_tokens,
        report.cost_usd
    );
    print_json(&judged.assessment)
}

/// `policy`: prints the effective policy.
pub(crate) fn policy(engine: &Engine) -> Result<()> {
    print_json(engine.policy())
}

/// `compare-text`: prints the `TextDiff` of two texts of `field`. Its
/// `gray_zone` says why the change would be flagged for a judgment.
pub(crate) fn compare_text(engine: &Engine, field: &str, before: &str, after: &str) -> Result<()> {
    let diff = engine
        .compare_text_as(field, before, after, None)
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
