-- 0046：探测点（probe point）。线路机客户入口的连通性改成可以从靠近客户的地方测（见 DESIGN.md「探测点」、probe_points/）。
-- 1. probe_point：一台跑 `fleet-agent probe` 的小机器（如香港），每 30 秒向中控取目标、建连、上报。
--    token_hash 是它上报用的令牌（sha256；null = 还没装好或已重装作废）；bootstrap_* 是「在探测点上执行的一次性安装命令」的令牌
--    （30 分钟、只能用一次，同软路由的接入命令）；install_* 是中控经 SSH 安装的进度（同软路由的一键部署）；
--    upgrade_sha256：要它换成 dist 里的这个 fleet-agent（`fleetctl probe upgrade`，探测点下次取目标时自己下载替换，换好后清空）。
-- 2. relay_reach.probe_point_id：这一行是谁测的（null = 中控自己，reach.rs）；rtt_ms：建连耗时（探测点记内核量的往返时间，中控那条不记）。
--    查询都按来源过滤（主探测来源，见 settings 的 reach_source），索引带上来源：0038 的 (relay_id, ts desc)、0043 的 (relay_id, port, ts desc)
--    换成带 probe_point_id 的（btree 里 null 也能按 is null 走索引）。表小（保留 2 天、每台每来源每 30 秒一行），建索引是毫秒级。
-- 删除探测点时它的检查记录一起删（on delete cascade）。
create table probe_point (
    id                   uuid primary key,
    name                 text not null unique,
    enabled              boolean not null default true,
    note                 text not null default '',
    token_hash           bytea,
    bootstrap_token_hash bytea,
    bootstrap_expires_at timestamptz,
    install_state        text check (install_state in ('running', 'done', 'failed')),
    install_step         text,
    install_error        text,
    install_host         text,
    install_port         int,
    install_started_at   timestamptz,
    install_finished_at  timestamptz,
    upgrade_sha256       text,
    last_seen_at         timestamptz,
    last_ip              inet,
    version              text,
    created_at           timestamptz not null default now(),
    updated_at           timestamptz not null default now()
);
create unique index probe_point_token on probe_point (token_hash) where token_hash is not null;
create unique index probe_point_bootstrap on probe_point (bootstrap_token_hash) where bootstrap_token_hash is not null;

alter table relay_reach add column probe_point_id uuid references probe_point on delete cascade;
alter table relay_reach add column rtt_ms real;

drop index if exists relay_reach_relay_ts;
drop index if exists relay_reach_relay_port_ts;
create index relay_reach_relay_src_ts on relay_reach (relay_id, probe_point_id, ts desc);
create index relay_reach_relay_src_port_ts on relay_reach (relay_id, probe_point_id, port, ts desc);
-- 删除探测点时级联删它的行
create index relay_reach_probe_point on relay_reach (probe_point_id) where probe_point_id is not null;
