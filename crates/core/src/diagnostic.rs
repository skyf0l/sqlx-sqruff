//! A finding mapped back to a location *inside the Rust file*, plus renderers.

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub file: String,
    /// 1-based line in the `.rs` file.
    pub line: usize,
    /// 1-based column in the `.rs` file (best-effort).
    pub col: usize,
    pub code: String,
    pub message: String,
}

impl Diagnostic {
    /// rustc-style: `path:line:col: CODE message`.
    pub fn render_human(&self) -> String {
        format!("{}:{}:{}: {} {}", self.file, self.line, self.col, self.code, self.message)
    }

    /// Minimal JSON line (no external dep).
    pub fn render_json(&self) -> String {
        format!(
            "{{\"file\":{},\"line\":{},\"col\":{},\"code\":{},\"message\":{}}}",
            json_str(&self.file),
            self.line,
            self.col,
            json_str(&self.code),
            json_str(&self.message),
        )
    }
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
