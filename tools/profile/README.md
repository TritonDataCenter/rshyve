# Boot-path profiling

DTrace harness that measures the boot cost of a VMM. The scripts attach
to the kernel probe `sdt:vmm::vmm-vexit`, so they need the global zone
and the `dtrace_kernel` privilege. A non-global zone cannot see the
probe, even as root. `run.sh` checks for the probe and stops if it is
missing, so an empty exit count is never read as a fast boot.

    export VMM_PROFILE_BIN=/path/to/target/release/firehyve
    tools/profile/run.sh -d 10 -o /var/tmp/pvh -- \
        -c 1 -m 512M --kernel /var/tmp/vmlinux \
        --initrd /var/tmp/initramfs.cpio \
        --cmdline "console=ttyS0 earlyprintk=serial" \
        -s 4,virtio-rnd -l com1,stdio bench

## Method

To compare the two direct-boot paths, run the command twice with the
same VM (1 vCPU, 512 MiB, virtio-rnd, COM1 on stdio, one initramfs) and
change only `--kernel`: a bzImage, then a PVH ELF. No flag selects the
protocol. The VMM reads it from the file and logs it as `protocol` on
the `kernel validated` line of `vmm.log`, so a run that profiled the
wrong path is visible.

| Number | Source in the output directory |
|---|---|
| VM exits | `profile.out`, the `--- VM exits ---` block |
| Off-CPU time | `profile.out`, the `--- off-CPU time (ns) ---` block |
| Top user stack | `top.txt`, first line |
| Guest boot time | `guest.log`, the kernel's own `[ nn.nnnnnn]` stamps |

Compare a result only with a later run on the same host.

## Scripts

- `profile.d`: VM-exit counts, 997 Hz user and kernel stacks, off-CPU time
- `vmexit.d`: VM-exit counts only
- `sample.d`: stack sampling only, for flame graphs
- `collapse.awk`: DTrace `%k` output to Brendan Gregg collapsed format
- `top-stacks.awk`: the top N collapsed stacks, with percentages
