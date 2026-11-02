-- 0104：计费台账「上架即记账」，去掉 24 小时待定（ledger.rs）。新节点进团队这一轮就建名额，开始收费日 = 购买日；
-- 补位只认替换节点作业（ledger::take_over），不再按时间猜配对。billing_pending 不再用：现有待定行按同一条规则转成名额后删表。
-- 转换（同 ledger::plan 的「加入」）：节点还在这个团队、仍可计费（同 ledger::BILLABLE）、没占着没结束的名额的，建名额并记 add；
--   开始收费日 = 订阅（自己的，没有就宿主机的）到期日往前推一个周期（PG 的 date - interval 'n month' 同 add_months，月底对齐），晚于今天取今天；
--   没有订阅的 = 第一次看到它那天（seen_at，组织时区）。事件时间 = seen_at。
--   地区：库里查不了离线 IP 库，按手动国家或 IP 画像缓存的国家代码，US = 美区，其余写代码、查不到写「未知」（可在台账里改）。
-- 已不可计费的待定行直接丢掉（原来也是不计费、不记事件）。生产执行时 0 行。可重复执行（表删了第二次什么都不做）。
do $$
begin
    if to_regclass('billing_pending') is null then
        return;
    end if;
    with conv as (
    select gen_random_uuid() id, p.team_id, p.machine_id, p.seen_at,
           display_name(m.kind::text, m.label, t.name, m.number) name,
           least(coalesce(sub.due_date - make_interval(months => case sub.cycle when 'quarterly' then 3 when 'semiannual' then 6 when 'yearly' then 12 else 1 end),
                          (p.seen_at at time zone org_tz())::date)::date,
                 (now() at time zone org_tz())::date) start_date,
           coalesce(case upper(coalesce(m.country, ic.country)) when 'US' then '美区' else upper(coalesce(m.country, ic.country)) end, '未知') region
    from billing_pending p
    join machine m on m.id = p.machine_id and m.team_id = p.team_id
    join team t on t.id = m.team_id
    left join lateral (select s.due_date, s.cycle::text cycle from subscription_eff s where s.machine_id in (m.id, m.host_id)
                       order by s.machine_id = m.id desc limit 1) sub on true
    left join ip_intel_cache ic on ic.ip = m.primary_ip
    where m.kind in ('vps', 'proxy') and m.lifecycle = 'active' and not t.spare
      and not exists (select 1 from billing_exempt x where x.machine_id = m.id)
      and not exists (select 1 from billing_slot s where s.holder_id = m.id and s.end_date is null)
    ), slots as (
        insert into billing_slot (id, team_id, region, start_date, holder_id, holder_name)
        select id, team_id, region, start_date, machine_id, name from conv
    )
    insert into billing_event (id, team_id, slot_id, at, kind, machine_name, detail)
    select gen_random_uuid(), team_id, id, seen_at, 'add', name, jsonb_build_object('region', region, 'start_date', to_char(start_date, 'YYYY-MM-DD'))
    from conv;
    drop table billing_pending;
end $$;
