use crate::{Error, Result, invalid};
use std::fmt;

pub const MAX_REPO_NAME_BYTES: usize = 256;
pub const MAX_REPO_NAME_SEGMENTS: usize = 4;
const MAX_SEGMENT_BYTES: usize = 64;
// Top-level names that collide with HTTP routes served next to Git URLs.
const RESERVED_TOP_LEVEL: &[&str] = &["-", "admin", "api", "metrics", "v1"];

/// External repository name: `/`-separated segments, unique case-insensitively.
/// The registered spelling is kept for display.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RepoName(String);
impl RepoName {
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        if name.is_empty() || name.len() > MAX_REPO_NAME_BYTES {
            return Err(invalid("repository name must be 1..256 bytes"));
        }
        let segments: Vec<_> = name.split('/').collect();
        if segments.len() > MAX_REPO_NAME_SEGMENTS {
            return Err(invalid("repository name has more than 4 segments"));
        }
        for segment in &segments {
            let bytes = segment.as_bytes();
            if bytes.is_empty()
                || bytes.len() > MAX_SEGMENT_BYTES
                || !bytes[0].is_ascii_alphanumeric()
                || !bytes
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(b))
            {
                return Err(invalid("invalid repository name segment"));
            }
        }
        if name.to_ascii_lowercase().ends_with(".git") {
            return Err(invalid("repository name must not end with .git"));
        }
        if RESERVED_TOP_LEVEL
            .iter()
            .any(|reserved| segments[0].eq_ignore_ascii_case(reserved))
        {
            return Err(invalid("repository name uses a reserved top-level name"));
        }
        Ok(Self(name))
    }
    pub fn from_bytes(name: &[u8]) -> Result<Self> {
        Self::new(
            std::str::from_utf8(name)
                .map_err(|_| invalid("repository name must be ASCII"))?
                .to_owned(),
        )
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// The uniqueness key. Two names that differ only by ASCII case conflict.
    pub fn normalized(&self) -> String {
        self.0.to_ascii_lowercase()
    }
}
impl fmt::Display for RepoName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Immutable internal repository identity. Every persistent key, Derived Data
/// SubKey and read-scope namespace uses this value instead of the name.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RepoKey(Vec<u8>);
impl RepoKey {
    /// New keys carry a NUL byte, so they never equal a legacy (ASCII name) key.
    pub const PREFIX: &'static [u8] = b"r1\0";
    const RANDOM_BYTES: usize = 16;
    pub fn generate() -> Result<Self> {
        let mut random = [0u8; Self::RANDOM_BYTES];
        getrandom::fill(&mut random)
            .map_err(|_| Error::Storage("operating system randomness is unavailable".into()))?;
        let mut key = Self::PREFIX.to_vec();
        key.extend(random);
        Ok(Self(key))
    }
    /// Pre-registry data used the repository name bytes as its key.
    pub fn legacy(name: &RepoName) -> Self {
        Self(name.as_str().as_bytes().to_vec())
    }
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        if let Some(random) = bytes.strip_prefix(Self::PREFIX) {
            if random.len() != Self::RANDOM_BYTES {
                return Err(invalid("invalid repository key"));
            }
        } else {
            RepoName::from_bytes(&bytes).map_err(|_| invalid("invalid repository key"))?;
        }
        Ok(Self(bytes))
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
    pub fn is_legacy(&self) -> bool {
        !self.0.starts_with(Self::PREFIX)
    }
}
impl fmt::Display for RepoKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.strip_prefix(Self::PREFIX) {
            Some(random) => write!(f, "r1-{}", hex::encode(random)),
            None => write!(f, "legacy-{}", String::from_utf8_lossy(&self.0)),
        }
    }
}
impl fmt::Debug for RepoKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RepoKey({self})")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum RepositoryState {
    Active,
    /// Writes are refused; reads and webhooks continue.
    ReadOnly,
    /// Read-only and webhook delivery stops.
    Archived,
    /// Invisible to every caller; mutable state is removed in the background.
    Deleting,
}
impl RepositoryState {
    pub fn code(self) -> u8 {
        match self {
            Self::Active => 0,
            Self::ReadOnly => 1,
            Self::Archived => 2,
            Self::Deleting => 3,
        }
    }
    pub fn from_code(code: u8) -> Result<Self> {
        Ok(match code {
            0 => Self::Active,
            1 => Self::ReadOnly,
            2 => Self::Archived,
            3 => Self::Deleting,
            _ => return Err(invalid("unknown repository state")),
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::ReadOnly => "read_only",
            Self::Archived => "archived",
            Self::Deleting => "deleting",
        }
    }
    pub fn writable(self) -> bool {
        self == Self::Active
    }
    /// The store-side check shared by every ref transaction.
    pub fn require_writable(self) -> Result<()> {
        match self {
            Self::Active => Ok(()),
            Self::ReadOnly | Self::Archived => {
                Err(Error::Precondition("repository is read-only".into()))
            }
            Self::Deleting => Err(Error::NotFound("repository".into())),
        }
    }
}
