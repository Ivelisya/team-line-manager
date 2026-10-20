import sqlite3, time
con = sqlite3.connect("/var/lib/fleetd/legacy.db", timeout=120)
cut = time.time() - 7 * 86400
with con:
    n = con.execute("delete from behavior_events where kind='new_domain' and ts < ?", (cut,)).rowcount
print(time.strftime("%Y-%m-%d %H:%M:%S"), "deleted new_domain older than 7d:", n)
