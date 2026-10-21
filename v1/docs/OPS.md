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

约定：脚本和 AI 都加 `--json`（输出 API `data` 原样 JSON）；错误走 stderr、退出码非 0；`<机器>` 可写 id、显示名（团队1-06）、线路机名、IP，歧义时命令会列出候选。

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
| 看作业进度、失败原因 | `fleetctl jobs --active`、`fleetctl job <id> --follow`；失败后 `fleetctl retry <id>` |
| 退役 | `fleetctl retire 团队1-06 --reason 到期不续 --yes --follow` |
| 彻底删除已退役机器 | `fleetctl purge 团队1-06 --yes`（它的入口近 35 天的流量还计入另一端未退役的机器时会被拒，报错里给出可删日期；另一端已退役的不受限） |
| 把节点换到另一台线路机 | `fleetctl move 团队1-06 --to 线路机3 --yes --follow`（分享链接会变，见 §6） |
| 线路机退役前搬空 | `fleetctl evacuate 线路机2 --to 线路机3 --yes --follow` |
| 恢复已退役的机器 | `fleetctl restore 团队1-06 --relay 线路机3 --reason 客户续约 --yes --follow`（先预检再提交，见 §6） |
| 改名 / 换团队 / 改编号 | `fleetctl rename 团队1-06 --team 团队2 --number 3` |
| 编号压缩成连续 | `fleetctl renumber 团队1`（预览），`--yes` 执行 |
| 升级 agent | `fleetctl upgrade-agent 团队1-06`、`fleetctl upgrade-agent --all --yes` |
| 入口启停、轮换凭据 | `fleetctl entries`、`fleetctl entry disable 线路机2:20001 --yes`、`fleetctl entry rotate … --yes` |
| 分享链接 | `fleetctl share 线路机2:20001`（需要 secrets，记审计） |
| 谁做过什么 | `fleetctl audit --target 团队1-06` |
| 盯实时变化 | `fleetctl watch` |
| 密钥管理 | 网页，或 `FLEET_PASSWORD=… fleetctl keys ls --user admin`（密钥管理只认登录会话） |

批量上架文件 `batch.json` 是条目数组，条目里的 `team`、`relay`、`provider`、`plan` 直接写名字：

```json
[{"kind":"vps","ip":"203.0.113.10","team":"团队1","relay":"线路机2","provider":"ATTVPS","plan":"基础版","ss_port":8388}]
```

root 密码不要写进文件：条目里省略 `login` 则用中控的密钥；首次登录必须用密码时，单台上架用 `--ssh-password-env VAR`。

## 3. 故障排查流程

按这个顺序，前一步解释不了再进下一步：

1. **看全局**：`fleetctl status`。告警里的「规则」决定走向：
   - `agent_offline`：到 2。
   - `no_entry`：节点没有入口；`fleetctl entries <机器>` 确认，缺就 `POST /machines/{id}/entries`（网页「新增入口」）。
   - `egress_fail` / `tcp_fail`：入口到出口不通，到 3。
   - `relay_apply_lag` / `relay_apply_error`：线路机没应用最新配置，到 4。
   - `relay_xray_down`：线路机上的 xray 停了，到 4。
   - `renewal_due` / `renewal_overdue`：续费提醒，不是故障。网页告警页这一行可直接「登记续费」，下一轮评估（≤30 秒）自动恢复。
   - 告警恢复后 30 分钟内又出现会重开原告警（`reopen_count` +1，告警页显示「反复 N 次」），不会每抖一次发一对触发/恢复通知。
2. **agent 离线**：`fleetctl show <机器>` 看最近上报时间；`fleetctl exec <机器> -- 'systemctl is-active fleet-agent; uptime'`。exec 返回 255 且 stderr 是连接失败 → 机器本身不通（去供应商面板看机器状态/重启）。agent 没运行则 `fleetctl logs <机器> --unit fleet-agent` 找原因，再 `fleetctl exec <机器> -- systemctl restart fleet-agent`（修改状态，先确认）。
3. **入口不通**：`fleetctl show <节点>` 看入口的 TCP/出口探测；`fleetctl logs <节点> --unit ssserver`；`fleetctl exec <节点> -- 'ss -ltnp | grep -E "8388|ssserver"'`；再看线路机 `fleetctl logs <线路机> --unit fleet-xray`。
4. **线路机配置未应用**：`fleetctl show <线路机>`（代数与 apply_error）；`fleetctl logs <线路机> --unit fleet-agent`；`fleetctl logs <线路机> --unit fleet-xray`。xray 配置错误会写在这里。
5. **作业失败**：`fleetctl job <id>` 看失败步骤和日志；修好原因后 `fleetctl retry <id>`；放弃用 `fleetctl cancel <id>`（已完成的步骤会补偿回退）。
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

## 6. 换线路机 / 搬空线路机 / 恢复已退役的机器

### 换线路机（节点换到另一台线路机）
1. `fleetctl move 团队1-06 --to 线路机3`（交互确认）或加 `--yes --follow`。节点必须在用，且只有一个在用入口（有多个用 `--from 线路机2` 指定换掉哪条）。
2. 作业先在目标线路机建新入口（新端口、新凭据），节点是落地机时同时把新线路机 IP 加进节点防火墙放行名单；新入口应用完、出口探测通过之后才摘旧入口，最后把旧线路机 IP 从放行名单拿掉。换线期间节点始终有一个可用入口，不断网。
3. **分享链接会变**：完成后用 `fleetctl share 团队1-06` 取新链接发给客户端，旧链接失效。网页上作业完成的通知里有「复制新分享链接」。
4. 想中止：摘旧入口之前 `fleetctl cancel <job>`，新入口被摘除、防火墙恢复，旧入口不动。摘了旧入口之后只能 `fleetctl retry` 到完成。旧线路机不在线时「等待旧线路机摘除」会失败，线路机恢复后重试。

### 搬空线路机（线路机退役前）
`fleetctl evacuate 线路机2 --to 线路机3 --yes --follow`：为线路机上每个在用入口的节点各建一个换线路机作业。入口停用/还在应用中、节点有别的作业的会列为「跳过」，处理后再搬一次。全部搬完（旧入口 removed）后才能 `fleetctl retire 线路机2`（有入口时退役会被拒绝，并提示先搬空）。

### 恢复已退役的机器
1. `fleetctl restore 团队1-06 --relay 线路机3 --reason 客户续约`：先做预检——IP 有没有被别的在用机器占用（占用则不能恢复）、恢复后的名字（旧名 → 新名，编号接在团队末尾，`--number` 可指定空号）、中控密钥还能不能登录机器。
2. 预检说中控密钥被拒时必须给 root 密码：`ROOT_PW=… fleetctl restore … --password-env ROOT_PW`。机器重装过、指纹变了加 `--reset-host-key`。家宽代理退役时出站参数已清掉，要重新给 `--upstream socks5://用户:密码@主机:端口`。线路机原名被占用用 `--label` 改名；换团队用 `--team`。
3. 机器保持原 id，重新走上架的收敛步骤（proxy 只有建入口、等应用、验收、上线）；旧入口不复活，建新入口，**分享链接是新的**。订阅、历史、审计保留。成功后 `retired_at` 清空，原因记「已恢复：…」。
4. 取消或失败后取消：机器回到已退役，IP 与编号释放，团队编号压缩。

## 7. 版本升级顺序
先升中控（`lab/deploy-server.sh` 就是先装 fleet-server、再把新 agent 放进 dist），再 `fleetctl upgrade-agent --all --yes` 升 agent：新 agent 的上报可能带旧中控不认的字段形状（如容器里 `load1` 为 null），旧中控会拒收整份上报。
