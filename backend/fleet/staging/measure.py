#!/usr/bin/env python3
"""measure.py — 在 a395 宿主侧采样 fleetd 与旧后端副本进程的 CPU% / RSS。
用法：measure.py SECONDS [--interval 5]  → 打印 JSON：各进程平均 CPU%（相对 1 核）、峰值、RSS。"""
import json, os, sys, time
BASE = os.path.expanduser("~/team-ops-eval/fleet-stage")
TCK = os.sysconf("SC_CLK_TCK")

def pid(name):
    try:
        return int(open(os.path.join(BASE, name)).read().strip())
    except Exception:
        return None

def stat(p):
    try:
        f = open(f"/proc/{p}/stat").read().rsplit(")", 1)[1].split()
        rss = next(int(l.split()[1]) for l in open(f"/proc/{p}/status") if l.startswith("VmRSS:"))
        return int(f[11]) + int(f[12]), rss
    except Exception:
        return None, None

def main():
    dur = float(sys.argv[1]) if len(sys.argv) > 1 else 300
    iv = 5.0
    procs = {"fleetd": pid("fleetd.pid"), "legacy_backend": pid("child.pid")}
    first = {k: (stat(p)[0], time.monotonic()) for k, p in procs.items()}
    samples = {k: [] for k in procs}
    rss = {k: [] for k in procs}
    prev = dict(first)
    end = time.monotonic() + dur
    while time.monotonic() < end:
        time.sleep(iv)
        for k, p in procs.items():
            t, r = stat(p)
            if t is None:
                continue
            pt, pm = prev[k]
            now = time.monotonic()
            if pt is not None:
                samples[k].append((t - pt) / TCK / (now - pm) * 100)
            prev[k] = (t, now)
            rss[k].append(r)
    out = {}
    for k, p in procs.items():
        t_end, m_end = prev[k]
        t0, m0 = first[k]
        s = samples[k]
        out[k] = {"pid": p, "avg_cpu_pct": round((t_end - t0) / TCK / (m_end - m0) * 100, 3) if t0 is not None and t_end is not None and m_end > m0 else None,
                  "max_5s_cpu_pct": round(max(s), 2) if s else None,
                  "rss_mb_avg": round(sum(rss[k]) / len(rss[k]) / 1024, 1) if rss[k] else None,
                  "rss_mb_max": round(max(rss[k]) / 1024, 1) if rss[k] else None, "samples": len(s)}
    out["duration_s"] = dur
    print(json.dumps(out, ensure_ascii=False))

main()
