use kasu_core::*;
use kasu_store::Repository;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionId {
    pub repo: BString,
    pub path: Option<RepoPath>,
}
impl ProjectionId {
    pub fn full(repo: impl Into<BString>) -> Self {
        Self {
            repo: repo.into(),
            path: None,
        }
    }
    pub fn path(repo: impl Into<BString>, path: RepoPath) -> Self {
        Self {
            repo: repo.into(),
            path: Some(path),
        }
    }
    pub fn namespace(&self) -> String {
        hex::encode(self.encode())
    }
}
/// Key prefixes of the Git OID, publication and reachability indexes of every
/// projection of `repo`; the length-prefixed key keeps other repositories out.
pub fn repository_index_prefixes(repo: &[u8]) -> Vec<Vec<u8>> {
    let mut prefixes = Vec::new();
    for (kind, separator) in [(0u8, "/"), (1, "")] {
        let mut encoded = Encoder::default();
        encoded.byte(kind);
        encoded.bytes(repo);
        let namespace = hex::encode(encoded.0);
        for family in ["git/oid/v1", "git/published/v1", "git/reachability/v1"] {
            prefixes.push(format!("{family}/{namespace}{separator}").into_bytes());
        }
    }
    prefixes
}
impl Canonical for ProjectionId {
    fn encode_to(&self, out: &mut Encoder) {
        out.byte(u8::from(self.path.is_some()));
        out.bytes(&self.repo);
        if let Some(path) = &self.path {
            path.encode_to(out);
        }
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        let kind = input.byte()?;
        let repo = input.bytes()?;
        match kind {
            0 => Ok(Self::full(repo)),
            1 => Ok(Self::path(repo, RepoPath::decode_from(input)?)),
            _ => Err(invalid("unsupported Git projection")),
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeTarget {
    Blob(BlobId),
    Tree(TreeId),
    Commit(ChangesetId),
    Tag,
}
impl NativeTarget {
    pub fn kind(&self) -> GitKind {
        match self {
            Self::Blob(_) => GitKind::Blob,
            Self::Tree(_) => GitKind::Tree,
            Self::Commit(_) => GitKind::Commit,
            Self::Tag => GitKind::Tag,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OidRecord {
    pub raw: ObjectId,
    pub native: NativeTarget,
}
impl Canonical for OidRecord {
    fn encode_to(&self, out: &mut Encoder) {
        self.raw.encode_to(out);
        self.native.kind().encode_to(out);
        match self.native {
            NativeTarget::Blob(id) => id.encode_to(out),
            NativeTarget::Tree(id) => id.encode_to(out),
            NativeTarget::Commit(id) => id.encode_to(out),
            NativeTarget::Tag => {}
        }
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        let raw = ObjectId::decode_from(input)?;
        let native = match GitKind::decode_from(input)? {
            GitKind::Blob => NativeTarget::Blob(BlobId::decode_from(input)?),
            GitKind::Tree => NativeTarget::Tree(TreeId::decode_from(input)?),
            GitKind::Commit => NativeTarget::Commit(ChangesetId::decode_from(input)?),
            GitKind::Tag => NativeTarget::Tag,
        };
        Ok(Self { raw, native })
    }
}
pub struct GitIndex<'a> {
    pub repo: Repository<'a>,
    pub projection: ProjectionId,
}
impl GitIndex<'_> {
    fn key(&self, oid: GitOid) -> Vec<u8> {
        format!("git/oid/v1/{}/{oid}", self.projection.namespace()).into_bytes()
    }
    pub fn find(&self, oid: GitOid) -> Result<Option<OidRecord>> {
        self.repo
            .meta
            .get(&self.key(oid))?
            .map(|raw| OidRecord::decode(&raw))
            .transpose()
    }
    pub fn require(&self, oid: GitOid) -> Result<OidRecord> {
        self.find(oid)?.ok_or_else(|| {
            Error::NotFound(format!(
                "Git {oid} in projection {}",
                self.projection.namespace()
            ))
        })
    }
    pub fn object(&self, oid: GitOid) -> Result<(GitKind, Vec<u8>)> {
        let record = self.require(oid)?;
        let bytes = self.repo.blobs.get(record.raw)?;
        let kind = record.native.kind();
        if GitOid::hash(kind.as_str(), &bytes) != oid {
            return Err(invalid("Git object hash mismatch"));
        }
        Ok((kind, bytes))
    }
    pub fn put(&self, oid: GitOid, native: NativeTarget, raw: &[u8]) -> Result<()> {
        if GitOid::hash(native.kind().as_str(), raw) != oid {
            return Err(invalid("Git object hash mismatch"));
        }
        let record = OidRecord {
            raw: self.repo.blobs.put(raw)?,
            native,
        };
        self.repo
            .meta
            .put_if_absent(&self.key(oid), &record.encode())?;
        if let NativeTarget::Commit(id) = record.native {
            self.repo.associate_changeset(&self.projection.repo, id)?;
        }
        Ok(())
    }
    /// Make `oid` of `source` resolvable in this projection without copying
    /// its stored bytes. Callers associate commit changesets first.
    pub fn copy_record(&self, source: &GitIndex<'_>, oid: GitOid) -> Result<OidRecord> {
        let record = source.require(oid)?;
        self.repo
            .meta
            .put_if_absent(&self.key(oid), &record.encode())?;
        Ok(record)
    }
    fn publication_key(&self, oid: GitOid) -> Vec<u8> {
        format!("git/published/v1/{}/{oid}", self.projection.namespace()).into_bytes()
    }
    pub fn publish_commit(&self, oid: GitOid) -> Result<()> {
        let id = self
            .find_changeset(oid)?
            .ok_or_else(|| invalid("only indexed commits can be explicitly published"))?;
        self.repo.require_membership(&self.projection.repo, id)?;
        // Explicit publication grants the complete history. Partial clients must
        // be able to fetch its promised leaves without an advertised ref.
        let objects = crate::reachable(self, [oid], &Default::default())?;
        let mut parents = Vec::new();
        for object in objects {
            if self.require(object)?.native.kind() != GitKind::Blob {
                parents.push(object);
                continue;
            }
            self.repo
                .meta
                .put_if_absent(&self.publication_key(object), &[])?;
        }
        // With blob:none, every visible parent must already have durable grants
        // for all of its promised blob leaves, even if publication is interrupted.
        for parent in parents {
            self.repo
                .meta
                .put_if_absent(&self.publication_key(parent), &[])?;
        }
        Ok(())
    }
    pub fn is_published(&self, oid: GitOid) -> Result<bool> {
        Ok(self.repo.meta.get(&self.publication_key(oid))?.is_some())
    }
    pub fn find_changeset(&self, oid: GitOid) -> Result<Option<ChangesetId>> {
        Ok(match self.find(oid)? {
            Some(OidRecord {
                native: NativeTarget::Commit(id),
                ..
            }) => Some(id),
            _ => None,
        })
    }
}
