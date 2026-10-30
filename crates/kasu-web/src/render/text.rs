//! Displaying bytes that may not be UTF-8, and HTML escaping for fragments
//! built outside templates.
use std::fmt::Write;

/// UTF-8 text with every invalid byte shown as `\xNN`.
pub fn lossy(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
        for byte in chunk.invalid() {
            let _ = write!(out, "\\x{byte:02X}");
        }
    }
    out
}
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}
/// Escaped display of arbitrary bytes.
pub fn html(bytes: &[u8]) -> String {
    escape(&lossy(bytes))
}
/// Content is shown as text when it has no NUL byte.
pub fn is_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0)
}
pub fn first_line(message: &[u8]) -> String {
    lossy(message.split(|b| *b == b'\n').next().unwrap_or_default())
}
pub fn short(id: &[u8]) -> String {
    hex::encode(id).chars().take(12).collect()
}
/// Lines without their terminators; a final terminator adds no empty line.
pub fn lines(content: &[u8]) -> Vec<&[u8]> {
    let mut lines: Vec<&[u8]> = content.split(|b| *b == b'\n').collect();
    if content.ends_with(b"\n") {
        lines.pop();
    }
    lines
        .into_iter()
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
        .collect()
}
pub fn strip_newline(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}
pub fn size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KiB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MiB", bytes as f64 / 1_048_576.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_utf8_is_marked_and_html_is_escaped() {
        assert_eq!(lossy(b"a\xffb\xc3"), "a\\xFFb\\xC3");
        assert_eq!(
            html(b"<a href=\"x\">'&"),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;"
        );
        assert_eq!(lines(b"a\r\nb\n"), [b"a".as_slice(), b"b"]);
        assert_eq!(lines(b"a\nb"), [b"a".as_slice(), b"b"]);
    }
}
