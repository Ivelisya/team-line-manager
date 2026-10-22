-- 0020：流量额度按网卡计量。供应商按网卡流量收费、各家算法不同：min = 本周期入、出总量取小（如 GOMAMI），max = 取大，
-- sum = 入出相加，out = 只算出站。不知道的供应商默认 max（不会比入、出、取小、取大这几种算法少算）
alter table provider add column traffic_rule text not null default 'max' check (traffic_rule in ('min', 'max', 'sum', 'out'));
-- 按供应商面板校准：在 traffic_used_base_at 这一刻面板上本周期已用 traffic_used_base 字节（机器周期中途接入，之前的用量量不到）。
-- 校准时刻在当前周期内时，已用 = 校准值 + 之后的网卡流量；进入新周期后不再生效
alter table subscription add column traffic_used_base bigint check (traffic_used_base >= 0),
    add column traffic_used_base_at timestamptz,
    add constraint subscription_traffic_used_base check ((traffic_used_base is null) = (traffic_used_base_at is null));
