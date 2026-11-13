# Configuration

`sqlx-sqruff` has no rule settings of its own. It uses sqruff's config files,
so the rules and their options are whatever your project's `.sqruff` says. It
reads one section of its own, `[sqlx-sqruff]`, to choose which files to scan.

## Config resolution

This mirrors sqruff's own behaviour. sqruff reads every config file it finds in
the places below and stacks them, so a value set in a later file overrides the
same value from an earlier one:

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
and version-controls it.

sqruff reads only its own sections (`[sqruff]`, `[sqruff:...]`, their
`sqlfluff` forms, and `[tool.sqruff]` or `[tool.sqlfluff]` in a TOML file).
When no config file sets an option there and no `--config` is given, sqruff's
built-in default rules apply, and `check` and `fix` print a `note:` line on
stderr saying so. This includes a project whose only config file holds just a
`[sqlx-sqruff]` section, and a run started outside the project: its `.sqruff`
is not found even when the paths you name are inside it, so the note also says
that config files are looked up from the current directory.

The `.sqruff` format, its sections and the per-rule options are sqruff's; see
its [Configuration](https://playground.quary.dev/docs/usage/configuration/)
docs, and its [Rules reference](https://playground.quary.dev/docs/reference/rules/)
for every rule code (`CP01`, `LT01`, ...) and what it does.

## Dialect

The only value the tool injects is `dialect`, resolved in this order:

1. `--dialect NAME`, else
2. the `dialect` set in the config, else
3. `postgres`, because sqruff's own `ansi` default mis-parses most postgres
   sqlx queries.

If your queries target another database, for example MySQL or SQLite with `?`
placeholders, set `dialect` in your `.sqruff`. An unknown dialect name, from
the config or the flag, is reported as an error (exit `2`).

## Skipping a query

sqruff's inline `noqa` comments work inside the SQL: end a line with
`-- noqa` to silence every rule on that line, or `-- noqa: CV05` for one rule.
sqruff does not report a comment that names no rule it knows, such as a
misspelt one; see the `NOQA` code in [output.md](output.md#findings-check).

## Selecting which files to scan

### Default

By default every `.rs` file under the search roots (the paths you name, or the
current directory) is scanned, respecting `.gitignore`, `.ignore`,
`.git/info/exclude` and your global git excludes file. `.gitignore` files are
honoured outside a git repository too. Hidden files and directories (names
starting with `.`) are skipped, and symbolic links found while walking are not
followed; a path you name on the command line is always followed. Each file is
scanned once, even when two of the paths you name contain it.

### Paths you name

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
  A directory you name in which nothing is selected is an error (exit `2`);
  the other paths are still scanned.
- Files found by walking a directory are scanned only when they end in `.rs`.
- Naming paths replaces `include`: `sqlx-sqruff check src/foo` scans `src/foo`
  and ignores `include`, because you have named your own scope. `exclude` and
  `.gitignore` still prune what is found inside it, as described above.

### The `[sqlx-sqruff]` section

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

| `include` | `exclude` | Result |
| --------- | --------- | ------ |
| unset | unset | every `.rs` (respecting `.gitignore`) |
| set | unset | only files matching `include` |
| unset | set | every `.rs` minus `exclude` |
| set | set | `include` minus `exclude` |

Globs are gitignore-style (`**` spans directories), matched relative to the
directory of the config file that defines them, whatever the current directory
and the paths you name. With `exclude = src/generated/**`,
`sqlx-sqruff check src` and a run from inside `src/` both skip
`src/generated/`.

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
