-- 0049：探测点「在线但整轮作废」不再失明、全部线路机同时连不上照记、客户入口连通性的小时汇总（见 probe_points/、reach.rs）。可重复执行。
-- 1. probe_point.last_kept_at：最近一次被采纳的轮次（写进了 relay_reach，或这一轮本来就没有要测的）；刚上线、离线后刚回来时从那一刻算起。
--    主来源超过 3 分钟没有被采纳的轮次按离线处理（退回中控 / 标灰），并开 probe_point_offline（「在线，但最近的检查都作废了」）。
--    discard_reason：最近一轮作废的原因（对照目标都连不上），被采纳的轮次清空。已有的探测点按最近上报算起，升级后不会一下子都算作废。
-- 2. relay_reach.all_down：这一轮对照目标是通的、三台以上线路机却全部连不上（以前整轮作废，现在照记）。relay_unreachable 的摘要据此写明
--    「全部线路机同时连不上（可能是供应商整段故障）」。旧行为 false。
-- 3. relay_reach_hourly：客户入口连通性的小时汇总（每台线路机 × 每一路来源 × 每小时：检查次数、通的次数、通的那些建连往返时间的平均与中位数；
--    中控那一路不记往返时间），随遥测小时汇总每 5 分钟重算当前小时（db::rollup_hour）。原始的 relay_reach 只留 [retention] reach_days（默认 2 天），
--    探测点页与线路机抽屉的 24 小时 / 7 天 / 30 天成功率读这张表。保留同 probe_hourly：一直留着，线路机物理删除、探测点删除时级联删。
--    probe_point_id 为 null 是中控那一路：唯一索引用 coalesce 把它折成全 0 的 uuid（on conflict 按同一个表达式）。
--    现有的原始记录（最近 2 天）在这里补算一遍。
alter table probe_point add column if not exists last_kept_at timestamptz,
                        add column if not exists discard_reason text;
update probe_point set last_kept_at = last_seen_at where last_kept_at is null;

alter table relay_reach add column if not exists all_down boolean not null default false;

create table if not exists relay_reach_hourly (
    relay_id       uuid not null references machine on delete cascade,
    probe_point_id uuid references probe_point on delete cascade,
    hour           timestamptz not null,
    total          int not null,
    ok             int not null,
    rtt_avg        real,
    rtt_p50        real
);
create unique index if not exists relay_reach_hourly_key on relay_reach_hourly (relay_id, coalesce(probe_point_id, '00000000-0000-0000-0000-000000000000'::uuid), hour);
create index if not exists relay_reach_hourly_hour on relay_reach_hourly (hour);
create index if not exists relay_reach_hourly_probe_point on relay_reach_hourly (probe_point_id) where probe_point_id is not null;

insert into relay_reach_hourly (relay_id, probe_point_id, hour, total, ok, rtt_avg, rtt_p50)
select relay_id, probe_point_id, date_trunc('hour', ts), count(*), count(*) filter (where ok),
       avg(rtt_ms) filter (where ok), percentile_cont(0.5) within group (order by rtt_ms) filter (where ok)
from relay_reach group by 1, 2, 3
on conflict (relay_id, coalesce(probe_point_id, '00000000-0000-0000-0000-000000000000'::uuid), hour) do update
  set total = excluded.total, ok = excluded.ok, rtt_avg = excluded.rtt_avg, rtt_p50 = excluded.rtt_p50;
