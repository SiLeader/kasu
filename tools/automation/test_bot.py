import contextlib
import os
from pathlib import Path
import random
import sys
import tempfile
import unittest

from kasu_bot import Budget, BotError, Journal, Retry, Tester, ancestors, bisect, wire, object_id


def node(n):
    return n.to_bytes(32, "big").hex()


class FakeClient:
    def __init__(self, graph):
        self.graph, self.budget = graph, Budget(10)

    def call(self, method, **args):
        assert method == "changeset"
        key = bytes(args["id"]).hex()
        return dict(id=wire(key), parents=[wire(p) for p in self.graph[key]])


class Oracle:
    def __init__(self, graph, culprit, journal, skipped=()):
        self.graph, self.culprit, self.journal, self.skipped = graph, culprit, journal, skipped
        self.calls = 0

    def run(self, key):
        results = self.journal.data.setdefault("verdicts", {})
        if key not in results:
            self.calls += 1
            results[key] = 125 if key in self.skipped else int(self.culprit in ancestors(self.graph, [key], Budget(2)))
            self.journal.save()
        return results[key]


class BotTests(unittest.TestCase):
    def test_ids_reject_scalars_and_invalid_byte_arrays(self):
        for value in [10**12, None, [0], [True]*32, [256]*32, "ff"]:
            with self.assertRaises(BotError):
                object_id(value)
        self.assertEqual(object_id([0]*32),node(0))
    def test_all_culprits_in_branched_merge_dags(self):
        with tempfile.TemporaryDirectory() as directory:
            for seed in range(12):
                randomizer = random.Random(seed)
                graph = {node(0): []}
                for n in range(1, 12):
                    graph[node(n)] = [node(p) for p in randomizer.sample(range(n), min(n, 2))]
                graph[node(12)] = [node(n) for n in range(1, 12)]
                for culprit in range(1, 13):
                    with contextlib.closing(Journal(Path(directory) / f"{seed}-{culprit}", {})) as journal:
                        tester = Oracle(graph, node(culprit), journal)
                        result = bisect(FakeClient(graph), [node(0)], node(12), tester)
                        self.assertEqual(result["id"], node(culprit))

    def test_skip_is_ambiguous_and_budget_resume_reuses_results(self):
        graph = {node(n): [node(n - 1)] if n else [] for n in range(12)}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "journal"
            with contextlib.closing(Journal(path, {})) as journal:
                tester = Oracle(graph, node(5), journal, [node(5)])
                with self.assertRaises(Retry):
                    bisect(FakeClient(graph), [node(0)], node(11), tester, max_tests=1)
                previous = dict(journal.data["verdicts"])
            with contextlib.closing(Journal(path, {})) as journal:
                tester = Oracle(graph, node(5), journal, [node(5)])
                result = bisect(FakeClient(graph), [node(0)], node(11), tester)
                self.assertEqual(result["status"], "ambiguous")
                self.assertIn(node(5), result["candidates"])
                self.assertTrue(all(journal.data["verdicts"][k] == v for k, v in previous.items()))
                before = tester.calls
                bisect(FakeClient(graph), [node(0)], node(11), tester)
                self.assertEqual(before, tester.calls)

    def test_journal_binding_and_exclusive_lock(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "journal"
            with contextlib.closing(Journal(path, dict(test="first"))):
                with self.assertRaises(BotError):
                    Journal(path, dict(test="first"))
            with self.assertRaises(BotError):
                Journal(path, dict(test="changed"))
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)

    def test_oversized_checkpoint_preserves_the_previous_durable_journal(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "journal"
            with contextlib.closing(Journal(path, {})) as journal:
                journal.data["oversized"] = "x" * (16 * 1024 * 1024)
                with self.assertRaises(BotError):
                    journal.save()
            with contextlib.closing(Journal(path, {})) as journal:
                self.assertNotIn("oversized", journal.data)

    def test_tester_timeout_kills_process_and_never_caches_a_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            script = path / "tester.py"
            pid = path / "pid"
            script.write_text("import os,time\nfrom pathlib import Path\nPath(" + repr(str(pid)) + ").write_text(str(os.getpid()))\ntime.sleep(30)\n")
            with contextlib.closing(Journal(path / "journal", {})) as journal:
                tester = Tester([sys.executable, str(script)], journal, Budget(3), 0.2, "http://localhost", "repo", None)
                with self.assertRaises(Retry):
                    tester.run(node(1))
                self.assertNotIn(node(1), journal.data["verdicts"])
                if pid.exists():
                    with self.assertRaises(ProcessLookupError):
                        os.kill(int(pid.read_text()), 0)

    def test_infrastructure_failure_is_not_a_bad_verdict_and_graph_limit_is_enforced(self):
        with tempfile.TemporaryDirectory() as directory:
            with contextlib.closing(Journal(Path(directory) / "journal", {})) as journal:
                tester = Tester([sys.executable, "-c", "import sys;sys.exit(2)"], journal, Budget(3), 1, "http://localhost", "repo", None)
                with self.assertRaises(BotError):
                    tester.run(node(1))
                self.assertNotIn(node(1), journal.data["verdicts"])
                graph = {node(0): [], node(1): [node(0)]}
                with self.assertRaises(BotError):
                    bisect(FakeClient(graph), [node(0)], node(1), Oracle(graph, node(1), journal), max_nodes=1)


if __name__ == "__main__":
    unittest.main()
