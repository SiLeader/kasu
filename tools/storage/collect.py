"""Collect Cargo's actual test artifacts for the fixed-toolchain container."""
import json
import pathlib
import shutil
import sys

out = pathlib.Path('/out')
out.mkdir(exist_ok=True)
manifest = []
for group, log in [('store', sys.argv[1]), ('shared', sys.argv[2])]:
    for line in pathlib.Path(log).read_text().splitlines():
        artifact = json.loads(line)
        if artifact.get('reason') != 'compiler-artifact' or not artifact.get('executable') or not artifact['profile']['test']:
            continue
        target = artifact['target']
        if not set(target['kind']).intersection({'lib', 'test'}):
            continue
        destination = out / f"test-{group}-{target['name']}"
        shutil.copy2(artifact['executable'], destination)
        manifest.append(str(destination))
required = {'test-store-kasu_store', 'test-store-postgres', 'test-store-s3', 'test-shared-shared_storage'}
if not required.issubset({pathlib.Path(path).name for path in manifest}):
    raise RuntimeError('Cargo did not produce all required production test binaries')
(out / 'tests.json').write_text(json.dumps(manifest))
shutil.copy2('/app/target/debug/kasu-server', out / 'kasu-server')
shutil.copy2('/app/target/debug/kasu-ingest', out / 'kasu-ingest')
