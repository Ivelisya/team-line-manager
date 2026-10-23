---
name: fleet-ops
description: 团队线路管理（fleet）的运维入口。查节点或线路机状态、排查告警、处理到期与续费提醒（看订阅、未来待付清单、登记续费含批量、改订阅）、维护前开维护窗口或静默告警（可预约）、看服务日志、在节点上执行命令、登录机器或软路由、上架新机器、管理供应商与套餐（新建、改价、归档、删除）、替换节点（新机器接替旧节点、软路由跟着换、旧的放备用）、退役（进备用、立即回收）与回收站（含旧面板导入）、改名与编号、升级 agent、取入口分享链接、管理客户端订阅（自己的 Clash Verge / OpenClash / v2rayN 按一个链接拉全部入口）、查看与接入软路由（OpenWrt + OpenClash：状态、出口、出口历史成功率与延迟、客户端、接入命令、一键部署）、管理探测点（从靠近客户的地方如香港测线路机入口：安装、主探测来源、升级）、查看或设置三网 PING（线路机到电信 / 联通 / 移动的延迟丢包）时使用，在中控上运维也用它；一律先用 fleetctl（登录用 fleetctl ssh），不要直接 ssh、psql 或手拼 curl。
---

# fleet-ops

`fleetctl` 通过中控完成运维：中控用自己的 SSH 密钥登录机器，每次执行都写审计。直接 ssh 会绕开审计和 known_hosts 校验。

## 开始

```
fleetctl whoami            # 确认连得上，并看本密钥的 scopes
fleetctl --json <命令>      # AI 一律加 --json，输出一行 JSON
```

- 配置：在中控上已经配好（见下文「在中控上运维」）。别处用环境变量 `FLEET_URL`、`FLEET_TOKEN`，或 `fleetctl login --url <地址>`（密钥从标准输入读一行，如 `< 密钥文件`；`--token <密钥>` 仍可用，但会留在 shell 历史与进程列表里）。实验室中控是 `http://192.168.31.60:8080`。
- `--json` 的 `ls` / `entries` / `incidents` / `policy ls` / `silence` 是与表格同列的精简对象（字段见文末速查），一台机器的完整视图用 `show`、一个团队或节点的完整策略用 `policy show`；`billing` 是订阅、流量额度与最近续费记录合成的对象；`router exits` 是按出口算好的统计；`provider` / `plan` 的 `add`、`set` 是改完后的那一项；其余命令是 API 的 data 原样。机器上百台时 `ls` 仍有几万字符，先用 `--team` / `--kind` / `--status` 缩小。 fleetctl 和中控的版本（`0.1.0+<提交哈希>`）不一致时会在 stderr 提示一行（每次运行至多一次，不影响 stdout 与 `--json`）：用中控上的 fleetctl 或重新编译。
- 成功退出码 0；失败退出码 1，错误 JSON 在 stderr（`{"error":{"code","message","status"}}`）。`exec` 单台时退出码等于远端命令的退出码。`job` / `job --follow`（以及各命令的 `--follow`）遇到作业失败或取消也退出 1，stderr 给 `{"error":{"code":"job_failed"|"job_cancelled","message",…}}`。
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
| 机器列表 | `fleetctl --json ls [--team 团队1] [--kind relay\|vps\|proxy] [--status down]`（退役完的在回收站：`fleetctl --json recycle ls`） |
| 一台机器的详情与健康原因 | `fleetctl --json show <机器>` |
| 指标曲线 | `fleetctl --json series <机器> --range 1h\|6h\|24h\|7d [--metric host\|traffic\|probe]` |
| 近 N 天可用率：每天 TCP / 出口探测的成功率，看哪天断过 | `fleetctl --json uptime <机器> [--days 30]`（1–90 天，按机器的账单时区分天）：`{days, items:[{date,tcp_total,tcp_ok,egress_total,egress_ok}], tcp_pct, egress_pct}`，`items` 旧→新、最后一项是今天（还没过完），没有探测的天全为 0；`*_pct` 是整段成功率（0–100），没有探测为 null。线路机是挂在它上面所有入口的合计 |
| 告警 / 确认告警 | `fleetctl --json incidents [--all] [--machine <机器>]`、`fleetctl ack <id>` |
| 快到期的机器、一台机器的费用与续费记录 | `fleetctl --json ls --due 7`（7 天内到期、含已过期的在用机器，按到期排；`status` 的 `overview.due_soon` / `overdue` 是按各台提前提醒天数算的即将到期 / 已过期台数）、`fleetctl --json billing <机器>`、未来待付清单 `fleetctl --json billing --due 30d --by-provider`（登记续费、改订阅见下文「到期与续费」） |
| 静默与维护窗口（进行中 / 已预约） | `fleetctl --json silence ls`（开、删见下文「维护窗口与静默」） |
| 入口列表 | `fleetctl --json entries [<机器>]` |
| 作业 | `fleetctl --json jobs [--active]`、`fleetctl job <id>`、`fleetctl job <id> --follow` |
| 审计 | `fleetctl --json audit [--target <机器>] [--limit 50] [--before <上一页最后一条的 id>]` |
| 实时事件 | `fleetctl watch --count 20` |
| 访问监控：节点访问了哪些域名/IP | `fleetctl --json access [targets\|summary\|new\|sources\|events] [--node <机器>] [--team 团队1] [--window 1h\|24h\|7d\|30d] [--search 域名] [--blocked] [--kind domain\|ip]` |
| 来源 IP 画像：某 IP 的运营商/城市/接入方式 | `fleetctl --json ip <ip> [<ip>...]`（只查中控本地离线库，不外发） |
| 节点来源画像分布（运营商/城市/接入方式、top 来源、学习期） | `fleetctl --json sources <机器> [--window 24h\|7d\|30d] [--limit 20]` |
| IP 库状态（加载了几个、缺什么） | `fleetctl --json ipintel-status`（`loaded` 应为 5；未安装时在中控跑 `lab/install-ipintel.sh`） |
| 访问记录保留期（原始记录、汇总各留几天） | `fleetctl --json settings retention`（改见下文「会改变状态的操作」） |
| 汇率（费用统一折成美元用的）：各币种的汇率、来源、上次更新、在用但没有汇率的 | `fleetctl --json settings fx`（手动设置、改回自动、立即更新见下文「会改变状态的操作」） |
| 访问规则（域名黑白名单，按团队/节点） | `fleetctl policy ls`、`policy show <团队\|节点>`；`policy set <团队\|节点> --mode observe\|allowlist\|denylist\|blocked\|inherit --domains a.com,=exact.org [--note …]`（只存草稿，不生效）；`policy preview`（最近 24h 每节点会阻止多少）→ `policy publish --yes`；`policy discard --yes`；`policy versions`；`policy rollback <版本> --yes`。`=` 开头是精确匹配，否则含子域。发布 / 回滚会立即改写线路机路由规则，先向用户确认 |
| 软路由列表（状态、出口通 / 总、客户端、CPU、内存、WAN） | `fleetctl --json routers [--team 团队2]` |
| 一台软路由的详情：系统、接口、OpenClash、每个出口对应的 fleet 入口与参数是否一致、延迟、连接、客户端 | `fleetctl --json router show <路由>`（`<路由>` 写 id、id 前缀或名称） |
| 软路由出口历史：每个出口测了几次、失败几次、成功率、连续两轮失败几次、延迟中位 / 最大、当前连接 | `fleetctl --json router exits <路由> [--range 1h\|6h\|24h\|7d]`（默认 24h；口径见文末速查。客户说「某个出口时好时坏」先看它，不要去路由上翻日志） |
| 供应商（缩写、账单时区、流量计量与重置、提前提醒、套餐数） | `fleetctl --json provider ls` |
| 套餐（供应商、规格、价格 / 周期、在用机器数） | `fleetctl --json plan ls [--provider <供应商>] [--all 含已归档]` |
| 三网 PING：线路机往外到各目标（默认武汉电信 / 联通 / 移动）的延迟与丢包 | `fleetctl --json ping <线路机>`（`current[]`：`name target mode cur_ms cur_loss`，`cur_loss` 是 %；`show <线路机>` 的 `ping` 字段相同）；全局目标 `fleetctl --json ping-targets` |
| 探测点与探测顺序：谁在测线路机的客户入口、此刻用的是哪一路（谁在代测）、各探测点到各线路机的近 1 小时平均延迟 | `fleetctl --json probe ls`（`{probe_points:[…（rank 是在探测顺序里排第几）], source:{order:[{id,name,label}], primary_name, central_enabled, effective:{state,label,substitute}}}`；`state`：online / blind（在线但探测无效）/ offline / pending / disabled） |

访问规则的健康提示：节点被设为 `blocked` 时健康原因里有 `policy_blocked`（info，不降级、不开告警）。

访问监控只有线路机开了访问日志、且有真实客户端流量时才有数据；`new` 是窗口内第一次出现的目标（旧系统的「新域名」），不是告警。
访问记录只留用户设的天数（`settings retention`：原始记录默认 7 天、汇总默认 31 天），到期中控自动删除：`access` / `sources` 的 `--window` 不能比汇总的保留期长（超了报错 400，换短一点的窗口，不要去改保留期）；`events` 只有原始记录保留期内的；「新目标」「新运营商 / 新城市」指保留期内没见过的。

排查顺序：`status` 找到告警 → `show <机器>` 读 `health.reasons` → `logs <机器>` 读服务日志 → 仍不明再 `exec`。

作业「出口验收」一直报「超时 12 秒」（或「线路机 X 连不上节点 SS 端口」）：先查节点的主机防火墙——供应商镜像自带的 ufw / firewalld 常只放行旧线路机。收敛（上架、`ss_refresh`、换线路机）会在 ufw / firewalld 运行时自动给线路机 IP 放行 SS 端口（ufw 规则注释 `fleet-ss`），作业日志有「主机防火墙」一行；有 warn 说没改成或 iptables 默认丢弃时，`fleetctl exec <节点> -- 'ufw status; iptables -S INPUT'` 看，手动放行线路机 IP 后 `retry`；不要 `ufw disable`。云厂商安全组中控管不到，告诉用户去供应商面板放行。

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

## 软路由

客户现场的 OpenWrt / iStoreOS 路由（OpenClash，mihomo 内核）经一条反向 SSH 隧道连中控（路由上的 `fleet-tunnel` 服务，与原有的 router-tunnel 并存），中控每 30 秒经隧道只读采集一次（隧道连上、断开时马上采一次）、每 5 分钟另起一次测全部出口的延迟（fleet 出口先测，每批 6 个出口并发，没通的 fleet 出口当场单独重测一次，重测通了不算不通；连续两轮不通才算不可用、开 `router_exits_down`；不通的出口之后每轮采集单独重测，连续两次测通就恢复；延迟测试没做成时 `detail.openclash.delay_error` 写原因，出口沿用上一次，不算不通）。接入只新增（隧道密钥、中控登录公钥、`/etc/init.d/fleet-tunnel`），不改路由的 root 密码、dropbear / LuCI 监听、OpenClash 配置与密钥、防火墙；用户明确要求过这些都不要动，AI 也不要去改。

- 状态 `state`：`pending`（还没接入，或刚重新生成接入命令、等在路由上执行）/ `online` / `offline`（接入过、此刻隧道连不上，`error` 是原因）。`online` 而 `error` 不为空 = 连得上但采集出错（脚本出错、输出读不了、超时），快照是上一次成功采集的，不是离线。
- OpenClash 的配置读不了（`detail.openclash.config_error`）时，`config_stale: true` 表示出口与 fleet 入口的对应沿用上次读到的。这时替换 / 换线路机 / 搬空的预览会提醒，执行时还读不了作业就失败、一台路由都不改（不会静默跳过）；用户确认那台路由没用这个节点，才重新发起加 `--no-routers`。`deploy` 是最近一次一键部署：`state` running / failed / done，`step` 正在做的那一步，`error` 失败原因。
- 出口的 `sync`：`ok` 参数和对应的 fleet 入口一致；`drift` 对上了入口但参数不一样（入口轮换过凭据、换了线路机后路由的配置没同步，或入口已删除）；`unmanaged` 不是 fleet 的入口。`drift` 持续两轮采集开告警 `router_exit_drift`；对上的入口还在用的，用 `fleetctl router sync <路由> [--exit <出口>]` 改成入口现在的参数（会重启 OpenClash、断开经它的全部连接：先不带 `--yes` 看预览、告诉用户，同意后 `--yes --follow`）；入口已删除 / 停用的同步不了（预览里 `skipped`），要换入口用 `repoint`。不要手改路由的配置。
- 告警（对象是路由：`subject.kind` 为 `router`，不能 `show`，用 `router show`）：`router_offline`（接入过、隧道连不上满 1 分钟，critical，中控停机的时段不算，连上即恢复；重新接入后 30 分钟还没连上也开，warning，摘要「重新接入后还没连上」；重新生成接入命令 / 重新部署时开着的离线告警直接关掉、不发恢复）、`router_collect_error`（「软路由读取出错」：连得上但采集出错满 60 秒，摘要「连得上，但没读到状态：…」，warning：多半是路由固件 / ucode 不兼容或路由太忙，转告用户，不是网络问题）、`router_openclash_down`（OpenClash 启用了但核心没在跑，critical）、`router_exits_down`（有 fleet 出口连续两次延迟测试不通，warning，摘要「N 个出口连续两次延迟测试不通：…」；全部 fleet 出口不通 critical，摘要「对应面板入口的 N 个出口都…」；客户自己的出口 `unmanaged` 不算；摘要末尾「（其中 2 个指向的团队2-03 正在维护，1 个指向的线路机2 正在告警）」说明指向的机器正在维护 / 告警，先看那台机器的告警，不要去查路由；不通的出口指向的都在维护中时不开）、`router_wan_down`（WAN 口断开，critical）、`router_exit_drift`（「软路由出口参数不一致」，warning，持续两轮采集才开，摘要「N 个出口的参数和对应的面板入口不一致：节点01（团队2-03 · 线路机2:20003，凭据和入口现在的不一样）」，见上一条）、`router_resource_high`（CPU 持续 10 分钟 > 90%、内存 / 存储 > 90%、连接表 > 80%，warning；阈值与机器不同是有意的：路由 overlay 小、CPU 尖峰多）。可以给单台路由开维护窗口或静默（`silence add --router <路由>`，见「维护窗口与静默」）；全部的静默对它们也生效。客户现场计划断电、换宽带、换光猫前先开维护窗口：期间开着的告警保持、不开新的、不发通知，到期后再判。
- 接入（会改路由，先向用户确认，见下一节的命令）有两种，路由上执行的是同一份脚本：
  - 中控连不到路由（路由在 NAT 后面，三台现场路由都是）：`fleetctl router add <名称> [--team 团队2]` 打印接入命令（30 分钟、只能用一次），让用户在路由上用 root 执行；过期了 `fleetctl router bootstrap <路由> --yes` 重新生成（现在的隧道密钥立刻作废，在线的路由马上掉线）。
  - 中控能直接连到路由的 SSH（公网映射、同一网段）：一键部署 `fleetctl router add <名称> --host <IP> [--port 22] [--user root] --password-stdin --yes --follow`（密码从标准输入读一行，不要写在参数里）；路由上已经授权过中控公钥时不用密码。已有的路由重新部署用 `router deploy`；上传密钥、启动隧道这两步失败时中控把原来的隧道密钥与端口写回，在线的路由不掉线（错误里写「已改回原来的隧道密钥与端口」）。失败原因原样转告用户；中控连不到它就改用接入命令。
- 删路由（`router rm --yes`）只撤销中控这边（隧道公钥、状态与指标）；输出里的 `cleanup` 是要在路由上执行的清理命令，转给用户。还有没做完的作业改过它的出口、没确认改回（「需要人工处理」）时删不了，报错里有作业 id：先把作业处理完；路由确实回不来了（扔了、刷机）才用 `router rm <路由> --force --yes`（先向用户确认：作业不再改回它，它上面被改过的出口要自己处理），之后作业能重试收尾或取消。
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
fleetctl silence add <机器> --rule agent_offline --for 30m --comment "重启 agent"                    # 只静默一条规则（规则 key 见文末；不能加 --maintenance）
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
6. 只有这一台和套餐不一样（调价、换周期、改提醒天数、自动续费、流量上限）时：`fleetctl subscription set <机器> [--price 45|none] [--currency USD|none] [--cycle monthly|quarterly|semiannual|yearly|none] [--due YYYY-MM-DD] [--auto-renew true|false] [--remind-days 7|none] [--tz Asia/Shanghai|none] [--traffic-gb 2000|0|none] [--reset-day 15|none] --yes`。给了值 = 这台单独设置，给 `none` = 改回跟随；和模板相同的值中控也按跟随存。只改给了的字段（命令先读现有订阅再整份提交），确认语逐项列出「旧 → 新」（带来源），没有变化不提交；没有订阅时新建只要 `--due`（没有套餐的机器还要 `--price`）。确认前先调中控的预览接口（`POST /machines/{id}/subscription/preview`，不写库）；报「中控版本较旧，先升级中控再用 subscription set」是中控还没有这个接口，什么都没提交，先升级中控再重试，不要绕过去直接 PUT。
   - 续费用 `renew`（留续费记录）；`subscription set --due` 只用来改正填错的到期日。
   - `--auto-renew true`：到期后中控按周期自动顺延到期日、记一条续费记录（操作人「系统」），到期前不提醒，到期日次日还没顺延才算过期；不会扣款，只是记账。
   - `--traffic-gb` 是这台单独设的流量上限（供应商的十进制 GB），`0` = 不限，`none` = 跟随套餐；`--reset-day` 是流量每月几号重置，`none` = 跟随供应商（按账单日 = 到期日几号，按自然月 = 1 号，或滚动 30 天）。
   - 本周期已用按 agent 选中的网卡计量（`billing` 写「按 ens3 网卡」，`show` 写「按 ens3 计」）、规则跟供应商（取小 / 取大 / 相加 / 只算出站；家宽代理和同机 IP 节点没有网卡，按业务流量、下载加上传，不跟供应商的规则）。**不能再按面板手填已用**（`--used` 已删，接口 400）：用户说「面板上已用 X GB」时，先看 `billing`——周期中途上架的写「从 X 开始统计，之前的没算进来」（下个周期自动完整），割接时录的写「接入后实测 A，加上接入前的面板读数 B（…重置后只算实测）」（已用 = A + B，A 是接入后网卡实测；和 `status` 的「本月业务流量」对不上多半是这个口径：本月业务流量只有接入后的、中途接入的带「（10-22 起）」，且是业务流量不是网卡）；数差得多就核对计量网卡与供应商的「流量重置」「流量计量」口径（见 OPS.md 流量额度一节），告诉用户原因，不要另想办法改数。
   - 不限流量的机器 `billing` 只有「本月流量：流量不限 · 本月 网卡 ↓ X / ↑ Y」（有 agent 按网卡下行 / 上行，家宽代理、同机 IP 节点按业务下载 / 上传）；有额度的才有「本周期已用」（滚动 30 天写「近 30 天已用」）。`show` 有「本月流量」一行（同样的写法，网卡的后面写「按 ens3 计」）。
   - 同机 IP 节点默认不单独记费用（记在宿主机），供应商对附加 IP 另收费时才给它设订阅（它没有套餐，要给 `--price`）。
7. 备用团队（只有一个；`fleetctl --json team ls` 的 `spare`；机器视图 / `billing --json` 的 `spare`、`spare_from`、`spare_since`、`recycle_on`，`billing` / `show` 有「备用」一行）：退役的、替换下来的机器都在这里，**在用、有订阅的过期满 1 天（`recycle_on` 那天 0 点起，账单时区）后中控自动清理并移进回收站**，审计操作者「系统」，不用 AI 去做。备用机器的续费提醒会写「明天 / 后天自动回收」并列出还指向它的软路由出口：用户要留就 `renew`（到期日顺延就不回收），或移出备用团队（`rename --team`，等于撤销退役）；客户还在用那个出口的，回收前先 `replace`。备用团队不能新建、取消、删除，只能改名（`fleetctl team set <团队> --name <新名> --yes`）。

## 会改变状态的操作：先向用户确认

下面这些先把对象和后果告诉用户，得到明确同意后再执行，并加 `--yes`（非交互环境不加 `--yes` 会被拒绝）。

```
fleetctl retire <机器> [--reason 原因] --yes               # 退役 = 移进备用团队：不停服务、不建作业，到期满 1 天中控自动回收；原团队编号前移，宿主机带上同机 IP 节点。先打印预览（去向与回收日、同机 IP 节点、还指向它的软路由出口、改名），确认前告诉用户。撤销 = rename --team 移出备用
fleetctl retire <机器> --now [--reason 原因] [--no-cleanup] --yes [--follow]   # 立即回收：摘入口、停服务、撤令牌，完成时进回收站（线路机、没有订阅、已过期满 1 天、没上线、本来就在备用里的只能这样）；宿主机的同机 IP 节点一并回收
fleetctl recycle ls [-q 关键词] [--source legacy]           # 回收站（只读）；show <id|名字|IP> 看一条（--json 带快照）。物理删除由中控自动做，没有手动删除
fleetctl recycle import --file legacy.json [--yes]         # 旧面板的退役机器导入回收站：不加 --yes 只预演，把会导入 / 跳过的名单给用户确认后再加 --yes
fleetctl entry rotate <入口> --yes                         # 旧分享链接立即失效
fleetctl entry disable <入口> --yes
fleetctl entry add-direct <线路机> --yes [--follow]          # 线路机直出入口：从线路机自己的 IP 出去（旧系统的「节点00-默认」），每台至多一条；链接 fleetctl share <线路机>-直出
fleetctl upgrade-agent <机器> | --all --yes                # 重启 agent；--all 先升一台金丝雀，成功后线路机逐台、VPS 节点并发；排队中的可 fleetctl cancel <作业>
fleetctl tune <机器>[,<机器>…] | --team <团队> | --all [--yes] [--follow]   # 重做系统调优（上架时已自动做；档 tune-v2：TCP 缓冲上限按套餐带宽、客户连接 RTT、内存推导，任何参数不往小改）：内核网络参数、BBR、日志体积上限、时间同步，不重启 ss/Xray、不断连接；--team/--all 先做一台（线路机优先）并跟到结束，成功后其余逐台提交，有进行中作业的跳过并列出；--all 要 --yes；同机 IP 节点、家宽代理 409；`ls` 的「调优」列看谁是旧档（tune-v1）/ 未调优，`show` 的「调优推导」看缓冲上限怎么来的
fleetctl tune <机器>[,<机器>…] --rollback --yes [--follow]   # 撤销调优：按机器上第一次调优前记的原值写回、删掉调优的 sysctl 配置（没有原值快照的机器作业失败，什么都不改）；先向用户确认
fleetctl ss-refresh <机器>[,<机器>…] | --team <团队> | --all [--yes] [--follow]   # 重新下发节点的 SS 配置：升级中控后把新的 ssserver 配置（如出站目标黑名单 fleet.acl）推到存量节点；配置有变化才重启 ssserver（该节点连接断开几秒），同机 IP 节点随宿主机一起刷，做完经入口验收出口（失败不自动回滚）；--team/--all 只含在用的 VPS 节点，先做一台并跟到结束，成功后其余逐台提交，有进行中作业的跳过并列出；--all 要 --yes；线路机、家宽代理、同机 IP 节点单独下发、没有入口的节点 409
fleetctl renumber <团队> --yes                             # 不加 --yes 只预览
fleetctl move <节点> --to <线路机> --yes [--from <线路机>] [--no-routers] [--follow]   # 换线路机：新入口验收后改软路由出口（同替换，失败全部换回、旧入口不摘）、再摘旧入口，不断网；入口线路机和端口都会变：用订阅链接的客户端自动更新，用分享链接的要换（完成后 fleetctl share <节点> 取新链接）。不加 --yes 先看预览
fleetctl move <节点A>,<节点B>,<节点C> --to <线路机> --yes [--from <线路机>] [--follow]   # 多个节点换线一律用这一条批量命令（逗号分隔），不要逐个 move：同搬空一起改软路由，每台路由只改一次、OpenClash 只重启一次（逐个 move 每个都重启一次、客户断线一次）。有一个不能换就一个都不提交；失败 / 重试 / 取消同搬空
fleetctl evacuate <线路机> --to <线路机> --yes [--no-routers] [--follow]          # 搬空线路机（退役前）：每个在用节点一个换线路机作业，软路由出口跟着改；作业并行建新入口，都验收完后由一个「带头」的一起改，每台路由只改一次、重启一次（任何一台失败全部换回、旧入口都不摘，同批一起失败，修好后都重试）。预览写「软路由 X 的 N 个节点的出口一起改，OpenClash 重启 1 次（断开它上面全部连接）」，确认前告诉用户
fleetctl rebalance <线路机> [--target <线路机>] [--exclude <节点>]…   # 搬迁建议（只读）：按近 7 天的速度重置前会用满的线路机，挑出搬走后能撑到重置日的最少入口和目标线路机（默认额度余量最多、在用、非故障），打印一条可直接执行的批量 fleetctl move A,B,C --to X 命令（照抄执行，不要拆成逐个 move）；--exclude 的节点不动（分享给别人的入口所在节点）。billing / show 的「预计用满」一行同源
fleetctl replace <旧节点> --with <新节点> [--to <团队>] [--number N] [--no-routers] --yes [--follow]   # 替换节点：不加 --yes 只预览（见下）
fleetctl router switch <路由> <组> <出口> [--for 4h]   # 临时切换出口：select 组经 mihomo API 切到组里已有的另一个出口，不改配置、不重启 OpenClash、不断已有连接；--for 到时自动切回（30m / 4h / 1d，最长 7 天）
fleetctl router switch <路由> <组> --back               # 切回第一次切之前的出口
fleetctl router sync <路由> [--exit <出口>]… --yes [--follow]   # 同步出口参数：参数和对应入口不一致的出口改成入口现在的凭据（server:port 不变；备份、重启 OpenClash、测延迟，失败自动改回）。不加 --yes 只预览
fleetctl repoint <旧入口> --to <新入口> [--router <路由>]… --yes [--follow]   # 改出口指向：软路由上指向旧入口的出口改成新入口（同替换的改路由：备份、重启 OpenClash、测延迟，失败全部换回），两个入口都不动、不改编号。不加 --yes 只预览
fleetctl provision ...                                     # 先 check，再确认，再提交
fleetctl retry <作业> [--reset-host-key] [--password-env VAR]   # 重试失败作业；只有作业因主机指纹不符失败、且确认机器重装过才加 --reset-host-key（同机 IP 节点不行：对宿主机升级 agent 后重试那个作业）；在用机器指纹变了也走这条：先 upgrade-agent 让它失败再重试；报「没有可用的 root 密码」时用 --password-env 补密码
fleetctl renew <机器> [--cycles N | --to YYYY-MM-DD] [--amount 39.46] [--note …] --yes   # 登记续费：用户确认付过款之后（见「到期与续费」）
fleetctl renew <机器,机器,…> | --provider <供应商> --due <Nd|日期> [--include-spare] [--ref 账单号] [--yes]   # 批量登记续费：不加 --yes 只预览；--yes 一个事务、一条审计
fleetctl billing --due <Nd|日期> [--by-provider] [--include-spare]   # 未来待付清单：按 供应商 × 到期日 分组，原币种小计与 USD 合计
fleetctl subscription set <机器> --price 45 … --yes          # 改订阅：只改给了的字段；值 = 单独设置，none = 改回跟随套餐 / 供应商
fleetctl settings retention [--raw 7] [--hourly 31] --yes   # 访问记录保留期（各 1–90 天，汇总不短于原始记录；只给一个时另一个不变）：调短后超出的访问记录几分钟内删掉、找不回来，只在用户明确要求时改
fleetctl settings fx CNY --per-usd 7.12 --yes   # 手动设置汇率（或 --usd 0.1404 = 1 CNY 合多少 USD）：之后每天的自动更新不再改它；--auto 改回自动的值；只在用户给了汇率或明确要求时改
fleetctl settings fx --refresh                  # 立即从汇率源（ECB）更新一次；失败保留上次的值（报错 502，原因在 last_error）
fleetctl router add <名称> --host <IP> --password-stdin --yes --follow   # 软路由一键部署：中控登录路由装上 fleet-tunnel（只新增）；不给 --host 只建记录、打印接入命令（不改路由，不用确认）
fleetctl router deploy <路由> --host <IP> [--password-stdin] --yes --follow   # 重新部署：换一把隧道密钥
fleetctl router bootstrap <路由> --yes                      # 重新生成接入命令：现在的隧道立刻作废，要在路由上执行新命令才连得回来
fleetctl router rm <路由> --yes                             # 删软路由：撤销隧道公钥、删掉状态与指标；路由上的清理命令在输出里，转给用户
fleetctl provider add <名称> --code ATT [--tz Asia/Shanghai] [--panel-url https://…] [--traffic-rule min|max|sum|out] [--traffic-reset month|billing|rolling30] [--remind-days 7]   # 新建供应商（不用 --yes）；不给的用默认：跟随组织时区、流量取大、自然月、提前 7 天提醒
fleetctl provider set <供应商> [--name …] [--code …] [--tz …|none] [--panel-url …|none] [--traffic-rule …] [--traffic-reset …] [--remind-days N] --yes   # 只改给了的项，确认语列出「旧 → 新」；时区、计量、重置、提醒立即对名下没单独设置的机器生效
fleetctl provider rm <供应商> --yes                         # 还有套餐（含归档的）或机器（含退役的）时中控拒绝
fleetctl plan add <名称> --provider <供应商> --price 299.99 --currency CNY --cycle monthly [--cpu 2 --mem-mb 2048 --disk-gb 40 --bandwidth-mbps 1000 --traffic-gb 2000] [--notes …]   # 新建套餐（不用 --yes）；价格写主单位（同 provision 的 --price），--traffic-gb 0 = 不限
fleetctl plan set <套餐> [--name …] [--price …] [--currency …] [--cycle …] [--cpu N|none …] [--traffic-gb N|none] [--notes …] [--provider <供应商>] [--archive|--unarchive] --yes   # 只改给了的项；价格、币种、周期、流量上限立即对用它、没单独设置的机器生效；不再上架的 --archive（用着的机器不受影响）
fleetctl plan rm <套餐> --yes                               # 还有机器（含退役的）用着时中控拒绝，改用 --archive
```

- 供应商、套餐：`<供应商>` 写 id、id 前缀、名称或缩写；`<套餐>` 写 id、id 前缀或名称，不同供应商有同名套餐时写 `供应商/套餐`（如 `GOMAMI/Mini`）。匹配到多个时报错并列出候选，照候选重试，不要猜。新建只是加一条目录，不碰机器，告诉用户建了什么即可；`set` 改价、改额度、改时区会立即影响已有机器的订阅（续费提醒、流量周期、`billing` 的价格），`rm` 删目录项，都先向用户确认。

- 上架前先确认供应商、套餐存在：`fleetctl --json provider ls`、`fleetctl --json plan ls --provider <供应商>`。不存在就先建——`fleetctl provider add <名称> --code <缩写>`、`fleetctl plan add <名称> --provider <供应商> --price 299.99 --currency CNY --cycle monthly [--traffic-gb 2000]`（名称、缩写、价格、币种、周期、流量额度问用户或照供应商的账单填，不要编）；已归档的套餐要用就 `plan set <套餐> --unarchive --yes`。不要手拼 curl 调 `/providers`、`/plans`，也不要 psql 去插。
- 上架：`fleetctl --json provision --file batch.json --dry-run` 只校验；通过后让用户确认名单，再去掉 `--dry-run` 加 `--yes --follow`。单台参数：`--kind vps --ip 1.2.3.4 --team 团队1 --relay 线路机2 --provider ATTVPS --plan 基础版 --due 2026-12-01`。选了套餐必须给 `--due`（到期日；价格、流量等跟随套餐，时区、提醒跟随供应商，`--provider` 可不给、取套餐的）；只有这台和套餐不同时才加 `--price 45` / `--currency` / `--cycle` / `--tz` / `--traffic-gb` / `--reset-day`（单独设置）。没有套餐时给了 `--due` 就要给 `--price`。root 密码放环境变量，用 `--ssh-password-env VAR` 传名字。`--ss-port`（批量文件里 `ss_port`）是线路机连节点用的公网（NAT 映射）端口，节点上 ssserver 固定监听 8388；非 NAT 机不要填。价格一律用 `--price 39.46`（主单位，同 `subscription set`）。
- 上架的登录试探：`--dry-run` 在 SSH 端口通了之后会用中控密钥试登录一次（不记主机指纹）。报「中控密钥登录不了 …（密钥被拒），也没填 root 密码」时，**不要猜、不要自己去机器上改**：把报错末尾那行中控公钥（`ssh-ed25519 …`，即中控的 `/etc/fleet/ssh/id_ed25519.pub`）原样给用户，让他录进供应商面板的 SSH 密钥（或机器 root 的 `~/.ssh/authorized_keys`）后再 `--dry-run`；或者向用户要 root 密码，用 `--ssh-password-env VAR` 传。用户说「面板里已经录过密钥了」却仍报这条，多半录的不是中控这把，照样把公钥给他核对。结果里的 `warnings`（如「系统是 Ubuntu 20.04，已过标准支持期」）不拦上架：转告用户，由他决定是先重装还是照常上架。端口不通（「SSH …:22 连不上」）通常是系统还没装好，等一会儿再查。作业第一步报「中控密钥登录不了，也没有可用的 root 密码」同理：录入报错里的公钥后 `fleetctl retry <作业>`，或 `--password-env` 补密码。
- 同机 IP 节点（已有 VPS 节点上多出来的 IP 当一台独立节点，按 IP 计节点）：`fleetctl provision --kind vps --host <宿主机> --ip <新 IP> --team 团队1 --relay 线路机2 --yes --follow`（批量文件条目写 `"host": "<宿主机>"`）。宿主机要是在用的普通 VPS 节点；IP 要已直接配在宿主机网卡上。不填 SSH 端口 / 密码 / SS 端口（SSH 与 agent 用宿主机的，SS 端口中控从 8389 起分配），供应商取宿主机的、费用默认不记。作业 `provision_ip` 第一步「在宿主机上核对 IP」：报「不在宿主机网卡上」或「中控看到的来源是 …」时是机器网络配置问题，让用户去供应商面板 / 机器上配好再 `fleetctl retry`。
- 升级 agent：新版本起不来、2 分钟内没上报，或线路机上新版本报的 Xray 不正常（没在跑、配置没应用到升级开始时的代数、有应用错误；升级前就这样的项不查），作业自动换回旧版本后失败（错误「…，已回滚到 <旧版本>」，线路机的写「线路机检查没通过（…）」：机器照常上报，把错误告诉用户即可）；错误里写「需要人工处理」时 agent 可能没在运行，按 `docs/OPS.md` §7 处理。`--all` 的金丝雀或任何一台线路机换上新版本后出问题，没开始的作业都取消（`error` 写明是哪台），不要逐台去重试或再 `--all`，先查失败的原因；推送之前就失败的（SSH 连不上、登录被拒）只算那一台，后面的照常继续，修好那台后 `retry` 它的作业。用户要停下 `--all`：排队中的作业 `fleetctl cancel <作业>`，等它的跟着取消；进行中的不能取消。错误是「中控上的 agent 在升级过程中换了版本…」时是中途重新部署过中控，问过用户后重新 `--all`。失败的升级作业 `retry` 从推送重来。
- 替换节点（用户说「把 团队2-01 换成新机器」「替换下来的放备用」）：**一律用 `fleetctl replace`，不要手改软路由的 OpenClash 配置、不要 `fleetctl ssh router` 去改、不要直接写数据库，也不要用 rename + retire 拼**。新节点先上架（同团队拿到临时编号、入口生效）；`fleetctl replace 团队2-01 --with 团队2-20` 先看预览（新节点 → 旧编号、旧节点 → 备用团队的哪个号、要改哪些软路由的哪些出口、编号压缩、阻断原因），告诉用户，同意后加 `--yes --follow`。作业先改软路由（同团队几台路由都改；备份 → 只改那个出口的 server/port/uuid/servername/public-key/short-id → 重启 OpenClash → 测延迟，任何一台失败全部换回原配置），再交换编号（同 rename，旧节点进备用团队，同退役的默认去向，到期满 1 天由中控自动回收）。阻断里有「软路由「X」现在离线」：等路由在线（`fleetctl routers`）或用户同意只换编号（`--no-routers`，路由上的出口就还指向旧节点）。旧节点默认去备用团队，用户要放别处再加 `--to`。作业失败：把 `fleetctl job <id>` 的错误告诉用户（改路由失败时路由都已换回、编号没动；写「需要人工处理」的是回滚也失败了，备份路径在错误里），处理后 `fleetctl retry <id>`；开始改路由之后不能取消。
- 节点被打、临时让客户走备用出口、过几小时切回（用户说「节点11 被打了，客户先切到备用」）：先看 `fleetctl --json router show <路由>` 的 `detail.groups`（哪个 select 组现在选着它、组里还有哪些出口），用 `fleetctl router switch <路由> <组> <备用出口> --for 4h`：只改 mihomo 当前的选择，不重启 OpenClash、不断已有连接，到时自动切回（不确定多久就不带 `--for`，恢复后 `--back`）。只能选组里已有的出口；url-test / fallback 组中控会拒绝（由 mihomo 自己选），这时才用下面的 repoint（会重启 OpenClash）。OpenClash 重启后 mihomo 一般会保留当前选择（实测：选择记在缓存里），到时照样由中控切回；只有组的选择被改成别的（有人手动改、换了配置）时记录才结束（审计「临时切换被路由改回」）。`fleetctl --json incidents` 里 `egress_fail` / `router_exits_down` 的 `suggested` 已经算好了切到哪个出口，照着执行前告诉用户。
- 改出口指向（用户说「把软路由的直连 / 默认出口换成线路机3」）：`fleetctl repoint 线路机1-直出 --to 线路机3-直出 [--router 团队2-老办公室]`。入口写入口 id、`线路机名:端口` 或节点显示名（直出入口是「线路机名-直出」）；目标线路机还没有直出入口就先 `fleetctl entry add-direct <线路机> --yes --follow`。旧入口照常在用（别的客户端、个人订阅还能用它），不要的话另行停用。不要手改路由的 OpenClash 配置、不要用 router-swap 一类脚本。
- 改名：`fleetctl rename <机器> --team 团队2 --number 3`。
- 国家（名字旁的国旗）：默认按 IP 查离线库自动定；查错了或想固定就 `fleetctl rename <机器> --country JP`（ISO 3166-1 两位代码，英国是 GB），`--country auto` 改回自动；上架时也可以 `fleetctl provision … --country US`。
- 分享链接 `fleetctl share <入口>` 需要 `secrets` scope，会记审计；链接是凭据，只给用户本人，不写进文件或日志。一批用 `fleetctl share --team <团队>` 或 `--machines a,b,c`（一次请求、一条审计，每行「名称 链接」，`--json` 给数组），不要逐个 `share`。给用户自己的客户端优先用订阅链接（`fleetctl sub`，换线路后自动更新），给别人才用 `share`。
- 客户端订阅（用户自己的 Clash Verge、OpenClash、v2rayN 一次拿全部入口）：`fleetctl sub ls|show <订阅>`；新建 `fleetctl sub add <名称> [--only-teams …|--except-teams …] [--no-direct] [--include <机器,…>] [--exclude <机器,…>]`（默认全部团队、不含备用团队、含线路机直出），改规则 `sub edit`，`sub rotate --yes` 旧链接立即失效，`sub rm --yes`。要 `secrets`；链接只在 add / rotate 输出一次，是凭据，只给用户本人。换节点（replace）、换线路机（move）、轮换凭据后用订阅的客户端会自动更新，不用再手改客户端配置。不要替用户把他现有的客户端改成订阅，那由用户决定。
- 密钥管理（`fleetctl keys ...`）只认登录会话，AI 不做；需要新密钥时请用户在网页「设置 → API 密钥」创建。

## --json 字段速查

- `ls`（每台一个精简对象）：`id name kind(relay|vps|proxy) lifecycle(active|provisioning|retiring|retired|failed) health(ok|degraded|down|unknown|maintenance|inactive) ip agent_online cpu_pct mem_pct disk_pct entries(入口数) open_incidents active_job{id,kind,state}`；百分比是整数，取不到为 null；家宽代理没有 agent，`agent_online` 为 null。`--due N` 时每项另有 `due_date due_days auto_renew`（`due_days` 按机器的账单时区算，负数是已过期几天）与 `fee{monthly_cents currency monthly_usd_cents usd_per_unit}`（月费的原币种与折成的 USD，没有汇率时 USD 两项为 null；没有订阅时 `fee` 为 null），表格多一列「月费」（USD，括注原价）。
- `show`（MachineView）：上面这些的完整版（`health.status`、`agent.online`、`host.{cpu_pct,mem_used,mem_total,disk_used,disk_total,load1}`、`entries[]` 完整入口视图），另有 `ssh_port revision`、`health.reasons[{key,rule_name,severity,text}]`（`rule_name` 是规则中文名，同通知）、`agent.health`（agent 自报：离线队列、访问日志解析不了 / 丢弃的行数等）、`failed_job`。
  同机关系：`host_machine{id,name}` 非 null 的是同机 IP 节点（它的 `agent`、`host` 都是宿主机的，agent 离线 / 高负载告警在宿主机名下；`logs --unit ssserver` 看它自己的实例 `fleet-ssserver@<端口>`，`exec` 在宿主机上执行；不能单独 `upgrade-agent`）；`ip_nodes[{id,name}]` 是宿主机上未退役的同机 IP 节点。
- `entries`（每个入口一个精简对象）：`id relay port node lifecycle(active|pending|disabled|removing|removed) applied apply_error tcp_ok egress_ok today_down today_up`（今日业务流量，字节）；`tcp_ok` / `egress_ok` 为 null 表示还没探测过。`node` 是「线路机名-直出」的是线路机直出入口：没有节点，`tcp_ok` 恒为 null；它出口不通时告警对象是线路机（`egress_fail`）；搬空不搬它，退役线路机时自动摘除。
- `incidents`（每条一个精简对象，`status` 里的 `incidents` 相同）：`id severity(warning|critical) rule rule_name subject_id subject_name summary reopen_count state(firing|resolved) opened_at resolved_at ack_by suggested`（`suggested`：中控按规则给的、可以直接执行的 fleetctl 命令，只有进行中的这几条有，其余为 `[]`：egress_fail / router_exits_down → `router switch … --for 4h`（把选着坏出口的 select 组切到组里能用的出口）、router_exit_drift → `router sync`、线路机的 traffic_quota_high → `rebalance`、agent_offline → `logs`；会改东西的照样先告诉用户；`ack_by` 非 null 即已确认；恢复后 30 分钟内复发会重开同一条并 +1，表格里显示「反复 N 次」；大于 0 说明在抖，查探测/网络而不是只看当前状态。机器、软路由、探测点、中控的告警都这样；严重度升级时确认会被清掉）。规则 key：agent_offline、clock_skew、no_entry、egress_fail、tcp_fail、renewal_due（过期后升为 critical；备用团队的机器只在还有软路由出口指向它时才报；旧记录里还有 renewal_overdue）、traffic_quota_high（流量额度：本周期已用 ≥ 90% warning、≥ 100% critical，不改健康状态，新周期或调高上限后自动恢复）、relay_apply_lag、relay_apply_error、relay_xray_down、relay_unreachable（主探测来源——中控或探测点——连续 3 轮连不上线路机的客户入口，或同一个入口端口最近 3 次都连不上，健康状态就变故障，告警再持续 1 分钟才开（整台不通约 5 轮、2.5 分钟），critical，摘要「…连续 3 次以上连不上客户入口」，开头写是谁测的、括号里有端口与原因）、host_cpu_high、host_mem_high、host_disk_high、ipintel_new_isp（来源出现新运营商）、ipintel_new_city（来源出现新城市）、ipintel_datacenter_source（机房 / VPN 来源 IP），这三个是 warning、学习期 7 天后才有、最后一次出现 24 小时后自动恢复，不用手动处理。中控自身的告警（对象是「中控」：`subject_id` 为 null、`subject_name` 为「中控」，不能 `show`）：central_channel_failing（通知渠道连续发送失败满 5 分钟，critical，摘要里有渠道名与原因；请用户在网页「设置 → 通知渠道」改配置并点「测试」，测试成功即恢复）、central_backup_stale（中控每日备份超过 26 小时没有新的，warning；`fleetctl server-logs` 帮不上，需要用户在中控上 `systemctl status fleet-backup`）、central_disk_low（中控本机磁盘可用低于 10%，critical，写满了中控会停；告诉用户尽快清理）、central_ingress（有 agent 的机器过半且至少 3 台同时失联，critical：边缘机 / Caddy / DNS 出问题，agent 到不了中控；期间不按台开 agent_offline）、central_view_stuck（机器视图——列表、总览、实时推送、告警评估共用的那份——连续 2 分钟算不出来，critical：期间机器告警停止评估、列表可能停在旧数据或报 503，`fleetctl status` / `ls` 也可能失败；多半是数据库有锁排队或慢查询，转告用户在中控上查数据库的锁与慢查询（`docs/OPS.md` §12 有查询语句），算成一次就自动恢复；AI 不要去 `show` 机器或逐台排查）、central_tunnel_host（「软路由回连地址不是中控」，warning：软路由隧道回连的主机——`[routers] ssh_host`，没配时 public_url 的主机名——解析到的不是中控（经 CDN / 只转发 80/443 的边缘机，或解析不了），摘要写解析到哪；现在连着的隧道不受影响，路由断线后就回连不上。转告用户把 fleet.toml 的 `[routers] ssh_host` 配成中控源站 IP 或一个直指源站的灰云域名，重启中控后给各路由重新生成接入命令或重新部署（`docs/OPS.md` §11）；AI 不要去改配置、不要去动路由）。这几条都要转告用户，AI 处理不了。探测点离线是 `probe_point_offline`（见「探测点」一节，`subject_name` 是探测点名，不能 `show`）。软路由的告警是 `router_` 开头的七条（见「软路由」一节），`subject_id` 是路由 id、`subject_name` 是路由名，用 `router show` 看，不要 `show`。
- `silence ls`（每条一个精简对象；`silence add` / `silence rm` 输出新建 / 删掉的那一条，同形）：`id subject_kind(machine|router|probe_point) subject_id subject_name rule rule_name starts_at ends_at state(active|scheduled) is_maintenance comment`；`subject_kind` / `subject_id` 为 null = 全部（机器、软路由、探测点），`rule` 为 null = 全部规则；`state` 是 `active`（进行中）或 `scheduled`（已预约，还没开始）。只列没结束的。
- `billing`：`id name host_machine spare subscription sub_template traffic_limit_gb quota{limit_gb,limit_source,used,period_start,period_end,mode,counted_from,rule,base,base_at} quota_pct renewals renewal_count`。`subscription` 为 null 是没有订阅，否则 `{price_cents currency cycle(monthly|quarterly|semiannual|yearly) due_date due_days due_state(ok|soon|overdue) billing_tz auto_renew remind_days traffic_gb traffic_reset_day overrides monthly_cents next_due period_days}`，全是生效值（`due_state` 同续费告警：soon = 提前提醒天数内且不是自动续费；`next_due` = 推一个周期后的到期日，`renew` 不带 `--to` 时由中控算），`overrides` 是单独设置了的字段名（其余跟随 `sub_template`：`{price_cents currency cycle billing_tz remind_days traffic_gb traffic_reset(month|billing|rolling30)}`；`rolling30` 时跟随的 `traffic_reset_day` 为 null）；金额都是分（`price_cents` 3946 = 39.46，币种看 `currency`）。`traffic_gb` 0 / null = 不限，`traffic_limit_gb`（= `quota.limit_gb`）是生效的上限（null = 不限），`quota.limit_source` 是它的来源（`subscription` 单独设置 / `plan` 跟随套餐 / `host` 同机 IP 节点与宿主机共用 / `none`），`quota_pct` 是本周期已用（`quota.used`，字节）占它（GB × 10⁹）的百分比（向下取整，没有上限为 null）；`quota.rule` 是网卡计量规则（`min|max|sum|out`，null = 业务流量，下载加上传）；`quota.mode` 是 `monthly`（`period_end` = 下次重置日）或 `rolling30`（近 30 天，`period_end` 为 null）；`quota.counted_from` 非 null = 周期中途上架、这之前的用量不在 `used` 里；`quota.base_at` 非 null 是割接时录的接入前读数（`base`，字节，只读，下个周期自动失效）。本月网卡流量在 `show --json` 的 `nic{today_rx,today_tx,month_rx,month_tx}`（rx 下行、tx 上行；家宽代理、同机 IP 节点为 null），`month_counted_from` 非 null = 本月中途上架，`agent.nic_ifaces` 是计量的网卡。`renewals` 是最近 10 条续费记录（新→旧）`{id old_due new_due amount_cents note actor created_at}`，`renewal_count` 是总条数。
  计费统一按 USD：`subscription` 里另有 `monthly_usd_cents`（月费折成 USD 的分）、`price_usd_cents`（每期价格折成 USD）、`usd_per_unit`（所用汇率，1 单位合多少 USD；USD 为 1），由中控按汇率算好（`round(原币种分 × usd_per_unit)`，四舍五入到分）；三个都是 null = 没有这个币种的汇率、未折算（不是 0 元，也不是 1:1）。原字段 `price_cents` / `currency` / `monthly_cents` 照旧是供应商收的原币种。文字输出的「费用」行后面写「；折合 37.50 USD/月（1 CNY = 0.125 USD）」或「未折算」。
- `renew`：刚登记的这条续费记录（同 `billing` 的 `renewals[]`）。`subscription set`：改完的 `subscription`（同上，`due_days` 按新到期日算）；没有变化时不提交，输出现在的。
- `settings retention`（改了之后输出改完的，同形）：`{raw_days, hourly_days, min, max, profile_days}`；`raw_days` 是原始访问记录（每一条：时间、来源 IP、目标）留几天，`hourly_days` 是汇总（小时汇总、新目标、来源画像）留几天、也是访问页能看的最长范围，`profile_days` 是来源画像实际留几天（不短于 IP 情报的学习期，只读）。没有变化时不提交，输出现在的。
- `policy ls`：`draft_count teams[{id name mode domains draft revision nodes[{id name mode domains source(node|team|default) draft revision}]}]`；`domains` 是域名个数，`draft` 为 null 或 `{mode,domains}`；节点的 `mode` / `domains` / `source` 是生效值，`draft` / `revision` 是节点自己的。
- `status --json` 的 `overview.failed`：上架失败（lifecycle=failed）的机器数，算在「有问题」里。`show` 的 `failed_job{id,kind,state}`：上架失败或退役失败（退役中）的机器带最近失败的作业，拿它的 id 去 `fleetctl job` / `fleetctl retry`（`ls --status failed` / `ls --status retiring` 找出这些机器）。
- 不加 `--json` 的 `status` / `show` / `entries` / `incidents` 是给人看的：种类、生命周期、健康、严重度、规则、入口状态、作业种类与状态都显示中文；中文名和计费周期都取中控的 `/meta`（网页同一份）；所有命令表格里的时间都是组织时区（中控 fleet.toml 的 `org_timezone`，默认北京时间；`--json` 里是 API 原样的 RFC 3339）；`ls` 的种类 / 生命周期 / 健康列保留原值（就是 `--kind`、`--status` 的取值）。脚本和 AI 只解析 `--json`。
- `status --json` 的 `overview.monthly_usd`：`{cents, by_currency:[{currency,cents,usd_cents,usd_per_unit}], unconverted:[{currency,cents}]}`——每月费用折成 USD 的合计（分，逐台折算后相加）、各原币种的金额与所用汇率；没有汇率的币种不算进 `cents`，单列在 `unconverted`。文字输出「每月费用」写 USD 合计，括注非 USD 币种的原价与汇率。
- `settings fx`：`{base, auto, url, fetched_at, rates_date, last_attempt_at, last_error, missing:[在用但没有汇率的币种], rates:[{currency, usd_per_unit, per_usd, source(base|manual|auto|null), auto_per_usd, manual{usd_per_unit,by,at}, in_use}]}`；`source` 为 null = 没有汇率（费用未折算）。文字输出只列在用和手动设过的币种。
- `status --json` 的 `overview.online` / `offline`：按 agent 连通计（家宽代理没有 agent 算在线），与健康无关。
- `status --json` 的 `overview.traffic_month`：`{up, down, counted_from}`——本月全部入口业务流量合计；`counted_from`（RFC 3339 或 null）非 null 表示在用线路机本月才接入、之前的量不在内，`status` 文字写「（10-22 起）」。
- `job`：`state`（queued|running|waiting|cancelling|succeeded|failed|cancelled）、`error`、`steps[]`、`logs[]`。`kind` 里 `provision_ip` 是上架同机 IP 节点。
- `retire --json`：进备用 `{mode:"spare", name（备用里的名字）, from, recycle_on, ip_nodes:[{id,name,from}]}`；`--now` 为 `{mode:"now", job_id, ip_node_jobs:[{machine_id,name,job_id}]}`，后者是一并回收的同机 IP 节点各自的退役作业。
- `recycle ls --json`：`{items:[{id source(fleet|legacy) source_id machine_id kind name last_name team ip host_name provider plan service_ref price_cents currency cycle activated_at retired_at due_date recycled_at reason traffic_up traffic_down nic_rx nic_tx actor}], total, next_offset}`；`recycle show --json` 另有 `snapshot`。`recycle import --json`：`{dry_run, inserted:[{source_id,name,id}], skipped:[{source_id,name,why}]}`。
- `upgrade-agent --all --json`：`{jobs:[{machine_id,name,kind,job_id,after}], skipped:[{machine_id,name,reason}], order}`；`jobs` 按执行顺序，第一个是金丝雀，`after` 是它要等的作业（等的那个换上新版本后失败、或被取消，它就被取消）；`order` 是顺序说明，原样转述给用户。中控上没有 agent 程序时报错（409），不建作业。
- `tune --json`：`{jobs:[{machine_id,name,job_id}], skipped:[{machine_id,name,reason}]}`（`--team` / `--all` 时金丝雀失败则多一个 `stopped:true`，其余没提交）；逐键结果（`applied ok kept skipped[{key,why,got}] before{键:改前的值} params snapshot warnings[第三方调优残留] route{initcwnd,initrwnd} journald ntp nofile container`；撤销时 `rollback:true`）在作业 `job <id>` 的 `steps[0].output`。`ls` 每台带 `tune`、`show` 带 `tune`：`{profile,at,current,params}`（`current:false` 且 `profile` 为 null = 未调优或已撤销，非 null = 旧档；`params` = `{bw,bw_plan,bw_source,rtt,rtt_source,rtt_samples,mem_mb,buf_max}`，旧档 / 未调优为 null），家宽代理和同机 IP 节点为 null。
- `exec`：`exit_code stdout stderr duration_ms truncated timed_out`；多台时是这些字段加 `machine` 的数组，某台没执行成（请求被拒、连不上）时那一项是 `{machine,id,exit_code:null,error:{code,message,status}}`。
- `ssh`：没有 JSON 输出（终端交给 ssh，输出是远端的原样），退出码同远端；登录之前的错误（找不到机器、不在中控上、审计没记上）照常是 stderr 的错误 JSON、退出码 1。

- `routers`：API 的 data 原样，每台 `{id name team{id,name} note tunnel_port created_at state(pending|online|offline) last_seen_at error summary deploy bootstrap_expires_at}`；`summary`（从没采集成功过为 null）：`uptime_s cpu_pct load1 cores mem_used mem_total disk_used disk_total conntrack conntrack_max temp_c wan{up,ip,rx_bps,tx_bps} openclash{enabled,running,version,mode,conns,up_bps,down_bps} exits{total,alive,managed,drift} clients`（速率 bit/s，字节为单位的是内存、存储）。
- `router show`：上面这些加 `switches[{group from to back_at by switched_at}]`（进行中的临时切换，`from` 是第一次切之前的）与 `detail{system interfaces[] openclash exits[] groups[] clients[] collected_at}`；`groups[]` 每个 `{name type now all[]}`（`type` 是 mihomo 的类型，`Selector` = select 组、能 `router switch`；`now` 当前选中的）；`exits[]` 每个 `{name type server port entry{id,name,node_id,relay_id,node_name,relay_name,port,lifecycle}|null sync(ok|drift|unmanaged) alive delay_ms delay_at fail_streak conns up_bps down_bps clients}`（`entry.name` 形如「团队2-03 · 线路机2:20003」；各出口的速率是按连接估的）；`clients[]` 每个 `{ip mac hostname online conns exits[{name,conns}]}`。`router add` 不带 --host 输出 `{id, bootstrap{cmd, expires_at}}`。
- `router exits`（算好的统计，不是原始点）：`{router{id,name} range step_s exits[{name tests fails success_pct fail_streaks delay_median_ms delay_max_ms conns current}] total{exits tests fails success_pct fail_streaks delay_median_ms delay_max_ms conns}}`。`tests` / `fails` 是这段时间测了几次延迟、几次没通（每 5 分钟一轮，没通的当场重测一次、重测通了算通，一轮算一次；三种范围都准）；`success_pct` 是通的比例（%，一位小数，没测过为 null）；`fail_streaks` 是「连着 ≥2 轮不通」出现了几段（同告警 `router_exits_down` 的条件）：1h 按每一轮数，6h / 24h 按 5 分钟一点数（一点一般一轮，有一次没通算没通），没测的点跳过不打断；7d / 30d 是小时汇总、看不出先后，为 null（要看用 `--range 24h`）。延迟只算通了的：1h 是每次的，6h / 24h 是每 5 分钟的平均，7d / 30d 是每小时的平均（最大值偏小）。`conns` 是最近一次采集的当前连接数；`current: false` 的出口已不在路由现在的配置里（`conns` 为 null）；范围内没有点、现在配置里有的出口也列出（`tests` 为 0）。
- `provider ls`：API 原样，每家 `{id name code panel_url timezone traffic_rule(min|max|sum|out) traffic_reset(month|billing|rolling30) remind_days}`，`timezone` 为 null = 跟随组织时区。`plan ls`：API 原样（按 `--provider` 筛、默认去掉已归档的），每个 `{id provider_id name cpu_cores mem_mb disk_gb bandwidth_mbps traffic_gb price_cents currency cycle notes archived_at machine_count}`，`price_cents` 是分（29999 = 299.99），`traffic_gb` null / 0 = 不限，`archived_at` 非 null = 已归档，`machine_count` 是在用（未退役）的机器数。`provider add / set`、`plan add / set` 输出改完后的那一项（同 ls 的一项；没有变化时不提交，输出现在的），`rm` 输出 `{id}`。

完整接口见仓库 `docs/API.md`，运维流程与何时才需要直接 ssh 见 `docs/OPS.md`。
