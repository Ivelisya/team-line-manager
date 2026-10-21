# 运维手册

日常运维一律走 `fleetctl`（人和 AI 共用）。它调中控 API；中控用自己的 SSH 密钥登录机器，所有写操作和每次远程命令都进审计（`fleetctl audit`、网页「审计」）。

## 1. 准备

1. 在网页「设置 → API 密钥」创建密钥，按用途选 scope（见 `docs/API.md`「API 密钥与 scope」）：
   - 只看：`read`
   - AI 日常运维：`read,write,exec`（给 `secrets` 才能取分享链接，按需再加）
   - 密钥设有效天数，不用了就撤销。token 只显示一次。
2. 取得 `fleetctl`：
   - Windows：`cargo build -p fleetctl --release`，得到 `target\release\fleetctl.exe`。
   - Linux x86_64：在 fleet-dev 上跑 `lab/install-fleetctl.sh`，从 `/opt/fleet/dist/fleetctl` 取。
3. 写配置并自检：
   ```
   fleetctl login --url http://192.168.31.60:8080 --token flk_xxxxxxxx_xxxx
   fleetctl whoami
   ```
   配置也可用环境变量 `FLEET_URL`、`FLEET_TOKEN`；文件位置：Linux/macOS `~/.config/fleetctl/config.toml`，Windows `%APPDATA%\fleetctl\config.toml`。

约定：脚本和 AI 都加 `--json`（一行 JSON：`ls` / `entries` / `incidents` / `policy ls` 是与表格同列的精简对象，单台完整视图用 `show`、`policy show`；其余是 API `data` 原样）；错误走 stderr、退出码非 0（没拿到中控响应时 code 为 `unreachable` 或 `timeout`，后者说明操作可能已执行，先查 `jobs` / `audit` 再决定重试）；人看的输出时间一律是北京时间；`<机器>` 可写 id、显示名（团队1-06）、线路机名、IP，歧义时命令会列出候选。

## 2. 常见任务 → 命令

| 任务 | 命令 |
|---|---|
| 今天有没有问题 | `fleetctl status` |
| 哪些机器不正常 | `fleetctl ls --status down`、`fleetctl ls --status degraded` |
| 某团队全部节点 | `fleetctl ls --team 团队1` |
| 一台机器详情、为什么不健康 | `fleetctl show 团队1-06`（看「健康原因」） |
| 最近一天的 CPU/内存/磁盘/网速 | `fleetctl series 团队1-06 --range 24h` |
| 入口的流量与探测 | `fleetctl series 团队1-06 --metric traffic` / `--metric probe` |
| 告警列表、确认 | `fleetctl incidents`、`fleetctl ack <id>` |
| 服务日志 | `fleetctl logs 团队1-06 --unit fleet-agent --lines 300`（unit：fleet-agent / fleet-xray / ssserver） |
| 中控自己的日志 | `fleetctl server-logs --lines 300` |
| 在一台机器上执行命令 | `fleetctl exec 团队1-06 -- systemctl status fleet-agent` |
| 对一个团队的全部节点执行 | `fleetctl exec --team 团队1 -- uptime`（并发 4，汇总退出码） |
| 上架 | `fleetctl provision --file batch.json --dry-run`，确认后 `fleetctl provision --file batch.json --yes --follow` |
| 已有落地机上多了一个 IP，加成一台节点（同机 IP 节点，见 §6） | `fleetctl provision --kind vps --host 团队1-03 --ip 203.0.113.7 --team 团队1 --relay 线路机2 --yes --follow` |
| 看作业进度、失败原因 | `fleetctl jobs --active`、`fleetctl job <id> --follow`；失败后 `fleetctl retry <id>` |
| 退役 | `fleetctl retire 团队1-06 --reason 到期不续 --yes --follow`（宿主机上的同机 IP 节点会一并退役，命令先列出来） |
| 彻底删除已退役机器 | `fleetctl purge 团队1-06 --yes`（它的入口近 35 天的流量还计入另一端未退役的机器时会被拒，报错里给出可删日期；另一端已退役的不受限） |
| 把节点换到另一台线路机 | `fleetctl move 团队1-06 --to 线路机3 --yes --follow`（分享链接会变，见 §6） |
| 线路机退役前搬空 | `fleetctl evacuate 线路机2 --to 线路机3 --yes --follow` |
| 恢复已退役的机器 | `fleetctl restore 团队1-06 --relay 线路机3 --reason 客户续约 --yes --follow`（先预检再提交，见 §6） |
| 改名 / 换团队 / 改编号 | `fleetctl rename 团队1-06 --team 团队2 --number 3` |
| 编号压缩成连续 | `fleetctl renumber 团队1`（预览），`--yes` 执行 |
| 升级 agent | `fleetctl upgrade-agent 团队1-06`、`fleetctl upgrade-agent --all --yes` |
| 入口启停、轮换凭据 | `fleetctl entries`、`fleetctl entry disable 线路机2:20001 --yes`、`fleetctl entry rotate … --yes` |
| 线路机直出入口（从线路机自己的 IP 出去，旧系统的「节点00-默认」） | `fleetctl entry add-direct 线路机2 --yes --follow`（每台线路机至多一条；线路机 agent 要先升到新版本）；链接 `fleetctl share 线路机2-直出` |
| 分享链接 | `fleetctl share 线路机2:20001`（需要 secrets，记审计） |
| 谁做过什么 | `fleetctl audit --target 团队1-06` |
| 盯实时变化 | `fleetctl watch` |
| 密钥管理 | 网页，或 `FLEET_PASSWORD=… fleetctl keys ls --user admin`（密钥管理只认登录会话） |

批量上架文件 `batch.json` 是条目数组，条目里的 `team`、`relay`、`provider`、`plan` 直接写名字：

```json
[{"kind":"vps","ip":"203.0.113.10","team":"团队1","relay":"线路机2","provider":"ATTVPS","plan":"基础版"}]
```

`ss_port` 只给 NAT 机填：供应商映射到节点 8388 的公网端口（节点上 ssserver 固定监听 8388），非 NAT 机省略。

同机 IP 节点在条目里写 `"host": "团队1-03"`（宿主机的名字），不写 `ssh_port`、`login`、`ss_port`、`provider`、`plan`。

root 密码不要写进文件：条目里省略 `login` 则用中控的密钥；首次登录必须用密码时，单台上架用 `--ssh-password-env VAR`。

## 3. 故障排查流程

按这个顺序，前一步解释不了再进下一步：

1. **看全局**：`fleetctl status`。告警里的「规则」决定走向：
   - `agent_offline`：到 2。
   - `no_entry`：节点没有入口；`fleetctl entries <机器>` 确认，缺就 `POST /machines/{id}/entries`（网页「新增入口」）。
   - `egress_fail` / `tcp_fail`：入口到出口不通，到 3。
   - `relay_apply_lag` / `relay_apply_error`：线路机没应用最新配置，到 4。
   - `relay_xray_down`：线路机上的 xray 停了，到 4。
   - `clock_skew`：agent 所在机器的时钟偏了超过 1 分钟（读数会缺失或停在旧值）；`fleetctl exec <机器> -- 'timedatectl'` 看 NTP 是否同步，没开就 `timedatectl set-ntp true`（修改状态，先确认）。
   - `renewal_due` / `renewal_overdue`：续费提醒，不是故障。网页告警页这一行可直接「登记续费」，下一轮评估（≤30 秒）自动恢复。
   - `traffic_quota_high`：流量额度吃紧（本周期已用 ≥ 90% 警告，≥ 100% 严重：供应商可能限速或另收费），不是故障、不改健康状态。`fleetctl show <机器>` 看「流量额度」；找供应商加流量、调高订阅的流量上限或把流量分走。新周期开始、上限调高后下一轮评估自动恢复。
   - 告警恢复后 30 分钟内又出现会重开原告警（`reopen_count` +1，告警页显示「反复 N 次」），不会每抖一次发一对触发/恢复通知。
   - 去抖：原因连续出现满 60 秒才开告警、连续消失满 60 秒才恢复；探测类更长（`egress_fail` 150 秒、`tcp_fail` 90 秒），偶发一次探测失败不报。续费类、流量额度不去抖。
   - 通知按渠道各一个发送队列，同一渠道内逐条按顺序发（一批告警同时触发时不会并发打到同一渠道），渠道之间互不等待（一个渠道一直超时不拖慢别的）。渠道回 429 时按它给的等待时长再试（Telegram 的 `parameters.retry_after`、通用的 `Retry-After` 头，最多等 60 秒），其它可重试的失败（网络错误、5xx）隔 0.5 秒、2 秒各再试一次；积压时通知会晚到。
2. **agent 离线**：`fleetctl show <机器>` 看最近上报时间；`fleetctl exec <机器> -- 'systemctl is-active fleet-agent; uptime'`。exec 返回 255 且 stderr 是连接失败 → 机器本身不通（去供应商面板看机器状态/重启）。agent 没运行则 `fleetctl logs <机器> --unit fleet-agent` 找原因，再 `fleetctl exec <机器> -- systemctl restart fleet-agent`（修改状态，先确认）。
3. **入口不通**：`fleetctl show <节点>` 看入口的 TCP/出口探测；`fleetctl logs <节点> --unit ssserver`；`fleetctl exec <节点> -- 'ss -ltnp | grep -E "8388|ssserver"'`；再看线路机 `fleetctl logs <线路机> --unit fleet-xray`。告警对象是线路机时，是它的直出入口（没有节点、没有 TCP 探测）出口不通：看线路机自己的出网（`fleetctl exec <线路机> -- 'curl -sS -m 10 https://1.1.1.1 -o /dev/null -w %{http_code}'`）和 fleet-xray 日志。
   同机 IP 节点（`show` 里有「宿主机」）：`logs <节点> --unit ssserver` 看的是宿主机上它自己的实例 `fleet-ssserver@<SS 端口>`，`exec <节点>` 在宿主机上执行；它的 agent 离线、高负载告警都在宿主机名下。出口不对时 `fleetctl exec <节点> -- 'ip -4 addr; curl -4 -sS --interface <它的 IP> <中控地址>/echo'` 看 IP 还在不在网卡上、回显的是不是它自己。
4. **线路机配置未应用**：`fleetctl show <线路机>`（代数与 apply_error）；`fleetctl logs <线路机> --unit fleet-agent`；`fleetctl logs <线路机> --unit fleet-xray`。xray 配置错误会写在这里。
5. **作业失败**：`fleetctl job <id>` 看失败步骤和日志；修好原因后 `fleetctl retry <id>`；放弃用 `fleetctl cancel <id>`（已完成的步骤会补偿回退）。报「主机指纹变化」且确认机器重装过：`fleetctl retry <id> --reset-host-key`；报「没有可用的 root 密码」：`ROOT_PW=… fleetctl retry <id> --password-env ROOT_PW`（网页作业的「重试」里同样能勾选 / 填写）。
6. **中控本身异常**：`fleetctl server-logs`。读不到时按错误提示授权 journal（`usermod -aG systemd-journal fleet` 后重启 fleet-server）。

## 4. 何时才需要直接 ssh

默认不需要。只有这些情形：

- 中控本机故障（fleet-server 起不来、数据库挂了）：fleetctl 依赖中控，此时 ssh 进中控机用 `journalctl -u fleet-server`、`systemctl`、`psql` 处理。
- 机器的 SSH 指纹变了或 known_hosts 校验失败（exec 返回 255，stderr 提示主机密钥）：先确认不是 IP 冲突或被冒用，再在中控机更新 known_hosts。
- 要交互式操作（编辑文件、长时间抓包）或持续数分钟的任务，超出 exec 的 10 分钟上限与 256 KiB 输出上限。
- 给机器装中控没管理的东西。

直接 ssh 做的改动不进中控审计，事后在备注（`fleetctl rename <机器> --note …`）里记一笔。

## 5. 安全要点

- 密钥按最小 scope 发；`exec` 等于 root shell，只给确实需要的 AI 或人。
- exec 的命令全文会写进审计，不要在命令里放密码、令牌。
- 登录有限速：同一用户名 15 分钟内错 5 次、同一 IP 错 20 次会被 429 拒绝。
- 危险操作（退役、删除、轮换、批量升级、会改状态的 exec）先向负责人确认，再加 `--yes`。

## 6. 换线路机 / 搬空线路机 / 恢复已退役的机器 / 同机 IP 节点

### 换线路机（节点换到另一台线路机）
1. `fleetctl move 团队1-06 --to 线路机3`（交互确认）或加 `--yes --follow`。节点必须在用，且只有一个在用入口（有多个用 `--from 线路机2` 指定换掉哪条）。
2. 作业先在目标线路机建新入口（新端口、新凭据），节点是落地机时同时把新线路机 IP 加进节点防火墙放行名单；新入口应用完、出口探测通过之后才摘旧入口，最后把旧线路机 IP 从放行名单拿掉。换线期间节点始终有一个可用入口，不断网。
3. **分享链接会变**：完成后用 `fleetctl share 团队1-06` 取新链接发给客户端，旧链接失效。网页上作业完成的通知里有「复制新分享链接」。
4. 想中止：摘旧入口之前 `fleetctl cancel <job>`，新入口被摘除、防火墙恢复，旧入口不动。摘了旧入口之后只能 `fleetctl retry` 到完成。旧线路机不在线时「等待旧线路机摘除」会失败，线路机恢复后重试。

### 搬空线路机（线路机退役前）
`fleetctl evacuate 线路机2 --to 线路机3 --yes --follow`：为线路机上每个在用入口的节点各建一个换线路机作业。入口停用/还在应用中、节点有别的作业的会列为「跳过」，处理后再搬一次。全部搬完（旧入口 removed）后才能 `fleetctl retire 线路机2`（有节点入口时退役会被拒绝，并提示先搬空）。线路机自己的直出入口不搬也不挡退役：退役作业把它一起摘掉，用它的客户要换到别的线路机的直出链接。

### 恢复已退役的机器
1. `fleetctl restore 团队1-06 --relay 线路机3 --reason 客户续约`：先做预检——IP 有没有被别的在用机器占用（占用则不能恢复）、恢复后的名字（旧名 → 新名，编号接在团队末尾，`--number` 可指定空号）、中控密钥还能不能登录机器。
2. 预检说中控密钥被拒时必须给 root 密码：`ROOT_PW=… fleetctl restore … --password-env ROOT_PW`。机器重装过、指纹变了加 `--reset-host-key`。家宽代理退役时出站参数已清掉，要重新给 `--upstream socks5://用户:密码@主机:端口`。线路机原名被占用用 `--label` 改名；换团队用 `--team`。
3. 机器保持原 id，重新走上架的收敛步骤（proxy 只有建入口、等应用、验收、上线）；旧入口不复活，建新入口，**分享链接是新的**。订阅、历史、审计保留。成功后 `retired_at` 清空，原因记「已恢复：…」。
4. 取消或失败后取消：机器回到已退役，IP 与编号释放，团队编号压缩。
5. 同机 IP 节点要等宿主机在用才能恢复（先恢复宿主机）；不试登录、不要密码，SS 端口重新分配。

### 同机 IP 节点（一台落地机上的多个 IP）
供应商给一台落地机加了 IP 时，每个附加 IP 上架成一台独立的节点（自己的团队编号、入口、出口 IP），按 IP 计节点：
1. 先在供应商那边把 IP 直接配到这台机器的网卡上（`ip -4 addr` 看得到；EIP / NAT 映射的不支持）；从这个 IP 发出的流量对外显示的要是它自己（作业第一步会用 `curl --interface` 访问中控自动核对）。
2. `fleetctl provision --kind vps --host <宿主机> --ip <新 IP> --team 团队1 --relay 线路机2 --yes --follow`（批量文件条目写 `"host"`）。不用填 SSH 端口、密码、SS 端口：作业用中控密钥登录宿主机，先核对 IP（不在网卡上、或经它访问中控回显的来源不是它都会失败并说明原因），再在宿主机上装它自己的 ssserver 实例 `fleet-ssserver@<端口>`（端口 8389 起由中控分配，从这个 IP 监听和出站）和防火墙表 `ss_guard_<端口>`，然后建入口、验收。宿主机原来的 ssserver 与防火墙不动、不重启。
3. `show <宿主机>` 列出「同机 IP 节点」，`show <同机节点>` 显示「宿主机」。它没有自己的 agent：在线状态、主机读数、曲线都是宿主机的，`upgrade-agent` 升级宿主机即可。供应商取宿主机的；费用默认不记（附加 IP 单独收费时在它的「费用」里填），宿主机的流量额度把它的流量算进去。
4. 单独退役同机节点只删它自己的实例；**退役宿主机时同机节点一并退役**（`retire` 会先列出来）。彻底删除宿主机要先删它的同机节点。
5. 只想下线宿主机自己的 IP、保留机器：本期不支持；可以停用宿主机的入口（`fleetctl entry disable …`），它的「没有在用的入口」告警可静默。
6. 同机 IP 节点随迁移 0019 引入：装上这一版后不能直接换回更早的中控（旧版看到它不认识的迁移会拒绝启动，见 §9），出问题只能修好再发版，或按 §10 从备份恢复。

## 7. 版本升级顺序
先升中控（`lab/deploy-server.sh` 就是先装 fleet-server、再把新 agent 放进 dist），再 `fleetctl upgrade-agent --all --yes` 升 agent：新 agent 的上报可能带旧中控不认的字段形状（如容器里 `load1` 为 null），旧中控会拒收整份上报。
反过来，期望状态里的新东西旧 agent 也不认：直出入口（出站 `direct`）要等那台线路机的 agent 升级后再建，否则线路机报「agent 版本过旧」、整台的配置停在旧代数（这时升级 agent，或取消那个新增入口作业把直出入口摘掉，都能恢复）。

## 8. PostgreSQL 参数建议
发行版默认值（`shared_buffers = 128MB`、`work_mem = 4MB`）对中控偏小，建议在中控机上调：

| 参数 | 建议 | 原因 |
|---|---|---|
| `shared_buffers` | 内存的 25% 左右（4 GB 内存给 1GB） | 原始时序表（主机样本每台每 10 秒一行、探测、访问记录）的当天分区和小时汇总表是一直在读写的热数据，放得进缓存时曲线查询、每 3 秒一次的实时视图不用反复读盘 |
| `work_mem` | 16–32 MB（4 GB 内存取 16MB） | 每 5 分钟的主机小时汇总要把当前小时的全部样本按（机器, 时间）排序算网卡增量：实验室 136 台、约 6 万行，4MB 时排序落盘约 5 MB 临时文件；机器详情的长时段曲线（按分钟 / 小时分组聚合）同理。它按每个排序 / 哈希节点各算一份，连接池上限 20 条，不要给太大 |
| `wal_compression` | `lz4`（默认 off） | WAL 里近八成是整页镜像：每次 checkpoint 之后某一页第一次被改，整页写进 WAL。中控一直在写的页（当天分区、小时汇总、agent 状态等）每个 checkpoint 周期都要各来一次。lz4 压缩整页镜像，CPU 开销很小；实验室平均每个整页镜像从约 5.8 KB 压到约 2.6 KB |
| `checkpoint_timeout` | `15min`（默认 5min） | 整页镜像每个 checkpoint 周期重来一遍，周期拉长后热页的整页镜像次数少很多（实验室同样 15 分钟从约 8100 个降到约 3400 个）；代价是崩溃后恢复要多重放一些 WAL（实验室一个周期约 22 MB，秒级） |
| `max_wal_size` | `2GB`（默认 1GB） | 两次 checkpoint 之间 WAL 攒到这个量就提前做 checkpoint（又带出一轮整页镜像）；留足余量，批量上架、agent 补传积压时也按 15 分钟一个周期走。平时 pg_wal 只占一两个周期的量 |

改法（`psql` 以超级用户执行）：`alter system set work_mem = '16MB'; select pg_reload_conf();` 立即生效；`alter system set shared_buffers = '1GB';` 要重启 PostgreSQL 才生效（fleet-server 的连接池会自动重连，正在进行的请求会失败一次）。改完用 `show work_mem;`、`show shared_buffers;` 确认。

WAL 三项由 `deploy/central/install.sh` 写进 `/etc/postgresql/<版本>/main/conf.d/fleet.conf`，`systemctl reload postgresql` 生效，不用重启；确认：`select name, setting, sourcefile from pg_settings where name in ('wal_compression', 'checkpoint_timeout', 'max_wal_size');`（用 `alter system` 改过同名参数的话 `postgresql.auto.conf` 优先，先 `alter system reset <参数>`）。实验室实测（fleet 库约 1.4 GB，用 pg_waldump 按库统计，排除测试库）：改前每 5 分钟约 20 MB、整页镜像占 78%，合 66 KB/s（约 5.5 GB/天）；改后每 15 分钟约 22 MB、整页镜像占 40%，合 25 KB/s（约 2 GB/天），少了约 63%。

## 9. 生产中控：部署与回滚

部署 / 升级：本机在 `v1/` 下运行 `deploy/central/deploy.sh debian13 fleet.kismet613.com`（幂等，首次安装也用它）。

- `Cargo.toml`、`Cargo.lock`、`crates`、`migrations`、`deploy`、`web` 有未提交的改动时拒绝部署。
- 版本号 `0.1.0+<哈希>` 里的哈希是最后一次改动 `Cargo.toml`、`Cargo.lock`、`crates`、`migrations`、`deploy` 的提交：只改前端或文档的提交不换哈希，fleet-dev 上不重编 server 和 agent。
- xray、ssserver、geoip.dat 先和中控 `/opt/fleet/dist` 比 sha256，相同的不上传，中控上的保持原样。
- 编译失败、或 install.sh 重启后等 60 秒未登录访问 `/api/v1/auth/me` 仍不是 401，部署以非 0 退出，后者会打出 fleet-server 最近 60 行日志。中控上的安装包无论成败都删掉。
- 换下来的旧 fleet-server 留在中控 `/opt/fleet/fleet-server.prev`（同一个二进制重复部署时不覆盖它）。

回滚（新版起不来，或上线后有严重问题）：

1. 先确认这次部署**没有新迁移**：`git diff --name-only <旧哈希> <新哈希> -- migrations` 为空（新旧哈希见中控 `journalctl -u fleet-server | grep 监听 | tail -2`）。fleet-server 启动时自动跑迁移；新迁移一旦执行过，旧版本发现库里有它不认识的迁移会拒绝启动，这时不能直接换回，只能修好再发新版本（或按 §10 从备份恢复，丢失备份之后的数据）。
2. 没有新迁移时，在中控上换回旧版（先复制再改名：正在运行的可执行文件不能直接覆盖）：
   ```
   cp -p /opt/fleet/fleet-server.prev /opt/fleet/fleet-server.new && mv -f /opt/fleet/fleet-server.new /opt/fleet/fleet-server && systemctl restart fleet-server
   curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8080/api/v1/auth/me     # 应为 401
   ```
3. 这样只换回了 fleet-server：前端 `/opt/fleet/web` 和 dist 里的 fleet-agent 还是新的，换回后先别上架、别 `upgrade-agent`（新 agent 的上报旧中控可能拒收，见 §7）。要整体回到旧版本：本机 `git checkout <旧提交>` 后再跑一次 deploy.sh（同样要求没有新迁移）。

## 10. 备份与恢复

中控每天北京时间 03:30 做一份备份（`fleet-backup.timer` → `/opt/fleet/backup/fleet-backup.sh`），放在中控 `/var/backups/fleet/fleet-<北京时间>.tar`，留 14 份；第一次装上时 install.sh 立即做一份。手动做一份：`systemctl start fleet-backup`，`journalctl -u fleet-backup -n 5` 看结果。一份 tar 里有：

- `fleet.dump`：`pg_dump -Fc` 整库。`host_metric`、`entry_traffic`、`probe_result`、`access_event` 四张原始时序表（按天分区，只保留几天，量最大）只备结构不备数据，其余表（机器、入口、订阅、作业、审计、告警、小时汇总等）全量。恢复后最近几天的 10 秒粒度曲线和访问明细是空的，小时汇总都在。
- `etc/fleet/`：`secret.key`（库里 SS 密码、REALITY 私钥、通知渠道配置的加密密钥，丢了这些就解不开）、`fleet.toml`、`ssh/id_ed25519`（中控登录所有机器的密钥）。
- `var/lib/fleet/known_hosts`：各机器的主机指纹。

整个包都是敏感数据：中控上 0640 root:fleet-backup（只有拉取用户能读），PVE 上 0600 root。

### 家里 PVE 每天取一份

中控上的 `fleet-backup` 用户只能用指定公钥登录，authorized_keys 带 `restrict,command="/opt/fleet/backup/fleet-backup-latest.sh"`：不管请求执行什么都只输出最新一份 tar，不能开 shell、不能转发端口。authorized_keys 由 root 持有，这个用户改不了。第一次配置：

1. PVE 上生成专用密钥：`ssh-keygen -t ed25519 -N '' -C pve-fleet-backup -f /root/.ssh/fleet-backup`
2. 把 `/root/.ssh/fleet-backup.pub` 的内容放到中控 `/etc/fleet/backup.pub`，再跑一次 deploy.sh：install.sh 看到这个文件才写 authorized_keys，没有就跳过。中控 sshd 配了 `AllowUsers` / `AllowGroups` 的话把 fleet-backup 加进去。
3. PVE 上安装（文件在仓库 `v1/deploy/backup/`）：
   ```
   install -m 755 fleet-backup-pull.sh /usr/local/sbin/
   install -m 644 fleet-backup-pull.service fleet-backup-pull.timer /etc/systemd/system/
   ```
   中控的 SSH 地址不是 `fleet.kismet613.com` 的 22 端口（域名走了 CDN、改过端口）时，改 service 里的 `FLEET_BACKUP_HOST`，或在 PVE 的 `/root/.ssh/config` 里给这个主机配 `HostName`、`Port`。
4. 手动跑一次（第一次连接会记下中控的主机指纹）：`systemctl daemon-reload && systemctl start fleet-backup-pull`，`journalctl -u fleet-backup-pull -n 5` 与 `ls -l /var/backups/fleet-prod` 确认后 `systemctl enable --now fleet-backup-pull.timer`。

之后每天 04:30 左右取一份，存 `/var/backups/fleet-prod/fleet-prod-<北京时间>.tar`（0600），留 30 份。取到的和上一份完全一样（中控的备份没更新）时不存、服务以失败退出，`systemctl --failed` 里能看到；去中控 `systemctl status fleet-backup` 查原因。撤销 PVE 的拉取权限：删掉中控的 `/etc/fleet/backup.pub` 与 `/var/lib/fleet-backup/.ssh/authorized_keys`。

### 从备份恢复

在新装（或重装）的中控机上按顺序做：

1. 正常部署一次：`deploy/central/deploy.sh <主机> <域名>`。装好 PostgreSQL、Caddy、fleet 用户、服务与定时器；这时库是空的，密钥是新生成的。
2. 把备份 tar 拷到中控并解开：`mkdir -p /root/restore && tar -xf fleet-<时间>.tar -C /root/restore`
3. 停服务、换库（`pg_restore` 以 postgres 执行，从标准输入读，对象属主恢复为 fleet）：
   ```
   systemctl stop fleet-server
   runuser -u postgres -- psql -c 'drop database fleet' -c 'create database fleet owner fleet'
   runuser -u postgres -- pg_restore -d fleet --exit-on-error < /root/restore/fleet.dump
   ```
4. 放回密钥与主机指纹。fleet.toml 用新装的那份（里面的库密码是第 1 步新生成的），备份里 fleet.toml 改过的其它设置（retention、numbering 等）对照着抄过来；备份里有 `etc/fleet/ipintel-overrides.toml`、`etc/fleet/backup.pub` 也照样放回：
   ```
   install -m 600 -o fleet -g fleet /root/restore/etc/fleet/secret.key /etc/fleet/secret.key
   install -m 600 -o fleet -g fleet /root/restore/etc/fleet/ssh/id_ed25519 /etc/fleet/ssh/id_ed25519
   install -m 644 -o fleet -g fleet /root/restore/etc/fleet/ssh/id_ed25519.pub /etc/fleet/ssh/id_ed25519.pub
   install -m 640 -o fleet -g fleet /root/restore/var/lib/fleet/known_hosts /var/lib/fleet/known_hosts
   diff /root/restore/etc/fleet/fleet.toml /etc/fleet/fleet.toml
   ```
5. 启动并确认：`systemctl start fleet-server`（启动时跑迁移：备份来自旧版本时补上新迁移）；`curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8080/api/v1/auth/me` 应为 401；`fleetctl status` 看机器。
6. 中控 IP 变了：把域名解析改到新机器。agent 按域名上报，解析生效后自动恢复；Caddy 自动签证书。
7. 删掉 `/root/restore`（里面有密钥）。
