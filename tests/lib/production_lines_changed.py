import subprocess, re, sys
base, head = sys.argv[1], sys.argv[2]
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
