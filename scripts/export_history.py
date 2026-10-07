#!/usr/bin/env python3
"""Build the static viewer's history metadata and lazy history shards."""
import argparse, datetime as dt, gzip, json, os, sqlite3
from collections import defaultdict
from pathlib import Path

MAX_DIFFS = 20
MAX_TEXT = 160

def exists(db, t):
    return db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (t,)).fetchone() is not None

def parse(raw):
    try: return json.loads(raw) if raw else {}
    except Exception: return {}

def small(v):
    if isinstance(v, str): return v if len(v) <= MAX_TEXT else v[:MAX_TEXT-1] + "…"
    if isinstance(v, (int, float, bool)) or v is None: return v
    if isinstance(v, list): return v if len(v) <= 4 and all(not isinstance(x,(dict,list)) for x in v) else f"[{len(v)} items]"
    if isinstance(v, dict): return f"{{{len(v)} fields}}"
    return str(v)[:MAX_TEXT]

def flat(v, p="", depth=0):
    if isinstance(v, dict) and depth < 2:
        out = {}
        for k, x in v.items():
            if k in {"fetchedAt","updatedAt","lastUpdatedUtc"}: continue
            key = f"{p}.{k}" if p else k
            if isinstance(x, dict): out.update(flat(x, key, depth+1))
            else: out[key] = small(x)
        return out
    return {p or "value": small(v)}

def diff(a, b):
    a, b = flat(parse(a)), flat(parse(b))
    out = []
    for k in sorted(set(a) | set(b)):
        if a.get(k) != b.get(k):
            out.append({"f":k,"a":a.get(k),"b":b.get(k)})
            if len(out) >= MAX_DIFFS: break
    return out

def gz(path, obj):
    path.parent.mkdir(parents=True, exist_ok=True)
    with gzip.open(path, "wt", encoding="utf-8", compresslevel=6) as f:
        json.dump(obj, f, ensure_ascii=False, separators=(",",":"))

def scalar(db, sql, default=0):
    try:
        r = db.execute(sql).fetchone()
        return r[0] if r and r[0] is not None else default
    except sqlite3.Error:
        return default

def make_meta(db, args):
    counts = {}
    tables = ["labs","adventures","stages","reviews","cracks","adventure_versions","stage_versions","review_versions"]
    for t in tables: counts[t] = scalar(db, f"SELECT COUNT(*) FROM {t}") if exists(db,t) else 0
    for s in ("active","removed","unfetched"):
        counts[f"adventures_{s}"] = scalar(db, f"SELECT COUNT(*) FROM adventures WHERE COALESCE(status,'active')='{s}'") if exists(db,"adventures") else 0
    fresh = {}
    if exists(db,"adventures"):
        fresh["latestAdventureFetch"] = scalar(db, "SELECT MAX(fetched_at) FROM adventures", None)
        fresh["latestReviewsCheck"] = scalar(db, "SELECT MAX(reviews_checked_at) FROM adventures", None)
    if exists(db,"labs"): fresh["latestDiscovery"] = scalar(db, "SELECT MAX(inserted_at) FROM labs", None)
    hs = [scalar(db,f"SELECT MAX(superseded_at) FROM {t}",None) for t in ("adventure_versions","stage_versions","review_versions") if exists(db,t)]
    fresh["latestHistoryChange"] = max((x for x in hs if x), default=None)
    return {
        "generatedAt": dt.datetime.now(dt.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00","Z"),
        "sourceRelease": args.source_release,
        "sourceSha": args.source_sha,
        "counts": counts,
        "freshness": fresh,
    }

def union_parts(db, detail=False):
    p = []
    if exists(db,"labs"):
        p.append("SELECT l.inserted_at t,'lab' k,l.guid g,'discovered' c,1 v,NULL x,COALESCE(a.title,json_extract(l.raw_json,'$.title')) title FROM labs l LEFT JOIN adventures a ON a.guid=l.guid")
    if exists(db,"adventure_versions"):
        p.append("SELECT v.superseded_at t,'adventure' k,v.adventure_guid g,v.change c,v.version_seq v,NULL x,a.title title FROM adventure_versions v LEFT JOIN adventures a ON a.guid=v.adventure_guid")
    if exists(db,"stage_versions"):
        p.append("SELECT v.superseded_at t,'stage' k,v.adventure_guid g,v.change c,v.version_seq v,v.stage_index x,a.title title FROM stage_versions v LEFT JOIN adventures a ON a.guid=v.adventure_guid")
    if exists(db,"review_versions"):
        p.append("SELECT v.superseded_at t,'review' k,r.adventure_guid g,v.change c,v.version_seq v,v.review_id x,a.title title FROM review_versions v JOIN reviews r ON r.id=v.review_id LEFT JOIN adventures a ON a.guid=r.adventure_guid")
    return p

def summary(db, meta):
    parts = union_parts(db)
    recent, days = [], []
    if parts:
        sql = " UNION ALL ".join(parts) + " ORDER BY t DESC LIMIT 500"
        recent = [{"t":r[0],"k":r[1],"g":r[2],"c":r[3],"v":r[4],"x":r[5],"title":r[6]} for r in db.execute(sql) if r[2]]
        agg = []
        if exists(db,"labs"):
            agg.append("SELECT inserted_at t,'lab' k,'discovered' c FROM labs")
        if exists(db,"adventure_versions"):
            agg.append("SELECT superseded_at t,'adventure' k,change c FROM adventure_versions")
        if exists(db,"stage_versions"):
            agg.append("SELECT superseded_at t,'stage' k,change c FROM stage_versions")
        if exists(db,"review_versions"):
            agg.append("SELECT superseded_at t,'review' k,change c FROM review_versions")
        raw = " UNION ALL ".join(agg)
        q = "WITH e AS ("+raw+") SELECT substr(t,1,10),k,c,COUNT(*) FROM e GROUP BY 1,2,3 ORDER BY 1 DESC"
        d = {}
        for day,k,c,n in db.execute(q):
            e=d.setdefault(day,{"date":day,"total":0,"kinds":{},"changes":{}})
            e["total"]+=n; e["kinds"][k]=e["kinds"].get(k,0)+n; e["changes"][c]=e["changes"].get(c,0)+n
        days=[d[k] for k in sorted(d,reverse=True)[:90]]
    return {**meta,"days":days,"recent":recent}

def prefix_history(db, prefix):
    labs=defaultdict(lambda:{"events":[]})
    if exists(db,"labs"):
        for guid, inserted_at in db.execute(
            "SELECT guid,inserted_at FROM labs WHERE lower(substr(guid,1,2))=?",
            (prefix,),
        ):
            labs[guid]["events"].append({
                "t": inserted_at, "k": "lab", "c": "discovered", "v": 1, "d": []
            })
    if exists(db,"adventure_versions"):
        cur=dict(db.execute("SELECT guid,raw_json FROM adventures WHERE lower(substr(guid,1,2))=?",(prefix,)))
        rows=db.execute("SELECT adventure_guid,version_seq,superseded_at,change,raw_json FROM adventure_versions WHERE lower(substr(adventure_guid,1,2))=? ORDER BY adventure_guid,version_seq",(prefix,)).fetchall()
        g=defaultdict(list)
        for r in rows:g[r[0]].append(r)
        for guid,seqs in g.items():
            for i,(_,v,t,c,raw) in enumerate(seqs):
                after=seqs[i+1][4] if i+1<len(seqs) else cur.get(guid)
                labs[guid]["events"].append({"t":t,"k":"adventure","c":c,"v":v,"d":diff(raw,after)})
    if exists(db,"stage_versions"):
        cur={(g,i):raw for g,i,raw in db.execute("SELECT adventure_guid,stage_index,raw_json FROM stages WHERE lower(substr(adventure_guid,1,2))=?",(prefix,))}
        rows=db.execute("SELECT adventure_guid,stage_index,version_seq,superseded_at,change,raw_json FROM stage_versions WHERE lower(substr(adventure_guid,1,2))=? ORDER BY adventure_guid,stage_index,version_seq",(prefix,)).fetchall()
        groups=defaultdict(list)
        for r in rows:groups[(r[0],r[1])].append(r)
        for (guid,idx),seqs in groups.items():
            for i,(_,_,v,t,c,raw) in enumerate(seqs):
                after=seqs[i+1][5] if i+1<len(seqs) else cur.get((guid,idx))
                labs[guid]["events"].append({"t":t,"k":"stage","x":idx,"c":c,"v":v,"d":diff(raw,after)})
    if exists(db,"review_versions"):
        rows=db.execute("SELECT r.adventure_guid,v.review_id,v.version_seq,v.superseded_at,v.change,v.raw_json,r.raw_json FROM review_versions v JOIN reviews r ON r.id=v.review_id WHERE lower(substr(r.adventure_guid,1,2))=? ORDER BY r.adventure_guid,v.review_id,v.version_seq",(prefix,)).fetchall()
        groups=defaultdict(list)
        for r in rows:groups[(r[0],r[1])].append(r)
        for (guid,rid),seqs in groups.items():
            for i,(_,_,v,t,c,raw,current) in enumerate(seqs):
                after=seqs[i+1][5] if i+1<len(seqs) else current
                labs[guid]["events"].append({"t":t,"k":"review","x":rid,"c":c,"v":v,"d":diff(raw,after)})
    for x in labs.values():x["events"].sort(key=lambda e:e["t"],reverse=True)
    return dict(labs)

def main():
    ap=argparse.ArgumentParser()
    ap.add_argument("--db",required=True); ap.add_argument("--out",default="web/data")
    ap.add_argument("--source-release",default=os.getenv("LABSWEEP_SOURCE_RELEASE","db-latest"))
    ap.add_argument("--source-sha",default=os.getenv("GITHUB_SHA"))
    args=ap.parse_args(); out=Path(args.out); out.mkdir(parents=True,exist_ok=True)
    (out/"history").mkdir(parents=True,exist_ok=True)
    db=sqlite3.connect(f"file:{Path(args.db).resolve()}?mode=ro",uri=True); db.execute("PRAGMA query_only=ON")
    meta=make_meta(db,args)
    (out/"meta.json").write_text(json.dumps(meta,separators=(",",":")),encoding="utf-8")
    gz(out/"history-summary.json.gz",summary(db,meta))
    total=shards=labsn=0
    for i in range(256):
        p=f"{i:02x}"; data=prefix_history(db,p); path=out/"history"/f"{p}.json.gz"
        if data:
            gz(path,data); shards+=1; labsn+=len(data); total+=sum(len(x["events"]) for x in data.values())
        elif path.exists(): path.unlink()
    print(f"history: {total:,} events for {labsn:,} labs in {shards} shards")

if __name__=="__main__": main()
