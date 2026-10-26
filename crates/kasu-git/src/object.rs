use kasu_core::*;

pub const HASH_KIND: gix::hash::Kind = gix::hash::Kind::Sha1;
pub fn git_error(error: impl std::fmt::Display) -> Error {
    invalid(format!("Git: {error}"))
}
pub fn oid_from_bytes(bytes: &[u8]) -> Result<GitOid> {
    Ok(GitOid(bytes.try_into().map_err(|_| {
        invalid("only SHA-1 Git repositories are supported")
    })?))
}
pub fn oid_from_hex(bytes: &[u8]) -> Result<GitOid> {
    let mut id = [0; 20];
    hex::decode_to_slice(bytes, &mut id).map_err(git_error)?;
    Ok(GitOid(id))
}
pub fn parse_commit(raw: &[u8]) -> Result<gix::objs::CommitRef<'_>> {
    gix::objs::CommitRef::from_bytes(raw, HASH_KIND).map_err(git_error)
}
pub fn parse_tag(raw: &[u8]) -> Result<gix::objs::TagRef<'_>> {
    gix::objs::TagRef::from_bytes(raw, HASH_KIND).map_err(git_error)
}
pub fn signature(raw: &[u8]) -> Result<Signature> {
    let parsed = gix::actor::SignatureRef::from_bytes(raw).map_err(git_error)?;
    // Preserve timezone spelling (including -0000) independently of normalized offset.
    let (timestamp, tz) = parsed
        .time
        .rsplit_once(' ')
        .ok_or_else(|| invalid("signature lacks timezone"))?;
    let timestamp = timestamp.parse::<i64>().map_err(git_error)?;
    if tz.is_empty() {
        return Err(invalid("signature lacks timezone"));
    }
    Ok(Signature {
        name: parsed.name.into(),
        email: parsed.email.into(),
        timestamp,
        tz: tz.as_bytes().into(),
    })
}
#[derive(Clone, Debug)]
pub struct RawTreeEntry {
    pub name: BString,
    pub oid: GitOid,
    pub mode: u16,
}
impl RawTreeEntry {
    pub fn is_tree(&self) -> bool {
        self.mode == 0o40000
    }
    pub fn is_gitlink(&self) -> bool {
        self.mode == 0o160000
    }
}
pub fn parse_tree(raw: &[u8]) -> Result<Vec<RawTreeEntry>> {
    let mut entries = Vec::new();
    let mut names = std::collections::BTreeSet::new();
    for entry in gix::objs::TreeRefIter::from_bytes(raw, HASH_KIND) {
        let entry = entry.map_err(git_error)?;
        let name: BString = entry.filename.into();
        let path = RepoPath::new(name.clone())?;
        if path.components().count() != 1 || !names.insert(name.clone()) {
            return Err(invalid("duplicate or invalid Git tree name"));
        }
        let mode = entry.mode.value();
        if !matches!(mode, 0o40000 | 0o120000 | 0o160000) && mode & 0o170000 != 0o100000 {
            return Err(invalid("unsupported Git tree mode"));
        }
        entries.push(RawTreeEntry {
            name,
            oid: oid_from_bytes(entry.oid.as_bytes())?,
            mode,
        });
    }
    Ok(entries)
}
pub fn synthesize_commit(cs: &Changeset, tree: GitOid, parents: &[GitOid]) -> Result<Vec<u8>> {
    if cs.git_provenance.is_some() || parents.len() != cs.parents.len() {
        return Err(invalid(
            "cannot synthesize Git provenance or mismatched parents",
        ));
    }
    encode_commit(cs, tree, parents, None)
}
pub(crate) fn synthesize_path_commit(
    cs: &Changeset,
    tree: GitOid,
    parents: &[GitOid],
    namespace: &str,
) -> Result<Vec<u8>> {
    encode_commit(cs, tree, parents, Some(namespace))
}
fn encode_commit(
    cs: &Changeset,
    tree: GitOid,
    parents: &[GitOid],
    projection: Option<&str>,
) -> Result<Vec<u8>> {
    if cs.message.contains(&0) {
        return Err(invalid(
            "NUL message cannot be published as a valid Git commit",
        ));
    }
    let mut out = format!("tree {tree}\n").into_bytes();
    for parent in parents {
        out.extend(format!("parent {parent}\n").bytes());
    }
    fn append(out: &mut Vec<u8>, key: &[u8], signature: &Signature) -> Result<()> {
        if signature
            .name
            .iter()
            .chain(signature.email.iter())
            .any(|b| matches!(*b, 0 | b'\n' | b'\r' | b'<' | b'>'))
        {
            return Err(invalid("signature cannot be encoded as a Git identity"));
        }
        let tz: &[u8] = &signature.tz;
        if tz.len() != 5 || !matches!(tz[0], b'+' | b'-') || !tz[1..].iter().all(u8::is_ascii_digit)
        {
            return Err(invalid("invalid Git timezone"));
        }
        out.extend(key);
        out.extend_from_slice(&signature.name);
        out.extend(b" <");
        out.extend_from_slice(&signature.email);
        out.extend(format!("> {} ", signature.timestamp).bytes());
        out.extend(tz);
        out.push(b'\n');
        Ok(())
    }
    append(&mut out, b"author ", &cs.author)?;
    append(&mut out, b"committer ", &cs.committer)?;
    // Synthesis v1 always includes this header, and preserves message bytes exactly.
    out.extend(format!("kasu-changeset {}\n", cs.id()).bytes());
    if let Some(projection) = projection {
        out.extend(format!("kasu-projection-v1 {projection}\n").bytes());
    }
    out.push(b'\n');
    out.extend_from_slice(&cs.message);
    Ok(out)
}
