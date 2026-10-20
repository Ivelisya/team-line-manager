# 原型数据快照（脱敏）

真实生产数据的一次只读快照，供 `prototype/` 下各屏使用。快照时刻 **2026-09-23 15:22:18 北京时间**（`as_of = 1790148138`）。页面里所有“N 秒前”“剩余天数”都要相对 `as_of` 计算，不要用浏览器当前时间。

## 文件与加载顺序

| 文件 | 大小（原始 / gzip） | 何时加载 | 内容 |
|---|---|---|---|
| `snapshot.json` | 852 KB / 104 KB | 首屏 | 总览、节点表格与卡片、节点抽屉、上架矩阵需要的全部字段；IP 情报（`ipintel`，见下文专节）约 350 KB 原始 / 28 KB gzip |
| `detail.json` | 685 KB / 41 KB | 进入 `/nodes/:id` | 每个节点的完整检查项、48 小时逐时可用性、供应商核对、备注、最近 10 条访问、90 天 top 目标与来源、故障、续费、审计；每个节点的来源 IP 明细与出口 IP 观测历史（`nodes.<id>.ipintel`） |
| `extra.json` | 225 KB / 19 KB | 线路机页、行为页、审计、探测任务 | 入口全量（含来源 IP 列表）、全部 78 条故障、行为汇总、81 个探测任务、150 条审计、续费记录；行为页来源行与时间线行带运营商等字段 |
| `series/traffic-5m.json` | 149 KB / 62 KB | 详情“流量”页签 | 每个节点 24 小时业务流量，5 分钟桶 |
| `series/traffic-1m.json` | 628 KB / 242 KB | 需要分钟精度时 | 每个节点 24 小时业务流量，1 分钟桶（最高真实精度） |
| `series/host-24h.json` | 523 KB / 112 KB | 详情“主机”页签 | 33 台有主机 agent 的节点，24 小时 5 分钟桶：CPU、内存、磁盘、网卡收发（均值与峰值）、负载、TCP/UDP 连接、进程、Swap |
| `series/latency-24h.json` | 331 KB / 47 KB | 详情“延迟”页签 | 每个节点 TCP 与出口验证探测，24 小时 5 分钟桶：均值、最小、最大、丢包 % |
| `plan-catalog.json` | 13 KB / 3 KB | 需要显示套餐名、规格或费用时 | 套餐目录：6 个套餐（规范名称、规格、标价、原名称别名、待核实项），`by_asset`（节点短名 → 套餐 id）、`by_node_id`（`t1-01` → 套餐 id）。页面显示套餐一律用 `plans[].name`，不要再显示 `billing.plan` 里的原文。来源 `architecture/plan-catalog/catalog.json` |
| `tools/extract.py`、`tools/build.py` | | | 重新生成用的脚本（见文末） |
| `tools/ipdb.py`、`tools/ipintel_remote.py`、`tools/ipintel_run.sh`、`tools/ipintel_merge.py` | | | IP 情报：离线库读取器、a395 上的富化脚本、往返脚本、合并脚本（见“IP 情报”一节） |

`snapshot.json` 的 `lazy` 字段列出了上面这些懒加载文件的路径。

## 通用约定

| 项 | 约定 |
|---|---|
| 时间 | Unix 秒（整数），显示时一律按北京时间。`*_text` 字段已经是北京时间字符串 |
| 字节 | 整数字节 B。显示时用 1024 进制（KiB/MiB/GiB，简报 §4.2） |
| 速率、带宽 | **比特每秒**（`*_bps`），十进制（简报 §4.3）。Xray 计数原本是 B/s，已乘 8 |
| 时间序列里的流量 | 每个桶内的**字节数**（`unit: "bytes_per_bucket"`）。换成速率：`bytes * 8 / step` |
| 方向 | `down` = 下行 ↓（流向用户客户端，Xray downlink）；`up` = 上行 ↑（来自用户客户端，Xray uplink）；主机网卡用 `rx`（接收）/`tx`（发送），两者不相加（简报 §4.12） |
| 延迟 | 毫秒（`*_ms`） |
| 百分比 | 0–100 的数（`*_pct`、`*_percent`），未取整，由 format 模块按 §4.4 取精度 |
| 金额 | `price` 是原始字符串（`"28.88"`），`price_cents`/`monthly_cents` 是整数美分；目前全部 USD、全部月付 |
| 序列 | 列式：`{t0, step, <field>: [...]}`，第 k 个点的时间是 `t0 + k*step`；`null` 表示该桶没有样本（断档），`0` 是真实的 0 |
| 缺失 | `null` = 缺测或不适用；能力不适用看 `capabilities`（例如没有 `host` 就显示“无主机”，不要显示“等待采样”） |

## `snapshot.json` 顶层

| 字段 | 说明 |
|---|---|
| `as_of`、`as_of_text`、`timezone` | 快照时刻 |
| `provenance`、`anonymization` | 来源与脱敏说明 |
| `site` | 站点名“团队线路管理”、描述、Komari 版本 |
| `dict` | 状态词表与术语（简报 §4.10）：健康、生命周期、流水线步骤、线路机分项、入口、访问结果、故障、检查项、原因、账期、能力、方向、供应商类型 |
| `teams` | 团队顺序：团队1、团队2、线路机 |
| `providers` | 4 家供应商（ATTVPS 29 台、NodeMach 8 台、OhnoICU 1 台、GoMami 3 台线路机），含类型、有无主机权限、各自的套餐/价格/带宽组合 |
| `kpis` | 总览 KPI 条：`normal`（正常 41/41 及各状态计数）、`attention`（需处理数）、`business_rate`（业务速率 ↓↑，采样覆盖 41/41 个入口，附近 2 小时每分钟迷你图 `spark`）、`today_business`（北京时间 0 点起）、`month_business`、`due_7d`（≤7 天含已过期，附最近一台）、`monthly_cost`（$1,281.35 · 41 项服务）、`events_today`（今日访问记录条数） |
| `groups` | 团队汇总表：节点数、5 种状态计数、业务速率与采样覆盖、今日、本月、月费 |
| `attention` | 需处理列表。快照时刻为空（全部正常），见“已知缺口” |
| `incidents_recent`、`incidents_total` | 最近 30 条故障记录（全部 78 条在 `extra.json`）：检查项、严重度、状态、原因、开始/恢复、持续秒数、确认、证据（rc、phase、实际/期望出口 IP、错误） |
| `status_replays` | 5 个**真实发生过**的非正常时刻（09-23 09:11 团队1-31 入口认证失败；09-20 18:45 团队1-15 同类故障；09-16 两次降级；09-15 11:21 16 台主机探针未知），可用来演示“需要关注”有内容的样子。每项带节点、状态、首个失败检查、原因、持续时长、证据 |
| `nodes[41]` | 全部资产：38 台落地节点 + 3 台线路机，按团队、编号排序。字段见下节 |
| `relays[3]` | 线路机：状态分项（agent/Xray/面板/采集/配置）、主机指标、带宽与利用率、TCP 连接、网卡速率、入口统计（总数/活跃/停用/未绑定/主节点端口）、节点数、版本（面板、Xray、agent）、面板自报 CPU/内存、采集（耗时、日志积压与大小、事件缺口）、配置核对（路由哈希前 12 位、规则数）、流量额度、账单 |
| `entries[41]` | 三台线路机的全部入口：端口、协议·安全、绑定节点、团队、备注（`remark_differs` 表示与资产名不同）、启用、有活动、连接数、上次活动、实时速率、3x-ui 累计 ↓↑、Xray 计数、出站（协议、目标、端口、outbound_tag）、链路 TCP/出口、出口期望/实际 IP 与是否一致、客户端标识、配额与过期、来源 IP 个数；`ipintel`：绑定节点的来源摘要 |
| `traffic.fleet_24h` | 全机队业务流量 24 小时，**1 分钟桶**（1,439 个点），附下行/上行峰值与时间、合计、缺测分钟数 |
| `rankings` | 排行前 10：实时速率、今日流量、24 小时 CPU 均值、内存均值、延迟最不稳定（TCP 24 小时标准差）、丢包最高 |
| `upcoming_due` | 按剩余天数排序的前 10 台 |
| `system` | 系统状态：主机 agent 在线 33/33、连通性探针 8/8、孤立主机记录 1、采集器、监控、备份、存储占用、数据保留、可用率口径说明 |
| `behavior_kpis`、`behavior_top` | 行为页小 KPI；24 小时全机队 top 20 目标域名、top 12 来源 IP（每行带运营商等字段）、最近 12 条行为变化与各类计数（`new_source` 事件带 `subject_intel`：来源的运营商、省市、ASN、接入方式） |
| `access_recent` | 每个节点最近 10 条访问记录（`{节点 id: [...]}`）：时间、来源 IP、目标、端口、协议、结果（已转发/已阻止）、客户端标识、目标类型；另加来源画像字段（见“IP 情报 · 行级字段”） |
| `ipintel` | IP 情报：来源 IP 与出口 IP 的运营商、ASN、省市、接入方式，全机队 / 团队 / 线路机分布，租期时间线，异常，出口汇总。见下文专节 |
| `onboarding` | `history`：22 条真实旧流程任务（单台也算一个批次；8 步或线路机 11 步，含每步耗时与错误，其中 3 条已回滚）；`demo_batch`：新流水线演示批次（见下） |

## 节点对象（`nodes[]`）

| 字段 | 说明 |
|---|---|
| `id` | 可读 id：`t1-01`、`t2-02`、`r1`–`r3`（原资产 UUID 已去掉） |
| `name`、`full_name`、`number` | 短名 `团队1-01`；完整组合名（只用于导出，IP 已替换）；编号 |
| `team`、`kind`、`capabilities` | 团队；`residential`/`proxy`/`relay`；能力：`host`（有主机 agent）、`proxy`（家宽代理，无主机）、`relay`、`probe`（由线路机代发的连通性探针） |
| `lifecycle` | 全部 `active`（在用） |
| `provider`、`provider_code`、`region` | 供应商与缩写；地区代码、中文名、Komari 里的国旗字符 |
| `ip`、`ip_version`、`ipv6` | 出口 IP（假）；3 台有 IPv6（假，2001:db8::/32） |
| `relay`、`relay_port`、`entry_label` | `线路机2 :20002`；线路机自身写 `线路机2 主节点` |
| `status` | `code`/`word`（ok 正常 …）、`reason`/`reason_word`、`since`、`checked_at`、`first_failed_check`、`maintenance`、各检查项状态计数、是否需处理、`failed_checks`（未通过的检查项，快照时刻均为空） |
| `limits` | 服务端下发的阈值：CPU/内存/磁盘 %、出口耗时提醒 ms |
| `latency` | `tcp_ms`（线路到上游 TCP）与 `tcp_at`、`egress_ms`（认证出口验证 HTTP）与 `egress_at`、`path_ms`（实际入口到出口）、期望/实际出口 IP 与是否一致；`strip_tcp`、`strip_egress`：最近 20 个真实样本（`dt` 为距 `t_end` 的秒数，`lost` 为丢包标记，另有中位数、丢包 %、窗口秒数）。TCP 每 30 秒一次（约 10 分钟），出口每 300 秒一次（约 95 分钟） |
| `availability` | `windows` 1/7/30 天成功率与采样覆盖率；`tracker30`：按北京时间日历日的 30 格（`state`: none 未监控 / ok / minor ≥99% / degraded ≥95% / fail / unknown，另有当日成功率、覆盖率、故障次数） |
| `rate` | 实时业务速率 `down_bps`/`up_bps`、采样时刻、是否新鲜、TCP 连接、上次活动 |
| `traffic` | `today`、`month`（北京时间口径，字节）、`coverage_start` 与 `month_partial`（本月数据从 09-15 起）、`last24h`、`peak_24h`、`hourly24`（24 个整点桶，最后一格是当前未满小时）、计数器重置/断档/统计有效标记、3x-ui 累计、`quota`（仅线路机有：线路机1、2 各 2000 GiB，线路机3 1000 GB 出站额度；已用、重置日）、`host_quality`（5 台节点的主机网卡流量在 09-17 有“待核验”时段） |
| `host` | 仅 `host` 能力：CPU %、核数、型号、内存/Swap/磁盘已用与总量与 %、负载 1/5/15 与每核 %、运行秒数、进程数、网卡收发 bps 与累计、TCP/UDP 连接、上报时间、是否新鲜、系统、内核、架构、虚拟化、GPU |
| `host_spark` | 仅 `host` 能力：最近 4 小时 48 个 5 分钟点：CPU %、内存 %、网卡收发 bps、负载、TCP 连接 |
| `agent` | `komari-agent` 或 `relay-probe`、版本、在线、上报时间、存在方式 |
| `billing` | 价格、美分、币种、账期（`monthly`/月付）、月费、到期日、`due_days`（按账期时区日历日算）、账期时区与 `tz_badge`（`LA`）、供应商时区、自动续费、提醒天数、服务号、套餐、来源链接、最近一次续费 |
| `bandwidth` | 标称带宽文本、`capacity_bps`、来源（configured / asset_bandwidth / relay_default）、利用率 ↓↑ % |
| `tags` | Komari 标签（已去掉供应商、团队、线路机、带宽，与简报 H27 一致） |
| `outbound` | 出站协议、目标、端口、outbound_tag、SS 加密方式（只有经流水线上架的节点知道） |
| `connection` | 入站 tag、端口、vless · reality、Reality dest/sni、客户端标识、凭据前 8 位（**演示值**）、来源 IP 个数、是否有活动、是否启用 |
| `behavior` | 24 小时连接数、目标数、来源数、被阻止、纯 IP 目标数、最后出现 |
| `ipintel` | `summary`（来源数 24h/14d、24 小时新来源、首位运营商与城市及占比、移动网络占比、运营商与省份列表、标记、关联异常 id）、`egress`（出口 IP 画像，见下文）、`breakdown`（来源分布，24h 与 14d）。没有流量的节点（团队1-33～36、线路机2 主节点）也有，数值为 0 |

## IP 情报（`ipintel`）

两类地址：**来源 IP**（连到线路机入口的客户端地址，14 天内 35 个）和**出口 IP**（38 台落地节点 + 3 台线路机，另有 3 个 IPv6）。查询全部在 a395 上离线完成，IP 没有发往任何在线服务。真实 IP 只在 a395 的临时目录（tmpfs）里出现，跑完就删；传回本机的只有假地址和真实的查询结果。

### 数据库

| id | 库 | 版本 | 许可 | 查什么 |
|---|---|---|---|---|
| `ip2region` | ip2region xdb v4 / v6（github.com/lionsoul2014/ip2region 的 `data/`） | 2026-08-28 | Apache-2.0 / MIT | 国家、省、市、运营商。**中国地址以它为准** |
| `dbip_city` | DB-IP IP to City Lite 2026-09 | 2026-09-01 | CC BY 4.0：页面上要署名 “IP Geolocation by DB-IP” 并链接 db-ip.com | 国家、州/省、城市、经纬度。**境外地址以它为准** |
| `dbip_asn` | DB-IP IP to ASN Lite 2026-09 | 2026-09-01 | CC BY 4.0（同上） | ASN、AS 组织 |
| `iptoasn` | iptoasn.com `ip2asn-combined.tsv.gz` | 2026-09-23 | PDDL 1.0 | ASN、AS 名称、注册国家 |

四个库放在 a395 的 `~/.cache/team-ops-ipintel/`（解压后共 186 MB）。读取器是 `tools/ipdb.py`，只用标准库，因为 a395 没有 pip。

### 口径

- **窗口**：`h24` 是 `as_of` 前 24 小时。`d14` 是前 14 天，但活动表从 09-14 23:35 才开始有记录，实际只覆盖约 8.7 天（`meta.windows.d14.data_from`）。
- **连接数**是 activity 表的行数：Xray access log 里一条连接一行，不是字节。这份数据没有按来源统计的流量。
- **会话**：同一来源在同一范围内，相邻两次连接间隔不超过 30 分钟，算同一段会话。`active_min` 是有连接的分钟数。
- **地点**：中国地址用 ip2region 的省、市，境外地址用 DB-IP 的州、城市和坐标，两个库的结果不混用。两库的原始结果都在 `dbs` 里，`agree` 标出国家、省、市、ASN 是否一致。坐标：中国地址用地级市中心点（`coord_basis: "gazetteer"`），境外地址用 DB-IP 的城市坐标（`"dbip"`）。
- **运营商** `carrier`：ip2region 的“电信 / 联通 / 移动”统一写成“中国电信 / 中国联通 / 中国移动”。ip2region 没写运营商、或写的是单位名时，按 ASN 归属取（`carrier_basis`）。
- **接入方式** `conn_type` 是启发式判断，每条都带置信度 `conn_conf`（high / medium / low）和中文理由 `conn_basis`：
  - `datacenter`：ASN 在云厂商列表里（阿里云、腾讯云、AWS 等）。`hosting`：ASN 在 IDC / 主机托管列表里，或 ASN 名称含 hosting、cloud、IDC 等词。来源地址和某台线路机相同时，另标 `internal`（值是线路机 id）。
  - `mobile`：移动运营商的专用 ASN，或 ASN 名称写明 5G / LTE / wireless，置信度高。本数据里是中国电信湖北的两个 5G 网络 ASN：AS137266（武汉）和 AS131285（省网）。
  - `broadband`：固网运营商 ASN（AT&T AS7018、Telmex AS8151、StarHub NGNBN 等），置信度中。中国三大运营商的骨干网和省网 ASN（AS4837、AS4134、AS9808、AS24445 等）同时承载家宽和手机流量，库里没有接入方式字段，所以默认按固定宽带、置信度**低**，并给出 `conn_alt: "mobile"`。有一种情况会把置信度提到中：同运营商同城的地址用 1.5–4 天后，在十几秒内被新地址接替，这是 PPPoE 重拨的节奏（`conn_basis_signal: "lease_handover"`）。
  - `private`：内网或保留地址（线路机2 的 IPv6 是 ULA）。`unknown`：不在任何列表里。

### `snapshot.ipintel`

| 字段 | 说明 |
|---|---|
| `meta` | 方法；四个库的版本与许可；窗口；`hourly24_t0`（与节点 `traffic.hourly24` 对齐的整点起点）；`daily14_days`（北京时间日历日）；会话口径；扫描行数（2,651,074）和耗时；假地址映射的校验结果 |
| `dict` | 接入方式、置信度、标记、异常类型和分布行字段的中文词 |
| `summary` | 全机队头条数字：来源数（24h / 14d）、24 小时新来源；运营商、省、市、ASN 的个数；移动网络占比；机房、内部、境外、库间分歧来源各几个；各维度前 5 名（24h、14d）；异常计数；出口的 ASN 分布与住宅数 |
| `sources[35]` | 每个来源 IP 一条，按首次出现排序：`ip`（假）、`carrier`、`carrier_basis`、`isp_ip2region`、`asn`、`as_org`（DB-IP）、`as_handle` / `as_desc`（iptoasn）、`country`、`country_name`、`province`、`city`、`geo_basis`、`lat` / `lon` / `coord_basis`、`conn_type` / `conn_conf` / `conn_basis` / `conn_alt`、`agree`、`flags`（internal / datacenter / foreign / db_conflict / geo_conflict / asn_conflict / private）、`internal`、`dbs`（四个库的原始结果）、`first_seen` / `last_seen` / `span_s`、`connections` / `sessions` / `active_min` / `nodes`（各含 `h24`、`d14`）、`node_ids`、`teams`、`relays`、`clients.d14`、`blocked_d14`、`daily14`（14 格，活动表开始之前的日子是 `null`）、`hourly24`（24 格）、`new_in_h24` |
| `leases` | 地址租期时间线，按“运营商 + 城市 + 接入方式”分组。每组列出各个地址（首末时间、连接数、节点数）和 `handovers`：旧地址停用前后 3 小时内、在相同节点上出现的新地址，记录时刻、间隔秒数、旧地址用了多久、共同节点数 |
| `breakdowns` | 分布，分 `fleet`、`teams`（团队1、团队2、线路机、未分配直连）、`relays`（线路机1–3，按来源接入的线路机）、`direct_history`（线路机3 已不存在的 direct 入口）。每个都有 `h24` 和 `d14`，各含 `total`（连接、来源、会话、有连接的分钟）以及 `carrier`、`province`、`city`、`asn`、`conn_type`、`country` 六个维度。分布行是 `{k, c 连接数, s 来源数, ss 会话数, m 分钟数, pct 连接占比}`；`asn` 行多一个 `label`，`city` 行带 `lat` / `lon` |
| `egress_summary` | 出口汇总：按 ASN、接入方式、主地点、两库各自的州、DB-IP 城市计数；住宅落地数；两库州或城市不一致的节点；IP 变更次数与节点；观测记录总数；有 IPv6 的节点 |
| `anomalies[23]` | 从真实数据推出的异常，`warn` 排在前面，再按时间倒序。类型见下表 |

### 节点、入口和行级字段

- `nodes[].ipintel.breakdown`：同上的分布，只有 carrier、province、city、asn、conn_type 五个维度，city 行不带坐标（坐标到 `sources` 里查）。
- `nodes[].ipintel.egress`：`ip`、`isp`、`asn`、`as_org`、`as_handle`、`as_desc`；主地点 `country` / `country_name` / `region` / `city`（境外以 DB-IP 为准）、`geo_basis`、`lat` / `lon`；`dbs.ip2region`（原始串和中文州名）、`dbs.dbip`（州、城市、坐标、ASN、组织）、`dbs.iptoasn`；`agree`；`conn_type` / `conn_conf` / `conn_basis`；`residential`（固网为 true，机房为 false）；`flags`；`v6`（有 IPv6 时同样一套）；`history`（`distinct_ips`、`observations`、首末时间、`via` 各来源的计数、`changes`、`stable`）。
- `entries[].ipintel`：绑定节点的来源数、24 小时新来源、首位运营商与城市、移动网络占比、标记。入口和节点一一对应，完整分布看节点。
- `detail.json` 的 `nodes.<id>.ipintel.sources`：这个节点上每个来源一行，包括首末时间、连接 / 会话 / 分钟（h24、d14）、用过的客户端标识（最多 4 个）、被阻止数、服务端 `new_source` 事件（id、时间、是否已知悉）。`egress_history.ips` 列出每个观测到的出口 IP：首末时间、次数、来源、关联的上架任务 id 与状态、是否与台账一致。
- **行级字段**加在 `access_recent`、`detail` 的 `recent10` / `top_sources`、`behavior_top.sources`、`extra` 的 `behavior.top_sources` / `timeline_recent` 上：`carrier`、`asn`、`as_org`、`country`、`province`、`city`、`conn_type`、`conn_conf`、`first_seen`、`last_seen`、`sessions`（14 天）、`sessions_h24`。节点行的首末时间和会话数指“这个来源在这个节点上”；全机队的行指来源整体。原有字段一律没动：`top_sources` 原有的 `first_seen` / `last_seen` 和 `behavior_top.sources` 原有的 `last_seen` 都保留原值。任何假来源 IP 都能在 `ipintel.sources` 里按 `ip` 查到完整画像。

### 异常类型（本快照 23 条）

| kind | 含义 | 本快照 |
|---|---|---|
| `foreign_source` | 境外来源（warn） | 1 条：09-15 01:08，新加坡 StarHub 固网（AS55430）连了团队1-01 两次 |
| `new_country` / `new_carrier` / `new_city` | 节点在最初 6 小时基线之后，第一次见到某个国家、运营商或城市。同一来源同一时刻波及的节点合成一条，并带服务端 `new_source` 事件 id | 新运营商 3 条：09-15 武汉移动；09-17 武汉电信 5G，9 个节点；09-20 开封移动，4 个节点。新城市 8 条，例如 09-18 和 09-19 荆门联通分别到 9 个、12 个节点，09-23 15:15 北京联通到 8 个节点 |
| `multi_location_node` | 节点 24 小时内从 ≥2 个省或 ≥3 家运营商接入 | 1 条，10 个节点（最多 3 省 3 市） |
| `multi_location_client` | 同一个 3x-ui 客户端标识 14 天内从 ≥2 个省接入 | 1 条，21 个标识 |
| `fanout_source` | 一个来源 24 小时内接入 ≥10 个节点 | 2 条：郑州联通 32 个节点，武汉电信 5G 10 个节点 |
| `datacenter_source` | 机房来源 | 2 条：线路机1、线路机2 自己的地址（AS979 NetLab Global）分别连过 17、28 个节点，连接很少，像链路探针 |
| `db_conflict` | 两库位置或归属不一致 | 1 条，14 个来源 |
| `egress_ip_change` | 出口 IP 换过 | 2 条：团队1-29、团队1-30 在 09-18 上架时先用了另一个 AT&T 地址，当晚重新上架后换成现在的地址（同一 ASN）。证据是上架任务 id |
| `egress_geo_conflict` | 两库给出的出口所在州不一致 | 33 个节点 |
| `egress_stable` | 出口 IP 没变 | 39 / 41 |

### 真实分布（快照时刻）

来源 IP：14 天窗口（实际 8.7 天）35 个，24 小时 10 个。下面的百分比都是连接数占比。

| 维度 | 14 天 | 24 小时 |
|---|---|---|
| 运营商 | 中国联通 89.60%（16 个地址）· 中国电信 10.37%（13）· 中国移动 0.02%（3）· NetLab Global 84 次（线路机自身，2）· StarHub 2 次（1） | 联通 84.24%（4）· 电信 15.75%（5）· NetLab Global 6 次 |
| 省份 | 河南 89.27%（10）· 湖北 10.73%（21）· 美国 84 次 · 北京 8 次 · 新加坡 2 次 | 河南 83.89% · 湖北 16.10% · 北京 8 次 · 美国 6 次 |
| 城市 | 郑州 89.24% · 武汉 10.65% · 荆门 0.06% · 开封 0.02% · 鄂州 0.02% · 洛杉矶 84 次 · 黄冈 9 次 · 北京 8 次 · 新加坡 2 次 | 郑州 83.89% · 武汉 16.07% · 鄂州 0.03% · 北京 · 洛杉矶 |
| ASN | AS4837 89.60% · AS137266 10.35% · AS131285 · AS24445 · AS979 · AS4134 · AS9808 · AS55430 | AS4837 · AS137266 · AS131285 · AS979 |
| 接入方式 | 固定宽带 89.62%（20 个地址，其中 7 个凭换址节奏升到中置信度）· 移动网络 10.37%（13 个，全是电信湖北 5G ASN，高置信度）· IDC 84 次（线路机自身） | 固定宽带 84.24% · 移动网络 15.75% |

- 团队1 14 天内联通占 78.39%、电信占 21.55%。团队2 几乎全是郑州联通，3 个地址每 3–4 天轮换一次。线路机2 上电信占 40.5%，主要来自团队1-01，它的来源以武汉电信 5G 为主。
- 郑州联通有两条互不相干的地址链：团队1 一条，4 个地址，分别用了 1.5、2.9、2.9 天后换址，间隔约 10 秒；团队2 一条，3 个地址，分别用了 4.0、3.1 天后换址。
- 出口：37 个在 AT&T AS7018（ATTVPS 29 个 + NodeMach 8 个），1 个在 Telmex AS8151（OhnoICU 墨西哥），3 台线路机在 AS979 NetLab Global。DB-IP 把其中 40 个放在加州（尔湾 30、弗里蒙特 7、洛杉矶 3），另 1 个在墨西哥格雷罗州。ip2region 给出的是：得州 22、加州 8、佐治亚 5、华盛顿特区 2，纽约、密苏里、伊利诺伊、新莱昂各 1。团队2-01、2-02 的 IPv6 也在 AT&T（DB-IP 说亚利桑那州凤凰城，ip2region 说得州）。线路机2 的 IPv6 是 ULA 内网地址。

## 演示上架批次（`onboarding.demo_batch`，`demo: true`）

8 行，按 `research/onboarding/pipeline-spec.md` 的 16 步（P1–P16）和 7 个阶段列（`phases`）组织；`brief_columns` 给出简报 §3.6 的 10 列映射。供应商、套餐、价格、带宽、线路机与下一个可用端口按真实形态构造，IP（198.51.100.201–208）、服务号、指纹都是虚构的。步骤耗时取自 pipeline-spec §8.2 的估算和 09-23 的实测。

| 行 | 状态 |
|---|---|
| 团队1-37、团队1-38（ATTVPS → 线路机3） | 16/16 完成（37 是金丝雀） |
| 团队1-39（→ 线路机2） | P13 线路机接入进行中，线路机批次等待窗口剩 42 秒 |
| 团队1-40（→ 线路机2） | P6 重装验收进行中（写盘 3:12） |
| 团队1-41（→ 线路机2） | P2 等待输入：密码被拒 |
| 团队1-42（→ 线路机3） | **失败**：P6 重装验收超时 20 分钟 |
| 团队1-43（→ 线路机3） | **风险行**：P1 主机指纹冲突，“主机公钥与上次不一致，已停止，未发送密码”（`risk` 字段含两把假指纹） |
| 团队1-44（NodeMach → 线路机1） | 代理产品，P2–P9、P11 跳过，其余完成 |

另有 `kpis`（总数 8 · 完成 3 · 进行中 2 · 需处理 2 · 失败 1 · 预计剩余）、`column_summary`（每列 `done/applicable`）、`relay_groups`（每台线路机的合批状态、Xray 重载次数与影响说明）。

## 脱敏

- 所有 IPv4 都换成 `198.51.100.0/24` 或 `203.0.113.0/24` 里的假地址，同一个真实 IP 始终对应同一个假 IP（出站 tag `ss-198-51-100-11` 里的点分形式也一起换了）。线路机是 `203.0.113.10/20/30`；落地节点出口 IP 从 `198.51.100.11` 起按团队、编号顺序分配；其余（来源 IP、历史冲突 IP、内网 IP、目标 IP）从 `203.0.113.40` 起分配；`198.51.100.200–254` 只给演示批次用。IPv6 换成 `2001:db8::/32`。
- 删除：VLESS client UUID（连接凭据）、分享链接、Komari agent token、密码（包括任务参数里的 SS 与 SOCKS 凭据）、主机公钥与 SSH 命令、API 密钥前缀与哈希、邮箱。
- 替换：资产 UUID → 可读 id；Komari UUID → 每次构建随机生成的假 UUID（盐值不保存，不能反推）；备注里的其他 UUID（如 VirtFusion 服务器号）→ 假 UUID；凭据前 8 位 → 演示值；Komari 管理员的用户 UUID → `admin`。
- 保留真实值：名称、团队、线路机、端口、供应商、服务号、套餐、价格、日期、全部指标与时间序列、访问目标域名、3x-ui 客户端标识（如 `node-01`，不是邮箱）。
- 主机公钥指纹（审计里线路机接入记录的 `SHA256:…`）换成假指纹，末尾标“（假）”。
- 构建后脚本逐个文件扫描过：没有文档网段以外的 IPv4、没有原始 UUID、没有 IPv6 真实地址、没有 token/密码/公钥/真实指纹/分享链接。
- IP 情报沿用同一张假地址映射。富化脚本在 a395 上重放 `build.py`，得到原来的映射，再逐个 JSON 路径核对：三个文件共 2,148 处假地址，一处不差。35 个来源 IP 中，18 个原快照里已经有；另外 17 个按首次出现顺序，接着分到 `203.0.113.72`–`203.0.113.88`。出口历史里的两个旧地址（团队1-29、团队1-30 上架时用过的）原快照已有（`203.0.113.53`、`.54`）。
- IP 情报保留真实值：运营商、ASN、AS 组织与名称、国家、省、市、城市中心点坐标、接入方式、两库原始结果。富化脚本和合并脚本输出前都做了泄漏扫描：没有文档网段以外的 IPv4 或 IPv6，也没有任何一个真实地址的字符串。

## 来源

全部只读，在测试服 a395 上以 `mode=ro&immutable=1` 打开，查询总耗时约 1.5 秒：

| 来源 | 用到的内容 |
|---|---|
| `~/team-ops-eval/teamops/db/operations-backup-20260923.db` | assets、relays（三台线路机最近一次采集快照）、meta（monitor、hosts、collector、backup、host_traffic_quality）、incidents、renewals、jobs、audit、health_hours、minutes（24 小时）、hours、activity（按 `activity_asset_time` 索引取每节点 10 条）、behavior_cache、behavior_events、destinations、sources_seen、fleet |
| `~/team-ops-eval/data/komari.db` | clients（系统、硬件、版本、标签、流量额度，不含 token）、ping_tasks、configs（站点名与描述） |
| `~/team-ops-eval/data/metrics.db` | metric_rollups：5 分钟桶 24 小时（主机指标与探测延迟、丢包），1 分钟桶最近 2 小时（延迟条的 20 个样本）。这份副本比运营库晚约 1.5 小时，已截到 `as_of` 为止 |

IP 情报另外读了（同样只读）：`activity` 全表顺序扫描一次（`NOT INDEXED`，2,651,074 行，约 4.6 秒，`nice -n 10`），以及 `sources_seen`、`behavior_events`（`new_source`）、`audit`（登记、编辑、状态翻转证据里的实际出口 IP）、`incidents` 证据、`jobs` 参数与验收结果、`meta.monitor` 当前检查、Komari `clients.ipv4/ipv6`。四个 IP 库见“IP 情报 · 数据库”。

口径与 team-ops 后端一致：今日/本月按北京时间零点和 1 号从 `hours` 表求和；速率来自线路机采集快照的 Xray 入口计数差；可用率按 `monitor.availability_windows`（成功 ÷（成功 + 失败），覆盖率 =（成功 + 失败）÷ 应有样本）；到期天数按资产 `billing_timezone` 的日历日。24 小时分钟和与 `hours` 表的今日合计相差 0.03%。

## 已知缺口

1. **快照时刻 41 台全部正常**，需处理 0、未知 0、7 天内到期 0（最近的是线路机1，8 天，LA 时区）。简报里“需处理 3”“7 天内到期 4”是示例，不是真实值。要演示有内容的“需要关注”和非正常状态，用 `status_replays`（真实发生过的时刻）和 `incidents_recent`。
2. 监控从 2026-09-15 开始，所以 30 格 tracker 前 21 格是 `none`（未监控），本月流量也从 09-15 起算（`month_partial: true`）。
3. 快照时刻没有未绑定入口、停用入口和直连入口（线路机3 以前的 `direct:` 入口已经不在了），入口表的“未绑定”“停用”只能靠演示。
4. 线路机流量额度的“已用”取 Komari 网卡发送累计，只有线路机1 按供应商截图校准过；线路机2、3 从 agent 开始计数。
5. 延迟条（`strip_*`）来自 Komari 探测任务（TCP 任务在线路机上执行，出口任务在节点或线路机代发的探针上执行）；“延迟”列的数值来自 team-ops 监控（线路机路径探针）。两个来源测的是同一段链路，但数值会有差异，这是真实情况。
6. NodeMach 8 台没有主机数据（`host: null`），靠线路机代发存在状态；OhnoICU 墨西哥 NAT 机只有 1 核 / 213 MiB 内存。
7. `ss_method` 只对经流水线上架的节点（团队1-21 起）有值；旧流程任务的操作者无法从数据里确定，`operator` 为 `null`。
8. 行为页“访问目标”全局去重数服务端没算，这里给的是按节点去重后求和（`targets_note` 已注明）。
9. 另有 1 条 Komari 主机记录没有对应资产（09-18 起离线），已排除，只在 `system.orphan_hosts` 里计数。
10. 每分钟的监控样本（`health_samples`，含每分钟的 TCP/出口/入口到出口耗时）已抽取但没有输出；详情页如果需要分钟级监控曲线，可以在 `build.py` 里加上。
11. IP 情报的 14 天窗口实际只有 8.7 天（活动表从 09-14 23:35 开始）。分布按连接数算，不是流量：access log 里没有按来源的字节数。
12. 接入方式是启发式判断。中国三家运营商的骨干网和省网 ASN 家宽、手机混用，所以 35 个来源里有 12 个只能给低置信度。要准确区分蜂窝和家宽，需要商业库（例如 IPinfo、MaxMind Connection Type），这里没有用。
13. DB-IP Lite 对中国地址常常只给到区县或街道，有时还会错到北京（湖北联通的地址被放在北京西城），所以中国地址以 ip2region 为准。ip2region 对美国地址多给注册地所在州（AT&T 的地址常落在得州），所以境外地址以 DB-IP 为准。两个库都没有住宅 / 机房字段，出口的 `residential` 是按 ASN 判断的。
14. 出口验证每 5 分钟跑一次，但结果不落库。出口 IP 的历史只来自登记、编辑、上架任务、状态翻转、故障证据、当前检查和主机探针上报，共 377 条。
15. 线路机3 已不存在的 8 个 `direct:` 入口，14 天内有少量连接，都来自线路机自身的地址。这些连接只计入 `breakdowns.direct_history` 和团队“未分配直连”，不属于任何节点。
16. 服务端没有给出全机队按来源去重的会话数。`sessions` 是按上面的 30 分钟口径在本地算的，团队、线路机、全机队各自单独计算（同一来源在不同范围里各算一次）。

## 重新生成

```sh
# 1) 在 a395 上只读抽取（原始数据含真实 IP，只放在临时目录，用完删除）
ssh <a395> 'nice -n 10 python3 -' < tools/extract.py | gzip > /tmp/raw.json.gz   # <a395> = 测试服 a395 的 SSH 地址
gunzip /tmp/raw.json.gz
# 2) 脱敏并生成（本机 Python 3.9+）
python3 tools/build.py /tmp/raw.json .
rm /tmp/raw.json
```

每次构建的假 UUID、凭据前缀和演示指纹都会变（盐值随机）；IP 映射按固定顺序分配，重建后保持一致。

IP 情报在 `build.py` 之后跑（重建会覆盖掉 `ipintel`，重跑这两步即可）：

```sh
# 3) a395 上离线富化：extract + 重放 build.py 得到同一张假地址映射，并与已发布文件逐路径核对，
#    然后查四个库、扫描 activity，只把假地址结果传回来（a395 的 tmpfs 临时目录跑完即删）
sh tools/ipintel_run.sh <a395> /tmp/ipintel.json
# 4) 合并进 snapshot / detail / extra（幂等：先删掉上次加的字段；原有字段不改），并做泄漏扫描
python3 tools/ipintel_merge.py /tmp/ipintel.json .
rm /tmp/ipintel.json
```

第一次在新机器上跑之前，把四个库下载到 a395 的 `~/.cache/team-ops-ipintel/`：ip2region 的 `data/ip2region_v4.xdb`、`data/ip2region_v6.xdb`（raw.githubusercontent.com/lionsoul2014/ip2region/master/…），DB-IP 的 `dbip-city-lite-YYYY-MM.mmdb.gz`、`dbip-asn-lite-YYYY-MM.mmdb.gz`（download.db-ip.com/free/，解压），iptoasn 的 `ip2asn-combined.tsv.gz`（iptoasn.com/data/）。
