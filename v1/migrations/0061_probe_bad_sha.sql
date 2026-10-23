-- 0061：探测点报上来的坏版本（probe_points/agent.rs）。可重复执行。
-- 探测点换上新程序后 5 分钟没能上报、自己换回了旧版本时，记下那个程序的 sha256（.fleet-agent.bad-sha256），之后不再换成它，
-- 也就不再下载——中控「下载后 5 分钟还没报上新版本」的失败判定永远起不了算，一直显示升级中。新探测点每份上报带上它（ProbeReport::bad_sha256），
-- 存在这里：要它升级到的正是这个就马上记成升级失败、不再下发；手动升级到这个版本直接 409（修好后发新版本，或重装探测点）。
-- 旧探测点不带，为 null。
alter table probe_point add column if not exists bad_sha256 text;
