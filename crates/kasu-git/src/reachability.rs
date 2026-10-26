use crate::{GitIndex, upload::object_links};
use kasu_core::*;
use std::collections::BTreeSet;

// Immutable radix sets share unchanged ranges between trees, commits and merges.
// The shape depends only on the set, never on insertion order or DAG topology.
const LEAF_SIZE: usize = 64;
const NODE_CACHE_SIZE: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Root {
    id: ObjectId,
    count: u64,
}
impl Canonical for Root {
    fn encode_to(&self, out: &mut Encoder) {
        self.id.encode_to(out);
        out.u64(self.count);
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        let root = Self {
            id: ObjectId::decode_from(input)?,
            count: input.u64()?,
        };
        if root.count == 0 {
            return Err(invalid("empty Git reachability root"));
        }
        Ok(root)
    }
}

#[derive(Clone)]
enum Node {
    Leaf(Vec<GitOid>),
    Branch(Box<[Option<Root>; 16]>),
}
impl Canonical for Node {
    fn encode_to(&self, out: &mut Encoder) {
        match self {
            Self::Leaf(ids) => {
                out.byte(0);
                out.len(ids.len());
                for id in ids {
                    id.encode_to(out);
                }
            }
            Self::Branch(children) => {
                out.byte(1);
                for child in children.iter() {
                    out.byte(u8::from(child.is_some()));
                    if let Some(child) = child {
                        child.encode_to(out);
                    }
                }
            }
        }
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        match input.byte()? {
            0 => {
                let len = input.count()?;
                if len == 0 || len > LEAF_SIZE {
                    return Err(invalid("invalid Git reachability leaf"));
                }
                let ids = (0..len)
                    .map(|_| GitOid::decode_from(input))
                    .collect::<Result<Vec<_>>>()?;
                if ids.windows(2).any(|w| w[0] >= w[1]) {
                    return Err(invalid("unsorted Git reachability leaf"));
                }
                Ok(Self::Leaf(ids))
            }
            1 => {
                let mut children = [None; 16];
                for child in &mut children {
                    *child = match input.byte()? {
                        0 => None,
                        1 => Some(Root::decode_from(input)?),
                        _ => return Err(invalid("invalid Git reachability child")),
                    };
                }
                let count = children
                    .iter()
                    .flatten()
                    .try_fold(0u64, |sum, r| sum.checked_add(r.count))
                    .ok_or_else(|| invalid("Git reachability count overflow"))?;
                if count <= LEAF_SIZE as u64 {
                    return Err(invalid("noncanonical Git reachability branch"));
                }
                Ok(Self::Branch(Box::new(children)))
            }
            _ => Err(invalid("unknown Git reachability node")),
        }
    }
}

fn digit(id: GitOid, depth: usize) -> Result<usize> {
    if depth >= 40 {
        return Err(invalid("Git reachability trie exceeds OID length"));
    }
    Ok(if depth.is_multiple_of(2) {
        id.0[depth / 2] >> 4
    } else {
        id.0[depth / 2] & 15
    } as usize)
}

struct Sets<'a, 'b> {
    index: &'a GitIndex<'b>,
    nodes: hashlink::LinkedHashMap<ObjectId, Node>,
}
impl<'a, 'b> Sets<'a, 'b> {
    fn new(index: &'a GitIndex<'b>) -> Self {
        Self {
            index,
            nodes: Default::default(),
        }
    }
    fn node_key(id: ObjectId) -> Vec<u8> {
        format!("git/reachability-node/v1/{id}").into_bytes()
    }
    fn root_key(&self, oid: GitOid) -> Vec<u8> {
        format!(
            "git/reachability/v1/{}/{oid}",
            self.index.projection.namespace()
        )
        .into_bytes()
    }
    fn cache(&mut self, id: ObjectId, node: Node) {
        self.nodes.insert(id, node);
        if self.nodes.len() > NODE_CACHE_SIZE {
            self.nodes.pop_front();
        }
    }
    fn load(&mut self, root: Root) -> Result<Node> {
        if let Some(node) = self.nodes.to_back(&root.id) {
            return Ok(node.clone());
        }
        let raw = self
            .index
            .repo
            .meta
            .get(&Self::node_key(root.id))?
            .ok_or_else(|| Error::NotFound(format!("Git reachability node {}", root.id)))?;
        if ObjectId::hash(&raw) != root.id {
            return Err(invalid("Git reachability node hash mismatch"));
        }
        let node = Node::decode(&raw)?;
        let count = match &node {
            Node::Leaf(ids) => ids.len() as u64,
            Node::Branch(children) => children.iter().flatten().map(|r| r.count).sum(),
        };
        if count != root.count {
            return Err(invalid("Git reachability node count mismatch"));
        }
        self.cache(root.id, node.clone());
        Ok(node)
    }
    fn store(&mut self, node: Node) -> Result<Root> {
        let count = match &node {
            Node::Leaf(ids) => ids.len() as u64,
            Node::Branch(children) => children.iter().flatten().map(|r| r.count).sum(),
        };
        let raw = node.encode();
        let id = ObjectId::hash(&raw);
        self.index
            .repo
            .meta
            .put_if_absent(&Self::node_key(id), &raw)?;
        self.cache(id, node);
        Ok(Root { id, count })
    }
    fn build(&mut self, ids: &[GitOid], depth: usize) -> Result<Option<Root>> {
        if ids.is_empty() {
            return Ok(None);
        }
        if ids.len() <= LEAF_SIZE {
            return self.store(Node::Leaf(ids.to_vec())).map(Some);
        }
        let mut children = [None; 16];
        let mut start = 0;
        while start < ids.len() {
            let bucket = digit(ids[start], depth)?;
            let mut end = start + 1;
            while end < ids.len() && digit(ids[end], depth)? == bucket {
                end += 1;
            }
            children[bucket] = self.build(&ids[start..end], depth + 1)?;
            start = end;
        }
        self.store(Node::Branch(Box::new(children))).map(Some)
    }
    fn children(&mut self, node: Node, depth: usize) -> Result<[Option<Root>; 16]> {
        match node {
            Node::Branch(children) => Ok(*children),
            Node::Leaf(ids) => {
                let mut children = [None; 16];
                if depth >= 40 {
                    return Err(invalid("Git reachability trie exceeds OID length"));
                }
                for (bucket, child) in children.iter_mut().enumerate() {
                    let group: Vec<_> = ids
                        .iter()
                        .copied()
                        .filter(|id| digit(*id, depth).ok() == Some(bucket))
                        .collect();
                    *child = self.build(&group, depth + 1)?;
                }
                Ok(children)
            }
        }
    }
    fn union(
        &mut self,
        left: Option<Root>,
        right: Option<Root>,
        depth: usize,
    ) -> Result<Option<Root>> {
        if left == right || right.is_none() {
            return Ok(left);
        }
        let Some(left) = left else {
            return Ok(right);
        };
        let right = right.unwrap();
        let a = self.load(left)?;
        let b = self.load(right)?;
        if let (Node::Leaf(a), Node::Leaf(b)) = (&a, &b) {
            let ids: BTreeSet<_> = a.iter().chain(b).copied().collect();
            return self.build(&ids.into_iter().collect::<Vec<_>>(), depth);
        }
        if depth >= 40 {
            return Err(invalid("Git reachability trie exceeds OID length"));
        }
        let mut a = self.children(a, depth)?;
        let b = self.children(b, depth)?;
        for i in 0..16 {
            a[i] = self.union(a[i], b[i], depth + 1)?;
        }
        self.store(Node::Branch(Box::new(a))).map(Some)
    }
    fn contains(&mut self, mut root: Option<Root>, oid: GitOid, mut depth: usize) -> Result<bool> {
        while let Some(current) = root {
            match self.load(current)? {
                Node::Leaf(ids) => return Ok(ids.binary_search(&oid).is_ok()),
                Node::Branch(children) => {
                    root = children[digit(oid, depth)?];
                    depth += 1;
                }
            }
        }
        Ok(false)
    }
    fn collect(&mut self, root: Root, out: &mut BTreeSet<GitOid>) -> Result<()> {
        let mut stack = vec![root];
        while let Some(root) = stack.pop() {
            match self.load(root)? {
                Node::Leaf(ids) => out.extend(ids),
                Node::Branch(children) => stack.extend((*children).into_iter().flatten()),
            }
        }
        Ok(())
    }
    fn difference(
        &mut self,
        left: Option<Root>,
        rights: &[Root],
        depth: usize,
        out: &mut BTreeSet<GitOid>,
    ) -> Result<()> {
        let Some(left) = left else {
            return Ok(());
        };
        if rights.contains(&left) {
            return Ok(());
        }
        if rights.is_empty() {
            return self.collect(left, out);
        }
        match self.load(left)? {
            Node::Leaf(ids) => {
                for id in ids {
                    let mut present = false;
                    for right in rights {
                        if self.contains(Some(*right), id, depth)? {
                            present = true;
                            break;
                        }
                    }
                    if !present {
                        out.insert(id);
                    }
                }
            }
            Node::Branch(children) => {
                if depth >= 40 {
                    return Err(invalid("Git reachability trie exceeds OID length"));
                }
                let mut excluded: [Vec<Root>; 16] = Default::default();
                for right in rights {
                    match self.load(*right)? {
                        Node::Branch(branches) => {
                            for (i, child) in branches.iter().enumerate() {
                                if let Some(child) = child {
                                    excluded[i].push(*child);
                                }
                            }
                        }
                        // A leaf can be passed to every child: binary search
                        // also rejects IDs belonging to another prefix. This
                        // avoids persisting temporary unions for arbitrary
                        // combinations of client have/want roots.
                        Node::Leaf(_) => {
                            for child in &mut excluded {
                                child.push(*right);
                            }
                        }
                    }
                }
                for i in 0..16 {
                    self.difference(children[i], &excluded[i], depth + 1, out)?;
                }
            }
        }
        Ok(())
    }
    fn cached(&self, oid: GitOid) -> Result<Option<Root>> {
        self.index
            .repo
            .meta
            .get(&self.root_key(oid))?
            .map(|raw| Root::decode(&raw))
            .transpose()
    }
    fn closure(&mut self, oid: GitOid) -> Result<Root> {
        let mut stack = vec![(oid, None)];
        let mut active = BTreeSet::new();
        while let Some((id, dependencies)) = stack.pop() {
            if self.cached(id)?.is_some() {
                continue;
            }
            if let Some((dependencies, seeds)) = dependencies {
                let mut root = None;
                for child in dependencies {
                    let child = self
                        .cached(child)?
                        .ok_or_else(|| invalid("missing Git reachability dependency"))?;
                    root = self.union(root, Some(child), 0)?;
                }
                // A known tree implies all of its descendants are already in
                // the parent closure. Inspect only changed Merkle paths, not
                // every file in the current snapshot at every commit.
                let mut added = BTreeSet::from([id]);
                let mut pending: Vec<GitOid> = seeds;
                while let Some(object) = pending.pop() {
                    if self.contains(root, object, 0)? || !added.insert(object) {
                        continue;
                    }
                    pending.extend(object_links(self.index, object, true)?);
                }
                let added = self.build(&added.into_iter().collect::<Vec<_>>(), 0)?;
                let root = self.union(root, added, 0)?.unwrap();
                self.index
                    .repo
                    .meta
                    .put_if_absent(&self.root_key(id), &root.encode())?;
                active.remove(&id);
            } else {
                if !active.insert(id) {
                    return Err(invalid("Git reachability dependency cycle"));
                }
                let kind = self.index.require(id)?.native.kind();
                let mut links = object_links(self.index, id, true)?;
                let (dependencies, seeds) = match kind {
                    GitKind::Commit => (links.split_off(1), links),
                    GitKind::Tag => (links, Vec::new()),
                    _ => (Vec::new(), links),
                };
                stack.push((id, Some((dependencies.clone(), seeds))));
                stack.extend(dependencies.into_iter().rev().map(|id| (id, None)));
            }
        }
        self.cached(oid)?
            .ok_or_else(|| invalid("missing Git reachability result"))
    }
}

pub(crate) fn reachable_difference(
    index: &GitIndex<'_>,
    wants: &[GitOid],
    haves: &[GitOid],
) -> Result<BTreeSet<GitOid>> {
    let mut sets = Sets::new(index);
    let mut wanted = Vec::new();
    for oid in wants {
        wanted.push(sets.closure(*oid)?);
    }
    let mut common = Vec::new();
    for oid in haves {
        common.push(sets.closure(*oid)?);
    }
    let mut result = BTreeSet::new();
    for want in wanted {
        sets.difference(Some(want), &common, 0, &mut result)?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProjectionId;
    use kasu_store::{FsBlobStore, Repository, SqliteStore};
    use proptest::prelude::*;

    fn verify(a: BTreeSet<GitOid>, b: BTreeSet<GitOid>) {
        let dir = tempfile::tempdir().unwrap();
        let meta = SqliteStore::memory().unwrap();
        let blobs = FsBlobStore::open(dir.path()).unwrap();
        let index = GitIndex {
            repo: Repository {
                meta: &meta,
                blobs: &blobs,
            },
            projection: ProjectionId::full(b"sets".as_slice()),
        };
        let mut sets = Sets::new(&index);
        let left = sets
            .build(&a.iter().copied().collect::<Vec<_>>(), 0)
            .unwrap();
        let right = sets
            .build(&b.iter().copied().collect::<Vec<_>>(), 0)
            .unwrap();
        let union = sets.union(left, right, 0).unwrap();
        let all: Vec<_> = a.union(&b).copied().collect();
        assert_eq!(
            union,
            sets.build(&all, 0).unwrap(),
            "set shape must be canonical"
        );
        assert_eq!(union, sets.union(right, left, 0).unwrap());
        assert_eq!(union, sets.union(union, left, 0).unwrap());
        let mut difference = BTreeSet::new();
        sets.difference(
            left,
            &right.into_iter().collect::<Vec<_>>(),
            0,
            &mut difference,
        )
        .unwrap();
        assert_eq!(difference, a.difference(&b).copied().collect());
        let mut covered = BTreeSet::new();
        sets.difference(
            union,
            &left.into_iter().chain(right).collect::<Vec<_>>(),
            0,
            &mut covered,
        )
        .unwrap();
        assert!(
            covered.is_empty(),
            "multiple common roots cover their union"
        );
        for id in all {
            assert_eq!(sets.contains(left, id, 0).unwrap(), a.contains(&id));
        }
        assert!(sets.nodes.len() <= NODE_CACHE_SIZE);
    }
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]
        #[test]
        fn radix_sets_match_ordered_sets(a in prop::collection::vec(0u32..2000, 0..800), b in prop::collection::vec(0u32..2000, 0..800)) {
            let ids = |values: Vec<u32>| values.into_iter().map(|n| GitOid::hash("blob", &n.to_be_bytes())).collect();
            verify(ids(a), ids(b));
        }
    }
    #[test]
    fn long_oid_prefixes_and_leaf_branch_differences() {
        let ids: Vec<_> = (0..=255u8)
            .map(|n| {
                let mut id = GitOid([0; 20]);
                id.0[19] = n;
                id
            })
            .collect();
        verify(
            ids.iter().copied().collect(),
            ids.iter().step_by(5).copied().collect(),
        );
        verify(
            ids.iter().step_by(5).copied().collect(),
            ids.iter().copied().collect(),
        );
        verify(
            ids[..100].iter().copied().collect(),
            ids[90..].iter().copied().collect(),
        );
    }
}
