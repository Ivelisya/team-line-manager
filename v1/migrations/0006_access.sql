-- 访问监控：小时汇总、来源汇总、首次出现目标
-- access_event（分区，保留 access_days 天）是原始记录；下面三张表长期保留 90 天，由 fleet-server 每 5 分钟重算最近两小时。

-- 全局按时间倒序取最近记录要用到（按入口查已有 (entry_id, ts desc)）；分区表上的索引会自动带到每个分区。
create index access_event_ts on access_event (ts desc);

-- 目标类型：IPv4 / IPv6 字面量为 ip，其余 domain
create function access_kind(h text) returns text language sql immutable parallel safe as $$
    select case when h ~ '^[0-9.]+$' or position(':' in h) > 0 then 'ip' else 'domain' end
$$;

-- 随机子域名合并：注册域名之下的标签含 UUID、或 ≥8 位纯十六进制且 ≥2 位数字、或 ≥20 位字母数字且 ≥4 位数字，
-- 整条合并为 `*.<注册域名>`。注册域名用简化规则：取最后两段；倒数第二段是 com/net/org/gov/edu/co 且最后一段是两字母国家码时取最后三段。
-- 输入应已小写、去尾点。IP 原样返回。
create function access_collapse(h text) returns text language plpgsql immutable parallel safe as $$
declare
    labels text[];
    n      int;
    reg    int := 2;
    i      int;
    l      text;
begin
    if h ~ '^[0-9.]+$' or position(':' in h) > 0 then
        return h;
    end if;
    labels := string_to_array(h, '.');
    n := cardinality(labels);
    if n < 3 then
        return h;
    end if;
    if labels[n - 1] in ('com', 'net', 'org', 'gov', 'edu', 'co') and length(labels[n]) = 2 then
        reg := 3;
    end if;
    if n <= reg then
        return h;
    end if;
    for i in 1 .. n - reg loop
        l := labels[i];
        if l ~ '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}'
           or (length(l) >= 8 and l ~ '^[0-9a-f]+$' and length(regexp_replace(l, '[^0-9]', '', 'g')) >= 2)
           or (length(l) >= 20 and l ~ '^[0-9a-z]+$' and length(regexp_replace(l, '[^0-9]', '', 'g')) >= 4)
        then
            return '*.' || array_to_string(labels[n - reg + 1 : n], '.');
        end if;
    end loop;
    return h;
end
$$;

-- 按 (节点, 线路机, 小时, 目标) 汇总。sources = 该小时内去重来源 IP 数。
create table access_hourly (
    node_id    uuid not null,
    relay_id   uuid not null,
    hour       timestamptz not null,
    dest_host  text not null,
    kind       text not null check (kind in ('domain', 'ip')),
    conns      int  not null,
    blocked    int  not null default 0,
    sources    int  not null default 0,
    primary key (node_id, hour, relay_id, dest_host)
);
create index access_hourly_hour  on access_hourly (hour);
create index access_hourly_relay on access_hourly (relay_id, hour);

create table access_source_hourly (
    node_id  uuid not null,
    hour     timestamptz not null,
    src_ip   inet not null,
    conns    int  not null,
    primary key (node_id, hour, src_ip)
);
create index access_source_hourly_hour on access_source_hourly (hour);

-- 节点第一次出现某目标的时间（「新出现的目标」的数据源）；total 为累计连接数
create table access_target_seen (
    node_id     uuid not null,
    dest_host   text not null,
    first_seen  timestamptz not null,
    last_seen   timestamptz not null,
    total       bigint not null default 0,
    primary key (node_id, dest_host)
);
create index access_target_seen_first on access_target_seen (first_seen desc);
create index access_target_seen_last  on access_target_seen (last_seen);
