//! `--config` is applied on top of the discovered sqruff config, as in sqruff's
//! own CLI, instead of replacing it.

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

/// `SELECT  a` has a double space, which LT01 reports.
const LT01_QUERY: &str = "fn f() {\n    let _ = sqlx::query(\"SELECT  a FROM t\");\n}\n";
/// MySQL backtick identifiers do not parse as postgres.
const MYSQL_QUERY: &str = "fn f() {\n    let _ = sqlx::query(\"SELECT `a` FROM t\");\n}\n";

/// A throwaway project directory with a `.sqruff` and an extra config file,
/// removed on drop. It also serves as `HOME` and `XDG_CONFIG_HOME`, so no
/// user-level sqruff config can affect the result.
struct Project(PathBuf);

impl Project {
    fn new(tag: &str, project_config: &str, extra_config: &str, src: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("sqlx_sqruff_stack_{}_{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("q.rs"), src).unwrap();
        fs::write(dir.join(".sqruff"), project_config).unwrap();
        fs::write(dir.join("extra.cfg"), extra_config).unwrap();
        Self(dir)
    }

    fn check(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sqlx-sqruff"))
            .args(["check", "q.rs", "--config", "extra.cfg"])
            .current_dir(&self.0)
            .env("HOME", &self.0)
            .env("XDG_CONFIG_HOME", &self.0)
            .output()
            .unwrap()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn report(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn the_project_rules_stay_when_the_extra_config_does_not_set_rules() {
    let p = Project::new(
        "keeps_rules",
        "[sqruff]\ndialect = postgres\nrules = CV05\n",
        "[sqruff]\nmax_line_length = 100\n",
        LT01_QUERY,
    );
    let out = p.check();
    assert_eq!(out.status.code(), Some(0), "{}", report(&out));
}

#[test]
fn the_extra_config_overrides_the_project_dialect() {
    let p = Project::new(
        "dialect",
        "[sqruff]\ndialect = postgres\nrules = CV05\n",
        "[sqruff]\ndialect = mysql\n",
        MYSQL_QUERY,
    );
    let out = p.check();
    assert_eq!(out.status.code(), Some(0), "{}", report(&out));
}

#[test]
fn an_unknown_dialect_in_the_extra_config_is_an_error_naming_it() {
    let p = Project::new(
        "bad_dialect",
        "[sqruff]\ndialect = postgres\nrules = CV05\n",
        "[sqruff]\ndialect = mysqll\n",
        MYSQL_QUERY,
    );
    let out = p.check();
    assert_eq!(out.status.code(), Some(2), "{}", report(&out));
    assert!(String::from_utf8_lossy(&out.stderr).contains("mysqll"), "{}", report(&out));
}
