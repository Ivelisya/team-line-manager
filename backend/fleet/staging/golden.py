#!/usr/bin/env python3
"""golden.py — 为 fleetd 的“旧接口原生实现”生成逐字节基准。

在 a395 上：把 ~/team-ops-eval/data/operations.db 拷一份（cp --reflink）到 ~/team-ops-eval/fleet-stage/golden/db，
用真实的 teamops 代码（stage_server.load_backend，时钟钉在副本的 as_of）对一组旧接口路径逐个 dispatch，
按旧 Handler 的方式编码（{"data": …}，ensure_ascii=False，紧凑分隔符）写到 golden/out/<序号>.json，
清单写 golden/index.json：[{path, status, file}]。fleetd 的 `legacy render` 对同一份副本、同一时刻输出，逐字节比较。

用法（python3.12 运行时）：golden.py [--out DIR]
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import urllib.parse
from pathlib import Path

HOME = Path(os.path.expanduser("~/team-ops-eval"))
BASE = HOME / "fleet-stage"
sys.path.insert(0, str(BASE / "vendor"))
import stage_server as ss  # noqa: E402


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default=str(BASE / "golden"))
    ap.add_argument("--release", default=str(HOME / "teamops-release"))
    a = ap.parse_args()
    out = Path(a.out)
    if out.exists():
        shutil.rmtree(out)
    (out / "db").mkdir(parents=True)
    (out / "out").mkdir()
    for name in ("operations.db", "komari.db", "metrics.db"):
        subprocess.run(["cp", "--reflink=auto", str(HOME / "data" / name), str(out / "db" / name)], check=True)
    # 另存一份给 fleetd 读（dispatch 会改写本副本：monitor/behavior 的缓存），保证两边看到的是同一份起点
    subprocess.run(["cp", "--reflink=auto", str(out / "db" / "operations.db"), str(out / "operations.golden.db")], check=True)
    as_of = ss.read_as_of(out / "db" / "operations.db")
    clock = ss.VClock(as_of, "pinned")
    app = ss.load_backend(out / "db", a.release, clock)
    ov = app.dispatch("GET", "/overview", {}, "golden")
    assets = ov["assets"]
    jobs = app.dispatch("GET", "/jobs", {"limit": "50"}, "golden")["jobs"]
    node = [x for x in assets if x.get("host_measured")][0]["uuid"]
    relay = [x for x in assets if x["team"] == "线路机"][0]["uuid"]
    paths = ["/overview", "/health", "/history?hours=24", "/history?hours=48", "/incidents", "/incidents?status=open",
             "/incidents?asset=" + node, "/renewals", "/audit", "/jobs?limit=50", "/jobs", "/behavior", "/activity?limit=500",
             "/activity?limit=10&team=团队1", "/activity?limit=20&search=google", "/activity?limit=5&action=blocked",
             "/panel/overview", "/provision/defaults", "/fleet", "/public-status", "/policies", "/export", "/schema", "/schema.md",
             "/api-keys", "/api-keys/usage", "/node?asset=" + node, "/node?asset=" + relay,
             "/behavior?asset=" + node + "&seconds=604800", "/activity?asset=" + node + "&limit=10&seconds=604800",
             "/history?asset=" + node + "&hours=24", "/history?asset=" + node + "&hours=545", "/node?asset=nope", "/jobs/nope"]
    paths += ["/jobs/" + j["id"] for j in jobs[:5]]
    index = []
    for i, p in enumerate(paths):
        path, _, q = p.partition("?")
        params = {k: v[-1] for k, v in urllib.parse.parse_qs(q).items()}
        status, body = ss.dispatch(app, path, params)
        data = json.dumps(body, ensure_ascii=False, separators=(",", ":")).encode()
        f = "out/%03d.json" % i
        (out / f).write_bytes(data)
        index.append({"path": p, "status": status, "file": f, "bytes": len(data)})
    (out / "index.json").write_text(json.dumps({"as_of": as_of, "db": str(out / "operations.golden.db"), "paths": index}, ensure_ascii=False, indent=1))
    print("golden: %d paths at as_of=%s -> %s" % (len(index), as_of, out))


if __name__ == "__main__":
    main()
