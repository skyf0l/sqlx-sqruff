//! Which `.rs` files the CLI scans: explicit paths, the `[sqlx-sqruff]` section
//! and ignore files, through the real binary.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const QUERY: &str = "fn f() {\n    let _ = sqlx::query(\"SELECT a FROM t\");\n}\n";
const CV05_QUERY: &str = "fn f() {\n    let _ = sqlx::query(\"SELECT a FROM t WHERE x = NULL\");\n}\n";

/// A throwaway project directory, removed on drop. It also serves as `HOME` and
/// `XDG_CONFIG_HOME`, so no user-level sqruff or git config can affect the
/// result.
struct Project(PathBuf);

impl Project {
    /// Create the directory with each `(relative path, contents)` file.
    fn new(tag: &str, files: &[(&str, &str)]) -> Self {
        let dir =
            std::env::temp_dir().join(format!("sqlx_sqruff_select_{}_{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        for (rel, contents) in files {
            let path = dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
        }
        Self(dir)
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    /// Run the binary from the project subdirectory `cwd` ("" for the root).
    fn run(&self, cwd: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sqlx-sqruff"))
            .args(args)
            .current_dir(self.0.join(cwd))
            .env("HOME", &self.0)
            .env("XDG_CONFIG_HOME", &self.0)
            .output()
            .unwrap()
    }

    /// The canonical paths of the given project files.
    fn files(&self, rels: &[&str]) -> BTreeSet<PathBuf> {
        rels.iter().map(|rel| self.path(rel).canonicalize().unwrap()).collect()
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

/// The canonical paths of the files that `list`, run from `cwd`, printed.
fn listed(out: &Output, cwd: &Path) -> BTreeSet<PathBuf> {
    assert_eq!(out.status.code(), Some(0), "{}", report(out));
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| {
            let (path, _line) = line.rsplit_once(':').unwrap_or_else(|| panic!("{line}"));
            cwd.join(path).canonicalize().unwrap()
        })
        .collect()
}

/// The stderr lines that start with `prefix` and contain every one of `parts`.
fn stderr_lines(out: &Output, prefix: &str, parts: &[&str]) -> Vec<String> {
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| l.starts_with(prefix) && parts.iter().all(|p| l.contains(p)))
        .map(str::to_string)
        .collect()
}

/// The canonical path of the file that a `<prefix> <path>: <message>` line
/// names. Paths are compared canonically because the same file can be spelled
/// differently, such as with the `\\?\` prefix that `canonicalize` adds on
/// Windows.
fn named_file(line: &str, prefix: &str, cwd: &Path) -> PathBuf {
    let rest = line.strip_prefix(prefix).unwrap_or_else(|| panic!("{line}")).trim_start();
    let (path, _message) = rest.split_once(": ").unwrap_or_else(|| panic!("{line}"));
    cwd.join(path).canonicalize().unwrap()
}

#[test]
fn a_path_that_does_not_exist_is_an_error_naming_it_before_any_work() {
    let p = Project::new("missing_path", &[("a.rs", QUERY)]);
    for command in [&["check"][..], &["fix", "--check"], &["fix"], &["list"], &["dump"]] {
        let mut args = command.to_vec();
        args.extend(["a.rs", "missing", "src/typo"]);
        let out = p.run("", &args);
        assert_eq!(out.status.code(), Some(2), "{command:?}: {}", report(&out));
        for missing in [PathBuf::from("missing"), Path::new("src").join("typo")] {
            let missing = format!("error: {}: ", missing.display());
            let lines = stderr_lines(&out, "error:", &[&missing, "no such file or directory"]);
            assert_eq!(lines.len(), 1, "{command:?}: {}", report(&out));
        }
        assert!(out.stdout.is_empty(), "{command:?}: {}", report(&out));
    }
}

#[cfg(unix)]
#[test]
fn an_unreadable_directory_is_reported_and_the_walk_continues() {
    use std::os::unix::fs::PermissionsExt;

    let p = Project::new("unreadable_dir", &[("a.rs", QUERY), ("locked/b.rs", QUERY)]);
    let locked = p.path("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let out = p.run("", &["list", "."]);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(out.status.code(), Some(2), "{}", report(&out));
    assert_eq!(stderr_lines(&out, "error:", &["locked"]).len(), 1, "{}", report(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.lines().any(|l| l.contains("a.rs:")), "{}", report(&out));
}

#[test]
fn inline_comments_are_not_part_of_a_section_value() {
    let config = "[sqlx-sqruff]\n\
                  exclude = skip/**   # generated code\n\
                  respect_gitignore = false  ; scan ignored files too\n";
    let p = Project::new(
        "inline_comments",
        &[
            (".sqruff", config),
            (".ignore", "ignored.rs\n"),
            ("a.rs", QUERY),
            ("ignored.rs", QUERY),
            ("skip/b.rs", QUERY),
        ],
    );
    let out = p.run("", &["list"]);
    assert_eq!(listed(&out, &p.0), p.files(&["a.rs", "ignored.rs"]), "{}", report(&out));
}

#[test]
fn continuation_lines_extend_a_glob_list() {
    let config = "[sqlx-sqruff]\ninclude =\n    a/*.rs\n    b/*.rs, c/*.rs\n";
    let p = Project::new(
        "continuation",
        &[(".sqruff", config), ("a/x.rs", QUERY), ("b/y.rs", QUERY), ("c/z.rs", QUERY), ("d/w.rs", QUERY)],
    );
    let out = p.run("", &["list"]);
    assert_eq!(listed(&out, &p.0), p.files(&["a/x.rs", "b/y.rs", "c/z.rs"]), "{}", report(&out));
}

#[test]
fn an_unknown_key_in_the_section_is_reported_with_the_config_path() {
    let p = Project::new("unknown_key", &[(".sqruff", "[sqlx-sqruff]\nexclud = skip/**\n"), ("a.rs", QUERY)]);
    let out = p.run("", &["list"]);
    assert_eq!(out.status.code(), Some(0), "{}", report(&out));
    let warnings = stderr_lines(&out, "warning:", &["'exclud'", "[sqlx-sqruff]"]);
    assert_eq!(warnings.len(), 1, "{}", report(&out));
    let config = p.path(".sqruff").canonicalize().unwrap();
    assert_eq!(named_file(&warnings[0], "warning:", &p.0), config, "{}", report(&out));
}

#[test]
fn an_invalid_boolean_is_an_error_naming_the_key_the_value_and_the_file() {
    let p = Project::new(
        "bad_bool",
        &[(".sqruff", "[sqlx-sqruff]\nrespect_gitignore = maybe\n"), ("a.rs", QUERY)],
    );
    let config = p.path(".sqruff").canonicalize().unwrap();
    for command in ["list", "check"] {
        let out = p.run("", &[command]);
        assert_eq!(out.status.code(), Some(2), "{command}: {}", report(&out));
        let errors = stderr_lines(&out, "error:", &["respect_gitignore", "maybe"]);
        assert_eq!(errors.len(), 1, "{command}: {}", report(&out));
        assert_eq!(named_file(&errors[0], "error:", &p.0), config, "{command}: {}", report(&out));
    }
}

const ANCHORED: &str = "[sqlx-sqruff]\ninclude = src/**/*.rs\nexclude = src/generated/**\n";

#[test]
fn globs_are_relative_to_the_config_file_not_the_cwd() {
    let p = Project::new(
        "anchored_cwd",
        &[(".sqruff", ANCHORED), ("src/a.rs", QUERY), ("src/generated/g.rs", QUERY), ("tests/t.rs", QUERY)],
    );
    let expected = p.files(&["src/a.rs"]);
    let from_root = p.run("", &["list"]);
    assert_eq!(listed(&from_root, &p.0), expected, "{}", report(&from_root));
    let from_src = p.run("src", &["list"]);
    assert_eq!(listed(&from_src, &p.path("src")), expected, "{}", report(&from_src));
}

#[test]
fn an_anchored_exclude_prunes_under_an_explicit_path() {
    let p = Project::new(
        "anchored_explicit",
        &[(".sqruff", ANCHORED), ("src/a.rs", QUERY), ("src/generated/g.rs", QUERY)],
    );
    let out = p.run("", &["list", "src"]);
    assert_eq!(listed(&out, &p.0), p.files(&["src/a.rs"]), "{}", report(&out));
}

#[test]
fn a_section_in_sqruff_ini_is_used() {
    let p = Project::new(
        "sqruff_ini",
        &[(".sqruff.ini", "[sqlx-sqruff]\nexclude = skip/**\n"), ("a.rs", QUERY), ("skip/b.rs", QUERY)],
    );
    let out = p.run("", &["list"]);
    assert_eq!(listed(&out, &p.0), p.files(&["a.rs"]), "{}", report(&out));
}

#[test]
fn a_nested_config_without_the_section_does_not_hide_the_parents_section() {
    let p = Project::new(
        "nested",
        &[
            (".sqruff", "[sqlx-sqruff]\nexclude = sub/skip/**\n"),
            ("sub/.sqruff", "[sqruff]\ndialect = postgres\n"),
            ("sub/a.rs", QUERY),
            ("sub/skip/b.rs", QUERY),
        ],
    );
    let out = p.run("sub", &["list"]);
    assert_eq!(listed(&out, &p.path("sub")), p.files(&["sub/a.rs"]), "{}", report(&out));
}

#[test]
fn the_section_of_the_extra_config_wins_over_the_discovered_one() {
    let p = Project::new(
        "extra_wins",
        &[
            (".sqruff", "[sqlx-sqruff]\nexclude = a/**\n"),
            ("extra.cfg", "[sqlx-sqruff]\nexclude = b/**\n"),
            ("a/x.rs", QUERY),
            ("b/y.rs", QUERY),
        ],
    );
    let out = p.run("", &["list", "--config", "extra.cfg"]);
    assert_eq!(listed(&out, &p.0), p.files(&["a/x.rs"]), "{}", report(&out));
}

#[test]
fn an_extra_config_without_the_section_keeps_the_discovered_one() {
    let p = Project::new(
        "extra_without_section",
        &[
            (".sqruff", "[sqlx-sqruff]\nexclude = a/**\n"),
            ("extra.cfg", "[sqruff]\ndialect = postgres\n"),
            ("a/x.rs", QUERY),
            ("b/y.rs", QUERY),
        ],
    );
    let out = p.run("", &["list", "--config", "extra.cfg"]);
    assert_eq!(listed(&out, &p.0), p.files(&["b/y.rs"]), "{}", report(&out));
}

#[test]
fn a_file_named_twice_is_processed_once() {
    let p = Project::new(
        "duplicate",
        &[(".sqruff", "[sqruff]\ndialect = postgres\nrules = CV05\n"), ("src/a.rs", CV05_QUERY)],
    );
    let out = p.run("", &["check", "src", "src/a.rs", "./src"]);
    assert_eq!(out.status.code(), Some(1), "{}", report(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.lines().filter(|l| l.contains("a.rs:")).count(), 1, "{}", report(&out));
}

#[test]
fn gitignore_is_honoured_outside_a_git_repository() {
    let p = Project::new(
        "gitignore",
        &[(".gitignore", "ignored.rs\n"), ("a.rs", QUERY), ("ignored.rs", QUERY)],
    );
    let out = p.run("", &["list"]);
    assert_eq!(listed(&out, &p.0), p.files(&["a.rs"]), "{}", report(&out));
}
