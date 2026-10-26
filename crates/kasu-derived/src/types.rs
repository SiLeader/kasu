use crate::*;
use kasu_core::{
    Canonical, Changeset, ChangesetId, Decoder, Encoder, Result, Tree, TreeId, invalid,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootTree(pub TreeId);
impl Canonical for RootTree {
    fn encode_to(&self, out: &mut Encoder) {
        self.0.encode_to(out);
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self(TreeId::decode_from(input)?))
    }
}
impl DerivedData for RootTree {
    const NAME: &'static str = "RootTree";
    const VERSION: u32 = 1;
    const MODE: DeriveMode = DeriveMode::Eager;
    const PARENTS: ParentSelection = ParentSelection::FirstParent;
    type SubKey = ();
    fn eager_keys(_: &Ctx<'_>, _: &Changeset) -> Result<Vec<()>> {
        Ok(vec![()])
    }
    fn derive(ctx: &Ctx<'_>, cs: &Changeset, _: &(), parents: &[Self]) -> Result<Self> {
        let base = match parents.first() {
            Some(tree) => tree.0,
            None => ctx.repo.put_tree(&Tree::default())?,
        };
        Ok(Self(apply(&ctx.repo, base, &cs.changes)?))
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Generation(pub u64);
impl Canonical for Generation {
    fn encode_to(&self, out: &mut Encoder) {
        out.u64(self.0);
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self(input.u64()?))
    }
}
impl DerivedData for Generation {
    const NAME: &'static str = "Generation";
    const VERSION: u32 = 1;
    const MODE: DeriveMode = DeriveMode::Eager;
    const PARENTS: ParentSelection = ParentSelection::AllParents;
    type SubKey = ();
    fn eager_keys(_: &Ctx<'_>, _: &Changeset) -> Result<Vec<()>> {
        Ok(vec![()])
    }
    fn derive(_: &Ctx<'_>, _: &Changeset, _: &(), parents: &[Self]) -> Result<Self> {
        Ok(Self(
            parents
                .iter()
                .map(|p| p.0)
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or_else(|| invalid("generation overflow"))?,
        ))
    }
}

// Persisted version 1 remains readable/backfillable, but is no longer eager.
// jumps[k] is the 2^k-th first-parent ancestor (never used for all-parent ancestry).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FirstParentSkipV1(pub Vec<ChangesetId>);
impl Canonical for FirstParentSkipV1 {
    fn encode_to(&self, out: &mut Encoder) {
        out.len(self.0.len());
        for id in &self.0 {
            id.encode_to(out);
        }
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        let mut out = Vec::new();
        for _ in 0..input.count()? {
            out.push(ChangesetId::decode_from(input)?);
        }
        Ok(Self(out))
    }
}
impl DerivedData for FirstParentSkipV1 {
    const NAME: &'static str = "FirstParentSkip";
    const VERSION: u32 = 1;
    const MODE: DeriveMode = DeriveMode::Lazy;
    const PARENTS: ParentSelection = ParentSelection::FirstParent;
    type SubKey = ();
    fn eager_keys(_: &Ctx<'_>, _: &Changeset) -> Result<Vec<()>> {
        Ok(vec![()])
    }
    fn derive(ctx: &Ctx<'_>, cs: &Changeset, _: &(), _: &[Self]) -> Result<Self> {
        let Some(first) = cs.parents.first() else {
            return Ok(Self(Vec::new()));
        };
        let mut jumps = vec![*first];
        for level in 0..63 {
            let ancestor = ctx.get::<Self>(jumps[level], &())?;
            match ancestor.0.get(level) {
                Some(id) => jumps.push(*id),
                None => break,
            }
        }
        Ok(Self(jumps))
    }
}

// Version 2 stores one skew jump rather than a logarithmic-size ancestor table.
// Combining two equal spans needs only the parent and one existing jump record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FirstParentJump {
    pub id: ChangesetId,
    pub depth: u64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FirstParentSkip {
    pub depth: u64,
    pub parent: Option<ChangesetId>,
    pub jump: Option<FirstParentJump>,
}
impl Canonical for FirstParentSkip {
    fn encode_to(&self, out: &mut Encoder) {
        out.u64(self.depth);
        match self.parent {
            None => out.raw(&[0]),
            Some(id) => {
                out.raw(&[1]);
                id.encode_to(out);
            }
        }
        match self.jump {
            None => out.raw(&[0]),
            Some(j) => {
                out.raw(&[1]);
                out.u64(j.depth);
                j.id.encode_to(out);
            }
        }
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        let depth = input.u64()?;
        let parent = match input.raw(1)?[0] {
            0 => None,
            1 => Some(ChangesetId::decode_from(input)?),
            _ => return Err(invalid("invalid first-parent flag")),
        };
        let jump = match input.raw(1)?[0] {
            0 => None,
            1 => Some(FirstParentJump {
                depth: input.u64()?,
                id: ChangesetId::decode_from(input)?,
            }),
            _ => return Err(invalid("invalid first-parent jump flag")),
        };
        match (depth, parent, jump) {
            (0, None, None) => {}
            (d, Some(p), Some(j))
                if j.depth < d
                    && (j.id == p) == (j.depth == d - 1)
                    && ((d - j.depth) & (d - j.depth).wrapping_add(1)) == 0 => {}
            _ => return Err(invalid("invalid first-parent skip record")),
        }
        Ok(Self {
            depth,
            parent,
            jump,
        })
    }
}
impl DerivedData for FirstParentSkip {
    const NAME: &'static str = "FirstParentSkip";
    const VERSION: u32 = 2;
    const MODE: DeriveMode = DeriveMode::Eager;
    const PARENTS: ParentSelection = ParentSelection::FirstParent;
    type SubKey = ();
    fn eager_keys(_: &Ctx<'_>, _: &Changeset) -> Result<Vec<()>> {
        Ok(vec![()])
    }
    fn derive(ctx: &Ctx<'_>, cs: &Changeset, _: &(), parents: &[Self]) -> Result<Self> {
        let Some(first) = cs.parents.first().copied() else {
            return Ok(Self {
                depth: 0,
                parent: None,
                jump: None,
            });
        };
        let parent = parents
            .first()
            .ok_or_else(|| invalid("missing first-parent skip"))?;
        let depth = parent
            .depth
            .checked_add(1)
            .ok_or_else(|| invalid("first-parent depth overflow"))?;
        let mut jump = FirstParentJump {
            id: first,
            depth: parent.depth,
        };
        if let Some(previous) = parent.jump {
            let target = ctx.get::<Self>(previous.id, &())?;
            if target.depth != previous.depth {
                return Err(invalid("first-parent jump depth mismatch"));
            }
            if let Some(next) = target.jump
                && parent.depth - previous.depth == target.depth - next.depth
            {
                jump = next;
            }
        }
        Ok(Self {
            depth,
            parent: Some(first),
            jump: Some(jump),
        })
    }
}
