#!/usr/bin/env python3
"""Read-only real-pool validation; provisioning is explicitly out of scope.

Run only against an already provisioned disposable pool; see tests/zfs.md.
"""
import argparse
import json
import subprocess


def run(*args):
    return subprocess.check_output(args, text=True, timeout=20)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--pool", required=True)
    parser.add_argument("--members", required=True, nargs="+")
    args = parser.parse_args()
    reference = run("zpool", "list", "-H", "-p", "-o", "name,size,allocated,free", args.pool).strip().split("\t")
    snapshot = json.loads(run(args.binary, "--json", "--no-helper", "--interval", "1"))
    rows = [row for row in snapshot["filesystems"] if row["fs_type"] == "zfs" and
            (row["source"] == args.pool or row["source"].startswith(args.pool + "/"))]
    assert len(rows) >= 2, "need a root dataset and a child dataset mounted"
    assert any(row["source"] == args.pool for row in rows), "root pool source missing"
    expected = sorted(args.members)
    pool = rows[0]["zfs"]
    assert pool["name"] == args.pool and pool["error"] is None, pool
    assert pool["total_bytes"] == int(reference[1]), (pool, reference)
    assert pool["used_bytes"] + pool["available_bytes"] == pool["total_bytes"], pool
    # Pool metadata can change between zpool and the monitor's sample. Each
    # snapshot must be internally consistent, not equal to an earlier instant.
    for row in rows:
        assert row["member_devices"] == expected, row
        assert row["zfs"] == pool, row
        assert row["read_rate"] is None and row["write_rate"] is None, row
    devices = {device["name"]: device for device in snapshot["devices"]}
    disks = {devices[name]["parent"] or name for name in expected}
    for name in disks:
        device = devices[name]
        usage = device["usage"]
        assert usage["accounting_keys"] == ["zfs:" + args.pool], usage
        assert usage["block_devices"] == [expected[0]], usage
        assert usage["filesystems"] == 1 and usage["unmeasured"] == 0, usage
        for key in ("total_bytes", "used_bytes", "available_bytes"):
            assert usage[key] == pool[key], (key, usage, pool)
        assert all(row["mount_point"] in device["mounts"] for row in rows), device
    print(f"PASS: {args.pool}: {len(rows)} datasets, {len(disks)} disks, one pool allocation per disk")


if __name__ == "__main__":
    main()
