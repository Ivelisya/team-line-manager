-- 0077：客户端订阅（自己的客户端按一个链接拉入口列表：Clash Verge、OpenClash 的 proxy-provider、v2rayN / Shadowrocket，见 api/client_subs.rs）。
-- 令牌只存 sha256；选择规则存团队 / 机器 id，不存入口参数：拉取时按当前入口现算，替换节点、换线路机、轮换凭据后客户端更新订阅就是新的。
-- team_mode：all = 全部团队（不含备用团队）；only = 只要 team_ids 里的团队；except = 全部团队（不含备用团队）除了 team_ids。
-- direct：带不带线路机直出入口。include_ids：另外带上的机器（节点，或线路机 = 它的直出入口），不看团队与 direct；exclude_ids：一律不带，优先于其余规则。
-- 引用的团队、机器删掉后对不上就不算，不加外键。last_fetch_*：最近一次成功拉取（10 分钟内不重复写）
create table if not exists client_sub (
    id            uuid primary key,
    name          text not null unique,
    token_hash    bytea not null unique,
    team_mode     text not null default 'all' check (team_mode in ('all', 'only', 'except')),
    team_ids      uuid[] not null default '{}',
    direct        boolean not null default true,
    include_ids   uuid[] not null default '{}',
    exclude_ids   uuid[] not null default '{}',
    enabled       boolean not null default true,
    created_at    timestamptz not null default now(),
    updated_at    timestamptz not null default now(),
    token_at      timestamptz not null default now(),
    last_fetch_at timestamptz,
    last_fetch_ip inet
);
