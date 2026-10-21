---
name: fleet-ops
description: 团队线路管理（fleet）的运维入口。查节点或线路机状态、排查告警、看服务日志、在节点上执行命令、上架新机器、退役或删除机器、改名与编号、升级 agent、取入口分享链接时使用；一律先用 fleetctl，不要直接 ssh、psql 或手拼 curl。
---

# fleet-ops

`fleetctl` 通过中控完成运维：中控用自己的 SSH 密钥登录机器，每次执行都写审计。直接 ssh 会绕开审计和 known_hosts 校验。

## 开始

```
fleetctl whoami            # 确认连得上，并看本密钥的 scopes
fleetctl --json <命令>      # AI 一律加 --json，拿 API 的 data 原样 JSON
```

- 配置：环境变量 `FLEET_URL`、`FLEET_TOKEN`，或 `fleetctl login --url <地址> --token <密钥>`。实验室中控是 `http://192.168.31.60:8080`。
- 成功退出码 0；失败退出码 1，错误 JSON 在 stderr（`{"error":{"code","message","status"}}`）。`exec` 单台时退出码等于远端命令的退出码。
- `<机器>` 写机器 id、显示名（`团队1-06`）、线路机名（`线路机2`）或 IP。歧义时命令报错并列出候选，照候选里的 id 重试。

## 先看，再动

| 要做什么 | 命令 |
|---|---|
| 总览 + 进行中的告警 | `fleetctl --json status` |
| 机器列表 | `fleetctl --json ls [--team 团队1] [--kind relay\|vps\|proxy] [--status down] [--retired]` |
| 一台机器的详情与健康原因 | `fleetctl --json show <机器>` |
| 指标曲线 | `fleetctl --json series <机器> --range 1h\|6h\|24h\|7d [--metric host\|traffic\|probe]` |
| 告警 / 确认告警 | `fleetctl --json incidents [--all]`、`fleetctl ack <id>` |
| 入口列表 | `fleetctl --json entries [<机器>]` |
| 作业 | `fleetctl --json jobs [--active]`、`fleetctl job <id>`、`fleetctl job <id> --follow` |
| 审计 | `fleetctl --json audit [--target <机器>] [--limit 50]` |
| 实时事件 | `fleetctl watch --count 20` |
| 访问监控：节点访问了哪些域名/IP | `fleetctl --json access [targets\|summary\|new\|sources\|events] [--node <机器>] [--team 团队1] [--window 1h\|24h\|7d\|30d] [--search 域名] [--blocked] [--kind domain\|ip]` |
| 来源 IP 画像：某 IP 的运营商/城市/接入方式 | `fleetctl --json ip <ip> [<ip>...]`（只查中控本地离线库，不外发） |
| 节点来源画像分布（运营商/城市/接入方式、top 来源、学习期） | `fleetctl --json sources <机器> [--window 24h\|7d\|30d] [--limit 20]` |
| IP 库状态（加载了几个、缺什么） | `fleetctl --json ipintel-status`（`loaded` 应为 5；未安装时在中控跑 `lab/install-ipintel.sh`） |
| 访问规则（域名黑白名单，按团队/节点） | `fleetctl policy ls`、`policy show <团队\|节点>`；`policy set <团队\|节点> --mode observe\|allowlist\|denylist\|blocked\|inherit --domains a.com,=exact.org [--note …]`（只存草稿，不生效）；`policy preview`（最近 24h 每节点会阻止多少）→ `policy publish --yes`；`policy discard --yes`；`policy versions`；`policy rollback <版本> --yes`。`=` 开头是精确匹配，否则含子域。发布 / 回滚会立即改写线路机路由规则，先向用户确认 |

访问规则的健康提示：节点被设为 `blocked` 时健康原因里有 `policy_blocked`（info，不降级、不开告警）。

访问监控只有线路机开了访问日志、且有真实客户端流量时才有数据；`new` 是窗口内第一次出现的目标（旧系统的「新域名」），不是告警。

排查顺序：`status` 找到告警 → `show <机器>` 读 `health.reasons` → `logs <机器>` 读服务日志 → 仍不明再 `exec`。

## 日志与执行

```
fleetctl logs <机器> --unit fleet-agent|fleet-xray|ssserver --lines 200
fleetctl server-logs --lines 200
fleetctl exec <机器> -- systemctl is-active fleet-agent
fleetctl exec <机器> -- 'df -h | tail -5'                   # 单个参数原样当脚本，可含管道
fleetctl exec --team 团队1 --timeout 30 -- uptime           # 多台并发 4，汇总每台退出码
```

- 多个参数会逐个加引号；要管道、重定向，把整条命令写成一个带引号的参数。
- 输出每个流最多 256 KiB（`truncated`）；超时 `timed_out=true`。
- 执行只读命令（systemctl status、journalctl、df、ss、cat 日志）。会改动机器状态的命令先向用户确认。
- 命令全文进审计，所以命令里不放密码或令牌。

## 会改变状态的操作：先向用户确认

下面这些先把对象和后果告诉用户，得到明确同意后再执行，并加 `--yes`（非交互环境不加 `--yes` 会被拒绝）。

```
fleetctl retire <机器> [--reason 原因] --yes [--follow]    # 摘入口、停服务，同团队编号前移
fleetctl purge <机器> --yes                                # 只对已退役；删除机器与遥测，不可恢复
fleetctl entry rotate <入口> --yes                         # 旧分享链接立即失效
fleetctl entry disable <入口> --yes
fleetctl upgrade-agent <机器> | --all --yes                # 重启 agent
fleetctl renumber <团队> --yes                             # 不加 --yes 只预览
fleetctl move <节点> --to <线路机> --yes [--from <线路机>] [--follow]   # 换线路机：新入口验收后才摘旧入口，不断网；分享链接会变，完成后 fleetctl share <节点> 取新链接
fleetctl evacuate <线路机> --to <线路机> --yes [--follow]          # 搬空线路机（退役前）：每个在用节点一个换线路机作业
fleetctl restore <已退役机器> --relay <线路机> [--team --number --label --upstream --password-env VAR --reason --reset-host-key] --yes [--follow]   # 恢复：先预检（IP 冲突、新名字、是否要 root 密码）再提交
fleetctl provision ...                                     # 先 check，再确认，再提交
```

- 上架：`fleetctl --json provision --file batch.json --dry-run` 只校验；通过后让用户确认名单，再去掉 `--dry-run` 加 `--yes --follow`。单台参数：`--kind vps --ip 1.2.3.4 --team 团队1 --relay 线路机2 --provider ATTVPS --plan 基础版`。root 密码放环境变量，用 `--ssh-password-env VAR` 传名字。
- 改名：`fleetctl rename <机器> --team 团队2 --number 3`。
- 分享链接 `fleetctl share <入口>` 需要 `secrets` scope，会记审计；链接是凭据，只给用户本人，不写进文件或日志。
- 密钥管理（`fleetctl keys ...`）只认登录会话，AI 不做；需要新密钥时请用户在网页「设置 → API 密钥」创建。

## --json 字段速查

- `ls` / `show`（MachineView）：`id name kind(relay|vps|proxy) lifecycle(active|provisioning|retiring|retired|failed) ip ssh_port revision`；`health.status`（ok|degraded|down|unknown|maintenance|inactive）与 `health.reasons[{key,rule_name,severity,text}]`（`rule_name` 是规则中文名，同通知）；`agent.online`；`host.{cpu_pct,mem_used,mem_total,disk_used,disk_total,load1}`；`entries[]`；`active_job`；`open_incidents`。
- `incidents`：`id rule rule_name subject_name severity(warning|critical) summary opened_at ack_by reopen_count`（恢复后 30 分钟内复发会重开同一条并 +1，表格里显示「反复 N 次」；大于 0 说明在抖，查探测/网络而不是只看当前状态）。规则 key：agent_offline、no_entry、egress_fail、tcp_fail、renewal_due、renewal_overdue、relay_apply_lag、relay_apply_error、relay_xray_down、host_cpu_high、host_mem_high、host_disk_high、ipintel_new_isp（来源出现新运营商）、ipintel_new_city（来源出现新城市）、ipintel_datacenter_source（机房 / VPN 来源 IP），后三个是 warning、学习期 7 天后才有、最后一次出现 24 小时后自动恢复，不用手动处理。
- 不加 `--json` 的 `status` / `show` / `entries` / `incidents` 是给人看的：种类、生命周期、健康、严重度、规则、入口状态、作业种类与状态都显示中文，时间是北京时间；`ls` 的种类 / 生命周期 / 健康列保留原值（就是 `--kind`、`--status` 的取值）。脚本和 AI 只解析 `--json`。
- `status --json` 的 `overview.online` / `offline`：按 agent 连通计（家宽代理没有 agent 算在线），与健康无关。
- `job`：`state`（queued|running|waiting|cancelling|succeeded|failed|cancelled）、`error`、`steps[]`、`logs[]`。
- `exec`：`exit_code stdout stderr duration_ms truncated timed_out`；多台时是这些字段加 `machine` 的数组。

完整接口见仓库 `docs/API.md`，运维流程与何时才需要直接 ssh 见 `docs/OPS.md`。
