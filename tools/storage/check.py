"""Run the same storage contracts and real-server flow on host or Linux."""
import json
import os
import pathlib
import subprocess

root = pathlib.Path(__file__).resolve().parents[2]
env = dict(os.environ, RUSTC_WRAPPER='')
prebuilt = pathlib.Path('/out/tests.json')
if env.get('KASU_PREBUILT_TESTS') == '1':
    env['KASU_TEST_INGEST_BIN'] = '/out/kasu-ingest'
    for executable in json.loads(prebuilt.read_text()):
        subprocess.run([executable, '--include-ignored'], env=env, check=True, cwd=root)
else:
    subprocess.run(['cargo', 'build', '-p', 'kasu-server', '-p', 'kasu-ingest', '--features', 'production', '--bins', '--locked'], env=env, check=True, cwd=root)
    env['KASU_TEST_INGEST_BIN'] = str(root / 'target/debug/kasu-ingest')
    subprocess.run(['cargo', 'test', '-p', 'kasu-store', '--features', 'production', '--locked', '--', '--include-ignored'], env=env, check=True, cwd=root)
    subprocess.run(['cargo', 'test', '-p', 'kasu-server', '--features', 'production', '--test', 'shared_storage', '--locked', '--', '--include-ignored'], env=env, check=True, cwd=root)
