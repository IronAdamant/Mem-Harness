import json, sys, glob, os, collections
S=sys.argv[1]
rows=[]
for f in sorted(glob.glob(f"{S}/out-*.jsonl")):
    name=os.path.basename(f)[4:-6]
    init=None; final=None; calls=collections.Counter(); errors=[]; first_usage=None; ids={}
    for line in open(f):
        try: m=json.loads(line)
        except: continue
        t=m.get("type")
        if t=="system" and m.get("subtype")=="init": init=m
        elif t=="assistant":
            msg=m["message"]
            if first_usage is None: first_usage=msg.get("usage")
            for c in msg.get("content",[]):
                if c.get("type")=="tool_use":
                    calls[c["name"]]+=1; ids[c["id"]]=c["name"]
        elif t=="user":
            for c in (m["message"].get("content") or []):
                if isinstance(c,dict) and c.get("type")=="tool_result" and c.get("is_error"):
                    txt=c.get("content"); txt=txt if isinstance(txt,str) else json.dumps(txt)
                    errors.append((ids.get(c["tool_use_id"],"?"), txt[:220]))
        elif t=="result": final=m
    print(f"=== {name}")
    if init:
        mt=[x for x in init.get("tools",[]) if x.startswith("mcp__")]
        print(" servers:", [(s["name"],s["status"]) for s in init.get("mcp_servers",[])])
        print(" mcp tools offered:", len(mt))
    if first_usage: print(" first turn usage:", {k:v for k,v in first_usage.items() if isinstance(v,int)})
    if final:
        u=final.get("usage",{})
        print(f" cost ${final.get('total_cost_usd',0):.4f} turns {final.get('num_turns')} time {final.get('duration_ms',0)/1000:.0f}s subtype {final.get('subtype')}")
        print(" usage:", {k:v for k,v in u.items() if isinstance(v,int)})
        print(" answer:", (final.get("result") or "")[:700].replace("\n"," | "))
    print(" tool calls:", sum(calls.values()), dict(calls))
    for n,e in errors: print("  ERR", n, "::", e.replace("\n"," "))
