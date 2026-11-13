# Output and exit codes

This page describes exactly what `sqlx-sqruff` prints and how it exits, for
scripts that read its output.

Results (findings, the output of `fix`, `list` and `dump`) go to stdout. The
`error:`, `warning:` and `note:` lines and the summary line go to stderr. The
samples below come from a `src/db.rs` file with one `query_as!` call.

## Findings (`check`)

`check` prints one line per finding:

```text
path:line:col: CODE message
```

With `--format jsonl`, it prints one JSON object per line instead (JSON Lines),
with the keys `file`, `line`, `col` (1-based, in characters), `code` and
`message`:

```json
{"code":"CP01","col":9,"file":"src/db.rs","line":5,"message":"Keywords must be upper case."}
```

`message` is sqruff's, except that a line number it quotes from the SQL (as in
`AL08`'s `Reuse of column alias 'a' from line 6.`) is given as the line of the
`.rs` file, like `line`.

`CODE` is the sqruff rule code (`CP01`, `LT01`, ...), or one of these codes for
a finding that is not a rule violation:

| Code | Meaning |
| ---- | ------- |
| `PARSE` | sqruff could not parse the SQL with the configured dialect. |
| `LEX` | sqruff could not split the SQL into tokens, for example at an unclosed quote. sqruff then also reports `PARSE` at the same place. |
| `NOQA` | A `-- noqa` comment that sqruff reports as malformed (see below). |
| `INTERNAL` | sqruff failed on the query, for example on an invalid inline `-- sqruff:` config line, or one of its rules failed. The message says why. |

A `NOQA` finding is `noqa` followed by something other than `:` (such as
`-- noqa?` or `-- noqa CV05`), or `-- noqa:` with nothing after the colon. A
comment that names no rule sqruff knows, such as `-- noqa:disable` (without
`=`) or `-- noqa: foo=CV05`, is accepted without a finding and silences
nothing. When sqruff's `warn_unused_ignores = True` is set, every `noqa`
comment that silences nothing is reported too, those included.

## Output of fix

With `--check`, `fix` prints, for each file it would change, a
`would fix N query(ies) in <path>` line, then each query it would change: its
`path:line`, the current literal with every line prefixed by `- `, then the
fixed literal with every line prefixed by `+ `:

```text
would fix 1 query(ies) in src/db.rs
src/db.rs:4
- r#"
-         select id, name as "name!: String"
-         from users u
-         where active = true
-         "#
+ r#"
+         SELECT id, name AS "name!: String"
+         FROM users
+         WHERE active = true
+         "#
```

Without `--check`, it prints a `fixed N query(ies) in <path>` line for each
file it writes.

## Paths

Every path the tool prints (in findings, `list`, `dump`, the output of `fix`,
warnings and errors) is spelled the same however you named it on the command
line: relative to the current directory, without a leading `./`, when the file
is inside it, and absolute otherwise. Paths use the separator of the operating
system, so `src\a.rs` on Windows.

## Order

Every command prints its results sorted by path, then line, then column. Paths
are compared one component at a time, so `src/m/x.rs` comes before `src/m.rs`,
and absolute paths come before relative ones. The `error:` lines on stderr are
sorted the same way, by the path they name, so naming the same paths in
another order prints the same lines. The `warning:` and `note:` lines about a
file come when that file is processed.

## Summary line

`check` and `fix` end with a summary line on stderr:

- `N finding(s); E error(s).` for `check`;
- `Q query(ies) in F file(s); N not applied; E error(s).` for `fix`, where with
  `--check` `Q` and `F` count what `fix` would change.

`E` is the number of `error:` lines printed before it, each of which makes the
run exit `2`.

## Exit codes

| Exit code | `check` | `fix --check` | `fix` |
| --------- | ------- | ------------- | ----- |
| `0` | no findings | nothing to change | no error, even when a change could not be applied |
| `1` | one or more findings | a file would change, or a needed change could not be applied | never |
| `2` | an error | an error | an error |

Plain `fix` prints a `warning:` line for each change it could not apply, and
still exits `0`. `list` and `dump` exit `0`, or `2` on an error.

### Errors

Every command exits `2` on these errors. They fall into two groups, which
differ in whether the run goes on.

**Errors that stop the run.** Only the `error:` lines are printed, without a
summary:

- A path given on the command line that does not exist. This is checked before
  any work, and each missing path is named.
- An invalid config, such as an unknown dialect (from the config or from
  `--dialect`), a `--config` file that cannot be read, or an invalid value in
  the `[sqlx-sqruff]` section.
- An invalid argument, such as an unknown flag, or `--dialect` given to `list`
  or `dump`.

**Errors after which the run goes on.** The run goes on with the other paths
and files, `check` and `fix` still print their summary, which counts the
errors, and the exit code is `2` at the end:

- A path that selects no file to scan, so that a CI step cannot pass while
  checking nothing. Every directory you name must hold at least one `.rs` file
  that the
  [file selection](configuration.md#selecting-which-files-to-scan) keeps. One
  that does not (for example an empty directory, or one whose files `exclude`
  or `.gitignore` prune) is named on an `error: <path>: no Rust files found`
  line, or, when the directory cannot be read, on a line that gives the
  reading error instead. With no path named, the same applies to the current
  directory. When an `include` matches nothing, the error names the config
  file and the directory its patterns are relative to.
- A file or directory that cannot be read (for example a file that is not
  UTF-8), a file that is not valid Rust, or a fixed file that cannot be
  written. Each such path is named on one `error: <path>: ...` line, or
  `error: <path>:<line>:<col>: ...` for a file that is not valid Rust. A file
  you name on the command line is always parsed as Rust; a file found by
  walking a directory is parsed only when it contains the text `query`, so one
  without it is never reported as invalid Rust (see
  [Unparsable Rust is an error for the whole file](limitations.md#unparsable-rust-is-an-error-for-the-whole-file)).
