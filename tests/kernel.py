#!/usr/bin/env python3
"""Real NFS outage and hidepid checks; run ONLY in a disposable QEMU/KVM VM.

See tests/kernel.md. No container/host execution: this test changes the guest's
proc mount, installs a temporary helper, exports loopback NFS, and drops only
that export's traffic in a dedicated nftables table. Cleanup runs on failure.
"""

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import pwd
import select
import shutil
import struct
import subprocess
import tempfile
import termios
import time

HELPER = Path('/usr/local/libexec/dwtop-collector')
RECEIPT = HELPER.with_name('.dwtop-collector.sha256')
EXPORT = Path('/etc/exports.d/dwtop-kernel-test.exports')
TABLE = 'dwtop_kernel_test'
SECRET = 'dwtop-peer-private-command-line'


def run(*args, **kwargs):
    return subprocess.run(args, check=True, text=True, capture_output=True, timeout=30, **kwargs)


def identity(account, groups=()):
    return dict(user=account.pw_uid, group=account.pw_gid, extra_groups=list(groups))


def snapshot(binary, account, *args, groups=()):
    started = time.monotonic()
    result = run(str(binary), '--json', '--interval', '0.1', *args,
                 env={**os.environ, 'HOME': '/nonexistent', 'XDG_CONFIG_HOME': '/nonexistent'},
                 **identity(account, groups))
    elapsed = time.monotonic() - started
    assert elapsed < 5, f'sampling blocked for {elapsed:.2f}s'
    assert not result.stderr, result.stderr
    data = json.loads(result.stdout)
    assert data['devices'], 'local disks disappeared'
    assert not data['demo']
    return data, elapsed


def persistent_hidepid(account, peer, writer, evidence):
    """Remounts must affect the very same capability helper, including caches."""
    run('mount', '-o', 'remount,hidepid=0,gid=0', '/proc')
    groups = (peer.pw_gid,)
    helper = subprocess.Popen([str(HELPER), '--stdio'], stdin=subprocess.PIPE,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              **identity(account, groups))
    helper_pid = helper.pid
    try:
        steps = ((0, 0, 'private'), (1, 0, 'redacted'), (2, 0, 'hidden'),
                 (1, account.pw_gid, 'private'), (2, peer.pw_gid, 'private'),
                 (2, 0, 'hidden'), (0, 0, 'private'))
        for number, (mode, group, visibility) in enumerate(steps):
            run('mount', '-o', f'remount,hidepid={mode},gid={group}', '/proc')
            kernel = subprocess.run(['cat', f'/proc/{writer.pid}/cmdline'],
                                    capture_output=True, **identity(account, groups))
            assert (kernel.returncode == 0) == (visibility == 'private'), (
                mode, group, kernel.returncode)
            request = {'version': 1, 'home': {'root': '/home', 'scan': False,
                       'interval_minutes': 5, 'rescan': False, 'known_generation': 0}}
            started = time.monotonic()
            helper.stdin.write(json.dumps(request).encode() + b'\n')
            helper.stdin.flush()
            assert select.select([helper.stdout], [], [], 5)[0], 'persistent helper stopped replying'
            reply = json.loads(helper.stdout.readline())
            elapsed = time.monotonic() - started
            assert reply['result'] == 'snapshot', reply
            assert elapsed < 5
            assert helper.pid == helper_pid and helper.poll() is None, 'helper restarted/exited'
            data = reply['snapshot']
            rows = {row['pid']: row for row in data['processes']}
            assert data['process_access']['privileged']
            if visibility == 'private':
                assert SECRET in rows[writer.pid]['command'], rows.get(writer.pid)
            elif visibility == 'redacted':
                assert rows[writer.pid]['command'] == '[python3]', rows.get(writer.pid)
                assert SECRET not in json.dumps(data)
            else:
                assert writer.pid not in rows
                assert SECRET not in json.dumps(data)
            (evidence / f'hidepid-persistent-{number}-{mode}-gid-{group}.json').write_text(
                json.dumps(data, indent=2))
            print(f'PASS persistent helper pid={helper_pid} hidepid={mode} gid={group}: '
                  f'{visibility}; {elapsed:.3f}s', flush=True)
    finally:
        helper.stdin.close()
        try:
            helper.wait(timeout=3)
        except subprocess.TimeoutExpired:
            helper.kill()
            helper.wait(timeout=3)
        helper.stdout.close()
        helper.stderr.close()
        run('mount', '-o', 'remount,hidepid=0,gid=0', '/proc')


def hidepid(binary, account, peer, evidence):
    writer = subprocess.Popen(['python3', '-c', 'import time; time.sleep(300)', SECRET],
                              **identity(peer))
    try:
        time.sleep(0.1)
        persistent_hidepid(account, peer, writer, evidence)
        for mode in (1, 2):
            run('mount', '-o', f'remount,hidepid={mode},gid=0', '/proc')
            # Prove real kernel enforcement before examining application output.
            denied = subprocess.run(['cat', f'/proc/{writer.pid}/cmdline'],
                                    capture_output=True, **identity(account))
            assert denied.returncode != 0, 'kernel did not enforce hidepid'
            listed = run('python3', '-c', f'import os; print("{writer.pid}" in os.listdir("/proc"))',
                         **identity(account)).stdout.strip()
            assert listed == ('True' if mode == 1 else 'False'), listed
            for helper in (False, True):
                data, elapsed = snapshot(binary, account, *(() if helper else ('--no-helper',)))
                rows = {row['pid']: row for row in data['processes']}
                assert data['process_access']['privileged'] == helper
                assert '/proc hidepid' in data['process_access']['message'], data['process_access']
                if not helper and mode == 1:
                    assert data['process_access']['unreadable'] > 0, 'denied stat miscounted as exit'
                if not helper and mode == 2:
                    assert 'count unavailable' in data['process_access']['message']
                if helper and mode == 1:
                    assert writer.pid in rows, 'helper lost noaccess peer counters'
                    assert rows[writer.pid]['command'] == '[python3]', rows[writer.pid]
                else:
                    assert writer.pid not in rows, rows.get(writer.pid)
                assert SECRET not in json.dumps(data), 'private command line leaked'
                (evidence / f'hidepid-{mode}-{helper}.json').write_text(json.dumps(data, indent=2))
                print(f'PASS hidepid={mode} helper={helper}: {elapsed:.3f}s; '
                      f'access={data["process_access"]}', flush=True)
            # Primary and supplementary membership must both exempt policy.
            for group, groups in ((account.pw_gid, ()), (peer.pw_gid, (peer.pw_gid,))):
                run('mount', '-o', f'remount,hidepid={mode},gid={group}', '/proc')
                visible = run('cat', f'/proc/{writer.pid}/cmdline', **identity(account, groups))
                assert SECRET in visible.stdout
                for helper in (False, True):
                    data, _ = snapshot(binary, account, *(() if helper else ('--no-helper',)), groups=groups)
                    rows = {row['pid']: row for row in data['processes']}
                    if helper:
                        assert SECRET in rows[writer.pid]['command'], rows.get(writer.pid)
                    else:
                        # gid lifts hidepid, not the separate ptrace/io permission check.
                        assert writer.pid not in rows
                print(f'PASS hidepid={mode} gid={group} groups={groups}: kernel exemption and direct/helper policy', flush=True)
    finally:
        run('mount', '-o', 'remount,hidepid=0,gid=0', '/proc')
        writer.terminate()
        writer.wait(timeout=3)


def read_pty(fd, data, seconds):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if select.select([fd], [], [], max(0, min(0.05, deadline - time.monotonic())))[0]:
            try:
                data.extend(os.read(fd, 65536))
            except OSError:
                break


def interactive(binary, account, root, helper, evidence):
    master, slave = pty.openpty()
    before = termios.tcgetattr(slave)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 36, 120, 0, 0))
    monitor = subprocess.Popen([str(binary), '--view', 'home', '--home-path', str(root),
                                '--interval', '0.1', *(() if helper else ('--no-helper',))],
                               stdin=slave, stdout=slave, stderr=slave,
                               env={**os.environ, 'TERM': 'xterm-256color', 'LC_ALL': 'C.UTF-8',
                                    'HOME': '/nonexistent', 'XDG_CONFIG_HOME': '/nonexistent'},
                               **identity(account))
    data = bytearray()
    try:
        read_pty(master, data, 4)
        assert monitor.poll() is None, data[-3000:]
        assert b'Focus' in data, 'UI did not render during hard NFS outage'
        # Switch away from the blocked home scanner; F1 must still respond.
        os.write(master, b'\x1bOP')
        before_help = len(data)
        read_pty(master, data, 0.8)
        assert len(data) > before_help and b'Help' in data[before_help:], 'F1 blocked'
        os.write(master, b'\x1b')
        read_pty(master, data, 0.3)
        os.write(master, b'q')
        monitor.wait(timeout=3)
        assert monitor.returncode == 0
        assert termios.tcgetattr(slave) == before, 'terminal not restored'
        (evidence / f'nfs-pty-{helper}.ansi').write_bytes(data)
        print(f'PASS hard NFS outage helper={helper}: home UI draws, F1 responds, q exits', flush=True)
    finally:
        if monitor.poll() is None:
            monitor.kill()
            monitor.wait(timeout=3)
        os.close(master)
        os.close(slave)


def nfs(binary, account, evidence):
    base = Path(tempfile.mkdtemp(prefix='dwtop-kernel-', dir='/var/tmp'))
    base.chmod(0o755)
    source = base / 'export'
    source.mkdir()
    target = Path(tempfile.mkdtemp(prefix='dwtop-nfs-', dir='/home'))
    target.chmod(0o755)
    (source / 'home').mkdir()
    (source / 'home' / 'payload').write_bytes(b'x' * 4096)
    EXPORT.write_text(f'{source} 127.0.0.1(rw,sync,no_subtree_check,no_root_squash)\n')
    mounted = dropped = False
    blocker = None
    try:
        run('systemctl', 'start', 'nfs-server')
        run('exportfs', '-ra')
        run('mount', '-t', 'nfs', '-o', 'vers=4.2,proto=tcp,hard,timeo=10,retrans=1,actimeo=0',
            f'127.0.0.1:{source}', str(target))
        mounted = True
        root = target / 'home'
        assert (root / 'payload').stat().st_size == 4096
        options = run('findmnt', '-no', 'OPTIONS', str(target)).stdout
        assert 'hard' in options and 'soft' not in options, options
        (evidence / 'nfs-mount-options.txt').write_text(options)
        # Dedicated guest-only table; never flush any existing firewall rules.
        run('nft', 'add', 'table', 'inet', TABLE)
        dropped = True
        run('nft', 'add', 'chain', 'inet', TABLE, 'output',
            '{ type filter hook output priority -10; policy accept; }')
        run('nft', 'add', 'rule', 'inet', TABLE, 'output', 'ip', 'daddr', '127.0.0.1',
            'tcp', 'dport', '2049', 'drop')
        blocker = subprocess.Popen(['stat', str(root / 'payload')], stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE)
        time.sleep(1.5)
        assert blocker.poll() is None, 'NFS metadata lookup did not block'
        for helper in (False, True):
            before, _ = snapshot(binary, account, '--home-path', str(root),
                                 *(() if helper else ('--no-helper',)))
            local = base / 'local-io'
            with local.open('wb') as stream:
                stream.write(os.urandom(8 * 1024 * 1024))
                stream.flush()
                os.fsync(stream.fileno())
            after, elapsed = snapshot(binary, account, '--home-path', str(root),
                                      *(() if helper else ('--no-helper',)))
            old = {disk['name']: disk for disk in before['devices']}
            written = sum(disk['write_bytes'] - old[disk['name']]['write_bytes']
                          for disk in after['devices'] if disk['name'] in old and not disk['partition'])
            assert written >= 8 * 1024 * 1024, written
            assert all(fs['fs_type'] not in ('nfs', 'nfs4') for fs in after['filesystems'])
            (evidence / f'nfs-outage-{helper}.json').write_text(json.dumps(after, indent=2))
            print(f'PASS hard NFS outage helper={helper}: sampling {elapsed:.3f}s, '
                  f'local disk writes +{written} bytes; NFS intentionally filtered', flush=True)
            interactive(binary, account, root, helper, evidence)
        run('nft', 'delete', 'table', 'inet', TABLE)
        dropped = False
        blocker.communicate(timeout=20)
        assert blocker.returncode == 0, 'NFS lookup did not recover'
        assert (root / 'payload').read_bytes() == b'x' * 4096
        for helper in (False, True):
            data, elapsed = snapshot(binary, account, '--home-path', str(root), '--scan-home',
                                    *(() if helper else ('--no-helper',)))
            assert data['home_tree'] is not None, data['home']
            assert data['home_tree']['privileged'] == helper, 'unexpected scanner fallback'
            print(f'PASS NFS restored helper={helper}: metadata and home scan recovered in {elapsed:.3f}s', flush=True)
    finally:
        if dropped:
            run('nft', 'delete', 'table', 'inet', TABLE)
        if blocker is not None and blocker.poll() is None:
            blocker.kill()
            blocker.communicate(timeout=10)
        if mounted:
            run('umount', str(target))
        EXPORT.unlink(missing_ok=True)
        run('exportfs', '-ra')
        shutil.rmtree(base)
        target.rmdir()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--isolated-vm', action='store_true')
    parser.add_argument('--only', choices=('hidepid', 'nfs', 'all'), default='all')
    parser.add_argument('--evidence', type=Path, required=True)
    parser.add_argument('binary', type=Path)
    parser.add_argument('collector', type=Path)
    args = parser.parse_args()
    virt = run('systemd-detect-virt', '--vm').stdout.strip()
    if not args.isolated_vm or os.geteuid() != 0 or virt not in ('qemu', 'kvm'):
        parser.error('requires root, --isolated-vm and a disposable QEMU/KVM VM')
    assert not HELPER.exists() and not RECEIPT.exists(), 'refusing existing helper installation'
    assert not EXPORT.exists(), 'refusing existing export configuration'
    assert not Path('/etc/dwtop/scan-roots').exists(), 'requires default helper scan policy'
    assert not Path('/etc/spctop/scan-roots').exists(), 'requires absent intermediate helper scan policy'
    assert not Path('/etc/disktop/scan-roots').exists(), 'requires absent legacy helper scan policy'
    assert os.readlink('/proc/self/ns/mnt') != os.readlink('/proc/1/ns/mnt'), (
        'run with unshare --mount --propagation private')
    if subprocess.run(['nft', 'list', 'table', 'inet', TABLE], capture_output=True).returncode == 0:
        raise AssertionError('refusing existing nft test table')
    for name in ('dwtop-test-owner', 'dwtop-test-peer'):
        try:
            pwd.getpwnam(name)
        except KeyError:
            pass
        else:
            raise AssertionError(f'refusing existing account {name}')
    binary, collector = args.binary.resolve(strict=True), args.collector.resolve(strict=True)
    args.evidence.mkdir(parents=True, exist_ok=True)
    (args.evidence / 'environment.txt').write_text(run('uname', '-a').stdout +
        run('cat', '/etc/os-release').stdout + run('mount').stdout)
    created = []
    try:
        for name in ('dwtop-test-owner', 'dwtop-test-peer'):
            run('useradd', '--no-create-home', '--user-group', name)
            created.append(name)
        account, peer = (pwd.getpwnam(name) for name in created)
        HELPER.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(collector, HELPER)
        os.chown(HELPER, 0, account.pw_gid)
        HELPER.chmod(0o750)
        run('setcap', 'cap_dac_read_search,cap_sys_ptrace=ep', str(HELPER))
        RECEIPT.write_text(hashlib.sha256(HELPER.read_bytes()).hexdigest() + '\n')
        RECEIPT.chmod(0o644)
        if args.only in ('all', 'hidepid'):
            hidepid(binary, account, peer, args.evidence)
        if args.only in ('all', 'nfs'):
            nfs(binary, account, args.evidence)
    finally:
        HELPER.unlink(missing_ok=True)
        RECEIPT.unlink(missing_ok=True)
        for name in reversed(created):
            run('userdel', name)
    print('All requested real kernel checks passed; mounts, accounts, helper and nft table cleaned.', flush=True)


if __name__ == '__main__':
    main()
