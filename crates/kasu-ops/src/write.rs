use crate::{Operations, TreeConflict, merge_trees};
use kasu_core::{BString, Changeset, ChangesetId, Result, Signature, Tree, TreeId};
use kasu_derived::diff;

#[derive(Clone, Debug)]
pub struct CommitMetadata {
    pub author: Signature,
    pub committer: Signature,
    pub message: BString,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictStage {
    MergeBases,
    Result,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RewriteResult {
    Created {
        id: ChangesetId,
        bases: Vec<ChangesetId>,
    },
    Conflicted {
        stage: ConflictStage,
        bases: Vec<ChangesetId>,
        conflicts: Vec<TreeConflict>,
    },
}

enum BaseTask {
    Resolve(ChangesetId, ChangesetId),
    Fold {
        bases: Vec<ChangesetId>,
        position: usize,
        accumulated: ChangesetId,
    },
}

impl Operations<'_> {
    fn commit_tree(
        &self,
        tree: TreeId,
        parents: Vec<ChangesetId>,
        meta: CommitMetadata,
    ) -> Result<ChangesetId> {
        let base = match parents.first() {
            Some(id) => self.root_tree(*id)?,
            None => self.ctx.repo.put_tree(&Tree::default())?,
        };
        self.create_changeset(&Changeset {
            parents,
            author: meta.author,
            committer: meta.committer,
            message: meta.message,
            changes: diff(&self.ctx.repo, base, tree)?,
            git_provenance: None,
        })
    }

    // Reduce all best bases through deterministic virtual merges. Explicit
    // frames avoid consuming the call stack on nested criss-cross histories.
    fn combined_base(
        &self,
        a: ChangesetId,
        b: ChangesetId,
    ) -> Result<std::result::Result<TreeId, Vec<TreeConflict>>> {
        let mut tasks = vec![BaseTask::Resolve(a, b)];
        let mut results = Vec::new();
        while let Some(task) = tasks.pop() {
            match task {
                BaseTask::Resolve(a, b) => {
                    let bases = self.merge_bases(a, b)?;
                    match bases.as_slice() {
                        [] => results.push(self.ctx.repo.put_tree(&Tree::default())?),
                        [id] => results.push(self.root_tree(*id)?),
                        [first, second, ..] => {
                            let pair = (*first, *second);
                            tasks.push(BaseTask::Fold {
                                bases,
                                position: 1,
                                accumulated: pair.0,
                            });
                            tasks.push(BaseTask::Resolve(pair.0, pair.1));
                        }
                    }
                }
                BaseTask::Fold {
                    bases,
                    position,
                    accumulated,
                } => {
                    let base = results.pop().expect("base task returns one tree");
                    let next = bases[position];
                    let merged = merge_trees(
                        &self.ctx.repo,
                        base,
                        self.root_tree(accumulated)?,
                        self.root_tree(next)?,
                    )?;
                    let Some(tree) = merged.tree else {
                        return Ok(Err(merged.conflicts));
                    };
                    if position + 1 == bases.len() {
                        results.push(tree);
                    } else {
                        let sig = Signature {
                            name: b"kasu virtual merge".as_slice().into(),
                            email: b"virtual@kasu.invalid".as_slice().into(),
                            timestamp: 0,
                            tz: b"+0000".as_slice().into(),
                        };
                        let accumulated = self.commit_tree(
                            tree,
                            vec![accumulated, next],
                            CommitMetadata {
                                author: sig.clone(),
                                committer: sig,
                                message: b"kasu virtual merge base v1".as_slice().into(),
                            },
                        )?;
                        let next = bases[position + 1];
                        tasks.push(BaseTask::Fold {
                            bases,
                            position: position + 1,
                            accumulated,
                        });
                        tasks.push(BaseTask::Resolve(accumulated, next));
                    }
                }
            }
        }
        Ok(Ok(results.pop().expect("root task returns one tree")))
    }

    pub fn merge(
        &self,
        a: ChangesetId,
        b: ChangesetId,
        meta: CommitMetadata,
    ) -> Result<RewriteResult> {
        let bases = self.merge_bases(a, b)?;
        if self.is_ancestor(a, b)? {
            return Ok(RewriteResult::Created { id: b, bases });
        }
        if self.is_ancestor(b, a)? {
            return Ok(RewriteResult::Created { id: a, bases });
        }
        let base = match self.combined_base(a, b)? {
            Ok(base) => base,
            Err(conflicts) => {
                return Ok(RewriteResult::Conflicted {
                    stage: ConflictStage::MergeBases,
                    bases,
                    conflicts,
                });
            }
        };
        let merged = merge_trees(&self.ctx.repo, base, self.root_tree(a)?, self.root_tree(b)?)?;
        match merged.tree {
            Some(tree) => Ok(RewriteResult::Created {
                id: self.commit_tree(tree, vec![a, b], meta)?,
                bases,
            }),
            None => Ok(RewriteResult::Conflicted {
                stage: ConflictStage::Result,
                bases,
                conflicts: merged.conflicts,
            }),
        }
    }

    /// Replay one changeset's first-parent delta onto `onto`, preserving author
    /// and message. Merge changesets are flattened to a single parent.
    pub fn rebase(
        &self,
        id: ChangesetId,
        onto: ChangesetId,
        committer: Signature,
    ) -> Result<RewriteResult> {
        let original = self.ctx.repo.changeset(id)?;
        let bases: Vec<_> = original.parents.first().copied().into_iter().collect();
        let base = match bases.first() {
            Some(id) => self.root_tree(*id)?,
            None => self.ctx.repo.put_tree(&Tree::default())?,
        };
        let merged = merge_trees(
            &self.ctx.repo,
            base,
            self.root_tree(onto)?,
            self.root_tree(id)?,
        )?;
        match merged.tree {
            Some(tree) => Ok(RewriteResult::Created {
                id: self.commit_tree(
                    tree,
                    vec![onto],
                    CommitMetadata {
                        author: original.author,
                        committer,
                        message: original.message,
                    },
                )?,
                bases,
            }),
            None => Ok(RewriteResult::Conflicted {
                stage: ConflictStage::Result,
                bases,
                conflicts: merged.conflicts,
            }),
        }
    }
}
