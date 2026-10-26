use kasu_core::*;
use kasu_derived::*;
use kasu_git::*;
use kasu_store::*;
use std::{collections::BTreeSet, path::Path};

#[derive(Debug)]
pub struct ImportReport {
    pub objects: usize,
    pub commits: usize,
    pub refs: Vec<(Vec<u8>, GitRef)>,
}
pub struct Importer<'a> {
    pub engine: &'a Engine,
    pub index: GitIndex<'a>,
}
impl Importer<'_> {
    // Import only stages validated immutable objects. Ref publication is explicit
    // and occurs after the complete graph has passed connectivity/type checks.
    pub fn import(&self, path: impl AsRef<Path>) -> Result<ImportReport> {
        let source = gix::open(path.as_ref()).map_err(git_error)?;
        if source.object_hash() != HASH_KIND {
            return Err(invalid("SHA-256 Git import is not supported"));
        }
        if source.is_shallow() {
            return Err(invalid("shallow Git import requires complete parents"));
        }
        let platform = source.references().map_err(git_error)?;
        let mut refs = Vec::new();
        let mut roots = Vec::new();
        for reference in platform.all().map_err(git_error)? {
            let reference = reference.map_err(git_error)?;
            let name = reference.name().as_bstr().to_vec();
            // Remote tracking refs and symbolic refs other than HEAD cannot be
            // silently discarded: callers must choose an importable repository.
            match reference.target() {
                gix::refs::TargetRef::Object(id) => {
                    let oid = oid_from_bytes(id.as_bytes())?;
                    roots.push((oid, None));
                    refs.push((name, oid));
                }
                gix::refs::TargetRef::Symbolic(_) => {
                    return Err(invalid(
                        "symbolic refs other than HEAD are not supported for import",
                    ));
                }
            }
        }
        let head = source.head().map_err(git_error)?;
        let head_value = if let Some(name) = head.referent_name() {
            GitRef::Symbolic {
                target: name.as_bstr().to_vec(),
            }
        } else {
            let id = source.head_id().map_err(git_error)?;
            let oid = oid_from_bytes(id.as_bytes())?;
            roots.push((oid, Some(GitKind::Commit)));
            GitRef::Direct {
                target: oid,
                peeled: None,
                kind: GitKind::Commit,
            }
        };
        let (objects, commits) = self.stage_objects(
            roots,
            &|oid| {
                let id = gix::hash::ObjectId::from_bytes_or_panic(&oid.0);
                if source.find_header(id).map_err(git_error)?.size() > MAX_OBJECT_SIZE {
                    return Err(invalid("Git object exceeds import size limit"));
                }
                let object = source.find_object(id).map_err(git_error)?;
                Ok((from_gix_kind(object.kind), object.data.to_vec()))
            },
            &BTreeSet::new(),
        )?;
        let mut projected = Vec::new();
        for (name, oid) in refs {
            let kind = self.index.require(oid)?.native.kind();
            if name.starts_with(b"refs/heads/") && kind != GitKind::Commit {
                return Err(invalid("branch target is not a commit"));
            }
            let peeled = if kind == GitKind::Tag {
                Some(peel(&self.index, oid)?.0)
            } else {
                None
            };
            projected.push((
                name,
                GitRef::Direct {
                    target: oid,
                    peeled,
                    kind,
                },
            ));
        }
        projected.push((b"HEAD".to_vec(), head_value));
        projected.sort_by(|a, b| a.0.cmp(&b.0));
        tracing::info!(objects, commits, "Git import validated");
        Ok(ImportReport {
            objects,
            commits,
            refs: projected,
        })
    }

    /// Stage a connectivity-checked graph without publishing any refs. Trusted
    /// objects must already be validated and public in this repository.
    pub fn stage_objects(
        &self,
        roots: Vec<(GitOid, Option<GitKind>)>,
        find: &dyn Fn(GitOid) -> Result<(GitKind, Vec<u8>)>,
        trusted: &BTreeSet<GitOid>,
    ) -> Result<(usize, usize)> {
        let ctx = Ctx {
            repo: Repository {
                meta: self.index.repo.meta,
                blobs: self.index.repo.blobs,
            },
        };
        let mut stack: Vec<_> = roots
            .into_iter()
            .map(|(oid, kind)| (oid, kind, false))
            .collect();
        let mut active = BTreeSet::new();
        let mut done = BTreeSet::new();
        let mut commits = 0;
        while let Some((oid, expected, expanded)) = stack.pop() {
            if done.contains(&oid) {
                if let Some(expected) = expected
                    && self.index.require(oid)?.native.kind() != expected
                {
                    return Err(invalid("Git dependency type mismatch"));
                }
                continue;
            }
            let (kind, bytes) = find(oid)?;
            let raw = &bytes;
            if raw.len() as u64 > MAX_OBJECT_SIZE {
                return Err(invalid("Git object too large"));
            }
            if GitOid::hash(kind.as_str(), raw) != oid || expected.is_some_and(|want| want != kind)
            {
                return Err(invalid("Git object hash or dependency type mismatch"));
            }
            if trusted.contains(&oid) {
                let record = self.index.require(oid)?;
                if record.native.kind() != kind || self.index.object(oid)?.1 != *raw {
                    return Err(invalid("trusted Git object mismatch"));
                }
                done.insert(oid);
                continue;
            }
            if !expanded {
                if !active.insert(oid) {
                    return Err(invalid("cyclic Git object graph"));
                }
                let deps = dependencies(kind, raw)?;
                stack.push((oid, expected, true));
                stack.extend(
                    deps.into_iter()
                        .rev()
                        .map(|(id, kind)| (id, Some(kind), false)),
                );
                continue;
            }
            // Native commits can return through Git after clone/push. Reuse the
            // existing mapping instead of inventing a second provenance-bearing
            // changeset for an OID that already identifies a native changeset.
            if let Some(existing) = self.index.find(oid)? {
                if existing.native.kind() != kind || self.index.object(oid)?.1 != *raw {
                    return Err(invalid("existing Git OID mapping disagrees with source"));
                }
                if let NativeTarget::Tree(id) = existing.native {
                    synthesize_tree(&self.index, id)?;
                }
                if let NativeTarget::Blob(id) = existing.native {
                    self.index
                        .repo
                        .associate_blob(&self.index.projection.repo, id)?;
                }
                if let NativeTarget::Commit(id) = existing.native {
                    self.index
                        .repo
                        .associate_changeset(&self.index.projection.repo, id)?;
                    if self
                        .engine
                        .derive::<GitCommit>(&ctx, id, &self.index.projection)?
                        .0
                        != oid
                    {
                        return Err(invalid("existing native commit projection mismatch"));
                    }
                    commits += 1;
                }
                active.remove(&oid);
                done.insert(oid);
                continue;
            }
            let native = match kind {
                GitKind::Blob => {
                    let blob = ctx.repo.put_blob(raw)?;
                    ctx.repo.associate_blob(&self.index.projection.repo, blob)?;
                    NativeTarget::Blob(blob)
                }
                GitKind::Tree => {
                    let mut tree = Tree::default();
                    for entry in parse_tree(raw)? {
                        let value = if entry.is_gitlink() {
                            TreeEntry::Leaf(Entry::Gitlink { commit: entry.oid })
                        } else {
                            match self.index.require(entry.oid)?.native {
                                NativeTarget::Tree(id) if entry.is_tree() => TreeEntry::SubTree(id),
                                NativeTarget::Blob(blob) if !entry.is_tree() => {
                                    TreeEntry::Leaf(if entry.mode == 0o120000 {
                                        Entry::Symlink { blob }
                                    } else {
                                        Entry::File {
                                            blob,
                                            executable: entry.mode & 0o100 != 0,
                                        }
                                    })
                                }
                                _ => return Err(invalid("tree target type mismatch")),
                            }
                        };
                        tree.entries.insert(entry.name, value);
                    }
                    NativeTarget::Tree(ctx.repo.put_tree(&tree)?)
                }
                GitKind::Commit => {
                    let parsed = parse_commit(raw)?;
                    let parents = parsed
                        .parents
                        .iter()
                        .map(|p| match self.index.require(oid_from_hex(p)?)?.native {
                            NativeTarget::Commit(id) => Ok(id),
                            _ => Err(invalid("parent is not a commit")),
                        })
                        .collect::<Result<Vec<_>>>()?;
                    let base = match parents.first() {
                        Some(id) => self.engine.derive::<RootTree>(&ctx, *id, &())?.0,
                        None => ctx.repo.put_tree(&Tree::default())?,
                    };
                    let root = match self.index.require(oid_from_hex(parsed.tree)?)?.native {
                        NativeTarget::Tree(id) => id,
                        _ => return Err(invalid("commit root is not a tree")),
                    };
                    let changes = diff(&ctx.repo, base, root)?;
                    // Empty directories and other trees the FileChange model cannot
                    // represent must fail explicitly rather than altering the tree.
                    if apply(&ctx.repo, base, &changes)? != root {
                        return Err(invalid(
                            "Git tree cannot be represented by file changes (e.g. empty directories)",
                        ));
                    }
                    let cs = Changeset {
                        parents,
                        author: signature(parsed.author)?,
                        committer: signature(parsed.committer)?,
                        message: parsed.message.into(),
                        changes,
                        git_provenance: Some(GitProvenance {
                            raw_commit: ctx.repo.blobs.put(raw)?,
                        }),
                    };
                    let id = ctx.repo.put_changeset(&cs)?;
                    ctx.repo.meta.put_if_absent(
                        &DerivedRef::of::<RootTree>(id, &()).storage_key(),
                        &RootTree(root).encode(),
                    )?;
                    self.engine.eager(&ctx, id)?;
                    commits += 1;
                    NativeTarget::Commit(id)
                }
                GitKind::Tag => {
                    let _ = parse_tag(raw)?;
                    NativeTarget::Tag
                }
            };
            let native_tree = match &native {
                NativeTarget::Tree(id) => Some(*id),
                _ => None,
            };
            self.index.put(oid, native, raw)?;
            if let Some(tree) = native_tree {
                // Warm the canonical Merkle-node cache bottom-up even when this
                // particular imported tree uses a noncanonical raw encoding.
                synthesize_tree(&self.index, tree)?;
            }
            if let Some(id) = self.index.find_changeset(oid)?
                && self
                    .engine
                    .derive::<GitCommit>(&ctx, id, &self.index.projection)?
                    .0
                    != oid
            {
                return Err(invalid("Git import round-trip mismatch"));
            }
            active.remove(&oid);
            done.insert(oid);
        }
        Ok((done.len(), commits))
    }

    // Ref CAS happens after import, so parsing/connectivity errors leave published
    // refs untouched. Branches, tags, HEAD and both logs publish atomically.
    pub fn publish(
        &self,
        store: &dyn GitRefStore,
        report: &ImportReport,
        actor: &[u8],
        time: i64,
    ) -> Result<()> {
        let mut names = BTreeSet::new();
        for (name, value) in &report.refs {
            let _ =
                gix::refs::FullName::try_from(BString::from(name.clone())).map_err(git_error)?;
            if !names.insert(name) || (name != b"HEAD" && !name.starts_with(b"refs/")) {
                return Err(invalid("invalid or duplicate publication ref"));
            }
            match value {
                GitRef::Direct {
                    target,
                    peeled,
                    kind,
                } => {
                    if self.index.object(*target)?.0 != *kind {
                        return Err(invalid("publication ref target type mismatch"));
                    }
                    if (name == b"HEAD" || name.starts_with(b"refs/heads/"))
                        && *kind != GitKind::Commit
                    {
                        return Err(invalid("branch/HEAD must target a commit"));
                    }
                    let expected_peel = if *kind == GitKind::Tag {
                        Some(peel(&self.index, *target)?.0)
                    } else {
                        None
                    };
                    if *peeled != expected_peel {
                        return Err(invalid("publication peeled tag mismatch"));
                    }
                }
                GitRef::Symbolic { target } => {
                    let _ = gix::refs::FullName::try_from(BString::from(target.clone()))
                        .map_err(git_error)?;
                    if name != b"HEAD" || !target.starts_with(b"refs/heads/") {
                        return Err(invalid("only HEAD to a branch can be symbolic"));
                    }
                }
            }
        }
        let mut native = Vec::new();
        let mut git = Vec::new();
        for (name, value) in &report.refs {
            if name.starts_with(b"refs/heads/") {
                let GitRef::Direct { target, .. } = value else {
                    return Err(invalid("symbolic branch"));
                };
                let target = self
                    .index
                    .find_changeset(*target)?
                    .ok_or_else(|| invalid("branch is not an indexed commit"))?;
                let expected = store.resolve(&self.index.projection.repo, name)?;
                if expected.target != Some(target) {
                    native.push(RefUpdate {
                        name: name.clone(),
                        expected,
                        new: Some(target),
                    });
                }
            } else {
                let expected = store.resolve_git(&self.index.projection.repo, name)?;
                if expected.value.as_ref() != Some(value) {
                    git.push(GitRefUpdate {
                        name: name.clone(),
                        expected,
                        new: Some(value.clone()),
                    });
                }
            }
        }
        if !matches!(
            store.compare_native_and_git(
                &self.index.projection.repo,
                &native,
                &git,
                actor,
                time
            )?,
            RefPublicationResult::Applied { .. }
        ) {
            return Err(invalid("concurrent ref update during import publication"));
        }
        Ok(())
    }
}
fn dependencies(kind: GitKind, raw: &[u8]) -> Result<Vec<(GitOid, GitKind)>> {
    match kind {
        GitKind::Blob => Ok(Vec::new()),
        GitKind::Tree => Ok(parse_tree(raw)?
            .into_iter()
            .filter(|e| !e.is_gitlink())
            .map(|e| {
                (
                    e.oid,
                    if e.is_tree() {
                        GitKind::Tree
                    } else {
                        GitKind::Blob
                    },
                )
            })
            .collect()),
        GitKind::Commit => {
            let commit = parse_commit(raw)?;
            let mut out = commit
                .parents
                .iter()
                .map(|p| Ok((oid_from_hex(p)?, GitKind::Commit)))
                .collect::<Result<Vec<_>>>()?;
            out.push((oid_from_hex(commit.tree)?, GitKind::Tree));
            Ok(out)
        }
        GitKind::Tag => {
            let tag = parse_tag(raw)?;
            Ok(vec![(
                oid_from_hex(tag.target)?,
                from_gix_kind(tag.target_kind),
            )])
        }
    }
}
