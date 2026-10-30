//! Server-side highlighting into class names (no inline styles, so the CSP
//! stays strict). Runs on blocking workers behind a semaphore.
use std::sync::Arc;
use syntect::{
    highlighting::ThemeSet,
    html::{ClassStyle, ClassedHTMLGenerator, css_for_theme_with_class_style},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};
use tokio::sync::Semaphore;

const STYLE: ClassStyle = ClassStyle::SpacedPrefixed { prefix: "hl-" };

pub struct Highlighter {
    syntaxes: SyntaxSet,
    permits: Semaphore,
}
impl Highlighter {
    pub fn new(concurrency: usize) -> Self {
        Self {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            permits: Semaphore::new(concurrency),
        }
    }
    /// The stylesheet for highlighted code, light and dark.
    pub fn css() -> String {
        let themes = ThemeSet::load_defaults();
        let light = css_for_theme_with_class_style(&themes.themes["InspiredGitHub"], STYLE)
            .unwrap_or_default();
        let dark = css_for_theme_with_class_style(&themes.themes["base16-ocean.dark"], STYLE)
            .unwrap_or_default();
        format!("{light}\n@media (prefers-color-scheme: dark) {{\n{dark}\n}}\n")
    }
    /// Highlighted lines as self-contained HTML, or None when no syntax
    /// matches. The caller enforces size limits.
    pub async fn lines(self: &Arc<Self>, path: &[u8], text: String) -> Option<Vec<String>> {
        let _permit = self.permits.acquire().await.ok()?;
        let this = self.clone();
        let path = path.to_vec();
        tokio::task::spawn_blocking(move || this.highlight(&path, &text))
            .await
            .ok()
            .flatten()
    }
    fn highlight(&self, path: &[u8], text: &str) -> Option<Vec<String>> {
        let name = String::from_utf8_lossy(path);
        let file = name.rsplit('/').next().unwrap_or_default();
        let extension = file.rsplit_once('.').map(|(_, e)| e).unwrap_or(file);
        let syntax = self
            .syntaxes
            .find_syntax_by_extension(extension)
            .or_else(|| self.syntaxes.find_syntax_by_first_line(text))?;
        if syntax.name == "Plain Text" {
            return None;
        }
        let mut generator =
            ClassedHTMLGenerator::new_with_class_style(syntax, &self.syntaxes, STYLE);
        for line in LinesWithEndings::from(text) {
            generator
                .parse_html_for_line_which_includes_newline(line)
                .ok()?;
        }
        Some(split_lines(&generator.finalize()))
    }
}
/// Split highlighted HTML at newlines, closing and reopening the spans that
/// cross a line so each line stands alone.
pub fn split_lines(html: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut open: Vec<&str> = Vec::new();
    let mut current = String::new();
    let mut rest = html;
    while !rest.is_empty() {
        if rest.starts_with("<span") {
            let end = rest.find('>').map(|i| i + 1).unwrap_or(rest.len());
            open.push(&rest[..end]);
            current.push_str(&rest[..end]);
            rest = &rest[end..];
        } else if let Some(after) = rest.strip_prefix("</span>") {
            open.pop();
            current.push_str("</span>");
            rest = after;
        } else if let Some(after) = rest.strip_prefix('\n') {
            for _ in &open {
                current.push_str("</span>");
            }
            lines.push(std::mem::take(&mut current));
            for tag in &open {
                current.push_str(tag);
            }
            rest = after;
        } else {
            let end = rest.find(['<', '\n']).unwrap_or(rest.len()).max(1);
            current.push_str(&rest[..end]);
            rest = &rest[end..];
        }
    }
    let visible = current.split('<').enumerate().any(|(i, part)| {
        if i == 0 {
            !part.is_empty()
        } else {
            part.split_once('>').is_some_and(|(_, t)| !t.is_empty())
        }
    });
    if visible {
        for _ in &open {
            current.push_str("</span>");
        }
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn spans_are_balanced_on_every_line() {
        let lines = split_lines("<span class=\"a\">x\ny</span>\nz\n");
        assert_eq!(
            lines,
            [
                "<span class=\"a\">x</span>",
                "<span class=\"a\">y</span>",
                "z"
            ]
        );
    }
    #[tokio::test]
    async fn rust_is_highlighted_with_classes_and_escaped() {
        let h = Arc::new(Highlighter::new(1));
        let lines = h
            .lines(b"src/main.rs", "fn main() { let s = \"<b>\"; }\n".into())
            .await
            .unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("hl-"));
        assert!(lines[0].contains("&lt;b&gt;"));
        assert!(!lines[0].contains("style="));
    }
}

#[cfg(test)]
mod throughput {
    use super::*;
    /// Highlighting at the configured limits (20,000 lines / 512 KiB).
    /// Run with `--ignored --nocapture` to print the rate.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "measurement"]
    async fn highlighting_a_file_at_the_limit() {
        let mut text = String::new();
        while text.len() < 500 * 1024 && text.lines().count() < 19_990 {
            text.push_str("fn f(x: u64) -> u64 { let y = \"s\"; x * 2 }\n");
        }
        let lines = text.lines().count();
        let highlighter = Arc::new(Highlighter::new(4));
        let started = std::time::Instant::now();
        let out = highlighter.lines(b"a.rs", text.clone()).await.unwrap();
        let elapsed = started.elapsed();
        assert_eq!(out.len(), lines);
        println!(
            "highlighted {lines} lines / {} KiB in {:.0} ms ({:.0} lines/s)",
            text.len() / 1024,
            elapsed.as_secs_f64() * 1000.0,
            lines as f64 / elapsed.as_secs_f64()
        );
    }
}
