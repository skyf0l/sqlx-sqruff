//! The output formats that scripts depend on: finding codes, `--format jsonl`,
//! the spelling of file paths, the order of the output and of the `error:`
//! lines, and the summary line, through the real binary.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;

const CONFIG: &str = "[sqruff]\ndialect = postgres\nrules = CV05,LT01\n";
const CV05_QUERY: &str = "fn f() {\n    let _ = sqlx::query(\"SELECT a FROM t WHERE x = NULL\");\n}\n";

/// A throwaway project directory with a `.sqruff`, removed on drop. It also
/// serves as `HOME` and `XDG_CONFIG_HOME`, so no user-level sqruff config can
/// affect the result.
struct Project(PathBuf);

impl Project {
    fn new(tag: &str, files: &[(&str, &[u8])]) -> Self {
        let dir = std::env::temp_dir().join(format!("sqlx_sqruff_output_{}_{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        fs::write(dir.join(".sqruff"), CONFIG).unwrap();
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

/// Every stdout line of `out` parsed as one JSON object.
fn json_lines(out: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line:?}\n{}", report(out))))
        .collect()
}

/// The `(file, line, col)` of each JSON finding, in output order.
fn positions(findings: &[Value]) -> Vec<(PathBuf, u64, u64)> {
    findings
        .iter()
        .map(|f| {
            (
                PathBuf::from(f["file"].as_str().unwrap()),
                f["line"].as_u64().unwrap(),
                f["col"].as_u64().unwrap(),
            )
        })
        .collect()
}

/// `rel`, a `/`-separated relative path, with the separator of the OS, as the
/// tool prints it.
fn os_path(rel: &str) -> String {
    rel.split('/').collect::<PathBuf>().display().to_string()
}

#[test]
fn non_rule_codes_are_parse_lex_and_internal_byte_for_byte_in_every_format() {
    let src = concat!(
        "fn f() {\n",
        "    let _ = sqlx::query(\"SELECT FROM WHERE ((\");\n",
        "    let _ = sqlx::query(\"SELECT 'abc\");\n",
        "    let _ = sqlx::query!(r#\"\n",
        "        -- sqruff:dialect:not_a_dialect\n",
        "        SELECT a FROM t\n",
        "    \"#);\n",
        "}\n",
    );
    let p = Project::new("codes", &[("a.rs", src.as_bytes())]);

    let jsonl = p.run("", &["check", "--format", "jsonl"]);
    assert_eq!(jsonl.status.code(), Some(1), "{}", report(&jsonl));
    let codes: Vec<(u64, String)> = json_lines(&jsonl)
        .iter()
        .map(|f| (f["line"].as_u64().unwrap(), f["code"].as_str().unwrap().to_string()))
        .collect();
    for (line, code) in [(2, "PARSE"), (3, "LEX"), (3, "PARSE"), (4, "INTERNAL")] {
        assert!(codes.contains(&(line, code.to_string())), "{code} at line {line}: {}", report(&jsonl));
    }

    let human = p.run("", &["check"]);
    assert_eq!(human.status.code(), Some(1), "{}", report(&human));
    let stdout = String::from_utf8_lossy(&human.stdout);
    for (line, code) in [(2, "PARSE"), (3, "LEX"), (4, "INTERNAL")] {
        let at = format!("a.rs:{line}:");
        assert!(
            stdout.lines().any(|l| l.starts_with(&at) && l.split_whitespace().nth(1) == Some(code)),
            "{code} at {at}: {}",
            report(&human)
        );
    }
    for old in ["????", "LXR", "ERROR"] {
        assert!(!stdout.contains(old), "{old}: {}", report(&human));
    }
}

#[test]
fn format_json_is_rejected_by_argument_parsing() {
    let p = Project::new("format_json", &[("a.rs", CV05_QUERY.as_bytes())]);
    let out = p.run("", &["check", "--format", "json"]);
    assert_eq!(out.status.code(), Some(2), "{}", report(&out));
    assert!(out.stdout.is_empty(), "{}", report(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--format") && stderr.contains("jsonl"), "{}", report(&out));
}

#[test]
fn format_jsonl_prints_one_json_object_per_finding() {
    let p = Project::new("format_jsonl", &[("a.rs", CV05_QUERY.as_bytes())]);
    let out = p.run("", &["check", "--format", "jsonl"]);
    assert_eq!(out.status.code(), Some(1), "{}", report(&out));
    let findings = json_lines(&out);
    assert_eq!(findings.len(), 1, "{}", report(&out));
    assert_eq!(findings[0]["code"], "CV05", "{}", report(&out));
    assert_eq!(findings[0]["file"], "a.rs", "{}", report(&out));
}

/// A project with two directories and several files, one that does not parse
/// as Rust and one that is not UTF-8.
fn two_dirs(tag: &str) -> Project {
    Project::new(
        tag,
        &[
            ("src/a.rs", CV05_QUERY.as_bytes()),
            ("src/b.rs", CV05_QUERY.as_bytes()),
            ("src/bad.rs", b"fn f() {\n    let _ = sqlx::query(\"SELECT 1\");\n"),
            ("src/nested/c.rs", CV05_QUERY.as_bytes()),
            ("src/not_utf8.rs", b"fn f() {}\n\xff\xfe\n"),
            ("tests/t.rs", CV05_QUERY.as_bytes()),
        ],
    )
}

const COMMANDS: [&[&str]; 5] = [&["check"], &["check", "--format", "jsonl"], &["fix", "--check"], &["list"], &["dump"]];

#[test]
fn every_command_prints_a_file_the_same_whichever_way_its_directory_is_named() {
    let p = two_dirs("same_path");
    let absolute = p.path("src").display().to_string();
    for command in COMMANDS {
        let runs: Vec<Output> = [&["src"][..], &["./src"], &[absolute.as_str()], &["src", "./src/a.rs"]]
            .iter()
            .map(|paths| p.run("", &[command, paths].concat()))
            .collect();
        for out in &runs[1..] {
            assert_eq!(out.status.code(), runs[0].status.code(), "{command:?}: {}", report(out));
            assert_eq!(out.stdout, runs[0].stdout, "{command:?}:\n{}\n---\n{}", report(out), report(&runs[0]));
            assert_eq!(out.stderr, runs[0].stderr, "{command:?}:\n{}\n---\n{}", report(out), report(&runs[0]));
        }

        let out = &runs[0];
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if command.contains(&"jsonl") {
            let files: Vec<Value> = json_lines(out).iter().map(|f| f["file"].clone()).collect();
            assert!(files.contains(&Value::from(os_path("src/a.rs"))), "{command:?}: {}", report(out));
        } else {
            assert!(stdout.contains(&os_path("src/a.rs")), "{command:?}: {}", report(out));
        }
        let not_utf8 = format!("error: {}: ", os_path("src/not_utf8.rs"));
        assert!(stderr.lines().any(|l| l.starts_with(&not_utf8)), "{command:?}: {}", report(out));
        let bad = format!("error: {}:", os_path("src/bad.rs"));
        assert!(stderr.lines().any(|l| l.starts_with(&bad)), "{command:?}: {}", report(out));
        for spelling in ["./", ".\\", absolute.as_str()] {
            assert!(!stdout.contains(spelling) && !stderr.contains(spelling), "{command:?} {spelling}: {}", report(out));
        }
    }
}

#[test]
fn with_no_path_a_file_is_printed_as_when_its_directory_is_named() {
    let p = two_dirs("no_path");
    for command in COMMANDS {
        let named = p.run("", &[command, &["src", "tests"]].concat());
        let default = p.run("", command);
        assert_eq!(default.status.code(), named.status.code(), "{command:?}: {}", report(&default));
        assert_eq!(default.stdout, named.stdout, "{command:?}:\n{}\n---\n{}", report(&default), report(&named));
        assert_eq!(default.stderr, named.stderr, "{command:?}:\n{}\n---\n{}", report(&default), report(&named));
    }
}

#[test]
fn a_file_outside_the_current_directory_is_printed_as_one_absolute_path() {
    let p = two_dirs("outside");
    fs::create_dir_all(p.path("other")).unwrap();
    let absolute = p.path("src").display().to_string();
    let relative = p.run("other", &["check", "--format", "jsonl", "../src"]);
    let named = p.run("other", &["check", "--format", "jsonl", &absolute]);
    assert_eq!(relative.stdout, named.stdout, "{}\n---\n{}", report(&relative), report(&named));
    assert_eq!(relative.stderr, named.stderr, "{}\n---\n{}", report(&relative), report(&named));
    let files: Vec<String> =
        json_lines(&relative).iter().map(|f| f["file"].as_str().unwrap().to_string()).collect();
    assert!(!files.is_empty(), "{}", report(&relative));
    let src = p.path("src").canonicalize().unwrap();
    for file in &files {
        assert!(Path::new(file).is_absolute(), "{file}");
        assert!(!file.starts_with(r"\\?\"), "{file}");
        assert!(Path::new(file).canonicalize().unwrap().starts_with(&src), "{file}");
    }
}

#[test]
fn findings_are_ordered_by_path_then_line_then_column() {
    // Created out of order, so that the order in which the file system lists
    // them is unlikely to be sorted. The first query has two findings.
    let two_rules = "fn f() {\n    let _ = sqlx::query(\"SELECT a FROM t WHERE x = NULL AND b  = 1\");\n    let _ = sqlx::query(\"SELECT b FROM t WHERE x = NULL\");\n}\n";
    let names = ["src/m.rs", "src/k.rs", "src/z.rs", "src/m/x.rs", "src/a.rs", "src/q.rs", "src/c.rs", "lib/b.rs", "src/b.rs"];
    let files: Vec<(&str, &[u8])> = names.iter().map(|name| (*name, two_rules.as_bytes())).collect();
    let p = Project::new("sorted", &files);

    for args in [&["check", "--format", "jsonl"][..], &["check", "--format", "jsonl", "src", "lib"]] {
        let out = p.run("", args);
        assert_eq!(out.status.code(), Some(1), "{}", report(&out));
        let got = positions(&json_lines(&out));
        assert_eq!(got.len(), names.len() * 3, "{args:?}: {}", report(&out));
        let mut sorted = got.clone();
        sorted.sort();
        assert_eq!(got, sorted, "{args:?}: {}", report(&out));
    }

    let human = p.run("", &["check"]);
    let jsonl = p.run("", &["check", "--format", "jsonl"]);
    let human_positions: Vec<(PathBuf, u64, u64)> = String::from_utf8_lossy(&human.stdout)
        .lines()
        .map(|line| {
            let (at, _finding) = line.split_once(": ").unwrap();
            let mut parts = at.rsplitn(3, ':');
            let col = parts.next().unwrap().parse().unwrap();
            let line = parts.next().unwrap().parse().unwrap();
            (PathBuf::from(parts.next().unwrap()), line, col)
        })
        .collect();
    assert_eq!(human_positions, positions(&json_lines(&jsonl)), "{}", report(&human));

    for command in ["list", "dump"] {
        let out = p.run("", &[command]);
        assert_eq!(out.status.code(), Some(0), "{}", report(&out));
        let listed: Vec<(PathBuf, u64)> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let at = if command == "dump" { line.strip_prefix("----- ")?.strip_suffix(" -----")? } else { line };
                let (path, line) = at.rsplit_once(':')?;
                Some((PathBuf::from(path), line.parse().unwrap()))
            })
            .collect();
        assert_eq!(listed.len(), names.len() * 2, "{command}: {}", report(&out));
        let mut sorted = listed.clone();
        sorted.sort();
        assert_eq!(listed, sorted, "{command}: {}", report(&out));
    }
}

#[test]
fn fix_warnings_of_a_file_are_ordered_by_line() {
    // sqruff cannot parse the query on line 2, and the fix of the query on line
    // 3 needs a line break that a one-line literal cannot hold.
    let src = concat!(
        "fn f() {\n",
        "    let _ = sqlx::query(\"SELECT 123 REGEXP '23' FROM t WHERE x = NULL\");\n",
        "    let _ = sqlx::query(\"with x as (select 1 as a) select a from x\");\n",
        "}\n",
    );
    let p = Project::new(
        "fix_order",
        &[("a.rs", src.as_bytes()), ("all.sqruff", b"[sqruff]\ndialect = postgres\nrules = all\n")],
    );
    let out = p.run("", &["fix", "--check", "--config", "all.sqruff", "a.rs"]);
    assert_eq!(out.status.code(), Some(1), "{}", report(&out));
    let lines: Vec<u64> = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter_map(|l| l.strip_prefix("warning: a.rs:")?.split(':').next()?.parse().ok())
        .collect();
    assert_eq!(lines, [2, 3], "{}", report(&out));
}

/// The paths that the `error:` lines name, in the order they were printed.
fn error_paths(out: &Output) -> Vec<PathBuf> {
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter_map(|l| l.strip_prefix("error: "))
        .map(|rest| PathBuf::from(rest.split(':').next().unwrap_or_default()))
        .collect()
}

/// Not valid Rust, and parsed even when found by walking a directory.
const NOT_RUST: &[u8] = b"fn f( { query\n";

/// The commands that report `error:` lines, with each way of running `fix`.
const ERROR_COMMANDS: [&[&str]; 5] = [&["check"], &["fix", "--check"], &["fix"], &["list"], &["dump"]];

#[test]
fn error_lines_are_in_path_order_whatever_the_order_of_the_named_paths() {
    let p = Project::new("error_order", &[("a.rs", NOT_RUST), ("b/notes.txt", CV05_QUERY.as_bytes()), ("c.rs", NOT_RUST)]);
    for command in ERROR_COMMANDS {
        for (orders, expected) in [
            ([&["nosuchdir", "nosuch.rs"][..], &["nosuch.rs", "nosuchdir"]], &["nosuch.rs", "nosuchdir"][..]),
            ([&["c.rs", "b", "a.rs"], &["a.rs", "b", "c.rs"]], &["a.rs", "b", "c.rs"]),
        ] {
            let outs = orders.map(|paths| p.run("", &[command, paths].concat()));
            for (paths, out) in orders.iter().zip(&outs) {
                assert_eq!(out.status.code(), Some(2), "{command:?} {paths:?}: {}", report(out));
                let expected: Vec<PathBuf> = expected.iter().map(PathBuf::from).collect();
                assert_eq!(error_paths(out), expected, "{command:?} {paths:?}: {}", report(out));
            }
            assert_eq!(outs[0].stderr, outs[1].stderr, "{command:?}: {}\n{}", report(&outs[0]), report(&outs[1]));
        }
    }
}

#[cfg(unix)]
#[test]
fn an_unreadable_directory_found_by_walking_is_reported_in_path_order() {
    use std::os::unix::fs::PermissionsExt;

    let p = Project::new("error_order_walk", &[("a.rs", NOT_RUST), ("m/b.rs", CV05_QUERY.as_bytes()), ("z.rs", NOT_RUST)]);
    let locked = p.path("m");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let outs: Vec<_> = ERROR_COMMANDS.into_iter().map(|command| (command, p.run("", command))).collect();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    let expected: Vec<PathBuf> = ["a.rs", "m", "z.rs"].iter().map(PathBuf::from).collect();
    for (command, out) in &outs {
        assert_eq!(out.status.code(), Some(2), "{command:?}: {}", report(out));
        assert_eq!(error_paths(out), expected, "{command:?}: {}", report(out));
    }
}

/// A file that does not parse as Rust because its closing brace is missing.
const UNPARSABLE: &[u8] = b"fn f() {\n    let _ = sqlx::query!(\"SELECT a FROM t WHERE x = NULL\");\n";

#[test]
fn check_and_fix_count_the_errors_in_the_same_words() {
    let p = Project::new("summary", &[("a.rs", CV05_QUERY.as_bytes()), ("b.rs", UNPARSABLE)]);
    for command in [&["check"][..], &["fix", "--check"], &["fix"]] {
        let out = p.run("", &[command, &["a.rs", "b.rs"]].concat());
        assert_eq!(out.status.code(), Some(2), "{command:?}: {}", report(&out));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.lines().any(|l| l.contains("; 1 error(s).")), "{command:?}: {}", report(&out));
    }
}

/// The `N` of the `N error(s).` that ends the summary on the last stderr line.
fn summary_errors(out: &Output) -> usize {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let last = stderr.lines().last().unwrap_or_default();
    let count = last.strip_suffix(" error(s).").and_then(|rest| rest.rsplit(' ').next()).and_then(|n| n.parse().ok());
    count.unwrap_or_else(|| panic!("no error count in {last:?}: {}", report(out)))
}

#[test]
fn the_summary_counts_every_error_line_including_the_paths_that_select_no_file() {
    let p = Project::new("summary_paths", &[("a.rs", CV05_QUERY.as_bytes()), ("b.rs", UNPARSABLE)]);
    fs::create_dir_all(p.path("empty")).unwrap();
    let paths: Vec<&str> = ["a.rs", "b.rs", "empty"].into_iter().chain(cfg!(unix).then_some("locked")).collect();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::create_dir_all(p.path("locked")).unwrap();
        fs::set_permissions(p.path("locked"), fs::Permissions::from_mode(0o000)).unwrap();
    }
    let outs: Vec<_> = [&["check"][..], &["fix", "--check"], &["fix"]]
        .into_iter()
        .map(|command| (command, p.run("", &[command, paths.as_slice()].concat())))
        .collect();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(p.path("locked"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    for (command, out) in &outs {
        assert_eq!(out.status.code(), Some(2), "{command:?}: {}", report(out));
        let errors = String::from_utf8_lossy(&out.stderr).lines().filter(|l| l.starts_with("error:")).count();
        assert_eq!(errors, paths.len() - 1, "{command:?}: {}", report(out));
        assert_eq!(summary_errors(out), errors, "{command:?}: {}", report(out));
    }
}
