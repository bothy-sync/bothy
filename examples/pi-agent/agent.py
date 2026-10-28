#!/usr/bin/env python3
"""Bothy robot agent: takes missions from a phone and sends telemetry back, with no server.

    agent.py login                                   # once: device sign-in, code on a phone
    BOTHY_PASSWORD=... agent.py run --folder robot   # first run joins (or creates) the database
    agent.py run --folder robot                      # later runs use the kept key
    agent.py status

Missions are documents in ``missions`` (``{"goal": ..., "state": "new"}``). The agent marks each
new one ``accepted`` and then ``done``. Every ``--interval`` seconds it writes a reading of the
board (CPU temperature, load, memory) to ``telemetry_logs``. It works offline: writes stay in
the local database and sync when the network is back.

Everything lives in ``--home`` (default ``~/.local/share/bothy-agent``): the database, the
Google grant and the data key, all readable only by the user.
"""

import argparse
import json
import os
import platform
import resource
import signal
import sys
import time

import bothy
import bothy.auth

APP_ID = "dev.bothy.notes"  # the Flutter demo's app id: the phone and the robot share a database
MISSIONS = "missions"
TELEMETRY = "telemetry_logs"


def log(event, **fields):
    print(json.dumps({"event": event, **fields}), flush=True)


def write_private(path, data):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "wb") as f:
        f.write(data)


def peak_rss_kib():
    """Peak resident memory of this process, in KiB."""
    peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    return peak // 1024 if sys.platform == "darwin" else peak  # macOS reports bytes


def read_first(*paths):
    for p in paths:
        try:
            with open(p) as f:
                return f.read().strip()
        except OSError:
            pass
    return None


def reading(device):
    """A telemetry record from /proc and /sys (fields are None where unavailable)."""
    temp = read_first("/sys/class/thermal/thermal_zone0/temp")
    mem = {}
    for line in (read_first("/proc/meminfo") or "").splitlines():
        k, _, v = line.partition(":")
        mem[k] = int(v.split()[0]) if v.split() else 0
    try:
        load1 = os.getloadavg()[0]
    except OSError:
        load1 = None
    return {
        "device": device,
        "cpu_temp_c": round(int(temp) / 1000, 1) if temp and temp.isdigit() else None,
        "load1": round(load1, 2) if load1 is not None else None,
        "mem_available_mib": mem.get("MemAvailable", 0) // 1024 or None,
        "agent_peak_rss_kib": peak_rss_kib(),
    }


class Agent:
    def __init__(self, args):
        self.args = args
        self.home = os.path.expanduser(args.home)
        os.makedirs(self.home, mode=0o700, exist_ok=True)
        self.db = None
        self.ros = None
        self.stopping = False

    def path(self, name):
        return os.path.join(self.home, name)

    def remote(self):
        if self.args.remote_dir:
            return bothy.FolderRemote(self.args.remote_dir)
        grant = bothy.auth.Grant.load(self.path("token.json"))
        if grant is None:
            sys.exit("not signed in; run `agent.py login` first")
        # No stored client file: the sign-in used Bothy's official client.
        client = read_first(self.path("client"))
        return bothy.DriveRemote.folder(self.args.folder, bothy.auth.TokenProvider(client, grant))

    def open(self):
        key_file = self.path("key")
        key = None
        if os.path.exists(key_file):
            with open(key_file, "rb") as f:
                key = f.read()
        password = None if key else os.environ.get("BOTHY_PASSWORD")
        self.db = bothy.Database.open(
            self.path("agent.db"), APP_ID, self.remote(),
            encrypt=password is not None, password=password, key=key,
            device_name=self.args.name or platform.node() or "robot",
            p2p=getattr(self.args, "p2p", False),
        )
        if self.db.db_id() is None:
            if self.args.offline:
                sys.exit("the first run needs the network to join the database")
            db_id = self.db.init_or_join()
            log("joined", db_id=db_id, encrypted=self.db.is_encrypted())
        if self.db.is_encrypted() and not os.path.exists(key_file):
            # Keep the key, not the password: later runs (and sync) need no Argon2 at all.
            write_private(key_file, self.db.export_key())
        if self.db.is_locked():
            sys.exit("the database is encrypted: set BOTHY_PASSWORD for the first run")

    def handle_missions(self):
        """Marks new missions accepted, "runs" them, and marks them done."""
        missions = self.db.collection(MISSIONS)
        done = 0
        for mid, m in list(missions.all()):
            if m.get("state", "new") != "new":
                continue
            now = int(time.time() * 1000)
            missions.patch(mid, {"state": "accepted", "accepted_by": self.db.device_id,
                                 "accepted_ms": now})
            log("mission", id=mid, goal=m.get("goal"), state="accepted")
            if self.ros:
                self.ros.publish_mission(mid, m)
            # A real robot would act here; the demo reports success right away.
            missions.patch(mid, {"state": "done", "done_ms": int(time.time() * 1000),
                                 "result": "ok"})
            log("mission", id=mid, state="done")
            done += 1
        return done

    def telemetry(self, at_ms, **extra):
        rec = {**reading(self.db.device_id[:8]), "at_ms": at_ms, **extra}
        self.db.collection(TELEMETRY).put(f"{self.db.device_id[:8]}-{at_ms}", rec)

    def simulate(self, hours):
        """Writes `hours` of one-per-minute telemetry at once (an accelerated offline run)."""
        start = int(time.time() * 1000) - hours * 3_600_000
        n = hours * 60
        for i in range(n):
            self.telemetry(start + i * 60_000, simulated=True)
        log("simulated", records=n, peak_rss_kib=peak_rss_kib())

    def run(self):
        self.open()
        if self.args.ros2:
            from ros2_bridge import Bridge  # needs a sourced ROS 2 environment

            self.ros = Bridge(self.db)
        if self.args.simulate_hours:
            self.simulate(self.args.simulate_hours)
        if not self.args.offline:
            self.db.start_auto_sync(debounce=2.0, poll_interval=self.args.poll)
        signal.signal(signal.SIGTERM, lambda *_: setattr(self, "stopping", True))
        try:
            while not self.stopping:
                done = self.handle_missions()
                self.telemetry(int(time.time() * 1000), missions_done=done)
                if self.args.once:
                    break
                deadline = time.time() + self.args.interval
                while not self.stopping and time.time() < deadline:
                    if self.ros:
                        self.ros.spin_once(0.5)
                    else:
                        time.sleep(0.5)
                st = self.db.status()
                log("tick", unsynced_ops=st["unsynced_ops"], documents=st["documents"],
                    peak_rss_kib=peak_rss_kib(), last_sync=_describe(self.db.auto_sync_status()))
        except KeyboardInterrupt:
            pass
        finally:
            if not self.args.offline:
                try:
                    self.db.stop_auto_sync()
                    report = self.db.sync()
                    log("synced", pushed_ops=report["pushed_ops"], pulled_ops=report["pulled_ops"])
                except bothy.BothyError as e:
                    log("sync_failed", error=type(e).__name__, message=str(e))
            log("stopped", peak_rss_kib=peak_rss_kib())
            if self.ros:
                self.ros.close()
            self.db.close()


def _describe(status):
    if status is None:
        return None
    if isinstance(status, Exception):
        return f"{type(status).__name__}: {status}"
    return {"pushed_ops": status["pushed_ops"], "pulled_ops": status["pulled_ops"]}


def cmd_login(args):
    home = os.path.expanduser(args.home)
    os.makedirs(home, mode=0o700, exist_ok=True)
    client = os.path.abspath(args.client) if args.client else None
    grant = bothy.auth.device_login(client)
    grant.save(os.path.join(home, "token.json"))
    if client:
        write_private(os.path.join(home, "client"), client.encode())
    elif os.path.exists(os.path.join(home, "client")):
        os.remove(os.path.join(home, "client"))
    log("signed_in", scopes=grant.scopes)


def cmd_status(args):
    agent = Agent(args)
    agent.args.offline = True
    agent.open()
    st = agent.db.status()
    st["missions"] = sum(1 for _ in agent.db.collection(MISSIONS).all())
    st["telemetry"] = sum(1 for _ in agent.db.collection(TELEMETRY).all())
    print(json.dumps(st))
    agent.db.close()


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--home", default="~/.local/share/bothy-agent",
                   help="directory for the database, the sign-in and the key")
    sub = p.add_subparsers(dest="cmd", required=True)
    login = sub.add_parser("login", help="device sign-in (once)")
    login.add_argument("--client",
                       help='"TVs and Limited Input devices" OAuth client JSON of your own Cloud '
                            "project (default: Bothy's official client)")
    for name in ("run", "status"):
        s = sub.add_parser(name)
        s.add_argument("--folder", default="robot", help="Drive folder Bothy/<folder>")
        s.add_argument("--remote-dir", help="sync through this directory instead of Drive")
        s.add_argument("--name", help="device name shown to the phone")
        if name == "run":
            s.add_argument("--interval", type=float, default=60, help="seconds between readings")
            s.add_argument("--poll", type=float, default=30, help="seconds between remote checks")
            s.add_argument("--offline", action="store_true", help="never touch the network")
            s.add_argument("--simulate-hours", type=int, default=0,
                           help="first write this many hours of per-minute readings")
            s.add_argument("--once", action="store_true", help="one round, then exit")
            s.add_argument("--ros2", action="store_true",
                           help="bridge missions and telemetry to ROS 2 topics")
            s.add_argument("--p2p", action="store_true",
                           help="direct channel to the phone while both are online (missions "
                                "arrive within about a second)")
    args = p.parse_args(argv)
    if args.cmd == "login":
        cmd_login(args)
    elif args.cmd == "status":
        cmd_status(args)
    else:
        Agent(args).run()


if __name__ == "__main__":
    main()
