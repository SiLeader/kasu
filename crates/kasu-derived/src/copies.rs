use crate::*;
use kasu_core::*;
use std::collections::BTreeSet;

// A lazy reverse index for one content ID. Only requested IDs are materialized.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PathIndex(pub Vec<RepoPath>);
impl Canonical for PathIndex {
    fn encode_to(&self, out: &mut Encoder) {
        out.len(self.0.len());
        for path in &self.0 {
            path.encode_to(out);
        }
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        let mut paths = Vec::new();
        for _ in 0..input.count()? {
            paths.push(RepoPath::decode_from(input)?);
        }
        if paths.windows(2).any(|p| p[0] >= p[1]) {
            return Err(invalid("invalid path index ordering"));
        }
        Ok(Self(paths))
    }
}
impl PathIndex {
    pub fn under<'a>(&'a self, prefix: &'a RepoPath) -> impl Iterator<Item = &'a RepoPath> {
        self.0.iter().filter(move |p| {
            *p == prefix
                || p.as_bytes()
                    .strip_prefix(prefix.as_bytes())
                    .is_some_and(|suffix| suffix.starts_with(b"/"))
        })
    }
}
impl DerivedData for PathIndex {
    const NAME: &'static str = "PathIndex";
    const VERSION: u32 = 1;
    const MODE: DeriveMode = DeriveMode::Lazy;
    const PARENTS: ParentSelection = ParentSelection::FirstParent;
    type SubKey = BlobId;
    fn derive(_: &Ctx<'_>, cs: &Changeset, blob: &BlobId, parents: &[Self]) -> Result<Self> {
        let mut paths: BTreeSet<_> = parents
            .first()
            .map(|p| p.0.iter().cloned().collect())
            .unwrap_or_default();
        for change in &cs.changes {
            paths.remove(change.path());
            if let FileChange::Set { path, entry } = change
                && entry.blob() == Some(*blob)
            {
                paths.insert(path.clone());
            }
        }
        Ok(Self(paths.into_iter().collect()))
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CopySource {
    pub parent: ChangesetId,
    pub path: RepoPath,
    pub rename: bool,
    pub similarity: u8,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CopyDetect(pub Vec<CopySource>);
impl Canonical for CopyDetect {
    fn encode_to(&self, out: &mut Encoder) {
        out.len(self.0.len());
        for source in &self.0 {
            source.parent.encode_to(out);
            source.path.encode_to(out);
            out.byte(u8::from(source.rename));
            out.byte(source.similarity);
        }
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        let mut sources = Vec::new();
        for _ in 0..input.count()? {
            let parent = ChangesetId::decode_from(input)?;
            let path = RepoPath::decode_from(input)?;
            let rename = match input.byte()? {
                0 => false,
                1 => true,
                _ => return Err(invalid("invalid copy kind")),
            };
            let similarity = input.byte()?;
            if similarity > 100 {
                return Err(invalid("invalid copy score"));
            }
            sources.push(CopySource {
                parent,
                path,
                rename,
                similarity,
            });
        }
        Ok(Self(sources))
    }
}
fn entry(ctx: &Ctx<'_>, id: ChangesetId, path: &RepoPath) -> Result<Option<Entry>> {
    Ok(
        match lookup(&ctx.repo, ctx.get::<RootTree>(id, &())?.0, path)? {
            Some(TreeEntry::Leaf(entry)) => Some(entry),
            _ => None,
        },
    )
}
fn same_kind(a: &Entry, b: &Entry) -> bool {
    matches!(
        (a, b),
        (Entry::File { .. }, Entry::File { .. }) | (Entry::Symlink { .. }, Entry::Symlink { .. })
    )
}
impl DerivedData for CopyDetect {
    const NAME: &'static str = "CopyDetect";
    const VERSION: u32 = 1;
    const MODE: DeriveMode = DeriveMode::Lazy;
    const PARENTS: ParentSelection = ParentSelection::None;
    type SubKey = RepoPath;
    fn dependencies(_: &Ctx<'_>, cs: &Changeset, _: &RepoPath) -> Result<Vec<DerivedRef>> {
        Ok(std::iter::once(cs.id())
            .chain(cs.parents.iter().copied())
            .map(|id| DerivedRef::of::<RootTree>(id, &()))
            .collect())
    }
    fn resolved_dependencies(
        ctx: &Ctx<'_>,
        cs: &Changeset,
        path: &RepoPath,
    ) -> Result<Vec<DerivedRef>> {
        let Some(blob) = entry(ctx, cs.id(), path)?.and_then(|e| e.blob()) else {
            return Ok(Vec::new());
        };
        let mut deps = Vec::new();
        for parent in &cs.parents {
            if entry(ctx, *parent, path)?.is_none() {
                deps.push(DerivedRef::of::<PathIndex>(*parent, &blob));
            }
        }
        Ok(deps)
    }
    fn derive(ctx: &Ctx<'_>, cs: &Changeset, path: &RepoPath, _: &[Self]) -> Result<Self> {
        let Some(current) = entry(ctx, cs.id(), path)? else {
            return Ok(Self::default());
        };
        let Some(blob) = current.blob() else {
            return Ok(Self::default());
        };
        let root = ctx.get::<RootTree>(cs.id(), &())?.0;
        let mut sources = Vec::new();
        for parent in &cs.parents {
            if entry(ctx, *parent, path)?.is_some() {
                continue;
            }
            let changes = diff(&ctx.repo, ctx.get::<RootTree>(*parent, &())?.0, root)?;
            let mut destinations = Vec::new();
            for change in &changes {
                if let FileChange::Set { path, entry: value } = change
                    && value.blob() == Some(blob)
                    && same_kind(value, &current)
                    && entry(ctx, *parent, path)?.is_none()
                {
                    destinations.push(path.clone());
                }
            }
            let mut exact = Vec::new();
            let mut removed = Vec::new();
            for candidate in ctx.get::<PathIndex>(*parent, &blob)?.0 {
                let old = entry(ctx, *parent, &candidate)?
                    .ok_or_else(|| invalid("invalid path index entry"))?;
                if !same_kind(&old, &current) {
                    continue;
                }
                if entry(ctx, cs.id(), &candidate)?.is_none() {
                    removed.push(candidate.clone());
                }
                exact.push(CopySource {
                    parent: *parent,
                    path: candidate,
                    rename: false,
                    similarity: 100,
                });
            }
            for (source, destination) in removed.iter().zip(&destinations) {
                if destination == path
                    && let Some(found) = exact.iter_mut().find(|s| s.path == *source)
                {
                    found.rename = true;
                }
            }
            exact.sort_by(|a, b| b.rename.cmp(&a.rename).then(a.path.cmp(&b.path)));
            if let Some(source) = exact.into_iter().next() {
                sources.push(source);
                continue;
            }
            if !matches!(current, Entry::File { .. }) {
                continue;
            }
            let bytes = ctx.repo.blob(blob)?;
            if bytes.contains(&0) || bytes.len() > 8 * 1024 * 1024 {
                continue;
            }
            if let Some(source) = similar_rename(ctx, *parent, path, &changes)? {
                sources.push(source);
            }
        }
        Ok(Self(sources))
    }
}

fn similar_rename(
    ctx: &Ctx<'_>,
    parent: ChangesetId,
    target: &RepoPath,
    changes: &[FileChange],
) -> Result<Option<CopySource>> {
    let mut before = Vec::new();
    let mut after = Vec::new();
    for change in changes {
        match change {
            FileChange::Delete { path } => {
                if let Some(value) = entry(ctx, parent, path)? {
                    before.push((path, value));
                }
            }
            FileChange::Set { path, entry: value } if entry(ctx, parent, path)?.is_none() => {
                after.push((path, value.clone()))
            }
            _ => {}
        }
    }
    let mut pairs = Vec::new();
    let mut used_before = BTreeSet::new();
    let mut used_after = BTreeSet::new();
    // Reserve exact rewrites before similarity matching, consistently for every
    // requested destination. Each deleted file has at most one rename target.
    for (new, new_value) in &after {
        for (old, old_value) in &before {
            if !used_before.contains(*old)
                && same_kind(old_value, new_value)
                && old_value.blob() == new_value.blob()
            {
                used_before.insert((*old).clone());
                used_after.insert((*new).clone());
                break;
            }
        }
    }
    let mut comparisons = 0;
    for (new, new_value) in &after {
        if used_after.contains(*new) || !matches!(new_value, Entry::File { .. }) {
            continue;
        }
        let bytes = ctx.repo.blob(new_value.blob().unwrap())?;
        if bytes.contains(&0) || bytes.len() > 8 * 1024 * 1024 {
            continue;
        }
        let new_lines = lines(&bytes);
        for (old, old_value) in &before {
            if used_before.contains(*old) || !matches!(old_value, Entry::File { .. }) {
                continue;
            }
            comparisons += 1;
            if comparisons > 1000 {
                return Err(invalid("rename candidate comparison limit exceeded"));
            }
            let old_bytes = ctx.repo.blob(old_value.blob().unwrap())?;
            if old_bytes.contains(&0) || old_bytes.len() > 8 * 1024 * 1024 {
                continue;
            }
            let common: usize = matching_lines(&old_bytes, &bytes)
                .into_iter()
                .map(|(_, n)| new_lines[n].len())
                .sum();
            let score = (common * 100 / old_bytes.len().max(bytes.len()).max(1)) as u8;
            if score >= 50 {
                pairs.push((score, (*new).clone(), (*old).clone()));
            }
        }
    }
    pairs.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    for (score, new, old) in pairs {
        if used_before.contains(&old) || used_after.contains(&new) {
            continue;
        }
        used_before.insert(old.clone());
        used_after.insert(new.clone());
        if new == *target {
            return Ok(Some(CopySource {
                parent,
                path: old,
                rename: true,
                similarity: score,
            }));
        }
    }
    Ok(None)
}
