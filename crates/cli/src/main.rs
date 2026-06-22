//! `sqlx-sqruff` CLI: check / fix / list inline sqlx SQL.
//!
//! Config follows sqruff: a project `.sqruff` (auto-discovered, or `--config`)
//! decides the rules; with none, sqruff's own defaults apply. The tool embeds no
//! rule presets. `--dialect` (default `postgres`) is the one injected default,
//! because sqruff's ansi default mis-parses most postgres sqlx queries.
//!
//! File selection (which `.rs` files to scan) is driven by an optional
//! `[sqlx-sqruff]` section of the same `.sqruff` — see [`config`]. By default
//! every `.rs` file is scanned, respecting `.gitignore`.

mod config;

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use config::CliSettings;
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use sqlx_sqruff_core::{
    engine::{check_extracted, fix_extracted},
    extract::extract_checked,
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
    /// Print each query's dedented SQL as the linter sees it (debug).
    Dump(ListArgs),
}

/// Shared config flags (mirror sqruff): explicit config file + dialect default.
#[derive(Parser)]
struct ConfigOpts {
    /// Explicit sqruff config file (otherwise a `.sqruff` is auto-discovered).
    #[arg(long)]
    config: Option<PathBuf>,
    /// SQL dialect injected into the config (sqruff's ansi default mis-parses
    /// postgres sqlx). Override for other databases.
    #[arg(long, default_value = "postgres")]
    dialect: String,
}

#[derive(Parser)]
struct CheckArgs {
    paths: Vec<PathBuf>,
    #[command(flatten)]
    cfg: ConfigOpts,
    #[arg(long, value_enum, default_value_t = OutFmt::Human)]
    format: OutFmt,
}

#[derive(Parser)]
struct FixArgs {
    paths: Vec<PathBuf>,
    #[command(flatten)]
    cfg: ConfigOpts,
    /// Don't write; exit non-zero if changes are needed.
    #[arg(long)]
    check: bool,
}

#[derive(Parser)]
struct ListArgs {
    paths: Vec<PathBuf>,
    #[command(flatten)]
    cfg: ConfigOpts,
}

#[derive(Clone, ValueEnum)]
enum OutFmt {
    Human,
    Json,
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

fn cmd_dump(a: ListArgs) -> Result<ExitCode> {
    let settings = CliSettings::load(a.cfg.config.as_deref())?;
    for path in collect_files(&a.paths, &settings)? {
        let src = std::fs::read_to_string(&path)?;
        for (line, sql) in sqlx_sqruff_core::engine::dump_file(&src) {
            println!("----- {}:{} -----\n{sql}", path.display(), line);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn engine(cfg: &ConfigOpts) -> Result<SqruffEngine> {
    SqruffEngine::discover(&cfg.dialect, cfg.config.as_deref()).map_err(anyhow::Error::msg)
}

fn cmd_check(a: CheckArgs) -> Result<ExitCode> {
    let engine = engine(&a.cfg)?;
    let settings = CliSettings::load(a.cfg.config.as_deref())?;
    let mut total = 0usize;
    let mut skipped = 0usize;
    for path in collect_files(&a.paths, &settings)? {
        let src = std::fs::read_to_string(&path)?;
        let p = path.display().to_string();
        // Parse once: extract_checked both surfaces the syn error (for the skip
        // warning) and yields the queries check_extracted needs, with no reparse.
        let queries = match extract_checked(&src) {
            Ok(q) => q,
            Err(e) => {
                warn_skip(&p, &e);
                skipped += 1;
                continue;
            }
        };
        for d in check_extracted(&p, &queries, &engine) {
            total += 1;
            match a.format {
                OutFmt::Human => println!("{}", d.render_human()),
                OutFmt::Json => println!("{}", d.render_json()),
            }
        }
    }
    eprintln!("{total} finding(s); {skipped} file(s) skipped (unparsable Rust).");
    Ok(if total == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

/// Warn when a file can't be parsed by `syn`, so whole-file skips are visible,
/// never silent. `err` is the `extract_checked` parse error.
fn warn_skip(path: &str, err: &str) {
    let first = err.lines().next().unwrap_or("parse error");
    eprintln!("warning: skipping {path} (unparsable by syn): {first}");
}

fn cmd_fix(a: FixArgs) -> Result<ExitCode> {
    let engine = engine(&a.cfg)?;
    let settings = CliSettings::load(a.cfg.config.as_deref())?;
    let mut changed_files = 0usize;
    let mut changed_queries = 0usize;
    let mut skipped = 0usize;
    for path in collect_files(&a.paths, &settings)? {
        let src = std::fs::read_to_string(&path)?;
        let p = path.display().to_string();
        // Parse once (see cmd_check): extract_checked feeds both the skip warning
        // and fix_extracted.
        let queries = match extract_checked(&src) {
            Ok(q) => q,
            Err(e) => {
                warn_skip(&p, &e);
                skipped += 1;
                continue;
            }
        };
        match fix_extracted(&p, &src, &queries, &engine) {
            Ok(out) => {
                if let Some(new_src) = out.new_src {
                    changed_files += 1;
                    changed_queries += out.queries_changed;
                    if a.check {
                        println!("would fix {} query(ies) in {p}", out.queries_changed);
                    } else {
                        std::fs::write(&path, new_src)?;
                        println!("fixed {} query(ies) in {p}", out.queries_changed);
                    }
                }
            }
            Err(e) => eprintln!("warning: {e}"),
        }
    }
    eprintln!(
        "{changed_queries} query(ies) in {changed_files} file(s); \
         {skipped} file(s) skipped (unparsable Rust)."
    );
    let needs_change = a.check && changed_files > 0;
    Ok(if needs_change { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

fn cmd_list(a: ListArgs) -> Result<ExitCode> {
    let settings = CliSettings::load(a.cfg.config.as_deref())?;
    for path in collect_files(&a.paths, &settings)? {
        let src = std::fs::read_to_string(&path)?;
        for q in sqlx_sqruff_core::extract::extract(&src) {
            println!("{}:{}", path.display(), q.line);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Discover the `.rs` files to scan.
///
/// - A directly-named **file** path is always processed (if it ends in `.rs`),
///   bypassing include/exclude — naming a file means you want it scanned.
/// - With **no** positional paths, the walk roots at `.` and `include` (when
///   set) acts as a whitelist; otherwise every `.rs` file is taken.
/// - `exclude` and `.gitignore` (when `respect_gitignore`) always prune, for
///   both explicit directory paths and the default walk.
fn collect_files(paths: &[PathBuf], settings: &CliSettings) -> Result<Vec<PathBuf>> {
    // `include` defines the default search set, so it only applies when the user
    // hasn't named their own paths. `exclude` always prunes.
    let use_include = paths.is_empty() && !settings.include.is_empty();
    let roots: Vec<PathBuf> =
        if paths.is_empty() { vec![PathBuf::from(".")] } else { paths.to_vec() };

    let mut out = Vec::new();
    for root in &roots {
        if root.is_file() {
            if is_rs(root) {
                out.push(root.clone());
            }
            continue;
        }
        let overrides = build_overrides(root, settings, use_include)
            .with_context(|| format!("building file filters for {}", root.display()))?;
        let rg = settings.respect_gitignore;
        let mut builder = WalkBuilder::new(root);
        builder
            .overrides(overrides)
            .git_ignore(rg)
            .git_global(rg)
            .git_exclude(rg)
            .ignore(rg)
            .parents(rg);
        for entry in builder.build() {
            let Ok(entry) = entry else { continue };
            let p = entry.path();
            if entry.file_type().is_some_and(|t| t.is_file()) && is_rs(p) {
                out.push(p.to_path_buf());
            }
        }
    }
    Ok(out)
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

    /// Collect, then return base-relative `/`-separated paths, sorted.
    fn collect_rel(paths: &[PathBuf], settings: &CliSettings, base: &Path) -> Vec<String> {
        let mut got: Vec<String> = collect_files(paths, settings)
            .unwrap()
            .into_iter()
            .map(|p| p.strip_prefix(base).unwrap_or(&p).to_string_lossy().replace('\\', "/"))
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
        let got = collect_files(std::slice::from_ref(&file), &settings).unwrap();
        assert_eq!(got, [file]);
    }
}
