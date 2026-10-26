use crate::{Backend, backend::status};
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
    response::{IntoResponse, Response},
};
use kasu_core::{GitKind, GitOid, GitRef, Tree, TreeId};
use kasu_git::*;
use kasu_ingest::Importer;
use kasu_store::*;
use std::{collections::BTreeSet, sync::Arc};
use tonic::Status;

enum PreparedRef {
    Native(RefUpdate),
    Git(GitRefUpdate),
}

impl Backend {
    pub fn with_receive_limits(mut self, limits: ReceiveLimits) -> Self {
        self.receive_limits = limits;
        self
    }
    fn prepare_receive_command(
        &self,
        index: &GitIndex<'_>,
        command: &ReceiveCommand,
    ) -> Result<PreparedRef, Status> {
        let policy = self;
        if !policy.allows_ref(&command.name) {
            return Err(Status::permission_denied("ref is not writable"));
        }
        if command.old.is_none() && !policy.allows_create() {
            return Err(Status::permission_denied("ref creation is disabled"));
        }
        if command.new.is_none() && !policy.allows_delete() {
            return Err(Status::permission_denied("ref deletion is disabled"));
        }
        let ops = self.ops();
        if command.name.starts_with(b"refs/heads/") {
            let expected = self
                .refs
                .resolve(&self.repository, &command.name)
                .map_err(status)?;
            let old = expected
                .target
                .map(|id| {
                    ops.engine
                        .derive::<GitCommit>(&ops.ctx, id, &index.projection)
                        .map(|c| c.0)
                })
                .transpose()
                .map_err(status)?;
            if old != command.old {
                return Err(Status::aborted("stale ref; fetch first"));
            }
            let new = command
                .new
                .map(|oid| {
                    index.find_changeset(oid).and_then(|id| {
                        id.ok_or_else(|| kasu_core::invalid("branch target is not a commit"))
                    })
                })
                .transpose()
                .map_err(status)?;
            if let (Some(old), Some(new)) = (expected.target, new)
                && !policy.allows_force(&command.name)
                && !ops.is_ancestor(old, new).map_err(status)?
            {
                return Err(Status::permission_denied(
                    "non-fast-forward update is disabled",
                ));
            }
            self.check_changeset_acl(expected.target, new)?;
            Ok(PreparedRef::Native(RefUpdate {
                name: command.name.clone(),
                expected,
                new,
            }))
        } else if command.name.starts_with(b"refs/tags/") {
            let expected = self
                .refs
                .resolve_git(&self.repository, &command.name)
                .map_err(status)?;
            let old = match &expected.value {
                None => None,
                Some(GitRef::Direct { target, .. }) => Some(*target),
                _ => return Err(Status::invalid_argument("symbolic tag is not writable")),
            };
            if old != command.old {
                return Err(Status::aborted("stale tag; fetch first"));
            }
            if old.is_some()
                && command.new.is_some()
                && old != command.new
                && !policy.allows_force(&command.name)
            {
                return Err(Status::permission_denied("tag replacement is disabled"));
            }
            if !policy.unrestricted_paths() {
                let empty = ops.ctx.repo.put_tree(&Tree::default()).map_err(status)?;
                let tree =
                    |target: Option<GitOid>| -> Result<TreeId, Status> {
                        let Some(target) = target else {
                            return Ok(empty);
                        };
                        let (target, kind) = peel(index, target).map_err(status)?;
                        match kind {
                            GitKind::Commit => ops
                                .root_tree(
                                    index.find_changeset(target).map_err(status)?.ok_or_else(
                                        || Status::invalid_argument("unindexed tag commit"),
                                    )?,
                                )
                                .map_err(status),
                            GitKind::Tree => match index.require(target).map_err(status)?.native {
                                NativeTarget::Tree(id) => Ok(id),
                                _ => Err(Status::invalid_argument("invalid tree tag")),
                            },
                            _ => Err(Status::permission_denied(
                                "path-restricted writes require commit or tree tags",
                            )),
                        }
                    };
                self.check_tree_acl(tree(old)?, tree(command.new)?)?;
            }
            let new = command
                .new
                .map(|target| {
                    let kind = index.require(target)?.native.kind();
                    let peeled = if kind == GitKind::Tag {
                        Some(peel(index, target)?.0)
                    } else {
                        None
                    };
                    Ok::<_, kasu_core::Error>(GitRef::Direct {
                        target,
                        kind,
                        peeled,
                    })
                })
                .transpose()
                .map_err(status)?;
            Ok(PreparedRef::Git(GitRefUpdate {
                name: command.name.clone(),
                expected,
                new,
            }))
        } else {
            Err(Status::permission_denied(
                "only branch and tag refs are writable",
            ))
        }
    }
    fn publish_receive_command(&self, update: PreparedRef, time: i64) -> Result<(), Status> {
        let applied = match update {
            PreparedRef::Native(update) => matches!(
                self.refs
                    .compare_and_swap(
                        &self.repository,
                        &[update],
                        self.write_actor(b"git-push"),
                        time
                    )
                    .map_err(status)?,
                CasResult::Applied(_)
            ),
            PreparedRef::Git(update) => matches!(
                self.refs
                    .compare_git(
                        &self.repository,
                        &[update],
                        self.write_actor(b"git-push"),
                        time
                    )
                    .map_err(status)?,
                GitCasResult::Applied(_)
            ),
        };
        if applied {
            Ok(())
        } else {
            Err(Status::aborted("concurrent update; fetch first"))
        }
    }
    fn receive_atomic(
        &self,
        index: &GitIndex<'_>,
        commands: &[ReceiveCommand],
        time: i64,
    ) -> Vec<(Vec<u8>, Option<String>)> {
        let mut native = Vec::new();
        let mut git = Vec::new();
        let mut statuses = Vec::new();
        for command in commands {
            let error = match self.prepare_receive_command(index, command) {
                Ok(PreparedRef::Native(update)) => {
                    native.push(update);
                    None
                }
                Ok(PreparedRef::Git(update)) => {
                    git.push(update);
                    None
                }
                Err(error) => Some(error.message().to_owned()),
            };
            statuses.push((command.name.clone(), error));
        }
        if statuses.iter().any(|s| s.1.is_some()) {
            for (_, error) in &mut statuses {
                error.get_or_insert_with(|| "atomic push failed".into());
            }
            return statuses;
        }
        let error = match self.refs.compare_native_and_git(
            &self.repository,
            &native,
            &git,
            self.write_actor(b"git-push"),
            time,
        ) {
            Ok(RefPublicationResult::Applied { .. }) => None,
            Ok(
                RefPublicationResult::NativeMismatch { .. }
                | RefPublicationResult::GitMismatch { .. },
            ) => Some("atomic push failed: concurrent update; fetch first".to_owned()),
            Err(error) => Some(status(error).message().to_owned()),
        };
        for (_, result) in &mut statuses {
            *result = error.clone();
        }
        statuses
    }
    pub fn receive_pack(&self, bytes: &[u8]) -> Result<Vec<u8>, Status> {
        let q = parse_receive(bytes).map_err(status)?;
        if q.atomic && !self.refs.supports_atomic_publication() {
            return Err(Status::invalid_argument(
                "backend does not support atomic push",
            ));
        }
        for command in &q.commands {
            tracing::trace!(old=?command.old,new=?command.new,name=%String::from_utf8_lossy(&command.name).escape_default(),"Git receive command");
        }
        let ops = self.ops();
        let projection = ProjectionId::full(self.repository.clone());
        let refs = resolve_advertised(
            &list_refs(ops.engine, &ops.ctx, self.refs.as_ref(), &projection).map_err(status)?,
        )
        .map_err(status)?;
        let index = GitIndex {
            repo: Repository {
                meta: ops.ctx.repo.meta,
                blobs: ops.ctx.repo.blobs,
            },
            projection,
        };
        let stage = || -> kasu_core::Result<()> {
            let public = reachable(&index, public_roots(&refs), &BTreeSet::new())?;
            for oid in &q.shallow {
                if !public.contains(oid) || index.object(*oid)?.0 != GitKind::Commit {
                    return Err(kasu_core::invalid("unavailable shallow boundary"));
                }
            }
            let objects = if q.pack.is_empty() {
                Default::default()
            } else {
                decode_received_pack(q.pack, &self.receive_limits, &|oid| {
                    if !public.contains(&oid) {
                        return Ok(None);
                    }
                    let (kind, bytes) = index.object(oid)?;
                    Ok(Some(RawObject {
                        kind,
                        bytes: bytes.into(),
                    }))
                })?
            };
            for object in objects.values() {
                match object.kind {
                    GitKind::Blob => {}
                    GitKind::Commit => {
                        parse_commit(&object.bytes)?;
                    }
                    GitKind::Tree => {
                        parse_tree(&object.bytes)?;
                    }
                    GitKind::Tag => {
                        parse_tag(&object.bytes)?;
                    }
                }
            }
            let roots = q
                .commands
                .iter()
                .filter_map(|command| {
                    command.new.map(|oid| {
                        (
                            oid,
                            command
                                .name
                                .starts_with(b"refs/heads/")
                                .then_some(GitKind::Commit),
                        )
                    })
                })
                .collect();
            Importer {
                engine: ops.engine,
                index: GitIndex {
                    repo: Repository {
                        meta: ops.ctx.repo.meta,
                        blobs: ops.ctx.repo.blobs,
                    },
                    projection: index.projection.clone(),
                },
            }
            .stage_objects(
                roots,
                &|oid| {
                    if let Some(object) = objects.get(&oid) {
                        Ok((object.kind, object.bytes.to_vec()))
                    } else if public.contains(&oid) {
                        index.object(oid)
                    } else {
                        Err(kasu_core::invalid("missing or unavailable push dependency"))
                    }
                },
                &public,
            )?;
            Ok(())
        };
        if let Err(error) = stage() {
            let message = status(error).message().to_owned();
            let statuses = q
                .commands
                .iter()
                .map(|c| (c.name.clone(), Some("unpacker error".into())))
                .collect::<Vec<_>>();
            if !q.report_status {
                return Err(Status::invalid_argument(message));
            }
            return receive_report(&q, Some(&message), &statuses).map_err(status);
        }
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Status::internal("clock before epoch"))?
            .as_secs() as i64;
        let statuses = if q.atomic {
            self.receive_atomic(&index, &q.commands, time)
        } else {
            q.commands
                .iter()
                .map(|command| {
                    (
                        command.name.clone(),
                        self.prepare_receive_command(&index, command)
                            .and_then(|update| self.publish_receive_command(update, time))
                            .err()
                            .map(|e| e.message().to_owned()),
                    )
                })
                .collect::<Vec<_>>()
        };
        if !q.report_status && statuses.iter().any(|s| s.1.is_some()) {
            return Err(Status::permission_denied("one or more refs were rejected"));
        }
        receive_report(&q, None, &statuses).map_err(status)
    }
}
pub(crate) async fn handle_receive(
    state: Arc<Backend>,
    path: String,
    request: Request<Body>,
) -> Response {
    let advert = path.ends_with("/info/refs");
    let suffix = if advert {
        "/info/refs"
    } else {
        "/git-receive-pack"
    };
    let repo = path
        .strip_suffix(suffix)
        .unwrap()
        .strip_suffix(".git")
        .unwrap_or(path.strip_suffix(suffix).unwrap());
    if let Err(error) = state.repo(repo.as_bytes()) {
        return (StatusCode::FORBIDDEN, error.message().to_owned()).into_response();
    }
    if (advert && request.method() != axum::http::Method::GET)
        || (!advert && request.method() != axum::http::Method::POST)
    {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if !advert
        && request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            != Some("application/x-git-receive-pack-request")
    {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    if request
        .headers()
        .get(header::CONTENT_ENCODING)
        .is_some_and(|v| v != "identity")
    {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    let input = if advert {
        Vec::new()
    } else {
        match axum::body::to_bytes(
            request.into_body(),
            state
                .receive_limits
                .pack_bytes
                .saturating_add(2 * 1024 * 1024),
        )
        .await
        {
            Ok(b) => b.to_vec(),
            Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        }
    };
    let result = crate::admission::blocking(move || {
        if advert {
            let ops = state.ops();
            let refs = resolve_advertised(
                &list_refs(
                    ops.engine,
                    &ops.ctx,
                    state.refs.as_ref(),
                    &ProjectionId::full(state.repository.clone()),
                )
                .map_err(status)?,
            )
            .map_err(status)?;
            receive_advertisement(&refs, state.refs.supports_atomic_publication()).map_err(status)
        } else {
            state.receive_pack(&input)
        }
    })
    .await;
    match result {
        Ok(Ok(bytes)) => (
            [
                (
                    header::CONTENT_TYPE,
                    if advert {
                        "application/x-git-receive-pack-advertisement"
                    } else {
                        "application/x-git-receive-pack-result"
                    },
                ),
                (header::CACHE_CONTROL, "no-cache, no-store, must-revalidate"),
            ],
            bytes,
        )
            .into_response(),
        Ok(Err(error)) => {
            let code = match error.code() {
                tonic::Code::InvalidArgument => StatusCode::BAD_REQUEST,
                tonic::Code::PermissionDenied => StatusCode::FORBIDDEN,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (code, error.message().to_owned()).into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
