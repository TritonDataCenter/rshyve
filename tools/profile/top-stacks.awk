# top-stacks.awk: show top-N collapsed stacks by sample count.
# Usage: awk -f top-stacks.awk -v N=20 collapsed.out

BEGIN { if (!N) N = 20 }

{
    # Last field is the count; the rest (joined with spaces) is the stack.
    count = $NF
    $NF = ""
    stack = $0
    sub(/[[:space:]]+$/, "", stack)
    total += count
    samples[stack] = count
}

END {
    # Sort descending by count (manual O(n^2), fine for ~hundreds of stacks).
    n = 0
    for (s in samples) { keys[n++] = s }
    for (i = 0; i < n; i++) {
        for (j = i + 1; j < n; j++) {
            if (samples[keys[j]] > samples[keys[i]]) {
                t = keys[i]; keys[i] = keys[j]; keys[j] = t
            }
        }
    }
    printf "# total samples: %d\n", total
    printf "# top %d stacks\n\n", N
    limit = (n < N) ? n : N
    for (i = 0; i < limit; i++) {
        c = samples[keys[i]]
        pct = (c * 100.0) / total
        printf "%8d %6.2f%%  %s\n", c, pct, keys[i]
    }
}
