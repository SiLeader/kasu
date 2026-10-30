//! Web UI inputs: request paths and Markdown from repository content.
use kasu_web::{
    render::markdown,
    rev::{self, Route},
};

/// Parsing never panics, and re-encoding a parsed view path yields a path
/// that parses to the same route. Paths the Web UI accepts are valid
/// repository paths; others (such as an encoded `/` making an empty
/// component) are rejected before use, so only valid ones must round-trip.
pub fn url(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Some(Route::View { repo, view, rest }) = Route::parse(text)
        && !rest.is_empty()
        && rest.iter().all(|s| !s.is_empty())
        && !view.is_empty()
        && view.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        let path = rev::join(&rest[1..]).unwrap();
        if kasu_core::RepoName::new(repo.clone()).is_err()
            || (!path.is_empty() && kasu_core::RepoPath::new(path.clone()).is_err())
        {
            return;
        }
        let again = rev::url(&repo, &view, Some(&rest[0]), Some(&path));
        let Some(Route::View { rest: parsed, .. }) = Route::parse(&again) else {
            panic!("re-encoded path does not parse: {again}");
        };
        assert_eq!(parsed[0], rest[0]);
        assert_eq!(rev::join(&parsed[1..]).unwrap(), path);
    }
}
/// Sanitized output never contains executable markup.
pub fn markdown(data: &[u8]) {
    if data.len() > 16384 {
        return;
    }
    let html = markdown::render(data, Some(&|url: &str, _| Some(format!("/x/{url}"))));
    // Text is escaped, so only real tags can carry markup.
    for tag in html
        .split('<')
        .skip(1)
        .map(|t| t.split('>').next().unwrap_or_default().to_ascii_lowercase())
    {
        let name = tag.split([' ', '/', '\n', '\t']).next().unwrap_or_default();
        assert!(
            ![
                "script", "iframe", "object", "embed", "form", "style", "svg", "math", "base",
                "meta", "link"
            ]
            .contains(&name),
            "<{name}> survived sanitizing"
        );
        // Attribute values are quoted; only names outside quotes matter.
        let outside: String = tag.split('"').step_by(2).collect::<Vec<_>>().join(" ");
        let handler = outside.split_whitespace().any(|word| {
            word.strip_prefix("on").is_some_and(|rest| {
                rest.split('=')
                    .next()
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_alphabetic()))
                    && word.contains('=')
            })
        });
        assert!(!handler, "an event handler survived in <{tag}>");
        assert!(!outside.contains("style="), "style survived in <{tag}>");
        for scheme in [
            "href=\"javascript:",
            "src=\"javascript:",
            "href=\"vbscript:",
            "src=\"data:text",
        ] {
            assert!(!tag.contains(scheme), "{scheme} survived in <{tag}>");
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds_replay() {
        for (target, run) in [
            ("web_url", super::url as fn(&[u8])),
            ("markdown", super::markdown),
        ] {
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("seeds")
                .join(target);
            for entry in std::fs::read_dir(dir).unwrap() {
                run(&std::fs::read(entry.unwrap().path()).unwrap());
            }
        }
    }
}
