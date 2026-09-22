#!/usr/sbin/dtrace -qs
/*
 * profile.d: combined VM-exit + stack-sample profile.
 *
 * Usage: dtrace -s profile.d -p <pid>
 *    or: dtrace -s profile.d -c './firehyve <argv>'
 */

BEGIN
{
    start = timestamp;
}

/* VM-exit accounting (kernel side) */
sdt:vmm::vmm-vexit
{
    @exits_total = count();
    @exits_by_cpu[arg1] = count();
}

/* On-CPU samples by stack at 997Hz, user and kernel separately */
profile-997
/pid == $target && arg1 != 0/
{
    @ustacks[ustack(40)] = count();
    @total_user = count();
}
profile-997
/pid == $target && arg0 != 0/
{
    @kstacks[stack(40)] = count();
    @total_kern = count();
}

/* Off-CPU accounting for VMM threads */
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
    printf("\n--- on-CPU samples (user vs kernel ctx) ---\n");
    printa(@total_user);
    printa(@total_kern);
    printf("\n--- off-CPU time (ns) ---\n");
    printa(@off_cpu_ns);
    printf("\n# USER STACKS BEGIN\n");
    printa("%k %@d\n", @ustacks);
    printf("# USER STACKS END\n");
    printf("\n# KERN STACKS BEGIN\n");
    printa("%k %@d\n", @kstacks);
    printf("# KERN STACKS END\n");
}
