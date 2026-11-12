# sqlx-sqruff

Lint and auto-format the SQL embedded inside `sqlx::query*!` macros and
`sqlx::query*("...")` calls in Rust source, by embedding the [`sqruff`](https://github.com/quarylabs/sqruff) library
in-process.

## Why

When you write SQL with [`sqlx`](https://github.com/launchbadge/sqlx), it lives
inside `sqlx::query!` macros or `sqlx::query("...")` calls as Rust string
literals, invisible to every SQL
formatter and linter you already use. Tools like sqruff, sqlfluff, or
pgFormatter only ever see `.sql` files, so the SQL embedded in your Rust code
never gets formatted or checked: keyword casing drifts, indentation rots, and
dead-code patterns slip past review.

`sqlx-sqruff` closes that gap. It finds the SQL inside your `query!` macros
and `query("...")` calls, runs a real SQL linter/formatter over it, and writes the cleaned-up SQL back
into your `.rs` files, so your inline queries get the same consistency and
checks as standalone `.sql` files.

- **One formatter for all your SQL.** Inline sqlx SQL is formatted
  and linted just like your `.sql` files: consistent keyword casing, indentation
  and layout, applied automatically with `fix`.
- **Catches real problems.** Unused CTEs and joins, `LIMIT` without
  `ORDER BY`, redundant `CASE` / `ELSE NULL`, and more, surfaced right inside
  your Rust source at `file:line:col` (human or JSON Lines output).
- **Postgres-correct out of the box.** Understands sqlx-specific casts like
  `as "col!: Type"` that trip up ansi-default SQL tooling (any dialect via
  `--dialect`).
- **Fast and CI-friendly.** Embeds the `sqruff` library in-process (no
  subprocess per file) and ships a `--check` gate for pipelines.
- **Your rules, not ours.** Configured by your project's `.sqruff`; the tool
  embeds no presets of its own.

## Install

```bash
cargo binstall sqlx-sqruff           # prebuilt binary, via cargo-binstall
cargo install sqlx-sqruff --locked   # from source, requires Rust 1.95 or newer
```

Prebuilt binaries for Linux, macOS and Windows (x86_64 and aarch64, except
Windows aarch64) are attached to each
[GitHub release](https://github.com/skyf0l/sqlx-sqruff/releases), with a
`.sha256` checksum next to each archive.

## Usage

```bash
sqlx-sqruff check  [PATHS...]              # lint, non-zero on findings
sqlx-sqruff check  [PATHS...] --format jsonl  # one JSON object per finding (JSON Lines)
sqlx-sqruff fix    [PATHS...]              # apply fixes, write back
sqlx-sqruff fix    [PATHS...] --check      # CI gate, no writes, shows each change
sqlx-sqruff list   [PATHS...]              # debug: list extracted queries
sqlx-sqruff dump   [PATHS...]              # debug: print each query's SQL as the linter sees it
```

Shared flags on `check` / `fix`:

- `--config FILE`: an extra sqruff config file, applied on top of the config
  that sqruff discovers (it does not replace it; see
  [Config resolution](#config-resolution)).
- `--dialect NAME`: SQL dialect, overriding the config's `dialect` (see
  [Config resolution](#config-resolution)).

`list` and `dump` do not lint, so they take only `--config`, and only for the
file selection in its `[sqlx-sqruff]` section (see
[Selecting which files to scan](#selecting-which-files-to-scan)). Passing
`--dialect` to them is an argument error (exit `2`).

With `--check`, `fix` prints each query it would change: its `path:line`, the
current literal with every line prefixed by `- `, then the fixed literal with
every line prefixed by `+ `.

`check` prints one line per finding, `path:line:col: CODE message`, or with
`--format jsonl` one JSON object per finding; see [Output](#output).

Exit codes:

- `check` exits `1` when there are findings, `0` otherwise.
- `fix --check` exits `1` when a file would change or when a needed change could
  not be applied, `0` otherwise. Plain `fix` prints a warning for each change it
  could not apply, and still exits `0`.
- Every command exits `2` on errors:
  - a path given on the command line that does not exist. This is checked
    before any work, and each missing path is named.
  - no file to scan: the paths you name, or the current directory, hold no
    `.rs` file that the [file selection](#selecting-which-files-to-scan)
    keeps, for example an empty directory or an `include` that matches
    nothing. The error names the paths searched, so a CI step cannot pass
    while checking nothing.
  - a file or directory that cannot be read (for example a file that is not
    UTF-8), a file that is not valid Rust (see *Unparsable Rust is an error for
    the whole file* under [Known limitations](#known-limitations)), or a fixed
    file that cannot be
    written. Each such path is named on an `error: <path>: ...` line, the run
    goes on with the other files and still prints its summary, and the exit
    code is `2` at the end.
  - an invalid config, such as an unknown dialect, a `--config` file that
    cannot be read, or an invalid value in the `[sqlx-sqruff]` section.
  - an invalid argument, such as an unknown flag.

`fix` cleans every query in place and **preserves each literal's line-shape**:
one-liners stay on one line, multi-line raw blocks keep block layout. Line-shape
is yours to choose. To opt a query into block formatting, write it across
multiple lines in a raw string (`r#"`...`"#`) and re-run `fix`; to keep it compact,
keep it on one line.

`fix` leaves a query unchanged, warns about it, and counts it as not applied
(so `fix --check` fails) when:

- sqruff cannot fully parse its SQL with the configured dialect, or one of
  sqruff's rules failed on it (an `INTERNAL` finding), because a fix computed
  from a partial parse may change what the query does. Check the dialect, or
  the SQL.
- sqruff's fix needs a different line layout than the literal has, for example
  a line break inside a one-line literal. Write the query as a multi-line raw
  string (`r#"`...`"#`), or disable the rule.
- fixing the rewritten query again would change it further, so `fix` would
  rewrite it on every run. See *Nested indentation needs block layout* under
  [Known limitations](#known-limitations).

After rewriting a query, and before writing the file, `fix` lints and fixes the
rewritten query again. When sqruff still reports a finding there, because part
of its fix needs a different line layout (for example the line breaks that
`LT05` or `LT09` add to a one-line query), `fix` writes the part that fits,
warns that the rest was not applied, and counts the query as not applied. The
warnings name the rule codes sqruff still reports, and a later `fix --check`
or `check` reports the same queries and rules.

`fix` writes each changed file through a temporary file in the same directory
that then replaces the original, so an interrupted run never leaves a
half-written file. The file keeps its permissions, a file named through a
symbolic link is written through the link, and a read-only file is reported as
an error instead of being replaced.

### Output

`check` prints one line per finding: `path:line:col: CODE message`. With
`--format jsonl`, it prints one JSON object per line instead (JSON Lines), with
the keys `file`, `line`, `col` (1-based, in characters), `code` and `message`.

`CODE` is the sqruff rule code (`CP01`, `LT01`, ...), or one of these codes for
a finding that is not a rule violation:

| Code | Meaning |
| ---- | ------- |
| `PARSE` | sqruff could not parse the SQL with the configured dialect. |
| `LEX` | sqruff could not split the SQL into tokens, for example at an unclosed quote. sqruff then also reports `PARSE` at the same place. |
| `NOQA` | a `-- noqa` comment that sqruff cannot read, or, when sqruff's `warn_unused_ignores` is set, one that silences nothing. |
| `INTERNAL` | sqruff failed on the query, for example on an invalid inline `-- sqruff:` config line, or one of its rules failed. The message says why. |

Every path the tool prints (in findings, `list`, `dump`, the output of `fix`,
warnings and errors) is spelled the same however you named it on the command
line: relative to the current directory, without a leading `./`, when the file
is inside it, and absolute otherwise. Paths use the separator of the operating
system, so `src\a.rs` on Windows.

Every command prints its results sorted by path, then line, then column. Paths
are compared one component at a time, so `src/m/x.rs` comes before `src/m.rs`,
and absolute paths come before relative ones.

### Config resolution

Mirrors sqruff's own behaviour. sqruff reads every config file it finds in the
places below and stacks them, so a value set in a later file overrides the same
value from an earlier one:

1. your user config directory: `~/.config/sqruff` when it exists, otherwise
   `$XDG_CONFIG_HOME/sqruff` on Linux, `~/Library/Application Support/sqruff`
   on macOS, or `%LOCALAPPDATA%\sqruff\sqruff` on Windows,
2. your home directory,
3. each ancestor of the current directory below your home directory (or below
   the filesystem root, when the current directory is outside your home
   directory), outermost first,
4. the current directory,
5. `--config FILE`, applied on top of all of them.

In each of those directories sqruff reads `.sqlfluff`, `.sqruff`,
`.sqruff.ini`, `pyproject.toml` and `sqruff.toml`, in that order. Discovery
starts from the current directory, not from the files being checked. A
`.sqruff` at your project root is **the recommended way**: your project owns
and version-controls it. With no config at all, sqruff's built-in defaults
apply.

The only value the tool injects is `dialect`, resolved in this order:

1. `--dialect NAME`, else
2. the `dialect` set in the config, else
3. `postgres`, because sqruff's own `ansi` default mis-parses most postgres
   sqlx queries.

If your queries target another database, for example MySQL or SQLite with `?`
placeholders, set `dialect` in your `.sqruff`. An unknown dialect name, from
the config or the flag, is reported as an error.

### Skipping a query

sqruff's inline `noqa` comments work inside the SQL: end a line with
`-- noqa` to silence every rule on that line, or `-- noqa: CV05` for one rule.

Since the rules and `.sqruff` format are sqruff's, see its docs directly:

- [Configuration](https://playground.quary.dev/docs/usage/configuration/): `.sqruff` format, sections, per-rule options.
- [Rules reference](https://playground.quary.dev/docs/reference/rules/): every rule code (`CP01`, `LT01`, ...) and what it does.

### Selecting which files to scan

By default every `.rs` file under the search roots (the paths you name, or the
current directory) is scanned, respecting `.gitignore`, `.ignore`,
`.git/info/exclude` and your global git excludes file. `.gitignore` files are
honoured outside a git repository too. Hidden files and directories (names
starting with `.`) are skipped, and symbolic links found while walking are not
followed; a path you name on the command line is always followed. Each file is
scanned once, even when two of the paths you name contain it.

The ignore files, `exclude` and the hidden-name rule prune only what the walk
finds inside a search root, never the root itself:

- A **file** you name is always scanned, whatever its extension (so
  `sqlx-sqruff check build/queries.txt` works), even when it is ignored or
  excluded.
- A **directory** you name is walked even when it is ignored, excluded or
  hidden. With `gen/` in `.gitignore`, `sqlx-sqruff check` and
  `sqlx-sqruff check .` skip `gen/`, but `sqlx-sqruff check gen` scans it, as
  does a run started inside `gen/`. A pattern that matches the files or
  directories inside it, such as `gen/**` or `*.gen.rs`, still prunes them.
- Files found by walking a directory are scanned only when they end in `.rs`.

To persist a project's scope, add a **`[sqlx-sqruff]`** section to your
`.sqruff`. sqruff itself ignores this section (it only reads `sqruff` /
`sqruff:*`), so it is consumed solely by this CLI:

```ini
[sqruff]
dialect = postgres

# CLI-only: which .rs files this tool walks.
[sqlx-sqruff]
# Whitelist: comma-separated globs, or one per indented line.
include =
    src/**/*.rs
    crates/**/*.rs
# Always pruned.
exclude = **/generated/**
# The default; set it to false to ignore .gitignore and the other ignore files.
respect_gitignore = true
```

Where the section is read from:

- The `--config FILE` file, when it has a `[sqlx-sqruff]` section.
- Otherwise the nearest `.sqruff.ini`, `.sqruff` or `.sqlfluff` file that has
  one, looking in the current directory and then in each parent directory.
  Within one directory, `.sqruff.ini` is looked at first, then `.sqruff`, then
  `.sqlfluff`, because that is the order in which sqruff lets them override
  each other. A config file without the section, for example a nested
  `.sqruff` that only sets rules, does not hide the section of a file further
  up.
- With no such file, every `.rs` file is scanned as described above.

The section's format:

- Lines starting with `#` or `;` are comments, and so is the rest of a line
  after a `#` or `;` that follows whitespace (`a.rs  # note`). A `#` or `;`
  inside a glob, with no whitespace before it, is part of the glob.
- An indented line continues the value of the key above it, so a long glob
  list can span several lines.
- An unknown key is reported with a warning that names the config file. An
  invalid `respect_gitignore` value is an error (exit `2`); it accepts `true`,
  `false`, `yes`, `no`, `on`, `off`, `1` and `0`.

Selection rules:

| `include` | `exclude` | Result |
| --------- | --------- | ------ |
| unset | unset | every `.rs` (respecting `.gitignore`) |
| set | unset | only files matching `include` |
| unset | set | every `.rs` minus `exclude` |
| set | set | `include` minus `exclude` |

- **Explicit positional paths win.** `sqlx-sqruff check src/foo` scans `src/foo`
  and ignores `include` (you've named your own scope); `exclude` and
  `.gitignore` still prune what is found inside it, as described above. A
  directly-named **file** is always scanned.
- Globs are gitignore-style (`**` spans directories), matched relative to the
  directory of the config file that defines them, whatever the current
  directory and the paths you name. With `exclude = src/generated/**`,
  `sqlx-sqruff check src` and a run from inside `src/` both skip
  `src/generated/`.

## Architecture

`crates/core` (library): `extract` (syn + proc-macro2 span -> literal byte spans)
-> `literal` (literal value with per-character source positions, and the SQL
view given to the linter) -> `sqruff_adapter` (the only file touching
`sqruff-lib`) -> `engine` (check/fix + writeback, with a value check on every
rewritten literal and a whole-file re-parse). `crates/cli` is the clap
front-end: file selection, the `[sqlx-sqruff]` config section and exit codes.

Contributors, human or agent: [AGENTS.md](AGENTS.md) lists the checks to run and
the rules `fix` must keep.

## Known limitations

- **What is extracted.** The `sqlx::query*!` macros (`query!`, `query_as!`,
  `query_scalar!`, and their `_unchecked` variants), and the runtime functions
  `sqlx::query`, `query_as`, `query_scalar` and their `_with` variants, for
  example `sqlx::query("...")` or `sqlx::query_as::<_, T>("...")`.
  `sqlx::raw_sql`, `QueryBuilder` and SQL passed straight to an executor method
  such as `.execute("...")` are not extracted.
- **The SQL must be a single string literal written in the call.** The query
  is read from one string literal, so a query string assembled some other way
  (several literals joined with `+`, as in `sqlx::query!("SELECT a " + "FROM t")`,
  a `concat!(...)`, a `const`, a variable, `format!`, `AssertSqlSafe(...)`, or
  runtime concatenation) is not extracted: `check`, `fix`, `list` and `dump`
  all skip such calls without a warning. Bind-argument literals that follow the
  SQL, including `.bind("...")`, are correctly left alone.
- **A bare name must be imported from sqlx.** A macro or call written without
  the `sqlx::` prefix, such as `query!(...)` or `query("...")`, is extracted
  only when that name is imported from sqlx in the same module or in an
  enclosing block (`use sqlx::query;`, `use sqlx::{query, query_as};`,
  `use sqlx::query as q;` or `use sqlx::*;`), because other crates export
  functions and macros with the same names. An import does not reach other
  modules, including the modules nested in its own, and a local item with the
  same name hides a `use sqlx::*;`. A bare macro also counts when the same
  file has `#[macro_use] extern crate sqlx;` at its top level. Method calls such as
  `client.query("...")` are never extracted.
- **A crate alias counts as sqlx.** After `use sqlx as db;` (in the same module
  or an enclosing block) or `extern crate sqlx as db;` (at the top level of
  the same file),
  `db::query!(...)` and `db::query("...")` are extracted like their `sqlx::`
  forms.
- **`SET` statements are skipped.** A query whose SQL starts with the word
  `SET` followed by a space (in any case, after any leading whitespace), such
  as `SET LOCAL app.user_id = ...`, is neither linted nor fixed.
- **Normal strings with other escapes are not rewritten.** A normal `"..."`
  string that uses escape sequences other than `\"` and `\\` (for example `\n`,
  `\x41`, `\u{...}`, or a `\` line continuation) is linted, but `fix` never
  rewrites it, and `fix --check` fails when such a query needs changes. Write
  the query as a raw string (`r#"..."#`) to let `fix` format it.
- **Nested indentation needs block layout.** When the first SQL line sits
  right after the opening quote, the indent that all following lines share is
  removed before linting and added back by `fix`, so the least indented of
  those lines stays where you put it. If a layout rule such as `LT02` wants
  every following line indented further, as in
  `"SELECT a FROM t WHERE\n            b = 1"`, applying that would only grow
  the shared indent on every run. `fix` then leaves the query unchanged, warns
  that fixing it again would change it further, and counts it as not applied,
  so `fix --check` fails and `check` keeps reporting the rule. Start the SQL on
  its own line instead (`r#"`, newline, SQL) to let `fix` lay out the
  indentation.
- **Whitespace-only lines reach sqruff as written.** The Rust indentation is
  removed from each SQL line before linting and added back by `fix`, but a line
  that holds only whitespace is left exactly as it is, because it can be part
  of a SQL string value. A whitespace-only line outside a string value, such as
  an indented blank line between two clauses, is therefore reported by `LT01`
  as trailing whitespace, and `fix` empties it.
- **Unparsable Rust is an error for the whole file.** If `syn` cannot parse a
  file, none of its queries are read. Every command reports the file on an
  `error: <path>: cannot parse as Rust: ...` line, goes on with the other
  files, and exits `2`. A file that does not contain the text `query` is not
  parsed at all, because it cannot hold a query, so it is never reported, even
  when it is not valid Rust.

Note: apart from the imports a bare name needs, a query's location doesn't
matter. A `sqlx::query!` or `sqlx::query("...")` inside a function body,
`impl`, closure, or the arguments of another macro (such as `assert!`, `vec!`
or `tokio::try_join!`, whenever those arguments parse as Rust expressions) is
picked up just the same.

## License

Licensed under either of

- Apache License, Version 2.0
  ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license
  ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

## Releasing

The CLI (`sqlx-sqruff`) and the library (`sqlx-sqruff-core`) have separate
versions: the CLI follows `version` in the root `Cargo.toml`, the library sets
its own in `crates/core/Cargo.toml` and stays `0.x`. Every release publishes
both, so both versions must be new.

1. Set the CLI `version` in the root `Cargo.toml`, the library `version` in
   `crates/core/Cargo.toml`, and the same library version on the
   `sqlx-sqruff-core` dependency in the root `Cargo.toml`. Merge to `main`.
2. Tag that commit `v<CLI version>` (for example `v1.0.0`) and push the tag.

The `Release` workflow then runs the full CI and checks, before publishing
anything, that:

- the tagged commit is on `main`,
- the tag matches the CLI version,
- neither crate's version is on crates.io yet, so a forgotten library version
  bump fails here instead of halfway through the publish.

It then publishes both crates to crates.io (library first) through trusted
publishing, creates the GitHub release and attaches the binaries.

If the `publish` job fails after the library was uploaded, for example on a
network error while uploading the CLI, open the workflow run and choose
**Re-run failed jobs**. That re-runs only the failed job and the jobs after
it. The `publish` job skips a crate whose version is already on crates.io, so
it resumes with the CLI. Do not choose **Re-run all jobs**: the version check
would then fail, because the library version is already published.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
