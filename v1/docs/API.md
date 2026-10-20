# fleet-server API v1

- 前缀 `/api/v1`。成功 `{"data": …}`；失败 `{"error":{"code","message"}}`（message 是给人看的中文）。
- 鉴权：会话 Cookie `fleet_session`（`POST /auth/login` 设置）；或 `Authorization: Bearer <api key>`（见下「API 密钥与 scope」）。
- 会话的写请求（POST/PUT/PATCH/DELETE）必须带头 `X-Fleet-Request: 1`（防 CSRF）；API 密钥不是浏览器，不要求这个头。
- 时间：RFC 3339 字符串；日期 `YYYY-MM-DD`。金额：整数分（`price_cents`）。流量：字节。速率：bit/s。
- 乐观锁：改机器要带 `revision`，不一致 → 409。
- 分页列表（`/jobs`、`/incidents`）：`limit`（1–500）+ `offset`（从 0 起），返回 `{data:{items, total, next_offset}}`；`total` 是同一筛选下的总条数，`next_offset` 只在后面还有数据时给出（否则 null）；`offset` 越过末尾得到空的 `items`，`total` 照给。

## 认证
| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/auth/login` | `{username,password}` → 设 Cookie，`{data:{username}}`。限速：同一用户名 15 分钟内失败 5 次、同一来源 IP 失败 20 次 → 429（`error.code="rate_limited"`，`error.detail.retry_after_s`，message 含剩余等待时间）。成功、失败、登出都写审计 |
| POST | `/auth/logout` | |
| GET | `/auth/me` | `{data:{username, kind:"session"\|"api_key", scopes:[...]}}`；未登录 401 |

## API 密钥与 scope
密钥是 `flk_<8位十六进制前缀>_<随机串>`，库里只存 sha256 与前缀。每个请求按「方法 + 路径」需要一个 scope，密钥没有就 403（会话用户拥有全部）：

| scope | 允许 |
|---|---|
| `read` | 所有 GET，**不含**下面三类 |
| `write` | 所有非 GET 的业务写操作（不含 `/machines/{id}/exec`） |
| `secrets` | `GET /entries/{id}/share` 这类凭据 |
| `exec` | `POST /machines/{id}/exec`、`GET /machines/{id}/logs`、`GET /server/logs`（都是通过 SSH 或本机读取） |

scope 之间不互相包含：只有 `write` 的密钥不能 GET。`/api-keys` 的管理只允许会话用户，用密钥调用一律 403。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api-keys` | `[{id,name,prefix,scopes,created_at,expires_at,last_used_at,revoked_at}]`（永不返回 token） |
| POST | `/api-keys` | `{name, scopes:["read",…], expires_days?: 1-3650}` → `{data:{id, token}}`，**token 只返回这一次**；写审计，实时流发 `api_keys` |
| DELETE | `/api-keys/{id}` | 撤销（幂等）；之后用它的请求 401 |

## 目录
| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/teams` | `[{id,name,sort_order,node_count}]` |
| POST | `/teams` | `{name,sort_order?}` |
| PATCH | `/teams/{id}` | `{name?,sort_order?}`（改团队名 = 该团队所有节点显示名一起变） |
| GET | `/providers` | `[{id,name,code,panel_url,timezone}]` |
| POST | `/providers` | `{name,code,panel_url?,timezone?}`；`timezone` 不传 / null / 空串 = 列默认值 UTC，否则须是 `at time zone` 认的时区（400）；名称或缩写重复 409 |
| PATCH | `/providers/{id}` | `{name?,code?,panel_url?,timezone?}`：没传的不改；`panel_url` null / 空串 = 清空；`timezone` null / 空串 = 回到 UTC。校验同 POST；只影响之后上架的机器（已上架机器的账单时区是当时抄过去的）。写审计 `provider.update` |
| GET | `/plans` | `[{id,provider_id,name,cpu_cores,mem_mb,disk_gb,bandwidth_mbps,traffic_gb,price_cents,currency,cycle,notes,archived_at,machine_count}]` |
| POST | `/plans` | 同上字段（不含 id/统计） |
| PATCH | `/plans/{id}` | 任意字段（没传的不改；规格类字段传 null = 清空）；`archived:true` 归档 |

## 机器
`MachineView`（列表与详情同形）：
```
{ id, kind: "relay"|"vps"|"proxy", name, team:{id,name}|null, number|null, label|null, ip, ssh_port,
  provider:{id,name}|null, provider_code|null, plan:{id,name,bandwidth_mbps,traffic_gb}|null, service_ref|null,
  lifecycle: "draft"|"provisioning"|"active"|"retiring"|"retired"|"failed", lifecycle_reason|null, note, revision,
  created_at, activated_at|null, retired_at|null,
  subscription: {price_cents,currency,cycle,due_date,due_days,billing_tz,auto_renew,remind_days,traffic_gb,traffic_reset_day}|null,
  agent: {version,last_report_at,online,xray_running,cores}|null,  // proxy 没有；cores 为 agent 报的可用核数（容器里是限额），没报过为 null
  host: {ts,cpu_pct,load1,mem_used,mem_total,disk_used,disk_total,net_rx_bps,net_tx_bps,tcp_conns}|null,  // load1 在容器里为 null（/proc/loadavg 是宿主机的）
  entries: [EntryView],                                // 节点：自己的入口；线路机：挂在上面的入口
  removed_traffic: {today_up,today_down,month_up,month_down},  // 已移除入口（如换线路机后的旧入口）本月/今天的流量，不在 entries 里
  health: {status: "ok"|"degraded"|"down"|"unknown"|"maintenance"|"inactive", reasons:[{key,severity,text}]},
  active_job: {id,kind,state}|null, open_incidents, maintenance,
  relay: {public_host,generation,applied_generation,applied_at,apply_error,port_min,port_max}|null }
EntryView = { id, relay:{id,name}, node:{id,name}, port, lifecycle, applied, apply_error,
  tcp:{ok,latency_ms,detail,ts}|null, egress:{…}|null, traffic:{today_up,today_down,month_up,month_down} }
```
| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/machines?scope=live\|retired\|all` | 默认 live（未退役）。排序：线路机在前，然后按团队、编号 |
| GET | `/machines/{id}` | 单台 |
| PATCH | `/machines/{id}` | `{revision, team_id?, number?, label?, note?, provider_id?, plan_id?, service_ref?}`。改团队/编号 = 改名：线路机备注随后自动同步，不换令牌、不断连接 |
| GET | `/machines/{id}/series?metric=host\|traffic\|probe&range=1h\|6h\|24h\|7d\|30d` | 时序。≤24h 用原始点（1 分钟聚合），更长用小时汇总 |
| GET | `/machines/{id}/retire` | 退役预览：`{data:{name, renames:[{machine_id,old_name,new_name}]}}`——编号保持连续，退役后同团队后面的节点依次前移 |
| POST | `/machines/{id}/retire` | `{confirm: <显示名>, reason?, cleanup?: true}` → `{data:{job_id}}`。退役完成时同团队编号自动压缩成 01..N（写审计） |
| POST | `/machines/{id}/purge` | 仅已退役。`{confirm: <显示名>}` → 作业；彻底删除机器与遥测，保留审计 |
| POST | `/machines/{id}/upgrade-agent` | 推送中控当前版本的 agent 并重启（作业 `upgrade_agent`；令牌顺带轮换） |
| POST | `/agents/upgrade` | 所有在用线路机与 VPS 节点各建一个 `upgrade_agent` 作业（有进行中作业的跳过） → `{data:{jobs:[{machine_id,job_id}]}}` |
| PUT | `/machines/{id}/subscription` | `{price_cents,currency,cycle,due_date,billing_tz,auto_renew,remind_days,traffic_gb?,traffic_reset_day?}` |
| POST | `/machines/{id}/renewals` | `{new_due,amount_cents?,note?}` |
| GET | `/machines/{id}/renewals` | |
| GET | `/teams/{id}/renumber` | 预览：`{data:{changes:[{machine_id,old_name,new_name}],token}}` |
| POST | `/teams/{id}/renumber` | `{token}`（预览返回的，防止预览后数据变了）→ 一次生效 |

## 上架
| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/provision/check` | 校验（不落库）：`{items:[ProvisionItem]}` → `{data:{items:[{ok,errors:[...],name,number,relay_port?}]}}` |
| POST | `/provision` | 同样的 body，全部通过才创建：每台建机器（provisioning）+ 一个作业 → `{data:{jobs:[{machine_id,job_id,name}]}}` |

```
ProvisionItem = {
  kind: "vps"|"proxy"|"relay",
  ip, ssh_port?(22), team_id (节点), number?(空=自动取最小空号), label (线路机),
  provider_id?, plan_id?, service_ref?, note?,
  relay_id (节点要挂的线路机),
  // vps / relay：首次登录方式（只在作业里用一次，成功后丢弃）
  login?: {user?:"root", password?: "...", }       // 不填则用中控的密钥
  // proxy：上游
  upstream?: {protocol:"socks5"|"http", host, port, username?, password?},
  // vps：节点 SS 端口（NAT 机填供应商映射后的公网端口）
  ss_port?: 8388,
  subscription?: {price_cents?, due_date, cycle?, billing_tz?}
}
```

## 线路机与入口
| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/relays` | `MachineView[]`（kind=relay） |
| POST | `/entries/{id}/actions` | `{action:"enable"\|"disable"\|"rotate", confirm?}`；rotate 需 `confirm="<线路机名>:<端口>"` |
| GET | `/entries/{id}/share` | `{data:{url}}`（vless:// 分享链接；记审计） |
| POST | `/machines/{node_id}/entries` | 给节点再挂一条入口：`{relay_id}` → 作业 |

## 作业
`Job = {id,kind,target:{id,name}|null,state,step,error,created_by,created_at,started_at,finished_at,steps:[{idx,key,title,state,started_at,finished_at,output,error}]}`
| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/jobs?state=active\|done\|all\|failed&target_id=&limit=100&offset=0` | 分页：`{items:[Job（不含 steps）], total, next_offset}`，按创建先后倒序（`id desc`） |
| GET | `/jobs/{id}` | 含 steps |
| GET | `/jobs/{id}/logs?after=<log id>` | `[{id,ts,level,msg}]` |
| POST | `/jobs/{id}/retry` | 失败的作业从失败那一步重来 |
| POST | `/jobs/{id}/cancel` | 进行中的作业：补偿已完成的步骤后停下 |

## 运维：远程执行与日志
需要 `exec` scope（会话用户直接可用）。中控用自己的 SSH 密钥和 known_hosts 登录；机器必须不是已退役、不是上游代理（proxy 没有 SSH）。命令原样当 bash 脚本执行（stdin 接 /dev/null），超时由远端 `timeout` 杀掉。每次执行写审计（actor、机器、命令全文、退出码、耗时），**不要在命令里写密码**。
| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/machines/{id}/exec` | `{command, timeout_s?: 1-600 (默认 60)}` → `{data:{exit_code, stdout, stderr, duration_ms, truncated, timed_out}}`。stdout/stderr 各截断到 256 KiB（`truncated=true`）；超时 `timed_out=true`、`exit_code=124`；SSH 连不上时 `exit_code=255` 且 stderr 有原因。机器不存在 404、已退役或 proxy 409、命令空/超时越界 400、SSH 无法启动 502 |
| GET | `/machines/{id}/logs?unit=fleet-agent\|fleet-xray\|ssserver&lines=200` | 白名单 unit（`ssserver` 对应 `fleet-ssserver`），lines 1-2000；内部走同一 exec 通道跑 `journalctl`，返回形状同 exec，另有 `unit`、`lines`。审计动作 `machine.logs` |
| GET | `/server/logs?lines=200` | 中控自己的 `journalctl -u fleet-server`。fleet 用户通常没权限读 journal：失败时 500 并说明如何授权（`usermod -aG systemd-journal fleet`） |

## 总览与告警
| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/overview` | `{nodes:{total,ok,degraded,down,unknown,maintenance}, relays:{…}, provisioning, retiring, due_7d, monthly_cost:[{currency,cents}]（按币种分别汇总，不折算）, traffic_month:{up,down}, net_rx_bps, net_tx_bps}`（只统计 active；速率不含故障或 agent 离线的机器） |
| GET | `/incidents?state=firing\|resolved\|all&limit=100&offset=0` | 分页：`{items:[{id,rule,subject:{id,name},subject_id,subject_name,severity,state,summary,opened_at,last_seen_at,resolved_at,ack_at,ack_by,reopen_count}], total, next_offset}`；`state` 默认 firing；`reopen_count`：恢复后 30 分钟内同一（规则, 对象）复发会重开原告警而不是新建，每次 +1；顺序：进行中在前 → 严重度高在前 → 开始时间新在前（同时刻按 id，翻页不重不漏） |
| POST | `/incidents/{id}/ack` | |
| GET/POST | `/silences`、DELETE `/silences/{id}` | `{subject_id?,rule?,starts_at,ends_at,is_maintenance,comment}` |
| GET | `/audit?target_id=&limit=` | |

## 返回形状补充（前端以此为准）

- `health.reasons[].severity`：`"warning"` | `"critical"`（告警的 `severity` 另有 `"info"`）。
- `GET /machines/{id}/series` → `{metric, range, resolution: "1m"|"1h", points: [...]}`，点里时间字段统一叫 `ts`：
  - host：`{ts, cpu_pct, mem_pct, disk_pct, load1, net_rx_bps, net_tx_bps}`（1h 分辨率另有 `cpu_max`，无 `load1`）
  - traffic：`{ts, up, down, conns}`（字节，该时间桶内的增量）
  - probe：`{ts, tcp_ok, tcp_ms, egress_ok, egress_ms}`（失败时 `*_ms` 为 null；1m 分辨率另有 `tcp_n`、`tcp_ok_n`（该分钟 TCP 探测次数 / 成功次数，线路机为其所有入口合计）；1h 分辨率另有 `tcp_success_pct`、`egress_success_pct`）
- 作业 `Job`：`{id, kind, target: {id,name}|null, state, step, step_count, error, created_by, created_at, started_at, finished_at}`；
  `state` ∈ `queued | running | waiting | cancelling | succeeded | failed | cancelled`；详情另有 `steps[]`（`output` 是 JSON 对象或 null）与 `cancellable`。
  作业 `kind` ∈ `provision_vps | provision_proxy | provision_relay | attach_entry | retire | purge | upgrade_agent`。
- 告警 `Incident`：`{id, rule, subject: {id,name}, subject_id, subject_name, severity, state: "firing"|"resolved", summary, opened_at, last_seen_at, resolved_at, ack_at, ack_by}`。
- 静默 `Silence`：`{id, subject_id, subject_name, rule, starts_at, ends_at, is_maintenance, comment}`。
- 续费 `Renewal`：`{id, old_due, new_due, amount_cents, note, actor, created_at}`。
- 审计：`{id, ts, actor, action, title, summary, target_kind, target_id, detail}`；界面显示 `title` 与 `summary`（中文，服务端生成），`detail` 是原始 JSON。

## 实时
`GET /api/v1/stream`：WebSocket（`ws://` / `wss://`），鉴权同其它 GET（会话 Cookie 或 `Authorization: Bearer`，需要 `read`），握手失败返回普通 HTTP 错误（401/403）。服务端只发 JSON 文本帧，不读客户端消息。
- **保活**：服务端每 15 s 发 ping，到下一次 ping 还没收到 pong 就断开（浏览器自动回 pong）。
- **会话复核**：每 60 s 查一次会话/密钥是否仍有效，失效则以关闭码 `4401` 关闭；客户端照常退避重连，握手会 401。
- **滞后**：服务端广播缓冲 256 条，某个连接读得太慢丢了消息时补发 `{type:"resync"}`，客户端应让全部缓存失效重取。
- **重连**：客户端断线后自己退避重连；连上后应重取一次机器类数据（断开期间错过的 `metrics` 增量不会补发）。

**资源变更通知**（收到后让对应查询失效重取）：`{type:"machine",id}`、`{type:"machines"}`、`{type:"relay",id}`、`{type:"job",id}`、`{type:"incidents"}`、`{type:"catalog"}`、`{type:"api_keys"}`（密钥增删撤销）、`{type:"policies"}`、`{type:"resync"}`。

**实时指标** `{type:"metrics", machines:{<机器id>:{…}}, entries:{<入口id>:{…}}, overview?}`：在用机器视图（`GET /machines` 的 `MachineView`）里随上报与时间变化的字段，和上一条相比**只带变了的字段**（没变的键不出现），客户端把它们写进本地缓存，不必重取。有新上报时最多每 3 s 一条，没有上报也每 30 s 比对一次（离线、到期天数、维护窗口随时间变）；没有变化不发。
- `machines[id]` 的键：
  - `host`：`[ts, cpu_pct, load1, mem_used, mem_total, disk_used, disk_total, net_rx_bps, net_tx_bps, tcp_conns] | null`（`ts` 为 Unix 秒，速率取整；各项可为 null，与 `MachineView.host` 同义）；
  - `seen`：`agent.last_report_at` 的 Unix 秒 | null；`agent`：`{version, online, xray_running, cores} | null`（不含 `last_report_at`，前端合并进原对象）；
  - `health`、`maintenance`、`removed_traffic`：与 `MachineView` 同形；`due_days`：`subscription.due_days`（无订阅为 null）。
- `entries[id]` 的键：`tcp` / `egress`：`[ok, latency_ms, ts, detail] | null`（`ts` 为 Unix 秒）；`traffic`：与 `EntryView.traffic` 同形。一条入口同时出现在节点和线路机的 `entries` 里，两边都要更新。
- `overview`：与 `GET /overview` 同形，变了才带。
- 机器的新建、改名、换团队、退役、作业、告警等结构变化不走 `metrics`，仍发上面的失效通知。

`fleetctl watch [--count N] [--metrics] [--json]` 订阅这个流（默认不显示 `metrics`）。

## agent（`/agent/v1`，Bearer 令牌）
见 `crates/fleet-proto/src/lib.rs`。另：`GET /echo` 返回请求方 IP（纯文本），供出口探测用，无需鉴权。

## 访问监控
线路机 agent 解析 Xray 访问日志上报，服务端写入 `access_event`（原始记录保留 `retention.access_days`=7 天），并每 5 分钟把当前小时和被标脏的小时重算进小时汇总（`access_hourly` / `access_source_hourly` / `access_target_seen`，保留 90 天，幂等；agent 断网后补传的旧记录在上报时把所在小时记进 `access_dirty`，下一轮补算；启动时补算缺口）。24h/7d/30d 的聚合结果在进程内缓存 `access_cache_secs`（默认 30）秒，设 0 关闭。全部为只读 GET，读权限即可。

公共筛选参数（空串等同没给，非法值返回 400）：`window=1h|24h|7d|30d`（默认 24h，events 默认 7d）、`team=<团队id>`、`node=<节点id>`、`search=`（大小写不敏感的子串，`%` `_` 按字面）、`action=all|blocked|routed`、`kind=all|domain|ip`。

- **窗口口径**：`1h` 直接查原始记录；`24h` 及以上查小时汇总，起点对齐到整点（`window_start` 返回实际起点），最新数据最多晚 5 分钟。
- **随机子域名合并**：注册域名之下的标签含 UUID、或 ≥8 位纯十六进制且 ≥2 位数字、或 ≥20 位字母数字且 ≥4 位数字，则整条合并为 `*.<注册域名>`（注册域名取最后两段，倒数第二段是 com/net/org/gov/edu/co 且最后一段是两字母国家码时取最后三段）。`events` 返回的是未合并的原始目标。
- **处理结果**：`action=blocked` 时 `conns` 即被阻止数；`action=routed` 时 `conns` 为去掉被阻止后的数，`blocked` 为 0。
- **来源数**：1h 窗口精确去重；24h 以上是「各小时去重数的最大值」，为偏小的近似（节点级的 `sources` 是精确去重）。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/access/summary?…` | `{kpi:{active_nodes,conns,targets,blocked}, nodes:[{node_id,name,team,conns,targets,domains,sources,blocked,last_ts}], window_start}`。`targets` 是各节点分别去重后相加；节点按连接数降序 |
| GET | `/access/targets?…&sort=conns\|last\|first&limit=100&offset=0` | `{items:[{node_id,name,dest_host,kind,conns,sources,blocked,first_seen,last_seen}], next_offset, window_start}`；`limit` 1–500，`offset` ≤ 100000，满页才给 `next_offset`；`first_seen` 是该节点历史上第一次出现（不限窗口）。不单独回总数：同一筛选下 `/access/summary` 的 `kpi.targets` 就是这张表的总行数（同一个聚合） |
| GET | `/access/sources?…&limit=100` | `{items:[{node_id,name,src_ip,conns,last_ts}], window_start}`，按 (节点, 来源 IP) 连接数降序；`search` 在这里匹配来源 IP；`limit` ≤ 500 |
| GET | `/access/events?node=&team=&search=&action=&kind=&window=&limit=100&before=<RFC3339>` | `{items:[{ts,node_id,name,src_ip,dest_host,dest_port,network,blocked}], next_before}`，按时间倒序；`before` 取上一页最后一条的 `ts`（严格小于）；`search` 同时匹配目标和来源 IP；只有最近 7 天 |
| GET | `/access/new-targets?window=24h&node=…&limit=200&offset=0` | `{items:[{node_id,name,dest_host,kind,first_seen,last_seen,total}], total, next_offset, window_start}`：顶层 `total` 是符合筛选的目标总数，`next_offset` 只在后面还有时给出；窗口内第一次出现的目标，按首次出现倒序；`total` 为累计连接数（汇总保留期内）。这是旧系统「新域名」的替代，**不产生告警** |
| GET | `/machines/{id}/access?window=24h` | `{window_start, top_domains:[{dest_host,kind,conns,blocked,sources}]（30）, top_sources:[{node_id,name,src_ip,conns,last_ts}]（20）, recent:[事件]（最近 20 条）, hourly:[{hour,conns,blocked}]（缺的小时补 0；至少 24 小时）}`。线路机汇总它名下所有节点；机器不存在 404 |

## 来源 IP 画像（IP 情报）
离线库在 fleet-server 进程内查询（ip2region v4/v6、DB-IP City/ASN Lite、iptoasn，mmap），**只查本地，绝不把 IP 发往在线服务**。库由 `deploy/ipintel/fetch-dbs.sh`（systemd timer `fleet-ipintel-fetch` 每月一次）下载到 `[ipintel] db_dir`（默认 `/var/lib/fleet/ipintel`），文件 mtime 变化后 5 分钟内自动热重载。库缺失或不全时降级：能查多少查多少，查不到的字段为 null，接口不报错。ASN 分类清单内置在 `crates/fleet-ipintel/data/asn-lists.toml`，可用 `/etc/fleet/ipintel-overrides.toml`（同样的 `[[vpn]] asn=[…] name="…"` 写法，`name=""` 表示删除）覆盖或追加。线路机和节点的 `primary_ip` 自动算「内部地址」（`[ipintel] internal_ips` 可追加运维出口）。全部为只读 GET，读权限即可。

画像摘要 `intel`：`{isp, asn, as_org, country, province, city, place, conn_type(residential|mobile|datacenter|vpn|unknown), conn_label, confidence(high|medium|low), reason, internal(我方机器名|null), private}`。`/access/sources` 与 `/machines/{id}/access` 的来源条目（`top_sources`）都带 `intel`，查不到为 null。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/ipintel/{ip}` | 单个地址的完整画像（`isp asn as_org country region city lat lon type type_word type_confidence type_reason type_alt flags sources dbs{四库原始结果}`）加 `degraded`（库没加载全）、`loaded`。非法 IP 400 |
| GET | `/ipintel?ips=a,b,c` | ≤200 个，`{items:[{ip, intel}|{ip, intel:null, error}], loaded, degraded}`；不写缓存 |
| GET | `/ipintel/status` | `{enabled, installed, loaded, expected:5, db_dir, version, files:[{id, loaded, bytes, mtime, build, entries}], errors, hosting_asns, lists（各清单条目数）, overrides{path,used,error}, cache_rows, mem, hint}`；未安装时 `hint` 提示运行 `lab/install-ipintel.sh` |
| GET | `/machines/{id}/sources?window=24h\|7d\|30d&limit=50` | 节点（线路机取名下全部节点）的来源画像：`distribution.{isp,city,province,country,conn_type}[{value,label,conns,sources,pct}]`（只统计外部来源；我方机器和非公网地址分别计入 `internal_conns` / `private_conns`）、`top_sources[{src_ip,conns,last_ts,intel}]`、`known.{isp,city,conn_type}[{value,label,first_seen,last_seen,conns,anomaly}]`（已学基线）、`learning{started_at,ends_at,learn_days,done,remaining_hours}`、`anomalies[]`（进行中的 ipintel 告警）、`empty`、`degraded`、`unresolved_sources` |

**异常告警**（warning，进 incident 表并走通知渠道）：节点第一条来源记录起 `learn_days`（默认 7）天内只学习；之后某节点出现新的运营商 → `ipintel_new_isp`，新的城市 → `ipintel_new_city`，某 (节点, 来源 IP) 第一次出现且为机房/VPN → `ipintel_datacenter_source`。跳过非公网地址和我方机器。一次性异常最后一次出现 `alert_ttl_hours`（默认 24）小时后自动恢复，不发恢复通知；同一个值恢复后不会再次告警。后台每 5 分钟从 `access_source_hourly` 增量维护 `node_source_seen`（水位在 `ipintel_state`；水位之前的小时若被补算，连接数差值经 `access_source_late` 补记，首次扫描按 7 天一页）。

## 生命周期操作：换线路机、搬空线路机、恢复已退役的机器
都是作业（可续跑、幂等、留日志），接口只做校验、建作业、写审计。

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/machines/{id}/move` | `{to_relay_id, from_relay_id?}` → `{job_id, name, from, to, share_changed:true, note}`。节点（vps / proxy）必须 active 且恰有一个 active 入口（有多个时用 `from_relay_id` 指定换掉哪条）；目标线路机必须 active、不同于当前、节点在它上面没有未移除入口；节点有进行中的作业时 409。写审计 `entry.move` |
| POST | `/relays/{id}/evacuate` | `{to_relay_id}` → `{from, to, jobs:[{machine_id,name,job_id}], skipped:[{machine_id,name,reason}], share_changed, note}`。为该线路机上每个在用入口的节点各建一个 move_entry 作业；暂时不能搬的（入口停用/未生效、节点有别的作业、目标上已有入口…）列入 `skipped`，不影响其它节点。写审计 `relay.evacuate` 与每个节点一条 `entry.move` |
| GET | `/machines/{id}/restore?team_id=&number=&label=&probe=` | 恢复预检（只对已退役机器）。见下 |
| POST | `/machines/{id}/restore` | 提交恢复 → `{job_id, machine_id, name, old_name, number}`；不满足条件 409（IP 被占用、编号被占、线路机同名、机器不是 retired）。写审计 `machine.restore` |

**换线路机作业 `move_entry`**（目标 = 节点，要求节点 active）步骤：`attach_new`（目标线路机建新入口：新端口、新 client_uuid / short_id，pending）→ `firewall_widen`（vps：节点 nft 放行名单加入新线路机 IP；proxy 跳过）→ `wait_applied_new` → `verify_new`（经新入口的出口探测通过）→ `detach_old`（旧入口 removing）→ `wait_detached_old` → `firewall_narrow`（节点在旧线路机上不再有入口时，把旧线路机 IP 从放行名单拿掉）。新入口验收通过之前旧入口不动，节点始终至少有一个可用入口。
- **取消**：`detach_old` 之前都能取消（`cancellable` 为 true）；补偿 = 摘除新入口、恢复防火墙，旧入口保持不动。开始摘旧入口之后不能取消（会让节点没有入口），失败了只能重试到完成；取消赶在摘除之后到达时保留新入口。
- **分享链接会变**：新入口有新的端口和凭据。`GET /jobs/{id}` 对 move_entry 多返回 `result:{new_entry_id, old_entry_id, from_relay_id, to_relay_id, share_changed:true, note}`；用 `GET /entries/{new_entry_id}/share` 取新链接（需要 secrets 权限）。作业日志里也有「客户端需要换成新的分享链接」。
- 旧线路机不在线时 `wait_detached_old` 会失败，线路机恢复后重试。
- 线路机退役守卫的错误提示里指向「搬空线路机」。

**恢复作业 `restore_vps` / `restore_relay` / `restore_proxy`**（目标 = 机器，要求 provisioning）：机器保持原 id（历史、账单、审计接上），lifecycle retired → provisioning，步骤与同类型上架完全相同（vps：preflight、bootstrap、converge、agent、wait_report、attach、wait_applied、verify、activate；relay：…、converge_relay、…；proxy：只有 attach、wait_applied、verify、activate）。旧入口已 removed，不复活，`attach` 建新入口。成功后 `retired_at` 清空，`lifecycle_reason` 记「已恢复：<原因>」，订阅保留。取消（或失败后取消）：机器回到 retired（团队、编号、退役原因回到恢复之前），摘掉已建的入口、撤销令牌、删出站秘密，团队编号压缩。

预检 `GET /machines/{id}/restore` 返回：`{id, kind, lifecycle, ip, old_name, name（恢复后的名字）, team:{id,name}|null, number, label, restorable, blockers[], warnings[], ip_conflict:{machine_id,name,lifecycle}|null, last_relay:{id,name}|null, retired_reason, ssh:{reachable,key_login:ok|denied|error|unreachable,host_key?}|null, needs:{relay,upstream,password:required|optional|no}}`。编号默认取团队里最小的空号（编号连续时就是 N+1），可用 `number` 指定空号；线路机按 label 唯一约束检查，原名被占用时用 `label` 改名。`ssh` 是服务端用中控密钥试一次登录（临时 known_hosts，不碰正式的）的结果，`probe=false` 跳过；`needs.password=required` 表示中控密钥被拒、必须给 root 密码。

`POST` 请求体：`{team_id?, number?, label?, relay_id?（节点要接入的在用线路机，节点必填）, login?:{password}（只用来推一次中控公钥，用完即丢）, upstream?:{protocol,host,port,username?,password?}（proxy 必填：退役时出站参数已删除）, ss_port?（vps，默认 8388；vps 恢复时重新生成 SS 密码）, reason?, reset_host_key?（机器重装过，允许预检覆盖已记录的主机指纹；退役时已忘记指纹，所以默认是重新记录）}`。

## 访问策略（域名黑白名单）
按团队或节点设置：有效策略 = 节点策略（`mode≠inherit`）> 团队策略 > 默认 `observe`。模式：`observe`（不产生规则）、`allowlist`（只放行名单内域名，其余阻止）、`denylist`（阻止名单内域名，其余放行）、`blocked`（暂停该节点所有新连接）、`inherit`（仅节点：跟随团队）。`allowlist`/`denylist` 至少一个域名。GET 需 `read`，写操作需 `write` 并写审计；预览是 POST 但无副作用（也需 `write`）。

**域名规范化**：逗号 / 中文逗号 / 顿号 / 分号 / 空白分隔，小写，去尾点；只接受 ASCII（国际化域名填 `xn--`）；每份 ≤300 个；必须含点、每标签 ≤63 且只含字母数字连字符（不以连字符开头结尾）、总长 ≤253；不能是 IP，不能带 URL、端口、通配符 `*`、正则或 `domain:` 之类前缀。`=example.com` 表示精确匹配（Xray `full:`），否则含子域（`domain:`）。保存时排序去重。错误信息中文并点名是哪一个域名（400）。

**草稿与发布**：`PUT` 只改草稿（`policy` 表的 `draft` + `revision` 乐观锁，`expected_revision` 与当前不符 409「草稿已被修改，请重新加载」；还没有记录的范围 revision 为 0）。草稿与生效值相同时草稿自动清除。`draft_hash` = 所有草稿规范化 JSON 的 sha256（十六进制），预览返回，发布 / 丢弃必须带上且一致（否则 409「草稿已变化，请重新预览」）。发布把所有草稿生效成新版本（`policy_version` 存完整快照），对**编译结果有变化**的节点所在的线路机代数 +1（每台只 +1 一次）。回滚 = 把旧版本快照作为新版本发布。版本状态由各线路机的 `applied_generation` 推导：`applying`（有线路机还没应用）→ `applied`；线路机回报应用失败且未应用 → `failed`；已退役的线路机不再等待（`gone`）。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/policies` | `{draft_hash, draft_count, teams:[{id,name,policy,nodes:[{id,name,number,has_entry,policy,effective,draft_effective}]}]}`。`policy`（范围自己的）= `{scope,id,mode,domains,note,draft:{mode,domains,note}\|null,revision,updated_by,updated_at}`；`effective`/`draft_effective`（仅节点）= `{mode,domains,note,source:"node"\|"team"\|"default"}`，分别是生效值与「草稿全部生效后」的值 |
| PUT | `/policies/{scope}/{id}` | `scope=team\|node`；body `{mode, domains:[…]\|"文本", note, expected_revision}` → `{policy, draft_hash, draft_count}`。团队不能 `inherit`；线路机没有策略 |
| POST | `/policies/preview` | 按最近 24 小时 `access_hourly` 模拟：`{draft_hash, draft_count, window_hours, affected_relays, changes:[{scope,id,name,from:{mode,domains},to:{…}}], nodes:[{node_id,name,team,mode,source,changed,total_conns,would_block,current_blocked,domains:[{host,conns}]（前 20）}]}`。只列草稿生效后非 observe 或有效策略有变化的节点；随机子域名合并行（`*.x.com`）按「整棵子树落在 domain 规则里」近似匹配 |
| POST | `/policies/publish` | body `{draft_hash}` → 新版本（同 versions 的一项） |
| POST | `/policies/discard` | body `{draft_hash}` → `{discarded}` |
| GET | `/policies/versions` | 最近 30 个：`[{id,seq,kind:"publish"\|"rollback",rollback_of_seq,actor,created_at,status:"applying"\|"applied"\|"failed",changes,relays:[{relay_id,name,generation,applied_generation,status,error}],is_latest,is_current,rollbackable}]`，`seq` 大的在前；`is_current` = 快照与当前生效值一致（不可回滚） |
| POST | `/policies/versions/{id}/rollback` | 与当前一致时 409；草稿不受影响 → 新版本 |

**下发**：`DesiredEntry.policy`（`fleet-proto`，`#[serde(default, skip_serializing_if = "Option::is_none")]`，旧 agent 忽略、新 agent 收不到 = observe）：`{"mode":"allowlist"|"denylist"|"blocked","domains":["domain:example.com","full:exact.org"]}`；observe 不带该字段。agent 在该入口默认路由规则之前插入 `policy-<入口id>-match`（名单内：allowlist 走原出站、denylist 走 `blocked`）与 `policy-<入口id>-default`（allowlist/blocked：其余走 `blocked`），只匹配该入口自己的 inboundTag——回环探测入站不受影响；规则变化经 `adrules` 整表替换热加载，不重启 Xray。被策略阻止的连接在访问日志里是 `[in-… -> blocked]`，访问监控里计为已阻止。入站 sniffing 保持 `routeOnly=false`（域名匹配靠它改写目标地址；改成 true 会让「客户端给 IP」的连接不再由线路机侧解析域名）。
**健康**：节点有效策略为 `blocked` 时，健康原因里加一条 `policy_blocked`（severity `info`，「访问策略：已暂停新连接」）；info 级原因不降级、不开告警。
**实时**：实时流 `{type:"policies"}`（草稿 / 发布 / 回滚 / 丢弃）。
