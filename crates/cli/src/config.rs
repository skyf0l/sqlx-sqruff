//! CLI-only settings read from a `[sqlx-sqruff]` section of a sqruff config
//! file.
//!
//! sqruff's own config parser DROPS every section it doesn't recognise (only
//! `sqruff` / `sqruff:*` and the `sqlfluff` aliases survive), so a
//! `[sqlx-sqruff]` section is invisible to the linter — it is ours alone. We
//! read it with a tiny INI scan rather than through `FluffConfig`, whose section
//! accessors (`get` / `get_section`) panic on an absent section.
//!
//! The section comes from the `--config` file when that file has one, else from
//! the nearest INI-format sqruff config file, walking up from the cwd, that has
//! one. Its globs are relative to the directory of that file.
//!
//! Defaults reproduce "scan every `.rs`, respecting `.gitignore`", so the
//! section is purely opt-in.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use crate::paths::shown;

const SECTION: &str = "sqlx-sqruff";
/// The INI-format files sqruff reads in each directory, in its load order: a
/// later file overrides an earlier one.
const CONFIG_FILES: [&str; 3] = [".sqlfluff", ".sqruff", ".sqruff.ini"];

/// File-selection settings. See the module docs for the include/exclude matrix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliSettings {
    /// Whitelist globs. When set (and no explicit paths are passed), only
    /// matching files are scanned.
    pub include: Vec<String>,
    /// Blacklist globs, always applied on top of `include` / explicit paths.
    pub exclude: Vec<String>,
    /// Honour `.gitignore` / `.ignore` / `.git/info/exclude` during the walk.
    pub respect_gitignore: bool,
    /// The directory of the config file that set these, which the globs are
    /// relative to. `None` when no file has a `[sqlx-sqruff]` section.
    pub base: Option<PathBuf>,
}

impl Default for CliSettings {
    fn default() -> Self {
        Self { include: Vec::new(), exclude: Vec::new(), respect_gitignore: true, base: None }
    }
}

/// A parsed `[sqlx-sqruff]` section.
#[derive(Debug, PartialEq, Eq)]
struct Section {
    settings: CliSettings,
    unknown_keys: Vec<String>,
}

impl CliSettings {
    /// Load the section from `extra_config` (an explicit `--config`) when that
    /// file has one, otherwise from the nearest sqruff config file that has one,
    /// walking up from the cwd. With none, [`CliSettings::default`].
    pub fn load(extra_config: Option<&Path>) -> Result<Self> {
        if let Some(path) = extra_config
            && let Some(settings) = Self::load_file(path)?
        {
            return Ok(settings);
        }
        let mut dir = std::env::current_dir().context("resolving current directory")?;
        loop {
            for name in CONFIG_FILES.iter().rev() {
                let candidate = dir.join(name);
                if candidate.is_file()
                    && let Some(settings) = Self::load_file(&candidate)?
                {
                    return Ok(settings);
                }
            }
            if !dir.pop() {
                return Ok(Self::default());
            }
        }
    }

    /// The section of the config file at `path`, or `None` when it has none.
    fn load_file(path: &Path) -> Result<Option<Self>> {
        let shown = shown(path);
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", shown.display()))?;
        let Some(section) = parse(&text).map_err(|e| anyhow!("{}: {e}", shown.display()))? else {
            return Ok(None);
        };
        for key in &section.unknown_keys {
            eprintln!("warning: {}: unknown key '{key}' in [{SECTION}]", shown.display());
        }
        let file = path
            .canonicalize()
            .with_context(|| format!("resolving config {}", shown.display()))?;
        Ok(Some(Self { base: file.parent().map(Path::to_path_buf), ..section.settings }))
    }
}

/// Parse the `[sqlx-sqruff]` section out of a config file's text, or `None`
/// when the text has no such section. Absent keys keep their default.
fn parse(text: &str) -> Result<Option<Section>, String> {
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut found = false;
    let mut in_section = false;
    let mut in_value = false;
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        let line = strip_inline_comment(trimmed);
        if in_value && raw.starts_with(char::is_whitespace) {
            if let Some((_, value)) = entries.last_mut() {
                value.push('\n');
                value.push_str(line);
            }
            continue;
        }
        in_value = false;
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_section = header.trim() == SECTION;
            found |= in_section;
            continue;
        }
        if in_section && let Some((key, value)) = line.split_once('=') {
            entries.push((key.trim().to_string(), value.trim().to_string()));
            in_value = true;
        }
    }
    if !found {
        return Ok(None);
    }
    let mut settings = CliSettings::default();
    let mut unknown_keys = Vec::new();
    for (key, value) in entries {
        match key.as_str() {
            "include" => settings.include = split_globs(&value),
            "exclude" => settings.exclude = split_globs(&value),
            "respect_gitignore" => {
                settings.respect_gitignore = parse_bool(&value).ok_or_else(|| {
                    format!(
                        "invalid value '{value}' for 'respect_gitignore' in [{SECTION}]: \
                         expected true or false"
                    )
                })?;
            }
            _ => unknown_keys.push(key),
        }
    }
    Ok(Some(Section { settings, unknown_keys }))
}

/// Cut `line` at the first `#` or `;` that follows whitespace.
fn strip_inline_comment(line: &str) -> &str {
    let mut previous = None;
    for (i, c) in line.char_indices() {
        if matches!(c, '#' | ';') && previous.is_some_and(char::is_whitespace) {
            return line[..i].trim_end();
        }
        previous = Some(c);
    }
    line
}

/// Split a glob list separated by commas or line breaks, trimming each entry
/// and dropping blanks.
fn split_globs(value: &str) -> Vec<String> {
    value.split([',', '\n']).map(str::trim).filter(|s| !s.is_empty()).map(ToOwned::to_owned).collect()
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The settings of a text that has a valid `[sqlx-sqruff]` section.
    fn section(text: &str) -> CliSettings {
        parse(text).unwrap().unwrap().settings
    }

    #[test]
    fn no_section_when_the_section_is_absent() {
        assert_eq!(parse("[sqruff]\ndialect = postgres\n"), Ok(None));
    }

    #[test]
    fn an_empty_section_keeps_the_defaults() {
        assert_eq!(section("[sqruff]\ndialect = postgres\n[sqlx-sqruff]\n"), CliSettings::default());
    }

    #[test]
    fn reads_include_exclude_and_trims() {
        let s = section(
            "[sqlx-sqruff]\ninclude = tests/**/*.rs, crates/**/*.rs\nexclude = **/target/**\n",
        );
        assert_eq!(s.include, ["tests/**/*.rs", "crates/**/*.rs"]);
        assert_eq!(s.exclude, ["**/target/**"]);
        assert!(s.respect_gitignore);
    }

    #[test]
    fn respect_gitignore_false_and_section_isolation() {
        // Keys outside [sqlx-sqruff] must not leak in; `respect_gitignore` parses.
        let s = section(
            "[sqruff]\ninclude = should_be_ignored\n[sqlx-sqruff]\nrespect_gitignore = false\n",
        );
        assert!(s.include.is_empty());
        assert!(!s.respect_gitignore);
    }

    #[test]
    fn comments_and_blank_lines_skipped() {
        let s = section(
            "[sqlx-sqruff]\n# a comment\n\ninclude = a.rs\n; semicolon comment\nexclude = b.rs\n",
        );
        assert_eq!(s.include, ["a.rs"]);
        assert_eq!(s.exclude, ["b.rs"]);
    }

    #[test]
    fn an_inline_comment_after_whitespace_is_dropped() {
        let s = section(
            "[sqlx-sqruff]\ninclude = src/**/*.rs, a.rs   # main code\n\
             exclude = gen/**\t; generated\nrespect_gitignore = false # scan everything\n",
        );
        assert_eq!(s.include, ["src/**/*.rs", "a.rs"]);
        assert_eq!(s.exclude, ["gen/**"]);
        assert!(!s.respect_gitignore);
    }

    #[test]
    fn a_comment_character_inside_a_glob_is_kept() {
        let s = section("[sqlx-sqruff]\ninclude = src/#raw#/*.rs, a;b.rs\n");
        assert_eq!(s.include, ["src/#raw#/*.rs", "a;b.rs"]);
    }

    #[test]
    fn indented_lines_continue_the_previous_value() {
        let s = section(
            "[sqlx-sqruff]\ninclude =\n    src/**/*.rs\n    # a comment line\n\n    \
             tests/*.rs, benches/*.rs  # inline\nexclude = gen/**\n",
        );
        assert_eq!(s.include, ["src/**/*.rs", "tests/*.rs", "benches/*.rs"]);
        assert_eq!(s.exclude, ["gen/**"]);
    }

    #[test]
    fn indented_lines_of_another_section_do_not_leak_in() {
        let s = section("[sqlx-sqruff]\nexclude = gen/**\n[sqruff]\nrules =\n    include = x\n");
        assert!(s.include.is_empty());
        assert_eq!(s.exclude, ["gen/**"]);
    }

    #[test]
    fn unknown_keys_are_collected_in_order() {
        let parsed = parse("[sqlx-sqruff]\nexclud = a\ninclude = b.rs\nrespect = no\n").unwrap().unwrap();
        assert_eq!(parsed.unknown_keys, ["exclud", "respect"]);
        assert_eq!(parsed.settings.include, ["b.rs"]);
    }

    #[test]
    fn an_invalid_boolean_is_an_error_naming_the_key_and_the_value() {
        let err = parse("[sqlx-sqruff]\nrespect_gitignore = maybe # unsure\n").unwrap_err();
        assert!(err.contains("respect_gitignore"), "{err}");
        assert!(err.contains("'maybe'"), "{err}");
    }
}
