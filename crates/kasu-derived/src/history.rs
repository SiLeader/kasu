use crate::*;
use kasu_core::*;

fn path_entry(ctx: &Ctx<'_>, id: ChangesetId, path: &RepoPath) -> Result<Option<TreeEntry>> {
    lookup(&ctx.repo, ctx.get::<RootTree>(id, &())?.0, path)
}
pub(crate) fn blob(ctx: &Ctx<'_>, id: ChangesetId, path: &RepoPath) -> Result<Option<BlobId>> {
    Ok(match path_entry(ctx, id, path)? {
        Some(TreeEntry::Leaf(entry)) => entry.blob(),
        _ => None,
    })
}
fn tree_dependencies(cs: &Changeset) -> Vec<DerivedRef> {
    std::iter::once(cs.id())
        .chain(cs.parents.iter().copied())
        .map(|id| DerivedRef::of::<RootTree>(id, &()))
        .collect()
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PathHistory {
    pub last: Option<ChangesetId>,
    pub previous: Option<ChangesetId>,
}
fn encode_id(id: Option<ChangesetId>, out: &mut Encoder) {
    match id {
        None => out.byte(0),
        Some(id) => {
            out.byte(1);
            id.encode_to(out);
        }
    }
}
fn decode_id(input: &mut Decoder<'_>) -> Result<Option<ChangesetId>> {
    match input.byte()? {
        0 => Ok(None),
        1 => Ok(Some(ChangesetId::decode_from(input)?)),
        _ => Err(invalid("invalid optional changeset")),
    }
}
impl Canonical for PathHistory {
    fn encode_to(&self, out: &mut Encoder) {
        encode_id(self.last, out);
        encode_id(self.previous, out);
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self {
            last: decode_id(input)?,
            previous: decode_id(input)?,
        })
    }
}
impl DerivedData for PathHistory {
    const NAME: &'static str = "PathHistory";
    const VERSION: u32 = 1;
    const MODE: DeriveMode = DeriveMode::Lazy;
    const PARENTS: ParentSelection = ParentSelection::FirstParent;
    type SubKey = RepoPath;
    fn dependencies(_: &Ctx<'_>, cs: &Changeset, _: &RepoPath) -> Result<Vec<DerivedRef>> {
        Ok(std::iter::once(cs.id())
            .chain(cs.parents.first().copied())
            .map(|id| DerivedRef::of::<RootTree>(id, &()))
            .collect())
    }
    fn derive(ctx: &Ctx<'_>, cs: &Changeset, path: &RepoPath, parents: &[Self]) -> Result<Self> {
        let previous = parents.first().cloned().unwrap_or_default();
        let old = cs
            .parents
            .first()
            .map(|id| path_entry(ctx, *id, path))
            .transpose()?
            .flatten();
        let new = path_entry(ctx, cs.id(), path)?;
        if old == new {
            Ok(previous)
        } else {
            Ok(Self {
                last: Some(cs.id()),
                previous: previous.last,
            })
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineOrigin {
    pub changeset: ChangesetId,
    pub path: RepoPath,
    pub line: u64,
}
impl Canonical for LineOrigin {
    fn encode_to(&self, out: &mut Encoder) {
        self.changeset.encode_to(out);
        self.path.encode_to(out);
        out.u64(self.line);
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self {
            changeset: ChangesetId::decode_from(input)?,
            path: RepoPath::decode_from(input)?,
            line: input.u64()?,
        })
    }
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Blame {
    pub blob: Option<BlobId>,
    pub lines: Vec<LineOrigin>,
}
impl Canonical for Blame {
    fn encode_to(&self, out: &mut Encoder) {
        match self.blob {
            None => out.byte(0),
            Some(blob) => {
                out.byte(1);
                blob.encode_to(out);
            }
        }
        out.len(self.lines.len());
        for origin in &self.lines {
            origin.encode_to(out);
        }
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        let blob = match input.byte()? {
            0 => None,
            1 => Some(BlobId::decode_from(input)?),
            _ => return Err(invalid("invalid blame blob")),
        };
        let mut lines = Vec::new();
        for _ in 0..input.count()? {
            lines.push(LineOrigin::decode_from(input)?);
        }
        Ok(Self { blob, lines })
    }
}
impl DerivedData for Blame {
    const NAME: &'static str = "Blame";
    const VERSION: u32 = 2;
    const MODE: DeriveMode = DeriveMode::Lazy;
    const PARENTS: ParentSelection = ParentSelection::None;
    type SubKey = RepoPath;
    fn dependencies(_: &Ctx<'_>, cs: &Changeset, path: &RepoPath) -> Result<Vec<DerivedRef>> {
        Ok(blame_dependencies(cs, path))
    }
    fn resolved_dependencies(
        ctx: &Ctx<'_>,
        cs: &Changeset,
        path: &RepoPath,
    ) -> Result<Vec<DerivedRef>> {
        Ok(blame_sources(ctx, cs, path, false)?
            .into_iter()
            .map(|(id, path)| DerivedRef::of::<Self>(id, &path))
            .collect())
    }
    fn derive(ctx: &Ctx<'_>, cs: &Changeset, path: &RepoPath, _: &[Self]) -> Result<Self> {
        let parents = blame_sources(ctx, cs, path, false)?
            .into_iter()
            .map(|(id, path)| ctx.get::<Self>(id, &path))
            .collect::<Result<Vec<_>>>()?;
        blame_lines(ctx, cs, path, &parents)
    }
}
fn blame_lines(ctx: &Ctx<'_>, cs: &Changeset, path: &RepoPath, parents: &[Blame]) -> Result<Blame> {
    let Some(blob) = blob(ctx, cs.id(), path)? else {
        return Ok(Blame::default());
    };
    if let Some(parent) = parents.iter().find(|p| p.blob == Some(blob)) {
        return Ok(parent.clone());
    }
    let current = ctx.repo.blob(blob)?;
    let mut result: Vec<_> = lines(&current)
        .iter()
        .enumerate()
        .map(|(n, _)| LineOrigin {
            changeset: cs.id(),
            path: path.clone(),
            line: n as u64 + 1,
        })
        .collect();
    let mut assigned = vec![false; result.len()];
    for parent in parents {
        let Some(old) = parent.blob else {
            continue;
        };
        let content = ctx.repo.blob(old)?;
        for (old, new) in matching_lines(&content, &current) {
            if !assigned[new] {
                result[new] = parent
                    .lines
                    .get(old)
                    .ok_or_else(|| invalid("invalid parent blame line count"))?
                    .clone();
                assigned[new] = true;
            }
        }
    }
    Ok(Blame {
        blob: Some(blob),
        lines: result,
    })
}

pub(crate) fn blame_dependencies(cs: &Changeset, path: &RepoPath) -> Vec<DerivedRef> {
    let mut deps = tree_dependencies(cs);
    deps.push(DerivedRef::of::<CopyDetect>(cs.id(), path));
    deps
}
pub(crate) fn blame_sources(
    ctx: &Ctx<'_>,
    cs: &Changeset,
    path: &RepoPath,
    copies: bool,
) -> Result<Vec<(ChangesetId, RepoPath)>> {
    let detected = ctx.get::<CopyDetect>(cs.id(), path)?;
    let mut out = Vec::new();
    for parent in &cs.parents {
        if blob(ctx, *parent, path)?.is_some() {
            out.push((*parent, path.clone()));
        } else if let Some(source) = detected
            .0
            .iter()
            .find(|source| source.parent == *parent && (source.rename || copies))
        {
            out.push((*parent, source.path.clone()));
        }
    }
    Ok(out)
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CopyBlame(pub Blame);
impl Canonical for CopyBlame {
    fn encode_to(&self, out: &mut Encoder) {
        self.0.encode_to(out);
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self(Blame::decode_from(input)?))
    }
}
