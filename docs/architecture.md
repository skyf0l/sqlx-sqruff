# Architecture

The workspace has two crates.

`crates/core` (`sqlx-sqruff-core`, the library) runs a query through four
modules:

1. `extract`: finds the queries with `syn` and turns each `proc-macro2` span
   into the byte span of its string literal.
2. `literal`: reads the literal's value with the source position of every
   character, and builds the SQL view given to the linter.
3. `sqruff_adapter`: the only file that uses `sqruff-lib`.
4. `engine`: check and fix, and writing the fixed literal back, with a value
   check on every rewritten literal and a re-parse of the whole file.

`crates/cli` (`sqlx-sqruff`, the binary) is the clap front end: file
selection, the `[sqlx-sqruff]` config section and exit codes.

[AGENTS.md](../AGENTS.md), for contributors human or agent, describes each
module in more detail, lists the checks to run and the rules `fix` must keep.
