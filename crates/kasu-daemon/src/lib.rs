use std::{
    fs::File,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};
use tonic::Status;
pub struct Lease {
    file: File,
}
impl Lease {
    pub fn acquire(path: &Path) -> Result<Self, Status> {
        let file: File = rustix::fs::open(
            path,
            rustix::fs::OFlags::RDWR
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::from_raw_mode(0o600),
        )
        .map_err(|_| Status::failed_precondition("cannot open daemon lock"))?
        .into();
        if !file
            .metadata()
            .map_err(|_| Status::internal("cannot inspect daemon lock"))?
            .is_file()
        {
            return Err(Status::failed_precondition("invalid daemon lock file"));
        }
        file.try_lock()
            .map_err(|_| Status::failed_precondition("daemon path is already locked"))?;
        Ok(Self { file })
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}
pub struct SocketLease {
    path: PathBuf,
    inode: u64,
    _lock: Lease,
}
impl SocketLease {
    pub fn bind(path: &Path) -> Result<(tokio::net::UnixListener, Self), Status> {
        if !path.is_absolute() || path.to_str().is_none() {
            return Err(Status::invalid_argument(
                "socket path must be absolute UTF-8",
            ));
        }
        let parent = path
            .parent()
            .ok_or_else(|| Status::invalid_argument("socket path has no parent"))?;
        let meta = parent
            .metadata()
            .map_err(|_| Status::invalid_argument("socket directory does not exist"))?;
        if meta.uid() != rustix::process::geteuid().as_raw()
            || meta.permissions().mode() & 0o077 != 0
        {
            return Err(Status::permission_denied(
                "socket directory must be private to its owner",
            ));
        }
        let lock = Lease::acquire(&path.with_extension("sock.lock"))?;
        if let Ok(meta) = path.symlink_metadata() {
            if !meta.file_type().is_socket() || meta.uid() != rustix::process::geteuid().as_raw() {
                return Err(Status::failed_precondition(
                    "socket path contains another file",
                ));
            }
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => return Err(Status::already_exists("socket is already active")),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) => {}
                Err(_) => {
                    return Err(Status::failed_precondition(
                        "cannot establish whether socket is stale",
                    ));
                }
            }
            std::fs::remove_file(path)
                .map_err(|_| Status::internal("cannot remove stale socket"))?;
        }
        let listener = tokio::net::UnixListener::bind(path)
            .map_err(|_| Status::internal("cannot bind daemon socket"))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| Status::internal("cannot set socket permissions"))?;
        let inode = path
            .symlink_metadata()
            .map_err(|_| Status::internal("cannot inspect socket"))?
            .ino();
        Ok((
            listener,
            Self {
                path: path.to_owned(),
                inode,
                _lock: lock,
            },
        ))
    }
}
impl Drop for SocketLease {
    fn drop(&mut self) {
        if self
            .path
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_socket() && m.ino() == self.inode)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
