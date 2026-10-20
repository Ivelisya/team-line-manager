#!/usr/bin/env python3
"""prodcopy_load.py BASE SECONDS — 模拟一个开着界面的浏览器：保持一条 SSE，按生产 /system.requests 里的读路径与节奏取数；
每 30 s 打印一次 RssAnon / SQLite / 渲染缓存。只读（GET），auth = dev-allow-local 的本机实例。"""
import json, sys, threading, time, urllib.request

BASE, DUR = sys.argv[1], float(sys.argv[2])
def get(p, timeout=60):
    t = time.time()
    try:
        with urllib.request.urlopen(BASE + p, timeout=timeout) as r:
            b = r.read()
            return json.loads(b) if b[:1] == b'{' else None, time.time() - t
    except Exception as e:
        return {"error": str(e)}, time.time() - t

sse_frames = []
def sse():
    while True:
        try:
            with urllib.request.urlopen(BASE + "/v2/api/stream", timeout=120) as r:
                for line in r:
                    if line.startswith(b"data:"):
                        sse_frames.append(line.decode()[5:].strip())
        except Exception:
            time.sleep(2)
threading.Thread(target=sse, daemon=True).start()

ov, _ = get("/v2/api/legacy/overview")
assets = [a["uuid"] for a in ((ov or {}).get("data") or {}).get("assets", [])]
nodes = [a for a in assets][:12]
ver = {"overview": None, "health": None}
t0 = time.time(); n = 0; slow = []
while time.time() - t0 < DUR:
    tick = int((time.time() - t0) // 10)
    for res in ("overview", "health"):
        q = f"?since={ver[res]}" if ver[res] else ""
        d, dt = get(f"/v2/api/v1/delta/{res}{q}")
        ver[res] = ((d or {}).get("data") or {}).get("version", ver[res])
    get("/v2/api/v1/activity/delta?limit=500")
    for r in ("central", "line1", "line3"):
        get(f"/v2/api/v1/relays/{r}/xray?range=1h")
    get("/v2/api/v1/hosts")
    if tick % 3 == 0:
        get("/v2/api/v1/probes/latest"); get("/v2/api/v1/alerts?limit=200&state=active")
    if tick % 6 == 0:
        for p in ("/v2/api/legacy/behavior", "/v2/api/legacy/activity?limit=500", "/v2/api/legacy/incidents", "/v2/api/legacy/history?hours=24",
                  "/v2/api/legacy/renewals", "/v2/api/legacy/komari/clients", "/v2/api/legacy/komari/hosts", "/v2/api/legacy/jobs?limit=50", "/v2/api/v1/quota"):
            d, dt = get(p)
            if dt > 2: slow.append((p, round(dt, 1)))
        for u in nodes:
            get(f"/v2/api/legacy/history?asset={u}&hours=24")
    if tick % 12 == 0 and nodes:
        u = nodes[(tick // 12) % len(nodes)]
        for p in (f"/v2/api/legacy/node?asset={u}", f"/v2/api/legacy/behavior?asset={u}&seconds=604800", f"/v2/api/legacy/activity?asset={u}&limit=500&seconds=1209600",
                  "/v2/api/legacy/behavior?seconds=604800", "/v2/api/legacy/audit"):
            d, dt = get(p, timeout=120)
            if dt > 2: slow.append((p, round(dt, 1)))
    if tick % 3 == 0:
        m, _ = get("/v2/api/v1/debug/mem")
        m = (m or {}).get("data") or {}
        if not m:  # 旧版本没有 debug/mem：退回 /system
            sy, _ = get("/v2/api/v1/system")
            m = {"rss_anon_kb": (((sy or {}).get("data") or {}).get("rss_anon_mb") or 0) * 1024}
        ls = m.get("legacy_store") or {}
        w = ((ls.get("memo") or {}).get("window") or {})
        print(f"{int(time.time()-t0):5d}s anon={m.get('rss_anon_kb',0)/1024:.1f}MB commit={((m.get('allocator') or {}).get('commit_mb') or 0):.1f} "
              f"sqlite={((m.get('sqlite') or {}).get('used_mb') or 0):.1f} rcache={ls.get('rcache_entries')}/{(ls.get('rcache_bytes') or 0)/1048576:.1f}MB "
              f"win_rows={w.get('rows')} sse={len(sse_frames)} slow={slow[-3:]}", flush=True)
    n += 1
    time.sleep(max(0, t0 + (n * 10) - time.time()))
