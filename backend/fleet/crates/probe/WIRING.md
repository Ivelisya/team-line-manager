# fleet-probe 接入说明（relayd）

阶段 2 的边缘探测：线路机在本机探测名下每个落地节点，只产出**逐次尝试的原始证据**，综合状态由中控的 `fleet-health` 计算（ARCHITECTURE §3.6 的“健康判定分工”）。

## 1. 这个 crate 提供什么

| 模块 | 内容 |
|---|---|
| `types`（无运行时依赖） | `ProbeResult`、`ProbeBatch`（推送契约 `fleet.probe.v1`）、`CheckKind`、`Status`、原因码常量 `reason::*`、`legacy_rc()` |
| `ss` | 最小 Shadowsocks 客户端：TCP 支持 aes-128/256-gcm、chacha20-ietf-poly1305（SIP004）和 2022-blake3-aes-128/256-gcm（SIP022）；UDP 支持 SIP004 单包往返 |
| `http` | 经任意字节管道（直连 TCP 或 SS 隧道）做一次 HTTPS GET；rustls + ring，根证书只加载一次，全局复用 `ClientConfig`（带会话恢复） |
| `checks` | `tcp_check`（L1）、`egress_attempt`（L2 一次尝试）、`control_check`（控制路径）、`udp_check`（STUN）、`ipv6_check` |
| `scheduler` | `Prober`：每节点独立循环，±10% 抖动，按节点哈希错峰；L2、控制路径、IPv6 共用并发上限 4；L2 每周期最多 3 次尝试、间隔 30 s，只有最后一次标 `final`（HL-04） |
| `targets` | 从 Xray JSON、3x-ui 数据库（`xrayTemplateConfig`，只读）、旧 komari-relay-probe 的 config.json（只取 uuid/name/ip/outbound_tag）读出 `NodeTarget` |
| `store` | 边缘 SQLite：`probe_raw`（每次尝试，保留 24 h）+ `probe_kv`（推送游标） |
| bin `fleet-probe` | 独立入口：`once`（跑一轮）、`run`（常驻 + CPU 统计）、`batch`（打印待推送批次）。验收和现场排障用，生产不装 |

特性：`default = ["runtime"]`。fleetd / health 用 `default-features = false`，只拿到类型和 JSON 契约，不引入 tokio、rustls、SQLite。

## 2. 检查项与节奏

| 检查 | 间隔 | 超时 | 判定 |
|---|---|---|---|
| `tcp`（L1） | 30 s ±10% | 5 s（与旧 probe.py 相同） | connect 成功记 ok，值为整毫秒且 ≥ 1；失败或超时一律为 `upstream_down` |
| `egress`（L2） | 60 s ±10%，从周期开始算 | 每个来源 8 s | 经 SS **按域名**依次访问 ① `https://www.cloudflare.com/cdn-cgi/trace` ② `https://api.ipify.org`（顺序与旧系统一致）。任一来源返回期望 IP 即 ok；返回了别的 IP 立即记 `egress_mismatch`；都失败时取最后一个错误 |
| `control` | 60 s ±10% | 8 s | 线路机不经 SS 直接访问同样两个来源。`node = "_relay"` |
| `udp` | 5 min（只对 `NodeTarget.udp = true` 的节点） | 每包 2 s | 经 SS UDP relay 向 `stun.cloudflare.com:3478`、`stun.l.google.com:19302` 各发最多 2 次 Binding。映射地址 ≠ 期望：`udp_egress_mismatch`；来源之间不一致：`udp_egress_unstable`；都没应答：`unknown(udp_no_reply)`（节点可能没开 UDP） |
| `ipv6` | 15 min | 8 s | 经 SS 访问只有 AAAA 的 `api6.ipify.org`。连通记 `fail(ipv6_egress_present)`；握手阶段被断开或超时记 `ok(ipv6_absent)`；连不上落地记 `unknown` |

L2 失败原因码（与旧 `monitor.py` 的 REASONS 同一口径，`legacy_rc` 字段给出对应的 curl 退出码）：

| 情况 | reason_code | legacy_rc |
|---|---|---|
| 连不上落地 SS 端口 | `upstream_down` | 7 |
| 整体超时（含密码错误：ssserver 对认证失败的连接不断开，只是不回数据，旧系统 curl 同样得到 28） | `timeout` | 28 |
| 隧道建好后 TLS 握手被断开 | `auth` | 35 |
| 出口 IP 不符 | `egress_mismatch` | -1 |
| 证书不可信（疑似劫持） | `tls_invalid` | -1 |
| HTTP 非 200 / 响应解析不出 IP | `http_error` / `bad_response` | -1 |
| 两个来源的控制路径都失败 | `probe_target_down`（status = unknown，本周期立即结束） | -1 |
| 加密方式不支持 | `unsupported_cipher`（status = unknown） | -1 |

控制路径的用法：L2 每次尝试前取在有效期（150 s）内的控制路径结果；某个来源的控制路径失败，本次只用另一个来源；两个都失败，失败的尝试记 `unknown(probe_target_down)`。`upstream_down` 和 `egress_mismatch` 不受控制路径影响（那是节点自己的问题）。

## 3. 推送契约 `fleet.probe.v1`

挂在 fleet-edge 遥测批次（`{"v":1,"kind":"relay","host_id",…,"samples":[…]}`）的**顶层 `"probe"` 键**上，没有新数据时不带这个键：

```json
"probe": {
  "schema": "fleet.probe.v1",
  "relay": "line3",
  "generated_at": 1790189486.03,
  "first_id": 1041, "last_id": 1096,
  "results": [
    {"id":1041,"node":"_relay","check":"control","status":"ok","value_ms":14.0,"actual_ip":"38.58.57.69",
     "reason_code":null,"measured_at":1790188915.74,"source":"cloudflare",
     "phases":{"connect_ms":2.9,"tls_ms":8.2,"first_byte_ms":14.1},"legacy_rc":0},
    {"id":1042,"node":"5e55148f-14bd-479f-bca4-83d938423b54","check":"tcp","status":"ok","value_ms":5.0,
     "actual_ip":null,"reason_code":null,"measured_at":1790188916.23,"legacy_rc":0},
    {"id":1043,"node":"6ffbedb9-629c-4b0b-a97c-9296b7b9fffd","check":"egress","status":"timeout","value_ms":null,
     "actual_ip":null,"reason_code":"timeout","measured_at":1790188930.1,"attempt":1,"final":false,
     "expected_ip":"189.235.77.73","legacy_rc":28,"detail":"cloudflare:timeout(timeout), ipify:timeout(timeout)"}
  ],
  "targets": [{"node":"…","name":"团队1-26 · ATT · 162.225.42.252","tag":"ss-162-225-42-252",
               "addr":"162.225.42.252","port":8388,"method":"aes-256-gcm","expected_ip":"162.225.42.252","udp":false}]
}
```

- `value_ms`：成功时的总耗时；失败、超时一律为 null，**绝不当作延迟**（与 fleet/API.md §5 一致）。
- 中控按自然键 `(node, check, measured_at, attempt)` 幂等合并，不依赖 `id`；`first_id`/`last_id` 用来发现边缘裁剪造成的缺口。
- `targets` 不含密码。
- 体量：16 个节点约 55 条/分钟、每条约 250–350 B，30 s 一批约 8 KB（未压缩）。

## 4. relayd 接入步骤

1. **依赖**（fleet-edge/Cargo.toml）：
   ```toml
   fleet-probe = { path = "../fleet/crates/probe" }
   ```
   tokio 特性由 cargo 自动合并（本 crate 需要 `sync`、`macros`、`io-util`），relayd 现有的 current_thread 运行时可以直接用。已在 a395 用一个独立工作区按 path 依赖引入两个 crate 验证过编译和运行。
   `probe/Cargo.toml` 和 `health/Cargo.toml` 末尾各有一个空的 `[workspace]`（和 onboard 同样的做法），两个 crate 目前各自独立构建，不影响 fleetd、fleet-edge 的工作区。作为别的工作区的 path 依赖使用时不用改；要把它们收进 `fleet/Cargo.toml` 的 members 时，删掉这两段 `[workspace]`（probe 里的 `[profile.release]` 也一并挪到工作区根）。

2. **探测对象**（阶段 2 观察模式，3x-ui 还在）：每 60 s 重读一次，变化时发给 `watch` 通道：
   ```rust
   let mut srcs = vec![targets::from_xray_file(Path::new("/usr/local/x-ui/bin/config.json"))?];
   srcs.push(targets::from_xui_db(Path::new("/etc/x-ui/x-ui.db"))?);        // 热加的出站只在这里
   let ids = targets::identities_from_legacy_assets(Path::new("/etc/komari-relay-probe/config.json"))?; // tag → uuid/期望 IP
   let list = targets::merge(srcs, &ids);
   ```
   现网 line3 的 `config.json` 是 9 月 22 日写的，只有 6 个 SS 出站，另外 5 个只在 `xrayTemplateConfig` 里，所以两个都要读。这些文件都是 root 0600：relayd 不以 root 运行时，由 fleet-helper 提供一份只读副本（或者在 relayd 的 unit 里加 `ReadOnlyPaths` 与 CAP_DAC_READ_SEARCH）。阶段 3 起改从 `RelaySpec` 的 `NodeOutbound{ip, port, cipher, sealed_pw}` + `ProbeTarget` 构造 `NodeTarget`，不再读 3x-ui。

3. **启动**：
   ```rust
   let (prober, mut rx) = Prober::new(ProbeConfig::default());
   let (targets_tx, targets_rx) = tokio::sync::watch::channel(list);
   tokio::spawn(prober.run(targets_rx));
   ```

4. **落库**：从 `rx` 收结果，攒到 64 条或每 1–2 s 调一次 `ProbeStore::insert`；每小时 `prune(now)`。`ProbeStore::with_connection` 可以直接用 relayd 的 edge.db 连接（表名带 `probe_` 前缀）。

5. **上报**：组遥测批次时调用 `store.pending(host_id, 2000, targets_info, now)`，有结果就放进批次的 `"probe"` 键；**批次写入 outbox 之后**调用 `store.ack(batch.last_id)`（outbox 负责投递，重复批次中控按自然键去重）。fleetd-ingest 需要在 `accept()` 里认识这个键（见 fleet/crates/health/WIRING.md §2）。

6. **自检**：relayd 启动时可以跑一次 `Prober::run_once(&list, &[Control])`，控制路径两个来源都失败时记日志（线路机自己出不了公网）。

## 5. 与旧 komari-relay-probe（probe.py）的差异

| 项目 | 旧 | 新 | 影响 |
|---|---|---|---|
| L2 实现 | curl → 本机隔离 xray（socks）→ SS | 进程内 SS 客户端直接发请求 | 少一跳 xray 和 curl 进程，出口耗时约 34–47 ms，旧系统约 72–106 ms（line3 实测）；3000 ms 的延迟阈值不受影响 |
| L2 周期 | 300 s | 60 s（架构 §3.6） | health 里 egress 的新鲜度改为 `fresh_after(60) = 180 s` |
| 超时 | curl connect 6 s / 总 12 s | 每个来源 8 s | 都失败时两种实现都得到 `timeout` |
| 连接落地失败 | curl rc 97 或 35 | `upstream_down`（rc 7），且不再试第二个来源 | 更准确 |
| 控制路径 / probe_target_down | 无（source-status.json 只管配置有效性） | 有 | 公共来源故障不再让所有节点一起变红 |
| UDP / IPv6 | 无 | 可选 | 信息性，不参与节点状态 |
| 并发 | 线程池 4 | 信号量 4 | 相同 |

## 6. 验证结果（2026-09-24）

- **a395 本地 ssserver 1.25**（`~/team-ops-eval/fleet-probe-build/lab`）：aes-256-gcm、2022-blake3-aes-256-gcm、chacha20-ietf-poly1305 三种都能经 SS 拿到出口 IP；密码错误 → `timeout`（3 次尝试）；端口没开 → `upstream_down`；期望 IP 不符 → `egress_mismatch`；UDP STUN 经 SS 拿到映射地址（a395 的 UDP 出口是 66.93.176.2，TCP 出口是 129.227.139.37，用不经 SS 的直接 STUN 核对过，确实不同，所以 `udp_egress_mismatch` 是真的）；tcp_only 的服务端 → `udp_no_reply`；2022 UDP → `unsupported_cipher`。
- **line3 只读实测**（二进制放在 `/tmp` 的临时目录，跑完即删，没有安装任何东西；只读 config.json、x-ui.db、komari config.json）：11 个 SS 节点（第 12 个入口 20001 是主节点，没有 SS 出站），控制路径两个来源都 ok，10 个节点 tcp/egress ok 且出口 IP 正确，团队1-31（MX）tcp 超时、egress 3 次尝试都超时。与同一时刻旧 `status.json` **11/11 一致**（旧：MX 为 tcp -1、egress rc 28、attempt 3，其余 10 个 ok 且 IP 相同）。本次共 26 条结果，CPU 0.020 s，RSS 2.8 MB。
- **CPU**：a395 上 16 个节点常驻 10 min（TCP 30 s、L2 60 s、控制路径、UDP 5 min、IPv6 15 min），共 537 条结果，10 min 平均 CPU 为单核的 0.035%（2 vCPU 总量的 0.017%），每分钟采样在 0.017%–0.067% 之间，RSS 3.5–5.7 MB。[估算] line3 的 CPU 比 a395 慢，按慢 3–5 倍算仍 < 0.2% 单核（< 0.1% 的 2 vCPU 总量），远低于 1% 预算；line3 上那次 `--once` 的实测（26 条 0.020 s）与这个估算相符。对照：旧 probe.py 在 line3 上长期占 0.8%。
- 单元测试：`cargo test --manifest-path probe/Cargo.toml` 8 项（EVP_BytesToKey、地址编解码、nonce 递增、chunked 解析、STUN RFC 5769 样例、出站解析、IP 提取、SQLite 批次与游标）。

## 7. 没做的（需要别的组件）

- L3 全链路（需要 relayd 管理的 Xray 探测入站 `pi-*`）；
- `/_whoami` 对照来源与 `egress_internet_or_dns_fail`（需要 relayd 内置回显 :7446）；在 L2 的来源列表里加一个 `Source` 即可接上；
- DNS 解析器国别（nodeagent 的职责，架构 §3.6）；
- SS 2022 的 UDP、NodeMach 的 L2′ SOCKS。

## 8. 构建

```bash
fleet/crates/sync.sh     # 只同步 probe、health 到 a395 ~/team-ops-eval/fleet-probe-build/src，并在那里写 musl 配置
ssh a395 'source ~/team-ops-eval/tools/env.sh; cd ~/team-ops-eval/fleet-probe-build/src && export CARGO_TARGET_DIR=../target && \
  cargo test --manifest-path probe/Cargo.toml && \
  cargo build --release --target x86_64-unknown-linux-musl --manifest-path probe/Cargo.toml'
```
musl 配置是 fleet-edge 的配方（系统 gcc 编 ring 与 bundled SQLite），由 sync.sh 写在 a395 的构建目录，不放进仓库，避免影响 fleet/crates 下其他 crate 的构建。静态二进制 2.9 MB。
