//! `billy run '<a> | <b> | <c>'`: runs a pipeline in-process, without a shell,
//! so a model can send one command string. Nothing here spawns processes.

use std::io::Write;

use anyhow::{anyhow, bail, Context, Result};
use clap::error::ErrorKind;
use clap::Parser;

use crate::commands::{execute, Cli, Cmd, Input};

pub fn run(line: &str, input: Input, out: &mut dyn Write) -> Result<()> {
    crate::commands::writes::IN_RUN.store(true, std::sync::atomic::Ordering::Relaxed);
    let segments = split_pipes(line)?;
    let mut stages: Vec<(String, Cmd)> = Vec::new();
    for (i, segment) in segments.iter().enumerate() {
        let n = i + 1;
        let mut words = shlex::split(segment)
            .ok_or_else(|| anyhow!("stage {n}: unbalanced quotes in `{}`", segment.trim()))?;
        if words.first().map(String::as_str) == Some("billy") {
            words.remove(0);
        }
        if words.is_empty() {
            bail!("stage {n} is empty");
        }
        let label = format!("stage {n} (`{}`)", words.join(" "));
        let argv = std::iter::once("billy".to_string()).chain(words);
        match Cli::try_parse_from(argv) {
            Ok(cli) => stages.push((label, cli.cmd)),
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::DisplayHelp
                        | ErrorKind::DisplayVersion
                        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) =>
            {
                write!(out, "{}", e.render())?;
                return Ok(());
            }
            Err(e) => bail!("{label}: {}", e.render().to_string().trim_end()),
        }
    }

    let last = stages.len() - 1;
    for (i, (label, cmd)) in stages.iter().enumerate() {
        match cmd {
            Cmd::Run(_) => bail!("{label}: `run` cannot be nested"),
            Cmd::Table(_) if i != last => {
                bail!("{label}: `table` output is for reading and must be the last stage")
            }
            _ => {}
        }
    }

    let mut input = input;
    for (i, (label, cmd)) in stages.into_iter().enumerate() {
        if i == last {
            return execute(cmd, input, out).with_context(|| label);
        }
        let mut buffer = Vec::new();
        execute(cmd, input, &mut buffer).with_context(|| label)?;
        input = Input::Bytes(buffer);
    }
    Ok(())
}

/// Splits on `|` outside single or double quotes.
fn split_pipes(line: &str) -> Result<Vec<String>> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in line.chars() {
        match (quote, c) {
            (None, '|') => segments.push(std::mem::take(&mut current)),
            (None, '\'' | '"') => {
                quote = Some(c);
                current.push(c);
            }
            (Some(q), _) if c == q => {
                quote = None;
                current.push(c);
            }
            _ => current.push(c),
        }
    }
    if quote.is_some() {
        bail!("unbalanced quotes in pipeline");
    }
    segments.push(current);
    if segments.iter().all(|s| s.trim().is_empty()) {
        bail!("empty pipeline");
    }
    Ok(segments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record;

    fn run_on(line: &str, input: &str) -> Result<String> {
        let mut out = Vec::new();
        run(line, Input::Bytes(input.as_bytes().to_vec()), &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    const INPUT: &str = r#"{"no":1310,"text":"a|b","amount":10.10}
{"no":1310,"text":"c","amount":5}
{"no":5820,"text":"Husleje","amount":12500}
"#;

    #[test]
    fn splits_pipes_outside_quotes() {
        let parts = split_pipes(r#"where 'text=a|b' | sum amount"#).unwrap();
        assert_eq!(parts, ["where 'text=a|b' ", " sum amount"]);
    }

    #[test]
    fn runs_filter_pipeline() {
        let out = run_on("where 'amount<100' | sum amount --by no", INPUT).unwrap();
        let recs = record::read_ndjson(out.as_bytes()).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0]["sum"].to_string(), "15.10");
    }

    #[test]
    fn accepts_billy_prefix_and_quoted_pipe() {
        let out = run_on("billy where 'text=a|b' | billy count", INPUT).unwrap();
        assert_eq!(out.trim(), r#"{"count":1}"#);
    }

    #[test]
    fn reports_failing_stage() {
        let err = run_on("sort no | sum text", INPUT).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.starts_with("stage 2 (`sum text`)"), "{msg}");
        let err = run_on("table | count", INPUT).unwrap_err();
        assert!(format!("{err:#}").contains("must be the last stage"));
        let err = run_on("count --bogus", INPUT).unwrap_err();
        assert!(format!("{err:#}").contains("--bogus"));
    }
}
