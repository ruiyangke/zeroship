"""Uncovered PRODUCTION lines, from llvm-cov's own per-line counts.

`cargo llvm-cov report --show-missing-lines` is the authoritative answer to
"which lines never ran" - reading raw coverage segments per line is not, and
produced a wrong target once. But it reports test scaffolding too: mock sinks
and scripted peers inside `#[cfg(test)]` modules show up as uncovered and are
not gaps worth closing.

This intersects that list with the production spans, using the same
predicate-matching `#[cfg(...test...)]` walk the dead-function tool uses, so
`#[cfg(all(test, unix))]` is not treated as production.

Usage: missing_production_lines.py <show-missing-lines output> [file-substr ...]
"""
import sys, os
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from dead_functions import test_spans

def main():
    if len(sys.argv) < 2:
        sys.exit(f"usage: {sys.argv[0]} <missing-lines-file> [file-substr ...]")
    wanted = sys.argv[2:]
    total_raw = total_prod = 0
    for row in open(sys.argv[1]):
        if ": " not in row:
            continue
        path, nums = row.split(": ", 1)
        rel = "libs/compio-postgres/src/" + path.split("/src/")[-1]
        if wanted and not any(w in rel for w in wanted):
            continue
        if not os.path.exists(rel):
            continue
        lines = [int(n) for n in nums.replace(",", " ").split() if n.isdigit()]
        src = open(rel).read().split("\n")
        spans = test_spans(src)
        prod = [n for n in lines if not any(a <= n <= b for a, b in spans)]
        total_raw += len(lines)
        total_prod += len(prod)
        if prod:
            runs, out = [], []
            for n in sorted(prod):
                if runs and n == runs[-1][1] + 1:
                    runs[-1][1] = n
                else:
                    runs.append([n, n])
            for a, b in runs:
                out.append(str(a) if a == b else f"{a}-{b}")
            print(f"{rel.split('/')[-1]:<20} {len(prod):>4} of {len(lines):>4} uncovered are production: {', '.join(out)}")
    print(f"\nTOTAL uncovered {total_raw}, of which production {total_prod}, test scaffolding {total_raw - total_prod}")

if __name__ == "__main__":
    main()
