use crate::{BString, Canonical, Decoder, Encoder, Result, invalid};
use sha1::{Digest, Sha1};
use std::{collections::BTreeMap, fmt, str::FromStr};

macro_rules! id {
    ($name:ident, $size:expr) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
        pub struct $name(pub [u8; $size]);
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&hex::encode(self.0))
            }
        }
        impl FromStr for $name {
            type Err = crate::Error;
            fn from_str(s: &str) -> Result<Self> {
                let mut value = [0; $size];
                hex::decode_to_slice(s, &mut value).map_err(|_| invalid("invalid object id"))?;
                Ok(Self(value))
            }
        }
        impl Canonical for $name {
            fn encode_to(&self, out: &mut Encoder) {
                out.raw(&self.0);
            }
            fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
                Ok(Self(input.raw($size)?.try_into().unwrap()))
            }
        }
    };
}
id!(ObjectId, 32);
id!(BlobId, 32);
id!(ChangesetId, 32);
id!(TreeId, 32);
id!(GitOid, 20);
impl ObjectId {
    pub fn hash(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }
}
impl BlobId {
    pub fn hash(bytes: &[u8]) -> Self {
        Self(ObjectId::hash(bytes).0)
    }
}
impl GitOid {
    pub fn hash(kind: &str, bytes: &[u8]) -> Self {
        let mut hash = Sha1::new();
        hash.update(format!("{kind} {}\0", bytes.len()));
        hash.update(bytes);
        Self(hash.finalize().into())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RepoPath(BString);
impl RepoPath {
    pub fn new(bytes: impl Into<BString>) -> Result<Self> {
        let bytes = bytes.into();
        if bytes.is_empty()
            || bytes.contains(&0)
            || bytes
                .split(|b| *b == b'/')
                .any(|s| s.is_empty() || s == b"." || s == b"..")
        {
            return Err(invalid(
                "path must be relative, nonempty, NUL-free, without dot components",
            ));
        }
        Ok(Self(bytes))
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    pub fn components(&self) -> impl Iterator<Item = &[u8]> {
        self.as_bytes().split(|b| *b == b'/')
    }
}
impl Canonical for RepoPath {
    fn encode_to(&self, out: &mut Encoder) {
        out.bytes(self.as_bytes());
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        Self::new(input.bytes()?)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Signature {
    pub name: BString,
    pub email: BString,
    pub timestamp: i64,
    pub tz: BString,
}
impl Canonical for Signature {
    fn encode_to(&self, out: &mut Encoder) {
        out.bytes(&self.name);
        out.bytes(&self.email);
        out.i64(self.timestamp);
        out.bytes(&self.tz);
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self {
            name: input.bytes()?.into(),
            email: input.bytes()?.into(),
            timestamp: input.i64()?,
            tz: input.bytes()?.into(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Entry {
    File { blob: BlobId, executable: bool },
    Symlink { blob: BlobId },
    Gitlink { commit: GitOid },
}
impl Entry {
    pub fn blob(&self) -> Option<BlobId> {
        match self {
            Self::File { blob, .. } | Self::Symlink { blob } => Some(*blob),
            Self::Gitlink { .. } => None,
        }
    }
}
impl Canonical for Entry {
    fn encode_to(&self, out: &mut Encoder) {
        match self {
            Self::File { blob, executable } => {
                out.byte(u8::from(*executable));
                blob.encode_to(out);
            }
            Self::Symlink { blob } => {
                out.byte(2);
                blob.encode_to(out);
            }
            Self::Gitlink { commit } => {
                out.byte(3);
                commit.encode_to(out);
            }
        }
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        Ok(match input.byte()? {
            tag @ 0..=1 => Self::File {
                blob: BlobId::decode_from(input)?,
                executable: tag == 1,
            },
            2 => Self::Symlink {
                blob: BlobId::decode_from(input)?,
            },
            3 => Self::Gitlink {
                commit: GitOid::decode_from(input)?,
            },
            _ => return Err(invalid("unknown entry tag")),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileChange {
    Set { path: RepoPath, entry: Entry },
    Delete { path: RepoPath },
}
impl FileChange {
    pub fn path(&self) -> &RepoPath {
        match self {
            Self::Set { path, .. } | Self::Delete { path } => path,
        }
    }
}
impl Canonical for FileChange {
    fn encode_to(&self, out: &mut Encoder) {
        out.byte(if matches!(self, Self::Set { .. }) {
            0
        } else {
            1
        });
        self.path().encode_to(out);
        if let Self::Set { entry, .. } = self {
            entry.encode_to(out);
        }
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        let tag = input.byte()?;
        let path = RepoPath::decode_from(input)?;
        match tag {
            0 => Ok(Self::Set {
                path,
                entry: Entry::decode_from(input)?,
            }),
            1 => Ok(Self::Delete { path }),
            _ => Err(invalid("unknown change tag")),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitProvenance {
    pub raw_commit: ObjectId,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Changeset {
    pub parents: Vec<ChangesetId>,
    pub author: Signature,
    pub committer: Signature,
    pub message: BString,
    pub changes: Vec<FileChange>,
    pub git_provenance: Option<GitProvenance>,
}
impl Changeset {
    pub fn validate(&self) -> Result<()> {
        if self
            .changes
            .windows(2)
            .any(|pair| pair[0].path() >= pair[1].path())
        {
            return Err(invalid("changes must be sorted with unique paths"));
        }
        Ok(())
    }
    pub fn id(&self) -> ChangesetId {
        ChangesetId(ObjectId::hash(&self.encode()).0)
    }
}
impl Canonical for Changeset {
    fn encode_to(&self, out: &mut Encoder) {
        out.raw(b"kasu.changeset\0\x01");
        out.len(self.parents.len());
        for parent in &self.parents {
            parent.encode_to(out);
        }
        self.author.encode_to(out);
        self.committer.encode_to(out);
        out.bytes(&self.message);
        out.len(self.changes.len());
        for change in &self.changes {
            change.encode_to(out);
        }
        match &self.git_provenance {
            None => out.byte(0),
            Some(p) => {
                out.byte(1);
                p.raw_commit.encode_to(out);
            }
        }
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        if input.raw(16)? != b"kasu.changeset\0\x01" {
            return Err(invalid("unknown changeset encoding"));
        }
        let mut parents = Vec::new();
        for _ in 0..input.count()? {
            parents.push(ChangesetId::decode_from(input)?);
        }
        let author = Signature::decode_from(input)?;
        let committer = Signature::decode_from(input)?;
        let message = input.bytes()?.into();
        let mut changes = Vec::new();
        for _ in 0..input.count()? {
            changes.push(FileChange::decode_from(input)?);
        }
        let git_provenance = match input.byte()? {
            0 => None,
            1 => Some(GitProvenance {
                raw_commit: ObjectId::decode_from(input)?,
            }),
            _ => return Err(invalid("unknown provenance tag")),
        };
        let value = Self {
            parents,
            author,
            committer,
            message,
            changes,
            git_provenance,
        };
        value.validate()?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TreeEntry {
    Leaf(Entry),
    SubTree(TreeId),
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Tree {
    pub entries: BTreeMap<BString, TreeEntry>,
}
impl Tree {
    pub fn id(&self) -> TreeId {
        TreeId(ObjectId::hash(&self.encode()).0)
    }
    pub fn validate(&self) -> Result<()> {
        for name in self.entries.keys() {
            let path = RepoPath::new(name.clone())?;
            if path.components().count() != 1 {
                return Err(invalid("tree name contains slash"));
            }
        }
        Ok(())
    }
}
impl Canonical for Tree {
    fn encode_to(&self, out: &mut Encoder) {
        out.raw(b"kasu.tree\0\x01");
        out.len(self.entries.len());
        for (name, entry) in &self.entries {
            out.bytes(name);
            match entry {
                TreeEntry::Leaf(entry) => {
                    out.byte(0);
                    entry.encode_to(out);
                }
                TreeEntry::SubTree(id) => {
                    out.byte(1);
                    id.encode_to(out);
                }
            }
        }
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        if input.raw(11)? != b"kasu.tree\0\x01" {
            return Err(invalid("unknown tree encoding"));
        }
        let mut entries = BTreeMap::new();
        let mut previous: Option<BString> = None;
        for _ in 0..input.count()? {
            let name: BString = input.bytes()?.into();
            if previous.as_ref().is_some_and(|p| p >= &name) {
                return Err(invalid("tree entries must be sorted and unique"));
            }
            previous = Some(name.clone());
            let entry = match input.byte()? {
                0 => TreeEntry::Leaf(Entry::decode_from(input)?),
                1 => TreeEntry::SubTree(TreeId::decode_from(input)?),
                _ => return Err(invalid("unknown tree entry tag")),
            };
            entries.insert(name, entry);
        }
        let tree = Self { entries };
        tree.validate()?;
        Ok(tree)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlobMetadata {
    pub size: u64,
    pub git_blob_oid: GitOid,
}
impl Canonical for BlobMetadata {
    fn encode_to(&self, out: &mut Encoder) {
        out.u64(self.size);
        self.git_blob_oid.encode_to(out);
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self {
            size: input.u64()?,
            git_blob_oid: GitOid::decode_from(input)?,
        })
    }
}
