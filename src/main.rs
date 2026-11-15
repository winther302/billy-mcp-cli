mod client;
mod commands;
mod env;
mod mcp;
mod notes;
mod period;
mod pipeline;
mod record;

use std::io::{self, Write};
use std::process::ExitCode;

use clap::Parser;
use commands::{Cli, Input};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    let result = commands::execute(cli.cmd, Input::Stdin, &mut out)
        .and_then(|()| out.flush().map_err(Into::into));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        // `billy postings | head` closes the pipe early; that is not an error.
        Err(e) if is_broken_pipe(&e) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("billy: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|c| c.downcast_ref::<io::Error>())
        .any(|io| io.kind() == io::ErrorKind::BrokenPipe)
}
