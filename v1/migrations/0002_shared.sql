-- 0002：几项并行优化共用的表结构（0001 已上线，之后改结构一律加新迁移，不再改旧文件）

-- 期望状态代数最近一次变化的时间：配置未应用的告警从这里起算，而不是从上次应用成功起算
alter table relay add column generation_at timestamptz;

-- 有新原始数据落进来的小时：小时汇总按它补算（中控停机后 agent 补传的旧数据也能汇总进去）
create table rollup_dirty (
    hour timestamptz primary key
);

-- 登录失败记录：按用户名、按来源 IP 限速
create table login_failure (
    id        bigserial primary key,
    username  text not null,
    ip        inet,
    ts        timestamptz not null default now()
);
create index login_failure_user_ts on login_failure (username, ts desc);
create index login_failure_ip_ts   on login_failure (ip, ts desc);
