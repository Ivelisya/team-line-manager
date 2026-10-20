# fleetd-ingest 接线说明（给 fleetd 维护者）

本模块自带数据库文件（缺省 `/var/lib/fleetd/ingest.db`，WAL + synchronous=FULL），不碰 `fleetd.db`。

## 1. Cargo

`fleet/Cargo.toml` 的 workspace 加成员（或 path 依赖）：

```toml
# fleet/fleetd/Cargo.toml
[dependencies]
fleetd-ingest = { path = "../../fleet-edge/fleetd-ingest" }   # 或把目录挪进 fleet/ 后改路径
```

依赖版本与 fleetd 一致（axum 0.8、rusqlite 0.40 bundled、tokio 1、sha2 0.10），不会引入第二份 SQLite。
`ingest-testd` 这个 bin 只给 a395 测试用，生产不需要。

## 2. 启动时打开

```rust
let ingest = fleetd_ingest::Ingest::open(fleetd_ingest::IngestCfg {
    db_path: cfg.data_dir.join("ingest.db"),
    ..Default::default()            // 256 KB 请求体、每主机 2 次/s 突发 60、汇总保留 90 d、Xray 分钟 35 d
})?;
ingest.spawn_maintenance();         // 在 tokio 运行时里调用；每小时清理过期行
```

## 3. 路由（`fleetd/src/api.rs`）

- 只读查询**放进** guard 之内（需要管理员会话）：

```rust
pub fn router(app: AppState) -> Router {
    Router::new()
        .route(...现有路由...)
        .merge(fleetd_ingest::api::read_routes(app.ingest.clone()))   // 新增
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}
```

  新增的只读接口：
  - `GET /v2/api/v1/hosts` 每台主机最新样本、`age_s`、`online`、agent 版本、outbox 丢弃数
  - `GET /v2/api/v1/hosts/{id}/series?metric=cpu,mem,disk,steal,iowait,load1,swap,rx,tx,tcp,udp,procs&range=24h[&step=300]` → `[[t, avg, max], …]`
  - `GET /v2/api/v1/relays/{id}/xray?range=1h[&kind=inbound&tag=inbound-20001]` 每个入站/用户的**累计值**（真实来源）、原始值、速率（B/s）、range 内增量、入站端口已建立连接数、xray/x-ui 进程与版本；给了 kind+tag 时附分钟序列
  - `GET /v2/api/v1/relays/{id}/probes?range=15m` relayd 的探测：在测节点（不含密码）、每个 (节点, 检查) 的最新结果（L2 取最新的 final 尝试）、range 内按状态计数
  - `GET /v2/api/v1/ingest/alerts` 当前原生告警观测（调试用）

  探测结果（批次顶层 `probe`，`fleet.probe.v1`）在 `accept()` 里按自然键 `(node, check, measured_at, attempt)` 写进 `probe_land`（与批次去重同一事务，保留 14 天），
  在测节点写进 `probe_targets`。fleet-health 从 `probe_land` 读（表结构与 fleet/crates/health/WIRING.md §2 建议的一致）。

- 上报接口**放在** guard 之外（guard 会把非 `/v2/api/` 路径 404 掉，而且 agent 没有管理员会话）：

```rust
pub fn into_service(app: AppState) -> IntoMakeServiceWithConnectInfo<Router, SocketAddr> {
    fleetd_ingest::api::ingest_routes(app.ingest.clone())   // POST /v2/ingest/{node,relay}
        .merge(router(app))
        .into_make_service_with_connect_info::<SocketAddr>()
}
```

  两个函数都返回泛型 `Router<S>`，直接 merge。`ingest_routes` 自带 `DefaultBodyLimit::max(256 KiB)`。

## 4. 告警

在规则评估循环里（`spawn_blocking`）调用 `ingest.evaluate(now)`，逐条转成 `alerts::Observation`：

```rust
for a in ingest.evaluate(now) {
    let mut l = Labels::new();
    l.insert("host".into(), a.host_id.clone());
    l.insert("kind".into(), a.kind.clone());
    l.insert("source".into(), "agent".into());
    obs.push(Observation {
        rule: a.rule.into(),                    // native_host_offline / native_host_resource / native_cpu_steal / native_cpu_iowait
        labels: l,
        severity: Severity::parse(a.severity).unwrap(),
        summary: a.summary,
        evidence: a.evidence,
        active_since: Some(a.active_since),     // 引擎按 for 计算，跨 fleetd 重启不丢
        for_s: a.for_s,                         // 120 / 120 / 600 / 600
        notify_resolved: true,
    });
}
```

规则：`native_host_offline`（120 s 没有批次；线路机 critical、节点 warning）；`native_host_resource`（内存或磁盘 ≥ 90% 连续 ≥ 120 s）；
`native_cpu_steal` / `native_cpu_iowait`（分钟均值 ≥ 15% 连续 ≥ 10 min）。只评估有未撤销令牌的主机（撤销令牌 = 下线）。
建议：fleetd 启动后 3 分钟内不开 `native_host_offline`（agent 的退避最长 90 s，中控重启后需要一两个周期才补齐）。

## 5. 命令行

```rust
// fleetd/src/main.rs
"agent-token" => std::process::exit(fleetd_ingest::tokens::cli(
    fleetd_ingest::IngestCfg { db_path: load(&a.config).data_dir.join("ingest.db"), ..Default::default() },
    &std::env::args().skip(2).filter(|x| x != "--config").collect::<Vec<_>>(),   // 视参数解析调整
)),
```

- `fleetd agent-token issue <host_id> [--name N]`：stdout 只打印令牌一次；数据库只存 sha256
- `fleetd agent-token revoke <host_id> [--name N]`；`fleetd agent-token list`
- 必须以 fleetd 用户执行：`runuser -u fleetd -- /opt/fleetd/current/fleetd agent-token issue 团队1-24`（root 执行且库属于其他用户时会拒绝，避免产生 root 所有的 WAL 文件）。rollout.sh 缺省就这样调用（可用 `TOKEN_CMD` 覆盖）。

## 6. Caddy

在现有 `# fleetd /v2/api (phase 1a)` 块旁边加（同一个 route{} 里、兜底 handle 之前）：

```caddy
        # fleetd /v2/ingest (edge agents)
        handle /v2/ingest/* {
            request_body {
                max_size 256KB
            }
            reverse_proxy 127.0.0.1:25900
        }
        # end fleetd /v2/ingest
```

## 7. 体量估算（1x：29 节点 + 3 线路机）

- 写入：32 台 × 每 30 s 一批 ≈ 1.1 事务/s（每批 1–2 行 host_1m + 3 行 host_recent，线路机加非零的 xray_1m）。
- `host_1m`：每台每分钟 1 行，约 90 B → 32 × 1440 × 90 d ≈ 4.1 M 行 ≈ 370 MB（90 天稳态）。中控磁盘紧张时把 `rollup_days` 调到 30（≈ 125 MB）。
- `xray_1m`：只存非零分钟；3 台 × 约 30 条目，活跃时 ≈ 130 k 行/天上限，35 天 ≲ 250 MB（实际远低于，空闲条目不写）。
- `ingest_seen`：7 天 ≈ 650 k 行 ≈ 40 MB。
