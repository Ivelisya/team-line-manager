-- 0056：探测点升级失败后不再反复下发（probe_points/agent.rs）。可重复执行。
-- 探测点自己的「换回来过的版本不再换」（fleet-agent 的 .fleet-agent.bad-sha256）只有新版本有；换回到的旧版本没有这段，
-- 一启动就又照 upgrade_sha256 升级，五分钟一轮反复下载、换上、换回。中控这边记下：
-- upgrade_version：请求时 dist 里程序的版本（展示用）；
-- upgrade_fetched_at：这次请求第一次下载程序的时刻。过了试运行时长（探测点 5 分钟内没上报成功就换回）还没报上目标 sha256，
--   就算这次升级失败：清掉 upgrade_sha256，记下 upgrade_failed = {sha256, to: 目标版本, from: 它这时报的版本, at}，要手动再触发；
--   之后它报上了目标 sha256（其实换成了，只是慢）就清掉 upgrade_failed。
alter table probe_point add column if not exists upgrade_version text;
alter table probe_point add column if not exists upgrade_fetched_at timestamptz;
alter table probe_point add column if not exists upgrade_failed jsonb;
