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
    if path
        .split(|b| *b == b'/')
        .next()
        .unwrap()
        .eq_ignore_ascii_case(b".kasu")
    {
        return Err(invalid("repository path collides with .kasu"));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub mod virtual_fs;
