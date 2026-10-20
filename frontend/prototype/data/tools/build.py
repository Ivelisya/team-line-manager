#!/usr/bin/env python3
"""Build the anonymized prototype snapshot from raw.json (extracted read-only from a395).

Outputs (under OUT dir):
  snapshot.json          everything the four prototype screens need
  series/traffic-1m.json per-node business traffic, 1-minute buckets, last 24 h (lazy)
  series/host-24h.json   per-host metrics, 5-minute buckets, last 24 h (lazy)
  series/latency-24h.json per-node TCP / egress probe latency, 5-minute buckets, last 24 h (lazy)
"""
import datetime as dt
import hashlib
import ipaddress
import json
import math
import os
import re
import secrets
import sys
import uuid as uuidlib
from collections import defaultdict, Counter
from zoneinfo import ZoneInfo

RAW = sys.argv[1]
OUT = sys.argv[2]
raw = json.load(open(RAW))
SALT = secrets.token_hex(16)  # discarded after the build: fake ids are not reversible
BJ = ZoneInfo("Asia/Shanghai")
NOW = raw["now"]
NOWM = int(NOW * 1000)


def bj(ts):
    return dt.datetime.fromtimestamp(ts, BJ)


def bj_text(ts, sec=True):
    return bj(ts).strftime("%Y-%m-%d %H:%M:%S" if sec else "%Y-%m-%d %H:%M") if ts else None


DAY0 = bj(NOW).replace(hour=0, minute=0, second=0, microsecond=0).timestamp()
MONTH0 = bj(NOW).replace(day=1, hour=0, minute=0, second=0, microsecond=0).timestamp()


def parse_iso(v):
    if not v:
        return None
    try:
        v = v.replace("Z", "+00:00")
        m = re.match(r"^(.*?\.)(\d+)(.*)$", v)
        if m:
            v = m[1] + m[2][:6] + m[3]
        return dt.datetime.fromisoformat(v).timestamp()
    except Exception:
        return None


def fake_hex(seed, n=8):
    return hashlib.sha256((SALT + str(seed)).encode()).hexdigest()[:n]


def fake_uuid(seed):
    h = hashlib.sha256((SALT + "uuid" + str(seed)).encode()).digest()
    return str(uuidlib.UUID(bytes=h[:16], version=4))


def r3(x, n=3):
    return None if x is None else round(x, n)


# ---------------------------------------------------------------------------
# Asset identity
# ---------------------------------------------------------------------------
assets = raw["assets"]
hosts = raw["meta"]["hosts"]
monitor = raw["meta"]["monitor"]
mnodes = monitor["nodes"]
kclients = {c["uuid"]: c for c in raw["komari_clients"]}
relays_raw = raw["relays"]

TEAM_ORDER = {"团队1": 1, "团队2": 2, "线路机": 3}


def short_name(a):
    return a["name"].split(" · ")[0]


def node_id(a):
    s = short_name(a)
    m = re.fullmatch(r"团队(\d+)-(\d+)", s)
    if m:
        return "t%s-%s" % (m[1], m[2])
    m = re.fullmatch(r"线路机(\d+)", s)
    if m:
        return "r%s" % m[1]
    return "a-" + fake_hex(a["uuid"])


ID = {a["uuid"]: node_id(a) for a in assets}
SHORT = {a["uuid"]: short_name(a) for a in assets}
BY_ID = {ID[a["uuid"]]: a for a in assets}


def sort_key(a):
    s = short_name(a)
    m = re.fullmatch(r"团队(\d+)-(\d+)", s)
    if m:
        return (TEAM_ORDER.get(a["team"], 9), int(m[2]))
    m = re.fullmatch(r"线路机(\d+)", s)
    return (TEAM_ORDER.get(a["team"], 9), int(m[1]) if m else 99)


assets.sort(key=sort_key)

PROVIDER_CODE = {"ATTVPS": "ATT", "NodeMach": "NM", "OhnoICU": "MX", "GoMami": "GM"}
PROVIDER_KIND = {"ATTVPS": "residential_vps", "NodeMach": "socks_proxy", "OhnoICU": "nat_vps", "GoMami": "relay_vps"}
PROVIDER_HOST_ACCESS = {"ATTVPS": True, "NodeMach": False, "OhnoICU": True, "GoMami": True}
REGION = {"🇺🇸": ("US", "美国"), "🇲🇽": ("MX", "墨西哥")}

# ---------------------------------------------------------------------------
# IP anonymization: seed the mapping so infrastructure gets tidy addresses.
# relays -> 203.0.113.10/20/30, landing nodes -> 198.51.100.11.., everything else after.
# ---------------------------------------------------------------------------
IPMAP = {}
POOL_NODE = iter(["198.51.100.%d" % i for i in range(11, 255)])
POOL_OTHER = iter(["203.0.113.%d" % i for i in range(40, 255)] + ["198.51.100.%d" % i for i in range(100, 200)])
for _i in range(200, 255):
    IPMAP["198.51.100.%d" % _i] = "198.51.100.%d" % _i  # demo rows use these literally
IP6MAP = {}


def seed_ip(real, fake):
    if real and real not in IPMAP:
        IPMAP[real] = fake


fleet_ips = {}
for j in raw["jobs"]:
    if j["kind"] == "relay" and j["params"].get("ip"):
        fleet_ips[j["params"]["name"]] = j["params"]["ip"]
for a in assets:
    if a["team"] == "线路机":
        k = kclients.get(a["uuid"], {})
        real = a.get("ip") or k.get("ipv4") or fleet_ips.get(a["name"])
        n = int(short_name(a)[-1])
        seed_ip(real, "203.0.113.%d" % (n * 10))
for a in assets:
    if a["team"] != "线路机" and a.get("ip") and a["ip"] not in IPMAP:
        IPMAP[a["ip"]] = next(POOL_NODE)
for a in assets:  # relay_ip fields on nodes point to relays already seeded
    if a.get("relay_ip") and a["relay_ip"] not in IPMAP:
        IPMAP[a["relay_ip"]] = next(POOL_OTHER)

IPV4_RE = re.compile(r"(?<![\d.])(\d{1,3}(?:\.\d{1,3}){3})(?![\d.])")
DASHED_RE = re.compile(r"(?<=ss-)(\d{1,3}(?:-\d{1,3}){3})\b|(?<=socks-)(\d{1,3}(?:-\d{1,3}){3})\b")
IPV6_RE = re.compile(r"(?<![0-9A-Za-z:])([0-9A-Fa-f]{0,4}(?::[0-9A-Fa-f]{0,4}){2,7})(?![0-9A-Za-z:])")
UUID_RE = re.compile(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b", re.I)
FPR_RE = re.compile(r"SHA256:[A-Za-z0-9+/]{40,}={0,2}")
KEY_RE = re.compile(r"(?:ssh-(?:ed25519|rsa)|ecdsa-sha2-nistp\d+) AAAA[0-9A-Za-z+/=]+")
EMAIL_RE = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")


DOC4A = ipaddress.ip_network("198.51.100.0/24")
DOC4B = ipaddress.ip_network("203.0.113.0/24")
DOC6 = ipaddress.ip_network("2001:db8::/32")


def map_ip(real):
    try:
        addr = ipaddress.ip_address(real)
    except ValueError:
        return real
    if addr.version == 4:
        if addr in DOC4A or addr in DOC4B:
            return real  # already a documentation address (fake)
        if real not in IPMAP:
            IPMAP[real] = next(POOL_OTHER)
        return IPMAP[real]
    key = str(addr)
    if addr in DOC6:
        return real
    if key not in IP6MAP:
        IP6MAP[key] = "2001:db8::%x" % (len(IP6MAP) + 1)
    return IP6MAP[key]


UUIDMAP = {}


def anon_str(s):
    if not isinstance(s, str) or not s:
        return s
    s = IPV4_RE.sub(lambda m: map_ip(m[1]) if all(0 <= int(p) <= 255 for p in m[1].split(".")) else m[1], s)

    def dashed(m):
        g = m[1] or m[2]
        fake = map_ip(g.replace("-", "."))
        return fake.replace(".", "-")
    s = DASHED_RE.sub(dashed, s)

    def v6(m):
        tok = m[1]
        if tok.count(":") < 2:
            return tok
        try:
            ipaddress.IPv6Address(tok)
        except ValueError:
            return tok
        return map_ip(tok)
    s = IPV6_RE.sub(v6, s)

    def uu(m):
        u = m[0].lower()
        if u in ID:
            return "asset:" + ID[u]
        UUIDMAP.setdefault(u, fake_uuid(u))
        return UUIDMAP[u]
    s = UUID_RE.sub(uu, s)
    s = EMAIL_RE.sub("[邮箱已移除]", s)
    s = FPR_RE.sub(lambda m: "SHA256:" + hashlib.sha256((SALT + m[0]).encode()).hexdigest()[:43] + "（假）", s)
    s = KEY_RE.sub("[公钥已移除]", s)
    return s


def anon(obj):
    if isinstance(obj, float):
        if 1.6e9 <= obj <= 2.0e9:
            return int(round(obj))  # unix timestamps: whole seconds are enough for display
        return round(obj, 3) if math.isfinite(obj) else None
    if isinstance(obj, str):
        return anon_str(obj)
    if isinstance(obj, list):
        return [anon(x) for x in obj]
    if isinstance(obj, dict):
        return {anon_str(k) if isinstance(k, str) else k: anon(v) for k, v in obj.items()}
    return obj


# ---------------------------------------------------------------------------
# Dictionaries (brief §4.10 / §4.12, ops inventory reason words)
# ---------------------------------------------------------------------------
DICT = {
    "health": {"ok": "正常", "degraded": "降级", "fail": "故障", "maintenance": "维护中", "unknown": "未知", "na": "不适用"},
    "health_modifier": {"stale": "数据过期"},
    "lifecycle": {"onboarding": "上架中", "unbound": "尚未接入", "active": "在用", "retiring": "下线中", "retired": "已下线"},
    "step": {"queued": "排队", "running": "进行中", "succeeded": "成功", "failed": "失败", "rolled_back": "已回滚",
             "skipped": "跳过", "cancelled": "已取消", "waiting_input": "等待输入", "waiting_gate": "等待闸门", "blocked": "已停止（风险）"},
    "relay_parts": {"agent": ["在线", "离线"], "xray": ["运行中", "已停止"], "panel": ["可达", "不可达"],
                    "collector": ["正常", "中断"], "config": ["已核对", "待核对"]},
    "entry": {"enable": ["启用", "停用"], "activity": ["有活动", "空闲"], "binding": ["已绑定", "未绑定"]},
    "access_action": {"routed": "已转发", "blocked": "已阻止"},
    "incident": {"critical": "故障", "warning": "提醒", "resolved": "已恢复", "ack": "已知悉"},
    "check": {"collector": "线路管理通道", "host": "主机探针", "cpu": "CPU 容量", "memory": "内存容量", "disk": "磁盘容量",
              "entry": "业务入口", "mapping": "固定出口绑定", "tcp": "线路到上游 TCP", "egress": "认证出口验证",
              "ingress": "实际入口到出口", "latency": "出口请求耗时"},
    "reason": {"timeout": "探测超时", "auth": "入口认证失败", "egress_mismatch": "出口不符", "policy_blocked": "规则阻止",
               "upstream_down": "上游不通", "probe_offline": "探针离线", "stale": "数据尚未更新", "unstable": "链路不稳定",
               "retrying": "出口重试中"},
    "billing_cycle": {"monthly": "月付", "quarterly": "季付", "semiannual": "半年付", "yearly": "年付",
                      "biennial": "两年付", "triennial": "三年付", "lifetime": "长期"},
    "capability": {"host": "主机", "proxy": "家宽代理", "relay": "线路机", "probe": "连通性探针", "no_host": "无主机"},
    "direction": {"down": "下行 ↓（流向用户客户端）", "up": "上行 ↑（来自用户客户端）", "rx": "接收（网卡 RX）", "tx": "发送（网卡 TX）"},
    "provider_kind": {"residential_vps": "家宽 VPS（有主机权限）", "nat_vps": "NAT 家宽 VPS", "socks_proxy": "代理产品（无主机权限）", "relay_vps": "线路机 VPS"},
}
CYCLE = {"月付": "monthly", "季付": "quarterly", "半年付": "semiannual", "年付": "yearly", "两年付": "biennial", "三年付": "triennial", "长期": "lifetime"}


def due_days(a):
    try:
        zone = ZoneInfo(a.get("billing_timezone") or "Asia/Shanghai")
        due = dt.date.fromisoformat(a["due_date"])
        return (due - dt.datetime.fromtimestamp(NOW, zone).date()).days
    except Exception:
        return None


def tz_badge(tz):
    return {"America/Los_Angeles": "LA", "Asia/Shanghai": None}.get(tz or "Asia/Shanghai", (tz or "").split("/")[-1][:3].upper())


# ---------------------------------------------------------------------------
# Channels (Xray inbounds) per asset, as store.overview() aggregates them
# ---------------------------------------------------------------------------
def fresh_relay(r):
    return -30 <= NOW - r.get("received_at", 0) < 120 and not r.get("error")


channels = defaultdict(list)
for rname, r in relays_raw.items():
    for i in r["inbounds"]:
        channels[i["uuid"]].append({**i, "relay": rname, "fresh": fresh_relay(r), "sample_at": r.get("ts")})


def agg_channel(rows):
    if not rows:
        return None
    c = dict(rows[0])
    if len(rows) > 1:
        for f in ("rate_up", "rate_down", "counter_up", "counter_down", "up", "down", "total", "tcp_connections"):
            vals = [x.get(f) for x in rows]
            c[f] = sum(vals) if all(v is not None for v in vals) else None
        c["ports"] = [x["port"] for x in rows]
        c["fresh"] = all(x["fresh"] for x in rows)
        c["source_ips"] = sorted({ip for x in rows for ip in x.get("source_ips", [])})
    return c


CH = {u: agg_channel(rows) for u, rows in channels.items()}
probes = {p["uuid"]: p for r in relays_raw.values() for p in r.get("probe", {}).get("assets", [])}
paths = {p["uuid"]: p for r in relays_raw.values() for p in r.get("paths", {}).get("assets", [])}
onlines = {rname: set(r["panel"].get("onlines") or []) for rname, r in relays_raw.items()}

# ---------------------------------------------------------------------------
# Traffic: hours (today / month), minutes (24 h series)
# ---------------------------------------------------------------------------
hours_by_asset = defaultdict(list)
for asset, bucket, up, down in raw["hours"]:
    hours_by_asset[asset].append((bucket, up, down))


def totals(asset, since):
    b0 = int(since // 3600) * 3600
    up = down = 0
    for bucket, u, d in hours_by_asset.get(asset, []):
        if b0 <= bucket <= NOW:
            up += u
            down += d
    return {"down": down, "up": up}


def first_hour(asset):
    b = [x[0] for x in hours_by_asset.get(asset, [])]
    return min(b) if b else None


M0 = int((NOW - 86400) // 60) * 60 + 60
MN = int(NOW // 60) * 60  # current (partial) minute excluded from series
N1 = (MN - M0) // 60
min_by_asset = defaultdict(dict)
for asset, bucket, up, down in raw["minutes"]:
    if M0 <= bucket < MN:
        min_by_asset[asset][bucket] = (up, down)

fleet_down = [None] * N1
fleet_up = [None] * N1
for asset, rows in min_by_asset.items():
    for bucket, (up, down) in rows.items():
        k = (bucket - M0) // 60
        fleet_down[k] = (fleet_down[k] or 0) + down
        fleet_up[k] = (fleet_up[k] or 0) + up


def resample(series_map, t0, step_in, step_out, n_out):
    """sum 1-min buckets into step_out buckets; None when no sample in the whole bucket"""
    down = [None] * n_out
    up = [None] * n_out
    for bucket, (u, d) in series_map.items():
        k = int((bucket - t0) // step_out)
        if 0 <= k < n_out:
            down[k] = (down[k] or 0) + d
            up[k] = (up[k] or 0) + u
    return down, up


T5 = int((NOW - 86400) // 300) * 300 + 300
N5 = int((int(NOW // 300) * 300 - T5) // 300)

# ---------------------------------------------------------------------------
# Availability from health_hours (monitor.availability_windows semantics)
# ---------------------------------------------------------------------------
hh = defaultdict(list)
for asset, bucket, ok, unk, failed, maint in raw["health_hours"]:
    hh[asset].append((bucket, ok, unk, failed, maint))
mstart = raw["meta"].get("monitor_started_per_asset", {})
MSTART_DEFAULT = raw["meta"]["monitor_started_at"]


def windows(asset):
    start = mstart.get(asset, MSTART_DEFAULT)
    out = {}
    for days in (1, 7, 30):
        since = max(start, NOW - days * 86400)
        b0 = int(since // 3600) * 3600
        ok = unk = failed = maint = 0
        for bucket, o, u, f, m in hh.get(asset, []):
            if bucket >= b0:
                ok += o; unk += u; failed += f; maint += m
        observed = ok + unk + failed + maint
        expected = max(observed, int(math.ceil((NOW - since) / 60)))
        eligible = expected - maint
        out[str(days)] = {"availability_percent": r3(ok * 100 / (ok + failed)) if ok + failed else None,
                          "coverage_percent": round((ok + failed) * 100 / eligible, 2) if eligible else None,
                          "successful": ok, "failed": failed, "unknown": unk, "missing": max(0, expected - observed),
                          "maintenance": maint, "since": since}
    return out


incidents_raw = raw["incidents"]
inc_by_asset_day = Counter()
for inc in incidents_raw:
    inc_by_asset_day[(inc["asset"], bj(inc["opened_at"]).date().isoformat())] += 1


def tracker30(asset):
    start = mstart.get(asset, MSTART_DEFAULT)
    today = bj(NOW).date()
    cells = []
    for i in range(29, -1, -1):
        d = today - dt.timedelta(days=i)
        d0 = dt.datetime.combine(d, dt.time(), BJ).timestamp()
        d1 = d0 + 86400
        ok = unk = failed = maint = 0
        for bucket, o, u, f, m in hh.get(asset, []):
            if d0 <= bucket < d1:
                ok += o; unk += u; failed += f; maint += m
        lo, hi = max(d0, start), min(d1, NOW)
        expected = max(ok + unk + failed + maint, int(math.ceil(max(0, hi - lo) / 60)))
        eligible = expected - maint
        avail = r3(ok * 100 / (ok + failed)) if ok + failed else None
        cov = round((ok + failed) * 100 / eligible, 1) if eligible > 0 else None
        if hi <= lo and ok + failed + unk + maint == 0:
            state = "none"
        elif avail is None:
            state = "unknown"
        elif failed == 0:
            state = "ok"
        elif avail >= 99:
            state = "minor"
        elif avail >= 95:
            state = "degraded"
        else:
            state = "fail"
        cells.append((d.isoformat(), state, avail, cov, ok, failed, unk, maint, inc_by_asset_day.get((asset, d.isoformat()), 0)))
    cols = list(zip(*cells))
    return {"start": cols[0][0], "end": cols[0][-1], "dates": list(cols[0]), "state": list(cols[1]), "availability": list(cols[2]),
            "coverage": list(cols[3]), "ok": list(cols[4]), "failed": list(cols[5]), "unknown": list(cols[6]),
            "maintenance": list(cols[7]), "incidents": list(cols[8])}


def hourly48(asset):
    h0 = int(NOW // 3600) * 3600 - 47 * 3600
    by = {b: (o, u, f, m) for b, o, u, f, m in hh.get(asset, [])}
    ok_, fail_, unk_, maint_ = [], [], [], []
    for k in range(48):
        o, u, f, m = by.get(h0 + k * 3600, (None, None, None, None))
        ok_.append(o); fail_.append(f); unk_.append(u); maint_.append(m)
    return {"t0": h0, "step": 3600, "ok": ok_, "failed": fail_, "unknown": unk_, "maintenance": maint_}


# ---------------------------------------------------------------------------
# Komari metric rollups: host series, ping latency
# ---------------------------------------------------------------------------
SERIES = {int(k): v for k, v in raw["metric_series"].items()}  # id -> (metric, entity, tags)
r5 = defaultdict(dict)  # (metric, entity, task) -> bucket -> row
for sid, bucket, count, s, mn, mx, last in raw["rollups_5m"]:
    metric, entity, tags = SERIES[sid]
    task = json.loads(tags).get("task_id") if tags and tags != "{}" else None
    r5[(metric, entity, task)][bucket] = (count, s, mn, mx, last)
r1 = defaultdict(dict)
for row in raw["rollups_1m"]:
    sid, bucket, count, s, mn, mx, first, last, fts, lts = row
    metric, entity, tags = SERIES[sid]
    task = json.loads(tags).get("task_id") if tags and tags != "{}" else None
    r1[(metric, entity, task)][bucket] = (count, s, mn, mx, first, last, fts, lts)

H5_T0 = (NOWM // 300000) * 300000 - 300000 * 288  # 24 h of complete 5-min buckets
H5_N = 288


def series5(metric, entity, fn=lambda c, s, mn, mx, last: s / c if c else None, n=H5_N, t0=H5_T0, task=None):
    rows = r5.get((metric, entity, task), {})
    out = []
    for k in range(n):
        row = rows.get(t0 + k * 300000)
        out.append(fn(*row) if row else None)
    return out


def rnd(v, n=1):
    return None if v is None else round(v, n)


def host_series(u, ram_total, disk_total):
    avg = lambda c, s, mn, mx, last: s / c if c else None
    mx_ = lambda c, s, mn, mx, last: mx
    cpu = [rnd(x, 1) for x in series5("cpu.usage", u)]
    mem = [rnd(x * 100 / ram_total, 1) if x is not None and ram_total else None for x in series5("memory.used", u)]
    disk = [rnd(x * 100 / disk_total, 2) if x is not None and disk_total else None for x in series5("disk.used", u)]
    rx = [None if x is None else int(round(x * 8)) for x in series5("net.in.rate", u)]
    tx = [None if x is None else int(round(x * 8)) for x in series5("net.out.rate", u)]
    rx_max = [None if x is None else int(round(x * 8)) for x in series5("net.in.rate", u, mx_)]
    tx_max = [None if x is None else int(round(x * 8)) for x in series5("net.out.rate", u, mx_)]
    load = [rnd(x, 2) for x in series5("load.average", u)]
    tcp = [None if x is None else int(round(x)) for x in series5("connections.tcp", u)]
    udp = [None if x is None else int(round(x)) for x in series5("connections.udp", u)]
    proc = [None if x is None else int(round(x)) for x in series5("process.count", u)]
    swap = [None if x is None else int(x) for x in series5("swap.used", u)]
    return {"cpu_pct": cpu, "mem_pct": mem, "disk_pct": disk, "net_rx_bps": rx, "net_tx_bps": tx,
            "net_rx_bps_max": rx_max, "net_tx_bps_max": tx_max, "load1": load, "tcp": tcp, "udp": udp,
            "processes": proc, "swap_used": swap}


ping_tasks = {t["id"]: t for t in raw["komari_ping_tasks"]}


def ping_series_keys(task_id):
    t = str(task_id)
    return [k for k in set(list(r1.keys()) + list(r5.keys())) if k[0] == "ping.latency_ms" and k[2] == t]


def pick_entity(task_id, prefer):
    keys = ping_series_keys(task_id)
    ents = [k[1] for k in keys]
    for p in prefer:
        if p in ents:
            return p
    return ents[0] if ents else None


def strip20(task_id, entity):
    """last 20 real samples of a Komari ping task (-1 = lost), from 1-min rollups first/last values"""
    if not task_id or not entity:
        return None
    rows = r1.get(("ping.latency_ms", entity, str(task_id)), {})
    samples = []
    for bucket in sorted(rows):
        if bucket > NOWM:
            continue
        count, s, mn, mx, first, last, fts, lts = rows[bucket]
        if count >= 2 and fts != lts:
            samples.append((fts, first))
            samples.append((lts, last))
        else:
            samples.append((lts, last))
    samples = [x for x in samples if x[0] <= NOWM][-20:]
    ms = [None if v < 0 else int(round(v)) for _, v in samples]
    lost = [v < 0 for _, v in samples]
    vals = sorted(v for v in ms if v is not None)
    med = vals[len(vals) // 2] if vals else None
    return {"task_id": task_id, "interval_s": ping_tasks.get(task_id, {}).get("interval"),
            "t": [int(t / 1000) for t, _ in samples], "ms": ms, "lost": lost, "median_ms": med,
            "loss_pct": round(sum(lost) * 100 / len(lost), 1) if lost else None,
            "window_s": int((samples[-1][0] - samples[0][0]) / 1000) if len(samples) > 1 else None}


def latency24(task_id, entity):
    if not task_id or not entity:
        return None
    lat = r5.get(("ping.latency_ms", entity, str(task_id)), {})
    loss = r5.get(("ping.loss", entity, str(task_id)), {})
    avg, mn_, mx_, lossp = [], [], [], []
    tot_n = tot_lost = 0
    sum_ = sumsq = 0.0
    for k in range(H5_N):
        b = H5_T0 + k * 300000
        row = lat.get(b)
        lrow = loss.get(b)
        if not row:
            avg.append(None); mn_.append(None); mx_.append(None); lossp.append(None)
            continue
        count, s, mn, mx, last = row
        lost = int(round(lrow[1])) if lrow else 0
        good = count - lost
        tot_n += count; tot_lost += lost
        if good > 0:
            a_ = (s + lost) / good  # remove the -1 placeholders of lost samples
            avg.append(int(round(a_)))
            mn_.append(int(round(mn)) if mn >= 0 else None)
            mx_.append(int(round(mx)))
            sum_ += s + lost
        else:
            avg.append(None); mn_.append(None); mx_.append(None)
        lossp.append(round(lost * 100 / count, 1) if count else None)
    return {"task_id": task_id, "avg_ms": avg, "min_ms": mn_, "max_ms": mx_, "loss_pct": lossp,
            "samples": tot_n, "lost": tot_lost}


# 5-min raw rows including sum_sq are not extracted; approximate jitter from 5-min avg spread instead.
def jitter_from(lat):
    vals = [v for v in (lat or {}).get("avg_ms", []) if v is not None]
    if len(vals) < 3:
        return None
    mean = sum(vals) / len(vals)
    return {"mean_ms": round(mean, 1), "stddev_ms": round(math.sqrt(sum((v - mean) ** 2 for v in vals) / len(vals)), 1),
            "p95_ms": sorted(vals)[int(len(vals) * 0.95) - 1]}


# ---------------------------------------------------------------------------
# Renewals, incidents, jobs lookups
# ---------------------------------------------------------------------------
last_renewal = {}
for r in sorted(raw["renewals"], key=lambda x: x["ts"]):
    last_renewal[r["asset"]] = r
ss_method = {}
for j in raw["jobs"]:
    ob = j["params"].get("outbound") or {}
    if j["status"] == "succeeded" and ob.get("method"):
        ss_method.setdefault(j["title"].split(" · ")[0], ob.get("method"))

# ---------------------------------------------------------------------------
# Nodes
# ---------------------------------------------------------------------------
nodes = []
node_extra_traffic = {}
host_24h = {}
latency_24h = {}
for a in assets:
    u = a["uuid"]
    nid = ID[u]
    sname = SHORT[u]
    k = kclients.get(u, {})
    h = hosts.get(u) or {}
    n = mnodes.get(u) or {}
    is_relay = a["team"] == "线路机"
    provider = a.get("provider")
    has_host = bool(h.get("metrics_available") and (h.get("ram_total") or 0) > 0)
    caps = []
    if is_relay:
        caps.append("relay")
    if has_host:
        caps.append("host")
    if provider == "NodeMach":
        caps.append("proxy")
    if (k.get("version") or "").startswith("relay-probe"):
        caps.append("probe")
    reg = REGION.get(k.get("region"), (None, None))
    ch = CH.get(u)
    relay_name = a.get("relay") or (sname if is_relay else None)
    port = a.get("relay_port")
    if is_relay and ch:
        port = ch["port"]
    entry_label = ("%s 主节点" % relay_name) if is_relay else ("%s:%s" % (relay_name, port) if relay_name else "直连")
    host_fresh = False
    hstamp = parse_iso(h.get("time"))
    if hstamp:
        host_fresh = -30 <= NOW - hstamp < 120
    # status
    status = n.get("status") or "unknown"
    checks = []
    first_failed = None
    for c in n.get("checks", []):
        ev = c.get("evidence") or {}
        evs = {kk: ev.get(kk) for kk in ("rc", "phase", "actual_ip", "expected_ip", "error", "attempt", "limit", "fresh", "online", "enable") if kk in ev}
        if "expected" in ev:
            evs["expected"] = ev["expected"]
            evs["actual"] = ev.get("actual")
        item = {"key": c["key"], "title": c["title"], "critical": c["critical"], "status": c["status"],
                "reason": c.get("reason"), "measured_at": c.get("measured_at") or None, "value": c.get("value"),
                "since": c.get("since"), "pending": c.get("pending"), "evidence": evs}
        checks.append(item)
        if first_failed is None and c["status"] not in ("ok", "na"):
            first_failed = c["key"]
    ck = {c["key"]: c for c in checks}
    metrics = n.get("metrics") or {}
    pr = probes.get(u) or {}
    pa = paths.get(u) or {}
    # traffic
    today = totals(u, DAY0)
    month = totals(u, MONTH0)
    fh = first_hour(u)
    cov_start = max(MONTH0, fh) if fh else None
    quota = None
    if k.get("traffic_limit"):
        quota = {"limit_bytes": k["traffic_limit"], "type": k.get("traffic_limit_type"),
                 "type_word": {"up": "只计出站（发送）", "down": "只计入站", "sum": "双向合计", "max": "取较大方向", "min": "取较小方向"}.get(k.get("traffic_limit_type")),
                 "used_bytes": h.get("net_total_up") if k.get("traffic_limit_type") == "up" else None,
                 "used_source": "Komari 网卡发送累计（按账期重置）", "reset_day": a.get("traffic_reset_day"),
                 "reset_timezone": a.get("traffic_timezone") or a.get("timezone")}
    # capacity / utilization (store.capacity semantics)
    cap_bps = a.get("capacity_bps")
    cap_src = "configured"
    if cap_bps is None and is_relay:
        cap_bps, cap_src = 1_000_000_000, "relay_default"
    if cap_bps is None:
        mm = re.fullmatch(r"\s*(\d+(?:\.\d+)?)\s*(Mbps|Gbps)\s*", str(a.get("bandwidth") or ""), re.I)
        if mm:
            cap_bps = float(mm[1]) * (1e9 if mm[2].lower() == "gbps" else 1e6)
            cap_src = "asset_bandwidth"
    util_down = util_up = None
    if cap_bps:
        if is_relay:
            if host_fresh:
                util_up = round((h.get("net_out") or 0) * 8 * 100 / cap_bps, 3)
                util_down = round((h.get("net_in") or 0) * 8 * 100 / cap_bps, 3)
        elif ch and ch.get("fresh") and ch.get("rate_down") is not None:
            util_down = round(ch["rate_down"] * 8 * 100 / cap_bps, 3)
            util_up = round(ch["rate_up"] * 8 * 100 / cap_bps, 3)
    bw_text = a.get("bandwidth")
    if not bw_text and is_relay:
        bw_text = "1 Gbps"
    host = None
    if has_host:
        cores = k.get("cpu_cores") or None
        host = {
            "cpu_pct": rnd(h.get("cpu"), 2), "cores": cores, "cpu_model": k.get("cpu_name"),
            "mem_used": h.get("ram"), "mem_total": h.get("ram_total"),
            "mem_pct": rnd(h["ram"] * 100 / h["ram_total"], 2) if h.get("ram_total") else None,
            "swap_used": h.get("swap"), "swap_total": h.get("swap_total"),
            "disk_used": h.get("disk"), "disk_total": h.get("disk_total"),
            "disk_pct": rnd(h["disk"] * 100 / h["disk_total"], 2) if h.get("disk_total") else None,
            "load1": h.get("load"), "load5": h.get("load5"), "load15": h.get("load15"),
            "load5_per_core_pct": round(h["load5"] * 100 / cores) if cores and h.get("load5") is not None else None,
            "uptime_s": h.get("uptime"), "processes": h.get("process"),
            "net_rx_bps": int((h.get("net_in") or 0) * 8), "net_tx_bps": int((h.get("net_out") or 0) * 8),
            "net_total_rx": h.get("net_total_down"), "net_total_tx": h.get("net_total_up"),
            "tcp_conns": h.get("connections"), "udp_conns": h.get("connections_udp"),
            "reported_at": hstamp, "fresh": host_fresh, "online": h.get("online"),
            "os": k.get("os"), "kernel": k.get("kernel_version"), "arch": k.get("arch"),
            "virtualization": k.get("virtualization"), "gpu": None if (k.get("gpu_name") in (None, "", "None")) else k.get("gpu_name"),
        }
        host_24h[nid] = {"t0": H5_T0 // 1000, "step": 300, **host_series(u, h.get("ram_total"), h.get("disk_total"))}
    agent = {"kind": "relay-probe" if (k.get("version") or "").startswith("relay-probe") else ("komari-agent" if k.get("version") else None),
             "version": k.get("version"), "online": h.get("online"), "reported_at": hstamp,
             "presence": "relay_probe" if pr.get("publishes_presence") else ("agent" if has_host else "relay_probe"),
             "host_metrics": has_host}
    tags_raw = [t for t in (k.get("tags") or "").split(";") if t]
    tags = [t for t in tags_raw if t not in (provider, a.get("relay"), a.get("team")) and not re.search(r"\d\s*[MG]bps", t) and "·" not in t and "Pulse" not in t]
    cycle = CYCLE.get(a.get("billing_cycle"), a.get("billing_cycle"))
    lr = last_renewal.get(u)
    tcp_task = a.get("tcp_task_id")
    eg_task = a.get("egress_task_id")
    rel_uuid = next((x["uuid"] for x in assets if x["team"] == "线路机" and x["name"] == a.get("relay")), None)
    tcp_ent = pick_entity(tcp_task, [rel_uuid, u]) if tcp_task else None
    eg_ent = pick_entity(eg_task, [u, rel_uuid]) if eg_task else None
    strip_tcp = strip20(tcp_task, tcp_ent)
    strip_eg = strip20(eg_task, eg_ent)
    lat_tcp = latency24(tcp_task, tcp_ent)
    lat_eg = latency24(eg_task, eg_ent)
    latency_24h[nid] = {"t0": H5_T0 // 1000, "step": 300, "tcp": lat_tcp, "egress": lat_eg}
    # traffic series for this node
    d5, u5 = resample(min_by_asset.get(u, {}), T5, 60, 300, N5)
    d1 = [None] * N1
    u1 = [None] * N1
    for bucket, (uu_, dd_) in min_by_asset.get(u, {}).items():
        kk = (bucket - M0) // 60
        d1[kk] = dd_
        u1[kk] = uu_
    node_extra_traffic[nid] = {"down": d1, "up": u1}
    nz = [(i, v) for i, v in enumerate(d5) if v]
    peak_i = max(range(N5), key=lambda i: (d5[i] or 0) + (u5[i] or 0)) if any(d5) or any(u5) else None
    win = windows(u)
    node = {
        "id": nid,
        "komari_uuid": fake_uuid(u),
        "name": sname,
        "full_name": a["name"],
        "number": a.get("number") or a.get("name_number") or (int(sname.split("-")[-1]) if "-" in sname else None),
        "team": a["team"],
        "kind": "relay" if is_relay else ("proxy" if provider == "NodeMach" else "residential"),
        "capabilities": caps,
        "lifecycle": "active",
        "provider": provider,
        "provider_code": PROVIDER_CODE.get(provider),
        "region": {"code": reg[0], "name": reg[1], "flag": k.get("region")},
        "ip": a.get("ip") or k.get("ipv4") or fleet_ips.get(a["name"]),
        "ip_version": "v4",
        "ipv6": k.get("ipv6") or None,
        "relay": relay_name,
        "relay_port": port,
        "relay_ip": a.get("relay_ip"),
        "entry_label": entry_label,
        "status": {
            "code": status, "word": DICT["health"].get(status, status),
            "reason": n.get("reason"), "reason_word": DICT["reason"].get(n.get("reason")) if n.get("reason") else None,
            "since": n.get("since"), "checked_at": n.get("checked_at"), "first_failed_check": first_failed,
            "maintenance": n.get("maintenance", False),
            "summary_counts": dict(Counter(c["status"] for c in checks)),
            "attention": u in (monitor.get("attention") or []),
        },
        "checks": checks,
        "limits": n.get("limits"),
        "latency": {
            "tcp_ms": metrics.get("tcp_ms"), "tcp_at": (ck.get("tcp") or {}).get("measured_at") or pr.get("tcp_at"),
            "egress_ms": metrics.get("egress_ms"), "egress_at": (ck.get("egress") or {}).get("measured_at") or pr.get("egress_at"),
            "egress_pending": metrics.get("egress_pending"),
            "path_ms": metrics.get("path_ms"), "path_at": (ck.get("ingress") or {}).get("measured_at") or pa.get("measured_at"),
            "ingress_ms": pr.get("ingress_ms"),
            "egress_expected_ip": pr.get("expected_ip"), "egress_actual_ip": pr.get("actual_ip") or pa.get("actual_ip"),
            "egress_verified": (pr.get("actual_ip") == pr.get("expected_ip")) if pr.get("expected_ip") and pr.get("actual_ip") else None,  # 超时没有实际出口 → 未知，不是“不符”
            "egress_limit_ms": (n.get("limits") or {}).get("latency_ms"),
            "strip_tcp": strip_tcp,
            "strip_egress": strip_eg,
            "stability_tcp_24h": jitter_from(lat_tcp),
        },
        "availability": {"windows": win, "tracker30": tracker30(u), "hourly48": hourly48(u),
                         "monitor_since": mstart.get(u, MSTART_DEFAULT)},
        "rate": None if not ch else {
            "down_bps": None if ch.get("rate_down") is None else int(round(ch["rate_down"] * 8)),
            "up_bps": None if ch.get("rate_up") is None else int(round(ch["rate_up"] * 8)),
            "sample_at": ch.get("sample_at"), "fresh": ch.get("fresh"), "source": "xray_inbound",
            "tcp_connections": ch.get("tcp_connections"), "last_active": ch.get("last_active"),
        },
        "traffic": {
            "today": today, "month": month, "day_start": DAY0, "month_start": MONTH0,
            "coverage_start": cov_start, "month_partial": bool(cov_start and cov_start > MONTH0 + 3600),
            "last24h": {"down": sum(v for v in d5 if v), "up": sum(v for v in u5 if v)},
            "peak_24h": None if peak_i is None else {"t": T5 + peak_i * 300, "down": d5[peak_i], "up": u5[peak_i], "step": 300},
            "series5m": {"t0": T5, "step": 300, "down": d5, "up": u5},
            "counter_reset": bool(ch and ch.get("counter_reset")), "sample_gap": bool(ch and ch.get("sample_gap")),
            "stats_valid": None if not ch else ch.get("stats_valid", True),
            "cumulative": None if not ch else {"down": ch.get("down"), "up": ch.get("up"), "source": "3x-ui 入口累计"},
            "quota": quota,
            "host_quality": raw["meta"]["host_traffic_quality"].get(u),
        },
        "host": host,
        "agent": agent,
        "billing": {
            "price": a.get("price"), "price_cents": a.get("price_cents"), "currency": a.get("currency") or "USD",
            "cycle": cycle, "cycle_word": DICT["billing_cycle"].get(cycle, a.get("billing_cycle")),
            "monthly_cents": a.get("price_cents"),
            "due_date": a.get("due_date"), "due_days": due_days(a), "expired_at": a.get("expired_at"),
            "billing_timezone": a.get("billing_timezone"), "tz_badge": tz_badge(a.get("billing_timezone")),
            "supplier_timezone": a.get("supplier_timezone"),
            "auto_renewal_supplier": a.get("auto_renewal_supplier"), "auto_renewal_komari": bool(k.get("auto_renewal")),
            "renewal_reminder_days": a.get("renewal_reminder_days"),
            "service_id": a.get("service_id"), "plan": a.get("plan"), "provider": provider,
            "source": a.get("source"),
            "last_renewal": None if not lr else {"ts": lr["ts"], "amount_cents": lr["amount_cents"], "old_due": lr["old_due"], "new_due": lr["new_due"]},
        },
        "bandwidth": {"text": bw_text, "capacity_bps": cap_bps, "source": cap_src,
                      "measurement": "host_interface" if is_relay else "xray_inbound_payload",
                      "util_down_pct": util_down, "util_up_pct": util_up},
        "tags": tags, "tags_raw": tags_raw,
        "outbound": None if is_relay else {
            "protocol": a.get("protocol"), "target": a.get("target"), "target_port": a.get("target_port"),
            "outbound_tag": a.get("outbound_tag"), "ss_method": ss_method.get(sname) if (a.get("protocol") or "").lower() == "shadowsocks" else None,
        },
        "connection": None if not ch else {
            "inbound_tag": ch.get("tag"), "port": ch.get("port"), "inbound_protocol": ch.get("inbound_protocol"),
            "security": ch.get("security"), "reality": ch.get("reality"),
            "client_label": ((ch.get("clients") or [{}])[0]).get("label"),
            "credential_prefix": fake_hex("cred" + u, 8),
            "credential_prefix_note": "演示值：真实连接凭据（client UUID）已删除",
            "source_ips": ch.get("source_ips", []), "source_ip_count": len(ch.get("source_ips", [])),
            "active": ((ch.get("clients") or [{}])[0]).get("label") in onlines.get(ch.get("relay"), set()),
            "enable": bool(ch.get("enable")),
        },
        "probe_tasks": {"tcp_task_id": tcp_task, "egress_task_id": eg_task, "ping_task_id": a.get("ping_task_id")},
        "supplier": {"specifications": a.get("supplier_specifications"),
                     "verification": None if not a.get("supplier_verification") else {kk: a["supplier_verification"].get(kk) for kk in ("status", "verified_at", "fields", "changed_fields", "due_date_precision")},
                     "inventory_checked_at": a.get("inventory_checked_at")},
        "note": a.get("note") or "",
        "remark_private": k.get("remark") or "",
        "remark_public": k.get("public_remark") or "",
        "revision": a.get("revision"), "updated_at": a.get("updated_at"), "sync_pending": a.get("sync_pending"),
        "created_at": parse_iso((k.get("created_at") or "").replace(" ", "T")),
        "behavior": None,
    }
    nodes.append(node)

NODE = {n["id"]: n for n in nodes}

# ---------------------------------------------------------------------------
# Relays and entries
# ---------------------------------------------------------------------------
relays = []
entries = []
for a in assets:
    if a["team"] != "线路机":
        continue
    rname = a["name"]
    r = relays_raw.get(rname) or {}
    node = NODE[ID[a["uuid"]]]
    panel = r.get("panel") or {}
    fresh = fresh_relay(r) if r else False
    ins = r.get("inbounds", [])
    rel_entries = []
    for i in ins:
        bound = ID.get(i["uuid"])
        pr = probes.get(i["uuid"]) or {}
        cl = (i.get("clients") or [{}])[0]
        e = {
            "id": "%s:%s" % (rname, i["port"]), "relay": rname, "relay_id": node["id"], "port": i["port"],
            "kind": i.get("kind"), "inbound_protocol": i.get("inbound_protocol"), "security": i.get("security"),
            "reality": i.get("reality"), "tag": i.get("tag"),
            "bound_node": bound, "bound_name": SHORT.get(i["uuid"]), "team": i.get("team"),
            "binding": "bound" if bound else "unbound",
            "remark": i.get("remark"), "remark_differs": (i.get("remark") or "") != (BY_ID[bound]["name"] if bound else ""),
            "enable": bool(i.get("enable")),
            "active": cl.get("label") in onlines.get(rname, set()),
            "tcp_connections": i.get("tcp_connections"), "last_active": i.get("last_active"),
            "rate_down_bps": None if i.get("rate_down") is None else int(round(i["rate_down"] * 8)),
            "rate_up_bps": None if i.get("rate_up") is None else int(round(i["rate_up"] * 8)),
            "cumulative": {"down": i.get("down"), "up": i.get("up"), "total_limit": i.get("total") or None},
            "counter": {"down": i.get("counter_down"), "up": i.get("counter_up")},
            "counter_reset": i.get("counter_reset"), "sample_gap": i.get("sample_gap"), "stats_valid": i.get("stats_valid"),
            "outbound": None if i.get("kind") == "master" else {"protocol": i.get("protocol"), "target": i.get("target"),
                                                                 "target_port": i.get("target_port"), "outbound_tag": i.get("outbound_tag")},
            "link": {"tcp_ms": pr.get("tcp_ms"), "tcp_at": pr.get("tcp_at"), "egress_ms": pr.get("egress_ms"), "egress_at": pr.get("egress_at"),
                     "healthy": pr.get("healthy")} if pr else None,
            "egress": {"expected_ip": pr.get("expected_ip"), "actual_ip": pr.get("actual_ip"),
                       "verified": (pr.get("actual_ip") == pr.get("expected_ip")) if pr.get("expected_ip") else None} if pr else None,
            "source_ips": i.get("source_ips", []),
            "client": {"label": cl.get("label"), "enabled": cl.get("enabled"), "quota_bytes": cl.get("quota_bytes") or 0,
                       "expires_ms": cl.get("expires_ms") or 0, "credential_prefix": fake_hex("cred" + i["uuid"], 8)},
            "expiry_time": i.get("expiry_time") or 0,
            "supported": i.get("supported"),
        }
        rel_entries.append(e)
    entries.extend(rel_entries)
    h = hosts.get(a["uuid"]) or {}
    cfg_ok = bool(r.get("config_valid"))
    parts = {
        "agent": bool(h.get("online")) and node["host"] is not None and node["host"]["fresh"],
        "xray": bool(panel.get("xray_running")),
        "panel": bool(panel.get("ok")),
        "collector": fresh,
        "config": cfg_ok,
    }
    bad = [p for p, v in parts.items() if not v]
    relays.append({
        "id": node["id"], "name": rname, "ip": node["ip"], "provider": node["provider"], "plan": node["billing"]["plan"],
        "status": {"code": "ok" if not bad else "fail", "word": "正常" if not bad else "故障", "parts": parts,
                   "reasons": [DICT["relay_parts"][p][1] for p in bad]},
        "bandwidth": node["bandwidth"],
        "host": node["host"],
        "tcp_connections": h.get("connections"),
        "rate": {"rx_bps": int((h.get("net_in") or 0) * 8), "tx_bps": int((h.get("net_out") or 0) * 8), "source": "host_interface"},
        "entries_summary": {"total": len(rel_entries), "active": sum(1 for e in rel_entries if e["active"]),
                            "disabled": sum(1 for e in rel_entries if not e["enable"]), "unbound": sum(1 for e in rel_entries if not e["bound_node"]),
                            "master_port": next((e["port"] for e in rel_entries if e["kind"] == "master"), None)},
        "node_count": sum(1 for x in assets if x.get("relay") == rname),
        "versions": {"panel": panel.get("version"), "xray": panel.get("xray_version"), "agent": kclients.get(a["uuid"], {}).get("version")},
        "panel": {"ok": panel.get("ok"), "error": panel.get("error"), "xray_running": panel.get("xray_running"),
                  "cpu_pct": rnd(panel.get("cpu"), 2), "mem_used": panel.get("mem_used"), "mem_total": panel.get("mem_total"),
                  "checked_at": panel.get("checked_at"), "online_clients": len(panel.get("onlines") or [])},
        "collector": {"received_at": r.get("received_at"), "sample_ts": r.get("ts"), "fresh": fresh, "error": r.get("error"),
                      "duration_s": (raw["meta"].get("collector", {}).get("duration_seconds") or {}).get(rname),
                      "log_backlog_bytes": r.get("log_backlog_bytes"), "log_size_bytes": r.get("log_size_bytes"),
                      "event_gap": r.get("event_gap")},
        "config": {"valid": cfg_ok, "verification": r.get("config_verification"), "logging": r.get("logging"),
                   "routing_hash": (r.get("routing_hash") or "")[:12], "template_routing_hash": (r.get("template_routing_hash") or "")[:12],
                   "startup_routing_hash": (r.get("startup_routing_hash") or "")[:12], "routing_hash_source": r.get("routing_hash_source"),
                   "runtime_rules": len(r.get("runtime_rules") or []), "applied": r.get("applied"),
                   "core_id": fake_hex(r.get("core_id"), 12) if r.get("core_id") else None},
        "probe": {"updated_at": (r.get("probe") or {}).get("updated_at"), "assets": len((r.get("probe") or {}).get("assets", [])),
                  "paths_updated_at": (r.get("paths") or {}).get("updated_at")},
        "traffic_quota": node["traffic"]["quota"],
        "billing": node["billing"],
        "ssh_port": (raw["fleet"].get(rname) or {}).get("ssh_port", 22),
    })

# ---------------------------------------------------------------------------
# Fleet aggregates
# ---------------------------------------------------------------------------
def team_rows():
    rows = []
    for team in sorted({n["team"] for n in nodes}, key=lambda t: TEAM_ORDER.get(t, 9)):
        ns = [n for n in nodes if n["team"] == team]
        dist = Counter(n["status"]["code"] for n in ns)
        sampled = [n for n in ns if n["rate"] and n["rate"]["fresh"] and n["rate"]["down_bps"] is not None]
        rows.append({
            "team": team, "count": len(ns),
            "status": {s: dist.get(s, 0) for s in ("ok", "degraded", "fail", "unknown", "maintenance")},
            "rate_down_bps": sum(n["rate"]["down_bps"] for n in sampled), "rate_up_bps": sum(n["rate"]["up_bps"] for n in sampled),
            "rate_sampled": len(sampled), "rate_expected": len(ns), "rate_complete": len(sampled) == len(ns),
            "active": sum(1 for n in sampled if (n["rate"]["down_bps"] + n["rate"]["up_bps"]) / 8 >= 1024),
            "today_bytes": sum(n["traffic"]["today"]["down"] + n["traffic"]["today"]["up"] for n in ns),
            "today": {"down": sum(n["traffic"]["today"]["down"] for n in ns), "up": sum(n["traffic"]["today"]["up"] for n in ns)},
            "month": {"down": sum(n["traffic"]["month"]["down"] for n in ns), "up": sum(n["traffic"]["month"]["up"] for n in ns)},
            "monthly_cents": sum(n["billing"]["monthly_cents"] or 0 for n in ns),
        })
    return rows


groups = team_rows()
sampled = [n for n in nodes if n["rate"] and n["rate"]["fresh"] and n["rate"]["down_bps"] is not None]
spark_n = 120
spark_down = [None if v is None else int(round(v * 8 / 60)) for v in fleet_down[-spark_n:]]
spark_up = [None if v is None else int(round(v * 8 / 60)) for v in fleet_up[-spark_n:]]
due7 = sorted([n for n in nodes if n["billing"]["due_days"] is not None and n["billing"]["due_days"] <= 7], key=lambda n: n["billing"]["due_days"])
next_due = sorted([n for n in nodes if n["billing"]["due_days"] is not None], key=lambda n: (n["billing"]["due_days"], n["id"]))
counts = Counter(n["status"]["code"] for n in nodes)
fleet_today = {"down": sum(n["traffic"]["today"]["down"] for n in nodes), "up": sum(n["traffic"]["today"]["up"] for n in nodes)}
fleet_month = {"down": sum(n["traffic"]["month"]["down"] for n in nodes), "up": sum(n["traffic"]["month"]["up"] for n in nodes)}
peak_k = max(range(N1), key=lambda i: fleet_down[i] or 0)
peak_up_k = max(range(N1), key=lambda i: fleet_up[i] or 0)
kpis = {
    "normal": {"ok": counts.get("ok", 0), "total": len(nodes), "degraded": counts.get("degraded", 0), "fail": counts.get("fail", 0),
               "unknown": counts.get("unknown", 0), "maintenance": counts.get("maintenance", 0)},
    "attention": len(monitor.get("attention") or []),
    "business_rate": {"down_bps": sum(n["rate"]["down_bps"] for n in sampled), "up_bps": sum(n["rate"]["up_bps"] for n in sampled),
                      "sampled_entries": len(sampled), "expected_entries": sum(1 for n in nodes if n["rate"]),
                      "spark": {"t0": MN - spark_n * 60, "step": 60, "down_bps": spark_down, "up_bps": spark_up, "label": "近 2 小时"}},
    "today_business": {**fleet_today, "total": fleet_today["down"] + fleet_today["up"], "since": DAY0, "since_text": "北京时间 0 点起"},
    "month_business": {**fleet_month, "total": fleet_month["down"] + fleet_month["up"], "since": MONTH0,
                       "coverage_start": raw["meta"]["started_at"]},
    "due_7d": {"count": len(due7), "items": [n["id"] for n in due7],
               "nearest": {"id": next_due[0]["id"], "name": next_due[0]["name"], "days": next_due[0]["billing"]["due_days"],
                           "due_date": next_due[0]["billing"]["due_date"]} if next_due else None},
    "monthly_cost": {"cents": sum(n["billing"]["monthly_cents"] or 0 for n in nodes), "currency": "USD", "services": len(nodes)},
    "events_today": raw["activity_today_count"],
}

traffic = {
    "fleet_24h": {"t0": M0, "step": 60, "unit": "bytes_per_bucket", "down": fleet_down, "up": fleet_up,
                  "peak_down": {"t": M0 + peak_k * 60, "bytes": fleet_down[peak_k], "bps": int(round((fleet_down[peak_k] or 0) * 8 / 60))},
                  "peak_up": {"t": M0 + peak_up_k * 60, "bytes": fleet_up[peak_up_k], "bps": int(round((fleet_up[peak_up_k] or 0) * 8 / 60))},
                  "total": {"down": sum(v for v in fleet_down if v), "up": sum(v for v in fleet_up if v)},
                  "missing_minutes": sum(1 for v in fleet_down if v is None),
                  "note": "Xray 入口计数（业务流量），全部入口按分钟求和，含线路机主节点入口与监控探测流量"},
}

# rankings
def top(key, n=10, rev=True, filt=lambda x: True):
    xs = [x for x in nodes if filt(x) and key(x) is not None]
    xs.sort(key=key, reverse=rev)
    return [{"id": x["id"], "name": x["name"], "team": x["team"], "value": key(x)} for x in xs[:n]]


def avg24(nid, field):
    s = host_24h.get(nid, {}).get(field)
    vals = [v for v in (s or []) if v is not None]
    return round(sum(vals) / len(vals), 2) if vals else None


rankings = {
    "rate": top(lambda x: (x["rate"]["down_bps"] + x["rate"]["up_bps"]) if x["rate"] and x["rate"]["down_bps"] is not None else None),
    "today": top(lambda x: x["traffic"]["today"]["down"] + x["traffic"]["today"]["up"]),
    "cpu_avg_24h": top(lambda x: avg24(x["id"], "cpu_pct"), filt=lambda x: x["host"] is not None),
    "mem_avg_24h": top(lambda x: avg24(x["id"], "mem_pct"), filt=lambda x: x["host"] is not None),
    "latency_unstable_24h": top(lambda x: (x["latency"]["stability_tcp_24h"] or {}).get("stddev_ms")),
    "loss_24h": top(lambda x: round(latency_24h[x["id"]]["tcp"]["lost"] * 100 / latency_24h[x["id"]]["tcp"]["samples"], 2)
                    if latency_24h[x["id"]]["tcp"] and latency_24h[x["id"]]["tcp"]["samples"] else None),
}
for rk in rankings["rate"]:
    n_ = NODE[rk["id"]]
    rk.update({"down_bps": n_["rate"]["down_bps"], "up_bps": n_["rate"]["up_bps"], "ip": n_["ip"]})

upcoming_due = [{"id": n["id"], "name": n["name"], "team": n["team"], "due_date": n["billing"]["due_date"], "due_days": n["billing"]["due_days"],
                 "tz_badge": n["billing"]["tz_badge"], "monthly_cents": n["billing"]["monthly_cents"], "provider": n["provider"]}
                for n in next_due[:10]]

# incidents & attention
def inc_row(inc):
    ev = inc.get("evidence") or {}
    evd = ev.get("evidence") or {}
    end = inc.get("resolved_at") or NOW
    return {"id": inc["id"], "node": ID.get(inc["asset"]), "name": SHORT.get(inc["asset"]), "check": inc["key"], "title": inc["title"],
            "severity": inc["severity"], "severity_word": DICT["incident"].get(inc["severity"]),
            "status": ev.get("status"), "status_word": DICT["health"].get(ev.get("status")),
            "reason": ev.get("reason"), "reason_word": DICT["reason"].get(ev.get("reason")) if ev.get("reason") else None,
            "opened_at": inc["opened_at"], "last_seen": inc["last_seen"], "resolved_at": inc["resolved_at"],
            "duration_s": round(end - inc["opened_at"], 1), "open": inc["resolved_at"] is None,
            "ack_at": inc["ack_at"], "ack_actor": "admin" if inc.get("ack_actor") and UUID_RE.fullmatch(inc["ack_actor"] or "") else inc.get("ack_actor"),
            "note": inc["note"],
            "evidence": {"value": ev.get("value"), "measured_at": ev.get("measured_at"),
                         **{kk: evd.get(kk) for kk in ("rc", "phase", "ms", "attempt", "actual_ip", "expected_ip", "error", "expected", "actual", "enable", "fresh") if kk in evd}}}


incidents = [inc_row(i) for i in incidents_raw]

# real past moments with non-ok nodes, for demonstrating populated attention states.
# Only incidents that lasted >= 120 s (onboarding blips of 10-15 s are skipped).
long_incs = [i for i in incidents_raw if ((i["resolved_at"] or NOW) - i["opened_at"]) >= 120]
cands = []
for inc in long_incs:
    t = inc["opened_at"] + 60
    live = [i for i in incidents_raw if i["opened_at"] <= t <= (i["resolved_at"] or NOW)]
    cands.append((t, live))
moments = []
for t, live in sorted(cands, key=lambda x: -x[0]):
    if any(abs(t - m[0]) < 1800 for m in moments):
        continue
    moments.append((t, live))


def replay(t, live):
    bynode = defaultdict(list)
    for i in live:
        bynode[i["asset"]].append(i)
    items = []
    for asset, incs in bynode.items():
        sts = [(i.get("evidence") or {}).get("status") for i in incs]
        st = "fail" if "fail" in sts else ("degraded" if "degraded" in sts else "unknown")
        first = sorted(incs, key=lambda i: (0 if (i.get("evidence") or {}).get("status") == st else 1, i["opened_at"]))[0]
        ev = first.get("evidence") or {}
        evd = ev.get("evidence") or {}
        items.append({"node": ID.get(asset), "name": SHORT.get(asset), "status": st, "word": DICT["health"][st],
                      "first_failed_check": first["key"], "check_title": first["title"],
                      "reason": ev.get("reason"), "reason_word": DICT["reason"].get(ev.get("reason")) if ev.get("reason") else None,
                      "value": ev.get("value"), "evidence": {kk: evd.get(kk) for kk in ("rc", "phase", "ms", "actual_ip", "expected_ip", "error") if kk in evd},
                      "since": min(i["opened_at"] for i in incs), "duration_s": round(t - min(i["opened_at"] for i in incs)),
                      "resolved_at": max((i["resolved_at"] or NOW) for i in incs),
                      "checks": sorted({i["key"] for i in incs}), "incident_ids": sorted(i["id"] for i in incs)})
    items.sort(key=lambda x: ({"fail": 0, "degraded": 1, "unknown": 2}[x["status"]], x["name"] or ""))
    return {"at": t, "at_text": bj_text(t), "nodes": items, "count": len(items),
            "counts": dict(Counter(x["status"] for x in items))}


status_replays = [replay(t, live) for t, live in moments[:4]]
largest = max(moments, key=lambda m: len({i["asset"] for i in m[1]})) if moments else None
if largest and all(abs(largest[0] - r["at"]) > 1 for r in status_replays):
    status_replays.append(replay(*largest))

attention = {"items": [], "unknown": list(monitor.get("unknown") or []), "computed_at": monitor["checked_at"],
             "definition": "未恢复故障 + 持续降级 ≥ 5 分钟（服务端 monitor.attention）",
             "note": "快照时刻 41 台全部正常，需处理为 0；如需演示有内容的状态，用 status_replays 里真实发生过的时刻"}

# system status
host_nodes = [n for n in nodes if "host" in n["capabilities"]]
probe_nodes = [n for n in nodes if "probe" in n["capabilities"]]
backup = raw["meta"].get("backup") or {}
system = {
    "host_agents": {"online": sum(1 for n in host_nodes if n["agent"]["online"] and n["host"]["fresh"]), "total": len(host_nodes)},
    "connectivity_probes": {"online": sum(1 for n in probe_nodes if n["agent"]["online"]), "total": len(probe_nodes)},
    "orphan_hosts": sum(1 for u in hosts if u not in ID),
    "collector": {**{kk: raw["meta"]["collector"].get(kk) for kk in ("workers", "configured", "running", "waiting", "oldest_wait_seconds", "checked_at")},
                  "duration_seconds": raw["meta"]["collector"].get("duration_seconds"), "interval_s": 10},
    "monitor": {"checked_at": monitor["checked_at"], "started_at": raw["meta"]["monitor_started_at"], "interval_s": 10},
    "backup": {"verified_at": backup.get("verified_at"), "file": backup.get("file"), "size_bytes": backup.get("size_bytes"),
               "integrity": backup.get("integrity"), "scope": backup.get("scope"), "offsite": backup.get("offsite")},
    "storage": {"operations_db_bytes": 1286787072, "komari_db_bytes": 593920, "metrics_db_bytes": 111779840,
                "note": "a395 上评估副本的文件大小（2026-09-23 采集）"},
    "retention": {"activity_days": 14, "minute_days": 35, "hour_days": 400, "komari_minute_rollup_minutes": 600, "komari_5min_rollup_minutes": 3000},
    "hosts_at": raw["meta"]["hosts_at"],
    "service_started_at": raw["meta"]["started_at"],
    "availability_definition": (raw["meta"].get("monitor_statistics") or {}).get("definition"),
}

# behavior
bc = raw["behavior_cache"]
act_cache = bc.get("activity", {})
summ = bc.get("summary", {})
beh_nodes = {}
for x in summ.get("nodes", []):
    nid = ID.get(x["asset"])
    if nid:
        beh_nodes[nid] = {kk: x.get(kk) for kk in ("connections", "domains", "sources", "blocked", "ip_only", "last_seen")}
        NODE[nid]["behavior"] = beh_nodes[nid]
behavior = {
    "window_s": 86400, "computed_at": act_cache.get("computed_at"),
    "kpis": {"active_nodes": sum(1 for v in beh_nodes.values() if v["connections"]), "connections": act_cache.get("total"),
             "targets_sum_by_node": sum(v["domains"] or 0 for v in beh_nodes.values()),
             "targets_note": "按节点去重后求和（全局去重需服务端另算）",
             "blocked": sum(v["blocked"] or 0 for v in beh_nodes.values())},
    "top_domains": [{"host": d_["host"], "kind": d_["kind"], "connections": d_["connections"], "sources": d_["sources"],
                     "blocked": d_["blocked"], "last_seen": d_["last_seen"]} for d_ in act_cache.get("domains", [])],
    "top_sources": [{"source_ip": s["source_ip"], "connections": s["connections"], "domains": s.get("domains"), "last_seen": s.get("last_seen")}
                    for s in act_cache.get("sources", [])],
    "nodes": beh_nodes,
    "top_targets_by_node": [{"node": ID.get(t["asset"]), **{kk: t.get(kk) for kk in ("host", "kind", "connections", "sources", "blocked", "first_seen", "last_seen", "total_recorded")}}
                            for t in summ.get("targets", [])[:40] if ID.get(t["asset"])],
    "events_recent": [{"id": e["id"], "node": ID.get(e["asset"]), "kind": e["kind"], "subject": e["subject"], "ts": e["ts"],
                       "message": e["message"], "acked": e["ack_at"] is not None} for e in raw["behavior_events_recent"] if ID.get(e["asset"])],
    "events_counts": [{"kind": k_, "total": t_, "unacked": u_, "last": l_} for k_, t_, u_, l_ in raw["behavior_events_counts"]],
    "new_domain_daily": [{"date": (dt.date(1970, 1, 1) + dt.timedelta(days=d_)).isoformat(), "count": c_} for d_, c_ in raw["behavior_new_domain_daily"]],
    "timeline_recent": [],
}
for r in raw["activity_recent"]:
    behavior["timeline_recent"].append({"ts": r["ts"], "node": ID.get(r["asset"]), "team": r["team"], "relay": r["relay"],
                                        "source_ip": r["source_ip"], "host": r["host"], "port": r["port"], "protocol": r["protocol"],
                                        "action": r["action"], "action_word": DICT["access_action"].get(r["action"], r["action"]),
                                        "client": r["client"], "kind": r["kind"]})
access_recent = {}
for u, rows in raw["activity_per_asset"].items():
    nid = ID.get(u)
    if not nid:
        continue
    access_recent[nid] = [{"ts": r["ts"], "source_ip": r["source_ip"], "host": r["host"], "port": r["port"], "protocol": r["protocol"],
                           "action": r["action"], "action_word": DICT["access_action"].get(r["action"], r["action"]),
                           "client": r["client"], "kind": r["kind"]} for r in rows]
top_targets_node = defaultdict(list)
for d_ in raw["destinations_top"]:
    if ID.get(d_["asset"]):
        top_targets_node[ID[d_["asset"]]].append({kk: d_[kk] for kk in ("host", "kind", "connections", "blocked", "first_seen", "last_seen")})
top_sources_node = defaultdict(list)
for s in raw["sources_top"]:
    if ID.get(s["asset"]):
        top_sources_node[ID[s["asset"]]].append({kk: s[kk] for kk in ("source_ip", "connections", "first_seen", "last_seen")})
for n in nodes:
    n["access"] = {"top_targets_90d": top_targets_node.get(n["id"], [])[:8], "top_sources": top_sources_node.get(n["id"], [])[:5],
                   "sources_seen": raw["sources_count"].get(BY_ID[n["id"]]["uuid"], 0)}

# ---------------------------------------------------------------------------
# Onboarding: real history (legacy single-node jobs) + demo batch (new pipeline)
# ---------------------------------------------------------------------------
JOB_STATUS = {"succeeded": "succeeded", "failed": "failed", "rolled_back": "rolled_back", "running": "running"}
STEP_STATUS = {"ok": "succeeded", "failed": "failed", "skipped": "skipped", "pending": "queued", "running": "running"}
history = []
for j in raw["jobs"]:
    steps = []
    for s in j["steps"]:
        steps.append({"key": s["key"], "name": s["name"], "status": STEP_STATUS.get(s["status"], s["status"]),
                      "duration_s": round(s["finished_at"] - s["started_at"], 1) if s.get("started_at") and s.get("finished_at") else None,
                      "error": s.get("error")})
    p = j["params"]
    kind = "relay" if j["kind"] == "relay" else ("proxy" if (p.get("outbound") or {}).get("type") == "socks" else "residential")
    history.append({
        "id": j["id"][:8], "legacy": True, "title": j["title"], "type": kind,
        "type_word": {"relay": "线路机", "proxy": "代理产品", "residential": "家宽节点"}[kind],
        "count": 1, "status": JOB_STATUS.get(j["status"], j["status"]), "status_word": DICT["step"].get(JOB_STATUS.get(j["status"], j["status"])),
        "started_at": j["created_at"], "finished_at": j["finished_at"],
        "duration_s": round(j["finished_at"] - j["created_at"], 1) if j["finished_at"] else None,
        "operator": None,
        "relay": p.get("relay"), "team": p.get("team"), "provider": p.get("provider"), "service_id": p.get("service_id"),
        "steps": steps, "ok_steps": sum(1 for s in steps if s["status"] == "succeeded"), "total_steps": len(steps),
        "error": (j.get("result") or {}).get("error"),
    })

PHASES = [
    {"key": "preflight", "name": "预检", "steps": ["P1"]},
    {"key": "keys", "name": "密钥", "steps": ["P2", "P3"]},
    {"key": "reinstall", "name": "重装", "steps": ["P4", "P5", "P6"]},
    {"key": "system", "name": "系统与 SS", "steps": ["P7", "P8", "P9"]},
    {"key": "register", "name": "登记与 agent", "steps": ["P10", "P11", "P12"]},
    {"key": "relay", "name": "线路机", "steps": ["P13", "P14"]},
    {"key": "deliver", "name": "验收交付", "steps": ["P15", "P16"]},
]
STEPS = [
    ("P1", "preflight", "预检", "30–45 s"), ("P2", "bootstrap_key", "推密钥", "5–10 s"), ("P3", "identify", "身份采集", "≈5 s"),
    ("P4", "dd_prepare", "重装准备", "20–40 s"), ("P5", "dd_execute", "重装执行", "30–90 s"), ("P6", "dd_verify", "重装验收", "5–10 分"),
    ("P7", "harden", "系统基线", "20–40 s"), ("P8", "firewall", "防火墙", "10–20 s"), ("P9", "ss", "Shadowsocks", "10–20 s"),
    ("P10", "register", "登记", "< 3 s"), ("P11", "agent", "komari-agent", "20–40 s"), ("P12", "outbound_check", "出站校验", "2–5 s"),
    ("P13", "relay_attach", "线路机接入", "每批 30–60 s"), ("P14", "probe_attach", "探针与监测", "10–20 s"),
    ("P15", "e2e_verify", "端到端验收", "20–40 s"), ("P16", "activate", "上线交付", "< 5 s"),
]
BRIEF_COLUMNS = [  # brief §3.6 10-column variant mapped onto the 16 steps
    {"name": "盘点", "steps": ["P1"]}, {"name": "主机指纹", "steps": ["P3"]}, {"name": "DD 重装", "steps": ["P4", "P5", "P6"]},
    {"name": "SSH 密钥", "steps": ["P2"]}, {"name": "SS 与加固", "steps": ["P7", "P8", "P9"]}, {"name": "接入线路机", "steps": ["P13"]},
    {"name": "主机探针", "steps": ["P11", "P14"]}, {"name": "登记", "steps": ["P10"]}, {"name": "验收", "steps": ["P12", "P15"]}, {"name": "交付", "steps": ["P16"]},
]
BATCH_T0 = NOW - 11 * 60 - 40


def mkrow(name, provider, plan, price, bw, relay, port, team, service_id, ip, due, tz, mode, states, note=None, extra=None):
    """states: dict step -> (status, duration_s or None, detail or None)"""
    cells = []
    for code, key, label, est in STEPS:
        st, dur, detail = states.get(code, ("queued", None, None))
        cell = {"step": code, "status": st, "duration_s": dur}
        if detail:
            cell["detail"] = detail
        cells.append(cell)
    return {"name": name, "provider": provider, "provider_code": PROVIDER_CODE[provider], "plan": plan, "price": price, "currency": "USD",
            "bandwidth": bw, "relay": relay, "relay_port": port, "team": team, "service_id": service_id, "ip": ip,
            "due_date": due, "billing_timezone": tz, "mode": mode, "cells": cells, "note": note, **(extra or {})}


OK = "succeeded"
SK = ("skipped", None, "代理产品无主机权限，跳过")
done_att = {"P1": (OK, 34, None), "P2": (OK, 6, None), "P3": (OK, 5, None), "P4": (OK, 27, None), "P5": (OK, 48, None),
            "P6": (OK, 402, "Debian 13 genericcloud，expected 公钥已出现"), "P7": (OK, 31, None), "P8": (OK, 14, None), "P9": (OK, 12, None),
            "P10": (OK, 2, None), "P11": (OK, 26, None), "P12": (OK, 3, None), "P13": (OK, 41, "线路机3 批次 #1：3 个入站合并为 1 次 Xray 重载"),
            "P14": (OK, 12, None), "P15": (OK, 24, "出口 IP 与登记一致"), "P16": (OK, 2, "连接链接已生成（未展示）")}
done_att2 = {k_: (v[0], (v[1] + d_) if v[1] else v[1], v[2]) for (k_, v), d_ in zip(done_att.items(), [3, 1, 0, -4, 7, 51, -2, 3, 1, 0, 6, 0, 0, 2, -3, 0])}
demo_rows = [
    mkrow("团队1-37", "ATTVPS", "基础版 VPS", "28.88", "50 Mbps", "线路机3", 20013, "团队1", "60391", "198.51.100.201", "2026-10-23", "Asia/Shanghai", "new",
          done_att, extra={"row_state": "active", "dod": "14/14"}),
    mkrow("团队1-38", "ATTVPS", "基础版 VPS", "28.88", "50 Mbps", "线路机3", 20014, "团队1", "60392", "198.51.100.202", "2026-10-23", "Asia/Shanghai", "new",
          done_att2, extra={"row_state": "active", "dod": "14/14"}),
    mkrow("团队1-39", "ATTVPS", "基础版 VPS", "28.88", "50 Mbps", "线路机2", 20018, "团队1", "60393", "198.51.100.203", "2026-10-23", "Asia/Shanghai", "new",
          {**{k_: done_att[k_] for k_ in ("P1", "P2", "P3", "P4", "P5", "P6", "P7", "P8", "P9")}, "P10": (OK, 2, None), "P11": (OK, 29, None), "P12": (OK, 3, None),
           "P13": ("running", 18, "线路机2 批次 #2：等待窗口剩 0:42，已排入 2 台"), "P14": ("queued", None, None), "P15": ("queued", None, None), "P16": ("queued", None, None)},
          extra={"row_state": "in_relay_batch"}),
    mkrow("团队1-40", "ATTVPS", "基础版 VPS", "28.88", "50 Mbps", "线路机2", 20019, "团队1", "60394", "198.51.100.204", "2026-10-23", "Asia/Shanghai", "new",
          {"P1": (OK, 31, None), "P2": (OK, 5, None), "P3": (OK, 5, None), "P4": (OK, 22, None), "P5": (OK, 51, None),
           "P6": ("running", 192, "写盘中 · 安装环境日志：解压写入 1.9/3.0 GiB")}, extra={"row_state": "running"}),
    mkrow("团队1-41", "ATTVPS", "基础版 VPS", "28.88", "50 Mbps", "线路机2", 20020, "团队1", "60395", "198.51.100.205", "2026-10-23", "Asia/Shanghai", "new",
          {"P1": (OK, 36, None), "P2": ("waiting_input", 9, "密码被拒（邮件密码可能不对），已尝试 1/2 次；请从供应商面板复制 root 密码")},
          extra={"row_state": "waiting_input", "actions": ["输入面板密码", "打开供应商面板"]}),
    mkrow("团队1-42", "ATTVPS", "基础版 VPS", "28.88", "50 Mbps", "线路机3", 20015, "团队1", "60396", "198.51.100.206", "2026-10-23", "Asia/Shanghai", "new",
          {"P1": (OK, 33, None), "P2": (OK, 7, None), "P3": (OK, 5, None), "P4": (OK, 25, None), "P5": (OK, 46, None),
           "P6": ("failed", 1200, "重装验收超时（20 分钟）：expected 公钥未出现，主机不可达 14 分钟；可能卡在安装环境")},
          extra={"row_state": "failed", "error_class": "provider", "actions": ["再等 10 分钟", "打开面板 VNC", "从这一步重试", "放弃此行"]}),
    mkrow("团队1-43", "ATTVPS", "基础版 VPS", "28.88", "50 Mbps", "线路机3", None, "团队1", "60397", "198.51.100.207", "2026-10-23", "Asia/Shanghai", "new",
          {"P1": ("blocked", 41, "主机公钥与上次不一致，已停止，未发送密码")},
          extra={"row_state": "blocked", "risk": {"kind": "host_key_mismatch", "word": "主机指纹冲突",
                                                  "message": "主机公钥与上次不一致，已停止，未发送密码",
                                                  "detail": "10 次扫描出现 2 把 ed25519：SHA256:" + fake_hex("k1", 10) + "… ×6 / SHA256:" + fake_hex("k2", 10) + "… ×4；该 IP 上有另一台机器应答",
                                                  "fingerprints_demo": True},
                 "actions": ["重新盘点", "复制工单模板", "改 IP", "放弃此行"]}),
    mkrow("团队1-44", "NodeMach", "高速代理", "34.98", "50 Mbps", "线路机1", 20014, "团队1", "34407", "198.51.100.208", "2026-10-26", "America/Los_Angeles", "proxy",
          {"P1": (OK, 4, "P1ʹ 代理可达性：SOCKS 出口 IP 与导入一致"), "P2": SK, "P3": SK, "P4": SK, "P5": SK, "P6": SK, "P7": SK, "P8": SK, "P9": SK,
           "P10": (OK, 2, None), "P11": SK, "P12": (OK, 2, None), "P13": (OK, 33, "线路机1 批次 #1：1 个入站"), "P14": (OK, 9, "agentless 探针"),
           "P15": (OK, 21, None), "P16": (OK, 1, "连接链接已生成（未展示）")}, extra={"row_state": "active", "dod": "14/14"}),
]
demo_batch = {
    "demo": True,
    "demo_note": "演示批次：按真实供应商 / 套餐 / 价格 / 线路机端口形态构造，IP、服务号、指纹均为虚构；步骤耗时取自 pipeline-spec §8.2 估算与 09-23 实测",
    "id": "b-0923-1509", "title": "09-23 新机 ×8（ATTVPS ×7 · NodeMach ×1）", "type": "residential", "type_word": "家宽节点",
    "status": "running", "status_word": "进行中",
    "recipe": "r2026.09 · Debian 13 genericcloud 20260914-2601 · ssserver 1.25.0 aes-256-gcm · agent 1.5.0",
    "canary": {"enabled": True, "passed": True, "node": "团队1-37"},
    "options": {"relay_strategy": "均衡", "dd_concurrency": 5, "relay_window_s": 90, "dry_run": False},
    "operator": "admin", "confirmed_count": 7, "started_at": BATCH_T0, "elapsed_s": round(NOW - BATCH_T0),
    "eta_s": 7 * 60,
    "phases": PHASES, "steps": [{"code": c, "key": k_, "name": n_, "estimate": e_} for c, k_, n_, e_ in STEPS],
    "brief_columns": BRIEF_COLUMNS,
    "rows": demo_rows,
    "relay_groups": [
        {"relay": "线路机3", "batch_no": 1, "state": "partial", "members": ["团队1-37", "团队1-38", "团队1-42", "团队1-43"], "xray_reloads": 1, "probe_restarts": 1,
         "impact": "新增 2 个入口 20013–20014，合并为一次 Xray 更新（中断约 1–3 秒）"},
        {"relay": "线路机2", "batch_no": 2, "state": "collecting", "window_left_s": 42, "members": ["团队1-39", "团队1-40", "团队1-41"], "xray_reloads": 0, "probe_restarts": 0,
         "impact": "预留入口 20018–20020，窗口结束后合并为一次 Xray 更新"},
        {"relay": "线路机1", "batch_no": 1, "state": "done", "members": ["团队1-44"], "xray_reloads": 1, "probe_restarts": 0,
         "impact": "新增 1 个入口 20014"},
    ],
}


def row_summary(r):
    sts = [c["status"] for c in r["cells"]]
    if r["row_state"] == "active":
        return "done"
    return r["row_state"]


summary = Counter(row_summary(r) for r in demo_rows)
demo_batch["kpis"] = {"total": len(demo_rows), "done": summary.get("done", 0),
                      "running": summary.get("running", 0) + summary.get("in_relay_batch", 0),
                      "attention": summary.get("waiting_input", 0) + summary.get("blocked", 0),
                      "failed": summary.get("failed", 0), "eta_s": demo_batch["eta_s"]}
col_summary = []
for code, key, label, est in STEPS:
    cs = [next(c for c in r["cells"] if c["step"] == code) for r in demo_rows]
    applicable = [c for c in cs if c["status"] != "skipped"]
    col_summary.append({"step": code, "done": sum(1 for c in applicable if c["status"] == OK), "applicable": len(applicable)})
demo_batch["column_summary"] = col_summary

onboarding = {"history": history, "demo_batch": demo_batch,
              "note": "history 是真实的旧流程单台任务（8 步或线路机 11 步），每条按“单台也是一个批次”展示；demo_batch 为新流水线演示"}

# probes list (/probes)
probe_list = []
for t in raw["komari_ping_tasks"]:
    try:
        cl = json.loads(t["clients"][2:-1]) if isinstance(t["clients"], str) and t["clients"].startswith("b'") else json.loads(t["clients"] or "[]")
    except Exception:
        cl = []
    probe_list.append({"id": t["id"], "name": t["name"], "type": t["type"], "target": t["target"], "interval_s": t["interval"],
                       "executors": [ID.get(c, "orphan") for c in cl]})

# renewals & audit
renewals = [{"id": r["id"], "ts": r["ts"], "node": ID.get(r["asset"]), "name": SHORT.get(r["asset"]), "actor": r["actor"],
             "amount_cents": r["amount_cents"], "currency": "USD", "old_due": r["old_due"], "new_due": r["new_due"], "note": r["note"]} for r in raw["renewals"]]


def actor_name(a_):
    if a_ and UUID_RE.fullmatch(a_):
        return "admin"
    return a_


audit_recent = []
for r in raw["audit"]:
    tgt = r["target"]
    audit_recent.append({"id": r["id"], "ts": r["ts"], "actor": actor_name(r["actor"]), "action": r["action"],
                         "target": ("asset:" + ID[tgt]) if tgt in ID else tgt, "target_name": SHORT.get(tgt), "detail": r["detail"]})

providers = []
for pname in ["ATTVPS", "NodeMach", "OhnoICU", "GoMami"]:
    ns = [n for n in nodes if n["provider"] == pname]
    providers.append({"name": pname, "code": PROVIDER_CODE[pname], "kind": PROVIDER_KIND[pname], "kind_word": DICT["provider_kind"][PROVIDER_KIND[pname]],
                      "host_access": PROVIDER_HOST_ACCESS[pname], "count": len(ns),
                      "plans": sorted({(n["billing"]["plan"], n["billing"]["price"], n["bandwidth"]["text"]) for n in ns})})
for p in providers:
    p["plans"] = [{"plan": a_, "price": b_, "bandwidth": c_} for a_, b_, c_ in p["plans"]]


# ---------------------------------------------------------------------------
# Split: core (first paint of the 4 screens) vs detail / extra / series (lazy)
# ---------------------------------------------------------------------------
H4 = 48  # 4 h of 5-min host samples for sparklines


def spark(nid):
    s_ = host_24h.get(nid)
    if not s_:
        return None
    return {"t0": s_["t0"] + (288 - H4) * 300, "step": 300,
            **{f: s_[f][-H4:] for f in ("cpu_pct", "mem_pct", "net_rx_bps", "net_tx_bps", "load1", "tcp")}}


def compact_strip(st):
    if not st:
        return None
    t = st["t"]
    return {"task_id": st["task_id"], "interval_s": st["interval_s"], "t_end": t[-1] if t else None,
            "dt": [t[-1] - x for x in t] if t else [], "ms": st["ms"], "lost": [1 if x else 0 for x in st["lost"]],
            "median_ms": st["median_ms"], "loss_pct": st["loss_pct"], "window_s": st["window_s"]}


core_nodes, detail_nodes, traffic_5m = [], {}, {}
for n in nodes:
    nid = n["id"]
    av = n["availability"]
    tr = av["tracker30"]
    tr_core = {k_: tr[k_] for k_ in ("start", "end", "state", "availability", "coverage", "incidents")}
    win_core = {k_: {"availability_percent": v["availability_percent"], "coverage_percent": v["coverage_percent"]} for k_, v in av["windows"].items()}
    traffic_5m[nid] = n["traffic"]["series5m"]
    tcore = {k_: v for k_, v in n["traffic"].items() if k_ != "series5m"}
    HH0 = int(NOW // 3600) * 3600 - 23 * 3600
    hd, hu = [None] * 24, [None] * 24
    for bucket, (uu_, dd_) in min_by_asset.get(BY_ID[nid]["uuid"], {}).items():
        k_ = int((bucket - HH0) // 3600)
        if 0 <= k_ < 24:
            hd[k_] = (hd[k_] or 0) + dd_
            hu[k_] = (hu[k_] or 0) + uu_
    tcore["hourly24"] = {"t0": HH0, "step": 3600, "down": hd, "up": hu, "last_partial": True}
    conn = n["connection"]
    conn_core = None if not conn else {k_: v for k_, v in conn.items() if k_ != "source_ips"}
    failed_checks = [c for c in n["checks"] if c["status"] not in ("ok", "na")]
    lat = dict(n["latency"])
    lat["strip_tcp"] = compact_strip(lat["strip_tcp"])
    lat["strip_egress"] = compact_strip(lat["strip_egress"])
    stab = lat.pop("stability_tcp_24h")
    core = {k_: n[k_] for k_ in ("id", "name", "full_name", "number", "team", "kind", "capabilities", "lifecycle", "provider", "provider_code",
                                 "region", "ip", "ip_version", "ipv6", "relay", "relay_port", "entry_label", "limits", "rate", "host", "agent",
                                 "billing", "bandwidth", "tags", "outbound", "behavior")}
    core["status"] = {**n["status"], "failed_checks": failed_checks}
    core["latency"] = lat
    core["availability"] = {"windows": win_core, "tracker30": tr_core}
    core["traffic"] = tcore
    core["connection"] = conn_core
    core["host_spark"] = spark(nid)
    core_nodes.append(core)
    detail_nodes[nid] = {
        "komari_uuid": n["komari_uuid"], "relay_ip": n["relay_ip"], "checks": n["checks"],
        "availability": {"windows": av["windows"], "tracker30": tr, "hourly48": av["hourly48"], "monitor_since": av["monitor_since"]},
        "latency_stability_tcp_24h": stab,
        "connection_source_ips": (conn or {}).get("source_ips", []),
        "access": {**n["access"], "recent10": access_recent.get(nid, [])},
        "probe_tasks": n["probe_tasks"], "supplier": n["supplier"], "note": n["note"],
        "remark_private": n["remark_private"], "remark_public": n["remark_public"],
        "revision": n["revision"], "updated_at": n["updated_at"], "sync_pending": n["sync_pending"], "created_at": n["created_at"],
        "incidents": [i for i in incidents if i["node"] == nid][:20],
        "renewals": [r for r in renewals if r["node"] == nid],
        "audit": [r for r in audit_recent if r["target"] == "asset:" + nid][:20],
    }

relays_core = []
for r in relays:
    relays_core.append(r)

snapshot = {
    "schema": "team-ops-prototype-snapshot/1",
    "generated_at": dt.datetime.now(BJ).isoformat(timespec="seconds"),
    "as_of": NOW, "as_of_text": bj_text(NOW) + " 北京时间",
    "timezone": "Asia/Shanghai",
    "provenance": {
        "operations_db": "a395:~/team-ops-eval/teamops/db/operations-backup-20260923.db（只读，immutable）",
        "komari_db": "a395:~/team-ops-eval/data/komari.db（只读）",
        "metrics_db": "a395:~/team-ops-eval/data/metrics.db（只读；截取到 as_of 为止）",
        "relay_snapshot_at": {k_: v.get("received_at") for k_, v in relays_raw.items()},
    },
    "anonymization": {
        "ipv4": "全部 IPv4 → 198.51.100.0/24 或 203.0.113.0/24（同一真实 IP 始终映射到同一假 IP；线路机 203.0.113.10/20/30，落地节点出口 IP 从 198.51.100.11 起按编号顺序；198.51.100.200–254 只给演示批次用）",
        "ipv6": "全部 IPv6 → 2001:db8::/32",
        "dropped": ["VLESS client UUID（连接凭据）", "分享链接", "Komari agent token", "密码", "主机公钥与 SSH 命令", "API 密钥前缀与哈希", "邮箱"],
        "replaced": ["资产 UUID → 可读 id（t1-01、r2）与随机假 komari_uuid", "正文中的其他 UUID → 随机假 UUID", "凭据前 8 位 → 随机演示值", "Xray core_id → 随机短哈希"],
        "kept_real": ["名称", "团队", "线路机", "端口", "供应商", "服务号", "套餐", "价格", "日期", "全部指标与时间序列", "访问目标域名", "3x-ui 客户端标识"],
    },
    "site": {"name": json.loads(raw["komari_configs"].get("sitename", '"团队线路管理"')),
             "description": json.loads(raw["komari_configs"].get("description", '""')),
             "komari_version": json.loads(raw["komari_configs"].get("system_version", '""')).split("-")[0]},
    "dict": DICT,
    "teams": [{"name": t, "order": TEAM_ORDER[t]} for t in sorted({n["team"] for n in nodes}, key=lambda t: TEAM_ORDER.get(t, 9))],
    "providers": providers,
    "kpis": kpis,
    "groups": groups,
    "attention": attention,
    "incidents_recent": incidents[:30],
    "incidents_total": len(incidents),
    "status_replays": status_replays,
    "nodes": core_nodes,
    "relays": relays_core,
    "traffic": traffic,
    "rankings": rankings,
    "upcoming_due": upcoming_due,
    "system": system,
    "behavior_kpis": behavior["kpis"],
    "behavior_top": {"window_s": 86400, "computed_at": behavior["computed_at"], "total_connections": behavior["kpis"]["connections"],
                     "domains": behavior["top_domains"][:20], "sources": behavior["top_sources"][:12],
                     "events_recent": behavior["events_recent"][:12], "events_counts": behavior["events_counts"]},
    "entries": [{k_: v for k_, v in e.items() if k_ != "source_ips"} | {"source_ip_count": len(e["source_ips"])} for e in entries],
    "access_recent": access_recent,
    "onboarding": onboarding,
    "lazy": {"detail": "detail.json", "extra": "extra.json", "traffic_5m": "series/traffic-5m.json", "traffic_1m": "series/traffic-1m.json",
             "host_24h": "series/host-24h.json", "latency_24h": "series/latency-24h.json"},
}
extra = {
    "as_of": NOW,
    "entries": entries,
    "incidents": incidents,
    "behavior": behavior,
    "probes": probe_list,
    "renewals": renewals,
    "audit_recent": audit_recent,
    "audit_counts": raw["audit_counts"],
}

snapshot = anon(snapshot)
detail = anon({"as_of": NOW, "nodes": detail_nodes})
extra = anon(extra)
traffic_5m = anon({"t0": T5, "step": 300, "unit": "bytes_per_bucket", "nodes": traffic_5m})
traffic_1m = anon({"t0": M0, "step": 60, "unit": "bytes_per_bucket", "nodes": node_extra_traffic})
host24 = anon({"t0": H5_T0 // 1000, "step": 300, "nodes": host_24h})
lat24 = anon({"t0": H5_T0 // 1000, "step": 300, "nodes": latency_24h})

os.makedirs(os.path.join(OUT, "series"), exist_ok=True)


def dump(obj, path):
    s_ = json.dumps(obj, ensure_ascii=False, separators=(",", ":"))
    with open(path, "w") as f:
        f.write(s_)
    import gzip
    return {"bytes": len(s_.encode()), "gzip": len(gzip.compress(s_.encode(), 9))}


sizes = {}
for name, obj in (("snapshot.json", snapshot), ("detail.json", detail), ("extra.json", extra),
                  ("series/traffic-5m.json", traffic_5m), ("series/traffic-1m.json", traffic_1m),
                  ("series/host-24h.json", host24), ("series/latency-24h.json", lat24)):
    sizes[name] = dump(obj, os.path.join(OUT, name))
print(json.dumps({"sizes": sizes, "ipv4_mapped": len(IPMAP) - 55, "pool_left": len(list(POOL_OTHER)), "ipv6_mapped": len(IP6MAP), "uuids_replaced": len(UUIDMAP),
                  "nodes": len(nodes), "relays": len(relays), "entries": len(entries), "incidents": len(incidents),
                  "replays": [(r["at_text"], r["count"]) for r in status_replays]}, ensure_ascii=False))
