//! The ONLY module that touches `sqruff-lib`. Keep the surface tiny so an
//! upgrade of the pinned (0.x) sqruff crates touches one file.
//!
//! Config follows sqruff's NATIVE behaviour: `discover()` uses
//! `FluffConfig::from_root`, which finds a project `.sqruff` (and applies
//! sqruff's own default rules when none exists), exactly like the `sqruff`
//! binary. The tool embeds NO rule presets. The single injected default is the
//! dialect: sqruff defaults to `ansi`, which mis-parses the majority of
//! postgres sqlx queries, so we override it (overridable via `--dialect`).

use std::path::Path;

use sqruff_lib::core::{config::FluffConfig, linter::core::Linter};
use sqruff_lib_core::{dialects::init::DialectKind, errors::SQLBaseError};

/// A single lint finding, 1-based line / 0-based col *within the dedented SQL*.
#[derive(Debug, Clone)]
pub struct Finding {
    pub code: String,
    pub line: usize,
    pub col: usize,
    pub desc: String,
    pub fixable: bool,
}

impl From<&SQLBaseError> for Finding {
    fn from(e: &SQLBaseError) -> Self {
        Self {
            code: e.rule_code().to_string(),
            line: e.line_no,
            col: e.line_pos,
            desc: e.desc().to_string(),
            fixable: e.fixable,
        }
    }
}

pub struct SqruffEngine {
    linter: Linter,
}

impl SqruffEngine {
    /// Production path: sqruff-native config discovery.
    /// - `extra_config`: an explicit config file (`--config`), or None.
    /// - `.sqruff` in the cwd / ancestors is discovered automatically.
    /// - `dialect` is injected as an override (default `postgres`).
    pub fn discover(dialect: &str, extra_config: Option<&Path>) -> Result<Self, String> {
        // `from_root` IGNORES an extra_config_path unless ignore_local_config is
        // set, so load an explicit `--config` directly with `from_file`; reserve
        // `from_root` for sqruff-native discovery (`.sqruff` walk + defaults).
        let mut cfg = match extra_config {
            Some(path) => FluffConfig::from_file(path),
            None => FluffConfig::from_root(None, false, None).map_err(|e| format!("{e:?}"))?,
        };
        cfg.override_dialect(parse_dialect(dialect)?)?;
        Self::with_config(cfg)
    }

    /// Build from an explicit config string (tests, or callers that hold their
    /// own `.sqruff` text). The string must set `dialect`.
    pub fn from_source(config_src: &str) -> Result<Self, String> {
        Self::with_config(FluffConfig::from_source(config_src, None))
    }

    fn with_config(config: FluffConfig) -> Result<Self, String> {
        let linter = Linter::new(config, None, None, true)?;
        Ok(Self { linter })
    }

    /// Lint only.
    pub fn lint(&self, sql: &str) -> Vec<Finding> {
        match self.linter.lint_string(sql, None, false) {
            Ok(lf) => lf.violations().iter().map(Finding::from).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Fix; returns `(fixed_sql, residual_unfixable_findings)`.
    pub fn fix(&self, sql: &str) -> (String, Vec<Finding>) {
        match self.linter.lint_string(sql, None, true) {
            Ok(lf) => {
                let residual =
                    lf.violations().iter().filter(|v| !v.fixable).map(Finding::from).collect();
                (lf.fix_string(), residual)
            }
            Err(_) => (sql.to_string(), Vec::new()),
        }
    }
}

fn parse_dialect(s: &str) -> Result<DialectKind, String> {
    use DialectKind::*;
    Ok(match s.to_ascii_lowercase().as_str() {
        "ansi" => Ansi,
        "athena" => Athena,
        "bigquery" => Bigquery,
        "clickhouse" => Clickhouse,
        "databricks" => Databricks,
        "db2" => Db2,
        "duckdb" => Duckdb,
        "mysql" => Mysql,
        "oracle" => Oracle,
        "postgres" => Postgres,
        "redshift" => Redshift,
        "snowflake" => Snowflake,
        "sparksql" => Sparksql,
        "sqlite" => Sqlite,
        "trino" => Trino,
        "tsql" => Tsql,
        other => return Err(format!("unknown dialect '{other}'")),
    })
}
