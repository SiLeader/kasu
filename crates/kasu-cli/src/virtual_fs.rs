use crate::{Remote, Result, Snapshot, SparseProfile, State, Tracked, invalid};
use kasu_core::{BlobId, Canonical, ChangesetId, Entry};
use kasu_store::FsBlobStore;
use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

mod lower;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    server: String,
    objects: String,
    repo: Vec<u8>,
    revision: Vec<u8>,
    origin: Vec<u8>,
    reference: Vec<u8>,
    sparse: SparseProfile,
    mountpoint: PathBuf,
}
pub struct MountRequest<'a> {
    pub server: &'a str,
    pub objects: &'a str,
    pub repo: Vec<u8>,
    pub revision: &'a [u8],
    pub mountpoint: &'a Path,
    pub backing: &'a Path,
    pub sparse: SparseProfile,
    pub token: Option<&'a str>,
}
fn command(program: &str, args: &[&std::ffi::OsStr]) -> Result<()> {
    let output = Command::new(program).args(args).output()?;
    if !output.status.success() {
        return Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(())
}
fn local_path(path: &Path) -> Result<PathBuf> {
    let path = std::path::absolute(path)?;
    let name = path
        .file_name()
        .ok_or_else(|| invalid("mount needs a directory name"))?;
    let parent = path.parent().unwrap().canonicalize()?;
    let path = parent.join(name);
    if path
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        return Err(invalid("mount control directories cannot be symlinks"));
    }
    let text = path
        .to_str()
        .ok_or_else(|| invalid("mount control paths must be UTF-8"))?;
    if text
        .bytes()
        .any(|b| b.is_ascii_whitespace() || b"\\,:".contains(&b))
    {
        return Err(invalid(
            "mount control paths cannot contain whitespace, backslash, comma or colon",
        ));
    }
    Ok(path)
}
fn save(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    use std::io::Write;
    let next = path.with_extension("next");
    let mut file = File::create(&next)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    std::fs::rename(next, path)?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
fn mounted(path: &Path) -> Result<Option<(String, String, String)>> {
    let info = std::fs::read_to_string("/proc/self/mountinfo")?;
    for line in info.lines() {
        let Some((fields, fs)) = line.split_once(" - ") else {
            continue;
        };
        if fields
            .split_whitespace()
            .nth(4)
            .is_some_and(|p| Path::new(p) == path)
        {
            let mut fs = fs.split_whitespace();
            let kind = fs.next().unwrap_or("").to_owned();
            let source = fs.next().unwrap_or("").to_owned();
            return Ok(Some((kind, source, fs.next().unwrap_or("").to_owned())));
        }
    }
    Ok(None)
}
fn recover_mounts(manifest: &Manifest, backing: &Path) -> Result<()> {
    if let Some((kind, source, options)) = mounted(&manifest.mountpoint)? {
        let upper = format!("upperdir={}", backing.join("upper").display());
        if kind != "overlay" || source != "kasu-virtual" || !options.split(',').any(|o| o == upper)
        {
            return Err(invalid("mountpoint belongs to another filesystem"));
        }
        command("umount", &[manifest.mountpoint.as_os_str()])?;
    }
    let lower = backing.join("lower");
    if let Some((kind, source, _)) = mounted(&lower)? {
        if !matches!(kind.as_str(), "fuse" | "fuse.kasu-virtual") || source != "kasu-virtual" {
            return Err(invalid("lower mount belongs to another filesystem"));
        }
        command("umount", &[lower.as_os_str()])?;
    }
    Ok(())
}

pub async fn mount(q: MountRequest<'_>) -> Result<()> {
    let mountpoint = local_path(q.mountpoint)?;
    let backing = local_path(q.backing)?;
    if backing.starts_with(&mountpoint) || mountpoint.starts_with(&backing) {
        return Err(invalid(
            "backing and mountpoint must be separate directories",
        ));
    }
    q.sparse.validate()?;
    let manifest_path = backing.join("mount.json");
    let fresh = !manifest_path.exists();
    if fresh
        && backing.exists()
        && std::fs::read_dir(&backing)?.any(|entry| {
            entry.is_err() || entry.is_ok_and(|entry| entry.file_name() != "mount.lock")
        })
    {
        return Err(invalid("new backing directory must be empty"));
    }
    std::fs::create_dir_all(&backing)?;
    std::fs::set_permissions(&backing, std::fs::Permissions::from_mode(0o700))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(backing.join("mount.lock"))?;
    lock.try_lock()
        .map_err(|_| invalid("virtual working copy is already mounted"))?;
    let mut remote = Remote::connect(q.server, q.objects, q.repo.clone(), q.token).await?;
    let manifest = if fresh {
        if mounted(&mountpoint)?.is_some() {
            return Err(invalid("mountpoint is already mounted"));
        }
        let (id, reference) = remote.resolve(q.revision).await?;
        let manifest = Manifest {
            version: 1,
            server: q.server.to_owned(),
            objects: q.objects.to_owned(),
            repo: q.repo,
            revision: q.revision.to_vec(),
            origin: id.0.to_vec(),
            reference: reference.unwrap_or_default(),
            sparse: q.sparse,
            mountpoint: mountpoint.clone(),
        };
        save(&manifest_path, &manifest)?;
        manifest
    } else {
        let manifest: Manifest = serde_json::from_slice(&std::fs::read(&manifest_path)?)?;
        if manifest.version != 1
            || manifest.server != q.server
            || manifest.objects != q.objects
            || manifest.repo != q.repo
            || manifest.revision != q.revision
            || manifest.sparse != q.sparse
            || manifest.mountpoint != mountpoint
        {
            return Err(invalid(
                "mount arguments differ from the persisted virtual working copy",
            ));
        }
        recover_mounts(&manifest, &backing)?;
        manifest
    };
    std::fs::create_dir_all(&mountpoint)?;
    if std::fs::read_dir(&mountpoint)?.next().is_some() {
        return Err(invalid("mountpoint must be empty"));
    }
    let upper = backing.join("upper");
    let lower_path = backing.join("lower");
    let work = backing.join("work");
    for path in [&lower_path, &work, &upper.join(".kasu/objects")] {
        std::fs::create_dir_all(path)?;
    }
    std::fs::set_permissions(upper.join(".kasu"), std::fs::Permissions::from_mode(0o700))?;
    let cache = FsBlobStore::open(upper.join(".kasu/objects"))?;
    let id = ChangesetId(
        manifest
            .origin
            .as_slice()
            .try_into()
            .map_err(|_| invalid("invalid virtual origin"))?,
    );
    let snapshot: Snapshot = remote
        .metadata_snapshot(&cache, id, &manifest.sparse)
        .await?;
    let ids: BTreeSet<_> = snapshot.values().filter_map(Entry::blob).collect();
    let sizes = remote
        .blob_sizes(&ids.into_iter().collect::<Vec<BlobId>>())
        .await?;
    let state_path = upper.join(".kasu/state.json");
    if !state_path.exists() {
        save(
            &state_path,
            &State {
                version: 1,
                generation: 0,
                server: manifest.server.clone(),
                objects: manifest.objects.clone(),
                repo: manifest.repo.clone(),
                base: manifest.origin.clone(),
                reference: manifest.reference.clone(),
                draft: false,
                sparse: manifest.sparse.clone(),
                files: snapshot
                    .iter()
                    .map(|(path, entry)| Tracked {
                        path: path.clone(),
                        entry: entry.encode(),
                        stamp: Default::default(),
                    })
                    .collect(),
                land_key: None,
                land_outcome: None,
            },
        )?;
    }
    let owner = std::fs::metadata(&upper)?;
    let fs = lower::Lower::new(snapshot, sizes, remote, owner.uid(), owner.gid())?;
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        fuser::MountOption::RO,
        fuser::MountOption::NoDev,
        fuser::MountOption::NoSuid,
        fuser::MountOption::DefaultPermissions,
        fuser::MountOption::FSName("kasu-virtual".into()),
        fuser::MountOption::Subtype("kasu-virtual".into()),
    ];
    let session = fuser::spawn_mount(fs, &lower_path, &config)?;
    let options = format!(
        "lowerdir={},upperdir={},workdir={},index=off,xino=off,metacopy=off",
        lower_path.display(),
        upper.display(),
        work.display()
    );
    if let Err(error) = command(
        "mount",
        &[
            "-t".as_ref(),
            "overlay".as_ref(),
            "kasu-virtual".as_ref(),
            "-o".as_ref(),
            options.as_ref(),
            mountpoint.as_os_str(),
        ],
    ) {
        session.umount_and_join()?;
        return Err(error);
    }
    println!("mounted {id} at {}", mountpoint.display());
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! { result = tokio::signal::ctrl_c() => { result?; }, _ = term.recv() => {} }
        if mounted(&mountpoint)?.is_none() {
            break;
        }
        match command("umount", &[mountpoint.as_os_str()]) {
            Ok(()) => break,
            Err(error) => eprintln!("kasu: {error}; close open files and signal again to unmount"),
        }
    }
    session.umount_and_join()?;
    drop(lock);
    Ok(())
}
