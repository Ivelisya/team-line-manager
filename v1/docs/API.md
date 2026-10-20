# fleet-server API v1

- 前缀 `/api/v1`。成功 `{"data": …}`；失败 `{"error":{"code","message"}}`（message 是给人看的中文）。
- 鉴权：会话 Cookie `fleet_session`（`POST /auth/login` 设置）；或 `Authorization: Bearer <api key>`（见下「API 密钥与 scope」）。
- 会话的写请求（POST/PUT/PATCH/DELETE）必须带头 `X-Fleet-Request: 1`（防 CSRF）；API 密钥不是浏览器，不要求这个头。
- 时间：RFC 3339 字符串；日期 `YYYY-MM-DD`。金额：整数分（`price_cents`）。流量：字节。速率：bit/s。
- 乐观锁：改机器要带 `revision`，不一致 → 409。

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
| POST | `/api-keys` | `{name, scopes:["read",…], expires_days?: 1-3650}` → `{data:{id, token}}`，**token 只返回这一次**；写审计，发 SSE `api_keys` |
| DELETE | `/api-keys/{id}` | 撤销（幂等）；之后用它的请求 401 |

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
`GET /api/v1/stream`（SSE）。事件 `data` 为 JSON：`{type:"machine",id}`、`{type:"machines"}`、`{type:"relay",id}`、`{type:"job",id}`、`{type:"incidents"}`、`{type:"catalog"}`、`{type:"api_keys"}`（密钥增删撤销）。前端据此让对应查询失效重取；另外每 30 s 一次 `{type:"tick"}` 刷新遥测。

## agent（`/agent/v1`，Bearer 令牌）
见 `crates/fleet-proto/src/lib.rs`。另：`GET /echo` 返回请求方 IP（纯文本），供出口探测用，无需鉴权。

## 访问监控
线路机 agent 解析 Xray 访问日志上报，服务端写入 `access_event`（原始记录保留 `retention.access_days`=7 天），并每 5 分钟把最近两小时重算进小时汇总（`access_hourly` / `access_source_hourly` / `access_target_seen`，保留 90 天，幂等；启动时补算缺口）。全部为只读 GET，读权限即可。

公共筛选参数（空串等同没给，非法值返回 400）：`window=1h|24h|7d|30d`（默认 24h，events 默认 7d）、`team=<团队id>`、`node=<节点id>`、`search=`（大小写不敏感的子串，`%` `_` 按字面）、`action=all|blocked|routed`、`kind=all|domain|ip`。

- **窗口口径**：`1h` 直接查原始记录；`24h` 及以上查小时汇总，起点对齐到整点（`window_start` 返回实际起点），最新数据最多晚 5 分钟。
- **随机子域名合并**：注册域名之下的标签含 UUID、或 ≥8 位纯十六进制且 ≥2 位数字、或 ≥20 位字母数字且 ≥4 位数字，则整条合并为 `*.<注册域名>`（注册域名取最后两段，倒数第二段是 com/net/org/gov/edu/co 且最后一段是两字母国家码时取最后三段）。`events` 返回的是未合并的原始目标。
- **处理结果**：`action=blocked` 时 `conns` 即被阻止数；`action=routed` 时 `conns` 为去掉被阻止后的数，`blocked` 为 0。
- **来源数**：1h 窗口精确去重；24h 以上是「各小时去重数的最大值」，为偏小的近似（节点级的 `sources` 是精确去重）。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/access/summary?…` | `{kpi:{active_nodes,conns,targets,blocked}, nodes:[{node_id,name,team,conns,targets,domains,sources,blocked,last_ts}], window_start}`。`targets` 是各节点分别去重后相加；节点按连接数降序 |
| GET | `/access/targets?…&sort=conns\|last\|first&limit=100&offset=0` | `{items:[{node_id,name,dest_host,kind,conns,sources,blocked,first_seen,last_seen}], next_offset, window_start}`；`limit` 1–500，`offset` ≤ 100000，满页才给 `next_offset`；`first_seen` 是该节点历史上第一次出现（不限窗口） |
| GET | `/access/sources?…&limit=100` | `{items:[{node_id,name,src_ip,conns,last_ts}], window_start}`，按 (节点, 来源 IP) 连接数降序；`search` 在这里匹配来源 IP；`limit` ≤ 500 |
| GET | `/access/events?node=&team=&search=&action=&kind=&window=&limit=100&before=<RFC3339>` | `{items:[{ts,node_id,name,src_ip,dest_host,dest_port,network,blocked}], next_before}`，按时间倒序；`before` 取上一页最后一条的 `ts`（严格小于）；`search` 同时匹配目标和来源 IP；只有最近 7 天 |
| GET | `/access/new-targets?window=24h&node=…&limit=200&offset=0` | `{items:[{node_id,name,dest_host,kind,first_seen,last_seen,total}], next_offset, window_start}`：窗口内第一次出现的目标，按首次出现倒序；`total` 为累计连接数（汇总保留期内）。这是旧系统「新域名」的替代，**不产生告警** |
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

**异常告警**（warning，进 incident 表并走通知渠道）：节点第一条来源记录起 `learn_days`（默认 7）天内只学习；之后某节点出现新的运营商 → `ipintel_new_isp`，新的城市 → `ipintel_new_city`，某 (节点, 来源 IP) 第一次出现且为机房/VPN → `ipintel_datacenter_source`。跳过非公网地址和我方机器。一次性异常最后一次出现 `alert_ttl_hours`（默认 24）小时后自动恢复，不发恢复通知；同一个值恢复后不会再次告警。后台每 5 分钟从 `access_source_hourly` 增量维护 `node_source_seen`（水位在 `ipintel_state`）。
