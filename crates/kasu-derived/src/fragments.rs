use crate::{
    history::{blame_dependencies, blame_sources, blob},
    *,
};
use kasu_core::*;
use std::collections::{BTreeMap, BTreeSet};

const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_LINES: usize = 100_000;
const MAX_SOURCES: usize = 1000;
const MAX_WORK: usize = 1_000_000;
const MOVE_SCORE: usize = 20;
const COPY_SCORE: usize = 40;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineMatchKey {
    pub path: RepoPath,
    pub copies: bool,
}
impl Canonical for LineMatchKey {
    fn encode_to(&self, out: &mut Encoder) {
        self.path.encode_to(out);
        out.byte(u8::from(self.copies));
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        let path = RepoPath::decode_from(input)?;
        let copies = match input.byte()? {
            0 => false,
            1 => true,
            _ => return Err(invalid("invalid line match mode")),
        };
        Ok(Self { path, copies })
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineTransfer {
    pub parent: ChangesetId,
    pub path: RepoPath,
    pub before: u64,
    pub after: u64,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LineMatches(pub Vec<LineTransfer>);
impl Canonical for LineMatches {
    fn encode_to(&self, out: &mut Encoder) {
        out.len(self.0.len());
        for line in &self.0 {
            line.parent.encode_to(out);
            line.path.encode_to(out);
            out.u64(line.before);
            out.u64(line.after);
        }
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        let mut lines = Vec::new();
        for _ in 0..input.count()? {
            lines.push(LineTransfer {
                parent: ChangesetId::decode_from(input)?,
                path: RepoPath::decode_from(input)?,
                before: input.u64()?,
                after: input.u64()?,
            });
        }
        if lines.windows(2).any(|w| w[0].after >= w[1].after) {
            return Err(invalid("unordered line transfers"));
        }
        Ok(Self(lines))
    }
}

struct Work(usize);
impl Work {
    fn step(&mut self) -> Result<()> {
        self.0 += 1;
        if self.0 > MAX_WORK {
            return Err(invalid("line fragment comparison limit exceeded"));
        }
        Ok(())
    }
}
struct Source {
    parent: ChangesetId,
    path: RepoPath,
    blob: BlobId,
}
struct Block {
    source: usize,
    before: usize,
    after: usize,
    len: usize,
    score: usize,
}

fn text(bytes: &[u8]) -> bool {
    bytes.len() <= MAX_BYTES && !bytes.contains(&0)
}
fn score(line: &[u8]) -> usize {
    line.iter().filter(|b| b.is_ascii_alphanumeric()).count()
}

fn blocks(
    old: &[u8],
    new: &[&[u8]],
    assigned: &[Option<LineTransfer>],
    source: usize,
    minimum: usize,
    work: &mut Work,
    out: &mut Vec<Block>,
) -> Result<()> {
    if !text(old) {
        return Ok(());
    }
    let old = lines(old);
    if old.len() > MAX_LINES {
        return Err(invalid("line fragment line limit exceeded"));
    }
    let mut positions = BTreeMap::<&[u8], Vec<usize>>::new();
    for (i, line) in old.iter().enumerate() {
        work.step()?;
        positions.entry(line).or_default().push(i);
    }
    for after in 0..new.len() {
        work.step()?;
        if assigned[after].is_some() {
            continue;
        }
        let Some(starts) = positions.get(new[after]) else {
            continue;
        };
        for before in starts {
            work.step()?;
            // Only maximal equal runs need to be extended. Without this guard
            // an unchanged block would cost quadratically many comparisons.
            if after > 0
                && *before > 0
                && assigned[after - 1].is_none()
                && old[before - 1] == new[after - 1]
            {
                continue;
            }
            let mut len = 0;
            let mut value = 0;
            while before + len < old.len()
                && after + len < new.len()
                && assigned[after + len].is_none()
                && old[before + len] == new[after + len]
            {
                work.step()?;
                value += score(new[after + len]);
                len += 1;
            }
            if value >= minimum {
                out.push(Block {
                    source,
                    before: *before,
                    after,
                    len,
                    score: value,
                });
            }
        }
    }
    Ok(())
}
fn apply_blocks(
    mut blocks: Vec<Block>,
    sources: &[Source],
    new: &[&[u8]],
    assigned: &mut [Option<LineTransfer>],
    minimum: usize,
) {
    blocks.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(b.len.cmp(&a.len))
            .then(a.source.cmp(&b.source))
            .then(a.before.cmp(&b.before))
            .then(a.after.cmp(&b.after))
    });
    for block in blocks {
        // Higher ranked blocks can split a candidate. Score the remaining
        // contiguous pieces separately instead of attributing short leftovers.
        let mut start = 0;
        while start < block.len {
            if assigned[block.after + start].is_some() {
                start += 1;
                continue;
            }
            let mut end = start;
            let mut value = 0;
            while end < block.len && assigned[block.after + end].is_none() {
                value += score(new[block.after + end]);
                end += 1;
            }
            if value >= minimum {
                let source = &sources[block.source];
                for i in start..end {
                    assigned[block.after + i] = Some(LineTransfer {
                        parent: source.parent,
                        path: source.path.clone(),
                        before: (block.before + i) as u64,
                        after: (block.after + i) as u64,
                    });
                }
            }
            start = end;
        }
    }
}

fn other_sources(ctx: &Ctx<'_>, cs: &Changeset, primary: &[Source]) -> Result<Vec<Source>> {
    let used: BTreeSet<_> = primary.iter().map(|s| (s.parent, s.path.clone())).collect();
    let mut result = Vec::new();
    for parent in &cs.parents {
        let mut stack = vec![(Vec::new(), ctx.get::<RootTree>(*parent, &())?.0)];
        while let Some((prefix, tree)) = stack.pop() {
            for (name, entry) in ctx.repo.tree(tree)?.entries {
                let mut path = prefix.clone();
                if !path.is_empty() {
                    path.push(b'/');
                }
                path.extend_from_slice(&name);
                match entry {
                    TreeEntry::SubTree(tree) => stack.push((path, tree)),
                    TreeEntry::Leaf(Entry::File { blob, .. }) => {
                        let path = RepoPath::new(path)?;
                        if !used.contains(&(*parent, path.clone())) {
                            result.push(Source {
                                parent: *parent,
                                path,
                                blob,
                            });
                            if result.len() > MAX_SOURCES {
                                return Err(invalid("line fragment source limit exceeded"));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    // Parent order, then path bytes, is the stable tie breaker.
    result.sort_by(|a, b| {
        cs.parents
            .iter()
            .position(|p| *p == a.parent)
            .cmp(&cs.parents.iter().position(|p| *p == b.parent))
            .then(a.path.cmp(&b.path))
    });
    Ok(result)
}

impl DerivedData for LineMatches {
    const NAME: &'static str = "LineMatches";
    const VERSION: u32 = 1;
    const MODE: DeriveMode = DeriveMode::Lazy;
    const PARENTS: ParentSelection = ParentSelection::None;
    type SubKey = LineMatchKey;
    fn dependencies(_: &Ctx<'_>, cs: &Changeset, key: &Self::SubKey) -> Result<Vec<DerivedRef>> {
        Ok(blame_dependencies(cs, &key.path))
    }
    fn derive(ctx: &Ctx<'_>, cs: &Changeset, key: &Self::SubKey, _: &[Self]) -> Result<Self> {
        if cs.parents.is_empty() {
            return Ok(Self::default());
        }
        let Some(current) = blob(ctx, cs.id(), &key.path)? else {
            return Ok(Self::default());
        };
        let mut sources = Vec::new();
        for (parent, path) in blame_sources(ctx, cs, &key.path, key.copies)? {
            if let Some(blob) = blob(ctx, parent, &path)? {
                sources.push(Source { parent, path, blob });
            }
        }
        let bytes = ctx.repo.blob(current)?;
        let new = lines(&bytes);
        if let Some(source) = sources.iter().find(|s| s.blob == current) {
            return Ok(Self(
                (0..new.len())
                    .map(|i| LineTransfer {
                        parent: source.parent,
                        path: source.path.clone(),
                        before: i as u64,
                        after: i as u64,
                    })
                    .collect(),
            ));
        }
        let mut assigned = vec![None; new.len()];
        for source in &sources {
            for (before, after) in matching_lines(&ctx.repo.blob(source.blob)?, &bytes) {
                if assigned[after].is_none() {
                    assigned[after] = Some(LineTransfer {
                        parent: source.parent,
                        path: source.path.clone(),
                        before: before as u64,
                        after: after as u64,
                    });
                }
            }
        }
        let regular = matches!(
            lookup(&ctx.repo, ctx.get::<RootTree>(cs.id(), &())?.0, &key.path)?,
            Some(TreeEntry::Leaf(Entry::File { .. }))
        );
        if regular && text(&bytes) && assigned.iter().any(Option::is_none) {
            if new.len() > MAX_LINES {
                return Err(invalid("line fragment line limit exceeded"));
            }
            let mut work = Work(0);
            let mut found = Vec::new();
            for (i, source) in sources.iter().enumerate() {
                if !matches!(
                    lookup(
                        &ctx.repo,
                        ctx.get::<RootTree>(source.parent, &())?.0,
                        &source.path
                    )?,
                    Some(TreeEntry::Leaf(Entry::File { .. }))
                ) {
                    continue;
                }
                blocks(
                    &ctx.repo.blob(source.blob)?,
                    &new,
                    &assigned,
                    i,
                    MOVE_SCORE,
                    &mut work,
                    &mut found,
                )?;
            }
            apply_blocks(found, &sources, &new, &mut assigned, MOVE_SCORE);
            if key.copies && assigned.iter().any(Option::is_none) {
                let candidates = other_sources(ctx, cs, &sources)?;
                let mut found = Vec::new();
                for (i, source) in candidates.iter().enumerate() {
                    if ctx.repo.blob_metadata(source.blob)?.size > MAX_BYTES as u64 {
                        continue;
                    }
                    blocks(
                        &ctx.repo.blob(source.blob)?,
                        &new,
                        &assigned,
                        i,
                        COPY_SCORE,
                        &mut work,
                        &mut found,
                    )?;
                }
                apply_blocks(found, &candidates, &new, &mut assigned, COPY_SCORE);
            }
        }
        Ok(Self(assigned.into_iter().flatten().collect()))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MovedBlame(pub Blame);
impl Canonical for MovedBlame {
    fn encode_to(&self, out: &mut Encoder) {
        self.0.encode_to(out);
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self(Blame::decode_from(input)?))
    }
}
fn match_key(path: &RepoPath, copies: bool) -> LineMatchKey {
    LineMatchKey {
        path: path.clone(),
        copies,
    }
}
fn origin_sources(matches: &LineMatches) -> BTreeSet<(ChangesetId, RepoPath)> {
    matches
        .0
        .iter()
        .map(|m| (m.parent, m.path.clone()))
        .collect()
}
fn inherit(
    ctx: &Ctx<'_>,
    cs: &Changeset,
    path: &RepoPath,
    matches: LineMatches,
    parents: BTreeMap<(ChangesetId, RepoPath), Blame>,
) -> Result<Blame> {
    let Some(blob) = blob(ctx, cs.id(), path)? else {
        return Ok(Blame::default());
    };
    let content = ctx.repo.blob(blob)?;
    let mut lines: Vec<_> = lines(&content)
        .iter()
        .enumerate()
        .map(|(n, _)| LineOrigin {
            changeset: cs.id(),
            path: path.clone(),
            line: n as u64 + 1,
        })
        .collect();
    for transfer in matches.0 {
        let origin = parents
            .get(&(transfer.parent, transfer.path))
            .and_then(|b| b.lines.get(transfer.before as usize))
            .ok_or_else(|| invalid("invalid fragment source line"))?
            .clone();
        *lines
            .get_mut(transfer.after as usize)
            .ok_or_else(|| invalid("invalid fragment destination line"))? = origin;
    }
    Ok(Blame {
        blob: Some(blob),
        lines,
    })
}
macro_rules! enhanced_blame {
    ($name:ident, $copies:literal, $version:literal) => {
        impl DerivedData for $name {
            const NAME: &'static str = stringify!($name);
            const VERSION: u32 = $version;
            const MODE: DeriveMode = DeriveMode::Lazy;
            const PARENTS: ParentSelection = ParentSelection::None;
            type SubKey = RepoPath;
            fn dependencies(
                _: &Ctx<'_>,
                cs: &Changeset,
                path: &RepoPath,
            ) -> Result<Vec<DerivedRef>> {
                Ok(vec![
                    DerivedRef::of::<RootTree>(cs.id(), &()),
                    DerivedRef::of::<LineMatches>(cs.id(), &match_key(path, $copies)),
                ])
            }
            fn resolved_dependencies(
                ctx: &Ctx<'_>,
                cs: &Changeset,
                path: &RepoPath,
            ) -> Result<Vec<DerivedRef>> {
                Ok(
                    origin_sources(&ctx.get::<LineMatches>(cs.id(), &match_key(path, $copies))?)
                        .into_iter()
                        .map(|(parent, path)| DerivedRef::of::<Self>(parent, &path))
                        .collect(),
                )
            }
            fn derive(ctx: &Ctx<'_>, cs: &Changeset, path: &RepoPath, _: &[Self]) -> Result<Self> {
                let matches = ctx.get::<LineMatches>(cs.id(), &match_key(path, $copies))?;
                let parents = origin_sources(&matches)
                    .into_iter()
                    .map(|(id, path)| Ok(((id, path.clone()), ctx.get::<Self>(id, &path)?.0)))
                    .collect::<Result<_>>()?;
                Ok(Self(inherit(ctx, cs, path, matches, parents)?))
            }
        }
    };
}
enhanced_blame!(MovedBlame, false, 1);
enhanced_blame!(CopyBlame, true, 2);
