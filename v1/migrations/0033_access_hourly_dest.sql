-- 0033：访问监控「域名与 IP」按目标合并后，展开一个目标看各节点（/access/targets?target=）按目标查小时汇总。
-- 原来只有按小时的索引，查一个目标要把窗口内全部目标的小时汇总扫一遍（30 天窗口就是 30 天的全部行）。
-- dest_host、hour 在汇总重算时不变（只改 conns / blocked / sources），不妨碍 0016 为它开的 HOT 更新。可重复执行。

create index if not exists access_hourly_dest on access_hourly (dest_host, hour);
