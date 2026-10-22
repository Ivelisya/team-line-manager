-- 0022：账户安全——两步验证（TOTP）与恢复码、会话记下登录来源

-- TOTP 密钥用 SecretBox 加密（明文是 20 字节原始密钥）。pending 是「设置中」的那把：扫码确认后才挪进 totp_secret_enc。
-- totp_last_step 是最近一次接受的时间步（Unix 秒 / 30），同一步（及更早的）的验证码不再接受，防重放
alter table app_user
    add column totp_secret_enc      bytea,
    add column totp_pending_enc     bytea,
    add column totp_pending_expires timestamptz,
    add column totp_last_step       bigint;

-- 恢复码：只存 sha256（规整成小写、去掉连字符后算），用一次就记 used_at
create table recovery_code (
    user_id   uuid not null references app_user on delete cascade,
    code_hash bytea not null,
    used_at   timestamptz,
    primary key (user_id, code_hash)
);

-- 用户最近成功登录过的来源（分组同 login_failure.ip：IPv4 单个地址、IPv6 /64），30 天内登录成功过的来源不受
-- 「同一用户名不分来源 100 次」的上限：否则攻击者用 20 个 /64 各错 5 次就能把本人一直锁在外面。会话登出、过期会删，所以单独记
create table login_source (
    user_id    uuid not null references app_user on delete cascade,
    ip         inet not null,
    last_ok_at timestamptz not null default now(),
    primary key (user_id, ip)
);

-- 会话列表要显示登录来源
alter table session add column ip inet, add column user_agent text;
create index session_by_user on session (user_id);
