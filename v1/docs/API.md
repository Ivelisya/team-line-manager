# fleet-server API v1

- 前缀 `/api/v1`。成功 `{"data": …}`；失败 `{"error":{"code","message"}}`（message 是给人看的中文）。
- 鉴权：会话 Cookie `fleet_session`（`POST /auth/login` 设置）；或 `Authorization: Bearer <api key>`（见下「API 密钥与 scope」）。
- 会话的写请求（POST/PUT/PATCH/DELETE）必须带头 `X-Fleet-Request: 1`（防 CSRF）；API 密钥不是浏览器，不要求这个头。
- 响应头：`/api` 的响应带 `X-Fleet-Version`（中控版本，`0.1.0+<提交哈希>`）；fleetctl 与它不一致时在 stderr 提示一次（不影响 stdout 与 `--json`）。所有响应带 `X-Content-Type-Options: nosniff`、`Referrer-Policy: same-origin`、`X-Frame-Options: DENY`；网页（`/api`、`/agent`、`/echo` 以外）另带 CSP（`script-src 'self'`，不许内联脚本与外部资源）；`public_url` 是 https 时带 HSTS。接口没自己设 `Cache-Control` 的一律 `no-store`。
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
| `secrets` | `GET /entries/{id}/share` 这类凭据；客户端订阅的管理 `/client-subs`（读写都要） |
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
| GET | `/meta` | 只在中控定义一次的枚举中文名、阈值、默认值与组织时区（服务端 `src/meta.rs`；一个会话里不变，网页启动时取一次，fleetctl 每条命令要打印中文名或时间时取一次）。`{org_timezone（fleet.toml 的 org_timezone，默认 Asia/Shanghai）, org_timezone_label（「北京时间」或 IANA 名，通知、审计、fleetctl 时间的括注）, org_utc_offset_s（组织时区现在的 UTC 偏移秒数，每小时刷新）, timezones:[…]（时区下拉的常用项；不在里面的照样能填，中控按 PG 时区库校验）, currencies:[…]（币种下拉的常用项；校验只要求 3 个字母）, cycles:[{key,label,months}], kinds, ip_node_label, lifecycles, healths, job_kinds, job_states, step_states, entry_states, traffic_rules, traffic_resets, due_states, policy_modes, rules（告警规则）: {key: 中文名}, severities:{key:{label,tone}}, thresholds:{traffic_quota_warn_pct, traffic_quota_over_pct, usage:{cpu,mem,disk,load:{warn,bad}}}（bad 与持续高负载告警同值；负载不告警）, defaults:{currency,cycle,remind_days,traffic_rule,traffic_reset,ss_method,ss_port}, terminal:{per_user, global, idle_s, max_age_s, max_message, max_dim, ticket_ttl_s, verify_ttl_s}（网页终端的上限，见「网页终端」）, ssh_public_key（中控公钥全文，即配置 `ssh_key` 旁的 `.pub`；读不到为 null。上架页「复制中控公钥」用：供应商面板录入它之后上架不用填 root 密码）}`。叫法：`vps` =「VPS 节点」、`proxy` =「家宽代理」、`relay` =「线路机」，加在宿主机上的 vps 是「同机 IP 节点」；健康只用 正常 / 降级 / 故障 / 未知 / 维护。读权限 |

## 目录
| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/teams` | `[{id,name,sort_order,spare,node_count,monthly_usd}]`（`monthly_usd`：团队里在用（active）机器的月费合计折成 USD，形状同总览的 `monthly_usd`，见「汇率」；`spare`：唯一的备用团队，系统管理（迁移 0041 建好，唯一索引 `team_one_spare`）：退役的机器放这里，在用、有订阅的订阅过期满 1 天后中控清理并移进回收站，见「退役与回收站」；机器视图的 `spare` 同它） |
| POST | `/teams` | `{name,sort_order?}`（建不了备用团队，`spare` 不收） |
| PATCH | `/teams/{id}` | `{name?,sort_order?}`（改团队名 = 该团队所有节点显示名一起变；备用团队也能改名，如「团队2 备用」→「备用」；不能取消备用） |
| DELETE | `/teams/{id}` | 备用团队 409（不能删）。还有机器（含已退役的、回收站里等待清除的）或软路由时 409（写明各有几台）；没有才删，团队自己的访问规则一起删。审计 `team.delete` |
| GET | `/providers` | `[{id,name,code,panel_url,timezone,traffic_rule,traffic_reset,remind_days}]`；`traffic_rule` 是这家怎么计量流量额度（网卡本周期入、出总量）：`min` 取小（如 GOMAMI）/ `max` 取大 / `sum` 相加 / `out` 只算出站。供应商同时是名下机器订阅的**模板**：`timezone`（账单时区）、`traffic_reset`（流量周期：`month` 账单时区的自然月、每月 1 号起；`billing` 账单日，即每台机器到期日是几号就几号起，月份没有这一天按月末；`rolling30` 滚动 30 天，账单时区的今天往前 29 天起、没有重置日，如 Blossom）、`remind_days`（到期前几天提醒，0–60）——订阅没单独设置这几项的跟随它，改了立即生效 |
| POST | `/providers` | `{name,code,panel_url?,timezone?,traffic_rule?,traffic_reset?,remind_days?}`；`traffic_rule` 不传 = `max`（不知道的供应商按取大，不会少算），`traffic_reset` 不传 = `month`，`remind_days` 不传 = 7，别的值 400；`panel_url` 须是 http(s) 地址（400）；`timezone` 不传 / null / 空串 = 存 null、跟随组织时区（`/meta` 的 org_timezone），否则须是 `at time zone` 认的时区（400）；名称或缩写重复 409 |
| PATCH | `/providers/{id}` | `{name?,code?,panel_url?,timezone?,traffic_rule?,traffic_reset?,remind_days?}`：没传的不改（`traffic_rule` 改了立即影响这家机器的 `quota.used`；`timezone` / `traffic_reset` / `remind_days` 改了立即影响没单独设置这几项的订阅）；`panel_url` null / 空串 = 清空；`timezone` null / 空串 = 改回跟随组织时区（null）。校验同 POST（`panel_url` 和库里现有的一样时不校验：加 http(s) 校验之前存下的旧值不挡改别的字段）。写审计 `provider.update` |
| DELETE | `/providers/{id}` | 还有套餐（含已归档的）或机器（含已退役的）时 409；没有才删。审计 `provider.delete` |
| GET | `/plans` | `[{id,provider_id,name,cpu_cores,mem_mb,disk_gb,bandwidth_mbps,traffic_gb,price_cents,currency,cycle,notes,archived_at,machine_count}]`。套餐是机器订阅的**模板**：`price_cents` / `currency` / `cycle`（每个计费周期的价格）与 `traffic_gb`（每个流量周期的额度，十进制 GB，null 或 0 = 不限）——用这个套餐的机器没单独设置的跟随它，改了立即生效。`machine_count`：在用（未退役）的机器数。`price_usd_cents` / `monthly_usd_cents`（每期价格、按周期折成的月费，折成 USD 的分）与 `usd_per_unit`（所用汇率）由中控按汇率算，没有该币种的汇率时都是 null（见「汇率」） |
| POST | `/plans` | 同上字段（不含 id/统计）。校验（PATCH 同，只看传了的）：`name` 1–32 字，`price_cents` ≥ 0，`currency` 3 个字母（存成大写，套餐、订阅、上架同一个校验），`cycle` 只能是 `/meta` 的 cycles，`notes` ≤ 2000 字，否则 400 |
| PATCH | `/plans/{id}` | 任意字段（没传的不改；规格类字段传 null = 清空）；`archived:true` 归档。机器的供应商就是套餐的：有机器（含已退役的）用着时改 `provider_id` 409（给新供应商另建套餐）。审计 `plan.update` 记套餐名、币种与改了的字段 `{name, currency, before:{…}, after:{…}}` |
| DELETE | `/plans/{id}` | 还有机器（含已退役的）用着时 409（不再上架的可以归档）；没有才删。审计 `plan.delete` |

## 机器
`MachineView`（列表与详情同形）：
```
{ id, kind: "relay"|"vps"|"proxy", name, team:{id,name}|null, number|null, label|null, ip, ssh_port,
  spare, spare_from|null, spare_since|null, recycle_on|null,
                                // spare：在备用团队里（= 已退役、等到期回收；机器照常运行）。spare_from：进备用前的显示名（「团队2-07」），
                                // spare_since：进备用的时刻（即退役时间，RFC3339），不在备用团队为 null（之前就在备用团队、没经退役进来的也为 null）。
                                // recycle_on：备用团队里在用、有订阅的机器自动回收的日子（账单时区，到期日 + 2 天，这天 0 点起回收，"2026-11-17"），其余为 null
  country|null, country_set|null, // 国家的 ISO 3166-1 两位代码（"US"），网页在名字旁显示国旗。country_set 是上架 / 修改时手动指定的；
                                // 没指定时 country 按 ip 查中控离线 IP 库，内网地址、家宽代理填的域名、库里查不到的为 null
  host_machine:{id,name}|null,  // 同机 IP 节点（kind 仍是 vps）：它所在的宿主机；它没有自己的 agent 和 SSH，下面的 agent、host 就是宿主机的，
                                // ssh_port 是宿主机的（只展示），provider 取宿主机的、plan 为 null；其它机器为 null
  ip_nodes:[{id,name}],         // 宿主机：它上面未退役的同机 IP 节点（按团队、编号）；其它机器为 []
  tune:{profile|null, at|null, current, params|null}|null,   // 系统调优记录：profile = 最近一次成功应用的调优档（tune-v2；null = 没调优过或已撤销），at = 那次的完成时间，
                                // current = 就是中控当前的档（false = 旧档如 tune-v1，或没调优过）；params = 那次的推导参数
                                // {bw, bw_plan|null, bw_source:"plan"|"default", rtt, rtt_source:"sampled"|"default", rtt_samples, mem_mb, buf_max}（tune-v2 起；旧档、未调优为 null）；
                                // 家宽代理、同机 IP 节点（系统设置是宿主机的）为 null
  provider:{id,name}|null, provider_code|null, plan:{id,name,bandwidth_mbps,traffic_gb}|null, service_ref|null,
                                // provider：有套餐时是套餐的供应商（机器上存的那份在上架、改套餐时按套餐写），没有套餐取机器的
  lifecycle: "draft"|"provisioning"|"active"|"retiring"|"retired"|"failed", lifecycle_reason|null, note, revision,
  created_at, activated_at|null, retired_at|null,
  subscription: {price_cents,currency,cycle,due_date,billing_day,due_days,due_state,billing_tz,auto_renew,remind_days,traffic_gb,traffic_reset_day,overrides,monthly_cents,monthly_usd_cents,price_usd_cents,usd_per_unit,next_due,period_days}|null,
                                // due_state：ok / soon（到期前 remind_days 天内、不是自动续费）/ overdue（已过期），续费告警、总览、网页、fleetctl 都按它；
                                // monthly_cents：折成每月的费用（分，四舍五入）；next_due：到期日再推一个周期（登记续费的默认新到期日，落在账单日，小月按月末）；billing_day：账单日（每月几号，见续费）；
                                // period_days：当前这一期的天数（到期日往前推一个周期到到期日），网页算剩余价值
                                // monthly_usd_cents / price_usd_cents：月费、每期价格折成 USD（分，round(原币种分 × usd_per_unit)，四舍五入到分）；usd_per_unit：所用汇率（1 单位合多少 USD，USD 为 1）；
                                // 没有该币种的汇率时三个都是 null（未折算，不当 1:1），见「汇率」。网页的月费、剩余价值都按 USD 显示
                                // 全是生效值（字段名同以前，旧调用方照用）：订阅只存到期日、自动续费和「单独设置」的项，其余跟随 sub_template——
                                // price_cents / currency / cycle / traffic_gb 跟随套餐，billing_tz / remind_days / traffic_reset_day 跟随供应商。
                                // overrides：单独设置了的字段名（如 ["price_cents"]），不在里面的就是跟随的。traffic_gb：0 / null = 不限。
                                // traffic_reset_day：单独设置的，或供应商 traffic_reset = billing 时是账单日（billing_day）、month 时 1、rolling30（滚动 30 天）时 null
  sub_template: {price_cents|null, currency, cycle, billing_tz, remind_days, traffic_gb|null, traffic_reset},
                                // 订阅模板（没有订阅也给）：套餐的价格 / 币种 / 周期 / 流量（没有套餐时 price_cents、traffic_gb 为 null，币种周期 USD / monthly），
                                // 供应商的时区 / 提醒天数 / 重置方式（供应商没填时区或没有供应商时是组织时区，没有供应商时 7 / month）。网页「跟随套餐：2000 GB」就是它
  agent: {version,last_report_at,online,xray_running,cores,health,clock_skew_ms,nic_ifaces}|null,  // proxy 没有；cores 为 agent 报的可用核数（容器里是限额），没报过为 null
                                // nic_ifaces：agent 计量流量用的网卡（如 ["ens3"]：默认路由所在网卡 ∪ 配有公网地址的物理网卡，
                                // 或 agent.toml 的 nic_ifaces），旧 agent 没报过为 []；不随实时推送更新
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
  nic: {today_rx,today_tx,month_rx,month_tx}|null,     // 按网卡计的今天 / 本月流量（字节，组织时区的今天 / 本月，同入口的 traffic）：rx = 下行（本机收），
                                // tx = 上行（本机发），只算 agent.nic_ifaces 那几块网卡。有自己 agent 的机器（线路机、VPS 节点、宿主机）才有，
                                // 家宽代理、同机 IP 节点为 null（看业务流量：entries[].traffic + removed_traffic）。来自小时汇总表，每次汇总后更新。
                                // 不做全站合计：同一个字节在线路机和节点的网卡上各算一次
  month_counted_from: "RFC 3339"|null,  // 本月（组织时区）的数据从这一刻才有：首次上架完成（activated_at）晚于本月 1 号 0 点时为它，否则 null。
                                // 网卡与业务的本月数都适用；之前的用量不在内（供应商控制台的数会更大），下个月自动为 null
  quota: {limit_gb, limit_source, used, period_start, period_end, mode, counted_from, rule, base, base_at, daily_avg, forecast_full_at},  // 流量额度：生效的上限、周期已用（字节）与周期起止（日期）。
                                // limit_gb：生效的上限（十进制 GB），没有上限为 null；告警、网页、fleetctl 都按它，不各自再算。
                                // limit_source：subscription（订阅单独设置）/ plan（跟随套餐，没有订阅时也是套餐的）/
                                // host（同机 IP 节点没有自己的上限，与宿主机共用：宿主机的已用含它的流量）/ none（不限）。
                                // 有自己 agent 的机器（线路机、VPS 节点、宿主机）按网卡：本周期物理网卡入、出总量按供应商的 traffic_rule
                                // 取小 / 取大 / 相加 / 只取出站（没有供应商按 max），rule 即所用规则；宿主机的网卡也走了它的同机 IP 节点的流量。
                                // 家宽代理和同机 IP 节点没有自己的网卡读数：rule 为 null，used 是业务流量（下载+上传，含已移除入口）。
                                // base / base_at：接入前读数——割接时按供应商面板录的本周期已用（字节）与录入时间，只读（PUT subscription 已不收新值），
                                // used = base + 录入时刻所在整点之后的用量（那一小时里录入之后的部分不计，至多差 1 小时）；
                                // 录入时间早于周期起点（已进入新周期）时自动失效，二者为 null。之后的版本删掉这两个字段。
                                // mode：monthly = 按月重置：有订阅时是账单时区里今天或之前最近一次（生效的）重置日起（月份没有这一天按月末，
                                // 如 31 号在 9 月是 30 号、2 月是 28 / 29 号），从该日账单时区零点累计；没有订阅：sub_template.billing_tz 的本月。
                                // rolling30 = 滚动 30 天（供应商 traffic_reset = rolling30 且订阅没单独设置重置日）：period_start = 账单时区的今天往前 29 天
                                // （含今天共 30 个日历天），每天往前挪一天，界面写「近 30 天已用」。
                                // period_end：下次重置日（这一天账单时区零点起是新周期）；rolling30 为 null。
                                // counted_from：本周期的数据从这一刻才有——首次上架完成（activated_at）晚于周期起点、且没有接入前读数时为它，否则 null；
                                // 之前的用量不在 used 里，实际剩余可能更少（rolling30 的新机器头 30 天都带）。来自小时汇总表，每次汇总后更新。
                                // daily_avg：近 7 天日均用量（字节 / 天，口径同 used 但不含接入前读数；周期或上架不足 7 天按已有天数，不足 1 天为 null）。
                                // forecast_full_at：按 daily_avg 预计用满的时刻（精确到小时），只在早于 period_end 时给出——已用满、没有上限、日均为 0、
                                // 滚动 30 天（没有重置日）、daily_avg 为 null 时为 null。界面写「预计 11-02 用满（重置 11-13）」，fleetctl billing / show 同一句
                                // 不限流量（limit_gb 为 null）的机器 used 照算，界面只显示本月（nic 或业务流量），不显示本周期
  health: {status: "ok"|"degraded"|"down"|"unknown"|"maintenance"|"inactive", reasons:[{key,severity,text}]},
  active_job: {id,kind,state}|null,                    // 进行中的作业（queued/running/waiting/cancelling）
  failed_job: {id,kind,state}|null,                    // lifecycle = failed（上架失败）时：失败的上架作业，重试回到 provisioning，取消则补偿退役；
                                                       // lifecycle = retiring 且退役作业失败时：该 retire 作业（等重试）；其它情况 null
  open_incidents, maintenance,   // maintenance：同机 IP 节点在宿主机维护时也为 true
  relay: {public_host,generation,applied_generation,applied_at,apply_error,port_min,port_max}|null }
EntryView = { id, relay:{id,name}, node:{id,name}, direct, port, lifecycle, applied, apply_error,
  tcp:{ok,latency_ms,detail,ts,median_ms}|null, egress:{…}|null, reach:{ok,ts,detail,fails}|null, traffic:{today_up,today_down,month_up,month_down} }
// direct：线路机直出入口——节点就是线路机自己（node.id = relay.id，node.name =「线路机名-直出」），出口是线路机自己的 IP；
//   只出现在线路机的 entries 里（一次）；没有 TCP 探测（tcp 恒为 null），出口不通时线路机自己报 egress_fail
// tcp：线路机 → 节点出站地址的 TCP 建连，latency_ms 是最近一次（新 agent 报内核量的往返时间，不含 SYN 重发等待），
//   median_ms 是近 1 小时成功样本的中位数（卡片读数）；egress：经入口访问 /echo 核对出口 IP，只判通不通、IP 对不对，
//   latency_ms 是这次出口验证的耗时（经节点去中控），不是线路延迟，median_ms 恒为 null
// reach：主探测来源（中控或探测点，见「探测点」；GET /settings/reach-source 说此刻是哪一路）→ 线路机上这个入口端口的最近一次 TCP 建连
//   （只建连，不握手），`detail` 是失败原因（带「端口 N：」前缀），通为 null；只有在用入口有，没检查过（新入口、停用入口、线路机没在探、
//   主来源离线且中控的检查关着）为 null。轮流连各端口，每个口隔「入口数 × 30 秒」才更新一次；
//   随完整加载更新（至多 5 分钟，连通结论变了立即），不在 metrics 推送里。连续 3 次连不上同时开 relay_unreachable 健康原因（告警再去抖 60 秒）；
//   `fails` 是这个端口到最近一次为止连续连不上的次数（通的为 0），网页 ≥ 3 才算入口不可达（「入口可达 x/y」里不算可达、入口表写「入口不可达」），1–2 次写「刚才连不上 N 次」
```
| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/machines?scope=live\|all` | 默认 live（未退役）；all = 还没进回收站的全部机器（含退役完、还没收进回收站的，最多一小时）；回收站里的看 `GET /recycle-bin`；别的值 400（`retired` 已删）。排序：线路机在前，然后按团队、编号。加 `view=brief` 只返回 `{id, kind, name, label, ip, lifecycle, host_id, team:{id,name}\|null}`（fleetctl 按名字找机器用；`host_id` 非 null 的是同机 IP 节点） |
| GET | `/machines/{id}` | 单台 |
| PATCH | `/machines/{id}` | `{revision, team_id?, number?, label?, note?, provider_id?, plan_id?, service_ref?, country?}`。country 是 ISO 3166-1 两位代码（不在表里 400，「UK」提示英国是 GB），null 或空串 = 改回按 IP 自动。改团队/编号 = 改名（换团队不给编号时排到新团队的最小空号）：线路机备注随后自动同步，不换令牌、不断连接。有套餐时供应商就是套餐的：只改了套餐的，供应商跟着换；同时给了另一个供应商 400 |
| GET | `/machines/{id}/series?metric=host\|traffic\|probe&range=1h\|6h\|24h\|7d\|30d` | 时序。≤24h 用原始点（1 分钟聚合），更长用小时汇总。同机 IP 节点的 `metric=host` 是宿主机的曲线 |
| GET | `/machines/{id}/availability?days=30` | 近 N 天（`days` 1–90，默认 30，越界 400）每天的探测成功率：`{data:{days, items:[{date,tcp_total,tcp_ok,egress_total,egress_ok}], tcp_pct, egress_pct}}`。探测小时汇总按这台机器生效的账单时区（没有订阅按 `sub_template.billing_tz`）分到日历天（一个小时桶按起点归天），`items` 旧→新共 `days` 项、最后一项是今天（还没过完），没有探测的天各项为 0；`tcp_pct` / `egress_pct` 是整段的成功率（0–100），没有探测为 null。入口同 `series`：节点取自己的，线路机取挂在它上面的（含已移除的）。机器不存在 404 |
| GET | `/machines/{id}/retire` | 退役预览，见「退役与回收站」 |
| POST | `/machines/{id}/retire` | 退役：默认移进备用团队，`now: true` 立即回收（退役作业），见「退役与回收站」 |
| POST | `/machines/{id}/upgrade-agent` | 推送中控当前版本的 agent 并重启（作业 `upgrade_agent`；令牌顺带轮换） → `{data:{machine_id,name,kind,job_id,after:null}}`。线路机顺带收敛 fleet-xray 单元（变了只 daemon-reload，不重启 Xray，等它下次重启才生效）与访问日志轮转（配置 + 每小时的 fleet-xray-logrotate.timer），这两样没装上只记警告。**回滚**：推送时机器上的旧程序留成同目录的 `/usr/local/bin/fleet-agent.prev`（硬链接，新程序原子替换；上一次留的被覆盖，成功后也留着）；重启后新版本起不来（隔 3 秒主进程换了，或不是 active），或 2 分钟内没有用新令牌、以新版本号上报，就把 .prev 换回去并重启，作业失败，错误写明「新版本 X 起不来（…）/ 重启后 120 秒没有上报，已回滚到 <旧版本>」；机器上没有 .prev 或换回去也没起来时写明需要人工处理。程序和 dist 里的相同（推送跳过）时不回滚。**线路机**另要新版本自己报的 Xray 状态做到：Xray 在跑、已应用的配置追上升级开始时的期望代数、没有应用错误（`relay.apply_error` 与 agent 自报的 `last_apply_error` 都算）；只查升级前（推送之前读的 agent 最近状态）就做到的项，记在作业参数 `xray_checks`、推送那一步输出 `xray_checks`。2 分钟做不到同样换回并失败，错误「新版本 X 重启后 120 秒线路机检查没通过（Xray 没在跑；已应用的配置代数 2，没到 3；应用配置出错：…），已回滚到 <旧版本>」（`error_kind` remote）。「新版本自己报的」：中控入库时给 `agent_state.info.xray` 记上报它的 agent 版本（`agent_version`）——重启后新进程最先补发旧进程退出时落盘的报告，之后几份不带 Xray 状态，按上报时间分不出来。同机 IP 节点 409（agent 是宿主机的，升级宿主机） |
| POST | `/machines/{id}/tune` | 重做系统调优（作业 `tune`，当前调优档 `tune-v2`，内容见 DESIGN.md §3）：缓冲区上限按这台的套餐带宽、RTT、内存推导，写内核网络参数、journald 体积上限、补时间同步，逐键直接写 `/proc/sys` 再读回，只有读回一致的键进 `/etc/sysctl.d/99-zz-fleet-tune.conf`；第一次动键前把原值记进机器上的 `/var/lib/fleet/tune-orig.conf`；不重启 ssserver / Xray、不断连接。请求体 `{rollback?:bool}`（默认 false；`{}` 即重做）：`rollback:true` = 撤销调优（同一个作业类型，参数 `{rollback:true}`）：按原值快照逐键写回、删掉 `99-zz-fleet-tune.conf` 与 `/etc/modules-load.d/90-fleet-conntrack.conf`，成功后机器的 `tune` = `{profile:null, at:null, current:false, params:null}`；机器上没有快照时作业失败（`remote`，什么都不改）。→ `{data:{machine_id,name,job_id}}`。同机 IP 节点 409「同机 IP 节点和宿主机共用系统设置：对宿主机「X」调优」，家宽代理 409「家宽代理没有我们的机器」，非在用 409，机器上有进行中的作业 409。作业只能在排队时取消。审计 `machine.tune`（`{name, profile, job_id}`；撤销为 `{name, rollback:true, job_id}`，摘要「X · 撤销调优」）。成功后机器的 `tune` = `{profile, at, current:true, params}`，不动 `revision` |
| POST | `/machines/{id}/ss-refresh` | 重新下发节点的 SS 配置（作业 `ss_refresh`）：在节点（宿主机）上跑与上架同一份 ssserver 配置脚本，把中控当前的配置模板（出站目标黑名单 `fleet.acl` 等）推到存量节点；SS 密码、端口从库里取，不做 SSH 加固 / 系统调优 / 装 agent、不推 ssserver 程序。脚本幂等：配置、黑名单、单元都没变就不重启，有变化重启一次该实例（现有连接断开几秒）；宿主机上在用的同机 IP 节点实例一起刷（没有入口的跳过）。无请求体（`{}`）。→ `{data:{machine_id,name,job_id}}`。同机 IP 节点 409「同机 IP 节点和宿主机共用 ssserver 的黑名单文件：对宿主机「X」下发，同机的实例一起刷」，线路机 409「线路机上没有落地节点的 ssserver」，家宽代理 409「家宽代理没有我们的 ssserver」，非在用 409，没有入口（没有线路机放行名单、也没法验收）409，机器上有进行中的作业 409。作业只能在排队时取消。审计 `machine.ss_refresh`（`{name, job_id}`，摘要为机器名） |
| POST | `/agents/upgrade` | 所有在用线路机与 VPS 节点（不含同机 IP 节点）各建一个 `upgrade_agent` 作业（有进行中作业的跳过；每台单独提交，一台冲突不影响其他台），但不一起跑：先升一台做**金丝雀**（有线路机先挑线路机；同类里在用入口少的在前，一样少按名字），它成功后其余线路机**一台接一台**，线路机都成功后 VPS 节点**并发**（作业引擎的并发上限 `job_workers`）。编排记在作业参数 `after`（要等的作业 id）：那个作业结束之前这个作业领取不到（一直是 queued）；它在推送之后失败了（新版本起不来、没上报、线路机检查没通过）或被取消，这个作业一步都不做、直接取消，`error` 写明原因（如「金丝雀「线路机2」升级失败，其余机器不再升级」「「线路机3」升级失败，后面的机器不再升级」「「线路机3」的升级没有完成（已取消），后面的机器不再升级」，再往后的沿用同一个原因），日志一条「没有开始：…」。它在推送之前就失败的（SSH 连不上、登录被拒、推送失败：机器上的程序没换过，与新版本无关）不算，这个作业照常开始——agent 步骤每次尝试先记作业参数 `pushed=false`，要用 dist 里的程序重启 agent 之前置 true，编排据此判断（没有记的按推送之后算）。排队中的作业可以取消（`POST /jobs/{id}/cancel`），等它的跟着取消。VPS 节点之间互不等待，一台失败不影响别的。发起时把 dist 里 agent 的 sha256 与版本号记进各作业参数 `rollout:{sha256,version}`，每个作业开始前、推送时各核对一次：中途重新部署过中控（dist 换了）就不推，正要推送的失败、没开始的直接取消，`error` 都是「中控上的 agent 在升级过程中换了版本（发起时是 X），请重新发起全部升级」。dist 里没有 agent 程序或读不出版本号（`--version`）时 409、不建作业 → `{data:{jobs:[{machine_id,name,kind,job_id,after}], skipped:[{machine_id,name,reason}], order}}`：`jobs` 按执行顺序（第一个是金丝雀，`after` 为 null）；`order` 是给人看的顺序说明，如「先升「线路机3」做金丝雀，成功后其余 2 台线路机逐台升级，线路机都成功后 40 台 VPS 节点并发升级；新版本在任何一台线路机上出问题，没开始的都会取消」（只有一台时「只有「X」一台」，一台都没建时「没有要升级的机器」） |
| PUT | `/machines/{id}/subscription` | `{due_date, price_cents?,currency?,cycle?,billing_tz?,remind_days?,traffic_gb?,traffic_reset_day?, auto_renew?}`。只有 `due_date` 必填。`price_cents` … `traffic_reset_day` 是**单独设置**，整份替换：没传或 null = 跟随模板（见 `sub_template`），给了值 = 这台单独设置；和模板相同的值也按跟随存（旧客户端整份带上生效值不会把套餐的值抄进来）。没有套餐的机器价格没有可跟随的，必须给 `price_cents`（400）。校验：`price_cents` / `traffic_gb` ≥ 0、`cycle` 四选一、`currency` 3 个字母（存成大写）、`remind_days` 0–60、`traffic_reset_day` 1–31、`billing_tz` 须是 `at time zone` 认的时区，否则 400。`auto_renew` 没传 = 不改（新建为否）。账单日（`billing_day`）：`due_date` 落在现有账单日上的（原样带回的到期日、小月的月末，如 31 号账单日的 2-28）不动，否则（或新建）记成 `due_date` 是几号。`traffic_gb` 是供应商的十进制 GB（10⁹ 字节，0 = 不限）。不再按面板校准：带 `traffic_used_gb`（数值或 null）400「流量已用改为由 agent 按网卡统计，不再按面板校准」；已录的接入前读数不受 PUT 影响，进入下一个周期自动失效（见 `quota.base`）。写审计 `subscription.set` |
| POST | `/machines/{id}/subscription/preview` | 请求体同上一条，只预览：同样校验、写入、规整，读出生效值后整个回滚（不写审计、不广播），返回 `{dry_run:true, effective:{due_date,price_cents,currency,cycle,billing_tz,remind_days,traffic_gb,traffic_reset_day,auto_renew,overrides[]}}`（fleetctl 用它在确认前显示「改前 → 改后」）。单独的路由而不是 PUT 的 query 参数：旧中控没有它（404），不会把预览当成真改；fleetctl 收到 404 报「中控版本较旧，先升级中控再用 subscription set」，不提交 |
| POST | `/machines/{id}/renewals` | `{new_due?, cycles?, amount_cents?, note?}`，`new_due` 与 `cycles` 给一个（都给、都不给、`cycles` 不在 1–120 时 400）：`cycles` = 中控按订阅生效的 `cycle` 从当前到期日推几个周期，落在账单日（`billing_day`，目标月没有这一天时落到月末；账单日不变，所以 31 号的 1-31 → 2-28 → 3-31，不会漂成 3-28；fleetctl `renew --cycles`）；`new_due` 直接给新到期日（网页对话框默认填 `subscription.next_due`），账单日同改订阅：落在现有账单日上的不动，否则改成它是几号。新到期日要晚于当前的。只顺延到期日，单独设置照旧 → `{id, old_due, new_due}`。**自动续费**（`auto_renew`）：中控每小时的维护在账单时区的今天晚于到期日时按周期顺延到不早于今天（账单日不变），每期记一条续费记录（`amount_cents` 取生效价格，`note`「自动续费」，`actor`「系统」）与审计 `renewal`（`detail.auto = true`，摘要「到期 … → …（自动续费）」）；先于备用到期判断，开了自动续费的备用机器不会被回收。只记账，不向供应商付款 |
| GET | `/machines/{id}/renewals` | |
| GET | `/teams/{id}/renumber` | 预览：`{data:{changes:[{machine_id,old_name,new_name}],notes:[string],token}}`。notes：各路由快照里指向要改名的节点的出口（出口名是路由上自己起的，不跟着改） |
| POST | `/teams/{id}/renumber` | `{token}`（预览返回的，防止预览后数据变了）→ 一次生效 |

## 上架
| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/provision/check` | 校验（不落库）：`{items:[ProvisionItem]}` → `{data:{items:[{ok,errors:[...],warnings:[...],name,number,ss_port}]}}`（`ss_port`：同机 IP 节点暂定的 SS 端口，其它为 null；`warnings` 是不影响 `ok` 的提醒）。还会探测 SSH 端口（同一批最多同时 8 个；连上后 5 秒内要读到 `SSH-` 开头的版本行才算通，不通时写明「连接被拒绝」「3 秒没有响应」「没有响应 SSH 握手」还是「不是 SSH 服务」）；端口通了再用中控密钥试登录一次（`BatchMode`、只用公钥、连接超时 5 秒、整个 10 秒，和端口探测共用那 8 个并发；不核对也不记录主机公钥——`StrictHostKeyChecking=no`、`UserKnownHostsFile=/dev/null`，记录指纹仍是上架作业第一步的事），只读 `/etc/os-release`：密钥被拒且没填 root 密码（`login.password` 为空等于没填）的报「中控密钥登录不了 ip:port（密钥被拒），也没填 root 密码。二选一：提供 root 密码…；或在供应商面板…录入中控公钥：<公钥全文>」（公钥全文在末尾）；填了密码的照旧通过（作业会用密码装上公钥）；登进去了而发行版已过标准支持期（`jobs/scripts.rs` 的 `OS_EOL`：Ubuntu < 22.04、Debian < 12）进 `warnings`；没填密码、试登录有别的问题（超时、被断开）也只进 `warnings`；同机 IP 节点登录宿主机核对这个 IP 在不在它的网卡上（同上架作业第一步；每台宿主机只登录一次，算一次探测）；整个检查最多 30 秒，到点没查完的条目报「检查超时」；回环、链路本地、未指定、组播地址直接报错不探测；按操作人限速（突发 300 次探测、每秒补 1 次，超了回 429 `rate_limited`） |
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
| POST | `/entries/{id}/actions` | `{action:"enable"\|"disable"\|"rotate", confirm?}`；rotate 需 `confirm="<线路机名>:<端口>"`，没给或不对 400，消息后面列出指向它的软路由出口、`detail.router_exits` 同下。→ `{generation, router_exits}`（停用、轮换时是指向它的软路由出口，启用为 `[]`）：轮换、停用不会改路由，这些出口之后连不上 |
| GET | `/entries/{id}/routers` | 指向这个入口的软路由出口（各路由最近一次采集的快照里 `exits[].entry.id` 是它的）：`[{id, name, state, exits:[出口名]}]`，路由按列表顺序；入口不存在 404。轮换、停用的确认里列出 |
| GET | `/entries/{id}/share` | `{data:{url}}`（vless:// 分享链接；记审计） |
| POST | `/machines/{node_id}/entries` | 给节点再挂一条入口：`{relay_id}` → 作业（`attach_entry`，审计 `entry.add`）。`relay_id` 指向的线路机不存在 400；线路机不在用、节点在它上面已有入口 409（不建作业）。**线路机直出入口**：路径和 `relay_id` 都是这台线路机（`POST /machines/{线路机}/entries {relay_id: 线路机}`），出站是 freedom、从线路机自己的公网 IP 出去，出口验收期望的就是线路机的 IP；每台线路机至多一条（409「这台线路机已经有直出入口」），`relay_id` 填别的线路机 409。端口、REALITY 参数、代数、等应用、出口验收、审计与普通入口相同；作业的「更新节点防火墙」一步跳过。分享链接 # 后是「线路机名-直出」，线路机改名后随期望状态同步 |

## 客户端订阅
自己的客户端（Clash Verge、OpenClash 的 proxy-provider、v2rayN / Shadowrocket）按一个链接拉入口列表。订阅 = 名称 + 令牌（32 字节随机，库里只存 sha256，新建、轮换时返回一次完整链接 `url`）+ 选择规则 + 启用状态 + 最近拉取。管理接口都要 `secrets`；新建、修改、轮换、删除写审计（`client_sub.create` / `update` / `rotate` / `delete`，记名称与规则摘要，不记链接）。

规则 `rule`（整份存、整份替换；存 id，不存入口参数）：`{team_mode: "all"|"only"|"except", team_ids: [uuid], direct: bool, include_ids: [uuid], exclude_ids: [uuid]}`，默认 `{all, [], true, [], []}`。按入口的节点判断（直出入口的节点就是线路机自己）：`exclude_ids` 优先，一律不带；`include_ids` 一律带上（不看团队与 `direct`）；直出入口看 `direct`；其余 `all` = 不在备用团队的、`only` = 团队在 `team_ids` 里的（列了备用团队就带上备用）、`except` = 不在备用团队也不在 `team_ids` 里的。`team_mode` 不对、`only` 没给团队、引用的团队或机器不存在 400。

候选入口：入口 `active`，节点与线路机都 `active`（退役中、已退役、已回收的不算；健康与否不看）。条目名 `国家 · 供应商 家宽 · 节点名 · 出口 IP · VLESS Reality`，直出 `国家 · 供应商 线路机名 直出 · 线路机 IP · VLESS Reality`（国家按机器的国家，没手动指定的按 IP 查离线库；没有国家、供应商的省掉那段；同一节点在几台线路机上都有入口时节点名后括注线路机）。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/client-subs` | `[ClientSub]`：`{id, name, enabled, rule, rule_text（一句话摘要）, items（按现在的入口算的条目数）, created_at, token_at, last_fetch_at, last_fetch_ip}` |
| POST | `/client-subs` | `{name（1–40 字，不能重名 409）, rule?}` → `ClientSub` + `url`（只这一次） |
| GET | `/client-subs/{id}` | `ClientSub` + `entries: [{name, entry_id, node_id, relay, port, server, direct}]`（不含凭据） |
| PATCH | `/client-subs/{id}` | `{name?, rule?, enabled?}`（没给的不变；停用后拉取 404，令牌不变）→ `ClientSub` |
| POST | `/client-subs/{id}/rotate` | 换令牌：旧链接立即 404，最近拉取清空 → `ClientSub` + `url`（只这一次） |
| DELETE | `/client-subs/{id}` | `{ok}` |

**拉取**：`GET /sub/{令牌}?format=clash|base64`（挂在根上，不在 `/api/v1` 下，不要登录）。`clash`（默认）是 mihomo 可直接用的 proxy-provider 文件（只有 `proxies:`，字符串都带双引号）；`base64` 是 vless:// 链接（同分享链接的写法，`#` 后是上面的条目名）一行一条、整体 base64。按请求时的入口现算。响应 `text/plain`、`Cache-Control: no-store`，另带 `Content-Disposition`（订阅名）与 `profile-update-interval: 6`（Clash Verge 每 6 小时更新）。令牌不对或订阅停用 404，并和登录共用按来源的失败限速（`auth::begin_attempt`：同一来源 15 分钟 20 次，含登录失败；限速键按令牌哈希分开）；超了 429（带 `Retry-After`），对的令牌也一样。成功不写审计，`last_fetch_at` / `last_fetch_ip` 10 分钟内只写一次。`format` 不对 400。

## 作业
`Job = {id,kind,target:{id,name}|null,state,step,step_count,step_title,error,manual,created_by,created_at,started_at,finished_at,steps:[{idx,key,title,state,started_at,finished_at,output,error}]}`（`step_count` = 该种作业的步骤数，`step_title` = 第 `step` 步的标题，做完为 null）

`manual`（列表与详情都有）：作业要人工处理时为 `{message: string, routers: [{id: string, name: string, exits: string[]}]}`，否则 null。目前只有一种：替换 / 换线路机停在「改软路由出口」失败（`state = failed`），而且有路由改回也失败了（参数 `routers_stuck` 非空）——这几台路由上的这些出口可能停在改了一半。`message` 是给人看的中文（哪台路由的哪些出口、原来指向哪、先让路由恢复连接再点重试，或只把这几个出口改回原地址后重试，不要整份换回备份；不夹英文原始错误，ssh 的原始输出在作业日志里）；`routers` 是这些路由与出口名。这时 `cancellable` 为 false。重试之后（排队、在跑）为 null，再次失败时按那一次的结果重新给出。
| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/jobs?state=active\|done\|all\|failed&target_id=&kind=&limit=100&offset=0` | `kind`：`provision`（各类上架）或作业类型原值（`attach_entry`、`move_entry`、`replace`、`repoint`、`retire`、`purge`、`upgrade_agent`、`tune`、`ss_refresh`、`provision_vps`…），别的值 400，与 `state` 等一起生效。分页：`{items:[Job（不含 steps、cancellable、result，另有 step_title、manual）], total, next_offset}`，按创建先后倒序（`id desc`） |
| GET | `/jobs/{id}` | 含 steps |
| GET | `/jobs/{id}/logs?after=<log id>` | `[{id,ts,level,msg}]`，按 id 升序，一次最多 1000 行（满页按最后一行的 id 接着拉） |
| GET | `/jobs/{id}/logs?tail=<N>[&after=<log id>]` | （after 之后的）最后 N 行（1-5000）：`{logs:[{id,ts,level,msg}], skipped, after}`，`skipped` 是前面没给的行数，`after` 是续拉用的游标（最后一行的 id，没有日志时原样返回） |
| POST | `/jobs/{id}/retry` | 失败的作业从失败那一步重来（`upgrade_agent` 从推送重来：失败时多半已经换回了旧程序）。可选请求体 `{reset_host_key?: bool, ssh_password?: string}`（不带请求体 = 都不要）：`reset_host_key` 清掉 known_hosts 里这台机器的记录、按现在的主机公钥重新记录：只允许失败那一步的类别是 `host_key`（中控确实看到了指纹不符）、目标不是同机 IP 节点（它登录的是宿主机：对宿主机的作业重试）的作业，否则 409；在其它检查都通过之后才做（拿不到公钥 409，整个重试作废）。确认机器重装过才用；`ssh_password` 是 root 密码，和上架时一样加密存进作业参数，供重试的作业在中控密钥登录不了时用（用过即丢；失败作业留存的密码 3 天后清除，之后重试要重新给）。只有 failed 的作业能重试（否则 409）；上架/恢复失败的机器重试时回到 provisioning。审计 `job.retry` 记 `reset_host_key` 与是否给了密码（不记密码） |
| POST | `/jobs/{id}/cancel` | 进行中的作业：补偿已完成的步骤后停下。`upgrade_agent` 只能在排队（queued）时取消：领到就收尾（没开始过，`started_at` 为 null、步骤原样），「全部升级」里等它的随后跟着取消；进行中、失败的 409（推送、重启到一半中断更危险）。`replace`、`repoint` 只能在排队时、或第一步（改软路由）失败后取消。详情的 `cancellable` 与这里同一套规则 |

## 运维：远程执行与日志
需要 `exec` scope（会话用户直接可用）。中控用自己的 SSH 密钥和 known_hosts 登录；机器必须不是已退役、不是家宽代理（proxy 没有 SSH）。命令原样当 bash 脚本执行（stdin 接 /dev/null），超时由远端 `timeout` 杀掉。每次执行写审计（actor、机器、命令全文、退出码、耗时），**不要在命令里写密码**。
| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/machines/{id}/exec` | `{command, timeout_s?: 1-600 (默认 60)}` → `{data:{exit_code, stdout, stderr, duration_ms, truncated, timed_out}}`。同机 IP 节点没有自己的 SSH：命令在它的宿主机上执行（审计记在这台节点名下）。stdout/stderr 各截断到 256 KiB（`truncated=true`）；超时 `timed_out=true`：到点远端 `timeout` 发 TERM，`exit_code=124`；命令不理会 TERM 的，再过 5 秒被 KILL（`timeout -k 5`），`exit_code=137`；远端没能按时结束、中控本地等到 `timeout_s+15` 秒仍没返回时也按超时返回 `exit_code=124`。SSH 连不上时 `exit_code=255` 且 stderr 有原因（命令自己退出 255 也是 255）。**只执行一次、不自动重试**（255 时命令可能已经执行过）。机器不存在 404、已退役或 proxy 409、命令空/超时越界 400、SSH 无法启动 502；同时最多 8 条命令在执行，排队 30 秒还轮不到回 429（`busy`） |
| GET | `/machines/{id}/logs?unit=fleet-agent\|fleet-xray\|ssserver&lines=200` | 白名单 unit（`ssserver` 对应 `fleet-ssserver`；同机 IP 节点对应宿主机上它自己的实例 `fleet-ssserver@<SS 端口>`，`fleet-agent` 是宿主机的），lines 1-2000；内部走同一 exec 通道跑 `journalctl`，返回形状同 exec，另有 `unit`（实际查的单元）、`lines`。审计动作 `machine.logs` |
| GET | `/server/logs?lines=200` | 中控自己的 `journalctl -u fleet-server`。fleet 用户通常没权限读 journal：journalctl 报错时 424（`error.code="journal_denied"`，message 说明如何授权：`usermod -aG systemd-journal fleet`）；journalctl 无法运行或 20 秒没返回时 500 |
| POST | `/audit/ssh` | `fleetctl ssh` 在中控本机用中控的密钥直接 ssh（不经中控执行），登录前报一声：`{kind: "machine"\|"router", id, command?: bool}` → `{ok:true}`，写审计 `ssh.login`（`target_kind` 是 kind，`detail = {name, command}`：对象名按库里的写，`command` 只记带没带命令，命令内容不记）。机器不存在 404，已退役、家宽代理 409；软路由按 `router` 表核对，不存在 404；kind 不对 400。fleetctl 没记上就不登录 |

## 网页终端
在浏览器里打开机器的 shell：中控本地开一个 PTY 跑 `ssh -tt`（中控自己的 SSH 密钥与 known_hosts，`StrictHostKeyChecking=yes`，同远程执行），WebSocket 双向搬终端字节。**只给会话用户**（API 密钥一律 403；命令行在中控上用 `fleetctl ssh`，不走这个接口）。机器要在用（`active`，或退役中的 `retiring`）：已退役、还没上架完成（`draft` / `provisioning` / `failed`）、家宽代理（没有 SSH）409；同机 IP 节点登录它的宿主机（宿主机同样要在用：已退役、还没上架完成也是 409）。软路由（`kind: "router"`）经它连到中控的反向隧道登录（`root@127.0.0.1:<tunnel_port>`，中控密钥，主机公钥按别名 `router-<id>` 在 `[routers] known_hosts` 里核对，同 `fleetctl ssh router`；每个终端自己一条连接，不复用采集的 ControlMaster）：不存在 404，还没接入过（`state = pending`，没记下主机公钥）、现在离线（隧道没连上，原因带上采集的 `error`）409；票据、再验证、名额、空闲 / 最长时限、协议、审计都和机器相同。

**终端里输入的命令不进审计**：只记谁、哪台、何时开 / 关、时长、上下行字节数、为什么断（前端在打开前提示）。

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/terminal/tickets` | `{kind: "machine" | "router", id, code?, password?}` → `{ticket, expires_at}`。先看目标（不存在 404、不能开 409、`kind` 不认识 400）与名额（429 `terminal_limit`），再**再验证一次**：用户开了两步验证要 `code`（6 位验证码或一个恢复码，密码不顶用），没开要 `password`（当前密码）；同一个登录会话 5 分钟内验证过的免输（记在中控内存里，中控重启后重来）。缺了 401 `totp_required` / `password_required`（不算失败），错了 401 `totp_invalid` / `password_invalid`：与登录共用失败计数和限速（429 `rate_limited`），写审计 `auth.verify_failed`（`detail.op = "terminal"`），用了恢复码写 `auth.totp_recovery_used`。票据是随机 32 字节（base64url，43 个字符），60 秒内在 WebSocket 上用一次：只能用一次（不管成没成都作废）、只能由开票的这个登录会话用、绑定这台机器；中控只存它的 sha256。每人手里没用掉的票据最多 10 张（再开 429 `rate_limited`）。中控正在重启时 503 `unavailable` |
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
- **审计**：`target_kind` 是 `machine` 或 `router`。`terminal.open`（`detail`：`kind`、`name`、`ip` / `port`（实际登录的地址：同机 IP 节点是宿主机的，软路由是 `127.0.0.1` 与隧道端口）、`host`（同机 IP 节点的宿主机 `{id, name}`，否则 null）、`client_ip`、`session`（登录会话的 id，同 `/account/sessions` 的 `id`））；`terminal.close`（同上，另有 `open_id`（对应的打开记录）、`duration_s`、`bytes_in`（上行：浏览器 → 机器）、`bytes_out`（下行：机器 → 浏览器）、`why`（`remote_exit` / `client_closed` / `client_gone` / `idle` / `max_age` / `session_invalid` / `shutdown` / `stalled` / `too_large` / `recheck_failed` / `error` / `spawn_failed`）、`reason`（同 `exit` 的原因）、`exit_code`（ssh 的退出码，没有为 null））。`terminal.open` 在起 ssh 之前写：中控异常退出（崩溃、被强杀）的话只有打开、没有关闭。

## 总览与告警
| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/overview` | `{nodes:{total,ok,degraded,down,unknown,maintenance}, relays:{…}, provisioning, retiring, failed（上架失败的机器数，网页「有问题」里也算它们）, due_soon（`subscription.due_state = soon` 的台数：到期前提前提醒天数内、不是自动续费，网页「即将到期」）, overdue（已过期的台数）, online, offline（按台机器、按 agent 连通：agent 没有近期上报为离线；同机 IP 节点跟随宿主机的 agent 计入；与健康无关，有严重告警但照常上报的算在线）, no_agent（不需要 agent 的台数，即家宽代理，不计入 online / offline）, ip_nodes（其中同机 IP 节点的台数）, monthly_usd:{cents, by_currency:[{currency,cents,usd_cents,usd_per_unit}], unconverted:[{currency,cents}]}（月费折成 USD：`cents` 是各台 `monthly_usd_cents` 之和；`by_currency` 列出各原币种（含 USD）的原币种合计、折成的 USD 与所用汇率；没有汇率的币种不算进 `cents`，原币种金额单列在 `unconverted`。见「汇率」）, traffic_month:{up,down,counted_from}, net_rx_bps, net_tx_bps}`（traffic_month 是本月（组织时区）全部入口的业务流量合计：每条入口的字节只在它的线路机上统计一次，含已移除的入口和月中退役、替换掉的机器；网卡流量不做合计。`counted_from`（RFC 3339 或 null）：本月数据不完整时的起点——在用（active）线路机里没有一台月初（组织时区 1 号 0 点）前就在用时，为其中最早的首次上架完成时刻（`activated_at`），之前的量不在合计里（界面标「10-22 起」）；有一台月初前就在用、或没有在用线路机时为 null。其余只统计 active；速率不含 agent 离线的机器——告警不影响，照常上报的机器照算；同机 IP 节点的读数就是宿主机的，速率不重复算，节点数与在线 / 离线照常按台计） |
| GET | `/overview/series?minutes=30` | 总览网速曲线：`{points:[{ts, net_rx_bps, net_tx_bps}]}`，`minutes` 1–60（默认 30，越界 400）；最近 N 个**结束满 20 s** 的分钟（agent 每 10 s 上报，刚结束的那一分钟还缺最后的上报，不给）每分钟一个点、旧→新，`ts` 为该分钟起点（RFC3339）。每个点 = 该分钟有主机指标的 active 机器速率之和，单台算法同 `/machines/{id}/series?metric=host`（每分钟计数器最大值与上一个有数据的分钟相减 ×8 / 秒数；计数器回绕的那一段不计）；没有数据的分钟为 0 |
| GET | `/probe-beats` | 探测心跳条：`{<机器id>: [[n, ok, ms], …20 格]}`。每格 3 分钟、按墙钟对齐（`date_bin('3 minutes', ts, 'epoch')`，即每小时的 :00 / :03 / … ），旧→新，最后一格是当前这格（还没走完）；第 i 格 = `[cb-57m+3i 分, cb-57m+3(i+1) 分)`，cb = 当前这格的起点；每格 `[n, ok, ms, retrans]`：`n` = 次数，`ok` = 成功次数，`ms` = 成功的延迟中位数（没有成功的为 null），`retrans` = 建连时 SYN 重发过的次数（丢包，不算进 ms；旧 agent 不报，为 0）；没有数据的格为 `[0,0,null,0]`。节点：线路机 → 节点的 TCP 探测，汇总它的入口；线路机：主探测来源 → 线路机的连通性（`relay_reach` 里那一路的行：中控或探测点每 30 秒连一次它的客户入口——public_host + 在用入口端口，每轮换下一个；中控只判通不通、ms 为 null，探测点的 ms 是建连往返时间的中位数；对照目标都连不上时那一轮不记（三台以上线路机全部连不上照记）；主来源离线时是代替它的中控的，中控的检查也关着时线路机没有格子），不再汇总它上面的入口；只含这 20 格里有数据的在用机器 |
| GET | `/incidents?state=firing\|resolved\|all&subject_id=&limit=100&offset=0` | 分页：`{items:[{id,engine,rule,rule_name,subject:{id,name},subject_id,subject_name,severity,state,summary,opened_at,last_seen_at,resolved_at,ack_at,ack_by,reopen_count}], total, acked, next_offset}`；`acked` = 同一筛选下已确认的条数（确认不关闭告警，`total` 含它们）；`state` 默认 firing；`subject_id`（可选，机器 id）只看这台机器的告警，与 `state`、分页组合，`total` 同样只数它的（如抽屉的最近告警：`?subject_id=<id>&state=all&limit=5`；不是 uuid 时 400）；`reopen_count`：恢复后 30 分钟内同一（规则, 对象）复发会重开原告警而不是新建，每次 +1；顺序：进行中在前 → 严重度高在前 → 开始时间新在前（同时刻按 id，翻页不重不漏）；带 `subject_id` 时不看严重度：进行中在前 → 开始时间新在前（一台机器的「最近告警」按时间读）。中控自身的告警（规则 `central_` 开头，见下表）对象不是机器：`subject`、`subject_id` 为 null，`subject_name` 为「中控」。每条另带 `suggested`（字符串数组：可以直接执行的 fleetctl 命令，只有进行中的、只有下面几条规则有，其余为 `[]`）与 `quick_switch`（其中临时切换的结构化一份 `[{router_id, router, group, from, to}]`，网页的「临时切到…」用）：`egress_fail`（节点出口不通）与 `router_exits_down` → 当前选中这个节点 / 不通出口的 select 组切到组里第一个能用、不是坏的出口（fleet 出口优先）`fleetctl router switch <路由> <组> <出口> --for 4h`；`router_exit_drift` → `fleetctl router sync <路由>`；`traffic_quota_high`（只线路机）→ `fleetctl rebalance <线路机>`；`agent_offline` → `fleetctl logs <机器>`。名字带空格等特殊字符的加单引号 |
| POST | `/incidents/{id}/ack` | 确认告警：不存在 404，已确认过 409；写审计 `incident.ack` |
| GET/POST | `/silences`、DELETE `/silences/{id}` | GET：还没结束的（`ends_at` 晚于现在，含还没开始的预约）`[{id, subject_kind, subject_id, subject_name, rule, rule_name, starts_at, ends_at, is_maintenance, comment}]`（`subject_kind` 为 `machine` / `router` / `probe_point`，全部为 null；`subject_name` 是机器的显示名、软路由 / 探测点的名称），按 `starts_at` 排——进行中的在前，已预约的随后；只有此刻在窗口内的才起作用（不开告警、不发通知；维护对机器还暂停健康判定，对软路由、探测点是开着的告警保持原状、不恢复、不发通知，到期后再判；全部的维护对三类都算）。删机器 / 软路由 / 探测点时它的静默一起删。POST `{subject_kind?,subject_id?,rule?,starts_at,ends_at,is_maintenance,comment（最多 2000 字）}` → `{id}`（`subject_kind` 是 `machine`（默认）/ `router` / `probe_point`，`subject_id` 是那张表的 id；两个都空 = 全部（机器、软路由、探测点一起），`rule` 空 = 全部规则）：`ends_at` 要晚于 `starts_at`、时长最多 30 天、`ends_at` 不能已经过去（`starts_at` 可以，「从现在开始」由客户端按自己的时钟填）、`starts_at` 最多在 90 天后，`rule` 必须是已知规则 key（且不能与 `is_maintenance` 同时给：维护针对整个对象；给了对象时要是这类对象的规则：软路由 `router_*`、探测点 `probe_point_*`、机器其余的，中控的 `central_*` 只能用在全部上）、`subject_id` 必须是存在的这类对象（给了 `subject_kind` 就要给 `subject_id`），否则 400（中文原因）；全部机器（`subject_id` 空）的用 API 密钥建时最长 24 小时（超了 400：全部机器的静默等于关掉监控），账号登录的会话仍是 30 天。DELETE 不存在或已删 404。新建、删除都写审计（详情是这条静默本身：对象类型与名字、规则、起止、维护、备注），并给所有启用的通知渠道发一条 `silence` 通知（谁、新建还是删除、对象（软路由、探测点带类型：「软路由「团队2-路由A」」）、维护还是静默、起止、备注；不看渠道的 `min_severity`，也不受任何静默影响）：全部机器的一律发（含预约的），单个对象的只在此刻生效（`starts_at` ≤ 现在 < `ends_at`）时发；软路由的（或全部的）变了另发实时事件 `routers` |
| GET | `/audit?target_id=&cat=&q=&limit=&before_id=` | 按 id 倒序；`before_id` 翻页（只要 id 小于它的，传上一页最后一条的 id），与筛选一起用。`cat`：`all`、`auth`（auth.*）、`exec`（exec、ssh.login、terminal.open/close、machine.logs、server.logs，以及已删掉的恢复预检留下的旧记录 restore.probe）、`machine`（machine / entry / relay / subscription / agent / job / renewal / recycle 开头的，除去 exec 类）、`config`（其余），别的值 400。`q`：不分大小写的包含匹配，`%` `_` 按字面；匹配操作人（按界面写法：`cli` 是「中控命令行」，`apikey:名称` 是「名称（API 密钥）」）、标题（`title` 含它的动作）、摘要（`summary`，写审计时存下；`detail` 里摘要不展示的值不算），有 `exec` scope 时另搜命令原文；查询超过 5 秒中止，回 503 `unavailable`（换更具体的关键词或先按分类筛选）。没有 `exec` scope 的密钥看不到命令：`detail.command` 换成「命令已隐藏（N 字）」，`summary` 里的命令同样隐藏。`exec` / `machine.logs` 在执行前就写一条 `detail.started=true` 的记录，执行完补上结果并去掉 `started`；`started` 还在说明在执行中或中控中途重启 |
| GET | `/notify-log?limit=100` | 通知发送记录（新→旧，`limit` 1–1000）：`[{id, channel_id, channel_name, incident_id, ts, ok, error, kind: "firing"\|"repeat"\|"resolved"\|"test"\|"silence", subject_name, rule_name}]`；一次投递一条，在送达或放弃时写：失败重试中的不记（还在发送队列里），入队满 30 分钟还没发出的放弃，记一条 `ok:false`、`error` 为「入队 30 分钟还没发出，放弃（最近一次失败：…）」；渠道拒绝（不值得重试）的当场记失败。`error` 只有 HTTP 状态与渠道 JSON 里的说明（Telegram `description`、企业微信 `errmsg`，截到 200 字），不存原始响应体；`subject_name` 是告警对象的显示名（中控自身的告警是「中控」，`silence`（静默 / 维护变更）是「静默 / 维护变更」），`rule_name` 是规则中文名（与通知正文一致）；测试消息两者为 null，机器或告警已删除时对应字段为 null。没收到某条告警「触发」的渠道不发它的「恢复」，也不记 |

**中控自身的告警**（selfmon.rs，规则 `central_` 开头）：对象是「中控」而不是机器，每 30 秒随告警评估检查一次；不去抖，恢复后再出现新开一条；被静默的不开、不发（只能用全部机器 + 规则 key 静默）；critical 每 4 小时提醒一次。通知里「机器」一行写「中控」。

| 规则 | 级别 | 开 | 恢复 |
|---|---|---|---|
| `central_channel_failing`（通知渠道发送失败） | critical | 某个启用的通知渠道连续发送失败满 5 分钟（`notify_channel.failing_since`）。每个渠道一条（库里 `subject_id` 是渠道 id）；这条不发给它自己 | 它发成功一次（含渠道测试）；渠道停用、删除时直接关，不发恢复（`detail.closed_by = "channel_removed"`） |
| `central_backup_stale`（备份过期） | warning | 备份目录（`[selfmon] backup_dir`，默认 `/var/backups/fleet`）里最新一份 `fleet-*.tar` / `fleet-*.tar.age` 超过 26 小时，或一份都没有、读不了；目录不存在（没装备份）不检查 | 有了 26 小时内的一份 |
| `central_disk_low`（中控磁盘空间不足） | critical | 中控本机 `/`、`/var/lib/postgresql`、`/var/lib/fleet`、备份目录所在的文件系统可用空间低于 10%（同 df） | 回到 11% 以上 |
| `central_ingress`（agent 同时失联） | critical | 有自己 agent 的在用机器（不含维护中的、同机 IP 节点）里同时离线的超过一半且至少 3 台：摘要「x/y 台 agent 同时失联：检查边缘机、Caddy、域名解析」。期间不新开单机的 `agent_offline`（已开着的照旧） | 离线的不再过半或不到 3 台；还没回来的从这时起按单机去抖、报 `agent_offline` |
| `central_tunnel_host`（软路由回连地址不是中控） | warning | 有软路由时，启动与每 10 分钟查一次隧道回连主机（`[routers] ssh_host`，没配时 `public_url` 的主机名，routers/selfcheck.rs）：解析不了，或解析到的地址不是中控本机网卡上的、它的 `ssh_port` 上 SSH 主机公钥也不是中控 sshd 的（多半是 CDN / 只转发 80/443 的边缘机）。摘要写解析到哪、建议把 `ssh_host` 配成源站 IP 或单独的灰云域名；现有隧道断线后才受影响 | 查出来是中控了；软路由都删了（下一次检查） |
| `central_view_stuck`（机器视图算不出来） | critical | 机器视图（列表、总览、实时推送、告警评估共用）连续 2 分钟算不出来（查询被锁住或超时）：摘要「机器视图已经 n 分钟算不出来（原因）：…」。期间 `central_ingress` 判断不了，开着的留着 | 算成一次 |

**探测点的告警**（`probe_points/`，规则 `probe_` 开头）：`probe_point_offline`（探测点离线，warning）——启用的、上报过的探测点超过 180 秒没上报；摘要「香港：5 分钟没有上报（最近一次 …）」，它是主探测来源时后面写明线路机的连通性暂由中控代替 / 暂时判不出。上报恢复即恢复；停用、删除时直接关、不发恢复（`detail.closed_by = probe_point_disabled / probe_point_removed`）。对象是探测点：`subject` / `subject_id` 为 null、`subject_name` 是它的名字（通知记录同）。每 30 秒检查一次，不去抖（条件自带 180 秒）。

另有死人开关：配置 `[selfmon] heartbeat_url` 后，告警评估每轮成功后最多每分钟 GET 它一次（OPS.md §12）。

## 返回形状补充（前端以此为准）

- `health.reasons[]`：`{key, rule_name, severity, text}`；`rule_name` 是规则中文名（同通知）；`severity`：`"info"` | `"warning"` | `"critical"`（info 只有 `policy_blocked`，不影响健康）。
  `renewal_due`（「即将到期」）：到期前提醒天数内（不是自动续费）为 warning，文案「3 天后到期」「今天到期」；过期后同一条规则升为 critical，文案「已过期 2 天」——告警不先恢复再新开，按「严重度只升不降」马上再发一次触发（旧的 `renewal_overdue` 并进来了，迁移 0041 把开着的改名接上）。备用团队的机器只在还有软路由出口指向它时才报（见「备用团队」）。和流量额度一样**不影响** `health.status`（账单到期不是机器出了问题：不算降级、故障），只开告警；不去抖，持续提醒每天一次。备用团队的机器文案后面接哪天自动回收（「，备用节点明天自动回收」）与还指向它的软路由出口（「；软路由出口 路由名 · 出口名 还指向它」）。
  `traffic_quota_high`（「流量额度吃紧」）：本周期已用（`quota.used`）到上限（订阅的 `traffic_gb`，没填按套餐的；0 / 空 = 不限，不评估）的 90% 为 warning，到 100% 为 critical；上限按十进制 GB（1 GB = 10⁹ 字节，同供应商），文案也写 GB、不换算成 TB，如「本周期流量已用 92%（1840 GB / 2000 GB），9 天后重置」「本周期流量已超出额度（已用 102%，2050 GB / 2000 GB）」（有流量重置日才写几天后重置）；滚动 30 天（`quota.mode = rolling30`）写「近 30 天流量已用 …」、不写几天后重置；周期中途上架（`quota.counted_from` 非 null）末尾加「（10-22 起统计，之前的量不到）」（日期按组织时区）。  **提前警告**：按近 7 天日均预计重置前用满（`quota.forecast_full_at` 非空）且距今不超过 7 天时，没到 90% 也开 warning，摘要写预计日期，如「本周期流量已用 52%（1051 GB / 2000 GB），按近 7 天的速度预计 11-03 用满，20 天后重置」（已用 ≥ 90% 时同样追加这一句）；已超出额度的 critical 不变。
  它和 info 一样**不影响** `health.status`，只开告警：不去抖，回落到 90% 以下（如新周期开始）下一轮评估即恢复；升为 critical 时马上再通知一次，持续提醒每天最多一次（同续费类）。同机 IP 节点自己没设上限时不评估（额度算在宿主机上）。
  `clock_skew`（warning，「agent 时钟不准」）：agent 在线且 `|agent.clock_skew_ms|` > 60 秒，文案如「agent 时钟慢 17 分钟：读数可能缺失或冻结，请校时」——样本按 agent 的时间入库，当前读数只看最近 15 分钟、持续负载只看最近 5 分钟；比中控超前 10 分钟以上的样本（含入口流量、探测）直接丢弃。和其它 warning 原因一样满 60 秒去抖后开告警。
  同机 IP 节点不报 `agent_offline`、`clock_skew`、`host_cpu_high` / `host_mem_high` / `host_disk_high`（agent 与主机是宿主机的，由宿主机报）；`egress_fail`、`tcp_fail`、`no_entry`、续费照常自己报（宿主机整台宕机时宿主机和它的每个同机节点各报各的探测告警，不抑制）。
- `GET /machines/{id}/series` → `{metric, range, resolution: "1m"|"1h", points: [...]}`，点里时间字段统一叫 `ts`：
  - host：`{ts, cpu_pct, mem_pct, disk_pct, load1, net_rx_bps, net_tx_bps}`（1h 分辨率另有 `cpu_max`，无 `load1`）
  - traffic：`{ts, up, down, conns}`（字节，该时间桶内的增量）
  - probe：`{ts, tcp_ok, tcp_ms, egress_ok, egress_ms}`（失败时 `*_ms` 为 null；1m 分辨率另有 `tcp_n`、`tcp_ok_n`（该分钟 TCP 探测次数 / 成功次数，线路机为其所有入口合计）；1h 分辨率另有 `tcp_success_pct`、`egress_success_pct`）
- 作业 `Job`：`{id, kind, target: {id,name}|null, state, step, step_count, step_title, error, manual, created_by, created_at, started_at, finished_at}`（`step` 当前或失败的步骤下标，`step_title` 它的标题，做完为 null；`manual` 见「作业」）；
  `state` ∈ `queued | running | waiting | cancelling | succeeded | failed | cancelled`；详情另有 `steps[]`（`output` 是 JSON 对象或 null；各步骤输出的 key 与中文展示见 `web/src/lib/jobs.ts` 的 `OUTPUT`，跳过的步骤为 `{reason}`）与 `cancellable`。
  作业 `kind` ∈ `provision_vps | provision_proxy | provision_relay | provision_ip | attach_entry | move_entry | replace | repoint | router_sync | retire | purge | upgrade_agent | tune | ss_refresh`（`restore_*` 已删，旧记录照样显示中文名）。
  `provision_ip`（上架同机 IP 节点）的步骤：`ip_check`（在宿主机上核对 IP，输出 `{ss_port, local_ips, egress_ip, bind_ip}`：最终的 SS 端口、宿主机本机的 IPv4（逗号加空格分隔的字符串）、经这个 IP 访问中控 /echo 回显的地址、1:1 NAT 映射时公网 IP 映射到的本机地址（在网卡上的为 null；写进机器的 `bind_ip`，详情 JSON 里同名字段）→ `converge`（推 ssserver、装它自己的实例与防火墙，输出同 VPS 节点，`bbr` 为 null——不做系统基线）→ `attach` → `wait_applied` → `verify` → `activate`。
  `ip_check` 失败：宿主机 SSH 连不上（network）；IP 不在宿主机网卡上、也没有哪个本机地址经 NAT 出去是它（「IP x 不在宿主机 H 的网卡上（本机：…），也没有哪个本机地址经 NAT 出去是这个 IP（…）」，remote；有几个本机地址都是它时也失败）；回显的来源不是这个 IP（出站路由不对，remote）。暂定的 SS 端口在宿主机上被占用时自动换成最小空闲端口，日志里有一条警告。
  同机 IP 节点的退役作业 `cleanup` 一步只删它自己的实例（输出 `ss` 是 gone / still_listening，指它自己的端口）。
  `upgrade_agent`（升级 agent）的步骤：`agent`「推送并重启 agent」（输出 `{binary_updated, agent_version, prev_version, xray_checks, xray_restarted}`：`xray_restarted` 为 true 表示线路机 DNS 是链路本地地址、为换上公共解析器重启了一次 Xray（其余为 null）；`agent_version` 是装上的版本，`prev_version` 是这次换下来、留作回滚的旧版本，程序没变时为 null；`xray_checks` 是升级线路机、程序换过时等上报还要做到的几项，如「Xray 在跑、配置应用到代数 3、没有应用错误」，升级前都没做到时「不查（升级前 Xray 就不正常）」，其余为 null；上架的 `agent` 步骤输出同形）→ `wait_report`「等待新版本上报」（等装上的版本用新令牌上报，线路机还要过 `xray_checks`，输出 `{first_report_at, agent_version, old_tokens_revoked}`）。回滚见 `POST /machines/{id}/upgrade-agent`；被编排取消的、排队时被取消的（见 `POST /agents/upgrade`、`POST /jobs/{id}/cancel`）`started_at` 为 null、步骤全是 pending。
  `tune`（系统调优）的步骤：`tune`「系统调优」（输出 `{profile, applied:[键], ok:[键], kept:[键], skipped:[{key,why:"missing"|"readonly",got}], before:{键:改前的值}, params:{bw, bw_plan, bw_source, rtt, rtt_source, rtt_samples, mem_mb, buf_max}, snapshot:"created"|"created_tuned"|"exists", warnings:[第三方调优残留], route:{initcwnd,initrwnd}|null, journald:"changed"|"same", ntp:"synced"|"unsynced"|"container", nofile, container, mem_mb, swap_mb, bbr}`：`applied` 这次改了并读回一致（改前的值在 `before`），`ok` 本来就是目标值，`kept` 现值已不小于「至少」类目标而不动（三元组逐项比），`skipped` 是键不存在（如 conntrack 模块没加载）或写不进（容器里的 `net.core.*`、供应商限制；读回的值写在 `got`）。`params` 是缓冲区上限的推导凭据：`bw` 参与计算的带宽（套餐 `bw_plan` 封顶 1000，没写为 1000、`bw_source:"default"`），`rtt` 线路机为客户入口连接的 P90（`rtt_source:"sampled"`，样本 `rtt_samples` 条；不足 20 条或节点为默认值），`mem_mb` 远端读的 MemTotal，`buf_max` 远端算出的上限（字节；中控按同一公式核对，不一致记警告）。`snapshot`：`created` 这次记了原值快照，`created_tuned` 记快照时已有 fleet 调优配置（快照里是旧档的值，记一条日志），`exists` 快照已在。`warnings`：第三方调优残留（`/etc/sysctl.d/99-tcpfit.conf`、tcpfit 的整形服务、网卡根上的 HTB），只报告不删，同时各记一条作业警告；`route`：默认路由上的 initcwnd / initrwnd（有才非 null），只记录、记一条作业日志，不改路由。键缺失 / 写不进 / 时间没同步上都不让步骤失败：容器里不记警告，非容器机器上写不进的键、时钟没同步上、服务的 LimitNOFILE 低于 65536 各记一条作业警告。只有脚本自己出错（写文件、原值快照失败）或远端回显的带宽与代入的不一致才失败，错误类别 `remote`。上架 / 恢复的 `converge`、`converge_relay` 步骤输出里的 `tune` 是同样的对象（同机 IP 节点为 null），`bbr` 沿用为是否已是 BBR）。撤销（参数 `rollback:true`）时同一步骤输出 `{rollback:true, applied:[写回原值的键], ok:[本来就是原值], kept:[], skipped:[…], before:{键:撤销前的值}, container}`；非容器机器上写不回的键记一条作业警告。
  `ss_refresh`（重新下发 SS 配置）的步骤：`ss`「下发 SS 配置」（输出 `{instances:[{port, ip, restarted, nft_relays}]}`：宿主机本机实例在前，再是在用的同机 IP 节点实例；`restarted` = 这次重启了 ssserver；同时把每个实例一条启用的入口记进作业参数 `entry_ids`）→ `verify`「出口验收」（这些入口经出口探测在本步开始之后至少有一次成功，输出 `{entries:[{latency_ms, egress_ip}]}`；没有启用的入口时跳过；等满 300 秒没有结果则失败，**不自动回滚**，原因写在作业日志）。
- 告警 `Incident`：`{id, engine（写它的引擎：machine 机器 / ipintel 来源画像 / central 中控自身 / router 软路由 / probe 探测点；前端按它决定点对象时开机器还是路由，不看规则前缀）, rule, rule_name（规则中文名，同通知）, subject: {id,name}, subject_id, subject_name, severity, state: "firing"|"resolved", summary, opened_at, last_seen_at, resolved_at, ack_at, ack_by, reopen_count}`（`subject_name` 机器已删除时为 null；中控自身的告警 `subject`、`subject_id` 为 null、`subject_name` 为「中控」）。
- 静默 `Silence`：`{id, subject_id, subject_name, rule, rule_name, starts_at, ends_at, is_maintenance, comment}`（`subject_id` / `subject_name` 为 null = 全部机器；`rule_name` 是规则中文名，同告警，`rule` 为 null（全部规则）时也为 null）。
- 续费 `Renewal`：`{id, old_due, new_due, amount_cents, currency, note, actor, created_at}`。`currency` 是登记时订阅的币种（迁移 0029 起记；之前的按迁移时订阅的币种补上），之后改订阅币种，历史金额不跟着变。币种一律 3 个大写字母（套餐、订阅、续费记录库里都有约束；套餐接口会去空白转大写，其它不合规的回 400）。
- 审计：`{id, ts, actor, action, title, summary, target_kind, target_id, detail}`；界面显示 `title` 与 `summary`（中文，服务端生成），`detail` 是原始 JSON。

## 实时
`GET /api/v1/stream`：WebSocket（`ws://` / `wss://`），鉴权同其它 GET（会话 Cookie 或 `Authorization: Bearer`，需要 `read`）；用会话 Cookie 连时 `Origin` 必须等于 `public_url` 的协议 + 主机 + 端口，否则 403（WebSocket 握手不受同源策略约束），握手失败返回普通 HTTP 错误（401/403）。服务端只发 JSON 文本帧，不读客户端消息。
- **保活**：服务端每 15 s 发 ping，到下一次 ping 还没收到 pong 就断开（浏览器自动回 pong）；同时发一个文本心跳 `{type:"hb"}`（浏览器脚本看不到协议层的 ping），客户端连着 45 s 什么都没收到就当连接已死、自己重连，`hb` 本身直接丢弃。
- **会话复核**：每 60 s 查一次会话/密钥是否仍有效，撤销会话或密钥时各连接马上再查一次（服务端内部通知，不推给前端；命令行 `user-set` 经 PostgreSQL `NOTIFY fleet_revoked` 转过来），失效则以关闭码 `4401` 关闭（客户端应回登录页，`fleetctl watch` 报错退出）；复核查询本身失败以 `1011` 关闭，客户端照常退避重连、握手时重新鉴权。
- **滞后**：服务端广播缓冲 1024 条，某个连接读得太慢丢了消息时补发 `{type:"resync"}`，客户端应让全部缓存失效重取。
- **重连**：客户端断线后自己退避重连；连上后应重取一次机器类数据（断开期间错过的 `metrics` 增量不会补发）。

**资源变更通知**（收到后让对应查询失效重取）：`{type:"machine",id}`、`{type:"machines"}`、`{type:"relay",id}`、`{type:"job",id}`（作业推进；目标机器的 `active_job` 走 `metrics`，它变了时客户端可重取这台机器的详情；改生命周期时另发 `machines`）、`{type:"incidents"}`、`{type:"catalog"}`、`{type:"api_keys"}`（密钥增删撤销）、`{type:"policies"}`、`{type:"settings"}`（面板设置变了：访问记录保留期、汇率——汇率变了机器视图里的 USD 折算值跟着变）、`{type:"resync"}`。

**实时指标** `{type:"metrics", machines:{<机器id>:{…}}, entries:{<入口id>:{…}}, overview?}`：在用机器视图（`GET /machines` 的 `MachineView`）里随上报与时间变化的字段，和上一条相比**只带变了的字段**（没变的键不出现），客户端把它们写进本地缓存，不必重取。有新上报或作业推进时最多每 3 s 一条，没有也每 30 s 比对一次（离线、到期天数、维护窗口随时间变）；没有变化不发。
- `machines[id]` 的键：
  - `host`：`[ts, cpu_pct, load1, mem_used, mem_total, disk_used, disk_total, net_rx_bps, net_tx_bps, tcp_conns] | null`（`ts` 为 Unix 秒，速率取整；各项可为 null，与 `MachineView.host` 同义）；
  - `seen`：`agent.last_report_at` 的 Unix 秒 | null；`agent`：`{version, online, xray_running, cores} | null`（不含 `last_report_at`、`health` 和 `clock_skew_ms`——agent 自报的累计计数、时钟偏差每次上报都变，只随 REST 重取更新；前端合并进原对象）；
  - `health`、`maintenance`、`removed_traffic`、`nic`、`month_counted_from`、`quota`、`active_job`：与 `MachineView` 同形（`nic`、`month_counted_from`、`quota` 每次汇总后、跨天时变）；`due_days` / `due_state`：`subscription.due_days` / `due_state`（无订阅为 null；跨天时一起变）。
- `entries[id]` 的键：`tcp` / `egress`：`[ok, latency_ms, ts, detail, median_ms] | null`（`ts` 为 Unix 秒；`median_ms` 见 EntryView）；`traffic`：与 `EntryView.traffic` 同形。一条入口同时出现在节点和线路机的 `entries` 里，两边都要更新。
- `overview`：与 `GET /overview` 同形，变了才带。
- 机器的新建、改名、换团队、退役、告警等结构变化不走 `metrics`，仍发上面的失效通知。
- 静默的新建 / 删除只发 `incidents`（不发 `machines`）：卡片上的 `maintenance`、`health`、总览的维护中计数随后（最多 3 s）经 `metrics` 送达，不必为它重取机器列表。

`fleetctl watch [--count N] [--metrics] [--json]` 订阅这个流（默认不显示 `metrics`）。

## agent（`/agent/v1`，Bearer 令牌）
见 `crates/fleet-proto/src/lib.rs`。另：`GET /echo` 返回请求方 IP（纯文本），供出口探测用，无需鉴权。线路机经入口访问它没拿到 IP 时，agent 核对两次：不走代理直连它一次，直连也拿不到 IP（中控重启、部署中）就不记这次出口探测；直连拿得到，再经入口访问备用目标 `https://www.gstatic.com/generate_204`，通的话说明出口好好的、只是节点去 /echo 的那条路不通（如边缘机反代配置坏了），同样不记，都不算节点出口失败。经入口回了 IP 但不是期望的照常记失败。探测入站的放行规则 `probe-<入口>` 因此也放行备用目标（与回显服务都是域名时合成一条，回显服务是 IP 时另加 `probe-<入口>-1`）。
TCP 探测（`ProbeSample.kind = tcp`）：线路机 → 节点出站地址建连，超时 10 秒（盖住 SYN 第 1、3、7 秒的三次重发），域名先解析、不计时；`latency_ms` 是内核 TCP_INFO 的往返时间，`retrans` 是建连时 SYN 重发次数（新字段，旧 agent 不报、旧中控忽略）。各入口的探测落在周期里按 id 错开的固定时刻，不在同一秒一齐发。

直出入口在期望状态里是 `"outbound": {"protocol": "direct"}`（`expect_egress_ip` 为线路机自己的 IP）。不认识它的旧 agent 整份期望状态解析失败、回报「agent 版本过旧」（线路机 apply_error），所以**先升级线路机 agent，再建直出入口**。agent 为它渲染入口自己的 freedom 出站（`out-<入口>`，不复用共享的 `direct`），不做 TCP 探测；路由和节点入口相同（AsIs，`block-private` / `block-bittorrent` 在前），另在该入口规则最前面加 `guard-<入口>-api`：目标端口是本机 Xray API（62789）或任一入口的回环探测入站（入口端口 + 10000）的连接一律 blocked（各入口的探测规则 `probe-<入口>` 本身也只放行到回显服务，即 echo_url 的主机与端口，和出口探测的备用目标，别的目标落到默认的 `deny`：探测入站是不要密码的 SOCKS，不能当跳板）——`block-private` 只拦 IP 目标，客户写解析到回环的域名（`localhost`、`127.0.0.1.nip.io`）时 freedom 会在线路机本机解析并连上 API。解析到内网其它地址的域名仍能连（与旧系统的主入口相同）；没有改成 IPIfNonMatch 先解析再过 `block-private`，是因为 fake-ip DNS 环境（实验室，域名都解析到 198.18.0.0/15）里那样会把所有域名都拦掉。

鉴权（三个接口相同）：先认令牌、再读请求体（没鉴权的请求不解压、不解析）。认令牌的结果在内存里缓存：有效的 30 秒、无效或已撤销的 10 秒；作业撤销令牌时马上作废。令牌查库没查到的按来源 IP 计失败：突发 30 次、之后每 2 秒 1 次，超出回 429 + `Retry-After`（`error.code="rate_limited"`）、不查库；缓存里认得的令牌（有效的、记着无效的）不计也不受限。合法 agent 收到的响应不变（请求体有问题照旧 400 / 413）。

`POST /agent/v1/report` 入库失败按类回：数据本身入不了库（SQLSTATE 22 类：坏字节、NUL、越界；23 类：违反约束）→ 422（`error.code="bad_report"`），agent 把这份报告移入 `dead/` 不再重发，其中的入口流量结转到下一份报告补报（同离线队列超限淘汰；这份本身带着结转补报的流量时不再结转，免得一份接一份地被拒）；其它（连接池超时、断连、磁盘满、只读…）→ 503，agent 退避重试（受本地队列的字节与时长上限约束）。agent 只把 4xx（401 撤销、403/404/408/429 除外）当永久拒绝，5xx 和网络错误一律重试。

上报的限额（按机器，在内存里，中控重启归满）：新的一份（seq 大于入库提交过的）要有一个令牌，突发 600、之后每秒补 1，扣不到回 429 + `Retry-After`；入库提交之后才扣令牌与主机样本余额，入库失败的那份不扣、也不算「提交过的 seq」（重发的同一份不扣）。agent 给的字符串里的 NUL 入库前去掉（text / jsonb 不收）。主机网卡计数器的每段增量在小时汇总里按 10 Gbit/s × 两个样本的间隔封顶、入口流量每小时按 10 Gbit/s 跑满封顶，用量求和夹到 9e18：离谱的计数器不会让汇总或机器列表溢出。样本时间戳只收「保留期以内 ~ 中控时间往后 10 分钟」（访问记录按面板里设的原始记录保留期，其余按 `[retention] raw_days`；agent 时钟快得更多时样本丢弃，健康原因 `clock_skew` 在偏差超过 60 秒时就已报出）。主机样本按 (机器, 时刻) 唯一，同一时刻重报的不再入库；另有主机样本余额：容量 8640 个（24 小时的量），每 5 秒补一个（两倍于 agent 10 秒一个的采样），超出的丢最早的——正常 agent 补传积压（最多 6 小时离线队列 + 12 小时内存缓冲）期间余额按两倍速回满，用不完；一台机器一天最多入库约 2.6 万条（正常 8640）。

`POST /agent/v1/applied`：每台线路机突发 120 份、之后 2 秒一份，超出回 429 + `Retry-After`（agent 照 Retry-After 重试，三次不成落盘、下轮补发）。`error` 与 `entries[].error` 截到 512 字，`entries` 最多 3000 条（不属于这台线路机的入口 id 忽略）。回报没改变任何东西（重发的同一份、同样的错误）时不发 `relay` / `machines` 失效通知。

## 访问监控
线路机 agent 解析 Xray 访问日志上报，服务端写入 `access_event`（原始记录，默认留 7 天），并每 5 分钟把当前小时和被标脏的小时重算进小时汇总（`access_hourly` / `access_source_hourly`，默认留 31 天；同一事务里重算那个小时所在那天（UTC）的天汇总 `access_daily`，留得一样久——窗口里整天的部分读它，首尾不满一天的读小时表，7d / 30d 主要读天表，结果与只读小时表相同；窗口最长 30d 且不超过汇总的保留期；`access_target_seen` 记每个目标的首次 / 最近出现，最近一次出现超过汇总的保留期就删；留多久见下文「访问记录保留期」；幂等；agent 断网后补传的旧记录在上报时把所在小时记进 `access_dirty`，下一轮补算；启动时补算缺口）。24h/7d/30d 的结果在进程内缓存：每轮汇总成功后整体作废；`summary` / `targets`（第一页）/ `sources` 最多缓存 `access_cache_secs`（默认 300）秒，单台机器的 `/machines/{id}/access` 与 `/machines/{id}/sources`（带最近原始记录、来源基线与异常）最多 30 秒；设 0 关闭。全部为只读 GET，读权限即可。

公共筛选参数（空串等同没给，非法值返回 400）：`window=1h|6h|24h|7d|30d`（默认 24h，events 默认 7d；1h 读原始记录，其余读小时汇总；除 events 外不能比汇总的保留期 `hourly_days` 长，超了 400，见「访问记录保留期」）、`team=<团队id>`、`node=<节点id>`、`search=`（大小写不敏感的子串，`%` `_` 按字面）、`action=all|blocked|routed`、`kind=all|domain|ip`。

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

## 汇率（计费折成 USD）

计费统一按美元（USD）展示：价格照供应商收的币种录入、存库（套餐、订阅、续费记录的 `price_cents` / `amount_cents` + `currency` 不变），展示用的折算值由中控按汇率算好放在视图里（`fx.rs`），网页、fleetctl 不自己折算。
汇率存在 `app_setting` 的 `fx_rates` 一行（不需要迁移）：

- **自动**：默认每天从公开汇率源拉一次（`fleet.toml` 的 `[fx] url`，默认 `https://api.frankfurter.dev/v1/latest?base=USD`——ECB 参考汇率，免费、不要密钥；回包 `{base:"USD", date, rates:{CNY:6.67,…}}`，也认 `{base_code, time_last_update_utc, rates}`）。后台每小时看一次：上次成功满 24 小时就拉，失败保留上次的值、记 `last_error` 与日志，一小时后再试。`[fx] auto = false` 关掉自动更新（只用手动的值，仍可点「立即更新」）。
- **手动**：每个币种可以手动覆盖，覆盖后自动更新不改它（自动的值照样更新、在 `auto_per_usd` 里看得到），取消覆盖后回到自动的值。手动优先于自动；USD 恒为 1，不能覆盖。
- **没有汇率**的币种不折算、不当 1:1：视图里它的 USD 字段为 null，合计里单列在 `unconverted`，网页标「未折算」。

**折算口径**：`usd_cents = round(原币种分 × usd_per_unit)`，四舍五入到分（.5 远离零），`usd_per_unit` = 1 单位该币种合多少 USD（自动的 = 1 / 源里的「1 USD 合多少该币种」）。
月费先按周期折成每月（`monthly_cents` = round(price_cents / 周期月数)，见机器视图），再折 USD（`monthly_usd_cents`）；每期价格直接折（`price_usd_cents`）。合计（总览、团队）是**逐台折好的 USD 相加**，和卡片上逐台的数字对得上，不是先按币种合计再折。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/settings/fx-rates` | 折算凭据：`{base:"USD", auto（配置里开着自动更新）, url（配置的汇率源）, fetched_at（最近一次拉取成功）|null, rates_date（源里的数据日期，如 2026-10-22）|null, source（最近一次成功用的地址）|null, last_attempt_at|null, last_error（最近一次失败的原因，成功后清掉）|null, rates:[{currency, usd_per_unit|null, per_usd|null（反向：1 USD 合多少）, source:"base"|"manual"|"auto"|null, auto_per_usd（自动拉到的原值，1 USD 合多少）|null, manual:{usd_per_unit,by,at}|null, in_use}], missing:[在用但没有汇率的币种]}`。`rates` 列出在用的币种（没归档的套餐、没退役机器的订阅）、源里有的、手动设过的和 USD，在用的排前面 |
| PUT | `/settings/fx-rates/{currency}` | 手动覆盖一个币种：`{usd_per_unit}`（1 单位合多少 USD）或 `{per_usd}`（1 USD 合多少该币种）给一个，要 `write`。币种 3 个字母（大小写都行）、不能是 USD，值要是 1e-6–1e6 的正数（`per_usd` 同样范围），否则 400。写审计 `settings.fx_rate`（`detail:{currency, from:{usd_per_unit,source}|null, to:{usd_per_unit,source}|null}`，摘要如「CNY：1 CNY = 0.14 USD（原为自动 0.1498 USD）」）、发实时事件 `{type:"settings"}`（机器视图缓存作废，网页重取机器、总览、团队、套餐）。返回同 GET；值没变时什么都不写 |
| DELETE | `/settings/fx-rates/{currency}` | 取消手动覆盖，回到自动的值（源里也没有就是未折算），要 `write`；审计、事件同上 |
| POST | `/settings/fx-rates/refresh` | 立即从汇率源拉一次（不等每天的自动更新），要 `write`。成功返回同 GET；失败 502 `fx_fetch_failed`（中文原因），保留上次的值、记 `last_error`。成功、失败都发 `{type:"settings"}` |

带 USD 折算值的地方（原字段都保留，旧调用方照用）：机器视图 `subscription.{monthly_usd_cents, price_usd_cents, usd_per_unit}`、`/overview` 的 `monthly_usd`、`/teams` 的 `monthly_usd`、`/plans` 的 `{price_usd_cents, monthly_usd_cents, usd_per_unit}`。续费记录（`amount_cents`）、回收站里的价格是历史账目，照原币种，不折算。

## 来源 IP 画像（IP 情报）
离线库在 fleet-server 进程内查询（ip2region v4/v6、DB-IP City/ASN Lite、iptoasn，mmap），**只查本地，绝不把 IP 发往在线服务**。库由 `deploy/ipintel/fetch-dbs.sh`（systemd timer `fleet-ipintel-fetch` 每月一次）下载到 `[ipintel] db_dir`（默认 `/var/lib/fleet/ipintel`），文件 mtime 变化后 5 分钟内自动热重载。库缺失或不全时降级：能查多少查多少，查不到的字段为 null，接口不报错。ASN 分类清单内置在 `crates/fleet-ipintel/data/asn-lists.toml`，可用 `/etc/fleet/ipintel-overrides.toml`（同样的 `[[vpn]] asn=[…] name="…"` 写法，`name=""` 表示删除）覆盖或追加。线路机和节点的 `primary_ip` 自动算「内部地址」（`[ipintel] internal_ips` 可追加运维出口）。全部为只读 GET，读权限即可。

画像摘要 `intel`：`{isp, asn, as_org, country, province, city, place, conn_type(residential|mobile|datacenter|vpn|unknown), conn_label, confidence(high|medium|low), reason, internal(我方机器名|null), private}`。`/access/sources` 与 `/machines/{id}/access` 的来源条目（`top_sources`）都带 `intel`，查不到为 null。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/ipintel/{ip}` | 单个地址的完整画像（`isp asn as_org country region city lat lon type type_word type_confidence type_reason type_alt flags sources dbs{四库原始结果}`）加 `degraded`（库没加载全）、`loaded`。非法 IP 400 |
| GET | `/ipintel?ips=a,b,c` | ≤200 个，`{items:[{ip, intel}|{ip, intel:null, error}], loaded, degraded}`；不写缓存 |
| GET | `/ipintel/status` | `{enabled, installed, loaded, expected:5, db_dir, version, files:[{id, loaded, bytes, mtime, build, entries}], errors, lists（各清单条目数）, overrides{path,used,error}, cache_rows, hint}`；未安装时 `hint` 提示运行 `lab/install-ipintel.sh` |
| GET | `/machines/{id}/sources?window=24h\|7d\|30d&limit=50` | `window` 不能比汇总的保留期长（超了 400）。节点的来源画像（线路机：分布与 top 来源只算经它的流量；`known` 基线与 `anomalies` 按节点记，取它名下入口的节点）：`distribution.{isp,city,province,country,conn_type}[{value,label,conns,sources,pct}]`（只统计外部来源；我方机器和非公网地址分别计入 `internal_conns` / `private_conns`）、`top_sources[{src_ip,conns,last_ts,intel}]`、`known.{isp,city,conn_type}[{value,label,first_seen,last_seen,conns,anomaly}]`（已学基线）、`learning{started_at,ends_at,learn_days,done,remaining_hours}`、`anomalies[]`（进行中的 ipintel 告警）、`empty`、`degraded`、`unresolved_sources` |

**异常告警**（warning，进 incident 表并走通知渠道）：节点第一条来源记录起 7 天内只学习；之后某节点出现新的运营商 → `ipintel_new_isp`，新的城市 → `ipintel_new_city`，某 (节点, 来源 IP) 第一次出现且为机房/VPN → `ipintel_datacenter_source`。跳过非公网地址和我方机器。一次性异常最后一次出现 `alert_ttl_hours`（默认 24）小时后自动恢复，不发恢复通知；同一个值恢复后，只要它还在来源画像里就不会再告警。来源画像（运营商、城市、接入方式、机房/VPN 来源 IP，一个值一行）最近一次出现超过 `profile_days`（见「访问记录保留期」）、且不在告警中的由每小时的清理删掉：之后同一个值再出现算「新的」，会再告警一次；一个节点的画像全删光（闲置超过这么久）后再有流量，学习期从头算。后台每 5 分钟从 `access_source_hourly` 增量维护 `node_source_seen`（水位在 `ipintel_state`；水位之前的小时若被补算，连接数差值经 `access_source_late` 补记，首次扫描按 7 天一页）。

## 退役与回收站
生命周期只有一条路：`在用 →（退役 = 移进备用团队）→ 订阅过期满 1 天 → 清理 → 回收站`。「立即回收」走同一条路，只是不等到期。回收站是一张审计表（`recycle_bin`），机器进去后从所有视图消失；撤销退役 = 把机器移出备用团队（`PATCH /machines/{id}` 改团队），不用重新上架。「恢复已退役机器」与手动「彻底删除」已删掉。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/machines/{id}/retire` | 退役预览，见下 |
| POST | `/machines/{id}/retire` | `{confirm: <显示名>, reason?, now?: false, cleanup?: true}`。默认移进备用团队 → `{mode:"spare", name（备用里的新名字）, from（原名）, recycle_on, ip_nodes:[{id, name, from}], auto_renew_off, notes:[string]}`；`now: true` 立即回收（退役作业）→ `{mode:"now", job_id, ip_node_jobs:[{machine_id,name,job_id}]}`。见下 |
| GET | `/recycle-bin?q=&source=&limit=100&offset=0` | 回收站列表（只读，不含快照），按回收时间倒序：`{items:[RecycleItem], total, next_offset}`。`q`：不分大小写的包含匹配，搜名字（含回收时的名字）、IP、团队、供应商、套餐、供应商编号、宿主机名；`source`：`fleet` / `legacy`（别的值 400）；`limit` 1–500 |
| GET | `/recycle-bin/{id}` | 单条：`RecycleItem` 加 `snapshot`；没有 404 |
| POST | `/recycle-bin/import` | 旧面板的退役机器导入（要 `write`），见下 |

`RecycleItem` = `{id, source:"fleet"|"legacy", source_id, machine_id|null, kind, name, last_name|null, team|null, ip|null, host_name|null, provider|null, plan|null, service_ref|null, price_cents|null, currency|null, cycle|null, activated_at|null, retired_at|null, due_date|null, recycled_at, reason|null, traffic_up|null, traffic_down|null, nic_rx|null, nic_tx|null, actor}`：
`name` 退役前的名字（进过备用的是 `spare_from`），`last_name` 回收时的名字（「备用-03」；立即回收的同 `name`，导入的为 null），`team` 退役前的团队；`retired_at` 进备用的时刻（立即回收的是退役作业完成的时刻）；价格、币种、周期是订阅的生效值（没有订阅取模板）；`reason` 退役原因（到期回收的接上「备用到期自动回收（到期日 …）」）；`traffic_*` 入口业务流量合计（字节，整个生命周期；线路机按它上面全部入口算），`nic_*` 网卡累计（没有网卡读数的家宽代理、同机 IP 节点为 null）。数值的口径以流量改造为准。
`snapshot`（fleet）：`{machine（机器行，不含秘密）, name, team, host, provider, plan, subscription（生效值）, renewals[], entries:[{relay,node,port,lifecycle,created_at,removed_at}], outbound（出站参数，不含秘密）}`；legacy 是导入时给的原始记录。回收站不受保留期清理影响，一直保留。

**退役预览** `GET /machines/{id}/retire` → `{name, renames:[{machine_id,old_name,new_name}], ip_nodes:[{id,name}], spare:{ok, why|null, team:{id,name}, to_name|null, recycle_on|null, ip_nodes:[{id,name,to_name}], notes:[string]（开着的自动续费会关掉、原团队压缩后撤销退役回不到原编号）}, routers:[{id,name,exits:[{exit,node}]}], blockers:[string]}`：
- `renames`：机器离开后原团队压缩编号的改名（两种去向一样）；`ip_nodes`：立即回收时一并回收的同机 IP 节点（宿主机才有）。
- `spare`：能不能移进备用（`ok`，不能时 `why`：线路机、已经在备用团队里、还没上线（上架中或上架失败）、没有订阅、订阅已经过期满 1 天——这些只能立即回收）；进去后的名字（备用团队最小的空号）、自动回收的日子（账单时区）、一起进备用的同机 IP 节点与它们的新名字（已经在备用团队的不算）。
- `routers`：各软路由最近一次采集里还指向这台（和它的同机 IP 节点）的出口，`node` 是指向的那台；只提示、不阻断，软路由不自动改（要换掉先「替换」）。
- `blockers`：立即回收提交时会 409 的原因，与提交时同一段判断（已经退役 / 退役中、线路机上还有节点入口、这台或它的同机 IP 节点有作业正在执行一步）；[] = 能立即回收。

**移进备用**（默认）：一个事务、不建作业。先锁团队（原团队、同机 IP 节点的团队、备用团队）再锁机器；进不了备用（同 `spare.why`）409「不能移进备用：…。要现在就清理并移进回收站，选「立即回收」（now: true）」；它或它的同机 IP 节点有进行中的作业 409；`reason` 最多 2000 字。改到备用团队最小的空号（与 `PATCH /machines/{id}` 同一段代码：审计 `machine.rename`、刷新线路机备注，并记下 `spare_since` / `spare_from`，开着的自动续费关掉：不关的话每到期顺延一期、永远不会回收；审计 `machine.rename` 的 detail 记 `auto_renew_off: true`，移出备用不自动打开），`lifecycle_reason` 记退役原因；宿主机上还没退役、不在备用团队的同机 IP 节点一起进（编号挨着，原因「随宿主机「名字」退役」）；原团队压缩编号。机器照常运行：入口、端口、软路由出口都不动，告警照常（备用期间故障照常通知）。审计 `machine.retire`（`detail.mode = "spare"`，`{name, to, reason, recycle_on, ip_nodes}`；同机 IP 节点各一条，带 `host`），摘要「团队1-01 → 备用-02（换新机）」。
移出备用（改到别的团队）= 撤销退役：`spare_since`、`spare_from`、`lifecycle_reason` 清空。

**立即回收**（`now: true`）：建退役作业 `retire`（摘入口 → 等线路机 → 清理 → 撤令牌 → 收尾），收尾时同一个事务移进回收站。线路机上还有节点入口时 409（提示先搬空，它自己的直出入口不算，由「摘除入口」一并摘掉）；这台（或它的同机 IP 节点）有作业正在执行一步（租约未过期）时 409，先取消并等它停下（升级 agent 不能取消）；`reason` 最多 2000 字；`cleanup: false` 不登录清理。宿主机：它上面还没开始退役的同机 IP 节点同一事务里一并置为退役中（作废它们进行中 / 失败的作业、关告警），各建一个 `cleanup: false` 的退役作业（只摘入口，宿主机的清理连同所有实例一起删），列在 `ip_node_jobs`。审计 `machine.retire`（`detail.mode = "now"`）。

**到期回收**（中控每小时的维护，`spare.rs`；中控启动时先跑一轮）：
1. 已退役、还没进回收站、没有进行中作业的（上架取消的、这一版之前就退役了的）移进回收站（审计 `machine.recycle`，`detail.auto = "retired_sweep"`）。
2. 回收站里的机器行在 `purge_blocker` 放行时自动物理删除（作业 `purge`，操作人「系统」，`auto = "spare_expiry"`；遥测、入口、机器行删掉，回收站这一行留着）：宿主机上还有同机 IP 节点（含已退役的）先删它们；它的入口近 35 天的小时流量还计入另一端未退役机器的统计时等到那天之后。物理删除失败过的不再自动重建（等人重试那个作业）。
3. 备用团队里在用、有订阅、账单时区的今天晚于「到期日 + 1 天」的（`recycle_on` 当天 0 点起）：发起退役作业（同立即回收，带清理，操作人「系统」，作业参数与审计 `auto = "spare_expiry"`），`lifecycle_reason` 接上「备用到期自动回收（到期日 …）」，经通知渠道发一条 `auto`（「自动回收」，发给所有启用的渠道，不看级别）；作业收尾时进回收站。机器（或它的同机 IP 节点）有进行中作业的这一轮跳过；宿主机上有不在备用团队的同机 IP 节点的不回收（留给人处理）。还指向它的软路由出口不拦回收（到期时供应商通常已停机）。
备用团队的机器是替换下来、等到期丢弃的，不续费：**没有软路由出口指向它的不发续费告警** `renewal_due`，也不计入总览的 `due_soon` / `overdue`（网页「即将到期」KPI 与筛选同口径）。还有软路由出口指向它的照样发（回收后这些出口就不通了）：文案写明哪天回收、列出这些出口，过期第 1 天的「已过期 1 天，备用节点明天自动回收；软路由出口 … 还指向它」是最后一次提醒。自动续费的先顺延（见续费），不会被回收。

**移进回收站**（`recycle::record`，退役作业收尾与上面第 1 步共用）：写一行 `recycle_bin`（`source = "fleet"`，`source_id` = 机器 id，同一台只写一次）、设 `machine.recycled_at`、写审计 `machine.recycle`（`{name（回收时的名字）, from（退役前的名字）, recycle_bin_id, auto?}`）。之后这台机器在 `GET /machines`（任何 scope）、`GET /machines/{id}`（404）里都看不到；它的入口与小时流量还留着给线路机的今天 / 本月统计用，物理删除最多推迟约 35 天，用户看不到。

**旧面板导入** `POST /recycle-bin/import`：`{items:[ImportItem], confirm?: false}`。`ImportItem` = `{source_id, kind:"vps"|"proxy"|"relay", name, team?, ip?, host_name?, provider?, plan?, service_ref?, price_cents?, currency?, cycle?, activated_at?（RFC3339）, retired_at?, due_date?（"YYYY-MM-DD"）, reason?, traffic_up?, traffic_down?, nic_rx?, nic_tx?, snapshot?（旧面板的原始记录）}`；`source` 固定为 `legacy`，`source_id` 用旧面板资产的 uuid（没有就用「供应商:service_id」）。1–1000 条；`source_id` / `name` 空、`kind` 不对、`ip` 不是 IP 时 400（「第 N 条（名字）：…」）。
→ `{dry_run, inserted:[{source_id, name, id|null}], skipped:[{source_id, name, why}]}`。默认只预演（`dry_run: true`，`id` 为 null，什么都不写）；`confirm: true` 才写，写审计 `recycle.import`（`{inserted, skipped, names}`，摘要「导入 3 台，跳过 1 台」）。跳过：同一批里重复的、已经导入过的（幂等键 `(source, source_id)`，「已经导入过（回收站 #12）」）、`ip` 或（`provider` 名字或代码不分大小写, `service_ref`）对上面板里还没回收的机器的（「是在用机器 团队2-05」，不在用的写出状态）。

## 生命周期操作：换线路机、搬空线路机、替换节点
都是作业（可续跑、幂等、留日志），接口只做校验、建作业、写审计。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/machines/{old}/replace?new_machine_id=&team_id=&number=&routers=` | 替换节点预览（只读库，不登录路由）。见下 |
| GET | `/entries/{old}/repoint?to_entry_id=&routers=<id,id>` | 改出口指向预览：软路由上指向入口 `old` 的出口改成入口 `to_entry_id`（两个入口都不动），`routers` 只看这几台。见下 |
| POST | `/entries/{old}/repoint` | `{to_entry_id, router_ids?:[…]}` → 预览同样的内容加 `job_id`。新入口就是旧入口、路由不存在 400；新入口不在用、旧入口已删除、没有出口要改、要改的路由不在线 409（见 blockers）。写审计 `entry.repoint` |
| POST | `/machines/{old}/replace` | `{new_machine_id, old_to?:{team_id?, number?}, routers?（默认 true）}` → 预览同样的内容加 `job_id`。输入不对 400（新节点不存在 / 就是旧节点、指定的团队不存在、编号不在 1–999）；不满足条件 409（见 blockers）。写审计 `machine.replace`（不含凭据） |
| GET | `/machines/{id}/move?to_relay_id=&from_relay_id=&routers=` | 换线路机预览（只读库）：`{name, from, to, routers_enabled, routers:[同替换，exits[].new_entry_id 为 null、to 为「server:新端口」], ok, blockers[], warnings[], share_changed, note}`；不能换时 `ok:false`、原因在 blockers |
| POST | `/machines/{id}/move` | `{to_relay_id, from_relay_id?, routers?（默认 true）}` → 预览同样的内容加 `job_id`。节点（vps / proxy）必须 active 且恰有一个 active 入口（有多个时用 `from_relay_id` 指定换掉哪条）；目标线路机必须 active、不同于当前、节点在它上面没有未移除入口；节点有进行中的作业、要改的软路由不在线（blockers）时 409。写审计 `entry.move`（含 routers） |
| GET | `/relays/{id}/evacuate?to_relay_id=&routers=` | 搬空预览（只读库）：同 POST 的响应，`jobs[]` 没有 `job_id` |
| POST | `/relays/{id}/evacuate` | `{to_relay_id, routers?（默认 true）}` → `{from, to, jobs:[{machine_id,name,job_id,routers}], skipped:[{machine_id,name,reason}], warnings, router_restarts:[{id,name,times,nodes}], share_changed, note}`（`router_restarts`：要改的软路由，同批一起改，每台 OpenClash 只重启一次（`times` 恒为 1，断开它上面全部连接），`nodes` 是有出口要改的节点数，多的在前；`routers=false` 时为空）。为该线路机上每个在用入口的节点各建一个 move_entry 作业，参数带同一个 `batch`（一起改软路由，见下）；暂时不能搬的（入口停用/还在应用中、节点有别的作业、目标上已有入口、要改的软路由不在线…）列入 `skipped`，不影响其它节点。线路机自己的直出入口不搬、也不列入 `skipped`（退役线路机时随之摘除；退役守卫只数节点入口）。写审计 `relay.evacuate` 与每个节点一条 `entry.move` |
| GET | `/relays/{id}/rebalance?target_relay_id=&exclude=` | 搬迁建议（只读，什么都不改）：这台线路机（要有流量上限和重置日）按近 7 天日均到重置日会用满时，挑出搬走后能撑到重置日的最少入口，目标线路机默认取「在用、非备用、非故障、额度余量最多」的（不限流量的最多）；`target_relay_id` 指定目标，`exclude` 是不动的节点 id（逗号分隔，如分享给别人的入口所在节点）。入口的流量按近 7 天业务流量算，再按这台网卡日均 ÷ 各入口合计折算；只挑在用的非直出入口、节点在目标上还没有入口的；先拿最大的，剩下的缺口某一个入口就够时换成够用的最小的。`{relay:{id,name}, limit_gb, used, period_end, daily_avg, forecast_full_at, projected_pct（按现在速度到重置日的用量占上限 %）, needed（会用满）, ok（挑出的入口够用）, reason（不用搬 / 搬不够 / 目标放不下的原因）, safe_pct（目标接收后到重置日预计不超过它上限的 %，80）, target:{id,name,limit_gb,used,daily_avg,projected_pct}\|null, moves:[{node:{id,name}, entry_id, port, period_bytes（本周期）, daily_bytes（搬走后这台每天少的），needs_from（节点在别处还有在用入口，move 要带 from_relay_id）}], after:{daily_avg, projected_pct}, target_after:{fits, projected_pct}, excluded:[{id,name}]}`。执行是对每个节点 `POST /machines/{id}/move` |

**换线路机作业 `move_entry`**（目标 = 节点，要求节点 active）步骤：`attach_new`（目标线路机建新入口：新端口、新 client_uuid / short_id，pending）→ `firewall_widen`（vps：节点 nft 放行名单加入新线路机 IP；proxy 跳过）→ `wait_applied_new` → `verify_new`（经新入口的出口探测通过）→ `routers`（软路由上指向旧入口的出口改成新入口，同替换节点的「改软路由出口」：全部在线才动手，逐台备份 → 改 → 重启 OpenClash → 测延迟，一台失败已改的全部换回、作业失败、旧入口不摘；出口写的是旧线路机 IP 就换成新线路机 IP，否则换成新线路机的 public_host，端口用新入口的；要改哪些出口执行时按路由当时的配置重新确定，见替换作业，`routers=false` 跳过）→ `detach_old`（旧入口 removing）→ `wait_detached_old` → `firewall_narrow`（节点在旧线路机上不再有入口时，把旧线路机 IP 从放行名单拿掉）。新入口验收通过之前旧入口不动，节点始终至少有一个可用入口。
- **取消**：`routers` 之前都能取消（`cancellable` 为 true）；补偿 = 摘除新入口、恢复防火墙，旧入口保持不动。`routers` 排队等别的作业改同一台路由时、失败且确认路由都已改回原配置停在这一步时也能取消；路由可能还改着一部分时（参数 `routers_applied` 非空：这一步中途被打断——外层超时、中控重启——或改回失败，`routers_stuck` 非空，作业给出 `manual`）不能取消（会摘掉路由还指着的新入口），重试之后、核对过路由之前也不能，重试照 `routers_applied` 接着改完；搬空同批被带头的认领之后（`routers_batch_leader`）也不能取消；开始改路由之后、摘旧入口之后不能取消（会摘掉路由已经指向的新入口 / 让节点没有入口），失败了只能重试到完成；取消赶在摘除之后到达时保留新入口。
- **同一台软路由一个一个改**：替换、换线路机（含搬空建的、重试的）的 `routers` 这一步按路由加锁（PG advisory lock，键由路由 id 算出，读配置 → 改 → 重启 → 测 / 改回期间持有，一个作业改几台时按 id 顺序加，往前改只试一次、拿不到就全部放开）。别的作业正在改同一台路由、而计划 / 快照 / 之前改过的说这台要改时，这一步不读它的配置、直接排队：作业是 waiting、这一步的 `started_at` 为 null（排队不算这一步的超时），参数 `routers_queued` / `routers_queued_id` 记着在等哪台，日志写一次「软路由「X」正在被别的作业改，排队等它改完」，每 10 秒再试——排队时只试那一台的锁（用连接池里的连接，不读配置、不新建连接）。作业的其余步骤并行。
- **搬空同批一起改**（jobs/steps/batch.rs）：搬空建的作业走到 `routers` 时，同批还有作业没走到这一步（建新入口、验收中）就排队等（`routers_queued` 写原因，能取消）；都到了由 id 最小的带头，把同批停在这一步的认领下来（它们的参数 `routers_batch_leader` = 带头的，带头的 `routers_batch` = 认领的作业），按各自的新入口一起改：每台路由一次备份、一次改完、一次重启，加锁、改回、改不了的判断同上。被认领的作业不能取消（`cancellable` 为 false），等带头的结果：它这一步做完就跟着完成（步骤输出是其中自己的那几个出口，`batch_leader` 是带头的；带头的输出每个出口带 `job`，另有 `batch`），它失败（全部换回、旧入口都不摘）就跟着失败；改回失败要人工处理时跟着的同样记 `routers_stuck`、给出 `manual`，重试也一直跟着，等带头的重试改完再完成。失败停着、取消了的不算同批在等的；之后单独重试的，同批别的已经改过时就自己改。
- **分享链接会变**：新入口有新的端口和凭据。`GET /jobs/{id}` 对 move_entry 多返回 `result:{new_entry_id, old_entry_id, from_relay_id, to_relay_id, share_changed:true, note}`；用 `GET /entries/{new_entry_id}/share` 取新链接（需要 secrets 权限）。作业日志里也有「客户端需要换成新的分享链接」。
- 旧线路机不在线时 `wait_detached_old` 会失败，线路机恢复后重试。
- `routers` 改完（或换回）路由后马上请采集循环采一次这几台（`routers::collect::Refresh`，等它采完，最多 30 秒）：紧接着发起的预览按新配置算出口，不用等下一轮 30 秒。改配置时 ssh 根本没连上（连接 / 握手 / 登录阶段失败，脚本没跑）的那台算没改过：不去换回它，错误写「（没连上，这台什么都没改）」，可以直接重试或取消。
- 线路机退役守卫的错误提示里指向「搬空线路机」。

**替换节点**：新节点接替旧节点——软路由上指向旧节点入口的出口改成新节点的入口，新节点接旧编号，旧节点去备用团队（同「退役」的默认去向：不建退役作业，由备用到期自动回收接手；`spare_from` 记下它原来的名字）。
- 预览 `GET /machines/{old}/replace` 返回 `{old:{id,name,kind,team:{id,name}|null}, new:{同}, new_name（新节点替换后的名字 = 旧节点现在的名字）, old_to:{team:{id,name,spare}|null, number, name}, renames:[{machine_id,old_name,new_name}]（新节点原来的团队压缩编号）, routers_enabled, routers:[{id,name,state:online|offline|pending, exits:[{exit,old_entry_id,new_entry_id,from:"server:port",to:"server:port"|null,error}]}], ok, blockers[], warnings[]}`。新入口对不上的出口照样列出：`to` 为 null、`error` 是原因（同时是 blocker；新节点没有在用入口时只有总的那条 blocker），其余 `error` 为 null。
- 条件：旧、新都是在用（active）的节点（不是线路机）、都没有进行中的作业、旧节点有团队编号、新节点至少有一个 active 入口。新节点可以在别的团队（替换时改到旧节点的团队，warnings 里提示）。旧节点去备用团队时 warnings 还提示什么时候回收：没有订阅（不会自动回收）、已过期满 1 天（一小时内就回收）、开着自动续费（进备用时关掉）。交换编号时旧节点的 `lifecycle_reason` 记「替换下来：新 接替了 旧」，旧节点的来源画像（`node_source_seen`，连接数记 0）复制给新节点，客户换过来不算新出现、新机器不用重新学。`old_to.team_id` 不给时去唯一的备用团队；编号不给取目标团队最小的空号（作业执行时再取一次）。
- 要改的路由：按各路由 `router_status.data` 里的出口（见软路由），出口对上的入口属于旧节点、未删除的都算（`routers::exits_to`，退役预览、备用到期提醒同一套匹配）。新入口按线路机一一对应（同一台线路机上的新入口；没有时新节点只有一个 active 入口就用它，否则 blockers 说明对不上）；出口原来写的是线路机 IP 就换成新线路机的 IP，否则换成它的 public_host。`routers_enabled` 时任何一台要改的路由不在线（`state` 不是 online）都是 blocker；`routers=false` 不看路由、只交换编号。

**改出口指向 `repoint`**（目标 = 旧入口所属的机器，直出入口是线路机自己；不限生命周期）只有一步 `routers`「改软路由出口」，同下面替换的这一步：出口从 `old_entry_id` 改成 `new_entry_id`（属于 `new_id`），匹配同换线路机（出口写的是旧线路机 IP 就换成新线路机的 IP，否则写新线路机的 public_host），参数 `router_ids` 不空时只看这几台路由。入口都不动：不建、不摘、不改编号。预览 `{old:{id,name,node_id,relay_id,port}, new:{同}, router_ids, routers（同替换）, ok, blockers, warnings}`。

**同步软路由出口 `router_sync`**（目标 = 软路由：`target` 是路由的 id 与名称；不限生命周期，一台路由同时只有一个）只有一步 `routers`「改软路由出口」，同替换的这一步：参数和对上的入口不一致（`sync = drift`）、入口还在用（active）的出口改成这个入口现在的 uuid / SNI / 公钥 / short-id，server:port 不变；参数 `exits` 不空时只改这几个。执行时按路由当时的配置重新确定，备份 → 改 → 重启 OpenClash → 测延迟，失败自动改回。发起见软路由的 `POST /routers/{id}/sync`。

**替换作业 `replace`**（目标 = 旧节点，要求 active）步骤：`routers`「改软路由出口」→ `renumber`「交换编号」。发起时按快照算的计划（改哪些路由的哪些出口）存进作业参数 `plan`（不含凭据），只作展示与审计。
- `routers`：要改的出口执行时重新确定：逐台经隧道读在线路由的 OpenClash 源配置（顶层 `secret:` 行不出路由），出口对上的入口属于旧节点（换线路机：就是旧入口）、未删除的都改，和预览同一套匹配——发起之后路由上新加的指向旧入口的出口照改，已经不指向旧入口的不改；和计划不一样时日志写一行「按软路由现在的配置要改：…（发起时的预览是：…）」。不在线的路由按最近一次采集看，有要改的出口就一台都不改，失败（`error_kind=network`）；在线路由的配置读不了（读失败、解析不了）也一台都不改，不论计划、快照里看不看得出它有要改的出口（错误提示确认没用的话重新发起选不改软路由 / `--no-routers`）；路由上没有源配置（读配置脚本以 3 退出）的算没有出口；改回时按路由现在的 `tunnel_port` 连（卡住期间重新接入换了端口也连得上）；配置读到了、但有出口对不上新入口（替换：新节点在旧入口的线路机上没有入口、又有不止一个入口，原因同预览）的，不论在不在计划里，一台都不改，作业失败（错误写「软路由「名」的出口「名」：原因」，同预览的 blocker）。要改的路由加锁（见上）后逐台经隧道执行 `routers/replace.rs` 的脚本：备份源配置（`uci get openclash.config.config_path`）到 `/etc/fleet-backup/openclash/<文件名>.<UTC 时间戳>`、只改出口代理块里的 server / port / uuid / servername（或 sni）/ reality-opts 的 public-key / short-id 6 行（核对它现在指向旧入口或已经是新入口，认不出、行数不对一行不写）、重启 OpenClash、等 mihomo API、逐个测出口延迟（generate_204，各 3 次）。一台失败：它自己改回，之前改好的按各自第一次改之前的备份改回，作业失败。**改回只动本作业改过的出口**：改回按本作业写进路由的整组出口参数（server、port、uuid、servername、public-key、short-id）核对，6 项都对上、现在仍是本作业写的才把那 6 行换回备份里的原值；之后被别的作业（或人）改走的（哪怕 server:port 相同、凭据不同）不动，作业日志写一行说明。写入的那一组在改路由时就记下，改回照记下的比对、不从库里现取，所以作业卡住期间新入口被轮换也不会误判成「已改回」。路由备份按文件名里的时间保留 7 天、最多 30 份，失败或需要人工处理（还没做完）的作业引用的备份一份不删；备份不在了而出口还指向本作业改成的地址时报错。`routers_applied` 参数记着改过的路由、第一次改之前的备份时间戳、要改的出口与写进各出口的整组参数（含凭据，参数不经 API 返回），确认都改回了、或这一步做完（成功时与 `routers_stuck` 一起清空）才清掉；中途被打断、改回失败时它留着，重试照它接着做（不重新确定出口、备份戳沿用第一次的）。改回失败的另记进 `routers_stuck`（`[{id,name,exits,was}]`，`was` 是各出口原来指向的 server:port），作业给出 `manual`，这时不能取消。ssh 自己失败时错误写中文原因（握手超时、连接被拒、连接中途断了……），ssh 的英文原始输出另记一行作业日志。路由开了 `router_self_proxy`（路由自身流量走 OpenClash）时，重启 OpenClash 会掐断隧道和这条 ssh：脚本主体在路由后台跑（不理 HUP / PIPE）、结果写进 `/tmp/fleet-replace.res.<MODE>.<STAMP>.*`，连接断了中控等隧道回连（最多 3 分钟）后取回结果照常判断，作业日志写「连接中途断了…等隧道回连后取结果」「隧道回连了，取回路由上的结果」。凭据执行时从库里取（同分享链接），只进 ssh 的 stdin，不进日志与审计。没做完的作业（`routers_applied` 不空）引用的新入口所在的节点和线路机，回收站不会物理删除（自动的和手动的都一样），作业结束或取消后放行。步骤输出 `{routers:[{id,name,exits:[{exit,to}],backup,delay_ms[]}]}`。没有路由要改、或 `routers=false` 时跳过。
- `renumber`：一个事务里先锁团队再锁两台机器，旧节点改到 `old_to`、新节点改成旧团队 + 旧编号，与 `PATCH /machines/{id}` 同一段代码（审计各一条 `machine.rename`、刷新入口所在线路机）；新节点原来的团队压缩编号（审计 `team.renumber`）。已经换好（重跑）的跳过；旧节点的团队编号在发起后被改过则失败（不猜）。
- **取消**：只在开始改路由之前——排队中、`routers` 排队等路由，或 `routers` 失败（路由都已换回）停在第一步；之后只能重试到完成。重试从失败那一步重来。

## 探测点（探测顺序）
线路机客户入口通不通（卡片心跳格、入口的 `reach`、`relay_unreachable`）原来只有中控测；中控在德国，客户在中国大陆，所以可以加「探测点」：放在靠近客户的地方（如家里 PVE 上的容器、香港）的小机器，跑 `fleet-agent probe`，每 30 秒按同样的轮换与作废规则测一轮报回来（为什么见 DESIGN.md「探测点」，代码 `probe_points/`）。

**探测顺序**存在 `app_setting` 的 `reach_source`（`{"order": ["<探测点 id>", …], "central": true}`，迁移 0076 把旧的 `primary` 换成了它）：按顺序用第一个能用的探测点，判定、心跳格、入口表只看那一路；第一个是**主探测来源**，顺序为空时主来源是中控。主来源的探测点用不了（超过 180 秒没上报、在线但 180 秒里的检查都作废了、停用、删掉）时：由顺序里下一个能用的探测点代测，都用不了时中控的检查开着就由中控代测（`effective.state = fallback`：连不上照报，摘要写「香港探测点（代替离线的PVE探测点）连续 3 次以上…」「中控（代替离线的香港探测点）…」；连得上不算恢复——主来源看到的问题代测的那一路未必看得到），中控的检查也关着就标灰（`none`：不开也不关告警）。并开告警 `probe_point_offline`（警告；对象是探测点，告警列表里 `subject` / `subject_id` 为 null、`subject_name` 是探测点的名字）：启用的、上报过的探测点超过 180 秒没上报；上报恢复后恢复，停用、删除时关掉不发恢复。`relay_unreachable` 的摘要写明是哪一路测的：「中控连续 3 次以上连不上客户入口（…）」「香港探测点连续 3 次以上…」（连续 3 次健康状态就变故障，告警再持续 60 秒才开，开的时候一般已经更多次）。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/settings/reach-source` | `{order: [{id, name, label}], primary_name, central_enabled, effective: {state: central\|probe\|fallback\|none, probe_point_id, label, primary_label, primary_issue, substitute}, offline_after_s: 180}`。`order` 是探测顺序，`primary_name` 是主来源的名字（顺序为空时「中控」）。`effective.probe_point_id` 是此刻用的是哪个探测点的记录（中控为 null），`substitute` 是 fallback 时代测的是谁（「香港探测点」「中控」），其余为 null。`effective.label` 是卡片、告警里的说法（「中控」「香港探测点」「香港探测点（代替离线的PVE探测点）」「中控（代替离线的香港探测点）」）；`primary_issue` 是 fallback / none 时主探测点为什么用不了（「香港探测点离线」「香港探测点探测无效」「香港探测点已停用」「香港探测点还没上报」「主探测点已删除」），其余为 null |
| PUT | `/settings/reach-source` | `{order?: [探测点 id, …], central_enabled?: bool}`，没给的保持原样，要 `write`；`order: []` = 主来源改成中控。探测点要存在（400）、不重复（400）、最多 8 个（400）、启用（409）；顺序为空时不能关中控的检查（409）。审计 `settings.reach_source`（`detail:{from, to, order_names}`），发 `{type:"settings"}` 与 `{type:"machines"}`。返回同 GET |
| GET | `/probe-points?window=1h\|24h\|7d\|30d` | 列表：`[{id, name, label, enabled, note, state: online\|blind\|offline\|pending\|disabled, installed, primary, rank, maintenance, last_seen_at, last_kept_at, discard_reason, window, last_ip, version, created_at, bootstrap_expires_at, install: {state: running\|done\|failed, step, error, host, port, started_at, finished_at} \| null, upgrade: {sha256, version} \| null, upgrade_failed: {sha256, to, from, at} \| null, relays: [{relay_id, name, avg_ms, median_ms, total, ok}]}]`。`state`：`blind` = 在线、但 3 分钟里没有一轮检查被采纳（每轮都因对照目标连不上作废），同离线处理（轮到探测顺序里的下一个 / 中控 / 标灰、开 `probe_point_offline`）；`primary` = 探测顺序里的第一个，`rank` = 排第几（从 1 起，不在顺序里为 null；在顺序里的不能停用、删除，409），这时 `discard_reason` 是最近一轮作废的原因，否则为 null；在线、离线、作废都扣掉中控收不到上报的时段。`last_kept_at`：最近一次被采纳的轮次（刚上线、离线后刚回来时从那一刻算起）。`maintenance`：在维护窗口里（它自己的或全部的，见 `/silences`）。`relays` 是近 `window`（默认 1h，别的值 400）到各线路机：平均 / 中位建连延迟（只算通的）、连了几次、通了几次；1h 读原始记录，24h / 7d / 30d 读小时汇总 `relay_reach_hourly`（按整点、含当前小时；平均延迟按各小时通的次数加权，中位数是各小时中位数的中位数）。`GET /probe-points/{id}` 是其中一个（1h） |
| GET | `/machines/{id}/reach-quality` | 线路机抽屉「连接质量」：各路来源连这台线路机客户入口的统计，`{sources: [{source: "central" \| 探测点 id, label, primary, windows: {"24h": {total, ok, avg_ms, median_ms} \| null, "7d": …, "30d": …}}]}`，来自小时汇总（同上）。中控在前、探测点按名字，30 天里没测过的来源不出现；中控只判通不通，`avg_ms` / `median_ms` 为 null；`primary` = 设置里的主探测来源（探测顺序的第一个，顺序为空时是中控）。不是线路机 400，没有这台 404 |
| POST | `/probe-points` | `{name, note?, ssh?: {host, port?, reset_host_key?}}`。名称 1–40 字、不含引号 / 反斜杠 / `$` / 反引号 / 控制字符，不能叫 central / 中控，不重名（409）。给 `ssh` 由中控经 SSH 安装（立即返回 `{id}`，进度看 `install`，见下）；不给返回 `{id, bootstrap: {cmd, expires_at}}`：一次性安装命令（30 分钟、只能用一次）。审计 `probe_point.create` |
| POST | `/probe-points/{id}/install` | 重装（换新令牌）：`{ssh?}` 同上；不给 `ssh` 发一条新的一次性安装命令（之前没用的那条作废；现在的令牌要等新命令执行了才作废）。正在经 SSH 安装时 409。审计 `probe_point.install`（`method: ssh\|command`；SSH 的先写「已开始」，结束时补 `result`） |
| PATCH | `/probe-points/{id}` | `{name?, note?, enabled?}`。主来源不能停用（409）。停用的探测点取目标拿到空列表、不测，上报照收（算心跳），不开离线告警。审计 `probe_point.update` |
| DELETE | `/probe-points/{id}` | 令牌作废、它的检查记录一起删（`relay_reach`、`relay_reach_hourly` 级联）、经 SSH 装过的从中控 known_hosts 删掉；主来源不能删（409）。返回 `{ok, cleanup}`：在探测点上执行的清理命令（中控不去动它）。审计 `probe_point.delete` |
| POST | `/probe-points/{id}/upgrade` | 记下 dist 里 fleet-agent 的 sha256（`upgrade`），探测点下次取目标（30 秒内）时自己下载、核对 sha256、试跑 `--version`、把旧的留成 `fleet-agent.prev` 换上、写试运行标记、退出让服务管理器拉起；报上来的程序 sha256 对上就清掉。新程序 5 分钟内一次都没上报成功就自己换回 `.prev`（记下这个 sha256，之后不再换成它）；中控在探测点下载程序 5 分钟后还没报上目标 sha256 时清掉 `upgrade`、记下 `upgrade_failed`（`{sha256, to, from, at}`，列表里显示，之后报上目标 sha256 就清掉），不再下发，要再调一次这个接口。新探测点在上报里带上它记着的坏版本（`bad_sha256`），中控存下：要它升级到的正是这个时当场记 `upgrade_failed`、不再下发（不等下载后的 5 分钟——它不会去下载）。清掉上次的 `upgrade_failed`。dist 里没有程序、读不出版本 409，还没装好 409，dist 里的正是这个探测点报过的坏版本 409（「这个版本在「X」上跑不起来过…修好后发新版本」：它不会再换成这个版本；重装探测点清掉记录）。返回 `{id, name, from, version, sha256}`。审计 `probe_point.upgrade` |
| GET | `/probe-bootstrap/{令牌}` | 公开（不要登录，同 `/router-bootstrap`）：令牌对、没过期才给安装脚本，给了就作废；同时发上报令牌（只在这份脚本里）。同一来源 10 分钟最多 20 次。审计 `probe_point.bootstrap_used`（操作者 `probe-bootstrap`，带来源 IP） |

经 SSH 安装（`ssh`）：中控用自己的密钥（`ssh_key`）登录 `root@host:port`，主机指纹首次记录到 `known_hosts`、之后严格校验（指纹变了失败，确认重装过系统再带 `reset_host_key: true`）；登不进时 `install.error` 里有中控公钥全文与加进 `/root/.ssh/authorized_keys` 的命令。步骤：连接探测点 → 登录探测点（要 root、x86_64）→ 上传 fleet-agent（`/opt/fleet/fleet-agent`）→ 安装服务（换新令牌）→ 等待首次上报（90 秒）。中控重启时还在跑的标为失败。安装脚本（两种方式共用，POSIX sh）：系统用户 `fleet-probe`、`/opt/fleet/`（归它，升级时它自己换程序）、`/etc/fleet/probe.toml`（0600：`server_url`、`token`、`name`）；systemd 写 `/etc/systemd/system/fleet-probe.service`，OpenRC 写 `/etc/init.d/fleet-probe`（`supervise-daemon` 守护、`rc-update add … default` 开机启动、日志 `/var/log/fleet-probe.log`）。

探测点用的 `/agent/v1/probe/*`（`Authorization: Bearer <探测点令牌>`，与机器的 agent 令牌是两套，互相用不了；无效 401）：
- `GET /agent/v1/probe/targets` → `{interval_s: 30, controls: ["1.1.1.1:443", "8.8.8.8:443", "223.5.5.5:443"], relays: [{relay_id, host, ports: [升序]}], upgrade?: {sha256, version}}`：在用的、有在用入口的线路机（host 是 `public_host`）；停用的探测点 `relays` 为空。
- `POST /agent/v1/probe/report` ← `{version, sha256?, discarded?, all_down?, bad_sha256?, results: [{relay_id, port, ok, rtt_ms?, detail?}]}`：`bad_sha256`：换上后没能上报、自己换回了旧版本的程序 sha256（`.fleet-agent.bad-sha256`，没有不写；旧探测点不带、旧中控忽略），中控存进 `probe_point.bad_sha256`（每份上报覆盖），要它升级到的正是这个就记 `upgrade_failed`、清掉升级请求。每台线路机连第 `turn % 端口数` 个端口，`rtt_ms` 是内核量的建连往返时间；对照目标都连不上时 `discarded` 写原因、`results` 为空（只算心跳）。对照目标通、三台以上线路机全部连不上时照常带结果、`all_down: true`（默认 false、false 时不写）：中控照记、行上标 `all_down`，`relay_unreachable` 摘要后面加「；全部线路机同时连不上（可能是供应商整段故障）」。兼容：旧探测点在这种情况下仍整轮作废（`discarded` 写「N 台线路机全部连不上」）；旧中控不认识 `all_down`，按旧规则丢掉这一轮。中控只收在用线路机的在用入口端口（别的丢掉，每台一条），`detail` 前加「端口 N：」，时间取中控收到的时刻；刷新 `last_seen_at`、`version`、`last_ip`；没作废的刷新 `last_kept_at`、清空 `discard_reason`，作废的记下 `discard_reason`。同一个探测点 10 秒内第二份回 429。返回 `{accepted, interval_s}`。
- `GET /agent/v1/probe/binary`：dist 里的 fleet-agent（安装命令下载、升级时用）。

表：`probe_point`（迁移 0046），`relay_reach` 加 `probe_point_id`（null = 中控）、`rtt_ms`（中控为 null），索引改成带来源的 `(relay_id, probe_point_id, ts desc)`、`(relay_id, probe_point_id, port, ts desc)`；保留期 `[retention] reach_days`（默认 2 天）。
迁移 0049：`probe_point.last_kept_at`、`discard_reason`；`relay_reach.all_down`；小时汇总 `relay_reach_hourly (relay_id, probe_point_id, hour, total, ok, rtt_avg, rtt_p50)`（唯一索引按 `coalesce(probe_point_id, 全 0 uuid)`，随遥测小时汇总每 5 分钟重算当前小时，一直留着，线路机物理删除 / 探测点删除时级联删）。

## 三网 PING（线路机往外测线路质量）
线路机上的 agent 每 60 秒一轮往外连一组目标，每个目标 5 次，记通了的那几次的延迟中位数与丢了几次（`ping.rs`、agent 的 `ping.rs`）。只展示、不开告警。与探测点分工：探测点管客户连不连得上入口（可用性、`relay_unreachable`），这里看线路的延迟与丢包。节点不测（客户不直连节点）。

- 目标：全局 8 个槽位（`app_setting` 的 `ping_targets`：`{mode, slots: [{name, target}] × 8}`），默认 武汉电信 `hb-ct-v4.ip.zstaticcdn.com`、武汉联通 `hb-cu-v4.ip.zstaticcdn.com`、武汉移动 `hb-cm-v4.ip.zstaticcdn.com`、BGP（留空，网页占位提示 `ip.zstaticcdn.com`）、Node 1–4（留空）；目标留空的不测。目标写 `host`、`host:port`、`[IPv6]:port` 或裸 IPv6。每台线路机可以单独覆盖（`relay.ping_override`：`{mode: null | tcp | icmp, slots: [null | {name, target}] × 8}`，null = 跟随全局；覆盖成空目标 = 这台不测这个槽位）。
- 方式：`tcp`（默认）建连量内核往返时间，目标不写端口时连 **80**（测速地址一般都开 80，443 不一定），每次超时 3 秒、间隔 0.2 秒；`icmp` 跑线路机上的系统 `ping -n -c 5 -i 0.5 -W 2`。agent 的 systemd 单元不给 `CAP_NET_RAW`、带 `NoNewPrivileges`，iputils 的 ping 拿不到原始套接字时改用 ICMP 数据报套接字（要 `net.ipv4.ping_group_range` 含 root 组，systemd 默认是全部组）；系统没有 ping、或报没权限 / 建不了套接字时这一轮退回 tcp，样本的 `mode` 写 tcp。
- 下发：随期望状态（`DesiredState.ping = {interval_s: 60, count: 5, mode, targets: [{slot, name, target}]}`，只含有目标的槽位，一个都没有为 null）。改全局时全部在用线路机代数 +1，改覆盖时那一台 +1。旧 agent 不认识这个字段，照常应用、不测。
- 上报：`Report.ping = [{slot, ts_ms, target, mode, sent, lost, rtt_ms}]`（旧 agent 不带、旧中控忽略）。只收线路机的、槽位 0–7、7 天以内的；同一 (线路机, 槽位, 时刻) 重报的不重复写。
- 存：`ping_result`（每台每目标每分钟一行，保留 `[retention] ping_days`，默认 7 天）、`ping_hourly`（小时汇总：发了几次、丢了几次、延迟的平均与最大，保留 `ping_hourly_days`，默认 90 天；后台每 5 分钟重算最近 7 小时），迁移 0048。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/settings/ping-targets` | `{mode, slots: [{name, target}] × 8, modes: ["tcp","icmp"], interval_s: 60, count: 5, tcp_default_port: 80, defaults}` |
| PUT | `/settings/ping-targets` | `{mode, slots}`（正好 8 个），要 `write`。名称最长 20 字（空的按「Node n」），目标格式不对、方式不对、槽位数不对 400。审计 `settings.ping_targets`（`detail:{from,to}`），全部在用线路机代数 +1，发 `{type:"settings"}`。返回同 GET |
| GET | `/machines/{id}/ping` | 线路机：`{mode, slots: [{slot, name, target, source: global\|override}], override, global, current: [同 /ping-beats 的一行]}`；不是线路机 404 |
| PUT | `/machines/{id}/ping` | `{override: null \| {mode?, slots?}}`：null 或全空 = 全部跟随全局。审计 `relay.ping_override`，这台代数 +1，发 `{type:"machine"}`。返回同 GET |
| GET | `/ping-beats` | 卡片：`{<线路机 id>: [{slot, name, target, mode, cur_ms, cur_loss, cur_ts, cells}]}`，每个在测的目标一行（按此刻合成的槽位；目标改过的只算新目标的数据）。`cells` 是近 1 小时 20 格（同 `/probe-beats` 按墙钟 3 分钟对齐，旧→新），每格 `[延迟中位数 ms, 丢包 %]`，没有数据为 `[null, null]`；`cur_*` 是 15 分钟内最近一次（`cur_loss` 是 %）。没有目标的线路机不出现 |
| GET | `/machines/{id}/ping-series?range=1h\|6h\|24h\|7d\|30d` | 抽屉曲线：`{range, step_s, series: [{slot, name, target, points: [{ts, rtt_ms, loss_pct}]}]}`；1h–24h 每分钟的原始点（`step_s` 60），7d / 30d 小时汇总（3600，`rtt_ms` 是这一小时各分钟中位数的平均）。别的 range 400 |

网页的分档（`lib/ping.ts`）：延迟 ≤ 220 ms 绿、≤ 320 ms 黄、更高红（跨境 150–200 ms 是常态，不按国内标准标红）；丢包 0 绿、≤ 10% 黄、更高红。

## 软路由（节选）
`GET /routers`、`GET /routers/{id}`：`state` 为 `pending`（还没接入，或重新接入后还没连上）/ `online` / `offline`（接入过、隧道连不上；采集每 30 秒一轮，隧道连上、断开时中控马上采这台一次，`state` 随即变、发 `routers` 事件）；`error` 在 offline / pending 时是连不上的原因，在 online 时不为空 = 连得上但采集出错（脚本出错、输出读不了、超时而隧道还通），`summary` / `detail` 是上一次成功采集的。`detail.openclash` 另有 `config_error`（配置取不回、解析不了的原因）、`config_stale`（这时出口与入口的对应沿用上次读到的）、`delay_error`（最近一次延迟测试没做成的原因，测成了为 null；出口沿用上一次）；`detail.system.clock_skew_s` = 中控时间 − 路由时间，正数是路由慢。`maintenance`：在维护窗口里（它自己的或全部的，见 `/silences`），卡片显示「维护」。实时事件 `{type:"routers"}` 只在状态、错误或快照里看得见的部分（接口通断、OpenClash 状态与配置、出口的对应与延迟测试结果、在线客户端）变了时发，CPU / 速率等每轮都变的数值不触发。
`DELETE /routers/{id}`：还有没做完的作业改过它的出口、没确认改回（参数 `routers_applied` 里有它）时 409，消息与 `detail.jobs` 给出作业 id。`?force=true`（路由回不来了）照删：把它从这些作业的 `routers_applied` / `routers_stuck` 里摘掉、作业日志记一条「改回放弃」，作业能重试收尾或取消；这些作业里有正在执行的仍 409。返回多带 `jobs`（摘掉了哪些作业），审计详情带 `force`、`jobs`。
`GET /routers` 与 `GET /routers/{id}` 另带 `switches`：进行中的临时切换 `[{group, from, to, back_at, by, switched_at}]`（`from` 是第一次切之前选中的、切回到它，`back_at` 为 null = 不自动切回）。`detail.groups`：策略组 `[{name, type, now, all}]`（mihomo /proxies 里有成员列表的：`type` 是 mihomo 的类型，`Selector` = select 组；`now` 当前选中的；mihomo 的 API 调不通时为空）。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/routers/{id}/sync?exits=<名,名>` | 同步出口参数预览（按最近一次采集）：`{router:{id,name}, routers:[{id,name,state,exits:[{exit, old_entry_id, new_entry_id, from, to, error}]}], ok, blockers, skipped, warnings}`；`from` / `to` 都是 server:port（不变，换的是凭据）；`skipped`：参数不一致、但对上的入口已删除 / 停用、同步不了的（改指向别的入口用 repoint）；点了名的出口没有、不是面板入口、已经一致、入口不在用，或路由不在线，进 `blockers` |
| POST | `/routers/{id}/sync` | `{exits?:[…]}`（空 = 全部不一致的）→ 预览同样的内容加 `job_id`（作业 `router_sync`）。有 blocker 409（`detail.blockers`），这台已经有没做完的同步作业 409。审计 `router.sync`（`detail:{name, exits, job_id}`） |
| POST | `/routers/{id}/switch` | 临时切换：`{group, to, back_after_s?}`（60 秒–7 天，不给 = 不自动切回）。按最近一次采集核对：组要在、是 select（url-test / fallback / load-balance 400 并说明）、`to` 在组里（400）、现在选中的不是它（409）、路由在线（409）；再经隧道调 mihomo `PUT /proxies/<组>` 并读回确认（路由上失败 409，带原因）。不改配置、不重启 OpenClash。返回 `{group, from（切之前选中的）, to, original（第一次切之前的，切回到它）, back_at, ended}`；这组已经在临时切换中时只换 `to` 与 `back_at`，切到的正好是 `original` 就等于切回（`ended: true`）。审计 `router.switch`（切回时 `router.switch_back`） |
| POST | `/routers/{id}/switch/back` | `{group}`：切回第一次切之前的出口；没有进行中的临时切换 404。审计 `router.switch_back`（`detail.why`「手动切回」） |

到 `back_at` 由采集循环（每 30 秒一轮）自动切回，审计 `router.switch_back`，操作者「系统」，`why`「到时自动切回」；切不了（路由不在线、mihomo 不通）下一轮再试。路由重启、OpenClash 重载后组选择回到配置默认值：采集时发现组现在选中的不是切到的（或组没了），记录结束、待切回取消，审计 `router.switch_reverted`（`detail:{name, group, to, now, back_at, note}`）；只看切换生效之后才开始的采集。记录在 `router_switch`（迁移 0078）。

`POST /routers/{id}/deploy`（重新部署）：上传密钥、启动隧道两步失败、路由上还没换成新密钥时，原来的隧道公钥、端口写回去（`deploy.error` 末尾写「已改回原来的隧道密钥与端口」）。
`GET /routers/{id}/metrics?range=` 与 `GET /routers/{id}/exits/metrics?range=`：`range` 为 `1h`（30 秒一点，原始点）/ `6h` / `24h`（5 分钟一点，原始点取平均；不传为 24h）/ `7d` / `30d`（1 小时一点，小时汇总），别的值 400，路由不存在 404。出口的返回 `{step_s, exits:[{name, points:[{ts, delay_ms, alive, conns, up_bps, down_bps, tests, tests_ok}]}]}`（按出口名、点按时间排）：延迟每 5 分钟测一轮，`tests` / `tests_ok` 是这一点里测了几次、通了几次——原始点测了的那一点 1 / (1 或 0)，没测的 0 / 0（`alive`、`delay_ms` 为 null）；5 分钟点是这 5 分钟的次数（一般 1 次），`alive` = 全通（没测为 null），`delay_ms` = 通了的平均；小时点是这一小时的次数，`alive` = 全通，另带 `alive_pct`（测通的百分比），`delay_ms` = 通了的平均。`conns` / 速率在聚合点里是平均。`fleetctl router exits` 按这些点算每个出口的统计。

`GET /routers/{id}` 的 `detail`（最近一次采集的快照）里 `clients[]`：`{ip, mac, hostname, online, conns, down_bps, up_bps, exits:[{name, conns}]}`。`down_bps` / `up_bps` 是这台客户端经 OpenClash 的下载 / 上传速率（bit/s）：按 mihomo 连接的来源 IP（`sourceIP`）汇总，取差方式同出口速率（同一连接两次采集的字节数之差累加、新连接全算，除以两次采集的间隔）；没有连接的客户端为 0；路由上第一次采集（没有上一次的读数）、mihomo API 调不通时为 null（同出口的 `up_bps` / `down_bps`）。上一次的读数存在路由上，中控重启不影响它们；只有 WAN 与 OpenClash 总速率（`summary.wan.rx_bps` / `tx_bps`、`openclash.up_bps` / `down_bps`）用中控内存里的上一次读数，中控重启后的第一轮为 null。

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
