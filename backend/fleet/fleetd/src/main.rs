//! fleetd 命令行：serve | check-config | notify-test --channel X | import-catalog [--file F] | version
use fleetd::alerts::{Outgoing, Severity};
use fleetd::app::{spawn_background, App, VERSION};
use fleetd::clock::{Clock, SystemClock};
use fleetd::config::{read_secret, AuthMode, Config};
use fleetd::notify::Notifier;
use std::path::PathBuf;
use std::sync::Arc;

fn usage() -> ! {
    eprintln!(
        "fleetd {VERSION}\n用法：\n  fleetd serve [--config /etc/fleetd/fleetd.toml]\n  fleetd check-config [--config …]\n  \
         fleetd notify-test --channel <名称> [--config …]\n  fleetd import-catalog [--file catalog.json] [--config …]\n  \
         fleetd diag komari|legacy [--session-file F] [--config …]\n  \
         fleetd agent-token issue|revoke <host_id> [--name N] | list [--config …]\n  fleetd version"
    );
    std::process::exit(2)
}

struct Args {
    cmd: String,
    config: PathBuf,
    channel: Option<String>,
    file: Option<PathBuf>,
    target: Option<String>,
    session_file: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().unwrap_or_else(|| usage());
    let mut a = Args {
        cmd,
        config: std::env::var("FLEETD_CONFIG").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/etc/fleetd/fleetd.toml")),
        channel: None,
        file: None,
        target: None,
        session_file: None,
    };
    while let Some(x) = it.next() {
        match x.as_str() {
            "--config" | "-c" => a.config = PathBuf::from(it.next().unwrap_or_else(|| usage())),
            "--channel" => a.channel = Some(it.next().unwrap_or_else(|| usage())),
            "--file" => a.file = Some(PathBuf::from(it.next().unwrap_or_else(|| usage()))),
            "--session-file" => a.session_file = Some(PathBuf::from(it.next().unwrap_or_else(|| usage()))),
            x if !x.starts_with('-') && a.target.is_none() => a.target = Some(x.to_string()),
            _ => usage(),
        }
    }
    a
}

fn init_log() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("FLEETD_LOG").unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_target(false).with_writer(std::io::stderr).init();
}

fn load(path: &PathBuf) -> Config {
    match Config::load(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("配置无效：{e}");
            std::process::exit(1)
        }
    }
}

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    // agent-token 的参数交给 fleetd-ingest 自己解析（issue/revoke/list、--name）
    let raw: Vec<String> = std::env::args().collect();
    if raw.get(1).map(String::as_str) == Some("agent-token") {
        let mut cfgp = std::env::var("FLEETD_CONFIG").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/etc/fleetd/fleetd.toml"));
        let mut rest = vec![];
        let mut it = raw.into_iter().skip(2);
        while let Some(x) = it.next() {
            if x == "--config" || x == "-c" {
                cfgp = PathBuf::from(it.next().unwrap_or_else(|| usage()));
            } else {
                rest.push(x);
            }
        }
        let c = load(&cfgp);
        std::process::exit(fleetd_ingest::tokens::cli(
            fleetd_ingest::IngestCfg { db_path: c.data_dir.join("ingest.db"), ..Default::default() },
            &rest,
        ));
    }
    if raw.get(1).map(String::as_str) == Some("vendor") {
        std::process::exit(vendor_cli(&raw[2..]));
    }
    if raw.get(1).map(String::as_str) == Some("legacy") {
        std::process::exit(legacy_cli(&raw[2..]));
    }
    if raw.get(1).map(String::as_str) == Some("user") {
        std::process::exit(user_cli(&raw[2..]));
    }
    let a = parse_args();
    match a.cmd.as_str() {
        "version" | "--version" => println!("fleetd {VERSION}"),
        "check-config" => check_config(&a),
        "notify-test" => notify_test(&a),
        "import-catalog" => import_catalog(&a),
        "serve" => serve(&a),
        "diag" => diag(&a),
        _ => usage(),
    }
}

fn check_config(a: &Args) {
    let c = load(&a.config);
    let mut problems = vec![];
    if let Some(p) = &c.legacy.key_file {
        if let Err(e) = read_secret(p) {
            problems.push(format!("legacy.key_file：{e}"));
        }
    }
    if let Some(p) = &c.komari.api_key_file {
        if let Err(e) = read_secret(p) {
            problems.push(format!("komari.api_key_file：{e}"));
        }
    }
    if let Some(p) = &c.catalog {
        if !p.exists() {
            problems.push(format!("catalog 不存在：{}", p.display()));
        }
    }
    let shadow = match Notifier::from_config(&c.notify) {
        Ok(n) => n.is_shadow(),
        Err(e) => {
            problems.push(format!("通知渠道：{e}"));
            true
        }
    };
    println!("listen          {}", c.listen);
    println!("data_dir        {}", c.data_dir.display());
    if c.legacy_source == "native" {
        println!("legacy          原生旧库 {}（旧后端已下线，不连接）", c.legacy_db_path().display());
    } else {
        println!("legacy          {}（key：{}）", c.legacy.base_url, if c.legacy.key_file.is_some() { "有" } else { "无" });
    }
    if c.komari.enabled {
        println!("komari          {}（API 密钥：{}）", c.komari.base_url, if c.komari.api_key_file.is_some() { "有" } else { "无，按需转发会话" });
    } else {
        println!("komari          关（komari/* 原生；不连 Komari）");
    }
    println!("auth            {}", match (c.auth.mode, c.komari.enabled && c.auth.komari_fallback) {
        (AuthMode::Komari, true) => "fleetd 账户，Komari 管理员会话回退",
        (AuthMode::Komari, false) | (AuthMode::Fleetd, _) => "仅 fleetd 账户",
        (AuthMode::DevAllowLocal, _) => "dev-allow-local（仅测试服）",
    });
    println!("legacy_source   {}", c.legacy_source);
    println!("edge probes     native_probe={} native_paths={}", c.edge.native_probe, c.edge.native_paths);
    println!("notify          {} 个渠道，{}", c.notify.channels.len(), if shadow { "影子模式" } else { "正常投递" });
    match &c.onboard_config {
        Some(p) => match onboard::config::Config::load(p) {
            Ok(_) => println!("onboard         开（{}）", p.display()),
            Err(e) => problems.push(format!("onboard_config：{e}")),
        },
        None => println!("onboard         关（未配置 onboard_config）"),
    }
    println!("edge            原生判定 {}（覆盖完整 {} s 后切换；线路机 host_id {:?}）", c.edge.native_health, c.edge.native_after_s,
        c.edge.relay_hosts);
    if c.ipintel.enabled {
        let have = c.ipintel.db_dir.join("MANIFEST").exists();
        println!("ipintel         开（库目录 {}{}；内部地址 {} 个；回填 {} 天）", c.ipintel.db_dir.display(),
            if have { "" } else { "，库文件缺失：情报字段为空，先跑 fetch-dbs.sh" }, c.ipintel.internal_ips.len(), c.ipintel_backfill_days);
    } else {
        println!("ipintel         关");
    }
    if problems.is_empty() {
        println!("OK");
    } else {
        for p in &problems {
            eprintln!("问题：{p}");
        }
        std::process::exit(1);
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).max_blocking_threads(4).enable_all().build().expect("runtime")
}

fn notify_test(a: &Args) {
    let c = load(&a.config);
    let Some(name) = a.channel.clone() else { usage() };
    let mut cfg = c.notify.clone();
    cfg.shadow = false;
    let n = match Notifier::from_config(&cfg) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("渠道配置无效：{e}");
            std::process::exit(1)
        }
    };
    let Some(ch) = n.channels.iter().find(|x| x.cfg.name == name) else {
        eprintln!("没有名为 {name} 的渠道（已配置：{}）", n.channels.iter().map(|x| x.cfg.name.as_str()).collect::<Vec<_>>().join("、"));
        std::process::exit(1)
    };
    let now = SystemClock.now();
    let msg = Outgoing {
        kind: "test".into(),
        severity: Severity::Info,
        group_key: None,
        title: "[测试] fleetd 通知渠道测试".into(),
        body: format!("[测试] fleetd 通知渠道测试\n渠道：{name}（{}）\n时间：{}\n这是一条测试消息，无需处理。",
            ch.cfg.kind, fleetd::alerts::fmt_time(now, chrono_tz::Asia::Shanghai)),
        link: format!("{}alerts", c.ui_base_url),
        alert_ids: vec![],
        alerts: vec![],
        ts: now,
    };
    let r = rt().block_on(n.send(ch, &msg));
    match r {
        Ok(()) => println!("已发送到 {name}"),
        Err(e) => {
            eprintln!("发送失败：{e}");
            std::process::exit(1)
        }
    }
}

fn import_catalog(a: &Args) {
    let c = load(&a.config);
    let file = a.file.clone().or(c.catalog.clone()).unwrap_or_else(|| usage());
    let store = match fleetd::store::Store::open(&c.db_path()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1)
        }
    };
    match fleetd::catalog::import(&store, &file, "cli", SystemClock.now()) {
        Ok(r) => println!("导入 {} 个套餐、{} 项资产（{}）", r.plans, r.assets, if r.changed { "已更新" } else { "无变化" }),
        Err(e) => {
            eprintln!("导入失败：{e}");
            std::process::exit(1)
        }
    }
}

fn serve(a: &Args) {
    init_log();
    let c = load(&a.config);
    let addr = c.listen_addr().unwrap();
    if c.auth.mode == AuthMode::DevAllowLocal {
        tracing::warn!("auth.mode = dev-allow-local：只接受本机直连请求，仅限测试服使用");
    }
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    rt().block_on(async move {
        let app = match App::build(c, clock) {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(error = %e, "startup failed");
                std::process::exit(1)
            }
        };
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(error = %e, %addr, "bind failed");
                std::process::exit(1)
            }
        };
        tracing::info!(%addr, version = VERSION, shadow = app.notifier.is_shadow(), legacy_source = %app.cfg.legacy_source, "fleetd listening");
        spawn_background(&app);
        let svc = fleetd::api::into_service(app.clone());
        let app_for_shutdown = app.clone();
        let shutdown = async move {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            tracing::info!("shutting down");
            app_for_shutdown.ingest.flush();
            // SSE 长连接不会自己结束：给在途请求 3 s，然后退出
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                std::process::exit(0);
            });
        };
        if let Err(e) = axum::serve(listener, svc).with_graceful_shutdown(shutdown).await {
            tracing::error!(error = %e, "server error");
        }
    });
}

/// 生产排障：只读地探一遍 Komari / 旧后端，不需要浏览器会话，不打印任何机密。
///   fleetd diag komari [--session-file F]   F 里放一个管理员 session_token（可选，用来走一遍 fleetd 转发会话的真实路径）
///   fleetd diag legacy
fn diag(a: &Args) {
    let c = load(&a.config);
    let target = a.target.clone().unwrap_or_else(|| "komari".into());
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).no_proxy().build().unwrap();
    rt().block_on(async move {
        let show = |label: &str, r: Result<(u16, String), String>| match r {
            Ok((st, body)) => println!("{label:<44} HTTP {st}  {}", body.chars().take(220).collect::<String>().replace('\n', " ")),
            Err(e) => println!("{label:<44} 失败：{e}"),
        };
        async fn call(rb: reqwest::RequestBuilder) -> Result<(u16, String), String> {
            let t0 = std::time::Instant::now();
            let r = rb.send().await.map_err(|e| e.to_string())?;
            let st = r.status().as_u16();
            let b = r.text().await.map_err(|e| e.to_string())?;
            Ok((st, format!("{:.0} ms  {b}", t0.elapsed().as_secs_f64() * 1000.0)))
        }
        match target.as_str() {
            "komari" => {
                let base = c.komari.base_url.trim_end_matches('/').to_string();
                println!("Komari {base}（cookie 名 {}）", c.auth.cookie_name);
                show("GET /api/version", call(client.get(format!("{base}/api/version"))).await);
                show("GET /api/me（无会话）", call(client.get(format!("{base}/api/me"))).await);
                let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "common:getVersion", "params": {}});
                show("POST /api/rpc2 common:getVersion（无 Origin）", call(client.post(format!("{base}/api/rpc2")).json(&body)).await);
                show("POST /api/rpc2 common:getVersion（Origin 本站）",
                    call(client.post(format!("{base}/api/rpc2")).header("Origin", "https://38.58.57.22").json(&body)).await);
                show("POST /api/rpc2 common:getVersion（Origin 外站）",
                    call(client.post(format!("{base}/api/rpc2")).header("Origin", "https://example.com").json(&body)).await);
                let list = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "admin:listClients", "params": {}});
                let summarize = |r: Result<(u16, String), String>| -> Result<(u16, String), String> {
                    r.map(|(st, b)| {
                        let json_part = b.splitn(2, "  ").nth(1).unwrap_or("").to_string();
                        let v: serde_json::Value = serde_json::from_str(&json_part).unwrap_or(serde_json::Value::Null);
                        let s = match v.get("result").and_then(|x| x.as_array()) {
                            Some(rows) => format!("result：{} 个客户端（token 字段已省略）", rows.len()),
                            None => format!("{}", v.get("error").cloned().unwrap_or(v)),
                        };
                        (st, s)
                    })
                };
                show("POST /api/rpc2 admin:listClients（无凭据）", summarize(call(client.post(format!("{base}/api/rpc2")).json(&list)).await));
                if let Some(p) = &c.komari.api_key_file {
                    match read_secret(p) {
                        Ok(k) => show("POST /api/rpc2 admin:listClients（API 密钥）",
                            summarize(call(client.post(format!("{base}/api/rpc2")).header("Authorization", format!("Bearer {k}")).json(&list)).await)),
                        Err(e) => println!("API 密钥文件：{e}"),
                    }
                } else {
                    println!("{:<44} 未配置（komari.api_key_file）", "Komari 系统 API 密钥");
                }
                if let Some(p) = &a.session_file {
                    match read_secret(p) {
                        Ok(tok) => {
                            let ck = format!("{}={}", c.auth.cookie_name, tok);
                            show("GET /api/me（会话）", call(client.get(format!("{base}/api/me")).header("Cookie", ck.clone())).await
                                .map(|(st, b)| (st, b.replace(&tok, "<token>"))));
                            show("POST /api/rpc2 admin:listClients（会话，同 fleetd）",
                                summarize(call(client.post(format!("{base}/api/rpc2")).header("Cookie", ck).json(&list)).await));
                        }
                        Err(e) => println!("会话文件：{e}"),
                    }
                } else {
                    println!("（加 --session-file <文件> 可用一个管理员 session_token 走一遍 fleetd 转发会话的真实路径）");
                }
            }
            "onboard" => {
                // 与 /v2/api/v1/onboard/* 同一个处理函数（fleetd 进程里挂的就是它），不经 HTTP、不需要会话
                let Some(p) = &c.onboard_config else {
                    println!("未配置 onboard_config");
                    return;
                };
                match onboard::config::Config::load(p).and_then(|cfg| onboard::api::OnboardApi::new(std::sync::Arc::new(cfg))) {
                    Ok(ob) => {
                        let (code, v) = ob.handle("GET", "/v2/api/v1/onboard/batches", "", b"", "diag").await;
                        println!("GET /v2/api/v1/onboard/batches → {code} {}", serde_json::to_string(&v).unwrap_or_default().chars().take(400).collect::<String>());
                        let (code, v) = ob.handle("GET", "/v2/api/v1/onboard/assets", "", b"", "diag").await;
                        println!("GET /v2/api/v1/onboard/assets  → {code} {}", serde_json::to_string(&v).unwrap_or_default().chars().take(200).collect::<String>());
                    }
                    Err(e) => println!("onboard 初始化失败：{e}"),
                }
            }
            "legacy" => {
                let base = c.legacy.base_url.trim_end_matches('/').to_string();
                let key = c.legacy.key_file.as_ref().and_then(|p| read_secret(p).ok());
                println!("旧后端 {base}（X-Ops-Key：{}）", if key.is_some() { "有" } else { "无" });
                for path in ["/health", "/overview"] {
                    let mut rb = client.get(format!("{base}{path}")).header("X-Ops-Actor", &c.legacy.actor);
                    if let Some(k) = &key {
                        rb = rb.header("X-Ops-Key", k);
                    }
                    show(&format!("GET {path}"), call(rb).await.map(|(st, b)| (st, format!("{} 字节", b.len()))));
                }
            }
            _ => usage(),
        }
    });
}

fn my_uid() -> Option<u32> {
    std::fs::read_to_string("/proc/self/status").ok()?.lines().find(|l| l.starts_with("Uid:"))?.split_whitespace().nth(1)?.parse().ok()
}

/// fleetd user export-komari [--db /var/lib/komari/data/komari.db]    （root：只读导出 Komari 账户，JSON 行）
/// fleetd user import [-|文件] [--config …]                            （fleetd 用户：导入上面的输出）
/// fleetd user set-password <用户名> [--config …]                      （fleetd 用户：从标准输入读一行密码；用户不存在就创建）
/// fleetd user list | disable <用户名> [--config …]
/// fleetd user session <用户名> --out FILE [--ttl-s 900]              （fleetd 用户：在中控本机签一个短期会话给运维脚本用，令牌只写进 0600 文件）
fn user_cli(args: &[String]) -> i32 {
    let mut cfgp = std::env::var("FLEETD_CONFIG").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/etc/fleetd/fleetd.toml"));
    let mut db = PathBuf::from("/var/lib/komari/data/komari.db");
    let mut pos = vec![];
    let mut out: Option<PathBuf> = None;
    let mut ttl = 900.0f64;
    let mut it = args.iter();
    while let Some(x) = it.next() {
        match x.as_str() {
            "--config" | "-c" => cfgp = PathBuf::from(it.next().cloned().unwrap_or_default()),
            "--db" => db = PathBuf::from(it.next().cloned().unwrap_or_default()),
            "--out" => out = it.next().map(PathBuf::from),
            "--ttl-s" => ttl = it.next().and_then(|v| v.parse().ok()).unwrap_or(900.0f64).clamp(60.0, 3600.0),
            _ => pos.push(x.clone()),
        }
    }
    let now = SystemClock.now();
    let sub = pos.first().cloned().unwrap_or_default();
    if sub == "export-komari" {
        let uri = format!("file:{}?mode=ro", db.display());
        let c = match rusqlite::Connection::open_with_flags(&uri, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("打开 {} 失败：{e}", db.display());
                return 1;
            }
        };
        let mut st = c.prepare("SELECT username, passwd, coalesce(two_factor,'') FROM users").unwrap();
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))).unwrap();
        for (u, p, t) in rows.flatten() {
            println!("{}", serde_json::json!({"username": u, "pw_scheme": "komari-sha256", "pw_hash": p, "totp_secret": if t.is_empty() { None } else { Some(t) }}));
        }
        return 0;
    }
    if my_uid() == Some(0) && sub != "list" {
        eprintln!("请以 fleetd 用户执行（避免产生 root 所有的数据库 WAL 文件）：runuser -u fleetd -- fleetd user …");
        return 2;
    }
    let c = load(&cfgp);
    let store = match fleetd::store::Store::open(&c.db_path()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    match sub.as_str() {
        "import" => {
            use std::io::Read;
            let src = pos.get(1).cloned().unwrap_or_else(|| "-".into());
            let mut text = String::new();
            if src == "-" {
                std::io::stdin().read_to_string(&mut text).ok();
            } else {
                text = std::fs::read_to_string(&src).unwrap_or_default();
            }
            let mut n = 0;
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                let v: serde_json::Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("跳过无效行：{e}");
                        continue;
                    }
                };
                let (u, s, h) = (v["username"].as_str().unwrap_or("").to_string(), v["pw_scheme"].as_str().unwrap_or("").to_string(), v["pw_hash"].as_str().unwrap_or("").to_string());
                if u.is_empty() || h.is_empty() || !matches!(s.as_str(), "komari-sha256" | "pbkdf2-sha256") {
                    eprintln!("跳过：{u}");
                    continue;
                }
                let t = v["totp_secret"].as_str().map(String::from);
                let r = store.call_blocking(move |c| {
                    fleetd::accounts::upsert_user(c, &u, &s, &h, t.as_deref(), "komari", now)?;
                    fleetd::store::audit(c, now, "cli", "auth.user_imported", &format!("user:{u}"), &serde_json::json!({"source": "komari"}))
                });
                if r.is_ok() {
                    n += 1;
                }
            }
            println!("导入 {n} 个账户（Komari 的旧哈希在第一次登录成功后自动升级为 PBKDF2）");
            0
        }
        "set-password" => {
            let Some(name) = pos.get(1).cloned() else {
                eprintln!("用法：fleetd user set-password <用户名>（从标准输入读一行密码）");
                return 2;
            };
            let mut pw = String::new();
            std::io::stdin().read_line(&mut pw).ok();
            let pw = pw.trim_end_matches(['\n', '\r']).to_string();
            if pw.chars().count() < 10 {
                eprintln!("密码至少 10 个字符");
                return 2;
            }
            let h = fleetd::accounts::hash_password(&pw);
            let r = store.call_blocking(move |c| {
                fleetd::accounts::upsert_user(c, &name, "pbkdf2-sha256", &h, None, "cli", now)?;
                c.execute("UPDATE sessions SET revoked_at=? WHERE revoked_at IS NULL AND user_id=(SELECT id FROM users WHERE username=?)", rusqlite::params![now, name])?;
                fleetd::store::audit(c, now, "cli", "auth.password_set", &format!("user:{name}"), &serde_json::json!({}))
            });
            match r {
                Ok(_) => {
                    println!("已设置（该用户的已有会话全部作废）");
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }
        "session" => {
            let (Some(name), Some(out)) = (pos.get(1).cloned(), out) else {
                eprintln!("用法：fleetd user session <用户名> --out FILE [--ttl-s 900]");
                return 2;
            };
            let r = store.call_blocking(move |c| -> rusqlite::Result<Option<String>> {
                let Some(u) = fleetd::accounts::user_by_name(c, &name) else { return Ok(None) };
                let tok = fleetd::accounts::create_session(c, u.id, "127.0.0.1", "cli:user-session", now)?;
                c.execute("UPDATE sessions SET expires_at=? WHERE token_sha256=?", rusqlite::params![now + ttl, fleetd::accounts::sha256_hex(&tok)])?;
                fleetd::store::audit(c, now, "cli", "auth.session_issued", &format!("user:{name}"), &serde_json::json!({"ttl_s": ttl, "via": "cli"}))?;
                Ok(Some(tok))
            });
            match r {
                Ok(Some(tok)) => {
                    use std::io::Write;
                    use std::os::unix::fs::OpenOptionsExt;
                    let f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&out);
                    match f.and_then(|mut f| f.write_all(tok.as_bytes())) {
                        Ok(_) => {
                            println!("会话已写入 {}（{} 秒后过期；用完请 POST /v2/api/v1/auth/logout）", out.display(), ttl as i64);
                            0
                        }
                        Err(e) => {
                            eprintln!("写 {} 失败：{e}", out.display());
                            1
                        }
                    }
                }
                Ok(None) => {
                    eprintln!("没有这个用户（或已停用）");
                    1
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }
        "disable" => {
            let Some(name) = pos.get(1).cloned() else { return 2 };
            let n = store.call_blocking(move |c| {
                let n = c.execute("UPDATE users SET disabled_at=? WHERE username=?", rusqlite::params![now, name]).unwrap_or(0);
                let _ = fleetd::store::audit(c, now, "cli", "auth.user_disabled", &format!("user:{name}"), &serde_json::json!({}));
                n
            });
            println!("{}", if n > 0 { "已停用" } else { "没有这个用户" });
            if n > 0 { 0 } else { 1 }
        }
        "list" => {
            let rows = store.call_blocking(|c| {
                let mut st = c.prepare("SELECT username, pw_scheme, totp_secret IS NOT NULL, source, disabled_at IS NOT NULL FROM users ORDER BY id").unwrap();
                st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?, r.get::<_, String>(3)?, r.get::<_, bool>(4)?)))
                    .unwrap().flatten().collect::<Vec<_>>()
            });
            for (u, s, t, src, d) in rows {
                println!("{u:<20} {s:<14} totp={} 来源={src}{}", if t { "开" } else { "关" }, if d { " 已停用" } else { "" });
            }
            0
        }
        _ => {
            eprintln!("用法：fleetd user export-komari [--db F] | import [-|F] | set-password <名> | disable <名> | list");
            2
        }
    }
}

/// fleetd legacy snapshot [--db /var/lib/team-ops/operations.db] [--golden /var/lib/fleetd/legacy.db]   （root：旧库 → 原生旧库）
/// fleetd legacy render --db F --now T --path '/overview?…' [--relays a,b] [--public-key TEXT]
/// fleetd legacy verify --golden DIR [--only N]      （DIR 由 staging/golden.py 生成；逐字节比较，不一致时给出首个差异）
fn legacy_cli(args: &[String]) -> i32 {
    use fleetd::compat;
    let mut db: Option<PathBuf> = None;
    let mut now: Option<f64> = None;
    let mut path: Option<String> = None;
    let mut golden: Option<PathBuf> = None;
    let mut relays: Option<Vec<String>> = None;
    let mut pubkey: Option<String> = None;
    let mut only: Option<usize> = None;
    let mut since: Option<f64> = None;
    let mut dry = false;
    let (mut uuid_a, mut ip_a, mut actor_a, mut reason_a, mut with_target) = (None::<String>, None::<String>, None::<String>, None::<String>, false);
    let sub = args.first().cloned().unwrap_or_default();
    let mut it = args.iter().skip(1);
    while let Some(x) = it.next() {
        let mut v = || it.next().cloned().unwrap_or_default();
        match x.as_str() {
            "--db" => db = Some(PathBuf::from(v())),
            "--now" => now = v().parse().ok(),
            "--path" => path = Some(v()),
            "--golden" => golden = Some(PathBuf::from(v())),
            "--relays" => relays = Some(v().split(',').filter(|s| !s.is_empty()).map(String::from).collect()),
            "--public-key" => pubkey = Some(v()),
            "--only" => only = v().parse().ok(),
            "--since" => since = v().parse().ok(),
            "--dry-run" => dry = true,
            "--uuid" => uuid_a = Some(v()),
            "--ip" => ip_a = Some(v()),
            "--actor" => actor_a = Some(v()),
            "--reason" => reason_a = Some(v()),
            "--with-target" => with_target = true,
            _ => {
                eprintln!("未知参数 {x}");
                return 2;
            }
        }
    }
    let open = |p: &PathBuf| rusqlite::Connection::open_with_flags(p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX);
    let mk_cfg = |c: &rusqlite::Connection| {
        let mut cfg = compat::CompatCfg::stage_defaults(c);
        if let Some(r) = &relays {
            cfg.config_relays = r.clone();
        }
        if let Some(k) = &pubkey {
            cfg.public_key = k.clone();
        }
        cfg
    };
    match sub.as_str() {
        "snapshot" => {
            // root：旧库一致性拷贝到 fleetd 的数据目录（旧系统照常运行，只读）
            let from = db.clone().unwrap_or_else(|| PathBuf::from("/var/lib/team-ops/operations.db"));
            let to = golden.clone().unwrap_or_else(|| PathBuf::from(fleetd::legacystore::DEFAULT_PATH));
            let backend = PathBuf::from("/etc/team-ops/backend.json");
            // root 运行时把文件交给 fleetd 用户；测试服（非 root）保持当前用户
            let owner = if my_uid() == Some(0) { Some("fleetd") } else { None };
            let r = fleetd::legacystore::snapshot(&from, &to, backend.exists().then_some(backend.as_path()), owner, SystemClock.now());
            match r {
                Ok(v) => {
                    println!("已导入 {} → {}\n{}", from.display(), to.display(), serde_json::to_string_pretty(&v["snapshot"]).unwrap_or_default());
                    println!("线路机名单（backend.json）：{}", v["compat"]["config_relays"]);
                    0
                }
                Err(e) => {
                    eprintln!("导入失败：{e}");
                    1
                }
            }
        }
        "komari-clients" => {
            // root：Komari 退役前把客户端静态资料（CPU 型号、架构、虚拟化、地区、备注、流量上限…）存进原生旧库，komari/clients 用
            let from = db.clone().unwrap_or_else(|| PathBuf::from("/var/lib/komari/data/komari.db"));
            let to = golden.clone().unwrap_or_else(|| PathBuf::from(fleetd::legacystore::DEFAULT_PATH));
            let r = (|| -> Result<usize, String> {
                let k = rusqlite::Connection::open_with_flags(format!("file:{}?mode=ro", from.display()), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI).map_err(|e| e.to_string())?;
                let mut st = k.prepare("SELECT * FROM clients").map_err(|e| e.to_string())?;
                let names: Vec<String> = st.column_names().iter().map(|s| s.to_string()).collect();
                let mut rows = st.query([]).map_err(|e| e.to_string())?;
                let mut out = vec![];
                while let Some(r) = rows.next().map_err(|e| e.to_string())? {
                    let mut m = serde_json::Map::new();
                    for (i, n) in names.iter().enumerate() {
                        if n == "token" {
                            continue;
                        }
                        m.insert(n.clone(), fleetd::compat::py::sql_to_json(r.get_ref(i).map_err(|e| e.to_string())?));
                    }
                    out.push(serde_json::Value::Object(m));
                }
                let c = rusqlite::Connection::open(&to).map_err(|e| e.to_string())?;
                c.busy_timeout(std::time::Duration::from_secs(30)).ok();
                c.execute("INSERT INTO meta VALUES('fleetd:komari_clients',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                    [fleetd::compat::pyjson::dumps(&serde_json::Value::Array(out.clone()))]).map_err(|e| e.to_string())?;
                Ok(out.len())
            })();
            if my_uid() == Some(0) {
                let _ = std::process::Command::new("chown").arg("fleetd:fleetd").arg(&to).arg(format!("{}-wal", to.display())).arg(format!("{}-shm", to.display())).status();
            }
            match r {
                Ok(n) => {
                    println!("已导入 {n} 个 Komari 客户端的静态资料（不含 token）→ {}", to.display());
                    0
                }
                Err(e) => {
                    eprintln!("导入失败：{e}");
                    1
                }
            }
        }
        "check-rollup" => {
            // 小时汇总（compat::rollup）vs 原 SQL：7 / 14 天的 /behavior 与 /activity（在库的临时副本上回填）
            let (Some(dbp), Some(now)) = (db.clone(), now) else {
                eprintln!("用法：fleetd legacy check-rollup --db F --now T");
                return 2;
            };
            let tmp = dbp.with_extension("rollup-check.db");
            let _ = std::fs::remove_file(&tmp);
            if let Err(e) = rusqlite::Connection::open(&dbp).and_then(|c| c.execute("VACUUM INTO ?1", [tmp.display().to_string()])) {
                eprintln!("复制失败：{e}");
                return 1;
            }
            let c = rusqlite::Connection::open(&tmp).unwrap();
            let _ = c.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;");
            let t0 = std::time::Instant::now();
            let mut rounds = 0;
            while !fleetd::compat::rollup::ready(&c) || rounds == 0 {
                // 与喂数一样：一块一个事务
                let tx = c.unchecked_transaction().unwrap();
                if let Err(e) = fleetd::compat::rollup::feed(&c, now) {
                    eprintln!("回填失败：{e:?}");
                    return 1;
                }
                tx.commit().unwrap();
                rounds += 1;
            }
            println!("回填 {rounds} 块 {:.0} ms", t0.elapsed().as_secs_f64() * 1000.0);
            let cfg = fleetd::compat::CompatCfg::stage_defaults(&c);
            let sql = fleetd::compat::Ctx { db: &c, now, cfg: &cfg };
            let mut cfg2 = cfg.clone();
            cfg2.memo = Some(std::sync::Arc::new(fleetd::compat::Memo::default()));
            let win = fleetd::compat::Ctx { db: &c, now, cfg: &cfg2 };
            let first = fleetd::compat::behavior::summary_live(&sql, &compat::split_query("/b?seconds=604800").1).ok();
            let asset = first.as_ref().and_then(|v| v["nodes"][0]["asset"].as_str().map(String::from)).unwrap_or_default();
            let host = first.as_ref().and_then(|v| v["targets"][0]["host"].as_str().map(String::from)).unwrap_or_default();
            let mut bad = 0;
            for q in [
                "seconds=604800".to_string(),
                "seconds=1209600".into(),
                "seconds=172800".into(),
                format!("seconds=604800&asset={asset}"),
                "seconds=604800&team=%E5%9B%A2%E9%98%9F1".into(),
                format!("seconds=604800&host={host}"),
                "seconds=604800&kind=ip".into(),
                "seconds=604800&offset=100".into(),
            ] {
                let pq = compat::split_query(&format!("/x?{q}")).1;
                let ta = std::time::Instant::now();
                let a = fleetd::compat::behavior::summary_live(&sql, &pq);
                let aa = fleetd::compat::store::activity(&sql, &pq);
                let dt_sql = ta.elapsed();
                let tb = std::time::Instant::now();
                let b = fleetd::compat::behavior::summary(&win, &pq);
                let bb = fleetd::compat::store::activity(&win, &pq);
                let dt_r = tb.elapsed();
                let (Ok(a), Ok(aa), Ok(b), Ok(bb)) = (a, aa, b, bb) else {
                    println!("{q}: 查询失败");
                    bad += 1;
                    continue;
                };
                let mut res = vec![];
                for (name, x, y) in [("nodes", &a["nodes"], &b["nodes"]), ("targets", &a["targets"], &b["targets"]), ("sources", &a["sources"], &b["sources"]),
                    ("next_offset", &a["next_offset"], &b["next_offset"]), ("act.domains", &aa["domains"], &bb["domains"]),
                    ("act.sources", &aa["sources"], &bb["sources"]), ("act.total", &aa["total"], &bb["total"])] {
                    let (sx, sy) = (compat::pyjson::dumps(x), compat::pyjson::dumps(y));
                    if sx == sy {
                        continue;
                    }
                    // 同值先后不同：按集合比
                    let set = |v: &serde_json::Value| -> std::collections::BTreeSet<String> { v.as_array().map(|a| a.iter().map(compat::pyjson::dumps).collect()).unwrap_or_default() };
                    if x.is_array() && set(x) == set(y) {
                        res.push(format!("{name}≈"));
                    } else {
                        bad += 1;
                        let p = sx.bytes().zip(sy.bytes()).position(|(p, q)| p != q).unwrap_or(sx.len().min(sy.len()));
                        res.push(format!("{name}✗ SQL…{}… 汇总…{}…", &sx[p.saturating_sub(60)..(p + 80).min(sx.len())], &sy[p.saturating_sub(60)..(p + 80).min(sy.len())]));
                    }
                }
                println!("{q:50} SQL {:>6.0} ms / 汇总 {:>5.1} ms  {}", dt_sql.as_secs_f64() * 1000.0, dt_r.as_secs_f64() * 1000.0, if res.is_empty() { "一致".into() } else { res.join(" ") });
            }
            let _ = std::fs::remove_file(&tmp);
            if bad == 0 { 0 } else { 1 }
        }
        "check-window" => {
            // 增量 24 h 窗口 vs 原 SQL：默认 summary 的 nodes/targets/sources 与 activity 聚合必须逐字节一致
            let (Some(dbp), Some(now)) = (db.clone(), now) else {
                eprintln!("用法：fleetd legacy check-window --db F --now T");
                return 2;
            };
            let c = match rusqlite::Connection::open_with_flags(&dbp, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            };
            let cfg = fleetd::compat::CompatCfg::stage_defaults(&c);
            let sql = fleetd::compat::Ctx { db: &c, now, cfg: &cfg };
            let mut cfg2 = cfg.clone();
            cfg2.memo = Some(std::sync::Arc::new(fleetd::compat::Memo::default()));
            let win = fleetd::compat::Ctx { db: &c, now, cfg: &cfg2 };
            let t0 = std::time::Instant::now();
            let a = fleetd::compat::behavior::summary_live(&sql, &Default::default());
            let a2 = fleetd::compat::behavior::activity_aggregates(&sql);
            let t1 = t0.elapsed();
            let t2 = std::time::Instant::now();
            let b = fleetd::compat::behavior::summary_window(&win).unwrap();
            let t3 = t2.elapsed();
            let t4 = std::time::Instant::now();
            let b2 = fleetd::compat::behavior::summary_window(&win).unwrap();
            let t5 = t4.elapsed();
            let b3 = fleetd::compat::behavior::activity_window(&win).unwrap();
            let (Ok(a), Ok(a2), Ok(b), Ok(b2), Ok(b3)) = (a, a2, b, b2, b3) else {
                eprintln!("查询失败");
                return 1;
            };
            let mut bad = 0;
            for k in ["nodes", "targets", "sources", "events", "next_offset", "offset", "started_at"] {
                let (x, y) = (fleetd::compat::pyjson::dumps(&a[k]), fleetd::compat::pyjson::dumps(&b[k]));
                let same = x == y && fleetd::compat::pyjson::dumps(&b2[k]) == y;
                println!("summary.{k:<12} {} ({} B)", if same { "一致" } else { "不一致" }, x.len());
                if !same {
                    bad += 1;
                    let p = x.bytes().zip(y.bytes()).position(|(p, q)| p != q).unwrap_or(0);
                    println!("   SQL …{}…\n   窗口 …{}…", &x[p.saturating_sub(120)..(p + 120).min(x.len())], &y[p.saturating_sub(120)..(p + 120).min(y.len())]);
                }
            }
            for k in ["domains", "sources", "total"] {
                let (x, y) = (fleetd::compat::pyjson::dumps(&a2[k]), fleetd::compat::pyjson::dumps(&b3[k]));
                println!("activity.{k:<10} {} ({} B)", if x == y { "一致" } else { "不一致" }, x.len());
                if x != y {
                    bad += 1;
                    let p = x.bytes().zip(y.bytes()).position(|(p, q)| p != q).unwrap_or(0);
                    println!("   SQL …{}…\n   窗口 …{}…", &x[p.saturating_sub(120)..(p + 120).min(x.len())], &y[p.saturating_sub(120)..(p + 120).min(y.len())]);
                }
            }
            // 带过滤的 /activity 聚合
            let asset0: String = c.query_row("SELECT asset FROM activity ORDER BY rowid DESC LIMIT 1", [], |r| r.get(0)).unwrap_or_default();
            let host0: String = c.query_row("SELECT host FROM activity ORDER BY rowid DESC LIMIT 1", [], |r| r.get(0)).unwrap_or_default();
            for (k, v) in [("asset", asset0.as_str()), ("team", "团队1"), ("team", "团队2"), ("action", "blocked"), ("action", "routed"), ("kind", "ip"), ("host", host0.as_str())] {
                let mut p = fleetd::compat::py::Params::new();
                p.insert(k.into(), v.into());
                p.insert("limit".into(), "5".into());
                let t0 = std::time::Instant::now();
                let x = fleetd::compat::store::activity(&sql, &p);
                let ts = t0.elapsed();
                let t0 = std::time::Instant::now();
                let y = fleetd::compat::store::activity(&win, &p);
                let tw = t0.elapsed();
                let (Ok(x), Ok(y)) = (x, y) else {
                    println!("activity {k}={v} 查询失败");
                    bad += 1;
                    continue;
                };
                let same = ["domains", "sources", "total", "rows"].iter().all(|f| fleetd::compat::pyjson::dumps(&x[*f]) == fleetd::compat::pyjson::dumps(&y[*f]));
                // 同值（connections 相同）之间的先后顺序 SQLite 随执行计划而变：按值集合比较
                let norm = |v: &serde_json::Value| -> Vec<String> {
                    let mut a: Vec<String> = v.as_array().map(|a| a.iter().map(fleetd::compat::pyjson::dumps).collect()).unwrap_or_default();
                    a.sort();
                    a
                };
                let same_set = ["domains", "sources"].iter().all(|f| norm(&x[*f]) == norm(&y[*f])) && x["total"] == y["total"] && x["rows"] == y["rows"];
                println!("activity?{k}={:<14} {}  SQL {:.0} ms / 窗口 {:.1} ms", v.chars().take(14).collect::<String>(),
                    if same { "一致" } else if same_set { "一致（同值先后不同）" } else { "不一致" }, ts.as_secs_f64() * 1000.0, tw.as_secs_f64() * 1000.0);
                let same = same || same_set;
                if !same {
                    bad += 1;
                    for f in ["domains", "sources", "total"] {
                        let (a, b) = (fleetd::compat::pyjson::dumps(&x[f]), fleetd::compat::pyjson::dumps(&y[f]));
                        if a != b {
                            let p = a.bytes().zip(b.bytes()).position(|(p, q)| p != q).unwrap_or(0);
                            println!("   {f} SQL …{}…\n   {f} 窗口 …{}…", &a[p.saturating_sub(100)..(p + 100).min(a.len())], &b[p.saturating_sub(100)..(p + 100).min(b.len())]);
                        }
                    }
                }
            }
            println!("SQL {:.0} ms；窗口首次（含加载）{:.0} ms，之后 {:.1} ms", t1.as_secs_f64() * 1000.0, t3.as_secs_f64() * 1000.0, t5.as_secs_f64() * 1000.0);
            if bad == 0 { 0 } else { 1 }
        }
        "set-ip" => {
            // fleetd 用户：改资产 IP（审计 asset_ip）；--with-target 连接地址一起改并在备注里记一笔
            let p = db.clone().unwrap_or_else(|| PathBuf::from(fleetd::legacystore::DEFAULT_PATH));
            let (Some(u), Some(ip)) = (uuid_a.clone(), ip_a.clone()) else {
                eprintln!("用法：fleetd legacy set-ip --uuid U --ip IP [--actor A] [--reason R] [--with-target]");
                return 2;
            };
            let c = match rusqlite::Connection::open(&p) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            };
            let _ = c.busy_timeout(std::time::Duration::from_secs(20));
            let cfg = compat::CompatCfg::default();
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
            let cx = compat::Ctx { db: &c, now, cfg: &cfg };
            match compat::writes::set_asset_ip(&cx, &u, &ip, actor_a.as_deref().unwrap_or("cli"), reason_a.as_deref().unwrap_or("手动更新"), with_target) {
                Ok(Some(a)) => {
                    println!("已更新：{}（ip {}，target {}）", a["name"].as_str().unwrap_or(""), a["ip"], a["target"]);
                    0
                }
                Ok(None) => {
                    println!("IP 没变");
                    0
                }
                Err(e) => {
                    eprintln!("失败：{e:?}");
                    1
                }
            }
        }
        "collapse-domains" => {
            // fleetd 用户：把 ts ≥ since 的逐条随机子域名新域名事件按主域名合并（compat::domains），可与运行中的 fleetd 并发
            let p = db.clone().unwrap_or_else(|| PathBuf::from(fleetd::legacystore::DEFAULT_PATH));
            let Some(since) = since else {
                eprintln!("用法：fleetd legacy collapse-domains [--db F] --since UNIX_TS [--dry-run]");
                return 2;
            };
            let c = match rusqlite::Connection::open(&p) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            };
            let _ = c.busy_timeout(std::time::Duration::from_secs(20));
            match compat::domains::backfill(&c, since, dry) {
                Ok(v) => {
                    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                    0
                }
                Err(e) => {
                    eprintln!("合并失败：{e}");
                    1
                }
            }
        }
        "catchup" => {
            // root，下线时：旧后端停止后，把快照之后的人工写入并入原生旧库（fleetd 需先停止）
            let from = db.clone().unwrap_or_else(|| PathBuf::from("/var/lib/team-ops/operations.db"));
            let to = golden.clone().unwrap_or_else(|| PathBuf::from(fleetd::legacystore::DEFAULT_PATH));
            let owner = if my_uid() == Some(0) { Some("fleetd") } else { None };
            match fleetd::legacystore::catchup(&from, &to, owner) {
                Ok(v) => {
                    println!("已并入 {} → {}：{}", from.display(), to.display(), v);
                    0
                }
                Err(e) => {
                    eprintln!("并入失败：{e}");
                    1
                }
            }
        }
        "render" => {
            let (Some(db), Some(now), Some(path)) = (db, now, path) else {
                eprintln!("用法：fleetd legacy render --db F --now T --path P");
                return 2;
            };
            let c = match open(&db) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            };
            let cfg = mk_cfg(&c);
            let (p, q) = compat::split_query(&path);
            let (code, body) = compat::respond(&compat::Ctx { db: &c, now, cfg: &cfg }, &p, &q);
            eprintln!("status {code}");
            print!("{body}");
            0
        }
        "verify" => {
            let Some(g) = golden else {
                eprintln!("用法：fleetd legacy verify --golden DIR");
                return 2;
            };
            let idx: serde_json::Value = match std::fs::read_to_string(g.join("index.json")).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string())) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("读 index.json：{e}");
                    return 1;
                }
            };
            let dbp = db.unwrap_or_else(|| PathBuf::from(idx["db"].as_str().unwrap_or("")));
            let now = now.unwrap_or_else(|| idx["as_of"].as_f64().unwrap_or(0.0));
            let c = match open(&dbp) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            };
            let cfg = mk_cfg(&c);
            let (mut same, mut sem, mut diff) = (0, 0, 0);
            for (n, e) in idx["paths"].as_array().cloned().unwrap_or_default().iter().enumerate() {
                if only.is_some_and(|o| o != n) {
                    continue;
                }
                let path = e["path"].as_str().unwrap_or("");
                let want = std::fs::read(g.join(e["file"].as_str().unwrap_or(""))).unwrap_or_default();
                let t0 = std::time::Instant::now();
                let (p, q) = compat::split_query(path);
                let (code, body) = compat::respond(&compat::Ctx { db: &c, now, cfg: &cfg }, &p, &q);
                let ms = t0.elapsed().as_millis();
                let wcode = e["status"].as_u64().unwrap_or(0) as u16;
                if code == wcode && body.as_bytes() == want.as_slice() {
                    same += 1;
                    println!("OK    {n:03} {code} {:>8} B {ms:>5} ms  {path}", body.len());
                    continue;
                }
                let a: Option<serde_json::Value> = serde_json::from_slice(&want).ok();
                let b: Option<serde_json::Value> = serde_json::from_str(&body).ok();
                if code == wcode && a.is_some() && a == b {
                    sem += 1;
                    println!("SEM   {n:03} {code} {ms:>5} ms  {path}  （值相同，字节/顺序不同）");
                } else {
                    diff += 1;
                    println!("DIFF  {n:03} {code}（旧 {wcode}） {ms:>5} ms  {path}");
                }
                let (x, y) = (want.as_slice(), body.as_bytes());
                let k = x.iter().zip(y.iter()).position(|(a, b)| a != b).unwrap_or(x.len().min(y.len()));
                let ctx = |s: &[u8]| String::from_utf8_lossy(&s[k.saturating_sub(160)..(k + 160).min(s.len())]).into_owned();
                println!("      首个差异 @{k}（旧 {} B，新 {} B）\n      旧：…{}…\n      新：…{}…", x.len(), y.len(), ctx(x), ctx(y));
            }
            println!("逐字节一致 {same}，仅格式不同 {sem}，不一致 {diff}");
            if diff + sem == 0 { 0 } else { 1 }
        }
        _ => {
            eprintln!("用法：fleetd legacy render|verify …");
            2
        }
    }
}

/// fleetd vendor import <vendor.json> [--komari-db /var/lib/team-ops/operations.db] [--dry-run] [--config …]
/// fleetd vendor meters [--config …]
fn vendor_cli(args: &[String]) -> i32 {
    let mut cfgp = std::env::var("FLEETD_CONFIG").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/etc/fleetd/fleetd.toml"));
    let mut komari: Option<PathBuf> = None;
    let mut dry = false;
    let mut pos = vec![];
    let mut it = args.iter();
    while let Some(x) = it.next() {
        match x.as_str() {
            "--config" | "-c" => cfgp = PathBuf::from(it.next().cloned().unwrap_or_default()),
            "--komari-db" => komari = it.next().map(PathBuf::from),
            "--dry-run" => dry = true,
            _ => pos.push(x.clone()),
        }
    }
    let c = load(&cfgp);
    let ldb = c.legacy_db_path();
    match pos.first().map(String::as_str) {
        Some("import") => {
            let Some(j) = pos.get(1) else {
                eprintln!("用法：fleetd vendor import <vendor.json> [--komari-db F] [--dry-run]");
                return 2;
            };
            let r = fleetd::vendor::import(&ldb, &c.data_dir.join("ingest.db"), std::path::Path::new(j), komari.as_deref(), dry, &c.edge.relay_hosts, SystemClock.now());
            // root 运行时：库文件（含 -wal/-shm）保持 fleetd 所有
            if my_uid() == Some(0) {
                let _ = std::process::Command::new("chown").arg("fleetd:fleetd").arg(&ldb).arg(format!("{}-wal", ldb.display())).arg(format!("{}-shm", ldb.display())).status();
            }
            match r {
                Ok(v) => {
                    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                    0
                }
                Err(e) => {
                    eprintln!("导入失败：{e}");
                    1
                }
            }
        }
        Some("meters") => {
            let Ok(conn) = rusqlite::Connection::open_with_flags(&ldb, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else { return 1 };
            println!("{}", serde_json::to_string_pretty(&fleetd::meter::report(&conn)).unwrap_or_default());
            0
        }
        _ => {
            eprintln!("用法：fleetd vendor import <vendor.json> [--komari-db F] [--dry-run] | meters");
            2
        }
    }
}
