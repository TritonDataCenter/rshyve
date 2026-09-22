#!/usr/sbin/dtrace -qs
/*
 * vmexit.d: VM exit accounting for a VMM run.
 *
 * Usage: dtrace -s vmexit.d -p <pid>
 *    or: dtrace -s vmexit.d -c './firehyve <argv>'
 *
 * Prints:
 *   - total VM exits
 *   - exits per vCPU
 *   - host CPU time spent in the VMM process (user + sys)
 *   - off-CPU (blocked/sleeping) time for VMM threads
 */

BEGIN
{
    start = timestamp;
    printf("tracing (ctrl-c to finish)\n");
}

sdt:vmm::vmm-vexit
{
    @exits_total = count();
    @exits_by_cpu[arg1] = count();
}

profile:::profile-997
/pid == $target/
{
    @on_cpu_samples[probename] = count();
}

sched:::off-cpu
/pid == $target/
{
    self->off = timestamp;
}
sched:::on-cpu
/pid == $target && self->off/
{
    @off_cpu_ns = sum(timestamp - self->off);
    self->off = 0;
}

END
{
    elapsed_ms = (timestamp - start) / 1000000;
    printf("\n=== elapsed wall: %d ms ===\n", elapsed_ms);
    printf("\n--- VM exits ---\n");
    printa(@exits_total);
    printf("\n--- exits by vCPU ---\n");
    printa(@exits_by_cpu);
    printf("\n--- on-CPU samples (997Hz, per thread-context) ---\n");
    printa(@on_cpu_samples);
    printf("\n--- off-CPU time (ns) ---\n");
    printa(@off_cpu_ns);
}
