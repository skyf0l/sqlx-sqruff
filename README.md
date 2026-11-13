# sqlx-sqruff

[![sqlx-sqruff on crates.io](https://img.shields.io/crates/v/sqlx-sqruff?label=sqlx-sqruff)](https://crates.io/crates/sqlx-sqruff)
[![sqlx-sqruff-core on crates.io](https://img.shields.io/crates/v/sqlx-sqruff-core?label=sqlx-sqruff-core)](https://crates.io/crates/sqlx-sqruff-core)
[![docs.rs](https://img.shields.io/docsrs/sqlx-sqruff-core)](https://docs.rs/sqlx-sqruff-core)
[![License: MIT OR Apache-2.0](https://img.shields.io/crates/l/sqlx-sqruff)](#license)

Lint and format the SQL inside your sqlx queries.

With [`sqlx`](https://github.com/launchbadge/sqlx), your SQL lives in Rust
string literals. SQL tools such as sqruff, sqlfluff or pgFormatter only read
`.sql` files, so they never see it: keyword case drifts, indentation rots, and
mistakes such as unused CTEs and joins, `LIMIT` without `ORDER BY`, or a
redundant `CASE` / `ELSE NULL` slip past review. `sqlx-sqruff` finds the SQL in
your `sqlx::query*!` macros and `sqlx::query*("...")` calls, checks it with the
[`sqruff`](https://github.com/quarylabs/sqruff) SQL linter, and writes the fixed
SQL back into your `.rs` files.

- **Your rules.** It reads your project's `.sqruff`; it has no presets of its own.
- **Postgres-correct by default.** It understands sqlx casts such as
  `as "col!: Type"`, which SQL tools that default to the `ansi` dialect misread.
  Any sqruff dialect works.
- **Fast and built for CI.** sqruff runs in the same process, not as a
  subprocess for each file. Findings are reported at `file:line:col`, as text
  or JSON Lines, with a non-zero exit code.

This README follows the steps of adopting the tool. The full reference is in
the [documentation](#documentation).

## Install

```bash
cargo binstall sqlx-sqruff           # prebuilt binary, via cargo-binstall
cargo install sqlx-sqruff --locked   # from source, requires Rust 1.95 or newer
```

Prebuilt binaries for Linux, macOS and Windows (x86_64 and aarch64, except
Windows aarch64) are attached to each
[GitHub release](https://github.com/skyf0l/sqlx-sqruff/releases), with a
`.sha256` checksum next to each archive.

## Check your queries

Add a `.sqruff` file at your project root. It is sqruff's own config file, and
your project owns and version-controls it:

```ini
[sqruff]
dialect = postgres
```

Then run `check` from the project root. It scans every `.rs` file under the
current directory, or under the paths you name (`sqlx-sqruff check src tests`),
and prints one line per finding:

```text
$ sqlx-sqruff check
src/db.rs:2:42: CP01 Keywords must be consistently upper case.
src/db.rs:2:65: LT01 Expected only single space before "ORDER". Found "   ".
src/db.rs:12:9: LT02 Line should not be indented.
3 finding(s); 0 error(s).
```

`--format jsonl` prints one JSON object per finding instead, with the keys
`file`, `line`, `col`, `code` and `message`. The rule codes are sqruff's; see
its [rules reference](https://playground.quary.dev/docs/reference/rules/). The
exact output format is in
[docs/output.md](https://github.com/skyf0l/sqlx-sqruff/blob/main/docs/output.md).

## Fix them

`fix --check` shows what `fix` would change, without writing anything:

```text
$ sqlx-sqruff fix --check
would fix 2 query(ies) in src/db.rs
src/db.rs:2
- "SELECT name from users WHERE active   ORDER BY name"
+ "SELECT name FROM users WHERE active ORDER BY name"
src/db.rs:10
- r#"
-         SELECT t.name FROM teams AS t
-           JOIN users AS u ON u.team_id = t.id
-         "#
+ r#"
+         SELECT t.name FROM teams AS t
+         JOIN users AS u ON u.team_id = t.id
+         "#
2 query(ies) in 1 file(s); 0 not applied; 0 error(s).
```

`sqlx-sqruff fix` then writes those changes into the files. It keeps the line
shape of each literal: a one-line query stays on one line, and a multi-line raw
string keeps its block layout. To have a query laid out as a block, write it
across several lines in a raw string (`r#"`...`"#`) and run `fix` again.

Each file is written through a temporary file in the same directory, so an
interrupted run never leaves a half-written file. When `fix` cannot apply a
change safely, it leaves the query as it is and warns; see
[When `fix` leaves a query unchanged](#when-fix-leaves-a-query-unchanged).

## Add it to CI

Install the tool with one of the commands above, then run either command from
the project root:

```bash
sqlx-sqruff check        # fails on any finding, including those fix cannot fix
sqlx-sqruff fix --check  # fails when fix would change a file, and shows the change
```

| Exit code | Meaning |
| --------- | ------- |
| `0` | No findings, nothing to change, and no error. |
| `1` | `check`: one or more findings. `fix --check`: a file would change, or a needed change cannot be applied. |
| `2` | An error, each reported on an `error:` line. |

A path that selects no `.rs` file is an error, so a CI step cannot pass while
checking nothing. Plain `fix` exits `0` even when it could not apply a change,
and warns about each one instead. All the errors are listed in
[docs/output.md](https://github.com/skyf0l/sqlx-sqruff/blob/main/docs/output.md#errors).

## Configure the rules

The rules and the `.sqruff` format are sqruff's; see its
[configuration docs](https://playground.quary.dev/docs/usage/configuration/).
Like sqruff, the tool stacks every config file it finds in your user config
directory, your home directory, and the current directory and its parents. The
lookup starts from the current directory, not from the files being checked.

- `--config FILE` adds a config file on top of the ones found; it does not
  replace them.
- The dialect is `--dialect NAME`, else the `dialect` in the config, else
  `postgres`. For MySQL or SQLite (`?` placeholders), set `dialect` in your
  `.sqruff`.
- When no config sets any sqruff option, sqruff's built-in default rules apply,
  and `check` and `fix` print a `note:` line saying so.

The exact lookup order is in
[docs/configuration.md](https://github.com/skyf0l/sqlx-sqruff/blob/main/docs/configuration.md#config-resolution).

## Choose which files to scan

By default, every `.rs` file under the paths you name (or the current
directory) is scanned, except hidden files and what `.gitignore` and the other
ignore files exclude. To set the scope for the whole project, add a
`[sqlx-sqruff]` section to your `.sqruff`. sqruff ignores it; only this tool
reads it:

```ini
[sqlx-sqruff]
include = src/**/*.rs, crates/**/*.rs
exclude = **/generated/**
```

Globs are gitignore-style and relative to the config file's directory. Paths
you name on the command line replace `include`; `exclude` and `.gitignore`
still prune what is found inside them, and a file you name is always scanned.
The other keys and rules are in
[docs/configuration.md](https://github.com/skyf0l/sqlx-sqruff/blob/main/docs/configuration.md#selecting-which-files-to-scan).

## When a finding stays

### Skip a query with `noqa`

sqruff's inline `noqa` comments work inside the SQL: end a line with `-- noqa`
to silence every rule on that line, or with `-- noqa: CV05` for one rule. A
comment that names a rule sqruff does not know, such as a misspelt one, is not
reported and silences nothing.

### When `fix` leaves a query unchanged

`fix` writes a change only when it is safe. Otherwise it leaves the query as it
is (or writes only the part of the fix that fits the literal's line layout),
prints a `warning:` or `note:` line, and counts the query as not applied, so
`fix --check` fails. This happens when:

- sqruff cannot fully parse the SQL, or one of its rules fails on it: check the
  dialect, or the SQL.
- The fix needs a different line layout, such as a line break in a one-line
  literal: write the query as a multi-line raw string, or disable the rule.
- Fixing the result again would change it further: start the SQL on its own
  line after the opening quote, or disable one of the rules the warning names.
- The literal is a normal string with escapes other than `\"` and `\\`: write
  it as a raw string (`r#"..."#`).

The details are in
[docs/usage.md](https://github.com/skyf0l/sqlx-sqruff/blob/main/docs/usage.md#when-fix-leaves-a-query-unchanged).

### Fixes that can change a query

With sqruff 0.44.0, the fixes of these rules can change what a query does, or
make the database reject it:

| Rule | What its fix can do |
| ---- | ------------------- |
| `AM02` | Rewrites `UNION` as `UNION DISTINCT`, which SQLite, T-SQL and Oracle reject. sqruff runs `AM02` with every dialect except `duckdb`, `exasol`, `postgres` and `sparksql`. ([sqruff#5291](https://github.com/quarylabs/sqruff/issues/5291)) |
| `CP02` | Changes the case of unquoted identifiers, including table names, which MySQL and MariaDB treat as case-sensitive by default on most Unix systems. |
| `RF03` | Qualifies a reference to the alias of an outer `unnest(...)` or `generate_series(...)` with the subquery's table (`w.id = k` becomes `w.id = w.k`, [sqruff#5288](https://github.com/quarylabs/sqruff/issues/5288)), and qualifies an `ORDER BY` reference to a quoted select alias (`ORDER BY "name"` becomes `ORDER BY t."name"`, [sqruff#5289](https://github.com/quarylabs/sqruff/issues/5289)). |
| `ST06` | Reorders select lists whose order is used by position: the column list of a CTE, a derived table or a `LATERAL` subquery, `CREATE TABLE ... AS SELECT` on postgres, and row-value comparisons such as `(a, b) IN (SELECT ...)`. ([sqruff#5290](https://github.com/quarylabs/sqruff/issues/5290)) |
| `ST07` | Rewrites only the first of chained `USING` joins (`... USING (k) ... USING (k)`), which PostgreSQL then rejects, and changes the columns that `SELECT *` returns. ([SQLFluff#8950](https://github.com/sqlfluff/sqlfluff/issues/8950)) |

When `fix` or `fix --check` runs with any of these rules enabled, it prints one
`warning:` line on stderr naming them. `AM02` counts only with a dialect that
sqruff runs it on, and `CP02` only with `mysql` or `mariadb`. `check` does not
warn, because it changes nothing.

Review the changes that `fix --check` shows, or that `fix` made, before you
commit them. To keep such a rule for `check` but not for `fix`, run
`sqlx-sqruff fix --config fix.sqruff` with this `fix.sqruff`:

```ini
[sqruff]
exclude_rules = ST06,ST07
```

`--config` overrides the same key of the discovered config, so also list the
rules your project's config already excludes.

### Queries that are not checked

- Only these are read: the `query!`, `query_as!` and `query_scalar!` macros and
  their `_unchecked` variants, and the `query`, `query_as` and `query_scalar`
  functions and their `_with` variants. `sqlx::raw_sql`, `QueryBuilder` and SQL
  passed straight to an executor method such as `.execute("...")` are not.
- The SQL must be one string literal written in the call. A query built with
  `+`, `concat!`, a `const`, a variable or `format!` is skipped without a
  warning.
- A bare `query!(...)` or `query("...")`, without `sqlx::`, counts only when the
  name is imported from sqlx (`use sqlx::query;`) in the same module or an
  enclosing block.
- Queries whose SQL starts with `SET` are skipped.
- A file that is not valid Rust is an error, and none of its queries are read.

`sqlx-sqruff list` prints every query the tool found, and `sqlx-sqruff dump`
prints each query's SQL as the linter sees it. All the rules, including crate
aliases, re-exports and macros, are in
[docs/limitations.md](https://github.com/skyf0l/sqlx-sqruff/blob/main/docs/limitations.md).

## Use it as a library

The engine is published as the `sqlx-sqruff-core` crate: it finds the SQL
literals in Rust source, and checks or fixes them with sqruff. From 1.0 on, it
follows semantic versioning for its public API. Its API docs are on
[docs.rs](https://docs.rs/sqlx-sqruff-core).

## Documentation

- [Usage](https://github.com/skyf0l/sqlx-sqruff/blob/main/docs/usage.md): commands, flags, and how `fix` rewrites a query.
- [Output and exit codes](https://github.com/skyf0l/sqlx-sqruff/blob/main/docs/output.md): the exact format of everything the tool prints, and every error.
- [Configuration](https://github.com/skyf0l/sqlx-sqruff/blob/main/docs/configuration.md): config file lookup, the dialect, `noqa`, and file selection.
- [Limitations](https://github.com/skyf0l/sqlx-sqruff/blob/main/docs/limitations.md): which queries are found, and which are not rewritten.

These links show the `main` branch, which can describe changes that are not
released yet. For the docs of your version, open the `docs` folder at its tag,
for example `https://github.com/skyf0l/sqlx-sqruff/tree/v1.0.0/docs`.

## License

Licensed under either of

- Apache License, Version 2.0
  ([LICENSE-APACHE](https://github.com/skyf0l/sqlx-sqruff/blob/main/LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license
  ([LICENSE-MIT](https://github.com/skyf0l/sqlx-sqruff/blob/main/LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

## Contribution

For human and agent contributors, [AGENTS.md](https://github.com/skyf0l/sqlx-sqruff/blob/main/AGENTS.md)
lists the checks to run and the rules `fix` must keep. See also the
[architecture](https://github.com/skyf0l/sqlx-sqruff/blob/main/docs/architecture.md)
and the [release steps](https://github.com/skyf0l/sqlx-sqruff/blob/main/docs/releasing.md).

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
