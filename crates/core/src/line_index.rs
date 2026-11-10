//! Conversions between byte offsets and line/column positions in a source file.

use proc_macro2::LineColumn;

pub(crate) struct LineIndex {
    /// Byte offset of the start of each line (line N at index N-1).
    line_starts: Vec<usize>,
}

impl LineIndex {
    pub(crate) fn new(src: &str) -> Self {
        // syn drops a leading byte order mark before lexing, so its line 1
        // columns, like an editor's, count from the char after it.
        let first = if src.starts_with('\u{feff}') { '\u{feff}'.len_utf8() } else { 0 };
        let mut line_starts = vec![first];
        for (i, b) in src.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        Self { line_starts }
    }

    /// `LineColumn`: 1-based line, 0-based column counted in `char`s.
    pub(crate) fn offset(&self, src: &str, lc: LineColumn) -> Option<usize> {
        let line_start = *self.line_starts.get(lc.line.checked_sub(1)?)?;
        let rest = src.get(line_start..)?;
        let mut byte = line_start;
        for (k, ch) in rest.chars().enumerate() {
            if k == lc.column {
                return Some(byte);
            }
            byte += ch.len_utf8();
        }
        Some(byte)
    }

    /// 1-based line and 1-based column (counted in `char`s) of byte offset `byte`.
    pub(crate) fn line_col(&self, src: &str, byte: usize) -> (usize, usize) {
        let line = self.line_starts.partition_point(|&start| start <= byte);
        let line_start = self.line_starts[line - 1];
        let col = src.get(line_start..byte).map_or(0, |s| s.chars().count()) + 1;
        (line, col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_inverts_offset() {
        let src = "ab\né日x\n\nz";
        let index = LineIndex::new(src);
        for (line, column) in [(1, 0), (1, 2), (2, 0), (2, 2), (3, 0), (4, 0), (4, 1)] {
            let byte = index.offset(src, LineColumn { line, column }).unwrap();
            assert_eq!(index.line_col(src, byte), (line, column + 1));
        }
    }
}
