#!/usr/sbin/dtrace -qs
/*
 * sample.d: user-stack sampling profiler for the VMM.
 *
 * Usage: dtrace -s sample.d -p <pid> -o stacks.out
 *    or: dtrace -s sample.d -c './firehyve <argv>' -o stacks.out
 *
 * Samples user stacks at 997Hz. Pair with collapse.awk and the FlameGraph
 * toolchain to render a flamegraph.
 */

profile-997
/pid == $target && arg1/
{
    @[ustack(40)] = count();
}

profile-997
/pid == $target && arg0/
{
    @kstacks[stack(40)] = count();
}

END
{
    printf("\n# user stacks\n");
    printa("%k %@d\n", @);
    printf("\n# kernel stacks (VMM threads only)\n");
    printa("%k %@d\n", @kstacks);
}
