-- IP 情报：画像缓存、节点来源画像基线、增量水位
-- 离线库在 fleet-server 进程内查询（crates/fleet-ipintel），这里只存「查过的结果」和「每个节点见过哪些运营商 / 城市 / 接入方式」。

-- 画像缓存：db_version 是各库版本拼成的串，库换版后旧行不再命中（读取时按版本过滤，过期行由后台清理）
create table ip_intel_cache (
    ip          inet primary key,
    isp         text not null default '',
    asn         int,
    as_org      text,
    country     text,
    province    text,
    city        text,
    conn_type   text not null default 'unknown' check (conn_type in ('residential', 'mobile', 'datacenter', 'vpn', 'unknown')),
    confidence  text not null default 'low',
    reason      text not null default '',
    db_version  text not null,
    updated_at  timestamptz not null default now()
);
create index ip_intel_cache_updated on ip_intel_cache (updated_at);

-- 节点来源画像：每个 (节点, 维度, 值) 第一次 / 最近一次出现和累计连接数。
-- dim：isp 运营商；city 城市（值为 国家|省|市）；conn_type 接入方式；ip 机房 / VPN 来源 IP（只记这两类，用于「首次出现」判定）。
-- 只记外部来源（跳过非公网地址和我方线路机 / 节点自己的 IP）。
-- flag：学习期（节点第一条记录起 learn_days 天）之后第一次出现的 isp / city / ip 值标 open，
--       最近一次出现超过 24 小时后改 done（done 之后同一个值再出现也不再告警）；学习期内出现的为 null。
create table node_source_seen (
    node_id     uuid not null references machine on delete cascade,
    dim         text not null check (dim in ('isp', 'city', 'conn_type', 'ip')),
    value       text not null,
    first_seen  timestamptz not null,
    last_seen   timestamptz not null,
    conns       bigint not null default 0,
    flag        text check (flag in ('open', 'done')),
    primary key (node_id, dim, value)
);
create index node_source_seen_open on node_source_seen (last_seen) where flag = 'open';

-- 后台循环的小状态（目前只有来源汇总的水位：此前已关闭的小时都已计入 conns）
create table ipintel_state (
    key  text primary key,
    ts   timestamptz not null
);
