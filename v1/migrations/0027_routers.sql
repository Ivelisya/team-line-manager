-- 0027：软路由（OpenWrt / iStoreOS + OpenClash，mihomo 内核）纳入面板。
-- 路由经反向 SSH 隧道连中控（账号 fleet-tunnel，只许把中控本机 127.0.0.1:<tunnel_port> 转发到路由的 22），
-- 中控经隧道登录路由，每 30 秒跑一段只读采集脚本：最新快照放 router_status，指标 30 秒一点，按 [retention] raw_days 清理，
-- 另有小时汇总（保留期同机器的小时汇总：跟着路由记录走）。

create table router (
    id                   uuid primary key,
    name                 text not null,
    team_id              uuid references team,
    note                 text not null default '',
    -- 中控本机上分给这台路由的隧道端口（路由 ssh -R 127.0.0.1:<端口>:127.0.0.1:22）；sshd 的 tunnel-keys 脚本也只认这一段
    tunnel_port          int  not null unique check (tunnel_port between 23000 and 23999),
    -- 中控生成的隧道公钥（ssh-ed25519 <base64>）；私钥只出现在接入脚本里，不落库。null = 还没发过（或刚重新生成接入命令）
    tunnel_pubkey        text,
    -- 这把隧道公钥发出去（接入脚本被取走 / 一键部署装上）的时间：之后连上过才算「接入过」，才有离线告警
    tunnel_key_at        timestamptz,
    -- 路由 dropbear 的 ed25519 主机公钥（ssh-ed25519 <base64>）：第一次经隧道连上（或一键部署登录）时记录，之后严格核对；重新接入时清掉
    host_key             text,
    -- 一次性接入令牌（sha256），30 分钟有效，取过接入脚本即清空
    bootstrap_token_hash bytea,
    bootstrap_expires_at timestamptz,
    -- 一键部署（中控用账号密码登录路由装好隧道）：最近一次的目标、进度与结果。密码不落库
    deploy_state         text check (deploy_state in ('running', 'failed', 'done')),
    deploy_step          text,
    deploy_error         text,
    deploy_host          text,
    deploy_port          int,
    deploy_user          text,
    deploy_started_at    timestamptz,
    deploy_finished_at   timestamptz,
    created_at           timestamptz not null default now(),
    updated_at           timestamptz not null default now()
);
create unique index router_name_key on router (name);
create unique index router_bootstrap_token on router (bootstrap_token_hash) where bootstrap_token_hash is not null;
create index router_team_idx on router (team_id);

-- 最近一次采集：reachable = 这次连上了；data = 最近一次成功采集的完整快照（接口的 detail），连不上时保留上一份
create table router_status (
    router_id   uuid primary key references router on delete cascade,
    ts          timestamptz not null,
    reachable   boolean not null,
    error       text,
    data        jsonb,
    last_ok_at  timestamptz
);

-- 30 秒一点。读数取不到的为 null（第一次采样没有前一次算不出速率、OpenClash 没在跑……）
create table router_metric (
    router_id      uuid not null references router on delete cascade,
    ts             timestamptz not null,
    cpu_pct        real,
    load1          real,
    mem_used       bigint,
    mem_total      bigint,
    disk_used      bigint,
    disk_total     bigint,
    conntrack      int,
    conntrack_max  int,
    temp_c         real,
    wan_rx_bps     bigint,
    wan_tx_bps     bigint,
    proxy_up_bps   bigint,
    proxy_down_bps bigint,
    conns          int,
    clients        int,
    exits_total    int,
    exits_alive    int,
    primary key (router_id, ts)
);
create index router_metric_ts_brin on router_metric using brin (ts);

-- 小时汇总：同列取平均，CPU 与速率另记最大值
create table router_metric_hourly (
    router_id          uuid not null references router on delete cascade,
    hour               timestamptz not null,
    samples            int not null,
    cpu_pct            real,
    cpu_pct_max        real,
    load1              real,
    mem_used           bigint,
    mem_total          bigint,
    disk_used          bigint,
    disk_total         bigint,
    conntrack          real,
    conntrack_max      int,
    temp_c             real,
    wan_rx_bps         bigint,
    wan_rx_bps_max     bigint,
    wan_tx_bps         bigint,
    wan_tx_bps_max     bigint,
    proxy_up_bps       bigint,
    proxy_up_bps_max   bigint,
    proxy_down_bps     bigint,
    proxy_down_bps_max bigint,
    conns              real,
    clients            real,
    exits_total        real,
    exits_alive        real,
    primary key (router_id, hour)
);

-- 每个出口 30 秒一点。alive / delay_ms 只在测了延迟的那一点有（每 5 分钟测一次全部出口），其余为 null；
-- 连接数、速率（同一连接两次之差累加，新连接全算、关掉的连接最后一段会漏掉：估计值）、客户端数每点都有
create table router_exit_metric (
    router_id  uuid not null references router on delete cascade,
    ts         timestamptz not null,
    exit       text not null,
    alive      boolean,
    delay_ms   int,
    conns      int not null default 0,
    up_bps     bigint,
    down_bps   bigint,
    clients    int not null default 0,
    primary key (router_id, exit, ts)
);
create index router_exit_metric_ts_brin on router_exit_metric using brin (ts);

create table router_exit_metric_hourly (
    router_id     uuid not null references router on delete cascade,
    exit          text not null,
    hour          timestamptz not null,
    samples       int not null,
    tests         int not null,          -- 这一小时测了几次延迟
    tests_ok      int not null,          -- 其中通的次数
    delay_ms      real,                  -- 通的那几次的平均延迟
    conns         real,
    up_bps        bigint,
    up_bps_max    bigint,
    down_bps      bigint,
    down_bps_max  bigint,
    clients       real,
    primary key (router_id, exit, hour)
);
