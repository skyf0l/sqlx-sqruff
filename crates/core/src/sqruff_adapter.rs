//! The ONLY module that touches `sqruff-lib`. Keep the surface tiny so an
//! upgrade of the pinned (0.x) sqruff crates touches one file.
//!
//! Config follows sqruff's NATIVE behaviour: `discover()` loads the same config
//! stack as the `sqruff` binary (user config directory, home, the ancestors of
//! the working directory and the working directory itself, with an explicit
//! `--config` file applied on top), and sqruff's own default rules apply when
//! no config sets `rules`. The tool embeds NO rule presets. The single injected
//! default is the dialect: sqruff defaults to `ansi`, which mis-parses the
//! majority of postgres sqlx queries, so `postgres` is used when neither
//! `--dialect` nor the config sets one.

use std::{
    any::Any,
    cell::Cell,
    panic::{self, AssertUnwindSafe},
    path::Path,
    str::FromStr,
    sync::Once,
};

use sqruff_lib::core::{
    config::{ConfigLoader, FluffConfig, Value},
    linter::core::Linter,
};
use sqruff_lib_core::{
    config::ConfigMap,
    dialects::init::{DialectKind, dialect_readout},
    errors::SQLBaseError,
};

use crate::diagnostic::{INTERNAL, LEX, NOQA, PARSE};

/// A single lint finding, 1-based line / 1-based col in chars *within the SQL
/// given to sqruff*.
#[derive(Debug, Clone)]
pub struct Finding {
    /// The sqruff rule code, or one of the codes in [`crate::diagnostic`] for
    /// a finding that is not a rule violation.
    pub code: String,
    pub line: usize,
    pub col: usize,
    pub desc: String,
    pub fixable: bool,
    /// A lexing or parsing error, or a rule that failed inside sqruff, rather
    /// than a rule violation: a fix computed for this SQL cannot be trusted.
    pub unparsable: bool,
}

impl From<&SQLBaseError> for Finding {
    fn from(e: &SQLBaseError) -> Self {
        let code = code(e);
        Self {
            code: code.to_string(),
            line: e.line_no,
            col: e.line_pos,
            desc: e.desc().to_string(),
            fixable: e.fixable,
            unparsable: code == PARSE || code == LEX || code == INTERNAL,
        }
    }
}

impl Finding {
    /// `desc`, with the line of the SQL that it quotes, if any, replaced by
    /// `line` of it. Of sqruff's messages, only AL08's quotes one:
    /// `Reuse of column alias 'a' from line 6.`
    pub fn message(&self, line: impl FnOnce(usize) -> usize) -> String {
        let quoted = if self.code == "AL08" {
            self.desc.strip_suffix('.').and_then(|desc| desc.rsplit_once(" from line "))
        } else {
            None
        };
        if let Some((head, n)) = quoted
            && let Ok(n) = n.parse()
        {
            return format!("{head} from line {}.", line(n));
        }
        self.desc.clone()
    }
}

/// The code of a sqruff finding. sqruff reports lexing errors as `LXR` and
/// every other finding without a rule as `????`: parse errors, malformed
/// `-- noqa` comments and rules that failed. Those are told apart by the
/// messages sqruff gives them.
fn code(e: &SQLBaseError) -> &'static str {
    match &e.rule {
        Some(rule) if rule.code == "LXR" => LEX,
        Some(rule) => rule.code,
        None if e.desc().starts_with("Malformed 'noqa' section") => NOQA,
        None if e.desc().starts_with("Unexpected exception") => INTERNAL,
        None => PARSE,
    }
}

/// The rules whose fix can change what a query does, each with the dialects
/// on which it can, or none for every dialect that sqruff runs the rule on.
/// The README's "Fixes that can change a query" section explains each one.
const RISKY_FIX_RULES: &[(&str, &[DialectKind])] = &[
    ("AM02", &[]),
    ("CP02", &[DialectKind::Mysql, DialectKind::Mariadb]),
    ("RF03", &[]),
    ("ST06", &[]),
    ("ST07", &[]),
];

pub struct SqruffEngine {
    linter: Linter,
    config_found: bool,
    risky_fix_rules: Vec<&'static str>,
}

impl SqruffEngine {
    /// Production path: sqruff-native config discovery.
    /// - `.sqruff` (and the other files sqruff reads) in the user config
    ///   directory, home, the cwd and its ancestors are discovered automatically.
    /// - `extra_config`: an explicit config file (`--config`) applied on top of
    ///   the discovered ones, or None.
    /// - `dialect` (`--dialect`) overrides the config's dialect; with neither,
    ///   `postgres` is used.
    pub fn discover(dialect: Option<&str>, extra_config: Option<&Path>) -> Result<Self, String> {
        let extra_config = extra_config
            .map(|path| {
                path.to_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("config path {} is not valid UTF-8", path.display()))
            })
            .transpose()?;
        let has_extra_config = extra_config.is_some();
        let raw = ConfigLoader
            .try_load_config_up_to_path(Path::new("."), extra_config, false)
            .map_err(|e| e.to_string())?;
        // sqruff keeps only its own sections, so `raw` is empty both when no
        // config file exists and when the files hold no sqruff option.
        let config_found = has_extra_config || !raw.is_empty();
        let configured = configured_dialect(raw.get("core"))?;
        let dialect = match (dialect, configured) {
            (Some(name), _) => Some(parse_dialect(name)?),
            (None, None) => Some(DialectKind::Postgres),
            (None, Some(_)) => None,
        };
        Self::build(raw, dialect, config_found)
    }

    /// Build from an explicit config string (tests, or callers that hold their
    /// own `.sqruff` text). The string must set `dialect`.
    pub fn from_source(config_src: &str) -> Result<Self, String> {
        let raw = ConfigLoader::try_from_source(config_src, None).map_err(|e| e.to_string())?;
        configured_dialect(raw.get("core"))?;
        Self::build(raw, None, true)
    }

    /// `raw` must hold no dialect name that `DialectKind` rejects, because
    /// sqruff panics on one while building the config.
    fn build(raw: ConfigMap, dialect: Option<DialectKind>, config_found: bool) -> Result<Self, String> {
        let linter = without_panicking(move || {
            let mut config = FluffConfig::new(raw, None, None);
            if let Some(kind) = dialect {
                config.override_dialect(kind)?;
            }
            Linter::new(config, None, None, true)
        })??;
        // A config-level problem such as an unknown rule code makes every lint
        // fail, so report it once here instead of once per query.
        linter.lint_string("SELECT 1\n", None, false).map_err(|e| e.to_string())?;
        let kind = linter.config().dialect_kind();
        let risky_fix_rules = linter
            .rules()
            .map_err(|e| e.to_string())?
            .iter()
            .filter(|rule| rule.force_enable() || !rule.dialect_skip().contains(&kind))
            .filter_map(|rule| {
                RISKY_FIX_RULES
                    .iter()
                    .find(|(code, dialects)| *code == rule.code() && (dialects.is_empty() || dialects.contains(&kind)))
                    .map(|(code, _)| *code)
            })
            .collect();
        Ok(Self { linter, config_found, risky_fix_rules })
    }

    /// Whether `--config` was given or a discovered config file sets a sqruff
    /// option. When `false`, sqruff's default rules apply.
    pub fn config_found(&self) -> bool {
        self.config_found
    }

    /// The enabled rules whose fix can change what a query does with the
    /// configured dialect.
    pub fn risky_fix_rules(&self) -> &[&'static str] {
        &self.risky_fix_rules
    }

    /// Lint only. `Err` carries sqruff's message when it cannot process `sql`
    /// at all (for example an invalid inline `-- sqruff:` config line).
    pub fn lint(&self, sql: &str) -> Result<Vec<Finding>, String> {
        let lf = self.linter.lint_string(sql, None, false).map_err(|e| e.to_string())?;
        Ok(lf.violations().iter().map(Finding::from).collect())
    }

    /// Fix; returns `(fixed_sql, findings)`, where `findings` are the findings
    /// sqruff reported on its first pass over `sql`, fixable or not, or
    /// sqruff's message as in [`lint`](Self::lint).
    pub fn fix(&self, sql: &str) -> Result<(String, Vec<Finding>), String> {
        let lf = self.linter.lint_string(sql, None, true).map_err(|e| e.to_string())?;
        let findings = lf.violations().iter().map(Finding::from).collect();
        Ok((lf.fix_string(), findings))
    }
}

/// Run `build`, turning a panic into an error: sqruff panics on some invalid
/// config values instead of returning an error.
fn without_panicking<T>(build: impl FnOnce() -> T) -> Result<T, String> {
    // The panic hook is process-wide, so swapping it per call would silence
    // other threads too, or leave it silent when two calls overlap. Wrap the
    // installed hook once instead, and mute it only on this thread.
    thread_local! {
        static MUTED: Cell<bool> = const { Cell::new(false) };
    }
    static WRAP_HOOK: Once = Once::new();
    WRAP_HOOK.call_once(|| {
        let hook = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if !MUTED.get() {
                hook(info);
            }
        }));
    });
    MUTED.set(true);
    let result = panic::catch_unwind(AssertUnwindSafe(build));
    MUTED.set(false);
    result.map_err(|payload| format!("invalid sqruff config: {}", panic_message(payload.as_ref())))
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown error")
}

/// The dialect set in a loaded config's `core` section (`[sqruff] dialect`), if
/// any, accepted exactly as sqruff accepts it.
fn configured_dialect(core: Option<&Value>) -> Result<Option<DialectKind>, String> {
    let Some(Value::String(name)) = core.and_then(Value::as_map).and_then(|c| c.get("dialect"))
    else {
        return Ok(None);
    };
    DialectKind::from_str(name).map(Some).map_err(|_| {
        format!("unknown dialect '{name}' in the sqruff config; {}", valid_dialects())
    })
}

/// A `--dialect` name, in any case.
fn parse_dialect(name: &str) -> Result<DialectKind, String> {
    DialectKind::from_str(&name.to_ascii_lowercase())
        .map_err(|_| format!("unknown dialect '{name}'; {}", valid_dialects()))
}

fn valid_dialects() -> String {
    format!("valid dialects: {}", dialect_readout().join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dialect_flag_accepts_every_sqruff_dialect_in_any_case() {
        assert_eq!(parse_dialect("mariadb"), Ok(DialectKind::Mariadb));
        assert_eq!(parse_dialect("MariaDB"), Ok(DialectKind::Mariadb));
        for name in dialect_readout() {
            assert!(parse_dialect(&name.to_uppercase()).is_ok(), "{name}");
        }
    }

    #[test]
    fn an_unknown_dialect_flag_is_an_error_naming_it() {
        let err = parse_dialect("NoSuchSql").unwrap_err();
        assert!(err.contains("NoSuchSql"), "{err}");
    }

    const BAD_CONFIGS: &[(&str, &str)] = &[
        ("[sqruff]\ndialect = postgres\nmax_line_length = abc\n", "invalid sqruff config"),
        (
            "[sqruff]\ndialect = postgres\n\n[sqruff:layout:type:comma]\nline_position = bogus\n",
            "invalid sqruff config",
        ),
        ("[sqruff]\ndialect = postgres\nrules = NOPE01\n", "NOPE01"),
        ("[sqruff]\ndialect = nope\n", "nope"),
        ("[sqruff]\ndialect = postgres\nmax_parse_depth = abc\n", "max_parse_depth"),
        ("[sqruff]\ndialect = postgres\ntemplater = nope\n", "nope"),
    ];

    #[test]
    fn an_invalid_config_is_an_error_not_a_panic() {
        for (config, problem) in BAD_CONFIGS {
            let result = std::panic::catch_unwind(|| SqruffEngine::from_source(config).err());
            let err = result
                .unwrap_or_else(|_| panic!("panicked on {config:?}"))
                .unwrap_or_else(|| panic!("accepted {config:?}"));
            assert!(err.contains(problem), "{config:?}: {err}");
        }
    }

    fn codes(engine: &SqruffEngine, sql: &str) -> Vec<(String, bool)> {
        engine.lint(sql).unwrap().into_iter().map(|f| (f.code, f.unparsable)).collect()
    }

    #[test]
    fn findings_that_are_not_rules_get_codes_pinned_byte_for_byte() {
        let engine = SqruffEngine::from_source("[sqruff]\ndialect = postgres\nrules = CV05\n").unwrap();
        assert_eq!(codes(&engine, "SELECT FROM WHERE ((\n"), [("PARSE".to_string(), true)]);
        assert_eq!(
            codes(&engine, "SELECT 'abc\n"),
            [("LEX".to_string(), true), ("PARSE".to_string(), true)]
        );
        assert_eq!(
            codes(&engine, "SELECT a FROM t WHERE x = NULL -- noqa:\n"),
            [("CV05".to_string(), false), ("NOQA".to_string(), false)]
        );
    }

    #[test]
    fn a_rule_that_failed_inside_sqruff_is_internal_and_blocks_fix() {
        // No known SQL makes a sqruff 0.44 rule fail, so the finding is built
        // the way its linter builds one.
        let failed = SQLBaseError {
            description: "Unexpected exception. Could you open an issue".to_string(),
            ..SQLBaseError::default()
        };
        let finding = Finding::from(&failed);
        assert_eq!(finding.code, INTERNAL);
        assert!(finding.unparsable);
    }

    #[test]
    fn the_risky_fix_rules_are_the_enabled_ones_that_sqruff_runs_with_the_dialect() {
        let all = "AM02,CP02,CV05,RF03,ST06,ST07";
        for (dialect, rules, expected) in [
            ("postgres", "CV05", &[][..]),
            ("postgres", all, &["RF03", "ST06", "ST07"]),
            ("mysql", all, &["AM02", "CP02", "RF03", "ST06", "ST07"]),
            // sqruff skips ST07 on clickhouse.
            ("clickhouse", "ST07", &[]),
        ] {
            let config = format!("[sqruff]\ndialect = {dialect}\nrules = {rules}\n");
            let engine = SqruffEngine::from_source(&config).unwrap();
            assert_eq!(engine.risky_fix_rules(), expected, "{config:?}");
        }
        let excluded = "[sqruff]\ndialect = sqlite\nrules = AM02,ST06\nexclude_rules = ST06\n";
        assert_eq!(SqruffEngine::from_source(excluded).unwrap().risky_fix_rules(), ["AM02"]);
    }

    #[test]
    fn valid_configs_build() {
        for config in [
            "[sqruff]\ndialect = postgres\n",
            "[sqruff]\ndialect = postgres\nrules = LT01,LT02,CV05,CP01\n",
            "[sqruff]\ndialect = postgres\nrules = LT09,CV05\n",
            "[sqruff]\ndialect = mysql\nmax_line_length = 100\n",
            "[sqruff]\ndialect = postgres\n\n[sqruff:layout:type:comma]\nline_position = leading\n",
        ] {
            let engine = SqruffEngine::from_source(config);
            assert!(engine.is_ok(), "{config:?}: {:?}", engine.err());
        }
    }
}
