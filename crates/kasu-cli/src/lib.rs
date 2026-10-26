mod commands;
mod filesystem;
mod remote;
mod sparse;
mod working;
pub use commands::*;
pub use remote::*;
pub use sparse::*;
pub use working::*;
pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;
pub(crate) fn invalid(message: &str) -> Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message).into()
}
pub fn validate_work_path(path: &[u8]) -> Result<()> {
    kasu_core::RepoPath::new(path)?;
    // A nested control directory can be discovered when a command is run from
    // that subtree, so reserving only the working-copy root is insufficient.
    for component in path.split(|b| *b == b'/') {
        if control_component(component, ".kasu") {
            return Err(invalid("repository path collides with .kasu"));
        }
        if control_component(component, ".git") {
            return Err(invalid("repository path collides with .git"));
        }
    }
    Ok(())
}

fn control_component(component: &[u8], name: &str) -> bool {
    let Ok(component) = std::str::from_utf8(component) else {
        return false;
    };
    // HFS+ ignores these characters when comparing names. Reserve aliases on
    // every platform so a portable checkout cannot overwrite control files.
    component
        .chars()
        .filter(
            |c| !matches!(*c as u32, 0x200c..=0x200f | 0x202a..=0x202e | 0x206a..=0x206f | 0xfeff),
        )
        .map(|c| match c {
            // Unicode case folding also aliases these characters to ASCII on
            // case-insensitive Apple filesystems (including APFS).
            '\u{212a}' => 'k',
            '\u{017f}' => 's',
            c => c.to_ascii_lowercase(),
        })
        .eq(name.chars())
}

#[cfg(test)]
mod tests {
    use super::validate_work_path;

    #[test]
    fn control_names_are_reserved_as_complete_components_only() {
        for path in [b".kasu".as_slice(), b".git", b"a/.KASU", b"a/.GIT/config"] {
            assert!(validate_work_path(path).is_err(), "{path:?}");
        }
        for path in [
            b".gitignore".as_slice(),
            b"a/.gitmodules",
            b"a/.kasu-gitlink",
            b"a/git/config",
            b"a/\xff",
        ] {
            validate_work_path(path).unwrap();
        }
    }

    #[test]
    fn hfs_control_directory_aliases_are_reserved() {
        for codepoint in (0x200c..=0x200f)
            .chain(0x202a..=0x202e)
            .chain(0x206a..=0x206f)
            .chain([0xfeff])
        {
            let ignored = char::from_u32(codepoint).unwrap();
            for name in [".git", ".kasu"] {
                for offset in 0..=name.len() {
                    let alias = format!("{}{ignored}{}", &name[..offset], &name[offset..]);
                    assert!(
                        validate_work_path(format!("nested/{alias}/config").as_bytes()).is_err()
                    );
                    validate_work_path(format!("{alias}-ordinary").as_bytes()).unwrap();
                }
            }
        }
        validate_work_path("ordinary/日本語".as_bytes()).unwrap();
    }

    #[test]
    fn unicode_casefold_control_directory_aliases_are_reserved() {
        for path in [".Kasu", "nested/.kaſu/state.json", "nested/.Kaſu"] {
            assert!(validate_work_path(path.as_bytes()).is_err());
        }
        validate_work_path("ordinary/Kaſu".as_bytes()).unwrap();
    }
}

#[cfg(target_os = "linux")]
pub mod virtual_fs;
