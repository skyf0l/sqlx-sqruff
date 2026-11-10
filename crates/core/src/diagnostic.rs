//! A finding mapped back to a location *inside the Rust file*, plus renderers.

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub file: String,
    /// 1-based line in the `.rs` file.
    pub line: usize,
    /// 1-based column in the `.rs` file, counted in chars.
    pub col: usize,
    pub code: String,
    pub message: String,
}

impl Diagnostic {
    /// rustc-style: `path:line:col: CODE message`.
    pub fn render_human(&self) -> String {
        format!("{}:{}:{}: {} {}", self.file, self.line, self.col, self.code, self.message)
    }

    /// One JSON object on a single line (JSON Lines).
    pub fn render_json(&self) -> String {
        serde_json::json!({
            "file": self.file,
            "line": self.line,
            "col": self.col,
            "code": self.code,
            "message": self.message,
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_line_round_trips_quotes_and_control_characters() {
        let d = Diagnostic {
            file: "dir \"x\"\\q.rs".into(),
            line: 3,
            col: 7,
            code: "CV05".into(),
            message: "form feed \u{c}, bell \u{7}, tab \t".into(),
        };
        let json = d.render_json();
        assert!(!json.contains('\n'), "not a single line: {json}");
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(v["file"], d.file.as_str());
        assert_eq!(v["line"], 3);
        assert_eq!(v["col"], 7);
        assert_eq!(v["code"], "CV05");
        assert_eq!(v["message"], d.message.as_str());
    }
}
