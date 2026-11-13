# Usage

This page describes every command and flag of `sqlx-sqruff`, and how `fix`
decides what to write. The output formats and exit codes are in
[output.md](output.md); config files and file selection are in
[configuration.md](configuration.md).

## Commands

```bash
sqlx-sqruff check  [PATHS...]                 # lint, non-zero on findings
sqlx-sqruff check  [PATHS...] --format jsonl  # one JSON object per finding (JSON Lines)
sqlx-sqruff fix    [PATHS...]                 # apply fixes, write back
sqlx-sqruff fix    [PATHS...] --check         # CI gate, no writes, shows each change
sqlx-sqruff list   [PATHS...]                 # debug: list extracted queries
sqlx-sqruff dump   [PATHS...]                 # debug: print each query's SQL as the linter sees it
```

`PATHS` are the files and directories to scan. With no path, the current
directory is scanned. How a named path combines with the ignore files and the
`[sqlx-sqruff]` section is described in
[Selecting which files to scan](configuration.md#selecting-which-files-to-scan).

`check` prints one line per finding, `path:line:col: CODE message`, or with
`--format jsonl` one JSON object per finding; see
[Findings](output.md#findings-check).

With `--check`, `fix` writes nothing and prints each change it would make; see
[Output of fix](output.md#output-of-fix).

## Flags

Shared flags on `check` and `fix`:

- `--config FILE`: an extra sqruff config file, applied on top of the config
  that sqruff discovers from the current directory. It does not replace it;
  see [Config resolution](configuration.md#config-resolution). Its
  `[sqlx-sqruff]` section, when it has one, also selects the files (see
  [Selecting which files to scan](configuration.md#selecting-which-files-to-scan)).
- `--dialect NAME`: the SQL dialect, overriding the config's `dialect` (see
  [Dialect](configuration.md#dialect)).

`list` and `dump` do not lint, so they take only `--config`, and only for the
file selection in its `[sqlx-sqruff]` section. Passing `--dialect` to them is
an argument error (exit `2`).

`sqlx-sqruff <command> --help` prints the flags of each command and its exit
codes.

## How fix edits a query

### Line shape

`fix` cleans every query in place and **preserves each literal's line shape**:
one-liners stay on one line, and multi-line raw blocks keep their block layout.
The line shape is yours to choose. To opt a query into block formatting, write
it across multiple lines in a raw string (`r#"`...`"#`) and run `fix` again; to
keep it compact, keep it on one line.

### When fix leaves a query unchanged

`fix` leaves a query unchanged, warns about it, and counts it as not applied
(so `fix --check` fails) when:

- sqruff wants to change it but cannot fully parse its SQL with the
  configured dialect, or one of sqruff's rules failed on it (an `INTERNAL`
  finding), because a fix computed from a partial parse may change what the
  query does. Check the dialect, or the SQL. A query that sqruff cannot parse
  but would not change is not counted, so `fix --check` passes it; `check`
  reports its `PARSE` or `LEX` finding and exits `1`.
- sqruff's fix needs a different line layout than the literal has, for example
  a line break inside a one-line literal. Write the query as a multi-line raw
  string (`r#"`...`"#`), or disable the rule.
- fixing the rewritten query again would change it further, so `fix` would
  rewrite it on every run. When the SQL starts right after the opening quote
  and continues on more lines of SQL, a layout rule can grow the indent of
  those lines on every run, and the warning says to start the SQL on its own
  line (see
  [Nested indentation needs block layout](limitations.md#nested-indentation-needs-block-layout)).
  Otherwise sqruff's own fixes do not settle on the query, for example when
  `ST09` swaps a join condition back and forth, or when `LT05` and `LT09` undo
  each other's change with a short `max_line_length`; the warning then says to
  disable one of the rules it names.

A normal `"..."` string with escapes other than `\"` and `\\` is never
rewritten either, and is also counted as not applied. `fix` reports it on a
`note:` line instead of a `warning:` line; see
[Normal strings with other escapes are not rewritten](limitations.md#normal-strings-with-other-escapes-are-not-rewritten).

### Checking the rewritten query

After rewriting a query, and before writing the file, `fix` lints and fixes the
rewritten query again:

- When sqruff cannot fully parse the rewritten SQL, or one of its rules fails
  on it, the rewrite is not written: `fix` warns that the fixed SQL no longer
  parses and counts the query as not applied.
- When sqruff still reports a finding there, because part of its fix needs a
  different line layout (for example the line breaks that `LT05` or `LT09` add
  to a one-line query), `fix` writes the part that fits, warns that the rest
  was not applied, and counts the query as not applied.

### Rules named in the warnings

The warnings about a line layout, and about a query that fixing again would
change, name the rules whose fix was not applied. These are the fixable rules
that `check` reports on the query as it stays in the file: the rewritten query
when `fix` wrote part of its change, and the original query when `fix` left it
unchanged. A later `fix --check` or `check` reports the same queries and rules;
`check` also reports the findings that sqruff cannot fix, which no warning
names.

### Writing files

`fix` writes each changed file through a temporary file in the same directory,
which then replaces the original, so an interrupted run never leaves a
half-written file. The file keeps its permissions, a file named through a
symbolic link is written through the link, and a read-only file is reported as
an error instead of being replaced.

### Fixes that can change a query

The fixes of some sqruff rules can change what a query does. They are listed,
with how to turn them off for `fix` only, in the README section
[Fixes that can change a query](../README.md#fixes-that-can-change-a-query).
