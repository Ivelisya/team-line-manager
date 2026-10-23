-- 0083：放置矩阵（线路机 × 节点的网络往返，见 placement.rs、DESIGN.md「放置矩阵」）。
-- 线路机上的 agent 每 5 分钟对全部在用 VPS 节点各测一次（ICMP，用不了时 TCP），随上报带回；这里只存小时汇总，保留 30 天。
-- samples / ok：这小时测了几轮、几轮通；p50_ms / max_ms：通了的那几轮往返的中位数与最大值；
-- rtts：通了的那几轮的往返（每小时十几个），用来重算 p50 与「近 24 小时」的中位数，不对外。
create table relay_node_rtt_hourly (
    relay_id  uuid not null references machine on delete cascade,
    node_id   uuid not null references machine on delete cascade,
    hour      timestamptz not null,
    samples   int not null,
    ok        int not null,
    p50_ms    real,
    max_ms    real,
    rtts      real[] not null default '{}',
    primary key (relay_id, node_id, hour)
);
create index relay_node_rtt_hourly_hour on relay_node_rtt_hourly (hour);
create index relay_node_rtt_hourly_node on relay_node_rtt_hourly (node_id);
