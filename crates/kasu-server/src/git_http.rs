use crate::{Backend, backend::status};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path, State},
    http::{Request, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use kasu_core::{Result, invalid};
use kasu_git::*;
use kasu_store::Repository;
use std::{
    io::{self, Read, Write},
    sync::Arc,
};
use tokio::sync::mpsc;

pub(crate) fn trace_packets(protocol: Protocol, direction: &str, bytes: &[u8]) {
    if !tracing::enabled!(tracing::Level::TRACE) {
        return;
    }
    if let Ok(packets) = parse_packets(bytes) {
        for packet in packets {
            match packet {
                Packet::Data(bytes) => {
                    tracing::trace!(?protocol,direction,packet=%String::from_utf8_lossy(bytes).escape_default(),"Git pkt-line")
                }
                special => tracing::trace!(?protocol,direction,packet=?special,"Git pkt-line"),
            }
        }
    }
}
pub fn git_http_router(state: Arc<Backend>) -> Router {
    Router::new()
        .route("/{*git_path}", get(handle).post(handle))
        .with_state(state)
}
fn projection_from_url(repo: &str) -> Result<ProjectionId> {
    projection_from_bytes(repo.as_bytes())
}
pub(crate) fn projection_from_bytes(repo: &[u8]) -> Result<ProjectionId> {
    let split = |marker: &[u8]| {
        repo.windows(marker.len())
            .position(|part| part == marker)
            .map(|n| (&repo[..n], &repo[n + marker.len()..]))
    };
    let (repo, directory) = if let Some((repo, path)) = split(b"/-/path-hex/") {
        (
            repo,
            hex::decode(path).map_err(|_| invalid("invalid hex projection path"))?,
        )
    } else if let Some((repo, path)) = split(b"/-/path/") {
        (repo, path.to_vec())
    } else {
        return Ok(ProjectionId::full(repo));
    };
    Ok(ProjectionId::path(
        repo.strip_suffix(b".git").unwrap_or(repo),
        kasu_core::RepoPath::new(directory)?,
    ))
}

fn error(status: tonic::Status) -> Response {
    let code = match status.code() {
        tonic::Code::Unauthenticated => StatusCode::UNAUTHORIZED,
        tonic::Code::PermissionDenied => StatusCode::FORBIDDEN,
        tonic::Code::NotFound => StatusCode::NOT_FOUND,
        tonic::Code::InvalidArgument => StatusCode::BAD_REQUEST,
        tonic::Code::FailedPrecondition => StatusCode::PRECONDITION_FAILED,
        tonic::Code::ResourceExhausted => StatusCode::TOO_MANY_REQUESTS,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let mut out = (code, status.message().to_owned()).into_response();
    out.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    if code == StatusCode::UNAUTHORIZED {
        out.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Basic realm=\"kasu\""),
        );
    }
    out
}
fn response(body: Body, advert: bool) -> Response {
    let content = if advert {
        "application/x-git-upload-pack-advertisement"
    } else {
        "application/x-git-upload-pack-result"
    };
    (
        [
            (header::CONTENT_TYPE, content),
            (header::CACHE_CONTROL, "no-cache, no-store, must-revalidate"),
        ],
        body,
    )
        .into_response()
}
fn unpack(bytes: Bytes, gzip: bool) -> Result<Vec<u8>> {
    if !gzip {
        return Ok(bytes.to_vec());
    }
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes.as_ref())
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut out)
        .map_err(kasu_store::storage_error)?;
    if out.len() > 16 * 1024 * 1024 {
        return Err(invalid("decompressed request exceeds limit"));
    }
    Ok(out)
}
struct ChannelWriter(mpsc::Sender<io::Result<Vec<u8>>>);
impl Write for ChannelWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .blocking_send(Ok(bytes.to_vec()))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "client disconnected"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
async fn handle(
    State(state): State<Arc<Backend>>,
    Path(path): Path<String>,
    request: Request<Body>,
) -> Response {
    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if let Err(status) = state.authenticate(authorization.as_deref()) {
        return error(status);
    }
    let receiving = path.ends_with("/git-receive-pack")
        || (path.ends_with("/info/refs")
            && request.uri().query() == Some("service=git-receive-pack"));
    let (repo, advert) = if let Some(repo) = path.strip_suffix("/info/refs") {
        (repo, true)
    } else if let Some(repo) = path.strip_suffix("/git-upload-pack") {
        (repo, false)
    } else if let Some(repo) = path.strip_suffix("/git-receive-pack") {
        (repo, false)
    } else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    let requested = match projection_from_url(repo) {
        Ok(projection) => projection,
        Err(_) => return error(crate::auth::not_found()),
    };
    let scoping = state.clone();
    let name = requested.repo.to_vec();
    let directory = requested.path.clone();
    let state = match crate::admission::blocking(move || {
        scoping.scope_git(
            authorization.as_deref(),
            &name,
            directory.as_ref(),
            receiving,
        )
    })
    .await
    {
        Ok(Ok(state)) => match state.admit().await {
            Ok(state) => Arc::new(state),
            Err(status) => return error(status),
        },
        Ok(Err(status)) => return error(status),
        Err(_) => return error(tonic::Status::internal("Git worker failed")),
    };
    // Every index below is keyed by the resolved repository, never by the URL name.
    let projection = ProjectionId {
        repo: state.key().into(),
        path: requested.path,
    };
    if receiving {
        if projection.path.is_some() {
            return StatusCode::METHOD_NOT_ALLOWED.into_response();
        }
        return crate::receive::handle_receive(state, path, request).await;
    }
    if advert
        && (request.method() != axum::http::Method::GET
            || request.uri().query() != Some("service=git-upload-pack"))
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !advert && request.method() != axum::http::Method::POST {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let protocol = match Protocol::from_header(
        request
            .headers()
            .get("git-protocol")
            .and_then(|v| v.to_str().ok()),
    ) {
        Ok(p) => p,
        Err(e) => return error(status(e)),
    };
    if !advert
        && request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            != Some("application/x-git-upload-pack-request")
    {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    let gzip = match request
        .headers()
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
    {
        None | Some("identity") => false,
        Some("gzip") => true,
        _ => return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response(),
    };
    let input = if advert {
        Bytes::new()
    } else {
        match axum::body::to_bytes(request.into_body(), 16 * 1024 * 1024).await {
            Ok(bytes) => bytes,
            Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        }
    };
    enum Prepared {
        Bytes(Vec<u8>),
        Pack(bool, FetchPlan, Vec<u8>),
    }
    let prepare_state = state.clone();
    let prepare_projection = projection.clone();
    let prepared = crate::admission::blocking(move || -> Result<Prepared> {
        let ops = prepare_state.ops();
        let projection = prepare_projection;
        let refs = resolve_advertised(&list_refs_with_head(
            ops.engine,
            &ops.ctx,
            prepare_state.refs.as_ref(),
            &projection,
            &prepare_state.record().head(),
        )?)?;
        if advert {
            return Ok(Prepared::Bytes(advertisement(protocol, &refs)?));
        }
        let input = unpack(input, gzip)?;
        trace_packets(protocol, "client", &input);
        match parse_upload(protocol, &input)? {
            UploadRequest::Empty => Ok(Prepared::Bytes(Vec::new())),
            UploadRequest::LsRefs(q) => Ok(Prepared::Bytes(ls_refs(&q, &refs)?)),
            UploadRequest::Fetch(q) => {
                let index = GitIndex {
                    repo: ops.ctx.repo,
                    projection,
                };
                let mut plan = plan_fetch(&index, &refs, &q)?;
                if protocol != Protocol::V2 && !q.done {
                    plan.send_pack = false;
                }
                let prefix = fetch_prefix(protocol, &q, &plan)?;
                if plan.send_pack {
                    Ok(Prepared::Pack(q.sideband, plan, prefix))
                } else {
                    Ok(Prepared::Bytes(prefix))
                }
            }
        }
    })
    .await;
    let prepared = match prepared {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => return error(status(e)),
        Err(_) => return error(tonic::Status::internal("Git worker failed")),
    };
    match prepared {
        Prepared::Bytes(bytes) => {
            trace_packets(protocol, "server", &bytes);
            response(Body::from(bytes), advert)
        }
        Prepared::Pack(sideband, plan, prefix) => {
            trace_packets(protocol, "server", &prefix);
            tracing::trace!(
                ?protocol,
                objects = plan.objects.len(),
                "Git pack payload omitted from trace"
            );
            let (tx, rx) = mpsc::channel(8);
            crate::admission::blocking(move || {
                let mut out = io::BufWriter::with_capacity(64 * 1024, ChannelWriter(tx));
                let index = GitIndex {
                    repo: Repository {
                        meta: state.ops().ctx.repo.meta,
                        blobs: state.ops().ctx.repo.blobs,
                    },
                    projection,
                };
                let result = (|| -> Result<()> {
                    out.write_all(&prefix).map_err(kasu_store::storage_error)?;
                    if sideband {
                        write_pack_cached(
                            &index,
                            plan.objects,
                            Some(&state.pack_cache),
                            Sideband(&mut out),
                        )?;
                        out.write_all(b"0000").map_err(kasu_store::storage_error)?;
                    } else {
                        write_pack_cached(&index, plan.objects, Some(&state.pack_cache), &mut out)?;
                    }
                    out.flush().map_err(kasu_store::storage_error)
                })();
                if let Err(error) = result {
                    tracing::debug!(%error,"Git pack stream ended");
                    if sideband {
                        let _ = write_packet(&mut out, b"\x03fatal: pack stream failed\n");
                        let _ = out.write_all(b"0000");
                        let _ = out.flush();
                    }
                }
            });
            response(
                Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)),
                false,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn directory_url_rejects_invalid_components_and_decodes_byte_paths() {
        for value in [
            "repo/-/path/../private",
            "repo/-/path//private",
            "repo/-/path-hex/ff0",
            "repo/-/path-hex/00",
            "repo/-/path-hex/2e2e",
            "repo/-/path-hex/2f",
        ] {
            assert!(projection_from_url(value).is_err(), "{value}");
        }
        let parsed = projection_from_url("repo/-/path-hex/ff2f61").unwrap();
        assert_eq!(parsed.path.unwrap().as_bytes(), b"\xff/a");
        assert_eq!(
            projection_from_url("repo.git/-/path/deploy").unwrap(),
            projection_from_url("repo/-/path-hex/6465706c6f79").unwrap()
        );
    }
}
