# Known limitations

This page lists exactly which queries `sqlx-sqruff` reads, and the cases where
it reads or rewrites a query differently from what you might expect.

Apart from the imports a bare name needs, a query's location does not matter.
A `sqlx::query!` or `sqlx::query("...")` inside a function body, `impl`,
closure, `macro_rules!` body, or the arguments of another macro (such as
`assert!`, `vec!`, `tokio::try_join!` or `tokio::select!`) is picked up just
the same.

## Which queries are read

### What is extracted

The `sqlx::query*!` macros (`query!`, `query_as!`, `query_scalar!`, and their
`_unchecked` variants), and the runtime functions `sqlx::query`, `query_as`,
`query_scalar` and their `_with` variants, for example `sqlx::query("...")` or
`sqlx::query_as::<_, T>("...")`. `sqlx::raw_sql`, `QueryBuilder` and SQL passed
straight to an executor method such as `.execute("...")` are not extracted.

### The SQL must be a single string literal written in the call

The query is read from one string literal, so a query string assembled some
other way (several literals joined with `+`, as in
`sqlx::query!("SELECT a " + "FROM t")`, a `concat!(...)`, a `const`, a
variable, `format!`, `AssertSqlSafe(...)`, or runtime concatenation) is not
extracted: `check`, `fix`, `list` and `dump` all skip such calls without a
warning. The SQL is the first argument, or for `query_as!` and
`query_as_unchecked!` the argument after the record type, so bind-argument
literals that follow the SQL, including `.bind("...")`, are correctly left
alone.

### A bare name must be imported from sqlx

A macro or call written without the `sqlx::` prefix, such as `query!(...)` or
`query("...")`, is extracted only when that name is imported from sqlx in the
same module or in an enclosing block (`use sqlx::query;`,
`use sqlx::{query, query_as};`, `use sqlx::query as q;` or `use sqlx::*;`),
because other crates export functions and macros with the same names. An
import does not reach other modules, including the modules nested in its own,
and a local item with the same name hides a `use sqlx::*;`. A bare macro also
counts when the same file has `#[macro_use] extern crate sqlx;` at its top
level. Method calls such as `client.query("...")` are never extracted.

### A local variable hides an imported function of the same name

After `use sqlx::query;`, the call in
`let query = |text| lookup(text); query("...")` is not extracted.

- A `let` hides the import from the next statement to the end of its block.
- A function or closure parameter hides it for the whole body.
- A binding in a `match` arm, `if let`, `while let` or `for` pattern hides it
  for that arm or body.
- A variable in an inner block does not hide the import outside that block.
- As in Rust, an item declared inside a block (a `fn`, `impl`, `trait`,
  `const` or `static`) does not see the variables of the blocks around it, so
  they do not hide the import inside that item, while a closure does see them.

`sqlx::query("...")` written with its path is still extracted. Variables do
not hide macros, so `query!(...)` still counts.

### A crate alias counts as sqlx

After `use sqlx as db;` (in the same module or an enclosing block) or
`extern crate sqlx as db;` (at the top level of the same file),
`db::query!(...)` and `db::query("...")` are extracted like their `sqlx::`
forms.

### sqlx re-exported under another path counts

A path whose last two segments are `sqlx` and a query function or macro name
counts, whatever comes before them: `crate::db::sqlx::query!(...)`,
`::app::sqlx::query("...")`. So does an import through such a path
(`use crate::db::sqlx::query;`). A module of your own named `sqlx` with a
function named `query` is therefore taken for sqlx. An alias of sqlx reached
through a longer path, such as `crate::db::query(...)` when `db` is sqlx
renamed, is not recognised.

### Macros whose arguments are not Rust expressions are searched by their tokens

In a macro such as
`tokio::select! { r = sqlx::query!("...").fetch_one(&pool) => r }`, every
`PATH!(...)` and `PATH(...)` among its tokens is checked with the rules above,
and each `{ ... }`, `( ... )` or `[ ... ]` group that parses as a Rust block or
list of expressions is read as Rust. Variables that the macro's own syntax
binds (the `r` in `r = ...`) are not known there, so they do not hide an
imported function.

### In a macro_rules! body, only a path ending in sqlx::NAME counts

Such as `sqlx::query!(...)`, `::sqlx::query("...")` or
`$crate::db::sqlx::query!(...)`. A bare name or a crate alias is not
recognised there, because the paths in a macro body are resolved where the
macro is used, not where it is defined. The SQL must be a string literal
written in the body: `sqlx::query!($sql)` has no SQL to read.

### SET statements are skipped

A query whose SQL starts with the word `SET` followed by a space (in any case,
after any leading whitespace), such as `SET LOCAL app.user_id = ...`, is
neither linted nor fixed.

### Unparsable Rust is an error for the whole file

If `syn` cannot parse a file, none of its queries are read. Every command
reports the file on an `error: <path>:<line>:<col>: cannot parse as Rust: ...`
line, at the position of `syn`'s error (for a bracket that is never closed,
that bracket; for an error at the end of the file, just after its last token),
goes on with the other files, and exits `2`. This holds for every file you name
on the command line, and for every file found by walking a directory that
contains the text `query`. A file found by walking a directory that does not
contain the text `query` is not parsed at all, because it cannot hold a query,
so it is never reported, even when it is not valid Rust.

## How the SQL is read and rewritten

### Normal strings with other escapes are not rewritten

A normal `"..."` string that uses escape sequences other than `\"` and `\\`
(for example `\n`, `\x41`, `\u{...}`, or a `\` line continuation) is linted,
but `fix` never rewrites it, and `fix --check` fails when such a query needs
changes. Write the query as a raw string (`r#"..."#`) to let `fix` format it.

### Nested indentation needs block layout

When the first SQL line sits right after the opening quote, the indent that
all following lines share is removed before linting and added back by `fix`,
so the least indented of those lines stays where you put it. If a layout rule
such as `LT02` wants every following line indented further, as in
`"SELECT a FROM t WHERE\n            b = 1"`, applying that would only grow the
shared indent on every run. `fix` then leaves the query unchanged, warns that
fixing it again would change it further, and counts it as not applied, so
`fix --check` fails and `check` keeps reporting the rule. Start the SQL on its
own line instead (`r#"`, newline, SQL) to let `fix` lay out the indentation.

A query whose SQL already starts on its own line, or is all on one line (even
when the closing quote is on the next line), gets this warning only when
sqruff's own fixes do not settle on it; the warning then says to disable one
of the rules it names instead.

### Whitespace-only lines reach sqruff as written

The Rust indentation is removed from each SQL line before linting and added
back by `fix`, but a line that holds only whitespace is left exactly as it is,
because it can be part of a SQL string value. A whitespace-only line outside a
string value, such as an indented blank line between two clauses, is therefore
reported by `LT01` as trailing whitespace, and `fix` empties it.
