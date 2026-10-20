#!/usr/bin/env python3
"""用**真实的**旧 monitor.py 生成 health crate 的对照 golden。

在 a395 上跑（需要 Python ≥ 3.11，旧代码的 fromisoformat 要能解析 9 位小数秒）：
    source ~/team-ops-eval/tools/env.sh
    python3 tools/gen_golden.py <含 teamops/ 的 release 目录> tests/fixtures/legacy-overview.json tests/fixtures/legacy-golden.json

输入是生产快照里的 5 个节点（ATT 有主机探针、NM 无主机、线路机自身、线路机3 名下两个）；
先跑一段脚本化场景（覆盖 HL-04 重试、阈值、出口不符、TCP 断、策略拦截、翻转不稳定、线路机失联、主机离线、
容量、维护窗口、数据过期），再跑一段固定种子的随机场景。每一步把**输入资产**和旧 Monitor 的**输出**都记下来，
Rust 测试逐步重放并比较 status / reason / since / availability 与每项检查的 status / reason / since / pending，
最后比较 1/7/30 d 可用率窗口。
"""
import copy
import datetime as dt
import json
import random
import sqlite3
import sys
import threading

release, overview_path, out_path = sys.argv[1:4]
sys.path.insert(0, release)
from teamops import monitor as M  # noqa: E402


class FakeStore:
    """monitor.py 用到的 store 接口的最小实现（meta 以 JSON 存取，与真 store 一样每次取出新对象）"""

    def __init__(self):
        self.lock = threading.RLock()
        self.db = sqlite3.connect(":memory:", check_same_thread=False)
        self.db.row_factory = sqlite3.Row
        self.ro = self.db
        self._meta = {}
        self.audits = []

    def get_meta(self, key, default=None):
        return json.loads(self._meta[key]) if key in self._meta else default

    def meta(self, key, value):
        self._meta[key] = json.dumps(value)

    def audit(self, actor, action, target, detail):
        self.audits.append((action, target))


PICK = ["dfe4c76d-adbb-4566-93a2-ed180d7a7d6b"]  # 团队1-01 · ATT（线路机2，有主机探针）
fixture = json.load(open(overview_path))
by_name = {a["name"]: a for a in fixture["assets"]}
nm = next(a for a in fixture["assets"] if " NM " in a["name"] or "· NM ·" in a["name"])
relay2 = by_name["线路机2"]
t26 = next(a for a in fixture["assets"] if a["name"].startswith("团队1-26"))
mx = next(a for a in fixture["assets"] if a["name"].startswith("团队1-31"))
base = [copy.deepcopy(a) for a in fixture["assets"] if a["uuid"] in PICK] + [copy.deepcopy(x) for x in (nm, relay2, t26, mx)]
relays = copy.deepcopy(fixture["relays"])


def iso(t):
    d = dt.datetime.fromtimestamp(t, dt.timezone.utc)
    return d.strftime("%Y-%m-%dT%H:%M:%S.") + "%06d123Z" % d.microsecond


def fresh_all(assets, now):
    for a in assets:
        if a.get("host"):
            a["host"]["time"] = iso(now - 3)
        if a.get("probe"):
            a["probe"]["tcp_at"] = now - 5
        if a.get("path"):
            a["path"]["measured_at"] = now - 10
    for r in relays.values():
        r["received_at"], r["ts"], r["fresh"] = now - 2, now - 1, True


def set_egress(a, now, kind, attempt=3):
    p = a.get("probe")
    if not p:
        return
    p["egress_at"] = now - 1
    p["egress_attempt"] = attempt
    if kind == "ok":
        p.update(egress_ms=80, actual_ip=a["ip"], egress_rc=0, egress_error=None, egress_phase=None, egress_attempt=1)
    elif kind == "timeout":
        p.update(egress_ms=-1, actual_ip=None, egress_rc=28, egress_error="trace:curl-28,plain:curl-28", egress_phase=None)
    elif kind == "auth":
        p.update(egress_ms=-1, actual_ip=None, egress_rc=35, egress_error="trace:curl-35", egress_phase="tls")
    elif kind == "mismatch":
        p.update(egress_ms=-1, actual_ip="203.0.113.9", egress_rc=-1, egress_error="egress_ip_mismatch", egress_phase=None)
    elif kind == "slow":
        p.update(egress_ms=4200, actual_ip=a["ip"], egress_rc=0, egress_error=None, egress_attempt=1)


def set_tcp(a, now, ok):
    p = a.get("probe")
    if not p:
        return
    p["tcp_at"] = now - 5
    if ok:
        p.update(tcp_ms=7, tcp_error=None, tcp_rc=0)
    else:
        p.update(tcp_ms=-1, tcp_error="timeout", tcp_rc=-1)


def set_path(a, now, ok, rc=0):
    p = a.get("path")
    if not p:
        return
    p["measured_at"] = now - 10
    p.update(healthy=ok, actual_ip=a["ip"] if ok else None, rc=0 if ok else rc, egress_ms=70 if ok else 18000.0)


store = FakeStore()
mon = M.Monitor(store)
now = float(int(fixture["as_of"]))
steps = []


def step(assets, label):
    global now
    now += 30
    ov = {"assets": copy.deepcopy(assets), "relays": copy.deepcopy(relays)}
    mon.evaluate(ov, now)
    rep = store.get_meta("monitor")
    out = {}
    for u, n in rep["nodes"].items():
        out[u] = {"status": n["status"], "reason": n["reason"], "since": n["since"], "availability": n["availability"],
                  "checks": [[c["key"], c["status"], c["reason"], c["since"], c["pending"]] for c in n["checks"]]}
    steps.append({"label": label, "now": now, "assets": ov["assets"], "relays": ov["relays"], "expected": out})


A, NMN, R2, T26, MX = range(5)
assets = base
# ---------- 脚本化场景 ----------
fresh_all(assets, now + 30)
for i in range(3):
    fresh_all(assets, now + 30)
    for a in assets:
        set_egress(a, now + 30, "ok")
    step(assets, "steady")
# HL-04：一个周期 3 次尝试，前两次 pending
for att in (1, 2, 3):
    fresh_all(assets, now + 30)
    set_egress(assets[A], now + 30, "timeout", att)
    step(assets, f"egress timeout attempt {att}")
for att in (1, 2, 3):
    fresh_all(assets, now + 30)
    set_egress(assets[A], now + 30, "timeout", att)
    step(assets, f"egress timeout cycle2 attempt {att}")
fresh_all(assets, now + 30)
step(assets, "no new egress measurement")
fresh_all(assets, now + 30)
set_egress(assets[A], now + 30, "ok")
step(assets, "egress recovers")
# 出口不符：不重试，直接计失败
for i in range(3):
    fresh_all(assets, now + 30)
    set_egress(assets[T26], now + 30, "mismatch", 1)
    step(assets, "egress mismatch")
fresh_all(assets, now + 30)
set_egress(assets[T26], now + 30, "ok")
step(assets, "mismatch recovers")
# TCP 断 + 路径失败 → ingress upstream_down
for i in range(3):
    fresh_all(assets, now + 30)
    set_tcp(assets[MX], now + 30, False)
    set_path(assets[MX], now + 30, False, 28)
    set_egress(assets[MX], now + 30, "timeout", 3)
    step(assets, "mx down")
for i in range(2):
    fresh_all(assets, now + 30)
    set_tcp(assets[MX], now + 30, True)
    set_path(assets[MX], now + 30, True)
    set_egress(assets[MX], now + 30, "ok")
    step(assets, "mx back")
# 策略拦截：allowlist 不含 cloudflare + 路径失败 → degraded(policy_blocked)
assets[NMN]["policy"] = {"mode": "allowlist", "domains": ["example.com"]}
for i in range(2):
    fresh_all(assets, now + 30)
    set_path(assets[NMN], now + 30, False, 35)
    step(assets, "policy blocked")
assets[NMN]["policy"] = {"mode": "observe", "domains": []}
fresh_all(assets, now + 30)
set_path(assets[NMN], now + 30, True)
step(assets, "policy restored")
# 翻转 → unstable
for i, k in enumerate(["timeout", "ok", "timeout", "ok", "timeout", "ok", "ok", "ok", "ok", "ok", "ok"]):
    fresh_all(assets, now + 30)
    set_egress(assets[A], now + 30, k, 3)
    step(assets, f"flap {i} {k}")
# 出口慢：latency degraded（不影响节点状态）
fresh_all(assets, now + 30)
set_egress(assets[T26], now + 30, "slow")
step(assets, "slow egress")
# 线路机失联：线路机2 自身 collector fail，名下节点 unknown
for i in range(2):
    fresh_all(assets, now + 30)
    relays["线路机2"]["fresh"] = False
    relays["线路机2"]["received_at"] = now - 400
    step(assets, "relay2 stale")
fresh_all(assets, now + 30)
step(assets, "relay2 back")
# 主机离线：host (2,2) 阈值
for i in range(3):
    fresh_all(assets, now + 30)
    assets[A]["host"]["online"] = False
    step(assets, "host offline")
for i in range(3):
    fresh_all(assets, now + 30)
    assets[A]["host"]["online"] = True
    step(assets, "host online")
# 主机数据过期
fresh_all(assets, now + 30)
assets[A]["host_fresh"] = False
step(assets, "host stale")
assets[A]["host_fresh"] = True
# CPU 高
for i in range(6):
    fresh_all(assets, now + 30)
    assets[R2]["host"]["cpu"] = 97.5
    step(assets, "cpu high")
assets[R2]["host"]["cpu"] = 12.0
# 入口停用 / 绑定不符
fresh_all(assets, now + 30)
assets[T26]["channel"]["enable"] = 0
step(assets, "entry disabled")
assets[T26]["channel"]["enable"] = 1
assets[T26]["channel"]["target_port"] = 9999
fresh_all(assets, now + 30)
step(assets, "mapping mismatch")
assets[T26]["channel"]["target_port"] = assets[T26]["target_port"]
fresh_all(assets, now + 30)
step(assets, "mapping restored")
# 维护窗口
assets[MX]["profile"] = {"maintenance_until": now + 600, "thresholds": {}}
for i in range(3):
    fresh_all(assets, now + 30)
    set_tcp(assets[MX], now + 30, False)
    set_egress(assets[MX], now + 30, "timeout", 3)
    step(assets, "maintenance")
assets[MX]["profile"] = {"maintenance_until": 0, "thresholds": {"latency_ms": 50}}
fresh_all(assets, now + 30)
set_tcp(assets[MX], now + 30, True)
set_egress(assets[MX], now + 30, "ok")
step(assets, "maintenance over, tight latency")
# 探测数据整体过期
for i in range(35):
    fresh_all(assets, now + 30)
    assets[T26]["probe"]["tcp_at"] = now - 1000
    step(assets, "t26 egress aging")

# ---------- 随机场景 ----------
rng = random.Random(20260924)
for i in range(160):
    t = now + 30
    fresh_all(assets, t)
    for idx, a in enumerate(assets):
        if a.get("probe") and rng.random() < 0.6:
            k = rng.choice(["ok", "ok", "ok", "timeout", "auth", "mismatch", "slow"])
            set_egress(a, t, k, rng.choice([1, 2, 3, 3]))
        if a.get("probe") and rng.random() < 0.15:
            set_tcp(a, t, rng.random() < 0.6)
        if a.get("path") and rng.random() < 0.3:
            set_path(a, t, rng.random() < 0.7, rng.choice([28, 35, 7, 0]))
        if a.get("host") and rng.random() < 0.1:
            a["host"]["online"] = rng.random() < 0.7
        if rng.random() < 0.05:
            a["host_fresh"] = rng.random() < 0.8
        if a.get("probe") and rng.random() < 0.05:
            a["probe"]["egress_at"] = t - 2000
    if rng.random() < 0.05:
        r = rng.choice(list(relays))
        relays[r]["fresh"] = False
    step(assets, f"random {i}")

windows = {a["uuid"]: mon.availability_windows(a["uuid"], now) for a in assets}
for w in windows.values():
    for v in w.values():
        v.pop("statistic", None)
json.dump({"generated_by": "gen_golden.py (real monitor.py)", "python": sys.version.split()[0], "steps": steps, "final_now": now,
           "windows": windows}, open(out_path, "w"), ensure_ascii=False, separators=(",", ":"))
print(f"{len(steps)} steps -> {out_path}")
