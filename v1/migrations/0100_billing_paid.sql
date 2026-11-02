-- 0100：计费台账记「每一期收了没有」。每个节点（计费名额 billing_slot）从开始收费日起每满一个月是一期（batches.rs 的 period，月底对齐），
-- 每一期只有两种状态：已收（这里有一行）/ 没收（没有）。按月看时列出起点落在这个月的那一期。
-- 按名额记：替换节点时名额与开始收费日不变，收费状态跟着名额走。不预填数据。可重复执行。
create table if not exists billing_paid (
    slot_id      uuid not null references billing_slot on delete cascade,
    -- 这一期的起点（开始收费日加整月数）
    period_start date not null,
    paid_at      timestamptz not null default now(),
    primary key (slot_id, period_start)
);

-- 增减记录加两种：paid（标记已收）、unpaid（改回没收）。约束名是 0097 建表时自动起的
alter table billing_event drop constraint if exists billing_event_kind_check;
alter table billing_event add constraint billing_event_kind_check
    check (kind in ('init', 'add', 'remove', 'swap', 'edit', 'manual_add', 'manual_end', 'exempt', 'paid', 'unpaid'));

-- 已有的修改记录里「起算日」改成「开始收费日」（界面上不再出现「起算日」）
update billing_event set detail = jsonb_set(detail, '{what}', to_jsonb(replace(detail ->> 'what', '起算日', '开始收费日')))
where kind = 'edit' and detail ->> 'what' like '%起算日%';
