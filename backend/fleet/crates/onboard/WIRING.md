# 批量上架流水线（阶段 4）· 接线说明

crate：`fleet/crates/onboard`（包名 `fleet-onboard`，库名 `onboard`，二进制 `fleetctl`）。
依据：`architecture/ARCHITECTURE.md` §8（P1–P16、并发、线路机批次零重启、可恢复性、冒名机）、`research/onboarding/pipeline-spec.md`、`research/critique/pipeline.md`。
今晚的范围：**（DD →）装 SS → 接入线路机**，做成批量、可重跑、可断点续跑的标准流程。

---

## 0. 今晚在生产怎么用（中控 38.58.57.22 上执行）

前提：这台线路机已经由 fleet-cutover 切到 `fleet-xray`（`/etc/fleet/xray/config.json` 是事实源），并且切换已 COMMIT。

```sh
# 一次性准备（root）
install -d -m 700 /etc/fleet/onboard /var/lib/fleet/onboard /var/lib/fleet/dist
ssh-keygen -t ed25519 -N '' -C fleet-onboard -f /etc/fleet/onboard/id_ed25519          # 上架专用密钥（不复用旧 provision.key）
cp fleetctl /usr/local/bin/ && chmod 755 /usr/local/bin/fleetctl                     # musl 静态二进制（a395 构建，见 §6）
cp shadowsocks-v1.25.0.x86_64-unknown-linux-musl.tar.xz{,.sha256} /var/lib/fleet/dist/   # 来自 fleet-cutover/dist
cp architecture/plan-catalog/catalog.json /etc/fleet/catalog.json
install -d -m 755 /var/lib/fleet/dist/fleet-edge                                     # P11 的 nodeagent 发布目录（fleetd 用户要能读）
install -m 644 fleet-edge/dist/current/{VERSION,SHA256SUMS} /var/lib/fleet/dist/fleet-edge/ && install -m 755 fleet-edge/dist/current/nodeagent /var/lib/fleet/dist/fleet-edge/
(cd /var/lib/fleet/dist/fleet-edge && grep ' nodeagent$' SHA256SUMS | sha256sum -c)                               # onboard.toml 加 [agent]，见 §5.1
ssh-keyscan -t ed25519 38.58.57.69 23.148.204.129 > /etc/fleet/onboard/relays.known_hosts  # 核对指纹后再用
# 线路机 root 的 authorized_keys 加上 relay_ed25519.pub（或复用运维已有的线路机密钥，配置里写路径即可）
cp onboard.toml.example /etc/fleet/onboard.toml && chmod 600 /etc/fleet/onboard.toml   # 按实际改 relays / allow_ips

# 每一批
fleetctl onboard plan   batch.csv                       # 校验、放置、线路机影响预览，打印计划哈希
fleetctl onboard run    batch.csv --confirm <计划哈希>   # 金丝雀先走完全链路，其余并发（默认 5）
fleetctl onboard status batch.csv [--watch 5] [--json]
fleetctl onboard resume batch.csv                        # 崩溃/中断后继续（租约过期即可接管）
fleetctl onboard resume batch.csv --row 团队1-37 --password-file ./new.pw   # 补密码 / DD 人工闸门确认后继续
fleetctl onboard resume batch.csv --row 团队1-37 --force                   # 风险停止（identity/conflict）人工处置后重试
fleetctl onboard cancel batch.csv [--row 团队1-37]       # 对已完成且可补偿的步骤逆序补偿（线路机入口按 tag 撤下）
fleetctl onboard reveal batch.csv --row 团队1-37         # 输出分享链接（vless://…）
```

CSV（表头必需，顺序随意）：`provider,ip,ssh_port,auth,plan,team,number[,relay,ss_port,dd,mac,service_id,note]`

| 列 | 说明 |
|---|---|
| auth | `key`（上架公钥已在 root 的 authorized_keys，例如供应商开机注入）或 `password:<文件>`（相对 CSV 所在目录；文件只含 root 密码，0600） |
| plan | 套餐 id / 规范名 / 目录别名（按供应商匹配），不在目录里的行不能确认；代理产品（无主机）拒绝 |
| relay | 线路机名（= onboard.toml `[[relays]].name`）；空 = 放到名下入口最少且未满 25 的线路机 |
| ss_port | NAT 套餐必填：供应商映射到节点 8388 的公网端口（线路机转发连这个端口；身份键是 `(ip, ssh_port)`） |
| dd | `none`（默认）/ `manual`：P4 停在「等待输入：请在供应商面板重装后继续」，重装后 `resume --row` |
| mac | 可选。填了 P3 必须匹配；DD 前后用 DMI product_uuid / MAC 比对是否同一台机器 |

---

## 1. 接入 fleet workspace

现在 crate 带一个空的 `[workspace]`，可以独立构建（没动 `fleet/Cargo.toml`、`fleet/fleetd`）。并入时：

1. 删除 `crates/onboard/Cargo.toml` 里的 `[workspace]` 两行；
2. `fleet/Cargo.toml`：`members = ["fleetd", "crates/onboard"]`；
3. `fleet/fleetd/Cargo.toml`：`fleet-onboard = { path = "../crates/onboard" }`（依赖版本与 fleetd 一致：tokio 1、rusqlite 0.40 bundled、serde_json preserve_order——fleetd 已开 `preserve_order`，不冲突）。

## 2. fleetd 路由（axum 0.8）

`onboard::api::OnboardApi::handle(method, path, query, body, actor) -> (u16, serde_json::Value)` 与框架无关。fleetd 里只要一个通配路由：

```rust
// app.rs：App 增加字段
pub onboard: Option<onboard::api::OnboardApi>,
// 启动时（fleetd.toml 新增 [onboard] config = "/etc/fleet/onboard.toml"，缺省则不挂）
let onboard = cfg.onboard_config.as_ref().map(|p| {
    let c = std::sync::Arc::new(onboard::config::Config::load(p).expect("onboard.toml"));
    onboard::api::OnboardApi::new(c).expect("onboard db")
        // P11：令牌由 fleetd 自己的 fleetd-ingest 签发/撤销（见 app.rs 的 OnboardAgentTokens，约 15 行）
        .with_agent_tokens(Arc::new(OnboardAgentTokens(ingest.clone())))
});

// api.rs：router() 里加一行（在 .fallback 之前；guard 中间件照常做鉴权、CSRF、审计）
.route("/v2/api/v1/onboard/{*rest}", axum::routing::any(onboard_proxy))

async fn onboard_proxy(State(app): State<AppState>, Extension(id): Extension<Identity>,
                       method: Method, RawQuery(q): RawQuery, uri: axum::http::Uri, body: Bytes) -> Response {
    let Some(ob) = &app.onboard else { return err(404, "上架流水线未启用") };
    let (code, v) = ob.handle(method.as_str(), uri.path(), q.as_deref().unwrap_or(""), &body, &id.username).await;
    json_resp(code, &v)
}
```

接口（前缀 `/v2/api/v1/onboard`）：

| 方法 | 路径 | 作用 |
|---|---|---|
| GET | `/batches` | 批次列表（含成功/待介入计数） |
| POST | `/batches` | `{csv, title?, options?}` → plan + 建批次；同一份 CSV 幂等（201 新建 / 200 已存在 / 422 有错误，body 带 issues） |
| GET | `/batches/{id}` | 批次视图 = 原型 `demo_batch` 同形（见 §3） |
| GET | `/batches/{id}/plan` | 重新计算计划（只读） |
| POST | `/batches/{id}/start` | `{plan_hash}`，不符 409；后台执行，202 |
| POST | `/batches/{id}/resume` | 接管过期租约继续；别人在跑时 409 |
| POST | `/batches/{id}/cancel` | `{row?}`：逆序补偿 |
| POST | `/batches/{id}/rows/{name}/actions` | `{action: provide_input\|continue\|retry, password?, force?}`；密码只写不读（落 0600 文件） |
| GET | `/batches/{id}/events?after=N` | 事件：`batch.updated` `node.state` `step.started` `step.finished` `relay_batch.updated` |
| GET | `/batches/{id}/relay-batches` | 线路机批次与零重启证据（pid/启动时刻/uptime、配置 sha 前后、API 加了什么/跳过什么） |
| GET | `/assets` | 本流水线登记的资产（不含机密） |

SSE：fleetd 的 `/v2/api/stream` 现在按资源哈希推送。最简单的接法：每秒查一次 `events` 表的最大 id（`db.events(batch, after, 1)`），变了就推 `{"resource":"onboard/<batch_id>","version":<id>}`；页面收到后 `GET /batches/{id}`（或 `events?after=`）增量更新。`events.id` 单调递增，可直接当 `Last-Event-ID`。

注意：fleetd 的 guard 里写操作要 `X-Fleet-Request: 1` + 同源；`start` / `cancel` / 带 `force` 的 `actions` 建议记入 fleetd 审计（`actor` 已传进来）。

## 3. 上架流水线页（prototype/original-refined/js/pages/onboarding.js）

`GET /batches/{id}` 的返回与 `prototype/data/snapshot.json → onboarding.demo_batch` 同形，可直接喂给 `onboarding-model.js` 的 `demoBatch(store, b)`（把 `store.onboarding().batch` 换成接口数据）：

- 顶层：`id title type type_word status status_word recipe canary{enabled,passed,node} options{relay_strategy,dd_concurrency,relay_window_s,dry_run} operator confirmed_count started_at elapsed_s eta_s phases steps rows relay_groups kpis`（另有 `plan_hash finished_at demo=false`）。
- `steps` / `phases`：与原型逐项相同（P1–P16、7 个阶段）。
- `rows[]`：`name`（= 短名「团队1-37」）`provider provider_code plan plan_id price currency bandwidth relay relay_port team service_id ip mode note row_state dod cells[]`；另有 `full_name ssh_port ss_public_port nat dd current_step reason canary`，风险停止的行带 `risk{message}` `error_class`。
- `cells[]`：`{step, status, duration_s, detail?, attempt?}`，status 取原型词表 `queued running succeeded failed skipped waiting_input blocked`（补偿过的格子是 `cancelled`）。
- `row_state`：`active running queued waiting_gate in_relay_batch waiting_input blocked failed cancelled`（内部的 `ready_for_relay` 显示为 `in_relay_batch`）。
- `relay_groups[]`：`{relay, batch_no, state: collecting|merging|partial|done, members[], xray_reloads(=0), probe_restarts, impact, window_left_s?}`，另带 `xray_restarts config_sha_before config_sha_after`。
- `kpis`：`{total, done, running, attention, failed, eta_s}`（eta 暂为 null，页面按估算自己算）。

行内操作对应：提交密码 → `actions {action:"provide_input", password}` + `resume`；从这一步重试 → `actions {action:"retry"}`（风险停止加 `force:true`）+ `resume`；放弃此行 / 取消未开始的行 → `cancel {row}`；DD「重装完成，继续」→ `actions {action:"continue"}` + `resume`。**暂停批次、跳过步骤、改 IP、再等 10 分钟**今晚没有后端（按钮先置灰；改 IP = 取消该行 + 新 CSV）。新建批次对话框（`onboarding-dialog.js`）把粘贴/CSV 直接 `POST /batches {csv}`，用返回的 `rows / issues / relay_impact / plan_hash` 渲染预览，确认时 `POST start {plan_hash}`。

## 4. 与 fleet-cutover 的配合（必须遵守）

1. **只在 fleet-xray 已接管、切换已 COMMIT 的线路机上跑**。`relay.rs` 会检查：配置文件可读、Xray 进程（`run -c /etc/fleet/xray/config.json`）在跑、API 通；xray 二进制默认 `auto`（取运行中进程的可执行文件，即 `/opt/fleet/xray/<sha16>/xray`，geo 文件同目录）。
2. **不要和 cutover 的写操作（`harden`、`switch`、`rollback`、重新 `relay-switch`）同时跑**。onboard 对配置文件的“读-改-写 + 经 API 生效”整段持有与 fleet-cutover 相同的排他锁 `flock -w 60 /run/lock/fleet-xray-config.lock`（可用环境变量 `FLEET_SPEC_LOCK` 覆盖，与 fleet-relay.py 一致），并做 sha256 CAS。
3. **上架之后不要再 `relay-switch.sh --rollback` / 重新 switch**：cutover 的规格是从 x-ui.db 重新生成的，x-ui.db 里没有本流水线新加的入口，重新 switch 会让这些入口消失（分享链接失效）。如果发生了（回滚再切回、或规格被重新生成），切回 fleet-xray 之后对每个批次执行 `fleetctl onboard reattach <batch.csv>`：按库里保存的原参数（同端口、同 REALITY 密钥、同 UUID）把已上线入口再经 API 加回并落盘，分享链接不变、零重启（a395 已演练）。
4. `fleet-xui-traffic` 按 x-ui.db 里的入站 tag 记流量：新入口不在 x-ui.db 里，**旧界面看不到它们的流量**（Xray stats 里有，按 email `node-NN` 统计）。
5. 所有写入都有备份：`/etc/fleet/xray/onboard-backups/config.json.<时间>.<pid>`。

## 5. 今晚没做 / 与旧系统的缺口（如实）

- **P11 装 nodeagent（fleet-edge，Komari 已停用）**：见 §5.1。**P14 探针：跳过**（relayd 按入口自动派生探测）。新节点不会出现在旧首页 / Komari / komari-relay-probe / team-ops-path-probe（它们读 operations.db、x-ui.db）；fleetd 的主机视图（`/v2/api/v1/hosts`）在 P11 之后就能看到它。本 crate 的资产在 `onboard.db` 的 `assets` 表（规范套餐 id、团队/编号、线路机/端口、tag），fleetd 接入时并进它的资产视图。
- **DD 只有人工闸门**。供应商适配器接口在 `steps.rs::p4_dd_prepare`；ATTVPS 客户中心和 OhnoICU（VirtFusion）客户面板都没有给客户用的公开重装 API（VirtFusion 的 API 是面向面板运营方的管理员 API——这一条我没有联网核实，按已知情况处理）。bin456789/reinstall（dd + NoCloud）自动化留给下一阶段；**今晚不重装任何真实机器**。
- 机密（SS 密码、REALITY 私钥、UUID、shortId、分享链接）今晚**明文**存在 `onboard.db`（0600，root）；保险库 / sealed box 是后续项。引导密码不入库（只存文件路径）。
- 计划哈希由运维 `--confirm` 确认；**硬件密钥签名计划哈希（§8.1 L2）没做**。
- 密码尝试次数上限（每个密码值 2 次）没做：密码被拒直接停在「等待输入」，不自动重试。
- P7 没有轮换 root 密码（spec 要求存为 console_root_password）；**用过的引导密码请在供应商面板重置**（P2 输出里有提醒）。
- P1 第二视角用所属线路机的 `ssh-keyscan`；中控 = 线路机2 时不会自动换成 line1/line3。P8 反测自动选「不在该节点白名单里的另一台线路机」当视角，所以 onboard.toml 里**至少配两台线路机**；只配一台且它就是中控时反测必然失败（中控在白名单里）。
- 线路机批次失败时整批撤销本次加上的对象，再逐台单独接入把坏的那台隔离出来；端口冲突（端口被别的进程占）自动作废预留、从 P10 重新分配。

### 5.1 P11 装 nodeagent

onboard.toml 里有 `[agent]` 才装；没有这一节 P11 记「跳过」并写明原因（旧配置行为不变）。

```toml
[agent]
dist = "/var/lib/fleet/dist/fleet-edge"    # fleet-edge/dist/<版本>/ 的拷贝：nodeagent + VERSION + SHA256SUMS（sha256 不符则批次不开始）
endpoint = "https://38.58.57.22"           # 缺省值，与 rollout.sh 相同（nodeagent POST {endpoint}/v2/ingest/node）
# sample_interval_s = 5 / batch_samples = 2 / first_batch_timeout_s = 90 / unit_file = …（缺省内置单元）
# 只给 fleetctl 独立运行用（fleetd 里忽略，走进程内 fleetd-ingest）：
# token_cmd  = "/opt/fleetd/current/fleetd agent-token issue {host_id} --name onboard"   # 以 fleetd 用户执行 fleetctl 时
# revoke_cmd = "/opt/fleetd/current/fleetd agent-token revoke {host_id}"
```

- **与 `fleet-edge/deploy/rollout.sh`（节点）逐项一致**：系统用户 `fleet-edge`（无 shell）；`/opt/fleet-edge/releases/<VERSION>-<sha8>/nodeagent`（0755，按 sha256 校验后原子改名，版本目录已有同 sha 就不传）；
  `/opt/fleet-edge/current` → 该目录；`/etc/fleet-edge/agent.toml`（0644，内容与 rollout.sh 生成的逐字节相同，含首行注释，所以之后用 rollout.sh 升级不会无谓重启）；
  `/etc/fleet-edge/token`（0600 fleet-edge）；`fleet-nodeagent.service`（内置一份，与 `fleet-edge/deploy/fleet-nodeagent.service` 逐字节相同，单元测试核对）+ `10-local.conf`（cgroup v1 补 `MemoryLimit=24M`）；
  以 fleet-edge 身份 `--once` 干跑（要看到「令牌文件可读」）；切 current、daemon-reload、enable，有变化或没在跑才 restart；只留最近 3 个版本。a395 上用真 rollout.sh 装的节点与 P11 装的节点布局逐项相同（只有系统用户的 uid/gid 数值不同：P9 先建了 fleet-ss）。
- **host_id** = 短名（「团队1-37」），与 fleetd 的主机视图、rollout.sh 同一口径。
- **令牌**：fleetd 里经进程内 fleetd-ingest（`Ingest::issue_token(host, "onboard")`）；本 crate 不打开 ingest.db，只通过 fleetd 注入的 `agent::AgentTokens` 查询（`HostView::query`：未撤销令牌的 sha256、host_latest 的接收时刻与 boot_id）。
  节点上已有令牌且仍有效 → 沿用（不改节点）；否则签发新令牌（这个 host_id 名下有不在这台机器上的旧令牌时先撤销——它们是上次签发了但没写到节点的孤儿）。签发后立刻把 sha256（不是明文）记进 P11 的输出；明文只在内存里、经 ssh stdin 写进 0600 文件，不落库、不进日志。
  **风险停止**：这个 host_id 有有效令牌且 5 分钟内收到过另一台机器（boot_id 不同）的上报 → `conflict`（编号可能被在用节点占着）；节点上 agent.toml 已登记为别的 host_id、或装着 fleet-relayd（线路机）→ `conflict`。
  fleetctl 独立运行时看不到 ingest：`token_cmd`/`revoke_cmd` 签发/撤销，只沿用「上次由本流水线签发的那个」（按 sha256 认），两者都没配则 P11 跳过并说明。
- **等第一批**：fleetd 里以 host_latest 为准——接收时刻晚于本次 P11 开始、boot_id 与节点一致；看不到 ingest（或 boot_id 因补传旧样本没更新）时退回节点 `/var/lib/fleet-nodeagent/status.json` 的 `last_ok ≥ 启动时刻`（rollout.sh 的判据）。上限 `first_batch_timeout_s`（90 s），超时带 status.json 与最近日志，按 transient 自动重试（幂等）。
- **预检**（只读，失败不动节点）：x86_64、systemd ≥ 239 且在运行、有 runuser、到 endpoint 的出站通（不通 = provider，不自动重试）。
- **补偿**（取消 / 放弃一行时；P11 只要执行过，失败的也补偿）：停用并删除单元、drop-in、状态目录、`/opt/fleet-edge`、`/etc/fleet-edge`、`fleet-edge` 用户（rollout.sh --remove 的节点部分；装着 relayd 的机器不删共用部分），然后撤销这个 host_id 的全部令牌（撤销 = fleetd 不再对它告警）。已上线的行不走补偿（走下线流程：`rollout.sh --remove … --revoke`）。

## 6. 构建与测试

```sh
fleet/crates/onboard/test/sync.sh            # 同步到 a395:~/team-ops-eval/onboard/src 并 cargo build --release（musl 静态，~3.6 MB）
# a395：cargo test --release                 # 单元测试（含 agent：配置/drop-in 与 rollout.sh 逐字节一致、内置单元与 fleet-edge 的一致、令牌决策表、SHA256SUMS 校验、脚本 bash -n、HostView 对 fleetd-ingest 真实库结构）；原有 12 个：diff 守卫 8 个（改已有入口/规则指向 direct/freedom 转发/balancerTag/端口冲突/静态段/精确删除）、CSV、URL 解析
# a395：cd ~/team-ops-eval/onboard && ./e2e.sh   # 整套演练（见下）
```

`test/lab.sh`（只用自己的 `onb-*` 容器和 `onb-net`，不碰 Leap dev 栈）：中控容器（fleetctl + xray 客户端）、线路机 L（Xray **26.7.28**，与线路机2/3 相同）和 M（Xray **26.6.27**，与线路机1 相同），配置按生产形状生成（api 127.0.0.1:62789、默认转发 direct、geoip:private/bittorrent → blocked、已有入口）；旧节点（已挂在 L:20002 上的现有入口）；回显服务（返回来源 IP，cloudflare trace 格式）；5 台新机（sshd + nft，无 systemd；n3 模拟 NAT 端口映射；n5 固定 MAC 用于 DD 闸门）。另有 systemd 节点镜像（`onb-sd:1`）单独验证了 systemd 路径（fleet-ssserver 单元、nftables.service enable、`systemctl reload ssh`）。

`test/p11-e2e.sh`（P11 专用，`test/p11-sync.sh` 同步到 a395:~/team-ops-eval/fleet-build-p11 后在那里跑；`p11-*` 容器、`p11-net` 11.98.0.0/24，systemd 节点镜像 onb-sd:1）：真 fleetd（自己的端口与 data_dir，ingest + 内嵌流水线，dev-allow-local）驱动 P1–P16——
P11 装 nodeagent、首批被 fleetd-ingest 接收、agent_tokens 有 onboard 令牌、`/v2/api/v1/hosts` 在线；用真 rollout.sh 给另一台装一遍逐项对比布局；P11 做完未记账时崩溃 → resume（沿用令牌、不重传、不重启、令牌仍 1 个）；
到中控出站断开 → P11 失败（未签发）→ 重试成功 → 取消该行 → 补偿（服务/文件/用户删除、令牌撤销）；fleetctl 独立运行（命令模式）装一台并重跑无变化。节点到中控用演练转发（nodeagent 只允许 http://127.0.0.1 明文，生产是 https://38.58.57.22）。

`test/e2e.sh` 覆盖：金丝雀先行 → 线路机批次 API 已加、落盘前**崩溃** → resume 接管（运行态已有、文件没有 → 跳过 API 只补落盘）→ n4 **注入故障**（错密码 → 等待输入，不影响其他行）→ 补密码后上线 → n5 **DD 人工闸门** → 模拟面板重装（新主机密钥、新 root 密码、同 MAC）→ resume → 重新钉公钥、MAC 一致、上线 → **重跑同一份 CSV 无变更** → `proof.sh diff`（Xray PID/启动时刻/重启次数不变、已有入口配置与运行态逐项不变、分享链接不变、配置文本 0 行删除）→ 接入全程挂着的**长连接不断** → 每个新入口 **客户端 → 线路机入口 → 节点 → 出口**（回显出口 = 节点 IP；公网 cloudflare trace 可达）。
