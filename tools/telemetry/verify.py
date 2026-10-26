#!/usr/bin/env python3
"""Real-process OTLP/HTTP acceptance using the official Rust protobuf decoder."""
import argparse
import http.client
import http.server
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile
import threading
import time


def wait_for(predicate, timeout=12):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.01)
    raise AssertionError("acceptance wait timed out")


class Collector:
    def __init__(self):
        self.lock = threading.Lock()
        self.requests = []
        self.redirect = None
        self.trap = 0
        self.delay = 0
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                size = int(self.headers.get("content-length", "0"))
                assert 0 <= size <= 16 * 1024 * 1024
                raw = self.rfile.read(size)
                with owner.lock:
                    owner.requests.append((self.path, raw, self.headers.get("authorization")))
                    redirect = owner.redirect
                    if self.path == "/trap":
                        owner.trap += 1
                if owner.delay:
                    time.sleep(owner.delay)
                self.send_response(307 if redirect else 200)
                if redirect:
                    self.send_header("location", redirect)
                self.send_header("content-type", "application/x-protobuf")
                self.send_header("content-length", "0")
                self.end_headers()

        self.http = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.http.daemon_threads = True
        self.thread = threading.Thread(target=self.http.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.http.server_port}"

    def close(self):
        self.http.shutdown()
        self.http.server_close()
        self.thread.join(2)

    def decode(self, decoder):
        with self.lock:
            requests = list(self.requests)
        spans = []
        for path, raw, authorization in requests:
            assert path == "/base/v1/traces", path
            assert authorization == "Bearer collector-auth-SENTINEL"
            result = subprocess.run([decoder], input=raw, capture_output=True, check=True)
            spans.extend(json.loads(result.stdout))
            for sentinel in [b"user-pat-SENTINEL", b"cache-hop-SENTINEL", b"authority-hop-SENTINEL", b"collector-auth-SENTINEL", b"PRIVATE_BAGGAGE_SENTINEL", b"NON_SPARSE_FILE_SENTINEL"]:
                assert sentinel not in raw, sentinel
        return spans


def verify(binary, out):
    binary = binary.resolve()
    out.mkdir(parents=True, exist_ok=True)
    collector = Collector()
    processes = []
    logs = []
    env = {k: v for k, v in os.environ.items() if not k.startswith(("KASU_", "OTEL_"))}
    env.update(OTEL_TRACES_EXPORTER="otlp", OTEL_EXPORTER_OTLP_ENDPOINT=collector.url + "/base", OTEL_EXPORTER_OTLP_HEADERS="authorization=Bearer collector-auth-SENTINEL", RUST_LOG="info")
    durations = {}

    def run(name, args, changes=None, expected=0):
        result = subprocess.run([str(binary / name), *map(str, args)], env=env | (changes or {}), capture_output=True, timeout=15)
        with (out / f"{name}-commands.log").open("ab") as log:
            log.write(result.stdout + result.stderr)
        assert result.returncode == expected, (name, result.returncode, result.stdout, result.stderr)
        return result

    def start(name, args, changes=None):
        path = out / f"{name}.log"
        log = path.open("wb")
        logs.append(log)
        child = subprocess.Popen([str(binary / name), *map(str, args)], env=env | (changes or {}), stdout=log, stderr=log)
        processes.append(child)
        return child, path

    def address(child, log, key):
        def ready():
            assert child.poll() is None, log.read_text()
            match = re.search(rf"\b{key}=([^\s]+)", log.read_text())
            return match.group(1) if match else None
        return wait_for(ready)

    def stop(child):
        child.send_signal(signal.SIGINT)
        assert child.wait(timeout=8) == 0

    try:
        with tempfile.TemporaryDirectory(prefix="kt-", dir="/tmp") as temporary:
            root = Path(temporary)
            source = root / "source"
            source.mkdir()
            git_env = env | dict(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null", GIT_AUTHOR_NAME="A", GIT_COMMITTER_NAME="A", GIT_AUTHOR_EMAIL="a@b", GIT_COMMITTER_EMAIL="a@b", GIT_AUTHOR_DATE="2000-01-01T00:00:00Z", GIT_COMMITTER_DATE="2000-01-01T00:00:00Z")
            def git(*args):
                subprocess.run(["git", "-C", str(source), *args], env=git_env, check=True, capture_output=True)
            git("init", "--initial-branch=main")
            (source / "app").mkdir()
            (source / "app/a.txt").write_text("telemetry fixture\n")
            (source / "private.txt").write_text("NON_SPARSE_FILE_SENTINEL\n")
            git("add", ".")
            git("commit", "-m", "fixture")
            run("kasu-ingest", [source, root / "data", "repo"])
            for name, value in [("user", "user-pat-SENTINEL"), ("cache", "cache-hop-SENTINEL"), ("authority", "authority-hop-SENTINEL")]:
                (root / name).write_text(value)
            server, server_log = start("kasu-server", [root / "data", "repo", "127.0.0.1:0", "127.0.0.1:0"], dict(KASU_TOKEN="user-pat-SENTINEL", KASU_OBJECT_TOKEN_FILE=str(root / "authority")))
            grpc = address(server, server_log, "grpc")
            http_address = address(server, server_log, "http")
            cache, cache_log = start("kasu-cache", ["--repo", "repo", "--upstream", f"http://{grpc}", "--token-file", root / "cache", "--upstream-token-file", root / "authority", "--listen", "127.0.0.1:0"])
            upper = address(cache, cache_log, "address")
            daemon, daemon_log = start("kasu-daemon", ["serve", "--repo", "repo", "--upstream", f"http://{upper}", "--token-file", root / "user", "--upstream-token-file", root / "cache", "--data", root / "disk", "--socket", root / "node.sock"])
            address(daemon, daemon_log, "socket")
            cli = dict(KASU_TOKEN="user-pat-SENTINEL", KASU_OBJECTS_ENDPOINT=f"unix://{root / 'node.sock'}", RUST_LOG="off")
            for name in ["cold", "warm"]:
                run("kasu", ["checkout", "main", root / name, "--repo", "repo", "--server", f"http://{grpc}", "--sparse", "app"], cli)
                assert (root / name / "app/a.txt").read_text() == "telemetry fixture\n"
                assert not (root / name / "private.txt").exists()

            payload = json.dumps(dict(repo=list(b"repo"), prefix=[])).encode()
            known = "11111111111111111111111111111111"
            def request(parent, auth=True, duplicate=False):
                connection = http.client.HTTPConnection(http_address, timeout=5)
                connection.putrequest("POST", "/v1/refs")
                connection.putheader("content-type", "application/json")
                connection.putheader("content-length", str(len(payload)))
                if auth:
                    connection.putheader("authorization", "Bearer user-pat-SENTINEL")
                connection.putheader("traceparent", parent)
                if duplicate:
                    connection.putheader("traceparent", parent)
                connection.putheader("tracestate", "vendor=acceptance")
                connection.putheader("baggage", "secret=PRIVATE_BAGGAGE_SENTINEL")
                connection.endheaders(payload)
                response = connection.getresponse()
                status = response.status
                response.read()
                connection.close()
                return status
            assert request(f"00-{known}-2222222222222222-01") == 200
            assert request(f"00-{known}-2222222222222222-01", auth=False) == 401
            assert request("malformed") == 200
            assert request(f"00-{known}-2222222222222222-01", duplicate=True) == 200
            assert request("00-33333333333333333333333333333333-4444444444444444-00") == 200
            time.sleep(1.2)
            spans = collector.decode(binary / "examples/inspect_otlp")
            cli_roots = sorted([s for s in spans if s["resource"]["service.name"] == "kasu-cli" and s["name"] == "operation"], key=lambda s: s["start_nanos"])
            assert len(cli_roots) == 2, cli_roots
            cold = [s for s in spans if s["trace_id"] == cli_roots[0]["trace_id"]]
            warm = [s for s in spans if s["trace_id"] == cli_roots[1]["trace_id"]]
            services = lambda rows: {s["resource"]["service.name"] for s in rows}
            assert services(cold) == {"kasu-cli", "kasu-daemon", "kasu-cache", "kasu-server"}, services(cold)
            assert services(warm) == {"kasu-cli", "kasu-daemon", "kasu-server"}, services(warm)
            by_id = {s["span_id"]: s for s in cold}
            for span in cold:
                if span["parent_span_id"]:
                    assert span["parent_span_id"] in by_id, span
                    parent = by_id[span["parent_span_id"]]
                    assert parent["trace_id"] == span["trace_id"]
                    assert parent["start_nanos"] <= span["start_nanos"] <= span["end_nanos"] <= parent["end_nanos"], (parent, span)
            for lower, upper in [("kasu-cli", "kasu-daemon"), ("kasu-daemon", "kasu-cache"), ("kasu-cache", "kasu-server")]:
                assert any(s["resource"]["service.name"] == upper and s["kind"] == 2 and by_id.get(s["parent_span_id"], {}).get("resource", {}).get("service.name") == lower for s in cold), (lower, upper)
            remote = [s for s in spans if s["trace_id"] == known]
            assert len(remote) == 2 and {s["attributes"]["kasu.code"] for s in remote} == {0, 16}
            assert all(s["parent_span_id"] == "2222222222222222" for s in remote)
            assert all(s["trace_id"] != "33333333333333333333333333333333" for s in spans)
            roots = [s for s in spans if s["resource"]["service.name"] == "kasu-server" and s["attributes"].get("transport") == "http" and not s["parent_span_id"]]
            assert len(roots) == 2, roots
            assert all(s["events"] == 0 and s["links"] == 0 and len(s["attributes"]) <= 16 for s in spans)
            assert services(spans) == {"kasu-cli", "kasu-daemon", "kasu-cache", "kasu-server", "kasu-ingest"}
            (out / "spans.json").write_text(json.dumps(spans, indent=2) + "\n")

            # Unsampled command context also suppresses downstream spans.
            before = len(spans)
            run("kasu", ["-C", root / "cold", "log", "--limit", "1"], cli | dict(OTEL_TRACES_SAMPLER="always_off"))
            time.sleep(1.2)
            assert len(collector.decode(binary / "examples/inspect_otlp")) == before
            # Disabled CLI produces no CLI spans; the independently enabled server can still trace its request.
            run("kasu", ["-C", root / "cold", "log", "--limit", "1"], cli | dict(OTEL_TRACES_EXPORTER="none"))
            time.sleep(1.2)
            assert len([s for s in collector.decode(binary / "examples/inspect_otlp") if s["resource"]["service.name"] == "kasu-cli"]) == len([s for s in spans if s["resource"]["service.name"] == "kasu-cli"])
            bad = run("kasu", ["-C", root / "cold", "status"], cli | dict(OTEL_EXPORTER_OTLP_TRACES_ENDPOINT="http://user:CONFIG_SECRET_SENTINEL@collector/"), expected=1)
            assert b"CONFIG_SECRET_SENTINEL" not in bad.stdout + bad.stderr
            # A stalled receiver is bounded by the export HTTP timeout.
            collector.delay = 4
            started = time.monotonic()
            run("kasu", ["-C", root / "cold", "log", "--limit", "1"], cli)
            durations["stalled_seconds"] = time.monotonic() - started
            assert 1.5 <= durations["stalled_seconds"] < 5
            collector.delay = 0
            # A collector redirect must not receive the configured authorization header at another URL.
            collector.redirect = collector.url + "/trap"
            started = time.monotonic()
            run("kasu", ["-C", root / "cold", "log", "--limit", "1"], cli)
            durations["redirect_seconds"] = time.monotonic() - started
            assert durations["redirect_seconds"] < 5
            assert collector.trap == 0
            collector.close()
            started = time.monotonic()
            run("kasu", ["-C", root / "cold", "log", "--limit", "1"], cli)
            durations["unavailable_seconds"] = time.monotonic() - started
            assert durations["unavailable_seconds"] < 5
            for child in reversed(processes):
                stop(child)
            processes.clear()
            result = dict(platform=os.uname().sysname + " " + os.uname().machine, spans=len(spans), cold_trace_spans=len(cold), warm_trace_spans=len(warm), services=sorted(services(spans)), remote_parent_statuses=[0, 16], ignored_unsampled=True, invalid_and_duplicate_context_new_roots=True, disabled_cli_no_export=True, no_baggage_or_credentials=True, no_redirect_followed=True, graceful_shutdown=True, durations=durations)
            (out / "result.json").write_text(json.dumps(result, indent=2) + "\n")
            print(json.dumps(result, indent=2))
    finally:
        for child in reversed(processes):
            if child.poll() is None:
                child.send_signal(signal.SIGINT)
                try:
                    child.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()
        for log in logs:
            log.close()
        if collector.thread.is_alive():
            collector.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    verify(args.bin_dir, args.out)
