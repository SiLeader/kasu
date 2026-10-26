use kasu_core::{Changeset, ChangesetId, Result, Tree, invalid};
use kasu_derived::{Ctx, Engine, Generation, RootTree, apply};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

pub struct Operations<'a> {
    pub engine: &'a Engine,
    pub ctx: Ctx<'a>,
}
impl Operations<'_> {
    pub fn create_changeset(&self, cs: &Changeset) -> Result<ChangesetId> {
        cs.validate()?;
        for parent in &cs.parents {
            self.ctx.repo.changeset(*parent)?;
        }
        for change in &cs.changes {
            if let kasu_core::FileChange::Set { entry, .. } = change
                && let Some(blob) = entry.blob()
            {
                self.ctx.repo.blob(blob)?;
            }
        }
        if cs.git_provenance.is_some() {
            // Native writes cannot assert Git provenance: ingest validates raw Git
            // objects and uses the store directly after connectivity verification.
            return Err(invalid("Git provenance is reserved for validated ingest"));
        }
        let base = match cs.parents.first() {
            Some(parent) => self.engine.derive::<RootTree>(&self.ctx, *parent, &())?.0,
            None => self.ctx.repo.put_tree(&Tree::default())?,
        };
        let root = apply(&self.ctx.repo, base, &cs.changes)?;
        let id = self.ctx.repo.put_changeset(cs)?;
        // Persist the already validated root without repeating tree edits.
        use kasu_core::Canonical;
        self.ctx.repo.meta.put_if_absent(
            &kasu_derived::DerivedRef::of::<RootTree>(id, &()).storage_key(),
            &RootTree(root).encode(),
        )?;
        self.engine.eager(&self.ctx, id)?;
        Ok(id)
    }
    pub fn is_ancestor(&self, ancestor: ChangesetId, descendant: ChangesetId) -> Result<bool> {
        let floor = self
            .engine
            .derive::<Generation>(&self.ctx, ancestor, &())?
            .0;
        let mut pending = vec![descendant];
        let mut seen = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            if id == ancestor {
                return Ok(true);
            }
            if self.engine.derive::<Generation>(&self.ctx, id, &())?.0 <= floor {
                continue;
            }
            pending.extend(self.ctx.repo.changeset(id)?.parents);
        }
        Ok(false)
    }
    /// Ancestors of `include` (inclusive) that are not ancestors of `exclude`.
    ///
    /// Walks in descending generation order so every child is painted before
    /// its parents, and stops as soon as the frontier is entirely excluded,
    /// so the cost is proportional to the new history rather than all of it.
    pub fn ancestors_excluding(
        &self,
        include: ChangesetId,
        exclude: Option<ChangesetId>,
    ) -> Result<BTreeSet<ChangesetId>> {
        const NEW: u8 = 1;
        const OLD: u8 = 2;
        let mut paint = BTreeMap::<ChangesetId, u8>::new();
        let mut pending = BinaryHeap::new();
        let mut interesting = 0usize;
        for (id, color) in [(Some(include), NEW), (exclude, OLD)] {
            let Some(id) = id else { continue };
            let previous = paint.entry(id).or_default();
            if *previous == 0 {
                pending.push((self.engine.derive::<Generation>(&self.ctx, id, &())?.0, id));
            }
            if *previous == NEW && color == OLD {
                interesting -= 1;
            } else if *previous == 0 && color == NEW {
                interesting += 1;
            }
            *previous |= color;
        }
        let mut result = BTreeSet::new();
        while interesting > 0 {
            let (_, id) = pending.pop().expect("interesting ancestor frontier");
            let color = paint[&id];
            if color == NEW {
                interesting -= 1;
                result.insert(id);
            }
            for parent in self.ctx.repo.changeset(id)?.parents {
                let previous = paint.entry(parent).or_default();
                let before = *previous;
                *previous |= color;
                if before == 0 {
                    if color == NEW {
                        interesting += 1;
                    }
                    pending.push((
                        self.engine.derive::<Generation>(&self.ctx, parent, &())?.0,
                        parent,
                    ));
                } else if before == NEW && *previous & OLD != 0 {
                    interesting -= 1;
                }
            }
        }
        Ok(result)
    }
    pub fn merge_bases(&self, a: ChangesetId, b: ChangesetId) -> Result<Vec<ChangesetId>> {
        // Descending generation order ensures every child is processed before
        // its parents. Bits 1/2 mark reachability from each tip; bit 4 marks an
        // ancestor of an already found common ancestor, which cannot be best.
        let mut paint = BTreeMap::<ChangesetId, u8>::new();
        let mut pending = BinaryHeap::new();
        let mut active = BTreeSet::new();
        for (id, color) in [(a, 1), (b, 2)] {
            *paint.entry(id).or_default() |= color;
            pending.push((self.engine.derive::<Generation>(&self.ctx, id, &())?.0, id));
            active.insert(id);
        }
        let mut processed = BTreeSet::new();
        let mut best = Vec::new();
        // Stale nodes must still propagate while another frontier can discover
        // a best base. Stop only when every remaining node is already dominated.
        while !active.is_empty() {
            let (_, id) = pending.pop().expect("active merge-base frontier");
            active.remove(&id);
            if !processed.insert(id) {
                continue;
            }
            let color = paint[&id];
            let common = color & 3 == 3;
            if common && color & 4 == 0 {
                best.push(id);
            }
            let propagated = color | if common { 4 } else { 0 };
            for parent in self.ctx.repo.changeset(id)?.parents {
                let previous = paint.entry(parent).or_default();
                if *previous | propagated != *previous {
                    *previous |= propagated;
                    if *previous & 4 == 0 {
                        active.insert(parent);
                    } else {
                        active.remove(&parent);
                    }
                    pending.push((
                        self.engine.derive::<Generation>(&self.ctx, parent, &())?.0,
                        parent,
                    ));
                }
            }
        }
        best.sort();
        Ok(best)
    }
}

mod metadata;
pub use metadata::*;

mod merge;
pub use merge::*;

mod write;
pub use write::*;
