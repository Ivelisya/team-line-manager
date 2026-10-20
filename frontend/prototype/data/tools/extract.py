import sqlite3, os, json, re, sys, time
H = os.path.expanduser
t0 = time.time()
def ro(p): 
    c = sqlite3.connect("file:"+H(p)+"?mode=ro&immutable=1", uri=True); return c
o = ro("~/team-ops-eval/teamops/db/operations-backup-20260923.db")
out = {}
meta = {k: json.loads(v) for k, v in o.execute("select key,value from meta where key in ('monitor','hosts','hosts_at','collector','backup','host_traffic_quality','monitor_statistics','availability_statistic_v2','started_at','monitor_started_at','behavior_started_at')")}
meta["monitor_started_per_asset"] = {k.split(":",1)[1]: json.loads(v) for k, v in o.execute("select key,value from meta where key like 'monitor_started:%'")}
NOW = meta["monitor"]["checked_at"]
out["now"] = NOW
out["meta"] = meta
out["assets"] = [json.loads(d) for (d,) in o.execute("select data from assets")]
rel = {}
for relay, ts, data in o.execute("select relay, ts, data from relays"):
    d = json.loads(data)
    for i in d.get("inbounds", []):
        i.pop("client_uuid", None)
    rel[relay] = d
out["relays"] = rel
out["fleet"] = {r: {k: v for k, v in json.loads(d).items() if k in ("ssh_port", "active", "ip")} for r, d in o.execute("select relay, data from fleet")}
cols = ["id","asset","key","title","severity","opened_at","last_seen","resolved_at","ack_at","ack_actor","note","evidence"]
out["incidents"] = [dict(zip(cols, r[:-1] + (json.loads(r[-1]) if r[-1] else None,))) for r in o.execute("select * from incidents order by opened_at desc")]
out["renewals"] = [dict(zip(["id","ts","asset","actor","amount_cents","old_due","new_due","note"], r)) for r in o.execute("select * from renewals")]
jobs = []
for r in o.execute("select id,kind,title,status,params,steps,result,created_at,finished_at from jobs order by created_at desc"):
    params = json.loads(r[4]) if r[4] else {}
    if isinstance(params.get("outbound"), dict):
        params["outbound"] = {k: v for k, v in params["outbound"].items() if k not in ("password", "pass", "user")}
    for k in ("password", "auth"):
        params.pop(k, None)
    steps = [{k: v for k, v in s.items() if k != "log"} for s in (json.loads(r[5]) if r[5] else [])]
    res = json.loads(r[6]) if r[6] else {}
    if isinstance(res, dict):
        res = {k: v for k, v in res.items() if k in ("name", "relay", "port", "egress_ip", "error", "rolled_back")}
    jobs.append({"id": r[0], "kind": r[1], "title": r[2], "status": r[3], "params": params, "steps": steps, "result": res, "created_at": r[7], "finished_at": r[8]})
out["jobs"] = jobs
aud = []
for r in o.execute("select id,ts,actor,action,target,detail from audit where action!='health_flip' order by id desc limit 150"):
    d = json.loads(r[5]) if r[5] else {}
    if isinstance(d, dict) and "before" in d and "after" in d and isinstance(d["before"], dict):
        b, a = d["before"], d["after"]
        d = {"changed": {k: [b.get(k), a.get(k)] for k in set(b) | set(a) if b.get(k) != a.get(k) and k not in ("updated_at", "revision", "sync_pending")}}
    aud.append({"id": r[0], "ts": r[1], "actor": r[2], "action": r[3], "target": r[4], "detail": d})
out["audit"] = aud
out["audit_counts"] = dict(o.execute("select action,count(*) from audit group by action").fetchall())
out["health_hours"] = o.execute("select asset,bucket,ok,unknown,failed,maintenance from health_hours").fetchall()
hs = []
for r in o.execute("select asset,bucket,ok,unknown,failed,maintenance,data from health_samples where bucket>=? order by asset,bucket", (NOW - 86400 - 120,)):
    d = json.loads(r[6]) if r[6] else {}
    hs.append([r[0], r[1], r[2], r[3], r[4], r[5], d.get("tcp_ms"), d.get("egress_ms"), d.get("path_ms"), d.get("cpu"), d.get("memory"), d.get("disk")])
out["health_samples"] = hs
out["minutes"] = o.execute("select asset,bucket,up,down from minutes where bucket>=?", (NOW - 90000,)).fetchall()
out["hours"] = o.execute("select asset,bucket,up,down from hours").fetchall()
acols = ["id","ts","asset","team","relay","source_ip","host","port","protocol","action","client","kind"]
act = {}
asset_ids = [a["uuid"] for a in out["assets"]] + sorted({i["uuid"] for d in rel.values() for i in d["inbounds"] if str(i.get("uuid","")).startswith("direct:")})
for u in asset_ids:
    act[u] = [dict(zip(acols, r)) for r in o.execute("select * from activity where asset=? and ts<=? order by ts desc limit 10", (u, NOW))]
out["activity_per_asset"] = act
out["activity_recent"] = [dict(zip(acols, r)) for r in o.execute("select * from activity where ts<=? order by ts desc limit 60", (NOW,))]
out["behavior_cache"] = {k: json.loads(v) for k, v in o.execute("select key,value from behavior_cache")}
out["behavior_events_recent"] = [dict(zip(["id","asset","kind","subject","window","ts","message","ack_at"], r)) for r in o.execute("select * from behavior_events where kind!='new_domain' order by id desc limit 60")]
out["behavior_events_counts"] = o.execute("select kind,count(*),sum(ack_at is null),max(ts) from behavior_events group by kind").fetchall()
out["behavior_new_domain_daily"] = o.execute("select cast((ts+28800)/86400 as integer),count(*) from behavior_events where kind='new_domain' group by 1 order by 1").fetchall()
out["destinations_top"] = [dict(zip(["asset","host","kind","first_seen","last_seen","connections","blocked"], r)) for r in o.execute("select * from (select *, row_number() over (partition by asset order by connections desc) rn from destinations) where rn<=12")]
out["sources_top"] = [dict(zip(["asset","source_ip","first_seen","last_seen","connections"], r)) for r in o.execute("select asset,source_ip,first_seen,last_seen,connections from (select *, row_number() over (partition by asset order by connections desc) rn from sources_seen) where rn<=6")]
out["sources_count"] = dict(o.execute("select asset,count(*) from sources_seen group by asset").fetchall())
out["activity_today_count"] = o.execute("select count(*) from activity where ts>=? and ts<=?", ((int((NOW+28800)//86400)*86400-28800), NOW)).fetchone()[0]
k = ro("~/team-ops-eval/data/komari.db")
kc = [r[1] for r in k.execute("pragma table_info(clients)")]
out["komari_clients"] = [{c: v for c, v in zip(kc, r) if c != "token"} for r in k.execute("select * from clients")]
out["komari_ping_tasks"] = [dict(zip(["id","weight","name","clients","all_clients","type","target","interval"], r)) for r in k.execute("select * from ping_tasks")]
out["komari_configs"] = dict(k.execute("select key,value from configs where key in ('sitename','description','system_version','expire_notification_lead_days','traffic_limit_percentage')").fetchall())
m = ro("~/team-ops-eval/data/metrics.db")
series = {r[0]: (r[1], r[2], r[3]) for r in m.execute("select id, metric_name, entity_id, tags from metric_series")}
out["metric_series"] = series
nowm = int(NOW * 1000)
out["rollups_5m"] = m.execute("select series_id,bucket_milli,count,sum,min_val,max_val,last_val from metric_rollups where resolution_id=2 and bucket_milli>=? and bucket_milli<=?", (nowm - 86400000 - 300000, nowm)).fetchall()
out["rollups_1m"] = m.execute("select series_id,bucket_milli,count,sum,min_val,max_val,first_val,last_val,first_ts_milli,last_ts_milli from metric_rollups where resolution_id=1 and bucket_milli>=? and bucket_milli<=?", (nowm - 7200000, nowm)).fetchall()
out["extract_seconds"] = time.time() - t0
sys.stdout.write(json.dumps(out, ensure_ascii=False, default=str))
