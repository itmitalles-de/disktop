#!/usr/bin/env python3
"""Measure real storage I/O of separate processes in a disposable container.

Run only in an isolated container as root, for example:

  docker run --rm --network none -v "$PWD:/work:ro" dwtop-test \
      python3 tests/live_io.py --isolated target/release/dwtop

Timing is not part of the assertions: dwtop is run once before any I/O
starts and once after the writer and reader have finished (while both are still
alive, so their rows exist), and the cumulative kernel counters are compared.
A rate over one fixed interval would depend on how slow the CI disk is.

A writer process (unprivileged, separate PID) writes a file on the
container's disk-backed overlay filesystem and fsyncs it; a reader process
drops the file's clean pages with posix_fadvise and reads it back from
storage. dwtop runs as the same unprivileged user without the helper and
must attribute both to the right PIDs, and the physical disks must show at
least the written bytes. tmpfs is never used: it is memory, not disk.
"""

import argparse
import ctypes
import json
import multiprocessing as mp
import os
from pathlib import Path
import pwd
import shutil
import subprocess
import tempfile
import time

MIB = 1024 * 1024
SIZE = 48 * MIB


def become(account):
    os.setgroups([])
    os.setgid(account.pw_gid)
    os.setuid(account.pw_uid)
    # Changing credentials makes a process non-dumpable, which would hide its
    # /proc/PID/io from its own user; a normal login process is dumpable.
    libc = ctypes.CDLL(None, use_errno=True)
    assert libc.prctl(4, 1, 0, 0, 0) == 0  # PR_SET_DUMPABLE


def writer(account, path, ready, go, done, finish):
    become(account)
    ready.set()
    go.wait(60)
    chunk = os.urandom(MIB)
    with open(path, "wb") as stream:
        for _ in range(SIZE // MIB):
            stream.write(chunk)
        stream.flush()
        os.fsync(stream.fileno())
    done.set()
    # Stay alive so dwtop can read this process's counters at its sample.
    finish.wait(120)


def reader(account, path, ready, go, done, finish):
    become(account)
    ready.set()
    go.wait(60)
    descriptor = os.open(path, os.O_RDONLY)
    try:
        # Clean pages are dropped without privilege; reads then hit storage.
        os.posix_fadvise(descriptor, 0, 0, os.POSIX_FADV_DONTNEED)
        total = 0
        while True:
            block = os.read(descriptor, MIB)
            if not block:
                break
            total += len(block)
        assert total == SIZE, total
    finally:
        os.close(descriptor)
    done.set()
    finish.wait(120)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--isolated", action="store_true", help="confirm this is a disposable container")
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    if not args.isolated or os.geteuid() != 0:
        parser.error("run as root with --isolated only in a disposable Linux container")
    binary = args.binary.resolve(strict=True)
    account = pwd.getpwnam("nobody")
    # /var/tmp lives on the container's writable layer, which is disk-backed.
    work = Path(tempfile.mkdtemp(prefix="dwtop-live-", dir="/var/tmp"))
    try:
        os.chown(work, account.pw_uid, account.pw_gid)
        path = work / "payload.bin"
        write_ready, write_go, write_done, read_ready, read_go, read_done, finish = (
            mp.Event() for _ in range(7))
        writing = mp.Process(target=writer, args=(account, path, write_ready, write_go, write_done, finish))
        reading = mp.Process(target=reader, args=(account, path, read_ready, read_go, read_done, finish))
        writing.start()
        reading.start()
        config = work / "config"
        config.mkdir()
        os.chown(config, account.pw_uid, account.pw_gid)

        def sample():
            # A short interval only bounds how long one run takes; the
            # assertions use cumulative counters, never this interval.
            result = subprocess.run(
                [str(binary), "--json", "--no-helper", "--interval", "0.5", "--all-devices"],
                capture_output=True,
                text=True,
                timeout=30,
                env={"PATH": "/usr/bin:/bin", "XDG_CONFIG_HOME": str(config), "LANG": "C"},
                user=account.pw_uid,
                group=account.pw_gid,
                extra_groups=[],
            )
            assert result.returncode == 0, f"dwtop failed: {result.stderr}"
            return json.loads(result.stdout)

        # Both processes must have dropped privileges (and be dumpable) so
        # their /proc/PID/io is readable before the baseline is taken.
        assert write_ready.wait(10) and read_ready.wait(10), "I/O processes did not start"
        before = sample()
        write_go.set()
        assert write_done.wait(60), "writer did not finish"
        read_go.set()
        assert read_done.wait(60), "reader did not finish"
        # The writer fsynced, so its data and the device counters are final.
        snapshot = sample()
        finish.set()
        rows = {row["pid"]: row for row in snapshot["processes"]}
        old = {row["pid"]: row for row in before["processes"]}
        assert writing.pid in rows, f"writer PID {writing.pid} missing; access: {snapshot['process_access']}"
        assert reading.pid in rows, f"reader PID {reading.pid} missing"
        assert writing.pid in old and reading.pid in old, "baseline rows missing"
        written = rows[writing.pid]["write_bytes"] - old[writing.pid]["write_bytes"]
        assert SIZE * 0.95 <= written <= SIZE * 1.3, f"writer measured {written / MIB:.1f} MiB"
        assert rows[writing.pid]["user"] == "nobody", rows[writing.pid]
        read = rows[reading.pid]["read_bytes"] - old[reading.pid]["read_bytes"]
        assert SIZE * 0.9 <= read <= SIZE * 1.3, f"reader measured {read / MIB:.1f} MiB from storage"
        reader_written = rows[reading.pid]["write_bytes"] - old[reading.pid]["write_bytes"]
        assert reader_written < MIB, "reader was charged with writes"
        print(f"PASS processes: writer {written / MIB:.1f} MiB written, reader {read / MIB:.1f} MiB read "
              f"(expected {SIZE // MIB} MiB each)")
        disks = [d for d in snapshot["devices"]
                 if not d["partition"] and d["kind"] not in ("dm", "md", "loop", "zram", "ram")]
        assert disks, "no physical disks visible"
        old_disks = {d["name"]: d for d in before["devices"]}
        device_written = sum(d["write_bytes"] - old_disks[d["name"]]["write_bytes"]
                             for d in disks if d["name"] in old_disks)
        assert device_written >= SIZE * 0.9, f"physical disks wrote only {device_written / MIB:.1f} MiB"
        print(f"PASS devices: physical disks wrote {device_written / MIB:.1f} MiB during the test")
        # Capacity use is measured for every device; null would mean "not measured".
        assert all(d["usage"] is not None for d in snapshot["devices"]), "device usage missing"
        mounted = [d for d in snapshot["devices"] if d["usage"]["filesystems"] > 0]
        for d in mounted:
            usage = d["usage"]
            assert usage["used_bytes"] <= usage["total_bytes"], d
        print(f"PASS usage: {len(mounted)} devices with mounted filesystems report used/total")
        access = snapshot["process_access"]
        assert not access["privileged"], access
        assert access["unreadable"] > 0, "root-owned processes should be hidden without the helper"
        own = {row["user"] for row in snapshot["processes"]}
        assert own == {"nobody"}, f"other users' processes leaked without the helper: {own}"
        print(f"PASS restriction: {access['unreadable']} other-user processes hidden and counted")
        assert snapshot["filesystems"], "no storage filesystems listed"
        assert snapshot["demo"] is False
    finally:
        for process in ("writing", "reading"):
            child = locals().get(process)
            if child is not None and child.is_alive():
                child.terminate()
                child.join(3)
        shutil.rmtree(work, ignore_errors=True)
    print("All live storage I/O checks passed.")


if __name__ == "__main__":
    main()
