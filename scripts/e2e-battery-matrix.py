#!/usr/bin/env python3
# Regenerates MATRIX.md (target x iteration) from the battery results.tsv.
# See docs/RUNNING-E2E.md, "Full battery".
"""Regenerate MATRIX.md (target x iteration) from results.tsv."""
import sys, collections
rows=[l.rstrip('\n').split('\t') for l in open(sys.argv[1])][1:]
res=collections.OrderedDict(); iters=set()
for it,tgt,st,secs,log in rows:
    iters.add(int(it))
    # later row for the same (target,iter) is a RE-RUN: keep it, note the retry
    key=tgt
    res.setdefault(key, {})
    prev=res[key].get(int(it))
    if prev and prev[0]=='FAIL' and st=='PASS':
        res[key][int(it)]=('FLAKY/FIXED', secs, log, True)
    elif prev and prev[0]=='PASS' and st=='PASS':
        res[key][int(it)]=('PASS', secs, log, True)
    else:
        res[key][int(it)]=(st, secs, log, False)
its=sorted(iters) or [1]
def cell(v):
    if not v: return '—'
    st,secs,log,rerun=v
    m={'PASS':'PASS','FAIL':'**FAIL**','FLAKY/FIXED':'**FLAKY**'}.get(st,st)
    s=f"{m} {int(secs)//60}m"
    if rerun: s+=" (re-run)"
    return s
print("# #167 e2e battery — results matrix\n")
print("Target × iteration. Duration in minutes. Re-run = target was executed again after a fix or retry.\n")
print("| Target | " + " | ".join(f"iter {i}" for i in its) + " |")
print("|---|" + "---|"*len(its))
for tgt,per in res.items():
    print(f"| `{tgt}` | " + " | ".join(cell(per.get(i)) for i in its) + " |")
tot=collections.Counter(v[0] for per in res.values() for v in per.values())
print(f"\n**Totals:** " + ", ".join(f"{k}={v}" for k,v in sorted(tot.items())))
