#!/usr/bin/env python3
"""Run the same checkout fixture on Linux when macOS rejects non-UTF-8 names.

KASU_LINUX_GO_GIT_BINARY points to a statically linked linux/arm64 client.
The entire checkout happens on the container filesystem; ASCII artifacts and
.git are copied back so the host can also run strict Git verification.
"""
import os
import io
import tarfile
from pathlib import Path
import subprocess
import sys
from urllib.parse import urlsplit, urlunsplit

url, destination, depth = sys.argv[1:]
parts = urlsplit(url)
url = urlunsplit((parts.scheme, f"host.docker.internal:{parts.port}", parts.path, parts.query, parts.fragment))
Path(destination).mkdir(parents=True)
command = [
    "docker", "run", "--rm", "--pull=never",
    "--mount", f"type=bind,source={os.environ['KASU_LINUX_GO_GIT_BINARY']},target=/client,readonly",
    "--env", "KASU_TOKEN", "--env", f"KASU_URL={url}", "--env", f"KASU_DEPTH={depth}",
    "--entrypoint", "/bin/sh", os.environ.get("KASU_GO_GIT_IMAGE", "rust:1.90-bookworm"),
    "-ceu", '/client "$KASU_URL" /checkout "$KASU_DEPTH"; tar -C /checkout -cf - .git manifest.yaml symlink',
]
result = subprocess.run(command, stdout=subprocess.PIPE)
if result.returncode:
    sys.exit(result.returncode)
head, separator, archive = result.stdout.partition(b"\n")
if not separator or len(head) != 40:
    raise RuntimeError("Linux client did not return an OID followed by artifacts")
with tarfile.open(fileobj=io.BytesIO(archive)) as artifacts:
    artifacts.extractall(destination, filter="data")
sys.stdout.buffer.write(head + b"\n")
