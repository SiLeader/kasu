//! Streamed tar.gz and zip snapshots of a directory. Archives honor the
//! caller's read scope and are bounded in entries and bytes.
use crate::{Backend, Mode, backend::status};
use kasu_core::{self as c, TreeEntry};
use kasu_proto::v1::{
    self as p,
    archive_server::{Archive, ArchiveServer},
};
use std::{io::Write, sync::Arc};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

const MAX_ENTRIES: usize = 100_000;
const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const CHUNK: usize = 256 * 1024;

/// A file of the archive, in path order.
struct Item {
    path: Vec<u8>,
    entry: c::Entry,
}
impl Backend {
    /// Every leaf below `path` at `id`, as the caller may read it.
    fn archive_items(&self, q: &p::ArchiveRequest, max_bytes: u64) -> Result<Vec<Item>, Status> {
        let id = self.id(&q.repo, &q.id)?;
        let ops = self.ops();
        let root = ops.root_tree(id).map_err(status)?;
        let start = if q.path.is_empty() {
            root
        } else {
            let path = c::RepoPath::new(q.path.clone()).map_err(status)?;
            if !self.read_enters(path.as_bytes()) {
                return Err(Status::permission_denied(
                    "directory is outside readable scope",
                ));
            }
            match kasu_derived::lookup(&ops.ctx.repo, root, &path).map_err(status)? {
                Some(TreeEntry::SubTree(tree)) => tree,
                _ => return Err(Status::not_found("directory not found")),
            }
        };
        let mut items = Vec::new();
        let mut bytes = 0u64;
        let mut pending = vec![(Vec::new(), start)];
        while let Some((prefix, tree)) = pending.pop() {
            for (name, value) in ops
                .ctx
                .repo
                .tree(tree)
                .map_err(status)?
                .entries
                .into_iter()
                .rev()
            {
                let mut path = prefix.clone();
                if !path.is_empty() {
                    path.push(b'/');
                }
                path.extend_from_slice(&name);
                let full_path = if q.path.is_empty() {
                    path.clone()
                } else {
                    [q.path.as_slice(), b"/", &path].concat()
                };
                match value {
                    TreeEntry::SubTree(child) if self.read_enters(&full_path) => {
                        pending.push((path, child));
                    }
                    TreeEntry::Leaf(entry) if self.read_allows(&full_path) => {
                        if let Some(blob) = entry.blob() {
                            let size = ops.ctx.repo.blob_metadata(blob).map_err(status)?.size;
                            bytes = bytes
                                .checked_add(size)
                                .filter(|n| *n <= max_bytes)
                                .ok_or_else(|| {
                                    Status::resource_exhausted("archive content exceeds byte limit")
                                })?;
                        }
                        items.push(Item { path, entry });
                    }
                    _ => {}
                }
                if items.len() > MAX_ENTRIES {
                    return Err(Status::resource_exhausted("archive exceeds 100000 entries"));
                }
            }
        }
        items.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(items)
    }
}
/// Forwards written bytes to the response stream in chunks.
struct Sink {
    tx: tokio::sync::mpsc::Sender<Result<p::ArchiveChunk, Status>>,
    buffer: Vec<u8>,
    written: u64,
}
impl Write for Sink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if data.len() as u64 > MAX_BYTES.saturating_sub(self.written) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "archive exceeds 2 GiB",
            ));
        }
        let size = data.len().min(CHUNK - self.buffer.len());
        self.buffer.extend_from_slice(&data[..size]);
        self.written += size as u64;
        if self.buffer.len() == CHUNK {
            self.flush()?;
        }
        Ok(size)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.buffer);
        self.tx
            .blocking_send(Ok(p::ArchiveChunk { data }))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "client went away"))
    }
}
fn archive_io(error: std::io::Error) -> Status {
    match error.kind() {
        std::io::ErrorKind::FileTooLarge => Status::resource_exhausted(error.to_string()),
        std::io::ErrorKind::BrokenPipe => Status::cancelled(error.to_string()),
        _ => Status::internal(error.to_string()),
    }
}
fn octal(field: &mut [u8], value: u64) {
    let width = field.len() - 1;
    let text = format!("{value:0width$o}");
    field[..width].copy_from_slice(&text.as_bytes()[text.len() - width..]);
}
fn tar_header(name: &[u8], size: u64, mode: u64, kind: u8, link: &[u8]) -> [u8; 512] {
    let mut header = [0u8; 512];
    header[..name.len().min(100)].copy_from_slice(&name[..name.len().min(100)]);
    octal(&mut header[100..108], mode);
    octal(&mut header[108..116], 0);
    octal(&mut header[116..124], 0);
    octal(&mut header[124..136], size);
    octal(&mut header[136..148], 0);
    header[156] = kind;
    header[157..157 + link.len().min(100)].copy_from_slice(&link[..link.len().min(100)]);
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    header[148..156].copy_from_slice(b"        ");
    let sum: u64 = header.iter().map(|b| u64::from(*b)).sum();
    octal(&mut header[148..155], sum);
    header[155] = b' ';
    header
}
fn pad(out: &mut impl Write, size: u64) -> std::io::Result<()> {
    let rest = (512 - size % 512) % 512;
    out.write_all(&vec![0; rest as usize])
}
/// A PAX record carries names and link targets that do not fit ustar.
fn pax(out: &mut impl Write, name: &[u8], link: &[u8]) -> std::io::Result<()> {
    let mut records = Vec::new();
    for (key, value) in [(b"path".as_slice(), name), (b"linkpath", link)] {
        if value.is_empty() || (value.len() < 100 && value.is_ascii()) {
            continue;
        }
        let body = [b" ".as_slice(), key, b"=", value, b"\n"].concat();
        let mut length = body.len() + 1;
        while length.to_string().len() + body.len() != length {
            length = length.to_string().len() + body.len();
        }
        records.extend(length.to_string().into_bytes());
        records.extend(body);
    }
    if records.is_empty() {
        return Ok(());
    }
    out.write_all(&tar_header(
        b"pax_header",
        records.len() as u64,
        0o644,
        b'x',
        b"",
    ))?;
    out.write_all(&records)?;
    pad(out, records.len() as u64)
}
fn write_tar(backend: &Backend, items: &[Item], prefix: &[u8], sink: Sink) -> Result<Sink, Status> {
    let io = archive_io;
    let mut out = flate2::write::GzEncoder::new(sink, flate2::Compression::default());
    for item in items {
        let name = [prefix, &item.path].concat();
        let blob = item.entry.blob();
        let content = match blob {
            Some(blob) => backend.ops().ctx.repo.blob(blob).map_err(status)?,
            None => Vec::new(),
        };
        let (mode, kind, link, data): (u64, u8, &[u8], &[u8]) = match &item.entry {
            c::Entry::File { executable, .. } => {
                (if *executable { 0o755 } else { 0o644 }, b'0', b"", &content)
            }
            c::Entry::Symlink { .. } => (0o777, b'2', &content, b""),
            // Submodule contents are not part of the repository.
            c::Entry::Gitlink { .. } => continue,
        };
        pax(&mut out, &name, link).map_err(io)?;
        out.write_all(&tar_header(&name, data.len() as u64, mode, kind, link))
            .map_err(io)?;
        out.write_all(data).map_err(io)?;
        pad(&mut out, data.len() as u64).map_err(io)?;
    }
    out.write_all(&[0; 1024]).map_err(io)?;
    out.finish().map_err(io)
}
fn write_zip(
    backend: &Backend,
    items: &[Item],
    prefix: &[u8],
    mut out: Sink,
) -> Result<Sink, Status> {
    let io = archive_io;
    let mut central = Vec::new();
    let mut count = 0u16;
    for item in items {
        let name = [prefix, &item.path].concat();
        let (data, external) = match &item.entry {
            c::Entry::File { blob, executable } => (
                backend.ops().ctx.repo.blob(*blob).map_err(status)?,
                if *executable { 0o100755u32 } else { 0o100644 } << 16,
            ),
            c::Entry::Symlink { blob } => (
                backend.ops().ctx.repo.blob(*blob).map_err(status)?,
                0o120777u32 << 16,
            ),
            c::Entry::Gitlink { .. } => continue,
        };
        let mut crc = flate2::Crc::new();
        crc.update(&data);
        let mut deflate =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        deflate.write_all(&data).map_err(io)?;
        let compressed = deflate.finish().map_err(io)?;
        let offset = out.written;
        if offset + compressed.len() as u64 >= u64::from(u32::MAX)
            || count == u16::MAX
            || name.len() > usize::from(u16::MAX)
        {
            return Err(Status::resource_exhausted(
                "archive needs ZIP64; use tar.gz",
            ));
        }
        let flags: u16 = if std::str::from_utf8(&name).is_ok() {
            1 << 11
        } else {
            0
        };
        let fixed = |signature: u32| {
            let mut header = signature.to_le_bytes().to_vec();
            if signature == 0x0201_4b50 {
                header.extend(0x031eu16.to_le_bytes()); // made by: Unix, 3.0
            }
            header.extend(20u16.to_le_bytes());
            header.extend(flags.to_le_bytes());
            header.extend(8u16.to_le_bytes());
            header.extend(0u32.to_le_bytes()); // DOS time and date
            header.extend(crc.sum().to_le_bytes());
            header.extend((compressed.len() as u32).to_le_bytes());
            header.extend((data.len() as u32).to_le_bytes());
            header.extend((name.len() as u16).to_le_bytes());
            header.extend(0u16.to_le_bytes());
            header
        };
        out.write_all(&fixed(0x0403_4b50)).map_err(io)?;
        out.write_all(&name).map_err(io)?;
        out.write_all(&compressed).map_err(io)?;
        central.extend(fixed(0x0201_4b50));
        central.extend(0u16.to_le_bytes()); // comment
        central.extend(0u16.to_le_bytes()); // disk
        central.extend(0u16.to_le_bytes()); // internal attributes
        central.extend(external.to_le_bytes());
        central.extend((offset as u32).to_le_bytes());
        central.extend(&name);
        count += 1;
    }
    let start = out.written;
    out.write_all(&central).map_err(io)?;
    let mut end = 0x0605_4b50u32.to_le_bytes().to_vec();
    end.extend([0u8; 4]);
    end.extend(count.to_le_bytes());
    end.extend(count.to_le_bytes());
    end.extend((central.len() as u32).to_le_bytes());
    end.extend((start as u32).to_le_bytes());
    end.extend(0u16.to_le_bytes());
    out.write_all(&end).map_err(io)?;
    Ok(out)
}

#[derive(Clone)]
pub struct ArchiveService {
    state: Arc<Backend>,
}
impl ArchiveService {
    pub fn new(state: Arc<Backend>) -> Self {
        Self { state }
    }
    pub fn into_server(self) -> crate::ObservedService<ArchiveServer<Self>> {
        use tower::Layer;
        let layer = crate::ServerLayer::grpc(self.state.clone());
        layer.layer(ArchiveServer::new(self))
    }
}
#[tonic::async_trait]
impl Archive for ArchiveService {
    type GetArchiveStream = ReceiverStream<Result<p::ArchiveChunk, Status>>;
    async fn get_archive(
        &self,
        request: Request<p::ArchiveRequest>,
    ) -> Result<Response<Self::GetArchiveStream>, Status> {
        let authorization = request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let input = request.into_inner();
        if input.prefix.len() > 256 || input.prefix.contains(&0) {
            return Err(Status::invalid_argument("invalid archive prefix"));
        }
        let state = self
            .state
            .scoped(authorization.as_deref(), &input.repo, Mode::Read)
            .await?;
        let format = p::ArchiveFormat::try_from(input.format)
            .map_err(|_| Status::invalid_argument("unknown archive format"))?;
        let worker = state.clone();
        let request = input.clone();
        let items = crate::admission::blocking(move || worker.archive_items(&request, MAX_BYTES))
            .await
            .map_err(|_| Status::internal("archive worker failed"))??;
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        crate::admission::blocking(move || {
            let sink = Sink {
                tx: tx.clone(),
                buffer: Vec::new(),
                written: 0,
            };
            let result = match format {
                p::ArchiveFormat::TarGz => write_tar(&state, &items, &input.prefix, sink),
                p::ArchiveFormat::Zip => write_zip(&state, &items, &input.prefix, sink),
            };
            match result.and_then(|mut sink| sink.flush().map_err(archive_io)) {
                Ok(()) => {}
                Err(error) => {
                    let _ = tx.blocking_send(Err(error));
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn archive_selection_limits_expanded_bytes_without_granting_object_access() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(kasu_store::SqliteStore::memory().unwrap());
        let blobs = Arc::new(kasu_store::FsBlobStore::open(dir.path()).unwrap());
        let backend = Backend::new(b"repo".to_vec(), store.clone(), blobs, store, None);
        let mut scoped = backend.scope(None, b"repo", Mode::Read).unwrap();
        let repository = scoped.ops().ctx.repo;
        let blob = repository.put_blob(&[0; 8]).unwrap();
        let signature = c::Signature {
            name: "A".into(),
            email: "a@example.com".into(),
            timestamp: 1,
            tz: "+0000".into(),
        };
        let id = repository
            .put_changeset(&c::Changeset {
                parents: vec![],
                author: signature.clone(),
                committer: signature,
                message: "archive".into(),
                changes: [b"docs/a".as_slice(), b"docs/sub/b", b"private/c"]
                    .into_iter()
                    .map(|path| c::FileChange::Set {
                        path: c::RepoPath::new(path).unwrap(),
                        entry: c::Entry::File {
                            blob,
                            executable: false,
                        },
                    })
                    .collect(),
                git_provenance: None,
            })
            .unwrap();
        repository.associate_changeset(scoped.key(), id).unwrap();
        scoped.read_scope = Some(Arc::new(crate::read_scope::ReadScope::new(
            scoped.key(),
            &[c::RepoPath::new(b"docs".as_slice()).unwrap()],
        )));
        let mut request = p::ArchiveRequest {
            repo: b"repo".to_vec(),
            id: id.0.to_vec(),
            ..Default::default()
        };
        let items = scoped.archive_items(&request, 16).unwrap();
        assert_eq!(
            items.iter().map(|i| i.path.as_slice()).collect::<Vec<_>>(),
            [b"docs/a".as_slice(), b"docs/sub/b"]
        );
        assert_eq!(
            scoped.archive_items(&request, 15).err().unwrap().code(),
            tonic::Code::ResourceExhausted
        );
        request.path = b"docs/sub".to_vec();
        assert_eq!(scoped.archive_items(&request, 8).unwrap()[0].path, b"b");
        request.path = b"private".to_vec();
        assert_eq!(
            scoped.archive_items(&request, 16).err().unwrap().code(),
            tonic::Code::PermissionDenied
        );
        assert!(
            !scoped
                .ops()
                .ctx
                .repo
                .has_blob_membership(scoped.object_namespace(), blob)
                .unwrap()
        );
    }

    #[test]
    fn sink_rejects_output_over_the_limit_including_archive_trailers() {
        let (tx, _rx) = tokio::sync::mpsc::channel(2);
        let mut sink = Sink {
            tx,
            buffer: vec![],
            written: MAX_BYTES - 4,
        };
        sink.write_all(b"1234").unwrap();
        assert_eq!(sink.written, MAX_BYTES);
        assert_eq!(
            archive_io(sink.write_all(b"x").unwrap_err()).code(),
            tonic::Code::ResourceExhausted
        );
        assert_eq!(sink.buffer, b"1234");
    }

    #[test]
    fn sink_splits_large_writes_without_losing_bytes() {
        let data: Vec<u8> = (0..5 * 1024 * 1024 + 17).map(|i| i as u8).collect();
        let expected = [b"header".as_slice(), &data].concat();
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let worker = std::thread::spawn(move || {
            let mut sink = Sink {
                tx,
                buffer: Vec::new(),
                written: 0,
            };
            sink.write_all(b"header").unwrap();
            sink.write_all(&data).unwrap();
            sink.flush().unwrap();
            sink.written
        });
        let mut received = Vec::new();
        while let Some(chunk) = rx.blocking_recv() {
            let chunk = chunk.unwrap();
            assert!(!chunk.data.is_empty() && chunk.data.len() <= CHUNK);
            received.extend(chunk.data);
        }
        assert_eq!(worker.join().unwrap(), expected.len() as u64);
        assert_eq!(received, expected);
    }

    #[test]
    fn tar_headers_have_valid_checksums_and_pax_carries_long_names() {
        let header = tar_header(b"a.txt", 5, 0o644, b'0', b"");
        let stored = std::str::from_utf8(&header[148..154]).unwrap();
        let mut copy = header;
        copy[148..156].copy_from_slice(b"        ");
        let sum: u64 = copy.iter().map(|b| u64::from(*b)).sum();
        assert_eq!(u64::from_str_radix(stored, 8).unwrap(), sum);
        let mut out = Vec::new();
        pax(&mut out, &[b'x'; 150], b"").unwrap();
        assert_eq!(out.len() % 512, 0);
        let record = String::from_utf8_lossy(&out[512..]);
        let length: usize = record.split(' ').next().unwrap().parse().unwrap();
        assert_eq!(length, " path=\n".len() + 150 + length.to_string().len());
        let mut none = Vec::new();
        pax(&mut none, b"short", b"").unwrap();
        assert!(none.is_empty());
    }
}
