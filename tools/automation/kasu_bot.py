#!/usr/bin/env python3
"""Reference metadata test gate, DAG bisect and journaled guarded revert."""
import argparse
import contextlib
import fcntl
import hashlib
import heapq
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request


class BotError(Exception):
    pass


class Retry(BotError):
    pass


class Rejected(BotError):
    def __init__(self, method, status):
        self.status = status
        super().__init__(f"metadata operation {method} rejected (HTTP {status})")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, message, headers, newurl):
        return None


def object_id(value):
    try:
        if isinstance(value, str):
            if len(value) != 64:
                raise BotError("changeset ID must be 64 hex characters")
            raw = bytes.fromhex(value)
        elif isinstance(value, (bytes, bytearray, list, tuple)) and len(value) == 32:
            if any(type(part) is not int or not 0 <= part <= 255 for part in value):
                raise BotError("invalid changeset ID bytes")
            raw = bytes(value)
        else:
            raise BotError("changeset ID must contain 32 bytes")
    except (ValueError, TypeError):
        raise BotError("invalid changeset ID") from None
    if len(raw) != 32:
        raise BotError("changeset ID must be 32 bytes")
    return raw.hex()


def wire(value):
    return list(bytes.fromhex(object_id(value)))


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


class Budget:
    def __init__(self, seconds):
        if not 0 < seconds <= 86400:
            raise BotError("timeout must be in (0, 86400] seconds")
        self.until = time.monotonic() + seconds

    def remaining(self):
        value = self.until - time.monotonic()
        if value <= 0:
            raise Retry("operation deadline exceeded; journal can be resumed")
        return value


class Client:
    def __init__(self, endpoint, repo, token_file, budget):
        parsed = urllib.parse.urlsplit(endpoint)
        if parsed.scheme not in ("http", "https") or not parsed.hostname or parsed.username is not None or parsed.password is not None or parsed.query or parsed.fragment:
            raise BotError("endpoint must be an HTTP(S) URL without credentials, query or fragment")
        self.endpoint, self.repo, self.budget = endpoint.rstrip("/"), list(repo.encode()), budget
        self.opener = urllib.request.build_opener(NoRedirect())
        self.token = None
        if token_file:
            try:
                with open(token_file, "rb") as source:
                    data = source.read(4097)
                token = data.rstrip(b"\r\n").decode("ascii")
            except (OSError, UnicodeError):
                raise BotError("cannot read token file") from None
            if len(data) > 4096 or not token or any(ord(c) < 32 or ord(c) == 127 for c in token):
                raise BotError("invalid token file")
            self.token = token

    def call(self, method, **fields):
        payload = json.dumps(dict(repo=self.repo, **fields)).encode()
        request = urllib.request.Request(self.endpoint + "/v1/" + method, data=payload, headers={"Content-Type": "application/json"})
        if self.token:
            request.add_header("Authorization", "Bearer " + self.token)
        try:
            with self.opener.open(request, timeout=min(30, self.budget.remaining())) as response:
                body = bytearray()
                while True:
                    self.budget.remaining()
                    chunk = response.read1(65536)
                    if not chunk:
                        break
                    body.extend(chunk)
                    if len(body) > 16 * 1024 * 1024:
                        raise BotError("metadata response exceeds 16 MiB")
        except urllib.error.HTTPError as error:
            if error.code >= 500 or error.code in (408, 429):
                raise Retry("metadata service temporarily unavailable") from None
            raise Rejected(method, error.code) from None
        except (OSError, urllib.error.URLError, TimeoutError):
            raise Retry("metadata transport failed; journal can be resumed") from None
        try:
            return json.loads(body)
        except (ValueError, UnicodeError):
            raise BotError("invalid metadata response") from None


class Journal:
    def __init__(self, path, binding):
        self.path = Path(path)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.lock = open(str(self.path) + ".lock", "a+b")
        try:
            fcntl.flock(self.lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            self.lock.close()
            raise BotError("journal is already in use") from None
        fingerprint = digest(binding)
        try:
            if self.path.exists():
                with self.path.open("rb") as source:
                    data = source.read(16 * 1024 * 1024 + 1)
                if len(data) > 16 * 1024 * 1024:
                    raise BotError("journal exceeds limit")
                self.data = json.loads(data)
                if not isinstance(self.data, dict) or self.data.get("schema") != 1:
                    raise BotError("invalid journal schema")
                if self.data.get("binding") != fingerprint:
                    raise BotError("journal belongs to a different operation or test configuration")
            else:
                self.data = {"schema": 1, "binding": fingerprint}
                self.save()
        except BaseException:
            self.close()
            raise

    def close(self):
        self.lock.close()

    def save(self):
        encoded = json.dumps(self.data, sort_keys=True, indent=2).encode() + b"\n"
        if len(encoded) > 16 * 1024 * 1024:
            raise BotError("journal exceeds 16 MiB; previous checkpoint retained")
        fd, temporary = tempfile.mkstemp(prefix=".kasu-bot-", dir=self.path.parent)
        try:
            with os.fdopen(fd, "wb") as target:
                target.write(encoded)
                target.flush()
                os.fsync(target.fileno())
            os.replace(temporary, self.path)
            directory = os.open(self.path.parent, os.O_RDONLY)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
        finally:
            with contextlib.suppress(FileNotFoundError):
                os.unlink(temporary)


def ancestors(graph, roots, budget):
    seen, pending = set(), list(roots)
    steps = 0
    while pending:
        steps += 1
        node = pending.pop()
        if node not in seen:
            seen.add(node)
            pending.extend(graph[node])
        if steps % 256 == 1:
            budget.remaining()
    return seen


def load_graph(client, bad, limit):
    graph, pending = {}, [bad]
    while pending:
        client.budget.remaining()
        node = pending.pop()
        if node in graph:
            continue
        if len(graph) >= limit:
            raise BotError("bisect graph exceeds configured changeset limit")
        cs = client.call("changeset", id=wire(node))
        if object_id(cs["id"]) != node:
            raise BotError("metadata changeset ID mismatch")
        parents = [object_id(parent) for parent in cs.get("parents", [])]
        graph[node] = parents
        pending.extend(parents)
    children = {node: [] for node in graph}
    degrees = {node: len(set(parents)) for node, parents in graph.items()}
    for node, parents in graph.items():
        for parent in set(parents):
            children[parent].append(node)
    ready = [node for node in graph if not degrees[node]]
    heapq.heapify(ready)
    ordered = []
    while ready:
        client.budget.remaining()
        node = heapq.heappop(ready)
        ordered.append(node)
        for child in children[node]:
            degrees[child] -= 1
            if not degrees[child]:
                heapq.heappush(ready, child)
    if len(ordered) != len(graph):
        raise BotError("metadata history is not a DAG")
    return graph, ordered


class Tester:
    def __init__(self, command, journal, budget, seconds, endpoint, repo, token_file):
        self.command, self.journal, self.budget, self.seconds = command, journal, budget, seconds
        self.environment = dict(os.environ, KASU_METADATA_ENDPOINT=endpoint, KASU_REPO=repo)
        if token_file:
            self.environment["KASU_TOKEN_FILE"] = str(Path(token_file).resolve())

    def run(self, node):
        known = self.journal.data.setdefault("verdicts", {})
        if node in known:
            return known[node]
        try:
            process = subprocess.Popen(self.command, env=dict(self.environment, KASU_CHANGESET=node), stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        except OSError:
            raise BotError("could not start tester") from None
        try:
            result = process.wait(timeout=min(self.seconds, self.budget.remaining()))
        except (subprocess.TimeoutExpired, Retry):
            raise Retry("tester deadline exceeded; this revision has no cached verdict") from None
        finally:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGKILL)
            process.wait()
        self.budget.remaining()
        if result not in (0, 1, 125):
            raise BotError(f"tester aborted with exit code {result}; revision was not classified")
        known[node] = result
        self.journal.save()
        return result


def bisect(client, good, bad, tester, max_nodes=10000, max_tests=64):
    graph, order = load_graph(client, bad, max_nodes)
    verdicts = tester.journal.data.setdefault("verdicts", {})
    if not isinstance(verdicts, dict) or any(object_id(key) != key or type(value) is not int or value not in (0, 1, 125) for key, value in verdicts.items()):
        raise BotError("invalid cached test verdicts")
    for node in good:
        if node not in graph or node == bad:
            raise BotError("good boundaries must be strict ancestors of bad")
        if tester.run(node) != 0:
            raise BotError("declared good boundary did not pass")
    if tester.run(bad) != 1:
        raise BotError("declared bad boundary did not fail")
    remaining = set(graph) - ancestors(graph, good, client.budget)
    verdicts = tester.journal.data["verdicts"]
    tests = 0
    while True:
        # Reapply cached decisions on resume, including all DAG parents.
        for node, verdict in verdicts.items():
            if node not in graph:
                raise BotError("journal verdict is outside the selected history")
            if verdict == 0:
                remaining -= ancestors(graph, [node], client.budget)
            elif verdict == 1:
                remaining &= ancestors(graph, [node], client.budget)
        if not remaining:
            raise BotError("test verdicts are inconsistent with monotonic bisection")
        if len(remaining) == 1 and verdicts.get(next(iter(remaining))) == 1:
            return {"status": "culprit", "id": next(iter(remaining)), "tested": len(verdicts)}
        available = [node for node in order if node in remaining and node not in verdicts]
        if not available:
            return {"status": "ambiguous", "candidates": sorted(remaining), "tested": len(verdicts)}
        if tests >= max_tests:
            raise Retry("bisect test budget exhausted; journal can be resumed")
        total = len(remaining)
        best, balance = None, -1
        for node in available:
            weight = len(ancestors(graph, [node], client.budget) & remaining)
            score = min(weight, total - weight)
            if score > balance:
                best, balance = node, score
        tester.run(best)
        tests += 1


def revert(client, bad, branch, mainline, operation_id, journal):
    state = journal.data
    if "response" in state:
        return state["response"]
    if "metadata" not in state:
        stamp = int(time.time())
        identity = dict(name=list(b"kasu revert bot"), email=list(b"kasu-bot@localhost"), timestamp=stamp, tz=list(b"+0000"))
        state["metadata"] = dict(author=identity, committer=identity, message=list(f"Revert {bad}\n\nOperation: {operation_id}\n".encode()))
        journal.save()
    metadata = state["metadata"]
    if "inverse" not in state:
        original = client.call("changeset", id=wire(bad))
        if object_id(original["id"]) != bad:
            raise BotError("metadata changeset ID mismatch")
        parents = original.get("parents", [])
        if (len(parents) > 1 and mainline is None) or (mainline is not None and not 1 <= mainline <= len(parents)):
            raise BotError("merge revert requires an explicit valid 1-based mainline parent")
        if parents:
            parent = parents[(mainline or 1) - 1]
            # Materialize repository membership for historical blob IDs reused
            # by CreateChangeset, without downloading any tree/blob contents.
            client.call("root-tree", id=parent)
            changes = [dict(path=item["path"], entry=item.get("after")) for item in client.call("diff", base=wire(bad), head=parent, content=False).get("files", [])]
        else:
            changes = [dict(path=item["path"], entry=None) for item in original.get("changes", [])]
        state["inverse"] = object_id(client.call("create-changeset", parents=[wire(bad)], changes=changes, metadata=metadata)["id"])
        journal.save()
    if "land_id" not in state:
        if "onto" not in state:
            reference = client.call("resolve-ref", name=list(branch.encode()))
            if not reference.get("target"):
                raise BotError("revert target branch has no head")
            head = object_id(reference["target"])
            if not client.call("is-ancestor", base=wire(bad), head=wire(head)).get("is_ancestor"):
                raise BotError("bad changeset is not an ancestor of the target branch")
            state["onto"] = head
            journal.save()
        rebased = client.call("rebase", id=wire(state["inverse"]), onto=wire(state["onto"]), committer=metadata["committer"])
        if not rebased.get("id"):
            result = dict(status="conflict", phase="rebase", conflicts=rebased.get("conflicts", []))
            state["response"] = result
            journal.save()
            return result
        state["land_id"] = object_id(rebased["id"])
        state["land_key"] = digest(dict(domain="kasu-revert-v1", repo=client.repo, operation_id=operation_id))
        journal.save()
    result = client.call("guarded-land", id=wire(state["land_id"]), name=list(branch.encode()), idempotency_key=list(bytes.fromhex(state["land_key"])), required_ancestor=wire(bad), max_attempts=8, timeout_ms=5000)
    outcome = result.get("outcome", 0)
    names = {0: "landed", 1: "conflict", 2: "retry", 3: "denied"}
    if outcome not in names:
        raise BotError("unknown Land outcome")
    response = dict(status=names[outcome], result=result, operation_id=operation_id)
    if outcome in (0, 1):
        state["response"] = response
        journal.save()
    return response


def check(client, revision, branch, tester):
    if object_id(client.call("changeset", id=wire(revision))["id"]) != revision:
        raise BotError("metadata changeset ID mismatch")
    before = None
    if branch is not None:
        before = client.call("resolve-ref", name=list(branch.encode()))
        if not before.get("target") or object_id(before["target"]) != revision:
            return dict(status="stale", id=revision)
    verdict = tester.run(revision)
    if branch is not None:
        after = client.call("resolve-ref", name=list(branch.encode()))
        if before.get("target") != after.get("target") or before.get("version") != after.get("version"):
            return dict(status="stale", id=revision)
    return dict(status={0: "passed", 1: "failed", 125: "skipped"}[verdict], id=revision, ref=before)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--endpoint", required=True)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--token-file")
    parser.add_argument("--journal", required=True)
    parser.add_argument("--timeout", type=float, default=300)
    commands = parser.add_subparsers(dest="operation", required=True)
    search = commands.add_parser("bisect")
    search.add_argument("--good", action="append", required=True)
    search.add_argument("--bad", required=True)
    search.add_argument("--test-id", required=True)
    search.add_argument("--max-nodes", type=int, default=10000)
    search.add_argument("--max-tests", type=int, default=64)
    search.add_argument("--test-timeout", type=float, default=30)
    search.add_argument("test", nargs=argparse.REMAINDER)
    gate = commands.add_parser("check")
    gate.add_argument("--revision", required=True)
    gate.add_argument("--ref")
    gate.add_argument("--test-id", required=True)
    gate.add_argument("--test-timeout", type=float, default=30)
    gate.add_argument("test", nargs=argparse.REMAINDER)
    undo = commands.add_parser("revert")
    undo.add_argument("--bad", required=True)
    undo.add_argument("--branch", default="refs/heads/main")
    undo.add_argument("--mainline", type=int)
    undo.add_argument("--operation-id", required=True)
    args = parser.parse_args(argv)
    journal = None
    try:
        budget = Budget(args.timeout)
        client = Client(args.endpoint, args.repo, args.token_file, budget)
        bad = object_id(args.revision if args.operation == "check" else args.bad)
        binding = dict(protocol_version=1, endpoint=client.endpoint, repo=args.repo, operation=args.operation, bad=bad)
        if args.operation in ("bisect", "check"):
            command = args.test[1:] if args.test[:1] == ["--"] else args.test
            if not command or not args.test_id or not 0 < args.test_timeout <= 86400:
                raise BotError("invalid bisect limits or missing tester command")
            binding.update(test_id=args.test_id, command_hash=digest(command))
            if args.operation == "bisect":
                if not 1 <= args.max_nodes <= 100000 or not 1 <= args.max_tests <= 10000:
                    raise BotError("invalid bisect limits")
                good = sorted(set(object_id(node) for node in args.good))
                binding.update(good=good)
            else:
                if args.ref is not None and not args.ref.startswith("refs/heads/"):
                    raise BotError("check ref must name a branch")
                binding.update(ref=args.ref)
            journal = Journal(args.journal, binding)
            tester = Tester(command, journal, budget, args.test_timeout, client.endpoint, args.repo, args.token_file)
            result = bisect(client, good, bad, tester, args.max_nodes, args.max_tests) if args.operation == "bisect" else check(client, bad, args.ref, tester)
        else:
            if not args.branch.startswith("refs/heads/") or not args.operation_id:
                raise BotError("revert requires a branch ref and operation ID")
            binding.update(branch=args.branch, mainline=args.mainline, operation_id=args.operation_id)
            journal = Journal(args.journal, binding)
            result = revert(client, bad, args.branch, args.mainline, args.operation_id, journal)
        code = {"culprit": 0, "landed": 0, "passed": 0, "failed": 1, "ambiguous": 2, "conflict": 2, "denied": 2, "skipped": 2, "retry": 3, "stale": 3}[result["status"]]
        print(json.dumps(result, sort_keys=True))
        return code
    except Retry as error:
        print(json.dumps(dict(status="retry", message=str(error))))
        return 3
    except BotError as error:
        print(json.dumps(dict(status="error", message=str(error))))
        return 1
    except (OSError, ValueError, KeyError, TypeError):
        # No remote response body, token, command argv or request headers in errors.
        print(json.dumps(dict(status="error", message="operation rejected or invalid configuration; journal was retained")))
        return 1
    finally:
        if journal:
            journal.close()


if __name__ == "__main__":
    sys.exit(main())
