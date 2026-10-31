-- 0093：客户批次账与客户到期页。与供应商订阅（subscription.due_date）完全无关：这是「卖给客户的节点什么时候到期」的另一本账。
-- 一个批次 = 首购一段 + 每次续期一段（段与段首尾相接，续期段起点 = 上一段终点；写入时校验不重叠）。
-- 每个团队一个只读分享链接（customer_page），客户不登录；页面不出现 IP、供应商、价格、节点名。可重复执行。
create table if not exists customer_batch (
    id          uuid primary key,
    team_id     uuid not null references team on delete cascade,
    -- 第 NN 批，团队内唯一；新建默认 = 团队内最大 + 1
    seq         int  not null check (seq between 1 and 999),
    -- 美区 / 墨西哥 / 菲律宾 …（1–20 字，接口里校验）
    region      text not null,
    -- 客户页标题下一行说明，≤ 60 字（接口里校验）
    note        text not null default '',
    created_at  timestamptz not null default now(),
    updated_at  timestamptz not null default now(),
    unique (team_id, seq)
);

create table if not exists customer_batch_period (
    id          uuid primary key,
    batch_id    uuid not null references customer_batch on delete cascade,
    start_date  date not null,
    end_date    date not null check (end_date > start_date),
    -- 计费月数：节点月 = count × months
    months      int  not null check (months between 1 and 36),
    -- 这一段的节点个数（续期可以变，如 5 → 4）
    count       int  not null check (count between 1 and 999),
    created_at  timestamptz not null default now()
);
create index if not exists customer_batch_period_batch_idx on customer_batch_period (batch_id, start_date);

-- 团队的客户页链接。令牌 32 字节随机 base64url：token_hash = sha256 用来查找，token_enc 用 SecretBox 加密存着，管理端随时能再复制链接
create table if not exists customer_page (
    team_id      uuid primary key references team on delete cascade,
    token_hash   bytea not null unique,
    token_enc    bytea not null,
    enabled      boolean not null default true,
    created_at   timestamptz not null default now(),
    token_at     timestamptz not null default now(),
    last_view_at timestamptz,
    last_view_ip inet
);
