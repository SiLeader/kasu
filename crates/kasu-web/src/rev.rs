//! URL layout: `/<repo>/-/<view>/<rev>/<path>`. The first `-` segment
//! separates the repository name (which never has a `-` segment) from the
//! view. `<rev>` is one segment; `<path>` is the rest. Every byte outside the
//! unreserved set is percent-encoded, so non-UTF-8 paths round-trip.
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode, percent_encode};

const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Encode one path segment (a ref name, a file name, ...).
pub fn segment(bytes: &[u8]) -> String {
    percent_encode(bytes, SEGMENT).to_string()
}
/// Encode a `/`-separated repository path segment by segment.
pub fn path(bytes: &[u8]) -> String {
    bytes
        .split(|b| *b == b'/')
        .map(segment)
        .collect::<Vec<_>>()
        .join("/")
}
/// The URL of `view` in `repo`, optionally at `rev` and `path`.
pub fn url(repo: &str, view: &str, rev: Option<&[u8]>, file: Option<&[u8]>) -> String {
    let mut out = repo_url(repo);
    out.push_str("/-/");
    out.push_str(view);
    if let Some(rev) = rev {
        out.push('/');
        out.push_str(&segment(rev));
        if let Some(file) = file.filter(|f| !f.is_empty()) {
            out.push('/');
            out.push_str(&path(file));
        }
    }
    out
}
pub fn repo_url(repo: &str) -> String {
    format!("/{}", path(repo.as_bytes()))
}

/// A parsed request path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Route {
    /// `/<repo>`: the repository top.
    Repo { repo: String },
    /// `/<repo>/-/<view>/<rest...>`, each rest segment decoded.
    View {
        repo: String,
        view: String,
        rest: Vec<Vec<u8>>,
    },
}
impl Route {
    /// Parse a raw (still percent-encoded) path that is not a top-level
    /// `/-/` page. None for paths that cannot name a repository.
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.strip_prefix('/')?;
        let mut segments: Vec<&str> = raw.split('/').collect();
        if segments.last() == Some(&"") {
            segments.pop();
        }
        let split = segments.iter().position(|s| *s == "-");
        let (name, rest) = match split {
            Some(0) | None if segments.is_empty() => return None,
            Some(0) => return None,
            Some(at) => (&segments[..at], Some(&segments[at + 1..])),
            None => (&segments[..], None),
        };
        let repo = name
            .iter()
            .map(|s| {
                String::from_utf8(percent_decode(s.as_bytes()).collect())
                    .ok()
                    .filter(|s| !s.is_empty())
            })
            .collect::<Option<Vec<_>>>()?
            .join("/");
        match rest {
            None => Some(Self::Repo { repo }),
            Some(rest) => {
                let (view, rest) = rest.split_first()?;
                Some(Self::View {
                    repo,
                    view: (*view).to_owned(),
                    rest: rest
                        .iter()
                        .map(|s| percent_decode(s.as_bytes()).collect())
                        .collect(),
                })
            }
        }
    }
}
/// Join decoded path segments; empty segments are invalid.
pub fn join(segments: &[Vec<u8>]) -> Option<Vec<u8>> {
    if segments.iter().any(|s| s.is_empty()) {
        return None;
    }
    Some(segments.join(&b'/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    #[test]
    fn repository_and_view_split_at_the_first_dash_segment() {
        assert_eq!(
            Route::parse("/team/app/-/blob/feature%2Fx/a%20b/%FF.txt"),
            Some(Route::View {
                repo: "team/app".into(),
                view: "blob".into(),
                rest: vec![b"feature/x".to_vec(), b"a b".to_vec(), b"\xff.txt".to_vec()],
            })
        );
        assert_eq!(
            Route::parse("/team/app"),
            Some(Route::Repo {
                repo: "team/app".into()
            })
        );
        assert_eq!(Route::parse("/team/app/"), Route::parse("/team/app"));
        assert_eq!(Route::parse("/-/login"), None);
        assert_eq!(Route::parse("/"), None);
        assert_eq!(Route::parse("/a//b"), None);
        assert_eq!(
            url(
                "team/app",
                "blob",
                Some(b"refs/heads/a%b"),
                Some(b"dir/f\xff")
            ),
            "/team/app/-/blob/refs%2Fheads%2Fa%25b/dir/f%FF"
        );
    }
    proptest! {
        #[test]
        fn rev_and_path_round_trip(
            rev in proptest::collection::vec(any::<u8>(), 1..40),
            file in proptest::collection::vec(proptest::collection::vec(any::<u8>().prop_filter("no slash", |b| *b != b'/'), 1..12), 0..5),
        ) {
            let joined = file.join(&b'/');
            let link = url("team/app", "tree", Some(&rev), Some(&joined));
            let Some(Route::View { repo, view, rest }) = Route::parse(&link) else {
                panic!("unparsed {link}");
            };
            prop_assert_eq!(repo, "team/app");
            prop_assert_eq!(view, "tree");
            prop_assert_eq!(&rest[0], &rev);
            prop_assert_eq!(join(&rest[1..]).unwrap(), joined);
        }
    }
}
