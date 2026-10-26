use crate::*;
use kasu_core::{GitOid, Result, invalid};
use std::collections::BTreeSet;

pub const RECEIVE_CAPABILITIES: &str =
    "report-status side-band-64k delete-refs ofs-delta object-format=sha1 agent=kasu/0.1.0";
#[derive(Clone, Debug)]
pub struct ReceiveCommand {
    pub old: Option<GitOid>,
    pub new: Option<GitOid>,
    pub name: Vec<u8>,
}
#[derive(Debug)]
pub struct ReceiveRequest<'a> {
    pub commands: Vec<ReceiveCommand>,
    pub shallow: Vec<GitOid>,
    pub report_status: bool,
    pub sideband: bool,
    pub atomic: bool,
    pub pack: &'a [u8],
}
pub fn receive_advertisement(refs: &[ResolvedRef], atomic: bool) -> Result<Vec<u8>> {
    let capabilities = if atomic {
        format!("{RECEIVE_CAPABILITIES} atomic")
    } else {
        RECEIVE_CAPABILITIES.to_owned()
    };
    let mut out = Vec::new();
    write_packet(&mut out, b"# service=git-receive-pack\n").map_err(kasu_store::storage_error)?;
    out.extend(b"0000");
    let mut first = true;
    for r in refs.iter().filter(|r| r.name.starts_with(b"refs/")) {
        if let Some(oid) = r.oid {
            let mut line = format!("{oid} ").into_bytes();
            line.extend(&r.name);
            if first {
                line.push(0);
                line.extend(capabilities.as_bytes());
                first = false;
            }
            line.push(b'\n');
            write_packet(&mut out, &line).map_err(kasu_store::storage_error)?;
        }
    }
    if first {
        write_packet(
            &mut out,
            format!("{} capabilities^{{}}\0{capabilities}\n", GitOid([0; 20])).as_bytes(),
        )
        .map_err(kasu_store::storage_error)?;
    }
    out.extend(b"0000");
    Ok(out)
}
fn optional_oid(raw: &[u8]) -> Result<Option<GitOid>> {
    let id = oid_from_hex(raw)?;
    Ok((id != GitOid([0; 20])).then_some(id))
}
pub fn parse_receive(mut bytes: &[u8]) -> Result<ReceiveRequest<'_>> {
    let mut commands = Vec::new();
    let mut shallow = Vec::new();
    let mut report_status = false;
    let mut sideband = false;
    let mut atomic = false;
    let mut names = BTreeSet::new();
    let mut first = true;
    loop {
        let header = bytes
            .get(..4)
            .ok_or_else(|| invalid("truncated receive command header"))?;
        let length = usize::from_str_radix(std::str::from_utf8(header).map_err(git_error)?, 16)
            .map_err(git_error)?;
        if length == 0 {
            bytes = &bytes[4..];
            break;
        }
        if !(5..=65520).contains(&length) || length > bytes.len() {
            return Err(invalid("invalid receive pkt-line length"));
        }
        let mut line = &bytes[4..length];
        bytes = &bytes[length..];
        line = line.strip_suffix(b"\n").unwrap_or(line);
        if let Some(raw) = line.strip_prefix(b"shallow ") {
            if !first || shallow.len() >= 1024 {
                return Err(invalid("invalid shallow command position or count"));
            }
            shallow.push(oid_from_hex(raw)?);
            continue;
        }
        if commands.len() >= 1024 {
            return Err(invalid("too many receive commands"));
        }
        if let Some(nul) = line.iter().position(|b| *b == 0) {
            if !first {
                return Err(invalid("capabilities only belong on the first command"));
            }
            let caps = std::str::from_utf8(&line[nul + 1..]).map_err(git_error)?;
            let mut seen = BTreeSet::new();
            for cap in caps.split_ascii_whitespace() {
                if !seen.insert(cap) {
                    return Err(invalid("duplicate receive capability"));
                }
                match cap {
                    "report-status" => report_status = true,
                    "side-band-64k" => sideband = true,
                    "atomic" => atomic = true,
                    "ofs-delta" | "delete-refs" | "object-format=sha1" => {}
                    c if c.starts_with("agent=") => {}
                    _ => return Err(invalid("unsupported receive capability")),
                }
            }
            line = &line[..nul];
        }
        first = false;
        let parts: Vec<_> = line.split(|b| *b == b' ').collect();
        let [old, new, name] = parts.as_slice() else {
            return Err(invalid("receive command needs old/new OID and ref"));
        };
        validate_git_ref_name(name)?;
        if !names.insert(name.to_vec()) {
            return Err(invalid("duplicate receive ref command"));
        }
        let old = optional_oid(old)?;
        let new = optional_oid(new)?;
        if old.is_none() && new.is_none() {
            return Err(invalid("receive command cannot have two zero OIDs"));
        }
        commands.push(ReceiveCommand {
            old,
            new,
            name: name.to_vec(),
        });
    }
    if commands.is_empty() && !bytes.is_empty() {
        return Err(invalid("pack without ref commands"));
    }
    Ok(ReceiveRequest {
        commands,
        shallow,
        report_status,
        sideband,
        atomic,
        pack: bytes,
    })
}
pub fn receive_report(
    q: &ReceiveRequest<'_>,
    unpack: Option<&str>,
    statuses: &[(Vec<u8>, Option<String>)],
) -> Result<Vec<u8>> {
    if !q.report_status {
        return Ok(Vec::new());
    }
    let clean = |s: &str| {
        s.replace(['\n', '\r', '\0'], " ")
            .chars()
            .take(256)
            .collect::<String>()
    };
    let mut report = Vec::new();
    write_packet(
        &mut report,
        format!(
            "unpack {}\n",
            unpack.map(clean).unwrap_or_else(|| "ok".into())
        )
        .as_bytes(),
    )
    .map_err(kasu_store::storage_error)?;
    for (name, error) in statuses {
        let mut line = if error.is_some() {
            b"ng ".to_vec()
        } else {
            b"ok ".to_vec()
        };
        line.extend(name);
        if let Some(error) = error {
            line.push(b' ');
            line.extend(clean(error).as_bytes());
        }
        line.push(b'\n');
        write_packet(&mut report, &line).map_err(kasu_store::storage_error)?;
    }
    report.extend(b"0000");
    if q.sideband {
        let mut out = Vec::new();
        std::io::Write::write_all(&mut Sideband(&mut out), &report)
            .map_err(kasu_store::storage_error)?;
        out.extend(b"0000");
        Ok(out)
    } else {
        Ok(report)
    }
}
