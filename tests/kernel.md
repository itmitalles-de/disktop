# Real kernel integration checks

`kernel.py` covers [issue #7](https://github.com/itmitalles-de/dwtop/issues/7)
and [issue #23](https://github.com/itmitalles-de/dwtop/issues/23)
in a **fresh disposable QEMU/KVM VM**. It must never run on the workstation,
on a production guest, or in a container sharing the workstation's kernel.
It requires explicit `--isolated-vm`, root, a private mount namespace and
QEMU/KVM detection. It refuses pre-existing test accounts, helper installation,
helper scan-root configuration, export configuration and its nftables table.

## Reproduce

1. Boot an Ubuntu 24.04 cloud image in a dedicated QEMU process with its own
   qcow2 overlay. Give it 2 vCPUs, 3 GiB RAM and a 12 GiB virtual disk. Use QEMU
   user networking and an available **127.0.0.1-only** SSH forwarded port; no
   host devices, shared host filesystem, host networking or privileged host
   mount operations are needed. Generate a temporary SSH key and a NoCloud
   seed with a temporary sudo-capable test user. Verify the image against the
   Ubuntu image's `SHA256SUMS` before booting it.
2. Install dependencies **inside that guest only**:

   ```sh
   sudo apt-get update
   sudo apt-get install -y nfs-kernel-server nfs-common nftables libcap2-bin python3
   sudo mkdir -p /etc/exports.d /opt/dwtop-kernel
   ```

3. Build both binaries with `cargo build --release` on a compatible Linux
   system and copy them, plus `tests/kernel.py`, into the VM. Install the
   binaries in `/opt/dwtop-kernel/` with mode 0755 so the temporary users can
   execute them (an SSH user's private home directory is unsuitable).
4. Run in the guest:

   ```sh
   sudo unshare --mount --propagation private \
     python3 /path/to/kernel.py --isolated-vm \
     --evidence /var/tmp/dwtop-kernel-evidence \
     /opt/dwtop-kernel/dwtop /opt/dwtop-kernel/dwtop-collector
   ```

   `--only hidepid` or `--only nfs` selects one part. Both parts are required
   for complete validation. Save stdout, stderr and the evidence directory
   outside Git. The test restores hidepid=0, unmounts its NFS export and removes
   its nftables table, export configuration, accounts and helper in `finally`
   blocks. On forced interruption or guest failure, shut down and discard only
   this disposable guest and its writable overlay; never use host-wide cleanup.
5. Copy the evidence out, shut down this VM, verify its QEMU PID exited, and
   remove its temporary SSH private key. Keep only the evidence needed for
   review. Nothing is installed on the host.

## Assertions

- Two real unprivileged accounts exercise `hidepid=1` and `hidepid=2`. The test
  proves kernel enforcement with direct cmdline reads and PID enumeration.
  It then compares direct collection with the real capability-bearing helper.
- Direct `hidepid=1` counts processes whose `stat` read fails with EACCES;
  `hidepid=2` reports that the hidden total is unavailable rather than claiming
  complete visibility or inventing a count. The helper redacts other users'
  command lines in mode 1 and omits their rows in mode 2. A unique synthetic
  command-line marker must not leak into the snapshots.
- One persistent helper PID survives `hidepid=0 -> 1 -> 2`, primary and
  supplementary `gid=` exemptions, exemption removal and return to mode zero.
  Cached private commands must disappear immediately under tighter policy and
  become readable again when relaxed.
- Primary and supplementary `gid=` exemptions lift hidepid for the helper.
  The direct kernel cmdline read also succeeds; direct `/proc/PID/io` remains
  subject to its separate ptrace access check, so other-user I/O rows are
  still unavailable without the helper.
- A real kernel NFSv4.2 server exports a guest-local directory over loopback.
  The client uses `hard,timeo=10,retrans=1,actimeo=0`. A dedicated guest nftables
  table drops only loopback TCP traffic to port 2049. A separate `stat` must
  demonstrably block before application checks start: cached metadata is not
  accepted as an outage test.
- While NFS is unavailable, direct and helper snapshots with a home path on
  the hard mount return in less than five seconds and continue measuring
  fsynced local writes. NFS rows remain intentionally excluded from the
  filesystem table. A home-view PTY renders, handles F1 and exits on `q`,
  restoring terminal settings, for both collection paths.
- Removing only that test table restores the original blocked `stat` and
  file access. Fresh direct/helper home scans must complete; the helper scan
  must report privileged execution, proving it did not silently fall back.

## Verified environment (2026-10-11)

Ubuntu 24.04 cloud image SHA-256
`6a81c37564db9b1ee84e141922625e1d7c5b389b99bb3c572e0243607d5bb4d2`,
guest kernel `6.8.0-142-generic`, 2 vCPUs, 3 GiB RAM, KVM, local ext4 root.
Both hidepid modes, both gid exemption forms and the persistent-helper
transitions passed. During the hard NFS outage, snapshots took approximately
1.31 seconds with direct and helper collection; local writes remained visible
and both PTYs responded. After
restoration, home scans completed in approximately 0.36 seconds. These
are observations from this VM, not universal latency guarantees. This does
not cover every NFS protocol/version, storage failure or procfs policy;
`hidepid=4` remains parser coverage only.

Primary references:
[Linux procfs policy](https://www.kernel.org/doc/html/latest/filesystems/proc.html),
[NFS client mount semantics](https://man7.org/linux/man-pages/man5/nfs.5.html),
[Ubuntu cloud images](https://cloud-images.ubuntu.com/noble/current/).
