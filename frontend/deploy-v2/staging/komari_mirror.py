"""Komari RPC mirror for the a395 staging server.

team-ops answers /komari/* by asking Komari through the plugin pump (provision.komari(method, params)).
Staging has no Komari process, so stage_server.py replaces provision.komari with KomariMirror.call:
the same four RPC methods, answered with read-only SQL over the komari.db / metrics.db copies and the
staging clock.  Result shapes follow what the production Komari 1.5.0-review20260917.4 binary returned
for the same calls (captured once with komari_probe.sh inside an isolated network namespace):

  admin:listClients        [client]  (GORM row; times RFC 3339 UTC; auto_renewal / hidden bool)
  admin:getAllPingTasks    [{id, weight, name, clients[], default_on, type, target, interval}]
  public:getRecordsByUUID  {count, has_gpu_data, records:[{client, time, cpu, gpu, ram, ram_total, swap,
                            swap_total, load, temp, disk, disk_total, net_in, net_out, net_total_up,
                            net_total_down, traffic_up, traffic_down, process, connections, connections_udp}]}
                           oldest first; 1-minute rollups for hours <= 4, 5-minute rollups above
  public:getPingRecords    {count, basic_info:[{client, loss, min, max}], records:[{task_id, time, value,
                            client}] newest first, tasks:[{avg, [clients], default_on, id, interval, loss,
                            max, min, name, total, type}]}; value = the bucket's LAST sample (-1 = lost),
                            not its mean

verify_mirror.py (next to this file) compares every method against the captured real output:
all records identical except the order of same-bucket rows from two executors (Go's sort is not stable).
"""
import datetime as dt
import json
import sqlite3
import threading

HOST_FIELDS = {  # metric -> (record field, aggregate)
    "cpu.usage": ("cpu", "mean"), "memory.used": ("ram", "mean_int"), "swap.used": ("swap", "mean_int"),
    "load.average": ("load", "mean"), "disk.used": ("disk", "mean_int"), "net.in.rate": ("net_in", "mean_int"),
    "net.out.rate": ("net_out", "mean_int"), "net.total.up": ("net_total_up", "last"),
    "net.total.down": ("net_total_down", "last"), "traffic.up": ("traffic_up", "sum_int"),
    "traffic.down": ("traffic_down", "sum_int"), "process.count": ("process", "mean_int"),
    "connections.tcp": ("connections", "mean_int"), "connections.udp": ("connections_udp", "mean_int"),
}
RECORD_ORDER = ["client", "time", "cpu", "gpu", "ram", "ram_total", "swap", "swap_total", "load", "temp", "disk",
                "disk_total", "net_in", "net_out", "net_total_up", "net_total_down", "traffic_up", "traffic_down",
                "process", "connections", "connections_udp"]


def go_time(ms):
    """Go's time.Time JSON for a UTC instant: RFC 3339, fractional seconds only when non-zero."""
    t = dt.datetime.fromtimestamp(ms / 1000, dt.timezone.utc)
    s = t.strftime("%Y-%m-%dT%H:%M:%S")
    if t.microsecond:
        s += ("." + "%06d" % t.microsecond).rstrip("0")
    return s + "Z"


def go_db_time(value):
    """GORM sqlite text ('2026-09-14 10:19:41.015742185+00:00') -> Go JSON ('2026-09-14T10:19:41.015742185Z')."""
    if not value:
        return value
    s = str(value).replace(" ", "T", 1)
    if s.endswith("+00:00"):
        s = s[:-6] + "Z"
    return s


class KomariMirror:
    def __init__(self, komari_db, metrics_db, clock):
        self.komari_db, self.metrics_db, self.clock = komari_db, metrics_db, clock
        self.local = threading.local()

    def _db(self, name, path):
        conn = getattr(self.local, name, None)
        if conn is None:
            conn = sqlite3.connect("file:%s?mode=ro" % path, uri=True, timeout=30, check_same_thread=False)
            conn.row_factory = sqlite3.Row
            setattr(self.local, name, conn)
        return conn

    @property
    def k(self):
        return self._db("k", self.komari_db)

    @property
    def m(self):
        return self._db("m", self.metrics_db)

    # -- RPC entry (signature of provision.komari) ---------------------------------------------------
    def call(self, method, params, timeout=60):
        handler = {"admin:listClients": self.list_clients, "admin:getAllPingTasks": self.ping_tasks,
                   "public:getRecordsByUUID": self.records, "public:getPingRecords": self.ping_records}.get(method)
        if handler is None:
            raise RuntimeError("测试服不支持 Komari 调用 " + method)
        return handler(params or {})

    def list_clients(self, params):
        out = []
        for r in self.k.execute("SELECT * FROM clients ORDER BY rowid"):
            c = dict(r)
            for key in ("auto_renewal", "hidden"):
                c[key] = bool(c.get(key))
            for key in ("expired_at", "created_at", "updated_at"):
                c[key] = go_db_time(c.get(key))
            for key in ("uuid", "token", "ipv4", "ipv6", "remark", "public_remark"):  # json:",omitempty"
                if c.get(key) in ("", None):
                    c.pop(key, None)
            out.append(c)
        return out

    def _tasks(self):
        tasks = []
        for r in self.k.execute("SELECT * FROM ping_tasks ORDER BY weight, id"):
            try:
                clients = json.loads(r["clients"] or "[]")
            except ValueError:
                clients = []
            tasks.append({"id": r["id"], "weight": r["weight"], "name": r["name"], "clients": clients,
                          "default_on": bool(r["all_clients"]), "type": r["type"], "target": r["target"],
                          "interval": r["interval"]})
        return tasks

    def ping_tasks(self, params):
        return self._tasks()

    def _window(self, hours):
        hours = float(hours)
        res = 60000 if hours <= 4 else 300000
        rid = 1 if res == 60000 else 2
        now_ms = int(self.clock() * 1000)
        start = now_ms - int(hours * 3600000)
        return rid, res, (start // res) * res, now_ms

    def records(self, params):
        uuid = str(params.get("uuid") or "")
        if not uuid:
            raise RuntimeError("UUID is required")
        rid, res, lo, hi = self._window(params.get("hours") or 24)
        series = {r["id"]: r["metric_name"] for r in self.m.execute(
            "SELECT id, metric_name FROM metric_series WHERE entity_id=? AND tags='{}'", (uuid,))}
        rows = {}
        if series:
            q = ",".join("?" * len(series))
            for r in self.m.execute(f"SELECT series_id, bucket_milli, count, sum, last_val FROM metric_rollups "
                                    f"WHERE resolution_id=? AND series_id IN ({q}) AND bucket_milli>=? AND bucket_milli<=?",
                                    (rid, *series, lo, hi)):
                field, agg = HOST_FIELDS.get(series[r["series_id"]], (None, None))
                if not field:
                    continue
                rec = rows.setdefault(r["bucket_milli"], {})
                n = r["count"] or 0
                if agg == "last":
                    rec[field] = r["last_val"]
                elif agg == "sum_int":
                    rec[field] = int(r["sum"])
                elif n:
                    v = r["sum"] / n
                    rec[field] = int(v) if agg == "mean_int" else v  # Go int() truncates
        records = []
        for bucket in sorted(rows):
            rec = {k: 0 for k in RECORD_ORDER}
            rec.update(rows[bucket])
            rec["client"], rec["time"] = uuid, go_time(bucket)
            records.append({k: rec[k] for k in RECORD_ORDER})
        return {"count": len(records), "has_gpu_data": False, "records": records}

    def ping_records(self, params):
        task_id = str(int(params.get("task_id")))
        uuid = params.get("uuid")
        rid, res, lo, hi = self._window(params.get("hours") or 24)
        tag = json.dumps({"task_id": task_id}, separators=(",", ":"))
        where, args = "metric_name='ping.latency_ms' AND tags=?", [tag]
        if uuid:
            where += " AND entity_id=?"
            args.append(uuid)
        series = {r["id"]: r["entity_id"] for r in self.m.execute("SELECT id, entity_id FROM metric_series WHERE " + where + " ORDER BY id", args)}
        records = []
        if series:
            q = ",".join("?" * len(series))
            # Legacy record = one sample per bucket: the bucket's last sample (-1 = lost), as Komari 1.5 returns it.
            for r in self.m.execute(f"SELECT series_id, bucket_milli, last_val FROM metric_rollups "
                                    f"WHERE resolution_id=? AND series_id IN ({q}) AND bucket_milli>=? AND bucket_milli<=?",
                                    (rid, *series, lo, hi)):
                records.append({"task_id": int(task_id), "time": go_time(r["bucket_milli"]), "value": int(r["last_val"]),
                                "client": series[r["series_id"]], "_k": (-r["bucket_milli"], ORDER_DESC(series[r["series_id"]]))})
        records.sort(key=lambda x: x["_k"])
        for x in records:
            x.pop("_k")
        basic = []
        for client in dict.fromkeys(x["client"] for x in records):
            vals = [x["value"] for x in records if x["client"] == client]
            ok = [v for v in vals if v >= 0]
            basic.append({"client": client, "loss": num(len(vals) - len(ok), len(vals)),
                          "min": min(ok, default=0), "max": max(ok, default=0)})
        task = next((t for t in self._tasks() if str(t["id"]) == task_id), None)
        tasks = []
        if task:
            vals = [x["value"] for x in records]
            ok = [v for v in vals if v >= 0]
            row = {"avg": int(sum(ok) / len(ok)) if ok else 0}
            if not uuid:
                row["clients"] = task["clients"]
            row.update({"default_on": task["default_on"], "id": task["id"], "interval": task["interval"],
                        "loss": num(len(vals) - len(ok), len(vals)), "max": max(ok, default=0), "min": min(ok, default=0),
                        "name": task["name"], "total": len(records), "type": task["type"]})
            tasks.append(row)
        return {"count": len(records), "basic_info": basic, "records": records, "tasks": tasks}


def num(part, whole):
    if not whole:
        return 0
    v = round(part * 100 / whole, 2)
    return int(v) if v == int(v) else v


def ORDER_DESC(client):
    # same-bucket rows of different executors come back in descending client-id order
    return tuple(-ord(ch) for ch in client)
