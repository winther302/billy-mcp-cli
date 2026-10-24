mod filters;
mod sources;
pub(crate) mod writes;

use std::io::{self, IsTerminal, Write};

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use serde_json::Value;

use crate::record;

const AFTER_HELP: &str = "\
Sources print one JSON record per line (NDJSON). Filters read records on stdin
and print records, so commands chain with pipes:

  billy postings --account 1310 --period 2025-Q3 | billy sum signedAmount
  billy invoices --unpaid | billy sort dueDate | billy pick invoiceNo,dueDate,balance | billy table
  billy accounts | billy where 'name~bank' | billy table

Quote conditions that contain > or < so the shell does not treat them as redirects.
Without a shell, `billy run` executes a whole pipeline in one call:

  billy run 'postings --period 2025 | sum signedAmount --by accountNo | sort sum -r | head -n 5 | table'

Writes take three steps. `draft` and `approve` only describe changes; `apply`
checks the whole batch against Billy and is a dry run unless given --confirm-cvr:

  billy draft --date 2025-07-03 --text 'Husleje juli' --line 5820:12500 --line 1310:-12500 | billy apply
  billy daybook --state draft --api-type billy-cli | billy approve | billy apply --confirm-cvr <CVR>

Inside `billy run`, apply only dry-runs unless BILLY_RUN_WRITES=1 is set.
Every applied change is logged to billy-audit.ndjson.";

#[derive(Parser, Debug)]
#[command(
    name = "billy",
    version,
    about = "Small, pipeable commands for the Billy accounting API",
    after_help = AFTER_HELP
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// source: the organization (company) the token belongs to
    Org(sources::OrgArgs),
    /// source: chart of accounts, sorted by account number
    Accounts(sources::AccountsArgs),
    /// source: approved postings, with accountNo and signedAmount (debit +, credit -)
    Postings(sources::PostingsArgs),
    /// source: sales invoices
    Invoices(sources::DocArgs),
    /// source: supplier bills
    Bills(sources::DocArgs),
    /// source: customers and suppliers
    Contacts(sources::ContactsArgs),
    /// source: daybook (journal) transactions, including drafts
    Daybook(sources::DaybookArgs),
    /// source: the daybooks (journals) drafts can go in
    Daybooks(sources::OrgArgs),

    /// filter: keep records matching every condition
    Where(filters::WhereArgs),
    /// filter: keep only the named fields
    Pick(filters::PickArgs),
    /// filter: sort records by a field
    Sort(filters::SortArgs),
    /// filter: keep the first N records
    Head(filters::HeadArgs),
    /// filter: count records, optionally per value of a field
    Count(filters::CountArgs),
    /// filter: add up a numeric field exactly, optionally per value of another field
    Sum(filters::SumArgs),
    /// filter: render records as an aligned text table (last stage only)
    Table(filters::TableArgs),

    /// write: describe one balanced draft (changes nothing until `apply`)
    Draft(writes::DraftArgs),
    /// write: turn daybook records into approval requests (changes nothing until `apply`)
    Approve(writes::ApproveArgs),
    /// write: check changes against Billy; dry run unless --confirm-cvr is given
    Apply(writes::ApplyArgs),

    /// serve these commands to AI assistants over MCP (stdio)
    Mcp,

    /// run a whole pipeline, e.g. 'postings --period 2025 | sum signedAmount'
    Run(RunArgs),
}

#[derive(clap::Args, Debug)]
pub struct RunArgs {
    /// Stages separated by |; each stage is a billy command without the `billy` prefix
    pub pipeline: String,
}

/// Where a command's input records come from.
pub enum Input {
    Stdin,
    Bytes(Vec<u8>),
}

impl Input {
    fn records(self, command: &str) -> Result<Vec<Value>> {
        match self {
            Input::Stdin => {
                let stdin = io::stdin();
                if stdin.is_terminal() {
                    bail!(
                        "`{command}` reads records from stdin; pipe a source into it, \
                         e.g. `billy accounts | billy {command} ...`"
                    );
                }
                record::read_ndjson(stdin.lock())
            }
            Input::Bytes(bytes) => record::read_ndjson(&bytes[..]),
        }
    }
}

pub fn execute(cmd: Cmd, input: Input, out: &mut dyn Write) -> Result<()> {
    let records = match cmd {
        Cmd::Org(a) => sources::org(a)?,
        Cmd::Accounts(a) => sources::accounts(a)?,
        Cmd::Postings(a) => sources::postings(a)?,
        Cmd::Invoices(a) => sources::invoices(a)?,
        Cmd::Bills(a) => sources::bills(a)?,
        Cmd::Contacts(a) => sources::contacts(a)?,
        Cmd::Daybook(a) => sources::daybook(a)?,
        Cmd::Daybooks(a) => sources::daybooks(a)?,

        Cmd::Where(a) => filters::where_(a, input.records("where")?)?,
        Cmd::Pick(a) => filters::pick(a, input.records("pick")?),
        Cmd::Sort(a) => filters::sort(a, input.records("sort")?),
        Cmd::Head(a) => filters::head(a, input.records("head")?),
        Cmd::Count(a) => filters::count(a, input.records("count")?),
        Cmd::Sum(a) => filters::sum(a, input.records("sum")?)?,
        Cmd::Table(a) => return filters::table(a, &input.records("table")?, out),

        Cmd::Draft(a) => writes::draft(a)?,
        Cmd::Approve(_) => writes::approve(input.records("approve")?)?,
        Cmd::Apply(a) => return writes::apply(a, input.records("apply")?, out),

        Cmd::Mcp => return crate::mcp::serve(),

        Cmd::Run(a) => return crate::pipeline::run(&a.pipeline, input, out),
    };
    record::emit(out, &records)
}
