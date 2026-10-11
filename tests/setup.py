#!/usr/bin/env python3
"""Exercise Setup and persisted preferences through the real terminal UI.

Run without root: python3 tests/setup.py target/release/dwtop
Every process uses a temporary XDG directory and its own pseudo-terminal.
"""

import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import stat
import struct
import subprocess
import sys
import tempfile
import termios
import time


F2 = b"\x1bOQ"
F10 = b"\x1b[21~"
F12 = b"\x1b[24~"
RIGHT = b"\x1b[C"
LEFT = b"\x1b[D"
DOWN = b"\x1b[B"
UP = b"\x1b[A"
BACKSPACE = b"\x7f"
ANSI = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]")


def environment(config_home, **overrides):
    env = {**os.environ, "XDG_CONFIG_HOME": str(config_home), "TERM": "xterm-256color"}
    # Exercise the color setting independently of a CI runner's color policy.
    env.pop("NO_COLOR", None)
    # Keep the terminal text English regardless of the runner's locale.
    for name in ("LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"):
        env.pop(name, None)
    env["LC_ALL"] = "C.UTF-8"
    for name, value in overrides.items():
        if value is None:
            env.pop(name, None)
        else:
            env[name] = value
    return env


DEFAULT_ARGUMENTS = ("--interval", "0.1", "--history", "10")


class Terminal:
    def __init__(self, binary, config_home, arguments=DEFAULT_ARGUMENTS, ready=b"Focus", **env):
        self.binary = binary
        self.config_home = config_home
        self.arguments = arguments
        self.ready = ready
        self.env = env
        self.master, self.slave = pty.openpty()
        self.before = termios.tcgetattr(self.slave)
        self.output = bytearray()
        self.child = None

    def __enter__(self):
        try:
            fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 120, 0, 0))
            self.child = subprocess.Popen(
                [str(self.binary), "--demo", *self.arguments],
                stdin=self.slave,
                stdout=self.slave,
                stderr=self.slave,
                env=environment(self.config_home, **self.env),
                start_new_session=True,
            )
            self.wait_for(
                lambda: self.ready in self.output and b"\x1b[?1049h" in self.output,
                "monitor startup",
            )
            return self
        except BaseException:
            self.close()
            raise

    def read(self, timeout=0.05):
        if select.select([self.master], [], [], timeout)[0]:
            self.output.extend(os.read(self.master, 65536))

    def wait_for(self, condition, label, timeout=3):
        deadline = time.monotonic() + timeout
        while not condition():
            assert self.child.poll() is None, f"{label}: process exited: {self.tail()}"
            assert time.monotonic() < deadline, f"{label}: timed out: {self.tail()}"
            self.read()

    def expect(self, text, after=0):
        self.wait_for(
            lambda: text in ANSI.sub(b"", bytes(self.output[after:])),
            f"terminal text {text!r}",
        )

    def send(self, keys):
        assert self.child.poll() is None, f"cannot send input to exited monitor: {self.tail()}"
        mark = len(self.output)
        os.write(self.master, keys)
        # Let the monitor consume each burst before the next one arrives.
        time.sleep(0.05)
        return mark

    def tail(self):
        return repr(ANSI.sub(b"", bytes(self.output[-1800:])))

    def quit(self):
        self.send(b"q")
        deadline = time.monotonic() + 3
        while self.child.poll() is None:
            assert time.monotonic() < deadline, f"q did not exit within three seconds: {self.tail()}"
            self.read()
        assert self.child.returncode == 0, f"unexpected exit {self.child.returncode}: {self.tail()}"
        assert termios.tcgetattr(self.slave) == self.before, "terminal mode was not restored"

    def close(self):
        if self.child is not None and self.child.poll() is None:
            self.child.kill()
            self.child.wait(timeout=3)
        termios.tcsetattr(self.slave, termios.TCSANOW, self.before)
        os.close(self.master)
        os.close(self.slave)

    def __exit__(self, *_):
        self.close()


def read_saved(path):
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        return None


def run_once(binary, config_home, *arguments, mode="--once"):
    result = subprocess.run(
        [str(binary), "--demo", mode, *arguments],
        env=environment(config_home),
        capture_output=True,
        text=True,
        timeout=3,
        check=True,
    )
    return result.stdout, result.stderr


DEFAULTS = {
    "version": 1,
    "device": None,
    "interval_ms": 1000,
    "history_seconds": 60,
    "color": True,
    "show_graph": True,
    "graph_style": "steps",
    "read_color": "green",
    "write_color": "yellow",
    "view": "filesystems",
    "sort": "io",
    "show_idle": False,
    "language": "auto",
    "show_virtual": False,
    "show_stacked": False,
    "home_path": "/home",
    "rescan_minutes": 5,
    "apparent_size": False,
    "process_rows": 5,
    "show_io_series": True,
    "show_usage_series": True,
    "usage_color": "magenta",
}


def persistence_and_overrides(binary, config_home):
    path = config_home / "dwtop/config.json"
    with Terminal(binary, config_home) as terminal:
        assert not path.exists(), "opening the monitor unexpectedly wrote settings"
        assert b"Settings:" not in terminal.output, "missing settings should silently use defaults"
        mark = terminal.send(F2)
        terminal.expect(b"Options", mark)
        # General: move from Color to the update interval and increase it.
        mark = terminal.send(RIGHT + DOWN + b"+")
        terminal.expect(b"Unsaved changes", mark)
        # Device: show loop/ram/zram and stacked devices, then select All disks.
        terminal.send(LEFT + DOWN + RIGHT + b" " + DOWN + b" " + DOWN + DOWN + b"\r")
        # Chart: Braille drawing and the next read color.
        terminal.send(LEFT + DOWN + RIGHT + DOWN + DOWN + b"\r" + DOWN + b"+")
        # Home: apparent sizes, then edit the path to /tmp.
        terminal.send(LEFT + DOWN + DOWN + RIGHT + DOWN + DOWN + b"\r" + UP + UP + b"\r")
        mark = terminal.send(BACKSPACE * 5 + b"/tmp")
        # Only changed cells are redrawn: the edit ends with "p_] Home path".
        terminal.expect(b"p_] Home path", mark)
        terminal.send(b"\r")
        assert not path.exists(), "editing Setup unexpectedly saved without F12"
        terminal.send(F12)
        terminal.wait_for(lambda: read_saved(path) is not None, "F12 saved settings")
        saved = read_saved(path)
        # --interval 0.1 became 0.2 in Setup and is saved; the untouched
        # --history 10 override is temporary and keeps the default.
        expected = {
            **DEFAULTS,
            "interval_ms": 200,
            "graph_style": "braille",
            "read_color": "yellow",
            "device": "all",
            "show_virtual": True,
            "show_stacked": True,
            "apparent_size": True,
            "home_path": "/tmp",
        }
        assert saved == expected, f"Setup did not persist exactly the edits: {saved}"
        assert stat.S_IMODE(path.stat().st_mode) == 0o600, "saved settings are not private"
        terminal.expect(b"Saved ")
        mark = terminal.send(F10)
        terminal.expect(b"MOUNT", mark)
        assert terminal.child.poll() is None, "F10 in Setup unexpectedly quit dwtop"
        terminal.quit()

    before = path.read_bytes()
    stdout, stderr = run_once(binary, config_home)
    assert not stderr, f"unexpected settings warning after restart: {stderr}"
    assert "all physical disks" in stdout.splitlines()[0], "restart lost the saved device choice"
    stdout, _ = run_once(binary, config_home, mode="--json")
    assert json.loads(stdout)["home"]["root"] == "/tmp", "restart lost the saved home path"
    assert path.read_bytes() == before, "reading settings unexpectedly rewrote them"

    # A 60-second saved interval makes a missing CLI override fail this 3-second
    # subprocess deadline, rather than accidentally passing with a short default.
    saved["interval_ms"] = 60_000
    path.write_text(json.dumps(saved))
    before = path.read_bytes()
    stdout, stderr = run_once(
        binary, config_home, "--interval", "0.1", "--device", "nvme0n1", "--home-path", "/srv",
        mode="--json",
    )
    assert not stderr, f"unexpected warning with CLI overrides: {stderr}"
    assert json.loads(stdout)["home"]["root"] == "/srv", "--home-path did not override"
    stdout, _ = run_once(binary, config_home, "--interval", "0.1", "--device", "/dev/nvme0n1")
    assert stdout.startswith("dwtop DEMO  nvme0n1"), "--device did not override the saved choice"
    assert path.read_bytes() == before, "temporary CLI overrides overwrote saved preferences"
    print("PASS Setup: F2 edits incl. home path, F12 persistence, F10 returns, restart and CLI overrides")


def overrides_not_persisted(binary, config_home):
    path = config_home / "dwtop/config.json"
    path.parent.mkdir(mode=0o700, parents=True)
    original = {
        "version": 1,
        "device": "dwtop-gone",
        "interval_ms": 1000,
        "history_seconds": 120,
        "color": True,
        "future_option": {"kept": True},
    }
    path.write_text(json.dumps(original))
    path.chmod(0o600)
    arguments = ("--interval", "0.1", "--history", "10", "--no-color", "--all-devices",
                 "--sort", "write", "--home-path", "/srv")
    with Terminal(binary, config_home, arguments, NO_COLOR="1") as terminal:
        terminal.expect(b"unknown keys")
        terminal.expect(b"Saved device dwtop-gone unavailable")
        # Saving without changes keeps every file value, despite CLI overrides,
        # NO_COLOR and the automatic fallback for the missing device.
        before = path.read_bytes()
        terminal.send(F12)
        terminal.wait_for(lambda: path.read_bytes() != before, "F12 rewrote settings")
        saved = read_saved(path)
        assert saved == {**DEFAULTS, **original}, f"F12 persisted overrides: {saved}"
        # An override that the user explicitly changes in Setup is saved.
        mark = terminal.send(F2 + DOWN + DOWN + RIGHT + DOWN + b"+")
        terminal.expect(b"Unsaved changes", mark)
        terminal.send(F12)
        terminal.wait_for(lambda: read_saved(path)["history_seconds"] != 120, "F12 saved Setup")
        saved = read_saved(path)
        assert saved["history_seconds"] == 20, f"Setup change was not saved: {saved}"
        for key in ("device", "interval_ms", "color", "future_option"):
            assert saved[key] == original[key], f"F12 changed untouched {key}: {saved}"
        for key in ("show_virtual", "sort", "home_path"):
            assert saved[key] == DEFAULTS[key], f"F12 saved the {key} override: {saved}"
        terminal.send(F10)
        terminal.quit()
    print("PASS overrides: CLI options, NO_COLOR and device fallback stay temporary; Setup edits save")


def german_interface(binary, config_home):
    path = config_home / "dwtop/config.json"
    with Terminal(binary, config_home, ready="Fokus".encode(), LC_ALL=None, LANG="de_DE.UTF-8") as terminal:
        terminal.expect("Hilfe".encode())
        terminal.expect("Speichern".encode())
        terminal.expect(b"FREI")
        mark = terminal.send(F2)
        terminal.expect(b"Optionen: Allgemein", mark)
        # General > Language: Auto -> English switches the running UI at once.
        mark = terminal.send(RIGHT + DOWN + DOWN + b"\r")
        # Only changed cells are redrawn, so look for short distinct fragments.
        terminal.expect(b"English] Language", mark)
        terminal.expect(b"Done", mark)
        terminal.send(F12)
        terminal.wait_for(lambda: read_saved(path) is not None, "F12 saved the language")
        assert read_saved(path)["language"] == "en", read_saved(path)
        terminal.send(F10)
        terminal.quit()
    # gettext's LANGUAGE priority list wins over LANG, except in the C locale.
    with Terminal(binary, config_home / "language", ready="Fokus".encode(), LC_ALL=None,
                  LANG="en_US.UTF-8", LANGUAGE="de_DE:en") as terminal:
        terminal.quit()
    with Terminal(binary, config_home / "c-locale", ready=b"Focus", LC_ALL="C.UTF-8",
                  LANGUAGE="de_DE") as terminal:
        terminal.quit()
    print("PASS language: German from LANG and from LANGUAGE (not in the C locale), "
          "Setup switches to English and F12 saves it")


def disk_overview(binary, config_home):
    """All disks are visible at once; the new overview options persist."""
    path = config_home / "dwtop/config.json"
    with Terminal(binary, config_home) as terminal:
        # Unchanged (blank) cells are not redrawn, so look for space-free text.
        for text in (b"Disks", b"nvme0n1", b"nvme1n1", b"sda", b"sdb", b"mounted]", b"100%",
                     b"MOUNT"):
            terminal.expect(text)
        # Tab moves the focus to the next disk; F12 below saves it.
        terminal.send(b"\t")
        mark = terminal.send(F2)
        terminal.expect(b"Options", mark)
        # Chart: hide the fill-level series and change its color.
        terminal.send(DOWN + DOWN + RIGHT + DOWN * 6 + b"\r" + DOWN + b"+")
        # Table: three process rows.
        mark = terminal.send(LEFT + DOWN + RIGHT + DOWN * 3 + b"-")
        # Only changed cells are redrawn: 5 -> 4 -> 3 shows as "4" then "3".
        terminal.expect(b"Process rows", mark)
        mark = terminal.send(b"-")
        terminal.expect(b"3", mark)
        terminal.send(F12)
        terminal.wait_for(lambda: read_saved(path) is not None, "F12 saved overview options")
        saved = read_saved(path)
        expected = {
            **DEFAULTS,
            "show_usage_series": False,
            "usage_color": "white",
            "process_rows": 3,
            "device": "nvme1n1",
        }
        assert saved == expected, f"overview options were not saved exactly: {saved}"
        terminal.send(F10)
        terminal.quit()
    stdout, _ = run_once(binary, config_home, "--interval", "0.1")
    disks = [line.split()[0] for line in stdout.splitlines()
             if line.startswith(("nvme", "sd", "loop", "dm-"))]
    assert disks == ["nvme0n1", "nvme1n1", "sda", "sdb"], f"--once disk list: {stdout}"
    assert "not mounted" in stdout, stdout
    stdout, _ = run_once(binary, config_home, "--interval", "0.1", mode="--json")
    devices = {d["name"]: d for d in json.loads(stdout)["devices"]}
    assert devices["sdb"]["usage"]["filesystems"] == 0, devices["sdb"]
    assert devices["nvme0n1"]["usage"]["filesystems"] == 3, devices["nvme0n1"]
    # LUKS on nvme1n1 is listed in JSON but folded into nvme1n1 on screen.
    assert devices["dm-0"]["backing_disks"] == ["nvme1n1"], devices["dm-0"]
    assert devices["nvme1n1"]["usage"]["filesystems"] == 1, devices["nvme1n1"]
    print("PASS overview: all disks with meters and graphs, Tab focus, options saved, --once/--json usage")


def views_and_home(binary, config_home):
    with Terminal(binary, config_home) as terminal:
        terminal.expect(b"*/home")
        initial = ANSI.sub(b"", bytes(terminal.output))
        assert b"READ/s" not in initial and b"IOPS" not in initial and b"COMMAND" not in initial, initial
        mark = terminal.send(b"v")
        terminal.expect(b"I/O:", mark)
        mark = terminal.send(DOWN + DOWN + b"\r")
        terminal.expect(b"COMMAND", mark)
        mark = terminal.send(b"\x1b")
        terminal.expect(b"MOUNT", mark)
        mark = terminal.send(b"i")
        terminal.expect(b"COMMAND", mark)
        mark = terminal.send(b"2")
        terminal.expect(b"tim/", mark)
        mark = terminal.send(b"\r")
        terminal.expect(b".cache/", mark)
        terminal.expect(b"smaller dirs]", mark)
        mark = terminal.send(BACKSPACE)
        terminal.expect(b"anna/", mark)
        mark = terminal.send(b"1")
        terminal.expect(b"MOUNT", mark)
        terminal.quit()
    stdout, _ = run_once(binary, config_home, "--interval", "0.1")
    assert "READ/s" not in stdout and "IOPS" not in stdout and "COMMAND" not in stdout, stdout
    for view in ("io", "processes"):
        stdout, _ = run_once(binary, config_home, "--interval", "0.1", "--view", view)
        assert "READ/s" in stdout and "IOPS" in stdout and "COMMAND" in stdout, stdout
    print("PASS views: Space default, F4/v menu, I/O shortcuts/back, Home drill-down and CLI alias")


def malformed_settings(binary, config_home):
    path = config_home / "dwtop/config.json"
    path.parent.mkdir(mode=0o700, parents=True)
    original = b'{"show_idle": true, this is deliberately incomplete\n'
    path.write_bytes(original)
    path.chmod(0o600)
    stdout, stderr = run_once(binary, config_home, "--interval", "0.1")
    assert "Settings:" in stderr and "cannot load" in stderr, "malformed JSON had no warning"
    assert stdout.splitlines()[1].startswith("SPACE "), "malformed JSON did not fall back to defaults"

    with Terminal(binary, config_home) as terminal:
        terminal.expect(b"Settings:")
        mark = terminal.send(F2)
        terminal.expect(b"Options", mark)
        mark = terminal.send(F12)
        terminal.expect(b"Save failed:", mark)
        assert path.read_bytes() == original, "F12 discarded the malformed settings file"
        assert list(path.parent.iterdir()) == [path], "failed save left temporary files"
        mark = terminal.send(F10)
        terminal.expect(b"MOUNT", mark)
        terminal.quit()
    assert path.read_bytes() == original, "exit overwrote malformed settings"
    print("PASS invalid settings: visible warning, usable defaults, original file preserved")


def vanished_device(binary, config_home):
    path = config_home / "dwtop/config.json"
    path.parent.mkdir(mode=0o700, parents=True)
    path.write_text(json.dumps({"version": 1, "device": "dwtop-gone"}))
    path.chmod(0o600)
    original = path.read_bytes()
    stdout, stderr = run_once(binary, config_home, "--interval", "0.1")
    assert "Saved device dwtop-gone unavailable" in stderr, "missing device had no warning"
    assert "automatic selection" in stderr, "device warning did not explain the fallback"
    assert "nvme0n1" in stdout.splitlines()[0], "missing device did not use the demo default"
    assert path.read_bytes() == original, "automatic fallback overwrote the saved device"
    result = subprocess.run(
        [str(binary), "--demo", "--once", "--device", "dwtop-gone"],
        env=environment(config_home), capture_output=True, text=True, timeout=3, check=False,
    )
    assert result.returncode != 0 and "does not exist" in result.stderr, result
    print("PASS vanished device: visible warning, automatic fallback, saved choice preserved")


def legacy_preferences(binary, config_home):
    legacy = config_home / "disktop/config.json"
    current = config_home / "dwtop/config.json"
    legacy.parent.mkdir(mode=0o700, parents=True)
    original = json.dumps({"view": "processes", "home_path": "/srv", "future_option": True})
    legacy.write_text(original)
    legacy.chmod(0o600)
    stdout, stderr = run_once(binary, config_home, "--interval", "0.1")
    assert "READ/s" not in stdout and "SPACE " in stdout, "legacy process default did not migrate"
    assert "future_option" in stderr, "legacy unknown key was not reported"
    assert not current.exists(), "reading legacy preferences wrote a new file"
    with Terminal(binary, config_home) as terminal:
        terminal.send(F12)
        terminal.wait_for(lambda: read_saved(current) is not None, "F12 migrated legacy settings")
        saved = read_saved(current)
        assert saved["view"] == "filesystems" and saved["home_path"] == "/srv", saved
        assert saved["future_option"] is True, saved
        terminal.send(b"i" + F12)
        terminal.wait_for(lambda: read_saved(current)["view"] == "io", "F12 persisted explicit I/O")
        terminal.quit()
    assert legacy.read_text() == original, "migration modified the original file"
    legacy.write_text("invalid")
    stdout, _ = run_once(binary, config_home, "--interval", "0.1")
    assert "READ/s" in stdout, "new I/O preference lost to legacy/default preferences"
    print("PASS migration: legacy processes opens Space; F12 preserves values; explicit new I/O wins")


def spctop_preferences(binary, config_home):
    previous = config_home / "spctop/config.json"
    oldest = config_home / "disktop/config.json"
    current = config_home / "dwtop/config.json"
    for path in (previous, oldest):
        path.parent.mkdir(mode=0o700, parents=True)
    original = json.dumps({"view": "io", "home_path": "/srv", "future_option": {"keep": True}})
    previous.write_text(original)
    previous.chmod(0o600)
    oldest.write_text(json.dumps({"view": "processes", "home_path": "/oldest"}))
    oldest.chmod(0o600)
    stdout, _ = run_once(binary, config_home, "--interval", "0.1")
    assert "READ/s" in stdout, "spctop I/O choice was migrated to Space or shadowed by disktop"
    assert not current.exists(), "loading wrote dwtop preferences"
    previous.write_text("invalid")
    stdout, stderr = run_once(binary, config_home, "--interval", "0.1")
    assert "cannot load" in stderr and "spctop/config.json" in stderr, stderr
    with Terminal(binary, config_home) as terminal:
        mark = terminal.send(F2)
        terminal.expect(b"Options", mark)
        mark = terminal.send(F12)
        terminal.expect(b"Save failed:", mark)
        assert not current.exists(), "invalid intermediate preferences were silently migrated"
        terminal.send(F10)
        terminal.quit()
    previous.write_text(original)
    with Terminal(binary, config_home) as terminal:
        terminal.expect(b"COMMAND")
        terminal.send(F12)
        terminal.wait_for(lambda: read_saved(current) is not None, "F12 migrated spctop")
        saved = read_saved(current)
        assert saved["view"] == "io" and saved["home_path"] == "/srv", saved
        assert saved["future_option"] == {"keep": True}, saved
        terminal.quit()
    assert previous.read_text() == original
    assert json.loads(oldest.read_text())["home_path"] == "/oldest"
    previous.write_text("invalid")
    stdout, stderr = run_once(binary, config_home, "--interval", "0.1")
    assert "READ/s" in stdout and "cannot load" not in stderr, stderr
    print("PASS rename chain: dwtop > spctop > disktop; I/O and unknown keys preserved; invalid higher file blocks migration")


def main():
    if len(sys.argv) != 2:
        raise SystemExit("Usage: python3 tests/setup.py /path/to/dwtop")
    binary = Path(sys.argv[1]).resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="dwtop-setup-test-") as temporary:
        root = Path(temporary)
        persistence_and_overrides(binary, root / "preferences")
        overrides_not_persisted(binary, root / "overrides")
        german_interface(binary, root / "german")
        disk_overview(binary, root / "overview")
        views_and_home(binary, root / "views")
        legacy_preferences(binary, root / "legacy")
        spctop_preferences(binary, root / "previous")
        malformed_settings(binary, root / "malformed")
        vanished_device(binary, root / "vanished")
    print("All Setup and configuration integration checks passed.")


if __name__ == "__main__":
    main()
