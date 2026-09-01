"""Count non-comment, non-test lines a commit range touches under
libs/compio-postgres/src.

    production_lines_changed.py <base> <head>

TWO POSITIONAL REVISIONS. No flags. This refuses anything else on purpose:
it was called as `--range a~1..a --repo <dir>` for several commits, which bound
base="--range" and head="a~1..a", and `git diff --name-only "--range..a~1..a"`
names no files - so it printed "TOTAL production lines: 0" for every one of
them. Zero is the answer that means "this commit is test-only", so a broken
invocation produced exactly the reassuring result the caller was looking for.
Validating argv is cheaper than noticing that.
"""
import subprocess, re, sys

_args = sys.argv[1:]
if len(_args) != 2 or any(a.startswith("-") for a in _args):
    sys.exit(f"usage: {sys.argv[0]} <base> <head>   (two revisions, no flags)")
for _rev in _args:
    if subprocess.run(
        ["git", "rev-parse", "--verify", "--quiet", f"{_rev}^{{commit}}"],
        capture_output=True,
    ).returncode:
        sys.exit(f"not a commit: {_rev}")
base, head = _args
files=subprocess.run(["git","diff","--name-only",f"{base}..{head}","--","libs/compio-postgres/src"],
                     capture_output=True,text=True).stdout.split()
def test_spans(src):
    spans=[]
    for i,l in enumerate(src):
        if l.lstrip().startswith("#[cfg(test)]"):
            depth=0; started=False
            for j in range(i,len(src)):
                depth+=src[j].count("{")-src[j].count("}")
                if "{" in src[j]: started=True
                if started and depth<=0:
                    spans.append((i+1,j+1)); break
            else: spans.append((i+1,len(src)))
    return spans
total=0
for f in files:
    cur=subprocess.run(["git","show",f"{head}:{f}"],capture_output=True,text=True).stdout.split("\n")
    spans=test_spans(cur)
    def in_test(n): return any(a<=n<=b for a,b in spans)
    diff=subprocess.run(["git","diff","-U0",f"{base}..{head}","--",f],capture_output=True,text=True).stdout
    newlines=[]
    for m in re.finditer(r'^@@ -\S+ \+(\d+)(?:,(\d+))? @@', diff, re.M):
        s=int(m.group(1)); n=int(m.group(2) or 1); newlines.extend(range(s,s+n))
    real=[x for x in newlines if not in_test(x) and x-1<len(cur) and cur[x-1].strip()
          and not cur[x-1].strip().startswith(("//","///","//!"))]
    if real: print(f"  {f}: {len(real)}"); total+=len(real)
print(f"TOTAL production lines: {total}")
