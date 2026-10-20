#!/usr/bin/env python3
"""Merge the anonymized ipintel.json (from ipintel_remote.py on a395) into snapshot / detail / extra.

  python3 tools/ipintel_merge.py <ipintel.json> <data_dir>

Idempotent: removes any earlier ipintel block and row fields first, then writes them again.
Existing fields are never changed; only new keys are added.
"""
import gzip
import ipaddress
import json
import os
import re
import sys

SRC_FILE, DATA = sys.argv[1:3]
I = json.load(open(SRC_FILE))
P = lambda n: os.path.join(DATA, n)
S = json.load(open(P("snapshot.json")))
D = json.load(open(P("detail.json")))
E = json.load(open(P("extra.json")))

ROW_KEYS = ("carrier", "asn", "as_org", "country", "province", "city", "conn_type", "conn_conf",
            "first_seen", "last_seen", "sessions", "sessions_h24", "src_flags")
SRC = {x["ip"]: x for x in I["sources"]}
PAIRS = {n: {p["ip"]: p for p in v["sources"]} for n, v in I["nodes"].items()}


def strip_rows(rows, keep=()):
    for r in rows or []:
        for k in ROW_KEYS:
            if k not in keep:
                r.pop(k, None)


def enrich_row(row, node=None, ip_key="source_ip"):
    """Row-level attributes: who the source is (carrier...) plus when it used this node (pair) or the fleet."""
    ip = row.get(ip_key)
    x = SRC.get(ip)
    if not x:
        return False
    p = PAIRS.get(node, {}).get(ip) if node else None
    add = {
        "carrier": x["carrier"], "asn": x["asn"], "as_org": x["as_org"], "country": x["country"],
        "province": x["province"], "city": x["city"], "conn_type": x["conn_type"], "conn_conf": x["conn_conf"],
        "first_seen": (p or x)["first_seen"], "last_seen": (p or x)["last_seen"],
        "sessions": (p or x)["sessions"]["d14"], "sessions_h24": (p or x)["sessions"]["h24"],
    }
    for k, v in add.items():
        row.setdefault(k, v)  # never overwrite a field the row already had (top_sources carry their own first/last_seen)
    return True


# ---------------------------------------------------------------- clean previous run
S.pop("ipintel", None)
for n in S["nodes"]:
    n.pop("ipintel", None)
for e in S["entries"]:
    e.pop("ipintel", None)
for rows in S["access_recent"].values():
    strip_rows(rows)
strip_rows(S["behavior_top"]["sources"], keep=("last_seen",))
for ev in S["behavior_top"]["events_recent"] + E["behavior"]["events_recent"]:
    ev.pop("subject_intel", None)
for dn in D["nodes"].values():
    dn.pop("ipintel", None)
    strip_rows(dn["access"]["recent10"])
    strip_rows(dn["access"]["top_sources"], keep=("first_seen", "last_seen"))
strip_rows(E["behavior"]["top_sources"], keep=("last_seen",))
strip_rows(E["behavior"]["timeline_recent"])

# ---------------------------------------------------------------- snapshot: fleet-level block
S["ipintel"] = {
    "schema": "team-ops-ipintel/1",
    **{k: I[k] for k in ("meta", "dict", "summary", "sources", "leases", "breakdowns", "egress_summary", "anomalies")},
}
S["provenance"]["ipintel"] = ("a395 离线库：ip2region xdb、DB-IP Lite City/ASN（CC BY 4.0，“IP Geolocation by DB-IP”）、"
                              "iptoasn ip2asn-combined；运营库 activity 全表只读扫描一次")
S["anonymization"]["ipintel"] = ("来源 IP 与出口 IP 沿用同一张假地址映射；原快照里没出现过的来源 IP 按首次出现顺序接着分配 203.0.113.x。"
                                 "运营商、ASN、AS 组织、省市、经纬度（城市中心点）、接入方式都是真实查询结果")

# ---------------------------------------------------------------- nodes / entries
for n in S["nodes"]:
    v = I["nodes"].get(n["id"])
    if not v:
        continue
    eg = dict(v["egress"]) if v.get("egress") else None
    if eg:
        h = eg["history"]
        eg["history"] = {k: h[k] for k in ("distinct_ips", "observations", "first_seen", "last_seen", "via", "changes", "stable")}
    n["ipintel"] = {"summary": v.get("summary"), "egress": eg, "breakdown": v.get("breakdown")}
by_node = {n["id"]: n for n in S["nodes"]}
for e in S["entries"]:
    v = I["nodes"].get(e.get("bound_node") or "")
    if not v or not v.get("summary"):
        continue
    sm = v["summary"]
    e["ipintel"] = {"node": e["bound_node"], "sources": sm["sources"], "new_sources_h24": sm["new_sources_h24"],
                    "top_carrier_h24": sm["top_carrier"]["h24"], "top_city_h24": sm["top_city"]["h24"],
                    "mobile_pct_h24": sm["mobile_pct"]["h24"], "flags": sm["flags"],
                    "note": "入口与绑定节点一一对应，完整分布见 nodes[].ipintel.breakdown"}

# ---------------------------------------------------------------- rows
miss = 0
for nid, rows in S["access_recent"].items():
    for r in rows:
        miss += not enrich_row(r, nid)
for r in S["behavior_top"]["sources"]:
    miss += not enrich_row(r)
for dn_id, dn in D["nodes"].items():
    for r in dn["access"]["recent10"]:
        miss += not enrich_row(r, dn_id)
    for r in dn["access"]["top_sources"]:
        miss += not enrich_row(r, dn_id)
    v = I["nodes"].get(dn_id)
    if v:
        dn["ipintel"] = {"sources": v["sources"], "egress_history": (v.get("egress") or {}).get("history")}
for r in E["behavior"]["top_sources"]:
    miss += not enrich_row(r)
for r in E["behavior"]["timeline_recent"]:
    miss += not enrich_row(r, r.get("node"))
for ev in S["behavior_top"]["events_recent"] + E["behavior"]["events_recent"]:
    if ev.get("kind") == "new_source" and ev.get("subject") in SRC:
        x = SRC[ev["subject"]]
        ev["subject_intel"] = {"carrier": x["carrier"], "province": x["province"], "city": x["city"], "country": x["country"],
                               "asn": x["asn"], "conn_type": x["conn_type"], "conn_conf": x["conn_conf"], "flags": x["flags"]}

# ---------------------------------------------------------------- leak scan + write
DOC4 = [ipaddress.ip_network("198.51.100.0/24"), ipaddress.ip_network("203.0.113.0/24")]
DOC6 = ipaddress.ip_network("2001:db8::/32")
V4 = re.compile(r"(?<![\d.])(\d{1,3}(?:\.\d{1,3}){3})(?![\d.])")
V6 = re.compile(r"(?<![0-9A-Za-z:])([0-9A-Fa-f]{0,4}(?::[0-9A-Fa-f]{0,4}){2,7})(?![0-9A-Za-z:])")


def scan(text, name):
    bad = 0
    for m in V4.finditer(text):
        try:
            a = ipaddress.ip_address(m[1])
        except ValueError:
            continue
        if not any(a in n for n in DOC4):
            bad += 1
    for m in V6.finditer(text):
        try:
            a = ipaddress.IPv6Address(m[1])
        except ValueError:
            continue
        if a not in DOC6:
            bad += 1
    if bad:
        raise SystemExit("leak scan failed in %s: %d addresses outside documentation ranges" % (name, bad))


sizes = {}
for name, obj in (("snapshot.json", S), ("detail.json", D), ("extra.json", E)):
    s = json.dumps(obj, ensure_ascii=False, separators=(",", ":"))
    scan(s, name)
    with open(P(name), "w") as f:
        f.write(s)
    b = s.encode()
    sizes[name] = {"bytes": len(b), "gzip": len(gzip.compress(b, 9))}
print(json.dumps({"sizes": sizes, "rows_without_intel": miss}, ensure_ascii=False))
