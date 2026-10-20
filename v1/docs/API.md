# fleet-server API v1

- 前缀 `/api/v1`。成功 `{"data": …}`；失败 `{"error":{"code","message"}}`（message 是给人看的中文）。
- 鉴权：会话 Cookie `fleet_session`（`POST /auth/login` 设置）；或 `Authorization: Bearer <api key>`。
- 写请求（POST/PUT/PATCH/DELETE）必须带头 `X-Fleet-Request: 1`。
- 时间：RFC 3339 字符串；日期 `YYYY-MM-DD`。金额：整数分（`price_cents`）。流量：字节。速率：bit/s。
- 乐观锁：改机器要带 `revision`，不一致 → 409。

## 认证
| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/auth/login` | `{username,password}` → 设 Cookie，`{data:{username}}` |
| POST | `/auth/logout` | |
| GET | `/auth/me` | `{data:{username}}`；未登录 401 |

## 目录
| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/teams` | `[{id,name,sort_order,node_count}]` |
| POST | `/teams` | `{name,sort_order?}` |
| PATCH | `/teams/{id}` | `{name?,sort_order?}`（改团队名 = 该团队所有节点显示名一起变） |
| GET | `/providers` | `[{id,name,code,panel_url,timezone}]` |
| POST | `/providers` | `{name,code,panel_url?,timezone?}` |
| GET | `/plans` | `[{id,provider_id,name,cpu_cores,mem_mb,disk_gb,bandwidth_mbps,traffic_gb,price_cents,currency,cycle,notes,archived_at,machine_count}]` |
| POST | `/plans` | 同上字段（不含 id/统计） |
| PATCH | `/plans/{id}` | 任意字段；`archived:true` 归档 |

## 机器
`MachineView`（列表与详情同形）：
```
{ id, kind: "relay"|"vps"|"proxy", name, team:{id,name}|null, number|null, label|null, ip, ssh_port,
  provider:{id,name}|null, provider_code|null, plan:{id,name,bandwidth_mbps,traffic_gb}|null, service_ref|null,
  lifecycle: "draft"|"provisioning"|"active"|"retiring"|"retired"|"failed", lifecycle_reason|null, note, revision,
  created_at, activated_at|null, retired_at|null,
  subscription: {price_cents,currency,cycle,due_date,due_days,billing_tz,auto_renew,remind_days,traffic_gb,traffic_reset_day}|null,
  agent: {version,last_report_at,online}|null,          // proxy 没有
  host: {ts,cpu_pct,load1,mem_used,mem_total,disk_used,disk_total,net_rx_bps,net_tx_bps,tcp_conns}|null,
  entries: [EntryView],                                // 节点：自己的入口；线路机：挂在上面的入口
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
| GET | `/jobs?state=active\|done\|all&limit=` | |
| GET | `/jobs/{id}` | 含 steps |
| GET | `/jobs/{id}/logs?after=<log id>` | `[{id,ts,level,msg}]` |
| POST | `/jobs/{id}/retry` | 失败的作业从失败那一步重来 |
| POST | `/jobs/{id}/cancel` | 进行中的作业：补偿已完成的步骤后停下 |

## 总览与告警
| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/overview` | `{nodes:{total,ok,degraded,down,unknown,maintenance}, relays:{…}, provisioning, retiring, due_7d, monthly_cost:[{currency,cents}]（按币种分别汇总，不折算）, traffic_month:{up,down}, net_rx_bps, net_tx_bps}`（只统计 active） |
| GET | `/incidents?state=firing\|resolved\|all` | |
| POST | `/incidents/{id}/ack` | |
| GET/POST | `/silences`、DELETE `/silences/{id}` | `{subject_id?,rule?,starts_at,ends_at,is_maintenance,comment}` |
| GET | `/audit?target_id=&limit=` | |

## 返回形状补充（前端以此为准）

- `health.reasons[].severity`：`"warning"` | `"critical"`（告警的 `severity` 另有 `"info"`）。
- `GET /machines/{id}/series` → `{metric, range, resolution: "1m"|"1h", points: [...]}`，点里时间字段统一叫 `ts`：
  - host：`{ts, cpu_pct, mem_pct, disk_pct, load1, net_rx_bps, net_tx_bps}`（1h 分辨率另有 `cpu_max`，无 `load1`）
  - traffic：`{ts, up, down, conns}`（字节，该时间桶内的增量）
  - probe：`{ts, tcp_ok, tcp_ms, egress_ok, egress_ms}`（失败时 `*_ms` 为 null；1h 分辨率另有 `tcp_success_pct`、`egress_success_pct`）
- 作业 `Job`：`{id, kind, target: {id,name}|null, state, step, step_count, error, created_by, created_at, started_at, finished_at}`；
  `state` ∈ `queued | running | waiting | cancelling | succeeded | failed | cancelled`；详情另有 `steps[]`（`output` 是 JSON 对象或 null）与 `cancellable`。
  作业 `kind` ∈ `provision_vps | provision_proxy | provision_relay | attach_entry | retire | purge | upgrade_agent`。
- 告警 `Incident`：`{id, rule, subject: {id,name}, subject_id, subject_name, severity, state: "firing"|"resolved", summary, opened_at, last_seen_at, resolved_at, ack_at, ack_by}`。
- 静默 `Silence`：`{id, subject_id, subject_name, rule, starts_at, ends_at, is_maintenance, comment}`。
- 续费 `Renewal`：`{id, old_due, new_due, amount_cents, note, actor, created_at}`。
- 审计：`{id, ts, actor, action, title, summary, target_kind, target_id, detail}`；界面显示 `title` 与 `summary`（中文，服务端生成），`detail` 是原始 JSON。

## 实时
`GET /api/v1/stream`（SSE）。事件 `data` 为 JSON：`{type:"machine",id}`、`{type:"machines"}`、`{type:"relay",id}`、`{type:"job",id}`、`{type:"incidents"}`、`{type:"catalog"}`。前端据此让对应查询失效重取；另外每 30 s 一次 `{type:"tick"}` 刷新遥测。

## agent（`/agent/v1`，Bearer 令牌）
见 `crates/fleet-proto/src/lib.rs`。另：`GET /echo` 返回请求方 IP（纯文本），供出口探测用，无需鉴权。
