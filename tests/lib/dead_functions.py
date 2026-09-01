#!/usr/bin/env python3
"""List production functions with ZERO executed coverage regions.

Why this exists: reading llvm-cov segments line by line and eyeballing which
function a dead run belongs to got the answer wrong three times in one session -
auth refusals, the two-phase-commit decoders, and Pool::query. A segment is a
region BOUNDARY at a (line, column); a zero on a line can belong to a
sub-expression of a covered statement, and a run of zero lines can sit inside a
neighbouring function.

So resolve the enclosing `fn` and aggregate over its whole span. A function is
reported only when EVERY counted region in it is zero, which is the claim worth
acting on: nothing executes it.

Usage: dead_functions.py <coverage.json> [src-file ...]
"""
import json, re, sys, os

CFG_TEST = re.compile(r'#\[cfg\([^\]]*\btest\b[^\]]*\]')
FN = re.compile(r'^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+(\w+)')

def test_spans(src):
    spans = []
    for i, l in enumerate(src):
        if CFG_TEST.search(l):
            depth = 0; started = False
            for j in range(i, len(src)):
                depth += src[j].count("{") - src[j].count("}")
                if "{" in src[j]: started = True
                if started and depth <= 0:
                    spans.append((i + 1, j + 1)); break
            else:
                spans.append((i + 1, len(src)))
    return spans

def fn_spans(src, tspans):
    in_test = lambda n: any(a <= n <= b for a, b in tspans)
    out = []
    for i, l in enumerate(src, 1):
        m = FN.match(l)
        if not m or in_test(i):
            continue
        depth = 0; started = False; end = i
        for j in range(i - 1, len(src)):
            depth += src[j].count("{") - src[j].count("}")
            if "{" in src[j]: started = True
            if started and depth <= 0:
                end = j + 1; break
        out.append((m.group(1), i, end))
    return out

def main():
    data = json.load(open(sys.argv[1]))
    wanted = sys.argv[2:]
    files = {}
    for exp in data["data"]:
        for f in exp["files"]:
            files[f["filename"]] = [s for s in f.get("segments", []) if s[3]]
    total = 0
    for name, segs in sorted(files.items()):
        if "/libs/compio-postgres/src/" not in name:
            continue
        rel = "libs/compio-postgres/src/" + name.split("/src/")[-1]
        if wanted and not any(w in rel for w in wanted):
            continue
        if not os.path.exists(rel):
            continue
        src = open(rel).read().split("\n")
        ts = test_spans(src)
        for fname, a, b in fn_spans(src, ts):
            inside = [s for s in segs if a <= s[0] <= b]
            if not inside:
                continue
            if all(s[2] == 0 for s in inside):
                print(f"  {rel.split('/')[-1]:<20} {fname:<44} lines {a}-{b}  regions={len(inside)}")
                total += 1
    print(f"functions with zero executed regions: {total}")

main()
