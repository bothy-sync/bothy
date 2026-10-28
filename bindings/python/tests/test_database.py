import json
import os
import shutil
import subprocess
import tempfile
import threading
import time
import unittest

import bothy

APP = "dev.bothy.pytest"


class Env:
    def __init__(self):
        self.dir = tempfile.mkdtemp(prefix="bothy-py-")
        self.remote_dir = os.path.join(self.dir, "remote")

    def open(self, name, **kw):
        return bothy.Database.open(os.path.join(self.dir, name + ".db"), APP,
                                   bothy.FolderRemote(self.remote_dir), device_name=name, **kw)

    def cleanup(self):
        shutil.rmtree(self.dir, ignore_errors=True)

    def remote_contains(self, needle):
        for root, _, files in os.walk(self.remote_dir):
            for f in files:
                with open(os.path.join(root, f), "rb") as fh:
                    if needle in fh.read():
                        return True
        return False


class DatabaseTest(unittest.TestCase):
    def setUp(self):
        self.env = Env()

    def tearDown(self):
        self.env.cleanup()

    def test_two_devices_converge(self):
        with self.env.open("a") as a, self.env.open("b") as b:
            db_id = a.init_or_join()
            self.assertEqual(b.init_or_join(), db_id)
            m = a.collection("missions")
            self.assertTrue(m.put("m1", {"goal": "dock", "n": 3, "x": 1.5, "tags": ["a"], "o": {"k": None}}))
            m.put("m2", {"goal": "scan"})
            self.assertEqual(a.sync()["pushed_ops"], 2)
            self.assertEqual(b.sync()["changed_docs"], 2)
            got = b.collection("missions").get("m1")
            self.assertEqual(got, {"goal": "dock", "n": 3, "x": 1.5, "tags": ["a"], "o": {"k": None}})
            self.assertIsInstance(got["n"], int)
            self.assertIsInstance(got["x"], float)

            b.collection("missions").patch("m1", {"state": "done"}, unset=["n"])
            self.assertTrue(b.collection("missions").delete("m2"))
            b.sync()
            a.sync()
            self.assertEqual([i for i, _ in m.list()], ["m1"])
            self.assertEqual(m.get("m1")["state"], "done")
            self.assertNotIn("n", m.get("m1"))
            self.assertIsNone(m.get("m2"))
            self.assertEqual(a.collections(), ["missions"])
            self.assertEqual(a.status()["documents"], 1)

    def test_all_pages_through_every_document(self):
        with self.env.open("a") as a:
            a.init()
            logs = a.collection("telemetry_logs")
            for i in range(25):
                logs.put(f"t{i:03d}", {"i": i})
            self.assertEqual([d["i"] for _, d in logs.all(page=10)], list(range(25)))

    def test_changes_report_remote_writes(self):
        with self.env.open("a") as a, self.env.open("b") as b:
            a.init_or_join()
            b.init_or_join()
            got = []
            stream = b.changes(timeout=5)  # subscribed from here on

            def watch():
                for c in stream:
                    got.append(c)
                    if c.remote:
                        return

            t = threading.Thread(target=watch)
            t.start()
            a.collection("missions").put("m1", {"goal": "dock"})
            a.sync()
            b.sync()
            t.join(10)
            self.assertTrue(any(c.remote and c.id == "m1" and c.collection == "missions" for c in got), got)
            self.assertEqual(list(b.changes(timeout=0.1)), [])

    def test_changes_subscribe_when_called(self):
        with self.env.open("a") as a, self.env.open("b") as b:
            a.init_or_join()
            b.init_or_join()
            stream = b.changes(timeout=2)
            a.collection("missions").put("m2", {"goal": "dock"})
            a.sync()
            b.sync()  # before the first next(): still reported
            got = [(c.collection, c.id, c.remote) for c in stream]
            self.assertIn(("missions", "m2", True), got)

    def test_p2p_delivers_writes_quickly(self):
        kw = dict(p2p=True, stun=[], p2p_host="127.0.0.1")
        with self.env.open("a", **kw) as a, self.env.open("b", **kw) as b:
            a.init_or_join()
            b.init_or_join()
            self.assertFalse(a.p2p_status()["running"])
            for db in (a, b):
                db.start_auto_sync(debounce=0.1, poll_interval=1.0)
            deadline = time.time() + 60
            while time.time() < deadline:
                states = [p["state"] for db in (a, b) for p in db.p2p_status()["peers"]]
                if states.count("connected") == 2:
                    break
                time.sleep(0.2)
            else:
                self.fail("no direct channel: %r %r" % (a.p2p_status(), b.p2p_status()))
            self.assertEqual(a.p2p_status()["peers"][0]["device_id"], b.device_id)
            stream = b.changes(timeout=5)
            start = time.time()
            a.collection("missions").put("m9", {"goal": "dock"})
            got = next(c for c in stream if c.id == "m9")
            self.assertTrue(got.remote)
            self.assertLess(time.time() - start, 2.0)
            a.stop_auto_sync()
            self.assertFalse(a.p2p_status()["running"])
            b.stop_auto_sync()

    def test_p2p_options_need_p2p(self):
        with self.assertRaises(bothy.InvalidInputError):
            self.env.open("a", stun=[])
        with self.assertRaises(bothy.InvalidInputError):
            self.env.open("b", p2p=True, p2p_host="not an ip")

    def test_conflicts(self):
        with self.env.open("a") as a, self.env.open("b") as b:
            a.init_or_join()
            b.init_or_join()
            a.collection("missions").put("m", {"goal": "first"})
            time.sleep(0.01)
            b.collection("missions").put("m", {"goal": "second"})
            a.sync(), b.sync(), a.sync()
            lost = a.conflicts()
            self.assertEqual(len(lost), 1)
            self.assertEqual((lost[0]["id"], json.loads(lost[0]["loser_value_json"])), ("m", "first"))
            self.assertEqual(a.clear_conflicts(), 1)

    def test_encryption(self):
        a = self.env.open("a", encrypt=True, password="pw")
        a.init_or_join()
        a.collection("missions").put("m", {"goal": "SECRET-MARKER-PY"})
        a.sync()
        key = a.export_key()
        self.assertEqual(len(key), 32)
        self.assertTrue(a.is_encrypted())
        a.close()
        with self.assertRaises(bothy.BothyError):
            a.sync()
        self.assertFalse(self.env.remote_contains(b"SECRET-MARKER-PY"))

        with self.assertRaises(bothy.WrongPasswordError):
            self.env.open("b", password="nope").init_or_join()
        with self.env.open("c", password="pw") as c:
            c.init_or_join()
            self.assertEqual(c.collection("missions").get("m"), {"goal": "SECRET-MARKER-PY"})

        with self.env.open("a") as again:
            self.assertTrue(again.is_locked())
            with self.assertRaises(bothy.KeyRequiredError):
                again.sync()
            with self.assertRaises(bothy.WrongPasswordError):
                again.unlock("wrong")
            again.unlock_with_key(key)
            again.sync()

    def test_bad_input(self):
        with self.env.open("a") as a:
            with self.assertRaises(bothy.InvalidInputError):
                a.collection("Bad Name").put("x", {})
            with self.assertRaises(TypeError):
                a.collection("ok").put("x", [1, 2])
            with self.assertRaises(bothy.InvalidInputError):
                self.env.open("e", encrypt=True)

    def test_token_callback_without_a_token(self):
        calls = []

        def tokens(force):
            calls.append(force)
            return ""

        db = bothy.Database.open(os.path.join(self.env.dir, "d.db"), APP,
                                 bothy.DriveRemote.folder("robot", tokens))
        with self.assertRaises(bothy.AuthRequiredError):
            db.init()
        self.assertTrue(calls)
        db.close()
        with self.assertRaises(bothy.InvalidInputError):
            bothy.DriveRemote.folder("robot", "not callable")

    def test_token_provider_defaults_to_the_official_client(self):
        path = os.path.join(self.env.dir, "token.json")
        with open(path, "w") as f:
            json.dump({"refresh_token": "r", "scopes": [], "client": "device"}, f)
        grant = bothy.auth.Grant.load(path)
        try:
            bothy.auth.TokenProvider(None, grant)
        except bothy.InvalidInputError as e:
            # A build without the official client (the secret is given only to release builds).
            self.assertIn("official", str(e))
        with self.assertRaises(bothy.InvalidInputError):
            bothy.auth.TokenProvider(os.path.join(self.env.dir, "missing.json"), grant)

    @unittest.skipUnless(os.environ.get("BOTHY_CLI"), "set BOTHY_CLI to the bothy binary")
    def test_cli_and_python_share_a_database(self):
        cli = os.environ["BOTHY_CLI"]
        home = os.path.join(self.env.dir, "cli")
        env = dict(os.environ, BOTHY_HOME=home, BOTHY_TOKEN_STORE="file")
        env.pop("BOTHY_DB", None)

        def run(*args):
            return subprocess.run([cli, "--json", *args], env=env, check=True,
                                  capture_output=True, text=True).stdout

        db_id = json.loads(run("init", "--remote", f"path:{self.env.remote_dir}", "--app-id", APP))["db_id"]
        run("put", "missions", "from_cli", '{"goal": "dock"}')
        run("sync")
        with self.env.open("py") as py:
            self.assertEqual(py.init_or_join(), db_id)
            self.assertEqual(py.collection("missions").get("from_cli"), {"goal": "dock"})
            py.collection("telemetry_logs").put("t1", {"temp_c": 41.5})
            py.sync()
        run("sync")
        self.assertEqual(json.loads(run("get", "telemetry_logs", "t1")), {"temp_c": 41.5})


if __name__ == "__main__":
    unittest.main()
