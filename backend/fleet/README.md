# fleet/ — fleetd（中控控制面，当前 0.14.0，原生模式）

团队线路管理新架构的中控进程：一个 Rust 静态二进制 `fleetd`，挂在 Caddy 的 `/v2/api/*`、`/v2/ingest/*` 下。
旧 team-ops 后端与 Komari 已下线：旧形状接口由原生旧库（`legacy.db`）渲染，主机 / 线路机数据来自边缘上报（relayd、nodeagent）。
接口契约见 [API.md](API.md)。架构依据：`architecture/ARCHITECTURE.md` v1.2 §9.1、§3.11、§3.8、§4、§6.5–6.7、§10.2。
下表前三行是 1a 时期的只读适配（`legacy_source = "team-ops"` 时才用；现状见文末「现状」）。

## 做什么

| 模块 | 文件 | 作用 |
|---|---|---|
| legacy-source | `src/legacy.rs` `src/cache.rs` `src/app.rs` | 旧后端 127.0.0.1:25800 的**只读**适配：唯一出口写死 GET，路径白名单 = 旧插件 allowedGet；带 `X-Ops-Key`（文件）与 `X-Ops-Actor: fleetd`。一个共享轮询器服务所有浏览器，节奏自适应（有 SSE 客户端：overview/health 10 s、panel 15 s、history 等 60 s、renewals/jobs/audit 300 s；没有：overview+health 60 s，其余按需）。按需请求 TTL 缓存 + 合并 + 到旧后端并发 ≤ 2。`/v2/api/legacy/<path>` 与旧 `/api/team-ops/<path>` 逐字节相同。 |
| Komari | `src/komari.rs` | 不走旧后端的 `/komari/*`；直接 `POST /api/rpc2`，只允许 7 个只读方法，结果原样拼回 `{"data":…}`（`komari/clients` 去掉 token）。鉴权用 Komari 系统 API 密钥（可选）或转发请求者会话。 |
| 鉴权 | `src/auth.rs` `src/accounts.rs` | 每个 `/v2/api/*`：fleetd 自有账户会话（`auth.mode = "fleetd"`，生产值）；`komari` = 下线前的过渡（另接受 Komari 管理员会话）。写操作另要 `X-Fleet-Request: 1` + `Sec-Fetch-Site` 同源。`dev-allow-local` 只给测试服（listen 非回环拒绝启动；带 X-Forwarded-For 的请求不放行）。 |
| SSE | `src/sse.rs` `src/api.rs` | `/v2/api/stream`：资源内容哈希变了就推 `{"resource","version","ts","path"}`；15 s 心跳；`Last-Event-ID` 补发（环 512 帧，epoch 防重启错位）；会话失效推 `auth` 并断开。 |
| 告警引擎 v1 | `src/alerts.rs` `src/rules.rs` | 纯状态机（假时钟可测）：pending→firing→resolved + ack；group_by [relay,cause]；group_wait 10/30 s、group_interval 5 min、repeat 30 min（critical 直到确认）/ 4 h；info 进 09:00（北京）摘要；relay_down / collector_stale 抑制；关联故障聚合；静默与维护窗口（≤ 7 d，带时区）。16 条规则全部从旧 `/overview` + `/health` 推导。 |
| 通知 | `src/notify.rs` | Telegram（带“打开”按钮）、企业微信群机器人、SMTP（lettre + rustls）、签名 Webhook（`X-Fleet-Signature: sha256=HMAC(secret, "<ts>.<body>")`）。**没配渠道 → 影子模式**：写 `notify_log`（status = shadow）。连续失败 3 次 → `channel_failing`。healthchecks.io 死人开关（默认关）。 |
| 套餐与资产 | `src/catalog.rs` | 启动时幂等导入 `catalog.json`（6 套餐、41 资产、3 台线路机）；`/v1/plans`、`/v1/assets`、`/v1/quota`。 |
| 探测历史 | `src/probes.rs` | 每次轮询把各节点最新的 TCP / 认证出口 / 入口到出口结果记下（按 measured_at 去重），24 h 原始样本 + 5 分钟桶；失败/超时/出口不一致只记状态、`ms` 为空。`/v1/probes/latest`（每类最近 20 个）、`/v1/probes?node=&range=`。 |
| 存储 | `src/store.rs` | SQLite（bundled），WAL，`synchronous=FULL`，单写线程；审计只追加 + 哈希链；保留：已恢复告警 90 d、notify_log 90 d、审计 365 d。 |

## 构建（a395）

```sh
fleet/sync.sh                                  # 工作区 → a395:~/team-ops-eval/fleet-build/src
ssh -i ~/.ssh/a395_kismet kismet@10.172.17.213
source ~/team-ops-eval/tools/env.sh
cd ~/team-ops-eval/fleet-build/src && CARGO_TARGET_DIR=~/team-ops-eval/fleet-build/target cargo build --release
# → target/x86_64-unknown-linux-musl/release/fleetd（static-pie，约 8 MB）
```

`fleet/.cargo/config.toml` 固定目标为 `x86_64-unknown-linux-musl`，并让 bundled SQLite 用系统 gcc 编（`-DSQLITE_DISABLE_LFS`，musl 1.2.4+ 没有 `*64` 符号）。

## 测试

```sh
FLEETD_FIXTURES=~/team-ops-eval/fleet-build/private/fixtures cargo test --release --no-fail-fast
```

- 单元测试（`--lib`，0.14.0 时 93 个）：告警状态机（分组、抑制、重复、确认、静默、维护、摘要时刻、关联聚合、作用域）、规则新鲜度按渲染时刻、启动宽限（重启时已有告警不恢复、不新开）、续费日历天数（账期时区）、配额档位、会话缓存、GET/白名单、Komari 只读方法、SSE 版本与补发（含预登记资源）、增量快照（文本存储逐位还原）、渲染缓存上限、24 h 窗口（增量同步与整窗重载逐字节相同）、审计哈希链、保留期、影子模式、渠道失败。
- `tests/replay.rs`（14 个，含重启宽限）：用 a395 抓的真实 `/overview` + `/health` 做底，重放 status_replays 的 5 个真实时刻，并注入 relay_down、关联故障、出口 IP（不再比对）、维护窗口、采集器停滞、续费 7/3/1、配额 80/85/100%。
- `tests/http.rs`（端到端）：假旧后端 + 假 Komari（回放真实响应）+ 真 fleetd HTTP：107 条旧路径逐字节相同、只发 GET 且带 key/actor、合并、并发 ≤ 2、401/403/405、SSE、静默 + 审计、套餐/资产/配额。
- 夹具（`deploy/v2/live/fixtures`，真实 IP）只放本机与 a395 的 `fleet-build/private/`（0700），不进仓库。

## 测试服（a395）

```sh
fleet/staging/stage.sh start [keepalive|pinned]   # 隔离命名空间：旧后端副本 :18397 + Komari 替身 + fleetd :18396
fleet/staging/stage.sh status | logs | stop
fleet/staging/stage.sh inject '{"op":"node_fail","node":"t1-31"}'   # node_fail / egress_mismatch / relay_down / quota / clear
fleet/staging/stage.sh api /v2/api/v1/alerts
# a395 上：python3.12 verify_stage.py equal|sse|auth|scenario；bash measure_all.sh（基线 / 空闲 / 活跃 CPU 与 RSS）
```

`fleet_stage.py` 复用 `deploy/v2/staging/stage_server.py` 的 `load_backend`（拷贝到 `~/team-ops-eval/fleet-stage/vendor/`，不改原文件）。

## 生产部署（main 执行）

文件：`fleet/dist/fleetd`（musl 静态二进制，sha256 见 `fleet/dist/fleetd.sha256`）、`architecture/plan-catalog/catalog.json`、`fleet/deploy/{install-central.sh,caddy-route.sh,rollback.sh,fleetd.service,fleetd.toml.example}`。

**升级（已装过 fleetd，保留配置与数据库）**：

```sh
# 本机（在 ~/Desktop/team-ops-v2 下）
scp -r fleet/deploy fleet/dist/fleetd root@38.58.57.22:/root/fleetd-deploy/
# 中控（root）
cd /root/fleetd-deploy/deploy && bash upgrade-central.sh ../fleetd   # 自检 → 更新 unit → 缺库就先 fetch-dbs → 切换 → 重启 → 核对，失败自动回退
bash caddy-route.sh                                                 # 补 /v2/ingest/* 路由（/v2/api/* 已有则不动）
/opt/fleetd/current/fleetd diag komari [--session-file F]           # 排查 Komari（Origin/CORS、会话转发）；diag legacy 查旧后端
```

**首次安装**：

```sh
# 本机（在 ~/Desktop/team-ops-v2 下）
ssh root@38.58.57.22 'mkdir -p /root/fleetd-deploy'
scp -r fleet/deploy fleet/dist/fleetd architecture/plan-catalog/catalog.json root@38.58.57.22:/root/fleetd-deploy/
# 中控（root）
cd /root/fleetd-deploy/deploy && bash install-central.sh ../fleetd ../catalog.json   # 装二进制 + 配置 + key + unit，启动并核对 /v2/api/v1/system
bash caddy-route.sh                                                               # 加 /v2/api/* 路由（备份、validate、reload、核对）
# 回退
bash rollback.sh               # 删路由 + disable --now fleetd（旧系统零改动）
bash rollback.sh --previous    # 切回上一版二进制
```

## 旧系统下线（原生旧库 + 闸门，API.md §12）

```sh
# 中控（root）：导入旧库（只读旧库，约 1 分钟）；fleetd 随即从导入时刻原生喂数，并每 60 s 比较一轮
/opt/fleetd/current/fleetd legacy snapshot
curl -s 127.0.0.1:25900/v2/api/v1/shadow/legacy | python3 -m json.tool | less   # .data.paths.gate.passing / clean_for_s
# 闸门通过后（连续 30 分钟 0 个未解释差异）：
bash /root/fleetd-deploy/deploy/decommission.sh          # 停旧后端 → catchup → legacy_source=native → Caddy
bash /root/fleetd-deploy/deploy/decommission.sh --rollback
# a395：读路径逐字节核对（staging/golden.py 生成基准）
fleetd legacy verify --golden ~/team-ops-eval/fleet-stage/golden
```

## 现状（0.14.0，原生模式）

旧后端与 Komari 已下线，fleetd 自己喂数、渲染旧形状接口（API.md §12.4）；配置见 `deploy/fleetd.toml.example`（与线上同形态）。

内存（0.14.0，中控实测见 `/v2/api/v1/system` 的 `rss_anon_mb`，构成看本机 `curl -s 127.0.0.1:25900/v2/api/v1/debug/mem`）：
24 h 访问窗口只存行（按时间有序）与每组计数 / 最大时间，不再每组一棵时间多重集合；增量快照的旧版本存紧凑 JSON 文本；
渲染缓存有条目 / 字节上限；7 / 14 天小时汇总 10 分钟没人看就释放。中控是 cgroup v1：unit 里只有 `MemoryMax` 生效。

启动宽限（`[alerts] startup_grace_s`，API.md §4）：重启后的边缘上报空窗不再触发主机离线 / 采集器停滞之类的影子告警。

测试服用线上数据副本验收（a395，`staging/prodcopy.sh`）：

```sh
fleet/staging/prodcopy.sh fetch                    # 中控 4 个库的一致性备份 → a395 ~/team-ops-eval/fleet-memfix/prodcopy（0700；中控不留副本）
fleet/staging/prodcopy.sh start [BIN]              # 副本的全新拷贝 + 生产同形态配置（native、komari 关、ipintel 开），127.0.0.1:18376
fleet/staging/prodcopy.sh load 900                 # 模拟开着界面的浏览器（SSE + 生产里出现过的读路径），每 30 s 打印内存
fleet/staging/prodcopy.sh mem 'collect=1'          # 内存构成；profile 构建另可 drop=rcache,snapshots,window,rollup,memo,conns
fleet/staging/prodcopy.sh alerts | logs | stop
```

常用核对：

```sh
fleetd legacy verify --golden DIR                     # 旧读路径逐字节核对（a395）
fleetd legacy check-window --db F --now T             # 24 h 增量窗口 vs SQL
fleetd legacy check-rollup --db F --now T             # 7 / 14 天小时汇总 vs SQL（在库的临时副本上回填）
fleetd legacy collapse-domains --since T [--dry-run]  # 随机子域名新域名事件按主域名合并（中控，fleetd 用户）
fleetd vendor import FILE [--dry-run] | vendor meters # 供应商读数 → 流量计基线
```

