# fleet-agent 实验室与部署说明

边缘程序 `fleet-agent`（crate：`v1/crates/fleet-agent`）。一个静态 musl 二进制，两种角色：`node`（只上报主机遥测）、`relay`（再加上声明式管理 Xray、流量/探测/访问日志）。

## 构建

# 在 fleet-dev:/root/src（lab/sync-server.sh 同步的整个工作区）里
# fleet-dev:/root/src-agent（只放 fleet-proto + fleet-agent，远端 Cargo.toml 的 members 改成这两个）
cargo test  -p fleet-agent
cargo build -p fleet-agent --release --target x86_64-unknown-linux-musl   # 约 5.3 MB
```

## 子命令

| 命令 | 作用 |
|---|---|
| `fleet-agent run`（默认） | 常驻：遥测、上报、（relay）长轮询期望状态并应用 |
| `fleet-agent once` | 采集两个主机样本，打印一份 Report JSON，检查令牌文件；不写状态、不上报 |
| `fleet-agent render --desired F` | 按 DesiredState 生成完整 xray.json 并打印（不碰本机） |
| `fleet-agent apply --desired F` | 不经中控，在本机直接应用（实验室测试；成功后写 `applied.json`） |
| `fleet-agent version` | 版本 |

`--config PATH`（默认 `/etc/fleet-agent/agent.toml`，或环境变量 `FLEET_AGENT_CONFIG`）。配置样例见 `agent.toml.example`。

## 文件与权限

| 路径 | 内容 |
|---|---|
| `/etc/fleet-agent/agent.toml` `token` | 配置、令牌（均 0600） |
| `/etc/fleet-agent/xray.json` | **由 agent 全权生成**（0600，含 REALITY 私钥与出站密码）；没有其他写者 |
| `/var/lib/fleet-agent/` (0700) | `state.json`（seq）、`queue/`（离线队列，`queue/dead/` 是被服务端永久拒绝的报告）、`stats.json`（流量基线）、`access.json`（访问日志游标）、`applied.json`（上次成功应用的期望状态）、`pending_ack.json`、`tmp/` |
| `/var/log/fleet-xray/access.log` | Xray 访问日志（agent 只读）；用 `deploy/logrotate/fleet-xray`（copytruncate；线路机上架时由中控装到 `/etc/logrotate.d/fleet-xray`，`lab/agent/fleet-xray.logrotate` 是同一份） |

**运行身份（v1 的取舍）**：agent 以 root 运行，但 `CapabilityBoundingSet=` 为空、`ProtectSystem=strict`（只有 `/var/lib/fleet-agent`、`/etc/fleet-agent` 可写）、`NoNewPrivileges`、`PrivateTmp`、`PrivateDevices`、`RestrictAddressFamilies`。原因：线路机上要 `systemctl restart fleet-xray`（仅 Xray 没在运行/骨架变更时）并读写 0600 的 xray.json；专用用户 + polkit 在 v1 里得不偿失。`/etc/fleet-agent` 必须事先存在（单元的 ReadWritePaths 依赖它）。Xray 单元同样加固，只保留 `CAP_NET_BIND_SERVICE`。

## 应用 Xray 配置的行为（relay）

1. 渲染完整配置（固定骨架 + 每个启用入口：VLESS REALITY 入站 / 出站 / 路由规则 / 回环探测 SOCKS 入站 `127.0.0.1:(port+10000)`）。入口端口 +10000 溢出或与其它端口冲突时该入口没有探测入站（只做 TCP 探测）。
2. `xray run -test`（副本里关掉日志路径）。
3. 读运行态 `lsi/lso/lsrules`，与磁盘上的上一份配置一起算差异：运行态多出来的孤儿一律删，缺的补上。顺序：ado → rmi（删除+变化）→ adi → `adrules` 整表替换 → rmo。
4. 核对运行态（tag 集合 + 规则 (ruleTag, 出站) 序列）。5. **最后**原子写 xray.json（tmp+fsync+rename+fsync 目录）。
6. 任一步失败：按相反顺序撤回，回报 `ok=false`（被点名的入口带错误，其余被回滚的入口标 `已回滚`）。
7. 只有「API 连不上且 systemd 里该服务不是 active」才写配置并 `systemctl restart`；Xray 在运行但 API 查询出错时**绝不重启**（重启会断开所有客户连接）。骨架（log/api/policy/静态出站）变化也只能重启生效。
8. 每个 DesiredState 重复应用是空操作。失败后按 15 s…5 min 退避重试同一代。
9. **启动对齐**：启动时若有 `applied.json`，先幂等再应用一遍（磁盘配置缺失/被改、Xray 新起都会被补齐），成功后才对外报告该代数；失败则代数报 0，向中控重新拉取期望状态。此外每 10 分钟做一次轻量对齐检查（磁盘 xray.json 等于期望渲染结果 + 运行态核对，不跑 `xray -test`），不一致就修复并记 warn 日志。

## 上报与健康

- 同一 seq 连续 20 次（或持续 30 分钟）被服务端回 5xx：视为该报告本身有问题，移入 `queue/dead/` 并继续发后面的；网络错误不计入。
- 服务端确认的 seq 大于本地最新 seq（时钟回拨/状态丢失）：本地序号推进到其后并 warn。
- Report 里的 `agent_health`（累计值，随带样本的报告发送）：离线队列长度/丢弃数/dead 数、访问日志尾读计数、被跳过的出口探测数、对齐修复次数、最近一次应用错误。
- 访问日志轮转靠「文件开头指纹」识别（rename 与 copytruncate 都能认）：先读完 `access.log.1` 里没读的，再从新文件开头读。
- 中控地址是明文 `http://` 且不在回环/内网时启动打醒目警告（不拒绝）；令牌文件权限宽于 0600 也警告。
- 后台任务（采集/上报/期望状态）任何一个在关停信号之前结束或 panic，进程以非零码退出，由 systemd 重启。关停时给上报器落盘缓冲、进行中的 apply 收尾的时间（最多约 35 s，单元里 `TimeoutStopSec=45`）。

`Outbound::Direct`（线路机自己的出口）**没有**加入 fleet-proto（server 在并发开发，避免破坏其 `match`）。

## 实验室当前状态（2026-10-20）

| 机器 | 装了什么 | 状态 |
|---|---|---|
| fleet-dev 192.168.31.60 | Rust 工具链；`/root/src`（工作区构建目录，lab/deploy-server.sh 在这里同时编 server 与 agent）；`/root/dl`（下载的 Xray 26.3.27 zip、shadowsocks-rust 1.25.0 tar.xz，已校验 sha256） | 无测试进程 |
| lab-relay1 192.168.31.61 | `/usr/local/bin/xray`（Xray 26.3.27，zip 的 SHA2-256 `23cd9af9…c8ae` 与官方 .dgst 一致）、`/usr/local/share/xray/{geoip,geosite}.dat`、`/usr/local/bin/fleet-agent`、`fleet-agent.service`、`fleet-xray.service`、`/etc/fleet-agent/{agent.toml,xray.json(空骨架)}` | 两个服务**已停止、未 enable**；`agent.toml` 指向 `http://192.168.31.60:8080`；没有 token 文件 |
| lab-node1 192.168.31.63 | `/usr/local/bin/ssserver`（shadowsocks-rust 1.25.0，musl，sha256 已验）、`ssserver.service`（0.0.0.0:8388，aes-256-gcm，tcp_and_udp，密码见 `/etc/ssserver/config.json`）、`/usr/local/bin/fleet-agent`、`fleet-agent.service`、`/etc/fleet-agent/agent.toml`（role=node） | ssserver 运行且 enable；fleet-agent 停止、未 enable；没有 token 文件 |
| lab-node2/3 | 未动 | |

接入真实中控时：写入 `/etc/fleet-agent/token`（0600），`systemctl enable --now fleet-agent`（relay 上 agent 会先写骨架再拉起 `fleet-xray`）。

## 联调工具

- `fake_server.py`：假中控（标准库）。`--port --token --dir --hold`；`DIR/desired.json` 作为下发内容；`DIR/fail` 写 `503`/`400` 注入故障、`DIR/revoke` 注入 401；上报记 `reports.jsonl`、回报记 `applied.jsonl`。
- `desired-add.json.tmpl` / `desired-remove.json.tmpl`：占位符 `__REALITY_PRIVATE_KEY__`、`__SS_PASSWORD__`。
- `lab-test.sh`：端到端（加入口 → 真实 REALITY 客户端经线路机和 lab-node1 访问外网 → 停 ssserver 证明确实走节点 → 幂等 → 删入口 → 无残留 → Xray 没重启）。
- 注意：lab-relay1 与 lab-node1 同一个公网出口 IP（家宽 NAT），egress 探测因此不能区分「走了节点」和「直连」；`lab-test.sh` 用「停 ssserver 后客户端失败」来证明路径。`geoip:private` 规则使得经入口访问 192.168.x.x 会被 blocked（访问日志里 `blocked=true`），测试用公网目标。
