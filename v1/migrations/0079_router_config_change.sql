-- 0079：软路由 OpenClash 配置的变更时间线（采集发现配置文件的 sha256 变了就记一条，见 routers/changes.rs）。
-- diff 是出口的增删改（按出口名比 server:port 与凭据是否变，凭据只记「变了」不存明文）；job_id / job_kind 是认出来的中控作业
-- （最近 10 分钟内改过这台路由、改完的 sha 与这次新 sha 一致的），都为空 = 路由上改的（手改，或团队1 旁路由上的看门服务重新生成）。
-- job_id 不设外键：作业记录清理后这里仍留着。每台路由只留最近 200 条。
create table if not exists router_config_change (
    id        uuid primary key,
    router_id uuid not null references router on delete cascade,
    at        timestamptz not null,
    old_sha   text not null,
    new_sha   text not null,
    job_id    uuid,
    job_kind  text,
    diff      jsonb not null default '[]',
    -- 比不出差异的原因（新配置解析不了、中控重启后没有旧配置）
    note      text
);
create index if not exists router_config_change_router_at on router_config_change (router_id, at desc);
