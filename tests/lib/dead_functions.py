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

A coverage JSON records LINE NUMBERS. Resolving spans from the working tree
therefore silently misreads the moment the tree moves off the commit the profile
was taken at - a commit that deletes 26 lines from client.rs shifts every span
below it, and the tool reports confident nonsense rather than failing. Pass
`--commit <sha>` (the sha the coverage run was stamped with) and source is read
from git at that sha instead of from disk, which removes the failure rather than
asking the reader to remember it.

Usage: dead_functions.py <coverage.json> [--commit <sha>] [src-file ...]
"""
import json, re, sys, os, subprocess

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

def read_source(rel, commit):
    """Source as it stood when the profile was taken."""
    if not commit:
        if not os.path.exists(rel):
            return None
        return open(rel).read().split("\n")
    out = subprocess.run(
        ["git", "show", f"{commit}:{rel}"], capture_output=True, text=True
    )
    if out.returncode != 0:
        return None
    return out.stdout.split("\n")


def main():
    argv = sys.argv[1:]
    commit = None
    if "--commit" in argv:
        i = argv.index("--commit")
        commit = argv[i + 1]
        del argv[i : i + 2]
    data = json.load(open(argv[0]))
    wanted = argv[1:]
    files = {}
    for exp in data["data"]:
        for f in exp["files"]:
            files[f["filename"]] = [s for s in f.get("segments", []) if s[3]]
    total = 0
    nodata = []
    for name, segs in sorted(files.items()):
        if "/libs/compio-postgres/src/" not in name:
            continue
        rel = "libs/compio-postgres/src/" + name.split("/src/")[-1]
        if wanted and not any(w in rel for w in wanted):
            continue
        src = read_source(rel, commit)
        if src is None:
            continue
        ts = test_spans(src)
        for fname, a, b in fn_spans(src, ts):
            inside = [s for s in segs if a <= s[0] <= b]
            if not inside:
                # NOT the same as "covered". A span the JSON carries no region
                # for is a span this run cannot speak about - the target may
                # never have been built, or the source may have moved since the
                # profile was taken. Reporting it as absent would let missing
                # data read as a pass, which is the failure mode this whole
                # tool exists to remove. Count it and say so.
                nodata.append(f"{rel.split('/')[-1]}::{fname}")
                continue
            if all(s[2] == 0 for s in inside):
                print(f"  {rel.split('/')[-1]:<20} {fname:<44} lines {a}-{b}  regions={len(inside)}")
                total += 1
    print(f"functions with zero executed regions: {total}")
    print(f"functions with NO coverage data (unjudged, not covered): {len(nodata)}")
    for n in nodata:
        print(f"  unjudged: {n}")

main()
