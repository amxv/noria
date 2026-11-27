#!/usr/bin/env python3
"""Compare isolated Linux idle memory/CPU with no accounts or provider traffic.

Usage: python3 scripts/benchmark-idle.py --baseline /tmp/noria-go --candidate target/release/noria
Every subprocess uses isolated HOME/NORIA_HOME/CODEX_HOME/CLAUDE_CONFIG_DIR.
The default measures the daemon only. --mode supervised measures the fallback
supervisor and daemon together, with a per-process breakdown. Neither includes UI.
Use matching optimized builds for performance comparisons; a debug run is a smoke check.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import signal
import statistics
import subprocess
import tempfile
import time
import urllib.request


def environment(root):
    env = {key: os.environ[key] for key in ("PATH", "LANG", "SystemRoot", "WINDIR") if key in os.environ}
    env.update(HOME=str(root), USERPROFILE=str(root), NORIA_HOME=str(root / "pool"),
               CODEX_HOME=str(root / "codex"), CLAUDE_CONFIG_DIR=str(root / "claude"),
               XDG_CONFIG_HOME=str(root / "config"), XDG_CACHE_HOME=str(root / "cache"),
               NORIA_NO_LAUNCH="1")
    return env


def process_stat(pid):
    # Account for spaces or parentheses in the comm field before splitting.
    return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()


def owned_process(pid, binary, root, role, direct_pid=None):
    """Pin a private lock's PID to this executable, isolated home and start time.

    PID files alone never authorize signalling: a PID can outlive its owner.
    The direct child is additionally proven by Popen; detached children must
    carry the exact private --home argument supplied by the service launcher.
    """
    try:
        fields = process_stat(pid)
        if fields[0] == "Z" or not os.path.samefile(f"/proc/{pid}/exe", binary):
            return None
        args = Path(f"/proc/{pid}/cmdline").read_bytes().rstrip(b"\0").split(b"\0")
        command = b"__supervise" if role == "supervisor" else b"__daemon"
        if len(args) < 2 or args[1] != command:
            return None
        if pid != direct_pid:
            home_index = args.index(b"--home") + 1
            if args[home_index] != os.fsencode(root / "pool"):
                return None
        return {"pid": pid, "start_ticks": int(fields[19]), "parent_pid": int(fields[1])}
    except (OSError, ValueError, IndexError):
        return None


def discover(binary, root, mode, direct_pid=None):
    found = {}
    roles = ("daemon",) if mode == "daemon" else ("supervisor", "daemon")
    for role in roles:
        try:
            content = (root / "pool" / f"{role}.lock").read_text().strip()
            if not content.startswith("pid="):
                continue
            pid = int(content[4:])
        except (OSError, ValueError):
            continue
        process = owned_process(pid, binary, root, role, direct_pid)
        if process is not None and (mode != "daemon" or pid == direct_pid):
            found[role] = process
    return found


def still_owned(process, binary, root, role, direct_pid):
    current = owned_process(process["pid"], binary, root, role, direct_pid)
    return current is not None and current["start_ticks"] == process["start_ticks"]


def counters(process):
    pid = process["pid"]
    status = {}
    for line in Path(f"/proc/{pid}/status").read_text().splitlines():
        if ":" in line:
            key, value = line.split(":", 1)
            values = value.strip().split()
            if values:
                status[key] = values[0]
    memory = {}
    for line in Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines()[1:]:
        key, value = line.split(":", 1)
        memory[key] = int(value.strip().split()[0])
    fields = process_stat(pid)
    if fields[0] == "Z" or int(fields[19]) != process["start_ticks"]:
        raise RuntimeError("measured process exited or restarted during sampling")
    return {
        "rss_kib": memory["Rss"], "pss_kib": memory["Pss"],
        "private_kib": memory["Private_Clean"] + memory["Private_Dirty"],
        "threads": int(status["Threads"]),
        "cpu_ticks": int(fields[11]) + int(fields[12]),
    }


def stop_isolated(binary, root, env, mode, process, processes):
    """Use public stop first, then bounded cleanup of verified private PIDs only."""
    direct_pid = process.pid if process is not None else None
    # Stop may rewrite daemon.lock with its own short-lived PID after acquiring
    # the released guard. Preserve the identities observed before invoking it.
    processes = {**discover(binary, root, mode, direct_pid), **processes}
    stopped = False
    try:
        result = subprocess.run([str(binary), "stop", "--home", str(root / "pool")],
                                env=env, cwd=root, stdin=subprocess.DEVNULL,
                                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=20)
        stopped = result.returncode == 0
    except subprocess.TimeoutExpired:
        pass
    deadline = time.monotonic() + 5
    while any(still_owned(item, binary, root, role, direct_pid)
              for role, item in processes.items()) and time.monotonic() < deadline:
        if process is not None:
            process.poll()
        time.sleep(0.05)
    forced = False
    for sig in (signal.SIGTERM, signal.SIGKILL):
        # Terminate the supervisor before the daemon to prevent any respawn.
        for role in ("supervisor", "daemon"):
            item = processes.get(role)
            if item is not None and still_owned(item, binary, root, role, direct_pid):
                forced = True
                try:
                    os.kill(item["pid"], sig)
                except ProcessLookupError:
                    pass
        deadline = time.monotonic() + 2
        while any(still_owned(item, binary, root, role, direct_pid)
                  for role, item in processes.items()) and time.monotonic() < deadline:
            if process is not None:
                process.poll()
            time.sleep(0.05)
    if process is not None:
        process.wait(timeout=3)
    if any(still_owned(item, binary, root, role, direct_pid)
           for role, item in processes.items()):
        raise RuntimeError("isolated benchmark processes did not exit")
    if not stopped or forced:
        raise RuntimeError("public stop failed to cleanly stop the isolated benchmark")


def wait_ready(binary, root, mode, process):
    deadline = time.monotonic() + 10
    runtime = root / "pool/daemon.json"
    expected = 2 if mode == "supervised" else 1
    direct_pid = process.pid if process is not None else None
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    while time.monotonic() < deadline:
        if process is not None and process.poll() is not None:
            raise RuntimeError("isolated daemon exited before becoming healthy")
        processes = discover(binary, root, mode, direct_pid)
        if len(processes) == expected and runtime.exists():
            if mode == "supervised" and processes["daemon"]["parent_pid"] != processes["supervisor"]["pid"]:
                raise RuntimeError("private daemon is not a child of the private supervisor")
            try:
                address = json.loads(runtime.read_text())["address"]
                secret = (root / "pool/secret").read_text()
                request = urllib.request.Request(f"http://{address}/_noria/health",
                                                 headers={"Authorization": "Bearer " + secret})
                with opener.open(request, timeout=2) as response:
                    if response.status == 200:
                        response.read()
                        return processes
            except (OSError, ValueError, KeyError):
                pass
        time.sleep(0.02)
    raise RuntimeError("isolated benchmark processes did not become healthy")


def measure(binary, seconds, mode="daemon"):
    with tempfile.TemporaryDirectory(prefix="noria-idle-") as name:
        root = Path(name)
        env = environment(root)
        subprocess.run([str(binary), "init"], env=env, cwd=root, check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=10)
        path = root / "pool/config.json"
        config = json.loads(path.read_text())
        # Let the kernel choose a port, then read daemon.json for the actual one.
        config.update(listen="127.0.0.1:0", auto_update=False, start_at_login=False, accounts=[])
        path.write_text(json.dumps(config))
        with (root / "daemon.log").open("wb") as log:
            started = time.monotonic()
            process = None
            processes = {}
            try:
                if mode == "supervised":
                    # The temporary HOME and absent session-manager environment
                    # force fallback supervision, without touching real services.
                    subprocess.run([str(binary), "start", "--home", str(root / "pool")],
                                   env=env, cwd=root, check=True, stdin=subprocess.DEVNULL,
                                   stdout=log, stderr=log, timeout=20)
                else:
                    process = subprocess.Popen([str(binary), "__daemon"], env=env, cwd=root,
                                               stdin=subprocess.DEVNULL, stdout=log, stderr=log)
                processes = wait_ready(binary, root, mode, process)
                ready_ms = (time.monotonic() - started) * 1000
                time.sleep(1)
                before = {role: counters(item) for role, item in processes.items()}
                sample_start = time.monotonic()
                time.sleep(seconds)
                after = {role: counters(item) for role, item in processes.items()}
                elapsed = time.monotonic() - sample_start
                for role, item in after.items():
                    item["cpu_ticks"] -= before[role]["cpu_ticks"]
                    item["cpu_percent"] = item["cpu_ticks"] / os.sysconf("SC_CLK_TCK") / elapsed * 100
                    item["pid"] = processes[role]["pid"]
                total = {key: sum(item[key] for item in after.values())
                         for key in ("rss_kib", "pss_kib", "private_kib", "threads", "cpu_ticks", "cpu_percent")}
                total.update(sample_seconds=round(elapsed, 3), ready_ms=round(ready_ms, 2),
                             process_count=len(processes), processes=after)
                return total
            finally:
                stop_isolated(binary, root, env, mode, process, processes)


def binary_metadata(binary):
    with tempfile.TemporaryDirectory(prefix="noria-build-info-") as name:
        root = Path(name)
        version = subprocess.run([str(binary), "--version"], env=environment(root), cwd=root,
                                 check=True, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, text=True, timeout=10).stdout.strip()
    digest = hashlib.sha256()
    with binary.open("rb") as source:
        for block in iter(lambda: source.read(1 << 20), b""):
            digest.update(block)
    return {"binary": str(binary), "binary_bytes": binary.stat().st_size,
            "version": version, "sha256": digest.hexdigest(),
            "build_path_hint": "debug" if "debug" in binary.parts else
                               "release" if "release" in binary.parts else "unspecified"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--seconds", type=float, default=10)
    parser.add_argument("--mode", choices=("daemon", "supervised"), default="daemon")
    args = parser.parse_args()
    if platform.system() != "Linux":
        parser.error("this measurement uses Linux /proc")
    if args.runs < 1 or not 1 <= args.seconds <= 60:
        parser.error("runs must be positive; seconds must be between 1 and 60")
    results = {"platform": platform.platform(), "architecture": platform.machine(), "mode": args.mode,
               "scope": "headless daemon alone, no accounts, loopback health only" if args.mode == "daemon"
                        else "fallback supervisor plus daemon, no accounts, loopback health only",
               "memory_accounting": "process sums; RSS can count shared pages twice, PSS apportions them",
               "cpu_accounting": "sum of process user+system ticks; 100% means one fully occupied CPU core",
               "measurements": {}}
    for label, binary in [("go", args.baseline.resolve()), ("rust", args.candidate.resolve())]:
        metadata = binary_metadata(binary)
        samples = [measure(binary, args.seconds, args.mode) for _ in range(args.runs)]
        results["measurements"][label] = {
            **metadata, "samples": samples,
            "median": {key: statistics.median(item[key] for item in samples)
                       for key in ("rss_kib", "pss_kib", "private_kib", "threads", "cpu_percent", "ready_ms")},
        }
    if any(item["build_path_hint"] == "debug" for item in results["measurements"].values()):
        results["interpretation"] = "Includes a debug-path binary: idle smoke check, not a release performance comparison."
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
