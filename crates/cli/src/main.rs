//! `sqlx-sqruff` CLI: check / fix / list inline sqlx SQL.
//!
//! Config follows sqruff: a project `.sqruff` (auto-discovered, or `--config`)
//! decides the rules; with none, sqruff's own defaults apply. The tool embeds no
//! rule presets. `--dialect` (default `postgres`) is the one injected default,
//! because sqruff's ansi default mis-parses most postgres sqlx queries.

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use sqlx_sqruff_core::{
    engine::{check_extracted, fix_extracted},
    extract::extract_checked,
    sqruff_adapter::SqruffEngine,
};
use walkdir::WalkDir;

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
    for path in rust_files(&a.paths) {
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
    let mut total = 0usize;
    let mut skipped = 0usize;
    for path in rust_files(&a.paths) {
        let src = std::fs::read_to_string(&path)?;
        let p = path.display().to_string();
        // Parse once: extract_checked both surfaces the syn error (for the skip
        // warning) and yields the queries check_extracted needs — no second parse.
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
    let mut changed_files = 0usize;
    let mut changed_queries = 0usize;
    let mut skipped = 0usize;
    for path in rust_files(&a.paths) {
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
    for path in rust_files(&a.paths) {
        let src = std::fs::read_to_string(&path)?;
        for q in sqlx_sqruff_core::extract::extract(&src) {
            println!("{}:{}", path.display(), q.line);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn rust_files(paths: &[PathBuf]) -> Vec<PathBuf> {
    let roots: Vec<PathBuf> =
        if paths.is_empty() { vec![PathBuf::from(".")] } else { paths.to_vec() };
    let mut out = Vec::new();
    for root in roots {
        if root.is_file() {
            if is_rs(&root) {
                out.push(root);
            }
            continue;
        }
        for entry in WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
            let p = entry.path();
            if entry.file_type().is_file() && is_rs(p) {
                out.push(p.to_path_buf());
            }
        }
    }
    out
}

fn is_rs(p: &Path) -> bool {
    p.extension().is_some_and(|e| e == "rs")
}
