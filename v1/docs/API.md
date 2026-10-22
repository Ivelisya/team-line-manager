# fleet-server API v1

- 前缀 `/api/v1`。成功 `{"data": …}`；失败 `{"error":{"code","message"}}`（message 是给人看的中文）。
- 鉴权：会话 Cookie `fleet_session`（`POST /auth/login` 设置）；或 `Authorization: Bearer <api key>`（见下「API 密钥与 scope」）。
- 会话的写请求（POST/PUT/PATCH/DELETE）必须带头 `X-Fleet-Request: 1`（防 CSRF）；API 密钥不是浏览器，不要求这个头。
- 时间：RFC 3339 字符串；日期 `YYYY-MM-DD`。金额：整数分（`price_cents`）。流量：字节。速率：bit/s。
- 乐观锁：改机器要带 `revision`，不一致 → 409。
- 状态码：输入不对 400（请求体、路径参数如坏 uuid、查询参数解析失败也是 JSON 400；枚举值不认识；引用的对象不存在，如 `provider_id` 指向不存在的供应商）；对象不存在 404；状态冲突 409（重复、被其他数据引用不能删、状态不允许）。
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
  host_machine:{id,name}|null,  // 同机 IP 节点（kind 仍是 vps）：它所在的宿主机；它没有自己的 agent 和 SSH，下面的 agent、host 就是宿主机的，
                                // ssh_port 是宿主机的（只展示），provider 取宿主机的、plan 为 null；其它机器为 null
  ip_nodes:[{id,name}],         // 宿主机：它上面未退役的同机 IP 节点（按团队、编号）；其它机器为 []
  provider:{id,name}|null, provider_code|null, plan:{id,name,bandwidth_mbps,traffic_gb}|null, service_ref|null,
  lifecycle: "draft"|"provisioning"|"active"|"retiring"|"retired"|"failed", lifecycle_reason|null, note, revision,
  created_at, activated_at|null, retired_at|null,
  subscription: {price_cents,currency,cycle,due_date,due_days,billing_tz,auto_renew,remind_days,traffic_gb,traffic_reset_day}|null,
  agent: {version,last_report_at,online,xray_running,cores,health,clock_skew_ms}|null,  // proxy 没有；cores 为 agent 报的可用核数（容器里是限额），没报过为 null
                                // clock_skew_ms：agent 时钟偏差（毫秒）= 中控收到时刻 − 报告的 sent_at_ms，正数 = agent 时钟慢；只按新鲜的报告
                                // （建报时离线队列为空、上一份 30 秒内收到）更新，误差 < 30 秒，没算过为 null。|偏差| > 60 秒时健康原因有 clock_skew
                                // health：agent 自报的健康数据（fleet-proto AgentHealth，自 agent 进程启动起累计，重启归零），旧 agent 没报过为 null：
                                // {queue_len 离线队列待发报告数, queue_dropped 超上限丢弃的报告数, queue_dead 移入 dead/ 的报告数,
                                //  access_unparsed 解析不了的日志行数, access_dropped_lines 丢弃的日志行数, access_rotation_gaps 日志轮转可能漏掉一段的次数,
                                //  access_truncations 日志被截断的次数, last_apply_error?（最近一次应用失败，成功后不带）,
                                //  disk_write_error?（报告写不进 agent 的磁盘队列——多半是磁盘满——时的错误：这份是直接从内存发来的；写得进时不带）}
  host: {ts,cpu_pct,load1,mem_used,mem_total,disk_used,disk_total,net_rx_bps,net_tx_bps,tcp_conns}|null,  // load1 在容器里为 null（/proc/loadavg 是宿主机的）
  entries: [EntryView],                                // 节点：自己的入口；线路机：挂在上面的入口
  removed_traffic: {today_up,today_down,month_up,month_down},  // 已移除入口（如换线路机后的旧入口）本月/今天的流量，不在 entries 里
  quota: {used, period_start},  // 流量额度周期已用的业务流量（字节，下载+上传，含已移除入口）与周期起点（日期）。
                                // 订阅有 traffic_reset_day：账单时区里今天或之前最近一次重置日起（月份没有这一天按月末，如 31 号在 9 月是 30 号），
                                // 从该日账单时区零点累计；否则为北京时间的本月。来自小时汇总表，每次汇总后更新。
                                // 宿主机的 used 含它的同机 IP 节点（含已退役的）入口的流量（额度共用）；同机 IP 节点自己的 used 只算它自己的
  health: {status: "ok"|"degraded"|"down"|"unknown"|"maintenance"|"inactive", reasons:[{key,severity,text}]},
  active_job: {id,kind,state}|null,                    // 进行中的作业（queued/running/waiting/cancelling）
  failed_job: {id,kind,state}|null,                    // lifecycle = failed（上架失败）时：失败的上架/恢复作业，重试回到 provisioning，取消则补偿退役；
                                                       // lifecycle = retiring 且退役作业失败时：该 retire 作业（等重试）；其它情况 null
  open_incidents, maintenance,   // maintenance：同机 IP 节点在宿主机维护时也为 true
  relay: {public_host,generation,applied_generation,applied_at,apply_error,port_min,port_max}|null }
EntryView = { id, relay:{id,name}, node:{id,name}, direct, port, lifecycle, applied, apply_error,
  tcp:{ok,latency_ms,detail,ts}|null, egress:{…}|null, traffic:{today_up,today_down,month_up,month_down} }
// direct：线路机直出入口——节点就是线路机自己（node.id = relay.id，node.name =「线路机名-直出」），出口是线路机自己的 IP；
//   只出现在线路机的 entries 里（一次）；没有 TCP 探测（tcp 恒为 null），出口不通时线路机自己报 egress_fail
```
| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/machines?scope=live\|retired\|all` | 默认 live（未退役）。排序：线路机在前，然后按团队、编号。加 `view=brief` 只返回 `{id, kind, name, label, ip, lifecycle, host_id, team:{id,name}\|null}`（fleetctl 按名字找机器用；`host_id` 非 null 的是同机 IP 节点） |
| GET | `/machines/{id}` | 单台 |
| PATCH | `/machines/{id}` | `{revision, team_id?, number?, label?, note?, provider_id?, plan_id?, service_ref?}`。改团队/编号 = 改名：线路机备注随后自动同步，不换令牌、不断连接 |
| GET | `/machines/{id}/series?metric=host\|traffic\|probe&range=1h\|6h\|24h\|7d\|30d` | 时序。≤24h 用原始点（1 分钟聚合），更长用小时汇总。同机 IP 节点的 `metric=host` 是宿主机的曲线 |
| GET | `/machines/{id}/availability?days=30` | 近 N 天（`days` 1–90，默认 30，越界 400）每天的探测成功率：`{data:{days, items:[{date,tcp_total,tcp_ok,egress_total,egress_ok}], tcp_pct, egress_pct}}`。探测小时汇总按这台机器的账单时区（没有订阅按北京时间）分到日历天（一个小时桶按起点归天），`items` 旧→新共 `days` 项、最后一项是今天（还没过完），没有探测的天各项为 0；`tcp_pct` / `egress_pct` 是整段的成功率（0–100），没有探测为 null。入口同 `series`：节点取自己的，线路机取挂在它上面的（含已移除的）。机器不存在 404 |
| GET | `/machines/{id}/retire` | 退役预览：`{data:{name, renames:[{machine_id,old_name,new_name}], ip_nodes:[{id,name}]}}`——编号保持连续，退役后同团队后面的节点依次前移；`ip_nodes` 是会一并退役的同机 IP 节点（宿主机才有，还没开始退役的），`renames` 按所有受影响的团队算 |
| POST | `/machines/{id}/retire` | `{confirm: <显示名>, reason?, cleanup?: true}` → `{data:{job_id, ip_node_jobs:[{machine_id,name,job_id}]}}`。退役完成时同团队编号自动压缩成 01..N（写审计）。线路机上还有节点入口时 409（提示先搬空）；它自己的直出入口不算，由退役作业的「摘除入口」一并摘掉。**宿主机**：它上面还没开始退役的同机 IP 节点在同一事务里一并置为退役中（`lifecycle_reason`「随宿主机「名字」退役」），作废它们进行中 / 失败的作业、关掉它们的告警，各建一个退役作业（`cleanup: false`：只摘入口，不登录清理——宿主机的清理连同所有实例一起删），列在 `ip_node_jobs`（没有为 []），各写一条审计 `machine.retire`。单独退役同机 IP 节点只删它自己的实例 |
| POST | `/machines/{id}/purge` | 仅已退役。`{confirm: <显示名>}` → 作业；彻底删除机器与遥测，保留审计。宿主机上还有同机 IP 节点（含已退役的）没删时 409：「这台机器上还有同机 IP 节点没有彻底删除（含已退役的）：…；先删它们」。它的入口近 35 天有小时流量、且入口另一端的机器未退役时 409：「这台机器的入口在 YYYY-MM-DD 前仍计入其他机器的流量统计，届时再彻底删除」（作业执行时再查一次） |
| POST | `/machines/{id}/upgrade-agent` | 推送中控当前版本的 agent 并重启（作业 `upgrade_agent`；令牌顺带轮换）。线路机顺带收敛 fleet-xray 单元（变了只 daemon-reload，不重启 Xray，等它下次重启才生效）与访问日志轮转（配置 + 每小时的 fleet-xray-logrotate.timer），这两样没装上只记警告。同机 IP 节点 409（agent 是宿主机的，升级宿主机） |
| POST | `/agents/upgrade` | 所有在用线路机与落地机（不含同机 IP 节点）各建一个 `upgrade_agent` 作业（有进行中作业的跳过；每台单独提交，一台冲突不影响其他台） → `{data:{jobs:[{machine_id,job_id}], skipped:[{machine_id,reason}]}}` |
| PUT | `/machines/{id}/subscription` | `{price_cents,currency,cycle,due_date,billing_tz,auto_renew,remind_days,traffic_gb?,traffic_reset_day?}` |
| POST | `/machines/{id}/renewals` | `{new_due,amount_cents?,note?}` |
| GET | `/machines/{id}/renewals` | |
| GET | `/teams/{id}/renumber` | 预览：`{data:{changes:[{machine_id,old_name,new_name}],token}}` |
| POST | `/teams/{id}/renumber` | `{token}`（预览返回的，防止预览后数据变了）→ 一次生效 |

## 上架
| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/provision/check` | 校验（不落库）：`{items:[ProvisionItem]}` → `{data:{items:[{ok,errors:[...],name,number,ss_port}]}}`（`ss_port`：同机 IP 节点暂定的 SS 端口，其它为 null） |
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
  // vps：线路机连节点 SS 用的公网端口（NAT 机填供应商映射到节点 8388 的那个公网端口；非 NAT 机不要填）。
  // 节点上 ssserver 与防火墙固定在 8388，这个值只写进线路机出站
  ss_port?: 8388,
  // 没给的：price_cents / currency / cycle 取套餐，billing_tz 取供应商时区（机器的供应商，否则套餐的）；都没有时 0 / USD / monthly / Asia/Shanghai
  subscription?: {price_cents?, currency?, due_date, cycle?, billing_tz?},
  // 同机 IP 节点（kind 必须是 vps）：加在这台落地机（宿主机）上，ip 是它自己的出口 IP（要已直接配在宿主机网卡上，EIP / NAT 映射的不行）。
  // 宿主机必须存在、是在用的普通落地机（不是线路机、家宽代理或同机 IP 节点）。这时 ssh_port / login / ss_port / provider_id / plan_id / reset_host_key
  // 不适用（忽略）：SSH 端口与供应商取宿主机的、不带套餐、不收 root 密码；SS 端口由中控在宿主机上分配（8389 起，同批依次递增，跳过宿主机上未退役同机节点已占的）。
  // 作业是 provision_ip；费用默认不记（subscription 照常可给）
  host_id?: uuid
}
```
校验时先锁团队行再锁宿主机行：同一台宿主机上的端口分配一次一个事务，和退役宿主机互斥。

## 线路机与入口
| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/relays` | `MachineView[]`（kind=relay） |
| POST | `/entries/{id}/actions` | `{action:"enable"\|"disable"\|"rotate", confirm?}`；rotate 需 `confirm="<线路机名>:<端口>"` |
| GET | `/entries/{id}/share` | `{data:{url}}`（vless:// 分享链接；记审计） |
| POST | `/machines/{node_id}/entries` | 给节点再挂一条入口：`{relay_id}` → 作业（`attach_entry`，审计 `entry.add`）。`relay_id` 指向的线路机不存在 400；线路机不在用、节点在它上面已有入口 409（不建作业）。**线路机直出入口**：路径和 `relay_id` 都是这台线路机（`POST /machines/{线路机}/entries {relay_id: 线路机}`），出站是 freedom、从线路机自己的公网 IP 出去，出口验收期望的就是线路机的 IP；每台线路机至多一条（409「这台线路机已经有直出入口」），`relay_id` 填别的线路机 409。端口、REALITY 参数、代数、等应用、出口验收、审计与普通入口相同；作业的「更新节点防火墙」一步跳过。分享链接 # 后是「线路机名-直出」，线路机改名后随期望状态同步 |

## 作业
`Job = {id,kind,target:{id,name}|null,state,step,step_count,step_title,error,created_by,created_at,started_at,finished_at,steps:[{idx,key,title,state,started_at,finished_at,output,error}]}`（`step_count` = 该种作业的步骤数，`step_title` = 第 `step` 步的标题，做完为 null）
| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/jobs?state=active\|done\|all\|failed&target_id=&limit=100&offset=0` | 分页：`{items:[Job（不含 steps、cancellable、result，另有 step_title）], total, next_offset}`，按创建先后倒序（`id desc`） |
| GET | `/jobs/{id}` | 含 steps |
| GET | `/jobs/{id}/logs?after=<log id>` | `[{id,ts,level,msg}]` |
| POST | `/jobs/{id}/retry` | 失败的作业从失败那一步重来。可选请求体 `{reset_host_key?: bool, ssh_password?: string}`（不带请求体 = 都不要）：`reset_host_key` 先清掉 known_hosts 里这台机器的记录、按现在的主机公钥重新记录（报「主机指纹变化」且确认机器重装过才用；拿不到公钥 409）；`ssh_password` 是 root 密码，和上架时一样加密存进作业参数，供重试的作业在中控密钥登录不了时用（用过即丢；失败作业留存的密码 3 天后清除，之后重试要重新给）。只有 failed 的作业能重试（否则 409）；上架/恢复失败的机器重试时回到 provisioning。审计 `job.retry` 记 `reset_host_key` 与是否给了密码（不记密码） |
| POST | `/jobs/{id}/cancel` | 进行中的作业：补偿已完成的步骤后停下 |

## 运维：远程执行与日志
需要 `exec` scope（会话用户直接可用）。中控用自己的 SSH 密钥和 known_hosts 登录；机器必须不是已退役、不是家宽代理（proxy 没有 SSH）。命令原样当 bash 脚本执行（stdin 接 /dev/null），超时由远端 `timeout` 杀掉。每次执行写审计（actor、机器、命令全文、退出码、耗时），**不要在命令里写密码**。
| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/machines/{id}/exec` | `{command, timeout_s?: 1-600 (默认 60)}` → `{data:{exit_code, stdout, stderr, duration_ms, truncated, timed_out}}`。同机 IP 节点没有自己的 SSH：命令在它的宿主机上执行（审计记在这台节点名下）。stdout/stderr 各截断到 256 KiB（`truncated=true`）；超时 `timed_out=true`：到点远端 `timeout` 发 TERM，`exit_code=124`；命令不理会 TERM 的，再过 5 秒被 KILL（`timeout -k 5`），`exit_code=137`；远端没能按时结束、中控本地等到 `timeout_s+15` 秒仍没返回时也按超时返回 `exit_code=124`。SSH 连不上时 `exit_code=255` 且 stderr 有原因（命令自己退出 255 也是 255）。**只执行一次、不自动重试**（255 时命令可能已经执行过）。机器不存在 404、已退役或 proxy 409、命令空/超时越界 400、SSH 无法启动 502 |
| GET | `/machines/{id}/logs?unit=fleet-agent\|fleet-xray\|ssserver&lines=200` | 白名单 unit（`ssserver` 对应 `fleet-ssserver`；同机 IP 节点对应宿主机上它自己的实例 `fleet-ssserver@<SS 端口>`，`fleet-agent` 是宿主机的），lines 1-2000；内部走同一 exec 通道跑 `journalctl`，返回形状同 exec，另有 `unit`（实际查的单元）、`lines`。审计动作 `machine.logs` |
| GET | `/server/logs?lines=200` | 中控自己的 `journalctl -u fleet-server`。fleet 用户通常没权限读 journal：journalctl 报错时 424（`error.code="journal_denied"`，message 说明如何授权：`usermod -aG systemd-journal fleet`）；journalctl 无法运行或 20 秒没返回时 500 |

## 总览与告警
| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/overview` | `{nodes:{total,ok,degraded,down,unknown,maintenance}, relays:{…}, provisioning, retiring, failed（上架失败的机器数，网页「有问题」里也算它们）, due_7d（7 天内到期或已过期、需要续费的：自动续费的只在已过期时算）, online, offline（按 agent 连通：agent 没有近期上报为离线，家宽代理没有 agent 算在线；与健康无关，有严重告警但照常上报的算在线）, monthly_cost:[{currency,cents}]（按币种分别汇总，不折算）, traffic_month:{up,down}, net_rx_bps, net_tx_bps}`（只统计 active；速率不含 agent 离线的机器——告警不影响，照常上报的机器照算；同机 IP 节点的读数就是宿主机的，速率不重复算，节点数与在线 / 离线照常按节点计） |
| GET | `/overview/series?minutes=30` | 总览网速曲线：`{points:[{ts, net_rx_bps, net_tx_bps}]}`，`minutes` 1–60（默认 30，越界 400）；最近 N 个**结束满 20 s** 的分钟（agent 每 10 s 上报，刚结束的那一分钟还缺最后的上报，不给）每分钟一个点、旧→新，`ts` 为该分钟起点（RFC3339）。每个点 = 该分钟有主机指标的 active 机器速率之和，单台算法同 `/machines/{id}/series?metric=host`（每分钟计数器最大值与上一个有数据的分钟相减 ×8 / 秒数；计数器回绕的那一段不计）；没有数据的分钟为 0 |
| GET | `/probe-beats` | 探测心跳条：`{<机器id>: [[n, ok, ms], …20 格]}`。每格 3 分钟、按墙钟对齐（`date_bin('3 minutes', ts, 'epoch')`，即每小时的 :00 / :03 / … ），旧→新，最后一格是当前这格（还没走完）；第 i 格 = `[cb-57m+3i 分, cb-57m+3(i+1) 分)`，cb = 当前这格的起点；`n` = TCP 探测次数，`ok` = 成功次数，`ms` = 成功探测的平均延迟（没有成功的为 null）；没有探测的格为 `[0,0,null]`。节点汇总它的入口、线路机汇总挂在它上面的所有入口；只含这 20 格里有探测数据的在用机器 |
| GET | `/incidents?state=firing\|resolved\|all&subject_id=&limit=100&offset=0` | 分页：`{items:[{id,rule,rule_name,subject:{id,name},subject_id,subject_name,severity,state,summary,opened_at,last_seen_at,resolved_at,ack_at,ack_by,reopen_count}], total, next_offset}`；`state` 默认 firing；`subject_id`（可选，机器 id）只看这台机器的告警，与 `state`、分页组合，`total` 同样只数它的（如抽屉的最近告警：`?subject_id=<id>&state=all&limit=5`；不是 uuid 时 400）；`reopen_count`：恢复后 30 分钟内同一（规则, 对象）复发会重开原告警而不是新建，每次 +1；顺序：进行中在前 → 严重度高在前 → 开始时间新在前（同时刻按 id，翻页不重不漏）；带 `subject_id` 时不看严重度：进行中在前 → 开始时间新在前（一台机器的「最近告警」按时间读） |
| POST | `/incidents/{id}/ack` | 确认告警：不存在 404，已确认过 409；写审计 `incident.ack` |
| GET/POST | `/silences`、DELETE `/silences/{id}` | GET：还没结束的（`ends_at` 晚于现在，含还没开始的预约）`[Silence]`，按 `starts_at` 排——进行中的在前，已预约的随后；只有此刻在窗口内的才起作用（不开告警、不发通知，维护还暂停健康判定）。POST `{subject_id?,rule?,starts_at,ends_at,is_maintenance,comment}` → `{id}`（`subject_id` 空 = 全部机器，`rule` 空 = 全部规则）：`ends_at` 要晚于 `starts_at`、时长最多 30 天、`ends_at` 不能已经过去（`starts_at` 可以，「从现在开始」由客户端按自己的时钟填）、`starts_at` 最多在 90 天后，`rule` 必须是已知规则 key（且不能与 `is_maintenance` 同时给：维护针对整台机器）、`subject_id` 必须是存在的机器，否则 400（中文原因）。DELETE 不存在或已删 404。新建、删除都写审计（详情是这条静默本身：对象名、规则、起止、维护、备注） |
| GET | `/audit?target_id=&limit=&before_id=` | 按 id 倒序；`before_id` 翻页（只要 id 小于它的，传上一页最后一条的 id）。没有 `exec` scope 的密钥看不到命令：`detail.command` 换成「命令已隐藏（N 字）」，`summary` 里的命令同样隐藏。`exec` / `machine.logs` 在执行前就写一条 `detail.started=true` 的记录，执行完补上结果并去掉 `started`；`started` 还在说明在执行中或中控中途重启 |
| GET | `/notify-log?limit=100` | 通知发送记录（新→旧，`limit` 1–1000）：`[{id, channel_id, channel_name, incident_id, ts, ok, error, kind: "firing"\|"repeat"\|"resolved"\|"test", subject_name, rule_name}]`；`subject_name` 是告警对象的显示名，`rule_name` 是规则中文名（与通知正文一致）；测试消息两者为 null，机器或告警已删除时对应字段为 null |

## 返回形状补充（前端以此为准）

- `health.reasons[]`：`{key, rule_name, severity, text}`；`rule_name` 是规则中文名（同通知）；`severity`：`"info"` | `"warning"` | `"critical"`（info 只有 `policy_blocked`，不影响健康）。
  `traffic_quota_high`（「流量额度吃紧」）：本周期已用（`quota.used`）到上限（订阅的 `traffic_gb`，没填按套餐的；0 / 空 = 不限，不评估）的 90% 为 warning，到 100% 为 critical，文案如「本周期流量已用 92%（1.84 TiB / 2 TiB），9 天后重置」「本周期流量已超出额度（已用 102%，2.05 TiB / 2 TiB）」（有流量重置日才写几天后重置）。
  它和 info 一样**不影响** `health.status`，只开告警：不去抖，回落到 90% 以下（如新周期开始）下一轮评估即恢复；升为 critical 时马上再通知一次，持续提醒每天最多一次（同续费类）。同机 IP 节点自己没设上限时不评估（额度算在宿主机上）。
  `clock_skew`（warning，「agent 时钟不准」）：agent 在线且 `|agent.clock_skew_ms|` > 60 秒，文案如「agent 时钟慢 17 分钟：读数可能缺失或冻结，请校时」——样本按 agent 的时间入库，当前读数只看最近 15 分钟、持续负载只看最近 5 分钟。和其它 warning 原因一样满 60 秒去抖后开告警。
  同机 IP 节点不报 `agent_offline`、`clock_skew`、`host_cpu_high` / `host_mem_high` / `host_disk_high`（agent 与主机是宿主机的，由宿主机报）；`egress_fail`、`tcp_fail`、`no_entry`、续费照常自己报（宿主机整台宕机时宿主机和它的每个同机节点各报各的探测告警，不抑制）。
- `GET /machines/{id}/series` → `{metric, range, resolution: "1m"|"1h", points: [...]}`，点里时间字段统一叫 `ts`：
  - host：`{ts, cpu_pct, mem_pct, disk_pct, load1, net_rx_bps, net_tx_bps}`（1h 分辨率另有 `cpu_max`，无 `load1`）
  - traffic：`{ts, up, down, conns}`（字节，该时间桶内的增量）
  - probe：`{ts, tcp_ok, tcp_ms, egress_ok, egress_ms}`（失败时 `*_ms` 为 null；1m 分辨率另有 `tcp_n`、`tcp_ok_n`（该分钟 TCP 探测次数 / 成功次数，线路机为其所有入口合计）；1h 分辨率另有 `tcp_success_pct`、`egress_success_pct`）
- 作业 `Job`：`{id, kind, target: {id,name}|null, state, step, step_count, step_title, error, created_by, created_at, started_at, finished_at}`（`step` 当前或失败的步骤下标，`step_title` 它的标题，做完为 null）；
  `state` ∈ `queued | running | waiting | cancelling | succeeded | failed | cancelled`；详情另有 `steps[]`（`output` 是 JSON 对象或 null；各步骤输出的 key 与中文展示见 `web/src/lib/jobs.ts` 的 `OUTPUT`，跳过的步骤为 `{reason}`）与 `cancellable`。
  作业 `kind` ∈ `provision_vps | provision_proxy | provision_relay | provision_ip | restore_vps | restore_proxy | restore_relay | restore_ip | attach_entry | move_entry | retire | purge | upgrade_agent`。
  `provision_ip`（上架同机 IP 节点）/ `restore_ip`（恢复同机 IP 节点）的步骤：`ip_check`（在宿主机上核对 IP，输出 `{ss_port, local_ips, egress_ip}`：最终的 SS 端口、宿主机本机的 IPv4（逗号加空格分隔的字符串）、经这个 IP 访问中控 /echo 回显的地址）→ `converge`（推 ssserver、装它自己的实例与防火墙，输出同落地机，`bbr` 为 null——不做系统基线）→ `attach` → `wait_applied` → `verify` → `activate`。
  `ip_check` 失败：宿主机 SSH 连不上（network）；IP 不在宿主机网卡上（「IP x 不在宿主机 H 的网卡上（本机：…）」，remote）；回显的来源不是这个 IP（出站路由不对，remote）。暂定的 SS 端口在宿主机上被占用时自动换成最小空闲端口，日志里有一条警告。
  同机 IP 节点的退役作业 `cleanup` 一步只删它自己的实例（输出 `ss` 是 gone / still_listening，指它自己的端口）。
- 告警 `Incident`：`{id, rule, rule_name（规则中文名，同通知）, subject: {id,name}, subject_id, subject_name, severity, state: "firing"|"resolved", summary, opened_at, last_seen_at, resolved_at, ack_at, ack_by, reopen_count}`（`subject_name` 机器已删除时为 null）。
- 静默 `Silence`：`{id, subject_id, subject_name, rule, rule_name, starts_at, ends_at, is_maintenance, comment}`（`subject_id` / `subject_name` 为 null = 全部机器；`rule_name` 是规则中文名，同告警，`rule` 为 null（全部规则）时也为 null）。
- 续费 `Renewal`：`{id, old_due, new_due, amount_cents, note, actor, created_at}`。
- 审计：`{id, ts, actor, action, title, summary, target_kind, target_id, detail}`；界面显示 `title` 与 `summary`（中文，服务端生成），`detail` 是原始 JSON。

## 实时
`GET /api/v1/stream`：WebSocket（`ws://` / `wss://`），鉴权同其它 GET（会话 Cookie 或 `Authorization: Bearer`，需要 `read`），握手失败返回普通 HTTP 错误（401/403）。服务端只发 JSON 文本帧，不读客户端消息。
- **保活**：服务端每 15 s 发 ping，到下一次 ping 还没收到 pong 就断开（浏览器自动回 pong）。
- **会话复核**：每 60 s 查一次会话/密钥是否仍有效，失效则以关闭码 `4401` 关闭（客户端应回登录页，`fleetctl watch` 报错退出）；复核查询本身失败以 `1011` 关闭，客户端照常退避重连、握手时重新鉴权。
- **滞后**：服务端广播缓冲 256 条，某个连接读得太慢丢了消息时补发 `{type:"resync"}`，客户端应让全部缓存失效重取。
- **重连**：客户端断线后自己退避重连；连上后应重取一次机器类数据（断开期间错过的 `metrics` 增量不会补发）。

**资源变更通知**（收到后让对应查询失效重取）：`{type:"machine",id}`、`{type:"machine_job",id}`（作业推进到这台机器：只需重取它的详情；列表里的 `active_job` 走 `metrics`，改生命周期时另发 `machines`）、`{type:"machines"}`、`{type:"relay",id}`、`{type:"job",id}`、`{type:"incidents"}`、`{type:"catalog"}`、`{type:"api_keys"}`（密钥增删撤销）、`{type:"policies"}`、`{type:"resync"}`。

**实时指标** `{type:"metrics", machines:{<机器id>:{…}}, entries:{<入口id>:{…}}, overview?}`：在用机器视图（`GET /machines` 的 `MachineView`）里随上报与时间变化的字段，和上一条相比**只带变了的字段**（没变的键不出现），客户端把它们写进本地缓存，不必重取。有新上报或作业推进时最多每 3 s 一条，没有也每 30 s 比对一次（离线、到期天数、维护窗口随时间变）；没有变化不发。
- `machines[id]` 的键：
  - `host`：`[ts, cpu_pct, load1, mem_used, mem_total, disk_used, disk_total, net_rx_bps, net_tx_bps, tcp_conns] | null`（`ts` 为 Unix 秒，速率取整；各项可为 null，与 `MachineView.host` 同义）；
  - `seen`：`agent.last_report_at` 的 Unix 秒 | null；`agent`：`{version, online, xray_running, cores} | null`（不含 `last_report_at`、`health` 和 `clock_skew_ms`——agent 自报的累计计数、时钟偏差每次上报都变，只随 REST 重取更新；前端合并进原对象）；
  - `health`、`maintenance`、`removed_traffic`、`quota`、`active_job`：与 `MachineView` 同形；`due_days`：`subscription.due_days`（无订阅为 null）。
- `entries[id]` 的键：`tcp` / `egress`：`[ok, latency_ms, ts, detail] | null`（`ts` 为 Unix 秒）；`traffic`：与 `EntryView.traffic` 同形。一条入口同时出现在节点和线路机的 `entries` 里，两边都要更新。
- `overview`：与 `GET /overview` 同形，变了才带。
- 机器的新建、改名、换团队、退役、告警等结构变化不走 `metrics`，仍发上面的失效通知。
- 静默的新建 / 删除只发 `incidents`（不发 `machines`）：卡片上的 `maintenance`、`health`、总览的维护中计数随后（最多 3 s）经 `metrics` 送达，不必为它重取机器列表。

`fleetctl watch [--count N] [--metrics] [--json]` 订阅这个流（默认不显示 `metrics`）。

## agent（`/agent/v1`，Bearer 令牌）
见 `crates/fleet-proto/src/lib.rs`。另：`GET /echo` 返回请求方 IP（纯文本），供出口探测用，无需鉴权。线路机经入口访问它失败时，agent 再不走代理直连它一次：直连也拿不到 IP（中控重启、部署中）就不记这次出口探测，不算节点出口失败。

直出入口在期望状态里是 `"outbound": {"protocol": "direct"}`（`expect_egress_ip` 为线路机自己的 IP）。不认识它的旧 agent 整份期望状态解析失败、回报「agent 版本过旧」（线路机 apply_error），所以**先升级线路机 agent，再建直出入口**。agent 为它渲染入口自己的 freedom 出站（`out-<入口>`，不复用共享的 `direct`），不做 TCP 探测；路由和节点入口相同（AsIs，`block-private` / `block-bittorrent` 在前），另在该入口规则最前面加 `guard-<入口>-api`：目标端口是本机 Xray API（62789）的连接一律 blocked——`block-private` 只拦 IP 目标，客户写解析到回环的域名（`localhost`、`127.0.0.1.nip.io`）时 freedom 会在线路机本机解析并连上 API。解析到内网其它地址的域名仍能连（与旧系统的主入口相同）；没有改成 IPIfNonMatch 先解析再过 `block-private`，是因为 fake-ip DNS 环境（实验室，域名都解析到 198.18.0.0/15）里那样会把所有域名都拦掉。

`POST /agent/v1/report` 入库失败按类回：数据本身入不了库（SQLSTATE 22 类：坏字节、NUL、越界；23 类：违反约束）→ 422（`error.code="bad_report"`），agent 把这份报告移入 `dead/` 不再重发，其中的入口流量结转到下一份报告补报（同离线队列超限淘汰；这份本身带着结转补报的流量时不再结转，免得一份接一份地被拒）；其它（连接池超时、断连、磁盘满、只读…）→ 503，agent 退避重试（受本地队列的字节与时长上限约束）。agent 只把 4xx（401 撤销、403/404/408/429 除外）当永久拒绝，5xx 和网络错误一律重试。

## 访问监控
线路机 agent 解析 Xray 访问日志上报，服务端写入 `access_event`（原始记录保留 `retention.access_days`=7 天），并每 5 分钟把当前小时和被标脏的小时重算进小时汇总（`access_hourly` / `access_source_hourly` 保留 31 天，窗口最长 30d；`access_target_seen` 记每个目标的首次 / 最近出现，最近出现超 90 天才删；幂等；agent 断网后补传的旧记录在上报时把所在小时记进 `access_dirty`，下一轮补算；启动时补算缺口）。24h/7d/30d 的结果在进程内缓存：每轮汇总成功后整体作废；`summary` / `targets`（第一页）/ `sources` 最多缓存 `access_cache_secs`（默认 300）秒，单台机器的 `/machines/{id}/access` 与 `/machines/{id}/sources`（带最近原始记录、来源基线与异常）最多 30 秒；设 0 关闭。全部为只读 GET，读权限即可。

公共筛选参数（空串等同没给，非法值返回 400）：`window=1h|24h|7d|30d`（默认 24h，events 默认 7d）、`team=<团队id>`、`node=<节点id>`、`search=`（大小写不敏感的子串，`%` `_` 按字面）、`action=all|blocked|routed`、`kind=all|domain|ip`。

- **窗口口径**：`1h` 直接查原始记录；`24h` 及以上查小时汇总，起点对齐到整点（`window_start` 返回实际起点），最新数据最多晚 5 分钟。
- **随机子域名合并**：注册域名之下的标签含 UUID、或 ≥8 位纯十六进制且 ≥2 位数字、或 ≥20 位字母数字且 ≥4 位数字，则整条合并为 `*.<注册域名>`（注册域名取最后两段，倒数第二段是 com/net/org/gov/edu/co 且最后一段是两字母国家码时取最后三段）。`events` 返回的是未合并的原始目标。
- **处理结果**：`action=blocked` 时 `conns` 即被阻止数；`action=routed` 时 `conns` 为去掉被阻止后的数，`blocked` 为 0。
- **直出入口**：按入口的节点端归属，节点就是线路机自己，`node_id` 是线路机 id、`name` 是「线路机名-直出」，`team` 为 null。
- **来源数**：1h 窗口精确去重；24h 以上是「各小时去重数的最大值」，为偏小的近似（节点级的 `sources` 是精确去重，24h 以上带 `search` / `kind` / `action` 筛选时为 null，见下）。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/access/summary?…` | `{kpi:{active_nodes,conns,targets,blocked}, nodes:[{node_id,name,team,conns,targets,domains,sources,blocked,last_ts}], window_start}`。`targets` 是各节点分别去重后相加；节点按连接数降序。节点的 `sources` 是去重来源 IP 数，跟随全部筛选（和同行的 `conns` 同一批连接）：`1h` 在原始记录上逐条带上 `search`（目标或来源 IP）、`kind`、`action`；`24h` 及以上来自来源小时表，没有目标维度，带了 `search`、`kind`、`action`（非 all）任一筛选时为 `null`（界面显示「—」） |
| GET | `/access/targets?…&sort=conns\|last\|first&limit=100&offset=0` | `{items:[{node_id,name,dest_host,kind,conns,sources,blocked,first_seen,last_seen}], next_offset, window_start}`；`limit` 1–500，`offset` ≤ 100000，满页才给 `next_offset`；`first_seen` 是该节点历史上第一次出现（不限窗口）。不单独回总数：同一筛选下 `/access/summary` 的 `kpi.targets` 就是这张表的总行数（同一个聚合） |
| GET | `/access/sources?…&limit=100` | `{items:[{node_id,name,src_ip,conns,last_ts}], window_start}`，按 (节点, 来源 IP) 连接数降序；`search` 在这里匹配来源 IP；`limit` ≤ 500 |
| GET | `/access/events?node=&team=&search=&action=&kind=&window=&limit=100&before=<游标>` | `{items:[{ts,node_id,name,src_ip,dest_host,dest_port,network,blocked}], next_before}`，按 (时间, 记录 id) 倒序；`next_before` 是不透明游标（满页才给），下一页把它原样放进 `before`（要 URL 编码）；同一时刻的多条记录不会被跳过；格式不对 400。`search` 同时匹配目标和来源 IP；只有最近 7 天 |
| GET | `/access/new-targets?window=24h&node=…&limit=200&offset=0` | `{items:[{node_id,name,dest_host,kind,first_seen,last_seen,total}], total, next_offset, window_start}`：顶层 `total` 是符合筛选的目标总数，`next_offset` 只在后面还有时给出；窗口内第一次出现的目标，按首次出现倒序；`total` 为累计连接数（汇总保留期内）。这是旧系统「新域名」的替代，**不产生告警** |
| GET | `/machines/{id}/access?window=24h` | `{window_start, top_domains:[{dest_host,kind,conns,blocked,sources}]（30）, top_sources:[{node_id,name,src_ip,conns,last_ts}]（20）, recent:[事件]（最近 20 条）, hourly:[{hour,conns,blocked}]（缺的小时补 0；至少 24 小时）}`。线路机只算经它的流量（它名下的入口，含已移除的；同一节点经别的线路机的流量不算），节点算它自己全部入口的；机器不存在 404 |

## 来源 IP 画像（IP 情报）
离线库在 fleet-server 进程内查询（ip2region v4/v6、DB-IP City/ASN Lite、iptoasn，mmap），**只查本地，绝不把 IP 发往在线服务**。库由 `deploy/ipintel/fetch-dbs.sh`（systemd timer `fleet-ipintel-fetch` 每月一次）下载到 `[ipintel] db_dir`（默认 `/var/lib/fleet/ipintel`），文件 mtime 变化后 5 分钟内自动热重载。库缺失或不全时降级：能查多少查多少，查不到的字段为 null，接口不报错。ASN 分类清单内置在 `crates/fleet-ipintel/data/asn-lists.toml`，可用 `/etc/fleet/ipintel-overrides.toml`（同样的 `[[vpn]] asn=[…] name="…"` 写法，`name=""` 表示删除）覆盖或追加。线路机和节点的 `primary_ip` 自动算「内部地址」（`[ipintel] internal_ips` 可追加运维出口）。全部为只读 GET，读权限即可。

画像摘要 `intel`：`{isp, asn, as_org, country, province, city, place, conn_type(residential|mobile|datacenter|vpn|unknown), conn_label, confidence(high|medium|low), reason, internal(我方机器名|null), private}`。`/access/sources` 与 `/machines/{id}/access` 的来源条目（`top_sources`）都带 `intel`，查不到为 null。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/ipintel/{ip}` | 单个地址的完整画像（`isp asn as_org country region city lat lon type type_word type_confidence type_reason type_alt flags sources dbs{四库原始结果}`）加 `degraded`（库没加载全）、`loaded`。非法 IP 400 |
| GET | `/ipintel?ips=a,b,c` | ≤200 个，`{items:[{ip, intel}|{ip, intel:null, error}], loaded, degraded}`；不写缓存 |
| GET | `/ipintel/status` | `{enabled, installed, loaded, expected:5, db_dir, version, files:[{id, loaded, bytes, mtime, build, entries}], errors, lists（各清单条目数）, overrides{path,used,error}, cache_rows, hint}`；未安装时 `hint` 提示运行 `lab/install-ipintel.sh` |
| GET | `/machines/{id}/sources?window=24h\|7d\|30d&limit=50` | 节点的来源画像（线路机：分布与 top 来源只算经它的流量；`known` 基线与 `anomalies` 按节点记，取它名下入口的节点）：`distribution.{isp,city,province,country,conn_type}[{value,label,conns,sources,pct}]`（只统计外部来源；我方机器和非公网地址分别计入 `internal_conns` / `private_conns`）、`top_sources[{src_ip,conns,last_ts,intel}]`、`known.{isp,city,conn_type}[{value,label,first_seen,last_seen,conns,anomaly}]`（已学基线）、`learning{started_at,ends_at,learn_days,done,remaining_hours}`、`anomalies[]`（进行中的 ipintel 告警）、`empty`、`degraded`、`unresolved_sources` |

**异常告警**（warning，进 incident 表并走通知渠道）：节点第一条来源记录起 `learn_days`（默认 7）天内只学习；之后某节点出现新的运营商 → `ipintel_new_isp`，新的城市 → `ipintel_new_city`，某 (节点, 来源 IP) 第一次出现且为机房/VPN → `ipintel_datacenter_source`。跳过非公网地址和我方机器。一次性异常最后一次出现 `alert_ttl_hours`（默认 24）小时后自动恢复，不发恢复通知；同一个值恢复后不会再次告警。后台每 5 分钟从 `access_source_hourly` 增量维护 `node_source_seen`（水位在 `ipintel_state`；水位之前的小时若被补算，连接数差值经 `access_source_late` 补记，首次扫描按 7 天一页）。

## 生命周期操作：换线路机、搬空线路机、恢复已退役的机器
都是作业（可续跑、幂等、留日志），接口只做校验、建作业、写审计。

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/machines/{id}/move` | `{to_relay_id, from_relay_id?}` → `{job_id, name, from, to, share_changed:true, note}`。节点（vps / proxy）必须 active 且恰有一个 active 入口（有多个时用 `from_relay_id` 指定换掉哪条）；目标线路机必须 active、不同于当前、节点在它上面没有未移除入口；节点有进行中的作业时 409。写审计 `entry.move` |
| POST | `/relays/{id}/evacuate` | `{to_relay_id}` → `{from, to, jobs:[{machine_id,name,job_id}], skipped:[{machine_id,name,reason}], share_changed, note}`。为该线路机上每个在用入口的节点各建一个 move_entry 作业；暂时不能搬的（入口停用/还在应用中、节点有别的作业、目标上已有入口…）列入 `skipped`，不影响其它节点。线路机自己的直出入口不搬、也不列入 `skipped`（退役线路机时随之摘除；退役守卫只数节点入口）。写审计 `relay.evacuate` 与每个节点一条 `entry.move` |
| GET | `/machines/{id}/restore?team_id=&number=&label=&probe=` | 恢复预检（只对已退役机器）。见下 |
| POST | `/machines/{id}/restore` | 提交恢复 → `{job_id, machine_id, name, old_name, number}`；输入不对 400（编号不在 1–999、指定的团队不存在）；不满足条件 409（IP 被占用、编号被占、线路机同名、机器不是 retired）。写审计 `machine.restore` |

**换线路机作业 `move_entry`**（目标 = 节点，要求节点 active）步骤：`attach_new`（目标线路机建新入口：新端口、新 client_uuid / short_id，pending）→ `firewall_widen`（vps：节点 nft 放行名单加入新线路机 IP；proxy 跳过）→ `wait_applied_new` → `verify_new`（经新入口的出口探测通过）→ `detach_old`（旧入口 removing）→ `wait_detached_old` → `firewall_narrow`（节点在旧线路机上不再有入口时，把旧线路机 IP 从放行名单拿掉）。新入口验收通过之前旧入口不动，节点始终至少有一个可用入口。
- **取消**：`detach_old` 之前都能取消（`cancellable` 为 true）；补偿 = 摘除新入口、恢复防火墙，旧入口保持不动。开始摘旧入口之后不能取消（会让节点没有入口），失败了只能重试到完成；取消赶在摘除之后到达时保留新入口。
- **分享链接会变**：新入口有新的端口和凭据。`GET /jobs/{id}` 对 move_entry 多返回 `result:{new_entry_id, old_entry_id, from_relay_id, to_relay_id, share_changed:true, note}`；用 `GET /entries/{new_entry_id}/share` 取新链接（需要 secrets 权限）。作业日志里也有「客户端需要换成新的分享链接」。
- 旧线路机不在线时 `wait_detached_old` 会失败，线路机恢复后重试。
- 线路机退役守卫的错误提示里指向「搬空线路机」。

**恢复作业 `restore_vps` / `restore_relay` / `restore_proxy`**（目标 = 机器，要求 provisioning）：机器保持原 id（历史、账单、审计接上），lifecycle retired → provisioning，步骤与同类型上架完全相同（vps：preflight、bootstrap、converge、agent、wait_report、attach、wait_applied、verify、activate；relay：…、converge_relay、…；proxy：只有 attach、wait_applied、verify、activate）。旧入口已 removed，不复活，`attach` 建新入口。成功后 `retired_at` 清空，`lifecycle_reason` 记「已恢复：<原因>」，订阅保留。取消（或失败后取消）：机器回到 retired（团队、编号、退役原因回到恢复之前），摘掉已建的入口、撤销令牌、删出站秘密，团队编号压缩。

预检 `GET /machines/{id}/restore` 返回：`{id, kind, lifecycle, ip, old_name, name（恢复后的名字）, team:{id,name}|null, number, label, restorable, blockers[], warnings[], ip_conflict:{machine_id,name,lifecycle}|null, last_relay:{id,name}|null, retired_reason, ssh:{reachable,key_login:ok|denied|error|unreachable,host_key?}|null, needs:{relay,upstream,password:required|optional|no}}`。编号默认取团队里最小的空号（编号连续时就是 N+1），可用 `number` 指定空号；线路机按 label 唯一约束检查，原名被占用时用 `label` 改名。`ssh` 是服务端用中控密钥试一次登录（临时 known_hosts，不碰正式的）的结果，`probe=false` 跳过；试登录和 `POST` 恢复一样要 `write` scope（只读密钥 403，可加 `probe=false`），并写审计 `restore.probe`；`needs.password=required` 表示中控密钥被拒、必须给 root 密码。

`POST` 请求体：`{team_id?, number?, label?, relay_id?（节点要接入的在用线路机，节点必填）, login?:{password}（只用来推一次中控公钥，用完即丢）, upstream?:{protocol,host,port,username?,password?}（proxy 必填：退役时出站参数已删除）, ss_port?（vps，公网（NAT 映射）端口，默认 8388；节点上 ssserver 固定监听 8388，非 NAT 机不要填；vps 恢复时重新生成 SS 密码）, reason?, reset_host_key?（机器重装过，允许预检覆盖已记录的主机指纹；退役时已忘记指纹，所以默认是重新记录）}`。

**同机 IP 节点的恢复**（作业 `restore_ip`，步骤同 `provision_ip`）：宿主机必须在用，否则预检 `restorable=false`、blockers 里有「宿主机「名字」现在是「…」：同机 IP 节点要等宿主机在用才能恢复」，`POST` 409。预检不试 SSH（`ssh` 为 null、`needs.password` 为 `"no"`）；`login`、`ss_port`、`reset_host_key` 不适用（忽略）；SS 端口在宿主机上重新分配（原来的可能已给了别的同机节点），SS 密码重新生成。

## 访问策略（域名黑白名单）
按团队或节点设置：有效策略 = 节点策略（`mode≠inherit`）> 团队策略 > 默认 `observe`。模式：`observe`（不产生规则）、`allowlist`（只放行名单内域名，其余阻止）、`denylist`（阻止名单内域名，其余放行）、`blocked`（暂停该节点所有新连接）、`inherit`（仅节点：跟随团队）。`allowlist`/`denylist` 至少一个域名。GET 需 `read`，写操作需 `write` 并写审计；预览是 POST 但无副作用（也需 `write`）。

**域名规范化**：逗号 / 中文逗号 / 顿号 / 分号 / 空白分隔，小写，去尾点；只接受 ASCII（国际化域名填 `xn--`）；每份 ≤300 个；必须含点、每标签 ≤63 且只含字母数字连字符（不以连字符开头结尾）、总长 ≤253；不能是 IP，不能带 URL、端口、通配符 `*`、正则或 `domain:` 之类前缀。`=example.com` 表示精确匹配（Xray `full:`），否则含子域（`domain:`）。保存时排序去重。错误信息中文并点名是哪一个域名（400）。

**草稿与发布**：`PUT` 只改草稿（`policy` 表的 `draft` + `revision` 乐观锁，`expected_revision` 与当前不符 409「草稿已被修改，请重新加载」；还没有记录的范围 revision 为 0）。草稿与生效值相同时草稿自动清除。`draft_hash` = 所有草稿规范化 JSON 加当前生效版本号（最新版本的 `seq`）的 sha256（十六进制），预览返回，发布 / 丢弃必须带上且一致（否则 409「草稿或生效版本已变化，请重新预览」：预览后草稿被改、或有人发布 / 回滚过，都要重新预览）。发布把所有草稿生效成新版本（`policy_version` 存完整快照），对**编译结果有变化**的节点所在的线路机代数 +1（每台只 +1 一次）。回滚 = 把旧版本快照作为新版本发布。版本状态由各线路机的 `applied_generation` 推导：`applying`（有线路机还没应用）→ `applied`；线路机回报应用失败且未应用 → `failed`；已退役的线路机不再等待（`gone`）。

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
