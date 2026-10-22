# fleet-server API v1

- 前缀 `/api/v1`。成功 `{"data": …}`；失败 `{"error":{"code","message"}}`（message 是给人看的中文）。
- 鉴权：会话 Cookie `fleet_session`（`POST /auth/login` 设置）；或 `Authorization: Bearer <api key>`（见下「API 密钥与 scope」）。
- 会话的写请求（POST/PUT/PATCH/DELETE）必须带头 `X-Fleet-Request: 1`（防 CSRF）；API 密钥不是浏览器，不要求这个头。
- 响应头：所有响应带 `X-Content-Type-Options: nosniff`、`Referrer-Policy: same-origin`、`X-Frame-Options: DENY`；网页（`/api`、`/agent`、`/echo` 以外）另带 CSP（`script-src 'self'`，不许内联脚本与外部资源）；`public_url` 是 https 时带 HSTS。接口没自己设 `Cache-Control` 的一律 `no-store`。
- 时间：RFC 3339 字符串；日期 `YYYY-MM-DD`。金额：整数分（`price_cents`）。流量：字节。速率：bit/s。
- 乐观锁：改机器要带 `revision`，不一致 → 409。
- 状态码：输入不对 400（请求体、路径参数如坏 uuid、查询参数解析失败也是 JSON 400；枚举值不认识；引用的对象不存在，如 `provider_id` 指向不存在的供应商）；对象不存在 404；状态冲突 409（重复、被其他数据引用不能删、状态不允许）。
- 分页列表（`/jobs`、`/incidents`）：`limit`（1–500）+ `offset`（从 0 起），返回 `{data:{items, total, next_offset}}`；`total` 是同一筛选下的总条数，`next_offset` 只在后面还有数据时给出（否则 null）；`offset` 越过末尾得到空的 `items`，`total` 照给。

## 认证
| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/auth/login` | `{username,password,totp?}` → 设 Cookie，`{data:{username}}`。限速（15 分钟窗口；来源 = IPv4 地址或 IPv6 /64）：同一用户名从同一来源失败 5 次、同一来源失败 20 次（不分用户名）、同一用户名失败 100 次（不分来源；该用户 30 天内登录成功过的来源不受这一条）→ 429；别处的失败不影响从自己的来源登录，成功登录只清本来源的失败记录（`error.code="rate_limited"`，`error.detail.retry_after_s`，message 含剩余等待时间）。成功、失败、登出都写审计。**两步验证**：用户开了 TOTP 时，密码对但没给 `totp` → 401 `totp_required`（照样计一次失败、不写审计，成功登录后随本来源的记录清掉；不计的话知道密码的人能不限次数地占用算密码哈希的名额。密码错时一律 `bad_credentials`，猜密码的人看不出开没开）；`totp` 是 6 位验证码（前后各容忍 30 秒，同一个时间步只收一次）或一个恢复码（用掉即作废，写审计 `auth.totp_recovery_used`），不对或重放 → 401 `totp_invalid`，算一次登录失败。验证码输错另有上限：同一用户名 1 小时内 10 次（登录与下面的再次校验合计），不分来源、登录成功过的来源也不例外 → 429 |
| POST | `/auth/logout` | 这个会话的实时流连接马上以 `4401` 关闭 |
| GET | `/auth/me` | `{data:{username, kind:"session"\|"api_key", scopes:[...]}}`；未登录 401 |

## 账户安全（两步验证、登录会话、改密码）
只允许会话用户，用 API 密钥调用一律 403（fleetctl 走 API 密钥，不受两步验证影响）。要再输一次密码 / 验证码的接口（开启与停用两步验证、重新生成恢复码、退出其它设备、改密码、新建 API 密钥）与登录共用失败限速（同一用户名 + 来源；验证码输错另计，见登录），输错写审计 `auth.verify_failed`：偷到会话 Cookie 的人不能借这些接口绑自己的认证器、把本人踢下线或铸一把密钥。撤销会话（登出、撤销、退出其它设备、改密码、`fleet-server user-set`）与撤销密钥后，实时流里对应的连接马上以 `4401` 关闭。输错当前密码回 401 `bad_password`、验证码不对回 401 `totp_invalid`、该给验证码没给回 401 `totp_required`——只有 `code="unauthorized"` 的 401 才表示会话失效。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/account/totp` | `{enabled, recovery_codes_left}` |
| POST | `/account/totp/setup` | `{password}`（当前密码）→ 生成待确认的密钥（20 字节，加密保存，10 分钟内有效；再调一次换一把）→ `{secret(base32), otpauth_url, qr_svg}`；已开启 409 |
| POST | `/account/totp/enable` | `{code}`（6 位，核对待确认的密钥）→ 开启并返回 `{recovery_codes:[10 个 xxxx-xxxx-xxxx]}`，**只返回这一次**（库里只存 sha256）；码不对 400 `totp_invalid`；审计 `auth.totp_enabled` |
| POST | `/account/totp/disable` | `{password, code}`（code 可以是恢复码）→ 停用，恢复码全部作废；审计 `auth.totp_disabled` |
| POST | `/account/totp/recovery-codes` | `{code}`（验证码或恢复码）→ 新的一组 `{recovery_codes}`，旧的全部作废；审计 `auth.totp_recovery_regenerated` |
| GET | `/account/sessions` | 当前用户未过期的会话 `[{id, created_at, last_seen_at, expires_at, ip, user_agent, current}]`，本设备排第一；`id` 是会话哈希前 8 字节的十六进制（推不出 Cookie），`user_agent` 最多 200 字符，`last_seen_at` 一分钟内不重复更新 |
| DELETE | `/account/sessions/{id}` | 撤销一个会话（不存在 404）→ `{ok, current}`；审计 `auth.session_revoked` |
| POST | `/account/sessions/revoke-others` | `{password, totp?}`（开了两步验证要 `totp`，或恢复码）→ 撤销本设备以外的全部会话 → `{revoked}`；审计 `auth.session_revoked` |
| POST | `/account/password` | `{current, new, totp?, revoke_api_keys?}`：新密码至少 12 个字符、不能等于用户名或当前密码；开了两步验证要 `totp`（或恢复码）。成功后撤销本设备以外的全部会话，清掉本来源以外「登录成功过的来源」（不再免于同一用户名 100 次的上限）；`revoke_api_keys: true` 同时撤销全部 API 密钥（默认不动：密钥不随改密码失效）→ `{ok, revoked_sessions, revoked_api_keys}`；审计 `auth.password_changed`。`fleet-server user-set` 清掉该用户全部会话与登录成功过的来源，`totp-reset` 也清掉登录成功过的来源 |

## API 密钥与 scope
密钥是 `flk_<8位十六进制前缀>_<随机串>`，库里只存 sha256 与前缀。每个请求按「方法 + 路径」需要一个 scope，密钥没有就 403（会话用户拥有全部）：

| scope | 允许 |
|---|---|
| `read` | 所有 GET，**不含**下面三类 |
| `write` | 所有非 GET 的业务写操作（不含 `/machines/{id}/exec`） |
| `secrets` | `GET /entries/{id}/share` 这类凭据 |
| `exec` | `POST /machines/{id}/exec`、`GET /machines/{id}/logs`、`GET /server/logs`（都是通过 SSH 或本机读取）；`POST /audit/ssh`（`fleetctl ssh` 登录前报的审计） |

scope 之间不互相包含：只有 `write` 的密钥不能 GET。`/api-keys` 的管理与网页终端（`/terminal/...`）只允许会话用户，用密钥调用一律 403。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api-keys` | `[{id,name,prefix,scopes,created_at,expires_at,last_used_at,revoked_at}]`（永不返回 token） |
| POST | `/api-keys` | `{name, scopes:["read",…], expires_days?: 1-3650, password, totp?}`（当前密码；开了两步验证还要 `totp`，或恢复码，失败同账户安全的再次校验）→ `{data:{id, token}}`，**token 只返回这一次**；写审计，实时流发 `api_keys`。`fleetctl keys create --user` 用登录的密码；命令行登录不支持两步验证，开了的账户到网页上建。名称 `central-local` 留给中控本机的 fleetctl（`fleet-server local-admin` 建，见 OPS.md §14），用它 400 |
| DELETE | `/api-keys/{id}` | 撤销（幂等）；之后用它的请求 401，用它连着的实时流马上以 `4401` 关闭 |

## 共用的枚举与常量
| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/meta` | 只在中控定义一次的枚举中文名、阈值、默认值与组织时区（服务端 `src/meta.rs`；一个会话里不变，网页启动时取一次，fleetctl 每条命令要打印中文名或时间时取一次）。`{org_timezone（fleet.toml 的 org_timezone，默认 Asia/Shanghai）, org_timezone_label（「北京时间」或 IANA 名，通知、审计、fleetctl 时间的括注）, org_utc_offset_s（组织时区现在的 UTC 偏移秒数，每小时刷新）, timezones:[…]（时区下拉的常用项；不在里面的照样能填，中控按 PG 时区库校验）, currencies:[…]（币种下拉的常用项；校验只要求 3 个字母）, cycles:[{key,label,months}], kinds, ip_node_label, lifecycles, healths, job_kinds, job_states, step_states, entry_states, traffic_rules, traffic_resets, due_states, policy_modes, rules（告警规则）: {key: 中文名}, severities:{key:{label,tone}}, thresholds:{traffic_quota_warn_pct, traffic_quota_over_pct, usage:{cpu,mem,disk,load:{warn,bad}}}（bad 与持续高负载告警同值；负载不告警）, defaults:{currency,cycle,remind_days,traffic_rule,traffic_reset,ss_method,ss_port}, terminal:{per_user, global, idle_s, max_age_s, max_message, max_dim, ticket_ttl_s, verify_ttl_s}（网页终端的上限，见「网页终端」）}`。叫法：`vps` =「VPS 节点」、`proxy` =「家宽代理」、`relay` =「线路机」，加在宿主机上的 vps 是「同机 IP 节点」；健康只用 正常 / 降级 / 故障 / 未知 / 维护。读权限 |

## 目录
| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/teams` | `[{id,name,sort_order,node_count}]` |
| POST | `/teams` | `{name,sort_order?}` |
| PATCH | `/teams/{id}` | `{name?,sort_order?}`（改团队名 = 该团队所有节点显示名一起变） |
| DELETE | `/teams/{id}` | 还有机器（含已退役的）或软路由时 409（写明还有几台）；没有才删，团队自己的访问规则一起删。审计 `team.delete` |
| GET | `/providers` | `[{id,name,code,panel_url,timezone,traffic_rule,traffic_reset,remind_days}]`；`traffic_rule` 是这家怎么计量流量额度（网卡本周期入、出总量）：`min` 取小（如 GOMAMI）/ `max` 取大 / `sum` 相加 / `out` 只算出站。供应商同时是名下机器订阅的**模板**：`timezone`（账单时区）、`traffic_reset`（流量周期：`month` 账单时区的自然月、每月 1 号起；`billing` 账单日，即每台机器到期日是几号就几号起，月份没有这一天按月末）、`remind_days`（到期前几天提醒，0–60）——订阅没单独设置这几项的跟随它，改了立即生效 |
| POST | `/providers` | `{name,code,panel_url?,timezone?,traffic_rule?,traffic_reset?,remind_days?}`；`traffic_rule` 不传 = `max`（不知道的供应商按取大，不会少算），`traffic_reset` 不传 = `month`，`remind_days` 不传 = 7，别的值 400；`panel_url` 须是 http(s) 地址（400）；`timezone` 不传 / null / 空串 = 存 null、跟随组织时区（`/meta` 的 org_timezone），否则须是 `at time zone` 认的时区（400）；名称或缩写重复 409 |
| PATCH | `/providers/{id}` | `{name?,code?,panel_url?,timezone?,traffic_rule?,traffic_reset?,remind_days?}`：没传的不改（`traffic_rule` 改了立即影响这家机器的 `quota.used`；`timezone` / `traffic_reset` / `remind_days` 改了立即影响没单独设置这几项的订阅）；`panel_url` null / 空串 = 清空；`timezone` null / 空串 = 改回跟随组织时区（null）。校验同 POST（`panel_url` 和库里现有的一样时不校验：加 http(s) 校验之前存下的旧值不挡改别的字段）。写审计 `provider.update` |
| DELETE | `/providers/{id}` | 还有套餐（含已归档的）或机器（含已退役的）时 409；没有才删。审计 `provider.delete` |
| GET | `/plans` | `[{id,provider_id,name,cpu_cores,mem_mb,disk_gb,bandwidth_mbps,traffic_gb,price_cents,currency,cycle,notes,archived_at,machine_count}]`。套餐是机器订阅的**模板**：`price_cents` / `currency` / `cycle`（每个计费周期的价格）与 `traffic_gb`（每个流量周期的额度，十进制 GB，null 或 0 = 不限）——用这个套餐的机器没单独设置的跟随它，改了立即生效。`machine_count`：在用（未退役）的机器数 |
| POST | `/plans` | 同上字段（不含 id/统计）。校验（PATCH 同，只看传了的）：`name` 1–32 字，`price_cents` ≥ 0，`currency` 3 个字母（存成大写，套餐、订阅、上架同一个校验），`cycle` 只能是 `/meta` 的 cycles，`notes` ≤ 2000 字，否则 400 |
| PATCH | `/plans/{id}` | 任意字段（没传的不改；规格类字段传 null = 清空）；`archived:true` 归档。机器的供应商就是套餐的：有机器（含已退役的）用着时改 `provider_id` 409（给新供应商另建套餐）。审计 `plan.update` 记套餐名、币种与改了的字段 `{name, currency, before:{…}, after:{…}}` |
| DELETE | `/plans/{id}` | 还有机器（含已退役的）用着时 409（不再上架的可以归档）；没有才删。审计 `plan.delete` |

## 机器
`MachineView`（列表与详情同形）：
```
{ id, kind: "relay"|"vps"|"proxy", name, team:{id,name}|null, number|null, label|null, ip, ssh_port,
  country|null, country_set|null, // 国家的 ISO 3166-1 两位代码（"US"），网页在名字旁显示国旗。country_set 是上架 / 修改时手动指定的；
                                // 没指定时 country 按 ip 查中控离线 IP 库，内网地址、家宽代理填的域名、库里查不到的为 null
  host_machine:{id,name}|null,  // 同机 IP 节点（kind 仍是 vps）：它所在的宿主机；它没有自己的 agent 和 SSH，下面的 agent、host 就是宿主机的，
                                // ssh_port 是宿主机的（只展示），provider 取宿主机的、plan 为 null；其它机器为 null
  ip_nodes:[{id,name}],         // 宿主机：它上面未退役的同机 IP 节点（按团队、编号）；其它机器为 []
  provider:{id,name}|null, provider_code|null, plan:{id,name,bandwidth_mbps,traffic_gb}|null, service_ref|null,
                                // provider：有套餐时是套餐的供应商（机器上存的那份在上架、改套餐时按套餐写），没有套餐取机器的
  lifecycle: "draft"|"provisioning"|"active"|"retiring"|"retired"|"failed", lifecycle_reason|null, note, revision,
  created_at, activated_at|null, retired_at|null,
  subscription: {price_cents,currency,cycle,due_date,billing_day,due_days,due_state,billing_tz,auto_renew,remind_days,traffic_gb,traffic_reset_day,overrides,monthly_cents,next_due,period_days}|null,
                                // due_state：ok / soon（到期前 remind_days 天内、不是自动续费）/ overdue（已过期），续费告警、总览、网页、fleetctl 都按它；
                                // monthly_cents：折成每月的费用（分，四舍五入）；next_due：到期日再推一个周期（登记续费的默认新到期日，落在账单日，小月按月末）；billing_day：账单日（每月几号，见续费）；
                                // period_days：当前这一期的天数（到期日往前推一个周期到到期日），网页算剩余价值
                                // 全是生效值（字段名同以前，旧调用方照用）：订阅只存到期日、自动续费和「单独设置」的项，其余跟随 sub_template——
                                // price_cents / currency / cycle / traffic_gb 跟随套餐，billing_tz / remind_days / traffic_reset_day 跟随供应商。
                                // overrides：单独设置了的字段名（如 ["price_cents"]），不在里面的就是跟随的。traffic_gb：0 / null = 不限。
                                // traffic_reset_day 总有值：单独设置的，或供应商 traffic_reset = billing 时是账单日（billing_day）、month 时 1
  sub_template: {price_cents|null, currency, cycle, billing_tz, remind_days, traffic_gb|null, traffic_reset},
                                // 订阅模板（没有订阅也给）：套餐的价格 / 币种 / 周期 / 流量（没有套餐时 price_cents、traffic_gb 为 null，币种周期 USD / monthly），
                                // 供应商的时区 / 提醒天数 / 重置方式（供应商没填时区或没有供应商时是组织时区，没有供应商时 7 / month）。网页「跟随套餐：2000 GB」就是它
  agent: {version,last_report_at,online,xray_running,cores,health,clock_skew_ms}|null,  // proxy 没有；cores 为 agent 报的可用核数（容器里是限额），没报过为 null
                                // online：120 秒内报过；中控收不到上报的时段（中控停机、库连不上）不算 agent 的沉默，恢复后重发还没到的头一两分钟不判离线
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
  quota: {limit_gb, limit_source, used, period_start, rule, base, base_at},  // 流量额度：生效的上限、周期已用（字节）与周期起点（日期）。
                                // limit_gb：生效的上限（十进制 GB），没有上限为 null；告警、网页、fleetctl 都按它，不各自再算。
                                // limit_source：subscription（订阅单独设置）/ plan（跟随套餐，没有订阅时也是套餐的）/
                                // host（同机 IP 节点没有自己的上限，与宿主机共用：宿主机的已用含它的流量）/ none（不限）。
                                // 有自己 agent 的机器（线路机、VPS 节点、宿主机）按网卡：本周期物理网卡入、出总量按供应商的 traffic_rule
                                // 取小 / 取大 / 相加 / 只取出站（没有供应商按 max），rule 即所用规则；宿主机的网卡也走了它的同机 IP 节点的流量。
                                // 家宽代理和同机 IP 节点没有自己的网卡读数：rule 为 null，used 是业务流量（下载+上传，含已移除入口）。
                                // base / base_at：本周期按供应商面板校准过（见 PUT subscription 的 traffic_used_gb）时的面板读数（字节）与校准时间，
                                // used = base + 校准时刻所在整点之后的用量（那一小时里校准之后的部分不计，至多差 1 小时；取小 / 取大按校准之后的入、出总量算）；
                                // 校准时间早于周期起点（已进入新周期）时不算，二者为 null。
                                // 有订阅：账单时区里今天或之前最近一次（生效的）重置日起（月份没有这一天按月末，如 31 号在 9 月是 30 号、2 月是 28 / 29 号），
                                // 从该日账单时区零点累计；没有订阅：sub_template.billing_tz 的本月。来自小时汇总表，每次汇总后更新。
  health: {status: "ok"|"degraded"|"down"|"unknown"|"maintenance"|"inactive", reasons:[{key,severity,text}]},
  active_job: {id,kind,state}|null,                    // 进行中的作业（queued/running/waiting/cancelling）
  failed_job: {id,kind,state}|null,                    // lifecycle = failed（上架失败）时：失败的上架/恢复作业，重试回到 provisioning，取消则补偿退役；
                                                       // lifecycle = retiring 且退役作业失败时：该 retire 作业（等重试）；其它情况 null
  open_incidents, maintenance,   // maintenance：同机 IP 节点在宿主机维护时也为 true
  relay: {public_host,generation,applied_generation,applied_at,apply_error,port_min,port_max}|null }
EntryView = { id, relay:{id,name}, node:{id,name}, direct, port, lifecycle, applied, apply_error,
  tcp:{ok,latency_ms,detail,ts,median_ms}|null, egress:{…}|null, traffic:{today_up,today_down,month_up,month_down} }
// direct：线路机直出入口——节点就是线路机自己（node.id = relay.id，node.name =「线路机名-直出」），出口是线路机自己的 IP；
//   只出现在线路机的 entries 里（一次）；没有 TCP 探测（tcp 恒为 null），出口不通时线路机自己报 egress_fail
// tcp：线路机 → 节点出站地址的 TCP 建连，latency_ms 是最近一次（新 agent 报内核量的往返时间，不含 SYN 重发等待），
//   median_ms 是近 1 小时成功样本的中位数（卡片读数）；egress：经入口访问 /echo 核对出口 IP，只判通不通、IP 对不对，
//   latency_ms 是这次出口验证的耗时（经节点去中控），不是线路延迟，median_ms 恒为 null
```
| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/machines?scope=live\|retired\|all` | 默认 live（未退役）。排序：线路机在前，然后按团队、编号。加 `view=brief` 只返回 `{id, kind, name, label, ip, lifecycle, host_id, team:{id,name}\|null}`（fleetctl 按名字找机器用；`host_id` 非 null 的是同机 IP 节点） |
| GET | `/machines/{id}` | 单台 |
| PATCH | `/machines/{id}` | `{revision, team_id?, number?, label?, note?, provider_id?, plan_id?, service_ref?, country?}`。country 是 ISO 3166-1 两位代码（不在表里 400，「UK」提示英国是 GB），null 或空串 = 改回按 IP 自动。改团队/编号 = 改名：线路机备注随后自动同步，不换令牌、不断连接。有套餐时供应商就是套餐的：只改了套餐的，供应商跟着换；同时给了另一个供应商 400 |
| GET | `/machines/{id}/series?metric=host\|traffic\|probe&range=1h\|6h\|24h\|7d\|30d` | 时序。≤24h 用原始点（1 分钟聚合），更长用小时汇总。同机 IP 节点的 `metric=host` 是宿主机的曲线 |
| GET | `/machines/{id}/availability?days=30` | 近 N 天（`days` 1–90，默认 30，越界 400）每天的探测成功率：`{data:{days, items:[{date,tcp_total,tcp_ok,egress_total,egress_ok}], tcp_pct, egress_pct}}`。探测小时汇总按这台机器生效的账单时区（没有订阅按 `sub_template.billing_tz`）分到日历天（一个小时桶按起点归天），`items` 旧→新共 `days` 项、最后一项是今天（还没过完），没有探测的天各项为 0；`tcp_pct` / `egress_pct` 是整段的成功率（0–100），没有探测为 null。入口同 `series`：节点取自己的，线路机取挂在它上面的（含已移除的）。机器不存在 404 |
| GET | `/machines/{id}/retire` | 退役预览：`{data:{name, renames:[{machine_id,old_name,new_name}], ip_nodes:[{id,name}]}}`——编号保持连续，退役后同团队后面的节点依次前移；`ip_nodes` 是会一并退役的同机 IP 节点（宿主机才有，还没开始退役的），`renames` 按所有受影响的团队算 |
| POST | `/machines/{id}/retire` | `{confirm: <显示名>, reason?, cleanup?: true}` → `{data:{job_id, ip_node_jobs:[{machine_id,name,job_id}]}}`。退役完成时同团队编号自动压缩成 01..N（写审计）。线路机上还有节点入口时 409（提示先搬空）；这台（或它的同机 IP 节点）有作业正在执行一步（租约未过期）时 409，先取消并等它停下（升级 agent 不能取消，等它做完）；`reason` 最多 2000 字；它自己的直出入口不算，由退役作业的「摘除入口」一并摘掉。**宿主机**：它上面还没开始退役的同机 IP 节点在同一事务里一并置为退役中（`lifecycle_reason`「随宿主机「名字」退役」），作废它们进行中 / 失败的作业、关掉它们的告警，各建一个退役作业（`cleanup: false`：只摘入口，不登录清理——宿主机的清理连同所有实例一起删），列在 `ip_node_jobs`（没有为 []），各写一条审计 `machine.retire`。单独退役同机 IP 节点只删它自己的实例 |
| POST | `/machines/{id}/purge` | 仅已退役。`{confirm: <显示名>}` → 作业；彻底删除机器与遥测，保留审计。宿主机上还有同机 IP 节点（含已退役的）没删时 409：「这台机器上还有同机 IP 节点没有彻底删除（含已退役的）：…；先删它们」。它的入口近 35 天有小时流量、且入口另一端的机器未退役时 409：「这台机器的入口在 YYYY-MM-DD 前仍计入其他机器的流量统计，届时再彻底删除」（作业执行时再查一次） |
| POST | `/machines/{id}/upgrade-agent` | 推送中控当前版本的 agent 并重启（作业 `upgrade_agent`；令牌顺带轮换） → `{data:{machine_id,name,kind,job_id,after:null}}`。线路机顺带收敛 fleet-xray 单元（变了只 daemon-reload，不重启 Xray，等它下次重启才生效）与访问日志轮转（配置 + 每小时的 fleet-xray-logrotate.timer），这两样没装上只记警告。**回滚**：推送时机器上的旧程序留成同目录的 `/usr/local/bin/fleet-agent.prev`（硬链接，新程序原子替换；上一次留的被覆盖，成功后也留着）；重启后新版本起不来（隔 3 秒主进程换了，或不是 active），或 2 分钟内没有用新令牌、以新版本号上报，就把 .prev 换回去并重启，作业失败，错误写明「新版本 X 起不来（…）/ 重启后 120 秒没有上报，已回滚到 <旧版本>」；机器上没有 .prev 或换回去也没起来时写明需要人工处理。程序和 dist 里的相同（推送跳过）时不回滚。**线路机**另要新版本自己报的 Xray 状态做到：Xray 在跑、已应用的配置追上升级开始时的期望代数、没有应用错误（`relay.apply_error` 与 agent 自报的 `last_apply_error` 都算）；只查升级前（推送之前读的 agent 最近状态）就做到的项，记在作业参数 `xray_checks`、推送那一步输出 `xray_checks`。2 分钟做不到同样换回并失败，错误「新版本 X 重启后 120 秒线路机检查没通过（Xray 没在跑；已应用的配置代数 2，没到 3；应用配置出错：…），已回滚到 <旧版本>」（`error_kind` remote）。「新版本自己报的」：中控入库时给 `agent_state.info.xray` 记上报它的 agent 版本（`agent_version`）——重启后新进程最先补发旧进程退出时落盘的报告，之后几份不带 Xray 状态，按上报时间分不出来。同机 IP 节点 409（agent 是宿主机的，升级宿主机） |
| POST | `/agents/upgrade` | 所有在用线路机与 VPS 节点（不含同机 IP 节点）各建一个 `upgrade_agent` 作业（有进行中作业的跳过；每台单独提交，一台冲突不影响其他台），但不一起跑：先升一台做**金丝雀**（有线路机先挑线路机；同类里在用入口少的在前，一样少按名字），它成功后其余线路机**一台接一台**，线路机都成功后 VPS 节点**并发**（作业引擎的并发上限 `job_workers`）。编排记在作业参数 `after`（要等的作业 id）：那个作业结束之前这个作业领取不到（一直是 queued）；它在推送之后失败了（新版本起不来、没上报、线路机检查没通过）或被取消，这个作业一步都不做、直接取消，`error` 写明原因（如「金丝雀「线路机2」升级失败，其余机器不再升级」「「线路机3」升级失败，后面的机器不再升级」「「线路机3」的升级没有完成（已取消），后面的机器不再升级」，再往后的沿用同一个原因），日志一条「没有开始：…」。它在推送之前就失败的（SSH 连不上、登录被拒、推送失败：机器上的程序没换过，与新版本无关）不算，这个作业照常开始——agent 步骤每次尝试先记作业参数 `pushed=false`，要用 dist 里的程序重启 agent 之前置 true，编排据此判断（没有记的按推送之后算）。排队中的作业可以取消（`POST /jobs/{id}/cancel`），等它的跟着取消。VPS 节点之间互不等待，一台失败不影响别的。发起时把 dist 里 agent 的 sha256 与版本号记进各作业参数 `rollout:{sha256,version}`，每个作业开始前、推送时各核对一次：中途重新部署过中控（dist 换了）就不推，正要推送的失败、没开始的直接取消，`error` 都是「中控上的 agent 在升级过程中换了版本（发起时是 X），请重新发起全部升级」。dist 里没有 agent 程序或读不出版本号（`--version`）时 409、不建作业 → `{data:{jobs:[{machine_id,name,kind,job_id,after}], skipped:[{machine_id,name,reason}], order}}`：`jobs` 按执行顺序（第一个是金丝雀，`after` 为 null）；`order` 是给人看的顺序说明，如「先升「线路机3」做金丝雀，成功后其余 2 台线路机逐台升级，线路机都成功后 40 台 VPS 节点并发升级；新版本在任何一台线路机上出问题，没开始的都会取消」（只有一台时「只有「X」一台」，一台都没建时「没有要升级的机器」） |
| PUT | `/machines/{id}/subscription` | `{due_date, price_cents?,currency?,cycle?,billing_tz?,remind_days?,traffic_gb?,traffic_reset_day?, auto_renew?, traffic_used_gb?}`。只有 `due_date` 必填。`price_cents` … `traffic_reset_day` 是**单独设置**，整份替换：没传或 null = 跟随模板（见 `sub_template`），给了值 = 这台单独设置；和模板相同的值也按跟随存（旧客户端整份带上生效值不会把套餐的值抄进来）。没有套餐的机器价格没有可跟随的，必须给 `price_cents`（400）。校验：`price_cents` / `traffic_gb` ≥ 0、`cycle` 四选一、`currency` 3 个字母（存成大写）、`remind_days` 0–60、`traffic_reset_day` 1–31、`billing_tz` 须是 `at time zone` 认的时区，否则 400。`auto_renew` 没传 = 不改（新建为否）。账单日（`billing_day`）：`due_date` 落在现有账单日上的（原样带回的到期日、小月的月末，如 31 号账单日的 2-28）不动，否则（或新建）记成 `due_date` 是几号。`traffic_gb` 是供应商的十进制 GB（10⁹ 字节，0 = 不限）。`traffic_used_gb`：按供应商面板校准本周期已用（GB，可带小数，如 901.6；负数 400），记成字节与当前时间（`quota.base` / `base_at`）；不传 = 不动原有校准（整份替换只针对其余字段），null = 清掉。写审计 `subscription.set` |
| POST | `/machines/{id}/renewals` | `{new_due?, cycles?, amount_cents?, note?}`，`new_due` 与 `cycles` 给一个（都给、都不给、`cycles` 不在 1–120 时 400）：`cycles` = 中控按订阅生效的 `cycle` 从当前到期日推几个周期，落在账单日（`billing_day`，目标月没有这一天时落到月末；账单日不变，所以 31 号的 1-31 → 2-28 → 3-31，不会漂成 3-28；fleetctl `renew --cycles`）；`new_due` 直接给新到期日（网页对话框默认填 `subscription.next_due`），账单日同改订阅：落在现有账单日上的不动，否则改成它是几号。新到期日要晚于当前的。只顺延到期日，单独设置照旧 → `{id, old_due, new_due}` |
| GET | `/machines/{id}/renewals` | |
| GET | `/teams/{id}/renumber` | 预览：`{data:{changes:[{machine_id,old_name,new_name}],token}}` |
| POST | `/teams/{id}/renumber` | `{token}`（预览返回的，防止预览后数据变了）→ 一次生效 |

## 上架
| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/provision/check` | 校验（不落库）：`{items:[ProvisionItem]}` → `{data:{items:[{ok,errors:[...],name,number,ss_port}]}}`（`ss_port`：同机 IP 节点暂定的 SS 端口，其它为 null）。还会探测 SSH 端口（同一批最多同时 8 个；连上后 5 秒内要读到 `SSH-` 开头的版本行才算通，不通时写明「连接被拒绝」「3 秒没有响应」「没有响应 SSH 握手」还是「不是 SSH 服务」）；同机 IP 节点登录宿主机核对这个 IP 在不在它的网卡上（同上架作业第一步；每台宿主机只登录一次，算一次探测）；整个检查最多 30 秒，到点没查完的条目报「检查超时」；回环、链路本地、未指定、组播地址直接报错不探测；按操作人限速（突发 300 次探测、每秒补 1 次，超了回 429 `rate_limited`） |
| POST | `/provision` | 同样的 body，全部通过才创建：每台建机器（provisioning）+ 一个作业 → `{data:{jobs:[{machine_id,job_id,name}]}}` |

```
ProvisionItem = {
  kind: "vps"|"proxy"|"relay",
  ip, ssh_port?(22), team_id (节点), number?(空=自动取最小空号), label (线路机),
  provider_id?, plan_id?, service_ref?, note?, country?(ISO 两位代码，空 = 按 IP 自动),
  relay_id (节点要挂的线路机),
  // vps / relay：首次登录方式（只在作业里用一次，成功后丢弃）
  login?: {user?:"root", password?: "...", }       // 不填则用中控的密钥
  // proxy：上游
  upstream?: {protocol:"socks5"|"http", host, port, username?, password?},
  // vps：线路机连节点 SS 用的公网端口（NAT 机填供应商映射到节点 8388 的那个公网端口；非 NAT 机不要填）。
  // 节点上 ssserver 与防火墙固定在 8388，这个值只写进线路机出站
  ss_port?: 8388,
  // 订阅只存到期日和给了的单独设置（同 PUT subscription），其余跟随套餐 / 供应商，不再抄一份。
  // 选了套餐（同机 IP 节点不算）必须给 subscription（只要 due_date），否则校验报「选了套餐要填到期日」；
  // 没有套餐时给了 subscription 就要给 price_cents（「没有套餐：订阅要填费用」）。provider_id 和套餐的供应商不一致报错，不给取套餐的
  subscription?: {due_date, price_cents?, currency?, cycle?, billing_tz?, remind_days?, traffic_gb?, traffic_reset_day?},
  // 同机 IP 节点（kind 必须是 vps）：加在这台 VPS 节点（宿主机）上，ip 是它自己的出口 IP（要已直接配在宿主机网卡上，EIP / NAT 映射的不行）。
  // 宿主机必须存在、是在用的普通 VPS 节点（不是线路机、家宽代理或同机 IP 节点）。这时 ssh_port / login / ss_port / provider_id / plan_id / reset_host_key
  // 不适用（忽略）：SSH 端口与供应商取宿主机的、不带套餐、不收 root 密码；SS 端口由中控在宿主机上分配（8389 起，同批依次递增，跳过宿主机上未退役同机节点已占的）。
  // 作业是 provision_ip；费用默认不记（subscription 照常可给，要带 price_cents）
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
| GET | `/jobs?state=active\|done\|all\|failed&target_id=&kind=&limit=100&offset=0` | `kind`：`provision`（各类上架）、`restore`（各类恢复）或作业类型原值（`attach_entry`、`move_entry`、`retire`、`purge`、`upgrade_agent`、`provision_vps`…），别的值 400，与 `state` 等一起生效。分页：`{items:[Job（不含 steps、cancellable、result，另有 step_title）], total, next_offset}`，按创建先后倒序（`id desc`） |
| GET | `/jobs/{id}` | 含 steps |
| GET | `/jobs/{id}/logs?after=<log id>` | `[{id,ts,level,msg}]`，按 id 升序，一次最多 1000 行（满页按最后一行的 id 接着拉） |
| GET | `/jobs/{id}/logs?tail=<N>[&after=<log id>]` | （after 之后的）最后 N 行（1-5000）：`{logs:[{id,ts,level,msg}], skipped, after}`，`skipped` 是前面没给的行数，`after` 是续拉用的游标（最后一行的 id，没有日志时原样返回） |
| POST | `/jobs/{id}/retry` | 失败的作业从失败那一步重来（`upgrade_agent` 从推送重来：失败时多半已经换回了旧程序）。可选请求体 `{reset_host_key?: bool, ssh_password?: string}`（不带请求体 = 都不要）：`reset_host_key` 清掉 known_hosts 里这台机器的记录、按现在的主机公钥重新记录：只允许失败那一步的类别是 `host_key`（中控确实看到了指纹不符）、目标不是同机 IP 节点（它登录的是宿主机：对宿主机的作业重试）的作业，否则 409；在其它检查都通过之后才做（拿不到公钥 409，整个重试作废）。确认机器重装过才用；`ssh_password` 是 root 密码，和上架时一样加密存进作业参数，供重试的作业在中控密钥登录不了时用（用过即丢；失败作业留存的密码 3 天后清除，之后重试要重新给）。只有 failed 的作业能重试（否则 409）；上架/恢复失败的机器重试时回到 provisioning。审计 `job.retry` 记 `reset_host_key` 与是否给了密码（不记密码） |
| POST | `/jobs/{id}/cancel` | 进行中的作业：补偿已完成的步骤后停下。`upgrade_agent` 只能在排队（queued）时取消：领到就收尾（没开始过，`started_at` 为 null、步骤原样），「全部升级」里等它的随后跟着取消；进行中、失败的 409（推送、重启到一半中断更危险）。详情的 `cancellable` 与这里同一套规则 |

## 运维：远程执行与日志
需要 `exec` scope（会话用户直接可用）。中控用自己的 SSH 密钥和 known_hosts 登录；机器必须不是已退役、不是家宽代理（proxy 没有 SSH）。命令原样当 bash 脚本执行（stdin 接 /dev/null），超时由远端 `timeout` 杀掉。每次执行写审计（actor、机器、命令全文、退出码、耗时），**不要在命令里写密码**。
| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/machines/{id}/exec` | `{command, timeout_s?: 1-600 (默认 60)}` → `{data:{exit_code, stdout, stderr, duration_ms, truncated, timed_out}}`。同机 IP 节点没有自己的 SSH：命令在它的宿主机上执行（审计记在这台节点名下）。stdout/stderr 各截断到 256 KiB（`truncated=true`）；超时 `timed_out=true`：到点远端 `timeout` 发 TERM，`exit_code=124`；命令不理会 TERM 的，再过 5 秒被 KILL（`timeout -k 5`），`exit_code=137`；远端没能按时结束、中控本地等到 `timeout_s+15` 秒仍没返回时也按超时返回 `exit_code=124`。SSH 连不上时 `exit_code=255` 且 stderr 有原因（命令自己退出 255 也是 255）。**只执行一次、不自动重试**（255 时命令可能已经执行过）。机器不存在 404、已退役或 proxy 409、命令空/超时越界 400、SSH 无法启动 502；同时最多 8 条命令在执行，排队 30 秒还轮不到回 429（`busy`） |
| GET | `/machines/{id}/logs?unit=fleet-agent\|fleet-xray\|ssserver&lines=200` | 白名单 unit（`ssserver` 对应 `fleet-ssserver`；同机 IP 节点对应宿主机上它自己的实例 `fleet-ssserver@<SS 端口>`，`fleet-agent` 是宿主机的），lines 1-2000；内部走同一 exec 通道跑 `journalctl`，返回形状同 exec，另有 `unit`（实际查的单元）、`lines`。审计动作 `machine.logs` |
| GET | `/server/logs?lines=200` | 中控自己的 `journalctl -u fleet-server`。fleet 用户通常没权限读 journal：journalctl 报错时 424（`error.code="journal_denied"`，message 说明如何授权：`usermod -aG systemd-journal fleet`）；journalctl 无法运行或 20 秒没返回时 500 |
| POST | `/audit/ssh` | `fleetctl ssh` 在中控本机用中控的密钥直接 ssh（不经中控执行），登录前报一声：`{kind: "machine"\|"router", id, command?: bool}` → `{ok:true}`，写审计 `ssh.login`（`target_kind` 是 kind，`detail = {name, command}`：对象名按库里的写，`command` 只记带没带命令，命令内容不记）。机器不存在 404，已退役、家宽代理 409；软路由按 `router` 表核对，不存在 404；kind 不对 400。fleetctl 没记上就不登录 |

## 网页终端
在浏览器里打开机器的 shell：中控本地开一个 PTY 跑 `ssh -tt`（中控自己的 SSH 密钥与 known_hosts，`StrictHostKeyChecking=yes`，同远程执行），WebSocket 双向搬终端字节。**只给会话用户**（API 密钥一律 403；命令行在中控上用 `fleetctl ssh`，不走这个接口）。机器要在用（`active`，或退役中的 `retiring`）：已退役、还没上架完成（`draft` / `provisioning` / `failed`）、家宽代理（没有 SSH）409；同机 IP 节点登录它的宿主机（宿主机同样要在用：已退役、还没上架完成也是 409）。软路由（`kind: "router"`）暂时 400「软路由终端稍后开放」。

**终端里输入的命令不进审计**：只记谁、哪台、何时开 / 关、时长、上下行字节数、为什么断（前端在打开前提示）。

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/terminal/tickets` | `{kind: "machine", id, code?, password?}` → `{ticket, expires_at}`。先看目标（不存在 404、不能开 409、`kind` 不认识或软路由 400）与名额（429 `terminal_limit`），再**再验证一次**：用户开了两步验证要 `code`（6 位验证码或一个恢复码，密码不顶用），没开要 `password`（当前密码）；同一个登录会话 5 分钟内验证过的免输（记在中控内存里，中控重启后重来）。缺了 401 `totp_required` / `password_required`（不算失败），错了 401 `totp_invalid` / `password_invalid`：与登录共用失败计数和限速（429 `rate_limited`），写审计 `auth.verify_failed`（`detail.op = "terminal"`），用了恢复码写 `auth.totp_recovery_used`。票据是随机 32 字节（base64url，43 个字符），60 秒内在 WebSocket 上用一次：只能用一次（不管成没成都作废）、只能由开票的这个登录会话用、绑定这台机器；中控只存它的 sha256。每人手里没用掉的票据最多 10 张（再开 429 `rate_limited`）。中控正在重启时 503 `unavailable` |
| GET | `/terminal/ws` | WebSocket，见下 |

`GET /api/v1/terminal/ws`（`ws://` / `wss://`）：
- **握手**：要会话 Cookie（401）、`Origin` 等于 `public_url` 的协议 + 主机 + 端口（403；`public_url` 不是 http(s) 地址时一律 403，`Origin: null` 永远对不上），API 密钥 403；同一个登录会话握手之后还没送票据的连接最多 5 个，再握手 429。失败是普通 HTTP 错误（浏览器的 WebSocket 拿不到状态码，只看到连接失败）。
- **第一条消息**（10 秒内，文本帧）送票据：`{"type":"auth","ticket":"…","cols":120,"rows":40}`，`cols` / `rows` 是初始窗口大小（可省，默认 80 × 24）。票据不放在地址里：反代、中控的请求日志都记不到它。票据不对、过期、用过、不是这个会话开的、机器在开票后不能开了、名额满了：服务端发 `exit` 说明原因后关闭。
- **之后**：二进制帧 = 终端字节（双向：浏览器发键盘输入，原样写进终端；服务端发终端输出，原样交给 xterm.js）；文本帧 = JSON 控制消息：浏览器发 `{"type":"resize","cols":120,"rows":40}`（整数，夹到 1–500；不认识的控制消息不理），服务端结束前发 `{"type":"exit","reason":"…"}`（中文原因，直接显示）再关闭。
- 浏览器发来的一条消息最多 **64 KiB**（粘贴大段文字请切开发），超了断开（关闭码 1009）。这一节的各个上限都在 `GET /meta` 的 `terminal` 里，前端取那里的数，不要自己写死。
- **关闭码**：一般 1000；登录会话失效 4401（同实时流，前端回登录页）；中控重启 1012；中控内部错误 1011。
- **什么时候断**：远端 shell 退出（`exit`、Ctrl-D）；ssh 连不上、被拒、主机指纹不对（退出码 255，ssh 的报错已经显示在终端里）；浏览器关掉或断线（心跳每 15 秒；往不读输入的程序里粘贴、输入积着的时候不读浏览器、也不查心跳）；**15 分钟没有输入**（按键、粘贴；改窗口大小、光有输出都不算）；**连续开满 2 小时**；登录会话失效（退出登录、在「账户安全」撤销这个会话、退出其它设备、改密码、会话过期、账户被停用：收到撤销立刻断，另外每 60 秒复核一次兜底）；浏览器 15 秒收不走输出（页面卡死）；中控重启（收到退出信号后先以「中控正在重启」断开全部终端、写完「关闭」审计，关闭码 1012；重新起来之前开票 503）。
- **上限**：每人同时 3 个、全中控同时 8 个（开票时先查，连上时为准）。
- 断开时中控结束本地的 ssh（整个进程组，先 TERM、3 秒后 KILL），远端的 shell 随连接断开收到 SIGHUP 退出（`nohup`、`setsid` 起的后台进程照常留着）。ssh 关掉了转义字符（`~.`、`~C` 都不起作用）、不做端口转发、不转发中控的环境变量，远端的 `TERM` 是 `xterm-256color`。
- **审计**：`terminal.open`（`detail`：`name`、`ip` / `port`（实际登录的地址：同机 IP 节点是宿主机的）、`host`（同机 IP 节点的宿主机 `{id, name}`，否则 null）、`client_ip`、`session`（登录会话的 id，同 `/account/sessions` 的 `id`））；`terminal.close`（同上，另有 `open_id`（对应的打开记录）、`duration_s`、`bytes_in`（上行：浏览器 → 机器）、`bytes_out`（下行：机器 → 浏览器）、`why`（`remote_exit` / `client_closed` / `client_gone` / `idle` / `max_age` / `session_invalid` / `shutdown` / `stalled` / `too_large` / `recheck_failed` / `error` / `spawn_failed`）、`reason`（同 `exit` 的原因）、`exit_code`（ssh 的退出码，没有为 null））。`terminal.open` 在起 ssh 之前写：中控异常退出（崩溃、被强杀）的话只有打开、没有关闭。

## 总览与告警
| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/overview` | `{nodes:{total,ok,degraded,down,unknown,maintenance}, relays:{…}, provisioning, retiring, failed（上架失败的机器数，网页「有问题」里也算它们）, due_soon（`subscription.due_state = soon` 的台数：到期前提前提醒天数内、不是自动续费，网页「即将到期」）, overdue（已过期的台数）, online, offline（按 agent 连通：agent 没有近期上报为离线，家宽代理没有 agent 算在线；与健康无关，有严重告警但照常上报的算在线）, monthly_cost:[{currency,cents}]（各台 `subscription.monthly_cents` 按币种分别相加，不做汇率折算）, traffic_month:{up,down}, net_rx_bps, net_tx_bps}`（只统计 active；速率不含 agent 离线的机器——告警不影响，照常上报的机器照算；同机 IP 节点的读数就是宿主机的，速率不重复算，节点数与在线 / 离线照常按节点计） |
| GET | `/overview/series?minutes=30` | 总览网速曲线：`{points:[{ts, net_rx_bps, net_tx_bps}]}`，`minutes` 1–60（默认 30，越界 400）；最近 N 个**结束满 20 s** 的分钟（agent 每 10 s 上报，刚结束的那一分钟还缺最后的上报，不给）每分钟一个点、旧→新，`ts` 为该分钟起点（RFC3339）。每个点 = 该分钟有主机指标的 active 机器速率之和，单台算法同 `/machines/{id}/series?metric=host`（每分钟计数器最大值与上一个有数据的分钟相减 ×8 / 秒数；计数器回绕的那一段不计）；没有数据的分钟为 0 |
| GET | `/probe-beats` | 探测心跳条：`{<机器id>: [[n, ok, ms], …20 格]}`。每格 3 分钟、按墙钟对齐（`date_bin('3 minutes', ts, 'epoch')`，即每小时的 :00 / :03 / … ），旧→新，最后一格是当前这格（还没走完）；第 i 格 = `[cb-57m+3i 分, cb-57m+3(i+1) 分)`，cb = 当前这格的起点；每格 `[n, ok, ms, retrans]`：`n` = 次数，`ok` = 成功次数，`ms` = 成功的延迟中位数（没有成功的为 null），`retrans` = 建连时 SYN 重发过的次数（丢包，不算进 ms；旧 agent 不报，为 0）；没有数据的格为 `[0,0,null,0]`。节点：线路机 → 节点的 TCP 探测，汇总它的入口；线路机：中控 → 线路机的连通性（`relay_reach`，中控每 30 秒连一次它的客户入口——public_host + 最小的在用入口端口——只判通不通，ms 为 null；中控自己的网络不通时那一轮不记），不再汇总它上面的入口；只含这 20 格里有数据的在用机器 |
| GET | `/incidents?state=firing\|resolved\|all&subject_id=&limit=100&offset=0` | 分页：`{items:[{id,rule,rule_name,subject:{id,name},subject_id,subject_name,severity,state,summary,opened_at,last_seen_at,resolved_at,ack_at,ack_by,reopen_count}], total, acked, next_offset}`；`acked` = 同一筛选下已确认的条数（确认不关闭告警，`total` 含它们）；`state` 默认 firing；`subject_id`（可选，机器 id）只看这台机器的告警，与 `state`、分页组合，`total` 同样只数它的（如抽屉的最近告警：`?subject_id=<id>&state=all&limit=5`；不是 uuid 时 400）；`reopen_count`：恢复后 30 分钟内同一（规则, 对象）复发会重开原告警而不是新建，每次 +1；顺序：进行中在前 → 严重度高在前 → 开始时间新在前（同时刻按 id，翻页不重不漏）；带 `subject_id` 时不看严重度：进行中在前 → 开始时间新在前（一台机器的「最近告警」按时间读）。中控自身的告警（规则 `central_` 开头，见下表）对象不是机器：`subject`、`subject_id` 为 null，`subject_name` 为「中控」 |
| POST | `/incidents/{id}/ack` | 确认告警：不存在 404，已确认过 409；写审计 `incident.ack` |
| GET/POST | `/silences`、DELETE `/silences/{id}` | GET：还没结束的（`ends_at` 晚于现在，含还没开始的预约）`[Silence]`，按 `starts_at` 排——进行中的在前，已预约的随后；只有此刻在窗口内的才起作用（不开告警、不发通知，维护还暂停健康判定）。POST `{subject_id?,rule?,starts_at,ends_at,is_maintenance,comment（最多 2000 字）}` → `{id}`（`subject_id` 空 = 全部机器，`rule` 空 = 全部规则）：`ends_at` 要晚于 `starts_at`、时长最多 30 天、`ends_at` 不能已经过去（`starts_at` 可以，「从现在开始」由客户端按自己的时钟填）、`starts_at` 最多在 90 天后，`rule` 必须是已知规则 key（且不能与 `is_maintenance` 同时给：维护针对整台机器）、`subject_id` 必须是存在的机器，否则 400（中文原因）；全部机器（`subject_id` 空）的用 API 密钥建时最长 24 小时（超了 400：全部机器的静默等于关掉监控），账号登录的会话仍是 30 天。DELETE 不存在或已删 404。新建、删除都写审计（详情是这条静默本身：对象名、规则、起止、维护、备注），并给所有启用的通知渠道发一条 `silence` 通知（谁、新建还是删除、对象、维护还是静默、起止、备注；不看渠道的 `min_severity`，也不受任何静默影响）：全部机器的一律发（含预约的），单台的只在此刻生效（`starts_at` ≤ 现在 < `ends_at`）时发 |
| GET | `/audit?target_id=&cat=&q=&limit=&before_id=` | 按 id 倒序；`before_id` 翻页（只要 id 小于它的，传上一页最后一条的 id），与筛选一起用。`cat`：`all`、`auth`（auth.*）、`exec`（exec、ssh.login、terminal.open/close、machine.logs、server.logs、restore.probe）、`machine`（machine / entry / relay / subscription / agent / job / renewal 开头的，除去 exec 类）、`config`（其余），别的值 400。`q`：不分大小写的包含匹配，`%` `_` 按字面；匹配操作人（按界面写法：`cli` 是「中控命令行」，`apikey:名称` 是「名称（API 密钥）」）、标题（`title` 含它的动作）、摘要（`summary`，写审计时存下；`detail` 里摘要不展示的值不算），有 `exec` scope 时另搜命令原文；查询超过 5 秒中止，回 503 `unavailable`（换更具体的关键词或先按分类筛选）。没有 `exec` scope 的密钥看不到命令：`detail.command` 换成「命令已隐藏（N 字）」，`summary` 里的命令同样隐藏。`exec` / `machine.logs` 在执行前就写一条 `detail.started=true` 的记录，执行完补上结果并去掉 `started`；`started` 还在说明在执行中或中控中途重启 |
| GET | `/notify-log?limit=100` | 通知发送记录（新→旧，`limit` 1–1000）：`[{id, channel_id, channel_name, incident_id, ts, ok, error, kind: "firing"\|"repeat"\|"resolved"\|"test"\|"silence", subject_name, rule_name}]`；一次投递一条，在送达或放弃时写：失败重试中的不记（还在发送队列里），入队满 30 分钟还没发出的放弃，记一条 `ok:false`、`error` 为「入队 30 分钟还没发出，放弃（最近一次失败：…）」；渠道拒绝（不值得重试）的当场记失败。`error` 只有 HTTP 状态与渠道 JSON 里的说明（Telegram `description`、企业微信 `errmsg`，截到 200 字），不存原始响应体；`subject_name` 是告警对象的显示名（中控自身的告警是「中控」，`silence`（静默 / 维护变更）是「静默 / 维护变更」），`rule_name` 是规则中文名（与通知正文一致）；测试消息两者为 null，机器或告警已删除时对应字段为 null。没收到某条告警「触发」的渠道不发它的「恢复」，也不记 |

**中控自身的告警**（selfmon.rs，规则 `central_` 开头）：对象是「中控」而不是机器，每 30 秒随告警评估检查一次；不去抖，恢复后再出现新开一条；被静默的不开、不发（只能用全部机器 + 规则 key 静默）；critical 每 4 小时提醒一次。通知里「机器」一行写「中控」。

| 规则 | 级别 | 开 | 恢复 |
|---|---|---|---|
| `central_channel_failing`（通知渠道发送失败） | critical | 某个启用的通知渠道连续发送失败满 5 分钟（`notify_channel.failing_since`）。每个渠道一条（库里 `subject_id` 是渠道 id）；这条不发给它自己 | 它发成功一次（含渠道测试）；渠道停用、删除时直接关，不发恢复（`detail.closed_by = "channel_removed"`） |
| `central_backup_stale`（备份过期） | warning | 备份目录（`[selfmon] backup_dir`，默认 `/var/backups/fleet`）里最新一份 `fleet-*.tar` / `fleet-*.tar.age` 超过 26 小时，或一份都没有、读不了；目录不存在（没装备份）不检查 | 有了 26 小时内的一份 |
| `central_disk_low`（中控磁盘空间不足） | critical | 中控本机 `/`、`/var/lib/postgresql`、`/var/lib/fleet`、备份目录所在的文件系统可用空间低于 10%（同 df） | 回到 11% 以上 |

另有死人开关：配置 `[selfmon] heartbeat_url` 后，告警评估每轮成功后最多每分钟 GET 它一次（OPS.md §12）。

## 返回形状补充（前端以此为准）

- `health.reasons[]`：`{key, rule_name, severity, text}`；`rule_name` 是规则中文名（同通知）；`severity`：`"info"` | `"warning"` | `"critical"`（info 只有 `policy_blocked`，不影响健康）。
  `traffic_quota_high`（「流量额度吃紧」）：本周期已用（`quota.used`）到上限（订阅的 `traffic_gb`，没填按套餐的；0 / 空 = 不限，不评估）的 90% 为 warning，到 100% 为 critical；上限按十进制 GB（1 GB = 10⁹ 字节，同供应商），文案也写 GB、不换算成 TB，如「本周期流量已用 92%（1840 GB / 2000 GB），9 天后重置」「本周期流量已超出额度（已用 102%，2050 GB / 2000 GB）」（有流量重置日才写几天后重置）。
  它和 info 一样**不影响** `health.status`，只开告警：不去抖，回落到 90% 以下（如新周期开始）下一轮评估即恢复；升为 critical 时马上再通知一次，持续提醒每天最多一次（同续费类）。同机 IP 节点自己没设上限时不评估（额度算在宿主机上）。
  `clock_skew`（warning，「agent 时钟不准」）：agent 在线且 `|agent.clock_skew_ms|` > 60 秒，文案如「agent 时钟慢 17 分钟：读数可能缺失或冻结，请校时」——样本按 agent 的时间入库，当前读数只看最近 15 分钟、持续负载只看最近 5 分钟；比中控超前 10 分钟以上的样本（含入口流量、探测）直接丢弃。和其它 warning 原因一样满 60 秒去抖后开告警。
  同机 IP 节点不报 `agent_offline`、`clock_skew`、`host_cpu_high` / `host_mem_high` / `host_disk_high`（agent 与主机是宿主机的，由宿主机报）；`egress_fail`、`tcp_fail`、`no_entry`、续费照常自己报（宿主机整台宕机时宿主机和它的每个同机节点各报各的探测告警，不抑制）。
- `GET /machines/{id}/series` → `{metric, range, resolution: "1m"|"1h", points: [...]}`，点里时间字段统一叫 `ts`：
  - host：`{ts, cpu_pct, mem_pct, disk_pct, load1, net_rx_bps, net_tx_bps}`（1h 分辨率另有 `cpu_max`，无 `load1`）
  - traffic：`{ts, up, down, conns}`（字节，该时间桶内的增量）
  - probe：`{ts, tcp_ok, tcp_ms, egress_ok, egress_ms}`（失败时 `*_ms` 为 null；1m 分辨率另有 `tcp_n`、`tcp_ok_n`（该分钟 TCP 探测次数 / 成功次数，线路机为其所有入口合计）；1h 分辨率另有 `tcp_success_pct`、`egress_success_pct`）
- 作业 `Job`：`{id, kind, target: {id,name}|null, state, step, step_count, step_title, error, created_by, created_at, started_at, finished_at}`（`step` 当前或失败的步骤下标，`step_title` 它的标题，做完为 null）；
  `state` ∈ `queued | running | waiting | cancelling | succeeded | failed | cancelled`；详情另有 `steps[]`（`output` 是 JSON 对象或 null；各步骤输出的 key 与中文展示见 `web/src/lib/jobs.ts` 的 `OUTPUT`，跳过的步骤为 `{reason}`）与 `cancellable`。
  作业 `kind` ∈ `provision_vps | provision_proxy | provision_relay | provision_ip | restore_vps | restore_proxy | restore_relay | restore_ip | attach_entry | move_entry | retire | purge | upgrade_agent`。
  `provision_ip`（上架同机 IP 节点）/ `restore_ip`（恢复同机 IP 节点）的步骤：`ip_check`（在宿主机上核对 IP，输出 `{ss_port, local_ips, egress_ip}`：最终的 SS 端口、宿主机本机的 IPv4（逗号加空格分隔的字符串）、经这个 IP 访问中控 /echo 回显的地址）→ `converge`（推 ssserver、装它自己的实例与防火墙，输出同 VPS 节点，`bbr` 为 null——不做系统基线）→ `attach` → `wait_applied` → `verify` → `activate`。
  `ip_check` 失败：宿主机 SSH 连不上（network）；IP 不在宿主机网卡上（「IP x 不在宿主机 H 的网卡上（本机：…）」，remote）；回显的来源不是这个 IP（出站路由不对，remote）。暂定的 SS 端口在宿主机上被占用时自动换成最小空闲端口，日志里有一条警告。
  同机 IP 节点的退役作业 `cleanup` 一步只删它自己的实例（输出 `ss` 是 gone / still_listening，指它自己的端口）。
  `upgrade_agent`（升级 agent）的步骤：`agent`「推送并重启 agent」（输出 `{binary_updated, agent_version, prev_version, xray_checks, xray_restarted}`：`xray_restarted` 为 true 表示线路机 DNS 是链路本地地址、为换上公共解析器重启了一次 Xray（其余为 null）；`agent_version` 是装上的版本，`prev_version` 是这次换下来、留作回滚的旧版本，程序没变时为 null；`xray_checks` 是升级线路机、程序换过时等上报还要做到的几项，如「Xray 在跑、配置应用到代数 3、没有应用错误」，升级前都没做到时「不查（升级前 Xray 就不正常）」，其余为 null；上架的 `agent` 步骤输出同形）→ `wait_report`「等待新版本上报」（等装上的版本用新令牌上报，线路机还要过 `xray_checks`，输出 `{first_report_at, agent_version, old_tokens_revoked}`）。回滚见 `POST /machines/{id}/upgrade-agent`；被编排取消的、排队时被取消的（见 `POST /agents/upgrade`、`POST /jobs/{id}/cancel`）`started_at` 为 null、步骤全是 pending。
- 告警 `Incident`：`{id, rule, rule_name（规则中文名，同通知）, subject: {id,name}, subject_id, subject_name, severity, state: "firing"|"resolved", summary, opened_at, last_seen_at, resolved_at, ack_at, ack_by, reopen_count}`（`subject_name` 机器已删除时为 null；中控自身的告警 `subject`、`subject_id` 为 null、`subject_name` 为「中控」）。
- 静默 `Silence`：`{id, subject_id, subject_name, rule, rule_name, starts_at, ends_at, is_maintenance, comment}`（`subject_id` / `subject_name` 为 null = 全部机器；`rule_name` 是规则中文名，同告警，`rule` 为 null（全部规则）时也为 null）。
- 续费 `Renewal`：`{id, old_due, new_due, amount_cents, currency, note, actor, created_at}`。`currency` 是登记时订阅的币种（迁移 0029 起记；之前的按迁移时订阅的币种补上），之后改订阅币种，历史金额不跟着变。币种一律 3 个大写字母（套餐、订阅、续费记录库里都有约束；套餐接口会去空白转大写，其它不合规的回 400）。
- 审计：`{id, ts, actor, action, title, summary, target_kind, target_id, detail}`；界面显示 `title` 与 `summary`（中文，服务端生成），`detail` 是原始 JSON。

## 实时
`GET /api/v1/stream`：WebSocket（`ws://` / `wss://`），鉴权同其它 GET（会话 Cookie 或 `Authorization: Bearer`，需要 `read`）；用会话 Cookie 连时 `Origin` 必须等于 `public_url` 的协议 + 主机 + 端口，否则 403（WebSocket 握手不受同源策略约束），握手失败返回普通 HTTP 错误（401/403）。服务端只发 JSON 文本帧，不读客户端消息。
- **保活**：服务端每 15 s 发 ping，到下一次 ping 还没收到 pong 就断开（浏览器自动回 pong）；同时发一个文本心跳 `{type:"hb"}`（浏览器脚本看不到协议层的 ping），客户端连着 45 s 什么都没收到就当连接已死、自己重连，`hb` 本身直接丢弃。
- **会话复核**：每 60 s 查一次会话/密钥是否仍有效，撤销会话或密钥时各连接马上再查一次（服务端内部通知，不推给前端；命令行 `user-set` 经 PostgreSQL `NOTIFY fleet_revoked` 转过来），失效则以关闭码 `4401` 关闭（客户端应回登录页，`fleetctl watch` 报错退出）；复核查询本身失败以 `1011` 关闭，客户端照常退避重连、握手时重新鉴权。
- **滞后**：服务端广播缓冲 1024 条，某个连接读得太慢丢了消息时补发 `{type:"resync"}`，客户端应让全部缓存失效重取。
- **重连**：客户端断线后自己退避重连；连上后应重取一次机器类数据（断开期间错过的 `metrics` 增量不会补发）。

**资源变更通知**（收到后让对应查询失效重取）：`{type:"machine",id}`、`{type:"machines"}`、`{type:"relay",id}`、`{type:"job",id}`（作业推进；目标机器的 `active_job` 走 `metrics`，它变了时客户端可重取这台机器的详情；改生命周期时另发 `machines`）、`{type:"incidents"}`、`{type:"catalog"}`、`{type:"api_keys"}`（密钥增删撤销）、`{type:"policies"}`、`{type:"settings"}`（面板设置变了：访问记录保留期）、`{type:"resync"}`。

**实时指标** `{type:"metrics", machines:{<机器id>:{…}}, entries:{<入口id>:{…}}, overview?}`：在用机器视图（`GET /machines` 的 `MachineView`）里随上报与时间变化的字段，和上一条相比**只带变了的字段**（没变的键不出现），客户端把它们写进本地缓存，不必重取。有新上报或作业推进时最多每 3 s 一条，没有也每 30 s 比对一次（离线、到期天数、维护窗口随时间变）；没有变化不发。
- `machines[id]` 的键：
  - `host`：`[ts, cpu_pct, load1, mem_used, mem_total, disk_used, disk_total, net_rx_bps, net_tx_bps, tcp_conns] | null`（`ts` 为 Unix 秒，速率取整；各项可为 null，与 `MachineView.host` 同义）；
  - `seen`：`agent.last_report_at` 的 Unix 秒 | null；`agent`：`{version, online, xray_running, cores} | null`（不含 `last_report_at`、`health` 和 `clock_skew_ms`——agent 自报的累计计数、时钟偏差每次上报都变，只随 REST 重取更新；前端合并进原对象）；
  - `health`、`maintenance`、`removed_traffic`、`quota`、`active_job`：与 `MachineView` 同形；`due_days` / `due_state`：`subscription.due_days` / `due_state`（无订阅为 null；跨天时一起变）。
- `entries[id]` 的键：`tcp` / `egress`：`[ok, latency_ms, ts, detail, median_ms] | null`（`ts` 为 Unix 秒；`median_ms` 见 EntryView）；`traffic`：与 `EntryView.traffic` 同形。一条入口同时出现在节点和线路机的 `entries` 里，两边都要更新。
- `overview`：与 `GET /overview` 同形，变了才带。
- 机器的新建、改名、换团队、退役、告警等结构变化不走 `metrics`，仍发上面的失效通知。
- 静默的新建 / 删除只发 `incidents`（不发 `machines`）：卡片上的 `maintenance`、`health`、总览的维护中计数随后（最多 3 s）经 `metrics` 送达，不必为它重取机器列表。

`fleetctl watch [--count N] [--metrics] [--json]` 订阅这个流（默认不显示 `metrics`）。

## agent（`/agent/v1`，Bearer 令牌）
见 `crates/fleet-proto/src/lib.rs`。另：`GET /echo` 返回请求方 IP（纯文本），供出口探测用，无需鉴权。线路机经入口访问它没拿到 IP 时，agent 核对两次：不走代理直连它一次，直连也拿不到 IP（中控重启、部署中）就不记这次出口探测；直连拿得到，再经入口访问备用目标 `https://www.gstatic.com/generate_204`，通的话说明出口好好的、只是节点去 /echo 的那条路不通（如边缘机反代配置坏了），同样不记，都不算节点出口失败。经入口回了 IP 但不是期望的照常记失败。探测入站的放行规则 `probe-<入口>` 因此也放行备用目标（与回显服务都是域名时合成一条，回显服务是 IP 时另加 `probe-<入口>-1`）。
TCP 探测（`ProbeSample.kind = tcp`）：线路机 → 节点出站地址建连，超时 10 秒（盖住 SYN 第 1、3、7 秒的三次重发），域名先解析、不计时；`latency_ms` 是内核 TCP_INFO 的往返时间，`retrans` 是建连时 SYN 重发次数（新字段，旧 agent 不报、旧中控忽略）。各入口的探测落在周期里按 id 错开的固定时刻，不在同一秒一齐发。

直出入口在期望状态里是 `"outbound": {"protocol": "direct"}`（`expect_egress_ip` 为线路机自己的 IP）。不认识它的旧 agent 整份期望状态解析失败、回报「agent 版本过旧」（线路机 apply_error），所以**先升级线路机 agent，再建直出入口**。agent 为它渲染入口自己的 freedom 出站（`out-<入口>`，不复用共享的 `direct`），不做 TCP 探测；路由和节点入口相同（AsIs，`block-private` / `block-bittorrent` 在前），另在该入口规则最前面加 `guard-<入口>-api`：目标端口是本机 Xray API（62789）或任一入口的回环探测入站（入口端口 + 10000）的连接一律 blocked（各入口的探测规则 `probe-<入口>` 本身也只放行到回显服务，即 echo_url 的主机与端口，和出口探测的备用目标，别的目标落到默认的 `deny`：探测入站是不要密码的 SOCKS，不能当跳板）——`block-private` 只拦 IP 目标，客户写解析到回环的域名（`localhost`、`127.0.0.1.nip.io`）时 freedom 会在线路机本机解析并连上 API。解析到内网其它地址的域名仍能连（与旧系统的主入口相同）；没有改成 IPIfNonMatch 先解析再过 `block-private`，是因为 fake-ip DNS 环境（实验室，域名都解析到 198.18.0.0/15）里那样会把所有域名都拦掉。

`POST /agent/v1/report` 入库失败按类回：数据本身入不了库（SQLSTATE 22 类：坏字节、NUL、越界；23 类：违反约束）→ 422（`error.code="bad_report"`），agent 把这份报告移入 `dead/` 不再重发，其中的入口流量结转到下一份报告补报（同离线队列超限淘汰；这份本身带着结转补报的流量时不再结转，免得一份接一份地被拒）；其它（连接池超时、断连、磁盘满、只读…）→ 503，agent 退避重试（受本地队列的字节与时长上限约束）。agent 只把 4xx（401 撤销、403/404/408/429 除外）当永久拒绝，5xx 和网络错误一律重试。

上报的限额（按机器，在内存里，中控重启归满）：新的一份（seq 大于入库提交过的）要有一个令牌，突发 600、之后每秒补 1，扣不到回 429 + `Retry-After`；入库提交之后才扣令牌与主机样本余额，入库失败的那份不扣、也不算「提交过的 seq」（重发的同一份不扣）。agent 给的字符串里的 NUL 入库前去掉（text / jsonb 不收）。主机网卡计数器的每段增量在小时汇总里按 10 Gbit/s × 两个样本的间隔封顶、入口流量每小时按 10 Gbit/s 跑满封顶，用量求和夹到 9e18：离谱的计数器不会让汇总或机器列表溢出。样本时间戳只收「保留期以内 ~ 中控时间往后 10 分钟」（访问记录按面板里设的原始记录保留期，其余按 `[retention] raw_days`；agent 时钟快得更多时样本丢弃，健康原因 `clock_skew` 在偏差超过 60 秒时就已报出）。主机样本按 (机器, 时刻) 唯一，同一时刻重报的不再入库；另有主机样本余额：容量 8640 个（24 小时的量），每 5 秒补一个（两倍于 agent 10 秒一个的采样），超出的丢最早的——正常 agent 补传积压（最多 6 小时离线队列 + 12 小时内存缓冲）期间余额按两倍速回满，用不完；一台机器一天最多入库约 2.6 万条（正常 8640）。

`POST /agent/v1/applied`：每台线路机突发 120 份、之后 2 秒一份，超出回 429 + `Retry-After`（agent 照 Retry-After 重试，三次不成落盘、下轮补发）。`error` 与 `entries[].error` 截到 512 字，`entries` 最多 3000 条（不属于这台线路机的入口 id 忽略）。回报没改变任何东西（重发的同一份、同样的错误）时不发 `relay` / `machines` 失效通知。

## 访问监控
线路机 agent 解析 Xray 访问日志上报，服务端写入 `access_event`（原始记录，默认留 7 天），并每 5 分钟把当前小时和被标脏的小时重算进小时汇总（`access_hourly` / `access_source_hourly`，默认留 31 天，窗口最长 30d 且不超过汇总的保留期；`access_target_seen` 记每个目标的首次 / 最近出现，最近一次出现超过汇总的保留期就删；留多久见下文「访问记录保留期」；幂等；agent 断网后补传的旧记录在上报时把所在小时记进 `access_dirty`，下一轮补算；启动时补算缺口）。24h/7d/30d 的结果在进程内缓存：每轮汇总成功后整体作废；`summary` / `targets`（第一页）/ `sources` 最多缓存 `access_cache_secs`（默认 300）秒，单台机器的 `/machines/{id}/access` 与 `/machines/{id}/sources`（带最近原始记录、来源基线与异常）最多 30 秒；设 0 关闭。全部为只读 GET，读权限即可。

公共筛选参数（空串等同没给，非法值返回 400）：`window=1h|24h|7d|30d`（默认 24h，events 默认 7d；除 events 外不能比汇总的保留期 `hourly_days` 长，超了 400，见「访问记录保留期」）、`team=<团队id>`、`node=<节点id>`、`search=`（大小写不敏感的子串，`%` `_` 按字面）、`action=all|blocked|routed`、`kind=all|domain|ip`。

- **窗口口径**：`1h` 直接查原始记录；`24h` 及以上查小时汇总，起点对齐到整点（`window_start` 返回实际起点），最新数据最多晚 5 分钟。
- **随机子域名合并**：注册域名之下的标签含 UUID、或 ≥8 位纯十六进制且 ≥2 位数字、或 ≥20 位字母数字且 ≥4 位数字，则整条合并为 `*.<注册域名>`（注册域名取最后两段，倒数第二段是 com/net/org/gov/edu/co 且最后一段是两字母国家码时取最后三段）。`events` 返回的是未合并的原始目标。
- **处理结果**：`action=blocked` 时 `conns` 即被阻止数；`action=routed` 时 `conns` 为去掉被阻止后的数，`blocked` 为 0。
- **直出入口**：按入口的节点端归属，节点就是线路机自己，`node_id` 是线路机 id、`name` 是「线路机名-直出」，`team` 为 null。
- **来源数**：1h 窗口精确去重；24h 以上是「各小时去重数的最大值」，为偏小的近似（节点级的 `sources` 是精确去重，24h 以上带 `search` / `kind` / `action` 筛选时为 null，见下）。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/access/summary?…` | `{kpi:{active_nodes,conns,targets,unique_targets,blocked}, nodes:[{node_id,name,team,conns,targets,domains,sources,blocked,last_ts}], window_start}`。`targets` 是各节点分别去重后相加，`unique_targets` 按目标去重（同一个目标只算 1 个，与 `group=target` 的 `total` 同口径）；节点按连接数降序。节点的 `sources` 是去重来源 IP 数，跟随全部筛选（和同行的 `conns` 同一批连接）：`1h` 在原始记录上逐条带上 `search`（目标或来源 IP）、`kind`、`action`；`24h` 及以上来自来源小时表，没有目标维度，带了 `search`、`kind`、`action`（非 all）任一筛选时为 `null`（界面显示「—」） |
| GET | `/access/targets?…&group=target&sort=conns\|last\|first&limit=100&offset=0` | 按目标合并各节点（`group=target`，「域名与 IP」页用）：`{items:[{dest_host,kind,nodes,conns,blocked,first_seen,last_seen}], total, next_offset, window_start}`，`nodes` 是访问过它的节点数，`conns` / `blocked` 各节点相加，`first_seen` / `last_seen` 取各节点里最早 / 最近的；不给来源数（各节点分别去重，相加没有意义）；`total` 是符合条件的目标总数 |
| GET | `/access/targets?…&group=node&target=&sort=conns\|last\|first&limit=100&offset=0` | 缺省（`group=node`）是「目标 × 节点」行；`target=` 只看这一个目标（合并后的名字，精确匹配；展开合并行看各节点）。`{items:[{node_id,name,dest_host,kind,conns,sources,blocked,first_seen,last_seen}], next_offset, window_start}`；`limit` 1–500，`offset` ≤ 100000，满页才给 `next_offset`；`first_seen` 是该节点历史上第一次出现（不限窗口）。不单独回总数：同一筛选下 `/access/summary` 的 `kpi.targets` 就是这张表的总行数（同一个聚合） |
| GET | `/access/sources?…&conn=&limit=100` | `{items:[{node_id,name,src_ip,conns,last_ts,intel}], scan_capped, window_start}`，按 (节点, 来源 IP) 连接数降序；`search` 在这里匹配来源 IP；`conn=residential\|mobile\|datacenter\|vpn\|unknown`（可选）只要这种接入方式的（`unknown` 含查不到画像的；画像缓存里已查过、接入方式不符的在库里就筛掉，其余按连接数最多取回 5000 个查画像再筛；`scan_capped` 为 true = 取回到了 5000 个、筛完不够 `limit` 个，后面可能还有）；`limit` ≤ 500 |
| GET | `/access/events?node=&team=&search=&action=&kind=&window=&limit=100&before=<游标>` | `{items:[{ts,node_id,name,src_ip,dest_host,dest_port,network,blocked}], next_before}`，按 (时间, 记录 id) 倒序；`next_before` 是不透明游标（满页才给），下一页把它原样放进 `before`（要 URL 编码）；同一时刻的多条记录不会被跳过；格式不对 400。`search` 同时匹配目标和来源 IP；回看所选窗口，只有原始记录保留期（`raw_days`）以内的，窗口比它长不报错 |
| GET | `/access/new-targets?window=24h&node=…&limit=200&offset=0` | `{items:[{node_id,name,dest_host,kind,first_seen,last_seen,total}], total, next_offset, window_start}`：顶层 `total` 是符合筛选的目标总数，`next_offset` 只在后面还有时给出；窗口内第一次出现的目标（之前在汇总的保留期内没见过），按首次出现倒序；`total` 为累计连接数（汇总保留期内）。这是旧系统「新域名」的替代，**不产生告警** |
| GET | `/machines/{id}/access?window=24h` | `{window_start, top_domains:[{dest_host,kind,conns,blocked,sources}]（30）, top_sources:[{node_id,name,src_ip,conns,last_ts}]（20）, recent:[事件]（原始记录保留期内最近 20 条）, hourly:[{hour,conns,blocked}]（缺的小时补 0；至少 24 小时）}`。线路机只算经它的流量（它名下的入口，含已移除的；同一节点经别的线路机的流量不算），节点算它自己全部入口的；机器不存在 404 |

## 访问记录保留期

访问监控照常记录，访问记录只留面板里设的天数，到期中控自动删除（`settings.rs`；设置存在 `app_setting` 表，迁移 0028）。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/settings/access-retention` | `{raw_days, hourly_days, min:1, max:90, profile_days}`：`raw_days` 是原始访问记录（`access_event`，每一条：时间、来源 IP、目标）留几天；`hourly_days` 是汇总留几天，也是访问页汇总类视图能选的最长窗口；`profile_days` 是来源画像实际留几天 = max(`hourly_days`, `[ipintel] learn_days`)（ipintel 关掉时就是 `hourly_days`），只读。没设过时 `raw_days` = 配置文件的 `[retention] access_days`（默认 7），`hourly_days` = 31 |
| PUT | `/settings/access-retention` | 同形，只认 `raw_days` / `hourly_days`（两个都要给，其余字段忽略），要 `write`：各 1–90 的整数、`hourly_days` ≥ `raw_days`（汇总是从原始记录算的），否则 400（中文原因）。返回改完的（同 GET）。有变化时写审计 `settings.access_retention`（`detail:{from:{raw_days,hourly_days}, to:{…}}`，摘要如「原始记录 7 → 3 天，汇总 31 → 14 天」）、发实时事件 `{type:"settings"}`，并马上在后台按新期限清一次（不等每小时的维护；删得多时要几分钟）；没有变化时什么都不写 |

各管什么（后台每小时按当前期限清一次，截止时间对齐整点）：
- `raw_days`：原始访问记录（按天分区，过期的整天删掉，最多多留不到一天）；agent 上报的访问记录只收这么久以内的；中控启动时补算汇总、机器抽屉的最近记录都回看这么久。
- `hourly_days`：小时汇总（`access_hourly`）、来源小时汇总（`access_source_hourly`）、补算用的迟到差值与脏标记、目标首次 / 最近出现（`access_target_seen`，最近一次出现超过这么久的删掉）、IP 画像缓存（`ip_intel_cache`：访问来源的 IP，另有机器自己的地址（国旗用）；超过这么久前查的删掉，再用到时重查离线库）。
- `profile_days`：来源画像（`node_source_seen` 的运营商 / 城市 / 接入方式 / 机房·VPN 来源 IP，最近一次出现超过这么久、且不在告警中的删掉）与已恢复的画像异常告警（`ipintel_*`，摘要里有来源 IP、城市；别的告警仍是恢复后 180 天删）。学习期比汇总长时按学习期留：学习期里学到的基线要留到学习期结束，否则学完一出现就被当成「新的」告警；多留的只是这几类汇总值，原始记录和小时汇总照样按上面两个天数删。
- 「新目标」「新运营商 / 新城市 / 首次出现的机房来源」都是「保留期内没见过」：超过期限没再出现的记录删了，之后再出现就算新的。
- 汇总类视图（`/access/summary`、`/access/targets`、`/access/sources`、`/access/new-targets`、`/machines/{id}/access`、`/machines/{id}/sources`）的 `window` 不能比 `hourly_days` 长（`1h` 读原始记录，总能看）：超了 400「访问记录的汇总只保留 N 天，看不了最近 7 天；要看更久，先在设置里把保留期调长」。`/access/events` 不拦，窗口比原始记录的保留期长时更早的已经没有了。
- 调长不会找回已经删掉的数据。每日备份里的汇总与来源画像随备份轮换才消失（OPS.md §13）。

## 来源 IP 画像（IP 情报）
离线库在 fleet-server 进程内查询（ip2region v4/v6、DB-IP City/ASN Lite、iptoasn，mmap），**只查本地，绝不把 IP 发往在线服务**。库由 `deploy/ipintel/fetch-dbs.sh`（systemd timer `fleet-ipintel-fetch` 每月一次）下载到 `[ipintel] db_dir`（默认 `/var/lib/fleet/ipintel`），文件 mtime 变化后 5 分钟内自动热重载。库缺失或不全时降级：能查多少查多少，查不到的字段为 null，接口不报错。ASN 分类清单内置在 `crates/fleet-ipintel/data/asn-lists.toml`，可用 `/etc/fleet/ipintel-overrides.toml`（同样的 `[[vpn]] asn=[…] name="…"` 写法，`name=""` 表示删除）覆盖或追加。线路机和节点的 `primary_ip` 自动算「内部地址」（`[ipintel] internal_ips` 可追加运维出口）。全部为只读 GET，读权限即可。

画像摘要 `intel`：`{isp, asn, as_org, country, province, city, place, conn_type(residential|mobile|datacenter|vpn|unknown), conn_label, confidence(high|medium|low), reason, internal(我方机器名|null), private}`。`/access/sources` 与 `/machines/{id}/access` 的来源条目（`top_sources`）都带 `intel`，查不到为 null。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/ipintel/{ip}` | 单个地址的完整画像（`isp asn as_org country region city lat lon type type_word type_confidence type_reason type_alt flags sources dbs{四库原始结果}`）加 `degraded`（库没加载全）、`loaded`。非法 IP 400 |
| GET | `/ipintel?ips=a,b,c` | ≤200 个，`{items:[{ip, intel}|{ip, intel:null, error}], loaded, degraded}`；不写缓存 |
| GET | `/ipintel/status` | `{enabled, installed, loaded, expected:5, db_dir, version, files:[{id, loaded, bytes, mtime, build, entries}], errors, lists（各清单条目数）, overrides{path,used,error}, cache_rows, hint}`；未安装时 `hint` 提示运行 `lab/install-ipintel.sh` |
| GET | `/machines/{id}/sources?window=24h\|7d\|30d&limit=50` | `window` 不能比汇总的保留期长（超了 400）。节点的来源画像（线路机：分布与 top 来源只算经它的流量；`known` 基线与 `anomalies` 按节点记，取它名下入口的节点）：`distribution.{isp,city,province,country,conn_type}[{value,label,conns,sources,pct}]`（只统计外部来源；我方机器和非公网地址分别计入 `internal_conns` / `private_conns`）、`top_sources[{src_ip,conns,last_ts,intel}]`、`known.{isp,city,conn_type}[{value,label,first_seen,last_seen,conns,anomaly}]`（已学基线）、`learning{started_at,ends_at,learn_days,done,remaining_hours}`、`anomalies[]`（进行中的 ipintel 告警）、`empty`、`degraded`、`unresolved_sources` |

**异常告警**（warning，进 incident 表并走通知渠道）：节点第一条来源记录起 `learn_days`（默认 7）天内只学习；之后某节点出现新的运营商 → `ipintel_new_isp`，新的城市 → `ipintel_new_city`，某 (节点, 来源 IP) 第一次出现且为机房/VPN → `ipintel_datacenter_source`。跳过非公网地址和我方机器。一次性异常最后一次出现 `alert_ttl_hours`（默认 24）小时后自动恢复，不发恢复通知；同一个值恢复后，只要它还在来源画像里就不会再告警。来源画像（运营商、城市、接入方式、机房/VPN 来源 IP，一个值一行）最近一次出现超过 `profile_days`（见「访问记录保留期」）、且不在告警中的由每小时的清理删掉：之后同一个值再出现算「新的」，会再告警一次；一个节点的画像全删光（闲置超过这么久）后再有流量，学习期从头算。后台每 5 分钟从 `access_source_hourly` 增量维护 `node_source_seen`（水位在 `ipintel_state`；水位之前的小时若被补算，连接数差值经 `access_source_late` 补记，首次扫描按 7 天一页）。

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

**恢复作业 `restore_vps` / `restore_relay` / `restore_proxy`**（目标 = 机器，要求 provisioning）：机器保持原 id（历史、账单、审计接上），lifecycle retired → provisioning，步骤与同类型上架完全相同（vps：preflight、bootstrap、converge、agent、wait_report、attach、wait_applied、verify、activate；relay：…、converge_relay、…；proxy：只有 attach、wait_applied、verify、activate）。旧入口已 removed，不复活，`attach` 建新入口。成功后 `retired_at` 清空，`lifecycle_reason` 记「已恢复：<原因>」，订阅保留。取消（或失败后取消）：机器回到 retired（团队、编号、退役原因回到恢复之前），摘掉已建的入口、撤销令牌、清掉出站秘密（出站参数的其余部分留着），团队编号压缩。

预检 `GET /machines/{id}/restore` 返回：`{id, kind, lifecycle, ip, old_name, name（恢复后的名字）, team:{id,name}|null, number, label, restorable, blockers[], warnings[], ip_conflict:{machine_id,name,lifecycle}|null, last_relay:{id,name}|null, retired_reason, ssh:{reachable,key_login:ok|denied|error|unreachable,host_key?,host_key_changed?}|null, outbound:{protocol,host,port,username}|null, needs:{relay,upstream,upstream_password,password:required|optional|no}}`。`outbound` 是退役时留下的出站参数（不含秘密，迁移 0029 起退役、取消时只清秘密）：VPS 节点是 SS 端口（NAT 映射的公网端口），家宽代理是上游；之前退役的为 null。`needs.upstream`：家宽代理没留下上游，要给全；`needs.upstream_password`：留下的上游有用户名，密码退役时已清掉，要重新给。`host_key_changed` 只在退役时存过主机公钥（`machine.retired_host_keys`，迁移 0023 起退役、取消时记）且探测拿到了公钥时出现：为 true 时 blockers 里有「主机指纹与退役时不同（退役时 …，现在 …）：确认机器重装过再勾选「重置主机指纹」…」；查询参数 `reset_host_key=true` 时这句改放 warnings、不阻断（`POST` 不探测，不看这一条；真正的拦截在作业预检，见下）。编号默认取团队里最小的空号（编号连续时就是 N+1），可用 `number` 指定空号；线路机按 label 唯一约束检查，原名被占用时用 `label` 改名。`ssh` 是服务端用中控密钥试一次登录（临时 known_hosts，不碰正式的）的结果，`probe=false` 跳过；试登录和 `POST` 恢复一样要 `write` scope（只读密钥 403，可加 `probe=false`），并写审计 `restore.probe`；`needs.password=required` 表示中控密钥被拒、必须给 root 密码。

`POST` 请求体：`{team_id?, number?, label?, relay_id?（节点要接入的在用线路机，节点必填）, login?:{password}（只用来推一次中控公钥，用完即丢）, upstream?:{protocol?,host?,port?,username?,password?}（proxy：没给的字段沿用退役时留下的，username 给空串 = 去掉；有用户名时 password 必填（退役时已清掉）；没留下上游的要给全，否则 400）, ss_port?（vps，公网（NAT 映射）端口，不给沿用退役前的，都没有为 8388；节点上 ssserver 固定监听 8388；vps 恢复时重新生成 SS 密码）, reason?, reset_host_key?（机器重装过，允许预检按现在的公钥重新记录）}`。主机指纹：退役时从 known_hosts 删掉的公钥存在机器行上，恢复作业的 `preflight` 先放回 known_hosts（已有这台主机的记录则不动）再核对，不符时失败、`error_kind=host_key`，不会把 root 密码和中控密钥交给 IP 上现在的主机；确认重装过后 `POST /jobs/{id}/retry {reset_host_key:true}` 或一开始就带 `reset_host_key:true`（不放回、照旧重新记录）。恢复成功后清空存的公钥。迁移 0023 之前退役的机器没存过，照旧按现在的公钥重新记录。

**同机 IP 节点的恢复**（作业 `restore_ip`，步骤同 `provision_ip`）：宿主机必须在用，否则预检 `restorable=false`、blockers 里有「宿主机「名字」现在是「…」：同机 IP 节点要等宿主机在用才能恢复」，`POST` 409。预检不试 SSH（`ssh` 为 null、`needs.password` 为 `"no"`）；`login`、`ss_port`、`reset_host_key` 不适用（忽略）；SS 端口沿用原来的，已被宿主机上别的同机节点占用（或迁移 0029 之前退役、没留下）时重新分配，SS 密码重新生成。

## 访问策略（域名黑白名单）
按团队或节点设置：有效策略 = 节点策略（`mode≠inherit`）> 团队策略 > 默认 `observe`。模式：`observe`（不产生规则）、`allowlist`（只放行名单内域名，其余阻止）、`denylist`（阻止名单内域名，其余放行）、`blocked`（暂停该节点所有新连接）、`inherit`（仅节点：跟随团队）。`allowlist`/`denylist` 至少一个域名。GET 需 `read`，写操作需 `write` 并写审计；预览是 POST 但无副作用（也需 `write`）。

**域名规范化**：逗号 / 中文逗号 / 顿号 / 分号 / 空白分隔，小写，去尾点；只接受 ASCII（国际化域名填 `xn--`）；每份 ≤300 个；必须含点、每标签 ≤63 且只含字母数字连字符（不以连字符开头结尾）、总长 ≤253；不能是 IP，不能带 URL、端口、通配符 `*`、正则或 `domain:` 之类前缀。`=example.com` 表示精确匹配（Xray `full:`），否则含子域（`domain:`）。保存时排序去重。错误信息中文并点名是哪一个域名（400）。

**草稿与发布**：`PUT` 只改草稿（`policy` 表的 `draft` + `revision` 乐观锁，`expected_revision` 与当前不符 409「草稿已被修改，请重新加载」；还没有记录的范围 revision 为 0）。草稿与生效值相同时草稿自动清除。`draft_hash` = 所有草稿规范化 JSON 加当前生效版本号（最新版本的 `seq`）的 sha256（十六进制），预览返回，发布 / 丢弃必须带上且一致（否则 409「草稿或生效版本已变化，请重新预览」：预览后草稿被改、或有人发布 / 回滚过，都要重新预览）。发布把所有草稿生效成新版本（`policy_version` 存完整快照），对**编译结果有变化**的节点所在的线路机代数 +1（每台只 +1 一次）。回滚 = 把旧版本快照作为新版本发布。版本状态由各线路机的 `applied_generation` 推导：`applying`（有线路机还没应用）→ `applied`；线路机回报应用失败且未应用 → `failed`；已退役的线路机不再等待（`gone`）。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/policies` | `{draft_hash, draft_count, teams:[{id,name,policy,nodes:[{id,name,number,has_entry,policy,effective,draft_effective}]}]}`。`policy`（范围自己的）= `{scope,id,mode,domains,note,draft:{mode,domains,note}\|null,revision,updated_by,updated_at}`；`effective`/`draft_effective`（仅节点）= `{mode,domains,note,source:"node"\|"team"\|"default"}`，分别是生效值与「草稿全部生效后」的值 |
| PUT | `/policies/{scope}/{id}` | `scope=team\|node`；body `{mode, domains:[…]\|"文本", note, expected_revision}` → `{policy, draft_hash, draft_count}`。团队不能 `inherit`；线路机没有策略；不用域名的模式（observe、blocked、inherit）存的域名一律为空（传了也丢掉）；与生效值相同的草稿存成没有草稿 |
| POST | `/policies/preview` | 按最近 24 小时 `access_hourly` 模拟：`{draft_hash, draft_count, window_hours, affected_relays, changes:[{scope,id,name,from:{mode,domains},to:{…},domains_changed,note_changed}]（`domains` 是个数；`domains_changed` / `note_changed`：域名、备注改没改）, nodes:[{node_id,name,team,mode,source,changed,total_conns,would_block,current_blocked,domains:[{host,conns}]（前 20）}]}`。只列草稿生效后非 observe 或有效策略有变化的节点；随机子域名合并行（`*.x.com`）按「整棵子树落在 domain 规则里」近似匹配 |
| POST | `/policies/publish` | body `{draft_hash}` → 新版本（同 versions 的一项） |
| POST | `/policies/discard` | body `{draft_hash}` → `{discarded}`（丢弃了几份草稿；只算与生效值不同的） |
| GET | `/policies/versions` | 最近 30 个：`[{id,seq,kind:"publish"\|"rollback",rollback_of_seq,actor,created_at,status:"applying"\|"applied"\|"failed",changes,relays:[{relay_id,name,generation,applied_generation,status,error}],is_latest,is_current,rollbackable}]`，`seq` 大的在前；`is_latest` = 最新一版（就是当前生效的）；`is_current` = 快照与当前生效值一致（规范化、去掉已删除的团队与节点后比，同回滚；不可回滚）。回滚会改什么按版本单独取（下一行） |
| GET | `/policies/versions/{id}/rollback` | 回滚预览：`{seq, rollbackable, changes}`，`changes` = 回滚到它会改什么（当前 → 这个版本，形状同版本的 `changes`）；与当前一致时 `rollbackable` 为 false、`changes` 为空。版本不存在 404 |
| POST | `/policies/versions/{id}/rollback` | 与当前一致时 409 → 新版本。草稿不动，只清掉正好等于回滚后值的草稿 |

**下发**：`DesiredEntry.policy`（`fleet-proto`，`#[serde(default, skip_serializing_if = "Option::is_none")]`，旧 agent 忽略、新 agent 收不到 = observe）：`{"mode":"allowlist"|"denylist"|"blocked","domains":["domain:example.com","full:exact.org"]}`；observe 不带该字段。agent 在该入口默认路由规则之前插入 `policy-<入口id>-match`（名单内：allowlist 走原出站、denylist 走 `blocked`）与 `policy-<入口id>-default`（allowlist/blocked：其余走 `blocked`），只匹配该入口自己的 inboundTag——回环探测入站不受影响；规则变化经 `adrules` 整表替换热加载，不重启 Xray。被策略阻止的连接在访问日志里是 `[in-… -> blocked]`，访问监控里计为已阻止。入站 sniffing 保持 `routeOnly=false`（域名匹配靠它改写目标地址；改成 true 会让「客户端给 IP」的连接不再由线路机侧解析域名）。
**健康**：节点有效策略为 `blocked` 时，健康原因里加一条 `policy_blocked`（severity `info`，「访问策略：已暂停新连接」）；info 级原因不降级、不开告警。
**实时**：实时流 `{type:"policies"}`（草稿 / 发布 / 回滚 / 丢弃）。
