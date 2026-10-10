#!/usr/bin/env python3
"""Exercise the privileged helper in an explicitly disposable Linux environment.

Run only in an isolated container/runner as root, with DAC_READ_SEARCH,
SYS_PTRACE and SETFCAP in its capability bounding set. This test installs at
the real helper path and refuses to replace a pre-existing installation:

  python3 tests/helper.py --isolated target/release/spctop target/release/spctop-collector

The UI itself runs as nobody with no supplementary groups or capabilities.
Every process, directory and installed file is test-owned.
"""

import argparse
import ctypes
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import pwd
import select
import shutil
import signal
import struct
import subprocess
import tempfile
import termios
import time
from unittest.mock import patch


HELPER = Path("/usr/local/libexec/spctop-collector")
RECEIPT = HELPER.with_name(".spctop-collector.sha256")
READ_CAPS = (1 << 2) | (1 << 19)
SCAN_CAPS = 1 << 2
FILE_CAPS = "cap_dac_read_search,cap_sys_ptrace=ep"
HOME = Path("/srv/spctop-home")
SCAN_ROOTS = Path("/etc/spctop/scan-roots")
LEGACY_SCAN_ROOTS = Path("/etc/disktop/scan-roots")
SECRET_NAME = "secret-file-name-never-reported"
MIB = 1024 * 1024


def status(pid, tid=None):
    path = Path(f"/proc/{pid}/status" if tid is None else f"/proc/{pid}/task/{tid}/status")
    return dict(line.split(":", 1) for line in path.read_text().splitlines())


def children(pid):
    return [int(value) for value in Path(f"/proc/{pid}/task/{pid}/children").read_text().split()]


def wait_until(predicate, description, timeout=5):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.02)
    raise AssertionError(f"Timed out waiting for {description}")


def unprivileged(account):
    return {"user": account.pw_uid, "group": account.pw_gid, "extra_groups": []}


def make_home(account):
    """alice is private to root; bob belongs to nobody. Only the helper may
    count alice's files, and no file name may ever be reported."""
    alice = HOME / "alice"
    (alice / "projects").mkdir(parents=True)
    (alice / "projects" / SECRET_NAME).write_bytes(os.urandom(2 * MIB))
    (alice / "notes.txt").write_bytes(b"x" * 4096)
    alice.chmod(0o700)
    bob = HOME / "bob"
    bob.mkdir()
    (bob / "music.bin").write_bytes(os.urandom(MIB // 4))
    os.chown(bob, account.pw_uid, account.pw_gid)
    HOME.chmod(0o755)


def run_json(binary, account, *arguments, timeout=20):
    result = subprocess.run(
        [str(binary), "--json", *arguments],
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
        **unprivileged(account),
    )
    assert result.returncode == 0, f"spctop {arguments} failed: {result.stderr}"
    return json.loads(result.stdout), result.stdout, result.stderr


def node(tree, name):
    root = tree["nodes"][0]
    for index in root["children"]:
        if tree["nodes"][index]["name"] == name:
            return tree["nodes"][index]
    raise AssertionError(f"{name} missing from {tree['root']}")


def check_threads(binary, account):
    """Per-thread capabilities, NO_NEW_PRIVS and the cleared environment of a
    helper that is sampling processes and scanning the home view."""
    master, slave = pty.openpty()
    before = termios.tcgetattr(slave)
    output = bytearray()
    monitor = None
    try:
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 120, 0, 0))
        monitor = subprocess.Popen(
            [str(binary), "--view", "home", "--home-path", str(HOME), "--interval", "0.2"],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env={**os.environ, "TERM": "xterm-256color", "LC_ALL": "C.UTF-8", "SPCTOP_INJECTED": "1"},
            start_new_session=True,
            **unprivileged(account),
        )
        wait_until(lambda: read_terminal(master, output, 0.05) or b"Focus" in output, "PTY monitor")
        helper_pid = wait_until(lambda: children(monitor.pid), "helper child")[0]

        def scanner_started():
            tasks = list(Path(f"/proc/{helper_pid}/task").iterdir())
            names = {task.name: (task / "comm").read_text().strip() for task in tasks}
            return names if "spctop-scan" in names.values() else None

        names = wait_until(scanner_started, "home scanner thread")
        ui = status(monitor.pid)
        assert all(int(value) == account.pw_uid for value in ui["Uid"].split()), "UI changed UID"
        for field in ("CapEff", "CapPrm"):
            assert int(ui[field], 16) == 0, f"UI retained {field}: {ui[field]}"
        for tid, comm in names.items():
            state = status(helper_pid, tid)
            expected = SCAN_CAPS if comm == "spctop-scan" else READ_CAPS
            assert all(int(value) == account.pw_uid for value in state["Uid"].split()), "Helper changed UID"
            assert int(state["CapEff"], 16) == expected, f"{comm}: CapEff {state['CapEff']}"
            assert int(state["CapPrm"], 16) == expected, f"{comm}: CapPrm {state['CapPrm']}"
            assert int(state["CapInh"], 16) == 0 and int(state["CapAmb"], 16) == 0
            assert int(state["NoNewPrivs"]) == 1, f"{comm} lacks NO_NEW_PRIVS"
        environment = Path(f"/proc/{helper_pid}/environ").read_bytes()
        assert environment == b"LANG=C\0", f"Helper inherited environment: {environment!r}"
        assert int(status(helper_pid)["Uid"].split()[0]) == account.pw_uid
        monitor.send_signal(signal.SIGTERM)
        monitor.wait(timeout=3)
        assert monitor.returncode == 0
        assert not Path(f"/proc/{helper_pid}").exists(), "Helper survived normal UI exit"
        print(f"PASS threads: sampling thread {READ_CAPS:#x}, scanner {SCAN_CAPS:#x}, NO_NEW_PRIVS, "
              "UI without capabilities, helper environment only LANG=C")
    finally:
        if monitor is not None and monitor.poll() is None:
            monitor.kill()
            monitor.wait(timeout=3)
        termios.tcsetattr(slave, termios.TCSANOW, before)
        os.close(master)
        os.close(slave)


def check_other_users(binary, account):
    """A root process's storage writes are visible only through the helper."""
    with tempfile.TemporaryDirectory(dir="/var/tmp") as directory:
        script = (
            "import os,sys,time\n"
            f"f=open('{directory}/root.bin','wb')\n"
            "f.write(os.urandom(16*1024*1024)); f.flush(); os.fsync(f.fileno())\n"
            "print('ready', flush=True); time.sleep(30)\n"
        )
        writer = subprocess.Popen(["python3", "-c", script], stdout=subprocess.PIPE, text=True)
        try:
            assert writer.stdout.readline().strip() == "ready"
            snapshot, _, _ = run_json(binary, account, "--no-helper", "--interval", "0.3")
            rows = {row["pid"]: row for row in snapshot["processes"]}
            access = snapshot["process_access"]
            assert writer.pid not in rows, "root process visible without the helper"
            assert not access["privileged"] and access["unreadable"] > 0, access
            snapshot, _, errors = run_json(binary, account, "--interval", "0.3")
            assert not errors, f"Unexpected helper fallback: {errors}"
            rows = {row["pid"]: row for row in snapshot["processes"]}
            access = snapshot["process_access"]
            assert access["privileged"] and access["unreadable"] == 0, access
            assert writer.pid in rows, "root process missing with the helper"
            row = rows[writer.pid]
            assert row["write_bytes"] >= 16 * MIB, row
            assert row["user"] == "root", row
        finally:
            writer.kill()
            writer.wait(timeout=3)
    print("PASS other users: a root writer is hidden and counted without the helper, "
          "and shown with its 16 MiB of writes through it")


def check_home_scan(binary, account):
    arguments = ("--scan-home", "--home-path", str(HOME), "--interval", "0.1")
    snapshot, text, _ = run_json(binary, account, "--no-helper", *arguments)
    tree = snapshot["home_tree"]
    assert not tree["privileged"]
    alice, bob = node(tree, "alice"), node(tree, "bob")
    assert alice["unreadable"] >= 1 and tree["nodes"][0]["unreadable"] >= 1, alice
    assert alice["allocated"] < MIB, "alice's private files were counted without the helper"
    assert bob["allocated"] >= MIB // 4 and bob["unreadable"] == 0, bob
    assert SECRET_NAME not in text
    snapshot, text, errors = run_json(binary, account, *arguments)
    assert not errors, errors
    tree = snapshot["home_tree"]
    assert tree["privileged"], "scan did not run in the helper"
    alice = node(tree, "alice")
    assert alice["allocated"] >= 2 * MIB and alice["unreadable"] == 0, alice
    assert alice["files"] == 2 and alice["dirs"] == 1, alice
    assert tree["nodes"][0]["unreadable"] == 0
    assert SECRET_NAME not in text, "a file name left the helper"
    projects = [tree["nodes"][index]["name"] for index in alice["children"]]
    assert projects == ["projects"], projects
    print("PASS home scan: private directory partial without the helper, complete through it; "
          "only directory names and sizes are returned")


def write_scan_roots(text, mode=0o644, path=SCAN_ROOTS):
    path.parent.mkdir(mode=0o755, exist_ok=True)
    path.write_text(text)
    os.chown(path, 0, 0)
    path.chmod(mode)


def run_scan(binary, account, root, scan=True):
    """Run --json for a home root; returns (exit code, snapshot or None, stderr)."""
    arguments = ["--home-path", str(root), "--interval", "0.1"] + (["--scan-home"] if scan else [])
    result = subprocess.run(
        [str(binary), "--json", *arguments],
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
        **unprivileged(account),
    )
    snapshot = json.loads(result.stdout) if result.returncode == 0 else None
    return result.returncode, snapshot, result.stderr


def check_scan_roots(binary, account):
    """The helper scans only roots in the root-owned allowlist (default
    /home); other roots are scanned without privileges, and a symlink below
    an allowed root cannot widen the helper's reach."""
    escape = HOME / "bob" / "escape-to-root"
    escape.symlink_to("/root")
    Path("/root/spctop-test-private").mkdir(exist_ok=True)
    try:
        # Allowed below the listed root: privileged.
        code, snapshot, errors = run_scan(binary, account, HOME / "bob")
        assert code == 0 and snapshot["home_tree"]["privileged"], errors
        assert not snapshot["home"].get("helper_refused"), snapshot["home"]
        # /root is not listed: the helper refuses and the UI scans it itself,
        # where nobody cannot read it.
        code, snapshot, errors = run_scan(binary, account, "/root", scan=False)
        assert code == 0, errors
        home = snapshot["home"]
        assert home["helper_refused"] and not home["privileged"], home
        code, snapshot, errors = run_scan(binary, account, "/root")
        assert code != 0 and "Permission denied" in errors, (code, errors)
        assert "spctop-test-private" not in errors
        # A symlink below an allowed root is not followed by the helper.
        code, snapshot, errors = run_scan(binary, account, escape)
        assert code != 0 and "Permission denied" in errors, (code, errors)
        # Without the file only /home is allowed.
        SCAN_ROOTS.unlink()
        code, snapshot, errors = run_scan(binary, account, "/home")
        assert code == 0 and snapshot["home_tree"]["privileged"], errors
        code, snapshot, errors = run_scan(binary, account, HOME, scan=False)
        assert code == 0 and snapshot["home"]["helper_refused"], snapshot["home"]
        # A file others can write is ignored: nothing is scanned privileged.
        write_scan_roots(f"{HOME}\n/home\n", mode=0o666)
        for root in (HOME, "/home"):
            code, snapshot, errors = run_scan(binary, account, root, scan=False)
            assert code == 0 and snapshot["home"]["helper_refused"], snapshot["home"]
    finally:
        escape.unlink()
        Path("/root/spctop-test-private").rmdir()
        write_scan_roots(f"# test root\n{HOME}\n")
    print("PASS scan roots: helper scans only allowlisted roots (default /home); /root, a symlink "
          "escape and an unsafe allowlist fall back to unprivileged scans")


def check_policy_migration(binary, account):
    """Legacy policies preserve restrictions; invalid policies never widen scans."""
    def expect(root, privileged):
        code, snapshot, errors = run_scan(binary, account, root, scan=False)
        assert code == 0, errors
        home = snapshot["home"]
        assert bool(home.get("helper_refused")) == (not privileged), home
        assert home["privileged"] == privileged, home

    try:
        SCAN_ROOTS.unlink()
        write_scan_roots(f"{HOME}\n", path=LEGACY_SCAN_ROOTS)
        expect(HOME, True)
        expect("/home", False)

        # The new policy replaces the old one, rather than merging allowed roots.
        write_scan_roots("/home\n")
        expect("/home", True)
        expect(HOME, False)

        # Truncating an oversized file exactly after '/' must not grant every
        # directory. Exercise the real root-owned current and legacy policies,
        # including refusal to fall back to a valid legacy policy.
        oversized = "#" + "x" * 65533 + "\n/srv/private\n"
        write_scan_roots(oversized)
        for root in ("/etc", "/home", HOME):
            expect(root, False)
        SCAN_ROOTS.unlink()
        write_scan_roots(oversized, path=LEGACY_SCAN_ROOTS)
        for root in ("/etc", "/home", HOME):
            expect(root, False)
        write_scan_roots(f"{HOME}\n", path=LEGACY_SCAN_ROOTS)

        # Invalid new policy must not fall back to the valid restrictive old one.
        write_scan_roots("/home\n", mode=0o666)
        expect("/home", False)
        expect(HOME, False)
        SCAN_ROOTS.unlink()
        SCAN_ROOTS.symlink_to("/nonexistent-spctop-policy")
        expect("/home", False)
        expect(HOME, False)
        SCAN_ROOTS.unlink()

        # A missing leaf beneath a dangling parent symlink is unsafe, not absent.
        SCAN_ROOTS.parent.rmdir()
        SCAN_ROOTS.parent.symlink_to("/nonexistent-spctop-policy")
        expect("/home", False)
        expect(HOME, False)
        SCAN_ROOTS.parent.unlink()
        SCAN_ROOTS.parent.mkdir(mode=0o755)

        # With the new policy absent, unsafe legacy parents/files still deny.
        LEGACY_SCAN_ROOTS.chmod(0o666)
        expect("/home", False)
        expect(HOME, False)
        LEGACY_SCAN_ROOTS.unlink()
        LEGACY_SCAN_ROOTS.parent.chmod(0o777)
        expect("/home", False)
        expect(HOME, False)
        LEGACY_SCAN_ROOTS.parent.chmod(0o755)
        LEGACY_SCAN_ROOTS.parent.rmdir()
        LEGACY_SCAN_ROOTS.parent.symlink_to("/nonexistent-spctop-policy")
        expect("/home", False)
        expect(HOME, False)
        LEGACY_SCAN_ROOTS.parent.unlink()

        # Only safe absence of both policies grants the documented /home default.
        expect("/home", True)
        expect(HOME, False)
    finally:
        for path in (SCAN_ROOTS, LEGACY_SCAN_ROOTS):
            if path.parent.is_symlink():
                path.parent.unlink()
            else:
                if os.path.lexists(path):
                    path.unlink()
                if path.parent.exists():
                    path.parent.chmod(0o755)
                    path.parent.rmdir()
        write_scan_roots(f"# test root\n{HOME}\n")
    print("PASS policy migration: legacy restrictions, new precedence, oversized and invalid file/ancestor "
          "fail-closed behavior and safe /home default")


def check_no_helper(binary, account):
    with tempfile.TemporaryFile(mode="w+") as output, tempfile.TemporaryFile(mode="w+") as errors:
        monitor = subprocess.Popen(
            [str(binary), "--no-helper", "--json", "--interval", "0.5"],
            stdout=output,
            stderr=errors,
            **unprivileged(account),
        )
        try:
            deadline = time.monotonic() + 5
            while monitor.poll() is None:
                assert time.monotonic() < deadline, "--no-helper did not finish"
                assert not children(monitor.pid), "--no-helper launched a helper"
                time.sleep(0.02)
            output.seek(0)
            errors.seek(0)
            assert monitor.returncode == 0, errors.read()
            assert not json.load(output)["process_access"]["privileged"]
            print("PASS --no-helper bypasses the installed helper")
        finally:
            if monitor.poll() is None:
                monitor.kill()
                monitor.wait(timeout=3)


def check_protocol(account):
    home = {"root": "/srv", "scan": False, "interval_minutes": 5, "rescan": False, "known_generation": 0}
    cases = {
        "protocol version": json.dumps({"version": 2, "home": home}).encode() + b"\n",
        "unknown field": json.dumps({"version": 1, "home": home, "command": "x"}).encode() + b"\n",
        "unknown home field": json.dumps({"version": 1, "home": {**home, "follow": True}}).encode() + b"\n",
        "traversal": json.dumps({"version": 1, "home": {**home, "root": "/srv/../etc"}}).encode() + b"\n",
        "relative": json.dumps({"version": 1, "home": {**home, "root": "etc"}}).encode() + b"\n",
        "oversized request": b"x" * 8193 + b"\n",
        "incomplete request": json.dumps({"version": 1, "home": home}).encode(),
    }
    for label, request in cases.items():
        result = subprocess.run(
            [str(HELPER), "--stdio"],
            input=request,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=5,
            check=False,
            **unprivileged(account),
        )
        reply = json.loads(result.stdout)
        assert result.returncode != 0 and reply["result"] == "error", f"Accepted {label}: {reply}"
        assert len(result.stdout) < 4096, f"Unbounded error for {label}"
    result = subprocess.run(
        [str(HELPER)], capture_output=True, timeout=5, check=False, **unprivileged(account)
    )
    assert result.returncode != 0 and b"launched by spctop" in result.stdout
    print("PASS helper rejects invalid versions, unknown fields, unsafe paths and unbounded/incomplete frames")


def read_terminal(master, output, timeout):
    if select.select([master], [], [], timeout)[0]:
        output.extend(os.read(master, 65536))
    return False


def start_pty_monitor(binary, account, interval):
    master, slave = pty.openpty()
    before = termios.tcgetattr(slave)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 120, 0, 0))
    monitor = subprocess.Popen(
        [str(binary), "--interval", interval],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env={**os.environ, "TERM": "xterm-256color", "LC_ALL": "C.UTF-8"},
        start_new_session=True,
        **unprivileged(account),
    )
    output = bytearray()
    deadline = time.monotonic() + 5
    while b"Focus" not in output or b"\x1b[?1049h" not in output:
        assert monitor.poll() is None, f"PTY monitor exited: {output!r}"
        assert time.monotonic() < deadline, f"PTY monitor did not draw: {output!r}"
        read_terminal(master, output, 0.05)
    helper_pid = wait_until(lambda: children(monitor.pid), "helper child")[0]
    return master, slave, before, monitor, helper_pid, output


def check_stopped_helper(binary, account, signal_exit=True):
    master, slave, before, monitor, helper_pid, output = start_pty_monitor(binary, account, "0.1")
    try:
        os.kill(helper_pid, signal.SIGSTOP)
        stopped_at = time.monotonic()
        # The next sample is now waiting on a helper that cannot answer.
        time.sleep(0.25)
        if not signal_exit:
            # After the ten-second deadline the monitor keeps running on direct
            # collection, reports the failure and stops and reaps the helper.
            deadline = time.monotonic() + 13
            while Path(f"/proc/{helper_pid}").exists():
                assert monitor.poll() is None, f"Helper timeout ended the monitor: {output!r}"
                assert time.monotonic() < deadline, "Stalled helper was not replaced"
                read_terminal(master, output, 0.05)
            assert time.monotonic() - stopped_at >= 9.5, "Helper response timed out prematurely"
            deadline = time.monotonic() + 3
            while b"timed out" not in output:
                assert monitor.poll() is None, f"Monitor exited after fallback: {output!r}"
                assert time.monotonic() < deadline, f"Missing timeout notice: {output!r}"
                read_terminal(master, output, 0.05)
            time.sleep(0.5)
            assert monitor.poll() is None, f"Monitor exited after fallback: {output!r}"
        monitor.send_signal(signal.SIGTERM)
        deadline = time.monotonic() + 3
        while monitor.poll() is None:
            assert time.monotonic() < deadline, "UI shutdown blocked on stopped helper"
            read_terminal(master, output, 0.05)
        while select.select([master], [], [], 0)[0]:
            read_terminal(master, output, 0)
        assert monitor.returncode == 0, f"Unexpected exit code: {monitor.returncode}"
        assert termios.tcgetattr(slave) == before, "Terminal attributes were not restored"
        assert b"\x1b[?1049l" in output and b"\x1b[?25h" in output, "Screen/cursor were not restored"
        assert not Path(f"/proc/{helper_pid}").exists(), "Stopped helper survived UI shutdown"
        if signal_exit:
            print("PASS SIGTERM interrupts a blocked helper response, restores the terminal and reaps the helper")
        else:
            print("PASS ten-second helper timeout shows a notice, falls back to direct collection and reaps the helper")
    finally:
        if Path(f"/proc/{helper_pid}").exists():
            os.kill(helper_pid, signal.SIGKILL)
        if monitor.poll() is None:
            monitor.kill()
            monitor.wait(timeout=3)
        termios.tcsetattr(slave, termios.TCSANOW, before)
        os.close(master)
        os.close(slave)


def check_abrupt_exit(binary, account):
    # Adopt only this test's orphaned descendants, so both a container PID 1 and
    # an ordinary CI process can explicitly reap the helper after killing its UI.
    libc = ctypes.CDLL(None, use_errno=True)
    previous_subreaper = ctypes.c_int()
    assert libc.prctl(37, ctypes.byref(previous_subreaper), 0, 0, 0) == 0  # GET_CHILD_SUBREAPER
    assert libc.prctl(36, 1, 0, 0, 0) == 0  # SET_CHILD_SUBREAPER
    master, slave, before, monitor, helper_pid, _ = start_pty_monitor(binary, account, "60")
    reaped = False
    try:
        monitor.kill()
        monitor.wait(timeout=3)

        def reap_helper():
            pid, result = os.waitpid(helper_pid, os.WNOHANG)
            return (pid, result) if pid else None

        _, result = wait_until(reap_helper, "orphaned helper exit on stdin EOF", timeout=3)
        reaped = True
        assert os.waitstatus_to_exitcode(result) == 0, f"Helper did not exit cleanly on EOF: {result}"
        print("PASS SIGKILL of UI closes private pipes; helper exits on EOF and is reaped")
    finally:
        if monitor.poll() is None:
            monitor.kill()
            monitor.wait(timeout=3)
        if not reaped:
            try:
                os.kill(helper_pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            try:
                os.waitpid(helper_pid, 0)
            except ChildProcessError:
                pass
        # SIGKILL cannot restore the UI's terminal; this test owns and restores it.
        termios.tcsetattr(slave, termios.TCSANOW, before)
        os.close(master)
        os.close(slave)
        assert libc.prctl(36, previous_subreaper.value, 0, 0, 0) == 0


INSTALLER_USERS = ("spctop-solo", "spctop-peer", "spctop-member")


def add_installer_accounts():
    """spctop-solo has a user-private primary group; the other two accounts
    are added to that group later to make it shared."""
    remove_installer_accounts()
    subprocess.run(["groupadd", "spctop-solo"], check=True)
    for name in INSTALLER_USERS:
        subprocess.run(["useradd", "--no-create-home", "--shell", "/usr/sbin/nologin",
                        "--gid", "spctop-solo" if name == "spctop-solo" else "nogroup", name], check=True)
    return pwd.getpwnam("spctop-solo")


def remove_installer_accounts():
    for name in INSTALLER_USERS:
        subprocess.run(["userdel", name], check=False, capture_output=True)
    subprocess.run(["groupdel", "spctop-solo"], check=False, capture_output=True)


def check_installer(script, build, account):
    digest = hashlib.sha256(build.read_bytes()).hexdigest()

    def install(success, *flags, owner=account, source=build, expect=None):
        command = ["bash", str(script), "--install", str(source), str(owner.pw_uid), str(owner.pw_gid), digest, *flags]
        result = subprocess.run(command, capture_output=True, text=True, timeout=30, check=False)
        output = result.stdout + result.stderr
        assert (result.returncode == 0) == success, f"Unexpected installer result for {flags}: {output}"
        assert expect is None or expect in output, f"Missing {expect!r} in installer output: {output}"

    def assert_installed(owner):
        installed = HELPER.stat()
        assert installed.st_uid == 0 and installed.st_gid == owner.pw_gid, (installed.st_uid, installed.st_gid)
        assert installed.st_mode & 0o7777 == 0o750, oct(installed.st_mode)
        assert RECEIPT.read_text().strip() == digest
        subprocess.run(["setcap", "-v", FILE_CAPS, str(HELPER)], check=True, stdout=subprocess.DEVNULL)
        getcap = subprocess.run(["getcap", str(HELPER)], capture_output=True, text=True, check=True).stdout
        assert "net_raw" not in getcap and "dac_override" not in getcap, getcap

    # nogroup is shared (sync, _apt) and not named after nobody, so the test
    # account needs the explicit opt-in.
    install(False, expect="Every member of this group could run the helper")
    assert not os.path.lexists(HELPER), "Installer used a shared group without --allow-shared-group"
    install(False, "--allow-owner-change", "--allow-shared-group")
    install(False, "--allow-shared-group", "--allow-shared-group")
    install(False, "--allow-any")
    shared = "--allow-shared-group"
    install(True, shared, expect="may be shared")
    assert_installed(account)
    install(True, shared)
    assert hashlib.sha256(HELPER.read_bytes()).hexdigest() == digest, "Clean update changed the binary"

    # A symlinked source is refused, in the last component or in a parent.
    link = build.with_name(build.name + ".link")
    parent_link = build.parent / "parent-link"
    link.symlink_to(build)
    parent_link.symlink_to(build.parent)
    try:
        install(False, shared, source=link)
        install(False, shared, source=parent_link / build.name, expect="free of symlinks")
    finally:
        link.unlink()
        parent_link.unlink()
    assert_installed(account)

    saved_receipt = RECEIPT.with_name(RECEIPT.name + ".test-backup")
    RECEIPT.rename(saved_receipt)
    try:
        install(False, shared)
    finally:
        saved_receipt.rename(RECEIPT)

    saved_helper = HELPER.with_name(HELPER.name + ".test-backup")
    HELPER.rename(saved_helper)
    try:
        HELPER.symlink_to(build)
        install(False, shared)
        assert HELPER.is_symlink(), "Installer replaced an untracked symlink"
    finally:
        HELPER.unlink()
        saved_helper.rename(HELPER)

    # Moving access to another group needs --allow-owner-change; a private
    # group needs no --allow-shared-group, but becomes shared by members.
    solo_build = build.with_name(build.name + ".solo")
    try:
        solo = add_installer_accounts()
        shutil.copy2(build, solo_build)
        os.chown(solo_build, solo.pw_uid, solo.pw_gid)
        install(False, owner=solo, source=solo_build, expect="--allow-owner-change")
        assert_installed(account)
        install(True, "--allow-owner-change", owner=solo, source=solo_build, expect="moving helper access")
        assert_installed(solo)
        install(True, owner=solo, source=solo_build)
        # Supplementary membership and another account's primary group.
        for change, undo, member in ((["--append", "--groups", "spctop-solo"], ["--groups", ""], "spctop-member"),
                                     (["--gid", "spctop-solo"], ["--gid", "nogroup"], "spctop-peer")):
            subprocess.run(["usermod", *change, member], check=True)
            install(False, owner=solo, source=solo_build, expect=member)
            install(True, shared, owner=solo, source=solo_build)
            assert_installed(solo)
            subprocess.run(["usermod", *undo, member], check=True)
        install(True, owner=solo, source=solo_build)
        install(False, shared, expect="--allow-owner-change")
        assert_installed(solo)
        install(True, shared, "--allow-owner-change")
        assert_installed(account)
    finally:
        solo_build.unlink(missing_ok=True)
        remove_installer_accounts()

    # A replacement must not silently erase a locally changed installed binary.
    with HELPER.open("ab") as stream:
        stream.write(b"changed-by-test")
    try:
        install(False, shared)
        assert HELPER.read_bytes().endswith(b"changed-by-test")
    finally:
        # Restore only this test-owned installation, then verify the normal path.
        HELPER.unlink()
        RECEIPT.unlink()
        install(True, shared)
    assert_installed(account)
    print("PASS installer: root ownership, only DAC_READ_SEARCH+SYS_PTRACE, clean update, "
          "changed/untracked/symlink refusal, shared-group and owner-change opt-ins")


def check_terminal_hangup(binary, account):
    """A terminal that goes away (PTY master closed, no SIGHUP) ends the UI
    promptly; the UI stops and reaps its helper itself."""
    master, slave, before, monitor, helper_pid, _ = start_pty_monitor(binary, account, "60")
    try:
        os.close(master)
        master = None
        started = time.monotonic()
        monitor.wait(timeout=3)
        elapsed = time.monotonic() - started
        assert monitor.returncode == 0, f"hangup exit code {monitor.returncode}"
        assert not Path(f"/proc/{helper_pid}").exists(), "helper was not stopped and reaped by the UI"
        print(f"PASS terminal hangup: UI exited cleanly after {elapsed:.2f}s and reaped its helper")
    finally:
        if monitor.poll() is None:
            monitor.kill()
            monitor.wait(timeout=3)
        if master is not None:
            # Only a terminal that is still open can be restored.
            termios.tcsetattr(slave, termios.TCSANOW, before)
            os.close(master)
        os.close(slave)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--isolated", action="store_true", help="confirm this is a disposable container/runner")
    parser.add_argument("binary", type=Path)
    parser.add_argument("helper_binary", type=Path)
    args = parser.parse_args()
    if not args.isolated or os.geteuid() != 0:
        parser.error("run as root with --isolated only in a disposable Linux container/runner")
    effective = int(status(os.getpid())["CapEff"], 16)
    required = READ_CAPS | (1 << 31)  # SETFCAP is needed only by the installer.
    assert effective & required == required, "Container needs DAC_READ_SEARCH, SYS_PTRACE and SETFCAP"
    assert not os.path.lexists(HELPER) and not os.path.lexists(RECEIPT), "Refusing a pre-existing helper installation"
    assert not os.path.lexists(HOME), f"Refusing a pre-existing {HOME}"
    assert not os.path.lexists(SCAN_ROOTS.parent), f"Refusing a pre-existing {SCAN_ROOTS.parent}"
    assert not os.path.lexists(LEGACY_SCAN_ROOTS.parent), f"Refusing a pre-existing {LEGACY_SCAN_ROOTS.parent}"
    binary = args.binary.resolve(strict=True)
    helper_binary = args.helper_binary.resolve(strict=True)
    script = Path(__file__).resolve().parents[1] / "scripts/setup-helper.sh"
    account = pwd.getpwnam("nobody")
    assert account.pw_uid != 0 and account.pw_gid != 0
    try:
        make_home(account)
        # The helper may scan the test tree only because it is listed here.
        write_scan_roots(f"# test root\n{HOME}\n")
        with tempfile.TemporaryDirectory(prefix="spctop-helper-test-") as directory:
            # Source ownership is checked by the real installer; allow nobody to
            # traverse the test staging directory without making it writable.
            os.chmod(directory, 0o755)
            build = Path(directory) / "spctop-collector"
            shutil.copy2(helper_binary, build)
            os.chown(build, account.pw_uid, account.pw_gid)
            # Popen(user=...) changes credentials but inherits root's environment.
            # Give the UI a private, accessible preferences directory instead of
            # resolving settings underneath the test runner's root home.
            config_home = Path(directory) / "preferences"
            config_home.mkdir(mode=0o700)
            os.chown(config_home, account.pw_uid, account.pw_gid)
            with patch.dict(os.environ, {"XDG_CONFIG_HOME": str(config_home)}):
                check_installer(script, build, account)
                check_threads(binary, account)
                check_other_users(binary, account)
                check_home_scan(binary, account)
                check_scan_roots(binary, account)
                check_policy_migration(binary, account)
                check_no_helper(binary, account)
                check_protocol(account)
                check_stopped_helper(binary, account)
                check_stopped_helper(binary, account, signal_exit=False)
                check_abrupt_exit(binary, account)
                check_terminal_hangup(binary, account)
    finally:
        # These paths were absent before this explicitly isolated test started.
        for path in (HELPER, RECEIPT):
            if os.path.lexists(path):
                path.unlink()
        shutil.rmtree(HOME, ignore_errors=True)
        for directory in (SCAN_ROOTS.parent, LEGACY_SCAN_ROOTS.parent):
            if directory.is_symlink():
                directory.unlink()
            else:
                shutil.rmtree(directory, ignore_errors=True)
    print("All capability helper checks passed.")


if __name__ == "__main__":
    main()
