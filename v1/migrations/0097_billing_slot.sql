-- 0097：按节点数向团队收费的自动计费台账，替换 0095 的手动客户批次账（生产上那两张表是空的，直接删）。
-- 计费名额（billing_slot）= 团队里一个按月计费的节点位置：起算日 + 地区 + 当前占用它的节点。替换节点时名额不变（新节点接过去、起算日不变），
-- 净增一个节点 = 新名额，净减一个 = 名额结束。批次、到期、月度应收都从名额派生（batches.rs），库里不存。
-- 对账（ledger.rs，每 60 秒）：节点离开 → 名额空出待定；新节点加入 → 待定（billing_pending）；24 小时内两边配上 = 替换，配不上 = 减少 / 新增。
-- customer_page（客户页链接）不变。可重复执行。
drop table if exists customer_batch_period;
drop table if exists customer_batch;

create table if not exists billing_slot (
    id               uuid primary key,
    team_id          uuid not null references team on delete cascade,
    -- 美区 / 墨西哥 / 菲律宾 …（按节点国家，1–20 字）
    region           text not null,
    -- 起算日（北京时间日期）：从这天起每月同日续一期（月底对齐）
    start_date       date not null,
    -- 当前占用的节点；手动名额与空出待定的为 null。机器被物理删除时置空（对账把它当空出）
    holder_id        uuid references machine on delete set null,
    -- 占用者离开的时刻：24 小时内有节点加入就接过去，否则名额在这一天（北京时间）结束
    vacated_at       timestamptz,
    -- 占用者（空出后是原占用者）最近的显示名：在团队里时每轮对账刷新，空出后留着，替换 / 减少记录里写它（进了备用改了名也还是原来的名字）
    holder_name      text,
    -- 空出时的占用者（不加外键：机器删了也留着）：原节点 24 小时内回来（撤销退役、取消不计费）就原样接回
    prev_holder_id   uuid,
    -- 结束日（北京时间）；null = 没结束。名额在「期起点 < 结束日」的期里计费
    end_date         date,
    -- 手动名额（系统外的节点，如墨西哥家宽）：不自动结束，只能手动结束
    manual           boolean not null default false,
    note             text not null default '',
    created_at       timestamptz not null default now()
);
create unique index if not exists billing_slot_holder on billing_slot (holder_id) where holder_id is not null and end_date is null;
create index if not exists billing_slot_team on billing_slot (team_id, start_date);

-- 新加入、还没定是「替换」还是「新增」的节点（24 小时内有名额空出就接过去）
create table if not exists billing_pending (
    machine_id uuid primary key references machine on delete cascade,
    team_id    uuid not null references team on delete cascade,
    seen_at    timestamptz not null default now()
);

-- 标了不计费的节点（自用、测试、送的）：对账把它当离开处理
create table if not exists billing_exempt (
    machine_id uuid primary key references machine on delete cascade,
    created_at timestamptz not null default now()
);

-- 增减记录：名字按当时的显示名快照存，一句话说明在读取时按 kind + detail 拼（api/batches.rs）
create table if not exists billing_event (
    id           uuid primary key,
    team_id      uuid not null references team on delete cascade,
    slot_id      uuid,
    at           timestamptz not null,
    kind         text not null check (kind in ('init', 'add', 'remove', 'swap', 'edit', 'manual_add', 'manual_end', 'exempt')),
    -- swap：machine_name = 新节点，other_name = 换下来的节点
    machine_name text,
    other_name   text,
    detail       jsonb not null default '{}'
);
create index if not exists billing_event_team_at on billing_event (team_id, at desc);
