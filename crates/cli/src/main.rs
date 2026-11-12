//! `sqlx-sqruff` CLI: check / fix / list inline sqlx SQL.
//!
//! Config follows sqruff: the config files sqruff discovers (a project `.sqruff`
//! and the others it reads, in the user config directory, home, the cwd and its
//! ancestors) decide the rules, and `--config` is applied on top of them; with
//! none, sqruff's own defaults apply. The tool embeds no rule presets. The
//! dialect is the one injected default: `--dialect`, else the config's
//! `dialect`, else `postgres`, because sqruff's ansi default mis-parses most
//! postgres sqlx queries.
//!
//! File selection (which `.rs` files to scan) is driven by an optional
//! `[sqlx-sqruff]` section of a sqruff config file — see [`config`]. By default
//! every `.rs` file is scanned, respecting `.gitignore`.

mod config;
mod paths;

use std::{
    collections::{HashMap, hash_map::Entry},
    fmt::Display,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use config::CliSettings;
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use paths::shown;
use sqlx_sqruff_core::{
    engine::{QueryChange, check_extracted, dump_extracted, fix_extracted},
    extract::{QueryLiteral, extract_checked, may_hold_a_query},
    sqruff_adapter::SqruffEngine,
};

#[derive(Parser)]
#[command(
    name = "sqlx-sqruff",
    version,
    about,
    after_help = "Exit status:\n  \
                  0  no findings, nothing to change, and no error\n  \
                  1  check: one or more findings; fix --check: a file would change, or a\n     \
                  needed change cannot be applied\n  \
                  2  an error, each reported on an `error:` line (see each command's --help)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Lint inline SQL; non-zero exit if any findings.
    #[command(after_help = exit_status("  0  no findings\n  1  one or more findings", READ_ERRORS))]
    Check(CheckArgs),
    /// Apply fixes and write them back into the scanned files.
    ///
    /// The fixes of some rules can change what a query does; when one of them
    /// is enabled, fix warns about it (see "Fixes that can change a query" in
    /// the README).
    #[command(after_help = exit_status(
        "  0  without --check: no error, even when a change could not be applied (each\n     \
         is reported on a `warning:` line); with --check: nothing to change\n  \
         1  with --check: a file would change, or a needed change cannot be applied",
        "a file or directory that cannot be read,\n     or a fixed file that cannot be written"
    ))]
    Fix(FixArgs),
    /// Print every extracted query (debug).
    #[command(after_help = exit_status("  0  no error", READ_ERRORS))]
    List(ListArgs),
    /// Print each query's SQL as the linter sees it (debug).
    #[command(after_help = exit_status("  0  no error", READ_ERRORS))]
    Dump(ListArgs),
}

/// The I/O errors of a command that writes no file.
const READ_ERRORS: &str = "a file or directory that cannot be read";

/// The exit status list of a command's help: `codes`, the command's own lines,
/// then the `2` that every command shares, with `io_errors` naming the I/O
/// errors the command can meet.
fn exit_status(codes: &str, io_errors: &str) -> String {
    format!(
        "Exit status:\n{codes}\n  \
         2  an error, each reported on an `error:` line: a path that does not exist\n     \
         or selects no file to scan; {io_errors};\n     \
         a file that is not valid Rust (a file found by walking a directory is\n     \
         parsed only when it contains the text `query`); or an invalid config or\n     \
         argument"
    )
}

const PATHS_HELP: &str = "Files and directories to scan (default: the current directory). A file \
    named here is scanned whatever its extension; a directory is walked for .rs files. Naming \
    paths replaces the [sqlx-sqruff] include patterns; exclude and the ignore files still prune \
    what is found inside a named directory.";

/// Where the README explains the rules whose fix can change what a query does.
const RISKY_FIX_RULES_DOC: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "#fixes-that-can-change-a-query");

// `list` and `dump` do not lint, so they take no `--dialect`.
#[derive(Parser)]
struct ListArgs {
    #[arg(help = PATHS_HELP)]
    paths: Vec<PathBuf>,
    /// Extra sqruff config file. Only its [sqlx-sqruff] section is used here,
    /// to select the files; without one, the section comes from the config
    /// files found by searching up from the current directory, not from the
    /// paths to scan.
    #[arg(long)]
    config: Option<PathBuf>,
}

/// Shared config flags (mirror sqruff): explicit config file + dialect default.
#[derive(Parser)]
struct ConfigOpts {
    /// Extra sqruff config file, applied on top of the config files that sqruff
    /// discovers (a .sqruff and others, looked up from the current directory,
    /// not from the paths to scan). Its [sqlx-sqruff] section, if it has one,
    /// also selects the files.
    #[arg(long)]
    config: Option<PathBuf>,
    /// SQL dialect, overriding the config's `dialect` (postgres when neither
    /// sets one).
    #[arg(long)]
    dialect: Option<String>,
}

#[derive(Parser)]
struct CheckArgs {
    #[arg(help = PATHS_HELP)]
    paths: Vec<PathBuf>,
    #[command(flatten)]
    cfg: ConfigOpts,
    /// Output format of the findings.
    #[arg(long, value_enum, default_value_t = OutFmt::Human)]
    format: OutFmt,
}

#[derive(Parser)]
struct FixArgs {
    #[arg(help = PATHS_HELP)]
    paths: Vec<PathBuf>,
    #[command(flatten)]
    cfg: ConfigOpts,
    /// Don't write; exit non-zero if a file would change or a needed change
    /// cannot be applied.
    #[arg(long)]
    check: bool,
}

#[derive(Clone, ValueEnum)]
enum OutFmt {
    /// One `path:line:col: CODE message` line per finding.
    Human,
    /// One JSON object per finding, each on its own line (JSON Lines), with the
    /// keys `file`, `line`, `col`, `code` and `message`.
    Jsonl,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Command::Check(a) => cmd_check(a),
        Command::Fix(a) => cmd_fix(a),
        Command::List(a) => cmd_list(a),
        Command::Dump(a) => cmd_dump(a),
    }
}

/// Counts the errors reported while the run goes on, so it can exit `2` at the
/// end.
#[derive(Default)]
struct Errors(usize);

impl Errors {
    fn report(&mut self, path: &Path, err: impl Display) {
        eprintln!("error: {}: {err}", shown(path).display());
        self.0 += 1;
    }

    /// Whether the file at `path` was named on the command line, or `None`
    /// after reporting the error that file collection found at `path`.
    fn file(&mut self, path: &Path, found: Found) -> Option<bool> {
        match found {
            Found::File { named } => Some(named),
            Found::Error(message) => {
                self.report(path, message);
                None
            }
        }
    }

    /// The text of `path`, or `None` after reporting why it cannot be read.
    fn read(&mut self, path: &Path) -> Option<String> {
        fs::read_to_string(path).map_err(|e| self.report(path, e)).ok()
    }

    /// The queries in `src`, the text of `path`, or `None` after reporting that
    /// it is not valid Rust, at the position of the error. A file found by
    /// walking a directory (not `named`) is parsed only when it may hold a query.
    fn extract(&mut self, path: &Path, src: &str, named: bool) -> Option<Vec<QueryLiteral>> {
        if !named && !may_hold_a_query(src) {
            return Some(Vec::new());
        }
        extract_checked(src)
            .map_err(|e| {
                let first = e.message.lines().next().unwrap_or("parse error");
                eprintln!("error: {}:{}:{}: cannot parse as Rust: {first}", shown(path).display(), e.line, e.col);
                self.0 += 1;
            })
            .ok()
    }

    /// `2` after any error, else `1` when `failed`, else `0`.
    fn exit_code(&self, failed: bool) -> ExitCode {
        if self.0 > 0 {
            ExitCode::from(2)
        } else if failed {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        }
    }
}

/// Report every explicitly given path that does not exist, in path order;
/// `true` when all do.
fn all_paths_exist(paths: &[PathBuf], errors: &mut Errors) -> bool {
    let mut missing: Vec<(PathBuf, String)> = Vec::new();
    for path in paths {
        if let Err(e) = fs::metadata(path) {
            let message =
                if e.kind() == io::ErrorKind::NotFound { "no such file or directory".to_string() } else { e.to_string() };
            missing.push((shown(path), message));
        }
    }
    missing.sort();
    for (path, message) in &missing {
        errors.report(path, message);
    }
    missing.is_empty()
}

fn cmd_dump(a: ListArgs) -> Result<ExitCode> {
    let mut errors = Errors::default();
    if !all_paths_exist(&a.paths, &mut errors) {
        return Ok(errors.exit_code(false));
    }
    let settings = CliSettings::load(a.config.as_deref())?;
    for (path, found) in collect_files(&a.paths, &settings)? {
        let Some(named) = errors.file(&path, found) else { continue };
        let Some(src) = errors.read(&path) else { continue };
        let Some(queries) = errors.extract(&path, &src, named) else { continue };
        for (line, sql) in dump_extracted(&queries) {
            println!("----- {}:{} -----\n{sql}", path.display(), line);
        }
    }
    Ok(errors.exit_code(false))
}

fn engine(cfg: &ConfigOpts) -> Result<SqruffEngine> {
    let engine =
        SqruffEngine::discover(cfg.dialect.as_deref(), cfg.config.as_deref()).map_err(anyhow::Error::msg)?;
    if !engine.config_found() {
        eprintln!(
            "note: no sqruff config found: no config file sets a sqruff option, so sqruff's \
             default rules apply; config files are looked up from the current directory, not \
             from the paths to scan"
        );
    }
    Ok(engine)
}

fn cmd_check(a: CheckArgs) -> Result<ExitCode> {
    let mut errors = Errors::default();
    if !all_paths_exist(&a.paths, &mut errors) {
        return Ok(errors.exit_code(false));
    }
    let engine = engine(&a.cfg)?;
    let settings = CliSettings::load(a.cfg.config.as_deref())?;
    let mut total = 0usize;
    for (path, found) in collect_files(&a.paths, &settings)? {
        let Some(named) = errors.file(&path, found) else { continue };
        let Some(src) = errors.read(&path) else { continue };
        let Some(queries) = errors.extract(&path, &src, named) else { continue };
        let p = path.display().to_string();
        for d in check_extracted(&p, &src, &queries, &engine) {
            total += 1;
            match a.format {
                OutFmt::Human => println!("{}", d.render_human()),
                OutFmt::Jsonl => println!("{}", d.render_json()),
            }
        }
    }
    eprintln!("{total} finding(s); {} error(s).", errors.0);
    Ok(errors.exit_code(total > 0))
}

fn cmd_fix(a: FixArgs) -> Result<ExitCode> {
    let mut errors = Errors::default();
    if !all_paths_exist(&a.paths, &mut errors) {
        return Ok(errors.exit_code(false));
    }
    let engine = engine(&a.cfg)?;
    let risky = engine.risky_fix_rules();
    if !risky.is_empty() {
        eprintln!(
            "warning: the fixes of these enabled rules can change what a query does: {}; review \
             the changes, or disable the rules for `fix` (see \"Fixes that can change a query\" \
             in the README: {RISKY_FIX_RULES_DOC})",
            risky.join(", ")
        );
    }
    let settings = CliSettings::load(a.cfg.config.as_deref())?;
    let mut changed_files = 0usize;
    let mut changed_queries = 0usize;
    let mut not_applied = 0usize;
    for (path, found) in collect_files(&a.paths, &settings)? {
        let Some(named) = errors.file(&path, found) else { continue };
        let Some(src) = errors.read(&path) else { continue };
        let Some(queries) = errors.extract(&path, &src, named) else { continue };
        let p = path.display().to_string();
        match fix_extracted(&p, &src, &queries, &engine) {
            Ok(out) => {
                let mut warnings: Vec<(usize, String)> = Vec::new();
                for line in &out.dropped_lines {
                    warnings.push((
                        *line,
                        "fix not applied: the rewritten literal would not keep the intended \
                         string value"
                            .to_string(),
                    ));
                }
                for (line, message) in &out.sqruff_errors {
                    warnings.push((*line, format!("fix not applied: sqruff failed: {message}")));
                }
                let with_codes = [
                    (&out.layout_kept, NotApplied::Layout),
                    (&out.partly_applied, NotApplied::PartLayout),
                    (&out.unstable, NotApplied::Unstable),
                    (&out.unsettled, NotApplied::Unsettled),
                ];
                for (entries, why) in with_codes {
                    for (line, codes) in entries {
                        warnings.push((*line, not_applied_warning(why, codes)));
                    }
                }
                for line in &out.unparsable_lines {
                    warnings.push((
                        *line,
                        "fix not applied: sqruff could not fully parse this SQL with the \
                         configured dialect, or one of its rules failed on it, so its fix may \
                         change the query; check the dialect or the SQL"
                            .to_string(),
                    ));
                }
                warnings.sort_by_key(|(line, _)| *line);
                for (line, warning) in &warnings {
                    eprintln!("warning: {p}:{line}: {warning}");
                }
                if out.skipped_escapes > 0 {
                    eprintln!(
                        "note: {p}: {} query(ies) not rewritten because they use escape \
                         sequences other than \\\" and \\\\; write them as raw strings \
                         (r#\"...\"#) to let `fix` format them",
                        out.skipped_escapes
                    );
                }
                not_applied += out.not_applied();
                if let Some(new_src) = out.new_src {
                    if a.check {
                        changed_files += 1;
                        changed_queries += out.changes.len();
                        println!("would fix {} query(ies) in {p}", out.changes.len());
                        for change in &out.changes {
                            print_change(&p, change);
                        }
                    } else if let Err(e) = write_atomically(&path, &new_src) {
                        errors.report(&path, e);
                    } else {
                        changed_files += 1;
                        changed_queries += out.changes.len();
                        println!("fixed {} query(ies) in {p}", out.changes.len());
                    }
                }
            }
            Err(discarded) => {
                eprintln!("warning: {}", discarded.message);
                not_applied += discarded.not_applied;
            }
        }
    }
    eprintln!(
        "{changed_queries} query(ies) in {changed_files} file(s); {not_applied} not applied; {} error(s).",
        errors.0
    );
    let needs_change = a.check && (changed_files > 0 || not_applied > 0);
    Ok(errors.exit_code(needs_change))
}

/// Why `fix` did not apply the fix of a query, or only part of it, for the
/// warnings that name the rules whose fix was not applied.
#[derive(Clone, Copy)]
enum NotApplied {
    /// The fix needs a different line layout.
    Layout,
    /// The part of the fix that fits the line layout was applied.
    PartLayout,
    /// The SQL starts right after the opening quote, and fixing the rewritten
    /// query again would change it further.
    Unstable,
    /// sqruff's own fixes do not settle on the query.
    Unsettled,
}

/// The text of a `warning:` line about a query whose fix was not applied for
/// `why`, naming the rule `codes` whose fix was not applied.
fn not_applied_warning(why: NotApplied, codes: &[String]) -> String {
    let named = !codes.is_empty();
    let rules = if named { format!(" ({})", codes.join(", ")) } else { String::new() };
    let the_rule = if named { "the rule" } else { "the rule that asks for the change" };
    match why {
        NotApplied::Layout => format!(
            "fix not applied: sqruff's change{rules} needs a different line layout; write the query \
             as a multi-line raw string (r#\"...\"#) or disable {the_rule}"
        ),
        NotApplied::PartLayout => format!(
            "fix partly applied: the rest of sqruff's change{rules} needs a different line layout \
             and was not applied; write the query as a multi-line raw string (r#\"...\"#) or \
             disable {the_rule}"
        ),
        NotApplied::Unstable => format!(
            "fix not applied: fixing the rewritten query again would change it further{rules}; \
             start the SQL on its own line after the opening quote, or disable {the_rule}"
        ),
        NotApplied::Unsettled if named => format!(
            "fix not applied: fixing the rewritten query again would change it further{rules}; \
             sqruff's fixes for the named rules do not settle on this query, so disable one of them"
        ),
        NotApplied::Unsettled => "fix not applied: fixing the rewritten query again would change it \
                                  further; sqruff's fixes do not settle on this query, so disable the \
                                  rules that keep changing it"
            .to_string(),
    }
}

/// Replace the file at `path` (or, for a symlink, the file it points to) with
/// `contents` through a temporary file in the same directory, so an interrupted
/// run never leaves a half-written file.
fn write_atomically(path: &Path, contents: &str) -> io::Result<()> {
    let target = path.canonicalize()?;
    // A rename replaces even a read-only file, so require write access to the
    // file itself, as an in-place write would.
    OpenOptions::new().write(true).open(&target)?;
    let permissions = fs::metadata(&target)?.permissions();
    let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else {
        return Err(io::Error::other("not a file path"));
    };
    let tmp = dir.join(format!(".{}.sqlx-sqruff-{}.tmp", name.to_string_lossy(), std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.set_permissions(permissions)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &target)
    })();
    if let Err(err) = result {
        return match fs::remove_file(&tmp) {
            Err(cleanup) if cleanup.kind() != io::ErrorKind::NotFound => Err(io::Error::new(
                err.kind(),
                format!("{err} (and could not remove {}: {cleanup})", shown(&tmp).display()),
            )),
            _ => Err(err),
        };
    }
    Ok(())
}

/// Print a rewritten literal as `path:line`, then its old source lines prefixed
/// with `- ` and its new ones prefixed with `+ `.
fn print_change(path: &str, change: &QueryChange) {
    println!("{path}:{}", change.line);
    for line in change.before.lines() {
        println!("- {line}");
    }
    for line in change.after.lines() {
        println!("+ {line}");
    }
}

fn cmd_list(a: ListArgs) -> Result<ExitCode> {
    let mut errors = Errors::default();
    if !all_paths_exist(&a.paths, &mut errors) {
        return Ok(errors.exit_code(false));
    }
    let settings = CliSettings::load(a.config.as_deref())?;
    for (path, found) in collect_files(&a.paths, &settings)? {
        let Some(named) = errors.file(&path, found) else { continue };
        let Some(src) = errors.read(&path) else { continue };
        let Some(queries) = errors.extract(&path, &src, named) else { continue };
        for q in queries {
            println!("{}:{}", path.display(), q.line);
        }
    }
    Ok(errors.exit_code(false))
}

/// What file collection found at a path.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Found {
    /// An error about the path. It sorts before a file at the same path, so it
    /// is reported before that file is read.
    Error(String),
    /// A file to scan, and whether it was named on the command line rather
    /// than only found by walking a directory.
    File { named: bool },
}

/// Discover the files to scan, each once, together with the errors found at
/// the paths, all sorted by path and spelled as [`shown`] prints them, so that
/// the commands report both in path order.
///
/// - A directly-named **file** path is always processed, whatever its
///   extension, bypassing include/exclude and the ignore files — naming a file
///   means you want it scanned.
/// - A walk takes only `.rs` files. With **no** positional paths, it roots at
///   `.` and `include` (when set) acts as a whitelist.
/// - `exclude` and `.gitignore` (when `respect_gitignore`) prune what a walk
///   finds below its root, but never the root itself.
/// - The globs are relative to `settings.base()`, else to the walked directory.
///
/// A directory that cannot be read is an error entry and is skipped. A search
/// root that selects no file, and whose walk found no error, is an error
/// entry, so that a run that misses a path it was given cannot pass.
fn collect_files(paths: &[PathBuf], settings: &CliSettings) -> Result<Vec<(PathBuf, Found)>> {
    // `include` defines the default search set, so it only applies when the user
    // hasn't named their own paths. `exclude` always prunes.
    let use_include = paths.is_empty() && !settings.include.is_empty();
    let roots: Vec<PathBuf> =
        if paths.is_empty() { vec![PathBuf::from(".")] } else { paths.to_vec() };
    // Whether a glob is valid does not depend on the directory it is relative
    // to, so an invalid one stops the run before any path is looked at.
    build_overrides(Path::new("."), settings, use_include).context("building file filters")?;

    // The index in `out` of each canonical path, so that a file found by a walk
    // and also named is marked as named, whichever root comes first.
    let mut seen: HashMap<PathBuf, usize> = HashMap::new();
    let mut out: Vec<(PathBuf, Found)> = Vec::new();
    for root in &roots {
        let canonical_root = match root.canonicalize() {
            Ok(path) => path,
            Err(e) => {
                out.push((shown(root), Found::Error(e.to_string())));
                continue;
            }
        };
        if canonical_root.is_file() {
            match seen.entry(canonical_root) {
                Entry::Occupied(at) => out[*at.get()].1 = Found::File { named: true },
                Entry::Vacant(at) => {
                    at.insert(out.len());
                    out.push((shown(root), Found::File { named: true }));
                }
            }
            continue;
        }
        // The walk runs on the canonical root so that its paths share a prefix
        // with `settings.base()`, which the globs are matched relative to.
        let base = settings.base().unwrap_or(&canonical_root);
        let overrides = build_overrides(base, settings, use_include)
            .with_context(|| format!("building file filters for {}", root.display()))?;
        let rg = settings.respect_gitignore;
        let mut builder = WalkBuilder::new(&canonical_root);
        builder
            .overrides(overrides)
            .git_ignore(rg)
            .git_global(rg)
            .git_exclude(rg)
            .ignore(rg)
            .parents(rg)
            .require_git(false)
            .sort_by_file_name(|a, b| a.cmp(b));
        // Counts the files that another root already selected too, so that
        // naming a directory twice is not an error.
        let mut selected = 0usize;
        let mut walk_errors = Vec::new();
        for entry in builder.build() {
            match entry {
                Ok(entry) => {
                    let p = entry.path();
                    if entry.file_type().is_some_and(|t| t.is_file()) && is_rs(p) {
                        selected += 1;
                        if let Entry::Vacant(at) = seen.entry(p.to_path_buf()) {
                            at.insert(out.len());
                            out.push((shown(&under_root(root, &canonical_root, p)), Found::File { named: false }));
                        }
                    }
                }
                Err(err) => walk_error(root, &canonical_root, &err, &mut walk_errors),
            }
        }
        if selected == 0 && walk_errors.is_empty() {
            walk_errors.push(match settings.file.as_deref().filter(|_| use_include) {
                Some(config) => (
                    shown(config),
                    Found::Error(format!(
                        "no Rust files match the [sqlx-sqruff] include patterns, which are \
                         relative to {}",
                        shown(base).display()
                    )),
                ),
                None => (shown(root), Found::Error("no Rust files found".to_string())),
            });
        }
        out.append(&mut walk_errors);
    }
    out.sort();
    Ok(out)
}

/// `path`, found by walking `canonical_root`, rewritten under `root` as the user
/// wrote it.
fn under_root(root: &Path, canonical_root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(canonical_root).map_or_else(|_| path.to_path_buf(), |rel| root.join(rel))
}

/// Push to `out` an error entry for each error in `err`, from the walk of
/// `root`.
fn walk_error(root: &Path, canonical_root: &Path, err: &ignore::Error, out: &mut Vec<(PathBuf, Found)>) {
    match err {
        ignore::Error::Partial(errs) => {
            for err in errs {
                walk_error(root, canonical_root, err, out);
            }
        }
        ignore::Error::WithDepth { err, .. } => walk_error(root, canonical_root, err, out),
        ignore::Error::WithPath { path, err } => {
            out.push((shown(&under_root(root, canonical_root, path)), Found::Error(walk_message(err))));
        }
        _ => out.push((shown(root), Found::Error(walk_message(err)))),
    }
}

/// The message of a walk error, without the absolute path that the walker adds
/// to an I/O error: the `error:` line already names that path as [`shown`]
/// spells it.
fn walk_message(err: &ignore::Error) -> String {
    match err {
        ignore::Error::WithDepth { err, .. } => walk_message(err),
        ignore::Error::Io(io) => {
            io.get_ref().and_then(|inner| inner.source()).map_or_else(|| io.to_string(), ToString::to_string)
        }
        _ => err.to_string(),
    }
}

/// Build the include/exclude glob matcher rooted at `base`. Include globs form a
/// whitelist (only added when `use_include`); exclude globs are negated.
fn build_overrides(
    base: &Path,
    settings: &CliSettings,
    use_include: bool,
) -> Result<ignore::overrides::Override> {
    let mut builder = OverrideBuilder::new(base);
    if use_include {
        for glob in &settings.include {
            builder.add(glob).with_context(|| format!("invalid include glob '{glob}'"))?;
        }
    }
    for glob in &settings.exclude {
        let negated = format!("!{glob}");
        builder.add(&negated).with_context(|| format!("invalid exclude glob '{glob}'"))?;
    }
    Ok(builder.build()?)
}

fn is_rs(p: &Path) -> bool {
    p.extension().is_some_and(|e| e == "rs")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// A throwaway directory tree, removed on drop. Each listed path is created
    /// with placeholder contents.
    struct Tree(PathBuf);

    impl Tree {
        fn new(tag: &str, files: &[&str]) -> Self {
            let base =
                std::env::temp_dir().join(format!("sqlx_sqruff_{}_{tag}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            for rel in files {
                let path = base.join(rel);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, "fn q() {}\n").unwrap();
            }
            Self(base)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Collect, then return base-relative `/`-separated paths, sorted. Paths
    /// are compared canonically, because the files are outside the current
    /// directory and so are printed resolved.
    fn collect_rel(paths: &[PathBuf], settings: &CliSettings, base: &Path) -> Vec<String> {
        let base = base.canonicalize().unwrap();
        let mut got: Vec<String> = collect_files(paths, settings)
            .unwrap()
            .into_iter()
            .map(|(p, _)| {
                let p = p.canonicalize().unwrap();
                p.strip_prefix(&base).unwrap_or(&p).to_string_lossy().replace('\\', "/")
            })
            .collect();
        got.sort();
        got
    }

    /// Settings that ignore VCS files, so an ancestor `.gitignore` (e.g. in the
    /// temp dir) can't perturb filesystem-walking tests.
    fn no_vcs() -> CliSettings {
        CliSettings { respect_gitignore: false, ..Default::default() }
    }

    #[test]
    fn include_is_a_whitelist_others_are_not() {
        let settings = CliSettings { include: vec!["a/**/*.rs".into()], ..Default::default() };
        let ov = build_overrides(Path::new("."), &settings, true).unwrap();
        assert!(ov.matched("a/x.rs", false).is_whitelist());
        assert!(!ov.matched("b/y.rs", false).is_whitelist());
    }

    #[test]
    fn exclude_negates_even_without_include() {
        let settings = CliSettings { exclude: vec!["**/skip/**".into()], ..Default::default() };
        let ov = build_overrides(Path::new("."), &settings, false).unwrap();
        assert!(ov.matched("pkg/skip/z.rs", false).is_ignore());
        assert!(!ov.matched("pkg/keep/z.rs", false).is_ignore());
    }

    #[test]
    fn no_filters_takes_every_rs_and_skips_other_extensions() {
        let tree = Tree::new("all", &["a/x.rs", "b/y.rs", "c.txt"]);
        let got = collect_rel(&[tree.path().to_path_buf()], &no_vcs(), tree.path());
        assert_eq!(got, ["a/x.rs", "b/y.rs"]);
    }

    #[test]
    fn exclude_prunes_under_an_explicit_path() {
        let tree = Tree::new("excl", &["keep/x.rs", "skip/y.rs"]);
        let settings = CliSettings { exclude: vec!["**/skip/**".into()], ..no_vcs() };
        let got = collect_rel(&[tree.path().to_path_buf()], &settings, tree.path());
        assert_eq!(got, ["keep/x.rs"]);
    }

    #[test]
    fn a_named_file_is_always_taken() {
        let tree = Tree::new("file", &["only.rs"]);
        let file = tree.path().join("only.rs");
        // An include whitelist that the file does not match must not exclude a
        // directly-named file.
        let settings = CliSettings { include: vec!["nomatch/**".into()], ..no_vcs() };
        let got: Vec<PathBuf> =
            collect_files(std::slice::from_ref(&file), &settings)
                .unwrap()
                .iter()
                .map(|(p, _)| p.canonicalize().unwrap())
                .collect();
        assert_eq!(got, [file.canonicalize().unwrap()]);
    }

    #[test]
    fn a_not_applied_warning_without_rule_codes_does_not_point_at_a_rule() {
        let codes = ["LT05".to_string(), "LT09".to_string()];
        for why in [NotApplied::Layout, NotApplied::PartLayout, NotApplied::Unstable, NotApplied::Unsettled] {
            let unnamed = not_applied_warning(why, &[]);
            assert!(!unnamed.contains("()"), "{unnamed}");
            assert!(!unnamed.contains("named") && !unnamed.ends_with("the rule"), "{unnamed}");
            assert!(unnamed.contains("disable"), "{unnamed}");
            let named = not_applied_warning(why, &codes);
            assert!(named.contains(" (LT05, LT09)"), "{named}");
        }
    }
}
