//! fleetctl onboard plan|run|status|resume|cancel <batch.csv|批次id>
use onboard::config::Config;
use onboard::db::Db;
use onboard::engine::Engine;
use onboard::{intake, planner, view};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const USAGE: &str = "用法：
  fleetctl onboard plan   <batch.csv> [--json] [--no-canary] [--concurrency N] [--window 秒]
  fleetctl onboard run    <batch.csv> --confirm <计划哈希> [--no-canary] [--concurrency N] [--window 秒]
  fleetctl onboard status <batch.csv|批次id> [--json] [--watch 秒]
  fleetctl onboard resume <batch.csv|批次id> [--row 节点 [--password-file 文件] [--force]] [--no-canary]
  fleetctl onboard cancel <batch.csv|批次id> [--row 节点]
  fleetctl onboard reattach <batch.csv|批次id>                 （线路机配置被整份重建后，把本批已上线入口原样加回）
  fleetctl onboard reveal <batch.csv|批次id> --row 节点          （输出分享链接，仅运维终端）
  fleetctl onboard api    <GET|POST> <路径> [JSON body]          （直接调 fleetd 处理函数，测试用）
通用：--config <onboard.toml>（默认 $FLEET_ONBOARD_CONFIG 或 /etc/fleet/onboard.toml）
CSV 列：provider,ip,ssh_port,auth(key|password:<文件>),plan,team,number[,relay,ss_port,dd(none|manual),mac,service_id,note]";

struct Args {
    pos: Vec<String>,
    flags: std::collections::BTreeMap<String, String>,
}
fn parse(argv: &[String]) -> Args {
    let mut pos = vec![];
    let mut flags = std::collections::BTreeMap::new();
    let mut i = 0;
    let bool_flags = ["--json", "--no-canary", "--force"];
    while i < argv.len() {
        let a = &argv[i];
        if a.starts_with("--") {
            if bool_flags.contains(&a.as_str()) {
                flags.insert(a.clone(), "1".into());
            } else {
                flags.insert(a.clone(), argv.get(i + 1).cloned().unwrap_or_default());
                i += 1;
            }
        } else {
            pos.push(a.clone());
        }
        i += 1;
    }
    Args { pos, flags }
}

fn die(m: impl std::fmt::Display) -> ! {
    eprintln!("错误：{m}");
    std::process::exit(2)
}

fn resolve(arg: &str) -> (String, Option<(String, PathBuf)>) {
    let p = Path::new(arg);
    if p.is_file() {
        let t = std::fs::read_to_string(p).unwrap_or_else(|e| die(format!("读 {arg}：{e}")));
        (intake::batch_id_for(&t), Some((t, std::fs::canonicalize(p).unwrap())))
    } else {
        (arg.to_string(), None)
    }
}

fn overrides(a: &Args) -> serde_json::Value {
    let mut o = json!({});
    if a.flags.contains_key("--no-canary") {
        o["canary"] = json!(false);
    }
    if let Some(n) = a.flags.get("--concurrency") {
        o["concurrency"] = json!(n.parse::<u64>().unwrap_or_else(|_| die("--concurrency 要整数")));
    }
    if let Some(n) = a.flags.get("--window") {
        o["relay_window_s"] = json!(n.parse::<u64>().unwrap_or_else(|_| die("--window 要整数（秒）")));
    }
    o
}

fn print_plan(p: &planner::PlanOut) {
    println!("批次 {} · 计划哈希 {} · {} 台 · 破坏性步骤 {} 个", p.batch_id, p.plan_hash, p.rows.len(), p.rows.iter().filter(|r| r.dd != "none").count());
    println!("{:<4} {:<10} {:<22} {:<16} {:<18} {:<9} {:<8} {}", "#", "节点", "IP:SSH（SS 公网端口）", "套餐", "价格", "线路机", "DD", "认证");
    for r in &p.rows {
        println!(
            "{:<4} {:<10} {:<22} {:<16} {:<18} {:<9} {:<8} {}",
            r.seq, r.short_name, format!("{}:{}（{}）", r.ip, r.ssh_port, r.ss_public_port), r.plan_name, format!("{} {}/月", r.price, r.currency),
            r.relay.clone().unwrap_or_default(), if r.dd == "manual" { "人工闸门" } else { "不重装" }, r.auth
        );
    }
    for i in &p.relay_impact {
        println!("线路机 {}：现有 {} 个入口，本批新增 {} 个（端口预估 {}）—— {}", i["relay"].as_str().unwrap_or(""), i["entries_now"], i["adding"], i["ports_preview"].as_str().unwrap_or(""), i["impact"].as_str().unwrap_or(""));
    }
    println!("选项：{}", p.options);
    for i in &p.issues {
        println!("{} 第 {} 行：{}", if i.level == "error" { "✗ 错误" } else { "! 提醒" }, i.seq, i.msg);
    }
    if p.errors() == 0 {
        println!("计划通过。确认执行：fleetctl onboard run <batch.csv> --confirm {}", p.plan_hash);
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) != Some("onboard") || argv.len() < 2 {
        println!("{USAGE}");
        std::process::exit(if argv.is_empty() { 0 } else { 2 });
    }
    let cmd = argv[1].clone();
    let a = parse(&argv[2..]);
    let cfg_path = a.flags.get("--config").cloned().or_else(|| std::env::var("FLEET_ONBOARD_CONFIG").ok()).unwrap_or_else(|| "/etc/fleet/onboard.toml".into());
    let cfg = Arc::new(Config::load(Path::new(&cfg_path)).unwrap_or_else(|e| die(e)));
    let db = Db::open(&cfg.db).unwrap_or_else(|e| die(e));
    let operator = std::env::var("FLEET_OPERATOR").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "ops".into());
    let target = a.pos.first().cloned().unwrap_or_else(|| if cmd == "api" { String::new() } else { die(USAGE) });
    match cmd.as_str() {
        "plan" => {
            let (_, csv) = resolve(&target);
            let (text, path) = csv.unwrap_or_else(|| die("plan 需要 CSV 文件"));
            let p = planner::plan(&cfg, &db, &text, path.parent().unwrap(), &overrides(&a)).await.unwrap_or_else(|e| die(e));
            if a.flags.contains_key("--json") {
                println!("{}", serde_json::to_string_pretty(&p.to_json()).unwrap());
            } else {
                print_plan(&p);
            }
            std::process::exit(if p.errors() > 0 { 1 } else { 0 });
        }
        "run" => {
            let (_, csv) = resolve(&target);
            let (text, path) = csv.unwrap_or_else(|| die("run 需要 CSV 文件"));
            let p = planner::plan(&cfg, &db, &text, path.parent().unwrap(), &overrides(&a)).await.unwrap_or_else(|e| die(e));
            if p.errors() > 0 {
                print_plan(&p);
                die("计划有错误");
            }
            let existing = db.batch(&p.batch_id).unwrap_or_else(|e| die(e));
            let expect = existing.as_ref().map(|b| b.plan_hash.clone()).unwrap_or(p.plan_hash.clone());
            if a.flags.get("--confirm") != Some(&expect) {
                print_plan(&p);
                die(format!("需要 --confirm {expect}（L2 确认：核对上面的计划后再执行）"));
            }
            let (id, created) = planner::ensure_batch(&db, &p, &text, Some(&path.display().to_string()), a.flags.get("--title").map(String::as_str), &operator).unwrap_or_else(|e| die(e));
            eprintln!("{} 批次 {id}", if created { "新建" } else { "继续" });
            let eng = Engine::new(cfg.clone(), db.clone()).unwrap_or_else(|e| die(e));
            let r = eng.run_batch(&id).await.unwrap_or_else(|e| die(e));
            print!("{}", view::text(&db, &cfg, &id).unwrap_or_else(|e| die(e)));
            println!("{}", serde_json::to_string(&r).unwrap());
            std::process::exit(if r["status"] == "finished" { 0 } else { 3 });
        }
        "status" => {
            let (id, _) = resolve(&target);
            let watch: u64 = a.flags.get("--watch").and_then(|x| x.parse().ok()).unwrap_or(0);
            loop {
                if a.flags.contains_key("--json") {
                    println!("{}", serde_json::to_string_pretty(&view::batch_view(&db, &cfg, &id).unwrap_or_else(|e| die(e))).unwrap());
                } else {
                    print!("{}", view::text(&db, &cfg, &id).unwrap_or_else(|e| die(e)));
                }
                if watch == 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(watch)).await;
                println!();
            }
        }
        "resume" => {
            let (id, _) = resolve(&target);
            let eng = Engine::new(cfg.clone(), db.clone()).unwrap_or_else(|e| die(e));
            if a.flags.contains_key("--no-canary") {
                let mut o = db.batch(&id).unwrap_or_else(|e| die(e)).unwrap_or_else(|| die("批次不存在")).options;
                o["canary"] = json!(false);
                db.set_batch_options(&id, &o).unwrap_or_else(|e| die(e));
            }
            if let Some(row) = a.flags.get("--row") {
                let m = eng.provide(&id, row, a.flags.get("--password-file").cloned(), a.flags.contains_key("--force")).unwrap_or_else(|e| die(e));
                eprintln!("{m}");
            }
            let r = eng.run_batch(&id).await.unwrap_or_else(|e| die(e));
            print!("{}", view::text(&db, &cfg, &id).unwrap_or_else(|e| die(e)));
            println!("{}", serde_json::to_string(&r).unwrap());
            std::process::exit(if r["status"] == "finished" { 0 } else { 3 });
        }
        "cancel" => {
            let (id, _) = resolve(&target);
            let eng = Engine::new(cfg.clone(), db.clone()).unwrap_or_else(|e| die(e));
            let r = eng.cancel(&id, a.flags.get("--row").map(String::as_str)).await.unwrap_or_else(|e| die(e));
            println!("{}", serde_json::to_string_pretty(&r).unwrap());
        }
        "reattach" => {
            let (id, _) = resolve(&target);
            let eng = Engine::new(cfg.clone(), db.clone()).unwrap_or_else(|e| die(e));
            let r = eng.reattach(&id).await.unwrap_or_else(|e| die(e));
            println!("{}", serde_json::to_string_pretty(&r).unwrap());
        }
        "reveal" => {
            let (id, _) = resolve(&target);
            let row = a.flags.get("--row").unwrap_or_else(|| die("要 --row"));
            let r = db.rows(&id).unwrap_or_else(|e| die(e)).into_iter().find(|r| &r.spec.short_name == row).unwrap_or_else(|| die("没有这一行"));
            println!("{}", db.secret(&r.id, "share_link").unwrap_or_else(|e| die(e)).unwrap_or_else(|| die("还没上线，没有分享链接")));
        }
        "api" => {
            let api = onboard::api::OnboardApi { cfg: cfg.clone(), db: db.clone(), tokens: None };
            let method = a.pos.first().cloned().unwrap_or_else(|| die(USAGE));
            let path = a.pos.get(1).cloned().unwrap_or_else(|| die(USAGE));
            let body = a.pos.get(2).cloned().unwrap_or_default();
            let (q_path, query) = path.split_once('?').map(|(p, q)| (p.to_string(), q.to_string())).unwrap_or((path.clone(), String::new()));
            let (code, v) = api.handle(&method, &q_path, &query, body.as_bytes(), &operator).await;
            println!("{code}\n{}", serde_json::to_string_pretty(&v).unwrap());
            // 后台任务（start/resume）需要进程活着：等它跑完
            if code == 202 {
                let id = v["data"]["batch_id"].as_str().unwrap_or("").to_string();
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    let b = db.batch(&id).ok().flatten();
                    if b.map(|b| b.status != "running" && b.status != "validated").unwrap_or(true) {
                        break;
                    }
                }
            }
        }
        _ => {
            println!("{USAGE}");
            std::process::exit(2);
        }
    }
}
