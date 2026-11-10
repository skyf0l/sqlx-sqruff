# AGENTS.md

Guidance for coding agents working on this repository. The user-facing behaviour
is documented in `README.md`; read its Usage and Known limitations sections
before changing behaviour, and update them in the same change.

## Layout

- `crates/core` (`sqlx-sqruff-core`, library): finds SQL string literals in Rust
  source and lints or rewrites them.
  - `extract.rs`: finds `sqlx::query*!` macros and `sqlx::query*(...)` calls with
    `syn`, with scope-aware handling of `use sqlx::...` imports and crate aliases.
  - `literal.rs`: parses a literal's source text into its value, with the source
    offset of every character, and builds the `LintView` (the SQL given to
    sqruff) together with the mapping back to source positions.
  - `engine.rs`: `check` maps findings to `.rs` line and column; `fix` rebuilds
    literals and splices them back.
  - `sqruff_adapter.rs`: the only module that uses `sqruff-lib`.
  - `line_index.rs`, `diagnostic.rs`: positions and output rendering.
- `crates/cli` (`sqlx-sqruff`, binary): argument parsing, file selection, the
  `[sqlx-sqruff]` config section (`config.rs`), output and exit codes.

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
  - sqruff could not fully parse its SQL (a fix built on a wrong parse can change
    what the query does);
  - it is a normal string using escapes other than `\"` and `\\`;
  - sqruff's change needs a different line layout (one-line literals stay on one
    line);
  - the rebuilt literal fails the value check.
- `check` and `fix` must agree: after `fix`, `check` must not report a finding
  that `fix` could have applied, and a second `fix` must change nothing. Add new
  literal shapes to `fix_converges_with_check_for_every_literal_shape`.
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

- The CLI follows `version` in the root `Cargo.toml`. The library has its own
  `version` in `crates/core/Cargo.toml` and stays `0.x`; it promises no API
  stability. The `sqlx-sqruff-core` dependency in the root `Cargo.toml` must name
  the library's version.
- Every release publishes both crates, so both versions must be new. The steps
  are in the README's Releasing section; the `Release` workflow checks the tag,
  the branch and both versions before publishing.
- `rust-version` (MSRV) is 1.95, because sqruff uses an API stabilised in that
  release. Keep it in sync with the `msrv` job in `.github/workflows/ci.yml`, and
  re-check it when upgrading sqruff.

## Public repository

This is a public open-source project. Do not mention other projects, companies,
hosts or people in code, comments, tests, commit messages or documentation.
