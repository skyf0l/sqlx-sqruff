# sqlx-sqruff

Lint and auto-format the SQL embedded inside `sqlx::query*!` macros in Rust
source, by embedding the [`sqruff`](https://github.com/quarylabs/sqruff) library
in-process.

## Why

When you write SQL with [`sqlx`](https://github.com/launchbadge/sqlx), it lives
inside `sqlx::query!` macros as Rust string literals, invisible to every SQL
formatter and linter you already use. Tools like sqruff, sqlfluff, or
pgFormatter only ever see `.sql` files, so the SQL embedded in your Rust code
never gets formatted or checked: keyword casing drifts, indentation rots, and
dead-code patterns slip past review.

`sqlx-sqruff` closes that gap. It finds the SQL inside your `query!` macros,
runs a real SQL linter/formatter over it, and writes the cleaned-up SQL back
into your `.rs` files, so your inline queries get the same consistency and
checks as standalone `.sql` files.

- **One formatter for all your SQL.** Inline `sqlx::query!` SQL is formatted
  and linted just like your `.sql` files: consistent keyword casing, indentation
  and layout, applied automatically with `fix`.
- **Catches real problems.** Unused CTEs and joins, `LIMIT` without
  `ORDER BY`, redundant `CASE` / `ELSE NULL`, and more, surfaced right inside
  your Rust source at `file:line:col` (human or JSON output).
- **Postgres-correct out of the box.** Understands sqlx-specific casts like
  `as "col!: Type"` that trip up ansi-default SQL tooling (any dialect via
  `--dialect`).
- **Fast and CI-friendly.** Embeds the `sqruff` library in-process (no
  subprocess per file) and ships a `--check` gate for pipelines.
- **Your rules, not ours.** Configured by your project's `.sqruff`; the tool
  embeds no presets of its own.

## Usage

```bash
sqlx-sqruff check  [PATHS...]              # lint, non-zero on findings
sqlx-sqruff check  [PATHS...] --format json  # structured diagnostics
sqlx-sqruff fix    [PATHS...]              # apply fixes, write back
sqlx-sqruff fix    [PATHS...] --check      # CI gate, no writes
sqlx-sqruff list   [PATHS...]              # debug: list extracted queries
sqlx-sqruff dump   [PATHS...]              # debug: print dedented SQL as the linter sees it
```

Shared flags on `check` / `fix`:

- `--config FILE`: explicit sqruff config (otherwise a `.sqruff` is auto-discovered).
- `--dialect NAME`: SQL dialect injected into the config (default `postgres`).

`fix` also takes `--all-literals` to fix single-line `"…"` literals (default:
only multi-line raw strings).

### Config resolution

Mirrors sqruff's own behaviour:

1. `--config FILE` (explicit), else
2. a `.sqruff` discovered in the cwd or any ancestor (**the recommended way**:
   your project owns and version-controls it), else
3. sqruff's built-in defaults.

The only value the tool injects is `dialect` (default `postgres`, overridable
with `--dialect`), because sqruff's `ansi` default mis-parses most postgres
sqlx queries.

Since the rules and `.sqruff` format are sqruff's, see its docs directly:

- [Configuration](https://playground.quary.dev/docs/usage/configuration/): `.sqruff` format, sections, per-rule options.
- [Rules reference](https://playground.quary.dev/docs/reference/rules/): every rule code (`CP01`, `LT01`, …) and what it does.

## Architecture

`crates/core` (library): `extract` (syn + proc-macro2 span → literal byte spans)
→ `literal` (dedent/reindent) → `sqruff_adapter` (the only file touching
`sqruff-lib`) → `engine` (check/fix + writeback with a re-parse safety
invariant). `crates/cli` is a thin clap front-end.

## Known limitations

- **Macros only, not the function API.** Only the `sqlx::query*!` **macros**
  (`query!`, `query_as!`, `query_scalar!`, and their `_unchecked` variants) are
  extracted. The runtime function forms (`sqlx::query("…")`,
  `sqlx::query_as::<_, T>("…")`) are plain function calls with no `!`, so they
  are silently ignored. Their SQL is typically built dynamically (`format!`,
  concatenation, conditional fragments), which can't be reliably extracted or
  safely auto-fixed in the first place.
- **The SQL must be a single string literal.** The query is read from one
  string literal, so a query string assembled some other way (a `concat!(…)`,
  a `const`, or runtime concatenation) is not extracted. Bind-argument literals
  that follow the SQL are correctly left alone.
- **Whole-file skip on unparsable Rust.** If `syn` can't parse a file, it is
  skipped with a warning (never silently), and no queries from it are linted.

Note: a macro's location doesn't matter. A `query!` inside a function body,
`impl`, or closure is picked up just the same.

## License

Licensed under either of

- Apache License, Version 2.0
  ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license
  ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
