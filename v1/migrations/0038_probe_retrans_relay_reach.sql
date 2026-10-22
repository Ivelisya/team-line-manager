-- 0038：探测口径修正。
-- 1. probe_result.retrans：TCP 探测建连时 SYN 重发了几次（新 agent 报，旧 agent 为 null）。latency_ms 改为内核量的往返时间，
--    不再含 1 s / 3 s 的重发等待，丢包另记在这里。只加可空列，不改已有行。
-- 2. relay_reach：中控 → 线路机的连通性（线路机卡片的心跳格），每 30 秒一行，只记通不通。
--    中控自己的网络不通时整轮不写（见 reach.rs），保留 2 天（db::cleanup_retention）。
alter table probe_result add column if not exists retrans smallint;

create table if not exists relay_reach (
    relay_id  uuid not null references machine on delete cascade,
    ts        timestamptz not null,
    ok        boolean not null,
    detail    text                                  -- 失败原因
);
create index if not exists relay_reach_relay_ts on relay_reach (relay_id, ts desc);
create index if not exists relay_reach_ts on relay_reach (ts);
