# 全量审计基线（2026-10）

- 审计对象：`089e5887`（生产版本），只读审计。本 PR 只加这一份文档，不改业务代码。
- 范围：`v1/crates/*`（约 6.7 万行 Rust，含上次没审到的 `api/machines.rs`、`views.rs`、`api/provision.rs`、`api/replace.rs`、`routers/*`、`jobs/steps*.rs`）、`v1/web/src`（约 2.2 万行 TS/TSX，全部 pages/features/components）、`v1/migrations/0001–0046`。
- 方法：逐模块读关键路径，同时用脚本扫这些模式：循环内 DB 调用、`for update`、advisory lock、长事务、高频 upsert、`unwrap`、ssh/shell 拼接、前端 `innerHTML`/`eval`/`target=_blank`。依赖扫描实际跑了 `cargo audit`（0.22.1，advisory-db 为当日版本）和 `pnpm@10 audit`。
- 没做的：没有连生产（185.13.159.131），所以「>1s 的 DB 写」只能从代码推断最可能的来源，需要用生产的 `pg_stat_statements` 或 `log_lock_waits` 去证实（见 PERF-0）。
- 严重级：P0 立即可被利用或正在造成事故；P1 高风险，尽快修；P2 中等，排期修；P3 低或加固项。

## 统计

| 级别 | 数量 |
|---|---|
| P0 | 0 |
| P1 | 2 |
| P2 | 10 |
| P3 | 9 |
| 撤回（上次的结论经核实不成立） | 1 |

## 上次报告（AUDIT_REPORT.md）核实结果

本次文件和上次只差行尾空白，所以上次给的行号仍然有效。

| 上次编号 | 本次结论 |
|---|---|
| H1 exec 密钥 = 全机群 root，默认不过期 | **成立 → P1-1** |
| H2 api_key 不绑定用户 | **成立 → P1-2** |
| M1 stream Origin 校验 | 降为 P3-1：`api/stream.rs:99-100` 的比较实际是「严格等于」，缺少 Origin、Origin 为 `null` 都会被拒；只有 `public_url` 配成非 http(s) 的 scheme 时 origin 会序列化为 `"null"`，这时才会放行 `Origin: null` |
| M2 审计泄露命令全文 | **撤回**：`api/audit.rs:62-71,129` 的 `hide_command` 会对没有 exec 权限的人隐藏命令，搜索命令也只对 exec 开放（`:54`） |
| M3 作业日志只需 read | 成立 → P2-1 |
| M4 没有超时和并发上限层 | 成立 → P2-2 |
| M5 react-router 漏洞 | 成立，且本次又多一条 → P2-3 |
| M6 密码长度没有上限 | 成立 → P2-4 |
| L1–L7 | 成立 → P3-2 到 P3-8 |

## P1

### P1-1 exec scope 密钥 = 全机群 root RCE，有效期可以不设
- 位置：`v1/crates/fleet-server/src/auth.rs:128`；`api/exec.rs:47-50,134-148`；`api/keys.rs:87-90`（`expires_days` 可以不填，不填就永不过期）；`local_admin.rs:57,97`（central-local 是永久的全权密钥）。
- 风险：一把泄露的密钥就能在所有机器上以 root 执行命令，而且没有到期时间来兜底。
- 修法：
  - 带 exec scope 的密钥强制设有效期（例如最长 30 天），或者每次使用都要求会话里做过二次验证。
  - central-local 改为只监听本地、按次签发的短期凭据。
  - 支持给 exec 密钥绑定来源 IP 白名单。

### P1-2 API 密钥不绑定用户
- 位置：`api/keys.rs:97`（插入时没有 user_id）；`auth.rs:456-496`（删除或停用用户时不处理密钥）。
- 风险：员工离职、被停用或改了密码之后，他建的密钥仍然有效。
- 修法：`api_key` 加 `created_by uuid references app_user on delete cascade`；鉴权时 join 用户，要求用户是启用状态；停用用户或重置密码时吊销他名下的密钥。

## P2：性能（生产里 >1s 的 DB 写）

### PERF-0（P2）先补可观测性
- 现状：连接池固定 20 个，等连接最多 5 秒（`db.rs:17-20`）。超过 1 秒的写目前无法区分是在等锁、等 I/O，还是排在分区 DDL 后面。
- 修法：生产开 `pg_stat_statements`、`log_lock_waits=on`、`deadlock_timeout=200ms`、`log_min_duration_statement=1000`；sqlx 的慢语句告警带上语句名。先确认下面哪一条是真正的来源，再动手改。

### P2-5 agent 上报是一个大事务，开头就 `for update` 锁住 agent_state 行
- 位置：`agent_api.rs:515-681`。
  - `:518` 设了 statement_timeout 10s。
  - `:520` `select ... from agent_state ... for update`。
  - 之后同一个事务里依次写：agent_state 的 jsonb 合并（`:554`）、host_metric、entry_traffic、probe_result、access_event（分区表）、ping（`ping.rs:269`）、placement（`placement.rs:73`），以及 rollup_dirty/access_dirty 的 upsert（`:663-679`）。
- 风险：
  - 任何一张分区表的插入只要排在 `maintain_partitions` 的建分区或摘分区锁后面（`db.rs:196,289`，lock_timeout 分别是 5s 和 2s），整个上报事务就跟着等，表现为成批的「>1s 写」，集中出现在每小时的整点。这是最可能的来源。
  - 整点后的头几分钟，所有 agent 补传的上一小时样本都会 upsert `rollup_dirty`/`access_dirty` 的**同一行**（`hour` 主键，`:663-679`）。这一行锁要一直持有到上报事务提交，所有 agent 的上报就在这一行上串行排队。
- 修法：
  1. 把脏标记的 upsert 移出事务，或者改为 `on conflict do nothing`，再加一次「touched_at 比我旧才更新」（`... do update ... where rollup_dirty.touched_at < excluded.touched_at - interval '30 s'`），减少同一行被反复写。
  2. 分区提前多建几天（现在 `maintain_partitions(..., 3)`，可以改为 7），并把建分区放到低峰时段，尽量不和整点重合。
  3. `for update` 换成 `select ... for no key update`，或者用乐观的 `update ... where last_seq < $seq returning`，缩短持锁时间。

### P2-6 agent_state.info 每 10 秒整行 jsonb 重写
- 位置：`agent_api.rs:551-556`。`REPORT_INTERVAL_S = 10`（`fleet-proto/src/lib.rs:224`）。
- 风险：每台机器每 10 秒把整个 info（含 xray、health、caps）合并后写一遍，TOAST 每次都要重写，产生大量 WAL 和死元组，而且没法走 HOT 更新。
- 修法：拆成两张表，热的 `agent_heartbeat(last_seq, last_report_at)` 和冷的 `info`；`info` 只在内容变化时才写（`where agent_state.info is distinct from <新值>`）。

### P2-7 放置矩阵 rtts 数组每次上报追加并全量重算 p50
- 位置：`placement.rs:73-85`。
- 风险：每个 (relay, node, hour) 行每 10 秒都要做一次 `rtts || new`，然后对整个数组 unnest 求 percentile。一小时内这是 O(n²) 的重写，而且发生在上报事务里，会把 P2-5 的持锁时间拉长。
- 修法：上报时只写原始样本，或者在内存里攒满一分钟再写；p50 放到 5 分钟一次的 rollup 里统一计算。

### P2-8 汇总循环无条件覆盖写
- 位置：
  - `db.rs:361-402`：每 5 分钟对当前小时（整点后头 10 分钟还要加上上一小时）的所有机器、入口、探测，执行 `insert ... on conflict do update`。
  - `ping.rs:303-318`：每 5 分钟重算 7 小时，**每小时一次重算 72 小时**的 ping_hourly。
- 风险：数据没变也会产生新的行版本，表膨胀，autovacuum 压力变大；每小时那一轮 72 小时的重算是一个很大的写。
- 修法：所有 `do update` 加 `where (t.cols) is distinct from (excluded.cols)`；ping 的 72 小时重算改成按脏标记来（复用 `db::rollup_dirty`）。

### P2-9 非分区大表逐行 delete
- 位置：`db.rs:404-436`（`relay_reach` 等），`ping.rs:322-323`（ping_result），`routers/collect.rs:1355-1356`（router_metric）。
- 风险：每小时一次的大批量 delete 会产生长事务和大量 WAL。如果数据量大，单条语句可能超过 1 秒，并和写入争 I/O。
- 修法：分批删（`delete ... where ctid in (select ctid ... limit 5000)` 循环执行），或者像 host_metric 一样按天分区后 drop。

## P2：安全 / 可靠性 / 依赖

### P2-1 作业日志只需 read 权限
- 位置：`auth.rs:564`（测试中断言了 `/jobs/x/logs` 需要 read），而机器日志需要 exec（`:129`）；日志写入点是 `jobs/mod.rs:354-357`，没有脱敏。
- 修法：作业日志改为需要 ops/exec 权限，或者在 `jobs::log` 统一对密码、token 做脱敏。

### P2-2 HTTP 没有全局超时和并发上限
- 位置：`lib.rs:77-79` 只有响应头、压缩和 Trace 三层。
- 修法：加 `TimeoutLayer`（不包括 WebSocket 和终端路由）、`ConcurrencyLimitLayer`，以及登录接口按 IP 限流。

### P2-3 react-router 6.30.x 两个 moderate 漏洞
- 内容：`<Link>` 反斜杠开放重定向；Arbitrary Constructor Injection。来源 `pnpm audit`，路径 `react-router-dom>react-router`，修复版本 >=7.18.0。
- 修法：升级到 react-router 7（v6 没有补丁）；升级前确认没有把用户可控的字符串传给 `<Link to>`。

### P2-4 密码长度没有上限
- 位置：`auth.rs:211-218,367`。
- 风险：超长密码让 argon2 消耗大量 CPU，可用来 DoS。
- 修法：限制最多 256 字节，并给登录接口加 body limit。

## P3

- **P3-1** `config.rs:205-211` 不校验 `public_url` 的 scheme；建议启动时要求是 http(s)（见 M1 的核实）。
- **P3-2** `cargo audit` 结果：rsa 0.9.10 RUSTSEC-2023-0071（Marvin），经 sqlx-mysql 引入，只出现在锁文件里，没有编译进来。修法：sqlx 关掉 default-features，只开 postgres，或者在 audit.toml 里 ignore 并注明原因。
- **P3-3** `pnpm audit` 结果：4 个 high（extract-zip ×2、basic-ftp、zod），全部来自 dev 依赖 `puppeteer-core`；还有 1 个 moderate，postcss-selector-parser（经 tailwindcss）。不会进入生产构建产物，升级 puppeteer-core 和 tailwind 即可。
- **P3-4** agent 的 `fleet-agent/src/server.rs:136,185` 用了 `lock().unwrap()`，mutex 一旦中毒 agent 就会 panic。修法：`lock().unwrap_or_else(|e| e.into_inner())`。
- **P3-5** `auth.rs:353,360` 只按 64 字符截断。
- **P3-6** `crypto.rs:32,42` 用 AES-GCM 加密时没有 AAD，密文可以在不同行或不同列之间互换；修法是把行 id 和列名作为 AAD。
- **P3-7** `.claude/skills/fleet-ops/SKILL.md:27` 里写着生产 IP。
- **P3-8** `install.sh:91-92` 把 fleet.toml 设为 fleet 用户所有，服务进程可以改自己的配置；建议改为 root:fleet 0640。
- **P3-9** 超大文件影响可维护性：`views.rs` 1985 行、`api/machines.rs` 1794 行、`routers/collect.rs` 1731 行、`jobs/steps.rs` 1425 行、`jobs/mod.rs` 1340 行（这里说的是行数，不少行很长）。建议按子领域拆分（见下文「优雅」一节）。

## 单列三项结论

### ① 实现是否优雅
**结论：整体偏好，但长行和大文件拖累了可读性。**
- 好的方面：
  - 注释写清楚了「为什么」，例如竞态、补传、整点边界（`db.rs:305-311`、`agent_api.rs:500-519`）。
  - 错误分类清晰（`db.rs:28-44`）。
  - 批量写入统一用 `unnest` 数组，没有逐条插入（`agent_api.rs:591-668`）。
  - 后台任务统一由 `supervise` 守护（`lib.rs:180-198`）。
- 问题：
  - 单行 SQL 和单行 Rust 很长，例如 `placement.rs:73-85`、`agent_api.rs:551-556`。
  - 有些文件超过 1500 行，同一个文件里混着 HTTP、SQL 和业务规则（`api/machines.rs`、`views.rs`）。
  - 命名大多是单字母或缩写（`es/ps/ac/mu/mt/du/dt`），离开上下文就很难读。

### ② 可复用性
**结论：该抽的大多抽了，剩下几处可以合并。**
- 已经复用得好的：
  - `db::rollup_dirty` 被遥测和访问两条汇总共用（`db.rs:312`、`access.rs:124`）。
  - ssh 命令统一经 `routers/tunnel.rs:12 common` 构造，`StrictHostKeyChecking=yes`。
  - 前端统一走 `lib/api.ts` 和 react-query，错误提示统一用 `ErrorNote`。
- 可以合并的：
  - 保留期删除散在 4 处：`db.rs:404`、`access.rs:175`、`ping.rs:321`、`routers/collect.rs:1355`。可以合成一张「表 → 保留期」清单交给同一个函数，P2-9 的分批删除也只需要改一处。
  - ping 的小时汇总（`ping.rs:303`）没有复用 `rollup_dirty`，所以只能靠「72 小时全量重算」兜底（P2-8）。
  - `app_setting` 的「advisory lock + upsert」在 `ping.rs:201-209` 和 `probe_points/mod.rs:276-293` 各写了一遍，可以抽成 `settings::put_locked(key, value)`。

### ③ 是否过度设计
**结论：没有明显的过度设计。**
- 全仓库没有自定义 trait，也没有为假想的扩展点加的泛型或插件层。唯一的泛型是 `rollup_dirty<F>`，它有两个真实调用方，是合理的。
- 配置项都对应真实的运维需求，例如保留期、探测点。
- 反而有少量欠设计：上报的热路径把心跳、明细、汇总标记都塞在一个事务里（P2-5、P2-6）。拆开不算过度设计，是必要的。
- 不建议为了复用把各领域的 SQL 抽成 ORM 或查询构造层，那样会损失可读性和性能可控性。
