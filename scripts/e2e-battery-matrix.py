#!/usr/bin/env python3
# Regenerates MATRIX.md (target x iteration) from the battery results.tsv.
# See docs/RUNNING-E2E.md, "Full battery".
#
# An iteration id is an integer, optionally prefixed by a letter naming the
# PHASE the run belongs to. All three phases are kept in one matrix: deleting the
# old columns would hide that a target used to be red, and relabelling them as
# current would claim evidence the current harness never produced.
#
#   pN  pre-hardening   the previous session's driver, before any fix below
#   hN  harness proof   out-of-band runs that PROVED a harness fix, pre-battery
#   N   battery         an iteration of the four-iteration battery
#
# Phases sort in that order, so the matrix reads left to right as the run
# actually happened.
"""Regenerate MATRIX.md (target x iteration) from results.tsv."""
import sys, collections

PHASES = {'p': (0, 'pre'), 'h': (1, 'harness')}

def parse_iter(s):
    """-> (phase_rank, number). 'p3' < 'h1' < '2'."""
    s = s.strip()
    if s[:1] in PHASES:
        return (PHASES[s[0]][0], int(s[1:]))
    return (2, int(s))

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
RANK_LABEL = {0: 'pre', 1: 'harness', 2: 'iter'}
def header(k):
    rank, n = k
    return f"{RANK_LABEL[rank]} {n}"
def cell(v):
    if not v: return '—'
    st,secs,log,rerun=v
    m={'PASS':'PASS','FAIL':'**FAIL**','FLAKY/FIXED':'**FLAKY**'}.get(st,st)
    s=f"{m} {int(secs)//60}m"
    if rerun: s+=" (re-run)"
    return s
print("# #167 e2e battery — results matrix\n")
print("Target × iteration. Duration in minutes. Re-run = target was executed again after a fix or retry.\n")
if any(k[0] == 0 for k in its):
    print("`pre N` columns are **pre-hardening**: iteration N of the ORIGINAL driver, kept as "
          "history. They predate the quiesce rewrite, the writer queue-depth fix and the "
          "bridge-out-tool preflight, so their reds are not evidence about the current "
          "harness — and their greens are not evidence about it either.\n")
if any(k[0] == 1 for k in its):
    print("`harness N` columns are the out-of-band runs that PROVED a harness fix before the "
          "battery was committed to: three back-to-back full-DB-loss drills on fresh live "
          "fixtures, and one complete load+chaos tail. They are real results on the current "
          "code, kept separate so they are never mistaken for battery iterations.\n")
print("| Target | " + " | ".join(header(i) for i in its) + " |")
print("|---|" + "---|"*len(its))
for tgt,per in res.items():
    print(f"| `{tgt}` | " + " | ".join(cell(per.get(i)) for i in its) + " |")
def totals(rank):
    return collections.Counter(v[0] for per in res.values() for k,v in per.items() if k[0]==rank)
bat, harn, pre = totals(2), totals(1), totals(0)
print("\n**Totals (battery):** " + (", ".join(f"{k}={v}" for k,v in sorted(bat.items())) or "no runs yet"))
if harn:
    print("\n**Totals (harness proof):** " + ", ".join(f"{k}={v}" for k,v in sorted(harn.items())))
if pre:
    print("\n**Totals (pre-hardening, history only):** " + ", ".join(f"{k}={v}" for k,v in sorted(pre.items())))
