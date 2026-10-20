-- 访问策略：团队/节点的域名黑白名单（草稿 → 预览 → 发布 → 回滚）
-- 有效策略 = 节点策略（mode <> inherit）> 团队策略 > 默认 observe。

-- 每个范围一行：当前生效值 + 可选草稿。草稿为 null = 没有未发布的改动。
create table policy (
    scope       text not null check (scope in ('team', 'node')),
    scope_id    uuid not null,                       -- team.id 或 machine.id（多态，不设外键；保存时校验存在）
    mode        text not null default 'observe'
                check (mode in ('observe', 'allowlist', 'denylist', 'blocked', 'inherit')),
    domains     text[] not null default '{}',        -- 规范化后的域名；`=example.com` 表示精确匹配
    note        text not null default '',
    draft       jsonb,                               -- {mode, domains, note}
    revision    int  not null default 1,             -- 乐观锁：每次改草稿 / 发布 / 丢弃 +1
    updated_by  text,
    updated_at  timestamptz not null default now(),
    primary key (scope, scope_id),
    check (scope = 'node' or (mode <> 'inherit'))    -- 团队没有可继承的上一级
);

-- 发布版本：存完整快照（所有范围的生效值），回滚 = 把旧快照作为新版本发布。
create table policy_version (
    id          uuid primary key,
    seq         bigint generated always as identity unique,   -- 显示用版本号
    kind        text not null default 'publish' check (kind in ('publish', 'rollback')),
    rollback_of uuid references policy_version,
    actor       text not null,
    created_at  timestamptz not null default now(),
    snapshot    jsonb not null,                      -- [{scope, scope_id, mode, domains, note}]
    changes     jsonb not null default '[]',         -- 相对上一版的变更摘要（给人看）
    relays      jsonb not null default '[]'          -- [{relay_id, generation}]：发布时被 +1 的线路机与它的新代数
);
create index policy_version_created on policy_version (created_at desc);
