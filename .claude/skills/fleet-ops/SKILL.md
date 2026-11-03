---
name: fleet-ops
description: 团队线路管理（fleet）的运维入口。查节点或线路机状态、排查告警、处理到期与续费提醒（看订阅、未来待付清单、登记续费含批量、改订阅）、维护前开维护窗口或静默告警（可预约）、看服务日志、在节点上执行命令、登录机器或软路由、上架新机器、管理供应商与套餐（新建、改价、归档、删除）、替换节点（新机器接替旧节点、软路由跟着换、旧的放备用）、看节点放在哪台线路机最好（放置矩阵：各线路机到节点的往返与换线建议）、退役（进备用、立即回收）与回收站（含旧面板导入）、改名与编号、升级 agent、取入口分享链接、管理客户端订阅（自己的 Clash Verge / OpenClash / v2rayN 按一个链接拉全部入口）、看计费台账与客户页（按节点数向团队收费：每个节点从开始收费日起每月一期，按月看谁已收 / 没收、标记已收、增减记录、手动计费节点、设节点不计费、客户页只读链接）、查看与接入软路由（OpenWrt + OpenClash：状态、出口、出口历史成功率与延迟、客户端、接入命令、一键部署）、管理探测点（从靠近客户的地方如香港测线路机入口：安装、主探测来源、升级）、查看或设置三网 PING（线路机到电信 / 联通 / 移动的延迟丢包）时使用，在中控上运维也用它；一律先用 fleetctl（登录用 fleetctl ssh），不要直接 ssh、psql 或手拼 curl。
---

# fleet-ops

`fleetctl` 通过中控完成运维：中控用自己的 SSH 密钥登录机器，每次执行都写审计。直接 ssh 会绕开审计和 known_hosts 校验。

## 开始

```
fleetctl whoami            # 确认连得上，并看本密钥的 scopes
fleetctl --json <命令>      # AI 一律加 --json，输出一行 JSON
```

- 配置：在中控上已经配好（见下文「在中控上运维」）。别处用环境变量 `FLEET_URL`、`FLEET_TOKEN`，或 `fleetctl login --url <地址>`（密钥从标准输入读一行，如 `< 密钥文件`；`--token <密钥>` 仍可用，但会留在 shell 历史与进程列表里）。实验室中控是 `http://192.168.31.60:8080`。
- `--json` 的 `ls` / `entries` / `incidents` / `policy ls` / `silence` 是与表格同列的精简对象（字段见同目录的 json-fields.md，要解析 `--json` 字段时先读它），一台机器的完整视图用 `show`、一个团队或节点的完整策略用 `policy show`；`billing` 是订阅、流量额度与最近续费记录合成的对象；`router exits` 是按出口算好的统计；`provider` / `plan` 的 `add`、`set` 是改完后的那一项；其余命令是 API 的 data 原样。机器上百台时 `ls` 仍有几万字符，先用 `--team` / `--kind` / `--status` 缩小。 fleetctl 和中控的版本（`0.1.0+<提交哈希>`）不一致时会在 stderr 提示一行（每次运行至多一次，不影响 stdout 与 `--json`）：用中控上的 fleetctl 或重新编译。
- 成功退出码 0；失败退出码 1，错误 JSON 在 stderr（`{"error":{"code","message","status"}}`）；没加 `--yes` 的危险操作被拒时 `code` 是 `confirm_required`（要先向用户确认，别当成失败去重试）；需要登录（`unauthorized`）/ 没有权限（`forbidden`）时 message 末尾另有一行下一步。参数写错（clap）是退出码 2，stderr 是纯文本用法提示，不是 JSON。`exec` 单台时退出码等于远端命令的退出码。`job` / `job --follow`（以及各命令的 `--follow`）遇到作业失败或取消也退出 1，stderr 给 `{"error":{"code":"job_failed"|"job_cancelled","message",…}}`。
- 没拿到中控响应时 `status` 为 null、`code` 是 `unreachable`（连不上，请求没发出去，可直接重试；`message` 带最底层原因）或 `timeout`（请求已发出、等响应超时）。`timeout` 时操作可能已在中控执行：先 `fleetctl --json jobs --active` / `fleetctl --json audit --limit 10` 核对，再决定是否重试，尤其是上架、退役、换线路机这类会建作业的命令。
- `<机器>` 写机器 id、显示名（`团队1-06`）、线路机名（`线路机2`）或 IP。歧义时命令报错并列出候选，照候选里的 id 重试。

## 在中控上运维

用户 ssh 进中控（root）后，本机 fleetctl 已配好全权密钥 central-local（部署时 `fleet-server local-admin` 建，令牌在 /root/.config/fleetctl/config.toml）：不用 login、不用令牌，`whoami` 显示 `apikey:central-local` 与全部 scopes。权限是全的，下文「先向用户确认」的操作照样先确认。

你不在中控上、用户要经中控运维时，每条命令经 ssh 在中控上执行（`ssh <中控> 'fleetctl --json status'`，生产中控的 ssh 别名是 debian13，本机没配这个别名就用地址 185.13.159.131），先 `ssh <中控> cat /root/AGENTS.md` 读一遍中控上的说明。

登录机器或软路由（只在中控上能用：要读中控的私钥 /etc/fleet/ssh/id_ed25519，root 才行）：

```
fleetctl ssh <机器>                                        # 交互式
fleetctl ssh <机器> -- 'journalctl -u fleet-agent -n 50'    # 执行完退出，退出码同远端（ssh 自己失败是 255）
fleetctl ssh router <名称> [-- 'logread | tail -50']        # 软路由（OpenWrt / iStoreOS，shell 是 ash）
```

- 用中控的密钥、只认中控记下的主机指纹（机器 /var/lib/fleet/known_hosts，软路由 /var/lib/fleet/router_known_hosts）；同机 IP 节点登录的是宿主机。软路由还没接入过（pending）登录不了，离线时多半连不上。
- 每次登录记审计 `ssh.login`（谁、哪台、带没带命令；命令内容不记）。单条命令用 `exec`：命令全文进审计、输出有上限；交互式操作、超过 10 分钟或 256 KiB 输出的才用 ssh。
- 带命令时标准输入照样转给远端（同 ssh）：在循环或 heredoc 脚本里调用时加 `</dev/null`。
- 软路由现有的登录与监听配置保持原样：root 密码、dropbear、LuCI、OpenClash 的 external-controller 与 secret、router-tunnel。
- 换 central-local：`/opt/fleet/fleet-server local-admin -c /etc/fleet/fleet.toml --rotate`（旧的立即失效）。

## 先看，再动

| 要做什么 | 命令 |
|---|---|
| 总览 + 进行中的告警 | `fleetctl --json status` |
| 机器列表 | `fleetctl --json ls [--team 团队1] [--kind relay\|vps\|proxy] [--status down]`（`--team` 写团队名或 id；`--kind`、`--status` 写错会报错，`--status` 取健康状态 ok\|degraded\|down\|unknown\|maintenance\|inactive 或生命周期 draft\|provisioning\|active\|retiring\|failed；退役完的在回收站：`fleetctl --json recycle ls`） |
| 一台机器的详情与健康原因 | `fleetctl --json show <机器>` |
| 指标曲线 | `fleetctl --json series <机器> --range 1h\|6h\|24h\|7d\|30d [--metric host\|traffic\|probe]`（数据量大：24h 约 1400 点、近 300 KB，先用短 `--range`） |
| 近 N 天可用率：每天 TCP / 出口探测的成功率，看哪天断过 | `fleetctl --json uptime <机器> [--days 30]`（1–90 天，按机器的账单时区分天）：`{days, items:[{date,tcp_total,tcp_ok,egress_total,egress_ok}], tcp_pct, egress_pct}`，`items` 旧→新、最后一项是今天（还没过完），没有探测的天全为 0；`*_pct` 是整段成功率（0–100），没有探测为 null。线路机是挂在它上面所有入口的合计 |
| 告警 / 确认告警 | `fleetctl --json incidents [--all] [--machine <机器>]`（一次最多取 500 条；多于 500 条时文字版尾注写明总数，用 `--machine` 缩小）、`fleetctl ack <id>` |
| 快到期的机器、一台机器的费用与续费记录 | `fleetctl --json ls --due 7`（7 天内到期、含已过期的在用机器，按到期排；`status` 的 `overview.due_soon` / `overdue` 是按各台提前提醒天数算的即将到期 / 已过期台数）、`fleetctl --json billing <机器>`、未来待付清单 `fleetctl --json billing --due 30d --by-provider`（登记续费、改订阅见下文「到期与续费」） |
| 静默与维护窗口（进行中 / 已预约） | `fleetctl --json silence ls`（开、删见下文「维护窗口与静默」） |
| 入口列表 | `fleetctl --json entries [<机器>]` |
| 作业 | `fleetctl --json jobs [--active | --failed] [--kind 类型] [--limit 30]`（默认只取最近 30 个，`--limit` 最大 500；`--json` 是数组、不带总数，取满了就可能还有，文字版尾注会写总数；系统发起的作业失败，如备用到期自动回收中途失败，会收到「作业失败」通知，按里面的 `fleetctl retry <id>` 处理）、`fleetctl job <id>`、`fleetctl job <id> --follow` |
| 清理作业记录（用户说清掉旧作业 / 测试留下的作业） | `fleetctl jobs-prune [--before YYYY-MM-DD]` 先报数量，用户同意后加 `--yes`：只删已成功、已取消的（连同步骤与日志），失败的、进行中的不动；不要直接改库 |
| 审计 | `fleetctl --json audit [--target <机器>] [--limit 50] [--before <上一页最后一条的 id>]` |
| 实时事件 | `fleetctl watch --count 20` |
| 节点放在哪台线路机最好：各节点到每台线路机近 24 小时的往返中位数与成功率、当前与最优线路机、建议换线（节点连线路机老断、要换线、客户说某节点慢时先看它） | `fleetctl --json placement [--team 团队2] [--node 团队2-04]`（`nodes[].cells` 与 `relays` 同序；`entry_relay_ids` 是节点在用入口所在的线路机；`suggest` 为 true 的是建议换的，`reason` 写慢多少 ms 或当前成功率；`suggestions[].command` 是按目标线路机合成的批量 `fleetctl move A,B --to X`，照抄执行、不要拆开；有多个入口的节点另在 `single_suggestions[].command` 里给带 `--from` 的单条，照抄执行）。**换线前先看 placement，不要去线路机上手写 ping / mtr 脚本逐个测** |
| 每日流量（每天全部、各节点、线路机计费量） | `fleetctl --json traffic [--days 30] [--team 团队1] [--node <机器>]`（看每日流量用它，不要拿小时表自己加；数据量大：不加 `--node` 时 `nodes[]` 不带逐日明细（`daily` / `nic_daily`），要哪台每天的就加 `--node`，也可用 `--days` / `--team` 缩小；口径：按组织时区的自然日，换线前的量算节点、留在旧线路机，直出单列） |
| 月度可用率汇总：哪家供应商、哪台节点这个月最不稳（出口为主 / TCP 成功率、断线次数与最长断线、窗口内告警次数） | `fleetctl --json report uptime [--month 2026-09 \| --days 30]`（默认本月算到今天；`--days` 1–90，和 `--month` 二选一；供应商 `providers` 与节点 `items` 都从差到好；成功率 null = 窗口内没有探测；单台每天的成功率用上面的 `uptime <机器>`） |
| 月报预览（总流量、月费、USD/GB 最高、可用率最差、告警最多的规则、到期） | `fleetctl report monthly [--month 2026-09]` 打印文字（默认上个月），`--json` 是数据加 `text`。立即发一次见下文「会改变状态的操作」 |
| 访问监控：节点访问了哪些域名/IP | `fleetctl --json access [targets\|summary\|new\|sources\|events] [--node <机器>] [--team 团队1] [--window 1h\|24h\|7d\|30d] [--search 域名] [--blocked] [--kind domain\|ip] [--limit 50]`（视图名写错会报错；`--limit` 最大 500，多于它时文字版尾注会提示） |
| 来源 IP 画像：某 IP 的运营商/城市/接入方式 | `fleetctl --json ip <ip> [<ip>...]`（只查中控本地离线库，不外发） |
| 节点来源画像分布（运营商/城市/接入方式、top 来源、学习期） | `fleetctl --json sources <机器> [--window 24h\|7d\|30d] [--limit 20]` |
| IP 库状态（加载了几个、缺什么） | `fleetctl --json ipintel-status`（`loaded` 应为 5；未安装时在中控跑 `lab/install-ipintel.sh`） |
| 访问记录保留期（原始记录、汇总各留几天） | `fleetctl --json settings retention`（改见下文「会改变状态的操作」） |
| 汇率（费用统一折成美元用的）：各币种的汇率、来源、上次更新、在用但没有汇率的 | `fleetctl --json settings fx`（手动设置、改回自动、立即更新见下文「会改变状态的操作」） |
| 访问规则（域名黑白名单，按团队/节点） | `fleetctl policy ls`、`policy show <团队\|节点>`；`policy set <团队\|节点> --mode observe\|allowlist\|denylist\|blocked\|inherit --domains a.com,=exact.org [--note …]`（只存草稿，不生效）；`policy preview`（最近 24h 每节点会阻止多少）→ `policy publish --yes`；`policy discard --yes`；`policy versions`；`policy rollback <版本> --yes`。`=` 开头是精确匹配，否则含子域。发布 / 回滚会立即改写线路机路由规则，先向用户确认 |
| 软路由列表（状态、出口通 / 总、客户端、CPU、内存、WAN） | `fleetctl --json routers [--team 团队2]` |
| 一台软路由的详情：系统、接口、OpenClash、每个出口对应的 fleet 入口与参数是否一致、延迟、连接、客户端 | `fleetctl --json router show <路由>`（`<路由>` 写 id、id 前缀或名称） |
| 软路由出口历史：每个出口测了几次、失败几次、成功率、连续两轮失败几次、延迟中位 / 最大、当前连接 | `fleetctl --json router exits <路由> [--range 1h\|6h\|24h\|7d]`（默认 24h；口径见 json-fields.md。客户说「某个出口时好时坏」先看它，不要去路由上翻日志） |
| 软路由的配置被谁改了、什么时候改的（排查「路由配置被谁改了」「出口怎么突然变了」先看它，不要去路由上翻备份） | `fleetctl --json router changes <路由> [--limit N]`（新的在前；每条 `source` 为 `job` 的带 `job_id` / `job_kind`，用 `fleetctl job <id>` 看那次作业；`router` = 路由上改的：手改，或旁路由上的看门服务重新生成）；文字版的 `router show` 末尾带最近 5 条（`--json` 不带，要变更用 `router changes`） |
| 供应商（缩写、账单时区、流量计量与重置、提前提醒、套餐数） | `fleetctl --json provider ls` |
| 套餐（供应商、规格、价格 / 周期、在用机器数） | `fleetctl --json plan ls [--provider <供应商>] [--all 含已归档]` |
| 三网 PING：线路机往外到各目标（默认武汉电信 / 联通 / 移动）的延迟与丢包 | `fleetctl --json ping <线路机>`（`current[]`：`name target mode cur_ms cur_loss`，`cur_loss` 是 %；`show <线路机>` 的 `ping` 字段相同）；全局目标 `fleetctl --json ping-targets` |
| 探测点与探测顺序：谁在测线路机的客户入口、此刻用的是哪一路（谁在代测）、各探测点到各线路机的近 1 小时平均延迟 | `fleetctl --json probe ls`（`{probe_points:[…（rank 是在探测顺序里排第几）], source:{order:[{id,name,label}], primary_name, central_enabled, effective:{state,label,substitute}}}`；`state`：online / blind（在线但探测无效）/ offline / pending / disabled） |

访问规则的健康提示：节点被设为 `blocked` 时健康原因里有 `policy_blocked`（info，不降级、不开告警）。

访问监控只有线路机开了访问日志、且有真实客户端流量时才有数据；`new` 是窗口内第一次出现的目标（旧系统的「新域名」），不是告警。
访问记录只留用户设的天数（`settings retention`：原始记录默认 7 天、汇总默认 31 天），到期中控自动删除：`access` / `sources` 的 `--window` 不能比汇总的保留期长（超了报错 400，换短一点的窗口，不要去改保留期）；`events` 只有原始记录保留期内的；「新目标」「新运营商 / 新城市」指保留期内没见过的。

告警带「近 7 天第 N 次」（`chronic` 为 true，即 `recent_count` ≥ 3）、或 `status` 末尾「近 7 天反复」列了某对象时，说明是同一处的老毛病：先考虑换线（`fleetctl placement --node <节点>`，告警的 `suggested` 里就有）或替换，而不是只确认。

排查顺序：`status` 找到告警 → `show <机器>` 读 `health.reasons` → `logs <机器>` 读服务日志 → 仍不明再 `exec`。

作业「出口验收」一直报「超时 12 秒」（或「线路机 X 连不上节点 SS 端口」）：先查节点的主机防火墙——供应商镜像自带的 ufw / firewalld 常只放行旧线路机。收敛（上架、`ss_refresh`、换线路机）会在 ufw / firewalld 运行时自动给线路机 IP 放行 SS 端口（ufw 规则注释 `fleet-ss`），作业日志有「主机防火墙」一行；有 warn 说没改成或 iptables 默认丢弃时，`fleetctl exec <节点> -- 'ufw status; iptables -S INPUT'` 看，手动放行线路机 IP 后 `retry`；不要 `ufw disable`。云厂商安全组中控管不到，告诉用户去供应商面板放行。

## 日志与执行

```
fleetctl logs <机器> --unit fleet-agent|fleet-xray|fleet-singbox|ssserver --lines 200   # fleet-singbox：线路机的 AnyTLS（sing-box）
fleetctl server-logs --lines 200
fleetctl exec <机器> -- systemctl is-active fleet-agent
fleetctl exec <机器> -- 'df -h | tail -5'                   # 单个参数原样当脚本，可含管道
fleetctl exec --team 团队1 --timeout 30 -- uptime           # 多台并发 4，汇总每台退出码
```

- 多个参数会逐个加引号；要管道、重定向，把整条命令写成一个带引号的参数。
- 输出每个流最多 256 KiB（`truncated`）；超时 `timed_out=true`。
- 执行只读命令（systemctl status、journalctl、df、ss、cat 日志）。会改动机器状态的命令先向用户确认。
- 命令全文进审计，所以命令里不放密码或令牌。

## 软路由

客户现场的 OpenWrt / iStoreOS 路由（OpenClash，mihomo 内核）经一条反向 SSH 隧道连中控（路由上的 `fleet-tunnel` 服务，与原有的 router-tunnel 并存），中控每 30 秒经隧道只读采集一次（隧道连上、断开时马上采一次）、每 5 分钟另起一次测全部出口的延迟（fleet 出口先测，每批 6 个出口并发，没通的 fleet 出口当场单独重测一次，重测通了不算不通；连续两轮不通才算不可用、开 `router_exits_down`；不通的出口之后每轮采集单独重测，连续两次测通就恢复；延迟测试没做成时 `detail.openclash.delay_error` 写原因，出口沿用上一次，不算不通）。接入只新增（隧道密钥、中控登录公钥、`/etc/init.d/fleet-tunnel`），不改路由的 root 密码、dropbear / LuCI 监听、OpenClash 配置与密钥、防火墙；用户明确要求过这些都不要动，AI 也不要去改。

- 状态 `state`：`pending`（还没接入，或刚重新生成接入命令、等在路由上执行）/ `online` / `offline`（接入过、此刻隧道连不上，`error` 是原因）。`online` 而 `error` 不为空 = 连得上但采集出错（脚本出错、输出读不了、超时），快照是上一次成功采集的，不是离线。
- OpenClash 的配置读不了（`detail.openclash.config_error`）时，`config_stale: true` 表示出口与 fleet 入口的对应沿用上次读到的。这时替换 / 换线路机 / 搬空的预览会提醒，执行时还读不了作业就失败、一台路由都不改（不会静默跳过）；用户确认那台路由没用这个节点，才重新发起加 `--no-routers`。`deploy` 是最近一次一键部署：`state` running / failed / done，`step` 正在做的那一步，`error` 失败原因。
- 出口的 `sync`：`ok` 参数和对应的 fleet 入口一致；`drift` 对上了入口但参数不一样（入口轮换过凭据、换了线路机后路由的配置没同步，或入口已删除；AnyTLS 出口的线路机只开 Reality / sing-box 没在跑也是）；`unmanaged` 不是 fleet 的入口。`drift` 持续两轮采集开告警 `router_exit_drift`；对上的入口还在用的，用 `fleetctl router sync <路由> [--exit <出口>]` 改成入口现在的参数（会重启 OpenClash、断开经它的全部连接：先不带 `--yes` 看预览、告诉用户，同意后 `--yes --follow`）；入口已删除 / 停用的同步不了（预览里 `skipped`），要换入口用 `repoint`。不点名只同步「已建立对应」的出口（中控作业写过的，或曾经参数一致的）；从没对上过入口的（路由上有人加的？预览里 `untrusted`，告警摘要「从未对上过，未自动同步」）不动，先告诉用户，确认是自己的才 `--exit` 点名；点名的出口入口是直出 / 备用团队 / 别的团队的会被拒绝，`--force` 要用户明确同意。不要手改路由的配置。
- 出口的协议：`router show` 出口表「协议」列（`--json` 的 `detail.exits[].proto`：reality / anytls）。Reality 被识别、用户要换 AnyTLS 时：`fleetctl router protocol <路由> --to anytls` 先不带 `--yes` 看预览（要切的出口、经哪台线路机、跳过的及原因：线路机只开 Reality、sing-box 没在跑、从没对上过入口），把预览告诉用户（会重启 OpenClash、断开经它的全部连接），同意后 `--yes --follow`；只切几个用 `--exit`；换回 `--to reality`。被跳过的按原因处理（sing-box 没在跑先 `upgrade-agent` 那台线路机），不要手改路由配置。替换、换线路机、改出口指向、同步会保持出口原来的协议，不用再切。
- 告警（对象是路由：`subject.kind` 为 `router`，不能 `show`，用 `router show`）：`router_offline`（接入过、隧道连不上满 3 分钟，critical，中控停机的时段不算，连上即恢复；重新接入后 30 分钟还没连上也开，warning，摘要「重新接入后还没连上」；重新生成接入命令 / 重新部署时开着的离线告警直接关掉、不发恢复）、`router_collect_error`（「软路由读取出错」：连得上但采集出错满 60 秒，摘要「连得上，但没读到状态：…」，warning：多半是路由固件 / ucode 不兼容或路由太忙，转告用户，不是网络问题）、`router_openclash_down`（OpenClash 启用了但核心没在跑，critical）、`router_exits_down`（有 fleet 出口连续两次延迟测试不通、且从第一次不通的测试起满 180 秒（同机器 `egress_fail` 的 150 秒再多一轮 30 秒，让机器告警先开）才开，warning，摘要「N 个出口连续两次延迟测试不通：…」；全部 fleet 出口不通 critical，摘要「对应面板入口的 N 个出口都…」；客户自己的出口 `unmanaged` 不算；摘要末尾「（其中 2 个指向的团队2-03 正在维护，1 个指向的线路机2 正在告警）」说明指向的机器正在维护 / 告警，先看那台机器的告警，不要去查路由；没有机器告警、维护能解释，且不通的出口全经同一台线路机时，摘要末尾写「（都经 线路机2）」：线路机在国内被挡时它自己的探测不一定看得出，先去查这台线路机的入口（放置矩阵、探测点）；不通的出口指向的都在维护中时不开；不通的出口指向的节点 / 线路机已有严重告警（出口不通、TCP 不通、agent 离线；同机 IP 节点看宿主机）时，路由告警开着但不发通知（quiet：原因已由机器告警在报），机器告警恢复后路由告警再等一轮、出口还不通才补发触发）、`router_wan_down`（WAN 口断开，critical）、`router_exit_drift`（「软路由出口参数不一致」，warning，持续两轮采集才开，摘要「N 个出口的参数和对应的面板入口不一致：节点01（团队2-03 · 线路机2:20003，凭据和入口现在的不一样）」，见上一条）、`router_resource_high`（CPU 持续 10 分钟 > 90%、内存 / 存储 > 90%、连接表 > 80%，warning；阈值与机器不同是有意的：路由 overlay 小、CPU 尖峰多）。可以给单台路由开维护窗口或静默（`silence add --router <路由>`，见「维护窗口与静默」）；全部的静默对它们也生效。客户现场计划断电、换宽带、换光猫前先开维护窗口：期间开着的告警保持、不开新的、不发通知，到期后再判。
- 配置变更时间线：每次采集发现 OpenClash 配置变了就记一条（`router changes`；时间、来源、出口的增删改，凭据只记「变了」，每台留最近 200 条）。排查「路由配置被谁改了」「出口怎么突然不通 / 指向变了」时先看它：来源是中控作业就转告用户是哪次作业（类型 + 作业 id）；`路由上改的` = 不是中控干的（有人在路由上手改，或团队1 旁路由上的看门服务重新生成了配置），不要去路由上翻备份、更不要手改。只读记录，没有回滚：中控作业失败时自己会改回，换线后旧入口已摘掉，回到旧备份只会断网。
- 接入（会改路由，先向用户确认，见下一节的命令）有两种，路由上执行的是同一份脚本：
  - 中控连不到路由（路由在 NAT 后面，三台现场路由都是）：`fleetctl router add <名称> [--team 团队2]` 打印接入命令（30 分钟、只能用一次），让用户在路由上用 root 执行；过期了 `fleetctl router bootstrap <路由> --yes` 重新生成（现在的隧道密钥立刻作废，在线的路由马上掉线）。
  - 中控能直接连到路由的 SSH（公网映射、同一网段）：一键部署 `fleetctl router add <名称> --host <IP> [--port 22] [--user root] --password-stdin --yes --follow`（密码从标准输入读一行，不要写在参数里）；路由上已经授权过中控公钥时不用密码。已有的路由重新部署用 `router deploy`；上传密钥、启动隧道这两步失败时中控把原来的隧道密钥与端口写回，在线的路由不掉线（错误里写「已改回原来的隧道密钥与端口」）。失败原因原样转告用户；中控连不到它就改用接入命令。
- 删路由（`router rm --yes`）只撤销中控这边（隧道公钥、状态与指标）；输出里的 `cleanup` 是要在路由上执行的清理命令，转给用户。还有没做完的作业改过它的出口、没确认改回（「需要人工处理」）时删不了，报错里有作业 id：先把作业处理完；路由确实回不来了（扔了、刷机）才用 `router rm <路由> --force --yes`（先向用户确认：作业不再改回它，它上面被改过的出口要自己处理），之后作业能重试收尾或取消。
- 穿透入口（`entries --json` 的 `mode` 是 `reverse`，文字表的「模式」列是「穿透」）：线路机不连节点，它的 TCP 是节点主动连线路机入口端口量的（节点 → 线路机，节点 agent 每 30 秒一次，只画曲线、不告警；`tcp_ok` 为 null 是节点 agent 还没升级，不是不通），告警与健康只看出口探测。用户说「这台节点入站丢包 / 新连接老转圈 / 供应商说只能出站」时先看预览 `fleetctl entry reverse <入口> --on`（不带 --yes），告诉用户影响，同意后 `--yes --follow`；切回用 `--off`。家宽代理、同机 IP 节点不支持；线路机或节点 agent 太旧时预览会说先 `upgrade-agent`。出口动态的节点（`--egress-ip auto`）也能穿透：只留 AnyTLS 的线路机放行的是中控最近收到该节点 agent 的来源 IP，IP 变了自动跟着改（约 10–20 秒）。节点出站只开少数端口（用 tcpdump 看，只有 53 80 443 853 2096 8080 8443 到得了线路机）的，开穿透加 `--bridge-port 8443`（1–10239，线路机 agent 要先升级到认 `bridge_port`；`entries` 的模式列写「穿透（桥 8443）」；`--off` 自动清掉）。存量入口不会自动切换，新上架按「新入口默认模式」（`settings entry-mode`）。
- 轮换入口凭据、停用入口不会改软路由：`entry rotate` / `entry disable` 的确认语会列出指向这个入口的软路由出口（之后它们就连不上了），先告诉用户。要换节点让路由跟着改用 `replace` / `move`，只改路由出口指向别的入口用 `repoint`。

## 探测点

中控在德国，客户在中国大陆：中控连不连得上线路机不代表客户。探测点是放在靠近客户的地方（家里 PVE 上的容器、香港）的小机器，跑 `fleet-agent probe`，每 30 秒连一遍各线路机的客户入口（同中控的轮换与作废规则），另记建连延迟。「探测顺序」（有序的探测点列表，第一个是主探测来源；空 = 主来源是中控）决定线路机卡片的心跳格、入口的「连不上」与 `relay_unreachable`：按顺序用第一个能用的。主来源离线（3 分钟没上报）、停用，或在线但 3 分钟里的检查都作废了（它自己连不上对照目标）时，由顺序里下一个能用的探测点代测，都用不了时中控的检查开着就由中控代测，关着就标灰；代测时连不上照报、连得上不算恢复，摘要写「香港探测点（代替离线的PVE探测点）…」「中控（代替探测无效的PVE探测点）…」。离线、作废都扣掉中控停机的时段。对照目标通、三台以上线路机全部连不上的轮次照记，`relay_unreachable` 摘要后面会带「；所有线路机同时连不上，可能是供应商整体故障」：这时先查供应商（同一家、同一网段的几台），不要逐台排查。

- 查看：`fleetctl --json probe ls`（`state` 多一种 `blind`：在线但探测无效，`discard_reason` 写原因）。24 小时 / 7 天 / 30 天到各线路机的成功率与延迟在网页（设置 → 探测点切时间窗；线路机抽屉「连接质量」的「入口连通性」，中控与各探测点并排），数据来自小时汇总，用来对比各路来源、核对某次投诉的时段。
- 加一台（会登录 / 改那台机器，先向用户确认）：中控能 SSH 到它就 `fleetctl probe add <名称> --host <IP> [--ssh-port 22]`（中控用自己的密钥登录 root，等到首次上报；登不进时报错里有中控公钥全文，转给用户加进探测点的 `/root/.ssh/authorized_keys`，再 `fleetctl probe install <名称> --host <IP>`）；否则 `fleetctl probe add <名称>` 打印一次性安装命令（30 分钟、只能用一次），让用户在探测点上用 root 执行，过期了 `fleetctl probe install <名称>`。
- 设探测顺序：`fleetctl probe order <探测点> [<探测点>…]`（按优先顺序，如 `probe order PVE 香港`；中控总在最后兜底，不写进去）；只写 `fleetctl probe order central` = 主来源改成中控。中控自己的检查：`fleetctl probe central on|off`（顺序为空时不能关）。改顺序会改变告警的判断依据，先向用户确认。
- 启停、改名：`fleetctl probe set <名称> --enable|--disable [--name …] [--note …]`（在探测顺序里的不能停用、删除：先用 `probe order` 把它移出去）；升级成中控 dist 里的 fleet-agent：`fleetctl probe upgrade <名称>`（探测点自己下载替换，等到报上新版本；新版本 5 分钟内一次都没上报成功会自己换回旧版本、不再换成这个版本，中控记成升级失败、不再下发，`probe ls` 显示「升级到 X 失败，已回到 Y」，告诉用户这个版本在探测点上跑不起来，修好后发新版本再 `probe upgrade`；同一个版本再 `probe upgrade` 中控会拒绝（「这个版本在「X」上跑不起来过」），不要反复试）；删除：`fleetctl probe rm <名称> --yes`，输出里的 `cleanup` 是要在探测点上执行的清理命令，转给用户。
- 探测点要停机、换机房：先开维护窗口 `fleetctl silence add --probe <名称> --for 2h --maintenance --comment …`（期间 `probe_point_offline` 不开、开着的保持、不发通知；它是主来源时照样由顺序里下一个或中控代测）。
- 告警 `probe_point_offline`（warning，对象是探测点：`subject_id` 为 null、`subject_name` 是探测点名）：探测点超过 3 分钟没上报，或在线但 3 分钟里的检查都作废了（摘要「在线，但最近 N 分钟的探测都无效：对照目标（…）都连不上」：探测点自己的出网坏了，不是线路机的问题）。转告用户在探测点上看 `rc-service fleet-probe status`、`/var/log/fleet-probe.log`（systemd：`journalctl -u fleet-probe`），见 OPS.md §17。

## 三网 PING

线路机每分钟往外连一轮目标（全局 8 个槽位，默认武汉电信 / 联通 / 移动的测速地址；每个 5 次，TCP 默认、不写端口连 80，或 ICMP），记延迟中位数与丢包。只展示、没有告警；它看线路质量，客户连不连得上入口看探测点。跨境 150–200 ms 是常态，不要当成问题报给用户；持续丢包才值得提。

- 查看：`fleetctl --json ping <线路机>`、`fleetctl --json ping-targets`。
- 改（会让线路机代数 +1、重新下发，先向用户确认）：全局 `fleetctl ping-targets set <槽位 1–8> [--name …] [--target host[:port]]`（`--target ""` = 不测）、`fleetctl ping-targets mode tcp|icmp`；单台 `fleetctl ping <线路机> set <槽位> …`、`fleetctl ping <线路机> mode tcp|icmp|global`、`fleetctl ping <线路机> reset [<槽位>]`。
- 某个目标一直 100% 丢包：多半是目标不开那个口（换 `host:443` 或 ICMP 试）；实际方式（`mode`）是 tcp 而设置是 icmp，说明线路机上的系统 ping 用不了（OPS.md §18）。旧 agent 不测（升级 agent 后才有）。

## 维护窗口与静默

要动机器之前（重启、换内核、改网络、迁机房，或用户说要停机维护），先给它开维护窗口：窗口内这台机器不开告警、不发通知，卡片显示「维护中」、暂停健康判定，免得一边维护一边告警刷屏。做完就删，别等它自己到期。
软路由（客户现场断电、换宽带）、探测点（停机、换机房）同样可以开：`--router <路由>` / `--probe <探测点>` 代替 `<机器>`；它们维护中开着的告警保持原状（不恢复、不发通知）、不开新的，到期后按那时的状态再判。

```
fleetctl silence add <机器> --for 2h --maintenance --comment "换内核"                               # 从现在开始
fleetctl silence add <机器> --start "2026-10-23 02:00" --for 4h --maintenance --comment "机房割接"    # 预约：--start 按北京时间，最多提前 90 天
fleetctl silence add <机器> --rule agent_offline --for 30m --comment "重启 agent"                    # 只静默一条规则（规则 key 见 json-fields.md 的 incidents 一条；不能加 --maintenance）
fleetctl silence add --router 团队2-路由A --for 4h --maintenance --comment "客户换宽带"              # 软路由的维护窗口
fleetctl silence add --router 团队2-路由A --rule router_exits_down --for 1h --comment "等客户换节点"  # 软路由只静默一条（要是 router_ 开头的规则）
fleetctl silence add --probe 香港 --for 2h --maintenance --comment "换机房"                          # 探测点的维护窗口
fleetctl --json silence ls                                                                          # 进行中的在前，已预约的随后
fleetctl silence rm <id 前缀>                                                                       # 提前结束 / 取消预约，告警即刻恢复
```

- `--for` 写 `30m` / `4h` / `2d`（可组合，如 `1h30m`），最长 30 天；更久说明机器不用了，该退役。`--all`（全部机器）最长 24 小时（等于关掉整个监控，API 密钥只能这么久；更长的请用户在网页用账号建）。`--comment` 写清在干什么，网页列表、审计和通知里都看得到。
- 给单台机器开维护窗口是维护流程的一部分，不必另问；`--all`（全部机器）要加 `--yes`，先向用户确认。不要拿静默盖住没处理的故障：机器正在告警、又不打算马上修时，先问用户。
- 新建、删除静默 / 维护窗口会通知所有启用的渠道（谁建的、对象、起止、备注；不受静默影响；软路由、探测点的对象带类型）：全部机器的一律发，单个对象的此刻生效才发、预约的不发。
- `<机器>`、`--router`、`--probe`、`--all` 只能给一个；`--all` 对软路由、探测点也生效。给软路由开维护窗口和给机器开一样，是维护流程的一部分，不必另问。
- 时间都按北京时间：表格里的、`--start` 不带时区的写法都是（与 fleetctl 跑在哪台机器、本机什么时区无关）；用户说的是别的时区就写带时区的 `2026-10-23T02:00:00+09:00`。`--start` 早于现在会报错。`id` 列是前 13 位，`rm` 给能唯一确定的前缀即可。

## 到期与续费

告警 `renewal_due` 是续费提醒（到期前 warning、过期后同一条升为 critical），不是故障、不改健康状态。开了自动续费的到期后中控自己顺延、记一条「自动续费」，不用处理。中控只记账：登记续费 = 到期日顺延、留一条续费记录，不会向供应商付款。续不续、付没付款只有用户知道。

1. 先看订阅：`fleetctl --json billing <机器>`（价格、周期、到期日与还剩几天、自动续费、最近的续费记录）；一次看所有快到期的：`fleetctl --json ls --due 7`（自动续费的也列出，`auto_renew` 为 true）。
2. 把机器、到期日、价格告诉用户，问是否已在供应商那边续费付款。不要替用户决定续不续；用户没说付过款就不登记。
3. 用户确认付过款后登记：
   ```
   fleetctl renew <机器> --yes                                                      # 到期日按订阅周期推一个周期，金额 = 订阅价格（同网页「登记续费」）
   fleetctl renew <机器> --cycles 3 --yes                                           # 一次付了 3 期：推 3 个周期，金额默认 3 期的钱
   fleetctl renew <机器> --to 2027-01-15 --amount 39.46 --note "账单 #2041" --yes    # 按供应商面板上的新到期日与实付金额
   ```
   一次付了同一家的好几台（到期日扎堆时）：**批量登记**，不要逐台 `renew`。先 `fleetctl billing --due 30d --by-provider` 看按 供应商 × 到期日 分组的待付清单（机器、原币种小计、折合 USD、供应商控制台链接；备用团队的不在里面，开了自动续费的单列、不算待付），向用户确认哪些已付，再：
   ```
   fleetctl renew 团队2-07,团队2-08,团队2-09 --ref "#2041"               # 先预览：每台 原到期 → 新到期、金额、币种、折合 USD；没加 --yes 不执行
   fleetctl renew --provider GOMAMI --due 2026-11-20 --ref "#2041"        # 按供应商选：该供应商到期日不晚于 11-20（或 --due 30d）的在用机器，不含备用团队和自动续费的（--include-spare 才带备用）
   fleetctl renew --provider GOMAMI --due 2026-11-20 --ref "#2041" --yes  # 预览无误后执行：一个事务里全部登记，审计只一条（列出机器）
   ```
   批量按订阅周期顺延（`--cycles N` 推 N 期）、金额各取订阅价格 × 期数；`--to` / `--amount` 只能用在单台。有一台不行（没有订阅等）整批都不登记。`--ref` 是账单号，写进每条续费记录的备注。
   推周期时目标月没有那一天按月末（1-31 推一个月是 2-28，闰年 2-29）。`--amount` 是订阅币种的金额，写 `39.46`（最多两位小数），实付和订阅价格不同就按实付填。登记后续费类告警下一轮评估（≤30 秒）自动恢复。
4. 不续了：不登记；机器不再用就按用户的意思退役（`retire`）。不要拿静默盖住到期提醒。
5. 订阅跟随模板：价格、币种、周期、流量上限跟随**套餐**，账单时区、提前提醒天数、流量重置（自然月 / 账单日 / 滚动 30 天）跟随**供应商**；订阅自己只存到期日、自动续费和「单独设置」的项。同一套餐的机器要一起调价 / 改额度，改套餐：`fleetctl plan set <套餐> --price 45 --yes`（`--traffic-gb`、`--currency`、`--cycle` 同理；立即对用它、没单独设置的机器生效，确认语写了几台在用机器跟着变，先向用户确认），不要逐台 `subscription set`；账单时区、提醒天数、流量重置整家一起改用 `fleetctl provider set <供应商> … --yes`。`billing` 的每项后面写着「（跟随套餐）」「（跟随供应商）」「（单独设置）」，`--json` 里 `subscription.overrides` 列出单独设置了的字段、`sub_template` 是跟随的值。
6. 只有这一台和套餐不一样（调价、换周期、改提醒天数、自动续费、流量上限）时：`fleetctl subscription set <机器> [--price 45|none] [--currency USD|none] [--cycle monthly|quarterly|semiannual|yearly|none] [--due YYYY-MM-DD] [--auto-renew true|false] [--remind-days 7|none] [--tz Asia/Shanghai|none] [--traffic-gb 2000|0|none] [--reset-day 15|none] [--traffic-rule sum|max|min|out|in|none] --yes`。给了值 = 这台单独设置，给 `none` = 改回跟随；和模板相同的值中控也按跟随存。只改给了的字段（命令先读现有订阅再整份提交），确认语逐项列出「旧 → 新」（带来源），没有变化不提交；没有订阅时新建只要 `--due`（没有套餐的机器还要 `--price`）。确认前先调中控的预览接口（`POST /machines/{id}/subscription/preview`，不写库）；报「中控版本较旧，先升级中控再用 subscription set」是中控还没有这个接口，什么都没提交，先升级中控再重试，不要绕过去直接 PUT。
   - 续费用 `renew`（留续费记录）；`subscription set --due` 只用来改正填错的到期日。
   - `--auto-renew true`：到期后中控按周期自动顺延到期日、记一条续费记录（操作人「系统」），到期前不提醒，到期日次日还没顺延才算过期；不会扣款，只是记账。
   - `--traffic-gb` 是这台单独设的流量上限（供应商的十进制 GB），`0` = 不限，`none` = 跟随套餐；`--reset-day` 是流量每月几号重置，`none` = 跟随供应商（按账单日 = 到期日几号，按自然月 = 1 号，或滚动 30 天）；`--traffic-rule` 是这台的流量计费方式：`sum` 双向（入 + 出），`max` / `min` / `out` / `in` 单向（取入出较大 / 较小 / 只算出站 / 只算入站），`none` = 跟随供应商；只对按网卡计量的机器有用。
   - 本周期已用按 agent 选中的网卡计量（`billing` 写「按 ens3 网卡」，`show` 写「按 ens3 计」）、规则取这台订阅单独设置的，否则跟供应商（双向：相加；单向：取小 / 取大 / 只算出站 / 只算入站；家宽代理和同机 IP 节点没有网卡，按业务流量、下载加上传，不跟供应商的规则）。**不能再按面板手填已用**（`--used` 已删，接口 400）：用户说「面板上已用 X GB」时，先看 `billing`——周期中途上架的写「从 X 开始统计，之前的没算进来」（下个周期自动完整），割接时录的写「接入后实测 A，加上接入前的面板读数 B（…重置后只算实测）」（已用 = A + B，A 是接入后网卡实测；和 `status` 的「本月业务流量」对不上多半是这个口径：本月业务流量只有接入后的、中途接入的带「（10-22 起）」，且是业务流量不是网卡）；数差得多就核对计量网卡与供应商的「流量重置」「流量计量」口径（见 OPS.md 流量额度一节），告诉用户原因，不要另想办法改数。
   - 不限流量的机器 `billing` 只有「本月流量：流量不限 · 本月 网卡 ↓ X / ↑ Y」（有 agent 按网卡下行 / 上行，家宽代理、同机 IP 节点按业务下载 / 上传）；有额度的才有「本周期已用」（滚动 30 天写「近 30 天已用」）。`show` 有「本月流量」一行（同样的写法，网卡的后面写「按 ens3 计」）。
   - 同机 IP 节点默认不单独记费用（记在宿主机），供应商对附加 IP 另收费时才给它设订阅（它没有套餐，要给 `--price`）。
7. 备用团队（只有一个；`fleetctl --json team ls` 的 `spare`；机器视图 / `billing --json` 的 `spare`、`spare_from`、`spare_since`、`recycle_on`，`billing` / `show` 有「备用」一行）：退役的、替换下来的机器都在这里，**在用、有订阅的过期满 1 天（`recycle_on` 那天 0 点起，账单时区）后中控自动清理并移进回收站**，审计操作者「系统」，不用 AI 去做。备用机器的续费提醒会写「明天 / 后天自动回收」并列出还指向它的软路由出口：用户要留就 `renew`（到期日顺延就不回收），或移出备用团队（`rename --team`，等于撤销退役）；客户还在用那个出口的，回收前先 `replace`。备用团队不能新建、取消、删除，只能改名（`fleetctl team set <团队> --name <新名> --yes`）。

## 计费台账（按节点数向团队收费）/ 客户页

按节点数向团队收钱的账，**收钱与供应商订阅（`subscription`、`renew`）无关**（只有新节点的开始收费日取订阅推出的购买日）。中控每分钟自动对账：团队里的计费节点（VPS / 家宽代理含同机 IP 节点、在用、不在备用团队、没标不计费）**加入即计费**（这一轮就进清单，开始收费日 = 购买日 = 订阅到期日往前推一个周期，同机 IP 节点看宿主机的订阅，没有订阅的 = 加入那天）、**离开即停止**（今天停止，已经开始的那一期照收）；**补位只认替换节点**（`fleetctl replace` 交换编号时新节点接过旧节点的位置，开始收费日、已收不变，记「X 补位接替 Y」），别的一走一来就是一减一增，不猜配对；撤销退役 / 取消不计费的节点这一期还没收完就原样接回。**每个节点从自己的开始收费日起每满一个月收一次钱（一期）**，每一期只有「已收 / 没收」两种状态，默认没收，由用户说了再标；**按月看 = 每个节点起点在这个月的那一期**（10-01 开始的 10 月那期是 10-01 – 10-31；10-30 开始的 10 月那期是 10-30 – 11-29）。手动添加的计费节点是系统外的节点（如墨西哥家宽），不会自动停止。每个团队一个只读客户页链接，客户不登录，页面按月列每个节点（名字、地区、这一期的区间、已付 / 待付），不出现 IP、供应商、价格。计费节点用名额 id 前缀（`batch list` 里的前 8 位）或节点名指定。

```
fleetctl billing month <团队> [2026-11]           # 只读：这个月要收几个、已收几个、没收哪些 + 每个节点一行（这一期的区间、已收 / 没收）；以后的月份标「还没到」
fleetctl billing paid <团队> <节点…> [--month 2026-10] [--yes]   # 标记已收（节点写名字，可多个，重名时报错并列出名额 id、改写 id 前缀；--all = 这个月所有没收的）；不加 --yes 只预览
fleetctl billing paid <团队> <节点…> --unpaid [--yes]            # 改回没收
fleetctl batch list <团队> [--events]              # 只读：计费节点表（开始收费日、状态）+ 客户页链接；--events 列增减记录
fleetctl billing init --since 2026-10-01 [--yes]  # 一次性开始记账（只给还没有计费节点的团队建）；不加 --yes 只预览。之前中控不对账
fleetctl billing slot <名额> [--start 2026-10-30] [--region …] [--note …] [--end 日期 | --reopen]   # 不带改动 = 显示；--end 只限手动添加的（系统节点离开即停止，只能改它的停止日），--reopen 只限手动添加的
fleetctl billing add <团队> --start 2026-10-15 --region 墨西哥 [--count N] [--note 墨西哥家宽]   # 手动添加计费节点
fleetctl billing rm <名额> --yes                  # 只能删手动添加的（录错了）
fleetctl billing exempt <节点> --on|--off         # 不收费（今天停止）/ 恢复收费（这一期没收完的接回，否则按购买日新开始）
fleetctl customer-page <团队> [--rotate | --disable | --enable | --delete] [--yes]   # 不带参数 = 显示链接与状态；--rotate 生成链接（已有则换新，旧链接立即失效，要 --yes）
```

- 用户问「团队1 这个月收了没有、还差谁」→ `billing month 团队1 [月份]`，照它的第一句话回答；问「哪些节点什么时候加进来的」→ `batch list 团队1 --events`。期、区间、个数都由中控算好，不要自己推算续费日。
- 用户说「团队1 10 月的都收了，除了团队1-10」→ 先 `billing paid 团队1 --all --month 2026-10` 预览，把预览里的名单去掉团队1-10 后按名字逐个写（`billing paid 团队1 团队1-01 团队1-02 … --month 2026-10`），给用户看预览、同意后加 `--yes`。
- 有节点占着的计费节点不能手动停止：不收这个节点的钱就 `billing exempt <节点> --on`。标记已收 / 没收、改开始收费日、停止、删除、`customer-page --rotate|--delete` 都会改变客户看到的内容：先把对象和后果告诉用户，同意后再做。链接发给谁由用户决定，不要自己转发。
- 日期都写 `YYYY-MM-DD`，月份写 `YYYY-MM`；「今天」按组织时区（默认北京时间）。

## 会改变状态的操作：先向用户确认

下面这些先把对象和后果告诉用户，得到明确同意后再执行，并加 `--yes`（非交互环境不加 `--yes` 会被拒绝）。不加 `--yes` 时，`move` / `evacuate` / `retire` 的预览（`--json` 也一样）在 stdout，确认错误在 stderr；加了 `--yes` 直接提交，stdout 里没有预览。

- `<入口>` 优先写 `线路机名:端口`（如 `线路机2:20001`）：表格里的 id 只显示前 8 位，批量建的入口常常相同，用 id 要写完整的。

```
fleetctl retire <机器> [--reason 原因] --yes               # 退役 = 移进备用团队：不停服务、不建作业，到期满 1 天中控自动回收；原团队编号前移，宿主机带上同机 IP 节点。先打印预览（去向与回收日、同机 IP 节点、还指向它的软路由出口、改名），确认前告诉用户。撤销 = rename --team 移出备用
fleetctl retire <机器> --now [--reason 原因] [--no-cleanup] --yes [--follow]   # 立即回收：摘入口、停服务、撤令牌，完成时进回收站（线路机、没有订阅、已过期满 1 天、没上线、本来就在备用里的只能这样）；宿主机的同机 IP 节点一并回收
fleetctl recycle ls [-q 关键词] [--source legacy]           # 回收站（只读）；show <id|名字|IP> 看一条（--json 带快照）。物理删除由中控自动做，没有手动删除
fleetctl recycle import --file legacy.json [--yes]         # 旧面板的退役机器导入回收站：不加 --yes 只预演，把会导入 / 跳过的名单给用户确认后再加 --yes
fleetctl entry rotate <入口> --yes                         # 旧分享链接立即失效
fleetctl entry disable <入口> --yes
fleetctl entry add-direct <线路机> --yes [--follow]          # 线路机直出入口：从线路机自己的 IP 出去（旧系统的「节点00-默认」），每台至多一条；链接 fleetctl share <线路机>-直出
fleetctl entry reverse <入口> --on|--off [--bridge-port N] [--yes] [--follow]  # 穿透开关：节点上的 bridge 主动连这个入口（供应商丢入站 SYN 的节点用），分享链接、订阅、软路由都不变。不加 --yes 只预览；验收不过中控自动切回原模式。节点出站只放行少数端口（连不上入口端口，开穿透探测一直 TLS 握手失败）时 --on 加 --bridge-port 8443：节点 bridge 连线路机的这个端口，线路机上入口同时听两个端口
fleetctl upgrade-agent <机器> | --all --yes                # 重启 agent；--all 先升一台金丝雀，成功后线路机逐台、VPS 节点并发；排队中的可 fleetctl cancel <作业>
fleetctl tune <机器>[,<机器>…] | --team <团队> | --all [--yes] [--follow]   # 重做系统调优（上架时已自动做；档 tune-v3：TCP 缓冲上限按套餐带宽、客户连接 RTT、内存推导，任何参数不往小改；线路机保留入口、探测、AnyTLS 端口段）：内核网络参数、BBR、日志体积上限、时间同步，不重启 ss/Xray、不断连接；--team/--all 先做一台（线路机优先）并跟到结束，成功后其余逐台提交，有进行中作业的跳过并列出；--all 要 --yes；同机 IP 节点、家宽代理 409；`ls` 的「调优」列看谁是旧档 / 未调优，`show` 的「调优推导」看缓冲上限怎么来的
fleetctl tune <机器>[,<机器>…] --rollback --yes [--follow]   # 撤销调优：按机器上第一次调优前记的原值写回、删掉调优的 sysctl 配置（没有原值快照的机器作业失败，什么都不改）；先向用户确认
fleetctl ss-refresh <机器>[,<机器>…] | --team <团队> | --all [--yes] [--follow]   # 重新下发节点的 SS 配置：升级中控后把新的 ssserver 配置（如出站目标黑名单 fleet.acl）推到存量节点；配置有变化才重启 ssserver（该节点连接断开几秒），同机 IP 节点随宿主机一起刷，做完经入口验收出口（失败不自动回滚）；--team/--all 只含在用的 VPS 节点，先做一台并跟到结束，成功后其余逐台提交，有进行中作业的跳过并列出；--all 要 --yes；线路机、家宽代理、同机 IP 节点单独下发、没有入口的节点 409
fleetctl speedtest <机器> [--size 25|100|500] [--target sjo|lax|il|nj] [--yes] [--follow]   # 按需测速：这台机器自己下载北美的测速文件（默认 100 MB、圣何塞）。**会占这台机器的流量额度（约用掉 size MB，计入供应商额度）**，确认里写明了，向用户确认后再 --yes；慢线路先 25 / 100，别随手 500。**只用北美目标（sjo 圣何塞 / lax 洛杉矶 / il 芝加哥 / nj 新泽西），不要用欧洲目标**（接口只收这四个，别想办法绕）。--follow 跟到结束再打印一行：平均 Mbit/s（MB/s）、耗时、首字节、是否下完；--json 的结果在作业 `steps[0].output`（`avg_mbps avg_mb_s total_s ttfb_s complete bytes target_name`）。同机 IP 节点对宿主机测，家宽代理 / 非在用机器 409，同一台同时只能一个作业
fleetctl report monthly --send [--month 2026-09] --yes   # 立即发一次月报（测试用；平时每月 1 号自动发上个月的）：只发给开了「月报」的已启用通知渠道，没有这样的渠道 409；会真的往 Telegram / 飞书发消息，先向用户确认。先用不带 --send 的 `report monthly` 看内容
fleetctl renumber <团队> --yes                             # 不加 --yes 只预览
fleetctl move <节点> --to <线路机> --yes [--from <线路机>] [--no-routers] [--follow]   # 换线路机：新入口验收后改软路由出口（同替换，失败全部换回、旧入口不摘）、再摘旧入口，不断网；入口线路机和端口都会变：用订阅链接的客户端自动更新，用分享链接的要换（完成后 fleetctl share <节点> 取新链接）。不加 --yes 先看预览
fleetctl move <节点A>,<节点B>,<节点C> --to <线路机> --yes [--from <线路机>] [--follow]   # 多个节点换线一律用这一条批量命令（逗号分隔），不要逐个 move：同搬空一起改软路由，每台路由只改一次、OpenClash 只重启一次（逐个 move 每个都重启一次、客户断线一次）。有一个不能换就一个都不提交；失败 / 重试 / 取消同搬空
fleetctl evacuate <线路机> --to <线路机> --yes [--no-routers] [--follow]          # 搬空线路机（退役前）：每个在用节点一个换线路机作业，软路由出口跟着改；作业并行建新入口，都验收完后由一个「带头」的一起改，每台路由只改一次、重启一次（任何一台失败全部换回、旧入口都不摘，同批一起失败，修好后都重试）。预览写「软路由 X 的 N 个节点的出口一起改，OpenClash 重启 1 次（断开它上面全部连接）」，确认前告诉用户
fleetctl placement [--team <团队>] [--node <节点>]   # 放置矩阵（只读）：线路机 agent 每 5 分钟测一次到全部在用 VPS 节点 IP 的往返（ICMP），最优 = 成功率 ≥ 99%、非故障、额度放得下（接下这个节点近 7 天的流量后到重置日仍在上限的 80% 内）的线路机里最快的；当前比最优慢 ≥ 10 ms 或当前成功率 < 99% 才建议，末尾给每台目标线路机一条批量 move 命令（先不带 --yes 看预览、告诉用户，同意后 --yes --follow）。某格成功率低不一定是链路坏：节点或云厂商挡 ICMP 时所有线路机都不通，这时 placement 不给建议，看 show 的 TCP 探测。整片「—」（某台线路机一格数据都没有）多半是它的 agent 单元太旧、系统 ping 没权限：看 `logs <线路机>` 里有没有「ping 用不了」，有就建议 `upgrade-agent <线路机>`（重写单元，先问用户）
fleetctl rebalance <线路机> [--target <线路机>] [--exclude <节点>[,<节点>…]]   # 搬迁建议（只读）：按近 7 天的速度重置前会用满的线路机，挑出搬走后能撑到重置日的最少入口和目标线路机（已超出额度的：搬走后日均降到原来 20% 以下，额度告警这时也建议它）（默认：在用、非故障、额度放得下的里面，被搬节点到它往返最小的；没有往返数据按额度余量最多的），打印一条可直接执行的批量 fleetctl move A,B,C --to X 命令（照抄执行，不要拆成逐个 move）；--exclude 的节点不动（分享给别人的入口所在节点）。billing / show 的「预计用满」一行同源
fleetctl replace <旧节点> --with <新节点> [--to <团队>] [--number N] [--no-routers] --yes [--follow]   # 替换节点：不加 --yes 只预览（见下）
fleetctl router switch <路由> <组> <出口> [--for 4h]   # 临时切换出口：select 组经 mihomo API 切到组里已有的另一个出口，不改配置、不重启 OpenClash、不断已有连接；--for 到时自动切回（30m / 4h / 1d，最长 7 天）
fleetctl router switch <路由> <组> --back               # 切回第一次切之前的出口
fleetctl router sync <路由> [--exit <出口>]… [--force] --yes [--follow]   # 同步出口参数：参数和对应入口不一致的出口改成入口现在的凭据（server:port 不变；备份、重启 OpenClash、测延迟，失败自动改回）。不加 --yes 只预览
fleetctl router protocol <路由> --to anytls|reality [--exit <出口>]… [--yes] [--follow]   # 切出口协议：指向面板入口的出口整块改写成 AnyTLS（或 Reality），同一个入口、同名，分组规则不变；不加 --yes 只预览（要切的、经哪台线路机、跳过的及原因）
fleetctl repoint <旧入口> --to <新入口> [--router <路由>]… [--exit <出口>]… --yes [--follow]   # 改出口指向：软路由上指向旧入口的出口（--exit 只改点名的）改成新入口（同替换的改路由：备份、重启 OpenClash、测延迟，失败全部换回），两个入口都不动、不改编号。不加 --yes 只预览
fleetctl provision ...                                     # 先 check，再确认，再提交
fleetctl retry <作业> [--reset-host-key] [--password-env VAR]   # 重试失败作业；只有作业因主机指纹不符失败、且确认机器重装过才加 --reset-host-key（同机 IP 节点不行：对宿主机升级 agent 后重试那个作业）；在用机器指纹变了也走这条：先 upgrade-agent 让它失败再重试；报「没有可用的 root 密码」时用 --password-env 补密码
fleetctl renew <机器> [--cycles N | --to YYYY-MM-DD] [--amount 39.46] [--note …] --yes   # 登记续费：用户确认付过款之后（见「到期与续费」）
fleetctl renew <机器,机器,…> | --provider <供应商> --due <Nd|日期> [--include-spare] [--ref 账单号] [--yes]   # 批量登记续费：不加 --yes 只预览；--yes 一个事务、一条审计
fleetctl billing --due <Nd|日期> [--by-provider] [--include-spare]   # 未来待付清单：按 供应商 × 到期日 分组，原币种小计与 USD 合计
fleetctl subscription set <机器> --price 45 … --yes          # 改订阅：只改给了的字段；值 = 单独设置，none = 改回跟随套餐 / 供应商
fleetctl settings entry-mode [normal|reverse] --yes          # 新入口默认模式（上架、加入口不指定时用，没设过是穿透）；只影响之后新建的入口
fleetctl settings retention [--raw 7] [--hourly 31] --yes   # 访问记录保留期（各 1–90 天，汇总不短于原始记录；只给一个时另一个不变）：调短后超出的访问记录几分钟内删掉、找不回来，只在用户明确要求时改
fleetctl settings fx CNY --per-usd 7.12 --yes   # 手动设置汇率（或 --usd 0.1404 = 1 CNY 合多少 USD）：之后每天的自动更新不再改它；--auto 改回自动的值；只在用户给了汇率或明确要求时改
fleetctl settings fx --refresh                  # 立即从汇率源（ECB）更新一次；失败保留上次的值（报错 502，原因在 last_error）
fleetctl router add <名称> --host <IP> --password-stdin --yes --follow   # 软路由一键部署：中控登录路由装上 fleet-tunnel（只新增）；不给 --host 只建记录、打印接入命令（不改路由，不用确认）
fleetctl router deploy <路由> --host <IP> [--password-stdin] --yes --follow   # 重新部署：换一把隧道密钥
fleetctl router bootstrap <路由> --yes                      # 重新生成接入命令：现在的隧道立刻作废，要在路由上执行新命令才连得回来
fleetctl router rm <路由> --yes                             # 删软路由：撤销隧道公钥、删掉状态与指标；路由上的清理命令在输出里，转给用户
fleetctl provider add <名称> --code ATT [--tz Asia/Shanghai] [--panel-url https://…] [--traffic-rule min|max|sum|out|in] [--traffic-reset month|billing|rolling30] [--remind-days 7]   # 新建供应商（不用 --yes）；不给的用默认：跟随组织时区、流量取大、自然月、提前 7 天提醒
fleetctl provider set <供应商> [--name …] [--code …] [--tz …|none] [--panel-url …|none] [--traffic-rule …] [--traffic-reset …] [--remind-days N] --yes   # 只改给了的项，确认语列出「旧 → 新」；时区、计量、重置、提醒立即对名下没单独设置的机器生效
fleetctl provider rm <供应商> --yes                         # 还有套餐（含归档的）或机器（含退役的）时中控拒绝
fleetctl plan add <名称> --provider <供应商> --price 299.99 --currency CNY --cycle monthly [--cpu 2 --mem-mb 2048 --disk-gb 40 --bandwidth-mbps 1000 --traffic-gb 2000] [--notes …]   # 新建套餐（不用 --yes）；价格写主单位（同 provision 的 --price），--traffic-gb 0 = 不限
fleetctl plan set <套餐> [--name …] [--price …] [--currency …] [--cycle …] [--cpu N|none …] [--traffic-gb N|none] [--notes …] [--provider <供应商>] [--archive|--unarchive] --yes   # 只改给了的项；价格、币种、周期、流量上限立即对用它、没单独设置的机器生效；不再上架的 --archive（用着的机器不受影响）
fleetctl plan rm <套餐> --yes                               # 还有机器（含退役的）用着时中控拒绝，改用 --archive
```

- 供应商、套餐：`<供应商>` 写 id、id 前缀、名称或缩写；`<套餐>` 写 id、id 前缀或名称，不同供应商有同名套餐时写 `供应商/套餐`（如 `GOMAMI/Mini`）。匹配到多个时报错并列出候选，照候选重试，不要猜。新建只是加一条目录，不碰机器，告诉用户建了什么即可；`set` 改价、改额度、改时区会立即影响已有机器的订阅（续费提醒、流量周期、`billing` 的价格），`rm` 删目录项，都先向用户确认。

- 上架前先确认供应商、套餐存在：`fleetctl --json provider ls`、`fleetctl --json plan ls --provider <供应商>`。不存在就先建——`fleetctl provider add <名称> --code <缩写>`、`fleetctl plan add <名称> --provider <供应商> --price 299.99 --currency CNY --cycle monthly [--traffic-gb 2000]`（名称、缩写、价格、币种、周期、流量额度问用户或照供应商的账单填，不要编）；已归档的套餐要用就 `plan set <套餐> --unarchive --yes`。不要手拼 curl 调 `/providers`、`/plans`，也不要 psql 去插。
- 上架：`fleetctl --json provision --file batch.json --dry-run` 只校验；通过后让用户确认名单，再去掉 `--dry-run` 加 `--yes --follow`。单台参数：`--kind vps --ip 1.2.3.4 --team 团队1 --relay 线路机2 --provider ATTVPS --plan 基础版 --due 2026-12-01`。选了套餐必须给 `--due`（到期日；价格、流量等跟随套餐，时区、提醒跟随供应商，`--provider` 可不给、取套餐的）；只有这台和套餐不同时才加 `--price 45` / `--currency` / `--cycle` / `--tz` / `--traffic-gb` / `--reset-day`（单独设置）。没有套餐时给了 `--due` 就要给 `--price`。root 密码放环境变量，用 `--ssh-password-env VAR` 传名字。`--ss-port`（批量文件里 `ss_port`）是线路机连节点用的公网（NAT 映射）端口，节点上 ssserver 固定监听 8388（供应商映射到 8388 或同号映射都行，同号的节点上会自动加端口转发）；非 NAT 机不要填。端口转发型 NAT 节点的入口 IP 和出站 IP 不同时加 `--egress-ip <出站公网 IP>`（批量文件 `egress_ip`；`--ip` 填入口 IP），之后改 / 清用 `fleetctl rename <机器> --egress-ip <IP|auto|none>`；供应商出口会变（共享 / 轮换出口）的填 `auto`（动态，只验收能出网，需线路机 agent 是新版；先 `upgrade-agent`）；`show` 有值时多一行「出口 IP」（动态的写「动态（最近 X.X.X.X）」）。价格一律用 `--price 39.46`（主单位，同 `subscription set`）。入口模式按「新入口默认模式」（没设过是穿透）；用户要普通的加 `--entry-mode normal`（批量文件条目写 `"entry_mode": "normal"`）。家宽代理、同机 IP 节点自动按普通建（校验结果里有提醒，不算失败）。
- 上架的登录试探：`--dry-run` 在 SSH 端口通了之后会用中控密钥试登录一次（不记主机指纹）。报「中控密钥登录不了 …（密钥被拒），也没填 root 密码」时，**不要猜、不要自己去机器上改**：把报错末尾那行中控公钥（`ssh-ed25519 …`，即中控的 `/etc/fleet/ssh/id_ed25519.pub`）原样给用户，让他录进供应商面板的 SSH 密钥（或机器 root 的 `~/.ssh/authorized_keys`）后再 `--dry-run`；或者向用户要 root 密码，用 `--ssh-password-env VAR` 传。用户说「面板里已经录过密钥了」却仍报这条，多半录的不是中控这把，照样把公钥给他核对。结果里的 `warnings`（如「系统是 Ubuntu 20.04，已过标准支持期」）不拦上架：转告用户，由他决定是先重装还是照常上架。端口不通（「SSH …:22 连不上」）通常是系统还没装好，等一会儿再查。作业第一步报「中控密钥登录不了，也没有可用的 root 密码」同理：录入报错里的公钥后 `fleetctl retry <作业>`，或 `--password-env` 补密码。
- 同机 IP 节点（已有 VPS 节点上多出来的 IP 当一台独立节点，按 IP 计节点）：`fleetctl provision --kind vps --host <宿主机> --ip <新 IP> --team 团队1 --relay 线路机2 --yes --follow`（批量文件条目写 `"host": "<宿主机>"`）。宿主机要是在用的普通 VPS 节点；IP 要已直接配在宿主机网卡上。不填 SSH 端口 / 密码 / SS 端口（SSH 与 agent 用宿主机的，SS 端口中控从 8389 起分配），供应商取宿主机的、费用默认不记。作业 `provision_ip` 第一步「在宿主机上核对 IP」：报「不在宿主机网卡上」或「中控看到的来源是 …」时是机器网络配置问题，让用户去供应商面板 / 机器上配好再 `fleetctl retry`。
- 升级 agent：新版本起不来、2 分钟内没上报，或线路机上新版本报的 Xray 不正常（没在跑、配置没应用到升级开始时的代数、有应用错误；升级前就这样的项不查），作业自动换回旧版本后失败（错误「…，已回滚到 <旧版本>」，线路机的写「线路机检查没通过（…）」：机器照常上报，把错误告诉用户即可）；错误里写「需要人工处理」时 agent 可能没在运行，按 `docs/OPS.md` §7 处理。`--all` 的金丝雀或任何一台线路机换上新版本后出问题，没开始的作业都取消（`error` 写明是哪台），不要逐台去重试或再 `--all`，先查失败的原因；推送之前就失败的（SSH 连不上、登录被拒）只算那一台，后面的照常继续，修好那台后 `retry` 它的作业。用户要停下 `--all`：排队中的作业 `fleetctl cancel <作业>`，等它的跟着取消；进行中的不能取消。错误是「中控上的 agent 在升级过程中换了版本…」时是中途重新部署过中控，问过用户后重新 `--all`。失败的升级作业 `retry` 从推送重来。
- 替换节点（用户说「把 团队2-01 换成新机器」「替换下来的放备用」）：**一律用 `fleetctl replace`，不要手改软路由的 OpenClash 配置、不要 `fleetctl ssh router` 去改、不要直接写数据库，也不要用 rename + retire 拼**。新节点先上架（同团队拿到临时编号、入口生效）；`fleetctl replace 团队2-01 --with 团队2-20` 先看预览（新节点 → 旧编号、旧节点 → 备用团队的哪个号、要改哪些软路由的哪些出口、编号压缩、阻断原因），告诉用户，同意后加 `--yes --follow`。作业先改软路由（同团队几台路由都改；备份 → 只改那个出口的 server/port/uuid/servername/public-key/short-id → 重启 OpenClash → 测延迟，任何一台失败全部换回原配置），再交换编号（同 rename，旧节点进备用团队，同退役的默认去向，到期满 1 天由中控自动回收）。阻断里有「软路由「X」现在离线」：等路由在线（`fleetctl routers`）或用户同意只换编号（`--no-routers`，路由上的出口就还指向旧节点）。旧节点默认去备用团队，用户要放别处再加 `--to`。作业失败：把 `fleetctl job <id>` 的错误告诉用户（改路由失败时路由都已换回、编号没动；写「需要人工处理」的是回滚也失败了，备份路径在错误里），处理后 `fleetctl retry <id>`；开始改路由之后不能取消。
- 节点被打、临时让客户走备用出口、过几小时切回（用户说「节点11 被打了，客户先切到备用」）：先看 `fleetctl --json router show <路由>` 的 `detail.groups`（哪个 select 组现在选着它、组里还有哪些出口），用 `fleetctl router switch <路由> <组> <备用出口> --for 4h`：只改 mihomo 当前的选择，不重启 OpenClash、不断已有连接，到时自动切回（不确定多久就不带 `--for`，恢复后 `--back`）。只能选组里已有的出口；url-test / fallback 组中控会拒绝（由 mihomo 自己选），这时才用下面的 repoint（会重启 OpenClash）。OpenClash 重启后 mihomo 一般会保留当前选择（实测：选择记在缓存里），到时照样由中控切回；只有组的选择被改成别的（有人手动改、换了配置）时记录才结束（审计「临时切换被路由改回」）。`fleetctl --json incidents` 里 `egress_fail` / `router_exits_down` 的 `suggested` 已经算好了切到哪个出口，照着执行前告诉用户。
- 改出口指向（用户说「把软路由的直连 / 默认出口换成线路机3」）：`fleetctl repoint 线路机1-直出 --to 线路机3-直出 [--router 团队2-老办公室]`。同一个入口被几个出口指着、只挪其中一个（如「节点20」和「节点07」都指着团队2-07 的入口，只把节点20 改到团队2-20）：加 `--exit 节点20`（可重复），没点到的出口不动；点名的出口不指向旧入口会报错。入口写入口 id、`线路机名:端口` 或节点显示名（直出入口是「线路机名-直出」）；目标线路机还没有直出入口就先 `fleetctl entry add-direct <线路机> --yes --follow`。旧入口照常在用（别的客户端、个人订阅还能用它），不要的话另行停用。不要手改路由的 OpenClash 配置、不要用 router-swap 一类脚本。
- 改名：`fleetctl rename <机器> --team 团队2 --number 3`。换团队（或预览里有别的机器连带改名）时不加 `--yes` 只打印预览：谁 → 谁、原团队连带改号的节点（原团队剩下的会自动压缩成连续编号，后面的全部改号）、一起走的同机 IP 节点、还指着的软路由出口、`注意：` 提醒。**把预览原样告诉用户，同意后再加 `--yes`**；有「面板名和出口名就对不上了」的提醒要特别说明：客户软路由的出口名（「节点08」）不跟着改，执行后面板名和出口名会错位，只能一台台挪回去。用户其实是想换掉这台机器的，用 `replace`（新节点接它的编号，团队里谁都不改名），不要用 rename 把它挪走。只改 `--label` / `--note` / `--country`、团队内改到空号的直接执行。
- 国家（名字旁的国旗）：默认按 IP 查离线库自动定；查错了或想固定就 `fleetctl rename <机器> --country JP`（ISO 3166-1 两位代码，英国是 GB），`--country auto` 改回自动；上架时也可以 `fleetctl provision … --country US`。
- 分享链接 `fleetctl share <入口>` 需要 `secrets` scope，会记审计；链接是凭据，只给用户本人，不写进文件或日志。第一行是 Reality（vless://），线路机的 AnyTLS 在跑时第二行是 AnyTLS（anytls://，自签证书、已带 insecure=1）。一批用 `fleetctl share --team <团队>` 或 `--machines a,b,c`（一次请求、一条审计，每行「名称 链接」，AnyTLS 那行是「名称 AnyTLS 链接」，`--json` 给数组、带 anytls_url），不要逐个 `share`。给用户自己的客户端优先用订阅链接（`fleetctl sub`，换线路后自动更新），给别人才用 `share`。
- AnyTLS（线路机入口的第二种协议，sing-box 只做外壳，策略、统计都还在 Xray）：`fleetctl show <线路机>` 的「AnyTLS」看在跑 / 没在跑 / 出错 / 未安装；没装的线路机 `fleetctl upgrade-agent <线路机>` 时顺带装（中控 dist 里要有 sing-box），之后 `fleetctl tune <线路机> --yes`（tune-v3 保留 AnyTLS 端口段）。端口 = 入口端口 + 20000（默认 40000–49999，云安全组要放行）。增删入口、轮换凭据会让这台线路机的 AnyTLS 连接断一下，Reality 不受影响。按线路机选入口协议：`fleetctl relay-set <线路机> --protocols anytls|reality|both`（anytls = 只留 AnyTLS，Reality 对公网关闭；中控会挡住有 Reality 软路由出口指着、AnyTLS 没在跑、agent 太旧的线路机（有穿透入口时 agent 要认穿透放行，否则提示先 `upgrade-agent`；穿透入口的 Reality 端口照旧开着、只放行它的节点）——Reality 出口先用 `router protocol <路由> --to anytls` 切过去；改成 `reality` 时 AnyTLS 软路由出口指着也被挡。按它给的原因处理，不要绕过）。详见 OPS.md §21
- 客户端订阅（用户自己的 Clash Verge、OpenClash、v2rayN 一次拿全部入口）：`fleetctl sub ls|show <订阅>`；新建 `fleetctl sub add <名称> [--only-teams …|--except-teams …] [--no-direct] [--include <机器,…>] [--exclude <机器,…>] [--protocols reality|anytls|both]`（默认全部团队、不含备用团队、含线路机直出、只 Reality；AnyTLS 条目只出在 AnyTLS 在跑的线路机上），改规则、协议 `sub edit`，`sub rotate --yes` 旧链接立即失效，`sub rm --yes`。要 `secrets`；链接只在 add / rotate 输出一次，是凭据，只给用户本人。换节点（replace）、换线路机（move）、轮换凭据后用订阅的客户端会自动更新，不用再手改客户端配置。不要替用户把他现有的客户端改成订阅，那由用户决定。
- 密钥管理（`fleetctl keys ...`）只认登录会话，AI 不做；需要新密钥时请用户在网页「设置 → API 密钥」创建。

## --json 字段速查

字段在同目录的 `json-fields.md`（每个命令的 `--json` 输出长什么样、枚举取值）：要解析 `--json` 输出、不确定字段名时先读它，不要猜。
