# 原版首页设计 DNA（/admin/home，LuminaPlus 1.3.3 补丁版 review20260917v4）

> 日期 2026-09-23。用途：给“在原版基础上美化”的工作定锚。本文只描述**原版现在长什么样**、每个值从哪来、首页内部有哪些毛病，不提出新的视觉方向。
> 结论先行：原版的“脸”由四样东西构成：浅灰画布上的白色发丝线卡片（圆角 14）、靛蓝强调色 `#5b5bd6`、胶囊形分段控件和状态 pill、以及卡片内“标签灰 12px + 数值黑 13px/600 + 2px 细进度线”的读数语法。美化时这四样都不能动；要修的是层叠冲突造成的不一致（字号 7 档、字重 11 档、圆角 9 种、三套卡片骨架、失效的悬停）。

---

## 0. 范围、来源与复现方法

### 0.1 “原版”指什么

线上看到的首页由 4 层 CSS 叠出来，后一层覆盖前一层。本文所有值都是**最终计算值**（真实浏览器里 `getComputedStyle` 读出来的），再注明它来自哪一层。

| 层 | 文件 | 作用 |
|---|---|---|
| L0 上游 | `lumina/styles/*.css`（LuminaPlus v1.3.3 源码，`~/Desktop/team-ops-v2/research/home/lumina-src/src/`） | 原主题：蓝色 `#3b82f6` 强调、径向渐变 KPI、8px 粗进度条、热力色流量条。**大部分视觉已被 L1 覆盖**，只在详情页、弹层、局部 hover 里还露出来 |
| L1 运营补丁（编译进产物） | `assets/index-DJY2ib8s-review20260917v4.css`，格式化副本 `prototype/original/harness/ref/index-v4.pretty.css`（下文“`pretty:行号`”指它） | 把 token 换成运营页同一套（`--bg/--card/--line/--fg/--accent`，与 `plugin/public/style.css` 第 1–2 行逐字相同），并追加 `.home-dashboard …` 扁平化规则（`pretty:4944–5120, 7103–7265, 7793–7833, 8158–8169, 8455–8496, 9027–9230`）。**这一层没有可读源码，只存在于编译产物里** |
| L2 refined 前半 | `assets/refined-review20260917v4.css` 第 1–49 行 | body 字体换成 `-apple-system,BlinkMacSystemFont,"PingFang SC"`；卡片先设 18px 圆角；`review-card-*` 四字段骨架；KPI 34px 行高等 |
| L3 home-refinement | `release/plugin/home/src/home-refinement.css`（同样被拼接进 `refined-…v4.css` 第 51 行以后） | KPI 改 27px/32px、圆角统一回 14px、卡片内距 16、网格间距 14、代理卡 `home-proxy-card`、入口流量表、证据行、焦点环 |

组件结构来自 `research/home/bundle-pretty/index-BwLl-MAK.js`（下文 `bundle:行号`）与可读补丁源码 `release/plugin/home/src/home-components.js`、`home-card-fields.js`。

### 0.2 复现方法（本地，不连生产）

`prototype/original/harness/` 用线上构建产物原样渲染首页，接口全部用 `prototype/data/snapshot.json`（脱敏真实快照）伪造：

- `mock_server.py`：`python3 mock_server.py 5187` → `http://127.0.0.1:5187/admin/home`。支持 `?theme=dark`、`?view=large|compact|mini`、`?bare=1`（只要 iframe）、`/admin/home/instance/<id>`、`/admin/home/traffic`。`/__cfg?offline=t1-02&health=t1-03:degraded&cpu=t1-04:94&due=t1-06:2026-09-20&slow=5000&fail=overview|rpc` 用来造离线、异常、阈值、加载、错误状态。
- `parent.html`：仿原生壳（顶栏 56px、侧栏 240px、内容区全幅 iframe）。**壳只是示意**，首页 iframe 内部是原产物。
- `cdp.mjs`：无头 Chrome（CDP）批量截图、悬停、点击、键盘聚焦、读取计算样式；`collect.js` 抽取 173 个选择器的计算样式与 token，结果在 `styles-{large,compact,mini}-{light,dark}.json`，`summarize.py` 打印摘要。
- 伪造数据与生产的差别：Ping 绑定按清单复现（30 个绑定，缺 11 个）；线路机 1/2 有绑定但快照里没有样本，所以显示空延迟条；详情页图表不区分时间范围。这些只影响数据，不影响样式。

### 0.3 截图索引（`prototype/original/shots/`，@2x 除注明外）

| 文件 | 内容 |
|---|---|
| `home-{large,compact,mini}-{light,dark}.png` | 1440×1500 带壳首屏，三种视图 × 深浅 |
| `shell-large-light-kpi-10.00.png` | 带壳首屏，KPI 出现 `10.00 MiB/s` 的舍入边界 |
| `top-{light,dark}.png` | 工具栏、KPI、分组页签、排序、地区条 |
| `full-large-{light,dark}.png` | 1200 宽 iframe 全页（@1x，41 张卡） |
| `card-large-{host,evidence,proxy,relay,relay-unbound,mx,offline}-{light,dark}.png` | 大卡片各形态：主机+Ping 条、证据行、家宽代理、线路机、线路机空条、墨西哥节点、离线 |
| `card-compact-*`、`card-mini-*` | 小卡片、紧凑卡（主机 / 代理 / 线路机） |
| `states-{large,compact,mini}-{light,dark}.png` | 健康 5 态 chip、CPU 76%/94% 阈值色、到期 4 天/已过期 |
| `kpi-offline-*` | 1 台离线时的 KPI |
| `state-*.png` | 卡片悬停、标题悬停、页签悬停、Ping 格子 tooltip、排序面板、今日流量弹层、三种焦点环 |
| `loading-2s-light.png`、`error-overview-light.png`、`error-rpc-light.png` | 加载、overview 失败、实时指标失败 |
| `instance-host-*`、`instance-proxy-*` | 详情页（负载 6 图 / Ping 图） |
| `traffic-*` | 流量明细页 |
| `resp-{390,900,1680}-*.png` | 响应式 |

---

## 1. 颜色 token

### 1.1 核心 token（L1，`pretty:1008–1110` 亮、`pretty:1120–1262` 暗；与运营页 `plugin/public/style.css` 第 1–2 行相同）

| token | 亮 | 暗 | 首页里用在哪（计算值实测） |
|---|---|---|---|
| `--bg`（= `--bg-0`） | `#f6f7f9` | `#0e1116` | `html` 画布；iframe 与原生壳 `--native-page #f5f6fa/#15161b` 差 1–2 级 |
| `--card`（= `--surface`、`--surface-a`、`--surface-elev` 亮色） | `#ffffff` | `#161a21` | KPI、卡片、卡片 footer、地区条、弹层、排序面板、tooltip |
| `--card-2`（= `--surface-sunken`） | `#f9fafb` | `#1c212a` | 分段控件轨道、排序按钮底、图标按钮 hover、地区 chip 选中、列表行 hover |
| `--line`（= `--border`、`--hairline`） | `rgba(17,24,39,.08)` | `rgba(255,255,255,.08)` | **所有**边框、分隔发丝线、2px/6px 进度轨道、空延迟格 |
| `--fg`（= `--text-primary`） | `#111827` | `#e5e7eb` | 标题、数值、字段值 |
| `--fg-2`（= `--text-secondary` **和** `--text-tertiary`） | `#6b7280` | `#9ca3af` | 所有标签、副行、单位、图标、未选中控件。原主题的三级文字被压成两级 |
| `--accent` | `#5b5bd6` | `#7c7cf0` | 进度线填充、流量额度条、焦点环、排序当前项图标、iframe 顶光 |
| `--accent-bg` | `color-mix(accent 20%)` | 同左 20% | 焦点光环 `box-shadow:0 0 0 3px`（运营页是 10%/16%，两边不同） |
| `--ok` | `#16a34a` | `#22c55e` | 在线圆点、`节点可用` pill、延迟格 ≤100ms |
| `--warn` | `#d97706` | `#f59e0b` | 进度线 ≥70%、到期 ≤7 天、延迟格 ≤300ms、`节点异常` |
| `--bad` | `#dc2626` | `#ef4444` | 进度线 ≥90%、已过期、离线圆点、延迟格 >300ms/失败、`节点不可用` |
| `--info` | `#2563eb` | `#60a5fa` | `维护中` pill（圆点 1.2s 呼吸） |
| `--neutral` | `#6b7280` | `#9ca3af` | 未知圆点、`未知`/`状态待更新`/`家宽代理`/`不提供主机数据` chip |
| `--{ok,warn,bad,info,neutral}-bg` | 各色 10% | 各色 16% | 状态 pill 底色 |
| `--floating-shadow` | `0 8px 30px rgba(0,0,0,.12)` | 同左 | 弹层、排序面板、tooltip（`pretty:1298`） |
| `--segment-shadow` | `0 1px 4px rgba(0,0,0,.12)` | 同左 | 分段控件选中项（暗色下几乎看不见） |
| `--canvas-glow` | `radial-gradient(1100px 560px at 50% -8%, accent 5%, transparent 60%)` | `none` | 亮色画布顶部极淡靛蓝“顶光”（空白页时可见，见 `loading-2s-light.png`） |

### 1.2 仍在起作用的上游 Lumina token（L0 残留）

| token | 亮 / 暗 | 露出的位置 |
|---|---|---|
| `--progress-cpu` | `#3b82f6` / `#539bf5` | 今日流量弹层“明细”链接、详情页图表默认色。**与靛蓝强调色冲突** |
| `--accent-strong` | `#2f5be0` / `#4184e4` | 弹层链接 hover |
| `--ring` | `rgba(59,130,246,.35)` / `rgba(83,155,245,.5)` | `.home-dashboard` 之外的 `:focus-visible`（全局 `pretty:8780`）——蓝色焦点环 |
| `--hover-bg` | `#f4f4f5` / `#2f3742` | 实际上几乎全部 hover 被 L1 覆盖（见 §6） |
| `--border-strong` | `rgba(24,24,27,.13)` / `#656c76` | 排序按钮原 hover 边框（被覆盖）、详情页图表卡 hover |
| `--fill-tertiary` | `rgba(24,24,27,.05)` / `rgba(101,108,118,.12)` | 弹层徽标、重试按钮 |
| `--latency-excellent…critical` | `#2fc66e #9fe339 #cbd83a #e2a928 #dc2626`（暗色 critical `#f47067`） | 详情页 Ping 任务 chip 的数值色（`pu()`：≤60/≤100/≤160/≤200/>200ms） |
| `--tag-*`、`--progress-memory/disk/network/load`、`--traffic-heat-spectrum`、`--speed-*` | 见 `lumina/styles/tokens.css` | 首页卡片里**全部失效**（标签 chip 统一成灰色描边，进度条统一成靛蓝），只剩详情页和死代码在用 |

上游原值（被覆盖、美化时不要回退到它们）：画布 `#f4f4f5`、强调 `#3b82f6`、文字三级 `#18181b/#52525b/#71717a`、暗色画布 `#212830`、卡片 `#262c36`。

### 1.3 状态与阈值配色（实测）

| 元素 | 规则 | 颜色 | 来源 |
|---|---|---|---|
| 资源进度线（CPU/内存/磁盘） | 占比 ≥0.9 `bad`；≥0.7 `warn`；否则 `accent` | `[data-tone]` → `--meter-color`（`pretty:7177–7185`） | `bundle:6494` `_d` |
| 负载进度线 | `load5/核数` >1 `bad`；≥0.7 `warn`；否则 `accent`；缺核数时退回 `load1/核数或4` 的占比 | 同上 | `bundle:5966` `tsLoadTone` |
| 流量额度条 | **不分阈值**，永远 `accent`；未设置上限时整条空轨道 | `.traffic-quota-track>span{background:var(--accent)}` `pretty:7195` | L1 |
| 延迟格 | ≤100ms `ok`；≤300ms `warn`；>300ms 或失败 `bad`；无样本 `empty`（= `--line`） | `pretty:7084–7092` | `bundle:3835` |
| 到期数值 | `已过期` → `--bad`；`今日` 或 ≤7 天 → `--warn`；其余 `--fg`（`homeExpiryModel` 算出的 8–30 天 `warn` 语气在首页没被用上） | 行内 style | `bundle:6859` |
| 在线圆点 | `online` true `ok` / false `bad` / 未知 `neutral` | `.node-status-dot` 6px（`pretty:7122–7133`） | |
| 健康 pill | ok→`节点可用`（线路机 `线路机正常`）ok；degraded→`节点异常` warn；fail→`节点不可用` bad；maintenance→`维护中` info（圆点呼吸）；unknown→`未知` neutral；overview 失败→`状态待更新` neutral | `.node-chip-{tone}` `pretty:7007–7026` | `home-components.js` `_l` |
| 离线卡 | 标题变 `color-mix(offline 72%, fg)`，圆点红；**卡片底色不变**（上游的 5% 红底和红边被 L1 `background:var(--card)` 覆盖） | `pretty:6451–6468` 被 `pretty:7103` 覆盖 | 见 `card-large-offline-light.png` |
| 标签 chip | 上游按 `<color>` 或关键字给 violet/blue/green；**L1 统一成灰描边**：`border:1px var(--line); color:var(--fg-2); bg:transparent` | `.home-dashboard .node-chip-outline` `pretty:7161` | |
| 价格 chip | 同描边，但文字 `--fg`、字重 600（上游是绿色填充） | `.home-dashboard .dstatus-price-chip` `pretty:7263` | |
| 详情页图表 | CPU `#5d88ff`、内存 `#a35cf5`、磁盘 `#f1873d`、网络 `#61c08f`、进程 `#d4a54a`；Ping 任务按 `oklch(0.7 0.2 h)` 均分色相（2 个任务 → 粉 0° 和青 180°），10 色备用盘 `#4878d0 #ee854a #6acc64 #d65f5f #956cb4 #8c613c #dc7ec0 #4aa7a0 #c7b446 #82c6e2` | `bundle-pretty/chartShared-Du5_uBlR.js:51–52` | |

---

## 2. 字体排印

### 2.1 字族

| 用途 | 计算值 | 来源 |
|---|---|---|
| 正文、标签、中文 | `-apple-system, "system-ui", "PingFang SC", sans-serif`（macOS 上是 SF Pro + 苹方） | `refined.css:3` 覆盖 `body`；`:root` 仍是 L0 的 `--font-sans: -apple-system, BlinkMacSystemFont, "SF Pro Text", "Inter Variable", "Inter", system-ui, "PingFang SC", "Hiragino Sans GB", "Microsoft YaHei", sans-serif`（`pretty:169`） |
| 数字 | `--font-numeric: "Inter Variable", "Inter", ui-sans-serif, system-ui, sans-serif`，**打包 7 个 Inter woff2**；`font-variant-numeric: tabular-nums slashed-zero` | `.tabular`、`.overview-card-value/-unit`、`.traffic-stat-*`、`.traffic-quota-remain/-usage`、`.server-card-meta-value/-unit`、`.compact-node-*-value` 等（`pretty:8693`、`lumina/styles/numeric.css`） |
| 全局 | `font-variant-numeric: tabular-nums`（整页，`:root` 与 `.home-dashboard`）；`letter-spacing:-.003em`；`-webkit-font-smoothing: antialiased`；`font-synthesis: none` | `pretty:8736` |
| 等宽 | 首页不用 | |

实际效果：同一张卡里 KPI、资源百分比、速率、到期天数是 Inter（带斜杠 0），IP、字段值（`线路机2:20002`、`50 Mbps`）、延迟 `12 ms`、代理卡入口流量表是 SF Pro。

### 2.2 字号 × 行高 × 字重（首页全部实测，亮暗相同）

| 角色 | 字号/行高 | 字重 | 字距 | 颜色 | 选择器 |
|---|---|---|---|---|---|
| 页标题 `首页` | 24/36 | 600 | −0.01em | fg | `.admin-home-toolbar h1`（≤600px 为 21px） |
| KPI 主值 | 27/32 | 600 | −0.035em | fg，Inter | `.home-dashboard .overview-card-value`（L3 `!important`） |
| 卡片标题（主机大卡） | 16/1.35 | 600 | −0.01em | fg | `.server-card-title-link`，min-height 28 |
| 卡片标题（代理卡 / 小卡片） | 15 | 600 | −0.01em | fg | `.home-proxy-card .server-card-title-link`、`.compact-node-title` |
| 卡片标题（紧凑卡） | 14 | 600 | −0.01em | fg | `.mini-node-title` |
| footer 数值（持续运行/到期） | 15/22.5 | 600 | −0.03em | fg 或状态色，Inter | `.server-card-meta-value` |
| 正文基准 | 13/19.5 | 400 | −0.003em | fg | `.home-dashboard` |
| 字段值、地址、资源数值、速率数值 | 13 | 400（字段）/ 600（数值） | — | fg / fg-2 | `.review-card-fields dd`、`.review-card-address`、`.metric-item .font-semibold`、`.traffic-stat-value` |
| 视图切换、排序按钮、地区 chip | 13 | 500 / 650 / 650 | — | fg-2 | 见 §4 |
| 标签、副行、单位、chip、分组页签、按钮 | 12/18 | 400（多数）、500（资源标签、页签）、600（额度标签、footer 标签、价格 chip） | 资源标签 +0.02em | fg-2 | 多处 |
| 资源副行（`2 核`、`165 MiB / 2.57 GiB`） | 11/13.2 | 560 | −0.01em | fg-2 | `.metric-detail` |
| IPv4 标记、容量 chip、代理身份 chip、表头 | 11 | 400/500 | — | fg-2 | `.review-card-address>span:last-child`、`.home-capacity`、`.home-proxy-identity .node-chip`、`.home-traffic-table thead th` |

统计：首页同时出现 **7 个字号**（11/12/13/14/15/16/24/27 里除 14 外都在大卡视图出现）和 **11 个字重**（400/500/550/560/600/620/640/650/660/700/720）。

---

## 3. 圆角、边框、阴影

| 值 | 用在 |
|---|---|
| `999px` 胶囊 | 分段控件轨道与按钮、分组页签、地区 chip、状态 pill、标签/价格/容量 chip、额度轨道 |
| `14px` | KPI 卡、卡片外壳 `.managed-node`、代理卡；内层 `article` 为 `14px 14px 0 0`，footer 为 `0 0 14px 14px` |
| `10px` | 吸顶工具栏底板、地区条、排序面板、今日流量弹层、Ping tooltip、小卡片信息格 |
| `9px` | 今日流量触发按钮（refined `.server-card-actions button`） |
| `8px` | 详情图标按钮、排序按钮、footer 文字按钮、`.home-dashboard :is(button,input,select)` |
| `6px` / `5px` / `4px` / `2px` / `0` | 地区计数（已无底色）/ 焦点 outline / KPI 图标按钮 / 紧凑卡进度线 / 延迟格与 2px 进度线 |
| 详情页 | 面板 `20px`、信息组与图表卡 `16px`、分段控件 `14px`+按钮 `10px`、切换器 `7px`（上游原样，没有被补丁统一） |

边框：一律 `1px solid var(--line)`；分区用同色发丝线（`border-top`）。卡片、KPI、地区条**无阴影**（L1 `box-shadow:none`）；只有浮层用 `--floating-shadow`，分段选中项用 `--segment-shadow`；详情页图表卡保留上游 `inset 0 1px rgba(255,255,255,.1), 0 12px 28px -24px rgba(0,0,0,.45)`。吸顶工具栏：`background: color-mix(card 85%, transparent)` + `backdrop-filter: saturate(180%) blur(12px)`。

---

## 4. 间距与栅格

### 4.1 页面纵向节奏（iframe 内，1200 宽时实测）

```
.app-main  padding 24（≤820px 16；≤600px 16/12）
工具栏      高 40，下边距 16（sticky top:0，z 40）
KPI 行      高 106，列间距 12，下边距 16
控制条      高 40，下边距 14，列间距 12（与卡片网格同列模板）
地区条      高 54，内距 10/12，下边距 16
卡片网格    列间距 14（≤760px 12），align-items:start
```

### 4.2 网格

| 区域 | 模板 | 1200 宽 iframe（= 1440 视口减 240 侧栏）实测 |
|---|---|---|
| KPI | `repeat(4, minmax(0,1fr))`，≤760px 变 2 列 | 4 × 279px |
| 大卡片 | `repeat(auto-fill, minmax(min(100%,360px),1fr))`（行内 style，`bundle:7207 Bd`） | 3 × 374.7px |
| 小卡片 | 同上，min 340 | 3 × 374.7px |
| 紧凑卡 | 组件给 `--mini-card-min-width:260px` 配 5/4/3/2/1 列断点，L3 改成 `auto-fill minmax(min(100%,280px))` | **仍是 3 × 374.7px**（4×280+3×14=1162>1152） |
| 控制条 | 大/小卡片沿用卡片列模板；紧凑用 min 340 | 页签占第 1 列，排序按钮 `grid-column:-2/-1` 右对齐 |
| 1680 宽 iframe | | 4 列约 397px |

卡片高度（1200 宽，大卡片）：主机 658px、证据行主机 654px、线路机 695px（套餐文字折 3 行）、代理 442px。小卡片主机 511、代理 415；紧凑卡主机 348、代理 409。

### 4.3 断点

| 宽度 | 变化 | 来源 |
|---|---|---|
| ≤820 | `.app-main` 内距 16 | `pretty:9191` |
| ≤768 | 控制条改 flex，排序按钮靠右 | `pretty:4864` |
| ≤760 | KPI 2 列、间距 10、内距 12、主值 24px、最小高 99；网格间距 12 | L3 |
| ≤720 | 卡片内距 16、标题 15；`review-card-address` 可换行；资源/流量区仍 2 列（L2 把上游的 1 列改回 2 列） | L0 `pretty:6982` + L2 |
| ≤680 | KPI 副行换紧凑写法 `↑1.53T ↓1.49T` | L0 |
| ≤600 | 标题 21px | `pretty:9137` |
| ≤560 | 分组页签整行、按钮 30px/11px | L0 |
| ≤380 | 同 680 的副行规则（L3 重复声明） | L3 |

### 4.4 卡片内部间距（大卡片）

内容区 `padding:16px; gap:12px`；四字段 `padding-top:10px; gap:8px 14px; 每格 min-height 34`；资源 2×2 `gap:12px 16px`，每项内部 `gap:5px`；后续每个分区 `border-top + padding-top 12 + margin-top 12`；footer 区 `padding-top 12, gap 10`；标签行 `gap 6, padding-top 2, min-height 24`；外壳 footer `padding 9px 14px, gap 6`。

---

## 5. 组件解剖

### 5.1 页首工具栏与视图切换（`Yd`，`bundle:7667`）

- `header.admin-home-toolbar`：flex 两端对齐，高 40，sticky 半透明白底 + 模糊，圆角 10，**无内边距**（标题贴着底板左缘，见 `top-light.png`）。
- `h1 首页`：24/36/600。
- `[role=group][aria-label=卡片视图]`：轨道 `--card-2`、内距 4、间距 4、胶囊；3 个按钮 `大卡片 / 小卡片 / 紧凑`，高 32、左右 12、13px/500、fg-2；选中 `aria-pressed=true`：底 `--card`、字 fg、`--segment-shadow`。整组宽 191.6px。
- 选择存 sessionStorage（`komaritheme:node-view-mode-session:desktop|mobile`）。

### 5.2 KPI 卡（`Ud`，`bundle:7218`）

`section.home-overview` 4 张 `article.overview-card[data-metric]`：

| 行 | 规格 | 内容（快照值） |
|---|---|---|
| 容器 | `display:grid; grid-template-rows:18px 32px auto; gap:5px; padding:13px 15px; min-height:104px; radius 14; 1px line; bg card; 无阴影；上游的径向色晕被 L3 关掉` | |
| 第 1 行 标签 | 12/18/400 fg-2（带 `text-transform:uppercase`，对中文无效）；右侧可选 32×32 图标按钮（`opacity:.82`，圆角 4，hover 变 fg） | `探针在线`、`主机传输速度`、`主机累计流量`（右上 `ChartNoAxesColumn` 19px → `/traffic`）、`每月费用`（右上 `CircleDollarSign` 18px → `/admin/billing`） |
| 第 2 行 主值 | 27/32/600 Inter，−0.035em；单位 12px fg-2，左边距 5 | `41` + `/ 41`；`9.95` + `MiB/s`；`3.02` + `TiB`；`$1281.35`（无单位） |
| 第 3 行 副行 | 12/18 fg-2，单行省略 | `41 在线 · 0 离线`；`↑ 4.93 MiB/s · ↓ 5.02 MiB/s`；`↑ 1.53 TiB · ↓ 1.49 TiB`；`41 项服务 · USD` |

- `主机累计流量` 标签的 `title` 给精确字节和“可能重复计量”说明；refined 里的 `::after` 说明被 L3 隐藏。
- 紧凑视图的 `is-dense` 缩小规则（`pretty:4913`）被 L3 全部盖掉，三种视图 KPI 一样大。
- 窄于 680：副行换 `↑4.95M ↓5.05M`（`Hd()` 只保留单位首字母）。

### 5.3 控制条：分组页签 + 排序（`Wd bundle:7365`、`zl bundle:5145`）

- `.home-group-tabs[role=group]`：与视图切换同一套胶囊分段（`--card-2` 轨道、内距 4、按钮高 32、选中白底 + 细阴影），但按钮 **12px/500**、`flex:1 0 auto`（最大 240px）所以四个按钮被均匀拉宽，整组 376px。`全部` + 主题 `homeGroupOrder`（团队1、团队2、线路机），选中值存 sessionStorage `komari:admin-home-group`。
- `.home-sort-trigger`：高 32、左右 12、圆角 **8**、1px line、底 `--card-2`、**13px/650** fg-2；图标 `ArrowUpNarrowWide/ArrowDownWideNarrow` 14px + 当前字段名（`默认`）。
- `.home-sort-panel`：最小宽 180，右对齐弹出，底 card、1px line、圆角 10、`--floating-shadow`，进入动画 `.2s cubic-bezier(.2,.8,.2,1)`（上移 4px、缩放 .98→1）；项 `默认 / 名称 / 实时网速 / 累计流量`，高 32、13px；当前项加粗并带 accent 方向图标；Esc 或点外部关闭（`state-sort-open-*.png`）。

### 5.4 地区条（`Gd`，`bundle:7395`）

- `section.home-region-bar`：通栏白卡，1px line、圆角 10、内距 10/12、下边距 16。只有地区数 >1 才出现。
- chip：高 32、左右 10、间距 6、胶囊、1px line、13px/650 fg-2；内容 = 国旗 22×14（`/assets/flags/US.svg`）+ 代码（`US`，+0.02em）+ 计数（12px/700 Inter fg-2，上游的底色块被 L1 去掉）。
- 选中（`aria-pressed=true`）：底 `--card-2`、字 fg；再点取消；不持久化。

### 5.5 卡片外壳与运营 footer（`vl`，`home-components.js`）

主机卡是“Lumina 卡片 + 运营 footer”拼成的：

```
div.managed-node          1px line，radius 14，bg card，flex 列（L1 pretty:9152）
├─ article.server-card    border 0，radius 14 14 0 0（内容见 5.6–5.8）
└─ div.managed-node-footer  border-top 1px line，padding 9px 14px，gap 6，flex 两端
   ├─ span.node-chip.node-chip-{tone}   状态 pill：12px、内距 2/8、胶囊、6px 实心圆点、底色 10%（暗 16%）
   └─ div.home-node-actions (gap 10→实测 12)
      ├─ span.node-chip-outline.home-capacity   11px 描边 chip：`50 Mbps` / `1 Gbps`（title 写配置带宽与“上下行分别计算利用率”）
      ├─ button 访问记录   12px fg-2，min-height 28，圆角 8，hover 下划线 → /admin/behavior?asset=
      └─ button 管理节点   同上 → /admin/nodes?asset=
```

### 5.6 大卡片（主机，`vd bundle:6543`；每一行按顺序）

以 `团队1-01` 为例（`card-large-host-light.png`，卡宽 374.7、高 658）：

1. **标题行**（`review-card-header` 网格：左 1fr 身份区 + 右 auto 操作区，间距 12）
   - 在线圆点 6px → 国旗 23×15 → 短名链接 `团队1-01`（16/600，title 是完整名 `团队1-01 · ATT · 198.51.100.11`），间距 6。
   - 右侧：今日流量触发（`ChartColumn` 15px，32×**34**，圆角 9）+ 详情链接（`ExternalLink` 15px，32×32，圆角 8，title `Debian… · 查看详情`），间距 6，图标 fg-2、线宽 1.5。
2. **地址行** `review-card-address`：左 `198.51.100.11`（13px fg-2），右 `IPv4`（11px fg-2），两端对齐，但右端只到身份区边缘，所以 `IPv4` 悬在卡片中部偏右。
3. **四字段** `dl.review-card-fields`（上边发丝线，2×2）：`连接入口 线路机2:20002`、`套餐 基础版 VPS`、`带宽 50 Mbps`、`流量上限 未设置`；dt 12px fg-2（下边距 2），dd 13px fg，允许换行（线路机的套餐 `GoMami · LAX.Pulse.Mini · 2 vCPU / 4 GB / 60 GB · 2000 GB 出站` 折成 3 行）。
4. **资源 2×2** `server-metric-grid`（无上边线，紧接字段）：每格
   - 头：图标 13px（`Cpu / MemoryStick / HardDrive / Gauge`）+ 标签 12px/500/+0.02em fg-2 ··· 右侧数值 13px/600 Inter + `%` 12px fg-2（左边距 1px）。
   - 进度线 2px，轨道 `--line`，填充按 §1.3 阈值。
   - 副行（`order:1` 被挪到进度线下面）11px/560 fg-2：`2 核`、`165 MiB / 2.57 GiB`、`1.91 GiB / 49.1 GiB`、`0.00 / 0.00 / 0.00`（负载 1/5/15）。
   - 数值精度：CPU/内存/磁盘 `toFixed(1)`（`0.0 %`），负载取整百分比（`0 %`、`81 %`）；负载 tooltip 解释口径。
   - 无主机数据时整块换成 `不提供主机数据` 灰 chip。
5. **实时流量** `server-traffic-section`（上边线 + 12/12）：首行 `实时`（12px fg-2，跨两列）；左右两格：`↑ 9.31 KiB/s` / `↓ 9.27 KiB/s`（箭头 15px fg-2，数值 13px/600，单位 12px/500）；下行 `Globe` 图标带 9px 小箭头 + `出站`/`入站`（12px fg-2）··· 累计 `51.8 GiB`（右对齐）。
6. **流量额度** `traffic-quota`（上边线）：`Database` 13px + `流量上限` + `未设置`（或 `剩余` + `886 GiB`），12px/600/+0.02em fg-2；右侧 `51.8 GiB / 未设置`（12px/560 Inter）；下方 6px 胶囊轨道，填充 accent（线路机 `1.09 TiB / 1.95 TiB` → 56%）。title `流量阈值 · 上下取大`。
7. **（可选）连接数**：`showConnections` 关闭，不渲染。
8. **延迟区**（上边线，三选一，见 5.11）。
9. **footer** `server-card-footer`（上边线，12，gap 10）：
   - 2 列：`RefreshCw` + `持续运行`（12px/600 fg-2）··· `10` + `天`（15px/600 Inter + 12px 单位）；`Calendar` + `到期` ··· `20 天`（颜色按 §1.3）。title 写到期日、账期时区、按日历日计算。
   - 标签行：描边 chip（12px、高 20、内距 2/8）`ATTVPS` `家宽` `线路机2`（过滤掉分组名、套餐名、`50 Mbps`）+ 价格 chip（`CircleDollarSign` 12px + `USD28.88/月`，600）。宽度不够时从尾部静默丢 chip（`连通性` 在 375px 卡里被丢掉，900 宽时才出现），整行 title 给完整列表。
10. **外壳 footer**（5.5）。

骨架：`.server-card.animate-pulse` 最小高 438px（真实 658），`[aria-busy]` 先隐藏 0.3s 再闪烁（`--card-2 → --card` 1.4s 线性）。

### 5.7 小卡片（compact，`cd bundle:6247`；`card-compact-host-light.png`）

1. 头 `compact-node-header`（min-h 30）：圆点、国旗、短名 15/600、流量触发、详情链接。
2. 元信息 `review-compact-meta`（2×2，12px fg-2，min-h 41）：`198.51.100.11 | IPv4`、`线路机2:20002 | 基础版 VPS`（只取四字段的前两个）。
3. 资源 `compact-node-vitals`（上边线 ~88% 不透明，10；2×2 间距 10/16）：`CPU 0%`、`内存 6.3%`…；标签 12px/660 fg-2，数值 13px/720 Inter（CPU `Mu()`：≥10 取整、<10 一位小数、0 写 `0%`）；2px 进度线（上游是 18 段圆角刻度，被 L1 换成直线）。
4. 信息格 `compact-node-info-strip`：1px line、圆角 10、2 列、间距 8；每格内距 8、min-h 52；`实时速率`（↑/↓ + `10.1 KiB/s`）、`累计流量`（↑/↓ + `51.8 GiB`）、`费用到期`（`Calendar 余 20天`、`$ USD28.88/月`）；数值 12px/600 Inter、单位 12px/620 fg-2，右对齐。3 个格子排进 2 列，第 4 格留空。
5. 流量额度 `compact-node-traffic`（上边线）：`流量上限 未设置` ··· `在线：10天`（12px/700 fg-2）/ `51.8 GiB / 未设置`（12px/640）；6px 胶囊 accent 轨道。
6. 延迟 `compact-node-bottom`（上边线）：同 5.11。
7. 外壳 footer。

### 5.8 紧凑卡（mini，`gd bundle:6466`；`card-mini-host-light.png`）

1. 头 `mini-node-header`：圆点、国旗、**完整名** `团队1-01 · ATT · 198.51.100.11`（14/600）、流量触发、详情链接（32×32，L1 把上游 24px 放大）。
2. chip 行（高 20，溢出隐藏）：价格描边 chip `USD28.88/月`（**无图标、灰字**，大卡是黑字 600）、`V4` 徽标（高 22）、标签 `ATTVPS 家宽 线路机2`。
3. 资源 2×2（上边线 8；间距 8/12）：标签 12px/560，数值 `strong` 13px/650 + `small` 12px fg-2；精度 ≥10 取整否则 1 位（`0.0%`）；2px 进度线（上游是 6px 虚线段）。
4. 流量两组（上边线 8，2 列）：左 `实时` ↑/↓ `9.73 KiB/s`，右 `累计` ↑/↓ `51.8 GiB`（13px/600）。
5. 延迟（上边线 8）。
6. 外壳 footer。无到期、运行时长、套餐、入口、上限。

### 5.9 家宽代理卡（`yl`，`home-components.js`；8 个 NodeMach 节点，三种视图共用，只改 `data-density`）

`card-large-proxy-light.png`（高 442，是同行主机卡的 2/3）：

- 外框：直接是 `article.server-card.managed-probe.home-proxy-card`（1px line、圆角 14、bg card），**没有外壳 footer**，footer 在卡内。内距 16、间距 12（小/紧凑密度 13/10）。
- 头：圆点（按健康语气：ok 绿、bad 红、其余灰）、国旗、**完整名**链接 15/600（`团队1-13 · NM · 198.51.100.23`，常折两行）；身份 chip 行（11px/20，内距 0/7）：描边 `线路机1` + 灰实心 `● 家宽代理`。右上：`History` 图标（访问记录）+ `ExternalLink`。
- 四字段 `dl.managed-probe-metrics`（列 `1fr 1.25fr`、间距 9/14）：`上游 TCP 47 ms`、`出口 IP 198.51.100.23`、`月费 $34.98`、`到期 2026-10-26`；dt 12/17 fg-2，dd 13/19/550 tabular。
- 入口流量 `section.home-channel`（上下两条发丝线，内距 10/0/8）：头 `入口流量`（12px/600）··· `Xray` `3 秒前`（11px fg-2；过期时变 warn）；表 `home-traffic-table`（12/20，列 42px + 两等分）：表头 11px/500 `周期 | ↑ 上传 | ↓ 下载`，行 `实时 0.00 Mbps`（650）、`今日 130.03 MiB`、`本月 1.35 GiB`（550），数值右对齐 tabular。
- 延迟区（5.11）。
- footer `managed-probe-footer`：状态 pill ··· `50 Mbps` `访问记录` `管理节点`，无上边线、无底色带。

### 5.10 线路机卡差异（`card-large-relay-light.png`）

与主机大卡同骨架；`连接入口 直接连接`；`带宽 1 Gbps`；`流量上限 1.95 TiB`（2000 GiB）；额度行变 `剩余 886 GiB` + `1.09 TiB / 1.95 TiB` 有填充；状态 pill `线路机正常`；标签只剩 `GoMami`；价格 `USD59/月`（整数不补 .00）。

### 5.11 延迟区（三种形态）

| 形态 | 条件 | 解剖 |
|---|---|---|
| Ping 格子条 `latency-strip` | 有首页 Ping 绑定且有样本 | 标签行 12px fg-2：`延迟 **12 ms**`（值 700 fg，`ze(x,1)` 所以会出现 `10.5 ms`）··· `丢包 0.0%`；下方 20 个 6×12px 方格、间距 2、**无圆角**，整条宽 158px 靠左；最新样本在右；格子可聚焦、悬停出 tooltip |
| 证据行 `dl.home-link-evidence` | 无 Ping 样本但 probe 新鲜 | 一行两端：`出口验证 309 ms` ··· `上游 TCP 13 ms`；12/20，dd 600，`ms` 400 fg-2；内距 6/0（`card-large-evidence-light.png`） |
| 空条 | 都没有（线路机） | `延迟 —` ··· `丢包 —` + 20 个 `--line` 灰格（`card-large-relay-unbound-light.png`） |

tooltip（`.node-health-hover-tooltip`，L1 改成浮层样式）：底 card、字 fg、1px line、圆角 10、`--floating-shadow`，位于格子上方 7px，带 6px 旋转方块尖角；文字 `20:22 - 20:22 · 24 ms`（浏览器本地时区）。触屏（`any-hover:none`）不显示。

### 5.12 今日流量弹层（`qu`，`bundle:5651`；`state-popover-light.png`）

- 触发：悬停（精确指针）、点击固定、聚焦；Esc/滚动/缩放/点外部关闭；portal 定位，宽 248。
- 外观：底 card、1px line、圆角 10、`--floating-shadow`、内距 12，12px 字；进入 `.12s` 下滑 3px。
- 内容：`主机今日流量`（头 12px fg-2）→ `↑ 上行 3.64 GiB`、`↓ 下行 3.63 GiB`（值 650 fg）；分隔线；`主机采样峰值` → `596 KiB/s 22:39`；footer（上边线，12px fg-2）`按 5 分钟采样 · 更新 20:26` ··· `明细`（**蓝色 `--progress-cpu`**，650）。
- 状态文案：`正在加载今日流量…`、`今日流量加载失败 [重试]`、`今日暂无采样数据`、`更新失败，当前为缓存数据`（带旋转重试按钮）。

### 5.13 chip 家族（全部是 `.node-chip` 变体）

| 变体 | 规格 |
|---|---|
| 状态 pill（实心） | 12px/1、内距 2/8、胶囊、gap 6、前置 6px 圆点 `currentColor`、底色 = 语气 10%（暗 16%）；`info` 圆点 1.2s 透明度呼吸 |
| 描边 chip | 同尺寸，`1px var(--line)`、字 fg-2、透明底、无圆点（标签、容量 11px、代理身份 11px） |
| 价格 chip | 描边 + `CircleDollarSign` 12px + 600 fg（大卡）；紧凑卡无图标 400 fg-2 |
| IP 徽标 `V4/V6` | 描边 chip，但高 22、+0.02em（只在紧凑卡） |
| 中性实心 | `不提供主机数据`、`家宽代理`、`未绑定` |

### 5.14 列表行（`Id`，代码仍在但不可达）

`Kt()` 把 `list` 强制改成 `compact`，工具栏也没有入口。CSS 仍在：行 min-h 40、内距 4/12、10 列（节点/系统/CPU/内存/磁盘/负载/实时/流量/网络/在线·到期），表头 sticky 12px/500 fg-2 半透明模糊，当前行左侧 2px accent（`pretty:8455–8496`）。美化时如要做表格视图，这是原版留下的唯一“表格语法”。

### 5.15 详情页 `/instance/<id>`（Instance chunk；`instance-host-light.png`）

- `← 返回`（13px fg-2）。
- 信息面板 `instance-panel`：圆角 **20**、1px border、内距 16；标题 `团队1-01 · ATT · 198.51.100.11 信息`（15/640）+ 22px 切换器（圆角 7）；三组 `系统 / 资源 / 网络`（3 列、间距 10；每组圆角 16、内距 9/12、`--surface-elev` 92%；组标题 12/700；行 12px，标签 fg-2、值右对齐，发丝线分隔）；总流量进度条 8px 胶囊，填充蓝→紫渐变，无上限时 135° 斜纹 + `51.8 GiB / ∞`。
- 运营动作行：`50 Mbps` `访问记录` `管理节点`（与卡片 footer 同组件）。
- 页签与范围：`负载 | Ping` 与 `实时 / 1 小时 / 4 小时 / 1 天 / 7 天 / 30 天` 两个分段控件（圆角 14 容器 + 10 按钮，12px/600，**不是首页的胶囊样式**）。
- 负载图表面板（圆角 20）：头 `负载图表` + 覆盖/采样 chip + `断点连线` 开关 + `刷新` + `实时`；6 张小图 3 列、间距 16；每张图卡圆角 16、内距 11/12、4% 指标色竖向渐变底、顶部 2px 指标色渐隐线（.75）、hover 上移 1px；uPlot 细线 + 淡填充，网格发丝线。
- Ping 面板（`instance-proxy-light.png`）：`延迟 | 丢包率` 分段、`削峰平滑`、`断点连线`、`隐藏全部`、`刷新`；任务 chip 胶囊描边（任务色），`名称 593.0 ms 0.0%`，数值按延迟阶梯着色；TCP 与出口验证同一根轴。
- 代理节点详情直接把 `yl` 代理卡拉到 1152px 通栏。

### 5.16 流量明细页 `/traffic`（`traffic-light.png`）

工具栏（`返回`、`流量明细` 18px/650、日期 `09/23`、刷新）；两张汇总卡（圆角 15、内距 18/20、总量 27px；标签保留上游 `.12em` 大字距，`X r a y 入口今日流量` 被拉开）；来源筛选胶囊组（圆角 10/7，选中 accent 10% 底 + accent 字，**与首页分段控件又不同**）；表格（行内距 12，列 `节点 / 来源 / 今日流量 / 上行速率 / 下行速率 / 操作`，来源 chip、`详情 ⌄`/`入口明细` 描边小按钮圆角 8）。

### 5.17 空、加载、错误（首页）

| 状态 | 表现 | 截图 |
|---|---|---|
| 首次加载 | 主题 `isReady` 前整页只有居中转圈（accent 顶边），iframe 顶光可见；约 2–9 秒 | `loading-2s-light.png` |
| 卡片骨架 | `[aria-busy].animate-pulse`：0.3s 后出现闪烁块，大卡 min-h 438 | — |
| overview 失败 | `getPublic` 串行等待 overview，失败时**整页**换成 `无法读取站点配置 / 请检查网络后重试。[重试]` | `error-overview-light.png` |
| 实时指标（rpc2）失败 | 页面照常，但所有数值变 `0`：`0 / 41`、`0 B/s`、`0.0 %`、`0 B / 2.57 GiB`，圆点变灰，`持续运行 —` | `error-rpc-light.png` |
| 节点列表错误 | `节点数据暂时无法加载`（14px）/ `正在等待后端自动重试`（12px），fg-2，高 40vh 居中 | — |
| 无节点 | `尚未连接到任何节点 · 等待后端推送或前往管理后台添加` | — |

---

## 6. 动效与交互状态（实测）

| 元素 | hover | active/选中 | focus-visible |
|---|---|---|---|
| 卡片 | **无可见变化**：上游 `.server-card:hover` 的底色与 `--border-strong` 边被 L1 `.home-dashboard :is(.server-card…){background:var(--card)}` 以同等优先级、更靠后覆盖（`state-card-hover-light.png`） | — | — |
| 卡片标题链接 | **无**（上游 hover 变 `--progress-cpu` 蓝，被 L1 `color:var(--fg)` 覆盖；无下划线）（`state-title-hover-light.png`） | — | 3px accent 20% 光环 |
| 图标按钮（详情、流量触发） | 底 `--card-2`、图标变 fg（白卡上 `#f9fafb` 几乎看不出） | 流量触发 `aria-expanded` 时 `--fill-tertiary` 底 | 光环 |
| KPI 图标按钮 | 不透明度 .82→1、变 fg；按下缩放 .94 | — | 3px 光环（上游 2px 灰 outline 被 L1 覆盖） |
| 分组页签 / 视图切换 | **无**（上游 hover 底色被 L1 `background:0 0` 以更高优先级覆盖）（`state-tab-hover-*.png`） | 白底 + `--segment-shadow`；暗色选中底 `#161a21` 反而比轨道 `#1c212a` 暗 | `0 0 3px accent-bg` 光环（`state-focus-light.png`） |
| 地区 chip | **无**（同上被覆盖） | `--card-2` 底 + fg 字 | 光环 |
| 排序按钮 | 只有文字变 fg（底与边的 hover 被覆盖） | 面板展开 | 光环 |
| footer 文字按钮 | 变 fg + 下划线 | — | **2px accent outline（offset 3、圆角 5）+ 3px 光环叠加**（`state-focus-card-light.png`） |
| 延迟格 | 出 tooltip，格子本身不变 | — | 同 hover 出 tooltip |
| 详情页图表卡 | 边框加深 + 上移 1px | — | — |

过渡：`.home-dashboard` 内 `background/border-color/color .15s`；分段按钮 `transform/background .18s ease-out`；浮层进入 `.2s cubic-bezier(.2,.8,.2,1)`；`prefers-reduced-motion` 时全部关闭（L1 与 refined 各写一遍）。

---

## 7. 深色模式差异

- token 见 §1.1：画布 `#0e1116`、卡片 `#161a21`、凹面 `#1c212a`、线 `rgba(255,255,255,.08)`、主文 `#e5e7eb`、次文 `#9ca3af`、强调 `#7c7cf0`，状态色提亮一级，状态底色 16%。
- 顶光 `--canvas-glow:none`；上游 KPI 深色特判 `background:var(--surface-a)` 仍在但被 L3 覆盖。
- 分段控件选中项比轨道更暗，`--segment-shadow` 在深色上不可见，选中态只靠文字亮度区分（`top-dark.png`）。
- 吸顶工具栏 85% `#161a21` 半透明，和画布 `#0e1116` 形成一块略亮的带子。
- 原生壳深色是 `#15161b/#1d1f27`、强调 `#9399ff`，与 iframe 的 `#0e1116/#161a21/#7c7cf0` 不同，接缝处能看出两块底色。
- 详情页深色沿用上游 Lumina 深色（`--surface-elev #2a313c` 等），和首页卡片的 `#161a21` 不是一套灰。

---

## 8. 首页内部缺陷清单（原版自身的问题，附证据）

### 8.1 单位、精度、写法不一致

| # | 现象 | 证据 |
|---|---|---|
| U1 | 同一页速率三种写法：主机 IEC 字节 `9.31 KiB/s`、`86 B/s`；代理入口十进制比特 `0.00 Mbps`、`<0.01 Mbps`；详情页网络图 `82.1 Kbps` 而同页文字 `10.1 KiB/s` | `card-large-host`、`card-large-proxy`、`instance-host-light.png`；`bundle:4613 homeRate` vs 主题 `Cn()` |
| U2 | 字节精度两套：主机 `vn()` 0–2 位可变（`51.8 GiB`、`2.57 GiB`），代理 `homeBytes()` 固定 2 位（`130.03 MiB`） | `bundle:4602` |
| U3 | 舍入边界：`9.995 MiB/s` 显示成 `10.00 MiB/s`（4 位有效数字），下一刻又变 `10.0` | `shell-large-light-kpi-10.00.png`；`vn()` 先按 <10 选 2 位再 `toFixed` |
| U4 | 百分比三种精度：大卡 `0.0 %`（1 位），小卡 `0%`/`6.3%`（`Mu()`），负载永远整数 `0 %`/`81 %` | `card-large-host`、`card-compact-host` |
| U5 | 数值与 `%` 之间有 1px 缝且单位变灰小一号，读成 `0.0 %`；小卡片又是紧贴的 `6.3%` | 同上 |
| U6 | 金额四种：KPI `$1281.35`（无千分位）、价格 chip `USD28.88/月`、整数价 `USD59/月`、代理 `$34.98`；KPI 副行又写 `USD` | `top-light.png`、`card-large-relay` |
| U7 | 到期三种：大卡 `20 天`（带色）、小卡 `余 20天`（无空格）、代理 `2026-10-26`（原始串、无色）；运行时长 `10 天` vs `在线：10天`（全角冒号） | `card-compact-host`、`card-large-proxy` |
| U8 | 上限 `2000 GiB` 显示为 `1.95 TiB`，而套餐字面是 `2000 GB 出站` | `card-large-relay-light.png` |
| U9 | 方向词相反：主机 `上行/下行`、`出站/入站`；代理 `↑ 上传 / ↓ 下载`（客户端视角）；KPI 先 ↑ 后 ↓ | 卡片截图 |
| U10 | 延迟精度随中位数变：`12 ms`、`10.5 ms`；证据行 `309 ms`；详情 chip `593.0 ms` | `ze(x,1)`；`instance-proxy-light.png` |
| U11 | 缺测显示成 0：实时指标失败时 `0 B/s`、`0.0 %`、`0 B / 2.57 GiB`；离线节点首载同样全 0 | `error-rpc-light.png`、`card-large-offline-light.png` |
| U12 | 同一行 `未设置` 既是字段值又是额度标签，额度条在未设置时仍画一条满宽空轨道 | `card-large-host-light.png` |

### 8.2 排版：挤、错位、截断

| # | 现象 | 证据 |
|---|---|---|
| L1 | `IPv4` 标记只对齐到身份区右缘（距内容区右缘约 82px，即操作按钮区左侧），看起来悬在卡片中间；11px 与左侧 13px IP 顶对齐，基线高出约 2px | `card-large-host-light.png`；`.review-card-address`（`refined.css:20`） |
| L2 | 两个标题图标按钮不一样高：流量触发 32×34 圆角 9（refined `.server-card-actions button{min-height:34px;border-radius:9px}`），详情链接 32×32 圆角 8 | 计算值 |
| L3 | 外壳 footer 内距 14，卡片内容内距 16，状态 pill 与上方内容左缘差 2px；代理卡 footer 在卡内、无分隔带，主机卡 footer 是通栏带，同一行两种 footer | `full-large-light.png`（@1x，y≈2900–4400，第 5–7 行） |
| L4 | 网格 `align-items:start` + 卡高 442/654/658/695 混排，行底参差；代理卡混在主机卡中间尤其明显 | `full-large-light.png` y≈2900–4400 |
| L5 | 标签行按宽度静默丢 chip（375px 卡丢掉 `连通性`），没有 `+1` 提示；ResizeObserver 测量后才定形 | `card-large-host` vs `resp-900-light.png` |
| L6 | 代理卡用完整名（含 IP）折成两行，主机卡用短名一行；紧凑卡又用完整名 | `card-large-proxy`、`card-mini-host` |
| L7 | 线路机套餐原文折 3 行，把卡片撑高 37px | `card-large-relay-light.png` |
| L8 | 小卡片信息格 3 格排进 2 列，右下角空一格；格内图标在左、数值在右，中间大片空白 | `card-compact-host-light.png` |
| L9 | 延迟格子条只有 158px 宽，`丢包` 在最右端，数值与格子脱节 | `card-large-host-light.png` |
| L10 | 吸顶工具栏白底板没有内边距，`首页` 贴着底板左缘；底板圆角 10 与下方卡片 14 不一致 | `top-light.png` |
| L11 | 地区条是一整条通栏白卡，只装两个 chip（`US 40`、`MX 1`），右侧 90% 空白 | `top-light.png` |
| L12 | 分组页签被 `flex:1 0 auto` 拉宽，四个按钮间距过大；同为分段控件，视图切换 13px、分组页签 12px、排序按钮 13px/650 圆角 8（不是胶囊） | 计算值 |
| L13 | 紧凑视图在 1152px 内容宽下仍是 3 列（与大卡片相同），只是卡变矮；`is-dense` KPI 规则全部失效 | `styles-mini-light.json` |
| L14 | 详情页代理卡被拉到 1152px 通栏，入口流量表三列相距 500px | `instance-proxy-light.png` |
| L15 | 详情 Ping 图 y 轴标签被裁：`000 ms`、`750 ms`（应为 `1,000`/`1,750`） | `instance-proxy-light.png` |
| L16 | 流量明细汇总卡标签保留 `.12em` 大字距，`Xray` 被拉成 `X r a y` | `traffic-light.png` |

### 8.3 状态表达互相打架

| # | 现象 | 证据 |
|---|---|---|
| S1 | 圆点（Komari 在线）与 pill（健康）各说各话：`节点不可用` 的卡圆点仍是绿色；离线卡圆点红、pill 仍是 `节点可用` | `states-large-light.png`、`card-large-offline-light.png` |
| S2 | 离线卡只有标题变红，上游的红底红边被补丁覆盖；而全部数值在首载时是 0 | `card-large-offline-light.png` |
| S3 | 代理卡圆点用健康语气，degraded/维护时变灰，与主机卡的“在线=绿”语义不同 | `home-components.js` `yl` |
| S4 | 强调色两套：交互和进度用靛蓝 `#5b5bd6`，弹层“明细”链接、上游全局焦点环、详情页默认图表色仍是蓝 `#3b82f6` | `state-popover-light.png` |
| S5 | 同一槽位图标含义不同：主机卡右上第一个是 `ChartColumn`（今日流量），代理卡是 `History`（访问记录），而代理卡 footer 里又有一个 `访问记录` | `card-large-proxy-light.png` |

### 8.4 交互反馈弱

| # | 现象 | 证据 |
|---|---|---|
| I1 | 卡片、标题、分组页签、地区 chip 的 hover 全部被补丁层按优先级吃掉，没有任何视觉反馈 | `state-card-hover-*`、`state-title-hover-*`、`state-tab-hover-*` |
| I2 | 图标按钮 hover 是 `#f9fafb` 叠在白卡上，基本不可见 | 计算值 |
| I3 | 焦点环两种：大部分是 3px 20% 光环（浅，暗色下更弱），footer 按钮是 2px 实线 + 光环双层 | `state-focus-light.png`、`state-focus-card-light.png` |
| I4 | 延迟 tooltip 文本 `20:22 - 20:22 · 24 ms` 起止相同，用浏览器本地时区；触屏不可见 | `state-latency-tooltip-light.png` |
| I5 | 口径说明（流量重复计量、到期时区、Mbps 精确值、带宽利用率）只在 `title` 里 | `bundle` 多处 |

### 8.5 更新与加载的抖动

| # | 现象 | 证据 |
|---|---|---|
| J1 | 实时指标每 2 秒全量推送，几乎每张卡每 2 秒重渲染；速率数字宽度随量级变化（`9.31 KiB/s` ↔ `86 B/s`），小卡信息格右对齐数值左右跳 | `inventory/home.md §11` |
| J2 | `排序：实时网速` 每 5 秒重排一次卡片 | `inventory/home.md §3.3` |
| J3 | `content-visibility:auto` 的占位高度（小卡 284、紧凑 228、代理 370、大卡骨架 438）与实际（511、348、442、658）差很多，滚动时滚动条跳动；实测离屏卡被量成 365/309 | `styles-{compact,mini}-light.json` `cards` |
| J4 | 主题配置要等两次 overview（约 254 KB/次）串行完成，之前整页只有转圈；overview 一失败整页报错 | `loading-2s-light.png`、`error-overview-light.png` |
| J5 | 标签行在字体加载完成后再测一次，chip 数量可能变化 | `Td`（`bundle:6800`） |

### 8.6 层叠与一致性（根因）

- 同一属性在 4 层里反复声明：KPI 圆角 16→14→18→14，卡片 14→18→14；`home-refinement` 里 `.overview-card-label{font-weight:500}` 和 `.overview-card-unit{font-size:13px}` 因为优先级低于 L1 的 `:is()` 组合而**从未生效**。
- 字号 7 档、字重 11 档、圆角 9 种（999/14/10/9/8/6/5/4/2），详情页再加 20/16/7。
- 三套卡片骨架（主机大卡 `server-card`+外壳 footer、`compact-node-card`、`mini-node-card`）加一套代理卡 `home-proxy-card`（dl + table），同一网格里混排。
- 分段控件四种样式：首页胶囊（两种字号）、排序方块按钮、详情页 14/10 圆角、流量页 10/7 圆角 + accent 选中。

---

## 9. DNA 不变量（美化时必须保留，才能“一眼认出是原来的平台”）

1. **画布与卡片**：浅灰 `#f6f7f9` 画布 + 白卡 + `rgba(17,24,39,.08)` 发丝线；卡片无阴影；亮色顶部极淡靛蓝顶光。深色 `#0e1116/#161a21`。
2. **靛蓝强调** `#5b5bd6`（暗 `#7c7cf0`）只出现在进度填充、额度条、焦点和当前项；状态色绿/琥珀/红/蓝各自独立。
3. **圆角语言**：卡片 14、浮层与条带 10、控件胶囊。
4. **胶囊分段控件**：凹面轨道 + 白色浮起选中项（视图切换、分组页签）。
5. **页面骨架与顺序**：`首页` 标题 + 右侧三档视图切换 → 4 张 KPI（探针在线 / 主机传输速度 / 主机累计流量 / 每月费用）→ 分组页签 + 右侧排序 → 地区 chip 条 → 3 列卡片网格。
6. **大卡片的行序与读数语法**：状态点+国旗+短名+两个图标 → IP+IPv4 → 2×2 字段 → 2×2 资源（灰标签 / 黑数值 / 2px 细线 / 灰副行）→ 实时 ↑↓ + 出站/入站 → 流量上限条 → 延迟文字 + 20 格绿条 → 持续运行 / 到期 → 标签 chip + 价格 chip → 状态 pill + 带宽 chip + 访问记录 / 管理节点。
7. **状态 pill**：带实心小圆点的胶囊、10% 底色；`节点可用` 绿。
8. **20 格方块心跳条**（6×12、间距 2）作为延迟的标志性图形。
9. **字体**：苹方/SF 正文 + Inter 等宽数字；数值黑、标签灰两级；KPI 大数字 27px 半粗。
10. **信息密度**：每张大卡的全部字段（清单见 `research/inventory/home.md §3`）都要在。

在这些不变量之内可以做的，就是把 §8 的问题收拢：统一字号/字重/圆角档位、恢复被吃掉的 hover、统一三套卡片与代理卡的骨架和 footer、对齐 `IPv4` 和图标按钮、统一单位与精度（按 `redesign-brief.md §4`）、缺测不显示 0、减少重渲染抖动。
