use crate::Operations;
use kasu_core::*;
use kasu_derived::*;

#[derive(Clone, Debug)]
pub struct LogPage {
    pub changesets: Vec<ChangesetId>,
    pub cursor: Vec<u8>,
}
#[derive(Clone, Debug)]
pub struct FileDiff {
    pub path: RepoPath,
    pub before: Option<Entry>,
    pub after: Option<Entry>,
    pub edits: Vec<ContentEdit>,
    pub binary: bool,
}
#[derive(Clone, Debug)]
pub struct ContentEdit {
    pub before_start: u64,
    pub after_start: u64,
    pub before: Vec<Vec<u8>>,
    pub after: Vec<Vec<u8>>,
}
impl Operations<'_> {
    pub fn root_tree(&self, id: ChangesetId) -> Result<TreeId> {
        Ok(self.engine.derive::<RootTree>(&self.ctx, id, &())?.0)
    }
    pub fn read_entry(&self, id: ChangesetId, path: &RepoPath) -> Result<Option<TreeEntry>> {
        lookup(&self.ctx.repo, self.root_tree(id)?, path)
    }
    pub fn read_file(&self, id: ChangesetId, path: &RepoPath) -> Result<(Entry, Vec<u8>)> {
        let Some(TreeEntry::Leaf(entry)) = self.read_entry(id, path)? else {
            return Err(Error::NotFound("file".into()));
        };
        let blob = entry
            .blob()
            .ok_or_else(|| invalid("gitlink has no file content"))?;
        Ok((entry, self.ctx.repo.blob(blob)?))
    }
    pub fn list_dir(&self, id: ChangesetId, path: Option<&RepoPath>) -> Result<Tree> {
        let tree = if let Some(path) = path {
            match self.read_entry(id, path)? {
                Some(TreeEntry::SubTree(id)) => id,
                Some(_) => return Err(invalid("path is not a directory")),
                None => return Err(Error::NotFound("directory".into())),
            }
        } else {
            self.root_tree(id)?
        };
        self.ctx.repo.tree(tree)
    }
    pub fn is_first_parent_ancestor(
        &self,
        ancestor: ChangesetId,
        descendant: ChangesetId,
    ) -> Result<bool> {
        let floor = self
            .engine
            .derive::<FirstParentSkip>(&self.ctx, ancestor, &())?
            .depth;
        let mut current = descendant;
        loop {
            let record = self
                .engine
                .derive::<FirstParentSkip>(&self.ctx, current, &())?;
            if record.depth <= floor {
                return Ok(record.depth == floor && current == ancestor);
            }
            current = match record.jump {
                Some(jump) if jump.depth >= floor => jump.id,
                _ => record
                    .parent
                    .ok_or_else(|| invalid("missing first-parent link"))?,
            };
        }
    }

    pub fn log(
        &self,
        start: ChangesetId,
        path: Option<&RepoPath>,
        limit: usize,
        cursor: &[u8],
    ) -> Result<LogPage> {
        if limit == 0 || limit > 1000 {
            return Err(invalid("log limit must be 1..1000"));
        }
        self.ctx.repo.changeset(start)?;
        let mut binding = Encoder::default();
        binding.raw(b"kasu.log\0\x01");
        start.encode_to(&mut binding);
        match path {
            None => binding.byte(0),
            Some(path) => {
                binding.byte(1);
                path.encode_to(&mut binding);
            }
        }
        let mut next = if cursor.is_empty() {
            match path {
                None => Some(start),
                Some(path) => {
                    self.engine
                        .derive::<PathHistory>(&self.ctx, start, path)?
                        .last
                }
            }
        } else {
            if !cursor.starts_with(&binding.0) {
                return Err(invalid("log cursor belongs to another query"));
            }
            let next = ChangesetId::decode(&cursor[binding.0.len()..])?;
            if !self.is_first_parent_ancestor(next, start)? {
                return Err(invalid("log cursor is outside the first-parent chain"));
            }
            if let Some(path) = path
                && self
                    .engine
                    .derive::<PathHistory>(&self.ctx, next, path)?
                    .last
                    != Some(next)
            {
                return Err(invalid("path log cursor does not identify a path change"));
            }
            Some(next)
        };
        let mut changesets = Vec::new();
        while changesets.len() < limit {
            let Some(id) = next else {
                break;
            };
            changesets.push(id);
            next = match path {
                None => self.ctx.repo.changeset(id)?.parents.first().copied(),
                Some(path) => {
                    self.engine
                        .derive::<PathHistory>(&self.ctx, id, path)?
                        .previous
                }
            };
        }
        let cursor = if let Some(next) = next {
            next.encode_to(&mut binding);
            binding.0
        } else {
            Vec::new()
        };
        Ok(LogPage { changesets, cursor })
    }
    pub fn diff(
        &self,
        base: ChangesetId,
        head: ChangesetId,
        paths: &[RepoPath],
        content: bool,
    ) -> Result<Vec<FileDiff>> {
        self.diff_with_exact_paths(base, head, paths, content, false)
    }
    pub fn diff_with_exact_paths(
        &self,
        base: ChangesetId,
        head: ChangesetId,
        paths: &[RepoPath],
        content: bool,
        exact_paths: bool,
    ) -> Result<Vec<FileDiff>> {
        let old = self.root_tree(base)?;
        let new = self.root_tree(head)?;
        let mut result = Vec::new();
        for change in diff(&self.ctx.repo, old, new)? {
            let path = change.path();
            if !paths.is_empty()
                && !paths.iter().any(|prefix| {
                    path == prefix
                        || (!exact_paths
                            && path.as_bytes().starts_with(prefix.as_bytes())
                            && path.as_bytes().get(prefix.as_bytes().len()) == Some(&b'/'))
                })
            {
                continue;
            }
            let before = match lookup(&self.ctx.repo, old, path)? {
                Some(TreeEntry::Leaf(entry)) => Some(entry),
                _ => None,
            };
            let after = match lookup(&self.ctx.repo, new, path)? {
                Some(TreeEntry::Leaf(entry)) => Some(entry),
                _ => None,
            };
            let mut edits = Vec::new();
            let mut binary = false;
            if content {
                let a = before
                    .as_ref()
                    .and_then(Entry::blob)
                    .map(|id| self.ctx.repo.blob(id))
                    .transpose()?
                    .unwrap_or_default();
                let b = after
                    .as_ref()
                    .and_then(Entry::blob)
                    .map(|id| self.ctx.repo.blob(id))
                    .transpose()?
                    .unwrap_or_default();
                binary = a.contains(&0) || b.contains(&0);
                if !binary {
                    let al = lines(&a);
                    let bl = lines(&b);
                    for edit in line_edits(&a, &b) {
                        edits.push(ContentEdit {
                            before_start: edit.before.start as u64,
                            after_start: edit.after.start as u64,
                            before: al[edit.before].iter().map(|b| b.to_vec()).collect(),
                            after: bl[edit.after].iter().map(|b| b.to_vec()).collect(),
                        });
                    }
                }
            }
            result.push(FileDiff {
                path: path.clone(),
                before,
                after,
                edits,
                binary,
            });
        }
        Ok(result)
    }
}
