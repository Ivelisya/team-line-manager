# 边缘程序功能规格（relayd / nodeagent / 上报协议）

> 来源：旧实现 `edge/fleet-edge`（Rust）、`backend/fleet`（fleetd 中控侧）。本文只描述旧系统「做了什么」与「为什么」，供 v3（Rust，协议可换）重写时不丢必要行为。
> 标注约定：`[旧]` = 旧代码事实；`[估算]` = 本文推算；`[推断]` = 读代码得出但无文档背书；**「不要照搬」** = 旧实现里的 bug / hack / 迁移遗留，重写时必须重新设计。
> 行号引用格式：`路径简写:行`。路径简写：
> - `E/` = `edge/fleet-edge/src/`　`I/` = `edge/fleet-edge/fleetd-ingest/`　`D/` = `edge/fleet-edge/deploy/`
> - `P/` = `backend/fleet/crates/probe/`　`O/` = `backend/fleet/crates/onboard/`　`F/` = `backend/fleet/fleetd/src/`　`API` = `backend/fleet/API.md`
> - `T/` = `edge/fleet-edge/test/`　`LAB` = `backend/fleet/crates/onboard/test/lab.sh`

---

## 0. 全景

| 程序 | 装在哪 | 身份 | 作用 |
|---|---|---|---|
| `nodeagent` | 落地节点（29 台，ssserver 所在机） | 系统用户 `fleet-edge`，无 capability | 只读主机遥测，只出站 HTTPS 到中控（`D/fleet-nodeagent.service:8-27`） |
| `relayd` | 线路机（3 台：line1 / central / line3，Xray 所在机） | root 但 `CapabilityBoundingSet=` 为空 + seccomp + 只读文件系统（`D/fleet-relayd.service:1-70`） | nodeagent 的全部遥测 + Xray 统计 + 探测 + 访问日志 + 管理模式（改 Xray） |

- 两个程序共用同一个库与同一个主循环（`E/runner.rs`），relayd 是「主循环 + `Relay` 扩展」（`E/bin/relayd.rs:8-49`，`E/relay.rs:66-142`，`E/runner.rs:14-26` 的 `Extension` trait）。
- 单线程主循环用 `sigtimedwait` 当定时器（`E/util.rs:43-83`）；relayd 另起线程：probe（tokio current_thread）、path（pathcheck 常驻线程）（`E/probe.rs:122-163`）。
- 配置：`/etc/fleet-edge/agent.toml`（`deny_unknown_fields`）+ `/etc/fleet-edge/token`（0600）。全部默认值见 `E/config.rs:25-244`；下文表格里直接列出默认值。
- 状态目录（`StateDirectory=fleet-relayd|fleet-nodeagent`，0700）里的文件：

| 文件 | 内容 | 谁写 |
|---|---|---|
| `outbox.dat` | 有界磁盘队列 | `E/outbox.rs` |
| `state.json` | `{boot_id, seq}` | `E/runner.rs:57-86` |
| `net.json` | 公网网卡计数基线 `{boot_id, iface, rx, tx}` | `E/hostmon.rs:129-150` |
| `status.json` | `{ts,last_ok,last_error,pending,pending_bytes,dropped,rejected,seq}`，每轮上报后写；rollout 靠它确认「第一批已被接收」 | `E/runner.rs:346-358`，`D/rollout.sh:268-283` |
| `xray.json` | 累计账 `{core_id, counters(prev), cum, accum_since, seed}` | `E/xray.rs:701-716` |
| `access.json` | 访问日志游标 `{path, inode, offset, counters}` | `E/accesslog.rs:435-445` |
| `manage.json` | `{generation, pending_acks, done_cmds, last_ack, rejected}` | `E/manage.rs:381-385` |
| `probe.db` | SQLite（WAL）：`probe_raw`（24 h）+ `probe_kv.probe_push_acked` | `P/store.rs`，`E/probe.rs:41-59` |
| `config-backups/config-<unix秒>-gen<N>.json` | 应用 spec 前的配置备份，留 30 份 | `E/manage.rs:855-858,1034-1042` |
| `pathcheck-client.json` | 全链路检查用的临时 Xray 客户端配置 | `E/pathcheck.rs:266` |

---

## 1. 通用遥测核心（nodeagent 与 relayd 共有）

### 1.1 身份与令牌

- 身份 = `host_id`（配置项，≤64 字节，非空）+ 令牌文件（`E/config.rs:254-257,297-309`）。
  - 节点的 `host_id` = 资产短名称（如 `团队1-24`）；线路机 `host_id` = `line1`/`central`/`line3`（`API:399`）。**不要照搬**：用人类可读、可改名的名称做身份（见 §9）。
- 令牌文件：必须 0600（`mode & 0o077 != 0` 直接报错）；内容 trim 后 ≥20 字节且全部 ASCII 可打印（`E/config.rs:297-309`）。令牌格式 `fe1_` + 64 hex（32 字节随机，`I/src/tokens.rs:9-13`），中控只存 sha256。
- 令牌每次连接失败后重读（`E/runner.rs:266-267`），因此换令牌不需重启 [旧]。
- `boot_id` = `/proc/sys/kernel/random/boot_id`（读不到 = `"unknown"`）（`E/hostmon.rs:90-92`）。
- `meta`：`{kernel=/proc/sys/kernel/osrelease, os=/etc/os-release PRETTY_NAME, ncpu=/proc/stat 里 cpuN 行数}`（`E/hostmon.rs:94-105`）。
- `agent` = `"<二进制名>/<版本>"`（`E/runner.rs:97`）。
- 上报地址：`{endpoint}/v2/ingest/{node|relay}`；`endpoint` 必须是 https（仅 `http://127.0.0.1|localhost|[::1]` 例外，测试用）；证书用 webpki 根正常校验（含 IP SAN），**没有不安全模式**（`E/config.rs:258-268`，`E/push.rs:1-3`）。生产 endpoint 是 IP 字面量 `https://38.58.57.22`（`D/rollout.sh:52`），因此要求中控证书带 IP SAN 且被公共根信任 [推断]。

### 1.2 采样（每 `sample_interval_s` 一次）

默认 5 s（允许 2–300，`E/config.rs:269`）；每 `batch_samples`（默认 2，允许 1–60）个样本组成一批 → 默认每 10 s 一批。

- 启动对齐：首个采样点 = 对齐墙钟 `interval` 的整数倍 + `fnv1a(host_id) % interval_ms` 的固定偏移（错峰）（`E/runner.rs:154-161`）。
- 进程被挂起 / 时钟跳变：跳过错过的周期，不补采（`E/runner.rs:188-192`）。
- 启动先打一次「底」（CPU、网卡速率需要前值）；**relayd 的 Xray 采集不打底**（每次采集都产生增量，丢掉就少计）（`E/runner.rs:159-160`）。

每个样本（`Sample`，`E/hostmon.rs:66-80`）字段、来源、口径：

| 字段 | 内容 | 来源与口径 |
|---|---|---|
| `ts` | 样本时刻，unix 秒，保留 2 位小数 | 本机时钟 |
| `cpu` | `{pct,user,system,iowait,steal}`（%，2 位小数；第一个样本或 total=0 时为 null） | `/proc/stat` 首行两次之差；`pct = 100 − (idle+iowait)/total`；`user = user+nice`；`system = system+irq+softirq`（`E/hostmon.rs:170-193`） |
| `load` | `[1m,5m,15m]` | `/proc/loadavg` |
| `mem` | `{total,used,avail,pct}`（字节） | `/proc/meminfo`；`avail = MemAvailable`（缺失则 `MemFree+Buffers+Cached`）；`used = total−avail`（`E/hostmon.rs:289-320`） |
| `swap` | `{total,used,pct}` | `SwapTotal/SwapFree` |
| `disk` | `{total,used,pct}`（仅 `/`） | `statvfs("/")`；`pct = used/(used+avail)`，与 `df` 同口径（`E/hostmon.rs:322-335`） |
| `net` | 见下 | `/proc/net/dev` 等 |
| `sock` | `{tcp,udp,tcp_tw,tcp_orphan}` | `/proc/net/sockstat` + `sockstat6`：`tcp = TCP.inuse + TCP6.inuse`，`udp = UDP+UDP6 inuse`，`tw`、`orphan` 取 TCP 行（`E/hostmon.rs:363-384`） |
| `procs` | 进程数 | `/proc` 下纯数字目录数 |
| `uptime` | 开机秒数（向下取整） | `/proc/uptime` |
| `xray` | 仅 relayd，见 §3 | |

`net` 子结构（`E/hostmon.rs:38-56,213-264`）：
- `ifaces`：参与求和的物理网卡（`/sys/class/net/<n>/device` 存在且非 `lo`；每 300 s 刷新；无物理网卡则退回默认路由网卡）。`rx_bytes/tx_bytes` = 它们的累计和；`rx_bps/tx_bps` = 与上次样本之差 / 时间差（`dt>0.5 s` 且计数不回退才算，否则 0）。
- `default_iface` / `pub_iface`：默认路由网卡（`/proc/net/route` 里目的 `00000000` 掩码 `00000000` 的最小 metric；无 IPv4 默认路由则看 `/proc/net/ipv6_route`）（`E/hostmon.rs:337-361`）。
- `pub_rx_bytes/pub_tx_bytes`：公网网卡开机以来累计。
- **`pub_rx_delta/pub_tx_delta`**：与「持久化基线」之差，供供应商流量配额按账期求和（出站用量 = Σ`pub_tx_delta`）。规则（`E/hostmon.rs:52-55,129-150,247-261`，`D/README.md:69-72`）：
  - 基线 `{boot_id, iface, rx, tx}` 落在 `net.json`，**每批写入 outbox 之后**才保存（`E/runner.rs:196`）。
  - 同一 `boot_id` 内计数回退 → 视为清零（增量=当前值）；`boot_id` 变了 → 基线按 0（增量=开机以来累计）；公网网卡换了 / 首次运行（无状态）→ 第一个样本只记基线、增量 0（不把开机以来的历史算进本账期）。

### 1.3 批（batch）与序号

批次 JSON（`E/runner.rs:292-319`）：

```json
{"v":1,"kind":"node|relay","host_id":"...","boot_id":"...","seq":<u64>,"agent":"relayd/0.x",
 "created_at":<unix秒,2位小数>,"meta":{"kernel":"","os":"","ncpu":2},
 "outbox":{"pending":<未确认批数>,"dropped":<累计被挤掉的批数>},
 "samples":[<Sample>...],
 "xray":{...}, "probe":{...}, "access":{...}, "legacy_probe":{...}}   // 后四项仅 relayd，且仅在有内容时出现
```

- `seq`：每批递增 1，持久化在 `state.json`（`{boot_id,seq}`，原子写、不 fsync：断电会换 boot_id，所以不影响语义）。**同一 boot_id 的状态才沿用**；无状态（首次 / 被清）则从当前 unix 秒起算（每 10 s 才用一个序号，所以一定大于本次开机以前用过的任何序号）（`E/runner.rs:57-86`）。**不要照搬**这个 hack，见 §9。
- 中控去重键 = `(host_id, boot_id, seq)`（§8.4）。
- SIGTERM/SIGINT：把未满的一批也 enqueue 后退出（`E/runner.rs:168-176`）。SIGHUP 忽略。

### 1.4 Outbox（磁盘环形队列）与离线行为

单文件 `outbox.dat`，0600（`E/outbox.rs:1-9`）：
- 头 32 B：`"FEOB" | ver u32=1 | head u64 | dropped u64 | reserved`；记录 `len u32 LE | crc32 u32 LE | payload(一批 JSON)`。
- `push`：追加 + `fdatasync`；若「未确认字节 + 新记录 > 上限」则**从头丢最旧的整条记录**并累计 `dropped`；单条记录 > 上限 → 报错丢弃本批（`E/outbox.rs:119-149`）。
- `ack`：只改头里的 head（不 fsync：崩溃后最多重发一批，靠中控去重）；全确认后截断为空；将超过 上限+32 B 时 compact（写新文件 + rename）→ 磁盘占用 ≤ 上限+32 B。
- 打开时从 head 起逐条校验 crc，撕裂尾部截掉；`outbox.dat.tmp` 残留删除（`E/outbox.rs:51-100`）。
- 上限 `outbox_max_bytes`：默认 2 MiB（允许 64 KiB–64 MiB）；relayd 在 rollout 里设 16 MiB（`D/rollout.sh:207`）。[估算] nodeagent 一批约 1–2 KB → 2 MiB ≈ 1000+ 批 ≈ 3 h 离线；relayd 批更大（含 xray entries、probe、access），16 MiB 估计同量级。

上报循环（`E/runner.rs:201-282`）：
- 只要 outbox 非空且到了 `next_push` 就发；**最旧的先发，收到确认再发下一条**（严格有序，不并发）。
- 补传：一轮最多 200 条或 20 s，之后 1 s 再来（`E/runner.rs:272-275`）。
- 离线时：继续采样、继续写 outbox（满了挤最旧并计数 `dropped`）；恢复后按序补传，**样本带原始 `ts`**，中控按样本时间落分钟桶（§8.5）。
- 背压：outbox 未确认字节 / 上限 = `pressure`，传给扩展的 `on_batch`；访问日志在 `pressure > 0.5` 时暂停读取（§5）（`E/relay.rs:120-123`）。
- 令牌读不到 → `max(backoff, 30 s)` 后重试，并写 status（`E/runner.rs:205-216`）。

HTTP 语义（`E/push.rs:4-66`）：
| 响应 | 边缘动作 |
|---|---|
| 2xx（含 `{"status":"duplicate"}`） | 确认（ack），出队；`"live":true` → 全局 `LIVE_UNTIL = now+90` |
| 400 / 413 / 422 | `Rejected`：**丢弃这一批**（计 `rejected`），不堵队列 |
| 429 | 重试，等 `Retry-After`（缺省 5 s；clamp 1–600 s） |
| 401 / 403 / 5xx / 网络错误 | 重试（401/403 = 令牌问题，不丢数据）；重读令牌；断开 keep-alive |

- 连接：connect 5 s、总超时 `http_timeout_s`（默认 15 s）、复用 1 条 keep-alive、`redirects=0`、`User-Agent: fleet-edge/<ver>`、`Authorization: Bearer <token>`、`Content-Type: application/json`（`E/push.rs:21-32`）。
- 退避（`E/push.rs:68-83`）：`base = min(5 s·2^min(fails,6), 60 s)`，乘 `[0.5,1.5)` 抖动 → 最长 90 s。**硬约束：上限必须小于中控的离线判定（120 s）**，否则中控重启后全体被误判离线。
- 种子：`Rng` 用 `fnv1a(host_id)` 与时间、pid 混合，只做抖动/错峰，不用于密码学。

### 1.5 `--once` 诊断模式（`E/runner.rs:100-118`）

采两个样本（间隔 1 s），打印一批 JSON，检查令牌文件可读，打印上报地址，不写状态、不上报。`rollout.sh` 以服务用户身份「干跑」它来校验配置与令牌（`D/rollout.sh:248-250`）。重写时应保留一个等价的自检命令。

### 1.6 资源与安全预算（实测，用于对照新实现）

- nodeagent：RSS ≈ 2 MB，CPU 0.02% 单核；`MemoryMax=24M CPUQuota=3% TasksMax=8`（`D/fleet-nodeagent.service:19-27`）。
- relayd：RSS ≈ 6 MB，CPU 0.45–0.53% 单核（含常驻 xray 客户端；进程自身 0.38–0.41%）；`MemoryMax=128M CPUQuota=50% TasksMax=64 Nice=10 IOSchedulingClass=idle OOMScoreAdjust=500 LimitNOFILE=256`（`D/fleet-relayd.service:28-41`，`D/README.md:120-130`）。
- **CPUQuota 是突发上限，不是平均值**：旧值 5% 把握手突发切成每 100 ms 5 ms，探测延迟被放大 3–6 倍；改 50%（`D/fleet-relayd.service:32-36`）。新实现若用 cgroup 限额，同样按突发设。
- relayd 要读 root 0600 的 Xray 配置（含 REALITY 私钥与 SS 密码），所以以 root 运行但无 capability；文件系统只读，仅 `StateDirectory`、`/etc/fleet/xray`、`/run/lock` 可写；`/root /etc/ssh /etc/shadow /etc/fleetd /var/lib/fleetd /etc/x-ui /etc/caddy ...` 不可见；禁用 `kill tkill tgkill rt_sigqueueinfo pidfd_send_signal process_vm_writev` 等 syscall（`D/fleet-relayd.service:42-69`）。

---

## 2. relayd 观测：Xray 统计、进程、连接（`E/xray.rs`）

### 2.1 采集频率与顺序

- **每个采样周期（默认 5 s）一次**：`collect()`（`E/xray.rs:930-1027`）：刷新进程 → 必要时重读 Xray 配置 → `QueryStats` → 增量与累计 → 连接数。
- **每批（默认 10 s）一次**：`on_batch()` 组装批次顶层 `xray`（`E/xray.rs:1040-1082`），并先 `check_seed()`（种子文件 mtime/sha 变化才应用）。
- 每 60 s：按 mtime 重读 Xray 配置（`E/xray.rs:851-900`）；`x-ui` 不存在时每 60 s 才重扫进程（`E/xray.rs:906-927`）。
- 每 10 分钟：`periodic_check`（lsi/lso/lsrules 与磁盘配置一致性自检，见 §6.7）。

### 2.2 Xray StatsService 调用

- gRPC over **h2c 明文**，目标 = 配置里 `api` 入站（默认从 Xray 配置的 `api.tag` 入站推出：`listen` 为路径则 `unix:/path`，否则 `listen:port`，`0.0.0.0`/空 → `127.0.0.1`；再缺省 `127.0.0.1:62789`）（`E/xray.rs:585-611,847-849`）。支持 `unix:` 前缀的 UDS（`E/xray.rs:137-143`），但生产现仍是 TCP loopback 62789（`D/README.md:47-48`）。
- 方法 `/xray.app.stats.command.StatsService/QueryStats`，**请求体为空消息**（`pattern=""`、`reset=false`），所以从协议层面不可能清零（`E/xray.rs:1-8,196-213`）。响应解码手写 protobuf：`Stat{name=1 string, value=2 int64}`，最大响应 16 MiB。
- **每 5 s 复用一条 h2 连接**；失败则丢连接重连重试一次；单次超时 **3 s**（`E/xray.rs:196-213`）。
- 唯一会改计数的调用：`StatsService/GetStats(name, reset=true)`，仅由 `reset_traffic` 命令调用，一次一个计数，3 s 超时（`E/xray.rs:215-234`）。
- 计数名格式 `inbound>>>TAG>>>traffic>>>uplink|downlink`、`user>>>EMAIL>>>traffic>>>…`、`outbound>>>TAG>>>…`（`E/xray.rs:481-500`）。`api` 入站自己的流量被忽略（`E/xray.rs:972-974`）。
- Xray 配置须开启统计：`stats:{}`、`policy.system.statsInboundUplink/Downlink=true`、`policy.levels.0.statsUserUplink/Downlink=true`（lab 另开 `statsUserOnline`，生产是否使用 `[推断]` 未用）（`LAB:39-49`，`T/fixture/srv.json:11-23`）。

### 2.3 增量与累计账（「真实来源」）

核心目的：Xray 重启会清零计数，但客户计费需要跨重启、跨 relayd 重启的**累计流量**。机制：

- `core_id = "<boot_id>:<xray pid>:<xray start_ticks>"`，标识 Xray 一次「纪元」（`E/xray.rs:956`）。
- 每个计数按 `delta_for`（`E/xray.rs:507-523`）求增量：
  - 首次启动且无种子（`!have_baseline`）：只记基线，增量 0；
  - 纪元变了（Xray 重启）：计数从 0 重来，增量 = 当前值；
  - 同纪元计数回退：视为被重置，增量 = 当前值；
  - 同纪元新出现的计数（运行中新增入站/用户）：增量 = 当前值；
  - 同纪元正常增长：当前值 − 上次值。
- 累计值 `cum[计数名] += 增量`，**跨 Xray 重启保持**，随批次写入状态目录 `xray.json`（enqueue 之后 `commit`，`E/xray.rs:1084-1088`）。基线 `prev` 是「合并而不是替换」：入口被 rmi 停用时计数可能暂时消失，重新出现时仍按原基线求差（`E/xray.rs:1002-1009`）。
- 速率（B/s）：增量 / `dt`，仅 `dt>0.5 s` 且有基线时（`E/xray.rs:997-1001`）。
- **种子（seed）**：从旧 3x-ui 切换时用 `D/xui-seed.py` 从 `x-ui.db`（`inbounds.up/down` 按 tag + `client_traffics.up/down` 按 email）只读导出 `{ts, source, entries:[{kind,tag,up,down}]}`；relayd 在 sha256 变化时应用：`cum` 整体换成种子值，`accum_since = ts`；若当前 Xray 在种子时刻之后才启动 → 下一次读数全部计为增量（精确），否则以当前读数为基线（`E/xray.rs:718-776`）。**这是一次性迁移机制，不要照搬**（§9）；但「累计值 + accum_since」的账本模型要保留。
- 管理操作前补采 `fold_now()`（`E/xray.rs:795-810`）：入站被移除/替换前把当前增量并入累计，随下一个样本上报，保证停用/换 UUID 不丢流量。
- `reset_counters`（`E/xray.rs:812-837`）：先补采 → 对每个名字 `GetStats(reset=true)` → 把「清零前的值 − 基线」并入 `cum`，**累计值不清零**，`prev[name]=0`；返回 `[{name, value_at_reset, delta_folded, cum}]`，并作为 `ledger_notes`（`{"type":"traffic_reset","at","counters"}`）随下一个 xray 批次上报（`E/xray.rs:572-574`，`E/relay.rs:104-106`）。

### 2.4 进程与版本

- 扫 `/proc/*/cmdline`：xray = `argv[0]` 文件名 == `bin`（默认 `xray`）**且**命令行含 `proc_match`（默认 `/etc/fleet/xray/config.json`，用来排除旧 komari-relay-probe 的隔离 xray）；x-ui = `argv[0]` 文件名 == `x-ui`（`E/xray.rs:358-385`）。
- 进程标识 `{pid, start_ticks(/proc/pid/stat 第22列), exe(argv[0])}`；`started_at = btime + start_ticks/CLK_TCK`。xray 仍存活判断 = `start_ticks(pid)` 不变（`E/xray.rs:392-394`）。
- 版本：**仅在 xray 进程换代时**执行一次 `<argv0> version`（5 s 超时），取首行第二列，如 `"Xray 26.7.28 (...)" → "26.7.28"`（`E/xray.rs:396-408,921-926`）。

### 2.5 连接数与来源

- 数据源 `/proc/net/tcp` + `/proc/net/tcp6`（状态 `01`=ESTABLISHED），统计「本地端口 ∈ 入站端口集合」的条数；同时记每端口来源 IP（去重、按首次出现顺序、每端口 ≤100 个；IPv4 映射的 IPv6 归一成 IPv4）（`E/xray.rs:414-477`）。**每个采样周期执行**。
- 入站端口集合 = 配置里所有非 api 入站的 `port` ∪ 计数名里 `inbound-<端口>` 的端口 ∪ `inbound_ports` 配置（`E/xray.rs:885-887,960-966`）。
- 样本 `xray.conns = {"<port>": n}`；批次 `xray.srcs = {"<port>": [ip...]}`。
- **不要照搬**：包含回环（探测自己的连接，见 §4.4/§9）；只覆盖 TCP；每 5 s 全表扫描。

### 2.6 样本里的 `xray` 与批次顶层 `xray` 结构

样本（`E/xray.rs:937-1026`）：

```json
{"running":true,"latency_ms":3.2,"ok":true,"dt":5.0,
 "baseline":true,        // 仅首次无基线
 "reset":true,           // 本次有计数被判定重置
 "d":[["inbound","inbound-20001",<up增量>,<down增量>],["user","node-07",up,down],...],  // 仅非零
 "conns":{"20001":12,...}}
// 失败时：{"running":..,"latency_ms":..,"ok":false,"err":"..","conns":{...}}
```

批次顶层 `xray`（`E/xray.rs:1067-1081`，`E/relay.rs:97-107`）：

```json
{"srcs":{"20001":["1.2.3.4",...]},"api":"127.0.0.1:62789",
 "config":{"path","mtime","error","inbounds":[{"tag","port","protocol","users":[email...]}]},
 "xray":{"running","pid","started_at","version"},"xui":{"running",...},
 "core_id":"...","last_ok":<unix秒>,"errors":<累计失败次数>,"accum_since":<unix秒>,"seed":{...},
 "ports":[...],
 "entries":[[kind,tag,cum_up,cum_down,raw_up,raw_down,rate_up_Bps,rate_down_Bps],...],
 "spec_generation":<u64>,"manage":{...},"ledger_notes":[...]}
```
- `entries` 含所有 kind=inbound/user/outbound 的计数（api 入站除外）。`manage` 见 §6.8。

---

## 3. relayd 探测（`E/probe.rs`、`E/pathcheck.rs`、`P/` 即旧 `fleet-probe`）

探测只产出**逐次尝试的原始证据**（`ProbeResult`），综合状态只在中控算（`P/WIRING.md:3-4`）。结果存 `probe.db`，批次顶层 `probe`（schema `fleet.probe.v1`）上报。

### 3.1 探测对象（targets）

- 来源（只读，每 **60 s** 重读，内容变化才通知调度器；读失败且列表为空时保持原对象，不清空）（`E/probe.rs:61-78,329-340`）：
  1. Xray 配置里所有 `outbounds[]` protocol==`shadowsocks`（取 `settings.servers[0]` 或扁平 `settings`：`address,port,method,password`）→ SS 目标；protocol==`socks`（NodeMach SOCKS5，`users[0].{user,pass}`）→ `method="socks5"`（`P/targets.rs:85-135`）。
  2. 身份映射 `/etc/komari-relay-probe/config.json` 的 `assets[]`：`outbound_tag → {uuid, name, ip(期望出口)}`；不存在则 `node = 出站 tag`。`expected_ip` 缺省 = 从 tag `ss-1-2-3-4` 解析出 `1.2.3.4`，否则 `address`（`P/targets.rs:78-82,163-204`）。**不要照搬**（§9：legacy 映射、tag 解析 IP）。新系统应由中控在 spec 里直接下发 `ProbeTarget{node_id, addr, port, cipher, secret, expected_ip, udp}`（`P/WIRING.md:91`）。
- `TargetInfo`（批次里的 `targets`，**不含密码**）：`{node,name,tag,addr,port,method,expected_ip,udp}`。中控据此把「没结果的节点」记 unknown 而不是已下线。
- 调度键包含 `node|tag|addr|port|method|username|password|expected_ip|udp`，任一变化 → 该节点循环重启（`P/scheduler.rs:239`）。
- 加密方式：SS TCP 支持 `aes-128-gcm`、`aes-256-gcm`、`chacha20-ietf-poly1305`（SIP004）、`2022-blake3-aes-128/256-gcm`（SIP022）；SS UDP 仅 SIP004 单包往返（2022 UDP 未实现 → `unsupported_cipher`）（`P/WIRING.md:11`，`P/checks.rs:373-387`）。

### 3.2 检查项总表（relayd 默认 `ProbeConfig`，`P/scheduler.rs:40-58`）

| check | 间隔 | 超时 | 目标 | 成功判定 / 值 | 失败码（`reason_code`，`legacy_rc`） |
|---|---|---|---|---|---|
| `tcp`（L1） | 30 s ±10%，每节点按 `fnv(node)` 固定错峰 | 5 s | 落地 `addr:port`（域名走系统解析） | connect 成功；`value_ms` = 整毫秒且 ≥1 | fail/timeout 一律 `upstream_down`（7）（`P/checks.rs:78-100`） |
| `egress`（L2） | 60 s ±10%（从周期开始算；重试拖过则立即开始下一周期） | 每个来源 8 s（含经 SS 的 TCP+TLS+HTTP） | 经 SS（或 SOCKS5）按**域名**依次访问 ① `https://www.cloudflare.com/cdn-cgi/trace`（取 `ip=` 行）② `https://api.ipify.org`（正文=IP） | HTTP 200 且解析出 IP → ok，`actual_ip`=该 IP，`value_ms`=总耗时；**不与 expected_ip 比较**（2026-09-24 起，只当数据上报） | 见 §3.3 |
| `control` | 60 s ±10%，`node="_relay"` | 8 s | 线路机**不经 SS** 直连上述两个来源 | 同 L2 | 同 |
| `udp` | 5 min ±10%，仅 `udp=true` 的节点 | 每包 2 s | 经 SS UDP relay 向 `stun.cloudflare.com:3478`、`stun.l.google.com:19302` 各最多 2 次 STUN Binding | 任一应答 → ok，`actual_ip` = 去重后的映射地址逗号拼接；不判不一致 | 全无应答 → `unknown(udp_no_reply)`；2022 密码 → `unsupported_cipher` |
| `ipv6` | 15 min ±10% | 8 s | 经 SS 访问只有 AAAA 的 `api6.ipify.org` | 连通 → `fail(ipv6_egress_present)`；握手阶段被断/超时 → `ok(ipv6_absent)`；连不上落地 → `unknown(upstream_down)` | |
| `path`（全链路，§3.5） | 见 §3.5 | 每次尝试 ≤18 s | 本机 Xray 客户端 → `127.0.0.1:<入口端口>`（VLESS+REALITY）→ cloudflare trace | rc=0 且拿到 ip | 见 §3.5 |
| `tcp`（线路机互探） | `peer_interval_s`（默认 30，clamp 10–600） | 5 s | 配置 `[relay].peers = ["<资产uuid>@<ip>:<端口>"]`（其他线路机的主节点入口） | connect 成功；`value_ms` 整毫秒 ≥1；`target`=`ip:port` | `upstream_down`（7）（`E/probe.rs:204-227`） |
| `egress`（线路机自检） | 随 path 周期 | 同 path | 主节点入口（路由到 freedom 的入口）的 path 结果改写成 egress 行：`node`=线路机资产 uuid，`expected_ip`=本机公网 IP，`target="127.0.0.1:<端口>"` | 同 path | 原因码映射略不同（7/97→`upstream_down`，35/60→`auth`）（`E/pathcheck.rs:315-352`） |

- 全局并发：`egress`/`control`/`ipv6` 共用信号量 **4**；`tcp`/`udp` 不占用（`P/scheduler.rs:80-186`）。
- HTTP 客户端：自写 HTTP/1.1，`Connection: close`，`User-Agent: fleet-probe/<ver>`，rustls+ring，webpki 根，ALPN `http/1.1`，会话恢复缓存（`P/http.rs:22-29,104-106`）。证书不可信 → `tls_invalid`；TLS 握手被断 → `auth`；HTTP 非 200 → `http_error`；解析不出 IP → `bad_response`。
- 启动自检：先跑一轮 `control`，两个来源都失败 → warn `probe_selfcheck`（线路机自己出不了公网）（`E/probe.rs:292-304`）。
- 结果持久化：探测线程收到结果后攒到 64 条或每 10 s 一次事务写 `probe.db`；每小时 `prune`（24 h）（`E/probe.rs:306-345`）。
- 批次组装：每批取游标之后最多 `batch_limit`（默认 200）条；**批次写入 outbox 之后**才前移游标（`ack(last_id)`，游标不回退）（`E/probe.rs:166-201`）。批内含 `first_id,last_id`（用来发现边缘裁剪造成的缺口）、`generated_at`、`relay`、`results`、`targets`。无新结果则整个 `probe` 键不出现（`--once` 例外：列出 targets）。

### 3.3 L2（egress）一个周期的状态机（`P/scheduler.rs:134-166`）

- 每周期最多 **3 次尝试**，间隔 30 s；成功或到第 3 次即停，**只有最后一次标 `final=true`**（HL-04：只有 final 算证据；`attempt` 1..3）。
- 来源选择：每次尝试前取 150 s 内的控制路径结果：某来源控制路径失败 → 本次只用另一个；两个都失败且原因不是 `upstream_down` → 该次记 `unknown(probe_target_down)`、`final=true`、本周期立即结束（公共来源故障不让全体节点变红）。`upstream_down` 不受控制路径影响（节点自己的问题）。
- 单次尝试：解析落地地址失败 → `upstream_down`；`unsupported_cipher` → `unknown`；依次试来源，连不上落地（`upstream_down`）立即停止换来源（`P/checks.rs:221-289`）。
- `legacy_rc`（兼容旧 curl 退出码）：ok=0，`timeout`=28，`auth`=35，`upstream_down`=7，其余 -1（`P/types.rs:114-124`）。
- `status` ∈ `ok|fail|timeout|unknown`；`value_ms` 失败/超时一律 null，**绝不当延迟**。

### 3.4 「漏出」标记 egress_leak（`E/probe.rs:229-256`）

- `leak_ips` = 本线路机公网 IP ∪ 各 peer 的 IP。对 `egress|udp|path` 且 `status==ok` 的结果，若 `actual_ip`（逗号拼接的任一项）∈ `leak_ips` → 改写为 `fail`、`reason_code="egress_leak"`、`value_ms=null`（path 的 `healthy=false`）。含义：落地节点出口等于某线路机公网 IP = 流量没走落地、从线路机直接出去了。**这是旧系统唯一保留的出口 IP 判定**（负责人决定不做出口 IP 比对，只保留漏出标记）。
- 例外：线路机自检行（`check=egress` 且 `target` 以 `127.0.0.1:` 开头）；path 的入口出口为 freedom（主节点，本来就从线路机出去）的端口（`direct_ports`）。node 探测线程里 `skip_ports=[]`（`E/probe.rs:323`）。

### 3.5 原生全链路检查 `check=path`（`E/pathcheck.rs`）

- 周期 `path_interval_s`（默认 60，clamp 30–3600）；每轮睡 `interval − 用时`，至少 5 s（`E/pathcheck.rs:511`）。配置 `[probe].path=false` 可关。
- 待测入口 = Xray 配置里 protocol=`vless` 且 `streamSettings.security=="reality"` 的所有入站；每个取**第一个用户**的 `id/flow`、`realitySettings.serverNames[0]`、`shortIds[0]`、`network`，由 `privateKey`（base64url 无填充 x25519）算出公钥（`E/pathcheck.rs:58-95`）。入口的路由出口 = 第一条「`inboundTag` 含该 tag 且无 `domain`/`ip` 字段」的规则的 `outboundTag`；出口协议 `freedom` = 主节点入口。期望 IP = 身份映射或 tag 解析。
- 做法：生成一份临时 Xray **客户端**配置（每个入口一个 `127.0.0.1:<空闲端口>` SOCKS 入站 `pc-in-N`（noauth, udp=false）→ VLESS+REALITY 出站 `pc-out-N`（`address=127.0.0.1`、`port=入口端口`、`encryption=none`、`fingerprint=chrome`、`serverName/publicKey/shortId`）；`log` 全部 `none`），用**线上同一个 xray 可执行文件**常驻运行，入口配置不变就复用（`E/pathcheck.rs:101-114,254-311`）。不改受管 Xray 配置。
- 一次尝试（`E/pathcheck.rs:117-178`）：连本机 SOCKS → SOCKS5 CONNECT `www.cloudflare.com:443` → TLS → `GET /cdn-cgi/trace`，取 `ip=`。rc 口径（curl 兼容）：0 成功、7 连不上本机客户端、97 SOCKS 握手失败、35 TLS、60 证书、56 HTTP 读失败、22 HTTP 非 200、28 超时。`phase` ∈ `connect|tls|read|deadline`。
- 每入口最多 **2 次尝试**，每次 ≤18 s（取 `18 s` 与「剩余-1 s」小者），整周期总 deadline **140 s**，并发 **3**；`ok = rc==0 && 拿到 ip`（**不比较出口 IP**）；`status`: ok / `timeout`(rc==28) / fail；失败 reason：28→`timeout`，7/97→`entry_unreachable`，35/60→`tls`，22/56→`http_error`，其余→`bad_response`；`final_attempt=true`，`source="cloudflare"`，`path{entry_port,entry_tag,healthy,rc,phase,ms,attempts[{rc,ms,phase,actual_ip}]}`。新起客户端后等其监听最多 5 s 再等 0.8 s。
- 自适应节奏（`E/pathcheck.rs:383-408`）：每周期（60 s）只测**主节点入口**（`[relay].master_port`，缺省=路由到 freedom 的入口）与「上次不健康的入口」；**全量每 `2×interval−5 s`（≈115 s）一次**；fleetd 在上报回执里给 `"live":true`（有人在看）后 90 s 内每周期都全量。
- 线路机自检：主节点入口的结果额外产出 `check=egress` 行（HL-04：第 1 次失败且有第 2 次时，第 1 次 `final=false`；`measured_at` 依次减 0.001 s 排序）（`E/pathcheck.rs:315-352`）。
- 客户端进程回收：见 §9「seccomp 下不能 kill」hack（`E/pathcheck.rs:237-303`）。

### 3.6 中控侧对探测的使用（边缘需要保证的契约）

- 自然键幂等：`(node, check, measured_at, attempt)`，不依赖 `id`。
- 保留：边缘 24 h；中控 `probe_land` 14 d；单批 ≤5000 条入库（`I/src/lib.rs:469-494`）。
- 体量 [旧文档]：16 节点约 55 条/分钟，每条 250–350 B，30 s 一批约 8 KB（`P/WIRING.md:73`）。

---

## 4. 访问日志上报 `fleet.access.v1`（`E/accesslog.rs`，`API:490-496`）

### 4.1 采集

- 路径取自 Xray 配置 `log.access`（每 60 s 随配置重读）；配置项 `[access].path` 可覆盖；`""`/`none` = 无日志（`E/accesslog.rs:303-313`）。生产路径 `/var/log/x-ui/xray-access.log`，由 `team-ops-logrotate.timer` 轮转（**不要停它**）（`D/README.md:57-60`）。
- 游标 `(inode, offset)`，**批次写进 outbox 之后才提交**（`commit`）；崩溃后从上次提交的游标重读，事件 id = `"<inode>:<offset>"`，中控按 id 去重（`E/accesslog.rs:435-445`）。
- 首次（无游标）：从**当前末尾**开始，不重放历史；换了日志文件路径：旧游标作废（同样从当前末尾）。
- 轮转（logrotate `create`+`delaycompress`）：先在同目录 `<文件名>.*`（排除 `.gz`）按旧 inode 找到旧文件读完剩余，再读新文件开头；找不到旧文件 → `rotation_gaps++` 并警告；`offset > size` → `truncations++` 从头读（`E/accesslog.rs:340-355,448-458`）。
- 每批最多 `max_events` 条（配置默认 **400**，clamp 1–2000）、`max_bytes` 2 MiB；到限就停在行边界，剩下下批接着读（**日志文件就是缓冲**）。追赶速度 = 每个主循环批（默认 10 s）最多 `max_events` 条（`D/README.md:60` 写「积压时每 30 s 追 400 条」，与代码的 10 s 批不符，以代码为准）。outbox 未确认字节 > 上限 50%（`pressure>0.5`）→ 暂停读取，只上报状态（`paused:true`），**不丢行**。
- 超过 32 KiB 的畸形行：跳到行尾丢弃，`dropped_lines++`、批内 `dropped`；半行（Xray 还在写）停在行首下次再读（`E/accesslog.rs:22,373-399`）。

### 4.2 行解析（逐字移植旧 `bridge.parse_access`，`E/accesslog.rs:131-238`）

格式：`YYYY/MM/DD HH:MM:SS[.frac] [from ]<来源ip:port> accepted|rejected <network>:<host>:<port> [<inbound> -> <outbound>] [email: <client>]`

- 时间戳是**进程本地时区**的墙钟，用 `mktime` 转 unix（解析失败用当前时间）（`E/accesslog.rs:52-71`）。**不要照搬**（时区依赖）。
- 路由分隔符 `->` / `>>` / `=>`（正则 `\s*(->|>>|=>)\s*`），必须恰好 2 段；入站 tag 必须满足「入口 tag」：`inbound-\d{2,5}` 或 `in-*`；network 必须 `tcp|udp`；host 去 `[]`、转小写、≤253 字符；端口 1–65535；源 IP 归一（IPv4 映射 IPv6 → IPv4）。
- 来源是**回环**的行（relayd 的探测 / path 客户端自己）→ 计入 `loopback` 不产生事件；解析失败 → `unparsed`。
- `kind`：host 解析成 IP → `"ip"` 否则 `"domain"`。
- `action`：`outbound == "blocked"` 或 status==`rejected` 或 outbound ∈ 配置里所有 blackhole 出站 tag（含 `fleet-default-deny`）→ `blocked`，否则 `accepted`（API 里旧名 `routed`）。`rule` 恒为 `null`（Xray 访问日志不含命中的规则）。`client` = `email:` 后的标签（≤120），`outbound` ≤120。

### 4.3 批内结构（`E/accesslog.rs:421-431`）

```json
{"schema":"fleet.access.v1",
 "source":{"file":"/var/log/.../xray-access.log","inode":123,"offset_from":0,"offset_to":4096},
 "dropped":0,
 "events":[{"id":"<inode>:<offset>","ts":1790190000.123,"inbound":"inbound-20007","client":"node-07",
            "source_ip":"1.2.3.4","source_port":51234,"network":"tcp|udp","host":"example.com","port":443,
            "kind":"domain|ip","action":"accepted|blocked","status":"accepted|rejected","rule":null,"outbound":"ss-..."}],
 "backlog_bytes":0,"size_bytes":0,"paused":false,
 "counters":{"lines","events","loopback","unparsed","dropped_lines","rotation_gaps","truncations"}}  // 累计，随状态持久化
// 文件不可读：{"schema","source":{"file"},"error":"...","dropped":0,"events":[]}
```
- `source.offset_to` = 下一批 `offset_from`；轮转时一批可跨两个 inode。fleetd 对 `rotation_gaps`、`truncations` 非零告警。

### 4.4 量级与限制

- [旧文档] 全量事件，不再是旧系统的「每分钟抽样 500 条」；ipintel 直接吃全量（`API:496`）。中控 `access_raw` 保留 3 d，单批最多入库 5000 条，id ≤96 字符（`I/src/lib.rs:496-512`，`I/schema.sql:123-132`）。
- 单批大小风险：400 条 × ≈300 B ≈ 120 KB，加探测 200 条 ≈ 70 KB、xray entries，可逼近中控 256 KB 请求体上限（§9）。

---

## 5. 管理模式 fleet.relayspec.v1（`E/manage.rs` + `API:461-488` + `F/relayspec.rs`）

### 5.1 总览：声明式「完整受管集合」+ generation

- relayd **不收「增删入站」这样的补丁**。中控每一代（generation）下发**完整的期望受管集合**，relayd 与当前配置文件逐 tag 比较得出增/删/替换（`API:473`）。
- 命令（commands）是另一条通道：`restart_xray`、`reset_traffic`、`xray_logs`（只读），随 spec 响应一起下发；与 generation 无关（`restart/reset/logs` 的 op 的 generation 为 null）。
- 「受管」划分（`E/manage.rs:109-136`）：
  - 入站：tag 为 `inbound-<2–5 位数字>` 或 `in-*`；
  - 出站：tag 前缀 `ss-` / `relay-` / `socks-`；
  - 路由规则：`ruleTag` 前缀 `fleet-` 或 `policy-`；
  - **其他一切不受管**：`api` 入站、`outbounds[0]`（R1 默认 blackhole `fleet-default-deny`）、`direct`/`blocked`、`geoip:private`/`bittorrent` 防护规则、以及上架流水线加的 `r-inbound-<port>` 规则（`ruleTag` 不以 `fleet-` 开头，**对 relayd 而言不受管**，`O/relay.rs:61`）、`log/stats/policy/metrics/api/dns` 等其他顶层段。
- 中控状态（`F/relayspec.rs:12-27`）：`relay_observed`、`relay_specs(relay,generation,managed,spec_sha256,base_config_sha256,reason,created_by)`、`relay_acks`、`relay_commands`、`ops(状态 pending|applied|rejected|partial|expired)`、`entry_meta(remark, disabled_inbound 原 JSON+原下标, ledger_note)`。op 与命令 TTL **180 s**（`F/relayspec.rs:30`）；API 说「relayd 60 s 内没取走 → expired」`[文档/代码不一致，见 §9]`。

### 5.2 通信（relayd 端，`E/manage.rs:526-620`）

| 动作 | 端点 | 频率 / 触发 | 备注 |
|---|---|---|---|
| 轮询期望 | `GET /v2/ingest/relay/spec?have=<generation>&wait=0` | 每 `poll_s`=**10 s**，单线程不长轮询 | 200=新 spec/命令；304=无变化；404=fleetd 未启用（`disabled_by_server`，只观测）；中控支持 `wait≤25` 长轮询（`F/api.rs:1247`），relayd 没用 |
| 观测上报 | `POST /v2/ingest/relay/observed` | 配置文件 sha256 变化时（每 60 s 查一次）+ 每小时 | body `{"schema":"fleet.relayobs.v1","relay":host_id,"config_sha256","generation","managed":{inbounds,outbounds,routing_rules},"observed_at"}`；**含 UUID、SS 密码、REALITY 私钥**（`managed_of` 原样）；404→`disabled_by_server` |
| 回执 | `POST /v2/ingest/relay/spec/ack` | 应用后立刻；失败留在 `manage.json` 的 `pending_acks` 里重试 | 4xx（除 404、429）视为永久拒绝，丢弃该回执 |

- 令牌同 §1.1；中控要求令牌的 host 在 `relay_hosts` 里（否则 403）（`F/api.rs:1204-1212`）。body ≤256 KB。
- 在线判定：中控记录最近一次 `GET spec` 的时间，**≤60 s 内**才算「管理模式在线」，所有触及线路机的操作需要它，否则 409（`F/app.rs:427-430`，`F/api.rs:1348`）。
- 中控回 `observed`：`{"ok":true,"generation":<中控当前>,"adopted":bool}`。**收编**：观测 sha 变化、`generation` 已追平、受管集合与期望不同 → 中控把观测到的集合收编为 `generation+1`（`reason=adopted_external_change`），以后写操作基于它（`F/relayspec.rs:101-126`）。外部写入者 = 上架流水线 / cutover 脚本在同一把 flock 下改配置。
- 响应 spec 文档（`F/relayspec.rs:155-170`）：`{"schema":"fleet.relayspec.v1","relay","generation","spec_sha256","issued_at","base_config_sha256","managed":{完整集合},"commands":[{"id":"cmd-…","kind","args","op_id","expires_at"}]}`。`generation==0`（未接管）且有命令 → 仍返回，`managed/spec_sha256/base_config_sha256=null`，relayd **只执行命令，不碰配置**，回执 `generation:0`。

### 5.3 应用一代 spec 的全流程（`E/manage.rs:707-887`）

先决：启动时 `self_check`（拿锁 5 s、在配置目录写删一个探针文件、读解析配置）失败 → `ready=Err`，之后所有 spec 直接 `rejected: not_ready:…`（`E/manage.rs:391-405,729-731`）。

全程持有 `flock /run/lock/fleet-xray-config.lock`（等 ≤60 s，与上架流水线、cutover **同一把锁**；`LOCK_EX|LOCK_NB` 每 100 ms 轮询）：

1. 读配置、sha256（`config_sha256_before`）。
2. `build_candidate(current, spec.managed)`（`E/manage.rs:147-199`）：
   - spec 里出现非受管 tag / 非 `fleet-|policy-` 规则 → 报错拒绝；
   - 入站/出站：受管对象**按 spec 的顺序依次填进文件里原本属于受管对象的位置**，非受管对象原位不动，多出来的追加末尾（所以纯调序也会改写文件）；spec 内重复 tag 去重；
   - 路由规则：非受管规则保持文件顺序；`policy-*` 规则整体作为一块，插在**第一条 `inboundTag` 含入口 tag 的非受管规则之前**（锚点：在 api / geoip:private / bittorrent 防护之后、各入口路由之前）；`fleet-*` 规则按 spec 顺序放**最后**。
3. 候选 == 当前 → `ack.diff={"noop":true}` 直接 `applied`（文件已是期望：首次收编，或上次写完文件、未回执就崩了）。
4. `spec.base_config_sha256 ≠ sha_before` → `rejected "base_changed"`（有人抢先改过；中控会先收编再重发）。
5. **守卫 `guard`**（`E/manage.rs:202-295`，无论 spec 怎么写都拒绝，`error:"guard: …"`）：
   - 非受管入站变了 / 非受管出站变了 / `outbounds[0]` 变了 / 非受管规则或其相对顺序变了 / `routing` 除 `rules` 外变了 / 其他顶层段变了；
   - **每个留下来的入口入站都必须有一条「只按 inboundTag 匹配的直通路由」**（含 `inboundTag` 且有 string 型 `outboundTag`/`balancerTag`，且 `domain,ip,source,sourcePort,port,network,protocol,user,attrs,localPort,localIP,vlessRoute,process` 全为空）；否则它的流量会落进 R1 默认 blackhole。
   - 同时产出 `diff`：`config_sha_before/after`（canonical JSON sha256 前16位）、入站/出站 `{changed,added,removed,identical}`、入站顺序 `{reordered,before,after}`、非受管是否逐字相同、`explicit_rules`/`policy_rules` 的增删改、`entries_without_route`。
6. `xray run -test -c <候选副本>`（90 s 超时；副本里 `log.access/error` 置 `"none"`，因为沙箱里 `/var/log` 只读，而日志路径不属于本次变更）。
7. 取运行态 `rv_before`（`lsi`+`lso`+`lsrules`），并 `fold_now()` 补采计数（入站被移除/替换期间不丢）。
8. **逐项经 Xray API 生效，不重启 Xray**（每项把对象写进临时 JSON 文件，调 `xray api <cmd> --server=<api> <file|tag>`，15 s 超时，`XRAY_LOCATION_ASSET` = 配置的 asset_dir 或 xray 所在目录）。顺序（`E/manage.rs:783-846`）：
   1. 出站：新增 `ado`；变化 `rmo`+`ado`；
   2. 入站：删除 `rmi`；变化（含客户端变化）`rmi`+`adi`；新增 `adi`；
   3. 规则：`cand.rules != before.rules` 时**整表替换** `adrules`（不带 `-append`，原子），保证位置；
   4. 出站删除 `rmo`（此时规则已不再引用）。
9. 核对运行态（`runtime_view` vs `spec_view(候选)`）：入站 tag 集合相等、规则的出站 tag 序列相等、期望出站都在 lso 里（`metrics_out` 等内部出站除外）。`[推断]`弱校验：只比 tag/序列，不比匹配条件内容（`E/manage.rs:467-497`）。
10. 备份旧配置到 `config-backups/config-<unix>-gen<N>.json`（留 30），再 **原子写 `config.json`**：同目录临时文件 `.fleet-relayd.tmp` → fsync → rename → fsync 目录，0600，缩进格式化 JSON + 换行（`E/manage.rs:59-79,855-859`）。**文件写在最后**。
11. 成功：回执 `status=applied`，`config_sha256_after` = 新文件 sha，含 `results[{object,tag,op,ok,error}]`、`diff`、`runtime{before,after}`、`duration_ms`、`xray_restarted=false`。

**回滚**（任一步失败，包括写文件失败，`E/manage.rs:862-880`）：按**相反顺序**对每个已执行步骤执行其 `undo` argv（出站 add→`rmo`；replace→`rmo`+`ado(旧)`；入站 remove→`adi(旧)`；入站 replace→`rmi`+`adi(旧)`；规则→`adrules(旧整表)`；出站 remove→`ado(旧)`），单项撤回失败继续撤其他；半步也撤（`rmi` 成功 `adi` 失败仍执行撤回）；之后再取运行态与「应用前」比对，回执写 `rolled_back_to_before` 与 `undo_errors`，`status=rejected`，error 文案「…；已撤回，运行态回到应用前」或「…但运行态未能确认回到应用前，请人工核对」。文件在失败时从未被改写。

**重复/幂等处理**（`E/manage.rs:622-705`）：
- 同 generation 且 `last_ack.spec_sha256` 相同 → 不重复应用（只处理命令）；回执复用上次回执内容（幂等，中控同 generation 重复回执按 `(relay,generation)` upsert）。
- 同一份 spec 在**同一份文件 sha**上已被拒绝过（`rejected=(spec_sha,file_sha)`）→ 不再重试，等中控收编后重发。
- 应用成功 → `generation=spec.generation`，清 `rejected`；应用后 `force_reload_config`、重报观测、立即 `periodic_check`。
- 中控在 spec 被拒时生成「回到上次已应用内容」的新一代（`revert_rejected_<gen>`，回退本身被拒则停下，避免循环）；op 超时过期时也回退（`F/relayspec.rs:190-276`）。

### 5.4 操作清单（中控 → relayd 的具体含义）

| 操作（中控 API，`API:440-447`） | 中控做什么 | relayd 改 Xray 的方式 | 输入 | 确认 | 回滚 | 幂等性 |
|---|---|---|---|---|---|---|
| **入口停用** `enable/disable` | `disable`：从 `managed.inbounds` 取出该端口入站，**连同原下标**存到 `entry_meta.disabled_inbound`，再 generation+1；`enable`：按保存的原下标插回（配置文件回到停用前的样子，不只是内容相同） | spec 里少/多一个入站 → `rmi` / `adi`（API，不重启）；停用前 `fold_now` 保流量 | 端口（`inbound-<port>`） | §5.3 第9步 | §5.3 | 已停用再停用 → 中控 409「没有入站」；同 spec 重复下发 → `noop applied`。停用后若该入口无直通路由不是问题（入站都没了）；启用时守卫要求有直通路由 |
| **改备注** `rename` | 只写中控 `entry_meta.remark`（≤64 字符，不要求管理模式在线） | **不动 Xray**（Xray 入站没有 remark） | 备注文本 | — | — | 天然幂等 |
| **轮换 UUID** `rotate_uuid` | 对 `inbound.settings.clients[]` 的每个 client 的 `id` 换成新 UUIDv4（保留 `email`、`flow`）；要求 `confirm="<线路机名>:<端口>"`；审计只记新旧前 8 位 | 该入站「变化」→ `rmi`+`adi`（客户端变化 = 入站替换，会断开该入站现有连接；流量计数靠 `fold_now` 与基线合并保持） | 端口、confirm | §5.3 | §5.3 | 同一 spec 重放 → noop；**重试被拒的轮换会生成新 UUID**（对操作者不幂等） |
| **分享链接** | 纯中控：`vless://<id>@<公网IP>:<port>?encryption=none&flow=<flow缺省xtls-rprx-vision>&security=reality&sni=<serverNames[0]缺省www.cloudflare.com>&fp=chrome&pbk=<publicKey或由privateKey推出>&sid=<shortIds[0]>&type=tcp#<URL编码的备注或"线路机名-端口">`；审计 `secret.reveal` | 无 | 端口 | — | — | 只读 |
| **重置入口流量** `reset_traffic` | 建 op + 命令 `reset_traffic {"tag":"inbound-<port>","users":true}` | `GetStats(reset=true)` 对 `inbound>>>TAG>>>traffic>>>uplink/downlink` + 该入站每个 client email 的 `user>>>…` 共 2+2N 个计数；校验名字格式（只允许入口入站/用户的 `traffic` 计数，4 段）；`relayd` 累计账先补采、**不清零**、值并入累计 | `tag` 或 `names[]`、`users`(缺省 true) | 命令结果 `data.counters[{name,value_at_reset,delta_folded,cum}]` | 无（不可回滚） | 命令 id 去重（`done_cmds`，保留 200 个）；重复重置本身无害（先折叠再清零）；`names.dedup()` 只去连续重复 |
| **重启 Xray** `restart_xray` | 建 op + 命令；要求 `confirm="<线路机名>"`（唯一会中断所有连接的操作） | 持锁 → 对**磁盘上的配置** `xray run -test` 不过则拒绝 → `fold_now` → `systemctl restart fleet-xray.service`（命令/单元可配）→ 最多等 **30 s**：全部入站端口在监听（`/proc/net/tcp*` 状态 `0A`）**且** QueryStats 可答 → 刷新进程 → 核对 lsi/lso/lsrules 与磁盘配置一致 | `{}` | 命令结果 `data{restart_ms,ports,runtime}`，不就绪 → 失败「重启后 N ms 内端口/API/规则未全部就绪」 | 无（配置未改，故障由 systemd 管） | 命令 id 去重 |
| **读日志** `xray_logs` | 下发命令 `{"which":"error"|"access","lines":1..500(缺省100)}`；中控同步最多等 25 s（轮询 500 ms） | 只读：配置 `log[which]` 指向的文件从末尾最多读 256 KiB，丢弃首尾不完整行，每行 ≤2000 字符，返回最后 N 行 `{lines,truncated,file,size}`；error 无文件（`""`/`none`）→ `journalctl -u fleet-xray.service -n N -o short-iso`；access 无文件 → 报错 | which、lines | 结果放在命令回执的 `result` 字段（fleetd 0.6.2 约定） | — | 只读 |
| **调序** `reorder_inbounds` | 要求 `confirm=线路机名`；`order` 必须恰好是现有受管入站端口的排列；以 relayd 观测到的实际顺序为准判断是否变化 | spec 里入站顺序变 → 候选改写文件，**API 层面无变更**（入站 `identical`），仅重写文件（原子） | 端口排列 | | | 顺序一致 → 中控 409「顺序没有变化」 |
| **访问规则（策略）** | 中控草稿→预览→发布；发布时按旧 `compile_routing` 生成 `policy-<节点uuid>-match` / `policy-<节点uuid>-default` 规则并作为 spec 新一代下发（每台线路机一个 op）；任一线路机被拒/过期 → 已应用的线路机回到发布前（`failed_rolled_back`） | 受管规则变 → 整表 `adrules`（§5.3 第8.3步）；规则位置见 §5.3 第2步 | 见下 §5.5 | §5.3 | §5.3 | spec 重放 → noop |
| **新增入口/下线节点（add/remove inbound）** | **旧系统里不经 relayd**：上架流水线（`O/relay.rs`）经 SSH 在同一把 flock 下用 Xray API 热加 `ado → adrules(append) → 端口三重检查 → adi → 核对 → 落盘`，relayd 随后通过 observed 收编（`O/relay.rs:1-7`，`O/guard.rs:1-31`）。relayd 自身能力上，spec 里增删入站/出站即可（§5.3），但中控没有对应 API | 同上 | | | | |

### 5.5 访问规则（策略）编译（`E/policy.rs`，逐字移植旧 `policy.py`；relayd 侧只用其中的 `digest`）

- 策略 `{mode ∈ observe|allowlist|denylist|blocked, domains[], note≤500}`；`allowlist/denylist` 必须有域名；每份最多 **300** 个域名。
- 域名规范化：trim+小写+去尾点；分隔符空白 / `,` / `，`；`=host` = 精确匹配，`*.host` 视作 `host`；只接受 ASCII（中文域名须先转 punycode `xn--`，由 fleetd 完成）；每个 label `[a-z0-9-]{1,63}` 且首尾为字母数字；必须含 `.`；不接受 IP、网址、端口、正则（`E/policy.rs:69-131`）。
- 生效优先级：`node:<uuid>` > `team:<团队>` > observe（缺省）。
- 编译（`E/policy.rs:140-184`）：对每个资产（入口 tag `inbound-<relay_port>`）：
  - `observe`：不生成规则；
  - `allowlist`：`policy-<uuid>-match`（`domain:x` / `full:x`，`outboundTag` = 该资产的出站 tag）+ `policy-<uuid>-default`（`outboundTag:"blocked"`）；
  - `denylist`：`policy-<uuid>-match`（`outboundTag:"blocked"`）；
  - `blocked`：只有 `policy-<uuid>-default → blocked`；
  - 全部块插在「第一条 inboundTag 含任一资产入口 tag 的非 `teamops-v1-` 规则」之前。**新系统已改用前缀 `policy-`（旧文档 `teamops-v1-`）**，`digest` = canonical JSON（`sort_keys`、紧凑分隔符、`ensure_ascii`）的 sha256，用于配置差异的 `config_sha_*`。

### 5.6 `periodic_check` 与 `state_json`

- `periodic_check`（启动时 + 每 10 min）：读磁盘配置，取 lsi/lso/lsrules，对比 → `runtime_check{ts, match, rules_match, inbounds_match | error}`；不一致仅 warn，**不自动修复**。同样错误只记一次日志（`E/manage.rs:499-522`）。
- 批次 `xray.manage`（`E/manage.rs:407-413`）：`{enabled, server_enabled, spec_generation, pending_acks, ready(true|错误串|null), last_ack{generation,status,applied_at,error}, runtime_check}`。批次 `xray.spec_generation` 用于 fleetd 重启后恢复状态（`API:478`）。

### 5.7 Xray 命令行调用的运行方式（`E/child.rs`，`E/manage.rs:419-445`）

- 调用：`<xray> api lsi|lso|lsrules|adi|rmi|ado|rmo|adrules --server=<api> [file|tag]`，`xray run -test -c file`，`xray version`；输出解析为 JSON（失败退回 `{"raw":…}`）。
- 需求：子进程带超时、边跑边读输出（`lsi` 在 central 上输出 >64 KiB，只 `try_wait` 不读管道会卡死）、**无论成败都回收**（避免僵尸）、stdout 上限 16 MiB / stderr 256 KiB。

---

## 6. Xray 配置形状与命名约定

### 6.1 命名

| 对象 | 约定 | 来源 |
|---|---|---|
| 入站 tag | `inbound-<port>`（端口 2–5 位），或 `in-*`；API 入站 tag `api` | `E/manage.rs:109-114`，`O/relay.rs:47` |
| 主节点入口 | 路由到 `freedom`(`direct`) 的入口；中控=线路机的「主节点」，端口 line1=20005、central=20001、line3=20001 | `D/relays.sh:6-13` |
| 入站端口 | 20001 起（200xx）；api 入站 `127.0.0.1:62789` | `LAB:40`，`D/README.md:47` |
| 客户端 email | `node-<NN>`（如 `node-07`）；用户计数名 `user>>>node-07>>>traffic>>>…` | `O/relay.rs:50-52`，`F/relayspec.rs:348` |
| 出站 tag（落地 SS） | `ss-<ip 点换横线>`，NAT 机加 `-<公网SS端口>`（如 `ss-75-37-237-38`、`ss-1-2-3-4-12345`）；SOCKS5 高速代理（NodeMach）`relay-<n>`/`socks-*` | `O/relay.rs:39-42`，`P/targets.rs:223` |
| 路由规则 tag | 上架流水线：`r-inbound-<port>`（不受 relayd 管）；relayd 受管：`fleet-explicit-<入站tag>`（靠默认出站出网的入口的显式规则，直连主节点）、`policy-<uuid>-match|default` | `O/relay.rs:49`，`T/ops/build.py:36`，`E/policy.rs` |
| 默认出站 | `outbounds[0]` = `{"tag":"fleet-default-deny","protocol":"blackhole"}`（R1 加固：默认丢弃）；其后 `direct`(freedom, domainStrategy AsIs)、`blocked`(blackhole)、各 `ss-*` | `T/ops/build.py:4,38`，`E/manage.rs:1053` |

### 6.2 VLESS+REALITY 入站（`O/relay.rs:44-63`；沿用旧 provision.py `add_inbound`，**代码注释明确「这些决定了与 mihomo 客户端的兼容性，不要改」**）

```json
{"listen":"0.0.0.0","port":<port>,"protocol":"vless","tag":"inbound-<port>",
 "settings":{"clients":[{"id":"<uuid v4>","email":"node-NN","flow":"xtls-rprx-vision"}],"decryption":"none","fallbacks":[]},
 "streamSettings":{"network":"tcp","security":"reality",
   "tcpSettings":{"acceptProxyProtocol":false,"header":{"type":"none"}},
   "realitySettings":{"show":false,"xver":0,"target":"<sni>:443","serverNames":["<sni>"],
     "privateKey":"<x25519 base64url>","minClientVer":"1.8.2","maxClientVer":"","maxTimeDiff":0,"shortIds":["<16 hex>"]}},
 "sniffing":{"enabled":true,"destOverride":["http","tls","quic"],"metadataOnly":false,"routeOnly":false}}
```
- `<sni>` 默认 `www.cloudflare.com`（`O/guard.rs`、`LAB:36`）；分享链接 `fp=chrome`，`flow=xtls-rprx-vision`，`type=tcp`。
- **mihomo/OpenClash 兼容约束**（客户是用 OpenClash/mihomo 软路由的跨境电商团队，`docs/HANDOFF.md:12`）：
  - `minClientVer` **必须 ≤ `1.8.2`**（上架守卫硬拒绝更大值，`O/guard.rs:77-80`；2026-09-24 D14 把 line3:20001 与 line1 全部 13 个入口设为 `1.8.2`，修复 mihomo 被 Xray 26.7+ 拒绝，`docs/DECISIONS-autopilot.md:34`）；
  - 新入口只允许 VLESS+REALITY（守卫要求 `protocol=vless && security=reality`）；
  - flow `xtls-rprx-vision`、sniffing 含 `quic`、tcp 传输 + `header.type none`，均不得改。

### 6.3 落地 SS 出站

```json
{"tag":"ss-<ip>[-<port>]","protocol":"shadowsocks",
 "settings":{"servers":[{"address":"<落地ip>","port":<ss公网端口>,"method":"aes-256-gcm","password":"<..>"}]}}
```
- 默认方法 `aes-256-gcm`，默认落地 SS 端口 **8388**（NAT 机：供应商把某个公网端口映射到节点的 8388，`ss_public_port` 即那个端口）（`O/config.rs:20-21`，`O/intake.rs:238-244`）。节点侧用 shadowsocks-rust `ssserver`（旧为 ss-libev，同端口同密码切换）（`docs/DECISIONS-autopilot.md:33`）。
- 上架守卫对新出站：只能是 shadowsocks，不许 `proxySettings`、不许 freedom/链式代理；新规则只能 `{type,ruleTag,inboundTag:[1个新入口],outboundTag:新出站}`，不许指向 direct/已有出站（`O/guard.rs:83-123`）。

### 6.4 路由/API/统计骨架（lab 与 build.py 复现的生产形状，`LAB:39-49`，`T/ops/build.py`）

- `api`：`{"tag":"api","services":["HandlerService","LoggerService","StatsService","RoutingService"]}`；API 入站 `{"listen":"127.0.0.1","port":62789,"protocol":"dokodemo-door"|"tunnel","settings":{"address":"127.0.0.1"},"tag":"api"}`（Xray 26.x 称 `tunnel`；relayd 两者都认，按 `api.tag` 找入站）；规则 `inboundTag:["api"] → outboundTag:"api"` 放第一条。UDS：relayd 支持 API 入站 `listen` 为绝对路径时自动用 `unix:`，但生产仍是 TCP loopback。
- 其余防护规则（不受管，顺序在 `policy-*` 之前）：`ip:["geoip:private"] → blocked`；`protocol:["bittorrent"] → blocked`；之后是各入口 `inboundTag → ss-*`；最后 relayd 管的 `fleet-explicit-*`（直连主节点 → `direct`）。
- `metrics:{"listen":"127.0.0.1:11111","tag":"metrics_out"}`（lsi/lso 核对时需忽略 `metrics_out`）；`stats:{}`；`policy.levels.0.{statsUserUplink,statsUserDownlink,statsUserOnline}` 与 `policy.system.{statsInboundUplink,statsInboundDownlink}=true`、出站统计 false；`log{access:"/var/log/x-ui/xray-access.log", error:"", loglevel:"warning"}`；`routing.domainStrategy:"AsIs"`。
- Xray 版本：26.6.27 / 26.7.28（lab）；资源文件 `geoip.dat/geosite.dat` 在 xray 所在目录（`XRAY_LOCATION_ASSET`）。数据面单元 `fleet-xray.service`（旧 3x-ui 已停用）。

---

## 7. 上报协议（relayd/nodeagent → fleetd）与中控存储

### 7.1 端点与鉴权

| 端点 | 方法 | 鉴权 | 说明 |
|---|---|---|---|
| `/v2/ingest/node` | POST | `Authorization: Bearer fe1_…` | nodeagent 批次；`kind` 必须 `"node"` |
| `/v2/ingest/relay` | POST | 同 | relayd 批次；`kind` 必须 `"relay"` |
| `/v2/ingest/relay/{observed,spec,spec/ack}` | POST/GET/POST | 同（且 host ∈ relay_hosts） | §5.2 |

- Bearer 长度 20–256；**不走会话鉴权**（在管理员 guard 之外）；Caddy 路由 `/v2/ingest/*`，请求体 ≤256 KB（Caddy `request_body max_size 256KB`；ingest 库默认 `body_max=1 MiB`，axum `DefaultBodyLimit`）（`I/src/api.rs:27-34,54-58`，`I/WIRING.md:106-119`，`I/src/lib.rs:45`）。
- 令牌管理（命令行，必须以 fleetd 用户执行，root 且库属别人时拒绝，避免 root 所有的 WAL）：`fleetd agent-token issue <host_id> [--name N]`（stdout 只打印一次）、`revoke <host_id> [--name N]`（无匹配退出码 3）、`list`（含 sha256 前12位、created/revoked、last_received）。`host_id` 校验：非空、≤64 字节、不含控制字符 / `/` / 空白（`I/src/tokens.rs:15-17,73-136`）。
- 存储：`agent_tokens(host_id,name,sha256 UNIQUE,created_at,revoked_at)`，**只存 sha256**；撤销保留行（审计），`revoked_at` 非空即失效；签发/撤销用 `synchronous=FULL`。认证 = `sha256(token)` 查未撤销行 → host_id（`I/src/lib.rs:199-210`）。
- 限速（内存令牌桶）：每台主机 2 次/s、桶容量 60（补传突发）；全局鉴权失败限速 20/s、容量 40（防止拿随机令牌打库）；超限回 429 + `Retry-After`（`I/src/lib.rs:212-236`）。

### 7.2 响应码

| 码 | 条件 |
|---|---|
| 200 `{"status":"ok","samples":n,"skipped":m[,"live":true]}` | 接收；`live` = fleetd 有 SSE 连接（有人在看），边缘据此提速全链路检查 |
| 200 `{"status":"duplicate"}` | `(host,boot_id,seq)` 已收过 |
| 400 | JSON 无效 / `v≠1` / `kind` 与接口不一致 / `boot_id` 为空或>64 / `seq` 非正整数 |
| 401 | 无 / 无效 / 已撤销令牌 |
| 403 | body 的 `host_id` ≠ 令牌对应的 host_id（注意边缘把 403 当重试，见 §9） |
| 413 | `samples.len() > 360` |
| 429 | 限速 |
| 5xx / 503 | 数据库忙 |

### 7.3 接收（`accept`，`I/src/lib.rs:241-528`）

整批在**一个事务**内（每批一个 SAVEPOINT）：去重表 + 汇总 + 最新值，恰好一次：

1. 去重：`INSERT OR IGNORE ingest_seen(host,boot_id,seq,received_at)`，已存在 → 只刷新 `host_latest.received_at`，回 duplicate。
2. 每个样本：`ts` 缺失 → `skipped`；`ts < now−7d`（`max_past_s`）或 `> now+300 s`（`max_future_s`）→ `skipped`（批次仍确认）；
   - 按分钟（`ts` 向下取整到 60 s）聚合 12 个指标 `cpu steal iowait load1 mem swap disk rx tx tcp udp procs`（取值路径见下），各存 `n`（样本数）/ `s`（和）/ `x`（最大值），**百分比与 load 放大 100 倍存整数**，其余整数；`ON CONFLICT` 跨批累加；
   - `ts ≥ now−900` 的样本写 `host_recent(mem,disk,cpu,steal,iowait)`；
   - relay：`sample.xray.d[]` 的 `[kind,tag,up,down]`（非零）按分钟累加进 `xray_1m`（`kind` 截 16、`tag` 截 128）。
3. 最新值 `host_latest`：仅当本批最新样本 `ts ≥` 已存 `sample_ts` 时才替换 `sample/meta/outbox/xray/boot_id/seq/agent`（补传的旧批次不回退）；`received_at = MAX`；`batches++`、`samples+=ok`；`first_seen` 首次写入。relay 的 `xray` 去掉 `entries` 后存，并附 `conns` 与 `last_query{ok,err,latency_ms,ts}`。
4. relay：`xray.entries[]` → `xray_latest`（累计/原始/速率；`ON CONFLICT … WHERE excluded.ts >= xray_latest.ts`，`ts = 批次 created_at`，只进不退；≤20000 条）。
5. relay：`probe`（`schema=fleet.probe.v1`）→ `probe_land`（`INSERT OR IGNORE`，自然键 `(node,check,measured_at,attempt)`，≤5000 条）与 `probe_targets`（覆盖）。
6. relay：`access`（`schema=fleet.access.v1`）→ `access_raw`（`INSERT OR IGNORE`，键 `(host,id)`，≤5000 条）与 `access_state`（`dropped` 累加、`counters/source` 覆盖）。
7. relay：`legacy_probe` → 过渡期表（不要照搬）。

指标取值路径（`I/src/lib.rs:88-104`）：`cpu.pct, cpu.steal, cpu.iowait, load[0], mem.pct, swap.pct, disk.pct, net.rx_bps, net.tx_bps, sock.tcp, sock.udp, procs`。

### 7.4 中控存储与保留（`I/src/schema.sql`、`purge` `I/src/lib.rs:851-863`；每小时清理）

| 表 | 内容 | 保留 |
|---|---|---|
| `agent_tokens` | 令牌 sha256 | 永久（撤销留审计） |
| `ingest_seen` | `(host,boot,seq,received_at)` | 7 d |
| `host_latest` | 每台主机最新状态 | 永久（单行） |
| `host_recent` | 最近 15 min 原始样本（告警用） | 15 min |
| `host_1m` | 1 分钟汇总 | 90 d（体量估算：32 台×1440×90 d≈4.1 M 行≈370 MB，`I/WIRING.md:121-126`） |
| `xray_1m` | 线路机每分钟每条目增量（非零） | 35 d |
| `xray_latest` | 累计值（真实来源）/原始/速率 | 永久 |
| `probe_land`、`probe_targets` | 探测证据 / 在测节点 | 14 d / 永久 |
| `access_raw`、`access_state` | 访问事件 / 最新游标计数 | 3 d / 永久 |

- 在线判定：`now − host_latest.received_at < 120 s`（`offline_s`）。
- 库：独立 `ingest.db`（WAL，`synchronous=NORMAL`，`mmap 1 GiB`，`cache 16 MB`），单个 `Mutex<Connection>`；**组提交**：批次写在一个敞开的事务里，每 1 s `flush` 提交一次（`I/src/lib.rs:192-197`）——见 §9「已回 200 但可能丢」。

### 7.5 中控原生告警（与边缘上报的时间常数耦合）

- `native_host_offline`：120 s 无批次（线路机 critical、节点 warning），只评估有未撤销令牌的主机；fleetd 启动后 180 s 内不开（边缘退避最长 90 s）（`I/src/lib.rs:728-756`，`API:401`）。
- `native_host_resource`：内存或磁盘 ≥90% 连续 ≥120 s（按原始样本）；`native_cpu_steal`/`native_cpu_iowait`：分钟均值 ≥15% 连续 ≥10 min，最新分钟须在 3 分钟内，缺分钟视为断开。
- `relay_xray_down` 的来源（`API:401`）：`xray_process_down`（进程不在）、`xray_api_unreachable`（Stats API 连续 ≥60 s 不可达）、`relayd_silent_nodes_failing`（relayd ≥120 s 不上报且名下 ≥2 个且过半节点失败）——边缘必须提供 `xray.running`、`last_query.ok`、`last_ok`、`errors`。

### 7.6 只读查询（中控→界面，非边缘职责，列出以便对齐字段）

`GET /v2/api/v1/hosts`、`/hosts/{id}/series?metric=…&range=&step=`（`[[t,avg,max],…]`，默认 step = range/720 按 60 s 取整）、`/relays/{id}/xray?range=&kind=&tag=`（累计值、窗口增量 `window_up/down`、速率、入站 `conns`、进程与版本）、`/relays/{id}/probes?range=`（在测节点 + 每 (node,check) 最新结果，egress 取最新 `final=1`）、`/ingest/alerts`（`I/src/api.rs:36-44,144-210`）。

---

## 8. 关键时间常数速查

| 项 | 值 | 位置 |
|---|---|---|
| 主机采样 / 批 | 5 s / 2 样本=10 s（可调 2–300 s、1–60） | `E/config.rs:218-222` |
| Xray QueryStats | 每采样周期（5 s），复用 h2，超时 3 s | `E/xray.rs:201` |
| 连接数扫描 | 每采样周期 | `E/xray.rs:1022` |
| 配置重读 | 每 60 s（mtime） | `E/xray.rs:854` |
| 进程重扫（x-ui 不存在时） | 60 s | `E/xray.rs:910` |
| 上报 HTTP 超时 | connect 5 s / 总 15 s | `E/push.rs:23-24` |
| 退避 | 5 s 起、翻倍、上限 60 s、×[0.5,1.5) | `E/push.rs:76` |
| 补传 | ≤200 条或 20 s / 轮 | `E/runner.rs:272` |
| 中控离线判定 | 120 s | `I/src/lib.rs:55` |
| spec 轮询 | 10 s（`wait=0`） | `E/config.rs:137` |
| observed | 60 s 查 sha + 每小时 | `E/manage.rs:576-585` |
| 管理在线窗口 | 60 s | `F/app.rs:429` |
| 应用锁等待 / 自检锁 | 60 s / 5 s | `E/manage.rs:732,393` |
| xray api / -test / version / GetStats | 15 s / 90 s / 5 s / 3 s | `E/manage.rs:440,462`，`E/xray.rs:400` |
| restart 就绪等待 | 30 s | `E/manage.rs:931` |
| runtime 自检 | 10 min | `E/manage.rs:501` |
| L1 tcp / L2 egress / control / udp / ipv6 | 30 s / 60 s / 60 s / 5 min / 15 min，±10% | `P/scheduler.rs:40-58` |
| path 周期 / 全量 | 60 s / ≈115 s（live 时每周期） | `E/pathcheck.rs:406` |
| 线路机互探 | 30 s（10–600） | `E/config.rs:74` |
| op/命令 TTL | 180 s | `F/relayspec.rs:30` |

---

## 9. 不要照搬（bug / hack / 迁移遗留 / 文档与代码不一致）

### 9.1 协议与数据可靠性

1. **403 当可重试 + 400/413/422 静默丢批**：`host_id` 与令牌不匹配（403）永远重试，堵住队列头部直到 outbox 把它挤掉；而 400/413/422 直接丢批——此时已 `commit` 过的访问日志游标、探测游标、`xray` 增量（`d`）随之永久丢失（`E/push.rs:57-63`，`E/runner.rs:245-250`，`E/relay.rs:130-141`）。新协议应区分「永久错误→死信并上报」与「可重试」，且游标/计数前移要与对端确认耦合，而不是与「进本地 outbox」耦合。
2. **请求体上限与批大小不匹配**：Caddy 256 KB，批次含 400 条访问事件（≈120 KB）+ 200 条探测（≈70 KB）+ xray entries，可能超限 → 413 → 整批被丢（上条）。新协议要按字节切分子批 / 分类型独立上报（access、probe 各走独立端点与独立游标）。
3. **「已回 200 但可能丢」**：中控组提交每 1 s 才 COMMIT，崩溃最多丢最后 1 s 已回 200 的批次；源码注释称「边缘按 seq 去重，下次重启后照常上报」——**不成立**：边缘已 ack 不会重发（`I/src/lib.rs:192-195`）。要么先提交再回 200，要么协议里带「中控已持久化到的序号」。
4. **以增量（delta）为主的流量账**依赖「每批恰好一次」：`net.pub_*_delta`、`xray.d`、`xray_1m` 都是加法语义，只能靠 `(host,boot_id,seq)` 去重；而 `seq` 在状态丢失时用 unix 秒起算（见下）是 hack。新协议建议**上报累计计数器 + 稳定 batch_id**，由中控做差并容忍重复/乱序（累计账 `xray_latest.cum` 已经是这么做的，增量表应由它派生）。
5. **`seq` 用 `now_f64() as u64` 兜底**（`E/runner.rs:74-78`）：只在「每 10 s 才用一个序号」这个假设下不撞；改批间隔或多实例就失效。改用持久单调计数 + 随机 `instance_id`/UUID 批号。
6. **令牌不绑定 kind**：`accept` 只校验 body 的 `kind` 与路径一致，一个 node 令牌可以以 `kind=relay` 上报（host_id 必须一致，风险有限）（`I/src/lib.rs:245-250`）。令牌应绑定角色。
7. **状态与异常信息只在本机文件**（`status.json`）：中控只看到 `outbox{pending,dropped}`，`rejected`、`last_error` 不上报。新协议应把 `last_error`、`rejected` 计数、`dropped` 随批上报。
8. 访问日志「`dropped`」只计畸形行；outbox 挤掉的批次含访问事件时，中控无从得知丢了哪个游标区间（只有 `source.offset_*` 的断档可推断）。

### 9.2 身份与部署硬编码

9. **用人类可读名称当身份**：节点 `host_id`=「团队1-24」，令牌按该字符串绑定，改名即换身份（`API:399`）。新系统用稳定 ID（UUID）+ 展示名分离。
10. 线路机的 uuid / IP / 端口 / 互探对象硬编码在 `D/relays.sh:6-13`，SSH 密钥按 IP 查本机清单文件（`D/rollout.sh:47,105-108`）；endpoint 是 IP 字面量（`D/rollout.sh:52`）。应由中控发放。
11. `D/rollout.sh` 里的 TOKEN_CMD 通过 `ssh central "runuser -u fleetd -- … agent-token issue"` 取令牌：令牌在 stdout 明文流转。新流程应在注册时一次性换发并安全落盘。

### 9.3 迁移遗留（新系统直接删）

12. `E/relay.rs:9-64` `LegacyFiles`：转发 `/var/lib/komari-relay-probe/status.json`、`/var/lib/team-ops-bridge/path-status.json`（`legacy_probe` 批次键与中控 `legacy_probe` 表）；`E/probe.rs:61-78` 与 `E/pathcheck.rs:399` 读 `/etc/komari-relay-probe/config.json` 取 `tag→uuid/name/ip`；`SupplementaryGroups=komari`（`D/rollout.sh:238-245`）。
13. x-ui 相关：扫 `x-ui` 进程（`xui_proc_name`、`xray.xui`）、`xui-seed.py` 与种子机制（`E/xray.rs:718-776`）、`proc_match` 排除旧隔离 xray、`P/targets.rs:144-153` 读 `x-ui.db`。累计账的 `accum_since`/`cum` 模型保留，种子/3x-ui 部分不要。
14. `fleet-explicit-*`、`R1 fleet-default-deny` 属于旧 provision 的路由惯例；重写后若路由模型变了，守卫的「必须有只按 inboundTag 的直通路由」规则也要同步重定义（别让流量静默落进默认 blackhole）。
15. API 里仍写 `teamops-v1-` 前缀、`routed` 动作名、`rule` 字段（恒 null）：这些只为兼容旧 activity 表（`E/accesslog.rs:43-49`，`E/policy.rs:6`）。

### 9.4 relayd 管理模式的实现问题

16. **JSON → Xray 对象靠 shell 出 `xray api …` 子进程 + 临时文件**（每步写一个 `*.fleet-relayd.stepN.json`，每个调用加载 geo 数据；cgroup 峰值 28 MB）；`lsi` 不带 `--isOnlyTags`（central 上 >64 KiB），`periodic_check` 每 10 min 跑 3 次（`E/manage.rs:437-477,499`）。Xray 的 JSON→protobuf 转换只存在于其 CLI/`infra/conf`，若不自己实现 AddInbound/AddOutbound 的 TypedMessage 编码，仍要调 CLI；但应：只取 tag（`--isOnlyTags=true`）、复用子进程、不落临时文件。
17. **seccomp 禁 kill 的 hack**：`child.rs` 与 `pathcheck.rs` 用「持有线程 fork 子进程 + `PR_SET_PDEATHSIG(SIGKILL)`，线程退出即让内核杀子进程」绕过 `kill/tgkill` 被 EPERM；起因是 2026-09-24 每 10 min 一个 `xray <defunct>`（管道写满卡死 + `Child::kill()` 被拒后不 wait）（`E/child.rs:1-10`）。若重写保留该 seccomp 过滤就必须保留等价机制（读线程持续排空、回收必达）；若放宽，改成正常 `kill + wait` 并保证 reap。
18. **受管/非受管划分靠 tag 前缀 + 位置锚点**（`is_entry_tag` 的 `inbound-\d{2,5}`、`in-*`；规则锚点=「第一条 inboundTag 含入口 tag 的非受管规则」）：上架流水线的 `r-inbound-*` 规则对 relayd 不受管，通过「非受管规则原位不动」间接保护。新设计应显式标记所有权，而不是靠命名猜。端口派生也不一致：连接统计、`relay_xray.port` 只认 `inbound-<port>`，`in-*` 入口取不到端口（`E/xray.rs:960-966`，`I/src/lib.rs:648`）。
19. **运行态核对偏弱**：`spec_view`/`runtime_view` 只比入站 tag、规则的出站 tag 序列、期望出站存在；不比规则匹配条件、入站配置内容（`E/manage.rs:467-497`）。
20. **`periodic_check` 只告警不修复**；崩溃发生在「API 已生效 / 文件未写」之间时，运行态与文件分叉（靠下一次相同 spec 的 noop 分支或人工 restart 收敛）。
21. **全量声明式 + generation + base_sha + 收编/回退代**：中控与边缘各有一套复杂状态机（`rejected=(spec_sha,file_sha)` 防重试、`revert_rejected_*`/`revert_expired_*`、`adopted_external_change`）。其根因是配置文件被多方写（relayd、上架 SSH、cutover）。重写应让**单一写者**拥有 Xray 配置，其他方只能通过它的 API 变更；若仍多写者，则保留 flock + base_sha CAS。
22. **轮询 `wait=0`**：操作延迟最多 +10 s，中控却已支持 `wait≤25` 长轮询（`E/manage.rs:538`，`F/api.rs:1247`）。重写用长轮询 / 双向流。
23. **入站替换会断连接**：`rotate_uuid` 与任何客户端变化走 `rmi+adi`，该入口全部连接断开。若 Xray 版本支持按用户增删（HandlerService `AlterInbound` 的 AddUser/RemoveUser），可缩小影响 `[推断：旧代码未使用]`；`API:474` 提到「客户端增删」但 relayd 未实现。
24. **观测上报含全部明文密钥**（UUID、SS 密码、REALITY 私钥，每次变化与每小时），中控库 0600 存储（`API:467`，`E/manage.rs:587-588`）。新协议至少应加密落库、分级披露，仅在「收编」需要时才送私钥。
25. 小问题：`handle_spec` 中 `generation==0` 分支的 `done_cmds` 不截断（`E/manage.rs:638`）；`names.dedup()` 只去连续重复（`E/manage.rs:1026`）；`fold_now` 失败在 apply 里被忽略（`E/manage.rs:755`）；`prune_backups` 靠文件名字典序（unix 秒位数变化后失效）；`expired` 命令也会记入 `done_cmds`。
26. `restart_xray` 通过子进程 `systemctl restart <unit>`（单元名取配置 `manage.unit`，默认 `fleet-xray.service`）；relayd 沙箱无 capability，能重启依赖 root + D-Bus/`AF_UNIX` 可达 `[推断]`。新实现应明确 Xray 的进程管理者（systemd / 自己托管子进程）及其权限边界。

### 9.5 探测与统计的口径问题

27. **path 探测使用入口「第一个用户」的真实凭据**：探测流量计入该客户的 `user>>>email` 与入站计数（每 60 s 若干 KB/入口，`E/pathcheck.rs:68-69`）；访问日志按回环跳过，但 `/proc/net/tcp` 连接数与 `srcs` 把 `127.0.0.1` 当来源计入（`E/xray.rs:436-477`）。`P/WIRING.md:127` 的设想是专用探测入站 `pi-*`。新系统应使用专用探测身份并从客户账里剔除。
28. 出口 IP 判定**已被产品决定取消**（只保留 `egress_leak`），但文档/类型仍保留 `egress_mismatch`、`udp_egress_mismatch/unstable`（`P/types.rs:88-103` 定义了但 `egress_attempt`/`udp_check` 不再产出）。`API:312` 还保留 `mismatch` 状态。重写按「实际只产出 `egress_leak`」设计。
29. 出口来源只有 cloudflare trace 与 ipify（均为外部在线服务）；线路机到它们的控制路径失败即 `probe_target_down`。可保留此思路，但来源列表应可配置。
30. 连接数按 TCP 连接状态扫 `/proc/net/tcp*`（O(全部 socket)），UDP 完全不计。Xray 自身有 `statsUserOnline`/在线用户 API，但旧代码未用 `[推断]`。
31. 访问日志用**本地时区墙钟**经 `mktime` 转时间（`E/accesslog.rs:52-71`）；时区变化 / DST 会让事件 `ts` 错位。新实现应让 Xray 输出带时区或用文件 mtime/读入时刻校正，并把「相信本地时区」写进配置。
32. `LIVE_UNTIL` 全局可变静态（`E/lib.rs:31-32`）：「有人在看→提速全链路」靠回执里的 `live:true` 暗号。要保留这个功能，但用正式的节奏指令字段（`probe_mode`/`ttl_s`）。

### 9.6 文档与代码不一致（以代码为准，重写别被文档误导）

| 项 | 文档 | 代码 |
|---|---|---|
| nodeagent 采样/上报频率 | `E/bin/nodeagent.rs:1`、`I/WIRING.md:121-126`：10 s 采样 / 30 s 一批 | 默认 5 s / 10 s（`E/config.rs:218-222`） |
| 访问日志每批条数 | `API:494`：500 | 400（`E/config.rs:140-142`，clamp 1–2000） |
| 请求体上限 | `API`/Caddy：256 KB | ingest 库默认 1 MiB（`I/src/lib.rs:45`） |
| L2 周期 | `API:314`：约 5 min | 60 s（`P/scheduler.rs:46`） |
| `WIRING` 称 L2 返回别的 IP 立即 `egress_mismatch` | `P/WIRING.md:25` | 已取消，只记数据（`P/checks.rs:259-262`） |
| UDP 映射地址不一致 | `P/WIRING.md:27` 判 mismatch/unstable | 只当数据（`P/checks.rs:417-420`） |
| op 过期 | `API:438`：relayd 60 s 内没取走 → expired | `OP_TTL_S=180`（`F/relayspec.rs:30`） |
| 访问日志 tag | 旧 `\d{2,5}` | 新增 `in-*`（`E/accesslog.rs:198-201`） |
| 入站不保证带 `inbound-` 前缀 | | 但端口派生仍只认 `inbound-<port>`（§9.4 #18） |

---

## 10. 重写必须保留的行为清单（精简）

**遥测**：host_id + 令牌（0600，可热换）+ boot_id；5 s 采样 / 10 s 批；§1.2 的全部指标与口径（尤其 `pub_*_delta` 的账期语义、磁盘 `df` 口径、物理网卡集合）；错峰；断网磁盘队列（有界、挤最旧并计数、CRC 校验、撕裂尾截断）、严格有序补传、退避上限 < 中控离线判定；SIGTERM 落盘；自检命令。

**Xray 观测**：只读 QueryStats（reset=false）；累计账（纪元/重启/回退规则、`cum` 跨重启、管理操作前补采、重置后账本备注）；进程/版本/`core_id`；每入站 `conns` + 来源 IP；配置解析出入站→端口/协议/用户 email 与 api 地址；`xray.running / last_query / errors / last_ok` 供告警。

**探测**：tcp 30 s / L2 60 s×3 次（final 规则）/ control / udp 5 min / ipv6 15 min / path（含 REALITY 全链路、自适应节奏、live 提速）/ 线路机互探与自检 egress / `egress_leak` 标记；结果幂等键 `(node,check,measured_at,attempt)`；`value_ms` 失败不当延迟；targets 列表不含密码。

**访问日志**：Xray access log 解析规则（入口 tag、tcp/udp、回环跳过、blocked 判定含 blackhole）、轮转/截断/半行/超长行处理、游标与事件 id `inode:offset`、背压暂停不丢行、全套计数。

**管理**：flock 与上架/切换共锁；守卫（非受管不变、R1 默认出站不变、每个入口有直通路由）；`xray run -test`；**先 API 生效 → 核对 → 最后原子写文件**，逆序回滚并回执 `rolled_back_to_before`；base-sha CAS 与 `base_changed`；`restart_xray`（先测磁盘配置、等端口/API/规则就绪）；`reset_traffic`（只限入口入站与用户计数，累计不清零）；`xray_logs`（只读，上限）；命令去重与过期；回执可靠投递（持久化重试）；备份保留 30 份；`policy-*` 规则位置与 `fleet-*` 末尾规则；Xray 入站参数（§6.2）与 mihomo 兼容约束。

**摄取（中控）**：Bearer 令牌（只存 sha256、可撤销）；`(host,boot_id,seq)` 去重；per-host 限速与全局鉴权失败限速；样本时间窗（过去 7 d / 未来 300 s）；1 分钟汇总（n/s/x，×100 整数）；`xray_latest` 累计值只进不退；探测/访问事件按自然键幂等；留存期（§7.4）；离线 120 s 判定与原生告警规则。
