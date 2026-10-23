---
name: fleet-ops
description: 团队线路管理（fleet）的运维入口。查节点或线路机状态、排查告警、处理到期与续费提醒（看订阅、登记续费、改订阅）、维护前开维护窗口或静默告警（可预约）、看服务日志、在节点上执行命令、登录机器或软路由、上架新机器、替换节点（新机器接替旧节点、软路由跟着换、旧的放备用）、退役（进备用、立即回收）与回收站（含旧面板导入）、改名与编号、升级 agent、取入口分享链接、查看与接入软路由（OpenWrt + OpenClash：状态、出口、客户端、接入命令、一键部署）时使用，在中控上运维也用它；一律先用 fleetctl（登录用 fleetctl ssh），不要直接 ssh、psql 或手拼 curl。
---

# fleet-ops

`fleetctl` 通过中控完成运维：中控用自己的 SSH 密钥登录机器，每次执行都写审计。直接 ssh 会绕开审计和 known_hosts 校验。

## 开始

```
fleetctl whoami            # 确认连得上，并看本密钥的 scopes
fleetctl --json <命令>      # AI 一律加 --json，输出一行 JSON
```

- 配置：在中控上已经配好（见下文「在中控上运维」）。别处用环境变量 `FLEET_URL`、`FLEET_TOKEN`，或 `fleetctl login --url <地址>`（密钥从标准输入读一行，如 `< 密钥文件`；`--token <密钥>` 仍可用，但会留在 shell 历史与进程列表里）。实验室中控是 `http://192.168.31.60:8080`。
- `--json` 的 `ls` / `entries` / `incidents` / `policy ls` / `silence` 是与表格同列的精简对象（字段见文末速查），一台机器的完整视图用 `show`、一个团队或节点的完整策略用 `policy show`；`billing` 是订阅、流量额度与最近续费记录合成的对象；其余命令是 API 的 data 原样。机器上百台时 `ls` 仍有几万字符，先用 `--team` / `--kind` / `--status` 缩小。
- 成功退出码 0；失败退出码 1，错误 JSON 在 stderr（`{"error":{"code","message","status"}}`）。`exec` 单台时退出码等于远端命令的退出码。`job` / `job --follow`（以及各命令的 `--follow`）遇到作业失败或取消也退出 1，stderr 给 `{"error":{"code":"job_failed"|"job_cancelled","message",…}}`。
- 没拿到中控响应时 `status` 为 null、`code` 是 `unreachable`（连不上，请求没发出去，可直接重试；`message` 带最底层原因）或 `timeout`（请求已发出、等响应超时）。`timeout` 时操作可能已在中控执行：先 `fleetctl --json jobs --active` / `fleetctl --json audit --limit 10` 核对，再决定是否重试，尤其是上架、退役、换线路机这类会建作业的命令。
- `<机器>` 写机器 id、显示名（`团队1-06`）、线路机名（`线路机2`）或 IP。歧义时命令报错并列出候选，照候选里的 id 重试。

## 在中控上运维

用户 ssh 进中控（root）后，本机 fleetctl 已配好全权密钥 central-local（部署时 `fleet-server local-admin` 建，令牌在 /root/.config/fleetctl/config.toml）：不用 login、不用令牌，`whoami` 显示 `apikey:central-local` 与全部 scopes。权限是全的，下文「先向用户确认」的操作照样先确认。

你不在中控上、用户要经中控运维时，每条命令经 ssh 在中控上执行（`ssh <中控> 'fleetctl --json status'`，生产中控的 ssh 别名是 debian13），先 `ssh <中控> cat /root/AGENTS.md` 读一遍中控上的说明。

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
| 快到期的机器、一台机器的费用与续费记录 | `fleetctl --json ls --due 7`（7 天内到期、含已过期的在用机器，按到期排；`status` 的 `overview.due_soon` / `overdue` 是按各台提前提醒天数算的即将到期 / 已过期台数）、`fleetctl --json billing <机器>`（登记续费、改订阅见下文「到期与续费」） |
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

访问规则的健康提示：节点被设为 `blocked` 时健康原因里有 `policy_blocked`（info，不降级、不开告警）。

访问监控只有线路机开了访问日志、且有真实客户端流量时才有数据；`new` 是窗口内第一次出现的目标（旧系统的「新域名」），不是告警。
访问记录只留用户设的天数（`settings retention`：原始记录默认 7 天、汇总默认 31 天），到期中控自动删除：`access` / `sources` 的 `--window` 不能比汇总的保留期长（超了报错 400，换短一点的窗口，不要去改保留期）；`events` 只有原始记录保留期内的；「新目标」「新运营商 / 新城市」指保留期内没见过的。

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

## 软路由

客户现场的 OpenWrt / iStoreOS 路由（OpenClash，mihomo 内核）经一条反向 SSH 隧道连中控（路由上的 `fleet-tunnel` 服务，与原有的 router-tunnel 并存），中控每 30 秒经隧道只读采集一次、每 5 分钟测一次全部出口的延迟（每批 6 个出口并发，没通的当场单独重测一次，重测通了不算不通；连续两轮不通才算不可用、开 `router_exits_down`）。接入只新增（隧道密钥、中控登录公钥、`/etc/init.d/fleet-tunnel`），不改路由的 root 密码、dropbear / LuCI 监听、OpenClash 配置与密钥、防火墙；用户明确要求过这些都不要动，AI 也不要去改。

- 状态 `state`：`pending`（还没接入，或刚重新生成接入命令、等在路由上执行）/ `online` / `offline`（接入过、此刻连不上，`error` 是原因）。`deploy` 是最近一次一键部署：`state` running / failed / done，`step` 正在做的那一步，`error` 失败原因。
- 出口的 `sync`：`ok` 参数和对应的 fleet 入口一致；`drift` 对上了入口但参数不一样（入口轮换过凭据、换了线路机后路由的配置没同步，或入口已删除）；`unmanaged` 不是 fleet 的入口。`drift` 要告诉用户，AI 不要去改路由的配置。
- 告警（对象是路由：`subject.kind` 为 `router`，不能 `show`，用 `router show`）：`router_offline`（接入过、连不上超过 2 分钟，critical）、`router_openclash_down`（OpenClash 启用了但核心没在跑，critical）、`router_exits_down`（有出口连续两次延迟测试不通，warning；全部不通 critical）、`router_wan_down`（WAN 口断开，critical）、`router_resource_high`（CPU 持续 10 分钟 > 90%、内存 / 存储 > 90%、连接表 > 80%，warning）。全局静默对它们生效；单台机器的静默管不到路由。
- 接入（会改路由，先向用户确认，见下一节的命令）有两种，路由上执行的是同一份脚本：
  - 中控连不到路由（路由在 NAT 后面，三台现场路由都是）：`fleetctl router add <名称> [--team 团队2]` 打印接入命令（30 分钟、只能用一次），让用户在路由上用 root 执行；过期了 `fleetctl router bootstrap <路由> --yes` 重新生成（现在的隧道密钥立刻作废）。
  - 中控能直接连到路由的 SSH（公网映射、同一网段）：一键部署 `fleetctl router add <名称> --host <IP> [--port 22] [--user root] --password-stdin --yes --follow`（密码从标准输入读一行，不要写在参数里）；路由上已经授权过中控公钥时不用密码。已有的路由重新部署用 `router deploy`。失败原因原样转告用户；中控连不到它就改用接入命令。
- 删路由（`router rm --yes`）只撤销中控这边（隧道公钥、状态与指标）；输出里的 `cleanup` 是要在路由上执行的清理命令，转给用户。

## 维护窗口与静默

要动机器之前（重启、换内核、改网络、迁机房，或用户说要停机维护），先给它开维护窗口：窗口内这台机器不开告警、不发通知，卡片显示「维护中」、暂停健康判定，免得一边维护一边告警刷屏。做完就删，别等它自己到期。

```
fleetctl silence add <机器> --for 2h --maintenance --comment "换内核"                               # 从现在开始
fleetctl silence add <机器> --start "2026-10-23 02:00" --for 4h --maintenance --comment "机房割接"    # 预约：--start 按北京时间，最多提前 90 天
fleetctl silence add <机器> --rule agent_offline --for 30m --comment "重启 agent"                    # 只静默一条规则（规则 key 见文末；不能加 --maintenance）
fleetctl --json silence ls                                                                          # 进行中的在前，已预约的随后
fleetctl silence rm <id 前缀>                                                                       # 提前结束 / 取消预约，告警即刻恢复
```

- `--for` 写 `30m` / `4h` / `2d`（可组合，如 `1h30m`），最长 30 天；更久说明机器不用了，该退役。`--all`（全部机器）最长 24 小时（等于关掉整个监控，API 密钥只能这么久；更长的请用户在网页用账号建）。`--comment` 写清在干什么，网页列表、审计和通知里都看得到。
- 给单台机器开维护窗口是维护流程的一部分，不必另问；`--all`（全部机器）要加 `--yes`，先向用户确认。不要拿静默盖住没处理的故障：机器正在告警、又不打算马上修时，先问用户。
- 新建、删除静默 / 维护窗口会通知所有启用的渠道（谁建的、对象、起止、备注；不受静默影响）：全部机器的一律发，单台的此刻生效才发、预约的不发。
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
   推周期时目标月没有那一天按月末（1-31 推一个月是 2-28，闰年 2-29）。`--amount` 是订阅币种的金额，写 `39.46`（最多两位小数），实付和订阅价格不同就按实付填。登记后续费类告警下一轮评估（≤30 秒）自动恢复。
4. 不续了：不登记；机器不再用就按用户的意思退役（`retire`）。不要拿静默盖住到期提醒。
5. 订阅跟随模板：价格、币种、周期、流量上限跟随**套餐**，账单时区、提前提醒天数、流量重置（自然月 / 账单日 / 滚动 30 天）跟随**供应商**；订阅自己只存到期日、自动续费和「单独设置」的项。同一套餐的机器要一起调价 / 改额度，请用户在网页「团队与套餐」里改套餐（立即对没单独设置的机器生效），不要逐台 `subscription set`。`billing` 的每项后面写着「（跟随套餐）」「（跟随供应商）」「（单独设置）」，`--json` 里 `subscription.overrides` 列出单独设置了的字段、`sub_template` 是跟随的值。
6. 只有这一台和套餐不一样（调价、换周期、改提醒天数、自动续费、流量上限）时：`fleetctl subscription set <机器> [--price 45|none] [--currency USD|none] [--cycle monthly|quarterly|semiannual|yearly|none] [--due YYYY-MM-DD] [--auto-renew true|false] [--remind-days 7|none] [--tz Asia/Shanghai|none] [--traffic-gb 2000|0|none] [--reset-day 15|none] --yes`。给了值 = 这台单独设置，给 `none` = 改回跟随；和模板相同的值中控也按跟随存。只改给了的字段（命令先读现有订阅再整份提交），确认语逐项列出「旧 → 新」（带来源），没有变化不提交；没有订阅时新建只要 `--due`（没有套餐的机器还要 `--price`）。
   - 续费用 `renew`（留续费记录）；`subscription set --due` 只用来改正填错的到期日。
   - `--auto-renew true`：到期后中控按周期自动顺延到期日、记一条续费记录（操作人「系统」），到期前不提醒，到期日次日还没顺延才算过期；不会扣款，只是记账。
   - `--traffic-gb` 是这台单独设的流量上限（供应商的十进制 GB），`0` = 不限，`none` = 跟随套餐；`--reset-day` 是流量每月几号重置，`none` = 跟随供应商（按账单日 = 到期日几号，按自然月 = 1 号，或滚动 30 天）。
   - 本周期已用按 agent 选中的网卡计量（`billing` 写「按 ens3 计」）、规则跟供应商（取小 / 取大 / 相加 / 只算出站；家宽代理和同机 IP 节点是业务流量）。**不能再按面板手填已用**（`--used` 已删，接口 400）：用户说「面板上已用 X GB」时，先看 `billing`——周期中途上架的写「X 起统计，之前的用量不在内」（下个周期自动完整），割接时录的写「含接入前读数 X（…起不再计入）」；数差得多就核对计量网卡与供应商的「流量重置」「流量计量」口径（见 OPS.md 流量额度一节），告诉用户原因，不要另想办法改数。
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
fleetctl tune <机器>[,<机器>…] | --team <团队> | --all [--yes] [--follow]   # 重做系统调优（上架时已自动做）：内核网络参数、BBR、日志体积上限、时间同步，不重启 ss/Xray、不断连接；--team/--all 先做一台（线路机优先）并跟到结束，成功后其余逐台提交，有进行中作业的跳过并列出；--all 要 --yes；同机 IP 节点、家宽代理 409；`ls` 的「调优」列看谁是旧档 / 未调优
fleetctl renumber <团队> --yes                             # 不加 --yes 只预览
fleetctl move <节点> --to <线路机> --yes [--from <线路机>] [--no-routers] [--follow]   # 换线路机：新入口验收后改软路由出口（同替换，失败全部换回、旧入口不摘）、再摘旧入口，不断网；分享链接会变，完成后 fleetctl share <节点> 取新链接。不加 --yes 先看预览
fleetctl evacuate <线路机> --to <线路机> --yes [--no-routers] [--follow]          # 搬空线路机（退役前）：每个在用节点一个换线路机作业，软路由出口跟着改；作业并行，同一台路由一次只改一个节点（其余的在「改软路由出口」这一步排队）
fleetctl replace <旧节点> --with <新节点> [--to <团队>] [--number N] [--no-routers] --yes [--follow]   # 替换节点：不加 --yes 只预览（见下）
fleetctl provision ...                                     # 先 check，再确认，再提交
fleetctl retry <作业> [--reset-host-key] [--password-env VAR]   # 重试失败作业；只有作业因主机指纹不符失败、且确认机器重装过才加 --reset-host-key（同机 IP 节点不行：对宿主机升级 agent 后重试那个作业）；在用机器指纹变了也走这条：先 upgrade-agent 让它失败再重试；报「没有可用的 root 密码」时用 --password-env 补密码
fleetctl renew <机器> [--cycles N | --to YYYY-MM-DD] [--amount 39.46] [--note …] --yes   # 登记续费：用户确认付过款之后（见「到期与续费」）
fleetctl subscription set <机器> --price 45 … --yes          # 改订阅：只改给了的字段；值 = 单独设置，none = 改回跟随套餐 / 供应商
fleetctl settings retention [--raw 7] [--hourly 31] --yes   # 访问记录保留期（各 1–90 天，汇总不短于原始记录；只给一个时另一个不变）：调短后超出的访问记录几分钟内删掉、找不回来，只在用户明确要求时改
fleetctl settings fx CNY --per-usd 7.12 --yes   # 手动设置汇率（或 --usd 0.1404 = 1 CNY 合多少 USD）：之后每天的自动更新不再改它；--auto 改回自动的值；只在用户给了汇率或明确要求时改
fleetctl settings fx --refresh                  # 立即从汇率源（ECB）更新一次；失败保留上次的值（报错 502，原因在 last_error）
fleetctl router add <名称> --host <IP> --password-stdin --yes --follow   # 软路由一键部署：中控登录路由装上 fleet-tunnel（只新增）；不给 --host 只建记录、打印接入命令（不改路由，不用确认）
fleetctl router deploy <路由> --host <IP> [--password-stdin] --yes --follow   # 重新部署：换一把隧道密钥
fleetctl router bootstrap <路由> --yes                      # 重新生成接入命令：现在的隧道立刻作废，要在路由上执行新命令才连得回来
fleetctl router rm <路由> --yes                             # 删软路由：撤销隧道公钥、删掉状态与指标；路由上的清理命令在输出里，转给用户
```

- 上架：`fleetctl --json provision --file batch.json --dry-run` 只校验；通过后让用户确认名单，再去掉 `--dry-run` 加 `--yes --follow`。单台参数：`--kind vps --ip 1.2.3.4 --team 团队1 --relay 线路机2 --provider ATTVPS --plan 基础版 --due 2026-12-01`。选了套餐必须给 `--due`（到期日；价格、流量等跟随套餐，时区、提醒跟随供应商，`--provider` 可不给、取套餐的）；只有这台和套餐不同时才加 `--price 45` / `--currency` / `--cycle` / `--tz` / `--traffic-gb` / `--reset-day`（单独设置）。没有套餐时给了 `--due` 就要给 `--price`。root 密码放环境变量，用 `--ssh-password-env VAR` 传名字。`--ss-port`（批量文件里 `ss_port`）是线路机连节点用的公网（NAT 映射）端口，节点上 ssserver 固定监听 8388；非 NAT 机不要填。价格一律用 `--price 39.46`（主单位，同 `subscription set`）；`--price-cents` 是旧写法，不再用。
- 同机 IP 节点（已有 VPS 节点上多出来的 IP 当一台独立节点，按 IP 计节点）：`fleetctl provision --kind vps --host <宿主机> --ip <新 IP> --team 团队1 --relay 线路机2 --yes --follow`（批量文件条目写 `"host": "<宿主机>"`）。宿主机要是在用的普通 VPS 节点；IP 要已直接配在宿主机网卡上。不填 SSH 端口 / 密码 / SS 端口（SSH 与 agent 用宿主机的，SS 端口中控从 8389 起分配），供应商取宿主机的、费用默认不记。作业 `provision_ip` 第一步「在宿主机上核对 IP」：报「不在宿主机网卡上」或「中控看到的来源是 …」时是机器网络配置问题，让用户去供应商面板 / 机器上配好再 `fleetctl retry`。
- 升级 agent：新版本起不来、2 分钟内没上报，或线路机上新版本报的 Xray 不正常（没在跑、配置没应用到升级开始时的代数、有应用错误；升级前就这样的项不查），作业自动换回旧版本后失败（错误「…，已回滚到 <旧版本>」，线路机的写「线路机检查没通过（…）」：机器照常上报，把错误告诉用户即可）；错误里写「需要人工处理」时 agent 可能没在运行，按 `docs/OPS.md` §7 处理。`--all` 的金丝雀或任何一台线路机换上新版本后出问题，没开始的作业都取消（`error` 写明是哪台），不要逐台去重试或再 `--all`，先查失败的原因；推送之前就失败的（SSH 连不上、登录被拒）只算那一台，后面的照常继续，修好那台后 `retry` 它的作业。用户要停下 `--all`：排队中的作业 `fleetctl cancel <作业>`，等它的跟着取消；进行中的不能取消。错误是「中控上的 agent 在升级过程中换了版本…」时是中途重新部署过中控，问过用户后重新 `--all`。失败的升级作业 `retry` 从推送重来。
- 替换节点（用户说「把 团队2-01 换成新机器」「替换下来的放备用」）：**一律用 `fleetctl replace`，不要手改软路由的 OpenClash 配置、不要 `fleetctl ssh router` 去改、不要直接写数据库，也不要用 rename + retire 拼**。新节点先上架（同团队拿到临时编号、入口生效）；`fleetctl replace 团队2-01 --with 团队2-20` 先看预览（新节点 → 旧编号、旧节点 → 备用团队的哪个号、要改哪些软路由的哪些出口、编号压缩、阻断原因），告诉用户，同意后加 `--yes --follow`。作业先改软路由（同团队几台路由都改；备份 → 只改那个出口的 server/port/uuid/servername/public-key/short-id → 重启 OpenClash → 测延迟，任何一台失败全部换回原配置），再交换编号（同 rename，旧节点进备用团队，同退役的默认去向，到期满 1 天由中控自动回收）。阻断里有「软路由「X」现在离线」：等路由在线（`fleetctl routers`）或用户同意只换编号（`--no-routers`，路由上的出口就还指向旧节点）。旧节点默认去备用团队，用户要放别处再加 `--to`。作业失败：把 `fleetctl job <id>` 的错误告诉用户（改路由失败时路由都已换回、编号没动；写「需要人工处理」的是回滚也失败了，备份路径在错误里），处理后 `fleetctl retry <id>`；开始改路由之后不能取消。
- 改名：`fleetctl rename <机器> --team 团队2 --number 3`。
- 国家（名字旁的国旗）：默认按 IP 查离线库自动定；查错了或想固定就 `fleetctl rename <机器> --country JP`（ISO 3166-1 两位代码，英国是 GB），`--country auto` 改回自动；上架时也可以 `fleetctl provision … --country US`。
- 分享链接 `fleetctl share <入口>` 需要 `secrets` scope，会记审计；链接是凭据，只给用户本人，不写进文件或日志。
- 密钥管理（`fleetctl keys ...`）只认登录会话，AI 不做；需要新密钥时请用户在网页「设置 → API 密钥」创建。

## --json 字段速查

- `ls`（每台一个精简对象）：`id name kind(relay|vps|proxy) lifecycle(active|provisioning|retiring|retired|failed) health(ok|degraded|down|unknown|maintenance|inactive) ip agent_online cpu_pct mem_pct disk_pct entries(入口数) open_incidents active_job{id,kind,state}`；百分比是整数，取不到为 null；家宽代理没有 agent，`agent_online` 为 null。`--due N` 时每项另有 `due_date due_days auto_renew`（`due_days` 按机器的账单时区算，负数是已过期几天）与 `fee{monthly_cents currency monthly_usd_cents usd_per_unit}`（月费的原币种与折成的 USD，没有汇率时 USD 两项为 null；没有订阅时 `fee` 为 null），表格多一列「月费」（USD，括注原价）。
- `show`（MachineView）：上面这些的完整版（`health.status`、`agent.online`、`host.{cpu_pct,mem_used,mem_total,disk_used,disk_total,load1}`、`entries[]` 完整入口视图），另有 `ssh_port revision`、`health.reasons[{key,rule_name,severity,text}]`（`rule_name` 是规则中文名，同通知）、`agent.health`（agent 自报：离线队列、访问日志解析不了 / 丢弃的行数等）、`failed_job`。
  同机关系：`host_machine{id,name}` 非 null 的是同机 IP 节点（它的 `agent`、`host` 都是宿主机的，agent 离线 / 高负载告警在宿主机名下；`logs --unit ssserver` 看它自己的实例 `fleet-ssserver@<端口>`，`exec` 在宿主机上执行；不能单独 `upgrade-agent`）；`ip_nodes[{id,name}]` 是宿主机上未退役的同机 IP 节点。
- `entries`（每个入口一个精简对象）：`id relay port node lifecycle(active|pending|disabled|removing|removed) applied apply_error tcp_ok egress_ok today_down today_up`（今日业务流量，字节）；`tcp_ok` / `egress_ok` 为 null 表示还没探测过。`node` 是「线路机名-直出」的是线路机直出入口：没有节点，`tcp_ok` 恒为 null；它出口不通时告警对象是线路机（`egress_fail`）；搬空不搬它，退役线路机时自动摘除。
- `incidents`（每条一个精简对象，`status` 里的 `incidents` 相同）：`id severity(warning|critical) rule rule_name subject_id subject_name summary reopen_count state(firing|resolved) opened_at resolved_at ack_by`（`ack_by` 非 null 即已确认；恢复后 30 分钟内复发会重开同一条并 +1，表格里显示「反复 N 次」；大于 0 说明在抖，查探测/网络而不是只看当前状态）。规则 key：agent_offline、clock_skew、no_entry、egress_fail、tcp_fail、renewal_due（过期后升为 critical；备用团队的机器只在还有软路由出口指向它时才报；旧记录里还有 renewal_overdue）、traffic_quota_high（流量额度：本周期已用 ≥ 90% warning、≥ 100% critical，不改健康状态，新周期或调高上限后自动恢复）、relay_apply_lag、relay_apply_error、relay_xray_down、relay_unreachable（中控连续 3 轮连不上线路机的客户入口，或同一个入口端口最近 3 次都连不上，critical，摘要里有端口与原因）、host_cpu_high、host_mem_high、host_disk_high、ipintel_new_isp（来源出现新运营商）、ipintel_new_city（来源出现新城市）、ipintel_datacenter_source（机房 / VPN 来源 IP），这三个是 warning、学习期 7 天后才有、最后一次出现 24 小时后自动恢复，不用手动处理。中控自身的告警（对象是「中控」：`subject_id` 为 null、`subject_name` 为「中控」，不能 `show`）：central_channel_failing（通知渠道连续发送失败满 5 分钟，critical，摘要里有渠道名与原因；请用户在网页「设置 → 通知渠道」改配置并点「测试」，测试成功即恢复）、central_backup_stale（中控每日备份超过 26 小时没有新的，warning；`fleetctl server-logs` 帮不上，需要用户在中控上 `systemctl status fleet-backup`）、central_disk_low（中控本机磁盘可用低于 10%，critical，写满了中控会停；告诉用户尽快清理）、central_ingress（有 agent 的机器过半且至少 3 台同时失联，critical：边缘机 / Caddy / DNS 出问题，agent 到不了中控；期间不按台开 agent_offline）。这几条都要转告用户，AI 处理不了。软路由的告警是 `router_` 开头的五条（见「软路由」一节），`subject_id` 是路由 id、`subject_name` 是路由名，用 `router show` 看，不要 `show`。
- `silence ls`（每条一个精简对象；`silence add` / `silence rm` 输出新建 / 删掉的那一条，同形）：`id subject_id subject_name rule rule_name starts_at ends_at state(active|scheduled) is_maintenance comment`；`subject_id` 为 null = 全部机器，`rule` 为 null = 全部规则；`state` 是 `active`（进行中）或 `scheduled`（已预约，还没开始）。只列没结束的。
- `billing`：`id name host_machine spare subscription sub_template traffic_limit_gb quota{limit_gb,limit_source,used,period_start,period_end,mode,counted_from,rule,base,base_at} quota_pct renewals renewal_count`。`subscription` 为 null 是没有订阅，否则 `{price_cents currency cycle(monthly|quarterly|semiannual|yearly) due_date due_days due_state(ok|soon|overdue) billing_tz auto_renew remind_days traffic_gb traffic_reset_day overrides monthly_cents next_due period_days}`，全是生效值（`due_state` 同续费告警：soon = 提前提醒天数内且不是自动续费；`next_due` = 推一个周期后的到期日，`renew` 不带 `--to` 时由中控算），`overrides` 是单独设置了的字段名（其余跟随 `sub_template`：`{price_cents currency cycle billing_tz remind_days traffic_gb traffic_reset(month|billing|rolling30)}`；`rolling30` 时跟随的 `traffic_reset_day` 为 null）；金额都是分（`price_cents` 3946 = 39.46，币种看 `currency`）。`traffic_gb` 0 / null = 不限，`traffic_limit_gb`（= `quota.limit_gb`）是生效的上限（null = 不限），`quota.limit_source` 是它的来源（`subscription` 单独设置 / `plan` 跟随套餐 / `host` 同机 IP 节点与宿主机共用 / `none`），`quota_pct` 是本周期已用（`quota.used`，字节）占它（GB × 10⁹）的百分比（向下取整，没有上限为 null）；`quota.rule` 是网卡计量规则（`min|max|sum|out`，null = 业务流量）；`quota.mode` 是 `monthly`（`period_end` = 下次重置日）或 `rolling30`（近 30 天，`period_end` 为 null）；`quota.counted_from` 非 null = 周期中途上架、这之前的用量不在 `used` 里；`quota.base_at` 非 null 是割接时录的接入前读数（`base`，字节，只读，下个周期自动失效）。本月网卡流量在 `show --json` 的 `nic{today_rx,today_tx,month_rx,month_tx}`（rx 下行、tx 上行；家宽代理、同机 IP 节点为 null），`month_counted_from` 非 null = 本月中途上架，`agent.nic_ifaces` 是计量的网卡。`renewals` 是最近 10 条续费记录（新→旧）`{id old_due new_due amount_cents note actor created_at}`，`renewal_count` 是总条数。
  计费统一按 USD：`subscription` 里另有 `monthly_usd_cents`（月费折成 USD 的分）、`price_usd_cents`（每期价格折成 USD）、`usd_per_unit`（所用汇率，1 单位合多少 USD；USD 为 1），由中控按汇率算好（`round(原币种分 × usd_per_unit)`，四舍五入到分）；三个都是 null = 没有这个币种的汇率、未折算（不是 0 元，也不是 1:1）。原字段 `price_cents` / `currency` / `monthly_cents` 照旧是供应商收的原币种。文字输出的「费用」行后面写「；折合 37.50 USD/月（1 CNY = 0.125 USD）」或「未折算」。
- `renew`：刚登记的这条续费记录（同 `billing` 的 `renewals[]`）。`subscription set`：改完的 `subscription`（同上，`due_days` 按新到期日算）；没有变化时不提交，输出现在的。
- `settings retention`（改了之后输出改完的，同形）：`{raw_days, hourly_days, min, max, profile_days}`；`raw_days` 是原始访问记录（每一条：时间、来源 IP、目标）留几天，`hourly_days` 是汇总（小时汇总、新目标、来源画像）留几天、也是访问页能看的最长范围，`profile_days` 是来源画像实际留几天（不短于 IP 情报的学习期，只读）。没有变化时不提交，输出现在的。
- `policy ls`：`draft_count teams[{id name mode domains draft revision nodes[{id name mode domains source(node|team|default) draft revision}]}]`；`domains` 是域名个数，`draft` 为 null 或 `{mode,domains}`；节点的 `mode` / `domains` / `source` 是生效值，`draft` / `revision` 是节点自己的。
- `status --json` 的 `overview.failed`：上架失败（lifecycle=failed）的机器数，算在「有问题」里。`show` 的 `failed_job{id,kind,state}`：上架失败或退役失败（退役中）的机器带最近失败的作业，拿它的 id 去 `fleetctl job` / `fleetctl retry`（`ls --status failed` / `ls --status retiring` 找出这些机器）。
- 不加 `--json` 的 `status` / `show` / `entries` / `incidents` 是给人看的：种类、生命周期、健康、严重度、规则、入口状态、作业种类与状态都显示中文；中文名和计费周期都取中控的 `/meta`（网页同一份）；所有命令表格里的时间都是组织时区（中控 fleet.toml 的 `org_timezone`，默认北京时间；`--json` 里是 API 原样的 RFC 3339）；`ls` 的种类 / 生命周期 / 健康列保留原值（就是 `--kind`、`--status` 的取值）。脚本和 AI 只解析 `--json`。
- `status --json` 的 `overview.monthly_usd`：`{cents, by_currency:[{currency,cents,usd_cents,usd_per_unit}], unconverted:[{currency,cents}]}`——每月费用折成 USD 的合计（分，逐台折算后相加）、各原币种的金额与所用汇率；没有汇率的币种不算进 `cents`，单列在 `unconverted`。旧字段 `overview.monthly_cost`（按币种分别合计、不折算）仍在。文字输出「每月费用」写 USD 合计，括注非 USD 币种的原价与汇率。
- `settings fx`：`{base, auto, url, fetched_at, rates_date, last_attempt_at, last_error, missing:[在用但没有汇率的币种], rates:[{currency, usd_per_unit, per_usd, source(base|manual|auto|null), auto_per_usd, manual{usd_per_unit,by,at}, in_use}]}`；`source` 为 null = 没有汇率（费用未折算）。文字输出只列在用和手动设过的币种。
- `status --json` 的 `overview.online` / `offline`：按 agent 连通计（家宽代理没有 agent 算在线），与健康无关。
- `job`：`state`（queued|running|waiting|cancelling|succeeded|failed|cancelled）、`error`、`steps[]`、`logs[]`。`kind` 里 `provision_ip` 是上架同机 IP 节点。
- `retire --json`：进备用 `{mode:"spare", name（备用里的名字）, from, recycle_on, ip_nodes:[{id,name,from}]}`；`--now` 为 `{mode:"now", job_id, ip_node_jobs:[{machine_id,name,job_id}]}`，后者是一并回收的同机 IP 节点各自的退役作业。
- `recycle ls --json`：`{items:[{id source(fleet|legacy) source_id machine_id kind name last_name team ip host_name provider plan service_ref price_cents currency cycle activated_at retired_at due_date recycled_at reason traffic_up traffic_down nic_rx nic_tx actor}], total, next_offset}`；`recycle show --json` 另有 `snapshot`。`recycle import --json`：`{dry_run, inserted:[{source_id,name,id}], skipped:[{source_id,name,why}]}`。
- `upgrade-agent --all --json`：`{jobs:[{machine_id,name,kind,job_id,after}], skipped:[{machine_id,name,reason}], order}`；`jobs` 按执行顺序，第一个是金丝雀，`after` 是它要等的作业（等的那个换上新版本后失败、或被取消，它就被取消）；`order` 是顺序说明，原样转述给用户。中控上没有 agent 程序时报错（409），不建作业。
- `tune --json`：`{jobs:[{machine_id,name,job_id}], skipped:[{machine_id,name,reason}]}`（`--team` / `--all` 时金丝雀失败则多一个 `stopped:true`，其余没提交）；逐键结果（`applied ok kept skipped[{key,why,got}] journald ntp nofile container`）在作业 `job <id>` 的 `steps[0].output`。`ls` 每台带 `tune`、`show` 带 `tune`：`{profile,at,current}`（`current:false` 且 `profile` 为 null = 未调优，非 null = 旧档），家宽代理和同机 IP 节点为 null。
- `exec`：`exit_code stdout stderr duration_ms truncated timed_out`；多台时是这些字段加 `machine` 的数组，某台没执行成（请求被拒、连不上）时那一项是 `{machine,id,exit_code:null,error:{code,message,status}}`。
- `ssh`：没有 JSON 输出（终端交给 ssh，输出是远端的原样），退出码同远端；登录之前的错误（找不到机器、不在中控上、审计没记上）照常是 stderr 的错误 JSON、退出码 1。

- `routers`：API 的 data 原样，每台 `{id name team{id,name} note tunnel_port created_at state(pending|online|offline) last_seen_at error summary deploy bootstrap_expires_at}`；`summary`（从没采集成功过为 null）：`uptime_s cpu_pct load1 cores mem_used mem_total disk_used disk_total conntrack conntrack_max temp_c wan{up,ip,rx_bps,tx_bps} openclash{enabled,running,version,mode,conns,up_bps,down_bps} exits{total,alive,managed,drift} clients`（速率 bit/s，字节为单位的是内存、存储）。
- `router show`：上面这些加 `detail{system interfaces[] openclash exits[] clients[] collected_at}`；`exits[]` 每个 `{name type server port entry{id,name,node_id,relay_id,node_name,relay_name,port,lifecycle}|null sync(ok|drift|unmanaged) alive delay_ms delay_at fail_streak conns up_bps down_bps clients}`（`entry.name` 形如「团队2-03 · 线路机2:20003」；各出口的速率是按连接估的）；`clients[]` 每个 `{ip mac hostname online conns exits[{name,conns}]}`。`router add` 不带 --host 输出 `{id, bootstrap{cmd, expires_at}}`。

完整接口见仓库 `docs/API.md`，运维流程与何时才需要直接 ssh 见 `docs/OPS.md`。
