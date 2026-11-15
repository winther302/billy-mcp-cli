# billy-mcp-cli

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

Attach the bilag (PDF, PNG or JPEG) to a daybook transaction, or leave out
`--to` to put it in Billy's inbox:

```sh
billy attach invoice.pdf --to <daybook transaction id> | billy apply --confirm-cvr 12345678
billy attach receipt.jpg --to bill:<bill id> | billy apply --confirm-cvr 12345678
```

- `--line ACCOUNT:AMOUNT[:TEXT]`: positive amounts are debits, negative are
  credits, and the lines must sum to zero.
- `draft`, `approve` and `attach` only ever create drafts, approve them or add
  files. Deleting is possible only through `delete`, and needs `--allow-delete`.
- Re-running is safe: a draft with the same marker (`--api-type`), date and
  voucher number is skipped, as is an update that changes nothing, or a
  `create --unique` whose record already exists.
- Every applied change is appended to `billy-audit.ndjson`, next to the `.env`
  holding the token, or at `BILLY_AUDIT_LOG`.

### Everything else in the API

The commands above cover everyday bookkeeping. Generic commands reach every
other resource in Billy's API (`billy resources` lists all 48 and what each
supports): products, bank lines and matching, bank payments, VAT returns,
invoice lines, reminders, contact persons, tax rates and more.

| Command | Does |
|---|---|
| `list <resource> [key=value...]` | List records. Query parameters go to Billy as-is: filters, `include=invoice.lines:embed`, `sortProperty`, `sortDirection` |
| `get <resource> [id...]` | Fetch records by id, or the ids of piped records. `get organization` and `get user` need no id |
| `create <resource> FIELD...` | Describe a new record. `--unique f,g` skips it if a record with the same values exists |
| `update <resource> [id] FIELD...` | Describe changing fields, of one record or of every piped record. The dry run shows from and to |
| `delete <resource> [id...]` | Describe deleting records |
| `call METHOD path [FIELD...]` | Any endpoint, such as `invoices/<id>/emails`. `GET` reads at once; other methods go through `apply` |

FIELD is `key=text` or `key:=json` (numbers, `true`, arrays, objects), and
dotted keys nest. `--json '{...}'` gives the whole record at once.

```sh
billy list bankLines isMatched=false | billy table
billy create contacts name='Acme ApS' registrationNo=12345678 isCustomer:=true countryId=DK --unique registrationNo | billy apply
billy invoices --state draft | billy update invoices state=approved | billy apply --confirm-cvr 12345678
billy call POST invoices/<id>/emails email.contactPersonId=<id> email.emailSubject=Faktura email.emailBody='Se vedhæftet' \
  | billy apply --confirm-cvr 12345678 --allow-send
billy call GET 'invoiceLogs?invoiceId=<id>'
```

Two kinds of change need an extra flag on `apply`, because they cannot be
taken back: deleting (`--allow-delete`), and sending emails, e-invoices or
reminders to people outside the company (`--allow-send`).

`apply --bulk` sends the creates and updates of one resource as a single bulk
save (`PATCH /v2/<resource>` with every record), and its deletes as one bulk
delete (`DELETE /v2/<resource>?ids[]=…`). Each bulk request succeeds or fails as
a whole. Billy lists bulk operations as supported but does not document their
format; these follow its API conventions.

Line and child resources only list when filtered by their parent, e.g.
`list invoiceLines invoiceId=…` or `list bankLines accountId=…`; the
`listNeeds` field of `billy resources` names the filter. A company access token
cannot list `files`, `organizations` or `users`.

Billy ignores query parameters it does not know, so a misspelled filter
returns everything. Check the count before relying on a filter.

### Language models

Inside `billy run`, `apply --confirm-cvr` is refused unless the process was
started with `BILLY_RUN_WRITES=1`. A model can always prepare and dry-run
changes, but only a person can switch on writes for it. Deletes and sends also
need `BILLY_RUN_DELETES=1` and `BILLY_RUN_SENDS=1` respectively.

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
the server's environment (plus `BILLY_RUN_DELETES` / `BILLY_RUN_SENDS` for
deletes and sends). Output over 100 KB is cut at a line boundary with a
note telling the assistant to narrow its query.

## Environment

| Variable | Purpose |
|---|---|
| `BILLY_KEY` | Access token (also read from `.env`) |
| `BILLY_RUN_WRITES=1` | Allow `apply --confirm-cvr` inside `billy run` |
| `BILLY_RUN_DELETES=1` | Also allow deletes inside `billy run` |
| `BILLY_RUN_SENDS=1` | Also allow emails, e-invoices and reminders inside `billy run` |
| `BILLY_AUDIT_LOG` | Path of the audit log |
| `BILLY_API_BASE` | Send requests to another server, such as a mock, for testing (the token goes there too) |
| `BILLY_PAGE_SIZE` | Smaller API pages (1–1000), for testing paging |

## License

[MIT](LICENSE)
