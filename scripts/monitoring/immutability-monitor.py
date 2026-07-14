import json,sys,time,urllib.request,hashlib,signal
RPC="http://127.0.0.1:8546"
TOPICS={"B2AGG":"0x501781209a1f8899323b96b4ef08b168df93e0a90c673d1e4cce39366cb62f9b",
        "CLAIM":"0x1df3f2a973a00d6635911755c260704e95e8a5876997546798770f76396fda4d",
        "GER":"0x65d3bf36615f1f02a134d12dfa9ea6b1d4a52386e825973cd27ddb70895c2319"}
def rpc(m,p):
    r=urllib.request.Request(RPC,json.dumps({"jsonrpc":"2.0","id":1,"method":m,"params":p}).encode(),{"Content-Type":"application/json"})
    return json.load(urllib.request.urlopen(r,timeout=30))["result"]
def block_logs():
    per={}
    for name,t in TOPICS.items():
        for l in rpc("eth_getLogs",[{"fromBlock":"0x0","toBlock":"latest","topics":[t]}]):
            b=int(l["blockNumber"],16); per.setdefault(b,[]).append(f"{name}:{int(l['logIndex'],16)}:{l['data']}")
    return {b:hashlib.sha256("|".join(sorted(v)).encode()).hexdigest()[:12] for b,v in per.items()}
dur=int(sys.argv[1]) if len(sys.argv)>1 else 3600
seen={}; viol=0; polls=0; resets=0; t_end=time.time()+dur; maxb=0
def _summary():
    print(f"════ IMMUTABILITY: polls={polls} blocks_tracked={len(seen)} resets={resets} VIOLATIONS={viol} (0=immutable) ════",flush=True)
def _on_term(_sig,_frm):
    # Graceful stop (release-acceptance sends SIGTERM): flush the tallied summary so
    # a supervisor has POSITIVE evidence (polls>0) that this monitor actually ran.
    _summary(); sys.exit(0)
signal.signal(signal.SIGTERM, _on_term)
print(f"immutability monitor: track every block's logs; flag change after first-seen; reset baseline on chain regression. dur={dur}s",flush=True)
while time.time()<t_end:
    try: cur=block_logs()
    except Exception: time.sleep(4); continue   # RPC down / bringup — wait, never exit
    polls+=1
    cmax=max(cur) if cur else 0
    if cmax < maxb-2:   # tip regressed => teardown+fresh bringup => new chain; re-baseline
        resets+=1; seen={}; print(f"  [reset {resets}] chain regressed {maxb}->{cmax} (fresh bringup); re-baselining",flush=True)
    maxb=max(maxb,cmax)
    for b,h in cur.items():
        if b in seen and seen[b]!=h:
            viol+=1; print(f"  ★ IMMUTABILITY VIOLATION: block {b} logs CHANGED {seen[b]}->{h} (poll {polls})",flush=True); seen[b]=h
        elif b not in seen: seen[b]=h
    time.sleep(5)
_summary()
