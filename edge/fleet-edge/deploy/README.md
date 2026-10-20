# fleet-edge 上线手册

两个代理都只读、只出站：

| 代理 | 装在哪 | 读什么 | 发到哪 |
|---|---|---|---|
| `nodeagent` | 29 台落地节点 | /proc、statvfs、/sys/class/net | `POST https://38.58.57.22/v2/ingest/node` |
| `relayd` | central、line1、line3 | 同上 + Xray `StatsService.QueryStats`（reset=false）+ 进程与连接数 + `/etc/fleet/xray/config.json`（入站/用户/api 地址/SS 出站）+ 内嵌 L1/L2 探测（批次顶层 `probe`）+ 访问日志 tail（批次顶层 `access`，fleet.access.v1）+ 管理模式（fleet.relayspec.v1） | `POST https://38.58.57.22/v2/ingest/relay`，`/v2/ingest/relay/{observed,spec,spec/ack}` |

nodeagent 只读、只出站。relayd 只有在 fleetd 下发期望状态或命令时才改 Xray（API.md §10 / §10.1）：全程持有 `/run/lock/fleet-xray-config.lock`，
守卫（非受管对象、R1 默认 blackhole、其他配置段必须逐字不变；每个入口都要有路由）→ `xray run -test` → 经 Xray API 逐项生效（不重启）→ lsi/lso/lsrules 核对 →
原子写配置；失败按相反顺序撤回。回执里带前后配置差异与运行态。只有 `restart_xray` 命令会重启 fleet-xray。
每 5 s 采样，每 10 s 一批（`sample_interval_s` / `batch_samples` 可调，rollout 的 `--interval` / `--batch`）；中控不可用时批次存在本机 `/var/lib/fleet-*/outbox.dat`（≤ 2 MB，满了丢最旧并计数），恢复后按顺序补传，中控按 `(host, boot_id, seq)` 去重。

## 前置（中控，由 fleetd 负责人完成）

1. fleetd 挂上 `fleetd-ingest`（见 `../fleetd-ingest/WIRING.md`），`fleetd agent-token` 命令可用。
2. Caddy 加 `/v2/ingest/*` 路由（`request_body max_size 256KB`）。
3. 验证：`curl -s -o /dev/null -w '%{http_code}' -X POST https://38.58.57.22/v2/ingest/node` 应返回 **401**（不是 404）。

## 步骤

```bash
cd ~/Desktop/team-ops-v2/fleet-edge
deploy/fetch-dist.sh                                    # 从 a395 取回二进制到 dist/current

# 金丝雀：团队1-24
deploy/rollout.sh 团队1-24 75.37.237.106 central
# 看 30 分钟：/v2/api/v1/hosts 里 团队1-24 online、age_s < 40；节点上
#   systemctl status fleet-nodeagent; cat /var/lib/fleet-nodeagent/status.json

# 其余节点（串行，失败即停）
deploy/rollout-batch.sh '^团队1-2[1-5]$'
deploy/rollout-batch.sh '^团队1-(2[6-9]|3[0-6])$'
deploy/rollout-batch.sh '^团队1-(0[1-9]|1[0-2])$'
deploy/rollout-batch.sh '^团队2-0[12]$'

# 线路机（relayd；ssh 走 ~/.ssh/config）——三台一起发，线路机自检（主节点入口 egress）与互探（对方主节点入口 tcp）的参数在 relays.sh 里
deploy/relays.sh                      # 或 ONLY=line3 deploy/relays.sh
```

每台主机的 SSH 私钥按 IP 从 `hosts-keys.txt` 查（`--key` 可覆盖）；一律 `-o IdentitiesOnly=yes -J <线路机>`。
rollout.sh 是幂等的：重复执行只会在二进制、配置、单元有变化时重启服务；令牌已存在就沿用（`--rotate-token` 重签）。

## 数据面切换（fleet-xray.service 接管 Xray）

- 缺省值就是切换后的形态：`config=/etc/fleet/xray/config.json`、进程 `xray … -c /etc/fleet/xray/config.json`、api 从配置的 api 入站推出（现在是 127.0.0.1:62789）。
  以后 api 换成 unix socket 时只要配置里 api 入站的 listen 是路径就自动跟上；也可 `--xray-api unix:/run/xray/api.sock`。
- 还没切换的线路机（central）先装也可以：配置不存在时 relayd 退回 127.0.0.1:62789、没有探测对象；切换后 60 s 内自动读到新配置，不用重装。
  但进程识别缺省按 fleet-xray 的命令行，切换前 xray 会显示“未运行”；要在切换前看 3x-ui 的 xray：`--xray-bin xray-linux-amd64 --xray-match bin/config.json --xray-config /usr/local/x-ui/bin/config.json`。
- 累计流量种子：在旧 x-ui 停止后于线路机上执行 `python3 xui-seed.py > seed.json`（只读 x-ui.db），取回后
  `deploy/rollout.sh --relay … --seed seed.json`。relayd 在下一批（≤ 30 s）应用：累计值 = 种子；种子时刻之后启动的 Xray 的计数全额计入，
  之后逐次加增量；Xray 重启（计数回退或 pid/启动时间变化）只重置基线，累计值不断。

## 访问日志（fleet.access.v1）

路径取自 Xray 配置 `log.access`（现在是 `/var/log/x-ui/xray-access.log`，由 `team-ops-logrotate.timer` 轮转，**不要停它**）。
解析与旧 bridge 逐字一致（本机回环来源不计），轮转时读完旧文件的剩余部分；游标在批次进 outbox 之后才提交，事件 id `inode:offset`，中控去重。
计数（批次 `access.counters`）：`rotation_gaps`（轮转后旧文件已压缩/删除、剩余部分没读到）和 `truncations` 非零要看；`dropped_lines` 只算超过 32 KiB 的畸形行。
每批最多 400 条，积压时每 30 s 追 400 条；outbox 积压过半暂停读取（日志本身就是缓冲）。

## 线路机自检与互探

- 自检：每 60 s 经本机主节点入口（line1 :20005、central :20001、line3 :20001；VLESS+REALITY，与全链路检查共用本机客户端）访问
  cloudflare trace，出 `check=egress`、`node=线路机资产 uuid`、`expected_ip=本机公网 IP`、`target=127.0.0.1:<端口>` 的行。
- 互探：每 30 s 从外部 TCP 连另外两台线路机的主节点入口，出 `check=tcp`、`node=对方资产 uuid`、`target=ip:端口` 的行。
- 参数在 agent.toml 的 `[relay]`（asset_uuid、public_ip、master_port、peers），由 `deploy/relays.sh` 写入。

## 网卡流量（供应商配额）

每个样本的 `net` 里：`pub_iface`（默认路由网卡）、`pub_rx_bytes`/`pub_tx_bytes`（开机以来累计）、`pub_rx_delta`/`pub_tx_delta`
（与上一个样本之差，基线按 boot_id 持久化在状态目录，重启 agent 不丢；开机后第一个样本 = 开机以来累计）。按账期把 `pub_tx_delta` 加起来就是出站用量。

## 原生全链路检查（check = path，取代 team-ops-path-probe）

relayd 用线上同一个 xray 可执行文件起一个常驻客户端（配置在 `/var/lib/fleet-relayd/pathcheck-client.json`，入口变了才重起；不改受管配置），
节奏自适应：每 60 s 测主节点入口（线路机自检）和上次不健康的入口，全量 120 s 一次；fleetd 在上报回执里给 `"live": true`（有人在看）时 90 s 内每 60 s 全量。
经 `127.0.0.1:<入口端口>`（VLESS+REALITY，入口第一个用户）访问 `https://www.cloudflare.com/cdn-cgi/trace`，结果以 `check=path` 进 fleet.probe.v1，
`path` 字段与旧 path-status.json 行同口径（healthy、rc、phase、ms、attempts）。来源是回环，不算客户活动。
客户端进程由 `PR_SET_PDEATHSIG` 回收（relayd 的 seccomp 禁止 kill）。管理模式的 `xray api …` / `xray run -test` 与 `xray version` 也走同一套（`src/child.rs`）：
输出由读线程持续读走（`lsi` 在 central 超过 64 KiB 管道缓冲），超时由持有线程退出触发 PDEATHSIG，一定 wait 回收。`rollout.sh --no-path` 关掉。

**CPUQuota 是突发上限**：原来的 5% 让每 100 ms 只有 5 ms CPU，探测与全链路检查的握手被切成 100 ms 一格（line3 实测 106/235 个周期被限流、
延迟放大 3–6 倍）；改成 50% 后中位数 54 ms（旧系统 74 ms），限流 4/83。平均占用不变（约 0.1%），与数据面争 CPU 时靠 Nice=10 让路。

## 只读命令 xray_logs

fleetd 可以下发 `{"kind":"xray_logs","args":{"which":"error"|"access","lines":1..500}}`：从配置 `log` 段的文件末尾最多读 256 KiB、返回最后 N 行
（每行 ≤ 2000 字符）；错误日志没配文件时读 `journalctl -u fleet-xray.service`。没有其他副作用。

## 过渡期：旧探测状态转发

relayd 在 `/var/lib/komari-relay-probe/status.json`、`/var/lib/team-ops-bridge/path-status.json` 的 mtime 变化时，把解析后的 JSON 原样放进批次顶层
`legacy_probe`（只读，每个 ≤ 256 KiB），供中控与旧 `relays.probe` / `relays.paths` 逐字节对照。前者所在目录是 `komari:komari 0750`，
所以 rollout 在有 komari 组的线路机上给 relayd 加 `SupplementaryGroups=komari`（仍然没有任何 capability）。旧组件停用后文件不再变化，就不再发送。

## 停用旧组件（由 main 在 fleetd 影子核对通过后执行；不删任何文件，都能 `--undo`）

```bash
deploy/retire-legacy-relay.sh line3 --dry-run     # 先看会做什么
deploy/retire-legacy-relay.sh line3               # 停用 komari-relay-probe / komari-relay-xray / komari-relay-source-sync.timer /
                                                   # team-ops-path-probe.timer，注释掉 team-ops-restricted-bridge 强制命令密钥
deploy/retire-legacy-relay.sh line3 --undo        # 按 /root/fleet-retired-legacy.json 恢复
deploy/retire-komari-agent.sh 团队1-24 75.37.237.106 central        # 节点（密钥按 IP 查清单）
deploy/retire-komari-agent.sh line3 38.58.57.69 direct             # 线路机
deploy/retire-komari-agent.sh 团队1-24 75.37.237.106 central --undo
```

两个脚本都先检查新代理在运行且 90 s 内成功上报过（`--force` 跳过）。注意：central 上的 bridge 密钥（`from="127.0.0.1,::1"`）是旧 team-ops 本机自调用的，
注释掉后旧后台的写操作与快照就停了，所以必须在旧系统下线时再做。

## 回滚 / 卸载

```bash
deploy/rollout.sh --remove 团队1-24 75.37.237.106 central            # 停止、禁用、删单元/二进制/配置/状态/用户
deploy/rollout.sh --remove 团队1-24 75.37.237.106 central --revoke   # 同时在中控撤销令牌（不再告警）
deploy/rollout-batch.sh '.' --remove                                  # 全部节点
```

## 资源上限（systemd）

| | MemoryMax | CPUQuota | 其他 |
|---|---|---|---|
| fleet-nodeagent | 24M | 3% | Nice=10、IOSchedulingClass=idle、OOMScoreAdjust=500、无 capability、ProtectSystem=strict |
| fleet-relayd | 128M | 50%（突发上限，见下） | 同上；以 **root 运行但没有任何 capability**（见下）；只有 StateDirectory、`/etc/fleet/xray`、`/run/lock` 可写；cgroup v1 主机（Ubuntu 20.04）由 rollout 加 `MemoryLimit=`；outbox 16 MB |

实测（生产，5 s 采样 / 10 s 上报）：nodeagent 0.020–0.024% 单核、RSS 约 2 MB（4 台 Debian 12 / Ubuntu 24.04 各 5 min）；
relayd 0.45–0.53% 单核（含全链路检查的常驻 xray 客户端；进程本身 0.38–0.41%）。
早期实测（a395）：nodeagent RSS 1.6 MiB、CPU 0.013% 单核；relayd（含 3 个节点的探测）RSS 6.3 MiB、CPU 0.08% 单核；
管理操作时 xray CLI 子进程让 cgroup 峰值到 28 MB（上限 128M 留了余量）。

nodeagent 用系统用户 `fleet-edge`（无 shell），令牌 `/etc/fleet-edge/token` 与配置 `/etc/fleet-edge/agent.toml` 都是 0600 属服务用户（nodeagent → fleet-edge，relayd → root；rollout 每次都会把已有文件收紧，权限变化不触发重启）。
不用 DynamicUser：Ubuntu 20.04 的 systemd 245 没有 `LoadCredential`，DynamicUser 读不到 0600 的令牌文件。

**relayd 为什么以 root 运行**：它要读 `/etc/fleet/xray/config.json`（root 0600，含 SS 出站密码，探测要用）。
备选是装一份普通用户可读的副本，但切换系统会随时重写原文件，副本会过期，而且磁盘上多一份密码。
root 是文件属主，读它不需要 capability，所以单元里 `CapabilityBoundingSet=` 为空（没有 DAC_OVERRIDE、KILL、PTRACE、NET_ADMIN……），
`SecureBits=noroot-locked`，文件系统整体只读（只有 `/var/lib/fleet-relayd` 可写），`/root`、`/etc/ssh`、shadow、sudoers、fleetd、x-ui、caddy 的目录对它不可见，
禁止 kill/tgkill/pidfd_send_signal/ptrace 系列系统调用（不能给同为 root 的 xray、sshd 发信号）。`systemd-analyze security` 评分 1.9（OK）。
