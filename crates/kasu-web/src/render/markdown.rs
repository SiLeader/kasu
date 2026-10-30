//! Markdown for READMEs, review descriptions and comments: CommonMark with
//! tables, then an allow-list sanitizer. Relative links are rewritten by the
//! caller (to blob/tree URLs, and images to raw URLs).
use pulldown_cmark::{CowStr, Event, Options, Parser, Tag};
use std::collections::HashSet;

/// Rewrite a relative destination; `image` tells links from images.
pub type Rewrite<'a> = &'a dyn Fn(&str, bool) -> Option<String>;

fn relative(url: &str) -> bool {
    !url.is_empty()
        && !url.starts_with('#')
        && !url.starts_with('/')
        && !url.starts_with("//")
        && !url.contains(':')
}
pub fn render(source: &[u8], rewrite: Option<Rewrite<'_>>) -> String {
    let text = crate::render::text::lossy(source);
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let events = Parser::new_ext(&text, options).map(|event| match event {
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            dest_url: rewritten(dest_url, false, rewrite),
            link_type,
            title,
            id,
        }),
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Image {
            dest_url: rewritten(dest_url, true, rewrite),
            link_type,
            title,
            id,
        }),
        // Raw HTML is escaped rather than passed to the sanitizer.
        Event::Html(html) | Event::InlineHtml(html) => Event::Text(html),
        other => other,
    });
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, events);
    sanitize(&html)
}
fn rewritten<'a>(url: CowStr<'a>, image: bool, rewrite: Option<Rewrite<'_>>) -> CowStr<'a> {
    match rewrite {
        Some(rewrite) if relative(&url) => match rewrite(&url, image) {
            Some(new) => CowStr::from(new),
            None => url,
        },
        _ => url,
    }
}
pub fn sanitize(html: &str) -> String {
    ammonia::Builder::default()
        .url_schemes(HashSet::from(["http", "https", "mailto"]))
        .link_rel(Some("noopener noreferrer nofollow"))
        .clean(html)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scripts_handlers_and_dangerous_urls_are_removed() {
        for input in [
            "<script>alert(1)</script>",
            "[x](javascript:alert(1))",
            "<img src=x onerror=alert(1)>",
            "<a href=\"javascript:alert(1)\">x</a>",
            "![x](data:text/html;base64,PHNjcmlwdD4=)",
            "<iframe src=//evil></iframe><form action=//evil><input></form>",
            "<p style=\"background:url(x)\">s</p>",
        ] {
            let html = render(input.as_bytes(), None);
            for bad in [
                "<script",
                "<a href=\"javascript",
                "<img src=x onerror",
                "<img src=\"x\" onerror",
                "<iframe",
                "<form",
                "<p style",
                "src=\"data:",
            ] {
                assert!(!html.contains(bad), "{input} -> {html}");
            }
        }
    }
    #[test]
    fn relative_links_and_images_are_rewritten() {
        let rewrite =
            |url: &str, image: bool| Some(format!("/{}/{url}", if image { "raw" } else { "blob" }));
        let html = render(
            b"[doc](docs/a.md) ![i](img.png) [ext](https://x.example) [top](#t)",
            Some(&rewrite),
        );
        assert!(html.contains("href=\"/blob/docs/a.md\""), "{html}");
        assert!(html.contains("src=\"/raw/img.png\""), "{html}");
        assert!(html.contains("href=\"https://x.example\""), "{html}");
        assert!(html.contains("href=\"#t\""), "{html}");
    }
}
