"""verify_mirror.py — compare KomariMirror with the real Komari RPC output captured by komari_probe.sh.

Run on a395 after copying live/fixtures/komari-probe/{*.json,shifted/*.json} to
~/team-ops-eval/v2-stage/verify/{real,shifted}/ (the shifted set needs the SHIFT=1 probe copy of metrics.db
still in ~/team-ops-eval/v2-stage/komari-probe/data).  Prints the number of mismatching records per call.
"""
import json, sys, os
sys.path.insert(0, os.path.expanduser("~/team-ops-eval/v2-stage/staging"))
from komari_mirror import KomariMirror
H = os.path.expanduser
V = H("~/team-ops-eval/v2-stage/verify")
UUID = "dfe4c76d-adbb-4566-93a2-ed180d7a7d6b"
def cmp(name, real, mine):
    real = real["result"]
    bad = 0
    if isinstance(real, dict) and "records" in real:
        rr, mm = real["records"], mine["records"]
        print(name, "count real/mine", real["count"], mine["count"])
        for i, (a, b) in enumerate(zip(rr, mm)):
            diffs = {k: (a.get(k), b.get(k)) for k in set(a) | set(b) if a.get(k) != b.get(k) and not (isinstance(a.get(k), float) and isinstance(b.get(k), (int, float)) and abs(a[k]-b[k]) < 1e-5 * max(1, abs(a[k])))}
            if diffs:
                bad += 1
                if bad <= 4: print("  rec", i, diffs)
        for k in real:
            if k != "records" and real[k] != mine.get(k):
                print("  key", k, "real", json.dumps(real[k], ensure_ascii=False)[:300], "\n      mine", json.dumps(mine.get(k), ensure_ascii=False)[:300])
    else:
        for i, (a, b) in enumerate(zip(real, mine)):
            a = dict(a); a.pop("token", None); b = dict(b); b.pop("token", None)
            if name == "ping_tasks": a.pop("target"); b.pop("target")
            d = {k: (a.get(k), b.get(k)) for k in set(a) | set(b) if a.get(k) != b.get(k)}
            if d:
                bad += 1
                if bad <= 4: print("  item", i, d)
        print(name, "len", len(real), len(mine))
    print(name, "mismatching", bad)
real_clock = 1790174336.3048396
m = KomariMirror(H("~/team-ops-eval/data/komari.db"), H("~/team-ops-eval/data/metrics.db"), lambda: real_clock)
L = lambda p, n: json.load(open(f"{V}/{p}/{n}.json"))
cmp("records_all_24", L("real", "records_all_24"), m.call("public:getRecordsByUUID", {"uuid": UUID, "load_type": "all", "hours": "24"}))
cmp("ping_records_task_uuid", L("real", "ping_records_task_uuid"), m.call("public:getPingRecords", {"task_id": "1", "hours": "24", "uuid": UUID}))
cmp("ping_records_task", L("real", "ping_records_task"), m.call("public:getPingRecords", {"task_id": "1", "hours": "24"}))
cmp("clients", L("real", "clients"), m.call("admin:listClients", {}))
cmp("ping_tasks", L("real", "ping_tasks"), m.call("admin:getAllPingTasks", {}))
sh_clock = 1790174396.259947
ms = KomariMirror(H("~/team-ops-eval/data/komari.db"), H("~/team-ops-eval/v2-stage/komari-probe/data/metrics.db"), lambda: sh_clock)
cmp("records_all_1", L("shifted", "records_all_1"), ms.call("public:getRecordsByUUID", {"uuid": UUID, "load_type": "all", "hours": "1"}))
cmp("records_all_4", L("shifted", "records_all_4"), ms.call("public:getRecordsByUUID", {"uuid": UUID, "load_type": "all", "hours": "4"}))
cmp("ping_1", L("shifted", "ping_records_task_uuid_1"), ms.call("public:getPingRecords", {"task_id": "1", "hours": "1", "uuid": UUID}))
cmp("ping_4", L("shifted", "ping_records_task_uuid_4"), ms.call("public:getPingRecords", {"task_id": "1", "hours": "4", "uuid": UUID}))
