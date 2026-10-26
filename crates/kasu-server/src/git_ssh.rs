use crate::{
    Backend,
    git_http::{projection_from_bytes, trace_packets},
};
use kasu_core::{Result, invalid};
use kasu_git::*;
use kasu_store::storage_error;
use std::io::{BufRead, Write};

mod pack_stream;

const REQUEST_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Service {
    Upload,
    Receive,
}

// Parse a single Git command and one shell-quoted argument, without invoking a
// shell or interpreting substitutions. Git quotes embedded apostrophes as '\''.
fn command(raw: &[u8]) -> Result<(Service, Vec<u8>)> {
    if raw.len() > 4096 || raw.contains(&0) {
        return Err(invalid("invalid SSH command length"));
    }
    let split = raw
        .iter()
        .position(|b| *b == b' ')
        .ok_or_else(|| invalid("missing SSH repository argument"))?;
    let service = match &raw[..split] {
        b"git-upload-pack" => Service::Upload,
        b"git-receive-pack" => Service::Receive,
        _ => return Err(invalid("unsupported SSH command")),
    };
    let mut path = Vec::new();
    let mut single = false;
    let mut ended = false;
    let mut started = false;
    let mut bytes = raw[split + 1..].iter().copied();
    while let Some(byte) = bytes.next() {
        if single {
            if byte == b'\'' {
                single = false;
            } else {
                path.push(byte);
            }
        } else {
            match byte {
                b' ' | b'\t' => {
                    if started {
                        ended = true;
                    }
                }
                _ if ended => return Err(invalid("SSH command requires exactly one argument")),
                b'\'' => {
                    single = true;
                    started = true;
                }
                b'\\' => {
                    path.push(
                        bytes
                            .next()
                            .ok_or_else(|| invalid("incomplete SSH argument escape"))?,
                    );
                    started = true;
                }
                b'"' | b'$' | b'`' | b';' | b'&' | b'|' | b'<' | b'>' | b'(' | b')' | b'\n'
                | b'\r' => return Err(invalid("unsupported unquoted SSH argument")),
                byte => {
                    path.push(byte);
                    started = true;
                }
            }
        }
    }
    if single || path.is_empty() {
        return Err(invalid("invalid SSH repository argument"));
    }
    let path = path.strip_prefix(b"/").unwrap_or(&path);
    let path = path.strip_suffix(b".git").unwrap_or(path).to_vec();
    Ok((service, path))
}

fn packet(reader: &mut impl BufRead, budget: &mut usize) -> Result<Option<Vec<u8>>> {
    let mut header = [0u8; 4];
    loop {
        match reader.read(&mut header[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(storage_error(error)),
        }
    }
    reader.read_exact(&mut header[1..]).map_err(storage_error)?;
    let len = usize::from_str_radix(std::str::from_utf8(&header).map_err(storage_error)?, 16)
        .map_err(storage_error)?;
    if len == 3 || len > MAX_PACKET {
        return Err(invalid("invalid SSH pkt-line length"));
    }
    let size = len.max(4);
    *budget = budget
        .checked_add(size)
        .filter(|n| *n <= REQUEST_BYTES)
        .ok_or_else(|| invalid("SSH request limit exceeded"))?;
    let mut packet = vec![0; size];
    packet[..4].copy_from_slice(&header);
    reader.read_exact(&mut packet[4..]).map_err(storage_error)?;
    Ok(Some(packet))
}
fn round(reader: &mut impl BufRead, budget: &mut usize, done: bool) -> Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    while let Some(packet) = packet(reader, budget)? {
        let end = packet == b"0000" || (done && matches!(&packet[4..], b"done" | b"done\n"));
        out.extend(packet);
        if end {
            return Ok(Some(out));
        }
    }
    if out.is_empty() {
        Ok(None)
    } else {
        Err(invalid("incomplete SSH request"))
    }
}
fn raw_advertisement(bytes: Vec<u8>, service: Service, protocol: Protocol) -> Result<Vec<u8>> {
    if service == Service::Upload && protocol == Protocol::V2 {
        return Ok(bytes);
    }
    let mut prefix = Vec::new();
    write_packet(
        &mut prefix,
        match service {
            Service::Upload => b"# service=git-upload-pack\n",
            Service::Receive => b"# service=git-receive-pack\n",
        },
    )
    .map_err(storage_error)?;
    prefix.extend(b"0000");
    bytes
        .strip_prefix(prefix.as_slice())
        .map(<[u8]>::to_vec)
        .ok_or_else(|| invalid("invalid Git advertisement envelope"))
}
fn refs(state: &Backend, projection: &ProjectionId) -> Result<Vec<ResolvedRef>> {
    let ops = state.ops();
    resolve_advertised(&list_refs(
        ops.engine,
        &ops.ctx,
        state.refs.as_ref(),
        projection,
    )?)
}
fn send_pack(
    state: &Backend,
    projection: &ProjectionId,
    q: &FetchRequest,
    plan: FetchPlan,
    out: &mut impl Write,
) -> Result<()> {
    let ops = state.ops();
    let index = GitIndex {
        repo: ops.ctx.repo,
        projection: projection.clone(),
    };
    tracing::trace!(
        objects = plan.objects.len(),
        "SSH Git pack payload omitted from trace"
    );
    if q.sideband {
        write_pack_cached(
            &index,
            plan.objects,
            Some(&state.pack_cache),
            Sideband(&mut *out),
        )?;
        out.write_all(b"0000").map_err(storage_error)?;
    } else {
        write_pack_cached(&index, plan.objects, Some(&state.pack_cache), &mut *out)?;
    }
    out.flush().map_err(storage_error)
}

pub fn serve_git_ssh(
    state: &Backend,
    authorization: &str,
    original_command: &[u8],
    protocol: Option<&str>,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<()> {
    if state.token.is_none()
        && state.projection_accounts.is_empty()
        && state.repository_accounts.is_empty()
    {
        return Err(invalid("SSH requires configured credential identities"));
    }
    let access = state
        .authenticate(Some(authorization))
        .map_err(|e| invalid(e.message()))?;
    let (service, path) = command(original_command)?;
    let projection = projection_from_bytes(&path)?;
    state
        .repo(&projection.repo)
        .map_err(|e| invalid(e.message()))?;
    access
        .check_git(&projection, service == Service::Receive)
        .map_err(|e| invalid(e.message()))?;
    if service == Service::Receive && projection.path.is_some() {
        return Err(invalid("SSH projection is read-only"));
    }
    let protocol = if service == Service::Receive {
        Protocol::V0
    } else {
        Protocol::from_header(protocol)?
    };
    let refs = refs(state, &projection)?;
    let advert = raw_advertisement(
        match service {
            Service::Upload => advertisement(protocol, &refs)?,
            Service::Receive => {
                receive_advertisement(&refs, state.refs.supports_atomic_publication())?
            }
        },
        service,
        protocol,
    )?;
    trace_packets(protocol, "ssh server", &advert);
    output.write_all(&advert).map_err(storage_error)?;
    output.flush().map_err(storage_error)?;
    let mut budget = 0;
    if service == Service::Receive {
        let state = state
            .scope_writer(&access)
            .map_err(|e| invalid(e.message()))?;
        let Some(mut bytes) = round(input, &mut budget, false)? else {
            return Ok(());
        };
        trace_packets(protocol, "ssh client", &bytes);
        if parse_receive(&bytes)?
            .commands
            .iter()
            .any(|c| c.new.is_some())
        {
            pack_stream::read_pack(input, &mut bytes, &state.receive_limits)?;
        }
        output
            .write_all(
                &state
                    .receive_pack(&bytes)
                    .map_err(|e| invalid(e.message()))?,
            )
            .map_err(storage_error)?;
        return output.flush().map_err(storage_error);
    }
    if protocol == Protocol::V2 {
        while let Some(bytes) = round(input, &mut budget, false)? {
            trace_packets(protocol, "ssh client", &bytes);
            match parse_upload(protocol, &bytes)? {
                UploadRequest::Empty => break,
                UploadRequest::LsRefs(q) => {
                    output
                        .write_all(&ls_refs(&q, &refs)?)
                        .map_err(storage_error)?;
                }
                UploadRequest::Fetch(q) => {
                    let ops = state.ops();
                    let index = GitIndex {
                        repo: ops.ctx.repo,
                        projection: projection.clone(),
                    };
                    let plan = plan_fetch(&index, &refs, &q)?;
                    let prefix = fetch_prefix(protocol, &q, &plan)?;
                    trace_packets(protocol, "ssh server", &prefix);
                    output.write_all(&prefix).map_err(storage_error)?;
                    if plan.send_pack {
                        send_pack(state, &projection, &q, plan, output)?;
                    }
                }
            }
            output.flush().map_err(storage_error)?;
        }
        return Ok(());
    }
    let Some(mut bytes) = round(input, &mut budget, false)? else {
        return Ok(());
    };
    trace_packets(protocol, "ssh client", &bytes);
    let UploadRequest::Fetch(initial) = parse_upload(protocol, &bytes)? else {
        return Ok(());
    };
    let ops = state.ops();
    let index = GitIndex {
        repo: ops.ctx.repo,
        projection: projection.clone(),
    };
    if initial.depth.is_some() || initial.since.is_some() || !initial.exclude.is_empty() {
        let plan = plan_fetch(&index, &refs, &initial)?;
        let prefix = fetch_prefix(protocol, &initial, &plan)?;
        trace_packets(protocol, "ssh server", &prefix);
        output.write_all(&prefix).map_err(storage_error)?;
        output.flush().map_err(storage_error)?;
    }
    let mut ack_sent = false;
    let mut public = None;
    loop {
        let Some(packet) = packet(input, &mut budget)? else {
            return Err(invalid("SSH negotiation ended before done"));
        };
        let done = matches!(&packet[4..], b"done" | b"done\n");
        let flush = packet == b"0000";
        trace_packets(protocol, "ssh client", &packet);
        if !initial.detailed
            && !ack_sent
            && let Some(raw) = packet[4..].strip_prefix(b"have ")
        {
            let oid = oid_from_hex(raw.strip_suffix(b"\n").unwrap_or(raw))?;
            if index.find_changeset(oid)?.is_some() {
                if public.is_none() {
                    public = Some(ReachableSet::new(&index, public_roots(&refs))?);
                }
                if authorized(&index, public.as_ref().unwrap(), oid)? {
                    write_packet(output, format!("ACK {oid}\n").as_bytes())
                        .map_err(storage_error)?;
                    output.flush().map_err(storage_error)?;
                    ack_sent = true;
                }
            }
        }
        bytes.extend(packet);
        if !done && !flush {
            continue;
        }
        let UploadRequest::Fetch(q) = parse_upload(protocol, &bytes)? else {
            return Err(invalid("invalid SSH fetch negotiation"));
        };
        let plan = plan_fetch(&index, &refs, &q)?;
        let mut ack = q.clone();
        ack.depth = None;
        ack.since = None;
        ack.exclude.clear();
        let prefix = if !q.detailed && ack_sent {
            Vec::new()
        } else {
            fetch_prefix(protocol, &ack, &plan)?
        };
        trace_packets(protocol, "ssh server", &prefix);
        output.write_all(&prefix).map_err(storage_error)?;
        output.flush().map_err(storage_error)?;
        if done {
            return send_pack(state, &projection, &q, plan, output);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn command_words_preserve_quoted_bytes_and_never_interpret_shell_expressions() {
        assert_eq!(
            command(b"git-upload-pack '/repo.git'").unwrap(),
            (Service::Upload, b"repo".to_vec())
        );
        assert_eq!(
            command(b"git-upload-pack 'repo'\\''name.git'").unwrap(),
            (Service::Upload, b"repo'name".to_vec())
        );
        assert_eq!(
            command(b"git-upload-pack 'repo/-/path-hex/ff.git'").unwrap(),
            (Service::Upload, b"repo/-/path-hex/ff".to_vec())
        );
        assert_eq!(
            command(b"git-upload-pack '$(literal).git'").unwrap().1,
            b"$(literal)"
        );
        for command_line in [
            b"sh -c whoami".as_slice(),
            b"git-upload-pack a.git;whoami",
            b"git-upload-pack a.git b.git",
            b"git-upload-pack 'unterminated",
            b"git-upload-pack $(id)",
            b"git-upload-pack x\0",
        ] {
            assert!(command(command_line).is_err());
        }
    }
    #[test]
    fn projection_suffix_preserves_dot_git_directory_names() {
        for (argument, directory) in [
            ("repo/-/path/app.git", b"app".as_slice()),
            ("repo.git/-/path/app.git.git", b"app.git"),
            ("repo/-/path-hex/6170702e676974.git", b"app.git"),
        ] {
            let (_, path) = command(format!("git-upload-pack '{argument}'").as_bytes()).unwrap();
            assert_eq!(
                projection_from_bytes(&path).unwrap(),
                ProjectionId::path(
                    b"repo".as_slice(),
                    kasu_core::RepoPath::new(directory).unwrap()
                )
            );
        }
    }
    #[test]
    fn incremental_pkt_line_reader_handles_fragmentation_delimiters_and_done() {
        use std::io::{BufReader, Cursor};
        let mut bytes = Vec::new();
        write_packet(&mut bytes, b"done\n").unwrap();
        bytes.extend(b"000000010002");
        let mut input = BufReader::with_capacity(1, Cursor::new(bytes));
        let mut budget = 0;
        assert_eq!(
            round(&mut input, &mut budget, true).unwrap().unwrap(),
            b"0009done\n"
        );
        assert_eq!(packet(&mut input, &mut budget).unwrap().unwrap(), b"0000");
        assert_eq!(packet(&mut input, &mut budget).unwrap().unwrap(), b"0001");
        assert_eq!(packet(&mut input, &mut budget).unwrap().unwrap(), b"0002");
        assert!(packet(&mut input, &mut budget).unwrap().is_none());
        for bytes in [b"0003".as_slice(), b"ffff", b"0010short", b"zzzz"] {
            assert!(packet(&mut Cursor::new(bytes), &mut 0).is_err());
        }
    }
}
