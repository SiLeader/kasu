use crate::*;
use kasu_core::*;
use kasu_derived::*;
use kasu_store::Repository;

pub fn register_git(engine: &mut Engine) {
    engine.register::<GitTree>();
    engine.register::<GitCommit>();
    engine.register::<PathCommit>();
}
fn index<'a>(ctx: &Ctx<'a>, projection: &ProjectionId) -> GitIndex<'a> {
    GitIndex {
        repo: Repository {
            meta: ctx.repo.meta,
            blobs: ctx.repo.blobs,
        },
        projection: projection.clone(),
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitTree(pub GitOid);
impl Canonical for GitTree {
    fn encode_to(&self, out: &mut Encoder) {
        self.0.encode_to(out);
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self(GitOid::decode_from(input)?))
    }
}
impl DerivedData for GitTree {
    const NAME: &'static str = "GitTree";
    const VERSION: u32 = 1;
    const MODE: DeriveMode = DeriveMode::Eager;
    const PARENTS: ParentSelection = ParentSelection::FirstParent;
    type SubKey = ProjectionId;
    // Publishing repositories explicitly schedules their projection keys; the
    // generic eager worker must not assume every repository is Git-public.
    fn dependencies(_: &Ctx<'_>, cs: &Changeset, _: &ProjectionId) -> Result<Vec<DerivedRef>> {
        Ok(vec![DerivedRef::of::<RootTree>(cs.id(), &())])
    }
    fn derive(ctx: &Ctx<'_>, cs: &Changeset, key: &ProjectionId, _: &[Self]) -> Result<Self> {
        let root = ctx.get::<RootTree>(cs.id(), &())?.0;
        let index = index(ctx, key);
        if let Some(path) = &key.path {
            let subtree = match lookup(&ctx.repo, root, path)? {
                Some(TreeEntry::SubTree(id)) => id,
                _ => ctx.repo.put_tree(&Tree::default())?,
            };
            return Ok(Self(synthesize_tree(&index, subtree)?));
        }
        if let Some(provenance) = &cs.git_provenance {
            let raw = ctx.repo.blobs.get(provenance.raw_commit)?;
            let commit = parse_commit(&raw)?;
            let oid = oid_from_hex(commit.tree)?;
            if index.require(oid)?.native != NativeTarget::Tree(root) {
                return Err(invalid("raw commit tree does not match RootTree"));
            }
            return Ok(Self(oid));
        }
        Ok(Self(synthesize_tree(&index, root)?))
    }
}

// Cache by native Merkle node, independently of changesets. Shared subtrees are
// never walked or recompressed again; no raw-tree variant is chosen globally.
pub fn synthesize_tree(index: &GitIndex<'_>, root: TreeId) -> Result<GitOid> {
    fn key(index: &GitIndex<'_>, id: TreeId) -> Vec<u8> {
        format!("git/tree/v1/{}/{id}", index.projection.namespace()).into_bytes()
    }
    let mut stack = vec![(root, false)];
    while let Some((id, expanded)) = stack.pop() {
        if index.repo.meta.get(&key(index, id))?.is_some() {
            continue;
        }
        let tree = index.repo.tree(id)?;
        if !expanded {
            stack.push((id, true));
            for entry in tree.entries.values() {
                if let TreeEntry::SubTree(id) = entry {
                    stack.push((*id, false));
                }
            }
            continue;
        }
        let mut entries: Vec<_> = tree.entries.iter().collect();
        entries.sort_by(|(a, x), (b, y)| {
            gix::objs::tree::name_order(
                a,
                matches!(x, TreeEntry::SubTree(_)),
                b,
                matches!(y, TreeEntry::SubTree(_)),
            )
        });
        let mut raw = Vec::new();
        for (name, entry) in entries {
            let (mode, oid) = match entry {
                TreeEntry::SubTree(child) => {
                    let value = index
                        .repo
                        .meta
                        .get(&key(index, *child))?
                        .ok_or_else(|| invalid("missing synthesized subtree"))?;
                    (b"40000".as_slice(), GitOid::decode(&value)?)
                }
                TreeEntry::Leaf(Entry::Gitlink { commit }) => (b"160000".as_slice(), *commit),
                TreeEntry::Leaf(entry) => {
                    let blob = entry.blob().unwrap();
                    let metadata = index.repo.blob_metadata(blob)?;
                    match index.find(metadata.git_blob_oid)? {
                        Some(record) if record.native != NativeTarget::Blob(blob) => {
                            return Err(invalid("Git blob OID maps to different native content"));
                        }
                        Some(_) => {}
                        None => {
                            let bytes = index.repo.blob(blob)?;
                            index.put(metadata.git_blob_oid, NativeTarget::Blob(blob), &bytes)?;
                        }
                    }
                    let mode = match entry {
                        Entry::File {
                            executable: true, ..
                        } => b"100755",
                        Entry::File { .. } => b"100644",
                        Entry::Symlink { .. } => b"120000",
                        _ => unreachable!(),
                    };
                    (mode.as_slice(), metadata.git_blob_oid)
                }
            };
            raw.extend(mode);
            raw.push(b' ');
            raw.extend_from_slice(name);
            raw.push(0);
            raw.extend(oid.0);
        }
        let oid = GitOid::hash("tree", &raw);
        index.put(oid, NativeTarget::Tree(id), &raw)?;
        index
            .repo
            .meta
            .put_if_absent(&key(index, id), &oid.encode())?;
    }
    GitOid::decode(
        &index
            .repo
            .meta
            .get(&key(index, root))?
            .ok_or_else(|| invalid("missing root tree"))?,
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitCommit(pub GitOid);
impl Canonical for GitCommit {
    fn encode_to(&self, out: &mut Encoder) {
        self.0.encode_to(out);
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self(GitOid::decode_from(input)?))
    }
}
impl DerivedData for GitCommit {
    const NAME: &'static str = "GitCommit";
    const VERSION: u32 = 1;
    const MODE: DeriveMode = DeriveMode::Eager;
    const PARENTS: ParentSelection = ParentSelection::AllParents;
    type SubKey = ProjectionId;
    fn dependencies(_: &Ctx<'_>, cs: &Changeset, key: &ProjectionId) -> Result<Vec<DerivedRef>> {
        Ok(vec![DerivedRef::of::<GitTree>(cs.id(), key)])
    }
    fn derive(ctx: &Ctx<'_>, cs: &Changeset, key: &ProjectionId, parents: &[Self]) -> Result<Self> {
        if key.path.is_some() {
            return Err(invalid("path histories must use PathCommit"));
        }
        let tree = ctx.get::<GitTree>(cs.id(), key)?.0;
        let parent_oids: Vec<_> = parents.iter().map(|p| p.0).collect();
        let raw = if let Some(provenance) = &cs.git_provenance {
            let bytes = ctx.repo.blobs.get(provenance.raw_commit)?;
            let original = parse_commit(&bytes)?;
            let original_parents = original
                .parents
                .iter()
                .map(|p| oid_from_hex(p))
                .collect::<Result<Vec<_>>>()?;
            if oid_from_hex(original.tree)? != tree || original_parents != parent_oids {
                return Err(invalid(
                    "raw commit connectivity differs from native changeset",
                ));
            }
            drop(original);
            bytes
        } else {
            synthesize_commit(cs, tree, &parent_oids)?
        };
        let oid = GitOid::hash("commit", &raw);
        index(ctx, key).put(oid, NativeTarget::Commit(cs.id()), &raw)?;
        Ok(Self(oid))
    }
}
