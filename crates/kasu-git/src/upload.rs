use crate::*;
use kasu_core::*;
use kasu_store::storage_error;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Protocol {
    V0,
    V1,
    V2,
}
impl Protocol {
    pub fn from_header(header: Option<&str>) -> Result<Self> {
        let mut result = Self::V0;
        let mut seen = false;
        for parameter in header.unwrap_or("").split(':') {
            if let Some(value) = parameter.strip_prefix("version=") {
                if seen {
                    return Err(invalid("duplicate protocol version"));
                }
                seen = true;
                result = match value {
                    "0" => Self::V0,
                    "1" => Self::V1,
                    "2" => Self::V2,
                    _ => Self::V0,
                };
            }
        }
        Ok(result)
    }
}
#[derive(Clone, Debug)]
pub struct ResolvedRef {
    pub name: Vec<u8>,
    pub oid: Option<GitOid>,
    pub peeled: Option<GitOid>,
    pub symbolic: Option<Vec<u8>>,
}
pub fn resolve_advertised(refs: &[AdvertisedRef]) -> Result<Vec<ResolvedRef>> {
    let map: BTreeMap<_, _> = refs.iter().map(|r| (r.name.as_slice(), &r.value)).collect();
    let mut out = Vec::new();
    for r in refs {
        let symbolic = match &r.value {
            GitRef::Symbolic { target } => Some(target.clone()),
            _ => None,
        };
        let mut value = Some(&r.value);
        let mut seen = BTreeSet::new();
        while let Some(GitRef::Symbolic { target }) = value {
            if !seen.insert(target) {
                return Err(invalid("symbolic ref cycle"));
            }
            value = map.get(target.as_slice()).copied();
        }
        let (oid, peeled) = match value {
            Some(GitRef::Direct { target, peeled, .. }) => (Some(*target), *peeled),
            _ => (None, None),
        };
        out.push(ResolvedRef {
            name: r.name.clone(),
            oid,
            peeled,
            symbolic,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}
pub const V0_CAPABILITIES: &str = "multi_ack_detailed side-band-64k no-progress shallow deepen-since deepen-not deepen-relative include-tag ofs-delta filter allow-reachable-sha1-in-want object-format=sha1 agent=kasu/0.1.0";
pub fn advertisement(protocol: Protocol, refs: &[ResolvedRef]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    if protocol == Protocol::V2 {
        for line in [
            "version 2\n",
            "agent=kasu/0.1.0\n",
            "ls-refs=unborn\n",
            "fetch=shallow filter\n",
            "object-format=sha1\n",
        ] {
            write_packet(&mut out, line.as_bytes()).map_err(storage_error)?;
        }
        out.extend(b"0000");
        return Ok(out);
    }
    write_packet(&mut out, b"# service=git-upload-pack\n").map_err(storage_error)?;
    out.extend(b"0000");
    if protocol == Protocol::V1 {
        write_packet(&mut out, b"version 1\n").map_err(storage_error)?;
    }
    let mut caps = V0_CAPABILITIES.to_string();
    if let Some(head) = refs.iter().find(|r| r.name == b"HEAD")
        && let Some(target) = &head.symbolic
    {
        caps.push_str(" symref=HEAD:");
        caps.push_str(
            std::str::from_utf8(target)
                .map_err(|_| invalid("non-UTF-8 symbolic ref capability"))?,
        );
    }
    let mut first = true;
    for r in refs {
        let Some(oid) = r.oid else {
            continue;
        };
        let mut line = format!("{oid} ").into_bytes();
        line.extend(&r.name);
        if first {
            line.push(0);
            line.extend(caps.as_bytes());
            first = false;
        }
        line.push(b'\n');
        write_packet(&mut out, &line).map_err(storage_error)?;
        if let Some(peeled) = r.peeled {
            let mut line = format!("{peeled} ").into_bytes();
            line.extend(&r.name);
            line.extend(b"^{}\n");
            write_packet(&mut out, &line).map_err(storage_error)?;
        }
    }
    if first {
        write_packet(
            &mut out,
            format!("{} capabilities^{{}}\0{caps}\n", GitOid([0; 20])).as_bytes(),
        )
        .map_err(storage_error)?;
    }
    out.extend(b"0000");
    Ok(out)
}
#[derive(Clone, Debug, Default)]
pub struct FetchRequest {
    pub wants: Vec<GitOid>,
    pub haves: Vec<GitOid>,
    pub shallow: BTreeSet<GitOid>,
    pub depth: Option<u32>,
    pub relative: bool,
    pub since: Option<i64>,
    pub exclude: Vec<Vec<u8>>,
    pub done: bool,
    pub sideband: bool,
    pub detailed: bool,
    pub include_tag: bool,
    pub filter: Option<FetchFilter>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FetchFilter {
    BlobNone,
}
#[derive(Clone, Debug, Default)]
pub struct LsRefsRequest {
    pub peel: bool,
    pub symrefs: bool,
    pub unborn: bool,
    pub prefixes: Vec<Vec<u8>>,
}
#[derive(Clone, Debug)]
pub enum UploadRequest {
    LsRefs(LsRefsRequest),
    Fetch(FetchRequest),
    Empty,
}
fn line(bytes: &[u8]) -> Result<&[u8]> {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if bytes.contains(&0) || bytes.contains(&b'\n') || bytes.contains(&b'\r') {
        return Err(invalid("invalid protocol line"));
    }
    Ok(bytes)
}
fn fetch_arg(q: &mut FetchRequest, arg: &[u8], v2: bool) -> Result<()> {
    if let Some(filter) = arg.strip_prefix(b"filter ") {
        if q.filter.is_some() {
            return Err(invalid("duplicate filter"));
        }
        q.filter = Some(match filter {
            b"blob:none" => FetchFilter::BlobNone,
            _ => return Err(invalid("unsupported filter (supported: blob:none)")),
        });
    } else if let Some(oid) = arg.strip_prefix(b"want ") {
        q.wants.push(oid_from_hex(oid)?);
    } else if let Some(oid) = arg.strip_prefix(b"have ") {
        q.haves.push(oid_from_hex(oid)?);
    } else if let Some(oid) = arg.strip_prefix(b"shallow ") {
        q.shallow.insert(oid_from_hex(oid)?);
    } else if let Some(depth) = arg.strip_prefix(b"deepen ") {
        if q.depth.is_some() {
            return Err(invalid("duplicate deepen"));
        }
        let depth = std::str::from_utf8(depth)
            .map_err(git_error)?
            .parse::<u32>()
            .map_err(git_error)?;
        if depth == 0 {
            return Err(invalid("depth must be positive"));
        }
        q.depth = Some(depth);
    } else if let Some(time) = arg.strip_prefix(b"deepen-since ") {
        if q.since.is_some() {
            return Err(invalid("duplicate deepen-since"));
        }
        q.since = Some(
            std::str::from_utf8(time)
                .map_err(git_error)?
                .parse()
                .map_err(git_error)?,
        );
    } else if let Some(name) = arg.strip_prefix(b"deepen-not ") {
        if name.is_empty() {
            return Err(invalid("empty deepen-not"));
        }
        q.exclude.push(name.to_vec());
    } else {
        match arg {
            b"done" => q.done = true,
            b"deepen-relative" => q.relative = true,
            b"include-tag" if v2 => q.include_tag = true,
            b"thin-pack" | b"ofs-delta" | b"no-progress" if v2 => {}
            _ => {
                return Err(invalid(format!(
                    "unsupported fetch argument: {}",
                    String::from_utf8_lossy(arg)
                )));
            }
        }
    }
    Ok(())
}
pub fn parse_upload(protocol: Protocol, bytes: &[u8]) -> Result<UploadRequest> {
    let packets = parse_packets(bytes)?;
    if packets.is_empty() || packets == [Packet::Flush] {
        return Ok(UploadRequest::Empty);
    }
    if protocol == Protocol::V2 {
        let delimiter = packets
            .iter()
            .position(|p| *p == Packet::Delimiter)
            .ok_or_else(|| invalid("v2 command delimiter missing"))?;
        if packets.last() != Some(&Packet::Flush) {
            return Err(invalid("v2 request must end with flush"));
        }
        let mut command = None;
        for packet in &packets[..delimiter] {
            let Packet::Data(data) = packet else {
                return Err(invalid("invalid command header"));
            };
            let arg = line(data)?;
            if let Some(value) = arg.strip_prefix(b"command=") {
                if command.replace(value).is_some() {
                    return Err(invalid("multiple v2 commands"));
                }
            } else if arg == b"object-format=sha1" || arg.starts_with(b"agent=") {
            } else {
                return Err(invalid("unadvertised v2 capability"));
            }
        }
        let args = &packets[delimiter + 1..packets.len() - 1];
        return match command {
            Some(b"ls-refs") => {
                let mut q = LsRefsRequest::default();
                for packet in args {
                    let Packet::Data(data) = packet else {
                        return Err(invalid("invalid ls-refs framing"));
                    };
                    match line(data)? {
                        b"peel" => q.peel = true,
                        b"symrefs" => q.symrefs = true,
                        b"unborn" => q.unborn = true,
                        arg => {
                            if let Some(prefix) = arg.strip_prefix(b"ref-prefix ") {
                                q.prefixes.push(prefix.to_vec());
                            } else {
                                return Err(invalid("unsupported ls-refs argument"));
                            }
                        }
                    }
                }
                Ok(UploadRequest::LsRefs(q))
            }
            Some(b"fetch") => {
                let mut q = FetchRequest {
                    sideband: true,
                    ..Default::default()
                };
                for packet in args {
                    let Packet::Data(data) = packet else {
                        return Err(invalid("invalid fetch framing"));
                    };
                    fetch_arg(&mut q, line(data)?, true)?;
                }
                validate_fetch(&q)?;
                Ok(UploadRequest::Fetch(q))
            }
            _ => Err(invalid("unsupported v2 command")),
        };
    }
    let mut q = FetchRequest::default();
    let mut wants_done = false;
    let mut first = true;
    let mut after_done = false;
    let mut filter_capability = false;
    for packet in packets {
        match packet {
            Packet::Flush => wants_done = true,
            Packet::Data(data) => {
                if after_done {
                    return Err(invalid("data follows done"));
                }
                let arg = line(data)?;
                if arg.starts_with(b"want ") {
                    if wants_done {
                        return Err(invalid("want after flush"));
                    }
                    let mut parts = arg.split(|b| *b == b' ');
                    parts.next();
                    let oid = parts.next().ok_or_else(|| invalid("missing want OID"))?;
                    q.wants.push(oid_from_hex(oid)?);
                    let caps: Vec<_> = parts.collect();
                    if !first && !caps.is_empty() {
                        return Err(invalid("capabilities outside first want"));
                    }
                    for cap in caps {
                        match cap {
                            b"multi_ack_detailed" => q.detailed = true,
                            b"side-band-64k" => q.sideband = true,
                            b"include-tag" => q.include_tag = true,
                            b"filter" => filter_capability = true,
                            b"deepen-relative" => q.relative = true,
                            b"deepen-since" | b"deepen-not" => {}
                            b"shallow" | b"no-progress" | b"ofs-delta" | b"object-format=sha1" => {}
                            v if v.starts_with(b"agent=") => {}
                            _ => return Err(invalid("unadvertised fetch capability")),
                        }
                    }
                    first = false;
                } else {
                    if arg.starts_with(b"have ") || arg == b"done" {
                        if !wants_done {
                            return Err(invalid("have/done before wants flush"));
                        }
                    } else if wants_done {
                        return Err(invalid("depth argument after wants flush"));
                    }
                    fetch_arg(&mut q, arg, false)?;
                    after_done = q.done;
                }
            }
            _ => return Err(invalid("v2 packet marker in v0/v1 request")),
        }
    }
    if !wants_done {
        return Err(invalid("wants flush missing"));
    }
    if q.filter.is_some() && !filter_capability {
        return Err(invalid("filter capability was not selected"));
    }
    validate_fetch(&q)?;
    Ok(UploadRequest::Fetch(q))
}
fn validate_fetch(q: &FetchRequest) -> Result<()> {
    if q.wants.is_empty() || q.wants.len() > 10_000 || q.haves.len() > 100_000 {
        return Err(invalid("invalid fetch object count"));
    }
    if q.depth.is_some() && (q.since.is_some() || !q.exclude.is_empty()) {
        return Err(invalid("deepen cannot be combined with since/not"));
    }
    if q.relative && q.depth.is_none() {
        return Err(invalid("deepen-relative requires deepen"));
    }
    Ok(())
}
pub fn ls_refs(q: &LsRefsRequest, refs: &[ResolvedRef]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for r in refs {
        if !q.prefixes.is_empty() && !q.prefixes.iter().any(|p| r.name.starts_with(p)) {
            continue;
        }
        let mut line = if let Some(oid) = r.oid {
            format!("{oid} ").into_bytes()
        } else if q.unborn && r.symbolic.is_some() {
            b"unborn ".to_vec()
        } else {
            continue;
        };
        line.extend(&r.name);
        if q.symrefs
            && let Some(target) = &r.symbolic
        {
            line.extend(b" symref-target:");
            line.extend(target);
        }
        if q.peel
            && let Some(oid) = r.peeled
        {
            line.extend(format!(" peeled:{oid}").bytes());
        }
        line.push(b'\n');
        write_packet(&mut out, &line).map_err(storage_error)?;
    }
    out.extend(b"0000");
    Ok(out)
}

#[derive(Debug)]
pub struct FetchPlan {
    pub objects: BTreeSet<GitOid>,
    pub shallow: BTreeSet<GitOid>,
    pub unshallow: BTreeSet<GitOid>,
    pub common: Vec<GitOid>,
    pub send_pack: bool,
}
pub(crate) fn object_links(
    index: &GitIndex<'_>,
    oid: GitOid,
    parents: bool,
) -> Result<Vec<GitOid>> {
    // Blob leaves have no edges; walking history must not load their payloads.
    if index.require(oid)?.native.kind() == GitKind::Blob {
        return Ok(Vec::new());
    }
    let (kind, raw) = index.object(oid)?;
    Ok(match kind {
        GitKind::Blob => Vec::new(),
        GitKind::Tree => parse_tree(&raw)?
            .into_iter()
            .filter(|e| !e.is_gitlink())
            .map(|e| e.oid)
            .collect(),
        GitKind::Tag => vec![oid_from_hex(parse_tag(&raw)?.target)?],
        GitKind::Commit => {
            let cs = parse_commit(&raw)?;
            let mut links = vec![oid_from_hex(cs.tree)?];
            if parents {
                links.extend(
                    cs.parents
                        .iter()
                        .map(|p| oid_from_hex(p))
                        .collect::<Result<Vec<_>>>()?,
                );
            }
            links
        }
    })
}
pub fn reachable(
    index: &GitIndex<'_>,
    roots: impl IntoIterator<Item = GitOid>,
    shallow: &BTreeSet<GitOid>,
) -> Result<BTreeSet<GitOid>> {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<_> = roots.into_iter().collect();
    while let Some(id) = stack.pop() {
        if seen.insert(id) {
            stack.extend(object_links(index, id, !shallow.contains(&id))?);
        }
    }
    Ok(seen)
}
pub fn public_roots(refs: &[ResolvedRef]) -> BTreeSet<GitOid> {
    refs.iter().filter_map(|r| r.oid).collect()
}
/// Whether `oid` is explicitly published or reachable from the public roots.
/// Build `public` once per request; each check is then an index lookup.
pub fn authorized(
    index: &GitIndex<'_>,
    public: &ReachableSet<'_, '_>,
    oid: GitOid,
) -> Result<bool> {
    Ok(index.is_published(oid)? || public.contains(oid)?)
}
fn commit_parents(index: &GitIndex<'_>, oid: GitOid) -> Result<Vec<GitOid>> {
    let (kind, raw) = index.object(oid)?;
    if kind != GitKind::Commit {
        return Err(invalid("expected Git commit"));
    }
    parse_commit(&raw)?
        .parents
        .iter()
        .map(|p| oid_from_hex(p))
        .collect()
}
fn authorize_wants(
    index: &GitIndex<'_>,
    public: &ReachableSet<'_, '_>,
    wants: &[GitOid],
) -> Result<()> {
    for oid in wants {
        if !authorized(index, public, *oid)? {
            return Err(Error::Denied("requested object is not public".into()));
        }
    }
    Ok(())
}
pub fn plan_fetch(
    index: &GitIndex<'_>,
    refs: &[ResolvedRef],
    q: &FetchRequest,
) -> Result<FetchPlan> {
    plan_fetch_protocol(index, refs, q, Protocol::V2)
}
pub fn plan_fetch_protocol(
    index: &GitIndex<'_>,
    refs: &[ResolvedRef],
    q: &FetchRequest,
    protocol: Protocol,
) -> Result<FetchPlan> {
    validate_fetch(q)?;
    let public = ReachableSet::new(index, public_roots(refs))?;
    authorize_wants(index, &public, &q.wants)?;
    let mut common = Vec::new();
    let mut seen_haves = BTreeSet::new();
    for oid in &q.haves {
        if seen_haves.insert(*oid)
            && index.find_changeset(*oid)?.is_some()
            && authorized(index, &public, *oid)?
        {
            common.push(*oid);
        }
    }
    let send_pack = q.done || (protocol == Protocol::V2 && !common.is_empty());
    let changing_depth = q.depth.is_some() || q.since.is_some() || !q.exclude.is_empty();
    if !send_pack && !changing_depth {
        return Ok(FetchPlan {
            objects: BTreeSet::new(),
            shallow: BTreeSet::new(),
            unshallow: BTreeSet::new(),
            common,
            send_pack,
        });
    }
    let unbounded =
        q.shallow.is_empty() && q.depth.is_none() && q.since.is_none() && q.exclude.is_empty();
    let (mut objects, shallow, unshallow) = if unbounded {
        (
            crate::reachability::reachable_difference(index, &q.wants, &common)?,
            BTreeSet::new(),
            BTreeSet::new(),
        )
    } else {
        let have_objects = send_pack
            .then(|| HaveObjects::new(index, &common, &q.shallow))
            .transpose()?;
        let mut excludes = BTreeSet::new();
        for name in &q.exclude {
            let r = refs
                .iter()
                .find(|r| {
                    r.name == *name
                        || r.name.strip_prefix(b"refs/heads/") == Some(name.as_slice())
                        || r.name.strip_prefix(b"refs/tags/") == Some(name.as_slice())
                })
                .and_then(|r| r.oid)
                .ok_or_else(|| invalid("unknown deepen-not ref"))?;
            let (target, kind) = peel(index, r)?;
            if kind != GitKind::Commit {
                return Err(invalid("deepen-not requires commit ref"));
            }
            let mut stack = vec![target];
            while let Some(id) = stack.pop() {
                if excludes.insert(id) {
                    stack.extend(commit_parents(index, id)?);
                }
            }
        }
        let mut objects = BTreeSet::new();
        let mut seeds = Vec::new();
        for want in &q.wants {
            let mut id = *want;
            loop {
                let (kind, raw) = index.object(id)?;
                if kind == GitKind::Tag {
                    if send_pack {
                        objects.insert(id);
                    }
                    id = oid_from_hex(parse_tag(&raw)?.target)?;
                } else {
                    if kind == GitKind::Commit {
                        seeds.push(id);
                    } else if send_pack {
                        objects.extend(reachable(index, [id], &BTreeSet::new())?);
                    }
                    break;
                }
            }
        }
        // The best remaining budget wins when DAG paths meet. None means unlimited.
        let initial = if q.relative && !q.shallow.is_empty() {
            None
        } else {
            q.depth.map(|d| d - 1)
        };
        let mut queue: VecDeque<_> = seeds.iter().copied().map(|id| (id, initial)).collect();
        let mut selected = BTreeMap::<GitOid, Option<u32>>::new();
        while let Some((id, mut budget)) = queue.pop_front() {
            if excludes.contains(&id) {
                continue;
            }
            let (kind, raw) = index.object(id)?;
            if kind != GitKind::Commit {
                return Err(invalid("commit parent has wrong type"));
            }
            let cs = parse_commit(&raw)?;
            if let Some(since) = q.since
                && signature(cs.committer)?.timestamp < since
            {
                continue;
            }
            if q.relative && budget.is_none() && q.shallow.contains(&id) {
                budget = q.depth;
            }
            if let Some(old) = selected.get(&id)
                && (old.is_none() || budget.is_some_and(|n| old.is_some_and(|o| o >= n)))
            {
                continue;
            }
            selected.insert(id, budget);
            let changing_depth = q.depth.is_some() || q.since.is_some() || !q.exclude.is_empty();
            if budget == Some(0) || (!changing_depth && q.shallow.contains(&id)) {
                continue;
            }
            let next = budget.map(|n| n - 1);
            for parent in cs.parents {
                queue.push_back((oid_from_hex(parent)?, next));
            }
        }
        if !seeds.is_empty() && selected.is_empty() {
            return Err(invalid("no commits satisfy the shallow boundary"));
        }
        // A Git shallow marker cuts all parents of a merge. Remove nodes that were
        // selected along a surviving parent edge but became unreachable at that cut.
        let changing_depth = q.depth.is_some() || q.since.is_some() || !q.exclude.is_empty();
        let mut cuts = BTreeSet::new();
        for id in selected.keys() {
            if (!changing_depth && q.shallow.contains(id))
                || commit_parents(index, *id)?
                    .iter()
                    .any(|p| !selected.contains_key(p))
            {
                cuts.insert(*id);
            }
        }
        let mut keep = BTreeSet::new();
        let mut pending = seeds;
        while let Some(id) = pending.pop() {
            if !selected.contains_key(&id) || !keep.insert(id) {
                continue;
            }
            if !cuts.contains(&id) {
                pending.extend(commit_parents(index, id)?);
            }
        }
        selected.retain(|id, _| keep.contains(id));
        let mut shallow = BTreeSet::new();
        let mut unshallow = BTreeSet::new();
        for id in selected.keys() {
            let parents = commit_parents(index, *id)?;
            let boundary = parents.iter().any(|p| !selected.contains_key(p));
            if boundary {
                shallow.insert(*id);
            } else if q.shallow.contains(id) {
                unshallow.insert(*id);
            }
            let Some(have_objects) = &have_objects else {
                continue;
            };
            objects.insert(*id);
            for tree in object_links(index, *id, false)? {
                let mut stack = vec![tree];
                while let Some(oid) = stack.pop() {
                    if have_objects.contains(oid)? || !objects.insert(oid) {
                        continue;
                    }
                    stack.extend(object_links(index, oid, true)?);
                }
            }
        }
        if let Some(have_objects) = &have_objects {
            let mut missing = BTreeSet::new();
            for oid in objects {
                if !have_objects.contains(oid)? {
                    missing.insert(oid);
                }
            }
            objects = missing;
        }
        (objects, shallow, unshallow)
    };
    if q.include_tag {
        for r in refs {
            if let Some(target) = r.peeled
                && objects.contains(&target)
            {
                let mut id = r.oid.unwrap();
                loop {
                    let (kind, raw) = index.object(id)?;
                    if kind != GitKind::Tag {
                        break;
                    }
                    objects.insert(id);
                    id = oid_from_hex(parse_tag(&raw)?.target)?;
                }
            }
        }
    }
    if q.filter == Some(FetchFilter::BlobNone) {
        let mut filtered = BTreeSet::new();
        for oid in objects {
            if index.require(oid)?.native.kind() != GitKind::Blob {
                filtered.insert(oid);
            }
        }
        objects = filtered;
    }
    // Explicit wants are never omitted, even if a common commit reaches them.
    // A partial client may have that commit while still missing its blobs.
    if send_pack {
        objects.extend(q.wants.iter().copied());
    }
    Ok(FetchPlan {
        objects,
        shallow,
        unshallow,
        common,
        send_pack,
    })
}
struct HaveObjects<'a, 'b> {
    commits: BTreeSet<GitOid>,
    objects: ReachableSet<'a, 'b>,
}
impl<'a, 'b> HaveObjects<'a, 'b> {
    fn new(index: &'a GitIndex<'b>, common: &[GitOid], shallow: &BTreeSet<GitOid>) -> Result<Self> {
        let mut commits = BTreeSet::new();
        let roots = if shallow.is_empty() {
            common.iter().copied().collect()
        } else {
            let mut trees = BTreeSet::new();
            let mut pending = common.to_vec();
            while let Some(id) = pending.pop() {
                if !commits.insert(id) {
                    continue;
                }
                let (_, raw) = index.object(id)?;
                let commit = parse_commit(&raw)?;
                trees.insert(oid_from_hex(commit.tree)?);
                if !shallow.contains(&id) {
                    for parent in commit.parents {
                        pending.push(oid_from_hex(parent)?);
                    }
                }
            }
            trees
        };
        Ok(Self {
            commits,
            objects: ReachableSet::new(index, roots)?,
        })
    }
    fn contains(&self, id: GitOid) -> Result<bool> {
        Ok(self.commits.contains(&id) || self.objects.contains(id)?)
    }
}
pub fn fetch_prefix(protocol: Protocol, q: &FetchRequest, plan: &FetchPlan) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let changing_depth = q.depth.is_some() || q.since.is_some() || !q.exclude.is_empty();
    if protocol == Protocol::V2 {
        if !q.done {
            write_packet(&mut out, b"acknowledgments\n").map_err(storage_error)?;
            if plan.common.is_empty() {
                write_packet(&mut out, b"NAK\n").map_err(storage_error)?;
            } else {
                for oid in &plan.common {
                    write_packet(&mut out, format!("ACK {oid}\n").as_bytes())
                        .map_err(storage_error)?;
                }
                if plan.send_pack {
                    write_packet(&mut out, b"ready\n").map_err(storage_error)?;
                }
            }
            out.extend(if plan.send_pack { b"0001" } else { b"0000" });
        }
        if plan.send_pack {
            if changing_depth && (!plan.shallow.is_empty() || !plan.unshallow.is_empty()) {
                write_packet(&mut out, b"shallow-info\n").map_err(storage_error)?;
                for id in &plan.shallow {
                    write_packet(&mut out, format!("shallow {id}\n").as_bytes())
                        .map_err(storage_error)?;
                }
                for id in &plan.unshallow {
                    write_packet(&mut out, format!("unshallow {id}\n").as_bytes())
                        .map_err(storage_error)?;
                }
                out.extend(b"0001");
            }
            write_packet(&mut out, b"packfile\n").map_err(storage_error)?;
        }
    } else {
        if changing_depth {
            for id in &plan.shallow {
                write_packet(&mut out, format!("shallow {id}\n").as_bytes())
                    .map_err(storage_error)?;
            }
            for id in &plan.unshallow {
                write_packet(&mut out, format!("unshallow {id}\n").as_bytes())
                    .map_err(storage_error)?;
            }
            out.extend(b"0000");
        }
        if changing_depth && !q.done && q.haves.is_empty() {
            return Ok(out);
        }
        if q.done {
            if let Some(id) = plan.common.last() {
                write_packet(&mut out, format!("ACK {id}\n").as_bytes()).map_err(storage_error)?;
            } else {
                write_packet(&mut out, b"NAK\n").map_err(storage_error)?;
            }
        } else {
            if q.detailed {
                for oid in &plan.common {
                    write_packet(&mut out, format!("ACK {oid} common\n").as_bytes())
                        .map_err(storage_error)?;
                }
            } else if let Some(oid) = plan.common.first() {
                write_packet(&mut out, format!("ACK {oid}\n").as_bytes()).map_err(storage_error)?;
            }
            if q.detailed || plan.common.is_empty() {
                write_packet(&mut out, b"NAK\n").map_err(storage_error)?;
            }
        }
    }
    Ok(out)
}
