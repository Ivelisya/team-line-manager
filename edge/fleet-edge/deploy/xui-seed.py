#!/usr/bin/env python3
"""从 3x-ui 的 x-ui.db **只读**导出 relayd 的累计流量种子（在线路机上以 root 执行，输出到 stdout）。

    python3 xui-seed.py [/etc/x-ui/x-ui.db] > /tmp/seed.json

口径：inbounds.up/down（按 tag，例如 inbound-20001）+ client_traffics.up/down（按 email）。
ts = 导出时刻。切换时应在旧 xray / x-ui 停止之后导出，这样 ts 之后启动的新 Xray 的计数会被 relayd 全额计入，
种子 + 新计数 = 连续的累计值。只读打开（mode=ro），不写 x-ui.db。
"""
import json, sqlite3, sys, time

db = sys.argv[1] if len(sys.argv) > 1 else "/etc/x-ui/x-ui.db"
con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
ts = time.time()
entries = []
for tag, up, down in con.execute("SELECT tag, up, down FROM inbounds"):
    if tag:
        entries.append({"kind": "inbound", "tag": tag, "up": int(up or 0), "down": int(down or 0)})
for email, up, down in con.execute("SELECT email, up, down FROM client_traffics"):
    if email:
        entries.append({"kind": "user", "tag": email, "up": int(up or 0), "down": int(down or 0)})
json.dump({"ts": round(ts, 3), "source": f"x-ui.db:{db}", "entries": entries}, sys.stdout, ensure_ascii=False)
print()
print(f"导出 {len(entries)} 个条目，ts={ts:.0f}", file=sys.stderr)
