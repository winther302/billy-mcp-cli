# billy-cli

Small, pipeable commands for the [Billy](https://www.billy.dk) accounting API,
in the Unix tradition: each command does one thing, and commands pass
newline-delimited JSON (one record per line) through pipes.

```sh
billy postings --account 1310 --period 2025-Q3 | billy sum signedAmount
billy invoices --unpaid | billy sort dueDate | billy pick invoiceNo,dueDate,grossAmount | billy table
```

It is built to be driven by people and by language models alike, and ships an
MCP server (`billy mcp`, see below). A model sends
one command string to `billy run`, which executes the pipeline in-process,
without a shell:

```sh
billy run 'postings --period 2025 | sum signedAmount --by accountNo | sort sum -r | head -n 5 | table'
```

> Unofficial. Not affiliated with or endorsed by Billy ApS or Shine.

## Install

```sh
cargo install --path .
```

Requires Rust 1.87 or newer.

## Authentication

Create an access token in Billy under **Settings → Access tokens**. `billy`
reads it from `BILLY_KEY` (or `billy_key`, `BILLY_ACCESS_TOKEN`) in the
environment, or from a `.env` file in the current directory or any parent:

```sh
billy_key=your-token-here
```

A token belongs to one company and does not expire. Treat it like a password.

## Commands

**Sources** fetch records from Billy:

| Command | Records |
|---|---|
| `org` | The company the token belongs to |
| `accounts` | Chart of accounts, sorted by number (`--archived` to include archived) |
| `postings` | Approved postings, plus `accountNo` and `signedAmount` (debit positive, credit negative) |
| `invoices`, `bills` | Sales invoices and supplier bills (`--state`, `--unpaid`, `--paid`, `--contact`, `--q`) |
| `contacts` | Customers and suppliers (`--customers`, `--suppliers`) |
| `daybook` | Daybook (journal) transactions, including drafts (`--state`, `--api-type`) |
| `daybooks` | The daybooks drafts can go in |

Sources print a short set of default fields. Add `--all` for Billy's full
record. Date filters take one syntax everywhere: `--period 2025`, `2025-Q3`,
`2025-H1`, `2025-03`, `2025-03-14` or `2025-01-01..2025-06-30`, or `--from`/`--to`.

**Filters** read records on stdin and never call Billy:

| Command | Does |
|---|---|
| `where <cond>...` | Keep records matching every condition: `=` `!=` `>` `>=` `<` `<=` `~` (contains) |
| `pick <fields>` | Keep only these fields; dotted paths reach nested values |
| `sort <field> [-r]` | Sort, numerically when both values are numbers |
| `head [-n N]` | Keep the first N records |
| `count [--by f]` | Count records, optionally per value |
| `sum <f> [--by g]` | Exact decimal sum and count, optionally per value |
| `table` | Render as an aligned table (last stage only) |

Quote conditions containing `>` or `<` in a shell (`billy where 'amount>1000'`)
so they are not read as redirects. Inside `billy run` no quoting is needed.

Run `billy <command> --help` for every flag.

## Writing to Billy

Writes take three steps, so every change can be read before it is made:

1. `draft` and `approve` only *describe* changes, as records.
2. `apply` checks the whole batch against Billy (accounts exist and are not
   archived, lines balance, the period is not locked, the change is not
   already there) and prints what would happen.
3. `apply --confirm-cvr <CVR>` makes the changes, but only if the CVR matches
   the company the token belongs to.

```sh
# Dry run: nothing changes
billy draft --date 2026-03-01 --text 'Repair' --line 3410:1000 --line '5820:-1000:Bank' --voucher R-17 | billy apply

# Create the draft
billy draft ... | billy apply --confirm-cvr 12345678

# Approve drafts this tool created
billy daybook --state draft --api-type billy-cli | billy approve | billy apply --confirm-cvr 12345678
```

- `--line ACCOUNT:AMOUNT[:TEXT]`: positive amounts are debits, negative are
  credits, and the lines must sum to zero.
- Writes only ever create drafts or approve them. Nothing is deleted.
- Re-running is safe: a draft with the same marker (`--api-type`), date and
  voucher number is skipped.
- Every applied change is appended to `billy-audit.ndjson`, next to the `.env`
  holding the token, or at `BILLY_AUDIT_LOG`.

### Language models

Inside `billy run`, `apply --confirm-cvr` is refused unless the process was
started with `BILLY_RUN_WRITES=1`. A model can always prepare and dry-run
changes, but only a person can switch on writes for it.

## MCP server

`billy mcp` serves the same commands to AI assistants over the
[Model Context Protocol](https://modelcontextprotocol.io) (stdio). It offers a
single tool, `billy`, which takes a pipeline string and runs it like
`billy run`. The tool description carries the full command reference, so the
assistant needs no other setup.

Claude Code:

```sh
claude mcp add billy -e BILLY_KEY=your-token-here -- billy mcp
```

Claude Desktop and other clients (`claude_desktop_config.json` or equivalent):

```json
{
  "mcpServers": {
    "billy": {
      "command": "billy",
      "args": ["mcp"],
      "env": { "BILLY_KEY": "your-token-here" }
    }
  }
}
```

Writes stay off for the assistant: it can draft and dry-run changes, but
`apply --confirm-cvr` is refused unless you add `"BILLY_RUN_WRITES": "1"` to
the server's environment. Output over 100 KB is cut at a line boundary with a
note telling the assistant to narrow its query.

## Environment

| Variable | Purpose |
|---|---|
| `BILLY_KEY` | Access token (also read from `.env`) |
| `BILLY_RUN_WRITES=1` | Allow `apply --confirm-cvr` inside `billy run` |
| `BILLY_AUDIT_LOG` | Path of the audit log |
| `BILLY_PAGE_SIZE` | Smaller API pages (1–1000), for testing paging |

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
