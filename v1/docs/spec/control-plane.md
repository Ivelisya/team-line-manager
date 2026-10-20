# 团队线路管理：旧控制面功能与业务规则清单（v3 重写依据）

> 范围：旧后端 `backend/fleet`（Rust fleetd + SQLite）与旧前端 `frontend/prototype`（原生 JS）。
> 目的：Rust + PostgreSQL + React 重写时，保留所有仍需要的能力及其隐含语义。
> 约定：引用格式为 `文件:行号`（路径相对仓库根）。标注 **【丢弃/重新设计】** 的条目是偶然复杂度或疑似缺陷，理由写在同一行。标注 **【待核实】** 的是由代码推导、没有跑旧系统验证的结论。
> 不在本文范围、重写时直接丢弃：Komari 兼容层（`komari.rs`、`komarinative.rs`、`/komari/*`、Komari 会话回落、`komari_*` SSE 资源、Komari 哈希导入）、team-ops 旧接口字节级兼容（`compat/py.rs`、`pyjson.rs`、`legacy_schema.json`、`/v2/api/legacy/*`、`/ops/api/*` 的 Python 序列化逐字节兼容）、旧后端影子比对（`shadow.rs`、`shadowcmp.rs`、`/v1/shadow/legacy`、`/v1/health/shadow`、下线闸门 `decommission.sh`）。它们都存在，重写不要带。

仓库里的主要来源：`backend/fleet/API.md`（契约）、`backend/fleet/README.md`、`fleetd/src/*.rs`、`fleetd/src/compat/*.rs`（旧 Python 逻辑的逐行移植，真正的业务规则在这里）、`crates/health`、`crates/ipintel`、`crates/onboard`（上架流水线，不在本次清单的指定来源里，只在 §1.6 摘要）。`fleetd-ingest`（边缘上报入库、原生主机告警阈值）不在仓库内，相关阈值无法核实。

---

## 0. 系统概览与数据流

1. 采集端：每台线路机跑 `relayd`，每台带主机的落地节点跑 `nodeagent`，按 agent 令牌 `POST /v2/ingest/{node,relay}`（`API.md:396-401`）。relayd 上报 Xray 计数、连接数、访问日志（`fleet.access.v1`，`API.md:490-496`）、TCP/出口/入口到出口探测；线路机还通过 `fleet.relayspec.v1` 长轮询取期望配置（`API.md:461-488`）。
2. 中控 fleetd 每 10 秒一拍「喂数」（`API.md:509`）：把原始上报写入「旧库」（minutes/hours/relays/activity/destinations/…），随后在旧库上评估健康（`monitor.evaluate`，`monitor_eval.rs:608`）、行为检测（`behavior_write.rs:246`）、告警（`rules.rs:204` → `alerts.rs:457`）。
3. 浏览器：`/v2/api/stream` SSE 推「资源变了」，页面按 `path` 重取；有 SSE 客户端时后台提高轮询节奏（`API.md:36-40,93`）。
4. 存储：两个 SQLite（`fleetd.db`：告警/通知/审计/账户/会话/探测/relayspec/目录；`legacy.db`：资产/流量/活动/健康/行为/故障/草稿/版本/API 密钥/续费），外加 ipintel 自己的 SQLite（`ipintel.db`）和 ingest.db。**【丢弃/重新设计】** 重写用一个 PostgreSQL，审计只保留一份（旧系统有两份：`fleetd.db audit`（带哈希链，`store.rs:37`）与 `legacy.db audit`（无链，`compat/writes.rs:12`），详见 §7）。
5. 资产模型：节点短名 `团队1-01`→`t1-01`，线路机 `线路机2`→`r2`（`rules.rs:76-91`）。「团队」= 客户/小组分组；线路机的 team 固定为字符串 `线路机`（`compat/store.rs:11`）。资产三类：主机型落地（ATTVPS 等，有 nodeagent）、代理型落地（NodeMach/PH 双 ISP，SOCKS5 账号，无主机）、线路机。数据量：约 41-42 项资产（3 台线路机 + 38-39 个落地）、6-7 个规范套餐、每台线路机十几个入口（`README.md:18`、`frontend/prototype/data/plan-catalog.json:274-316`）。

---

## 1. 页面逐页清单

导航（`frontend/prototype/original-refined/js/app.js:23-48`）：首页 / 运营总览 / 节点 / 线路 / 节点行为 / 运维（上架流水线、事项与告警、套餐与配额、API 密钥）/ 通知（告警通知）/ 账户（账户与安全）。原型里还保留了 Komari 原生页入口（主机配置、主题、插件、系统……），生产构建用 `liveNav` 过滤掉（`app.js:54-63`），**重写不做**。

全局外壳行为：
- hash 路由 `#/页面?参数`，所有筛选/排序/分组/抽屉写进 URL，可分享（`app.js:236-248`）；深链 `#/lines/r1`→`#/lines?relay=r1`，`#/nodes/t1-01`→`#/nodes?node=t1-01`（`app.js:275-276`，告警通知里的链接依赖它，`alerts.rs:789-798`）。
- 主题 light/dark/system、宽松/紧凑密度（仅运营页）、命令面板 `⌘K`（节点/线路机/入口/套餐搜索 + 跳转 + 切换主题/密度 + 当前节点的「复制 IP / 重新验证 / 重启 Xray」，`app.js:482-517`）、快捷键（`g h/o/n/r/b/p/a` 跳页、`j/k` 行间移动、`[ ]` 抽屉切上/下一个节点、`/` 聚焦搜索，`app.js:481,527-537`）。
- 顶栏事项计数：`GET /alerts?state=unacked&limit=1` 的 `counts`，进行中且未确认数 = `firing - acked`；有 critical 红色否则黄色；点击去事项页（`app.js:566-581`）。
- 数据鲜度条：按各来源年龄 / 预期间隔：≤2 倍「实时」、2-5 倍「延迟」、>5 倍「数据过期/已断开」；预期间隔 monitor 10 s、xray 10 s、panel 15 s、komari 2 s（`shared/format.js:583-602`）；连续 3 次读取失败算断开（`app.js:558`）。
- 首屏 15 秒没有数据 → 超时错误页 + 重新加载（`app.js:600-614`）。401 → 整屏登录页（`login.js`）。
- 会话失效时清掉 localStorage 里的 stale-while-revalidate 缓存（`swr.js`、`login.js:34`）。

### 1.1 首页 `#/home`（`pages/home.js`）
- 页头：鲜度 + 视图切换（大卡片/小卡片/紧凑，存 localStorage `refined:home:view`，快捷键 1/2/3）。
- 4 张 KPI（按当前团队/地区/套餐筛选后的可见节点算，`home.js:98-116`）：
  1. 探针在线 `on / 总数`，副行离线、未知数。「在线」= 探针/代发探针在线（`agent.online`），**不代表业务可用**（`node-model.js:29-33`）。
  2. 网卡速率（所有主机网卡 rx+tx 之和；线路机与落地相加所以业务会重复计一次，口径写在 tooltip）。筛选内无主机时显示「筛选内的节点都没有主机」而不是 0。
  3. 网卡累计流量（点击 action → `#/nodes?sort=-month`）。
  4. 每月费用 = `Σ billing.monthly_cents`（USD），副行「N 项服务」（action → `#/nodes?sort=-cost`）。
- 控制条：团队分段（带计数，零计数禁用）；排序菜单（默认 / 名称 / 网卡速率 / 网卡累计，再点同项翻转方向）+ 分组（不分组 / 按套餐 / 按线路机）；筛选条：地区 chips（多选，>1 个地区才显示）+ 套餐多选菜单 + 清除。
- 排序规则：**离线节点始终沉底**；缺测值排最后；默认键 = `teamOrder*10000 + 编号`；按速率/累计排序时行序每 5 秒最多重排一次（FLIP 动画），鼠标悬停或键盘焦点在网格内时暂停重排（`home.js:65-84,343-369`）。
- 卡片网格（`node-card.js`）：三种节点（主机/家宽代理/线路机）× 三种视图。卡内容：探针圆点（在线/离线/未知）+ 地区旗 + 名称 + 「离线 N 分钟」、IP（可复制）+ IPv4/IPv6 标记、入口（线路机:端口，线路机自身为「主节点:端口」）、套餐（规范名，可点开弹层：规格摘要、价格、周期、同套餐台数、「只看这个套餐」）、带宽、流量上限、4 条资源进度线（CPU/内存/磁盘/负载，阈值取节点 limits 默认 90/90/85，负载每核 100%；≥阈值红、≥0.9×阈值黄，`format.js:287-292`、`node-model.js:119-132`）、网卡速率与累计（主机型）或 Xray 入口「实时/今日/本月 ↓↑」小表（代理型，含来源与年龄，120 秒内算新鲜）、流量额度条（≥90% 黄、≥100% 红）、延迟心跳 20 格 + 「延迟 X ms · 丢包 Y%」、持续运行、到期倒计时、标签（供应商 + 标签 + 线路机名）、月费 chip、健康 pill + 带宽/利用率 chip、按钮「访问记录」「管理节点」（打开抽屉对应页签）、今日流量弹层（悬停 150 ms 预览、点击固定）。
- 分组行显示「N 台 · 小计月费」。
- 空状态：无节点 →「去上架流水线」；筛选无结果 →「清除筛选」。

### 1.2 运营总览 `#/dashboard`（`pages/dashboard.js`）
- 页头：鲜度、密度、「新增家宽节点」（打开上架向导）、刷新（实时模式真的重读 overview+health）。
- 4 张 KPI：正常 `ok / 总数`（副行 故障·降级·维护中·未知 或「全部正常」）；业务速率（Xray 入口速率之和，**采样不全时标「部分」并只含已采样入口，覆盖 `已采/应采`**）；今日业务流量（北京时间 0 点起）；每月费用。
- 面板「团队」表：每团队一行（含「线路机」）：台数/有活动数（≥8192 bps=1 KiB/s 算活动，`data.js:15,237`）、状态 pill 计数（只列非零）、业务速率（↓/↑，不完整标「部分」）、今日、本月业务流量、月费；整行点击：团队→`#/nodes?team=`，线路机→`#/lines`。
- 面板「业务速率排行」前 5（切换「速率/今日」，URL `rank=today`）：速率口径按 ↓ 下行排序；始终补满 5 行（用 0 速率节点补）；5 秒节流重排。
- 面板「需处理」+ 小节「未知」：口径只用 `store.attention()`（见 §2.4）；列：节点、检查项、状态、原因、持续；「在节点页查看」→`#/nodes?status=attention`。
- 面板「24 小时业务流量」图：1h/6h/24h 切换，1 分钟桶，↓ 填充 ↑ 仅线，峰值标记，缺测 N 分钟，表格视图切换。
- 数据：`groups`（`compat/store.rs:369-401`）按 team 汇总 `count/monthly_cents/daily_bytes/monthly_bytes/active/rate_*`；`rate_complete = rate_sampled == rate_expected` 才给汇总速率。

### 1.3 节点 `#/nodes`（`pages/nodes.js`、`shared/data.js:537-653`）
- 页头：密度、新增家宽节点、**导出 CSV**（当前视图；列：节点、IP、团队、入口、套餐、带宽、状态、状态说明、TCP ms、出口 ms、近30天成功率%、采样覆盖%、下行/上行 bps、本月/今日业务流量 B、CPU%、内存%、月费 USD、到期日、剩余天数、账期时区；`=+-@` 开头文本前置 `'` 防公式注入，`nodes.js:562-589`）、刷新。
- 4 张 KPI：正常、需处理（可点：切换「需处理」分段）、7 天内到期（可点；副行最近一台到期）、每月费用（可点：按月费降序）。
- 状态分段：全部 / 需处理 / 未知 / 7 天内到期（剩余≤7 含已过期）/ 尚未接入（lifecycle=unbound），每段带计数；团队分段；搜索框（支持 token：`team: relay: status: plan: due: provider: region: cap: ip:`，OR 逗号、AND 空格，`ip:` 支持前缀与 `*`，`due:<7d` `due:expired` `due:>30`，`status:attention|stale|ok|degraded|fail|maintenance|unknown`，含自动补全，`data.js:598-642`、`nodes.js:394-460`）；套餐/线路机多选菜单；分组（套餐/线路机/团队，组头带「小计月费 · 本月流量」，可折叠）。
- 表格 11 列、每行两行文字：节点+IP / 团队+入口 / 套餐+带宽·利用率 / 状态 pill+「原因·持续」或「最近检查 N 秒前」/ TCP+出口延迟（失败显示「失败/超时」；出口显示「重试中」）/ 近 30 天成功率+覆盖 / 业务速率（120 秒内才算新鲜，否则「未采样」）/ 本月业务流量+流量上限条或今日 / 主机 CPU·内存微型进度（无主机显示「无主机」chip，数据过期显示「主机数据过期」）/ 月费+到期。所有数值列可排序（URL `sort=-cost`，shift 多列排序），状态排序键 = 健康 rank*1e10 + (需处理?0:1e9) + since。
- 实时更新：只补丁变化的格子；离开筛选的行先淡出 5 秒再收起；行序节流 5 秒；鼠标/焦点在表内暂停。
- 点行 → 节点抽屉（见 §1.7）。

### 1.4 线路 `#/lines`（`pages/lines.js`）
- 页头：新增线路机（线上走 fleet-cutover，不走网页）/ 新增家宽节点（上架向导）。
- 控制条：线路机分段（带入口数）+ 跨线路机搜索（端口、节点、IP、客户端标识、备注、出口 IP、转发目标）。
- 每台线路机一块面板：
  - 头：名称、健康 pill、带宽、接收/发送利用率（取主机网卡）、鲜度（relayd 上报：≤60 s 新鲜、>60 延迟、>180 已断开；旧 3x-ui 面板已退役，`lines.js:214-232`）。
  - 描述行：IP、入口数、节点数、套餐、到期；分项状态：探针 在线/离线、Xray 运行中/已停止、采集 正常/中断（旧：面板 可达/不可达、配置 已核对/待核对）（`format.js:643-656`）。
  - 资源读数：CPU、内存、磁盘、负载、**流量上限**（本期出站用量/额度、剩余、每月 N 日按时区重置）、TCP 连接数 + 网卡速率。
  - 入口表：端口(+协议·安全)、绑定节点（主节点 chip /未绑定 chip /节点链接带健康点）、备注（仅与节点名不同时显示）、状态（启用/停用）、活动（有活动：近 5 分钟有流量或当前有连接 / 空闲 + 上次活动）、TCP/出口延迟（主节点入口取线路机自己的探测）、业务速率 ↓↑、累计用量 ↓↑（重置用量后清零；tooltip 合计与限额）、出口（期望 IP + 实际出口 IP，仅信息）、操作（启用/停用、重置用量）。排序：端口/活动/业务速率/累计用量；每台线路机三个筛选：团队/活动/状态。
  - 展开行：入口标签、客户端标识、连接凭据前 8 位（只显示前缀）、Reality 目标/SNI、转发到、备注、来源 IP（前 3 个带 ipintel 画像）、24 小时来源画像、最近活动、连接数、出口核对、累计合计、Xray 计数；按钮：停用/启用、重置用量（需确认）、编辑备注（≤64 字符）、更换连接凭据（需输入 `线路机名:端口` 确认）、重新验证链路。
  - 脚部：「软件与数据更新详情」折叠（Xray 版本、agent 版本、访问记录是否开启、规则条数、模板哈希、采集耗时、日志积压、relayd 上报与 Stats API 状态）、**重启 Xray**（唯一会断连的操作，需输入线路机名确认，`API.md:446`）。
- 「接入任务」面板（最近 5 条，步骤时间线，失败显示原因与「已按相反顺序撤回」）与「管理操作记录」面板（审计 150 条，按动作/操作者过滤，默认隐藏 `system` 自动记录，行内展开 JSON 详情，备份完整性标签）。**动作中文名表见 `lines.js:49-58`，重写时审计动作表需要一份映射。**
- 写操作都经 relayd 异步 op（见 §1.10 / `API.md:434-448`）：线路机最近 60 秒内没来取过期望状态则 409 拒绝。
- 3x-ui 面板相关逻辑（`panelOk`、`retired`、`retry-panel`、`PANEL_FRESH_S`、「面板不可达」提示）**【丢弃】**：3x-ui 已退役（`lines.js:133-135`）。

### 1.5 节点行为 `#/behavior`（`pages/behavior.js`）
- 页面分段：节点行为 / 访问规则。
- 节点行为：筛选条（团队、节点、时间范围 1h/24h/7d/14d、搜索域名或来源 IP、目标类型 域名/IP、处理结果 已阻止/已转发；即时生效）；4 张 KPI（有活动的节点、连接记录、访问目标=各节点分别去重后相加、已阻止的连接）；页签：域名与 IP / 行为变化（有未读点，**不统计 new_domain**）/ 来源地址（带 ipintel 运营商·城市·接入方式画像）/ 节点概览；表尾分页每页 100；导出（JSON）。
  - 域名与 IP：目标、节点、来源数、连接数（tooltip：24h 条数、累计记录）、处理结果 pill（已转发 N / 已阻止 N）、最近连接（tooltip 首次时间）。
  - 行为变化：事件类型 新来源/连接突增/阻止增多/新域名，行内「标记已查看」（可撤销 toast）。
  - 来源地址：选了节点则是该节点的来源行（客户端标识），否则全机队 top 来源。
  - 节点概览：连接、域名、来源、出口验证延迟、最近活动。
- 访问规则（策略）：左栏选范围（团队/节点），右栏编辑模式 + 域名列表 + 备注；「保存草稿」（乐观锁 revision）→「预览」（每节点 would_block 连接数与前 20 个域名，基于最近 24 小时）→「应用」（异步，每台线路机一个 op）→「规则发布记录」（版本状态：已应用/进行中/已回滚/回滚失败，已应用且非最新版本可回滚）、「丢弃全部草稿」（需 draft_hash）。
- **缺陷**：前端事件类型枚举用 `blocked_rise`（`behavior.js:46`），后端写入的是 `blocked_burst`（`behavior_write.rs:276`）。前端规则模式词 `allow/deny/pause`（`behavior.js:50-53`）与后端 `allowlist/denylist/blocked`（`writes.rs:342`）也不同，靠 `live-policies.js` 适配。**【丢弃/重新设计】** 重写统一枚举。

### 1.6 上架流水线 `#/onboarding`（`pages/onboarding.js`，后端 `crates/onboard`）
- 批次卡（标题、状态、金丝雀、操作者、已用/预计剩余、配方、并发与合批窗口）；4 张 KPI（成功、进行中、待介入、预计剩余，可点过滤）；「线路机合批」表（窗口内到达同一线路机的入口合并成一次 Xray 更新，显示窗口剩余秒数与 Xray 重载/探针重启次数）；「上架矩阵」（行=主机，列=7 阶段或 16 步，单元格=步骤状态；状态筛选、搜索、阶段/步骤列切换、行操作菜单、步骤抽屉：日志/时间线/输入输出）；「全部批次」列表。
- 16 步（`crates/onboard/src/model.rs:8-23`）：P1 预检、P2 推密钥、P3 身份采集、P4-P6 重装（DD 人工闸门：准备/执行/验收）、P7 系统基线、P8 防火墙、P9 Shadowsocks、P10 登记、P11 nodeagent、P12 出站校验、P13 线路机接入（合批）、P14 探针与监测、P15 端到端验收、P16 上线交付；代理产品跳过 P2-P9、P11。可跳过步骤 P7/P11/P14（`onboarding-model.js:501`）。
- 批次动作：确认计划（带 `plan_hash`）并开始、继续（接管中断的批次）、取消批次（逆序补偿）、行级：重试/跳过/改 IP/取消（`API.md:406`）。
- 重写建议：此页是独立子系统，只保留「批次→行→步骤」状态机 + 事件流；演示批次与 `demo` 模式 **【丢弃】**。

### 1.7 节点抽屉（`node-drawer.js`，任何页面通过 `app.openNode(id, tab)` 打开）
- 页签：概览 / 连接质量 / 流量 / 主机（仅有主机）/ 费用 / 访问记录 / 管理；URL `?node=&tab=`；`[ ]` 切换上/下一个节点（顺序取当前页的行序）。
- 概览：综合状态 pill、出口 pill（连通/超时/失败/未知，**不再有「不符」**）、探针状态、团队/供应商、20 格延迟心跳、三项读数（出口验证、上游 TCP、入口到出口 ms）、「查看检查详情」（逐项检查：名称+「关键」标记、状态、原因、数值、测量时间，失败排前）、本月业务流量/近 24 小时/业务速率三张卡。
- 连接质量：近 30 天成功率（30 格日条 + 1/7/30 天成功率与覆盖）、连接路径（入口、协议、Reality、转发到、入口标签、客户端标识、连接凭据前缀、来源地址数）、出口 IP 历史卡（ipintel）、24 小时延迟图（上游 TCP / 出口验证分两图两轴，5 分钟均值）、故障记录（近 30 天，开始/恢复、检查、状态、证据、「标记已知悉」）。
- 流量：今日/本月/近 24 小时峰值、24 小时业务流量图（5 分钟桶，缺测分钟）、今日网卡（主机型：接收/发送/峰值，5 分钟采样）；本月「监控 MM-DD 起，本月不完整」提示（`month_partial`）。
- 主机：系统/内核/架构/虚拟化/CPU/显卡/agent 版本、资源（核数、内存、Swap、磁盘、负载 1/5/15 与每核%、进程、TCP/UDP）、网络（IPv4/IPv6、速率、累计、运行时长、最近上报）、6 张 24 小时负载图（CPU/内存/磁盘/负载/网卡/TCP，1h/4h/1d，断点连线开关；7d/30d 置灰）。
- 费用：套餐+规格+同套餐台数、月费、账期、到期日+剩余天数、账期时区、供应商时区、服务号、供应商后台链接、自动续费、提醒天数、续费记录表（登记时间、金额、原到期、新到期、备注）；按钮「编辑资产」「登记续费」。
- 访问记录：最近 10 条连接（时间、来源 IP+画像、目标 host/协议:端口/域名或仅 IP、已转发/已阻止）、来源画像卡（24 小时/14 天）、来源 IP 表（情报+新来源确认状态）；8 秒没有来源数据则降级为该节点自己的访问记录并给「重试」。
- 管理：启用/停用入口、重置累计用量（需键入节点名确认）、编辑备注、更换连接凭据（键入节点名确认，旧凭据立即失效）、重新验证链路、监控与维护（四个阈值 + 维护截止）、确认故障（批量）。

### 1.8 套餐与配额 `#/plans`（`pages/plans.js`）
- 4 KPI：套餐数/供应商数；资产数/线路机数；每月费用（**前端对 asset.price.amount 求和，仅 cycle=monthly**，`plans.js:50`）；7 天内到期。
- 套餐表（点行展开名下资产：短名、团队、月费、「登记价与目录价不同」提示、到期倒计时+到期日+时区徽标）；线路机流量配额表（额度条、已用 GB/额度 GB、重置「每期第 N 天」、时区、到期、采样时间）+ 口径说明；续费到期表（最近 12 项，由近到远）。
- 5 分钟重取一次（不跟随 SSE，`plans.js:109`）。
- 前端清洗内部备注（`stripInternal`/`INTERNAL` 正则，过滤「待核实/Qn/Komari」，`plans.js:15`、`plans.js(js/):21-35`）**【丢弃/重新设计】**：这是因为套餐目录文件里混着维护者备注。重写把备注放单独字段且不下发给前端。

### 1.9 事项与告警 `#/alerts`（`pages/alerts.js`）
- KPI：进行中（严重/警告数）、未确认（`firing - acked`）、已压住（静默·维护·上级事项）、待定（pending 数 + 提示级数）。
- 影子模式横幅（没配通知渠道时「告警只记录不外发」+ 去通知页）。
- 事项表：状态分段 进行中/未确认/已确认/已结束/全部（`state=open|unacked|acked|resolved|all`）；分组 不分组/按线路机/按原因；列 级别、事项（规则中文名+摘要）、对象（节点或线路机链接）、原因（中文）+ 检查项、持续（resolved 用结束时间）+开始时间、状态（进行中脉冲/已确认/待定/已结束 + 压住原因）、「确认」；展开行：条件开始/触发/确认(人+时间)/结束/通知状态/压住来源 + 全部标签 + 证据 key=value + 「打开节点详情」。
- 「静默与维护窗口」面板：列表（类型、匹配条件「标签 = 值 且 …」、起止、时区、状态 生效中/未开始/已过期/已删除、创建人）+「新建静默」「新建维护窗口」对话框（范围=线路机/节点/团队/供应商/原因，开始时间默认现在，时长 1h/2h/4h/8h/1d/3d/7d，说明 ≤200 字）+ 结束并删除（确认对话框）。
- 实时：SSE `alerts`/`silences` 时重取，30 秒重画持续时间。

### 1.10 通知 `#/notify`（`pages/notify.js`）
- 渠道表（名称+类型、发往=打码目标、最低级别、最近结果、已发送/失败数、连续失败数）；**渠道增删改只能改配置文件**（只读页）；「死人开关已开启」提示。
- 通知记录（保留 90 天）：时间、级别、内容（标题+「分组/重复提醒/每日摘要」+事项数）、渠道、结果（已发送/失败/只记录，延迟 ms）；状态过滤 全部/只记录/已发送/失败；展开看全文正文、错误、group_key、事项 id。
- 重写时应把渠道配置做成数据库表 + UI 管理（现状：配置文件 + 重启，`notify.rs:111-118`）。

### 1.11 API 密钥 `#/api-keys`（`pages/api-keys.js`）与账户 `#/account`（`pages/account.js`）、登录（`login.js`）
- API 密钥：列表（名称+前缀、权限 只读/读写/完整、创建、过期、最近使用+次数、状态 可使用/已撤销/已过期）、最近调用（50 条：时间、密钥名、方法、路径、状态码、来源 IP）、接口说明（导出 JSON）；新建（名称 1-100 字符、权限、有效期 30/90/180 天/长期）、新密钥只显示一次（带复制与调用示例）；撤销需勾选「我确认」。语义见 §7。
- 账户：用户名/登录方式/2FA 状态；修改密码（≥10 字符，其他会话全部作废）；两步验证开启（生成密钥+otpauth 链接→验证器→6 位码确认）/关闭（6 位码）；会话列表（设备=UA 简称、IP、登录、最近活动、到期、「退出这个会话」，当前会话不可自踢）。
- 登录页：用户名+密码，后端回 `totp_required` 才出现验证码框；失败文案与锁定见 §7。

---

## 2. 健康与监控语义

### 2.1 检查项（每个资产一组，`compat/monitor_eval.rs:359-530`，阈值常量 `:44-46`）
| key | 名称 | 适用 | 关键(critical) | 判定 |
|---|---|---|---|---|
| collector | 线路管理通道 | 全部 | 是 | 线路机新鲜(`relay.fresh`)=ok；线路机自身未新鲜=fail；落地节点所属线路机未新鲜=unknown。`relay_fresh`=`-30 ≤ now-received_at < max(120, 3×5)=120 s` 且无 error（`compat/store.rs:237-241`） |
| host | 主机探针 | `host_measured` 或 provider=ATTVPS 或线路机，否则 `na` | 仅线路机 | `host_fresh && online`=ok；`!host_fresh`=unknown；否则 fail(`probe_offline`)。`host_fresh` = hosts 快照与 host.time 都在 120 s 内（`compat/store.rs:345-346`） |
| cpu / memory / disk | CPU/内存/磁盘容量 | 全部（无数据=unknown） | 否 | `value ≥ limit` → degraded；默认 limit 90/90/85（`cpu_percent/memory_percent/disk_percent`），内存与磁盘百分比 = `round(100*used/total,2)` |
| entry | 业务入口 | 有 `relay_port` 的节点 | 是 | 线路机新鲜时：入口启用=ok，否则 fail；不新鲜=unknown |
| mapping | 固定出口绑定 | 同上 | 是 | 期望 `[outbound_tag,target,target_port,protocol]` 与线路机实际入口配置一致=ok，否则 fail；协议名归一：`socks5→socks`、`ss→shadowsocks` |
| tcp | 线路到上游 TCP | 同上 | 否 | 新鲜期 `max(120, 3×30)=120 s`；无 `tcp_ms` 字段=`na`；过期=unknown；有 ms 且无错=ok；否则 fail(`upstream_down`)。**不参与节点状态**（`deciding` 排除） |
| egress | 认证出口验证 | 同上 | 是 | 新鲜期 `max(120,3×300)=900 s`（旧口径；原生探测按 `fresh_after(60)=180 s`）；0.13 起**只看连通**：取到 `actual_ip` 且无 error 即通过，不比对资产 IP（`ip_check=false`）；一周期内最多 3 次尝试，`egress_attempt<3` 的失败记 pending（显示「重试中」，状态 ok，不计丢失）；失败原因由 rc 映射：28→timeout，35/52/55/56→auth，7/97→upstream_down，其余 unknown；`ip_check=true` 时不一致→reason=`egress_mismatch` |
| ingress | 实际入口到出口 | 同上 | 是 | 新鲜期 240 s（配置 `path_fresh_s`；relayd 自适应巡检主入口与异常入口 60 s、健康入口 120 s）；`path.healthy`=ok；否则若访问策略对 `www.cloudflare.com` 判 blocked → degraded(`policy_blocked`)，否则 fail；若 tcp 已失败且 ingress 失败/降级 → reason 改 `upstream_down`。原生探测没有 L3 的节点记 `na`（`crates/health/src/engine.rs:304-309`） |
| latency | 出口请求耗时 | 同上 | 否 | 出口 ms ≥ `latency_ms`(默认 3000) → degraded；仅信息，**不参与节点状态** |
| udp / ipv6 | UDP/IPv6 出口 | 原生探测新增 | 否 | 信息性，只产生检查行，不参与状态（`engine.rs:24,311-319`） |

- 单节点可覆盖阈值（`node_profile`）：`cpu_percent/memory_percent/disk_percent` ∈ [1,100]，`latency_ms` ∈ [100,60000]，超限报「监控阈值超出允许范围」；`maintenance_until` 必须在未来 7 天内（`compat/writes.rs:146-200`）。

### 2.2 去抖状态机（`transition`，`monitor_eval.rs:765-888`）
- 每个检查项持久化 `{status, fails, oks, last, since, history}`。仅当「本次测量时刻(`measured_at`，或 collector/entry/mapping 用 `[status,evidence]`) ≠ 上次」且非 unknown、非 pending 才推进。
- 阈值 `(fail_after, recover_after)`：ingress/egress=(2,1)，host=(2,2)，其余=(1,1)。ok：`oks+1`，`oks≥recover_after`→ok；若原是 fail 且未达恢复数则先降为 degraded。非 ok：`fails+1`，`status=="fail" && fails≥fail_after`→fail，否则 degraded。
- 「不稳定」：仅 egress/ingress，保留最近 6 次(0/1)历史，状态为 ok 但翻转次数 ≥3 → 强制 degraded，reason=`unstable`。
- 状态变化只对 **critical 检查**写审计 `health_flip`；节点级状态翻转（ok/degraded/fail 之间）也写（`:659-668`）。
- 维护中：状态与 since 冻结为上次值，不推进、不写审计、不开故障（`:779-797`）。

### 2.3 节点状态推导（`deciding`，`monitor_eval.rs:333-343,507-517`）
按优先级取第一条命中：① 任一 **critical** 检查为 fail → `fail`（reason 取该检查的 reason）；② 否则任一非 tcp/latency 检查为 degraded → `degraded`；③ 否则任一 critical 检查为 unknown → `unknown`；④ 否则 `ok`。维护窗口（`maintenance_until > now`）时 status 覆盖为 `maintenance`。`availability`：fail→0，unknown→null，其余→1。
- **【待核实/疑似缺陷】** `deciding` 对 `fail` 要求 critical，对 `degraded` 要求状态恰为 degraded；非关键检查（节点的 host、cpu/memory/disk 的 fail）去抖后一旦变成 `fail` 就既不是 fail(非 critical) 也不是 degraded，节点会回到 ok；只有刚失败的第一拍是 degraded。host 离线由独立的 `host_offline` 告警兜底，所以实际影响有限，但状态机语义不自洽。重写时明确：非关键检查最多产生 degraded。

### 2.4 UI 的状态词与「需处理」
- 健康词（`format.js:606-613`，rank 越小越靠前）：故障(bad,0)、降级(warn,1)、未知(unknown,2,空心点)、维护中(info,3)、正常(ok,4)、不适用(5)。
- **需处理** `attention` = 开着的故障（critical 检查 fail，或 cpu/memory/disk 降级且 `now - since ≥ 120 s` 开出的 incident）∪ 降级持续 ≥300 s（`ATTENTION_DEGRADED_S`），再去掉维护中节点（`monitor_eval.rs:694-707`、`data.js:15,167-173`）。服务端给 `monitor.attention`，前端兜底用同一规则。
- **未知**：节点状态 unknown（关键检查过期、线路机失联）；评估本身超过 5×10 s 没更新则前端加「数据过期」空心点（`data.js:182-185`、`format.js:619-623`）。
- **离线**：只指探针/agent 离线（`agent.online=false`，`node-model.js:29-33`），与健康状态独立；首页排序里离线沉底。
- **维护**：`maintenance_until` 未来 ≤7 天；期间不开 incident、不入 attention、可用率记入 `maintenance` 样本、告警规则 `host_resource`/节点类规则跳过（`rules.rs:287,381`）。
- 状态副行文案（`format.js:712-723`）：retrying→「重试中」；ok→「最近检查 N 秒前」；其余→「原因词 · 持续时间」。原因词表 `format.js:686-694`（auth 入口认证失败、egress_mismatch 出口变更、policy_blocked 规则阻止、probe_offline 探针离线、retrying 出口重试中、stale 数据尚未更新、timeout 探测超时、unstable 链路不稳定、upstream_down 上游不通）。

### 2.5 故障 incident（`monitor_eval.rs:866-886`）
- 开：critical 检查 fail；或 cpu/memory/disk degraded 且持续 ≥120 s。severity：critical 检查→critical，否则 warning。每 (asset,key) 同时只有一条未恢复（唯一索引）。
- 更新：持续期间刷新 `last_seen/severity/evidence`；恢复：检查回 ok→`resolved_at`，审计 `incident_resolved`；检查项从评估集合消失也自动关闭。
- 确认 `incident_ack`：写 `ack_at/ack_actor/note(≤1000)`，找不到「故障记录不存在」，审计（`writes.rs:212-221`）。
- 保留：已恢复 incident 365 天（`monitor_eval.rs:896`）。列表上限 250 条（`monitor.rs:16`）。

### 2.6 可用率与覆盖（`monitor.rs:26-49`、`monitor_eval.rs:680-692`）
- 每分钟一个样本桶（`health_samples`，14 天）与每小时汇总（`health_hours`，400 天）：计数 `ok / unknown / failed / maintenance`；`availability` 为 1 记 ok，0 记 failed，null 记 unknown，维护期记 maintenance；用 `INSERT OR IGNORE` 保证同一分钟只计一次。
- 窗口 1/7/30 天：`since = max(monitor_started:<asset>, now - N天)`；
  `availability_percent = ok*100/(ok+failed)`，**保留 3 位小数**；
  `coverage_percent = (ok+failed)*100/(expected - maintenance)`，**2 位小数**，其中 `expected = max(observed, ceil((now-since)/60))`；
  同时给出 `successful/failed/unknown/missing/maintenance_samples`、`since` 与统计口径版本 `availability_statistic_v2`。unknown 与维护**不计入分母**。
- 30 天日条（`monitor_eval.rs:709-748`）：每天 `coverage = ok*100/(ok+failed)`（2 位，字段名虽叫 coverage 实为当日成功率）+ 当日 incident 数；前端图例：正常 / ≥99% / ≥95% / 故障 / 未监控（`ui/viz.js:191`，分档在 `data/tools/build.py` 中，未审阅）。前端格式化：成功率 ≥100 才写「100%」，否则最多 2 位且**永不进位成 100**（`format.js:241-250`）。
- 重写提示：保存分钟样本 14 天足够；hourly 可改 PG 连续聚合。`monitor_started:*`（该节点监控起点）必须保留，否则新节点的覆盖率会失真。

### 2.7 延迟条与探测历史（`probes.rs`、`API.md:301-347`、`ui/viz.js:117-148`）
- 三类探测样本：`tcp`（线路机到落地 SS 端口）、`egress`（认证出口验证）、`path`（实际入口到出口）。每次喂数按 `measured_at` 去重入库：原始样本 24 小时 + 5 分钟桶（`n, ok, bad, retry, sum_ms, min, max`）。
- **失败/超时/不一致只记状态，ms 为 NULL，绝不当延迟值**。状态：ok / fail / timeout（rc=28 或错误含 timed out/timeout）/ mismatch / retry（认证出口一周期内未到最后一次的失败，不计丢失）。
- `loss_pct = (fail+timeout+mismatch)/(samples - retry)`；avg/min/max 只统计 ok；空桶不出现，由前端按 step 补空。API：`/v1/probes/latest`（每节点每类最近 20 个，可 `?since=` 增量，带 ETag）、`/v1/probes?node=&range=1h|4h|6h|12h|24h&kind=&raw=1`（raw 每类最多 5000 条）。保留 `now - 86400 - 600`（`probes.rs:222-227`）。
- 线路机自身探测（r1/r2/r3）：对端 TCP 心跳（被测线路机由另外两台测）+ 经主节点的出口验证（自己测）（`probes.rs:128-190`）。
- UI 心跳条：20 格 = 最近 20 个 TCP 样本；格色 = 丢失→红，否则按 TCP 阈值着色（≤100 ms 正常、≤300 ms 黄、>300 红，`format.js:296-324`）；说明行「延迟 中位数 · 丢包 %」；没有 TCP 样本但有链路证据时显示「出口验证 X · 上游 TCP Y」；都没有则空格。延迟显示：<1 ms →「<1 ms」；<1000 ms 整数 ms；≥1 s 两位小数 s；失败/超时显示词而不是数字（`node-model.js:14-24`）。
- 采样分辨率：有人看 10 s、无人看 60 s；TCP 约 10-30 s 一次、认证出口约 5 min 一周期、入口到出口约 60 s（`API.md:314`）。
- 原生探测引擎额外语义（`crates/health/src/engine.rs`，是未来的"新输入"）：`probe_target_down`（两个公共来源控制路径都失败→egress=unknown，不计入可用率）、`correlated_node_failure`（同线路机/供应商 ≥3 个且 ≥80% 节点 egress fail、而线路机控制路径正常，则原因码改为关联故障，状态仍 fail）、控制路径结果 150 s 内有效。

### 2.8 线路机状态（`rules.rs:320-353`、`crates/health` 之外）
- 线路机分项：探针 / Xray / 采集 / （旧：面板、配置）。`relay_down`、`relay_mgmt_down`、`relay_xray_down` 见 §3.2。
- 访问策略对健康的影响：策略 `blocked` 对探测域名时 ingress 降级为 `policy_blocked` 而非故障（`monitor_eval.rs:477-480`）。

---

## 3. 告警引擎

### 3.1 对象与状态机（`alerts.rs`）
- 实例 = `rule + labels` 的指纹（`fingerprint`，`:229-238`，所有标签都参与，包含 `cause`）。状态 `pending → firing → resolved`，另有 `ack`。严重度 `info < warning < critical`（词：信息/警告/严重）。
- 新条件：`since = active_since(数据源给的开始时刻，取 min(now)) 或 now`；`now - since ≥ for_s` 直接 firing，否则 pending（数据源时刻使重启不重新计时）。已存在则更新 severity/summary/evidence，`pending` 满 `for_s` 转 firing（`:457-547`）。
- 条件消失（在本次评估 `scope` 内且不在 `hold` 的节点中）→ resolved；pending 就消失 → 静默结束（不通知、不进摘要）；firing 且已通知过 → 结束并发「已恢复」；未通知过（被压住/没到分组等待）→ 只进摘要（`:519-545`）。
- `scope`：每次评估声明"这次真的评估了哪些规则"，不在 scope 的规则既不新建也不恢复（数据源不可用时保持原状）。`hold`：状态未知的节点上已有告警保持，不因没数据而恢复（「没有数据 ≠ 恢复」，`:455-457`）。
- `ack`：只写 `ack_at/ack_by`（首次生效），已结束 409「告警已结束，无需确认」，不存在 409「告警不存在或已结束」；确认后停止重复提醒；**不**影响恢复判断（恢复仍由真实测量决定）（`:549-561`、`alerts.js:157`）。
- **【丢弃/重新设计】** 指纹含 `cause`：同一节点故障原因在 `auth→timeout` 间变化会旧告警 resolved、新告警 firing，重复通知；`quota`/`renewal_due` 的 `threshold` 标签每升一档也新建一条、旧的静默结束（`notify_resolved=false`）。重写建议指纹 = rule + 对象，原因/档位放 annotation，档位变化走「升级」而不是新实例。

### 3.2 全部规则（`rules.rs`；默认值 `config.rs:193-231,287-312`）
| rule | 级别 | for | 触发条件 / 作用域 |
|---|---|---|---|
| `node_health_fail` | critical | 0（`since`取检查 since 或节点 since） | 节点状态 fail；取第一条 `fail && critical` 检查；cause = 节点 reason→检查 reason→检查 key；标签 `check`；`rules.rs:433-453` |
| `node_health_degraded` | warning | `degraded_for_s`=300 | 节点 degraded；取第一条非 tcp/latency/cpu/memory/disk 的降级检查（资源类走 host_resource）；cause 同上或 `<key>_degraded`；`:454-474` |
| `egress_mismatch` | critical | 0 | `EGRESS_IP_COMPARE=false`（`:22`）：**已关闭**，保留在 scope 里只为让旧告警恢复 **【丢弃】** |
| `host_offline` | warning | `host_offline_s`=120 | 节点：`host_required && host_sync_ok && !host_ok`（`host_ok = online && 上报年龄≤60 s`）；线路机：管理通道正常但主机离线；`host_sync_ok = overview.now - hosts_at ≤ 60`；`:312-401` |
| `host_resource` | warning | 120 | 非维护节点/线路机的 cpu/memory/disk 检查 degraded；cause=`cpu_high/memory_high/disk_high`；摘要含阈值；`:286-310` |
| `host_sync_stale` | warning | 60 | `hosts_at` 距渲染时刻 >60 s（或无）；**此时暂停 host_offline 判断**；全局 `kind=system` |
| `collector_stale` | critical | 120 | `health.collector_at` 距 health 渲染时刻 >60 s；抑制所有节点告警 |
| `monitor_stale` | critical | 180 | `health.monitor_at` >60 s；抑制所有节点告警 |
| `relay_down` | critical | `relay_for_s`=60 | 线路机主机探针离线 **且** 管理通道中断；since = 两者最后时刻较大者；抑制该线路机名下所有节点与线路机级告警 |
| `relay_mgmt_down` | critical | 60 | 管理通道中断（主机在线）；`host_sync` 不可信时只看管理通道（`:338-342`） |
| `relay_xray_down` | critical | 60 | 管理通道 ok 且面板报告 Xray 未运行；原生来源见 `API.md:401`（`xray_process_down`、`xray_api_unreachable`(Stats API ≥60 s 不可达)、`relayd_silent_nodes_failing`(relayd ≥120 s 不上报且名下 ≥2 且过半节点失败)） |
| `correlated_node_failure` | critical | 0 | 同线路机（`scope=relay`）或跨线路机同一供应商（`scope=provider`，要求涉及 ≥2 台线路机且含未被线路机级聚合的新成员）有 ≥`correlate_min_nodes`=3 个关键故障节点，其故障开始时间在 `correlate_window_s`=120 s 滑窗内；成员标 `aggregated:<id>`，只发一条通知；`:477-553` |
| `renewal_due` | `≤1 天`critical / `≤3`warning / `≤7`info | 0 | `renewal_days=[7,3,1]`；天数 = **账期时区日历天数**（`due_calendar_days`，`:120-131`）；阈值档 = 最小的 t ≥ days（已过期落在最小档）；**不发恢复通知**（续费后自动结束）；标签含 `threshold` |
| `quota` | warning / ≥100% critical | 0 | 线路机 `host.net_total_up ÷ (traffic_quota_gb×1e9)`；`quota_start_pct`=80 起每 `quota_step_pct`=5 一档（`quota_step`，`:141-147`）；只保留当前档；不发恢复；`:354-376` |
| `legacy_source_down` | critical | 60 | 数据源 `/overview` 连续 ≥3 次错误或过旧（>3×idle间隔+30 s）；`cause=legacy_unreachable`；另有 `komari_unreachable` **【丢弃】**（`app.rs:1048-1081`）。重写改为「采集管线停滞」 |
| `channel_failing` | critical | 0 | 通知渠道连续 3 次投递失败；`notify.rs:254-276` |
| `relay_egress_mismatch` | critical | — | 已关闭（出口只看连通），**【丢弃】** |
| `relay_tcp_down` | critical | — | 线路机互测 TCP 心跳：每个对端最近两次都失败（`native.rs:460-540`） |
| `relay_probe_stalled` | warning | — | 线路机批次照常到，但自己的 tcp/egress 结果超过 5 分钟没新行 |
| `egress_route_leak` | critical | — | 节点实际出口 IP 等于某线路机公网 IP（流量没走落地）；**唯一保留的出口安全规则**（`native.rs:544-575`） |
| `native_host_offline` | 线路机 critical / 节点 warning | 120 s 无批次 | 启动后 180 s 内与启动宽限内不开 |
| `native_host_resource`、`native_cpu_steal`、`native_cpu_iowait` | warning | — | 阈值在仓库外的 `fleetd-ingest` 里，**【待核实】** |
| `ipintel_new_isp` / `ipintel_new_city` / `ipintel_datacenter_source` | warning | — | 见 §6 |
| `ipintel_shared_identity` | info | — | 见 §6，只进日报 |
| `ipintel_egress_ip_changed` | critical | — | 0.13 起不再有（出口 IP 变化改为自动更新资产 IP，`API.md:572-575`）**【丢弃】** |

**启动宽限**（`rules.rs:46-72`、`API.md:211-213`）：`STARTUP_RULES = host_offline, host_sync_stale, relay_down, relay_mgmt_down, collector_stale, monitor_stale`（以及 `native_host_offline`）：fleetd 启动后 `startup_grace_s`=120 s 内一律不新开、已有的也不恢复；之后直到评估用的数据确实是启动后产生的（收到第一批边缘上报且 hosts_at / collector_at / monitor_at 都晚于启动）才结束宽限，最长 `startup_grace_max_s`=600 s。**重写时在 PG 里持久化的告警与"启动后数据新鲜度"逻辑要一并实现，避免重启造成「恢复→再触发」风暴。**

**新鲜度按数据自己的渲染时刻算**（`rules.rs:208-216`）：评估用缓存里的数据，不能拿评估时刻去比缓存年龄；缓存本身过旧交给 `legacy_source_down`。

### 3.3 分组、通知节奏、重复、摘要（`alerts.rs:646-787`，默认 `config.rs:194-214`）
- 组键 `relay=<线路机或->|cause=<原因，缺省为规则名>`（`group_key`，`:240-244`）。只有 `severity ≥ warning` 且已 fired 的告警参与分组通知；**info 不单独通知，只进每天摘要**。
- `group_wait`：critical 10 s，其他 30 s；组首次变化（新 firing 或待发恢复）从最早变化时刻起算；组「已打开」(`open`，上次发送时仍有 firing) 时再发要等 `group_interval`=300 s；只有恢复没有新增则立刻发。
- 重复提醒：无新变化时，若组内仍有 **未确认、已通知过、未被压住** 的 firing，则 `repeat_critical`=1800 s（含 critical）/ `repeat_warning`=14400 s（否则）后重发一次（kind=`repeat`，文案「重复提醒（未确认）」）。
- 每日摘要：每天 `digest_hour`=9 点（`digest_tz`=Asia/Shanghai）发一次 kind=`digest`，内容：当前严重/警告/信息数、进行中的 info 事项、自上次摘要以来「自行恢复」（未单独通知）的告警（`render_digest`，`:874-932`）。摘要不受渠道 `min_severity` 限制（`notify.rs:140,146`）。
- 压住与抑制（`recompute_suppression`，`:563-635`，每次 tick 与评估后重算；结果写入 `suppressed`）：优先级 ① 命中生效中的静默/维护窗口→`silence:<id>`/`maintenance:<id>`；② `ipintel_egress_ip_changed` 被同节点 egress_mismatch 抑制→`inhibited:<id>`（已过时）；③ 节点级告警被 `collector_stale/monitor_stale`、该线路机 `relay_down`、或 `correlated_node_failure`（`aggregated:<id>`）压住；④ 线路机级告警（非 relay_down）被 `relay_down` 压住，`relay_mgmt_down` 还被全局停滞压住；`correlated_node_failure` 自身不被压。被压住的告警照常记录状态，只是不通知；压住期间自行恢复的进摘要。
- 静默/维护窗口（`Silence`，`:124-147`）：1-8 个 matcher（`name/value/negate`），**全部满足才匹配**（空 matcher 永不匹配）；可匹配标签 `node relay relay_id provider team cause kind asset check channel entry threshold scope`；时间 `starts_at ≤ now < ends_at`；窗口 ≤7 天；时间可写本地时间串（按 `tz`，默认 Asia/Shanghai）、Unix 秒或 RFC3339，或 `duration_minutes`；状态 scheduled/active/expired/deleted；软删除（`deleted_at/deleted_by`）；审计 `silence.create/delete`、`maintenance.create/delete`。静默与维护窗口只有 `kind` 不同，语义完全一样 **【丢弃/重新设计】** 合并为一种「抑制窗口」+ 类型标签。
- 持久化：告警/分组状态由调用方取 dirty 集合落库；已结束且已结算（已发恢复通知、已进摘要）的告警从内存移除（`take_dirty`，`:416-442`）。重启后从库恢复未结束的告警、待发恢复/待进摘要的告警与分组状态（`load`，`:378`）。

### 3.4 通知渠道与格式（`notify.rs`）
- 四种渠道：Telegram（`sendMessage`，正文截断 3800 字节，`disable_web_page_preview`，带「打开」inline 按钮=通知链接）、企业微信群机器人（text，截断 1900 字节，`errcode==0` 才算成功）、SMTP（lettre，`starttls`/`tls`/`none`，纯文本，主题=标题，多收件人）、签名 Webhook（JSON `{kind,severity,group_key,title,text,link,alerts[],ts}`，头 `X-Fleet-Timestamp` + `X-Fleet-Signature: sha256=HMAC-SHA256(secret, "<ts>.<body>")`）。URL 只允许 https（或 `http://127.0.0.1`）；超时 15 s；渠道各有 `min_severity`（默认 warning，`d_min_sev`）；机密一律不回显（chat id 打码 `ab****yz`、webhook 只给主机名）。
- 影子模式：没配渠道或 `notify.shadow=true`（默认 true）→ 每条通知写 `notify_log`（`channel=shadow,status=shadow`，error 写「影子模式：本应发往 …」）；`GET /channels` 带 `shadow:true` 与说明。重写时影子模式作为「渠道 dry-run」开关保留有价值。
- 失败计数：每渠道 `sent/failed/consecutive_failures/last_*`（内存，重启清零）；连续 ≥3 → `channel_failing` critical；一旦成功连续数清零。
- healthchecks.io 死人开关：可选，每 `max(30, healthchecks_interval_s=60)` 秒 GET ping（`app.rs:1186-1206`）。
- 文本格式（`render_group`，`:807-872`）：标题 `[严重] 节点故障 · 线路机3 · 认证失败`（已恢复前缀「已恢复」；线路机缺省写「全局」；cause 用 `CAUSE_LABELS` 中文，`:274-287`）；正文第一行标题，第二行统计「新增 N，持续 N，恢复 N，重复提醒（未确认）」，每条 `● [严重] 规则名：摘要 / 证据：k=v …(最多 8 个标量，数组>6 显示「[N 项]」) / 开始 MM-DD HH:MM:SS · 已持续 X · 已确认 / 深链`，恢复 `✓ 已恢复 规则名：摘要（持续 X）`，末行「事项队列：<UI>/alerts」。**通知只写技术对象（节点、入口、线路机、原因、证据、链接），不写客户影响。** 持续时间词：`<60 s` 秒、`<1h` 分 秒、`<1d` 小时 分、否则 天 小时（`:306-317`）。
- 通知链接：`#/nodes/<短名>`、`#/lines/<r1>`、否则 `#/alerts`；`ui_base_url` 配置（默认 `https://<IP>/#/`，`config.rs:289`，**硬编码默认 IP**）。
- 保留：已结束告警 90 天、通知记录 90 天、静默/维护 90 天（以 `ends_at` 计）、审计 365 天；**每 360×10 s = 1 小时跑一次**（`store.rs:238-248`、`app.rs:1166`）。

### 3.5 告警 API（`API.md:216-297`）
`GET /alerts?state=open|firing|pending|acked|unacked|resolved|all&severity&rule&relay&node&limit(默认200,≤1000)`，排序 firing→pending→其余，同状态按级别、时间倒序；`counts`（全局，不受筛选影响）给角标；`GET /alerts/{id}`（含通知记录）；`POST /alerts/{id}/ack`；静默/维护窗口 CRUD；`/notify-log?limit&status&channel`；`/channels`；`/audit`。

---

## 4. 计费、套餐与流量计量

### 4.1 套餐目录与资产（`catalog.rs`、`frontend/prototype/data/plan-catalog.json`）
- **目录是一份 JSON 文件（`architecture/plan-catalog/catalog.json`）**，启动时按 sha256 幂等导入 `plans/assets` 表（全量 DELETE+INSERT，审计 `catalog.import`）；资产必须引用存在的 plan_id（`catalog.rs:14-56`）。**【丢弃/重新设计】** 重写让套餐成为数据库一等表（CRUD + 审计），资产 `plan_id` 外键，不再有「目录文件 ↔ 旧库 asset.plan 文本」双源以及前端 `applyPlans/aliasMap/scrubPlans`（`js/plans.js:37-120`）这套文本别名清洗。
- 套餐字段：`id`（如 `gomami.lax-pulse.mini`）、`canonical_name`、`provider`/`provider_name`、`category`（`relay`/`landing`）、`category_word`（线路机/家宽）、`product_line`、`host_access`、`supplier_plan_name`、`specs{vcpu, ram_mb, disk_gb, disk_unit, bandwidth_bps, bandwidth_text, bandwidth_direction(unspecified|in_out_each), traffic_unlimited(true|false|null=待核实), traffic_quota_gb 或 traffic_quota_bytes, traffic_direction(outbound|null), traffic_period(billing_cycle), traffic_text, ip_kind(residential|datacenter), nat, ipv6, region, summary}`、`price{amount,currency,cycle}`/`price_cents`、`cycle_word`、`member_count`、`monthly_total_cents`、`aliases`、`open_questions`、`alias_details`（接口去掉）。内部维护字段 `open_questions/questions/verification/todo…` 不进任何接口（`catalog.rs:76-91`）。**`traffic_unlimited` 是三态（不限 / 有额度 / 待核实）**，待核实时 UI 显示「流量待核实」而不是「不限」（`plan-catalog.json` Q1）。
- 现有套餐（原型 7 个，后端测试 6 个）：ATTVPS 基础版 $28.88（27+ 台，无限流量，50 Mbps，住宅）、ATTVPS 标准版 $35.88（2 台）、NodeMach 高速代理 $34.98（8 台 SOCKS5，无主机）、OhnoICU NAT Small $2.99（墨西哥 NAT，流量待核实）、GoMami Pulse Mini $59（线路机，2000 GB 出站/账期）、GoMami Pulse Nano $29（线路机，1000 GB 出站）、PH-DualISP $19.32（1000 GB/月）。全部 USD 月付。
- 资产与套餐的关系：资产带 `price`（登记价，可与目录价不同，`override` 标记）、`price_cents`、`currency`、`billing_cycle`（「月付」等）、`due_date`、`expired_at`、`billing_timezone`、`service_id`（供应商服务号）、`provider`、`team`、`ip`、`egress_ip`、`relay`、`relay_port`、`outbound_tag`、`target/target_port`、`protocol`、`note`、`revision`、`capacity_bps`、`profile{maintenance_until, thresholds}`、`traffic_reset_day/traffic_timezone`、`supplier_timezone`、`auto_renewal_supplier`、`renewal_reminder_days`、`tags`、`lifecycle`（onboarding/unbound/active/retiring/retired，`format.js:625`）。
- 资产视图 `/v1/assets`（`catalog.rs:143-176`）：目录规范名 + 旧系统实时到期/账期/时区/天数；`id` 为短名；`due=null` 表示旧系统暂不可达；`quota.used_bytes` 只对线路机有值。

### 4.2 价格与币种
- 价格以**整数美分**存（`price_cents`），字符串 `price="28.88"`；编辑校验（`writes.rs:105-114`）：十进制、最多两位小数（多于两位且非零→无效）、范围 0..=10,000,000 分（≤ $100,000），错误文案「请输入有效的 USD 月付金额，最多两位小数」。`0` 显示「免费」；`billing.pending` 显示「待定」。
- 汇总：`monthly_cents = Σ price_cents`（`compat/store.rs:415`，团队小计 `:381`）。**前端有周期折算**：`monthlyCents(cents, cycle)`，季付/半年/年付/两年/三年按 months 折算并 `round`，`lifetime` 返回 null 显示「长期」（`format.js:328-346,371-385`）；后端汇总**没有**折算。当前全部月付所以没暴露。**【待核实/疑似缺陷】** 重写在服务端统一折算（建议存 `price_cents` + `cycle_months`，月费 = `price_cents / cycle_months` 取整或保留 NUMERIC）。
- 多币种：`moneyTotals` 按币种分别求和绝不换算（`format.js:388-397`）；界面目前只有 USD。

### 4.3 账期、到期日与时区
- `due_date`（`YYYY-MM-DD`，供应商后台的日历日）+ `expired_at`（该日 0 点在账期时区的 ISO 时间，如 `2026-10-01T00:00:00-07:00`）+ `billing_timezone`（默认 `Asia/Shanghai`；线路机为 `America/Los_Angeles`）。编辑 `due_date` 时重算 `expired_at`（`writes.rs:115-123`；`zone.from_local_datetime(...).earliest()`）。
- **剩余天数 = 账期时区的日历天数差**（日期标签，不是 24 小时取整）：`days = due_date - today(在 billing_timezone)`；只有 `expired_at` 时先转到账期时区取日期；时区名非法返回 null（`rules.rs:120-131`、`compat/store.rs:52-72`、前端 `format.js:473-476`）。单元测试给出：同一时刻 UTC 07:22，LA 账期 10-01→8 天，上海 10-13→20 天；UTC 16:30 时上海已换日→19 天而 LA 仍 8 天（`rules.rs:569-585`）。**前端按当前时间重算（Intl）与后端各算一遍 **【丢弃/重新设计】**：重写只在服务端给 `due_in_days`（及 `due_date` + tz），前端只做倒计时展示，避免双实现。
- 倒计时分层（`format.js:479-501`）：`lifetime` 或 ≥36500 天→「长期」；<0 已过期（红）；0 今天到期（红）；1-7 黄；>7 普通。**7 天内到期 = 剩余 ≤7 含已过期**（`isDue7`）。非上海时区在日期后加徽标（LA/NY/…，`format.js:460-470`）。
- 续费登记（`edit_asset` 的 `renewal`，`writes.rs:124-135`）：请求同时带新 `due_date`（和可选新 `price`）；规则：新到期日**必须晚于**原到期日（字符串比较，错误「登记续费时，新到期日必须晚于原到期日」）；写 `renewals(ts, asset, actor, amount_cents(=编辑后的 price_cents), old_due, new_due, note≤500)`；`revision+1`、`updated_at`、审计 `renewal`（detail 含 before/after）。**到期日由人输入，不按账期周期自动顺延。** `API.md:454` 描述的 `POST /assets/{uuid}/renewals {new_due, amount_cents, note}` 是新契约，但实现落在 `legacy-write assets/edit`（`API.md:531`）。
- 乐观锁：所有资产/节点档案编辑要求 `expected_revision == revision`（缺省 1），否则「资产信息已变化，请刷新后重试」/「节点信息已变化，请刷新详情后重试」（`writes.rs:73-75,150-152`）。重写保留（HTTP 409 + `current_revision`）。
- 名称规则：非空、≤100 字符、全局唯一（「名称已被其他资产使用」）；**线路机改名被禁止**（名称是管理通道标识，「请通过备注补充别名」，`writes.rs:92-104`）；备注 ≤500。`capacity_bps` ∈ [1, 1e13]（「带宽须为有效的 bit/s 数值」）。
- 提醒：只有 `renewal_due` 告警（§3.2），全局 `[7,3,1]` 天，对节点与线路机都生效。**资产自带的 `renewal_reminder_days`/`auto_renewal_supplier` 只在抽屉费用页签展示，告警引擎完全不读**（`node-drawer.js:360-361` vs `rules.rs:258`，grep 全仓无其它使用）**【丢弃/重新设计】** 要么让它生效（每资产自定义提醒天数、抑制已开供应商自动续费的提醒），要么删字段。
- 到期 UI：计费汇总表「续费到期」最近 12 项；KPI「7 天内到期」；节点列表到期列（含时区徽标）、首页卡片「到期」。

### 4.4 流量配额与计量
- **配额口径：十进制 GB（1 GB = 10⁹ B）**，方向 outbound（出站=网卡发送），周期 `billing_cycle`；仅线路机有额度（Mini 2000 GB、Nano 1000 GB）。落地家宽无限或未核实。`/v1/quota` 的 `caveats` 明确：用量不是供应商面板读数；Komari 的 `traffic_limit` 是 GiB 口径（2000 GiB = 2.147×10¹² B，与 2000 GB 差 7%）（`API.md:150-157`、`catalog.rs:214-219`）。**前端口径**：普通字节一律 IEC（KiB/MiB/GiB，`format.js:141-151`），只有套餐配额用十进制 GB 的 `decGB`（`node-model.js:164-170`），速率用 SI bit（bps/Kbps/Mbps，`format.js:158-163`；用户可选 bit/byte）。**重写需明确「存储单位字节 + 展示单位」，禁止混用；配额/账单展示 GB，传输速率 bit，网卡累计 IEC 要在 UI 标注。**
- 用量来源 = fleetd 流量计 `fleetd_meter`（`meter.rs`）：每 30 s 读 nodeagent/relayd 的网卡累计 `tx_bytes/rx_bytes` 与 `boot_id`；同次开机且不倒退→加差值；重启或计数清零→加新计数本身；**线路机按账单周期清零**：周期起点 = 到期日的「日」每月（到期 10-01→每月 1 日；10-13→13 日；该月没有这一天取月末），按资产账期时区 0 点（`period_start`，`meter.rs:48-78`，测试 `:145-155`）。落地节点不清零（累计）。基线用 `fleetd vendor import`（供应商面板读数 + 之后网卡积分，`host_1m` 速率×60 积分，`:116-124`）；没有基线时从此刻起计（`:89`）。`hosts.net_total_up/down` 取自此处。
- **两套「重置日」**：计量按到期日的「日」，而 `/v1/quota` 展示的 `reset_day` 取资产的 `traffic_reset_day`，`timezone` 取 `traffic_timezone` 或 `billing_timezone`（`catalog.rs:202-203`）。两者应相同但没有强约束 **【待核实】**；重写只保留一个（账期起始日）。
- 业务流量（Xray 入口计数）另走 `minutes`（35 天）/`hours`（400 天）：入口 tag→资产映射（节点按 `relay_port`；线路机主入口归线路机；其余 `direct:<线路机>:<端口>`）；同一分钟整分钟覆盖（幂等）；今日/本月边界按**北京时间**（`start_times`，`compat/store.rs:18-24`）；`traffic.month_partial`（监控起点晚于月初时标「本月不完整」）。注意：业务口径含监控探测流量，且线路机与落地相加会重复计一次。
- 告警阈值见 `quota` 规则（§3.2）；UI 额度条 ≥90% 黄、≥100% 红（`viz.js:100-104`，与告警 80%/5% 档不同）。
- 重置入口用量（`reset_traffic`）：relayd 的累计账**不清零**，命令前先并入；结果带 `value_at_reset/delta_folded/cum`，本期用量 = `cum − cum_at_reset`（`API.md:487`）。

---

## 5. 行为与访问分析

### 5.1 记录什么（`API.md:490-496`，`compat/behavior_write.rs`）
- 来源：relayd 解析 Xray `access.log`，每批 ≤500 条，事件 `{id=<inode>:<offset>, ts, inbound, client, source_ip, source_port, network(tcp|udp), host, port, action(accepted|blocked), rule(恒 null), outbound}`；`blocked` = 出站为 blackhole（含 `fleet-default-deny`）或日志 rejected；回环来源（relayd/探测自身）跳过；`dropped` 只计超 32 KiB 的畸形行；`rotation_gaps/truncations` 计数会告警；事件按 `id` 去重。
- 入库 `activity` 表：`id=<线路机>:<event_id>`、`ts, asset, team, relay, source_ip, host, port, protocol, action(routed|blocked), client, kind(domain|ip)`。归属：入口 tag 末段端口 → 该线路机下 `relay_port` 相同的资产；线路机主入口端口归线路机自身；找不到→ `direct:<线路机>:<端口>`，team=「未分配直连」（outbound==direct）或「未归属入口」（`behavior_write.rs:179-206`）。
- 增量维护的派生表：`destinations(asset,host,kind,first_seen,last_seen,connections,blocked)`、`sources_seen(asset,source_ip,…)`、`behavior_buckets(asset, 5 分钟桶, connections, blocked)`、`behavior_events`、`behavior_cache`（默认查询缓存）、`beh_pair_h`（小时汇总，供 7/14 天窗口）。
- 记录量：约 2.7 条/秒（`app.rs:866` 注释）≈ 23 万条/天，24 小时窗口约 26 万行（`compat/window.rs:1` 注释），14 天约 330 万行。**【丢弃/重新设计】** `window.rs`（内存增量窗口，581 行）和 `rollup.rs`（小时汇总，473 行）是为了在 SQLite 上让 `/behavior` `/activity` 秒回而写的缓存体系，并要求与旧 SQL 逐字节同序（GROUP BY 输出顺序、LIMIT 临时 B 树的稳定性等）。PG 上用分区表 + 索引 + 物化视图/连续聚合即可，不要移植。

### 5.2 检测规则（`behavior_events.kind`）
| kind | 触发 | 窗口 key | 文案 |
|---|---|---|---|
| `new_domain` | 事件 `kind=domain`，节点已过「基线期」，且该 (asset,host) 在 `destinations` 里第一次出现；每资产每天同一 subject 唯一 | `ts // 86400` | 「首次记录到此域名；新域名不等于异常网站」 |
| `new_source` | 基线期后该 (asset,source_ip) 第一次出现 | `ts // 86400` | 「出现新的入口来源 IP；也可能是正常网络切换或共享出口」 |
| `connection_spike` | 上一个完整 5 分钟桶连接数 ≥ `max(300, 5×此前有流量时段的中位数)`，且此前桶 ≥12 个（回看 2 小时）（`behavior_write.rs:246-282`） | 该桶起点 | 「5 分钟内 N 次入口连接，此前有流量时段中位数 M；…连接数不等于用户数或页面浏览量」 |
| `blocked_burst` | 上一个完整 5 分钟桶被阻止连接 ≥20 | 该桶起点 | 「5 分钟内 N 次连接被规则阻止；核对访问规则是否符合预期」 |
- **基线期**：每个资产首个事件时间存为 `behavior_baseline:<asset>`，之后 30 分钟(1800 s)内只学习不报 new_*（`behavior_write.rs:152-158`）。检测只针对有 `relay_port` 的资产。唯一约束 `(asset,kind,subject,window)` 保证同窗不重复（`INSERT OR IGNORE`）。
- 确认：`behavior_ack(id)` 写 `ack_at`；找不到「行为提示不存在」，审计 `behavior_ack`（`writes.rs:224-232`）。前端「未读」圆点不统计 `new_domain`。
- **随机子域名合并**（`domains.rs`，生产开启 `domain_collapse`）：主机在注册域名（PSL）之下的标签满足「含 UUID」「≥8 位纯十六进制且含 ≥2 位数字」「≥20 位纯字母数字且含 ≥4 位数字」任一条，则合并为一条 `subject="*.<注册域名>"`，每资产每天一条，`evidence`={collapsed_by, domain, count, examples(前5), last_ts}，message 随数量更新（`domains.rs:42-65,77-101`）。`API.md:564-566` 注：原文是 `≥8位含数字的十六进制串`，代码是 `≥2 位数字`。
- `/behavior` 的 `events` 生产环境「新域名与其他事件各取最近 200 条」（`events_split`，`behavior.rs:144-152`）；旧实现合在一起取 200 条，新域名多时会把连接突增/新来源挤出列表。
- **【丢弃/重新设计】** `new_domain` 噪音极大：运维脚本每日清理 7 天前的 `new_domain` 事件（`docs/ops/fleet-prune-domains.py`，注意它直接删 `legacy.db`，而默认保留期是 90 天），UI 不统计其未读、也没有逐条明细（`behavior.js:373-377`）。重写建议：不存 `new_domain` 事件，改为按资产的「首次出现目标」表（已有 `destinations.first_seen`）查询；只对"信誉/分类异常"的域名告警，或完全去掉。

### 5.3 查询语义（`behavior.rs`）
- `/behavior` 汇总返回 `nodes`（按资产：连接数、域名数、来源数、阻止数、仅 IP 数、最近）、`targets`（按 (asset,host)：连接、来源数、阻止数、窗口内首次/最近、`first_seen`、`total_recorded`；分页 `limit` 10-200 默认 100、`offset` ≤100000，`next_offset` 满页才给）、`sources`（按 (asset,source_ip)，限 100）、`events`、`started_at`、`raw_retention_days=14`、`target_retention_days=90`。参数：`seconds`（300 ~ 14 天，默认 86400）、`asset/team/source_ip/host/action/kind/protocol/search`（search 对 host、source_ip、client 做 LIKE，≤200 字符）。
- `/activity` 默认 24 小时的 top：`domains`（host,kind 分组，top 40，含阻止数）与 `sources`（top 30）+ `total`。
- 增量接口：`GET /v1/activity/delta?after=<rid>&limit=500` → `{cursor, more, rows[], }`，行 = 旧 activity 字段 + 递增 `rid` + `source_intel`；不带 `after` 取最近 N 条 + 当前游标；SSE `activity` 每拍最多一次（`API.md:558-560`）。
- 7 天/14 天 `/activity` 不能超过 604800 秒（7 天）（`API.md:46`）。

### 5.4 保留期（`behavior_write.rs:312-320`、`compat/store.rs:425`）
原始 activity 14 天；`destinations`/`sources_seen`/`behavior_events` 90 天；`behavior_buckets` 14 天；`beh_pair_h` 15 天（`rollup.rs:21`）；`behavior_cache` 覆盖写；ipintel 小时分布/来源明细 35 天、情报缓存 30 天（`ipintel/src/service.rs:75,84`）。`ipintel_backfill_days ≤ 14`（因为旧系统只留 14 天访问记录，`config.rs:419`）。

### 5.5 访问策略（`policy.rs`、`policy_apply.rs`、`writes.rs:299-382`）
- 范围 `team:<名>` 或 `node:<uuid>`；有效策略 = 节点策略 > 团队策略 > 默认 `{mode:"observe", domains:[]}`（`policy.rs:11-21`）。节点模式可为 `inherit`（回退到团队）。
- 模式：`observe`（不产生规则）、`allowlist`、`denylist`、`blocked`（暂停新连接）。`allowlist/denylist` 必须有域名（「启用白名单或黑名单前必须填写域名」）。
- 域名规范化：去空白/逗号/中文逗号分隔，小写，去尾点，**只接受 ASCII**（国际化域名请填 xn--），每份最多 300 个，必须含点、每标签 ≤63 且字母数字/连字符、总长 ≤253、不能是 IP（「请填写域名，不支持 IP 地址白名单」）、不能带 URL/端口/正则；`=example.com` 表示精确匹配（`full:`），否则 `domain:`（含子域）。匹配规则 `h == p` 或 `h.endswith("."+p)`（精确模式只等值）（`policy.rs:23-27`）。
- 草稿（`drafts` 表）：`scope, data{mode,domains,note≤500}, rev, ts`；保存要求 `expected_revision == 当前 rev`（「草稿已被修改，请重新加载」）；`draft_hash = sha256(sorted JSON of 所有草稿 policy)`；apply/discard 都要带 `draft_hash`，不一致「草稿已变化，请重新预览/加载」。预览按最近 24 小时 `activity` 按 (asset,host) 模拟哪些会被阻止（每节点 ≤20 个域名，只对有 `relay_port` 的资产）。
- 发布（`apply`，`policy_apply.rs:58-106`）：存在 `interrupted/rollback_failed/applying` 版本时拒绝（「存在未完成或未核实的发布…」）；编译 → 每台线路机 `policy-<uuid>-match`（allow/deny 域名规则：allowlist 走节点出站，denylist 走 blocked）与 `policy-<uuid>-default`（allowlist/blocked 其余走 blocked）；作为 relayspec 新一代下发（relayd 守卫 + `xray -test` + 逐项 API 热加，**不重启 Xray**，`API.md:474,483-484`）；版本表 `versions(id=<UTC时间>-<4字节hex>, ts, actor, policies, status, result)`；`reconcile`：全部 op applied→生效并清草稿；任一被拒/过期→已应用的线路机回到发布前 → `failed_rolled_back`（或 `rollback_failed`）。规则在 Xray 配置里的位置：`policy-*` 块放在第一条入口 inboundTag 路由之前（`API.md:482`）。
- 保留：版本表最近 30 条展示（`policy.rs:50`）。

---

## 6. IP 情报（`crates/ipintel`）

- 做什么：**只查本地离线库，不把任何 IP 发往在线服务**。四个库 mmap：ip2region xdb、DB-IP City Lite、DB-IP ASN Lite、iptoasn TSV（`/v1/ipintel/status` 的 `loaded` 应为 5，`API.md:362`、`lib.rs:1-9`）。库由 systemd timer `fleetd-ipintel-fetch` 定期更新，变化后 20 个 tick 内热重载（`app.rs:859`）。
- 对每个 IP 合成画像：运营商 `isp`、ASN/`as_org`、国家/省/市/`place`、坐标、接入方式 `conn_type`（住宅宽带 residential / 移动网络 mobile / 机房 datacenter / VPN·代理 vpn / 未知 unknown）+ 理由 + 置信度（high/medium/low）、`flags`（`internal`=我方线路机/落地/运维出口，`private`=非公网）。
- 接入方式判定顺序（`classify.rs:1-16`）：① 非公网→unknown+private；② 内部地址只打 `internal` 标；③ 精选 ASN 列表：VPN（M247、Datacamp/CDN77、PacketHub、Proton、Mullvad、NordVPN）→vpn；云（AWS/Google/Azure/Cloudflare/阿里腾讯华为百度/Oracle/DO/Vultr/Linode/OVH/Hetzner/Contabo…）→datacenter；IDC/托管（含我方线路机所在的 AS979 NetLab）→datacenter；蜂窝专用 ASN→mobile（均 high）；④ AS 名称关键词：5G/LTE/WIRELESS/CELLULAR/MOBILITY→mobile high，VPN/PROXY/ANONYM/TOR→vpn medium；⑤ 机房关键词（HOSTING/SERVER/CLOUD/DATACENTER/IDC/VPS/COLO/DEDICATED）或 iptoasn 机房集合→datacenter medium；⑥ 中国三大运营商骨干/省网 ASN→residential low（`alt=mobile`，家宽与手机共用）；⑦ 固网运营商列表（AT&T 固网、Comcast、Charter、Verizon 固网、Cox、Lumen、Frontier、Telmex、Megacable、StarHub、Singtel 等）→residential medium；⑧ 其余→unknown low。**局限（代码注释自己写了）：没有 VPN/Tor 出口清单，只能靠 ASN；识别不了住宅代理。** 重写要把这些 ASN 清单做成数据（可热更新的表）而不是硬编码。
- 学习与异常（`service.rs:422-606`，配置默认 `:62-89`）：
  - 节点第一条事件时间 = 学习期起点，**`learn_s`=7 天内只学习**（按事件时间算，回填历史也成立）。
  - `ipintel_new_isp`（warning）：学习期后某节点出现新的运营商维度值；`ipintel_new_city`（warning）：新城市；跳过 `internal/private` 来源。
  - `ipintel_datacenter_source`（warning）：每个 (节点,来源 IP) 第一次出现且类型为机房/VPN。
  - `ipintel_shared_identity`（info，只进日报）：同一 (线路机, 客户端标识) 在 `shared_window_s`=600 s 内从两个相距 ≥`shared_min_km`=150 km 的城市接入（缺坐标退回「省/国不同」）；排除内部前缀 `relay-/zhongzhuan/probe/fleet-/komari/node-00` 与显式忽略；若学习期前或最近 `shared_known_days`=14 天窗口之外这个标识已从这两个城市都接入过，视为既有用法不报。
  - 一次性异常（new_isp/new_city/datacenter）打开 `alert_ttl_s`=24 h 后自动关闭（告警随之结束，不发恢复通知）；一号多地可 bump 计数；异常可手动 `POST /ipintel/anomalies/{id}/close`（审计 `ipintel.anomaly.close`）。异常进入告警引擎，标签 `node/relay` 与其他告警一致（静默、维护窗口、relay_down 抑制照常）。
  - 出口 IP 历史：`egress_confirm`=2（新地址连续被测到 2 次才确认）；0.13 起出口 IP 变化改为**自动更新资产 `ip/egress_ip/名称里的 IP`**（审计 `asset_ip`，原因「出口 IP 自动更新」），线路机上入口实际连的落地地址变了→自动同步资产 `target/target_port`（审计 `asset_target`）（`API.md:572-575`、`writes.rs:410-466`）。**【丢弃/重新设计】** 自动改写资产 IP 并 `name.replace(old_ip,new_ip)` 是脆弱设计（名称里内嵌 IP），重写把 IP 作为属性、名称不含 IP。
- 展示位置：行为页「来源地址」页签与「行为变化」对象列（运营商·城市·接入方式）；节点抽屉「访问记录」「连接质量」（来源画像卡、来源 IP 表、出口 IP 历史）；线路页入口展开行（来源 IP + 24h 画像）；节点行为的来源行带 `source_intel`。
- API：`GET /v1/ipintel/{ip}`、`/ipintel?ips=a,b,c`（≤200）、`/v1/nodes/{id}/sources?window=1h|24h|7d|14d|30d|≤35天秒数&limit=50(1-500)`（分布 isp/city/province/country/asn/type 带 pct、top 来源、已学基线、出口当前与历史、异常；无访问记录返回 `empty:true` 的合法空结构）、`/ipintel/anomalies?status=open|closed|all&kind&node&severity&since&limit`、`/ipintel/status`、`/ipintel/backfill`。启动回填最近 14 天（按天取样，每节点 8 页，限速）；现在访问日志直接喂 ipintel（全量，不再抽样 500 条/分钟，`API.md:496`）。
- **【丢弃/重新设计】** 回填（`ipintel_backfill_*`）和"从旧 /activity 每 60 s 抽 500 条"的老路径随旧系统一起丢；新系统访问日志入库后直接 `INSERT ... ON CONFLICT` 增量维护 `node_sources`。

---

## 7. 账户、认证、会话、API 密钥、审计

### 7.1 登录与账户（`accounts.rs`、`API.md:410-428`）
- 密码：**PBKDF2-HMAC-SHA256，310,000 轮，16 字节随机盐**，格式 `310000$<salt hex>$<hash hex>`，常量时间比较（`accounts.rs:3,19,59-87`）。从 Komari 导入的旧哈希（`base64(sha256(pw+固定盐))`）首次成功登录后自动升级。**重写：直接用 Argon2id（或同等）；不需要 Komari 导入；`pw_scheme` 列保留以便将来升级。**
- 改密：旧密码校验 + 新密码 **≥10 字符**；成功后**其他会话全部作废**；只允许 fleetd 会话（`via=komari` 403）。
- 2FA：可选 TOTP RFC 6238，SHA1、6 位、30 s、允许 ±1 步（`totp_check`，`:151-156`）；开启流程 `setup`（生成密钥 + `otpauth://`）→ 验证 6 位码 → `enable`；关闭也需 6 位码。**【丢弃/重新设计】** 没有备份恢复码；TOTP 密钥明文存库；SHA1 TOTP 够用但应加密存储并发放恢复码。
- 登录限速：同一来源 IP **或**同一用户名，15 分钟内失败 5 次→锁 15 分钟，返回 `429 {"error":"尝试次数过多，请 N 分钟后再试"}`；内存实现，重启清零（`accounts.rs:249-250`）。**【丢弃/重新设计】** 重写放 PG/Redis，并区分用户名锁（防爆破）与 IP 锁（防枚举），不要让攻击者用别人的用户名锁住他。
- 响应文案：`401 用户名或密码错误`、`401 需要两步验证码 {totp_required:true}`、`401 两步验证码不正确 {totp_required:true}`、`403 请求来源校验失败`。
- 账户来源：`users(id, username unique, pw_scheme, pw_hash, totp_secret, totp_pending, source, created_at, updated_at, disabled_at)`；CLI：`user list/set-password/disable/import/export-komari`（无 UI 创建用户；生产只有 `admin` 一个）。重写需要用户管理 UI/CLI（至少创建、禁用、重置密码），角色模型是空白（目前所有登录用户都是管理员）。

### 7.2 会话
- Cookie `fleet_session`（`HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=604800`），值=随机 32 字节 hex，库里只存 sha256；**绝对有效期 7 天、闲置 24 小时失效**；`last_seen` 最多每分钟写一次；校验结果缓存 30 s（退出/改密/撤销即时生效靠主动失效）。会话列表字段：`id, created_at, last_seen, expires_at, ip, ua(≤200 字符), current`；`DELETE /auth/sessions/{id}` 撤销（审计 `auth.session_revoked`）；`POST /auth/logout` 清 Cookie（`Max-Age=0`）。
- CSRF：所有写（含登录）要求请求头 `X-Fleet-Request: 1`，且若带 `Sec-Fetch-Site` 只能 `same-origin`/`none`，否则 `403 请求来源校验失败`；请求体 JSON ≤64 KB（agent 上报 ≤256 KB）。重写保留（SameSite=Strict + 自定义头 + Origin 校验）。
- 本机直连免会话（`direct_local`：对端回环且无 `X-Forwarded-For/Forwarded/Via`）只给 `system/debug/health` 观察用；`dev-allow-local` 模式给测试服。**【丢弃/重新设计】** 重写不要有「本机免认证」后门（`auth.rs:50-53`），改用独立的 metrics 端口/令牌。
- 错误统一格式 `{"error":"中文说明"}`，成功 `{"data":…}`，`Cache-Control: no-store`、`X-Content-Type-Options: nosniff`（`API.md:12-13`）。

### 7.3 API 密钥（`writes.rs:247-297`、`legacyhttp.rs:319-360`、`legacy_schema.json:324-327`）
- 格式 `tlm_<32 字节 base64url 无填充>`；库里存 `sha256` 与前 12 字符 `prefix`；明文只在创建响应里出现一次。字段 `id(8字节hex), name(1-100 字符), scope, prefix, created_at, expires_at(null=长期), key_hash, revoked_at, last_used_at, use_count`；创建要求 `expires_days` 为 0-3650 整数（0=长期，否则「有效期需为 0–3650 的整数天，0 表示长期有效」），权限必须是 read/write/full（「权限无效」），名称错误「密钥名称需为 1–100 个字符」；UI 预设有效期 30/90/180/长期。
- 权限语义：`read` = 所有 GET（`/api-keys*` 除外）；`write` = read + 所有 POST（`/api-keys*` 除外，包括策略发布、入口操作）；`full` = write + `/api-keys*`（列表、用量、创建、撤销）；所有密钥禁止 `/sync/*`、`/komari/pending|done`（`legacy_schema.json:324-327`、`compat/mod.rs:230`）。不满足返回 `403 forbidden for api key` / `403 write scope required`；缺头 `401 missing api key`；无效/撤销/过期 `401 invalid api key`。
- 使用记录：每次调用写 `api_key_usage(ts, key_id, method, path, status, ip)` 并更新 `last_used_at/use_count`（`api_key_usage`，`:293-297`）；`/komari/hosts/<uuid>/metrics` 路径归一后记录；UI「最近调用」展示最近 50 条。
- 认证方式：请求头 `X-Api-Key`，路径白名单 = 旧插件 `allowedGet` + `public-status`；撤销 = `UPDATE revoked_at`，立即生效。**【待核实】** `API.md:458` 新契约是 `GET/POST /v2/api/v1/api-keys`（会话鉴权）；API 密钥能否访问 `/v2/api/v1/*`（而不仅是 legacy 形状接口）在代码里没有看到（只有 `ops_api` 处理 `X-Api-Key`）。重写要明确：API 密钥用于「外部自动化/AI agent 调用」的哪些 v3 端点、scope→端点矩阵、是否限 IP、限速。
- 审计：`apikey.create/revoke` 写审计（actor=登录名；密钥调用的 actor=`apikey:<name>`，`legacyhttp.rs:356`）。创建响应里的 `scope` 提示：只读=查看资产、线路、健康、行为、费用；读写=另可确认故障与行为事件、编辑资产、执行入口操作；完整=另可管理 API 密钥（`api-keys.js:94-96`）。

### 7.4 审计日志（`store.rs:142-194`）
- **有哈希链**（`fleetd.db`）：`audit(id, ts, actor, action, target, detail, prev_hash, hash)`，只追加；`hash = sha256(prev_hash + "\n" + "{ts:.6}\n{actor}\n{action}\n{target}\n{detail_json}")`，首条 `prev_hash = "0"×64`；`audit_verify` 逐行校验链，返回第一处断点 id，`GET /v1/audit` 返回 `{chain_ok, chain_broken_at, audit[]}`；保留期 365 天且**从左边缘截断**——校验从保留的第一条的 `prev_hash` 起算。局限：哈希链只防事后改库，不防整库回滚/有权限者重算链；`audit` 在库内单写线程里写（`synchronous=FULL`）。**重写建议保留链 + 周期性把链头哈希外发（日志/不可变存储）。**
- **另一份无链审计**（`legacy.db audit`，`compat/writes.rs:12-18`、`monitor_eval.rs:565`）：`audit(ts, actor[:100], action, target[:150], detail=JSON)`，承载资产编辑、续费、故障确认、行为确认、API 密钥、策略草稿/发布/回滚、节点档案、系统健康翻转、故障开关、IP 变更等 —— 旧界面「管理操作记录」展示的就是这份。**【丢弃/重新设计】** 重写统一一张审计表（带链），动作命名统一（现在是 `asset_edit`、`renewal`、`node_profile`、`incident_ack`、`behavior_ack`、`policy_draft/discard/apply_*`、`apikey.create` 混着下划线与点号）。
- 动作清单：`alert.ack`、`silence.create/delete`、`maintenance.create/delete`、`catalog.import`、`auth.login/login_failed(带来源IP)/logout/password_changed/totp_enabled/totp_disabled/session_revoked/user_imported/password_set`、`ipintel.anomaly.close`、`onboard.<动作>`、`entry.rename/reset_traffic/…`、`relay.restart_xray/reorder_inbounds/spec_ack`、`secret.reveal`（查看分享链接）、`asset_edit/renewal/asset_ip/asset_target/asset_bandwidth/asset_add/node_profile/incident_open/incident_resolved/incident_ack/behavior_ack/health_flip/policy_*/apikey.*`（动作中文名映射见 `lines.js:49-65`）。
- 审计 `detail` 与 `target` 约定：`target` 形如 `asset:<uuid>`/`relay:<名>`/`team:<名>`/`apikey:<id>`/`alert:<id>`/`silence:<id>`/`maintenance:<id>`，`detail` 为 JSON，资产编辑带 `{before, after}` 全量。注意 `detail` 可能含 UUID/IP（脱敏：UUID 只记前 8 位）。

---

## 8. 实时（SSE、增量、轮询节奏）

### 8.1 SSE `GET /v2/api/stream`（`sse.rs`、`API.md:54-96`）
- 帧：`retry: 3000`；`event: hello`（`epoch, last_id, versions{资源:版本}, resources{资源:path}, extra{…}, heartbeat_s=15`）；`event: change`（`{resource, version, ts, path}`，**内容哈希变了才发**；资源第一次取到时不发）；`event: heartbeat`（15 s，`{ts, clients}`）；`event: reset`（客户端全量重取）；`event: auth`（会话失效，服务端随后断开）；`id` = `<epoch>-<序号>`；`Last-Event-ID` 补发（保留最近 512 帧；epoch 不同或太旧→`reset`）。头 `Cache-Control: no-store, no-transform`。
- 资源：旧形状 `overview, health, panel, history, incidents, behavior, activity, renewals, jobs, audit, provision_defaults`（**重写要重新规划：`overview/health/panel` 是大聚合，应拆成细粒度资源**），自有 `alerts, notify, silences, probes, policies(草稿保存/丢弃/发布/回滚/回执定版), ipintel, ops, onboard`。
- 收到 `change` 后去 GET `path`（命中渲染缓存）；写成功后服务端立即重取 overview/health/panel 并推送；会话 30 s 复核，失效推 `auth`。
- 第一个 SSE 客户端连上→后台轮询切「有人查看」节奏；最后一个断开→回空闲节奏（`app.active()`）。

### 8.2 轮询节奏（`API.md:36-40`、`README.md:12`、`app.rs`）
| 资源 | 有人看 | 无人看 |
|---|---|---|
| overview + health | 10 s | 60 s |
| panel/overview | 15 s | 按需 |
| history/incidents/behavior/activity | 60 s | 按需 |
| renewals/jobs/audit | 300 s | 按需 |
| ipintel 访问源抽样 | 30 s | 60 s |
| 告警时钟（评估 + tick） | 2 s（`alert_clock`） | 同 |
| 喂数 | 10 s 一拍 | 同 |
| relayd 取期望状态 | 10 s（`wait=0`），observed 60 s 检查/每小时 | 同 |
| 流量计 | 30 s | 同 |
| housekeeping（保留期清理） | 每小时 | 同 |
| 前端 `plans` 页 | 5 分钟 | 同 |
- 有 SSE 连接时 `POST /v2/ingest/*` 的 200 带 `"live": true`，relayd 随后 90 s 内每 60 s 全量巡检（`API.md:577`）。
- **【丢弃/重新设计】** 「共享轮询器 + 渲染缓存 + 数据代」机制（`cache.rs`、`app.rs poller`、`Memo`）本质是为了不让多个浏览器各自触发全量重算 SQLite。PG + 物化/预计算的资源表即可；SSE 改为按资源推送 + 客户端拉取。

### 8.3 增量接口
- `GET /v2/api/v1/delta/{overview|health}?since=<版本>` → `{version, full, patch|data}`；补丁为 JSON Merge Patch 变体（`null` 是普通值，`$del` 删键，以 `uuid` 为键的数组用 `{"$uuid":{…}, "$removed":[…], "$order":[…]}`）；服务端只保留最近 8 个版本，太旧/重启/不带 since→整份；没快照 503（`API.md:583-594`、`delta.rs`）。**【丢弃/重新设计】** 为了弥补 overview 是一个 ~MB 级大 JSON 而发明的补丁协议。重写直接把资源拆小（节点行/线路机/入口分别推 `change`）。
- `GET /v1/probes/latest?since=<until>` 增量；`GET /v1/activity/delta?after=<rid>`（见 §5.3）。ETag/304 支持。

### 8.4 relayd 管理协议（`API.md:430-488`）
- 触及线路机的操作都是异步 op：返回 `202 {op_id, relay, generation, status:pending}`；**只有最近 60 s 内来取过期望状态的线路机才接受**，否则 `409 线路机N 的 relayd 还没开启管理模式…`；`op.status` ∈ pending/applied/rejected/partial/expired；relayd 180 s 没回执→`expired`（该代期望自动回退到上次已应用内容）；60 s 没被取走→`expired`（可重试）。`GET /v1/ops?relay=&status=&limit=`、`/ops/{op_id}`；SSE `ops`。
- 入口操作：`enable|disable|reset_traffic|rename(≤64)|rotate_uuid(confirm=线路机名:端口)`；线路机操作 `restart_xray(confirm=线路机名)`；`GET /relays/{relay}/entries/{port}/share`（审计 `secret.reveal`）。关键约束：relayd 守卫——不改非受管对象（api 入站、`outbounds[0]` 默认 blackhole `fleet-default-deny`、direct/blocked 等）；保留的入口入站必须有「只按 inboundTag 匹配」的直通路由；候选配置 `xray run -test` 必过；规则整表替换 `adrules` + `lsrules` 核对；任一步失败逆序撤销并回 `rejected`（`API.md:482-488`）。generation/`base_config_sha256` 乐观并发；外部写入者（fleet-cutover、onboard）改了配置→fleetd 收编为新一代（`adopted_external_change`）。
- 这块协议是核心资产，重写应保持 relayd 侧契约不变（`fleet.relayspec.v1`、`fleet.relayobs.v1`、`fleet.access.v1`），只重做中控实现。

---

## 9. 数据量与保留期

| 数据 | 量级/保留 | 出处 |
|---|---|---|
| 资产 | ~42（3 线路机 + 39 落地）；套餐 6-7 | `API.md:117-119`、`plan-catalog.json` |
| 每线路机入口 | 十几个（线路机1 名下 NodeMach 8 台 SOCKS5…） | `API.md:392` |
| minutes（业务流量 1 分钟桶） | 35 天 | `compat/store.rs:425` |
| hours（业务流量小时） | 400 天 | 同上 |
| activity（访问记录） | 原始 14 天；约 2.7 条/s | `behavior.rs:55`、`app.rs:866` |
| destinations / sources_seen / behavior_events | 90 天 | `behavior_write.rs:315` |
| behavior_buckets | 14 天 | 同上 |
| beh_pair_h（小时汇总） | 15 天 | `rollup.rs:21` |
| health_samples（分钟） | 14 天 | `monitor_eval.rs:894` |
| health_hours | 400 天 | `:895` |
| incidents（已恢复） | 365 天 | `:896` |
| 告警（已恢复） | 90 天 | `store.rs:242` |
| notify_log | 90 天 | `:243` |
| 审计（fleetd.db） | 365 天（左截断） | `:245` |
| 静默/维护窗口 | `ends_at` 后 90 天 | `:246` |
| probe_raw / probe_5m | 24 小时 + 10 分钟缓冲 | `probes.rs:222-227` |
| 会话 | 7 天绝对 / 24 小时闲置（过期行靠查询过滤，未见清理任务） | `accounts.rs:17-18` |
| ipintel 小时分布、来源明细 | 35 天；情报缓存 30 天 | `service.rs:75,84` |
| `api_key_usage` | **未见保留策略** **【待核实】** | `writes.rs:293` |
| `renewals` | **未见保留策略**（永久） | `writes.rs:131` |
| SSE 补发环 | 512 帧 | `API.md:91` |
| 响应/请求体 | 请求 ≤64 KB（agent ≤256 KB）；probes raw ≤5000/类 | `API.md:12,398,347` |
- 观测中控内存：24 h 访问窗口只存行（~26 万行）；7/14 天小时汇总 10 分钟没人看就释放（`README.md:105`）。

---

## 10. 建议丢弃 / 重新设计（汇总，含理由）

1. **Komari 与旧 team-ops 兼容层整体**：理由见文首；包括 `legacy_source=native/team-ops` 双模式、`/v2/api/legacy/*`、`/ops/api/*`、`/api/team-ops/*` Caddy 转发、逐字节 Python JSON 序列化、`legacy-write/*`、`schema.md`。
2. **旧「overview」大 JSON + delta 补丁 + 渲染缓存/数据代**：见 §8.2/8.3；改成细粒度 REST 资源 + SSE 资源通知。
3. **在内存里重建 SQL 结果的 window.rs / rollup.rs / Memo**：见 §5.1，PG 索引 + 聚合。
4. **两份审计（一份带链、一份无链）、两个 SQLite 库**：统一为 PG 单一审计表（保留链 + 链头外发）；`legacy.db`/`fleetd.db`/`ingest.db`/`ipintel.db` 合并 schema。
5. **配置文件驱动的通知渠道**：改为库表 + UI + 渠道测试发送（现状无测试发送功能，`notify.rs`）。
6. **目录 JSON 文件 + 前端文本别名清洗（applyPlans/scrubPlans/stripInternal）**：套餐表 + 外键；内部备注与展示字段分开。
7. **告警指纹含 `cause`/`threshold`**（§3.1）：重复告警与「静默结束」；改为 `(rule, subject)` + 状态升级。
8. **静默 vs 维护窗口两种对象**（§3.3）：同一语义；合并。
9. **`egress_mismatch`、`relay_egress_mismatch`、`ipintel_egress_ip_changed`、`EGRESS_IP_COMPARE`、`ip_check` 参数、`probe` 里 `mismatch` 状态**：已被「出口只看连通」取代，死代码/死状态。
10. **自动改写资产 IP/名称**（§6）：名称里内嵌 IP 的设计应废除。
11. **`new_domain` 事件**（§5.2）与 `fleet-prune-domains.py` 的人工清理；`blocked_rise`/`blocked_burst`、规则模式词不一致（§1.5）。
12. **非关键检查在去抖后变 fail 而不影响节点状态的矛盾**（§2.3，待核实）。
13. **资产 `renewal_reminder_days/auto_renewal_supplier` 展示但不生效**（§4.3）；**`traffic_reset_day` 与到期日的「日」两套重置日**（§4.4）。
14. **前后端各算一遍到期天数 / 月费折算**（§4.2/4.3）：只在服务端算。
15. **会话/锁定状态存内存**（§7.1）、本机免认证后门（§7.2）、TOTP 无恢复码且明文存储。
16. **`/v2/api/v1/debug/*`、`/system` 里的内存构成/请求耗时统计**：运维观测改用 Prometheus/OTel。
17. **前端「演示条（?demo=1）」、`createLive` 模拟器、原型快照 `snapshot.json`/`detail.json`/`extra.json` 懒加载文件**：原型脚手架，不属于产品需求；`shared/live.js`、`data.js stressNodes`、`status_replays` 全部丢弃。
18. **3x-ui 面板、Komari agent、`panel` 状态/`routing_hash`/`config.valid`**：已退役；线路机状态改以 relayd/Xray Stats 为准。
19. **硬编码默认 IP/路径**（`ui_base_url=https://38.58.57.22/#/`、`/var/lib/fleetd`、`127.0.0.1:25900`）：全部配置化。
20. **onboarding 页的原型批次模拟（`onboarding-model.js` 的 tick/ACTIONS）**：只保留与后端 `crates/onboard` 的真实批次状态机对接的部分。
