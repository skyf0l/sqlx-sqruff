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
    collections::HashSet,
    fmt::Display,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use config::CliSettings;
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use paths::shown;
use sqlx_sqruff_core::{
    engine::{QueryChange, check_extracted, dump_extracted, fix_extracted},
    extract::{QueryLiteral, extract_checked},
    sqruff_adapter::SqruffEngine,
};

#[derive(Parser)]
#[command(name = "sqlx-sqruff", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Lint inline SQL; non-zero exit if any findings.
    Check(CheckArgs),
    /// Apply fixes and write back into .rs files.
    Fix(FixArgs),
    /// Print every extracted query (debug).
    List(ListArgs),
    /// Print each query's SQL as the linter sees it (debug).
    Dump(ListArgs),
}

// `list` and `dump` do not lint, so they take no `--dialect`.
#[derive(Parser)]
struct ListArgs {
    paths: Vec<PathBuf>,
    /// Extra sqruff config file. Only its `[sqlx-sqruff]` section is used here,
    /// to select the files.
    #[arg(long)]
    config: Option<PathBuf>,
}

/// Shared config flags (mirror sqruff): explicit config file + dialect default.
#[derive(Parser)]
struct ConfigOpts {
    /// Extra sqruff config file, applied on top of the discovered ones (a
    /// `.sqruff` and the other files sqruff reads).
    #[arg(long)]
    config: Option<PathBuf>,
    /// SQL dialect, overriding the config's `dialect` (postgres when neither
    /// sets one).
    #[arg(long)]
    dialect: Option<String>,
}

#[derive(Parser)]
struct CheckArgs {
    paths: Vec<PathBuf>,
    #[command(flatten)]
    cfg: ConfigOpts,
    /// Output format of the findings.
    #[arg(long, value_enum, default_value_t = OutFmt::Human)]
    format: OutFmt,
}

#[derive(Parser)]
struct FixArgs {
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

    /// The text of `path`, or `None` after reporting why it cannot be read.
    fn read(&mut self, path: &Path) -> Option<String> {
        fs::read_to_string(path).map_err(|e| self.report(path, e)).ok()
    }

    /// The queries in `src`, the text of `path`, or `None` after reporting that
    /// it is not valid Rust.
    fn extract(&mut self, path: &Path, src: &str) -> Option<Vec<QueryLiteral>> {
        extract_checked(src)
            .map_err(|e| {
                let first = e.lines().next().unwrap_or("parse error");
                self.report(path, format_args!("cannot parse as Rust: {first}"));
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

/// Report every explicitly given path that does not exist; `true` when all do.
fn all_paths_exist(paths: &[PathBuf]) -> bool {
    let mut all_exist = true;
    for path in paths {
        if let Err(e) = fs::metadata(path) {
            if e.kind() == io::ErrorKind::NotFound {
                eprintln!("error: {}: no such file or directory", shown(path).display());
            } else {
                eprintln!("error: {}: {e}", shown(path).display());
            }
            all_exist = false;
        }
    }
    all_exist
}

fn cmd_dump(a: ListArgs) -> Result<ExitCode> {
    if !all_paths_exist(&a.paths) {
        return Ok(ExitCode::from(2));
    }
    let settings = CliSettings::load(a.config.as_deref())?;
    let mut errors = Errors::default();
    for path in collect_files(&a.paths, &settings, &mut errors)? {
        let Some(src) = errors.read(&path) else { continue };
        let Some(queries) = errors.extract(&path, &src) else { continue };
        for (line, sql) in dump_extracted(&queries) {
            println!("----- {}:{} -----\n{sql}", path.display(), line);
        }
    }
    Ok(errors.exit_code(false))
}

fn engine(cfg: &ConfigOpts) -> Result<SqruffEngine> {
    SqruffEngine::discover(cfg.dialect.as_deref(), cfg.config.as_deref()).map_err(anyhow::Error::msg)
}

fn cmd_check(a: CheckArgs) -> Result<ExitCode> {
    if !all_paths_exist(&a.paths) {
        return Ok(ExitCode::from(2));
    }
    let engine = engine(&a.cfg)?;
    let settings = CliSettings::load(a.cfg.config.as_deref())?;
    let mut errors = Errors::default();
    let mut total = 0usize;
    let mut failed_files = 0usize;
    for path in collect_files(&a.paths, &settings, &mut errors)? {
        let Some(src) = errors.read(&path) else {
            failed_files += 1;
            continue;
        };
        let Some(queries) = errors.extract(&path, &src) else {
            failed_files += 1;
            continue;
        };
        let p = path.display().to_string();
        for d in check_extracted(&p, &src, &queries, &engine) {
            total += 1;
            match a.format {
                OutFmt::Human => println!("{}", d.render_human()),
                OutFmt::Jsonl => println!("{}", d.render_json()),
            }
        }
    }
    eprintln!("{total} finding(s); {failed_files} file(s) could not be checked.");
    Ok(errors.exit_code(total > 0))
}

fn cmd_fix(a: FixArgs) -> Result<ExitCode> {
    if !all_paths_exist(&a.paths) {
        return Ok(ExitCode::from(2));
    }
    let engine = engine(&a.cfg)?;
    let settings = CliSettings::load(a.cfg.config.as_deref())?;
    let mut errors = Errors::default();
    let mut changed_files = 0usize;
    let mut changed_queries = 0usize;
    let mut not_applied = 0usize;
    let mut failed_files = 0usize;
    for path in collect_files(&a.paths, &settings, &mut errors)? {
        let Some(src) = errors.read(&path) else {
            failed_files += 1;
            continue;
        };
        let Some(queries) = errors.extract(&path, &src) else {
            failed_files += 1;
            continue;
        };
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
                for (line, codes) in &out.layout_kept {
                    warnings.push((
                        *line,
                        format!(
                            "fix not applied: sqruff's change{} needs a different line layout; \
                             write the query as a multi-line raw string (r#\"...\"#) or disable \
                             the rule",
                            rules(codes)
                        ),
                    ));
                }
                for (line, codes) in &out.partly_applied {
                    warnings.push((
                        *line,
                        format!(
                            "fix partly applied: the rest of sqruff's change{} needs a different \
                             line layout and was not applied; write the query as a multi-line raw \
                             string (r#\"...\"#) or disable the rule",
                            rules(codes)
                        ),
                    ));
                }
                for (line, codes) in &out.unstable {
                    warnings.push((
                        *line,
                        format!(
                            "fix not applied: fixing the rewritten query again would change it \
                             further{}; start the SQL on its own line after the opening quote, or \
                             disable the rule",
                            rules(codes)
                        ),
                    ));
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
                        failed_files += 1;
                    } else {
                        changed_files += 1;
                        changed_queries += out.changes.len();
                        println!("fixed {} query(ies) in {p}", out.changes.len());
                    }
                }
            }
            Err(e) => {
                eprintln!("warning: {e}");
                failed_files += 1;
            }
        }
    }
    eprintln!(
        "{changed_queries} query(ies) in {changed_files} file(s); {not_applied} not applied; \
         {failed_files} file(s) failed."
    );
    let needs_change = a.check && (changed_files > 0 || not_applied > 0 || failed_files > 0);
    Ok(errors.exit_code(needs_change))
}

/// ` (LT05, LT09)` for the rule codes of a warning, or nothing when there are
/// none.
fn rules(codes: &[String]) -> String {
    if codes.is_empty() { String::new() } else { format!(" ({})", codes.join(", ")) }
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
    if !all_paths_exist(&a.paths) {
        return Ok(ExitCode::from(2));
    }
    let settings = CliSettings::load(a.config.as_deref())?;
    let mut errors = Errors::default();
    for path in collect_files(&a.paths, &settings, &mut errors)? {
        let Some(src) = errors.read(&path) else { continue };
        let Some(queries) = errors.extract(&path, &src) else { continue };
        for q in queries {
            println!("{}:{}", path.display(), q.line);
        }
    }
    Ok(errors.exit_code(false))
}

/// Discover the files to scan, each once, sorted by path and spelled as
/// [`shown`] prints them.
///
/// - A directly-named **file** path is always processed, whatever its
///   extension, bypassing include/exclude and the ignore files — naming a file
///   means you want it scanned.
/// - A walk takes only `.rs` files. With **no** positional paths, it roots at
///   `.` and `include` (when set) acts as a whitelist.
/// - `exclude` and `.gitignore` (when `respect_gitignore`) prune what a walk
///   finds below its root, but never the root itself.
/// - The globs are relative to `settings.base`, else to the walked directory.
///
/// A directory that cannot be read is reported in `errors` and skipped. Finding
/// no file at all is an error, so that a run that checks nothing cannot pass.
fn collect_files(
    paths: &[PathBuf],
    settings: &CliSettings,
    errors: &mut Errors,
) -> Result<Vec<PathBuf>> {
    // `include` defines the default search set, so it only applies when the user
    // hasn't named their own paths. `exclude` always prunes.
    let use_include = paths.is_empty() && !settings.include.is_empty();
    let roots: Vec<PathBuf> =
        if paths.is_empty() { vec![PathBuf::from(".")] } else { paths.to_vec() };

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for root in &roots {
        let canonical_root = match root.canonicalize() {
            Ok(path) => path,
            Err(e) => {
                errors.report(root, e);
                continue;
            }
        };
        if canonical_root.is_file() {
            if seen.insert(canonical_root) {
                out.push(shown(root));
            }
            continue;
        }
        // The walk runs on the canonical root so that its paths share a prefix
        // with `settings.base`, which the globs are matched relative to.
        let base = settings.base.as_deref().unwrap_or(&canonical_root);
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
        for entry in builder.build() {
            match entry {
                Ok(entry) => {
                    let p = entry.path();
                    if entry.file_type().is_some_and(|t| t.is_file())
                        && is_rs(p)
                        && seen.insert(p.to_path_buf())
                    {
                        out.push(shown(&under_root(root, &canonical_root, p)));
                    }
                }
                Err(err) => report_walk_error(root, &canonical_root, &err, errors),
            }
        }
    }
    if out.is_empty() {
        let searched: Vec<String> = roots.iter().map(|root| shown(root).display().to_string()).collect();
        let what = if use_include { "Rust files matching the [sqlx-sqruff] include patterns" } else { "Rust files" };
        bail!("no {what} found in {}", searched.join(", "));
    }
    out.sort();
    Ok(out)
}

/// `path`, found by walking `canonical_root`, rewritten under `root` as the user
/// wrote it.
fn under_root(root: &Path, canonical_root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(canonical_root).map_or_else(|_| path.to_path_buf(), |rel| root.join(rel))
}

fn report_walk_error(root: &Path, canonical_root: &Path, err: &ignore::Error, errors: &mut Errors) {
    match err {
        ignore::Error::Partial(errs) => {
            for err in errs {
                report_walk_error(root, canonical_root, err, errors);
            }
        }
        ignore::Error::WithDepth { err, .. } => report_walk_error(root, canonical_root, err, errors),
        ignore::Error::WithPath { path, err } => {
            errors.report(&under_root(root, canonical_root, path), err);
        }
        _ => errors.report(root, err),
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
        let mut got: Vec<String> = collect_files(paths, settings, &mut Errors::default())
            .unwrap()
            .into_iter()
            .map(|p| {
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
            collect_files(std::slice::from_ref(&file), &settings, &mut Errors::default())
                .unwrap()
                .iter()
                .map(|p| p.canonicalize().unwrap())
                .collect();
        assert_eq!(got, [file.canonicalize().unwrap()]);
    }
}
