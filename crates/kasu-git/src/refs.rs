use crate::*;
use kasu_core::*;
use kasu_derived::*;
use kasu_store::GitRefStore;
use std::collections::BTreeMap;

pub fn validate_branch_name(name: &[u8]) -> Result<()> {
    if !name.starts_with(b"refs/heads/") {
        return Err(invalid("native branch name must start with refs/heads/"));
    }
    validate_git_ref_name(name)
}
pub fn validate_git_ref_name(name: &[u8]) -> Result<()> {
    if !name.starts_with(b"refs/") || name.len() > 1024 {
        return Err(invalid("invalid Git ref namespace or length"));
    }
    gix::refs::FullName::try_from(BString::from(name))
        .map_err(|e| invalid(format!("invalid branch name: {e}")))?;
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvertisedRef {
    pub name: Vec<u8>,
    pub value: GitRef,
}
pub fn list_refs(
    engine: &Engine,
    ctx: &Ctx<'_>,
    store: &dyn GitRefStore,
    projection: &ProjectionId,
) -> Result<Vec<AdvertisedRef>> {
    list_refs_with_head(engine, ctx, store, projection, b"refs/heads/main")
}
/// Advertise refs with `head` as the HEAD symref unless HEAD is stored explicitly.
pub fn list_refs_with_head(
    engine: &Engine,
    ctx: &Ctx<'_>,
    store: &dyn GitRefStore,
    projection: &ProjectionId,
    head: &[u8],
) -> Result<Vec<AdvertisedRef>> {
    let mut refs = BTreeMap::new();
    for (name, state) in store.list(&projection.repo, b"refs/heads/")? {
        if let Some(target) = state.target {
            let target = if projection.path.is_some() {
                let Some(oid) = engine.derive::<PathCommit>(ctx, target, projection)?.0 else {
                    continue;
                };
                oid
            } else {
                engine.derive::<GitCommit>(ctx, target, projection)?.0
            };
            refs.insert(
                name,
                GitRef::Direct {
                    target,
                    peeled: None,
                    kind: GitKind::Commit,
                },
            );
        }
    }
    for (name, state) in store.list_git(&projection.repo, b"")? {
        if let Some(value) = state.value {
            let value = match value {
                GitRef::Direct { target, .. } if projection.path.is_some() => {
                    let Some(value) = project_target(engine, ctx, projection, target)? else {
                        continue;
                    };
                    value
                }
                value => value,
            };
            refs.insert(name, value);
        }
    }
    refs.entry(b"HEAD".to_vec()).or_insert(GitRef::Symbolic {
        target: head.to_vec(),
    });
    Ok(refs
        .into_iter()
        .map(|(name, value)| AdvertisedRef { name, value })
        .collect())
}

fn project_target(
    engine: &Engine,
    ctx: &Ctx<'_>,
    projection: &ProjectionId,
    target: GitOid,
) -> Result<Option<GitRef>> {
    let source = GitIndex {
        repo: kasu_store::Repository {
            meta: ctx.repo.meta,
            blobs: ctx.repo.blobs,
        },
        projection: ProjectionId::full(projection.repo.clone()),
    };
    let index = GitIndex {
        repo: kasu_store::Repository {
            meta: ctx.repo.meta,
            blobs: ctx.repo.blobs,
        },
        projection: projection.clone(),
    };
    let mut tags = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut oid = target;
    let (mut projected, leaf_kind) = loop {
        if !seen.insert(oid) {
            return Err(invalid("tag cycle"));
        }
        let record = source.require(oid)?;
        match record.native {
            NativeTarget::Commit(id) => {
                let Some(oid) = engine.derive::<PathCommit>(ctx, id, projection)?.0 else {
                    return Ok(None);
                };
                break (oid, GitKind::Commit);
            }
            NativeTarget::Tree(root) => {
                let path = projection
                    .path
                    .as_ref()
                    .ok_or_else(|| invalid("path projection required"))?;
                let Some(TreeEntry::SubTree(subtree)) = lookup(&ctx.repo, root, path)? else {
                    return Ok(None);
                };
                break (synthesize_tree(&index, subtree)?, GitKind::Tree);
            }
            NativeTarget::Blob(_) => return Ok(None),
            NativeTarget::Tag => {
                let (_, raw) = source.object(oid)?;
                oid = oid_from_hex(parse_tag(&raw)?.target)?;
                tags.push(raw);
            }
        }
    };
    let peeled = (!tags.is_empty()).then_some(projected);
    for raw in tags.iter().rev() {
        let header_end = raw
            .iter()
            .position(|b| *b == b'\n')
            .ok_or_else(|| invalid("invalid tag header"))?;
        let mut rewritten = format!("object {projected}\n").into_bytes();
        let remainder = &raw[header_end + 1..];
        // A rewritten target invalidates a source signature. Keep the tag's
        // identity and message, excluding the detached signature suffix.
        let signature = [
            b"\n-----BEGIN PGP SIGNATURE-----".as_slice(),
            b"\n-----BEGIN PGP MESSAGE-----".as_slice(),
            b"\n-----BEGIN SIGNED MESSAGE-----".as_slice(),
            b"\n-----BEGIN SSH SIGNATURE-----".as_slice(),
        ]
        .iter()
        .filter_map(|marker| remainder.windows(marker.len()).position(|w| w == *marker))
        .min()
        .unwrap_or(remainder.len());
        rewritten.extend_from_slice(&remainder[..signature]);
        projected = GitOid::hash("tag", &rewritten);
        index.put(projected, NativeTarget::Tag, &rewritten)?;
    }
    Ok(Some(GitRef::Direct {
        target: projected,
        peeled,
        kind: if peeled.is_some() {
            GitKind::Tag
        } else {
            leaf_kind
        },
    }))
}

pub fn peel(index: &GitIndex<'_>, start: GitOid) -> Result<(GitOid, GitKind)> {
    let mut seen = std::collections::BTreeSet::new();
    let mut oid = start;
    loop {
        if !seen.insert(oid) {
            return Err(invalid("tag cycle"));
        }
        let kind = index.require(oid)?.native.kind();
        if kind != GitKind::Tag {
            return Ok((oid, kind));
        }
        let (_, raw) = index.object(oid)?;
        let tag = parse_tag(&raw)?;
        let target = oid_from_hex(tag.target)?;
        if index.require(target)?.native.kind() != from_gix_kind(tag.target_kind) {
            return Err(invalid("tag target type mismatch"));
        }
        oid = target;
    }
}
pub fn from_gix_kind(kind: gix::objs::Kind) -> GitKind {
    match kind {
        gix::objs::Kind::Blob => GitKind::Blob,
        gix::objs::Kind::Tree => GitKind::Tree,
        gix::objs::Kind::Commit => GitKind::Commit,
        gix::objs::Kind::Tag => GitKind::Tag,
    }
}
