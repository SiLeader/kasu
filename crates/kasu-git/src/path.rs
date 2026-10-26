use crate::*;
use kasu_core::*;
use kasu_derived::*;
use kasu_store::Repository;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathCommit(pub Option<GitOid>);
impl Canonical for PathCommit {
    fn encode_to(&self, out: &mut Encoder) {
        match self.0 {
            None => out.byte(0),
            Some(oid) => {
                out.byte(1);
                oid.encode_to(out);
            }
        }
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self(match input.byte()? {
            0 => None,
            1 => Some(GitOid::decode_from(input)?),
            _ => return Err(invalid("invalid path commit")),
        }))
    }
}
pub fn git_is_ancestor(index: &GitIndex<'_>, ancestor: GitOid, descendant: GitOid) -> Result<bool> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![descendant];
    while let Some(oid) = stack.pop() {
        if oid == ancestor {
            return Ok(true);
        }
        if seen.insert(oid) {
            let (kind, raw) = index.object(oid)?;
            if kind != GitKind::Commit {
                return Err(invalid("expected projected commit"));
            }
            stack.extend(
                parse_commit(&raw)?
                    .parents
                    .iter()
                    .map(|oid| oid_from_hex(oid))
                    .collect::<Result<Vec<_>>>()?,
            );
        }
    }
    Ok(false)
}
impl DerivedData for PathCommit {
    const NAME: &'static str = "PathCommit";
    const VERSION: u32 = 1;
    const MODE: DeriveMode = DeriveMode::Lazy;
    const PARENTS: ParentSelection = ParentSelection::AllParents;
    type SubKey = ProjectionId;
    fn dependencies(_: &Ctx<'_>, cs: &Changeset, key: &ProjectionId) -> Result<Vec<DerivedRef>> {
        if key.path.is_none() {
            return Err(invalid("PathCommit requires a directory"));
        }
        Ok(vec![DerivedRef::of::<GitTree>(cs.id(), key)])
    }
    fn derive(ctx: &Ctx<'_>, cs: &Changeset, key: &ProjectionId, parents: &[Self]) -> Result<Self> {
        ctx.repo.associate_changeset(&key.repo, cs.id())?;
        let tree = ctx.get::<GitTree>(cs.id(), key)?.0;
        let index = GitIndex {
            repo: Repository {
                meta: ctx.repo.meta,
                blobs: ctx.repo.blobs,
            },
            projection: key.clone(),
        };
        let mut tips = Vec::new();
        for tip in parents.iter().filter_map(|parent| parent.0) {
            if !tips.contains(&tip) {
                tips.push(tip);
            }
        }
        let mut reduced = Vec::new();
        for tip in &tips {
            let mut redundant = false;
            for other in &tips {
                if tip != other && git_is_ancestor(&index, *tip, *other)? {
                    redundant = true;
                    break;
                }
            }
            if !redundant {
                reduced.push(*tip);
            }
        }
        if reduced.is_empty() && tree == GitOid::hash("tree", b"") {
            return Ok(Self(None));
        }
        if let [parent] = reduced.as_slice() {
            let (_, raw) = index.object(*parent)?;
            if oid_from_hex(parse_commit(&raw)?.tree)? == tree {
                return Ok(Self(Some(*parent)));
            }
        }
        let raw = crate::object::synthesize_path_commit(cs, tree, &reduced, &key.namespace())?;
        let oid = GitOid::hash("commit", &raw);
        index.put(oid, NativeTarget::Commit(cs.id()), &raw)?;
        Ok(Self(Some(oid)))
    }
}
