-- 团队线路管理 v1 — 初始结构
-- 约定：主键 uuid（应用层生成 UUIDv7）；时间一律 timestamptz；唯一性尽量用部分唯一索引在库里保证。

-- ── 组织与目录 ─────────────────────────────────────────────────────────────
create table team (
    id          uuid primary key,
    name        text not null unique,                 -- 显示名，如「团队1」
    sort_order  int  not null default 0,
    created_at  timestamptz not null default now()
);

create table provider (
    id          uuid primary key,
    name        text not null unique,                 -- ATTVPS / NodeMach / GoMami …
    code        text not null unique,                 -- ATT / NM / GM（显示用缩写）
    panel_url   text,
    timezone    text not null default 'UTC',          -- 供应商账期时区
    created_at  timestamptz not null default now()
);

create type billing_cycle as enum ('monthly', 'quarterly', 'semiannual', 'yearly');

create table plan (
    id              uuid primary key,
    provider_id     uuid not null references provider,
    name            text not null,                    -- 规范名，如「ATTVPS 基础版」
    cpu_cores       int,
    mem_mb          int,
    disk_gb         int,
    bandwidth_mbps  int,
    traffic_gb      int,                              -- null = 不限
    price_cents     int  not null,
    currency        text not null default 'USD',
    cycle           billing_cycle not null default 'monthly',
    notes           text not null default '',
    archived_at     timestamptz,
    created_at      timestamptz not null default now(),
    unique (provider_id, name)
);

-- ── 机器 ───────────────────────────────────────────────────────────────────
-- kind: relay = 线路机；vps = 我们能登录的落地机（装 ssserver + agent）；proxy = 供应商提供的上游代理（无登录、无 agent）
create type machine_kind      as enum ('relay', 'vps', 'proxy');
create type machine_lifecycle as enum ('draft', 'provisioning', 'active', 'retiring', 'retired', 'failed');

create table machine (
    id               uuid primary key,
    kind             machine_kind not null,
    team_id          uuid references team,
    number           int  check (number between 1 and 999),
    label            text,                            -- 线路机名（线路机2）；节点不用，节点显示名由 团队+编号 计算
    primary_ip       inet not null,                   -- 出口 IP；查重键
    ssh_port         int  not null default 22 check (ssh_port between 1 and 65535),
    provider_id      uuid references provider,
    plan_id          uuid references plan,
    service_ref      text,                            -- 供应商服务号
    lifecycle        machine_lifecycle not null default 'draft',
    lifecycle_reason text,
    note             text not null default '',
    revision         int  not null default 1,        -- 乐观锁
    created_at       timestamptz not null default now(),
    updated_at       timestamptz not null default now(),
    activated_at     timestamptz,
    retired_at       timestamptz,
    check (kind = 'relay' or (team_id is not null and number is not null)),
    check (kind <> 'relay' or label is not null)
);
create unique index machine_ip_live     on machine (primary_ip)        where lifecycle <> 'retired';
create unique index machine_number_live on machine (team_id, number)   where kind <> 'relay' and lifecycle <> 'retired';
create unique index machine_label_live  on machine (label)             where kind = 'relay' and lifecycle <> 'retired';
create index machine_lifecycle_idx      on machine (lifecycle);

-- 节点到上游的出站参数（vps：本机 ssserver；proxy：供应商 SOCKS5/HTTP）。秘密字段由应用层加密。
create table node_outbound (
    machine_id   uuid primary key references machine on delete cascade,
    protocol     text not null check (protocol in ('shadowsocks', 'socks5', 'http')),
    host         text not null,                       -- vps 为 IP；proxy 可为域名
    port         int  not null check (port between 1 and 65535),
    method       text,                                -- shadowsocks 加密方式
    username     text,
    secret_enc   bytea not null,                      -- 密码（加密）
    updated_at   timestamptz not null default now()
);

-- 线路机参数与下发代数
create table relay (
    machine_id          uuid primary key references machine on delete cascade,
    public_host         text not null,                -- 客户端连接用的地址
    port_min            int  not null default 20000,
    port_max            int  not null default 29999,
    reality_dest        text not null default 'www.cloudflare.com:443',
    reality_sni         text not null default 'www.cloudflare.com',
    reality_private_enc bytea not null,
    reality_public_key  text not null,
    min_client_ver      text,
    generation          bigint not null default 0,    -- 期望状态代数：入口任何变化 +1
    applied_generation  bigint not null default 0,    -- agent 回报已应用的代数
    applied_at          timestamptz,
    apply_error         text,
    check (port_min < port_max)
);

-- ── 入口（线路机上的一条 VLESS REALITY 入站 → 节点）──────────────────────
create type entry_lifecycle as enum ('pending', 'active', 'disabled', 'removing', 'removed');

create table entry (
    id           uuid primary key,
    relay_id     uuid not null references machine,
    node_id      uuid not null references machine,
    port         int  not null check (port between 1024 and 65535),
    client_uuid  uuid not null,
    short_id     text not null,
    lifecycle    entry_lifecycle not null default 'pending',
    changed_gen  bigint not null default 0,            -- 最近一次改动所在的线路机代数；applied_generation ≥ 它即已生效
    apply_error  text,
    created_at   timestamptz not null default now(),
    updated_at   timestamptz not null default now(),
    removed_at   timestamptz
);
create unique index entry_port_live      on entry (relay_id, port)    where lifecycle <> 'removed';
create unique index entry_node_relay_live on entry (node_id, relay_id) where lifecycle <> 'removed';
create index entry_node_idx              on entry (node_id);

-- ── 边缘 agent ─────────────────────────────────────────────────────────────
create table agent_token (
    id            uuid primary key,
    machine_id    uuid not null references machine on delete cascade,
    token_hash    bytea not null unique,              -- sha256(token)
    created_at    timestamptz not null default now(),
    revoked_at    timestamptz,
    last_used_at  timestamptz
);
create index agent_token_machine on agent_token (machine_id) where revoked_at is null;

create table agent_state (
    machine_id      uuid primary key references machine on delete cascade,
    agent_version   text,
    boot_id         text,
    last_seq        bigint not null default 0,
    last_report_at  timestamptz,
    info            jsonb not null default '{}'       -- 系统信息（内核、CPU 型号、xray 版本…）
);

-- ── 时序（按天分区；分区由 fleet-server 每天维护，过期整区删除）──────────────
create table host_metric (
    machine_id    uuid not null,
    ts            timestamptz not null,
    cpu_pct       real,
    load1         real,
    mem_used      bigint,
    mem_total     bigint,
    disk_used     bigint,
    disk_total    bigint,
    net_rx_bytes  bigint,                             -- 开机以来累计
    net_tx_bytes  bigint,
    tcp_conns     int
) partition by range (ts);
create index on host_metric (machine_id, ts desc);

create table entry_traffic (
    entry_id    uuid not null,
    ts          timestamptz not null,
    up_bytes    bigint not null,                      -- 本采样间隔增量
    down_bytes  bigint not null,
    conns       int
) partition by range (ts);
create index on entry_traffic (entry_id, ts desc);

create table probe_result (
    entry_id    uuid not null,
    ts          timestamptz not null,
    kind        text not null check (kind in ('tcp', 'egress')),
    ok          boolean not null,
    latency_ms  real,
    detail      text                                  -- 失败原因 / 出口 IP
) partition by range (ts);
create index on probe_result (entry_id, kind, ts desc);

create table access_event (
    entry_id   uuid not null,
    ts         timestamptz not null,
    src_ip     inet,
    dest_host  text not null,
    dest_port  int,
    network    text,                                  -- tcp / udp
    blocked    boolean not null default false
) partition by range (ts);
create index on access_event (entry_id, ts desc);

-- 小时汇总（长期保留，不分区）
create table host_metric_hourly (
    machine_id  uuid not null,
    hour        timestamptz not null,
    cpu_avg     real, cpu_max real, mem_used_avg bigint, mem_total_max bigint, disk_used_max bigint, disk_total_max bigint,
    net_rx      bigint, net_tx bigint,
    primary key (machine_id, hour)
);
create table entry_traffic_hourly (
    entry_id    uuid not null,
    hour        timestamptz not null,
    up_bytes    bigint not null,
    down_bytes  bigint not null,
    conns_max   int,
    primary key (entry_id, hour)
);
create table probe_hourly (
    entry_id     uuid not null,
    hour         timestamptz not null,
    kind         text not null,
    total        int not null,
    ok           int not null,
    latency_avg  real,
    primary key (entry_id, hour, kind)
);

-- ── 账单 ───────────────────────────────────────────────────────────────────
create table subscription (
    machine_id    uuid primary key references machine on delete cascade,
    price_cents   int  not null,
    currency      text not null default 'USD',
    cycle         billing_cycle not null default 'monthly',
    due_date      date not null,
    billing_tz    text not null default 'Asia/Shanghai',
    auto_renew    boolean not null default false,
    remind_days   int  not null default 7,
    traffic_gb    int,                                -- 覆盖套餐的流量上限；null = 用套餐
    traffic_reset_day int check (traffic_reset_day between 1 and 31),
    updated_at    timestamptz not null default now()
);
create table renewal (
    id            uuid primary key,
    machine_id    uuid not null references machine on delete cascade,
    old_due       date not null,
    new_due       date not null,
    amount_cents  int,
    note          text not null default '',
    actor         text not null,
    created_at    timestamptz not null default now()
);

-- ── 作业 ───────────────────────────────────────────────────────────────────
create type job_state as enum ('queued', 'running', 'waiting', 'succeeded', 'failed', 'cancelling', 'cancelled');

create table job (
    id           uuid primary key,
    kind         text not null,                       -- provision_node / retire / renumber / purge / …
    target_id    uuid,                                -- 通常是 machine.id
    params       jsonb not null default '{}',
    state        job_state not null default 'queued',
    step         int  not null default 0,             -- 当前步序号
    error        text,
    created_by   text not null,
    created_at   timestamptz not null default now(),
    started_at   timestamptz,
    finished_at  timestamptz,
    run_after    timestamptz not null default now(),
    lease_owner  text,
    lease_until  timestamptz
);
create index job_runnable on job (run_after) where state in ('queued', 'running', 'waiting', 'cancelling');
-- 一台机器同一时间只允许一个进行中的作业
create unique index job_one_active_per_target on job (target_id)
    where target_id is not null and state in ('queued', 'running', 'waiting', 'cancelling');

create table job_step (
    job_id       uuid not null references job on delete cascade,
    idx          int  not null,
    key          text not null,
    state        text not null default 'pending' check (state in ('pending', 'running', 'done', 'skipped', 'failed', 'compensated')),
    started_at   timestamptz,
    finished_at  timestamptz,
    output       jsonb,
    error        text,
    primary key (job_id, idx)
);
create table job_log (
    id      bigserial primary key,
    job_id  uuid not null references job on delete cascade,
    ts      timestamptz not null default now(),
    level   text not null default 'info',
    msg     text not null
);
create index on job_log (job_id, id);

-- ── 告警 ───────────────────────────────────────────────────────────────────
create type incident_state as enum ('firing', 'resolved');
create type severity as enum ('info', 'warning', 'critical');

create table incident (
    id            uuid primary key,
    rule          text not null,                      -- node_down / egress_fail / renewal_due / …
    subject_id    uuid not null,                      -- machine 或 entry
    severity      severity not null,
    state         incident_state not null default 'firing',
    summary       text not null,
    detail        jsonb not null default '{}',
    opened_at     timestamptz not null default now(),
    last_seen_at  timestamptz not null default now(),
    resolved_at   timestamptz,
    ack_at        timestamptz,
    ack_by        text
);
create unique index incident_open_one on incident (rule, subject_id) where state = 'firing';
create index incident_subject on incident (subject_id, opened_at desc);

create table silence (
    id          uuid primary key,
    subject_id  uuid,                                 -- null = 全部
    rule        text,                                 -- null = 全部规则
    starts_at   timestamptz not null,
    ends_at     timestamptz not null,
    is_maintenance boolean not null default false,    -- 维护：连健康判定一起暂停
    comment     text not null default '',
    created_by  text not null,
    created_at  timestamptz not null default now(),
    deleted_at  timestamptz,
    check (ends_at > starts_at)
);

create table notify_channel (
    id          uuid primary key,
    kind        text not null check (kind in ('telegram', 'wecom', 'email', 'webhook')),
    name        text not null,
    config_enc  bytea not null,
    min_severity severity not null default 'warning',
    enabled     boolean not null default true,
    created_at  timestamptz not null default now()
);
create table notify_log (
    id          bigserial primary key,
    channel_id  uuid references notify_channel on delete set null,
    incident_id uuid references incident on delete set null,
    ts          timestamptz not null default now(),
    ok          boolean not null,
    error       text
);

-- ── 账户与审计 ─────────────────────────────────────────────────────────────
create table app_user (
    id             uuid primary key,
    username       text not null unique,
    password_hash  text not null,                     -- argon2id PHC 串
    totp_secret_enc bytea,
    created_at     timestamptz not null default now(),
    disabled_at    timestamptz
);
create table session (
    id_hash      bytea primary key,                   -- sha256(cookie)
    user_id      uuid not null references app_user on delete cascade,
    created_at   timestamptz not null default now(),
    expires_at   timestamptz not null,
    last_seen_at timestamptz not null default now(),
    ip           inet,
    user_agent   text
);
create table api_key (
    id            uuid primary key,
    name          text not null,
    prefix        text not null unique,
    key_hash      bytea not null unique,
    scopes        text[] not null default '{}',
    created_at    timestamptz not null default now(),
    expires_at    timestamptz,
    revoked_at    timestamptz,
    last_used_at  timestamptz
);
create table audit_log (
    id           bigserial primary key,
    ts           timestamptz not null default now(),
    actor        text not null,
    action       text not null,
    target_kind  text,
    target_id    uuid,
    detail       jsonb not null default '{}'
);
create index on audit_log (target_id, id desc);
