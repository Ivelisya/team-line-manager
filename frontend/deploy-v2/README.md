# deploy/v2 — 团队线路管理 v2 的实时数据层

新版界面（`prototype/original-refined/`）只认识 `prototype/shared/data.js` 的 store。本目录让同一个 store 直接跑在**生产 team-ops 只读接口**上：字段、单位、口径与 `prototype/data/README.md` 完全一致，只是值是实时的真实值（不脱敏）。

```
live/adapter.js      createLiveFetch / createLivePoller（浏览器与 node 通用，无依赖，只发 GET；全局 ≤3 并发）
live/synth.js        纯函数：接口响应 → snapshot / detail / extra / series（规则逐条照抄 tools/build.py）
live/boot.js         dist 用的接线：loadSnapshot → 适配层、createLive → 轮询器、globalThis.TEAMOPS
live/fixtures/       a395 上用真实后端代码抓的接口响应（含真实 IP，只留本机，禁止发布）
build.sh             生成 dist/（app · shared · original · data · live），见 LIVE-MODE.md §1
LIVE-MODE.md         只读预览的写操作清单、演示数据、深链、XSS、PHASE 2 改动计划
tools/dist.mjs       build.sh 的 index.html 改写（CSP）与 dist 检查；tools/smoke.mjs 无头 Chrome 冒烟
tests/               node --test：结构、单位、store 选择器、轮询 diff、401、去重、缓存、请求预算、与 build.py 对照
staging/             a395 测试服：stage_server.py、komari_mirror.py、run.sh、preview/（dist 出来之前的预览页）
```

## 1. 接入方式

```js
import { loadSnapshot } from './shared/index.js';
import { createLiveFetch, createLivePoller, AUTH_MESSAGE, AUTH_LINK } from './live/adapter.js';

const liveFetch = createLiveFetch({ onAuthError: ({ message, link }) => showLogin(message, link) }); // 401 → “请先登录管理员账户” + /admin
const store = await loadSnapshot({ base: new URL('./data/', import.meta.url), fetch: liveFetch });
const live = createLivePoller(store, liveFetch).start();   // 与 createLive(store) 同一套控制器：start/stop/tick/on/age/sources
```

页面改动只有两处：`loadSnapshot()` 传 `fetch: liveFetch`，`createLive(store)` 换成 `createLivePoller(store, liveFetch)`。`plan-catalog.json` 等非数据文件原样交给真实 fetch（deploy 时随 dist 放在 `data/` 下）。

- `liveFetch` 按文件名识别 `snapshot.json`、`detail.json`、`extra.json`、`series/{traffic-5m,traffic-1m,host-24h,latency-24h}.json`，其余请求透传；对这些数据文件传入非 GET 会直接抛错，适配层自身只发 `GET /api/team-ops/<path>`。
- 同一时刻相同的请求只发一次（in-flight 去重），`maxAge` 复用短时结果。
- 401：第一次时调用 `onAuthError({status, message, link:'/admin'})`，之后不再发请求，轮询自动停止；store 的加载会以 `读取失败 401` 失败，页面据此显示登录提示。
- 轮询器创建时会把 `store.detail(id)`、`store.series(id, kind)` 改成**按节点取数**（打开哪个节点才请求哪个节点），所以页面应在调用它们之前创建轮询器。
- 时钟：每次刷新把 `store.raw.as_of` 设为服务器的 `now`，并给 store 装上连续时钟（服务器时间 + 本地流逝），所以“N 秒前”、剩余天数、`staleOf` 都按服务器时间算，不受浏览器时钟偏差影响。
- 轮询 diff 与 `shared/live.js` 同形：`{seq, now, dt, catchUp, nodes:{id:{'rate.down_bps':…}}, relays, entries, fleet, events:[{type:'status'|'attention'|'incident'}]}`。`status`、延迟条、`traffic.hourly24`、`availability.tracker30`、`host_spark` 整体替换（与 live.js 一致），其余逐叶子写入，`touched(diff, id, 'rate.')` 照常可用。线路机行的 `host/bandwidth/billing` 与节点行共用对象，diff 同步镜像。增删节点或入口时 `diff.fleet.reload` 为真（store 不能增行，页面提示刷新）。
- `document.hidden` 时暂停，回到前台立即补一次（`catchUp: true`）。

## 2. 请求与频率

| 时机 | 请求 | 体积（原始 / gzip） |
|---|---|---|
| 首屏（并行 1 轮，10 个） | `overview`、`health`、`history?hours=24`、`incidents`、`behavior`、`activity?limit=500`、`komari/clients`、`renewals`、`jobs?limit=50`、`komari/hosts` | 约 820 KB / 87 KB |
| 首屏之后（轮询器 start 后 2.5 s + 空闲时） | 只取看得见的：无主机节点（8 台，首页“今日入口流量”弹层要 24 h 峰值）的 `history?asset=…&hours=24` | **首次访问共 18 个**（旧版 147 个）；全部请求共用一个调度器，**同时最多 3 个**，首屏 / 轮询优先、打开的抽屉其次、后台最后 |
| 打开线路页或上架流水线 | 已结束任务详情 `jobs/<id>`（22 个，只第一次；永久缓存在 localStorage，只存用到的字段） | 之后 0 个 |
| 按需（`need()`） | `need('traffic', ids)`：该节点 24 h 流量 + 本月首小时（按月缓存）；`need('sparks', ids)`：主机迷你图 4 h 种子（当前界面没有迷你图，所以一个都不取） | |
| 每 10 s（`intervalMs`，可调到 15 s） | `overview` + `health` | 约 450 KB / 46 KB（与现有运营页相同） |
| 每 60 s | `history?hours=24`、`incidents`、`behavior`、`activity?limit=500`；节点状态一变，下一次刷新就补读 `incidents` | 约 280 KB / 25 KB |
| 每 5 min | `komari/clients`、`renewals`、`jobs`（新任务再取详情）、`komari/hosts` | 约 80 KB / 14 KB |
| 每 10 min | 逐节点 24 h 流量：只刷新上面“看得见”的节点（8 台 + 打开过的） | 约 70 KB / 17 KB |
| 打开节点详情 | `node?asset=`（服务端会重算 overview，约 350 KB，只在打开时取一次）、`behavior?asset=&seconds=1209600`、`activity?asset=&limit=10&seconds=1209600`、该节点两个探测任务的 `komari/pings/<id>/results?hours=24&uuid=`、该节点 24 h 流量与本月首小时 | 约 9 个 |
| 打开节点图表 | `history?asset=`（流量）、`komari/hosts/<uuid>/metrics?range=1d`（主机）、`komari/pings/<id>/results`（延迟） | |

选项：`createLiveFetch({ maxInflight: 3, trafficLane: 'needed'|'background'|'eager'|'off', jobsLane: 'needed'|true|'eager'|false, hostSpark: 'visible'|'seed'|'accumulate', storage })`（默认即括号里第一个）；`createLivePoller(store, liveFetch, { intervalMs: 10000, historyMs: 60000, minuteMs: 60000, slowMs: 300000, trafficMs: 600000, detailWaitMs: 2500, lanesDelayMs: 2500 })`。轮询器另有 `intervalS`、`need(kind, ids)`、`onError(fn)`、`onRecover(fn)`，`age()` 从首次加载起算，`status()` 带 `failing`、`peakInflight`、`queue`。`node tests/burst.mjs [adapter.js]` 打印一次首次访问的请求数与峰值并发（旧版：147 个、峰值 10；现在：首页 18 个、线路页 40 个、再开一个抽屉 +9，峰值 3）。

## 3. 字段映射

`ov` = `/overview` 的 `assets[]` 元素，`hn` = `/health` 的 `monitor.nodes[uuid]`，`k` = `/komari/clients` 元素，`ch` = `ov.channel`。规则与 `tools/build.py` 一致，下表只写来源。

### 节点（`snapshot.nodes[]`）

| 字段 | 来源 | 实时情况 |
|---|---|---|
| `id` `name` `full_name` `number` `team` `kind` | `ov.name`（短名规则同 build.py：`团队1-01`→`t1-01`，`线路机2`→`r2`）、`ov.team`、`ov.provider` | 与快照相同 |
| `capabilities` | `ov.host.metrics_available && ram_total>0` → host；线路机 → relay；NodeMach → proxy；`k.version` 以 relay-probe 开头 → probe | 相同 |
| `provider` `provider_code` `region` | `ov.provider`；`k.region`（国旗）→ 代码、中文名 | 相同 |
| `ip` `ipv6` `relay` `relay_port` `entry_label` | `ov.ip`（缺时 `k.ipv4`）、`k.ipv6`、`ov.relay`/`relay_port`；线路机取主节点入口端口 | 真实 IP |
| `status` | `hn.status/reason/since/checked_at/maintenance/checks`；`attention` = uuid ∈ `health.monitor.attention`；`failed_checks` = 非 ok/na 的检查 | 相同 |
| `limits` | `hn.limits` | 相同 |
| `latency.*_ms` `*_at` `egress_*` | `hn.metrics`、检查项 `measured_at`，缺时 `ov.probe` / `ov.path` | 相同 |
| `latency.strip_tcp` `strip_egress` | 每次轮询读 `ov.probe` 的 `tcp_at/tcp_ms/tcp_error`、`egress_at/egress_ms/egress_error`，按探测时刻去重累积，最多 20 个；不足 20 个时前面补空格（`ms: null, lost: 0`），另带 `samples`、`source: "monitor"` | **来源不同**：快照用 Komari 1 分钟汇总；实时用线路机探针的逐次结果（也是“延迟”列的来源）。打开页面时只有 1 个样本，TCP 约 10 分钟、出口约 100 分钟填满 |
| `availability.windows["30"]` | `hn.windows["30"]` | 相同 |
| `availability.windows["1"]` `["7"]` | `/node?asset=` 的 `windows` | **首屏为 null**（显示“—”），打开该节点后补上 |
| `availability.tracker30` | `hn.days[30]`：`availability` = 当日成功率，`incidents` = 当日故障次数，`state` 按 build.py 的阈值推导；另带 `day_basis: "utc"` | **口径不同**：服务端按 UTC 日历日（北京时间 08:00 分界）统计，快照按北京日历日；`coverage` 与逐日样本数为 null（接口不给） |
| `rate` | `ch.rate_down/rate_up`（B/s）×8 → bit/s，`ch.sample_at/fresh/tcp_connections/last_active` | 相同（下行 = 流向用户客户端） |
| `traffic.today` `month` | `ov.daily` / `ov.monthly`（服务端按北京时间 0 点、1 号从 hours 表求和） | 相同 |
| `traffic.last24h` `peak_24h` `hourly24` | 逐节点 `/history?asset=&hours=24`（5 分钟桶），网格与 build.py 相同 | 相同；`hourly24` 最后一格（当前未满小时）多含正在累积的当前分钟 |
| `traffic.coverage_start` `month_partial` | 逐节点本月首小时（`/history?asset=&hours=本月小时数+2` 的第一行） | 相同 |
| `traffic.cumulative` `counter_reset` `sample_gap` `stats_valid` `quota` `host_quality` | `ch.down/up`（3x-ui 累计）、`ch` 标记；配额 `k.traffic_limit*` + `ov.host.net_total_up`；`ov.host_traffic_quality` | 相同 |
| `host` | `ov.host`（Komari 最新状态，5 s 同步）+ `k` 的核数、型号、系统、内核、架构、虚拟化、GPU；网卡 B/s ×8 | 相同 |
| `host_spark` | 种子：`komari/hosts/<uuid>/metrics?range=4h` 的 1 分钟记录按 5 分钟求均值；之后每次轮询把 `ov.host` 样本并入当前 5 分钟桶 | 数值口径同快照（5 分钟均值），种子请求完成前为空 |
| `agent` `billing` `bandwidth` `tags` `outbound` `connection` `behavior` | `k.version`、资产账单字段 + `ov.due_days`（账期时区日历日）+ `/renewals` 最近一次；带宽按 build.py 的 capacity 规则；`k.tags` 过滤；`ss_method` 来自成功的 `jobs/<id>` 参数；`connection` 来自 `ch` 与面板在线列表；`behavior` 来自 `/behavior` 的 24 h 汇总 | 相同；`connection.credential_prefix` 是**真实** client UUID 前 8 位（快照是演示值） |
| `ipintel` | 无：来源是 a395 上的离线 IP 库，生产没有 | **null** |

### 线路机、入口

| 对象 | 来源 | 实时情况 |
|---|---|---|
| `relays[]` | `ov.relays[name]`（采集快照：面板、配置核对、路由哈希、日志积压、探针时间）、`overview.collector.duration_seconds`、节点行的 host/带宽/账单；`ssh_port` 取 `/fleet`、线路机上架任务参数，否则 22（与 build.py 同） | 全部实时；`config.core_id` 为真实前 12 位 |
| `entries[]` | `ov.relays[name].inbounds[]` + 绑定节点的 `probe` | 全部实时；`client.credential_prefix` 为真实值；`ipintel` 为 null |

### 其余顶层字段

| 字段 | 来源 | 实时情况 |
|---|---|---|
| `as_of` `as_of_text` `timezone` `schema` `dict` `teams` `providers` | 服务器 `now`；词表与规则内置（同 build.py） | 相同 |
| `kpis` | 由节点求和；`attention` = monitor.attention 数；`events_today` = `overview.events_today` | 相同；`business_rate.spark` 为近 2 小时 **5 分钟**点（`step: 300`，24 个点；快照是 1 分钟 120 个点） |
| `groups` `upcoming_due` `attention` `status_replays` `incidents_recent` `incidents_total` | 节点、`/incidents`（服务端上限 250 条） | 相同（同一时刻逐项核对一致） |
| `traffic.fleet_24h` | `/history?hours=24` | **5 分钟桶**（`step: 300`，287 个点，另带 `resolution_s`）；快照是 1 分钟 1439 个点。`missing_minutes` = 空桶数 × 5 |
| `rankings.rate` `today` | 节点 | 相同 |
| `rankings.cpu_avg_24h` `mem_avg_24h` `latency_unstable_24h` `loss_24h` | 需要每台 24 h 主机与延迟序列 | **空数组**（避免首屏 70+ 个重请求） |
| `system` | `overview.collector/backup/retention/hosts_at/started_at/monitor_started_at`、`health.monitor.checked_at`、`komari/hosts`（孤立主机数） | `storage.*_bytes`、Komari 汇总保留时长为 null；`availability_definition` 为服务端 meta 原文（内置） |
| `behavior_kpis` `behavior_top` | `/behavior`（24 h 汇总）+ `/activity` 的 24 h 聚合 | 相同；`events_recent` / `events_counts` 只统计 24 h 内（服务端不给全量计数）；来源行的运营商等 IP 情报字段为 null |
| `access_recent` | `/activity?limit=500`（全机队最近 500 条）按节点取 10 条；打开节点时用 `/activity?asset=&limit=10&seconds=1209600` 补齐 | 冷门节点首屏可能不足 10 条；行级 IP 情报字段为 null |
| `onboarding.history` | `/jobs` + `jobs/<id>`（步骤耗时与错误） | 相同；`demo_batch` 为 **null**（新流水线尚未上线，不展示演示数据） |
| `site` | 名称内置“团队线路管理” | `description`、`komari_version` 为 null（team-ops 接口不给） |
| `provenance` `anonymization` | 实时来源说明 | 形状不同（`source: "live"`、`applied: false`） |

### 懒加载文件

| 文件 | 来源 | 说明 |
|---|---|---|
| `detail.json` | 全部节点由已有数据合成（检查项、30 天、故障、续费、`/audit` 最近 150 条）；打开的节点再补 `/node`（windows 1/7、24 h 逐分钟样本 → `hourly48` 后 24 格）、14 天目标与来源画像、最近 10 条访问、24 h TCP 稳定性 | `hourly48` 前 24 格为 null（接口只留 24 h）；`top_targets_90d` 取 14 天窗口内最近活跃的目标按 90 天累计排序 |
| `extra.json` | 入口全量、`/incidents` 全部、行为、`/komari/pings`、续费、审计 | `audit_counts` 只统计最近 150 条（接口上限） |
| `series/traffic-5m.json` | 逐节点 `/history?asset=` | 相同 |
| `series/traffic-1m.json` | 同上 | 接口最细 5 分钟：`step: 300`、`resolution_s: 300` |
| `series/host-24h.json` | `komari/hosts/<uuid>/metrics?range=1d`（5 分钟记录） | `*_max` 等于均值（每桶只有一条记录） |
| `series/latency-24h.json` | `komari/pings/<id>/results?hours=24&uuid=`（TCP 优先线路机执行者，出口优先节点自身，同 build.py） | Komari 每桶只给**最后一个样本**：`avg_ms` 是该样本，`min_ms/max_ms` 为 null，`loss_pct` 为 0/100 |

**覆盖统计**（`tests/adapter.test.mjs` 逐叶子路径比较原型快照的全部字段）：节点 232 条叶子路径实时有值、6 条首屏为 null（windows 1/7 四项；延迟条样本不足 2 个时的 `window_s` 两项）；线路机 121 条、入口 53 条全部实时；其余顶层 1768 条实时、27 条为 null（访问记录行的 IP 情报 12 条、行为事件的来源画像 8 条、存储大小与 Komari 保留时长 5 条、`site` 2 条）。按 README 的节点字段表（23 行）计：19 行与快照同源同值，3 行实时但来源或分辨率不同（延迟条、tracker30 与 windows 1/7、迷你图），1 行为 null（ipintel）。

## 4. 与快照构建器的对照

`staging/crosscheck_build.sh` 在 a395 上对**同一份库副本**跑 `tools/extract.py` + `tools/build.py`（关掉字符串脱敏，数值取整与 anon() 相同），`tests/crosscheck.mjs` 把它与适配层从真实接口抓回的结果逐叶子比较（`node tests/crosscheck.mjs --all` 打印明细）。结果：41 个节点、线路机、入口、团队汇总、KPI、到期、状态回放**没有未解释的差异**。已解释的差异只有：

| 路径 | 原因 |
|---|---|
| `latency.strip_*`、`host_spark` | 数据来源不同（见上表） |
| `availability.tracker30.*` | 服务端按 UTC 日，快照按北京日 |
| `availability.windows.1/7` | 只有 `/node` 给，首屏为 null |
| `traffic.hourly24` 最后一格 | 接口多含当前正在累积的一分钟（样例：团队1-01 ↑ 38.7 MB vs 41.4 MB） |
| `kpis.business_rate.spark`、`traffic.fleet_24h` | 接口 5 分钟桶 |
| `kpis.events_today` | 服务端计数不设上界，含 as_of 之后到达的 1 条 |
| `credential_prefix`、`core_id`、`komari_uuid` | 快照里是假值 |
| `system.storage`、Komari 保留时长 | 接口不提供 |

对照中发现并修正过的问题：取整。build.py 用 Python `round()`（按浮点精确值、平局取偶），JS 的 `Math.round` 在 0.5 处向上，导致 `load5_per_core_pct` 4 台差 1、`cpu_pct` 2 台差 0.01。`synth.round()` 现在逐位复现 Python 的结果（精确判定平局）。

## 5. 测试

```sh
cd ~/Desktop/team-ops-v2/deploy/v2 && ~/.local/bin/node --test      # 44 项：tests 44 · pass 44 · fail 0
~/.local/bin/node tests/crosscheck.mjs            # 5 个样本节点的差异明细；--all 为全部
```

- `adapter.test.mjs`：原型快照每个顶层键、每个节点 / 线路机 / 入口字段在实时快照里存在且 JSON 类型相同（或为 null）；detail / extra 形状；单位（×8、整数秒、北京时间文本）；用 `prototype/shared/data.js` 的 `createStore` 建 store，自动扫描 `original-refined/js` 里出现的全部 `store.xxx(` 调用逐个执行（页面新增选择器而测试未覆盖时直接失败），并跑表格、面板、格式化；轮询两次：第一次用 a395 上 +10 s 的真实第二帧，第二次人为让团队1-05 故障，断言 diff、状态与需处理事件、KPI、时钟；按需详情；去重与只发 GET；401；缓存；延迟条与迷你图累积。
- `crosscheck.test.mjs`：见第 4 节。
- 夹具：`live/fixtures/`（146 个响应，5 MB）由 `stage_server.py capture` 生成——在 a395 上把真实 teamops 代码加载进进程、时钟钉在 as_of、对新副本逐个调用 `Application.dispatch`。`komari-probe/` 是真实 Komari 二进制的 RPC 返回（隔离网络命名空间里跑的，客户端 token 已换成假值），`crosscheck/` 是 build.py 的未脱敏输出。**三者都含真实 IP / UUID，只留本机，不要发布或提交**；`run.sh` 发现静态目录里有 fixtures 会拒绝同步。
- 重抓：`ssh a395 '~/team-ops-eval/teamops/runtime/python/bin/python3.12 ~/team-ops-eval/v2-stage/staging/stage_server.py capture --out ~/team-ops-eval/v2-stage/fixtures'`，再用 tar 拉回 `live/fixtures/`。

## 6. 测试服（a395）

```sh
deploy/v2/staging/run.sh            # 同步并（重）启动；status / stop / logs
ssh -i ~/.ssh/a395_kismet -N -L 18395:127.0.0.1:18395 kismet@10.172.17.213
open http://127.0.0.1:18395/admin/v2/
```

- `run.sh` 把 `deploy/v2/dist`（没有时用 `staging/preview/`：shared + live + 一个实时表格预览页）和服务端代码用 tar 同步到 `~/team-ops-eval/v2-stage/`，nohup 启动，pid 写 `stage.pid`，日志 `stage.log`。停止时只杀 pid 文件里、命令行确实是 `python3.12 staging/stage_server.py serve` 的进程。
- `stage_server.py serve`：每次启动 `cp --reflink=auto` 一份新副本（`~/team-ops-eval/data/{operations,komari,metrics}.db` → `v2-stage/db/`），加载真实 `teamops.server.Application`（不调用 `main()`，所以采集、监控、行为缓存三个循环都不启动），把 `bridge`、`provision.remote` 换成直接报错，`provision.komari` 换成 Komari 镜像；`GET /api/team-ops/<path>` 按生产插件 `allowedGet` 白名单（启动时从 release 的 `plugin/script.js` 读）分发，状态码映射同生产 Handler；其他方法一律 403“测试服只读：已拒绝写操作”；`/admin/v2/*` 出静态文件。
- 隔离：后端进程在 `unshare -Urn` 的用户 + 网络命名空间里，只有未启用的 loopback，连不到任何外部地址（启动日志 `isolated=True`，`/__stage/status` 的 `network_isolated`）；宿主侧只有一个 127.0.0.1:18395 → unix socket 的转发。
- 时钟：副本是 2026-09-23 15:22:18 的冻结时刻。`--clock keepalive`（默认）从 as_of 起走真实流逝时间，每 10 s 把线路机快照、探针、路径、面板、Komari 主机与采集器的时间戳整体后移（年龄保持 as_of 时的样子），再在副本上跑真实的 `Monitor.evaluate()`；数值停在 as_of，计数不增长，24 h 图在 as_of 之后是空的。`pinned` 钉在 as_of（抓夹具用），`shifted` 只走时钟不保鲜。
- Komari：`komari_mirror.py` 用 SQL 读副本回答 `admin:listClients`、`admin:getAllPingTasks`、`public:getRecordsByUUID`、`public:getPingRecords`，形状按生产 Komari 1.5.0-review20260917.4 二进制的真实返回（`komari_probe.sh` 在隔离命名空间里跑过一次，通知、GeoIP、消息渠道全关，token 换假值）：≤4 h 用 1 分钟汇总、以上用 5 分钟；主机记录取桶均值并按 Go 截断取整；Ping 记录取桶内**最后一个样本**。`verify_mirror.py` 逐条比对结果一致，只有两个执行者同桶记录的先后不同（Go 的排序本身不稳定）。
- `--auth-token T`：要求 cookie `stage_session=T`，用来演示 401 与“请先登录管理员账户”。
- 静态文件与生产 Caddy 同一套响应头：`Content-Security-Policy`（与 `deploy/prod/deploy.sh` 逐字相同）、`X-Frame-Options: SAMEORIGIN`、`Cache-Control: no-cache`；dist 布局下 `/admin/v2`、`/admin/v2/` 302 到 `/admin/v2/app/`（同生产），目录不带斜杠时补斜杠。
- 冒烟：`./build.sh && ./staging/run.sh`，开隧道后 `~/.local/bin/node tools/smoke.mjs`（1440 浅 / 深、390；每个路由的控制台、异常、CSP、失败请求、首次访问请求数与峰值并发；截图写 `shots/phase1/`，含真实数据，只留本机）。

## 7. 已知问题与建议

1. **tracker30 按 UTC 日**：北京日历日需要 `health_hours` 逐日数据，现有接口没有。新架构里建议 `/health` 直接给北京日的 30 格（ok / failed / unknown / maintenance 计数）。
2. **没有 1 分钟流量接口**：全机队 24 h 图、KPI 迷你图、1 分钟序列都只能 5 分钟。
3. **近 1 / 7 天可用率只在 `/node`**，而 `/node` 每次重算整份 overview、返回约 350 KB，所以只在打开节点时取一次。建议 `/health` 带上 1 / 7 天窗口。
4. **24 h 排行（CPU、内存、延迟抖动、丢包）为空**：要逐台拉 24 h 序列（70+ 个请求），建议服务端预聚合。
5. **延迟条改为线路机探针逐次结果**，打开页面时从 1 个样本开始积累；Komari 的 Ping 记录每桶只剩最后一个样本，做不出快照那种 20 格。
6. `ipintel` 与行级 IP 情报字段全部为 null（离线库只在 a395）。
7. 行为事件计数、审计计数只能统计接口返回的窗口（24 h / 最近 150 条）。
8. `connection.credential_prefix` / `entries[].client.credential_prefix` 现在是真实 VLESS client UUID 的前 8 位（接口本来就向管理员会话返回完整 UUID）。是否在界面上显示请你定。
9. 首次访问：首页 18 个请求（10 个核心 + 8 台无主机节点的 24 h 流量），线路页 / 上架流水线第一次多 22 个任务详情（之后走缓存），打开一个抽屉约 9 个；同时最多 3 个。每 10 s 的 `overview + health` 约 46 KB gzip，与现有运营页相同，接口没有增量 / ETag；`/overview` 每次都在服务端重算，是 CPU 大头，新架构里应改为推送。
10. 站点描述与 Komari 版本 team-ops 接口不给；需要的话可另读 Komari 的 `/api/public`。
11. 节点或入口增减时 store 不能加行，只发 `diff.fleet.reload`，页面需提示刷新。
