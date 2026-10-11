# Real ZFS membership and accounting validation

`zfs.py` is a read-only regression check for an **already provisioned disposable
pool**, with at least two mounted datasets and members on otherwise unused disks.
It never creates, imports, modifies or destroys pools. Do not point it at disks
that contain other filesystems: its per-disk accounting assertions intentionally
expect only the fixture pool.

Use an isolated VM with OpenZFS tools/kernel support and two newly created virtual
disks. Verify their identity in the VM before provisioning; **never run these
provisioning commands on the host or substitute an existing data disk**. The
2026-10-10 validation used an Ubuntu 24.04 KVM guest, OS disk `/dev/vda`, and two
separate 2 GiB qcow2 data disks exposed as `/dev/vdb` and `/dev/vdc`:

```sh
sudo zpool create -m /home/tester/zfs-work/pool dwtoptest mirror /dev/vdb /dev/vdc
sudo zfs create dwtoptest/child
sudo dd if=/dev/urandom of=/home/tester/zfs-work/pool/child/data bs=1M count=8 status=none
sudo zfs set quota=32M dwtoptest/child
sudo zpool sync dwtoptest
python3 tests/zfs.py --binary ./target/release/dwtop --pool dwtoptest --members vdb1 vdc1
sudo python3 tests/zfs.py --binary ./target/release/dwtop --pool dwtoptest --members vdb1 vdc1
```

Both ordinary `dwtoptest` and `dwtoptest/child` mount sources must name
`vdb1,vdc1` as members. Both whole disks must list the mount points and account
for **one** pool, with identical physical pool size/allocated/free. Dataset
statvfs numbers remain separate (the quota makes their capacities different).
UI aggregate deduplication is covered by the `zfs_summary_counts_one_pool_across_datasets_and_disks`
unit test.

JSON v1 keeps `usage.block_devices` as actual block device names. The additive
`usage.accounting_keys` field carries the shared `zfs:<pool>` identity; older
helper replies without that field retain the block-device fallback.

For a real unavailable-query check, replace `zpool` only inside a private mount
namespace in that disposable VM:

```sh
sudo unshare --mount --propagation private sh -c \
  'mount --bind /bin/false /usr/sbin/zpool && ./target/release/dwtop --json --no-helper'
```

The ZFS rows must retain dataset statvfs values, have `zfs.error`, and leave pool
allocation unknown. No fabricated membership or dataset-derived pool capacity is
allowed. After an earlier successful query, cached membership remains visible
while capacity is unmeasured. Unit tests additionally verify a 30 ms subprocess
timeout, bounded output, cleared environment, empty child capability sets and
`NoNewPrivs=1`.

Cleanup only the fixture created in the disposable VM:

```sh
sudo zpool destroy dwtoptest
```

## Sources and interpretation

[OpenZFS zpool-list(8)](https://openzfs.github.io/openzfs-docs/man/v2.2/8/zpool-list.8.html)
documents scripted `-H`, exact `-p`, resolved `-L`, full-path `-P` and verbose
`-v` output. The parser follows the
[OpenZFS 2.2 print_list_stats implementation](https://github.com/openzfs/zfs/blob/zfs-2.2.7/cmd/zpool/zpool_main.c):
leaf rows start with a tab, class headers are space-separated, and verbose leaf
columns do not follow the custom pool column selection. Unused spares do not
back a mounted pool; active spare members occur in its normal vdev tree. Logs,
special/dedup and cache devices participate in the pool and are attributed.

Pool bars use physical `allocated`/`free`, not dataset `statvfs` and not an
estimate of application-usable space after RAID-Z overhead, quotas or reserved
space. Discovery requires a root-owned, non-writable installed `zpool` and a
readable `/dev/zfs`; missing tools/access, unsupported output, timeout and
unresolved/file-backed vdevs are explicitly reported. File-backed pools are not
attributed to an invented block device.
