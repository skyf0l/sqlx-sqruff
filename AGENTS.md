# AGENTS.md

Guidance for coding agents working on this repository. The user-facing behaviour
is documented in `README.md`; read its Usage and Known limitations sections
before changing behaviour, and update them in the same change.

## Layout

- `crates/core` (`sqlx-sqruff-core`, library): finds SQL string literals in Rust
  source and lints or rewrites them.
  - `extract.rs`: finds `sqlx::query*!` macros and `sqlx::query*(...)` calls with
    `syn`, with scope-aware handling of `use sqlx::...` imports, crate aliases
    and the local variables that hide an imported function. The tokens of
    macros whose arguments are not Rust expressions, and of `macro_rules!`
    bodies, are searched for `PATH!(...)` and `PATH(...)` with the same rules
    (only paths ending in `sqlx::NAME` inside `macro_rules!`).
  - `literal.rs`: parses a literal's source text into its value, with the source
    offset of every character, and builds the `LintView` (the SQL given to
    sqruff) together with the mapping back to source positions.
  - `engine.rs`: `check` maps findings to `.rs` line and column; `fix` rebuilds
    literals and splices them back.
  - `sqruff_adapter.rs`: the only module that uses `sqruff-lib`. It turns
    sqruff's codes for findings that are not rules (`????`, `LXR`) into the
    codes defined in `diagnostic.rs`.
  - `line_index.rs`, `diagnostic.rs`: positions and output rendering.
- `crates/cli` (`sqlx-sqruff`, binary): argument parsing, file selection, the
  `[sqlx-sqruff]` config section (`config.rs`), output and exit codes.
  `paths.rs` gives every printed path its one spelling; `collect_files` applies
  it to each file and sorts the files, so output stays in path order. It
  returns the errors it finds (walk errors, paths that select nothing) as
  entries of the same sorted list, which the commands report when they reach
  them, so the `error:` lines are in path order too. Missing named paths are
  reported, sorted, before any work, and stop the run.

The output formats, finding codes, path spelling and output order in the
README's Output section are promises to users' scripts: change them only on
purpose, and update that section and the tests in
`crates/cli/tests/output_format.rs` with them.

## Gates

Run these and judge them by exit status; all must pass before a change is done:

```bash
RUSTFLAGS='-D warnings' cargo test --workspace --locked
RUSTFLAGS='-D warnings' cargo clippy --workspace --all-targets --locked
nix run nixpkgs#actionlint -- .github/workflows/ci.yml .github/workflows/release.yml
```

When packaging matters, also run
`cargo publish --workspace --dry-run --locked --allow-dirty` with
`CARGO_TARGET_DIR` set to a new, empty directory: `target/package` keeps earlier
builds of the same version and can make a dry run fail or pass for the wrong
reason.

The code is not formatted with `rustfmt`. Match the style of the surrounding
code and do not reformat files you are not changing.

## Rules `fix` must keep

`fix` writes into users' source files, so these are hard requirements, each
covered by tests:

- A rewritten literal's value may differ from the original only by sqruff's
  change to the SQL. `has_value` re-parses every rebuilt literal with `syn` and
  drops the edit when its value is not the intended one; the whole file must
  still parse after all edits.
- Lines that sqruff leaves alone keep their exact bytes, including whitespace
  inside multi-line SQL string constants. The `LintView` indentation removal and
  the re-indentation in `engine.rs` must stay exact inverses.
- A literal is not rewritten, and is reported as not applied, when:
  - sqruff would change it but could not fully parse its SQL (a fix built on a
    wrong parse can change what the query does). An unparsable query that
    sqruff would not change is not counted: `fix --check` answers whether `fix`
    has work it would do or could not do, and `check` reports parse errors;
  - it is a normal string using escapes other than `\"` and `\\`;
  - sqruff's change needs a different line layout (one-line literals stay on one
    line);
  - the rebuilt literal fails the value check;
  - sqruff cannot fully parse the rebuilt literal's SQL, or one of its rules
    fails on it (a `PARSE`, `LEX` or `INTERNAL` finding in `recheck`);
  - fixing the rebuilt literal again would change it further.
- `check` and `fix` must agree: after `fix`, `check` must not report a finding
  that `fix` could have applied, and a second `fix` must change nothing. Add new
  literal shapes to `fix_converges_with_check_for_every_literal_shape`.
- `recheck` in `engine.rs` lints and fixes every rebuilt literal again before
  it is written, as the next run would. A rebuilt literal that sqruff still
  reports findings on is counted as not applied, with its rule codes, so the
  summary of `fix` matches what `fix --check` and `check` report afterwards.
  Keep that call the one place every rebuilt literal passes through.
- The rule codes of a literal reported as not applied (`layout_kept`,
  `partly_applied`, `unstable`, `unsettled`) are the fixable codes that
  `check` reports on the literal that stays in the file, taken from a lint run
  by `push_checked_codes`, not from sqruff's fix run: in a fix run, each rule
  sees the SQL after the fixes of the rules before it, so its codes can differ.
  An unstable literal whose SQL starts right after the opening quote and
  continues on more lines of SQL (`LintView::sql` has more than one line, so
  not just a closing quote on its own line) goes to `unstable` (the warning suggests starting
  the SQL on its own line); any other goes to `unsettled` (sqruff's own fixes
  do not settle, and the warning suggests disabling a rule).
- Literal parsing must match the compiler. Add new escape or delimiter cases to
  `content_matches_the_compiler_value`, which compares with `syn`'s
  `LitStr::value()`.

Before changing any of these, run the tool on a copy of a real sqlx codebase
(for example a clone of the public sqlx repository): `fix`, then
`fix --check` must report no further changes, and nothing may panic.

## sqruff

- `sqruff-lib` and `sqruff-lib-core` are pinned to an exact version and used only
  from `sqruff_adapter.rs`, so an upgrade touches one file.
- Read the pinned version's source before relying on any sqruff function or
  default; its documentation is thin and its behaviour changes between versions.
- sqruff panics on some invalid config values. Config loading runs inside
  `without_panicking`, which turns those panics into errors. Keep new
  config-loading calls inside it.
- The CLI's dialect order is: `--dialect`, then the config's `dialect`, then
  `postgres`. Dialect names are validated with sqruff's own `DialectKind`.
- sqruff gives parse errors, malformed `-- noqa` comments and failed rules the
  same code, so `code()` in `sqruff_adapter.rs` tells them apart by the start of
  sqruff's messages. Re-check those messages when upgrading sqruff. An
  unrecognised message falls back to `PARSE`, which blocks `fix`.
- `Finding::message` in `sqruff_adapter.rs` rewrites the SQL line number that
  `AL08`'s message quotes into the `.rs` line. When upgrading sqruff, search
  its rules for other messages that quote a position and add them there.

## Tests

- Reproduce a bug with a failing test before fixing it, and keep the test.
- Test CLI behaviour through the real binary in `crates/cli/tests/`. Every such
  test must set `HOME` and `XDG_CONFIG_HOME` to its temporary directory, because
  sqruff merges user-level config files into the config it uses.
- Pass an explicit sqruff config in tests (`--config`, a `.sqruff` in the test
  directory, or `SqruffEngine::from_source`), so sqruff's default rules or a
  developer's environment cannot change the result.
- Do not compare whole output strings or golden files. Assert the rules: exit
  status, that a line names `path:line`, that a file is unchanged, a JSON key.

## Versions and releases

- The CLI and the library share one version: `version` under
  `[workspace.package]` in the root `Cargo.toml`, which both crates inherit.
  Bumping it means also bumping the `version` of the `sqlx-sqruff-core`
  dependency under `[workspace.dependencies]` in the same file.
- From 1.0 on, the library follows semantic versioning for its public API: a
  breaking change to a public item of `sqlx-sqruff-core` needs a new major
  version.
- Every release publishes both crates, so the version must be new for both. The steps
  are in the README's Releasing section; the `Release` workflow checks the tag,
  the branch and both versions before publishing.
- `rust-version` (MSRV) is 1.95, because sqruff uses an API stabilised in that
  release. Keep it in sync with the `msrv` job in `.github/workflows/ci.yml`, and
  re-check it when upgrading sqruff.

## Public repository

This is a public open-source project. Do not mention other projects, companies,
hosts or people in code, comments, tests, commit messages or documentation.
