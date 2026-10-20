# fleetd API

fleetd 只监听 `127.0.0.1:25900`，浏览器经 Caddy 的 `handle /v2/api/*` 访问。所有路径都在 `/v2/api/` 下。

## 0. 通用约定

| 项 | 规则 |
|---|---|
| 鉴权 | 每个 `/v2/api/*` 请求都要 Komari **管理员会话**（浏览器已有的 `session_token` Cookie，同源请求自动带上）。fleetd 用它向 Komari `GET /api/me` 校验，结果按 Cookie 哈希缓存 30 s。 |
| 未登录 | `401 {"error":"请先登录管理员账户"}`（与旧 `/api/team-ops/*` 文案相同，适配器的 401 处理可直接复用） |
| Komari 不可用 | `503 {"error":"无法验证登录状态，请稍后重试"}` |
| 写操作 | 只有 fleetd 自己的写：确认告警、静默 / 维护窗口。必须带请求头 **`X-Fleet-Request: 1`**；若带 `Sec-Fetch-Site`，只能是 `same-origin` 或 `none`。否则 `403 {"error":"请求来源校验失败"}`。请求体 JSON（`Content-Type: application/json`），≤ 64 KB。每次写都进审计（actor = Komari 用户名）。 |
| 响应 | `application/json; charset=utf-8`，`Cache-Control: no-store`，`X-Content-Type-Options: nosniff`。fleetd 自有接口统一包一层 `{"data": …}`；错误统一 `{"error": "中文说明"}`。 |
| 时间 | 全部是 Unix 秒（浮点）。展示用北京时间；账期相关字段另带时区名。 |
| 旧接口 | `/v2/api/legacy/*` 只读，`POST` 等一律 `405 {"error":"旧系统接口只读"}`。 |

---

## 1. `/v2/api/legacy/<path>` —— 旧接口，逐字节兼容

浏览器适配器只需把 `apiBase` 从 `/api/team-ops/` 改成 **`/v2/api/legacy/`**，其余不变：路径、查询参数、状态码、响应体（`{"data": …}` / `{"error": …}`）都与旧插件 `/api/team-ops/*` 相同。

允许的路径（= 旧插件 `allowedGet` + `public-status`；其余 `404 {"error":"接口不存在"}`）：

```
overview  health  history  activity  incidents  behavior  panel/overview  renewals  jobs  jobs/:id  audit
node  fleet  policies  policies/preview  export  schema  schema.md  api-keys  api-keys/usage
provision/defaults  public-status
komari/hosts  komari/hosts/:uuid/metrics  komari/pings  komari/pings/:task_id/results  komari/clients
```

数据从哪来、多新：

| 路径 | 来源 | 新鲜度 |
|---|---|---|
| `overview`、`health` | 共享轮询器（所有浏览器共用一份） | 有 SSE 客户端时每 10 s；没有时每 60 s |
| `panel/overview` | 共享轮询器 | 有客户端时 15 s，否则按需 |
| `history?hours=24`、`incidents`、`behavior`、`activity?limit=500` | 共享轮询器 | 有客户端时 60 s，否则按需 |
| `renewals`、`jobs?limit=50`、`audit` | 共享轮询器 | 有客户端时 300 s，否则按需 |
| 其余（`node?asset=`、`history?asset=`、`behavior?asset=`、`activity?asset=`、`jobs/:id` …） | 按需代理：TTL 缓存（默认 15 s；`history`/`behavior`/`metrics`/`results` 60 s；`komari/pings`、`schema` 300 s；`jobs/:id` 30 s）、同一请求合并、到旧后端并发 ≤ 2 | 见 TTL |
| `komari/*` | **直接读 Komari**（不经旧后端的 `/komari/*`），形状与旧接口相同；`komari/clients` 去掉 `token` | 按需，TTL 同上 |

额外响应头：`X-Fleet-Cache: hit|miss`、`X-Fleet-Age: <秒>`（缓存年龄）、`ETag`（v0.3.4 起；带 `If-None-Match` 且内容没变时回 `304`，空体——200 的响应体仍与旧接口逐字节相同）。查询参数顺序无关（fleetd 规范化后作缓存键）。压缩由 Caddy 的站点级 `encode zstd gzip` 负责（对 `/v2/api/*` 同样生效）。

v0.3.4 起的限制：
- 按需代理的 `/activity` 的 `seconds` 不能超过 604800（7 天），否则 `400 {"error":"seconds 不能超过 604800（7 天）…"}`（14 天的查询在旧后端上要 16–20 s）。节点抽屉请改用 `seconds=604800`。
- 按需请求到旧后端的超时 15 s（超时回 503）；共享轮询器（overview/health/panel/…）有一个独占的旧后端槽，按需请求永远占不到它，慢请求不再拖慢 10 s 轮询与告警输入。
- SSE：资源第一次取到时不推 `change`（只有内容真的变了才推）。

`schema.md` 与旧插件一样返回 `text/markdown`。

---

## 2. `GET /v2/api/stream` —— SSE（浏览器不再轮询）

标准 `EventSource`（同源，自动带 Cookie）：

```js
const es = new EventSource('/v2/api/stream');
es.addEventListener('hello', e => { const h = JSON.parse(e.data); /* h.versions、h.resources */ });
es.addEventListener('change', e => { const c = JSON.parse(e.data); refetch(c.path); });
es.addEventListener('reset', () => refetchAll());
es.addEventListener('auth', () => showLogin());           // 会话失效，服务端随后断开
es.addEventListener('heartbeat', () => {});               // 每 15 s
```

帧格式：

```
retry: 3000

event: hello
data: {"epoch":1790186662,"last_id":"1790186662-41","versions":{"overview":7,"health":7,"alerts":3,…},
       "resources":{"overview":"/v2/api/legacy/overview","health":"/v2/api/legacy/health","panel":"/v2/api/legacy/panel/overview",
                    "history":"/v2/api/legacy/history?hours=24","incidents":"/v2/api/legacy/incidents","behavior":"/v2/api/legacy/behavior",
                    "activity":"/v2/api/legacy/activity?limit=500","renewals":"/v2/api/legacy/renewals","jobs":"/v2/api/legacy/jobs?limit=50",
                    "audit":"/v2/api/legacy/audit","provision_defaults":"/v2/api/legacy/provision/defaults"},
       "extra":{"alerts":"/v2/api/v1/alerts","notify":"/v2/api/v1/notify-log","silences":"/v2/api/v1/silences",
                "probes":"/v2/api/v1/probes/latest","ipintel":"/v2/api/v1/ipintel/anomalies","policies":"/v2/api/legacy/policies"},"heartbeat_s":15}

id: 1790186662-42
event: change
data: {"resource":"overview","version":8,"ts":1790186710.2,"path":"/v2/api/legacy/overview"}

event: heartbeat
data: {"ts":1790186725.0,"clients":1}
```

- `resource`：`overview | health | panel | history | incidents | behavior | activity | renewals | jobs | audit | provision_defaults`（旧接口，内容哈希变了才发），以及 fleetd 自有的 `alerts`（告警状态变化）、`notify`（写了新的通知记录）、`silences`、`probes`（记到新的探测样本）、`policies`（访问规则：草稿保存 / 丢弃 / 发布成功，以及喂数核对 relayd 回执后版本定版或回滚；`path` = `/v2/api/legacy/policies`；从启动起就在 hello 的 `versions` 里，初值 0）。有 Komari 系统 API 密钥时还有 `komari_hosts`、`komari_clients`。
- `path`：收到后去 GET 这个地址（它就在缓存里，命中很快）。
- `id` 形如 `<epoch>-<序号>`；浏览器断线重连会自动带 `Last-Event-ID`，服务端补发之后的帧（保留最近 512 帧）；太旧或 fleetd 重启过（epoch 不同）则发 `event: reset`，客户端全量重取一次。
- 第一个 SSE 客户端连上时，共享轮询器立即切到“有人查看”的节奏；最后一个断开后回到空闲节奏。
- 本机直连（不经 Caddy）`curl -sN http://127.0.0.1:25900/v2/api/stream` 不需要会话（运维观察；帧里只有资源名 / 版本 / 路径），心跳时也不做会话复核；连着时同样算一个客户端（节奏切到“有人查看”）。
- 响应头 `Cache-Control: no-store, no-transform`（Caddy 不压缩这条流）。

---

## 3. 套餐与资产

### `GET /v2/api/v1/plans`

```json
{"data":{"count":6,"plans":[
  {"id":"gomami.lax-pulse.mini","canonical_name":"GoMami Pulse Mini","provider":"gomami","provider_name":"GoMami",
   "category":"relay","category_word":"线路机","product_line":"gomami.lax-pulse","host_access":true,
   "supplier_plan_name":"LAX.Pulse.Mini",
   "specs":{"vcpu":2,"ram_mb":4096,"disk_gb":60,"disk_unit":"GB","bandwidth_mbps":1000,"bandwidth_direction":"in_out_each",
            "traffic_unlimited":false,"traffic_quota_gb":2000,"traffic_direction":"outbound","traffic_period":"billing_cycle",
            "traffic_text":"Allowance 2000 GB / OUTBOUND","ip_kind":"datacenter","isp":"NetLab Global（AS979）","nat":false,"ipv6":null,"region":"US-LAX"},
   "price":{"amount":"59.00","currency":"USD","cycle":"monthly"},"aliases":["…"],"open_questions":["…"],
   "spec_summary":"…","asset_count":2}
]}}
```

即 `catalog.json` 的 `plans[]` 原样（去掉 `alias_details`）+ `asset_count`。

### `GET /v2/api/v1/assets`

41 项资产（含 3 台线路机），每项把目录的规范套餐与旧系统的实时账单字段合在一起：

```json
{"data":{"count":41,"live_source":"legacy /overview","assets":[
  {"asset_name":"线路机1","short_name":"线路机1","id":"r1","team":"线路机","provider":"GoMami","service_id":"4374",
   "category":"relay","plan_id":"gomami.lax-pulse.mini","canonical_name":"GoMami Pulse Mini",
   "spec_summary":"2 核 / 4 GB / 60 GB；…","specs":{…同 plans…},
   "price":{"amount":"59.00","currency":"USD","cycle":"monthly","override":false},
   "due":{"due_date":"2026-10-01","expired_at":"2026-10-01T00:00:00-07:00","billing_timezone":"America/Los_Angeles",
          "days":8,"billing_cycle":"月付"},
   "quota":{"unlimited":false,"quota_gb":2000,"direction":"outbound","period":"billing_cycle",
            "text":"Allowance 2000 GB / OUTBOUND","used_bytes":1195862834632},
   "legacy_uuid":"8a2b6a08-f0ee-429d-9003-b795feb7d311","in_legacy":true}
]}}
```

- `id`：界面短名（`团队1-01`→`t1-01`，`线路机2`→`r2`）。
- `due.days`：**账期时区的日历天数**（与旧界面一致：日期标签，不是 24 h 取整）；`due` 为 `null` 表示旧系统暂不可达。
- `quota.used_bytes` 只对线路机有值（见下）。

### `GET /v2/api/v1/quota`

线路机出站用量对比套餐配额：

```json
{"data":{
  "relays":[
    {"relay":"线路机1","id":"r1","plan_id":"gomami.lax-pulse.mini","canonical_name":"GoMami Pulse Mini",
     "quota_gb":2000,"quota_bytes":2000000000000,"direction":"outbound","period":"billing_cycle",
     "reset_day":1,"timezone":"America/Los_Angeles",
     "used_bytes":1195862834632,"used_pct":59.8,"sampled_at":"2026-09-23T07:22:17.886149717Z",
     "legacy_monthly":{"up":13997258828,"down":802758309691},
     "komari_traffic_limit":2147483648000,"komari_traffic_limit_type":"up","due_days":8}
  ],
  "source":"用量：Komari Agent 上报的本期累计出站 net_total_up（经旧系统 /overview 的主机快照，约 5 s 同步）；配额：套餐目录 catalog.json",
  "caveats":["用量是 Komari 的计数，不是供应商面板读数；…","目录配额按十进制 GB（1 GB = 10^9 B）；Komari 的 traffic_limit 是另一口径（线路机1/2 为 2000 GiB）…","…"],
  "alert":"80% 起每 5% 一档（warning），≥ 100% 为 critical"}}
```

`komari_traffic_limit*` 只在 fleetd 读到过 Komari 客户端列表（有人打开过 `komari/clients` 或配置了 Komari API 密钥）时有值，否则为 `null`。

---

## 4. 事项队列（告警）

### 告警对象

```json
{"id":42,"rule":"node_health_fail","title":"节点故障","severity":"critical","state":"firing",
 "labels":{"kind":"node","node":"t1-31","node_name":"团队1-31","relay":"线路机3","relay_id":"r3","team":"团队1",
           "provider":"ATTVPS","entry":"线路机3:20031","check":"ingress","cause":"auth"},
 "summary":"团队1-31 实际入口到出口失败：auth（入口 线路机3:20031）",
 "evidence":{"check":"ingress","status":"fail","reason":"auth","measured_at":1790186801.2,"rc":35,"phase":"tls","expected_ip":"…"},
 "since":1790186780.0,"fired_at":1790186790.1,"resolved_at":null,"last_seen":1790186850.3,
 "ack_at":null,"ack_by":null,"acked":false,
 "suppressed":null,"group_key":"relay=线路机3|cause=auth","notified":"firing",
 "link":"https://38.58.57.22/admin/v2/app/#/nodes/t1-31"}
```

| 字段 | 说明 |
|---|---|
| `state` | `pending`（条件成立、还没到持续时间）→ `firing` → `resolved` |
| `severity` | `critical` / `warning` / `info`（info 不单独通知，只进每天 09:00 的摘要） |
| `since` | 条件开始的时刻（取旧 monitor 的 `since` 或主机最后上报时间，所以 fleetd 重启不会重新计时） |
| `suppressed` | `null` 或 `silence:<id>` / `maintenance:<id>` / `inhibited:<告警 id>`（例如被 `relay_down` 抑制）/ `aggregated:<告警 id>`（并入关联故障）。被压住的告警照常记录状态，只是不通知。 |
| `notified` | 最近一次通知里它的状态（`firing`/`resolved`/`null`=还没通知过） |
| `group_key` | 通知分组 `relay=<线路机或->|cause=<原因>` |
| `link` | 通知里用的深链（`#/nodes/<短名>`、`#/lines/<r1>`、否则 `#/alerts`） |

规则（`rule`）一览：

| rule | 级别 | 触发（全部从旧系统 `/overview` + `/health` 推导） |
|---|---|---|
| `node_health_fail` | critical | 旧 monitor 判定节点 `fail`（原因 = 旧 reason/检查项，如 `auth`、`timeout`、`upstream_down`、`stale`） |
| `node_health_degraded` | warning | 节点 `degraded` 持续 ≥ 5 min（原因如 `unstable`、`policy_blocked`、`<检查>_degraded`） |
| `egress_mismatch` | critical | 认证出口 / 实际入口到出口的出口 IP ≠ 期望 IP |
| `host_offline` | warning | 主机探针 ≥ 120 s 没有上报或离线（节点；线路机在管理通道正常时） |
| `host_resource` | warning | CPU / 内存 / 磁盘超过阈值 ≥ 120 s |
| `host_sync_stale` | warning | 插件同步 Komari 主机快照停了（此时暂停 `host_offline` 判断） |
| `relay_down` | critical | 线路机主机离线 **且** 管理通道中断 ≥ 60 s；抑制该线路机名下所有节点告警 |
| `relay_mgmt_down` | critical | 管理通道中断（主机在线）≥ 60 s |
| `relay_xray_down` | critical | 面板报告 Xray 未运行 ≥ 60 s |
| `collector_stale` / `monitor_stale` | critical | 采集循环 / 健康评估停止（原生模式下是 fleetd 自己的喂数与 monitor 评估）；抑制全部节点告警 |
| `correlated_node_failure` | critical | 同一线路机（或跨线路机的同一供应商）≥ 3 个节点在 2 min 内相继故障 → 一条通知，成员标 `aggregated` |
| `renewal_due` | 7 天 info / 3 天 warning / ≤ 1 天（含过期）critical | 节点与线路机的到期日，账期时区日历天数；续费后自动结束，不发恢复通知 |
| `quota` | warning（≥ 100% critical） | 线路机本期出站 ≥ 80%，每 5% 一档；只保留当前档 |
| `legacy_source_down` | critical | fleetd 读不到旧后端（`cause=legacy_unreachable`）或 Komari（`komari_unreachable`） |
| `channel_failing` | critical | 某通知渠道连续 3 次投递失败 |

新鲜度（`host_offline`、`host_sync_stale`、`collector_stale`、`monitor_stale`）按数据自己的渲染时刻算（overview 的 `now`、health 的 `time`），不按评估时刻：
评估用的是轮询缓存里的那一份（空闲时最多 60 s 前取的），缓存本身过旧由 `legacy_source_down` 负责。

启动宽限（`[alerts] startup_grace_s = 120`、`startup_grace_max_s = 600`）：fleetd 启动后 `startup_grace_s` 内，以及之后直到评估用的数据确实是启动后产生的
（原生模式收到第一批边缘上报且主机快照在那之后刷新过；health 的 `collector_at` / `monitor_at` 都在启动之后），最长到 `startup_grace_max_s`，
`host_offline`、`host_sync_stale`、`relay_down`、`relay_mgmt_down`、`collector_stale`、`monitor_stale` 与 `native_host_offline` 不新开、已有的也不当作恢复（重启不会造成“恢复 → 再触发”）。
`/v2/api/v1/system` 的 `alerts.evaluation.startup_hold` 显示上一次评估时宽限是否仍在。

### `GET /v2/api/v1/alerts`

查询参数：`state=open|firing|pending|acked|unacked|resolved|all`（默认 `open` = pending + firing）、`severity=`、`rule=`、`relay=`（线路机全名）、`node=`（短名）、`limit=`（默认 200，≤ 1000）。排序：firing → pending → 其余，同状态按级别、时间倒序。

```json
{"data":{"state":"open","shadow":true,
  "counts":{"pending":0,"firing":3,"acked":1,"suppressed":1,"critical":2,"warning":1,"info":0},
  "alerts":[ {…告警对象…} ]}}
```

`counts` 是全局计数（不受筛选影响），给侧栏角标用。

### `GET /v2/api/v1/alerts/{id}`

告警对象 + `"notifications":[{"id":17,"ts":…,"channel":"shadow","status":"shadow","kind":"group","title":"…"}]`（含它的通知记录）。

### `POST /v2/api/v1/alerts/{id}/ack`（写）

请求体可选 `{"note":"已联系供应商"}`。返回更新后的告警对象。确认后该告警不再重复提醒（critical 30 min / warning 4 h 的重复）。已结束的告警：`409 {"error":"告警已结束，无需确认"}`；不存在：`409 {"error":"告警不存在或已结束"}`。

### 静默与维护窗口

对象：

```json
{"id":3,"kind":"maintenance","matchers":[{"name":"relay","value":"线路机1","negate":false}],
 "starts_at":1790190000.0,"ends_at":1790197200.0,"tz":"Asia/Shanghai","comment":"换机房",
 "created_by":"admin","created_at":1790186900.1,"deleted_at":null,"deleted_by":null,
 "status":"active"}
```

`status`：`scheduled` / `active` / `expired` / `deleted`。

- `GET /v2/api/v1/silences?kind=silence|maintenance&all=1`（默认只列未删除、未过期的）→ `{"data":{"silences":[…]}}`
- `GET /v2/api/v1/maintenance?all=1` → `{"data":{"maintenance":[…]}}`
- `POST /v2/api/v1/silences`、`POST /v2/api/v1/maintenance`（写）：

  ```json
  {"matchers":{"relay":"线路机1"},            // 或 [{"name":"node","value":"t1-31","negate":false}, …]，1–8 条，全部满足才匹配
   "starts_at":"2026-09-24 10:00",           // 可省略（= 现在）；也可 Unix 秒或 RFC 3339
   "ends_at":"2026-09-24 12:00",             // 或 "duration_minutes": 120
   "tz":"Asia/Shanghai",                     // 本地时间按这个时区解释，默认 Asia/Shanghai
   "comment":"换机房"}
  ```

  可匹配的标签：`node relay relay_id provider team cause kind asset check channel entry threshold scope`。时间窗 ≤ 7 天。成功 `201 {"data":{…对象…}}`；参数错误 `400 {"error":"…"}`。
- `DELETE /v2/api/v1/silences/{id}`、`DELETE /v2/api/v1/maintenance/{id}`（写）→ `{"data":{"id":3,"deleted":true}}`

### `GET /v2/api/v1/notify-log`

参数 `limit`（默认 100）、`status=shadow|sent|failed`、`channel=`。

```json
{"data":{"shadow":true,"notifications":[
  {"id":17,"ts":1790186800.2,"channel":"shadow","status":"shadow","kind":"group","severity":"critical",
   "group_key":"relay=线路机3|cause=auth","title":"[严重] 节点故障 · 线路机3 · auth",
   "body":"[严重] 节点故障 · 线路机3 · auth\n新增 1\n\n● [严重] 节点故障：团队1-31 实际入口到出口失败：auth（入口 线路机3:20031）\n  证据：check=ingress status=fail reason=auth rc=35 phase=tls\n  开始 09-24 02:13:00 · 已持续 10 秒\n  https://38.58.57.22/admin/v2/app/#/nodes/t1-31\n\n事项队列：https://38.58.57.22/admin/v2/app/#/alerts",
   "alert_ids":[42],"error":null,"latency_ms":null}]}}
```

`kind`：`group`（分组首发 / 变化）、`repeat`（未确认重复）、`digest`（每天 09:00 摘要）。影子模式下 `channel = "shadow"`、`status = "shadow"`，`error` 里写“本应发往 …”。通知文案只写技术对象（节点、入口、线路机、原因、证据、链接），不写客户影响。

### `GET /v2/api/v1/channels`

```json
{"data":{"shadow":true,"healthchecks":false,
  "note":"未配置任何通知渠道：影子模式，所有通知只写入通知记录（status = shadow）",
  "channels":[{"name":"tg-ops","type":"telegram","min_severity":"warning","target":"chat -1****90",
               "state":{"sent":12,"failed":0,"consecutive_failures":0,"last_status":"sent","last_at":1790186800.2,"last_error":null}}]}}
```

机密一律不返回（chat id 打码、webhook 只给主机名）。

### `GET /v2/api/v1/audit?limit=100`

```json
{"data":{"chain_ok":true,"chain_broken_at":null,"audit":[
  {"id":7,"ts":1790186901.0,"actor":"admin","action":"maintenance.create","target":"maintenance:3",
   "detail":{"matchers":[…],"starts_at":…,"ends_at":…,"tz":"Asia/Shanghai","comment":"换机房"}}]}}
```

动作：`alert.ack`、`silence.create/delete`、`maintenance.create/delete`、`catalog.import`。审计表只追加、带哈希链。

---

## 5. 探测历史（延迟条）

旧接口每个节点只给“最新一次”探测。fleetd 每次轮询 `/overview` 时把三类结果记下来（按 `measured_at` 去重），保留 24 h：原始样本 + 5 分钟桶。
**失败、超时、出口不一致一律记为状态，`ms` 为 `null`，绝不当作延迟值。**

| kind | 来源（`/overview` 资产字段） | 含义 |
|---|---|---|
| `tcp` | `probe.tcp_ms / tcp_at / tcp_rc / tcp_error` | 线路机到落地 SS 端口的 TCP |
| `egress` | `probe.egress_ms / egress_at / egress_rc / egress_error / actual_ip / egress_attempt` | 认证出口验证（一个周期最多 3 次尝试） |
| `path` | `path.egress_ms / measured_at / healthy / rc / actual_ip` | 实际入口到出口 |

样本 `status`：`ok`（有 `ms`）｜`fail`｜`timeout`（rc = 28 或错误含 timeout）｜`mismatch`（出口 IP ≠ 期望）｜`retry`（认证出口周期内未到最后一次的失败尝试，不计入丢失率）。

采样分辨率 = 轮询周期：有人查看时 10 s，没人时 60 s（fleetd 一直在记，所以页面一打开就有完整的 20 格）。TCP 探测本身约 10–30 s 一次、认证出口约 5 min 一个周期、入口到出口约 60 s，20 格分别覆盖约 3–20 min / 100 min / 20 min。

### `GET /v2/api/v1/probes/latest`

每个节点每类最近 20 个样本（旧 → 新）：

```json
{"data":{"recent_size":20,"nodes":{
  "t1-01":{"node":"t1-01","name":"团队1-01","uuid":"dfe4c76d-…","relay":"线路机2",
    "tcp":{"samples":20,"last":{"ts":1790186815.5,"status":"ok","ms":8,"rc":0,"phase":"connect"},
           "recent":[{"ts":1790185675.5,"status":"ok","ms":9,"rc":0,"phase":"connect"}, …,
                     {"ts":1790186755.5,"status":"timeout","ms":null,"rc":28,"error":"Connection timed out"}, …]},
    "egress":{"samples":20,"last":{"ts":…,"status":"ok","ms":627,"rc":0,"attempt":1,"actual_ip":"75.37.237.38","expected_ip":"75.37.237.38"},"recent":[…]},
    "path":{"samples":20,"last":{"ts":…,"status":"ok","ms":333.7,"rc":0,"actual_ip":"…","expected_ip":"…"},"recent":[…]}}
}}}
```

不足 20 个时 `recent` 就是已有的那些（`samples` 给出个数），前面补空格由界面决定。SSE 资源 `probes` 在记到新样本时推送（`path` = `/v2/api/v1/probes/latest`）。

增量（v0.3.4）：`GET /v2/api/v1/probes/latest?since=<上次的 until>` 只返回 `ts > since` 的新样本：
`{"data":{"since":…,"until":…,"nodes":{"t1-01":{"tcp":{"new":[…]},"egress":{"new":[…]},"path":{"new":[…]}}}}}`（没有新样本的节点不出现）。
不带 `since` 的全量响应里也有 `until`。两种响应都带 `ETag`，支持 `If-None-Match` → 304。

### `GET /v2/api/v1/probes?node=<短名或 uuid>&range=1h|4h|6h|12h|24h&kind=tcp|egress|path|all&raw=1`

```json
{"data":{"node":"t1-01","range":"1h","since":1790183215.0,"kinds":{
  "tcp":{"step":300,"buckets":[
     {"t":1790183100,"samples":10,"ok":9,"failed":1,"retry":0,"loss_pct":10.0,"avg_ms":8.4,"min_ms":5,"max_ms":14}, …],
   "raw":[{"ts":…,"status":"ok","ms":8,…}, …]},
  "egress":{…},"path":{…}}}}
```

`loss_pct` = (fail + timeout + mismatch) / (samples − retry)；`avg/min/max` 只统计 `ok` 样本；没有样本的 5 分钟桶不出现（界面按 `step` 自己补空）。`raw=1` 附原始样本（每类最多 5000 条）。

---

## 5a. IP 情报（fleet/crates/ipintel，v0.2.0 起）

来源 IP 的运营商 / 城市 / 接入方式（只查中控本地的离线库，不把任何 IP 发往在线服务）、每个节点的来源画像、出口 IP 历史、异常。
接口形状见 `fleet/crates/ipintel/WIRING.md` §4，这里只列路径（全部走同一套会话鉴权与 CSRF）：

| 方法 路径 | 说明 |
|---|---|
| `GET /v2/api/v1/ipintel/{ip}` | 单个 IP 的情报、在哪些节点上出现过、是不是某节点的出口、相关异常 |
| `GET /v2/api/v1/nodes/{id}/sources?window=1h\|24h\|7d\|14d\|30d&limit=50` | 节点来源分布（isp/city/province/country/asn/type，按连接数，带 pct）、top 来源、已学到的基线、出口当前值与历史、该节点的异常。`id` = uuid、短名或名称 |
| `GET /v2/api/v1/ipintel/anomalies?status=open\|closed\|all&kind=&node=&severity=&since=&limit=` | 异常列表与计数 |
| `POST /v2/api/v1/ipintel/anomalies/{id}/close`（写，需 `X-Fleet-Request: 1`） | 关闭异常（出口 IP 变更只能这样结束）；fleetd 写审计 `ipintel.anomaly.close` |
| `GET /v2/api/v1/ipintel/status` | 离线库版本、`loaded`（应为 5）、错误、表行数 |
| `GET /v2/api/v1/ipintel/backfill` | fleetd 侧状态：启动回填进度（`backfill`: running/done/done-earlier/disabled）、请求数、事件数、循环次数、已开异常数 |

异常进入告警引擎（规则 `ipintel_new_isp`、`ipintel_new_city`、`ipintel_shared_identity`、`ipintel_datacenter_source` 为 warning，`ipintel_egress_ip_changed` 为 critical 且不会自动结束），出现在 `/v2/api/v1/alerts` 里，标签 `node`/`relay` 与其他告警一致（静默、维护窗口、relay_down 抑制照常生效）。SSE 资源名 `ipintel`（有新异常或关闭时推送，`path` = 异常列表）。

数据来源：旧 `/health`（节点目录与出口 IP）+ `/activity?limit=500`（来源，60 s 一次，有人查看时 30 s）；启动时回填每个节点最近 14 天（按天取样，每节点 8 页，限速）。三台线路机的公网 IP 固定视为内部地址（它们自己的探测连接不算“机房来源”）。

## 5b. `GET /v2/api/v1/health/shadow`（影子核对）

fleetd 用同一份旧 `/overview` 跑移植版 monitor（fleet/crates/health），逐节点与旧 `/health` 比较状态与原因码：

```json
{"data":{"evaluations":8,"compared":328,"agree":326,"agree_pct":99.39,"last_ms":8.8,
  "last_disagree":[{"node":"t1-31","ours":{"status":"degraded","reason":"auth"},"legacy":{"status":"fail","reason":"auth"}}],
  "disagree_by_reason":{"degraded/auth vs fail/auth":1}}}
```

只用于核对；告警仍来自旧 `/health`。偶发差异来自两边评估时刻相差一步（旧 monitor 在自己的周期评估）。

### 5b 续：`native`（v0.3.0 起）

`GET /v2/api/v1/health/shadow` 的 `data.native` 给出逐线路机的原生判定状态：

```json
{"mode":"auto","native_after_s":600,"rows_loaded":1234,"last_eval_ms":3.8,
 "relays":[{"relay":"线路机3","host_id":"line3","relayd_age_s":10.3,"nodes":11,"targeted":11,"fresh":11,"missing":[],"stale":[],
            "complete":true,"complete_since":1790191145.2,"source":"native","compared":110,"agree":108,
            "last_disagree":[{"node":"t1-31","native":{"status":"ok","reason":null},"legacy":{"status":"degraded","reason":"unstable"}}]}]}
```

`legacy_only` 列出 relayd 不探测的节点（SOCKS5 代理产品，例如线路机1 名下的 NodeMach 8 台）：它们不计入覆盖要求，状态始终来自旧 `/health`。覆盖窗口的起点持久化在 fleetd 库里，重启间隔 ≤ 300 s 时沿用，不再重新等 600 s。本机直连（无 X-Forwarded-For）的 `curl http://127.0.0.1:25900/v2/api/v1/health/shadow` 不需要会话。

规则：某线路机名下旧系统的全部（可探测）节点都在 relayd 的在测列表里、每个节点的 tcp 与 egress 结果都在 180 s 内、relayd 自己 90 s 内上报过——这种“覆盖完整”持续 600 s 后，`source` 变成 `native`，这台线路机名下节点的状态（以及由它推导的告警）改用原生判定（fleet-health）；覆盖一断立即退回 `legacy`。`/v2/api/legacy/health` 始终原样转发旧系统，不受影响。原生判定没有 L3（实际入口到出口），`ingress` 检查记 `na`。

## 5c. 边缘代理（fleet-edge：relayd / nodeagent）

- 上报：`POST /v2/ingest/{node,relay}`（**不走**会话鉴权，用 agent 令牌 `Authorization: Bearer …`；Caddy 路由 `/v2/ingest/*`，请求体 ≤ 256 KB）。
- 令牌：中控上 `runuser -u fleetd -- /opt/fleetd/current/fleetd agent-token issue <host_id> [--name N]`（只打印一次，库里只存 sha256）；`revoke`、`list` 同理。线路机 host_id：`line1`=线路机1、`central`=线路机2、`line3`=线路机3（`[edge] relay_hosts` 可改）；节点 host_id 是资产短名称（「团队1-24」）。
- 只读（会话鉴权）：`GET /v2/api/v1/hosts`、`/v2/api/v1/hosts/{id}/series?metric=…&range=24h`、`/v2/api/v1/relays/{id}/xray?range=1h[&kind=&tag=]`（线路机 Xray 累计流量——真实来源）、`/v2/api/v1/relays/{id}/probes?range=15m`、`/v2/api/v1/ingest/alerts`。形状见 `fleet-edge/fleetd-ingest/WIRING.md`。
- 告警：`native_host_offline`（120 s 无批次；线路机 critical、节点 warning；fleetd 启动后 180 s 内以及启动宽限期内不开）、`native_host_resource`、`native_cpu_steal`、`native_cpu_iowait`；`relay_xray_down` 现在还来自 relayd：`cause=xray_process_down`（进程不在）、`xray_api_unreachable`（Stats API 连续 ≥ 60 s 不可达）、`relayd_silent_nodes_failing`（relayd ≥ 120 s 不上报且名下 ≥ 2 个且过半节点失败）。旧面板证据（x-ui 在跑且报告 Xray 停）的判定保留，切到 fleet-xray 的线路机上它不会再触发。
- 同一节点同时有 `egress_mismatch`（旧健康检查）与 `ipintel_egress_ip_changed` 时，后者被前者抑制（`suppressed = inhibited:<id>`），只发一条。

## 5d. 上架流水线（fleet/crates/onboard）

`/v2/api/v1/onboard/*`（会话鉴权；写操作要 `X-Fleet-Request: 1`）原样转给 onboard 的 API：`GET/POST /batches`、`GET /batches/{id}`、`/plan`、`POST /start {plan_hash}`、`/resume`、`/cancel {row?}`、`POST /batches/{id}/rows/{name}/actions`、`GET /events?after=N`、`/relay-batches`、`GET /assets`。接口形状见 `fleet/crates/onboard/WIRING.md` §2–3。fleetd 对 2xx 的写操作记审计 `onboard.<动作>`，并推 SSE 资源 `onboard`。未配置 `onboard_config` 时返回 `404 {"error":"上架流水线未启用…"}`。

---

## 8. 账户与登录（v0.4.0 起；替代 Komari 登录）

| 方法 路径 | 请求 | 响应 |
|---|---|---|
| `POST /v2/api/v1/auth/login` | 头 `X-Fleet-Request: 1`（防登录 CSRF；`Sec-Fetch-Site` 须同源）；体 `{"username","password","totp"?}` | `200 {"data":{"username","expires_at","idle_timeout_s":86400,"totp_enabled"}}` + `Set-Cookie: fleet_session=…; Path=/; HttpOnly; SameSite=Strict; Max-Age=604800; Secure`<br>`401 {"error":"用户名或密码错误"}`<br>`401 {"error":"需要两步验证码","totp_required":true}`（再带 `totp` 重试）/ `401 {"error":"两步验证码不正确","totp_required":true}`<br>`429 {"error":"尝试次数过多，请 N 分钟后再试"}`（同一来源 IP 或用户名 15 分钟内失败 5 次，锁 15 分钟）<br>`403 {"error":"请求来源校验失败"}` |
| `POST /v2/api/v1/auth/logout` | 写（`X-Fleet-Request: 1`） | `200 {"data":{"logged_out":true}}` + 清 Cookie（`Max-Age=0`） |
| `GET /v2/api/v1/auth/me` | — | `{"data":{"username","via":"fleetd"\|"komari","fleetd_account":bool,"totp_enabled":bool,"komari_fallback":bool}}`；未登录 401 |
| `POST /v2/api/v1/auth/password` | `{"old_password","new_password"}`（≥ 10 字符） | `200`；其他会话全部作废。只允许 fleetd 会话（`via=komari` 时 403） |
| `POST /v2/api/v1/auth/totp/setup` | — | `{"data":{"secret","otpauth_uri"}}`（未启用） |
| `POST /v2/api/v1/auth/totp/enable` | `{"code"}` | `{"data":{"totp_enabled":true}}`；码错 400 |
| `POST /v2/api/v1/auth/totp/disable` | `{"code"}` | `{"data":{"totp_enabled":false}}` |
| `GET /v2/api/v1/auth/sessions` | — | `{"data":{"sessions":[{"id","created_at","last_seen","expires_at","ip","ua","current"}]}}` |
| `DELETE /v2/api/v1/auth/sessions/{id}` | 写 | `{"data":{"revoked":id}}` |

- 会话：Cookie `fleet_session`（随机 32 字节，库里只存 sha256），绝对 7 天、闲置 24 小时失效；服务端校验结果缓存 30 s（退出 / 改密码 / 撤销即时生效）。
- **过渡期**：`/v2/api/*` 同时接受 fleetd 会话与 Komari 的 `session_token`（`auth.komari_fallback = true`）。Komari 下线时关掉它。
- 界面判断显示哪种登录：`GET /v2/api/v1/system` 的 `data.auth`（本机直连免会话；经 Caddy 需要会话，所以登录页直接用 fleetd 登录即可）：`{"mode":"Komari","fleetd_accounts":true,"komari_fallback":true,"login":"/v2/api/v1/auth/login"}`。建议：新界面一律显示 fleetd 登录框；401 时跳登录。
- 账户来源：生产已从 Komari 导入管理员 `admin`（Komari 的 `base64(sha256(密码+固定盐))` 旧哈希，第一次登录成功后自动升级为 PBKDF2-SHA256 31 万轮，密码不变）。命令行（中控）：`/opt/fleetd/current/fleetd user export-komari | runuser -u fleetd -- /opt/fleetd/current/fleetd user import -`、`runuser -u fleetd -- fleetd user set-password <名>`（标准输入读一行）、`user list`、`user disable <名>`。
- 审计：`auth.login`、`auth.login_failed`（带来源 IP）、`auth.logout`、`auth.password_changed`、`auth.totp_enabled/disabled`、`auth.session_revoked`、`auth.user_imported`、`auth.password_set`。

## 9. 原生写操作（v0.5 起，按下面的契约实现；未上线的路径返回 404）

所有写：会话 + `X-Fleet-Request: 1`，写审计（actor = 登录名）。修改带版本的对象用乐观锁：请求带 `expected_revision`，不一致 `409 {"error":"…已变化，请刷新后重试","current_revision":n}`。

### 9.1 触及线路机的操作都是异步的（op）

返回 `202 {"data":{"op_id":"op-…","relay":"线路机3","generation":42,"status":"pending"}}`（纯命令类如 reset_traffic / restart_xray 的 `generation` 为 null）。**只有该线路机的 relayd 在最近 60 s 内来取过期望状态（管理模式在线）才接受**，否则 `409 {"error":"线路机N 的 relayd 还没开启管理模式…"}`；在线的线路机见 `/v2/api/v1/system` 的 `data.edge.managed_relays`，至少一台在线时 `features` 才含 `entries`、`restart_xray`。relayd 180 s 内没回执的 op 记 `expired`，对应那一代期望自动回退到上次已应用内容。管理侧查看：`GET /v2/api/v1/relays/{id}/spec`（`generation`、`managed_mode`、`management_live`、`observed`、`last_applied`；id 可以是 `线路机3` 或 `line3`）。进度：`GET /v2/api/v1/ops/{op_id}` →
`{"data":{"op_id","kind","relay","generation","status":"pending|applied|rejected|partial|expired","created_by","created_at","acked_at","result":{…relayd 回执…},"error"}}`；
SSE 资源 `ops`（`path` = `/v2/api/v1/ops/{op_id}`）在状态变化时推送。`GET /v2/api/v1/ops?relay=&status=&limit=` 列表。relayd 60 s 内没取走 → `expired`（可重试）。

| 操作 | 路径与体 | 说明 |
|---|---|---|
| 入口启用 / 停用 | `POST /v2/api/v1/relays/{relay}/entries/{port}/actions` `{"action":"enable"\|"disable"}` | 线路机名如 `线路机3`，port 为入站端口（`inbound-<port>`） |
| 重置入口流量 | `{"action":"reset_traffic"}` | 旧 x-ui“重置流量”；账本按 relayd 累计值处理，不产生负增量 |
| 改备注 | `{"action":"rename","remark":"…"}` | ≤ 64 字符 |
| **轮换 UUID** | `{"action":"rotate_uuid","confirm":"线路机3:20007"}` | 客户端订阅会变：`confirm` 必须等于 `<线路机名>:<端口>`，否则 400；审计记旧/新 UUID 前 8 位；新分享链接用 `GET /v2/api/v1/relays/{relay}/entries/{port}/share`（审计 `secret.reveal`） |
| 重启 Xray | `POST /v2/api/v1/relays/{relay}/actions` `{"action":"restart_xray","confirm":"线路机3"}` | 唯一会中断连接的操作 |
| 访问规则 | `GET /v2/api/v1/policies`（生效版本 + 草稿 + 历史）；`PUT /v2/api/v1/policies/draft` `{"scope":"team:团队1"\|"node:<uuid>","policy":{"mode","domains"},"base_rev"}`；`POST /v2/api/v1/policies/preview` → 每台线路机的规则差异；`POST /v2/api/v1/policies/apply` `{"draft_hash"}` → 每台线路机一个 op（`{"data":{"version_id","ops":[…]}}`）；`POST /v2/api/v1/policies/discard` `{"draft_hash"}`；`POST /v2/api/v1/policies/rollback` `{"version_id"}` | 语义同旧 `/policies/*`（草稿哈希防并发） |

### 9.2 不触及线路机的写（同步）

| 操作 | 路径与体 |
|---|---|
| 资产编辑 | `PATCH /v2/api/v1/assets/{uuid}` `{"expected_revision", …字段}`（字段同旧 `/assets/edit`） |
| 续费登记 | `POST /v2/api/v1/assets/{uuid}/renewals` `{"new_due":"YYYY-MM-DD","amount_cents","note"}` |
| 节点档案（阈值 / 维护） | `PUT /v2/api/v1/nodes/{uuid}/profile` `{"expected_revision","maintenance_until","thresholds":{…}}`（维护 ≤ 7 天） |
| 故障确认 | `POST /v2/api/v1/incidents/{id}/ack` `{"note"}` |
| 行为确认 | `POST /v2/api/v1/behavior/ack` `{…同旧 /behavior/ack}` |
| API 密钥 | `GET /v2/api/v1/api-keys`；`POST /v2/api/v1/api-keys` `{"name","scope":"read"\|"write"\|"full","expires_days"}` → 明文 `tlm_…` 只返回一次；`POST /v2/api/v1/api-keys/{id}/revoke` |
| 上架 | 见 5d（`/v2/api/v1/onboard/*`）；旧的 `provision/relay|node`、`fleet/add|bind|commit` 一律改走上架流水线 |

## 10. relayd 管理模式协议（`fleet.relayspec.v1`；fleetd 侧 v0.5 实现）

relayd 用自己的 agent 令牌（`Authorization: Bearer …`，host 的 kind 必须是 relay）。所有路径在 `/v2/ingest/relay/`（Caddy 已转发，体 ≤ 256 KB）。

1. **上报观测**（配置文件 sha256 变化时，以及每小时一次）：`POST /v2/ingest/relay/observed`
   `{"schema":"fleet.relayobs.v1","relay":"line3","config_sha256":"…","generation":41,"managed":{"inbounds":[<Xray 入站 JSON>…],"outbounds":[…],"routing_rules":[…]},"observed_at":…}`
   —— `managed` 只含受管对象：入站 tag `inbound-<port>` / `in-*`，出站 tag `ss-*` / `relay-*` / `socks-*`，路由规则 `ruleTag` 以 `fleet-` 或 `policy-` 开头；`api`、`dns`、统计等不受管。含客户端 UUID 与 SS 密码（经 TLS，fleetd 库 0600）。
   fleetd 回 `{"ok":true,"generation":<fleetd 当前 generation>}`。**外部写入者**（fleet-cutover、onboard 在同一把 flock 下改了配置）会让 `config_sha256` 变化而 generation 不变：fleetd 把观测到的受管集合“收编”为新一代（`generation+1`，`reason:"adopted_external_change"`），以后的写操作都基于它。
2. **取期望状态**（长轮询）：`GET /v2/ingest/relay/spec?have=<generation>&wait=25`
   - `have` 等于当前 generation 且没有待执行命令 → 等最多 `wait` 秒，仍无变化回 `304`；
   - 否则 `200 {"schema":"fleet.relayspec.v1","relay","generation","spec_sha256","issued_at","base_config_sha256","managed":{…完整的受管集合…},"commands":[{"id":"cmd-…","kind":"restart_xray"\|"reset_traffic","args":{…},"op_id","expires_at"}]}`。
   尚未接管（generation 0）但有命令时（例如对还没下发过期望的线路机 `restart_xray`），返回 `generation:0`、`managed:null`、`spec_sha256:null`、`base_config_sha256:null` 与 `commands`：relayd 只执行命令，不动配置。
   `managed` 是**完整**的期望集合（不是补丁）：relayd 与当前文件里的受管对象逐 tag 比较，得出增 / 删 / 替换。
3. **应用**（relayd，全程持有 `flock -w 60 /run/lock/fleet-xray-config.lock`）：读 `config.json` → 若其 sha256 ≠ `base_config_sha256` 且差异不只来自本次期望（有人抢先改过），回 `status:"rejected","error":"base_changed"`（fleetd 会先收编再重发）→ 生成候选配置 → `xray run -test` → 经 Xray API（`adi/rmi`、`ado/rmo`、`adrules/rmrules`、客户端增删）逐项生效，**不重启 Xray** → 原子写 `config.json` → 解锁。单项失败时撤销本次已加的对象，整体回 `rejected`。
4. **回执**：`POST /v2/ingest/relay/spec/ack`
   `{"generation","spec_sha256","status":"applied"\|"rejected"\|"partial","config_sha256_before","config_sha256_after","xray_restarted":false,"applied_at","results":[{"object":"inbound"\|"outbound"\|"rule"\|"client","tag","op":"add"\|"remove"\|"replace","ok","error"}],"commands":[{"id","ok","error","finished_at"}],"error"?}`
   同一 generation 重复回执幂等。fleetd 据此把对应 op 置为 applied / rejected，并推 SSE `ops`。
5. 遥测里带 `xray.spec_generation`（relayd 最近成功应用的 generation），fleetd 重启后据此恢复状态；generation 0 表示“未接管，跟随文件”，relayd 什么都不做。

### 10.1 relayd 实现约定（fleet-edge 已按此实现，2026-09-24 定稿）

1. **规则位置**：Xray 不能按下标插入，relayd 用一次不带 `-append` 的 `adrules`（整表替换，原子）并用 `lsrules` 核对。候选顺序：非受管规则保持文件顺序；spec 里的 `policy-*` 规则按 spec 顺序作为一块，插在第一条 inboundTag 含入口 tag（`inbound-<port>` / `in-*`）的非受管规则之前（即旧 compile_routing 的锚点：在 api / geoip:private / bittorrent 防护之后、各入口路由之前）；`fleet-*` 规则按 spec 顺序放在最后。fleetd 的策略编译器按旧编译器的顺序输出 `policy-<节点uuid>-match`、`policy-<节点uuid>-default`。
2. **守卫**（无论 spec 怎么写都拒绝，`error:"guard:<原因>"`）：改动非受管对象（api 入站、`outbounds[0]` 即 R1 默认 blackhole `fleet-default-deny`、direct/blocked 等非 ss/relay/socks 出站、非受管规则或其相对顺序、其他顶层段）；结果里保留的入口入站没有“只按 inboundTag 匹配”的直通路由（否则流量落进 R1 blackhole）；候选 `xray run -test` 不过。回执另带 `diff`（前后 sha、入站/出站 增删改不变计数、非受管与 explicit 规则是否不变、policy 规则增删改）和 `runtime`（前后 lsi/lso/lsrules 视图）；fleetd 随 op 一起保存。
3. **应用顺序**：先加/换出站（ado 或 rmo+ado）→ 入站删/换/加（rmi、rmi+adi、adi；客户端变化 = 入站替换）→ 一次整表 adrules → 最后删出站（rmo）→ 核对运行态 = 候选 → flock 下原子写 config.json。任一步失败逆序撤销（含恢复旧规则表），回 `rejected` 与逐项结果。spec 永不重启 Xray。
4. **base sha**：`base_config_sha256` = config.json 文件字节的 sha256。不一致回 `rejected base_changed`；例外：文件里的受管集合已等于 spec（写完文件、未回执就崩溃）时回 `applied`。relayd 以格式化 JSON 重写文件，fleetd 只拿 relayd 报的 sha（`config_sha256_after`、observed）比较，不用自己渲染的字节。
5. **轮询**：relayd 每 10 s `GET /spec?have=<gen>&wait=0`（单线程，不做长轮询；最多多 10 s 延迟）。`observed`：每 60 s 检查一次 sha，变了就报，另每小时一次。遥测带 `xray.spec_generation`。
6. **命令**：`restart_xray {}`——先对磁盘上的文件 `xray run -test`，不过就拒绝；然后 systemctl restart，最多等 30 s 让所有入站端口监听、API 可答、规则一致。`reset_traffic`——args `{"tag":"inbound-20001","users":true}` 或 `{"names":[完整计数器名…]}`，对这些计数器 `GetStats(reset=true)`；relayd 的累计账（`/relays/{id}/xray` 的真实来源）**不清零**，重置前的值先并入；命令结果带 `data:{"counters":[{"name","value_at_reset","delta_folded","cum"}]}`，fleetd 记为账期说明（本期用量 = cum − cum_at_reset）。
7. 入口改备注只是 fleetd 的元数据（Xray 入站没有 remark），relayd 什么都不做；入口停用 / 启用 = 把该入站移出 / 放回 `managed.inbounds`（fleetd 保存原 JSON），relayd 在 rmi 前先并入计数器，停用不丢流量。

## 11. 访问日志上报（`fleet.access.v1`，relayd → fleetd，替代旧 access log 采集）

relay 批次顶层字段 `access`：
`{"schema":"fleet.access.v1","source":{"file":"/var/log/xray/access.log","inode":123,"offset_from":0,"offset_to":4096},"dropped":0,"events":[{"id":"<inode>:<offset>","ts":1790190000.123,"inbound":"inbound-20007","client":"node-07","source_ip":"1.2.3.4","source_port":51234,"network":"tcp"\|"udp","host":"example.com","port":443,"action":"accepted"\|"blocked","rule":"policy-…"\|null,"outbound":"ss-…"}]}`
relayd 实现备注：`rule` 恒为 null（Xray 访问日志没有命中的规则）；`action` 在出站为 `blocked`、日志为 rejected 或出站是配置里任何 blackhole（含 `fleet-default-deny`）时为 `blocked`，否则 `accepted`；回环来源（relayd / 探测自身）跳过；入站 tag 接受 `inbound-\d{2,5}` 与 `in-*`；每批最多 500 条（积压时每 30 s 追 500 条，日志文件就是缓冲，outbox 过半时暂停读取）；`dropped` 只计 relayd 丢弃的行（超过 32 KiB 的畸形行）；另有累计 `counters{lines,events,loopback,unparsed,dropped_lines,rotation_gaps,truncations}`（fleetd 对 rotation_gaps、truncations 告警）；`source.offset_to` = 下一批的 `offset_from`，轮转时一批可跨两个 inode，事件 id 带精确的 `inode:offset`，relayd 重启后重放的 id 不变。

fleetd 按 `id` 去重，写入与旧 `activity` 表同形的行（`asset` 由 `inbound` → 节点映射得出，`kind`/`protocol` 规则同旧 bridge），行为统计（`behavior`、`destinations`、`sources_seen`）在入库时增量维护；IP 情报（ipintel）直接吃这里的事件（全量，不再是每分钟 500 条抽样）。

## 12. 下线旧系统后

- `/v2/api/legacy/*` **继续提供同样的形状**，但数据来自 fleetd 自己的库（迁移导入 + relayd / nodeagent / 探测 / 告警引擎），包括 `komari/*`（主机 = nodeagent / relayd，延迟 = 探测历史）。切换前有影子比对：`GET /v2/api/v1/shadow/legacy`（逐路径字段级差异）；0 个未解释差异持续 30 分钟才下线。
- 旧 `/api/team-ops/*` 与 `/ops/api/*`（`X-Api-Key: tlm_…`，旧密钥继续有效）由 Caddy 转到 fleetd 的兼容层。
- 判断是否已下线：`GET /v2/api/v1/system` 的 `data.legacy_source`：`"team-ops"`（现在）→ `"native"`（下线后）。`native` 时界面不必再请求 `komari/*` 以外的差异路径，也不要再引导去原版后台（Komari）。

### 12.1 原生旧库（fleetd ≥ 0.6，`/var/lib/fleetd/legacy.db`）

- **导入**：`fleetd legacy snapshot`（root，只读旧库：`VACUUM INTO` 一致性拷贝，约 1 分钟）；旧进程配置里不在库中的三项（backend.json 线路机名单、provision 公钥、软件包清单）写入 meta `fleetd:compat`。
- **读**：compat 模块逐行移植旧读路径（overview/health/history/activity/behavior/incidents/node/renewals/audit/jobs/api-keys/schema/panel/fleet/provision/defaults/public-status/policies/export/komari/hosts），
  输出按 Python `json.dumps(ensure_ascii=False, separators=(",",":"))` 序列化（float repr、int/float 类型、键序）。核对：`fleetd legacy verify --golden DIR`（staging/golden.py 生成；同库同时刻 39/39 逐字节一致）。
- **原生喂数**（10 s 一拍，从导入时刻接着写）：

| 旧表 / meta | 来源 | 说明 |
|---|---|---|
| minutes / hours | relayd `xray_1m` | 入口 tag → 资产（节点按 relay_port；主入口归线路机；其余 `direct:<线路机>:<端口>`）；同一分钟整分钟覆盖（幂等，无缺口不重复），有样本的分钟对所有入口写行（零也写，与旧采样一致）；hours = 该小时 minutes 之和 |
| relays（线路机快照） | relayd 计数器 / conns / srcs / Xray 状态 + fleetd 观测到的受管配置 + `legacy_probe` | 旧 `store.ingest` 的归属、速率、重置、基线逐行移植；x-ui 口径 up/down = 接管时的值 + relayd 累计增量；`panel` 改为 relayd 的 Xray 状态（x-ui 已停用） |
| activity | relayd `fleet.access.v1`（带原始 `status`） | id = `<线路机>:<inode>:<offset>`，与旧 bridge 同号，天然去重；新插入时增量维护行为统计 |
| destinations / sources_seen / behavior_* | 同上 | 移植 `behavior.ingest/detect/_refresh/maintenance`（与 Python 差分 0 行差异） |
| hosts / hosts_at | nodeagent / relayd 样本；只有连通性探针的主机按 relayd TCP+出口验证判在线；ping 汇总来自 relayd 探测 | 旧 Komari 插件同步的形状 |
| monitor / health_* / incidents / 系统审计 | 移植 `monitor.evaluate/transition/diagnose/maintenance`（与 Python 差分 0 行差异） | 在原生旧库上每 10 s 评估 |
| collector | 喂数心跳 | |

- **闸门**：`GET /v2/api/v1/shadow/legacy` → `data.paths.gate`（`passing`、`clean_for_s`、窗口内未解释差异数）；每 60 s 一轮、21 个路径（含轮换的 /node），原生渲染时刻取旧响应里的 `now`。差异分类：
  `sample_time`（采样时刻；`now/checked_at/received_at/hosts_at…` 两边须在 300 s 内，否则算未解释——喂数停了会暴露）、
  `sampled_volume`（计数/速率/资源占用）、`source_changed`（x-ui 停用后的旧 panel、bridge 游标、core_id 等）、
  `window_edge`（列表在时间窗或条数截断边界的成员差异）、`unexplained`（其余一切，必须为 0）；
  同一条记录（带 uuid/key 的对象）两边的测量输入时刻不同时，结论字段（status/reason/since/evidence…）的差异归 `sample_time`，名称/归属/配置等仍严格比较。
  通过条件：连续 30 分钟每轮 0 个未解释差异且窗口内至少 25 轮（fleetd 重启会清零重新计时）。

### 12.2 下线后的写接口

- `POST /v2/api/v1/legacy-write/<旧 POST 路径>`（会话 + `X-Fleet-Request: 1`；只在 `legacy_source = "native"` 时可用，否则 409）：请求体与返回同旧接口。
  已原生：`assets/edit`（含续费登记）、`node/profile`、`incidents/ack`、`behavior/ack`、`api-keys/create|revoke`、`policies/draft|discard|apply`。
  错误文案、审计记录、返回值与旧系统相同（与 Python 差分核对）。`features` 在 native 时增加 `legacy.native`、`legacy.write`、`assets.edit`、`renewals`、`node.profile`、`incidents.ack`、`behavior.ack`、`api_keys`、`policies`、`policies.apply`。
- **策略发布**：草稿 + 已生效 → 每台线路机的 `policy-<uuid>-match` / `policy-<uuid>-default` 规则（编译与旧 compile_routing 相同），作为 relayspec 新一代下发；
  版本记在旧库 versions 表（applying → applied；任一线路机被拒/过期 → 已应用的线路机回到发布前 = `failed_rolled_back`）。不再需要 `routing_hashes`（relayd 用 base sha 做乐观并发）。
- `panel/action`、`fleet/*`、`provision/*` 返回 400 并指向新流程（入口操作 / 上架）。
- Komari 客户端资料同步（旧插件的 `admin:editClient`）需要 Komari 管理员凭据，fleetd 没有：资产改动后 `sync_pending` 保持 true，由界面以管理员会话补做或以后接入 Komari API 密钥。

### 12.3 下线步骤（`deploy/decommission.sh`，root）

1. 闸门 `passing = true`；2. `systemctl stop team-ops`；3. 停 fleetd → `fleetd legacy catchup`（快照之后人在旧系统里的写入：资产/续费/密钥与调用记录/草稿/版本/任务/fleet 表/非 system 审计/故障与行为确认，并入原生旧库）；
4. `/etc/fleetd/fleetd.toml` 设 `legacy_source = "native"`、启动 fleetd 并核对；5. `caddy-decommission.sh`（`/api/team-ops/*`、`/ops/api/*` → fleetd；`/` 与 `/admin*` → `/admin/v2/app/`）；6. disable team-ops。
不动：team-ops-logrotate（给 Xray 访问日志限长）、team-ops-path-probe 与 komari-relay-probe（过渡期探测文件由 relayd 转发）、Komari。
回退：`decommission.sh --rollback`（原生期间的写入不回写旧库）。

### 12.4 现状（fleetd 0.11，2026-09-24）：原生模式下的数据来源、缓存与增量

- **运行形态**：`legacy_source = "native"`，`[komari] enabled = false`（`komari/*` 由 fleetd 原生渲染：主机 = nodeagent / relayd 样本，ping = 探测历史；
  客户端资料里 `cpu_cores` / os / kernel 来自 nodeagent meta）。旧后端与 Komari 均不再是数据源。
- **请求路径不做全量计算**：
  - 旧形状 GET（`/api/team-ops/*`、`/ops/api/*`）按 **(路径+查询, 数据代)** 缓存渲染结果：数据代在每拍喂数结束（≤10 s）与每次写入后 +1，同一代内任意多个查看者共用一次渲染（命中 1–3 ms）。
    7 天 / 14 天等长窗口（`seconds > 86400`）允许 60 s 内的结果跨代复用。`/v2/api/v1/system` 的 `render_cache` = `{hits, renders, gen}`。
  - 默认 24 h 的 `/behavior`、`/activity` 与其等值过滤（asset/team/action/kind/source_ip/host）：内存增量窗口（compat::window）。
  - 7 / 14 天（`seconds > 86400`，等值过滤 asset/team/kind/source_ip/host，不含 action/protocol/search）：小时汇总 `beh_pair_h`（compat::rollup；写入时维护，
    起点所在的不完整小时读原始记录，结果与旧 SQL 同值；首次上线按块回填 15 天）。action/protocol/search 仍走带索引的 SQL（`activity(action|kind|source_ip, ts)` 索引）。
  - `/v2/api/v1/probes/latest`（不带 since）按探测代缓存。
  - `/system.requests[]`：每个路径（数字 / uuid 归一）的 `count`、`avg_ms`、最近 200 次的 `p50_ms` / `p95_ms`。
- **SSE 与增量**（`/v2/api/stream`）：
  - 资源 `activity`：每拍有新访问记录时发一次（≤1 次 / 10 s），`path = /v2/api/v1/activity/delta`。界面记住游标，按游标取增量追加，不必重取整份 `/activity`：
    `GET /v2/api/v1/activity/delta?after=<cursor>&limit=500` → `{"data":{"cursor":<int>,"more":<bool>,"rows":[…]}}`。行 = 旧 `/activity` rows 的字段 + `rid`（游标，递增）+ `source_intel`（isp/asn/country/region/city/place/type/type_word/internal），按 rid 升序；
    不带 `after`：最近 `limit` 条 + 当前游标（首屏用）；`more = true` 时用返回的 cursor 继续取。`features` 含 `activity.delta`。
  - 其余资源（overview / health / alerts / probes …）的 `change` 仍按 path 重取；由于渲染缓存，重取在同一代内是缓存命中，不再触发重算。
- **行为事件**：
  - `/behavior` 的 `events` 在生产里 **新域名与其他事件各取最近 200 条**（旧系统合在一起取 200 条，新域名多时连接突增 / 新来源被挤出列表）。
  - 随机命名的子域名（含 UUID、≥8 位含数字的十六进制串、≥20 位含 ≥4 个数字的字母数字串）按 **主域名（公共后缀表）合并成一条新域名事件**：
    `subject = "*.<主域名>"`，每资产每天一条；事件行多一个字段 `evidence`（JSON 文本）：`{"collapsed_by":"registrable_domain","domain":"fbcdn.net","count":117,"examples":[前 5 个主机],"last_ts":…}`，
    `message` 随数量更新。普通子域名不合并。回填：`fleetd legacy collapse-domains --since <unix> [--dry-run]`（fleetd 用户）。
- **探测与告警**：
  - 线路机自身探测（relayd → `probe_land`）：对端 TCP 心跳（`check=tcp`，node = 被测线路机 uuid，另外两台线路机测）与经主节点的出口验证（`check=egress`，自己测）。
    在 `/v2/api/v1/probes/latest` 里是节点 `r1` / `r2` / `r3`（`detail.from` = 测量方、`target`、`source`；出口非最终尝试记 `retry`）；overview 的线路机 `host.ping.{control,peer_tcp,self_egress}` 为 20 格汇总。
  - 规则：`relay_egress_mismatch`（最近两个周期出口 ≠ 线路机 IP，严重）、`relay_tcp_down`（每个对端最近两次都连不上，严重）、
    `relay_probe_stalled`（线路机批次照常到，但它自己的 tcp/egress 结果超过 5 分钟没有新行，警告）。
  - **出口只看连通性（0.13 起）**：出口验证 / 实际入口到出口取到 IP 就算通过；实际出口 IP 只记录展示（`actual_ip`），不与资产 IP 比对，
    不再有 egress_mismatch / relay_egress_mismatch / ipintel_egress_ip_changed 告警。节点最近两次出口验证都取到同一个新 IP → 自动改资产 ip / egress_ip / 名称里的 IP
    （审计 `asset_ip`，reason「出口 IP 自动更新」）；线路机上入口实际连的落地地址变了 → 自动同步资产 target / target_port（审计 `asset_target`）。
    唯一的出口安全规则 `egress_route_leak`（严重）：节点的实际出口 IP 等于某台线路机的公网 IP。手动改 IP：`fleetd legacy set-ip --uuid U --ip IP [--with-target]`。
  - 「实际入口到出口」新鲜期 240 s（relayd 自适应巡检：主入口与异常入口 60 s、健康入口 120 s 一轮）。
  - 有 SSE 连接时，`POST /v2/ingest/{node,relay}` 的 200 响应带 `"live": true`，relayd 随后 90 s 内每 60 s 全量巡检。
- **流量配额**：`net_total_up/down` = fleetd 流量计（`fleetd_meter`：同一次开机按网卡计数差值累加，重启 / 清零按新计数；线路机按供应商账单日清零）；
  基线用 `fleetd vendor import <json>`（供应商面板读数 + 读数后网卡积分），`fleetd vendor meters` 查看。
- **上架 P11**：`/etc/fleet/onboard.toml` 有 `[agent]`（`dist = /var/lib/fleet/dist/fleet-edge`，含 nodeagent + VERSION + SHA256SUMS）时，上架第 11 步安装 nodeagent，
  令牌由 fleetd 进程内的 fleetd-ingest 签发 / 撤销，等到第一批样本（boot_id 对上）才算完成；取消会删掉节点上的 nodeagent 并撤销令牌。没有 `[agent]` 时跳过。

### 12.5 overview / health 增量（fleetd ≥ 0.12，`features`：`delta.overview`、`delta.health`）

- SSE `change` 帧的 `version` 就是增量的版本号（`resource` = `overview` / `health`）。
- `GET /v2/api/v1/delta/{overview|health}?since=<你手上的版本>` → `{"data":{"version":v,"full":false,"patch":{…}}}`；
  `since` 等于当前版本 → `patch = {}`；`since` 太旧（服务端保留最近 8 个版本）/ 服务重启过 / 不带 since → `{"version":v,"full":true,"data":<整份，同 /overview 的 data>}`。
  还没有快照（刚启动）→ 503，读整份即可。
- 补丁规则（JSON Merge Patch 的变体；`null` 是普通值，不表示删除）：
  - 对象：只列出变化的键。值是对象 → 递归应用到原值（原值不是对象就当 `{}`）；其他值（数字、字符串、null、数组）直接替换。被删除的键在 `"$del": ["k", …]`。
  - 元素全是带 `uuid` 的对象的数组（`overview.assets`）→ `{"$uuid": {"<uuid>": <子补丁或新元素>}, "$removed": ["<uuid>", …], "$order": ["<uuid>", …]}`；
    先删 `$removed`，再逐个应用 `$uuid`（不存在的按新元素追加），有 `$order` 时按它重排。
  - 参考实现：`fleet/fleetd/src/delta.rs` 的 `apply()`（单元测试里 diff → apply 往返一致）。
- 写入成功后服务端会立刻重取 overview / health / panel 并推送新版本；界面照常跟 SSE 走即可。

---

## 6. `GET /v2/api/v1/system`

fleetd 自身状态（本机 `curl http://127.0.0.1:25900/v2/api/v1/system` 不需要会话；经 Caddy 需要）：

```json
{"data":{"service":"fleetd","version":"0.1.0","pid":1234,"started_at":…,"uptime_s":3600,"now":…,
  "cpu_pct":0.1,"cpu_avg_5m_pct":0.08,"cpu_total_s":2.9,"rss_mb":96.0,"rss_anon_mb":16.7,"rss_file_mb":79.3,"threads":4,
  "cadence":"idle","sse_clients":0,"sse_versions":{"overview":60,"health":60,"alerts":2},
  "legacy":{"base_url":"http://127.0.0.1:25800","key":true,"concurrency":2,
            "stats":{"requests":121,"errors":0,"consecutive_errors":0,"last_ok_at":…,"last_error":null,"bytes":28000000},
            "latency":{"n":121,"p50_ms":180.2,"p95_ms":420.0,"max_ms":610.3}},
  "komari":{"base_url":"http://127.0.0.1:25774","api_key":false,"reachable":true,"stats":{…},"latency":{…},"health":{…}},
  "polls":[{"resource":"overview","path":"/overview","source":"legacy","interval_s":60,"active_s":10,"idle_s":60,
            "version":60,"polls":60,"age_s":12.3,"last_latency_ms":190.1,"bytes":455000,"errors":0,
            "consecutive_errors":0,"last_error":null}, …],
  "proxy":{"requests":40,"cache_hits":31,"legacy_cache_entries":14,"legacy_cache_bytes":1900000,"komari_cache_entries":2},
  "auth":{"mode":"Komari","cached_sessions":1,"upstream_checks":3},
  "alerts":{"in_memory":2,"pending":0,"firing":2,"suppressed":0,"last_digest_at":…,"next_digest_at":…,
            "evaluation":{"last_at":…,"observations":2,"duration_ms":4.1,"skipped":null,"evaluations":60}},
  "notify":{"shadow":true,"channels":0},
  "db":{"path":"/var/lib/fleetd/fleetd.db","size_bytes":180224},
  "ipintel":{"enabled":true,"loaded":5,"state":{"backfill":"done","backfill_requests":320,"backfill_events":…,"loops":…}}}}
```

`rss_file_mb` 是 mmap 的文件页（原生旧库 legacy.db / ingest.db 的 SQLite mmap、ipintel 离线库；干净页，内存紧张时内核直接回收）；堆看 `rss_anon_mb`。
内存构成（本机直连免鉴权）：`GET /v2/api/v1/debug/mem[?collect=1]` → 各缓存的条目 / 字节（渲染缓存、增量快照、24 h 窗口、7/14 天汇总、history 缓存）、SQLite 堆、分配器提交量；`collect=1` 先让分配器归还空闲页。`alerts.evaluation.startup_hold` = 上一次评估时启动宽限是否仍在。

---

## 7. 接入要点（给界面）

1. `createLiveFetch({ apiBase: '/v2/api/legacy/' })`，其余照旧。
2. 用 `/v2/api/stream` 替代定时轮询：`hello` 后按需取一次首屏；之后每个 `change` 只重取 `path`；`reset` 全量重取；`auth` 显示登录提示（文案同 401）。
3. 事项队列页：`GET /v2/api/v1/alerts`（SSE `alerts` 变化时重取）+ `notify-log`；确认、静默、维护窗口用上面的写接口，记得带 `X-Fleet-Request: 1`。
4. 通知设置页：`GET /v2/api/v1/channels`（目前影子模式）；套餐与配额页：`plans` / `assets` / `quota`。
5. 延迟条：首屏取一次 `/v2/api/v1/probes/latest`（每节点每类 20 格，线路机为 r1/r2/r3），之后跟 SSE `probes` 重取（或 `?since=<上次 until>` 只取新样本）；节点图表用 `/v2/api/v1/probes?node=…&range=24h`。
6. 访问记录：首屏 `GET /v2/api/v1/activity/delta?limit=500`，之后 SSE `activity` 变化时 `?after=<cursor>` 追加（§12.4）。
