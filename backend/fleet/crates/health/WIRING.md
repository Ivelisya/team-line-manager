# fleet-health 接入说明（fleetd）

综合健康判定只在中控算（ARCHITECTURE §3.6 的“健康判定分工”）。输入是边缘上报的逐次探测结果（`fleet-probe` 的 `fleet.probe.v1`）、nodeagent 主机遥测、线路机遥测新鲜度和入口观测，输出与旧 `monitor.py` 同一套状态、原因码、since 和可用率。

## 1. 结构

| 模块 | 内容 |
|---|---|
| `legacy` | 旧 `monitor.py` 的逐行移植：`diagnose`、`Monitor::transition`（THRESHOLDS、HL-04 pending、HL-05 六周期内翻转 ≥ 3 次记 unstable、`fresh_after(i)=max(120,3i)`）、`Monitor::evaluate`（决定性检查、since、维护冻结、每分钟样本）、`availability_windows`。输入就是旧的资产 JSON |
| `engine` | 新架构入口 `Engine`：把新输入拼成旧资产形状交给 `legacy`，再叠加 v1.2 新增的规则（见 §4） |

依赖只有 serde、serde_json 和 `fleet-probe`（`default-features = false`，只有类型）。

## 2. fleetd 接入步骤

1. **依赖**：`fleet-health = { path = "../crates/health" }`（fleet/fleetd/Cargo.toml）。`health/Cargo.toml` 和 `probe/Cargo.toml` 末尾各有一个空的 `[workspace]`，开发期各自独立构建；作为 path 依赖直接可用（已在 a395 验证）。要收进 `fleet/Cargo.toml` 的 members 时删掉这两段。

2. **收探测结果**：fleetd-ingest 的 `accept()`（在 fleet-edge/fleetd-ingest，归 fleet-edge 那边维护）在 `kind == "relay"` 时读批次顶层的 `"probe"`（`fleet_probe::ProbeBatch`），按自然键 `(node, check, measured_at, attempt)` 写入落地表 `probe_land`，和批次去重在同一个事务里。建议的表：
   ```sql
   CREATE TABLE probe_land(land_id INTEGER PRIMARY KEY AUTOINCREMENT, relay TEXT NOT NULL, node TEXT NOT NULL,
     check_kind TEXT NOT NULL, measured_at REAL NOT NULL, attempt INTEGER NOT NULL DEFAULT 0, status TEXT NOT NULL,
     doc TEXT NOT NULL, UNIQUE(node, check_kind, measured_at, attempt));
   ```
   `_relay` 控制路径结果按线路机归组（`Inputs.control[relay]`）。

3. **组输入**（每次评估，例如每 10 s，或每批入库后）：
   ```rust
   let mut inp = fleet_health::Inputs::default();
   inp.nodes = /* 意图：NodeSpec{uid, name, team, relay, provider, ip, relay_port, outbound_tag, target, target_port,
                  protocol, host_required, policy, profile, l3} */;
   for r in new_probe_results { inp.probes.entry(r.node.clone()).or_default().apply(&r); } // 或常驻内存增量更新
   inp.hosts  = /* host_latest：received_at、online、cpu、ram/ram_total、disk/disk_total */;
   inp.relays = /* 线路机名 → RelayObs{fresh: now - received_at <= 90, received_at, ts} */;
   inp.entries = /* 入口观测：enable、outbound_tag、target、target_port、protocol */;
   inp.control = /* 线路机名 → 最近的 control 结果 */;
   let out = engine.evaluate(&inp, now);
   ```
   `NodeProbes` 最好常驻内存（每节点每类只留最新一条），不必每次从库里查。

4. **用输出**：
   - `out.evaluation.nodes[*]`：`status`（ok/degraded/fail/unknown/maintenance）、`reason`、`since`、`availability`（1/0/null）、`checks[*]`（每项的 status/reason/since/pending/evidence），直接对应旧 `/health` 的 `monitor.nodes`；
   - `out.evaluation.flips`：检查级与节点级的状态翻转（写审计 `health_flip`）；
   - `out.evaluation.incident_open`：应开启或保持故障的 (节点, 检查)：critical 且 fail，或 cpu/memory/disk degraded ≥ 120 s；**某个检查回到 ok 就关闭它的故障**（旧 `transition` 的 incidents 部分由告警引擎实现）；
   - `out.evaluation.attention_degraded`：degraded 持续 ≥ 300 s 的节点（旧“需要关注”的一部分，另一部分是有未关闭故障的节点）；
   - `out.correlations`：`correlated_node_failure` 聚合（按线路机、按供应商各一条），告警引擎发一条 critical，不抑制也不被抑制；
   - `engine.windows(uid, now)`：1/7/30 d 可用率窗口，字段与旧 `availability_windows` 相同。

5. **持久化**：`Engine` 可序列化（`serde_json::to_vec(&engine)`）。各检查的状态机（`monitor.states`）、节点状态（`monitor.status`）、上一次报告（维护期要用）在每次评估后落库；分钟桶和小时桶（`monitor.minutes`/`hours`）可以改写成 tsdb 的 `health_min` 表，这时只需把 `Monitor::evaluate_nodes` 里写桶的那几行换成写表。每天调用一次 `monitor.prune(now)`（分钟 14 d、小时 400 d）。

6. **影子运行**：阶段 2 与旧系统并行时，可以直接把旧 `/api/team-ops/overview` 的 `assets`、`relays` 喂给 `legacy::Monitor::evaluate(&assets, &relays, now)`，结果与旧 monitor 相同（§3），用来核对 fleetd 的接线本身有没有问题；再用 `Engine` 跑新探测数据，比较两者差异即为 §4 列出的有意差异加上探测本身的差别。

## 3. 与旧系统一致性的验证

- **生产快照**（`tests/legacy_fixture.rs`）：旧 `/overview` 快照喂给移植版，与旧 `/health` 在同一时刻的输出比较：41 个节点、433 项检查，状态、原因、可用性、critical、pending **全部一致**。快照当时全部为 ok，所以还需要下一项。
- **真实 monitor.py 的逐步 golden**（`tests/legacy_golden.rs`，生成器 `tools/gen_golden.py`）：在 a395 用 Python 3.12 直接 import 生产版 `teamops/monitor.py`，在 5 个节点（有主机探针的 ATT、无主机的 NM、线路机自身、线路机3 名下两个）上跑 253 步：脚本化场景覆盖 HL-04 三次尝试、`fail_after=2`、一次成功即恢复、出口不符、TCP 断导致 ingress 记 upstream_down、策略拦截记 degraded(policy_blocked)、翻转记 unstable、出口慢（latency）、线路机失联、主机离线（2,2 阈值）、主机数据过期、CPU 高、入口停用、绑定不符、维护窗口、数据老化；再加 160 步固定种子的随机扰动。golden 里出现过的 (状态, 原因) 组合有 20 种。Rust 重放后比较每步每节点的 status/reason/since/availability 和每项检查的 status/reason/since/pending，最后比较 1/7/30 d 窗口：**1265 次节点判定、12397 次检查判定、15 个窗口，差异 0**。
- **新输入**（`tests/engine.rs`）：全 ok；HL-04（前两次 pending，第一个最终失败到 degraded，第二个到 fail，一次成功恢复）；probe_target_down；有无控制路径佐证时的 correlated_node_failure；线路机失联；UDP 只是信息性检查。

重新生成 golden：
```bash
fleet/crates/sync.sh
ssh a395 'source ~/team-ops-eval/tools/env.sh; cd ~/team-ops-eval/fleet-probe-build/src/health && \
  python3 tools/gen_golden.py <含 teamops/ 的 release 目录> tests/fixtures/legacy-golden.json'
```
（本机的 Python 3.9 解析不了 9 位小数秒的 ISO 时间，旧代码需要 ≥ 3.11。）

## 4. 有意的差异（只在新输入出现时触发；旧输入下结果不变）

| # | 新行为 | 旧行为 | 原因 |
|---|---|---|---|
| 1 | 边缘报告两个公共来源的控制路径都失败时，egress 记 `unknown(probe_target_down)`，不进状态机，不计入可用率 | 记 fail(timeout)，所有节点一起变红 | 架构 §3.6 L2 判定规则、F22 |
| 2 | 同一线路机（或同一供应商）名下 ≥ 3 个且 ≥ 80% 的节点 egress 为 fail，并且这些节点所属线路机的控制路径 ok：状态仍是 fail（计入可用率），原因码改为 `correlated_node_failure`，另输出一条聚合 | 每个节点各报各的原因 | 架构 §3.6、§3.11（critical，不抑制） |
| 3 | 没有部署 L3（`NodeSpec.l3 = false`）时 `ingress` 记 na | 没有 path 数据时记 unknown，整个节点变 unknown | 阶段 2 还没有 Xray 探测入站；L3 上线后把 `l3` 置 true，并把结果填进 `NodeProbes.path` |
| 4 | egress 新鲜度按 60 s 周期算，`fresh_after(60) = 180 s` | 按 300 s 周期算，900 s | 新 L2 周期是 60 s；`EngineConfig.params` 可调，设成 300 就回到旧值 |
| 5 | UDP、IPv6 作为信息性检查附加（`udp`、`ipv6`），和 tcp、latency 一样不参与节点状态 | 没有 | 这两项只出告警（`udp_egress_*`、`ipv6_egress_present`） |
| 6 | 非最终的失败尝试（`final = false`）按旧 pending 处理；最终失败映射为 `egress_attempt = 3` | attempt < 3 视为 pending | 尝试次数可配置时也不会误判 |

不移植的部分：incidents 表、审计写入、`/health` 的 `days` 日历（属于 fleetd 的告警引擎和 API 层，由它们用上面的输出实现）。`egress_internet_or_dns_fail` 需要 `/_whoami` 对照来源，等 relayd 的回显端口上线后在 `engine` 里加一条规则即可。线路机失联时节点原因码仍是旧的 `stale`；架构要求实时界面显示 `unknown(probe_offline)`，这属于展示层映射，这里没有改原因码，这样才能和旧系统逐字相等。

## 5. 构建与测试

```bash
fleet/crates/sync.sh
ssh a395 'source ~/team-ops-eval/tools/env.sh; cd ~/team-ops-eval/fleet-probe-build/src && CARGO_TARGET_DIR=../target cargo test --manifest-path health/Cargo.toml'
```
