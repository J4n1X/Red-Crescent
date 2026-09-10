//! .lhtml template parsing and compilation.
//!
//! A template is split into HTML and Lua segments by a scanner that understands
//! Lua string literals, long brackets and comments, so a `?>` inside them does
//! not terminate a `<?lua ... ?>` block. The segments are then compiled into a
//! single Lua chunk in which HTML becomes `_out("...")` calls and code blocks
//! are inlined verbatim. Newline padding keeps every code line in the generated
//! chunk on the same line number as in the source file, so Lua error messages
//! point at real `.lhtml` lines.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Literal HTML output. `line` is the source line the segment starts on.
    Html { text: String, line: u32 },
    /// A `<?lua ... ?>` code block, inlined verbatim into the chunk.
    Code { code: String, line: u32 },
    /// A `<?lua= expr ?>` expression block, emitted through `_out_expr`.
    Expr { code: String, line: u32 },
}

impl Segment {
    fn line(&self) -> u32 {
        match self {
            Segment::Html { line, .. }
            | Segment::Code { line, .. }
            | Segment::Expr { line, .. } => *line,
        }
    }
}

#[derive(Debug)]
pub enum TemplateError {
    NotFound,
    Io(String),
    Parse { line: u32, msg: String },
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TemplateError::NotFound => write!(f, "template not found"),
            TemplateError::Io(e) => write!(f, "failed to read template: {e}"),
            TemplateError::Parse { line, msg } => write!(f, "parse error on line {line}: {msg}"),
        }
    }
}

impl std::error::Error for TemplateError {}

/// Split a template into HTML and Lua segments.
pub fn parse(src: &str) -> Result<Vec<Segment>, TemplateError> {
    let bytes = src.as_bytes();
    let mut segments = Vec::new();
    let mut i = 0usize;
    let mut line: u32 = 1;
    let mut html_start = 0usize;
    let mut html_line: u32 = 1;

    while i < bytes.len() {
        if bytes[i] == b'\n' {
            line += 1;
            i += 1;
            continue;
        }
        if bytes[i] == b'<' && src[i..].starts_with("<?lua") {
            let after = i + "<?lua".len();
            // Only "<?lua=", "<?lua" + whitespace, or "<?lua?>" open a block;
            // anything else (e.g. "<?luax") is plain HTML.
            let (is_expr, code_start) = match bytes.get(after) {
                Some(b'=') => (true, after + 1),
                Some(c) if c.is_ascii_whitespace() => (false, after),
                Some(b'?') => (false, after),
                _ => {
                    i += 1;
                    continue;
                }
            };

            if html_start < i {
                segments.push(Segment::Html {
                    text: src[html_start..i].to_string(),
                    line: html_line,
                });
            }

            let open_line = line;
            let (code_len, newlines) =
                scan_lua_block(&src[code_start..]).ok_or_else(|| TemplateError::Parse {
                    line: open_line,
                    msg: "unterminated <?lua block (missing '?>'; note that a '?>' inside a \
                          comment does not close the block)"
                        .to_string(),
                })?;

            let code = src[code_start..code_start + code_len].to_string();
            segments.push(if is_expr {
                Segment::Expr {
                    code,
                    line: open_line,
                }
            } else {
                Segment::Code {
                    code,
                    line: open_line,
                }
            });

            line += newlines;
            i = code_start + code_len + "?>".len();
            html_start = i;
            html_line = line;
            continue;
        }
        i += 1;
    }

    if html_start < src.len() {
        segments.push(Segment::Html {
            text: src[html_start..].to_string(),
            line: html_line,
        });
    }

    Ok(segments)
}

/// Scan Lua code for the closing `?>`, skipping string literals, long brackets
/// and comments. Returns (code length, newline count) or None if unterminated.
fn scan_lua_block(s: &str) -> Option<(usize, u32)> {
    let b = s.as_bytes();
    let mut i = 0usize;
    let mut nl: u32 = 0;

    while i < b.len() {
        match b[i] {
            b'\n' => {
                nl += 1;
                i += 1;
            }
            b'?' if b.get(i + 1) == Some(&b'>') => return Some((i, nl)),
            b'\'' | b'"' => i = scan_short_string(b, i),
            b'[' => {
                if let Some(level) = long_bracket_level(b, i) {
                    i = scan_long_bracket(b, i, level, &mut nl)?;
                } else {
                    i += 1;
                }
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                let j = i + 2;
                if let Some(level) = long_bracket_level(b, j) {
                    i = scan_long_bracket(b, j, level, &mut nl)?;
                } else {
                    // Line comment: runs to end of line, `?>` inside it is text.
                    i = j;
                    while i < b.len() && b[i] != b'\n' {
                        i += 1;
                    }
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// Scan a short string starting at the opening quote; returns the index just
/// past the closing quote. An unescaped newline ends the scan early (Lua would
/// reject the string anyway — the compiler will report the real error).
fn scan_short_string(b: &[u8], start: usize) -> usize {
    let quote = b[start];
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'\n' => return i,
            c if c == quote => return i + 1,
            _ => i += 1,
        }
    }
    i
}

/// If position `i` opens a long bracket (`[`, `[=`, `[==`...` followed by `[`),
/// return its level.
fn long_bracket_level(b: &[u8], i: usize) -> Option<usize> {
    if b.get(i) != Some(&b'[') {
        return None;
    }
    let mut j = i + 1;
    while b.get(j) == Some(&b'=') {
        j += 1;
    }
    (b.get(j) == Some(&b'[')).then_some(j - i - 1)
}

/// Scan past a long bracket (string or comment body) of the given level,
/// starting at its opening `[`. Returns the index just past the closing
/// bracket, or None if unterminated.
fn scan_long_bracket(b: &[u8], start: usize, level: usize, nl: &mut u32) -> Option<usize> {
    let mut i = start + level + 2;
    while i < b.len() {
        match b[i] {
            b'\n' => {
                *nl += 1;
                i += 1;
            }
            b']' => {
                let mut j = i + 1;
                let mut eq = 0;
                while b.get(j) == Some(&b'=') {
                    j += 1;
                    eq += 1;
                }
                if eq == level && b.get(j) == Some(&b']') {
                    return Some(j + 1);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// Compile parsed segments into a single Lua chunk with source-aligned lines.
pub fn generate(segments: &[Segment]) -> String {
    let mut out = String::new();
    let mut cur_line: u32 = 1;

    for seg in segments {
        while cur_line < seg.line() {
            out.push('\n');
            cur_line += 1;
        }
        match seg {
            Segment::Html { text, .. } => {
                out.push_str("_out(\"");
                escape_lua_string_into(&mut out, text);
                out.push_str("\"); ");
            }
            Segment::Expr { code, .. } => {
                out.push_str("_out_expr(");
                out.push_str(code);
                out.push_str("); ");
                cur_line += count_newlines(code);
            }
            Segment::Code { code, .. } => {
                out.push_str(code);
                out.push(' ');
                cur_line += count_newlines(code);
            }
        }
    }
    out
}

fn count_newlines(s: &str) -> u32 {
    s.bytes().filter(|&b| b == b'\n').count() as u32
}

/// Escape text into a single-line Lua short-string literal body.
fn escape_lua_string_into(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\x{:02X}", c as u32));
            }
            c => out.push(c),
        }
    }
}

/// A template compiled to a Lua chunk, plus the file metadata it was built from.
pub struct CompiledTemplate {
    /// Chunk name in mlua's `@file` convention, so errors read `file:line: msg`.
    pub chunk_name: String,
    pub lua_source: String,
    mtime: Option<SystemTime>,
    len: u64,
}

/// Cache of compiled templates keyed by canonical path, invalidated by
/// mtime + length. With `enabled == false` (dev mode) every load re-reads
/// and re-compiles the file.
pub struct TemplateCache {
    enabled: bool,
    map: RwLock<HashMap<PathBuf, Arc<CompiledTemplate>>>,
}

impl TemplateCache {
    pub fn new(enabled: bool) -> Self {
        TemplateCache {
            enabled,
            map: RwLock::new(HashMap::new()),
        }
    }

    /// Load (or fetch from cache) the compiled form of the template at `abs`.
    /// `display_name` is the serve-dir-relative path used in error messages.
    pub fn load(
        &self,
        abs: &Path,
        display_name: &str,
    ) -> Result<Arc<CompiledTemplate>, TemplateError> {
        let meta = std::fs::metadata(abs).map_err(io_to_template_error)?;
        let mtime = meta.modified().ok();
        let len = meta.len();

        if self.enabled
            && let Some(cached) = self
                .map
                .read()
                .expect("template cache lock poisoned")
                .get(abs)
            && cached.mtime == mtime
            && cached.len == len
        {
            return Ok(Arc::clone(cached));
        }

        let source = std::fs::read_to_string(abs).map_err(io_to_template_error)?;
        let segments = parse(&source)?;
        let compiled = Arc::new(CompiledTemplate {
            chunk_name: format!("@{display_name}"),
            lua_source: generate(&segments),
            mtime,
            len,
        });

        if self.enabled {
            self.map
                .write()
                .expect("template cache lock poisoned")
                .insert(abs.to_path_buf(), Arc::clone(&compiled));
        }
        Ok(compiled)
    }
}

fn io_to_template_error(e: std::io::Error) -> TemplateError {
    if e.kind() == std::io::ErrorKind::NotFound {
        TemplateError::NotFound
    } else {
        TemplateError::Io(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html(text: &str, line: u32) -> Segment {
        Segment::Html {
            text: text.to_string(),
            line,
        }
    }
    fn code(code_: &str, line: u32) -> Segment {
        Segment::Code {
            code: code_.to_string(),
            line,
        }
    }
    fn expr(code_: &str, line: u32) -> Segment {
        Segment::Expr {
            code: code_.to_string(),
            line,
        }
    }

    #[test]
    fn parses_basic_blocks() {
        let segs = parse("a<?lua x() ?>b").unwrap();
        assert_eq!(segs, vec![html("a", 1), code(" x() ", 1), html("b", 1)]);
    }

    #[test]
    fn parses_expr_blocks() {
        let segs = parse("<?lua= 1 + 1 ?>").unwrap();
        assert_eq!(segs, vec![expr(" 1 + 1 ", 1)]);
    }

    #[test]
    fn close_marker_inside_short_string_is_ignored() {
        let segs = parse(r#"<?lua local s = "?>" ?>ok"#).unwrap();
        assert_eq!(segs, vec![code(r#" local s = "?>" "#, 1), html("ok", 1)]);
    }

    #[test]
    fn close_marker_inside_long_bracket_is_ignored() {
        let segs = parse("<?lua local s = [==[?>]==] ?>ok").unwrap();
        assert_eq!(segs, vec![code(" local s = [==[?>]==] ", 1), html("ok", 1)]);
    }

    #[test]
    fn close_marker_inside_line_comment_is_ignored() {
        let segs = parse("<?lua -- not here ?>\nprint(1) ?>x").unwrap();
        assert_eq!(
            segs,
            vec![code(" -- not here ?>\nprint(1) ", 1), html("x", 2)]
        );
    }

    #[test]
    fn close_marker_inside_block_comment_is_ignored() {
        let segs = parse("<?lua --[[ ?> ]] print(1) ?>x").unwrap();
        assert_eq!(segs, vec![code(" --[[ ?> ]] print(1) ", 1), html("x", 1)]);
    }

    #[test]
    fn unterminated_block_is_an_error() {
        let err = parse("line one\n<?lua print(").unwrap_err();
        match err {
            TemplateError::Parse { line, .. } => assert_eq!(line, 2),
            other => panic!("expected parse error, got {other:?}"),
        }
    }

    #[test]
    fn lookalike_tag_stays_html() {
        let segs = parse("<?luax ?>").unwrap();
        assert_eq!(segs, vec![html("<?luax ?>", 1)]);
    }

    #[test]
    fn empty_block_parses() {
        let segs = parse("a<?lua?>b").unwrap();
        assert_eq!(segs, vec![html("a", 1), code("", 1), html("b", 1)]);
    }

    #[test]
    fn tracks_lines_across_segments() {
        let src = "line1\nline2\n<?lua\n  local x = 1\n?>\n<?lua= x ?>";
        let segs = parse(src).unwrap();
        assert_eq!(
            segs,
            vec![
                html("line1\nline2\n", 1),
                code("\n  local x = 1\n", 3),
                html("\n", 5),
                expr(" x ", 6),
            ]
        );
    }

    #[test]
    fn generated_chunk_lines_match_source_lines() {
        let src = "<p>one</p>\n<p>two</p>\n<?lua\n  local x = 1\n  error(\"boom\")\n?>";
        let generated = generate(&parse(src).unwrap());
        let lines: Vec<&str> = generated.lines().collect();
        assert!(
            lines[4].contains("error(\"boom\")"),
            "generated:\n{generated}"
        );
    }

    #[test]
    fn html_containing_quotes_and_newlines_round_trips() {
        let src = "say \"hi\"\\\nnext";
        let generated = generate(&parse(src).unwrap());
        assert_eq!(generated, "_out(\"say \\\"hi\\\"\\\\\\nnext\"); ");
    }

    #[test]
    fn same_line_mix_generates_valid_statement_sequence() {
        let src = "<?lua if x then ?><b>y</b><?lua end ?>";
        let generated = generate(&parse(src).unwrap());
        assert_eq!(generated, " if x then  _out(\"<b>y</b>\");  end  ");
    }
}
