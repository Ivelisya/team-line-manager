-- 0018：补上 0016 漏设的 fillfactor，删掉两条没用的索引

-- 0016 让 node_source_seen 的索引列不含 last_seen，但没给页留空位：来源同步每 5 分钟改 last_seen / conns，
-- 页是满的，新版本只能写到别的页，实验室实测 HOT 只占 15%。同汇总表一样留 30%。只对之后写入的页生效
alter table node_source_seen set (fillfactor = 70);

-- 策略版本列表按 seq 排序（seq 有唯一索引），没有查询按 created_at 排序或筛选，实验室 idx_scan = 0
drop index policy_version_created;

-- lifecycle 只有 6 个取值，查询几乎都写 lifecycle <> 'retired'（用不上它）；只剩「已退役」列表的 = 'retired' 会走它，
-- 而 machine 只有几百行，顺序扫描一样快。留着它每次改生命周期都要多写一份索引
drop index machine_lifecycle_idx;
