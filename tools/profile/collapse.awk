# collapse.awk: convert DTrace ustack()/stack() %k output to collapsed
# "func1;func2;...;funcN count" format (Brendan Gregg flamegraph input).
#
# Usage: awk -f collapse.awk stacks.out > collapsed.out
#
# DTrace %k output format per stack:
#   module`symbol+0xN
#   module`symbol+0xN
#   ...
#   <space><count>
#   <blank line>

BEGIN { n = 0 }

# Emit the current stack on blank lines.
/^[[:space:]]*$/ {
    if (n > 1 && cnt > 0) {
        out = stack[n - 1]
        for (i = n - 2; i >= 0; i--) out = out ";" stack[i]
        print out, cnt
    }
    n = 0
    cnt = 0
    next
}

# Count line: just a number (with optional leading/trailing space).
/^[[:space:]]*[0-9]+[[:space:]]*$/ {
    cnt = $1 + 0
    next
}

# Frame line: anything else with content.
{
    line = $0
    sub(/^[[:space:]]+/, "", line)
    sub(/[[:space:]]+$/, "", line)
    if (line == "") next
    sub(/\+0x[0-9a-fA-F]+$/, "", line)
    stack[n++] = line
}

END {
    if (n > 1 && cnt > 0) {
        out = stack[n - 1]
        for (i = n - 2; i >= 0; i--) out = out ";" stack[i]
        print out, cnt
    }
}
