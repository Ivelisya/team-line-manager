-- 0016：让汇总表、来源画像的高频更新走 HOT（不动索引）

-- 只有每小时一次的保留期清理（access::prune）用到它，表很小，顺序扫描即可；
-- 汇总每 5 分钟都要改 last_seen，带着它每次更新都得写索引（非 HOT），索引膨胀到表的好几倍
drop index access_target_seen_last;

-- 汇总表当前小时的行追加写满整页、下一轮重算就要改：页里留 30% 空间给新版本，同页更新才能走 HOT。
-- 只对之后写入的页生效，已有的页不重排
alter table access_hourly        set (fillfactor = 70);
alter table access_source_hourly set (fillfactor = 70);
alter table access_target_seen   set (fillfactor = 70);

-- 来源同步每 5 分钟改 last_seen：索引列不再含它（谓词列 flag 很少变），这些更新可以走 HOT。
-- 两处用法（ipintel::reconcile_incidents 收尾过期的 open、按节点取 open）都只扫 flag = 'open' 的那几行
drop index node_source_seen_open;
create index node_source_seen_open on node_source_seen (node_id) where flag = 'open';
