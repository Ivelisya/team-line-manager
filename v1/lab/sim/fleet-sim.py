#!/usr/bin/env python3
# 实验室压测/造数模拟器（只用 python3 标准库；在 fleet-dev 上运行，直连本机 PostgreSQL 与 fleet-server）。
#
# 它造出一批假机器（线路机 + 节点），像真 fleet-agent 一样向 /agent/v1/report 上报，用来看界面、图表、告警和服务端负载。
# 不碰真实机器的数据；所有模拟行都能精确清除（见下方"标记"）。通常经 lab/sim/run-sim.sh 从本机驱动。
#
# 子命令：
#   seed  --relays 6 --nodes 150 [--extra-teams 3] [--backfill-hours 24] [--no-backfill] [--seed 1]
#         直接用 SQL 插入假机器：线路机、节点（约 85% vps / 15% proxy）、出站参数、入口、订阅、续费记录、agent 令牌；
#         令牌明文存 /root/fleet-sim/tokens.json（0600）；随后用真实的上报接口回放最近 N 小时历史（图表不会是空的）。
#   backfill --hours 24   单独补历史（seed 已含；一般不用）。
#   run   --interval 60   常驻：每个周期里每台带 agent 的模拟机各上报一次（错开发送时间），proxy 没有 agent，
#                         它的健康由线路机对它的探测决定。线路机的期望状态代数变了（界面里动了模拟线路机）会回报 applied。
#   purge                 在一个事务里删除全部模拟数据（含时序、访问汇总、告警、作业、审计、模拟团队）；先停 run。
#   status                各表里模拟数据的数量、在线数、未恢复告警。
#
# 标记（purge 靠它们，不靠 tokens.json）：machine.note = 'fleet-sim'；IP 全在 10.77.0.0/16；
#   模拟团队名以「模拟团队」开头；入口/令牌/时序等都挂在这些机器 id 下。
# 故障画像（seed 时随机分配，记在 tokens.json 的 profile 里）：
#   cpu_high 持续高 CPU（降级）  disk_high 磁盘 91%（降级）（这两台按 10 秒节奏上报，原因见 cmd_run）  renew_soon 2 天后到期（降级；线路机 3 天）
#   flaky 探测约 35% 失败（忽好忽坏，会开关告警）  offline 停止上报（离线，告警）  proxy_dead 上游代理不通（宕）
# 加密列说明：reality_private_enc / node_outbound.secret_enc 填的是随机字节，不是合法密文——服务端只在
#   /agent/v1/desired 和上架/退役作业里解密它们，模拟器从不请求 /desired，也不要对模拟机器做上架/SSH 类操作。
#
# 环境变量：FLEET_DB（默认 postgres://fleet:fleet@127.0.0.1/fleet）、FLEET_URL（默认 http://127.0.0.1:8080）、
#           SIM_DIR（默认 /root/fleet-sim）、FLEET_API_KEY（status --api 用）。
import argparse
import base64
import collections
import concurrent.futures
import datetime
import gzip
import hashlib
import json
import math
import os
import random
import secrets
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
import uuid

SIM_DIR = os.environ.get("SIM_DIR", "/root/fleet-sim")
TOKENS = os.path.join(SIM_DIR, "tokens.json")
DB = os.environ.get("FLEET_DB", "postgres://fleet:fleet@127.0.0.1/fleet")
BASE = os.environ.get("FLEET_URL", "http://127.0.0.1:8080").rstrip("/")
NOTE = "fleet-sim"
TEAM_PREFIX = "模拟团队"
AGENT_VERSION = "0.1.0"
AGENT_OFFLINE_S = 120  # 与服务端 views::AGENT_OFFLINE_S 一致

# 与真 agent 的单份报告上限一致（crates/fleet-agent/src/reporter.rs）
CAP_HOST, CAP_ENT, CAP_PROBE, CAP_ACC = 360, 3000, 1000, 2000


def log(msg):
    print(time.strftime("%H:%M:%S ") + msg, flush=True)


def die(msg, code=1):
    print("错误：" + msg, file=sys.stderr, flush=True)
    sys.exit(code)


def now_ms():
    return int(time.time() * 1000)


# ── 数据库（psql 子进程）──────────────────────────────────────────────────────

def psql(sql, tx=False, quiet_out=True):
    cmd = ["psql", "-X", "-q", "-At", "-v", "ON_ERROR_STOP=1"] + (["-1"] if tx else []) + [DB]
    r = subprocess.run(cmd, input=sql, text=True, capture_output=True)
    if r.returncode:
        die("psql 失败：" + r.stderr.strip()[-2000:])
    return r.stdout


def query(sql):
    """返回 list[dict]（经 json_agg，不需要解析 psql 的列分隔）。"""
    r = subprocess.run(["psql", "-X", "-q", "-At", "-v", "ON_ERROR_STOP=1", "-c",
                        "select coalesce(json_agg(_q), '[]'::json) from (" + sql + ") _q", DB],
                       text=True, capture_output=True)
    if r.returncode:
        die("psql 查询失败：" + r.stderr.strip()[-2000:])
    return json.loads(r.stdout.strip() or "[]")


class Raw(str):
    """原样拼进 SQL 的片段（表达式）。"""


def lit(v):
    if v is None:
        return "null"
    if isinstance(v, Raw):
        return str(v)
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (int, float)):
        return repr(v)
    return "'" + str(v).replace("'", "''") + "'"


def bytea(b):
    return Raw("'\\x" + b.hex() + "'::bytea")


def insert(table, cols, rows):
    if not rows:
        return ""
    body = ",\n".join("(" + ",".join(lit(v) for v in r) + ")" for r in rows)
    return "insert into %s (%s) values\n%s;\n" % (table, ",".join(cols), body)


def uuid7():
    b = bytearray(int(time.time() * 1000).to_bytes(6, "big") + os.urandom(10))
    b[6] = (b[6] & 0x0F) | 0x70
    b[8] = (b[8] & 0x3F) | 0x80
    return str(uuid.UUID(bytes=bytes(b)))


def load_tokens():
    try:
        with open(TOKENS, encoding="utf-8") as f:
            return json.load(f)
    except FileNotFoundError:
        die("没有 %s：先 seed" % TOKENS)


# ── 随机辅助 ──────────────────────────────────────────────────────────────────

CN_A = [36, 39, 49, 58, 60, 101, 106, 110, 111, 112, 113, 114, 115, 116, 117, 118, 119, 120, 121, 122, 123, 124, 125,
        171, 175, 180, 182, 183, 202, 211, 218, 219, 220, 221, 222, 223]
OTHER_A = [24, 73, 98, 81, 82, 90, 5, 31]
DOMAINS = [
    "www.google.com", "api.github.com", "www.youtube.com", "i.ytimg.com", "api.openai.com", "chat.openai.com",
    "www.cloudflare.com", "cdn.jsdelivr.net", "registry.npmjs.org", "pypi.org", "files.pythonhosted.org",
    "www.wikipedia.org", "upload.wikimedia.org", "www.reddit.com", "x.com", "abs.twimg.com", "www.netflix.com",
    "discord.com", "cdn.discordapp.com", "web.telegram.org", "api.telegram.org", "www.amazon.com",
    "m.media-amazon.com", "www.bing.com", "login.microsoftonline.com", "graph.microsoft.com", "play.google.com",
    "fonts.gstatic.com", "www.gstatic.com", "docs.google.com", "mail.google.com", "github.com",
    "raw.githubusercontent.com", "objects.githubusercontent.com", "hub.docker.com", "registry-1.docker.io",
    "www.instagram.com", "scontent.cdninstagram.com", "www.tiktok.com", "api.anthropic.com", "claude.ai",
    "www.notion.so", "slack.com", "zoom.us", "www.paypal.com", "open.spotify.com", "gateway.discord.gg",
]
_cum = []
_acc = 0.0
for _i in range(len(DOMAINS)):
    _acc += 1.0 / (_i + 1) ** 0.9
    _cum.append(_acc)
DOM_CUM = _cum


def rand_ip(r):
    a = r.choice(CN_A) if r.random() < 0.9 else r.choice(OTHER_A)
    return "%d.%d.%d.%d" % (a, r.randint(0, 255), r.randint(0, 255), r.randint(1, 254))


def poisson(r, lam):
    if lam <= 0:
        return 0
    if lam > 30:
        return max(0, int(r.gauss(lam, math.sqrt(lam)) + 0.5))
    limit, k, p = math.exp(-lam), 0, 1.0
    while True:
        p *= r.random()
        if p <= limit:
            return k
        k += 1


def clamp(x, lo, hi):
    return lo if x < lo else hi if x > hi else x


def diurnal(t_ms):
    """日内起伏 0.24~1.0：北京时间 21 点最忙，凌晨最闲。"""
    h = ((t_ms / 1000.0 + 8 * 3600) % 86400) / 3600.0
    return 0.62 + 0.38 * math.sin(2 * math.pi * (h - 15) / 24)


# ── 模拟世界 ──────────────────────────────────────────────────────────────────

class Mach:
    def __init__(self, row, tok, now):
        self.id = row["id"]
        self.kind = row["kind"]
        self.ip = row["ip"]
        self.name = row["label"] if self.kind == "relay" else "%s-%02d" % (row["team"], row["number"])
        t = tok.get(self.id, {})
        self.token = t.get("token")
        self.profile = t.get("profile", "")
        self.offline_from = t.get("offline_from_ms")
        r = random.Random("m" + self.id)
        self.cores = row["cpu_cores"] or r.choice([1, 2, 2, 4])
        self.mem_total = (row["mem_mb"] or r.choice([1024, 2048, 4096, 8192])) * 1048576
        self.disk_total = (row["disk_gb"] or r.choice([25, 40, 60, 80])) * 10 ** 9
        self.cpu_base = r.uniform(8, 25) if self.kind == "relay" else r.uniform(3, 20)
        self.mem_base = r.uniform(0.28, 0.6)
        self.disk_base = 0.91 if self.profile == "disk_high" else r.uniform(0.2, 0.55)
        self.cpu, self.mem, self.disk, self.load = self.cpu_base, self.mem_base, self.disk_base, 0.3
        self.rx = r.randint(10 ** 10, 8 * 10 ** 11)
        self.tx = r.randint(10 ** 10, 8 * 10 ** 11)
        self.conns = 20
        self.boot_ms = now - r.randint(2, 60) * 86400000
        self.boot_id = str(uuid.UUID(int=r.getrandbits(128)))
        self.offset = r.random()
        self.host = []
        self.entries = []
        self.probes = []
        self.access = []
        self.acc_events = 0
        self.pending = collections.deque(maxlen=60)
        self.seq = 0
        self.info_at = 0
        self.fails = 0
        self.pause_until = 0.0
        self.lock = threading.Lock()
        self.applied_gen = 0

    def has_agent(self):
        return self.kind != "proxy"

    def active(self, t):
        return self.offline_from is None or t < self.offline_from

    def sysinfo(self):
        return {"hostname": "sim-%s-%s" % (self.kind, self.ip.replace(".", "-")), "kernel": "6.1.0-sim",
                "os": "Debian GNU/Linux 12 (bookworm)", "cpu_model": "Simulated vCPU @ 2.60GHz",
                "cpu_cores": self.cores, "arch": "x86_64", "ipv4": [self.ip], "ipv6": []}


class Ent:
    def __init__(self, row):
        self.id, self.relay_id, self.node_id = row["id"], row["relay_id"], row["node_id"]
        r = random.Random("e" + self.id)
        self.base = min(5e6, r.lognormvariate(math.log(1.2e5), 0.9))
        self.up_ratio = r.uniform(0.06, 0.2)
        self.rtt = r.uniform(18, 190)
        self.eg_extra = r.uniform(250, 900)
        self.access_rate = r.uniform(0.15, 1.6)
        self.clients = [rand_ip(r) for _ in range(r.randint(1, 4))]
        self.jit = r.randint(0, 9000)
        self.level = r.gauss(0, 0.3)
        # 偶尔出现的来源：每个入口固定 2 个（不是每次全随机），画像告警偶尔演示得到，但不会无限冒出新运营商 / 城市。放在最后取：前面各项的随机序列不变
        self.rare = [rand_ip(r) for _ in range(2)]
        self.down_rate = self.base
        self.up_rate = self.base * self.up_ratio
        self.conns = 3


class World:
    def __init__(self, machines, entries, tokens, seed=1, restore=None):
        now = now_ms()
        self.r = random.Random(seed)
        self.mach = {m["id"]: Mach(m, tokens, now) for m in machines}
        self.ents = {}
        self.t = now // 10000 * 10000
        self.agg = {}
        self.last_prune = 0
        self.sync_entries(entries)
        for mid, row in (restore or {}).items():
            m = self.mach.get(mid)
            if not m:
                continue
            m.cpu, m.load = row["cpu_pct"] or m.cpu, row["load1"] or m.load
            if row["mem_total"]:
                m.mem = row["mem_used"] / row["mem_total"]
            if row["disk_total"]:
                m.disk = row["disk_used"] / row["disk_total"]
            m.rx, m.tx, m.conns = row["net_rx_bytes"] or m.rx, row["net_tx_bytes"] or m.tx, row["tcp_conns"] or m.conns

    def sync_entries(self, rows):
        keep = set()
        for row in rows:
            if row["relay_id"] not in self.mach or row["node_id"] not in self.mach:
                continue
            keep.add(row["id"])
            if row["id"] not in self.ents:
                self.ents[row["id"]] = Ent(row)
        for eid in list(self.ents):
            if eid not in keep:
                del self.ents[eid]
        self.recompute_agg()

    def recompute_agg(self):
        agg = {}
        for e in self.ents.values():
            for mid in (e.relay_id, e.node_id):
                a = agg.setdefault(mid, [0.0, 0.0, 0])
                a[0] += e.down_rate
                a[1] += e.up_rate
                a[2] += e.conns
        self.agg = agg

    def advance_to(self, until_ms):
        while self.t + 10000 <= until_ms:
            self.t += 10000
            self.step(self.t)

    def step(self, t):
        r = self.r
        d = diurnal(t)
        k = (t // 10000) % 6
        tick30 = k % 3 == 0
        tick60 = k == 0
        if tick30:
            for e in self.ents.values():
                relay = self.mach[e.relay_id]
                node = self.mach[e.node_id]
                e.level += -0.12 * e.level + r.gauss(0, 0.22)
                rate = e.base * d * math.exp(e.level)
                down = int(rate * 30 * r.uniform(0.85, 1.15))
                up = int(down * e.up_ratio * r.uniform(0.8, 1.2))
                e.down_rate, e.up_rate = down / 30.0, up / 30.0
                e.conns = max(0, int(1 + rate / 60000 + r.gauss(0, 1)))
                if not relay.active(t):
                    continue
                relay.entries.append({"entry_id": e.id, "ts_ms": t, "up_bytes": up, "down_bytes": down, "conns": e.conns})
                fail_p = 0.01
                if node.profile == "flaky":
                    fail_p = 0.35
                dead = node.profile == "proxy_dead"
                ts = t - e.jit
                ok = not dead and r.random() > fail_p
                if ok:
                    ms = e.rtt * r.lognormvariate(0, 0.15) * (3 if r.random() < 0.02 else 1)
                    relay.probes.append({"entry_id": e.id, "ts_ms": ts, "kind": "tcp", "ok": True, "latency_ms": round(ms, 1)})
                else:
                    why = "connect: Connection refused (os error 111)" if dead or r.random() < 0.5 else "timeout 5s"
                    relay.probes.append({"entry_id": e.id, "ts_ms": ts, "kind": "tcp", "ok": False, "detail": why})
                if tick60:
                    ok = not dead and r.random() > fail_p
                    if ok:
                        ms = (e.rtt * 2 + e.eg_extra) * r.lognormvariate(0, 0.2)
                        relay.probes.append({"entry_id": e.id, "ts_ms": ts - 1500, "kind": "egress", "ok": True,
                                             "latency_ms": round(ms, 1), "detail": node.ip})
                    else:
                        why = r.choice(["timeout 12s", "socks5 握手失败：connection reset by peer", "出口 IP 203.0.113.%d，期望 %s" % (r.randint(2, 250), node.ip)])
                        relay.probes.append({"entry_id": e.id, "ts_ms": ts - 1500, "kind": "egress", "ok": False, "detail": why})
                for _ in range(poisson(r, e.access_rate * d * 2)):
                    relay.access.append(self.access_event(e, t))
                    relay.acc_events += 1
            self.recompute_agg()
        for m in self.mach.values():
            if not m.has_agent() or not m.active(t):
                continue
            down, up, conns = self.agg.get(m.id, (0.0, 0.0, 0))
            bps = (down + up) * 8
            if m.profile == "cpu_high":
                m.cpu = r.uniform(92, 99)
            else:
                target = m.cpu_base * (0.5 + d) + 35 * min(1.0, bps / 150e6)
                m.cpu = clamp(m.cpu + 0.25 * (target - m.cpu) + r.gauss(0, 1.2), 0.8, 60.0)
            m.mem = clamp(m.mem + 0.03 * (m.mem_base * (0.9 + 0.2 * d) - m.mem) + r.gauss(0, 0.003), 0.1, 0.93)
            if m.profile == "disk_high":
                m.disk = clamp(0.91 + r.gauss(0, 0.004), 0.88, 0.95)
            else:
                m.disk = min(0.97, m.disk + 0.0015 / 8640)
            m.load = 0.7 * m.load + 0.3 * (m.cpu / 100.0 * m.cores * r.uniform(0.8, 1.25) + 0.05)
            base_rx, base_tx = r.uniform(800, 2500) * 10, r.uniform(800, 2500) * 10
            if m.kind == "relay":
                m.rx += int((down + up) * 10 + base_rx)
                m.tx += int((down + up) * 10 + base_tx)
                m.conns = int(15 + conns * 3 + r.gauss(0, 2))
            else:
                m.rx += int(up * 10 + base_rx)
                m.tx += int(down * 10 + base_tx)
                m.conns = int(4 + conns + r.gauss(0, 1))
            m.conns = max(m.conns, 1)
            m.host.append({"ts_ms": t, "cpu_pct": round(m.cpu, 2), "load1": round(m.load, 2),
                           "mem_used": int(m.mem * m.mem_total), "mem_total": m.mem_total,
                           "disk_used": int(m.disk * m.disk_total), "disk_total": m.disk_total,
                           "net_rx_bytes": m.rx, "net_tx_bytes": m.tx, "tcp_conns": m.conns})

    def access_event(self, e, t):
        r = self.r
        x = r.random()
        if x < 0.03:
            host = "%s.cdn.example-edge.net" % secrets.token_hex(6)
        elif x < 0.04:
            host = "%d.%d.%d.%d" % (r.choice([142, 104, 151, 185, 172]), r.randint(0, 255), r.randint(0, 255), r.randint(1, 254))
        elif x < 0.07:
            host = "svc%d.example.org" % r.randint(1, 300)
        else:
            host = r.choices(DOMAINS, cum_weights=DOM_CUM)[0]
        src = r.choice(e.clients) if r.random() > 0.05 else r.choice(e.rare)
        udp = r.random() < 0.05
        port = r.choice([443, 53]) if udp else r.choices([443, 80, 8443, 5228, 993], [82, 10, 3, 3, 2])[0]
        return {"entry_id": e.id, "ts_ms": t - r.randint(0, 29999), "src_ip": src, "dest_host": host,
                "dest_port": port, "network": "udp" if udp else "tcp", "blocked": False}

    def agents(self):
        return [m for m in self.mach.values() if m.has_agent() and m.token]


# ── 从库里装配世界 ────────────────────────────────────────────────────────────

SQL_MACHINES = """select m.id::text id, m.kind::text kind, m.label, t.name team, m.number, host(m.primary_ip) ip,
  pl.cpu_cores, pl.mem_mb, pl.disk_gb
  from machine m left join team t on t.id = m.team_id left join plan pl on pl.id = m.plan_id
  where m.note = 'fleet-sim' and m.lifecycle <> 'retired' order by m.kind = 'relay' desc, t.name, m.number, m.label"""
SQL_ENTRIES = """select e.id::text id, e.relay_id::text relay_id, e.node_id::text node_id, e.port, e.lifecycle::text lc
  from entry e join machine r on r.id = e.relay_id join machine n on n.id = e.node_id
  where e.lifecycle in ('active', 'pending') and r.note = 'fleet-sim' and n.note = 'fleet-sim'"""
SQL_RELAYS = """select r.machine_id::text id, r.generation, r.applied_generation from relay r
  join machine m on m.id = r.machine_id where m.note = 'fleet-sim' and m.lifecycle <> 'retired'"""


def build_world(seed=1, restore=False):
    tokens = load_tokens()
    machines = query(SQL_MACHINES)
    if not machines:
        die("库里没有模拟机器：先 seed")
    entries = query(SQL_ENTRIES)
    rest = None
    if restore:
        rows = query("""select m.id::text mid, h.* from machine m cross join lateral (
            select cpu_pct, load1, mem_used, mem_total, disk_used, disk_total, net_rx_bytes, net_tx_bytes, tcp_conns, extract(epoch from now() - ts) ago
            from host_metric where machine_id = m.id and ts > now() - interval '3 days' order by ts desc limit 1) h
            where m.note = 'fleet-sim' and m.kind <> 'proxy'""")
        rest = {x["mid"]: x for x in rows}
    w = World(machines, entries, tokens, seed, rest)
    w.last_ago = min((x["ago"] for x in (rest or {}).values()), default=None)
    for x in query(SQL_RELAYS):
        if x["id"] in w.mach:
            w.mach[x["id"]].applied_gen = x["applied_generation"]
    return w, tokens


# ── 上报（与 fleet-agent 的线格式一致）────────────────────────────────────────

def http_post(path, obj, token, gz=True, timeout=15):
    # /agent/v1/applied 没有解压层，只有 /report 能发 gzip
    body = json.dumps(obj, separators=(",", ":")).encode()
    hdr = {"Authorization": "Bearer " + token, "Content-Type": "application/json", "User-Agent": "fleet-agent/%s-sim" % AGENT_VERSION}
    zipped = bool(gz and len(body) >= 1024)
    if zipped:
        body = gzip.compress(body, 3)
        hdr["Content-Encoding"] = "gzip"
    req = urllib.request.Request(BASE + path, data=body, headers=hdr, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, resp.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")
    except Exception as e:  # 连接失败、超时
        return 0, str(e)


def post_report(rep, token):
    return http_post("/agent/v1/report", rep, token)


def build_reports(m, now, relay_gen=None):
    """把机器缓冲里的样本打成 1..n 份报告（按单份上限切），seq 递增。"""
    info_due = m.info_at == 0 or now - m.info_at > 3600 * 1000
    out = []
    while True:
        h, e, p, a = m.host[:CAP_HOST], m.entries[:CAP_ENT], m.probes[:CAP_PROBE], m.access[:CAP_ACC]
        if not (h or e or p or a) and not (info_due and not out):
            break
        del m.host[:len(h)], m.entries[:len(e)], m.probes[:len(p)], m.access[:len(a)]
        m.seq = max(m.seq + 1, now_ms())
        rep = {"seq": m.seq, "boot_id": m.boot_id, "agent_version": AGENT_VERSION, "sent_at_ms": now_ms()}
        if info_due and not out:
            rep["info"] = m.sysinfo()
            m.info_at = now
        for k, v in (("host", h), ("entries", e), ("probes", p), ("access", a)):
            if v:
                rep[k] = v
        if m.kind == "relay":
            rep["xray"] = {"running": True, "version": "25.10.15", "started_at_ms": m.boot_ms,
                           "applied_generation": m.applied_gen if relay_gen is None else relay_gen}
        rep["agent_health"] = {"queue_len": len(m.pending), "queue_dropped": 0, "queue_dead": 0,
                               "access_lines": m.acc_events, "access_events": m.acc_events, "access_unparsed": 0,
                               "access_dropped_lines": 0, "access_rotation_gaps": 0, "access_truncations": 0,
                               "probes_skipped": 0, "realign_fixes": 0}
        out.append(rep)
    return out


def drain(m, stats, backoff=True):
    """按序发送机器队列里的报告；失败就停（下个周期再试，不重试风暴）。stats 是 Stats。"""
    if not m.lock.acquire(blocking=False):
        return
    try:
        while m.pending:
            if backoff and time.time() < m.pause_until:
                return
            rep = m.pending[0]
            t0 = time.time()
            st, text = post_report(rep, m.token)
            dt = (time.time() - t0) * 1000
            if st == 200:
                m.pending.popleft()
                m.fails = 0
                stats.ok(dt)
                continue
            stats.err()
            m.fails += 1
            if st == 401:
                m.pause_until = time.time() + 600
                stats.warn("%s 返回 401（令牌无效或已撤销）：暂停 10 分钟" % m.name)
            elif st in (400, 413, 422):
                m.pending.popleft()
                stats.warn("%s seq=%s 被服务端拒绝 HTTP %s：%s（丢弃这份）" % (m.name, rep["seq"], st, text[:160]))
            else:
                m.pause_until = time.time() + min(300, 10 * 2 ** min(m.fails, 5))
                if m.fails <= 3 or m.fails % 10 == 0:
                    stats.warn("%s 上报失败 HTTP %s（第 %d 次）：%s" % (m.name, st or "-", m.fails, text[:160]))
            return
    finally:
        m.lock.release()


class Stats:
    def __init__(self):
        self.lock = threading.Lock()
        self.n_ok = self.n_err = 0
        self.lat = []

    def ok(self, ms):
        with self.lock:
            self.n_ok += 1
            self.lat.append(ms)

    def err(self):
        with self.lock:
            self.n_err += 1

    def warn(self, msg):
        log("警告 " + msg)

    def take(self):
        with self.lock:
            o, e, lat = self.n_ok, self.n_err, sorted(self.lat)
            self.n_ok = self.n_err = 0
            self.lat = []
        p95 = lat[int(len(lat) * 0.95)] if lat else 0
        avg = sum(lat) / len(lat) if lat else 0
        return o, e, avg, p95


# ── seed ──────────────────────────────────────────────────────────────────────

def cmd_seed(a):
    if query("select id from machine where note = '%s' limit 1" % NOTE):
        die("库里已有模拟数据：先 purge 再 seed")
    if os.path.exists(TOKENS):
        die("%s 已存在而库里没有模拟机器：确认后删掉它再 seed" % TOKENS)
    rnd = random.Random(a.seed)
    os.makedirs(SIM_DIR, exist_ok=True)

    # 目录与团队
    teams = query("select id::text id, name, sort_order from team order by sort_order, name")
    real_teams = [t for t in teams if not t["name"].startswith(TEAM_PREFIX)]
    sim_teams = [t for t in teams if t["name"].startswith(TEAM_PREFIX)]
    sql = ["begin;"]
    for i in range(len(sim_teams), a.extra_teams):
        t = {"id": uuid7(), "name": TEAM_PREFIX + "ABCDEFGH"[i], "sort_order": 100 + i}
        sql.append(insert("team", ["id", "name", "sort_order"], [(t["id"], t["name"], t["sort_order"])]))
        sim_teams.append(t)
    all_teams = real_teams + sim_teams
    if not all_teams:
        die("库里没有任何团队")
    # 编号接在现有最大号之后（含已退役，避免和历史撞）
    nxt = {t["id"]: (query("select coalesce(max(number), 0) m from machine where team_id = '%s'" % t["id"])[0]["m"]) + 1 for t in all_teams}
    weights = [0.22 if t in real_teams else 0.18 for t in all_teams]

    plans = query("""select pl.id::text id, pl.provider_id::text provider_id, pl.name, pl.cpu_cores, pl.mem_mb, pl.disk_gb, pl.traffic_gb,
        pl.price_cents, pl.currency, pl.cycle::text cycle, p.code, p.timezone from plan pl join provider p on p.id = pl.provider_id
        where pl.archived_at is null order by pl.name""")
    vps_plans = [p for p in plans if p["cpu_cores"]] or plans
    proxy_plans = [p for p in plans if not p["cpu_cores"]] or plans
    relay_plans = [p for p in vps_plans if (p["cpu_cores"] or 0) >= 2 and p["code"] in ("ATT", "GM")] or vps_plans

    taken = query("select host(primary_ip) ip from machine where primary_ip << '10.77.0.0/16' and lifecycle <> 'retired'")
    if taken:
        die("10.77.0.0/16 里已有非模拟机器：" + ", ".join(x["ip"] for x in taken[:5]))

    machines = []  # dict
    for i in range(a.relays):
        p = rnd.choice(relay_plans) if relay_plans else None
        machines.append({"id": uuid7(), "kind": "relay", "label": "模拟线路机%02d" % (i + 1), "ip": "10.77.0.%d" % (i + 1),
                         "plan": p, "ref": "SIM-R%02d" % (i + 1), "team": None, "number": None, "tags": []})
    n_proxy = round(a.nodes * 0.15)
    kinds = ["proxy"] * n_proxy + ["vps"] * (a.nodes - n_proxy)
    rnd.shuffle(kinds)
    for i, kind in enumerate(kinds):
        t = rnd.choices(all_teams, weights)[0]
        num = nxt[t["id"]]
        nxt[t["id"]] += 1
        plan = rnd.choice(proxy_plans if kind == "proxy" else vps_plans) if plans else None
        machines.append({"id": uuid7(), "kind": kind, "label": None, "ip": "10.77.%d.%d" % (1 + i // 250, 1 + i % 250),
                         "plan": plan, "ref": "SIM-N%04d" % (i + 1), "team": t, "number": num, "tags": []})
    relays = [m for m in machines if m["kind"] == "relay"]
    nodes = [m for m in machines if m["kind"] != "relay"]
    vps = [m for m in nodes if m["kind"] == "vps"]
    proxies = [m for m in nodes if m["kind"] == "proxy"]

    # 故障画像
    prof = {}
    if a.faults:
        pool = vps[:]
        rnd.shuffle(pool)
        for name, n in (("cpu_high", 1), ("disk_high", 1), ("renew_soon", 1), ("flaky", 2), ("offline", 2)):
            for _ in range(n):
                if len(pool) > 3:
                    prof[pool.pop()["id"]] = name
        if proxies:
            prof[rnd.choice(proxies)["id"]] = "proxy_dead"
        if len(relays) >= 2:
            prof[relays[1]["id"]] = "renew_soon"

    # 机器
    cols = ["id", "kind", "team_id", "number", "label", "primary_ip", "ssh_port", "provider_id", "plan_id", "service_ref",
            "lifecycle", "note", "created_at", "updated_at", "activated_at"]
    rows = []
    for m in machines:
        days = rnd.randint(3, 90)
        p = m["plan"]
        rows.append((m["id"], Raw("'%s'::machine_kind" % m["kind"]), m["team"]["id"] if m["team"] else None, m["number"], m["label"],
                     Raw("'%s'::inet" % m["ip"]), 22, p["provider_id"] if p else None, p["id"] if p else None, m["ref"],
                     Raw("'active'::machine_lifecycle"), NOTE, Raw("now() - interval '%d days 2 hours'" % days),
                     Raw("now() - interval '%d days'" % days), Raw("now() - interval '%d days'" % days)))
    sql.append(insert("machine", cols, rows))

    # 线路机参数：代数先按入口数，下面算完入口再回填
    entries = []  # (id, relay, node, port, changed_gen)
    port_next = {m["id"]: 20000 for m in relays}
    gen = {m["id"]: 0 for m in relays}

    def add_entry(rl, nd):
        gen[rl["id"]] += 1
        entries.append((uuid7(), rl["id"], nd["id"], port_next[rl["id"]], gen[rl["id"]]))
        port_next[rl["id"]] += 1

    for i, nd in enumerate(nodes):
        add_entry(relays[i % len(relays)], nd)
    for nd in nodes:
        if rnd.random() < 0.25 and len(relays) > 1:
            cur = {e[1] for e in entries if e[2] == nd["id"]}
            other = [rl for rl in relays if rl["id"] not in cur]
            add_entry(rnd.choice(other), nd)
    rows = []
    for m in relays:
        pk = base64.urlsafe_b64encode(os.urandom(32)).decode().rstrip("=")
        g = gen[m["id"]]
        rows.append((m["id"], m["ip"], 20000, 29999, bytea(os.urandom(48)), pk, g, g,
                     Raw("now()"), Raw("now() - interval '30 days'")))
    sql.append(insert("relay", ["machine_id", "public_host", "port_min", "port_max", "reality_private_enc", "reality_public_key",
                                "generation", "applied_generation", "applied_at", "generation_at"], rows))
    rows = []
    for m in nodes:
        if m["kind"] == "vps":
            rows.append((m["id"], "shadowsocks", m["ip"], 8388, "aes-256-gcm", None, bytea(os.urandom(44))))
        else:
            rows.append((m["id"], "socks5", m["ip"], 1080, None, "sim", bytea(os.urandom(44))))
    sql.append(insert("node_outbound", ["machine_id", "protocol", "host", "port", "method", "username", "secret_enc"], rows))
    rows = [(eid, rl, nd, port, str(uuid.uuid4()), os.urandom(8).hex(), Raw("'active'::entry_lifecycle"), cg,
             Raw("now() - interval '%d days'" % rnd.randint(1, 60))) for eid, rl, nd, port, cg in entries]
    sql.append(insert("entry", ["id", "relay_id", "node_id", "port", "client_uuid", "short_id", "lifecycle", "changed_gen", "created_at"], rows))

    # 订阅与续费记录
    months = {"monthly": 1, "quarterly": 3, "semiannual": 6, "yearly": 12}
    srows, rrows = [], []
    for m in machines:
        p = m["plan"]
        cyc = rnd.choices(list(months), [60, 20, 10, 10])[0]
        base = p["price_cents"] if p else rnd.choice([299, 1999, 2888, 5900])
        price = int(base * months[cyc] * (0.9 if months[cyc] > 1 else 1) * rnd.choice([1, 1, 1, 1.1]))
        due = rnd.randint(5, 30 * months[cyc])
        remind, auto = 3, rnd.random() < 0.35
        if prof.get(m["id"]) == "renew_soon":
            due, remind, auto = (3 if m["kind"] == "relay" else 2), 7, False
        tz = (p["timezone"] if p and rnd.random() < 0.7 else "Asia/Shanghai")
        srows.append((m["id"], price, p["currency"] if p else "USD", Raw("'%s'::billing_cycle" % cyc), Raw("current_date + %d" % due), tz,
                      auto, remind, p["traffic_gb"] if p else None, rnd.randint(1, 28) if rnd.random() < 0.5 else None))
        if rnd.random() < 0.2:
            k = rnd.randint(2, 25)
            rrows.append((uuid7(), m["id"], Raw("((current_date - %d) - interval '%d month')::date" % (k, months[cyc])), Raw("current_date - %d" % k),
                          price, p["currency"] if p else "USD", "模拟续费", "fleet-sim", Raw("now() - interval '%d days'" % k)))
    sql.append(insert("subscription", ["machine_id", "price_cents", "currency", "cycle", "due_date", "billing_tz", "auto_renew", "remind_days",
                                       "traffic_gb", "traffic_reset_day"], srows))
    sql.append(insert("renewal", ["id", "machine_id", "old_due", "new_due", "amount_cents", "currency", "note", "actor", "created_at"], rrows))

    # 令牌（服务端存 sha256(token)，见 agent_api::authenticate）
    toks, trows = {}, []
    off_from = now_ms() - 15 * 60 * 1000
    for m in machines:
        if m["kind"] == "proxy":
            toks[m["id"]] = {"name": m["label"] or "%s-%02d" % (m["team"]["name"], m["number"]), "kind": "proxy", "profile": prof.get(m["id"], "")}
            continue
        tok = secrets.token_urlsafe(32)
        e = {"name": m["label"] or "%s-%02d" % (m["team"]["name"], m["number"]), "kind": m["kind"], "token": tok, "profile": prof.get(m["id"], "")}
        if e["profile"] == "offline":
            e["offline_from_ms"] = off_from
        toks[m["id"]] = e
        trows.append((uuid7(), m["id"], bytea(hashlib.sha256(tok.encode()).digest()), Raw("now() - interval '%d days'" % rnd.randint(1, 60))))
    sql.append(insert("agent_token", ["id", "machine_id", "token_hash", "created_at"], trows))
    sql.append("commit;\n")

    tmp = TOKENS + ".tmp"
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        json.dump(toks, f, ensure_ascii=False, indent=1)
    os.replace(tmp, TOKENS)
    try:
        psql("".join(sql))
    except SystemExit:
        os.unlink(TOKENS)
        raise
    log("已插入：线路机 %d，节点 %d（vps %d / proxy %d），入口 %d，团队（模拟）%d，令牌 %d"
        % (len(relays), len(nodes), len(vps), len(proxies), len(entries), len(sim_teams), len(trows)))
    log("故障画像：" + ", ".join("%s=%s" % (toks[k]["name"], v) for k, v in prof.items()))
    log("令牌已写入 %s" % TOKENS)
    if not a.no_backfill and a.backfill_hours > 0:
        do_backfill(a.backfill_hours, a.seed, a.workers)
    cmd_status(argparse.Namespace(api=False))


# ── 回放历史（走真实上报接口，分区/脏小时标记由服务端自己处理）────────────────

def do_backfill(hours, seed, workers):
    world, tokens = build_world(seed)
    end = now_ms() // 10000 * 10000 - 10000
    start = (end - int(hours * 3600 * 1000)) // 30000 * 30000
    world.t = start - 10000
    agents = world.agents()
    stats = Stats()
    log("回放 %.1f 小时历史：%d 台带 agent 的机器，%d 条入口" % (hours, len(agents), len(world.ents)))
    pool = concurrent.futures.ThreadPoolExecutor(workers)

    def ship(m, final):
        if m.kind != "relay" and len(m.host) < 300 and not final:
            return
        for rep in build_reports(m, start):
            m.pending.append(rep)
        drain(m, stats, backoff=False)

    t0 = time.time()
    while world.t < end:
        w_end = min(end, world.t + 20 * 60 * 1000)
        world.advance_to(w_end)
        final = world.t >= end
        list(pool.map(lambda m: ship(m, final), agents))
        if sum(1 for m in agents if m.pending) > len(agents) // 2:
            die("回放时多数机器发送失败（服务端不可达或在报错），中止；已写入的部分保留，purge 可清理")
        done = (world.t - start) / max(1, end - start)
        log("  回放进度 %3d%%  成功 %d 失败 %d" % (done * 100, stats.n_ok, stats.n_err))
    pool.shutdown()
    # 离线画像的机器：回放结束时 last_report_at 被设成了此刻，调回 15 分钟前，让它们一开始就是离线
    off = [k for k, v in tokens.items() if v.get("profile") == "offline"]
    if off:
        psql("update agent_state set last_report_at = now() - interval '15 minutes' where machine_id in (%s);" % ",".join("'%s'" % x for x in off))
    o, e, avg, p95 = stats.take()
    log("回放完成：%d 份报告成功，%d 失败（平均 %.0f ms，p95 %.0f ms），用时 %.0f 秒" % (o, e, avg, p95, time.time() - t0))


def cmd_backfill(a):
    do_backfill(a.hours, a.seed, a.workers)


# ── run ───────────────────────────────────────────────────────────────────────

def refresh(world, pool, stats):
    """同步库里的入口集合；线路机期望代数领先于已应用代数时回报 applied（模拟 agent 把配置应用上了）。"""
    try:
        ents = query(SQL_ENTRIES)
        rel = {x["id"]: x for x in query(SQL_RELAYS)}
    except SystemExit:
        return
    world.sync_entries(ents)
    for rid, x in rel.items():
        m = world.mach.get(rid)
        if not m or not m.token:
            continue
        m.applied_gen = x["applied_generation"]
        if x["generation"] > x["applied_generation"]:
            rep = {"generation": x["generation"], "ok": True,
                   "entries": [{"id": e.id, "ok": True} for e in world.ents.values() if e.relay_id == rid]}
            m.applied_gen = x["generation"]

            def send(m=m, rep=rep):
                st, text = http_post("/agent/v1/applied", rep, m.token, gz=False)
                log("%s 回报 applied generation=%d -> HTTP %s %s" % (m.name, rep["generation"], st, "" if st == 204 else text[:120]))
            pool.submit(send)


def cmd_run(a):
    if a.interval > 90:
        log("警告：interval=%d 超过 90 秒，服务端会把 120 秒没上报的机器判离线" % a.interval)
    world, tokens = build_world(a.seed, restore=True)
    ago = world.last_ago
    # 从最后一条样本接着走（最多补 30 分钟），期间的样本在头几个周期里补发
    if ago is not None and ago < 1800:
        world.t = (now_ms() - int(ago * 1000)) // 10000 * 10000
    agents = [m for m in world.agents() if m.active(now_ms())]
    if not agents:
        die("没有可上报的机器")
    stats = Stats()
    pool = concurrent.futures.ThreadPoolExecutor(8)
    log("开始模拟：%d 台机器上报（%d 台离线不发），%d 条入口，周期 %d 秒，目标 %s" %
        (len(agents), len(world.agents()) - len(agents), len(world.ents), a.interval, BASE))
    refresh(world, pool, stats)
    start = time.time()
    # 服务端的「持续高负载」要求最近 5 分钟的样本跨度 >= 270 秒；60 秒一批上报时新样本最多晚 60 秒到，条件时有时无。
    # 所以 cpu_high / disk_high 这两台按真 agent 的 10 秒节奏上报（其余按 --interval）。
    ivl = {m.id: (min(10, a.interval) if m.profile in ("cpu_high", "disk_high") else a.interval) for m in agents}
    due = {m.id: start + m.offset * ivl[m.id] for m in agents}
    next_log = start + 60
    next_refresh = start + 30
    while True:
        now = time.time()
        mid = min(due, key=due.get)
        wait = min(due[mid] - now, 1.0, max(0.0, min(next_log, next_refresh) - now))
        if wait > 0:
            time.sleep(wait)
            continue
        if now >= next_refresh:
            refresh(world, pool, stats)
            next_refresh = now + 30
        if now >= next_log:
            o, e, avg, p95 = stats.take()
            pend = sum(len(m.pending) for m in agents)
            log("周期统计：成功 %d 失败 %d 平均 %.0f ms p95 %.0f ms，积压 %d 份，入口 %d" % (o, e, avg, p95, pend, len(world.ents)))
            next_log = now + 60
        if due[mid] > now:
            continue
        m = world.mach[mid]
        world.advance_to(now_ms())
        for rep in build_reports(m, now_ms()):
            m.pending.append(rep)
        pool.submit(drain, m, stats)
        due[mid] += ivl[mid]
        if due[mid] < now - ivl[mid]:
            due[mid] = now + ivl[mid]


# ── purge ─────────────────────────────────────────────────────────────────────

# (标签, SQL)
PURGE_STEPS = [
    ("host_metric", "delete from host_metric where machine_id = any(array(select id from sim_m))"),
    ("entry_traffic", "delete from entry_traffic where entry_id = any(array(select id from sim_e))"),
    ("probe_result", "delete from probe_result where entry_id = any(array(select id from sim_e))"),
    ("access_event", "delete from access_event where entry_id = any(array(select id from sim_e))"),
    ("host_metric_hourly", "delete from host_metric_hourly where machine_id in (select id from sim_m)"),
    ("entry_traffic_hourly", "delete from entry_traffic_hourly where entry_id in (select id from sim_e)"),
    ("probe_hourly", "delete from probe_hourly where entry_id in (select id from sim_e)"),
    ("access_hourly", "delete from access_hourly where node_id in (select id from sim_m) or relay_id in (select id from sim_m)"),
    ("access_source_hourly", "delete from access_source_hourly where node_id in (select id from sim_m)"),
    ("access_target_seen", "delete from access_target_seen where node_id in (select id from sim_m)"),
    ("access_source_late", "delete from access_source_late where node_id in (select id from sim_m)"),
    ("node_source_seen", "delete from node_source_seen where node_id in (select id from sim_m)"),
    ("ip_intel_cache", "delete from ip_intel_cache where ip in (select ip from sim_ip)"),
    ("notify_log", "delete from notify_log where incident_id in (select id from incident where subject_id in (select id from sim_m union select id from sim_e))"),
    ("incident", "delete from incident where subject_id in (select id from sim_m union select id from sim_e)"),
    ("silence", "delete from silence where subject_id in (select id from sim_m union select id from sim_t)"),
    ("policy", "delete from policy where (scope = 'node' and scope_id in (select id from sim_m)) or (scope = 'team' and scope_id in (select id from sim_t))"),
    ("job(+steps/logs)", "delete from job where target_id in (select id from sim_m)"),
    ("audit_log", "delete from audit_log where target_id in (select id from sim_m union select id from sim_e union select id from sim_t)"),
    ("real_relays_bumped", "update relay set generation = generation + 1, generation_at = now() "
     "where machine_id in (select relay_id from entry where id in (select id from sim_e)) and machine_id not in (select id from sim_m)"),
    ("entry", "delete from entry where id in (select id from sim_e)"),
    ("machine(+outbound/relay/token/state/subscription/renewal)", "delete from machine where id in (select id from sim_m)"),
    ("team", "delete from team where id in (select id from sim_t) and not exists (select 1 from machine m where m.team_id = team.id)"),
]


def purge_sql():
    s = ["begin;",
         "create temp table sim_m on commit drop as select id from machine where note = 'fleet-sim';",
         "create temp table sim_e on commit drop as select id from entry where node_id in (select id from sim_m) or relay_id in (select id from sim_m);",
         "create temp table sim_t on commit drop as select id from team where name like '%s%%';" % TEAM_PREFIX]
    # 只清「仅被模拟节点用过」的来源 IP 画像缓存，要在删 access_source_hourly 之前算好
    s.append("create temp table sim_ip on commit drop as select distinct src_ip ip from access_source_hourly where node_id in (select id from sim_m) "
             "except select distinct src_ip from access_source_hourly where node_id not in (select id from sim_m);")
    s.append(r"\echo ## 清点")
    s.append("select 'machines', count(*) from sim_m union all select 'entries', count(*) from sim_e union all select 'sim_teams', count(*) from sim_t;")
    s.append(r"\echo ## 删除行数")
    for label, sql in PURGE_STEPS:
        s.append("with d as (%s returning 1) select '%s', count(*) from d;" % (sql, label))
    s.append("commit;")
    return "\n".join(s) + "\n"


def cmd_purge(a):
    active = subprocess.run(["systemctl", "is-active", "--quiet", "fleet-sim"]).returncode == 0
    if active and not a.force:
        die("fleet-sim 服务还在运行：先 stop（run-sim.sh stop），或加 --force")
    out = psql(purge_sql())
    print(out.strip())
    left = query("select (select count(*) from machine where note = '%s') m, (select count(*) from team where name like '%s%%') t" % (NOTE, TEAM_PREFIX))[0]
    if left["m"] or left["t"]:
        log("注意：仍有残留 机器 %d 团队 %d（模拟团队里若被挪进了真实机器会保留）" % (left["m"], left["t"]))
    if os.path.exists(TOKENS):
        os.unlink(TOKENS)
        log("已删除 " + TOKENS)
    log("purge 完成")


# ── status ────────────────────────────────────────────────────────────────────

def cmd_status(a):
    q = lambda s: query(s)
    sm = "select id from machine where note = '%s'" % NOTE
    se = "select id from entry where node_id in (%s) or relay_id in (%s)" % (sm, sm)
    for r in q("select kind::text kind, lifecycle::text lifecycle, count(*) n from machine where note = '%s' group by 1, 2 order by 1, 2" % NOTE):
        print("机器 %-6s %-9s %d" % (r["kind"], r["lifecycle"], r["n"]))
    for r in q("select name, (select count(*) from machine m where m.team_id = t.id and m.note = '%s') n from team t order by sort_order, name" % NOTE):
        if r["n"] or r["name"].startswith(TEAM_PREFIX):
            print("团队 %-12s 模拟节点 %d" % (r["name"], r["n"]))
    one = q("""select
      (select count(*) from entry where id in (%(se)s)) entries,
      (select count(*) from agent_token where machine_id in (%(sm)s) and revoked_at is null) tokens,
      (select count(*) from agent_state where machine_id in (%(sm)s)) reported,
      (select count(*) from agent_state where machine_id in (%(sm)s) and last_report_at > now() - interval '%(off)d seconds') online,
      (select count(*) from subscription where machine_id in (%(sm)s)) subs,
      (select count(*) from renewal where machine_id in (%(sm)s)) renewals,
      (select count(*) from incident where state = 'firing' and subject_id in (%(sm)s)) firing,
      (select count(*) from incident where subject_id in (%(sm)s)) incidents_all""" % {"sm": sm, "se": se, "off": AGENT_OFFLINE_S})[0]
    print("入口 %(entries)d  令牌 %(tokens)d  已上报过 %(reported)d  当前在线(%(off)ds内) %(online)d  订阅 %(subs)d  续费记录 %(renewals)d" % dict(one, off=AGENT_OFFLINE_S))
    print("告警：未恢复 %d，历史共 %d" % (one["firing"], one["incidents_all"]))
    for r in q("select rule, severity::text sev, count(*) n from incident where state = 'firing' and subject_id in (%s) group by 1, 2 order by 3 desc" % sm):
        print("  firing %-18s %-8s %d" % (r["rule"], r["sev"], r["n"]))
    ts = q("""select 'host_metric' t, count(*) n, min(ts) lo, max(ts) hi from host_metric where machine_id = any(array(%(sm)s))
      union all select 'entry_traffic', count(*), min(ts), max(ts) from entry_traffic where entry_id = any(array(%(se)s))
      union all select 'probe_result', count(*), min(ts), max(ts) from probe_result where entry_id = any(array(%(se)s))
      union all select 'access_event', count(*), min(ts), max(ts) from access_event where entry_id = any(array(%(se)s))""" % {"sm": sm, "se": se})
    for r in ts:
        print("时序 %-14s %9d 行  %s ~ %s" % (r["t"], r["n"], (r["lo"] or "-")[:19], (r["hi"] or "-")[:19]))
    hr = q("""select (select count(*) from host_metric_hourly where machine_id in (%(sm)s)) h, (select count(*) from entry_traffic_hourly where entry_id in (%(se)s)) t,
      (select count(*) from probe_hourly where entry_id in (%(se)s)) p, (select count(*) from access_hourly where node_id in (%(sm)s)) a,
      (select count(*) from access_target_seen where node_id in (%(sm)s)) s""" % {"sm": sm, "se": se})[0]
    print("汇总 host_metric_hourly %(h)d  entry_traffic_hourly %(t)d  probe_hourly %(p)d  access_hourly %(a)d  access_target_seen %(s)d" % hr)
    if os.path.exists(TOKENS):
        tk = json.load(open(TOKENS, encoding="utf-8"))
        cnt = collections.Counter(v.get("profile") or "normal" for v in tk.values())
        print("画像：" + ", ".join("%s=%d" % kv for kv in sorted(cnt.items())))
    if getattr(a, "api", False):
        key = os.environ.get("FLEET_API_KEY")
        if not key:
            print("--api 需要环境变量 FLEET_API_KEY")
            return
        req = urllib.request.Request(BASE + "/api/v1/machines", headers={"Authorization": "Bearer " + key})
        data = json.load(urllib.request.urlopen(req, timeout=30))["data"]
        c = collections.Counter(m["health"]["status"] for m in data if m["note"] == NOTE)
        print("界面健康状态（API）：" + ", ".join("%s=%d" % kv for kv in sorted(c.items())))


def main():
    ap = argparse.ArgumentParser(description="fleet 实验室模拟器")
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("seed")
    p.add_argument("--relays", type=int, default=6)
    p.add_argument("--nodes", type=int, default=150)
    p.add_argument("--extra-teams", type=int, default=3)
    p.add_argument("--backfill-hours", type=float, default=24)
    p.add_argument("--no-backfill", action="store_true")
    p.add_argument("--no-faults", dest="faults", action="store_false")
    p.add_argument("--seed", type=int, default=1)
    p.add_argument("--workers", type=int, default=4)
    p.set_defaults(fn=cmd_seed)
    p = sub.add_parser("backfill")
    p.add_argument("--hours", type=float, default=24)
    p.add_argument("--seed", type=int, default=1)
    p.add_argument("--workers", type=int, default=4)
    p.set_defaults(fn=cmd_backfill)
    p = sub.add_parser("run")
    p.add_argument("--interval", type=int, default=60)
    p.add_argument("--seed", type=int, default=1)
    p.set_defaults(fn=cmd_run)
    p = sub.add_parser("purge")
    p.add_argument("--force", action="store_true")
    p.set_defaults(fn=cmd_purge)
    p = sub.add_parser("status")
    p.add_argument("--api", action="store_true", help="另用 FLEET_API_KEY 调 /api/v1/machines 统计界面健康状态")
    p.set_defaults(fn=cmd_status)
    a = ap.parse_args()
    a.fn(a)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
