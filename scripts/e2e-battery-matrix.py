#!/usr/bin/env python3
# Regenerates MATRIX.md (target x iteration) from the battery results.tsv.
# See docs/RUNNING-E2E.md, "Full battery".
#
# Iteration ids are either an integer (a run against the CURRENT harness) or an
# integer prefixed with `p` (a run against a SUPERSEDED harness — "p" for
# pre-hardening). Both are kept: deleting the old columns would hide that a
# target used to be red, and relabelling them as current would claim evidence
# the current harness never produced. Pre-hardening columns sort first and are
# marked in the header.
"""Regenerate MATRIX.md (target x iteration) from results.tsv."""
import sys, collections

def parse_iter(s):
    """-> (is_current, number). Pre-hardening ids ('p3') sort before current."""
    s = s.strip()
    return (0, int(s[1:])) if s.startswith('p') else (1, int(s))

rows=[l.rstrip('\n').split('\t') for l in open(sys.argv[1])][1:]
res=collections.OrderedDict(); iters=set()
for it,tgt,st,secs,log in rows:
    key_it = parse_iter(it)
    iters.add(key_it)
    # later row for the same (target,iter) is a RE-RUN: keep it, note the retry
    res.setdefault(tgt, {})
    prev=res[tgt].get(key_it)
    if prev and prev[0]=='FAIL' and st=='PASS':
        res[tgt][key_it]=('FLAKY/FIXED', secs, log, True)
    elif prev and prev[0]=='PASS' and st=='PASS':
        res[tgt][key_it]=('PASS', secs, log, True)
    else:
        res[tgt][key_it]=(st, secs, log, False)
its=sorted(iters) or [(1,1)]
def header(k):
    cur,n = k
    return f"iter {n}" if cur else f"pre {n}"
def cell(v):
    if not v: return '—'
    st,secs,log,rerun=v
    m={'PASS':'PASS','FAIL':'**FAIL**','FLAKY/FIXED':'**FLAKY**'}.get(st,st)
    s=f"{m} {int(secs)//60}m"
    if rerun: s+=" (re-run)"
    return s
print("# #167 e2e battery — results matrix\n")
print("Target × iteration. Duration in minutes. Re-run = target was executed again after a fix or retry.\n")
if any(not k[0] for k in its):
    print("`pre N` columns are **pre-hardening**: iteration N of the ORIGINAL driver, kept as "
          "history. They predate the quiesce rewrite, the writer queue-depth fix and the "
          "bridge-out-tool preflight, so their reds are not evidence about the current "
          "harness — and their greens are not evidence about it either.\n")
print("| Target | " + " | ".join(header(i) for i in its) + " |")
print("|---|" + "---|"*len(its))
for tgt,per in res.items():
    print(f"| `{tgt}` | " + " | ".join(cell(per.get(i)) for i in its) + " |")
cur_tot=collections.Counter(v[0] for per in res.values() for k,v in per.items() if k[0])
pre_tot=collections.Counter(v[0] for per in res.values() for k,v in per.items() if not k[0])
print("\n**Totals (current harness):** " + (", ".join(f"{k}={v}" for k,v in sorted(cur_tot.items())) or "no runs yet"))
if pre_tot:
    print("\n**Totals (pre-hardening, history only):** " + ", ".join(f"{k}={v}" for k,v in sorted(pre_tot.items())))
