#!/usr/bin/env python3
"""IP intelligence for the prototype snapshot. Runs ON a395 only, so real IPs never leave it.

usage (on a395, inside a throwaway tmpfs dir that also holds raw.json from extract.py):
  python3 ipintel_remote.py raw.json build.py <published_dir> <ipdb_dir> > ipintel.json

1. Replays build.py on raw.json to recover the exact real -> fake IP mapping of the published files,
   and checks it against the published snapshot/detail/extra (every fake IP at every JSON path must match).
2. Looks up every connecting source IP and every egress IP in four offline databases (ipdb.py):
   ip2region xdb, DB-IP lite city, DB-IP lite ASN, iptoasn ip2asn-combined. Nothing goes online.
3. One read-only full scan of `activity` (about 2.7 M rows, ~2 s) -> per-source, per-(node, source),
   per node / team / relay breakdowns for 24 h and 14 d, lease timelines, anomalies.
4. Prints JSON keyed by FAKE IPs only, after checking that no real IP string is in it.
"""
import contextlib
import io
import ipaddress
import json
import math
import os
import re
import sqlite3
import sys
import time
from collections import Counter, defaultdict

RAW, BUILD, PUB, DBDIR = sys.argv[1:5]
T_START = time.time()
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ipdb  # noqa: E402

# ---------------------------------------------------------------------------
# 1. replay build.py -> IPMAP (real -> fake)
# ---------------------------------------------------------------------------
_argv = sys.argv
sys.argv = [BUILD, RAW, os.path.join(os.path.dirname(os.path.abspath(RAW)), "replay")]
ns = {"__name__": "__replay__", "__file__": BUILD}
with contextlib.redirect_stdout(io.StringIO()):
    exec(compile(open(BUILD, encoding="utf-8").read(), BUILD, "exec"), ns)
sys.argv = _argv
IPMAP, IP6MAP, ID, SHORT = ns["IPMAP"], ns["IP6MAP"], ns["ID"], ns["SHORT"]
raw = ns["raw"]
NOW = raw["now"]
AS_OF = int(round(NOW))
ASSETS = {a["uuid"]: a for a in ns["assets"]}
KCL = {c["uuid"]: c for c in raw["komari_clients"]}

FAKE_RE = re.compile(r"(?:198\.51\.100|203\.0\.113)\.\d{1,3}(?![\d])|2001:db8:[0-9a-f:]*[0-9a-f]")


def ip_locs(obj, path, out):
    if isinstance(obj, dict):
        for k, v in obj.items():
            for m in FAKE_RE.findall(str(k)):
                out.add((path + "/{key}", m))
            ip_locs(v, path + "/" + str(k), out)
    elif isinstance(obj, list):
        for i, v in enumerate(obj):
            ip_locs(v, "%s[%d]" % (path, i), out)
    elif isinstance(obj, str):
        for m in FAKE_RE.findall(obj):
            out.add((path, m))
    return out


verify = {}
for fn in ("snapshot.json", "detail.json", "extra.json"):
    a = ip_locs(json.load(open(os.path.join(os.path.dirname(os.path.abspath(RAW)), "replay", fn))), "", set())
    b = ip_locs(json.load(open(os.path.join(PUB, fn))), "", set())
    # the published files may already carry an ipintel block from an earlier run: compare only what build.py writes
    b = {x for x in b if "/ipintel" not in x[0]}
    verify[fn] = {"locations": len(a), "missing_in_replay": len(b - a), "extra_in_replay": len(a - b)}
if any(v["missing_in_replay"] or v["extra_in_replay"] for v in verify.values()):
    sys.stderr.write("IP mapping replay does not match the published files: %s\n" % json.dumps(verify))
    sys.exit(2)

USED = set(IPMAP.values())
POOL = ["203.0.113.%d" % i for i in range(40, 255)] + ["198.51.100.%d" % i for i in range(100, 200)]


def fake(real):
    if real is None:
        return None
    a = ipaddress.ip_address(real)
    if a.version == 6:
        return ns["map_ip"](str(a))
    if real in IPMAP:
        return IPMAP[real]
    for f in POOL:
        if f not in USED:
            USED.add(f)
            IPMAP[real] = f
            return f
    raise RuntimeError("fake IPv4 pool exhausted")


# ---------------------------------------------------------------------------
# 2. offline databases
# ---------------------------------------------------------------------------
X4 = ipdb.Xdb(os.path.join(DBDIR, "ip2region_v4.xdb"))
X6 = ipdb.Xdb(os.path.join(DBDIR, "ip2region_v6.xdb"))
CITY_F = sorted(f for f in os.listdir(DBDIR) if f.startswith("dbip-city-lite") and f.endswith(".mmdb"))[-1]
ASN_F = sorted(f for f in os.listdir(DBDIR) if f.startswith("dbip-asn-lite") and f.endswith(".mmdb"))[-1]
CITY = ipdb.Mmdb(os.path.join(DBDIR, CITY_F))
ASNDB = ipdb.Mmdb(os.path.join(DBDIR, ASN_F))
I2A_F = "ip2asn-combined.tsv.gz"
I2A = ipdb.Ip2Asn(os.path.join(DBDIR, I2A_F))
I2A_MTIME = int(os.path.getmtime(os.path.join(DBDIR, I2A_F)))

ZH = {
    "United States": "美国", "Singapore": "新加坡", "Mexico": "墨西哥", "China": "中国", "Canada": "加拿大", "Japan": "日本",
    "Hong Kong": "中国香港", "Taiwan": "中国台湾", "Australia": "澳大利亚", "Germany": "德国", "United Kingdom": "英国",
    "Texas": "得克萨斯州", "California": "加利福尼亚州", "Georgia": "佐治亚州", "Missouri": "密苏里州", "Illinois": "伊利诺伊州",
    "New York": "纽约州", "Washington, D.C.": "华盛顿特区", "District of Columbia": "华盛顿特区", "Florida": "佛罗里达州",
    "Virginia": "弗吉尼亚州", "New Jersey": "新泽西州", "Nuevo León": "新莱昂州", "Guerrero": "格雷罗州",
    "Irvine": "尔湾", "Fremont": "弗里蒙特", "Los Angeles": "洛杉矶", "Pacifica": "帕西菲卡", "Ciudad Apodaca": "阿波达卡",
    "San Jose": "圣何塞", "Dallas": "达拉斯", "Atlanta": "亚特兰大", "Chicago": "芝加哥", "Kangkar": "康加",
}
PROV_PINYIN = {
    "Beijing": "北京市", "Tianjin": "天津市", "Shanghai": "上海市", "Chongqing": "重庆市", "Hebei": "河北省", "Shanxi": "山西省",
    "Liaoning": "辽宁省", "Jilin": "吉林省", "Heilongjiang": "黑龙江省", "Jiangsu": "江苏省", "Zhejiang": "浙江省", "Anhui": "安徽省",
    "Fujian": "福建省", "Jiangxi": "江西省", "Shandong": "山东省", "Henan": "河南省", "Hubei": "湖北省", "Hunan": "湖南省",
    "Guangdong": "广东省", "Hainan": "海南省", "Sichuan": "四川省", "Guizhou": "贵州省", "Yunnan": "云南省", "Shaanxi": "陕西省",
    "Gansu": "甘肃省", "Qinghai": "青海省", "Inner Mongolia": "内蒙古", "Guangxi": "广西", "Tibet": "西藏", "Ningxia": "宁夏",
    "Xinjiang": "新疆", "Hong Kong": "香港特别行政区", "Macao": "澳门特别行政区", "Taiwan": "台湾省",
}
# DB-IP lite names CN places by city or by district/street; map the ones that occur to prefecture cities
CITY_PINYIN = {
    "Zhengzhou": "郑州市", "Guancheng": "郑州市", "Kaifeng": "开封市", "Wuhan": "武汉市", "Shizishan": "武汉市", "Huashan": "武汉市",
    "Jingmen": "荆门市", "Ezhou": "鄂州市", "Huanggang": "黄冈市", "Beijing": "北京市", "Jinrongjie": "北京市", "Shanghai": "上海市",
    "Guangzhou": "广州市", "Shenzhen": "深圳市", "Hangzhou": "杭州市", "Jinan": "济南市",
}
# prefecture-city / state centroids for the places that occur (public gazetteer values, WGS84)
GAZ = {
    "中国|河南省|郑州市": (34.7466, 113.6254), "中国|河南省|开封市": (34.7972, 114.3076), "中国|湖北省|武汉市": (30.5928, 114.3055),
    "中国|湖北省|荆门市": (31.0354, 112.1994), "中国|湖北省|鄂州市": (30.3913, 114.8949), "中国|湖北省|黄冈市": (30.4539, 114.8724),
    "中国|北京市|北京市": (39.9042, 116.4074), "中国|河南省": (33.8820, 113.6140), "中国|湖北省": (30.9756, 112.2707),
    "新加坡": (1.3521, 103.8198), "美国|得克萨斯州": (31.0545, -97.5635), "美国|加利福尼亚州": (36.7783, -119.4179),
    "美国|佐治亚州": (32.1656, -82.9001), "美国|密苏里州": (37.9643, -91.8318), "美国|伊利诺伊州": (40.6331, -89.3985),
    "美国|纽约州": (42.1657, -74.9481), "美国|华盛顿特区": (38.9072, -77.0369), "墨西哥|新莱昂州": (25.5922, -99.9962),
    "美国|加利福尼亚州|尔湾": (33.6846, -117.8265), "美国|加利福尼亚州|帕西菲卡": (37.6138, -122.4869),
    "墨西哥|新莱昂州|阿波达卡": (25.7817, -100.1886),
}

CN_CARRIER = {"电信": "中国电信", "联通": "中国联通", "移动": "中国移动", "广电": "中国广电", "铁通": "中国铁通", "教育网": "中国教育网"}
ASN_CARRIER = {
    4134: "中国电信", 4809: "中国电信", 4812: "中国电信", 4811: "中国电信", 23724: "中国电信", 4816: "中国电信",
    137266: "中国电信", 131285: "中国电信", 4837: "中国联通", 4808: "中国联通", 9929: "中国联通", 17621: "中国联通",
    17622: "中国联通", 17623: "中国联通", 17816: "中国联通", 9808: "中国移动", 24445: "中国移动", 24400: "中国移动",
    24444: "中国移动", 24547: "中国移动", 38019: "中国移动", 56040: "中国移动", 56041: "中国移动", 56042: "中国移动",
    56044: "中国移动", 56046: "中国移动", 56047: "中国移动", 56048: "中国移动", 58453: "中国移动", 4538: "中国教育网",
    7018: "AT&T", 20057: "AT&T", 8151: "Telmex", 979: "NetLab Global", 55430: "StarHub", 174: "Cogent",
}
DATACENTER_ASN = {16509: "Amazon", 14618: "Amazon", 8987: "Amazon", 15169: "Google", 396982: "Google Cloud", 8075: "Microsoft",
                  13335: "Cloudflare", 37963: "阿里云", 45102: "阿里云", 45090: "腾讯云", 132203: "腾讯云", 55990: "华为云",
                  136907: "华为云", 38365: "百度云", 31898: "Oracle Cloud", 14061: "DigitalOcean", 20473: "Vultr", 63949: "Linode",
                  16276: "OVH", 24940: "Hetzner", 51167: "Contabo", 12876: "Scaleway", 36351: "IBM Cloud", 54113: "Fastly"}
HOSTING_ASN = {979: "NetLab Global", 21859: "Zenlayer", 25820: "IT7", 35916: "Multacom", 906: "DMIT", 36352: "ColoCrossing",
               40676: "Psychz", 8100: "QuadraNet", 46844: "Sharktech", 3258: "xTom", 9009: "M247", 60781: "Leaseweb",
               28753: "Leaseweb", 23724: "中国电信 IDC", 4816: "中国电信 IDC"}
MOBILE_ASN = {21928: "T-Mobile US", 6167: "Verizon Wireless", 22394: "Verizon Wireless", 20057: "AT&T Mobility", 17547: "M1",
              4773: "Singtel Mobile", 10091: "StarHub Mobile", 28403: "Telcel"}
# consumer ISPs whose ASN mixes fixed broadband and mobile data (no per-prefix access-type in any lite DB)
CN_MIXED_ASN = {4134, 4812, 4837, 4808, 17621, 17622, 17623, 17816, 9808, 24445, 24400, 24444, 24547, 38019,
                56040, 56041, 56042, 56044, 56046, 56047, 56048}
FIXED_ASN = {7018: "AT&T 固网", 7922: "Comcast", 20115: "Charter", 11427: "Charter", 20001: "Charter", 10796: "Charter",
             11351: "Charter", 12271: "Charter", 33363: "Charter", 701: "Verizon 固网", 22773: "Cox", 209: "Lumen", 5650: "Frontier",
             7029: "Windstream", 6128: "Optimum", 30036: "Mediacom", 11492: "Cable One", 8151: "Telmex 固网", 28548: "Megacable",
             17072: "Totalplay", 11888: "izzi", 55430: "StarHub 固网（NGNBN）", 9506: "Singtel 固网", 7473: "Singtel"}
MOBILE_KW = re.compile(r"\b5G\b|\b4G\b|\bLTE\b|MOBILE[- ]NETWORK|WIRELESS|CELLULAR|GPRS|MOBILITY", re.I)
HOSTING_KW = re.compile(r"HOSTING|\bHOST\b|SERVER|CLOUD|DATA ?CENT(?:ER|RE)|\bIDC\b|\bVPS\b|COLO|DEDICATED|-SDN\b", re.I)
CT_WORD = {"mobile": "移动网络", "broadband": "固定宽带", "datacenter": "云机房", "hosting": "IDC", "education": "教育科研网", "private": "内网地址", "unknown": "未知"}
EDU_KW = re.compile(r"研究所|大学|学院|教育网|CERNET|CSTNET|ACADEMY OF SCIENCES", re.I)


def z(v):
    return None if v in (None, "", "0", 0) else v


def zh(v):
    return ZH.get(v, v) if v else v


def short_org(org):
    if not org:
        return None
    s = re.sub(r",?\s*(Inc\.?|LLC|Ltd\.?|Co\.,? ?Ltd\.?|S\.A\. de C\.V\.|Pte Ltd|Corporation|Corp\.?)$", "", org).strip(" ,")
    return s or org


def lookup(real):
    a = ipaddress.ip_address(real)
    r = (X4 if a.version == 4 else X6).search(str(a)) or ""
    p = (r.split("|") + [""] * 5)[:5]
    if p[0] == "Reserved":
        p = ["", "", "", "", ""]
    i2r = {"country": z(p[4]), "country_name": z(p[0]), "region": z(p[1]), "city": z(p[2]), "isp": z(p[3]), "raw": r or None}
    c = CITY.lookup(str(a)) or {}
    sub = (c.get("subdivisions") or [{}])[0].get("names", {})
    loc = c.get("location", {})
    dbip = {"country": c.get("country", {}).get("iso_code"), "country_name": c.get("country", {}).get("names", {}).get("zh-CN"),
            "region": sub.get("en"), "city": c.get("city", {}).get("names", {}).get("en"),
            "lat": loc.get("latitude"), "lon": loc.get("longitude")}
    asn = ASNDB.lookup(str(a)) or {}
    ia = I2A.lookup(str(a)) or {}
    return {"v": a.version, "ip2region": i2r, "dbip": dbip,
            "dbip_asn": {"asn": asn.get("autonomous_system_number"), "org": asn.get("autonomous_system_organization")},
            "iptoasn": {"asn": ia.get("asn"), "country": ia.get("country"), "desc": ia.get("as_desc")}}


def city_key(dbip_city):
    return dbip_city.split(" (")[0].strip() if dbip_city else None


def enrich(real, internal=None):
    L = lookup(real)
    i2r, dbip, da, ia = L["ip2region"], L["dbip"], L["dbip_asn"], L["iptoasn"]
    asn = da["asn"] or ia["asn"]
    as_org = da["org"]
    as_handle = (ia["desc"] or "").split(" ")[0] or None
    country = i2r["country"] or dbip["country"] or ia["country"]
    isp_raw = i2r["isp"]
    flags = []
    # carrier: ip2region ISP for the big CN carriers, else by ASN, else short AS org
    if isp_raw in CN_CARRIER:
        carrier, basis = CN_CARRIER[isp_raw], "ip2region"
        if asn in ASN_CARRIER and ASN_CARRIER[asn] != carrier:
            flags.append("db_conflict")
    elif asn in ASN_CARRIER:
        carrier, basis = ASN_CARRIER[asn], "asn"
        if isp_raw and country == "CN":
            flags.append("db_conflict")  # e.g. ip2region names an institute, BGP origin is a carrier
    elif not ipaddress.ip_address(real).is_global:
        carrier, basis = "内网地址", "reserved"
    else:
        carrier, basis = short_org(as_org) or short_org(isp_raw) or (as_handle or "未知"), "as_org"
    # primary place: ip2region for China (carrier-grade CN data), DB-IP elsewhere (finer, with coordinates);
    # never mix a region from one database with a city from the other
    if country == "CN":
        province, city = i2r["region"], i2r["city"]
        country_name, geo_basis = "中国", "ip2region"
    else:
        country_name = dbip["country_name"] or zh(i2r["country_name"]) or country
        if dbip["country"] == country and dbip["region"]:
            province, city, geo_basis = zh(dbip["region"]), zh(city_key(dbip["city"])), "dbip"
        else:
            province, city, geo_basis = zh(i2r["region"]), zh(i2r["city"]), "ip2region"
    # agreement between the two geo databases
    agree = {"country": None, "province": None, "city": None, "asn": None}
    if i2r["country"] and dbip["country"]:
        agree["country"] = i2r["country"] == dbip["country"]
    if country == "CN":
        dp = PROV_PINYIN.get(dbip["region"] or "")
        if dp and i2r["region"]:
            agree["province"] = dp == i2r["region"]
        dc = CITY_PINYIN.get(city_key(dbip["city"]) or "")
        if dc and i2r["city"]:
            agree["city"] = dc == i2r["city"]
    else:
        if i2r["region"] and dbip["region"]:
            agree["province"] = i2r["region"].replace("Washington, D.C.", "District of Columbia") == dbip["region"]
        if i2r["city"] and dbip["city"]:
            agree["city"] = i2r["city"] == city_key(dbip["city"])
    if da["asn"] and ia["asn"]:
        agree["asn"] = da["asn"] == ia["asn"]
    if agree["province"] is False or agree["city"] is False or agree["asn"] is False:
        flags.append("geo_conflict" if (agree["province"] is False or agree["city"] is False) else "asn_conflict")
    names = " ".join(x for x in (as_org, ia["desc"], isp_raw) if x)
    # connection type
    addr = ipaddress.ip_address(real)
    if not addr.is_global:
        ct, conf, why = "private", "high", "%s地址，不在公网路由表里，任何库都查不到归属" % (
            "唯一本地（ULA）" if addr.version == 6 and addr in ipaddress.ip_network("fc00::/7") else "保留 / 内网")
        flags.append("private")
    elif internal:
        ct, conf, why = "hosting", "high", "与线路机 %s 的地址相同（%s，IDC）" % (internal, HOSTING_ASN.get(asn) or as_org)
    elif asn in DATACENTER_ASN:
        ct, conf, why = "datacenter", "high", "AS%s 属于云厂商 %s" % (asn, DATACENTER_ASN[asn])
    elif asn in HOSTING_ASN:
        ct, conf, why = "hosting", "high", "AS%s %s 是 IDC / 主机托管网络" % (asn, HOSTING_ASN[asn])
    elif asn in MOBILE_ASN:
        ct, conf, why = "mobile", "high", "AS%s 是移动运营商 %s 的蜂窝网络" % (asn, MOBILE_ASN[asn])
    elif MOBILE_KW.search(names):
        ct, conf, why = "mobile", "high", "ASN 名称标明移动网络：%s" % (ia["desc"] or as_org)
    elif HOSTING_KW.search(names):
        ct, conf, why = "hosting", "medium", "ASN 名称含机房关键词：%s" % (ia["desc"] or as_org)
    elif EDU_KW.search(names) and asn not in CN_MIXED_ASN:
        ct, conf, why = "education", "medium", "教育 / 科研网：%s" % (isp_raw or as_org)
    elif asn in CN_MIXED_ASN:
        ct, conf, why = "broadband", "low", "AS%s（%s）同时承载家宽和手机流量，库里没有接入方式，暂按固定宽带计" % (asn, carrier)
    elif asn in FIXED_ASN:
        ct, conf, why = "broadband", "medium", "AS%s 是固网运营商（%s），不在机房 ASN 列表" % (asn, FIXED_ASN[asn])
    else:
        ct, conf, why = "unknown", "low", "ASN 不在任何分类列表"
    if ct == "broadband" and "db_conflict" in flags and EDU_KW.search(isp_raw or ""):
        why += "；ip2region 标为“%s”，与 BGP 宣告方不一致" % isp_raw
    if country and country != "CN" and not internal and ct not in ("hosting", "datacenter"):
        flags.append("foreign")
    if ct in ("hosting", "datacenter"):
        flags.append("datacenter")
    if internal:
        flags.append("internal")
    gz = None
    if country == "CN":
        gz = GAZ.get("中国|%s|%s" % (province, city)) or GAZ.get("中国|%s" % province)
    elif geo_basis == "ip2region" or dbip["lat"] is None:
        gz = GAZ.get("%s|%s|%s" % (country_name, province, city)) or GAZ.get("%s|%s" % (country_name, province)) or GAZ.get(country_name or "")
    return {
        "v": L["v"], "carrier": carrier, "carrier_basis": basis, "isp_ip2region": isp_raw,
        "asn": asn, "as_org": as_org, "as_handle": as_handle, "as_desc": ia["desc"],
        "country": country, "country_name": country_name, "province": province, "city": city, "geo_basis": geo_basis,
        "lat": gz[0] if gz else dbip["lat"], "lon": gz[1] if gz else dbip["lon"],
        "coord_basis": "gazetteer" if gz else ("dbip" if dbip["lat"] is not None else None),
        "conn_type": ct, "conn_conf": conf, "conn_basis": why, "conn_alt": "mobile" if (asn in CN_MIXED_ASN and ct == "broadband") else None,
        "dbs": {"ip2region": {k: i2r[k] for k in ("country", "region", "city", "isp", "raw")},
                "dbip_city": dbip, "dbip_asn": da, "iptoasn": ia},
        "agree": agree, "flags": sorted(set(flags)), "internal": internal,
    }


# ---------------------------------------------------------------------------
# 3. activity scan (read-only, NOT INDEXED = one sequential pass)
# ---------------------------------------------------------------------------
o = sqlite3.connect("file:" + os.path.expanduser("~/team-ops-eval/teamops/db/operations-backup-20260923.db") + "?mode=ro&immutable=1", uri=True)
H24 = NOW - 86400
D14 = NOW - 14 * 86400
M24 = int(H24 // 60)
HH0 = int(NOW // 3600) * 3600 - 23 * 3600
DAY_NOW = int((NOW + 28800) // 86400)
DAYS = [DAY_NOW - 13 + i for i in range(14)]
GAP_MIN = 30

PAIR = {}
t_scan = time.time()
nrows = 0
for asset, src, client, relay, team, action, ts in o.execute(
        "SELECT asset,source_ip,client,relay,team,action,ts FROM activity NOT INDEXED WHERE ts<=? AND ts>?", (NOW, D14)):
    nrows += 1
    k = (asset, src)
    P = PAIR.get(k)
    if P is None:
        P = PAIR[k] = {"c14": 0, "c24": 0, "min": set(), "day": Counter(), "hour": [0] * 24, "clients": Counter(),
                       "relay": Counter(), "team": Counter(), "blocked": 0, "first": ts, "last": ts}
    P["c14"] += 1
    m = int(ts // 60)
    P["min"].add(m)
    P["day"][int((ts + 28800) // 86400)] += 1
    if ts > H24:
        P["c24"] += 1
    h = int((ts - HH0) // 3600)
    if 0 <= h < 24:
        P["hour"][h] += 1
    P["clients"][client or ""] += 1
    P["relay"][relay or ""] += 1
    P["team"][team or ""] += 1
    if action == "blocked":
        P["blocked"] += 1
    if ts < P["first"]:
        P["first"] = ts
    if ts > P["last"]:
        P["last"] = ts
t_scan = time.time() - t_scan
ACT_MIN_TS = o.execute("SELECT min(ts) FROM activity").fetchone()[0]
SEEN = {(a, s): (f, l, c) for a, s, f, l, c in o.execute("SELECT asset,source_ip,first_seen,last_seen,connections FROM sources_seen")}
NEW_SRC_EV = defaultdict(list)
for eid, asset, subj, ts, msg, ack in o.execute("SELECT id,asset,subject,ts,message,ack_at FROM behavior_events WHERE kind='new_source'"):
    NEW_SRC_EV[(asset, subj)].append({"event_id": eid, "ts": int(ts), "message": msg, "acknowledged": ack is not None})


def sessions(mins):
    if not mins:
        return 0
    s = sorted(mins)
    n = 1
    for a, b in zip(s, s[1:]):
        if b - a > GAP_MIN:
            n += 1
    return n


def asset_ref(u):
    if u in ID:
        return ID[u]
    m = re.fullmatch(r"direct:(线路机\d+):(\d+)", u or "")
    return "direct:%s:%s" % (m[1], m[2]) if m else "unknown:" + (u or "")[:8]


def asset_team(u):
    a = ASSETS.get(u)
    return a["team"] if a else "未分配直连"


def asset_relay(u, P=None):
    a = ASSETS.get(u)
    if a:
        return a.get("relay") or (SHORT[u] if a["team"] == "线路机" else None)
    m = re.fullmatch(r"direct:(线路机\d+):\d+", u or "")
    if m:
        return m[1]
    return (P["relay"].most_common(1)[0][0] if P else None) or None


RELAY_REAL = {}
for u, a in ASSETS.items():
    if a["team"] == "线路机":
        k = KCL.get(u, {})
        for ip in (a.get("ip"), k.get("ipv4"), k.get("ipv6")):
            if ip:
                RELAY_REAL[ip] = ID[u]

# ---------------------------------------------------------------------------
# 4. per source (fleet level)
# ---------------------------------------------------------------------------
SRC = defaultdict(lambda: {"pairs": []})
for (u, s), P in PAIR.items():
    SRC[s]["pairs"].append(u)
# stable order for new fake allocations: first appearance, then address
src_order = sorted(SRC, key=lambda s: (min(PAIR[(u, s)]["first"] for u in SRC[s]["pairs"]), int(ipaddress.ip_address(s))))
EN = {}
for s in src_order:
    EN[s] = enrich(s, RELAY_REAL.get(s))
    fake(s)


def pct(a, b):
    return round(100.0 * a / b, 2) if b else None


sources_out = []
for s in src_order:
    us = SRC[s]["pairs"]
    mins14 = set().union(*(PAIR[(u, s)]["min"] for u in us))
    mins24 = {m for m in mins14 if m >= M24}
    c14 = sum(PAIR[(u, s)]["c14"] for u in us)
    c24 = sum(PAIR[(u, s)]["c24"] for u in us)
    day = Counter()
    hour = [0] * 24
    clients = Counter()
    for u in us:
        day.update(PAIR[(u, s)]["day"])
        for i, v in enumerate(PAIR[(u, s)]["hour"]):
            hour[i] += v
        clients.update(PAIR[(u, s)]["clients"])
    seen = [SEEN.get((u, s)) for u in us if SEEN.get((u, s))]
    first = min([x[0] for x in seen] + [PAIR[(u, s)]["first"] for u in us])
    last = max([x[1] for x in seen] + [PAIR[(u, s)]["last"] for u in us])
    e = EN[s]
    SRC[s].update({"c14": c14, "c24": c24, "first": first, "last": last, "min14": mins14, "min24": mins24})
    sources_out.append({
        "ip": fake(s), **{k: e[k] for k in ("v", "carrier", "carrier_basis", "isp_ip2region", "asn", "as_org", "as_handle", "as_desc",
                                             "country", "country_name", "province", "city", "geo_basis", "lat", "lon", "coord_basis",
                                             "conn_type", "conn_conf", "conn_basis", "conn_alt", "agree", "flags", "internal", "dbs")},
        "first_seen": int(first), "last_seen": int(last), "span_s": int(last - first),
        "connections": {"h24": c24, "d14": c14},
        "sessions": {"h24": sessions(mins24), "d14": sessions(mins14)},
        "active_min": {"h24": len(mins24), "d14": len(mins14)},
        "nodes": {"h24": sum(1 for u in us if PAIR[(u, s)]["c24"]), "d14": len(us)},
        "node_ids": sorted({asset_ref(u) for u in us}),
        "teams": sorted({asset_team(u) for u in us}),
        "relays": sorted({asset_relay(u, PAIR[(u, s)]) or "" for u in us} - {""}),
        "clients": {"d14": len([c for c in clients if c])},
        "blocked_d14": sum(PAIR[(u, s)]["blocked"] for u in us),
        "daily14": [day.get(d, 0) if d >= int((ACT_MIN_TS + 28800) // 86400) else None for d in DAYS],
        "hourly24": hour,
        "new_in_h24": first > H24,
    })
SRC_BY_FAKE = {x["ip"]: x for x in sources_out}

# lease timelines: sources grouped by carrier + place, ordered by first appearance (the "same user, new address" view)
clusters = defaultdict(list)
for s in src_order:
    e = EN[s]
    if e["internal"]:
        continue
    clusters[(e["carrier"], e["country"], e["province"], e["city"], e["conn_type"])].append(s)
leases = []
for key, ss in sorted(clusters.items(), key=lambda kv: -sum(SRC[s]["c14"] for s in kv[1])):
    rows = sorted(({"ip": fake(s), "first_seen": int(SRC[s]["first"]), "last_seen": int(SRC[s]["last"]),
                    "connections_d14": SRC[s]["c14"], "nodes_d14": len(SRC[s]["pairs"]), "_nodes": set(SRC[s]["pairs"])} for s in ss),
                  key=lambda r: r["first_seen"])
    # handovers: a busy address stops and another busy address of the same carrier + city starts within 3 h
    # on (mostly) the same nodes -> the same user got a new address. Several users can share a city, so match
    # on node overlap, not on list order.
    hand = []
    for a in rows:
        if a["connections_d14"] < 1000 or a["last_seen"] > NOW - 600:
            continue
        cands = [b for b in rows if b is not a and b["connections_d14"] >= 1000 and b["first_seen"] > a["first_seen"]
                 and abs(b["first_seen"] - a["last_seen"]) <= 3 * 3600 and a["_nodes"] & b["_nodes"]]
        if cands:
            b = max(cands, key=lambda b: (len(a["_nodes"] & b["_nodes"]) / len(a["_nodes"] | b["_nodes"]), -abs(b["first_seen"] - a["last_seen"])))
            hand.append({"from": a["ip"], "to": b["ip"], "at": b["first_seen"], "gap_s": b["first_seen"] - a["last_seen"],
                         "lease_s": a["last_seen"] - a["first_seen"], "shared_nodes": len(a["_nodes"] & b["_nodes"])})
    hand.sort(key=lambda h: h["at"])
    for r in rows:
        r.pop("_nodes")
    leases.append({"carrier": key[0], "country": key[1], "province": key[2], "city": key[3], "conn_type": key[4],
                   "sources": len(rows), "connections_d14": sum(r["connections_d14"] for r in rows), "rows": rows, "handovers": hand})
# lease pattern raises confidence for mixed-ASN "broadband" guesses (multi-day address, then same-city handover = PPPoE redial)
REV = {fake(s): s for s in src_order}
HANDOVER_FROM, HANDOVER_TO = {}, {}
for L in leases:
    for h in L["handovers"]:
        HANDOVER_FROM.setdefault(h["from"], h)
        HANDOVER_TO.setdefault(h["to"], h)
for x in sources_out:
    hf, ht = HANDOVER_FROM.get(x["ip"]), HANDOVER_TO.get(x["ip"])
    if x["conn_type"] == "broadband" and x["conn_conf"] == "low" and (hf or ht) and x["span_s"] >= 12 * 3600:
        x["conn_conf"] = "medium"
        bits = []
        if ht:
            bits.append("接替同城同运营商上一个用了 %.1f 天的地址（相隔 %d 秒）" % (ht["lease_s"] / 86400, ht["gap_s"]))
        if hf:
            bits.append("用了 %.1f 天后被同城新地址接替（相隔 %d 秒）" % (hf["lease_s"] / 86400, hf["gap_s"]))
        x["conn_basis"] += "；" + "，".join(bits) + "，像家宽 PPPoE 重拨，不像手机网络频繁换址"
        x["conn_basis_signal"] = "lease_handover"
        x["conn_alt"] = None
        EN[REV[x["ip"]]].update({k: x[k] for k in ("conn_conf", "conn_basis", "conn_alt")})

# ---------------------------------------------------------------------------
# 5. breakdowns (fleet / team / relay / node) for 24 h and 14 d
# ---------------------------------------------------------------------------
DIMS = ("carrier", "province", "city", "asn", "conn_type", "country")


def dim_key(sx, dim):
    if dim == "province":
        return (sx["province"] if sx["country"] == "CN" else sx["country_name"]) or "未知"
    if dim == "city":
        if sx["country"] == "CN":
            return "%s·%s" % (sx["province"] or "?", sx["city"] or "?")
        return "%s·%s" % (sx["country_name"] or "?", sx["city"] or sx["province"] or "?")
    return sx[dim] if sx[dim] is not None else "未知"


def breakdown(pairs, dims=DIMS, coords=True):
    out = {}
    for win in ("h24", "d14"):
        per_src = {}
        for (u, s) in pairs:
            P = PAIR[(u, s)]
            c = P["c24"] if win == "h24" else P["c14"]
            if not c:
                continue
            m = {x for x in P["min"] if x >= M24} if win == "h24" else P["min"]
            ps = per_src.setdefault(s, [0, set()])
            ps[0] += c
            ps[1] |= m
        tot_c = sum(v[0] for v in per_src.values())
        res = {"total": {"connections": tot_c, "sources": len(per_src),
                         "sessions": sum(sessions(v[1]) for v in per_src.values()),
                         "active_min": sum(len(v[1]) for v in per_src.values())}}
        for dim in dims:
            acc = {}
            for s, (c, m) in per_src.items():
                sx = SRC_BY_FAKE[fake(s)]
                k = dim_key(sx, dim)
                a = acc.setdefault(k, {"k": k, "c": 0, "s": 0, "ss": 0, "m": 0})
                a["c"] += c
                a["s"] += 1
                a["ss"] += sessions(m)
                a["m"] += len(m)
                if dim == "asn":
                    a["label"] = "AS%s %s" % (sx["asn"], sx["carrier"])
                if dim == "city" and coords:
                    a["lat"], a["lon"] = sx["lat"], sx["lon"]
            rows = sorted(acc.values(), key=lambda r: (-r["c"], str(r["k"])))
            for r in rows:
                r["pct"] = pct(r["c"], tot_c)
            res[dim] = rows
        out[win] = res
    return out


pairs_all = list(PAIR)
by_team = defaultdict(list)
by_relay = defaultdict(list)
by_node = defaultdict(list)
for (u, s) in pairs_all:
    by_team[asset_team(u)].append((u, s))
    by_relay[asset_relay(u, PAIR[(u, s)]) or "未知"].append((u, s))
    by_node[asset_ref(u)].append((u, s))
breakdowns = {
    "fleet": breakdown(pairs_all),
    "teams": {t: breakdown(v) for t, v in sorted(by_team.items())},
    "relays": {r: breakdown(v) for r, v in sorted(by_relay.items())},
    "direct_history": {n: breakdown(v) for n, v in sorted(by_node.items()) if n.startswith("direct:")},
}
# every asset gets a breakdown, empty when it had no traffic in 14 d; node level drops the country dim and
# city coordinates (both are on the source records) to keep the first-paint snapshot small
node_bd = {ID[u]: breakdown(by_node.get(ID[u], []), dims=("carrier", "province", "city", "asn", "conn_type"), coords=False) for u in ASSETS}

# per (node, source) rows
pairs_out = defaultdict(list)
for (u, s), P in PAIR.items():
    n = asset_ref(u)
    seen = SEEN.get((u, s))
    m24 = {x for x in P["min"] if x >= M24}
    cl = P["clients"].copy()
    cl.pop("", None)
    pairs_out[n].append({
        "ip": fake(s),
        "first_seen": int(min(seen[0], P["first"]) if seen else P["first"]),
        "last_seen": int(max(seen[1], P["last"]) if seen else P["last"]),
        "connections": {"h24": P["c24"], "d14": P["c14"]},
        "sessions": {"h24": sessions(m24), "d14": sessions(P["min"])},
        "active_min": {"h24": len(m24), "d14": len(P["min"])},
        "clients": [k for k, _ in cl.most_common(4)],
        "blocked_d14": P["blocked"],
        "new_source_events": [{k: v for k, v in ev.items()} for ev in NEW_SRC_EV.get((u, s), [])],
    })
for n in pairs_out:
    pairs_out[n].sort(key=lambda r: (-r["connections"]["d14"], r["ip"]))

# ---------------------------------------------------------------------------
# 6. egress (landing nodes + relays): each DB's view + observation history
# ---------------------------------------------------------------------------
obs = defaultdict(lambda: defaultdict(lambda: {"first": None, "last": None, "n": 0, "via": Counter(), "jobs": []}))
JOB_STATUS = ns.get("JOB_STATUS", {})
STEP_WORD = ns["DICT"]["step"]


def observe(u, ip, ts, via, job=None):
    if not u or not ip or not ts:
        return
    try:
        ipaddress.ip_address(ip)
    except ValueError:
        return
    r = obs[u][ip]
    r["first"] = ts if r["first"] is None else min(r["first"], ts)
    r["last"] = ts if r["last"] is None else max(r["last"], ts)
    r["n"] += 1
    r["via"][via] += 1
    if job and job not in r["jobs"]:
        r["jobs"].append(job)


def walk(u, ts, d, via):
    if isinstance(d, dict):
        for k, v in d.items():
            if k in ("actual_ip", "egress_ip") and isinstance(v, str):
                observe(u, v, ts, via)
            else:
                walk(u, ts, v, via)
    elif isinstance(d, list):
        for v in d:
            walk(u, ts, v, via)


for ts, act, tgt, det in o.execute("SELECT ts,action,target,detail FROM audit WHERE target IS NOT NULL"):
    if tgt not in ASSETS:
        continue
    try:
        d = json.loads(det) if det else {}
    except ValueError:
        continue
    if act == "asset_add" and isinstance(d, dict) and d.get("ip"):
        observe(tgt, d["ip"], ts, "登记")
    elif act == "asset_edit" and isinstance(d, dict) and isinstance(d.get("after"), dict) and d["after"].get("ip"):
        observe(tgt, d["after"]["ip"], ts, "编辑")
        if isinstance(d.get("before"), dict) and d["before"].get("ip"):
            observe(tgt, d["before"]["ip"], ts, "编辑前")
    else:
        walk(tgt, ts, d, "状态翻转" if act == "health_flip" else "审计")
for asset, key, opened, ev in o.execute("SELECT asset,key,opened_at,evidence FROM incidents"):
    if asset in ASSETS and ev:
        walk(asset, opened, json.loads(ev), "故障证据")
by_name = {SHORT[u]: u for u in ASSETS}
for j in raw["jobs"]:
    r = j.get("result") if isinstance(j.get("result"), dict) else {}
    nm = r.get("name") or j["params"].get("name") or j.get("title")
    u = by_name.get((nm or "").split(" · ")[0])
    if u:
        st = JOB_STATUS.get(j["status"], j["status"])
        ref = {"job": j["id"][:8], "status": st, "status_word": STEP_WORD.get(st, st), "at": int(j["created_at"])}
        if j["params"].get("ip") or j["params"].get("egress_ip"):
            observe(u, j["params"].get("egress_ip") or j["params"].get("ip"), j["created_at"], "上架任务", ref)
        if r.get("egress_ip"):
            observe(u, r["egress_ip"], j["finished_at"] or j["created_at"], "上架验收", ref)
for u, n in raw["meta"]["monitor"]["nodes"].items():
    if u in ASSETS:
        walk(u, n.get("checked_at"), n.get("checks"), "当前检查")
for u, k in KCL.items():
    if u in ASSETS and k.get("ipv4"):
        observe(u, k["ipv4"], NOW, "主机探针上报")


def brief(e_):
    return {"carrier": e_["carrier"], "asn": e_["asn"], "as_handle": e_["as_handle"], "region": e_["province"], "city": e_["city"],
            "conn_type": e_["conn_type"], "ip2region": e_["dbs"]["ip2region"]["raw"],
            "dbip": " / ".join(x for x in (e_["dbs"]["dbip_city"]["region"], e_["dbs"]["dbip_city"]["city"]) if x) or None}


def view(e_):
    if not e_:
        return None
    d = e_["dbs"]
    return {
        "isp": e_["carrier"], "asn": e_["asn"], "as_org": e_["as_org"], "as_handle": e_["as_handle"], "as_desc": e_["as_desc"],
        "country": e_["country"], "country_name": e_["country_name"], "region": e_["province"], "city": e_["city"],
        "geo_basis": e_["geo_basis"], "lat": e_["lat"], "lon": e_["lon"],
        "dbs": {
            "ip2region": {"country": d["ip2region"]["country"], "region": d["ip2region"]["region"], "region_zh": zh(d["ip2region"]["region"]),
                          "city": d["ip2region"]["city"], "city_zh": zh(d["ip2region"]["city"]), "isp": d["ip2region"]["isp"], "raw": d["ip2region"]["raw"]},
            "dbip": {"country": d["dbip_city"]["country"], "region": d["dbip_city"]["region"], "region_zh": zh(d["dbip_city"]["region"]),
                     "city": d["dbip_city"]["city"], "city_zh": zh(city_key(d["dbip_city"]["city"])),
                     "lat": d["dbip_city"]["lat"], "lon": d["dbip_city"]["lon"], "asn": d["dbip_asn"]["asn"], "as_org": d["dbip_asn"]["org"]},
            "iptoasn": {"asn": d["iptoasn"]["asn"], "country": d["iptoasn"]["country"], "desc": d["iptoasn"]["desc"]},
        },
        "agree": e_["agree"],
        "conn_type": e_["conn_type"], "conn_conf": e_["conn_conf"], "conn_basis": e_["conn_basis"],
        "residential": True if e_["conn_type"] == "broadband" else (False if e_["conn_type"] in ("hosting", "datacenter") else None),
        "flags": [f for f in e_["flags"] if f in ("geo_conflict", "asn_conflict", "db_conflict", "datacenter", "private")],
    }


egress_nodes = {}
for u, a in sorted(ASSETS.items(), key=lambda kv: ns["sort_key"](kv[1])):
    ip = a.get("ip")
    k = KCL.get(u, {})
    ip6 = k.get("ipv6") or None
    internal = ID[u] if a["team"] == "线路机" else None
    e = enrich(ip) if ip else None
    e6 = enrich(ip6) if ip6 else None
    ips = sorted(obs.get(u, {}).items(), key=lambda kv: kv[1]["first"] or 0)
    changes = []
    for (ia, ra), (ib, rb) in zip(ips, ips[1:]):
        ea, eb = enrich(ia), enrich(ib)
        changes.append({"at": int(rb["first"]), "from": fake(ia), "to": fake(ib), "from_last_seen": int(ra["last"]),
                        "from_meta": brief(ea), "to_meta": brief(eb),
                        "same_asn": ea["asn"] == eb["asn"], "same_place": (ea["province"], ea["city"]) == (eb["province"], eb["city"]),
                        "evidence": ra["jobs"] + rb["jobs"]})
    v = view(e)
    if v and internal:
        v["conn_basis"] = "线路机 VPS：AS%s %s 是 IDC / 主机托管网络" % (e["asn"], HOSTING_ASN.get(e["asn"]) or e["as_org"])
    egress_nodes[ID[u]] = {
        "ip": fake(ip) if ip else None, **(v or {}),
        "v6": ({"ip": fake(ip6), **view(e6)} if e6 else None),
        "history": {
            "distinct_ips": len(ips), "observations": sum(r["n"] for _, r in ips),
            "first_seen": int(min((r["first"] for _, r in ips), default=0)) or None,
            "last_seen": int(max((r["last"] for _, r in ips), default=0)) or None,
            "via": dict(sum((r["via"] for _, r in ips), Counter())),
            "ips": [{"ip": fake(i), "first_seen": int(r["first"]), "last_seen": int(r["last"]), "observations": r["n"],
                     "via": dict(r["via"]), "jobs": r["jobs"], "matches_ledger": i == ip} for i, r in ips],
            "changes": changes,
            "stable": len(ips) <= 1 and all(i == ip for i, _ in ips),
        },
    }


def cnt(key):
    c = Counter()
    for n, e in egress_nodes.items():
        c[key(n, e)] += 1
    return [{"k": k, "n": v} for k, v in c.most_common()]


land = {n: e for n, e in egress_nodes.items() if not n.startswith("r")}
egress_summary = {
    "landing_nodes": len(land), "relays": len(egress_nodes) - len(land),
    "by_asn": cnt(lambda n, e: "AS%s %s" % (e.get("asn"), e.get("isp"))),
    "by_conn_type": cnt(lambda n, e: e.get("conn_type")),
    "by_region": cnt(lambda n, e: "%s·%s" % (e.get("country_name"), e.get("region") or "?")),
    "by_region_ip2region": cnt(lambda n, e: "%s·%s" % (e["dbs"]["ip2region"]["country"], e["dbs"]["ip2region"]["region_zh"] or "?")),
    "by_region_dbip": cnt(lambda n, e: "%s·%s" % (e["dbs"]["dbip"]["country"], e["dbs"]["dbip"]["region_zh"] or "?")),
    "by_city_dbip": cnt(lambda n, e: "%s·%s" % (e["dbs"]["dbip"]["region_zh"] or "?", e["dbs"]["dbip"]["city_zh"] or e["dbs"]["dbip"]["city"] or "?")),
    "residential_landing": sum(1 for e in land.values() if e.get("residential")),
    "region_disagree": [n for n, e in egress_nodes.items() if e["agree"]["province"] is False],
    "city_disagree": [n for n, e in egress_nodes.items() if e["agree"]["city"] is False],
    "ip_changes": sum(len(e["history"]["changes"]) for e in egress_nodes.values()),
    "nodes_with_ip_change": [n for n, e in egress_nodes.items() if e["history"]["changes"]],
    "observations": sum(e["history"]["observations"] for e in egress_nodes.values()),
    "with_ipv6": [n for n, e in egress_nodes.items() if e.get("v6")],
    "note": "出口 IP 只在登记、上架任务、状态翻转、故障证据和当前检查里留有记录；每 5 分钟的出口验证结果不落库，所以这是“有记录的观测”，不是逐次实测",
}

# ---------------------------------------------------------------------------
# 7. anomalies derived from the real data (grouped, so one event = one thing that happened)
# ---------------------------------------------------------------------------
anoms = []
NAME_BY_ID = {ID[u]: SHORT[u] for u in ID}


def A(kind, severity, ts, title, **kw):
    anoms.append({"kind": kind, "severity": severity, "ts": int(ts) if ts else None, "title": title,
                  **{k: v for k, v in kw.items() if v is not None}})


def place_of(e_):
    return " ".join(x for x in (e_["province"], e_["city"]) if x) or e_["country_name"]


node_first = {}
for (u, s), P in PAIR.items():
    node_first[u] = min(node_first.get(u, 1e18), P["first"])
WARMUP = 6 * 3600  # a node's first 6 h of traffic is its baseline
groups = {}
for u in sorted({u for u, _ in PAIR}, key=asset_ref):
    ps = sorted((SEEN.get((u, s), (PAIR[(u, s)]["first"],))[0], s) for (uu, s) in PAIR if uu == u)
    seen_car, seen_city, seen_cty = set(), set(), set()
    for f, s in ps:
        e = EN[s]
        if e["internal"]:
            continue
        car, city, cty = e["carrier"], (e["province"], e["city"]), e["country"]
        if seen_car and f - node_first[u] >= WARMUP:
            kind, prior = None, None
            if cty not in seen_cty:
                kind, prior = "new_country", set(seen_cty)
            elif car not in seen_car:
                kind, prior = "new_carrier", set(seen_car)
            elif city not in seen_city:
                kind, prior = "new_city", {"%s %s" % c for c in seen_city}
            if kind:
                g = groups.setdefault((s, kind), {"nodes": [], "ts": f, "prior": set(), "event_ids": [], "c": 0})
                g["nodes"].append(asset_ref(u))
                g["ts"] = min(g["ts"], f)
                g["prior"] |= prior
                g["event_ids"] += [ev["event_id"] for ev in NEW_SRC_EV.get((u, s), [])]
                g["c"] += PAIR[(u, s)]["c14"]
        seen_car.add(car)
        seen_city.add(city)
        seen_cty.add(cty)
KIND_TITLE = {"new_country": "新国家", "new_carrier": "新运营商", "new_city": "新城市"}
for (s, kind), g in groups.items():
    e = EN[s]
    n = len(g["nodes"])
    who = NAME_BY_ID.get(g["nodes"][0], g["nodes"][0]) if n == 1 else "%d 个节点" % n
    A(kind, "warn" if kind == "new_country" or e["conn_type"] in ("hosting", "datacenter") else "info", g["ts"],
      "%s：%s · %s%s 首次出现在 %s" % (KIND_TITLE[kind], place_of(e), e["carrier"],
                                    "（%s）" % CT_WORD.get(e["conn_type"]) if e["conn_conf"] != "low" else "", who),
      source=fake(s), carrier=e["carrier"], asn=e["asn"], place=place_of(e), conn_type=e["conn_type"], nodes=sorted(g["nodes"]),
      prior=sorted(g["prior"]), connections_d14=g["c"], event_ids=sorted(set(g["event_ids"])) or None)

# nodes used from several provinces / carriers in the last 24 h (internal relay probes excluded)
MULTI_NODE = {}
for n, ps in sorted(pairs_out.items()):
    if n.startswith("direct:"):
        continue
    live = [SRC_BY_FAKE[p["ip"]] for p in ps if p["connections"]["h24"] and not SRC_BY_FAKE[p["ip"]]["internal"]]
    provs = sorted({x["province"] or x["country_name"] for x in live})
    cities = sorted({place_of(x) for x in live})
    cars = sorted({x["carrier"] for x in live})
    if len(provs) >= 2 or len(cars) >= 3:
        MULTI_NODE[n] = {"node": n, "provinces": provs, "cities": cities, "carriers": cars, "sources_h24": len(live)}
if MULTI_NODE:
    rows = sorted(MULTI_NODE.values(), key=lambda r: (-len(r["provinces"]), -len(r["cities"]), r["node"]))
    A("multi_location_node", "info", AS_OF, "%d 个节点近 24 小时从多个省份接入（最多 %d 省 %d 市）" % (
        len(rows), len(rows[0]["provinces"]), len(rows[0]["cities"])), nodes=[r["node"] for r in rows], rows=rows)

for x in sources_out:
    if x["nodes"]["h24"] >= 10 and not x["internal"]:
        A("fanout_source", "info", x["last_seen"], "同一来源 24 小时内接入 %d 个节点（%s · %s）" % (x["nodes"]["h24"], place_of(x), x["carrier"]),
          source=x["ip"], nodes_h24=x["nodes"]["h24"], teams=x["teams"], connections_h24=x["connections"]["h24"])
    if "datacenter" in x["flags"]:
        A("datacenter_source", "info" if x["internal"] else "warn", x["last_seen"],
          ("机房来源：%s 自己的地址（AS%s %s）连了 %d 个节点" % (NAME_BY_ID.get(x["internal"], x["internal"]), x["asn"], x["carrier"], x["nodes"]["d14"]))
          if x["internal"] else "机房来源：AS%s %s 连了 %d 个节点" % (x["asn"], x["carrier"], x["nodes"]["d14"]),
          source=x["ip"], internal=x["internal"], asn=x["asn"], nodes=x["node_ids"], connections_d14=x["connections"]["d14"],
          first_seen=x["first_seen"], last_seen=x["last_seen"],
          note="线路机自身地址：连接数很少、间隔稀疏，像链路探针（实际入口到出口）走业务入口产生的连接" if x["internal"] else None)
    if "foreign" in x["flags"]:
        A("foreign_source", "warn", x["first_seen"], "境外来源：%s · %s（AS%s，%s）" % (place_of(x), x["carrier"], x["asn"], CT_WORD.get(x["conn_type"])),
          source=x["ip"], nodes=x["node_ids"], connections_d14=x["connections"]["d14"], first_seen=x["first_seen"], last_seen=x["last_seen"])
conf_rows = []
for x in sources_out:
    if "db_conflict" in x["flags"] or "geo_conflict" in x["flags"]:
        d = x["dbs"]
        conf_rows.append({"source": x["ip"], "ip2region": d["ip2region"]["raw"], "dbip": " / ".join(v for v in (d["dbip_city"]["region"], d["dbip_city"]["city"]) if v),
                          "asn": x["asn"], "as_org": x["as_org"], "agree": x["agree"], "connections_d14": x["connections"]["d14"]})
if conf_rows:
    A("db_conflict", "info", None, "%d 个来源 IP 两库位置或归属不一致（中国地址以 ip2region 为准）" % len(conf_rows), rows=conf_rows)
# many-location clients: one 3x-ui client label used from several provinces within 14 d
cl_loc = defaultdict(lambda: defaultdict(int))
for (u, s), P in PAIR.items():
    e = EN[s]
    if e["internal"]:
        continue
    for c, v in P["clients"].items():
        if c:
            cl_loc[c][(e["province"] or e["country_name"], place_of(e))] += v
cl_rows = []
for c, locs in sorted(cl_loc.items()):
    provs = sorted({k[0] for k in locs})
    if len(provs) >= 2:
        cl_rows.append({"client": c, "provinces": provs, "places": [{"place": k[1], "connections": v} for k, v in sorted(locs.items(), key=lambda kv: -kv[1])]})
if cl_rows:
    A("multi_location_client", "info", None, "%d 个客户端标识 14 天内从 2 个以上省份接入" % len(cl_rows), rows=cl_rows)
for n, e in egress_nodes.items():
    for ch in e["history"]["changes"]:
        A("egress_ip_change", "info", ch["at"], "%s 出口 IP 换过：%s → %s（%s）" % (
            NAME_BY_ID.get(n, n), ch["from"], ch["to"], "同 ASN、同地" if ch["same_asn"] and ch["same_place"] else ("同 ASN" if ch["same_asn"] else "ASN 也变了")),
          node=n, **{k: ch[k] for k in ("from", "to", "from_meta", "to_meta", "evidence")})
rd = egress_summary["region_disagree"]
if rd:
    A("egress_geo_conflict", "info", None, "%d 个出口 IP 两库给出的州不一致（ip2region 多为注册地，DB-IP 更接近实际机房）" % len(rd), nodes=rd)
stable = [n for n, e in egress_nodes.items() if e["history"]["stable"]]
A("egress_stable", "info", None, "%d / %d 个出口 IP 在全部 %d 条记录里没有变化" % (len(stable), len(egress_nodes), egress_summary["observations"]),
  nodes_count=len(stable))
SEV = {"warn": 0, "info": 1}
anoms.sort(key=lambda a: (SEV[a["severity"]], -(a["ts"] or 0), a["kind"]))
for i, a in enumerate(anoms, 1):
    a["id"] = "ipa-%02d" % i

# per-node rollup for node rows, cards and drawers
node_summary = {}
for n, bd in node_bd.items():
    ps = pairs_out.get(n, [])
    srcs = [SRC_BY_FAKE[p["ip"]] for p in ps]

    def top1(w, dim):
        r = bd[w][dim][0] if bd[w][dim] else None
        return {"k": r["k"], "pct": r["pct"]} if r else None
    ids = [a["id"] for a in anoms if n in (a.get("nodes") or []) or a.get("node") == n]
    flags = []
    if n in MULTI_NODE:
        flags.append("multi_location_h24")
    if any(x["internal"] for x in srcs):
        flags.append("internal_probe_source")
    if any("foreign" in x["flags"] for x in srcs):
        flags.append("foreign_source")
    if any(a["kind"] in ("new_city", "new_carrier", "new_country") and a["ts"] and a["ts"] > H24 for a in anoms if a["id"] in ids):
        flags.append("new_place_h24")
    node_summary[n] = {
        "sources": {"h24": bd["h24"]["total"]["sources"], "d14": bd["d14"]["total"]["sources"]},
        "new_sources_h24": sum(1 for p in ps if p["first_seen"] > H24),
        "top_carrier": {"h24": top1("h24", "carrier"), "d14": top1("d14", "carrier")},
        "top_city": {"h24": top1("h24", "city"), "d14": top1("d14", "city")},
        "carriers_h24": [r["k"] for r in bd["h24"]["carrier"]],
        "provinces_h24": [r["k"] for r in bd["h24"]["province"]],
        "mobile_pct": {w: next((r["pct"] for r in bd[w]["conn_type"] if r["k"] == "mobile"), 0.0) for w in ("h24", "d14")},
        "flags": flags, "anomaly_ids": ids,
    }

# ---------------------------------------------------------------------------
# 8. summary + output
# ---------------------------------------------------------------------------
F = breakdowns["fleet"]


def top(win, dim, n=5):
    return [{"k": r["k"], "label": r.get("label"), "connections": r["c"], "sources": r["s"], "pct": r["pct"]} for r in F[win][dim][:n]]


def share(win, dim, key):
    r = next((r for r in F[win][dim] if r["k"] == key), None)
    return r["pct"] if r else 0.0


summary = {
    "sources": {"h24": F["h24"]["total"]["sources"], "d14": F["d14"]["total"]["sources"]},
    "new_sources_h24": sum(1 for x in sources_out if x["new_in_h24"]),
    "carriers": {"h24": len(F["h24"]["carrier"]), "d14": len(F["d14"]["carrier"])},
    "provinces": {"h24": len(F["h24"]["province"]), "d14": len(F["d14"]["province"])},
    "cities": {"h24": len(F["h24"]["city"]), "d14": len(F["d14"]["city"])},
    "asns": {"h24": len(F["h24"]["asn"]), "d14": len(F["d14"]["asn"])},
    "mobile_pct": {"h24": share("h24", "conn_type", "mobile"), "d14": share("d14", "conn_type", "mobile")},
    "datacenter_sources": sum(1 for x in sources_out if "datacenter" in x["flags"]),
    "internal_sources": sum(1 for x in sources_out if x["internal"]),
    "foreign_sources": sum(1 for x in sources_out if "foreign" in x["flags"]),
    "db_conflict_sources": sum(1 for x in sources_out if "db_conflict" in x["flags"] or "geo_conflict" in x["flags"]),
    "top": {w: {d: top(w, d) for d in ("carrier", "province", "city", "asn", "conn_type")} for w in ("h24", "d14")},
    "anomalies": {"warn": sum(1 for a in anoms if a["severity"] == "warn"), "info": sum(1 for a in anoms if a["severity"] == "info")},
    "egress": {"asn": egress_summary["by_asn"], "residential_landing": egress_summary["residential_landing"],
               "landing_nodes": egress_summary["landing_nodes"], "ip_changes": egress_summary["ip_changes"]},
}

meta = {
    "method": "离线：在测试服 a395 上用本地库文件查询，IP 没有发往任何在线服务；运营库以 mode=ro&immutable=1 只读打开",
    "enriched_at": int(time.time()), "as_of": AS_OF,
    "databases": [
        {"id": "ip2region", "name": "ip2region xdb（IPv4 / IPv6）", "version": time.strftime("%Y-%m-%d", time.gmtime(X4.created_at)),
         "licence": "Apache-2.0 / MIT", "url": "https://github.com/lionsoul2014/ip2region/tree/master/data",
         "provides": ["国家", "省份", "城市", "运营商"]},
        {"id": "dbip_city", "name": "DB-IP IP to City Lite", "file": CITY_F, "version": time.strftime("%Y-%m-%d", time.gmtime(CITY.meta.get("build_epoch", 0))),
         "licence": "CC BY 4.0", "attribution": "IP Geolocation by DB-IP", "url": "https://db-ip.com",
         "provides": ["国家", "州 / 省", "城市（中国多为区县或街道）", "经纬度"]},
        {"id": "dbip_asn", "name": "DB-IP IP to ASN Lite", "file": ASN_F, "version": time.strftime("%Y-%m-%d", time.gmtime(ASNDB.meta.get("build_epoch", 0))),
         "licence": "CC BY 4.0", "attribution": "IP Geolocation by DB-IP", "url": "https://db-ip.com", "provides": ["ASN", "AS 组织"]},
        {"id": "iptoasn", "name": "iptoasn.com ip2asn-combined", "file": I2A_F, "version": time.strftime("%Y-%m-%d", time.gmtime(I2A_MTIME)),
         "licence": "PDDL 1.0", "url": "https://iptoasn.com", "provides": ["ASN", "AS 名称", "注册国家"]},
    ],
    "windows": {"h24": {"from": int(H24), "to": AS_OF}, "d14": {"from": int(D14), "to": AS_OF, "data_from": int(ACT_MIN_TS),
                "note": "活动表从 %s 起才有记录，14 天窗口实际覆盖约 %.1f 天" % (time.strftime("%m-%d %H:%M", time.gmtime(ACT_MIN_TS + 28800)), (NOW - ACT_MIN_TS) / 86400)}},
    "hourly24_t0": HH0, "daily14_days": [time.strftime("%Y-%m-%d", time.gmtime(d * 86400)) for d in DAYS],
    "session_gap_s": GAP_MIN * 60,
    "session_note": "会话 = 同一来源在同一范围内相邻两次连接间隔不超过 30 分钟的一段；active_min = 有连接的分钟数",
    "activity_rows": nrows, "scan_s": round(t_scan, 2), "pairs": len(PAIR), "distinct_sources": len(SRC),
    "verify": verify,
}
DICT_OUT = {
    "conn_type": {"mobile": "移动网络（蜂窝）", "broadband": "固定宽带", "datacenter": "云机房", "hosting": "IDC / 主机托管", "education": "教育科研网", "private": "内网 / 保留地址", "unknown": "未知"},
    "conn_type_short": CT_WORD,
    "confidence": {"high": "高", "medium": "中", "low": "低"},
    "carrier_basis": {"ip2region": "ip2region 运营商字段", "asn": "按 ASN 归属", "as_org": "AS 组织名"},
    "geo_basis": {"ip2region": "ip2region（中国地址）", "dbip": "DB-IP（境外地址）"},
    "flag": {"internal": "线路机自身地址", "datacenter": "机房来源", "foreign": "境外来源", "db_conflict": "库间归属不一致",
             "geo_conflict": "库间位置不一致", "private": "内网 / 保留地址", "asn_conflict": "库间 ASN 不一致", "multi_location_h24": "24 小时多地接入",
             "internal_probe_source": "有线路机探针来源", "foreign_source": "有境外来源", "new_place_h24": "24 小时内出现新地点"},
    "anomaly": {"new_country": "新国家", "new_carrier": "新运营商", "new_city": "新城市", "multi_location_node": "节点多地接入",
                "multi_location_client": "客户端多地接入", "fanout_source": "单一来源接入多个节点", "datacenter_source": "机房来源",
                "foreign_source": "境外来源", "db_conflict": "库间分歧", "egress_ip_change": "出口 IP 变更",
                "egress_geo_conflict": "出口定位分歧", "egress_stable": "出口 IP 稳定"},
    "severity": {"warn": "提醒", "info": "信息"},
    "breakdown_row": {"k": "分组键", "c": "连接数", "s": "来源 IP 数", "ss": "会话数", "m": "有连接的分钟数", "pct": "连接占比 %"},
}
out = {"meta": meta, "dict": DICT_OUT, "summary": summary, "sources": sources_out, "leases": leases, "breakdowns": breakdowns,
       "nodes": {n: {"summary": node_summary.get(n), "breakdown": node_bd.get(n), "sources": pairs_out.get(n, []), "egress": egress_nodes.get(n)}
                 for n in sorted(set(node_bd) | set(egress_nodes))},
       "egress_summary": egress_summary, "anomalies": anoms}
s = json.dumps(out, ensure_ascii=False, separators=(",", ":"), default=list)

# leak check: no real IP (any we touched) may appear in the output
reals = set(IPMAP) | set(IP6MAP) | set(SRC) | {a.get("ip") for a in ASSETS.values()} | {k.get("ipv6") for k in KCL.values()}
reals = {r for r in reals if r and not FAKE_RE.fullmatch(r)}
for r in reals:
    if re.search(r"(?<![\d.:a-f])" + re.escape(r) + r"(?![\d:a-f])", s):
        sys.stderr.write("LEAK: a real address is still in the output\n")
        sys.exit(3)
for m in re.finditer(r"(?<![\d.])(\d{1,3}(?:\.\d{1,3}){3})(?![\d.])", s):
    if not FAKE_RE.fullmatch(m[1]):
        sys.stderr.write("LEAK: non-documentation IPv4 in output\n")
        sys.exit(3)
sys.stderr.write("ok rows=%d scan=%.2fs total=%.2fs bytes=%d\n" % (nrows, t_scan, time.time() - T_START, len(s)))
sys.stdout.write(s)
