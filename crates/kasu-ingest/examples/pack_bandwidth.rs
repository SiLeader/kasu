//! Compare the same object's non-delta and delta pack sizes on a complete Git fixture.
use kasu_core::{GitOid, Result};
use kasu_derived::Engine;
use kasu_git::*;
use kasu_ingest::Importer;
use kasu_store::{FsBlobStore, Repository, SqliteStore};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::Instant,
};

fn git(path: &Path, args: &[&str], input: &[u8]) -> Vec<u8> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Writing and reading concurrently also handles large object lists/packs.
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 {
        eprintln!("usage: pack_bandwidth <complete-bare-git-repository> <pinned-commit>");
        std::process::exit(2);
    }
    let source = Path::new(&args[0]);
    let tip = oid_from_hex(args[1].as_encoded_bytes())?;
    let dir = tempfile::tempdir().unwrap();
    let meta = SqliteStore::open(dir.path().join("meta.db"))?;
    let blobs = FsBlobStore::open(dir.path().join("objects"))?;
    let mut engine = Engine::standard();
    register_git(&mut engine);
    let index = GitIndex {
        repo: Repository {
            meta: &meta,
            blobs: &blobs,
        },
        projection: ProjectionId::full(b"bandwidth".as_slice()),
    };
    let importer = Importer {
        engine: &engine,
        index,
    };
    let report = importer.import(source)?;
    eprintln!(
        "imported commits={} objects={}",
        report.commits, report.objects
    );
    let index = &importer.index;
    let refs = [ResolvedRef {
        name: b"refs/heads/main".to_vec(),
        oid: Some(tip),
        peeled: None,
        symbolic: None,
    }];
    println!(
        "scenario,tip,have,objects,kasu_bytes,git_no_delta_bytes,git_delta_bytes,plan_us,warm_plan_us,cold_pack_us,warm_pack_us"
    );
    for (name, depth, distance) in [
        ("full", None, None),
        ("depth1", Some(1), None),
        ("incremental1", None, Some(1)),
        ("incremental10", None, Some(10)),
    ] {
        let have = distance.map(|distance| {
            oid_from_hex(
                git(source, &["rev-parse", &format!("{tip}~{distance}")], b"").trim_ascii(),
            )
            .unwrap()
        });
        let q = FetchRequest {
            wants: vec![tip],
            haves: have.into_iter().collect(),
            depth,
            done: true,
            ..Default::default()
        };
        let started = Instant::now();
        let plan = plan_fetch(index, &refs, &q)?;
        let plan_us = started.elapsed().as_micros();
        let started = Instant::now();
        let warm_plan = plan_fetch(index, &refs, &q)?;
        let warm_plan_us = started.elapsed().as_micros();
        assert_eq!(warm_plan.objects, plan.objects);
        if depth.is_none() {
            let mut revisions = vec!["rev-list", "--objects", "--no-object-names"];
            let tip_arg = tip.to_string();
            let have_arg = have.map(|oid| format!("^{oid}"));
            revisions.push(&tip_arg);
            if let Some(have) = &have_arg {
                revisions.push(have);
            }
            let expected: std::collections::BTreeSet<_> = git(source, &revisions, b"")
                .split(|b| *b == b'\n')
                .filter(|line| !line.is_empty())
                .map(|line| oid_from_hex(line).unwrap())
                .collect();
            assert_eq!(
                plan.objects, expected,
                "object selection differs from Git rev-list"
            );
        }
        let cache = PackCache::new(32 * 1024 * 1024);
        let started = Instant::now();
        let mut pack = Vec::new();
        write_pack_cached(index, plan.objects.iter().copied(), Some(&cache), &mut pack)?;
        let cold_us = started.elapsed().as_micros();
        let started = Instant::now();
        let mut warm = Vec::new();
        write_pack_cached(index, plan.objects.iter().copied(), Some(&cache), &mut warm)?;
        let warm_us = started.elapsed().as_micros();
        assert_eq!(warm, pack);
        // Verify each emitted pack using Git's strict parser. The disposable
        // source already contains the parent objects of incremental packs.
        git(source, &["index-pack", "--strict", "--stdin"], &pack);
        let list: Vec<_> = plan
            .objects
            .iter()
            .flat_map(|oid| format!("{oid}\n").into_bytes())
            .collect();
        let sizes: Vec<_> = ["--window=0", "--window=10"]
            .into_iter()
            .map(|window| {
                git(
                    source,
                    &[
                        "-c",
                        "pack.compression=1",
                        "pack-objects",
                        "--stdout",
                        "--no-reuse-object",
                        "--no-reuse-delta",
                        "--depth=50",
                        window,
                    ],
                    &list,
                )
                .len()
            })
            .collect();
        println!(
            "{name},{tip},{},{},{},{},{},{plan_us},{warm_plan_us},{cold_us},{warm_us}",
            have.map(|oid: GitOid| oid.to_string()).unwrap_or_default(),
            plan.objects.len(),
            pack.len(),
            sizes[0],
            sizes[1]
        );
    }
    Ok(())
}
