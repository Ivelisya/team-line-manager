# ipintel 接入 fleetd

本 crate 是独立的库（`fleet/crates/ipintel/`），还没有加进 workspace，fleetd 也还没有调用它。下面是接入需要改的全部地方，按顺序做。所有路径相对 `fleet/`。

## 0. 它提供什么

| 能力 | 入口 |
|---|---|
| 离线库查询（ip2region v4/v6 xdb、DB-IP City/ASN Lite mmdb、iptoasn TSV，全部 mmap） | `ipintel::lookup::Dbs::open(dir)` → `.lookup(ip, internal)` |
| 服务（库 + 状态库 + 检测器） | `ipintel::IpIntel::open(cfg) -> Arc<IpIntel>` |
| 输入 | `upsert_nodes(iter NodeMeta)`、`ingest_sources(iter SourceEvent)`、`ingest_egress(iter EgressObs)`；旧系统 JSON 适配在 `ipintel::legacy`（`health_nodes` / `health_egress` / `activity_events`） |
| 告警 | `observations(now) -> Vec<AlertObs>`；规则集合 `ipintel::RULES` |
| 定时维护 | `maintain(now)`（每 60 s）、`reload_if_changed()`（每 10 min） |
| HTTP | `ipintel::api::router::<S>(svc) -> Router<S>`（axum 0.8） |
| 库下载 | `deploy/fetch-dbs.sh` + `deploy/fleetd-ipintel-fetch.{service,timer}` |

## 1. workspace 与依赖

`fleet/Cargo.toml`：

```toml
[workspace]
resolver = "2"
members = ["fleetd", "crates/ipintel"]
```

`fleet/fleetd/Cargo.toml` 的 `[dependencies]` 加一行：

```toml
ipintel = { path = "../crates/ipintel" }
```

依赖版本已与 fleetd 对齐（axum 0.8 同一组 feature、rusqlite 0.40 bundled、serde_json 同 feature），不会引入第二份 libsqlite3-sys。新增的传递依赖：`maxminddb 0.32`（feature `mmap`）、`memmap2 0.9`、`regex 1`、`ipnetwork`。release profile 沿用 workspace 的（`opt-level="s"`、`panic="abort"`）。

## 2. 配置

`fleetd/src/config.rs` 的 `Config` 加字段（`ipintel::Config` 自带 `#[serde(default)]`，整段不写也能跑）：

```rust
#[serde(default)]
pub ipintel: ipintel::Config,
```

`deploy/fleetd.toml.example` 追加（下面都是默认值）：

```toml
[ipintel]
enabled = true
db_dir = "/var/lib/fleetd/ipintel"        # fetch-dbs.sh 的 DEST
state_db = "/var/lib/fleetd/ipintel.db"   # 本 crate 独占的 SQLite（WAL）
cache_ttl_days = 30                       # 情报缓存 TTL；库换版后缓存按版本号自动失效
learn_s = 604800                          # 节点学习期 7 天（按事件时间算；启动回填 14 天历史后立即生效）
alert_ttl_s = 86400                       # 一次性异常（新运营商/新城市/机房来源/一号多地）最后一次出现后 24 h 自动结束
shared_window_s = 600                     # 一号多地窗口 10 min
shared_min_km = 150                       # 两地至少相距 150 km（缺坐标时退回“省不同”）
shared_ignore_clients = []                # 不参与一号多地的客户端标识
egress_confirm = 2                        # 新出口 IP 连续测到 2 次（不同 measured_at）才确认变更
retention_days = 35
internal_ips = ["38.58.57.22"]            # 必填全部 3 台线路机的公网 IP（+ 运维出口）：/health 里线路机名称不带 IP，自动加入的只有落地节点出口。
                                          # 不填的话，线路机自己的探测连接（AS979 NetLab，机房）会触发“机房来源”告警

[ipintel.severity]
ipintel_new_isp = "warning"
ipintel_new_city = "warning"
ipintel_shared_identity = "warning"
ipintel_datacenter_source = "warning"
ipintel_egress_ip_changed = "critical"
```

`check-config` 里可以加一行：`enabled` 时 `db_dir` 不存在只告警、不报错（库缺失时服务照常起来，查询返回空字段，`/ipintel/status` 里 `errors` 列出缺哪个）。

## 3. App 状态

`fleetd/src/app.rs`：

```rust
pub struct App {
    // …
    pub ipintel: Option<Arc<ipintel::IpIntel>>,
}
```

`App::build` 里（打开 store 之后）：

```rust
let ipintel = if cfg.ipintel.enabled {
    match ipintel::IpIntel::open(cfg.ipintel.clone()) {
        Ok(s) => { tracing::info!(status = %s.status()["version"], "ipintel ready"); Some(s) }
        Err(e) => { tracing::warn!(error = %e, "ipintel disabled"); None }
    }
} else { None };
```

打开四个库约 170 ms（a395 实测，主要是给 iptoasn 建行偏移索引 + 扫一遍 AS 描述建机房名单）。

## 4. 路由

`fleetd/src/api.rs` 的 `router()`，在 `.fallback(not_found)` **之前**合并（这样 ipintel 的路由也走 fleetd 的 `guard`：Komari 管理员会话、`X-Fleet-Request: 1` 写校验、安全响应头）：

```rust
pub fn router(app: AppState) -> Router {
    let mut r = Router::new()
        .route("/v2/api/legacy/{*path}", get(legacy_proxy))
        // … 原有路由 …
        .route("/v2/api/v1/probes/latest", get(probes_latest));
    if let Some(ii) = &app.ipintel {
        r = r.merge(ipintel::api::router::<AppState>(ii.clone()));
    }
    r.fallback(not_found)
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}
```

`guard` 里鉴权通过后（`req.extensions_mut().insert(ident)` 那一行旁边）加一行，关闭异常时记录操作人：

```rust
req.extensions_mut().insert(ipintel::api::Actor(ident.username.clone()));
```

路由一览（全部 `{"data": …}` / `{"error": "中文"}`）：

| 方法 路径 | 说明 |
|---|---|
| `GET /v2/api/v1/ipintel/{ip}` | 单个 IP：`intel`（isp、asn、as_org、country/region/city、lat/lon、`type` residential/mobile/datacenter/vpn/unknown + `type_reason`/`type_confidence`/`type_alt`、`flags`、`sources`、`dbs` 四库原始结果）、`seen.as_source`（哪些节点、首末时间、次数、用过的客户端标识）、`seen.as_egress`（是哪个节点的出口）、相关异常。非法 IP → 400 |
| `GET /v2/api/v1/nodes/{id}/sources?window=1h\|24h\|7d\|14d\|30d\|<秒>&limit=50` | `id` 可以是资产 uuid、短名 `t1-01` 或名称。`distribution.{isp,city,province,country,asn,type}`（按连接数，带 `pct`、来源个数）、`top_sources`（带情报）、`known`（学到的运营商 / 城市基线）、`egress.current` / `egress.history`（带情报）、该节点的异常。未知节点 → 404 |
| `GET /v2/api/v1/ipintel/anomalies?status=open\|closed\|all&kind=&node=&severity=&since=&limit=` | 异常列表（open 在前，按级别、时间），`counts.open / by_kind / by_severity` |
| `POST /v2/api/v1/ipintel/anomalies/{id}/close`（写） | body 可选 `{"note":"…"}`。出口 IP 变更只能这样结束。已关闭 / 不存在 → 409 |
| `GET /v2/api/v1/ipintel/status` | 库文件、版本、加载错误、iptoasn 机房名单大小、表行数、进程内存 |

写操作审计：`close` 在 fleetd 侧包一层最省事——或者在 `api.rs` 里给这个路径单独加 handler，调用 `ii.close_anomaly(...)` 后 `store::audit(c, now, actor, "ipintel.anomaly.close", &id.to_string(), &json!({"note": note}))`。

## 5. 后台任务（`app.rs` 的 `spawn_background`）

```rust
if let Some(ii) = app.ipintel.clone() {
    tokio::spawn(ipintel_loop(app.clone(), ii));
}
```

```rust
async fn ipintel_loop(app: Arc<App>, ii: Arc<ipintel::IpIntel>) {
    let scope: HashSet<String> = ipintel::RULES.iter().map(|s| s.to_string()).collect();
    // ① 启动回填：每个节点最近 14 天的访问（学习期按事件时间算，回填后基线立即可用；历史上的“新运营商”等会记成已关闭的异常）
    if let Some(h) = app.cached_data("/health", 600.0).or(fetch_json(&app, "/health").await) {
        let nodes = ipintel::legacy::health_nodes(&h);
        let ii2 = ii.clone();
        let n2 = nodes.clone();
        let _ = tokio::task::spawn_blocking(move || ii2.upsert_nodes(n2, now_s())).await;
        for n in nodes {
            let key = format!("/activity?asset={}&limit=5000&seconds=1209600", n.node);
            if let Some(v) = fetch_json(&app, &key).await {
                let ev = ipintel::legacy::activity_events(&v);
                let ii2 = ii.clone();
                let _ = tokio::task::spawn_blocking(move || ii2.ingest_sources(ev, now_s())).await;
            }
        }
    }
    let mut tick: u64 = 0;
    loop {
        // ② 每 30 s：/health（节点目录 + 出口 IP）和 /activity?limit=500（来源）
        let h = fetch_json(&app, "/health").await;       // app.legacy.get(key, 25.0)：与共享轮询器同一份缓存
        let a = fetch_json(&app, "/activity?limit=500").await;
        let ii2 = ii.clone();
        let now = app.clock.now();
        let opened = tokio::task::spawn_blocking(move || {
            let mut opened = 0;
            if let Some(h) = &h {
                let _ = ii2.upsert_nodes(ipintel::legacy::health_nodes(h), now);
                if let Ok(r) = ii2.ingest_egress(ipintel::legacy::health_egress(h), now) { opened += r.anomalies_opened.len(); }
            }
            if let Some(a) = &a {
                if let Ok(r) = ii2.ingest_sources(ipintel::legacy::activity_events(a), now) { opened += r.anomalies_opened.len(); }
            }
            if tick % 2 == 0 { let _ = ii2.maintain(now); }
            opened
        }).await.unwrap_or(0);
        // ③ 告警：独立 scope，与旧规则互不影响
        let ii2 = ii.clone();
        if let Ok(Ok(obs)) = tokio::task::spawn_blocking(move || ii2.observations(now)).await {
            let obs: Vec<Observation> = obs.into_iter().map(|o| Observation {
                rule: o.rule, labels: o.labels,
                severity: Severity::parse(&o.severity).unwrap_or(Severity::Warning),
                summary: o.summary, evidence: o.evidence, active_since: o.active_since,
                for_s: o.for_s, notify_resolved: o.notify_resolved,
            }).collect();
            app.engine.lock().unwrap().evaluate(now, obs, &scope);
        }
        if opened > 0 {
            app.hub.publish("ipintel", Some("/v2/api/v1/ipintel/anomalies".into()), now);
        }
        // ④ 每 10 min：库文件换了就重新 mmap
        if tick % 20 == 0 {
            let ii2 = ii.clone();
            if let Ok(Some(r)) = tokio::task::spawn_blocking(move || ii2.reload_if_changed()).await {
                tracing::info!(result = ?r, "ipintel databases reloaded");
            }
        }
        tick += 1;
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

async fn fetch_json(app: &App, key: &str) -> Option<Value> {
    let (r, _) = app.legacy.get(key, 25.0).await;
    let e = r.ok()?;
    if e.status != 200 { return None; }
    serde_json::from_slice(&e.body).ok()
}
```

说明：

- `AlertObs` 与 `alerts::Observation` 字段一一对应，只有 `severity` 是字符串，用 `Severity::parse` 转。
- 标签：节点类异常 `kind=node, node=<短名>, node_name, relay, team, cause=<rule>, subject, anomaly_id`，所以 `relay_down` 抑制、静默 / 维护窗口按 `node`/`relay` 匹配都照常生效，通知分组键是 `relay=<线路机>|cause=ipintel_*`。一号多地是 `kind=identity, client=<标识>, relay`（深链落到 `#/alerts`）。
- **出口 IP 变更不会自动结束**：异常是 sticky，`observations()` 每轮都会带上它，直到有人 `POST …/anomalies/{id}/close`；关闭后下一轮评估告警进入 resolved 并发恢复通知（`notify_resolved=true`）。告警上的“确认”只停重复提醒，不关闭异常。其余四类最后一次出现 24 h 后自动结束，不发恢复通知。
- `/activity?limit=500` 每 30 s 取一次：1x（约 2.7 条/s）是全量，10x（约 30 条/s）会漏一部分——分布和“新 X”判定依然成立（只是计数偏低），接 relayd 访问日志后改成全量。重复行按 `id` 去重（进程内 5 万条）。
- `/activity?asset=…&seconds=…` 的 `limit` 上限以旧后端为准；回填只影响基线厚度，不影响正确性。

`alerts.rs` 的 `rule_title()` 加一条兜底（中文标题）：

```rust
r if r.starts_with("ipintel_") => ipintel::service::rule_title(r).unwrap_or("IP 情报"),
```

SSE 资源名 `ipintel`（有新异常时发 `change`，path 指向异常列表）；前端在 `/v2/api/stream` 的 `hello.versions` 里会看到它。

## 6. 部署（中控 38.58.57.22）

`deploy/install-central.sh` 里（fleetd 二进制安装之后、启动之前）：

```sh
install -d -o root -g root -m 0755 /opt/fleetd/current/ipintel
install -m 0644 crates/ipintel/deploy/fetch-dbs.sh /opt/fleetd/current/ipintel/fetch-dbs.sh
install -m 0644 crates/ipintel/deploy/fleetd-ipintel-fetch.service /etc/systemd/system/
install -m 0644 crates/ipintel/deploy/fleetd-ipintel-fetch.timer /etc/systemd/system/
systemctl daemon-reload
# 首次下载（约 1 分钟；下载约 190 MB，落盘约 230 MB）。失败不阻塞 fleetd：库缺失时 ipintel 照常起，只是字段为空
systemctl start fleetd-ipintel-fetch.service || journalctl -u fleetd-ipintel-fetch -n 30 --no-pager
systemctl enable --now fleetd-ipintel-fetch.timer
```

- 下载服务以 `fleetd` 用户运行，写 `/var/lib/fleetd/ipintel/`（在 fleetd 的 StateDirectory 里，0700）。需要 `curl`、`gzip`、`sha1sum`、`sha256sum`、`od`（Ubuntu 自带）。
- 每月 3 日 04:20（北京）± 30 min 更新；校验：ip2region 对 GitHub contents API 公布的 git blob sha1 与 size；DB-IP 对下载页公布的 SHA1（针对解压后的 mmdb，已实测一致）；iptoasn 没有公布哈希，校验 Content-Length、`gzip -t`、行数与首行格式。任何一个失败都保留旧文件、服务退出码非 0。
- 磁盘：`/var/lib/fleetd/ipintel/` 约 230 MB（ip2region 48 MB、DB-IP city 127 MB + asn 9.5 MB、iptoasn 46 MB）。上线前 `df -h /var/lib` 确认余量。
- 回滚：`[ipintel] enabled = false` 重启 fleetd 即可；`ipintel.db` 与库文件可以留着。

## 7. 内存（MemoryMax）

a395 实测（release，四个库 mmap）：

| 时刻 | RssAnon（堆） | RssFile（映射页） |
|---|---|---|
| 加载前 | 0.35 MB | 2.6 MB |
| 加载后（iptoasn 建索引 + 扫描后已 `MADV_DONTNEED`） | 3.6 MB | 4.3 MB |
| 20 万个随机地址查询后（最坏情况：均匀随机，几乎碰遍所有页） | 3.8 MB | 约 140 MB |

- 堆上只有 iptoasn 行偏移（约 2.9 MB）、机房 ASN 集合（约 2 200 个）和进程内缓存（≤ 2 万条）。**库本身不进堆**。
- 映射页是干净的文件页，cgroup 内存吃紧时内核直接回收、不会 OOM；真实流量只查几千个不同地址，常驻映射页在几 MB 量级。
- 在 `MemoryMax=128M`、`MemorySwapMax=0` 的 cgroup 里实测（同一 cgroup 先 `cp` 一份库，让页缓存也记在它头上）：
  - 100 万次均匀随机查询：cgroup 顶到 128 MiB（anon 8 MB、file 124 MB），`memory.events` 的 `max` 计数 38 万次（内核在回收文件页），**`oom` / `oom_kill` 都是 0**；代价是冷页要回盘，查询变慢。
  - 2 万个不同的随机地址：anon 0.4 MB、file 124 MB，`oom_kill 0`；冷页查询约 350 µs/次（热页约 4 µs/次）。真实来源每分钟新增几十个，可以忽略。
- 结论：**现有 `MemoryMax=128M` 够用、不会 OOM**。建议另加 `MemoryHigh=112M`，让回收提前、平缓地发生；若以后 fleetd 自身堆涨到 60 MB 以上，再把 `MemoryMax` 提到 160M。

## 8. 验证（a395）

```sh
source ~/team-ops-eval/tools/env.sh
cd ~/team-ops-eval/ipintel-build
sh deploy/fetch-dbs.sh ~/team-ops-eval/ipintel-db
IPINTEL_DB_DIR=~/team-ops-eval/ipintel-db IPINTEL_FIXTURES=~/team-ops-eval/ipintel-fixtures cargo test --release -- --nocapture
cargo run --release --example ipintel -- lookup ~/team-ops-eval/ipintel-db 8.8.8.8 202.102.224.68
cargo run --release --example ipintel -- bench  ~/team-ops-eval/ipintel-db 200000
```

接进 fleetd 后，在 staging 上：`curl -s --cookie "session_token=…" http://127.0.0.1:25900/v2/api/v1/ipintel/status | jq .data.loaded` 应为 5。

## 9. 口径与限制（写给看告警的人）

- 只查离线库，IP 不发往任何在线服务，不查 PTR。
- 地点：国内以 ip2region 为准（省、市、运营商），境外以 DB-IP 为准（州、城市、坐标）；两库分歧记 `geo_conflict`，不混用。ASN 以 DB-IP 为准、iptoasn 补空，分歧记 `asn_conflict`。
- 接入方式是启发式（`src/classify.rs` 顶部有完整顺序）：精选 ASN 名单（公有云、IDC、VPN 出口网络、移动专网、固网运营商）→ AS 名称关键词 → iptoasn 全表关键词扫描得到的机房名单 → 中国三大运营商骨干 / 省网 ASN 记住宅（置信度低，`type_alt=mobile`，因为家宽和手机共用这些 ASN）。识别不了“住宅代理”，也没有 Tor 出口清单。
- 学习期：每个节点第一条访问事件后 7 天内（事件时间）只学习；一号多地另外把学习期内同一标识已经用过的城市组合当作既有用法。
