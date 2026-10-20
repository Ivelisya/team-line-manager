//! 单线程主循环：每 `sample_interval_s` 采一次样，每 `batch_samples` 个样本成一批写进 outbox，
//! 然后按顺序上报（最旧的先发，确认后再发下一条）。等待用 sigtimedwait，SIGTERM/SIGINT 时把
//! 未满的一批也落盘后退出。
use crate::config::Config;
use crate::hostmon::{self, Sampler};
use crate::outbox::Outbox;
use crate::push::{Backoff, Outcome, Pusher};
use crate::util::{self, now_f64, Rng};
use crate::{error, info, warn};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// relayd 在主机样本之外附加的数据（Xray 计数等）。
pub trait Extension {
    /// 屏蔽信号之后、进入主循环之前调用一次（在这里起后台线程，线程继承信号屏蔽）
    fn start(&mut self) {}
    /// 每个采样周期调用一次，结果放在 sample.xray
    fn on_sample(&mut self, ts: f64) -> Option<Value>;
    /// 每批调用一次，返回的键值放在批次顶层（"xray"、"probe"、"access"）。`pressure` = outbox 未确认字节 / 上限（背压用）
    fn on_batch(&mut self, pressure: f64) -> Vec<(&'static str, Value)>;
    /// 每个主循环周期调用（管理模式拉取期望状态等）
    fn tick(&mut self) {}
    /// 这一批已持久化进 outbox 之后调用（保存计数基线）
    fn commit(&mut self);
}

pub struct Opts {
    pub kind: &'static str,
    pub name: &'static str,
    pub config: String,
    pub once: bool,
}

pub fn parse_args(kind: &'static str, name: &'static str) -> Opts {
    let mut o = Opts { kind, name, config: "/etc/fleet-edge/agent.toml".into(), once: false };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--once" => o.once = true,
            "--config" | "-c" => o.config = it.next().unwrap_or_else(|| usage(name)),
            "--version" | "-V" => {
                println!("{name} {}", crate::VERSION);
                std::process::exit(0)
            }
            _ => usage(name),
        }
    }
    o
}

fn usage(name: &str) -> ! {
    eprintln!("{name} {}\n用法：{name} [--config /etc/fleet-edge/agent.toml] [--once] [--version]\n  --once  采两个样本（间隔 1 s）打印一批 JSON 后退出，不写状态、不上报", crate::VERSION);
    std::process::exit(2)
}

struct SeqState {
    path: PathBuf,
    boot_id: String,
    seq: u64,
}

impl SeqState {
    fn load(dir: &std::path::Path, boot_id: &str) -> SeqState {
        let path = dir.join("state.json");
        let mut seq = 0;
        if let Ok(s) = std::fs::read_to_string(&path) {
            if let Ok(v) = serde_json::from_str::<Value>(&s) {
                if v["boot_id"].as_str() == Some(boot_id) {
                    seq = v["seq"].as_u64().unwrap_or(0);
                }
            }
        }
        if seq == 0 {
            // 本次开机没有状态（首次启动或状态被清）：从当前 unix 秒起算。每 30 s 才用一个序号，
            // 所以一定大于本次开机里以前用过的任何序号，中控按 (boot_id, seq) 去重不会误伤。
            seq = now_f64() as u64;
        }
        SeqState { path, boot_id: boot_id.into(), seq }
    }
    fn next(&mut self) -> u64 {
        self.seq += 1;
        let _ = util::write_atomic(&self.path, json!({"boot_id": self.boot_id, "seq": self.seq}).to_string().as_bytes());
        self.seq
    }
}

pub fn run(o: Opts, mut ext: Option<Box<dyn Extension>>) -> i32 {
    let cfg = match Config::load(&o.config) {
        Ok(c) => c,
        Err(e) => {
            error!("config_invalid", "err={}", crate::log::q(&e));
            return 1;
        }
    };
    let meta = hostmon::meta();
    let agent = format!("{}/{}", o.name, crate::VERSION);
    let mut sampler = Sampler::new();

    if o.once {
        let _ = sampler.sample(now_f64());
        if let Some(x) = ext.as_mut() {
            x.on_sample(now_f64());
        }
        std::thread::sleep(Duration::from_secs(1));
        let mut s = sampler.sample(now_f64());
        if let Some(x) = ext.as_mut() {
            s.xray = x.on_sample(s.ts);
        }
        let b = batch(&o, &cfg, &meta, &agent, 0, vec![s], 0, 0, ext.as_mut().map(|x| x.on_batch(0.0)).unwrap_or_default());
        println!("{}", serde_json::to_string_pretty(&b).unwrap());
        match cfg.read_token() {
            Ok(_) => eprintln!("令牌文件可读：{}", cfg.token_file.display()),
            Err(e) => eprintln!("注意：{e}"),
        }
        eprintln!("上报地址：{}（--once 不上报）", cfg.url(o.kind));
        return 0;
    }

    util::block_signals();
    let dir = cfg.state_dir();
    let mut outbox = match Outbox::open(&dir, cfg.outbox_max_bytes) {
        Ok(ob) => ob,
        Err(e) => {
            error!("outbox_open_failed", "dir={} err={}", crate::log::q(&dir.display().to_string()), crate::log::q(&e.to_string()));
            return 1;
        }
    };
    let mut seq = SeqState::load(&dir, &meta.boot_id);
    sampler = sampler.with_net_state(&dir);
    if let Some(x) = ext.as_mut() {
        x.start();
    }
    info!(
        "start",
        "agent={agent} host_id={} kind={} endpoint={} state_dir={} pending={} dropped={} seq={}",
        crate::log::q(&cfg.host_id),
        o.kind,
        crate::log::q(&cfg.url(o.kind)),
        crate::log::q(&dir.display().to_string()),
        outbox.pending(),
        outbox.dropped(),
        seq.seq
    );

    let mut rng = Rng::new(util::fnv1a(&cfg.host_id));
    let mut backoff = Backoff { fails: 0 };
    let mut pusher: Option<Pusher> = None;
    let mut rejected: u64 = 0;
    let mut last_ok: f64 = 0.0;
    let mut last_err: Option<String> = None;
    let interval = Duration::from_secs(cfg.sample_interval_s);

    // 错峰：按 host_id 哈希在周期内取一个固定偏移，并对齐墙钟
    let iv_ms = cfg.sample_interval_s * 1000;
    let off_ms = util::fnv1a(&cfg.host_id) % iv_ms;
    let now_ms = (now_f64() * 1000.0) as u64;
    let first_ms = ((now_ms.saturating_sub(off_ms)) / iv_ms + 1) * iv_ms + off_ms;
    // 先打一次底（CPU/网卡速率需要前值）。扩展（Xray）不打底：它的每次采集都会产生增量，丢掉就少计。
    let _ = sampler.sample(now_f64());
    let mut next_tick = Instant::now() + Duration::from_millis(first_ms - now_ms);
    let mut next_push = Instant::now();
    let mut samples: Vec<hostmon::Sample> = Vec::with_capacity(cfg.batch_samples as usize);

    loop {
        let now = Instant::now();
        let wake = if outbox.pending() > 0 { next_tick.min(next_push) } else { next_tick };
        if wait_until(wake, now) {
            info!("shutdown", "signal=TERM partial_samples={}", samples.len());
            if !samples.is_empty() {
                let xb = ext.as_mut().map(|x| x.on_batch(outbox.pending_bytes() as f64 / cfg.outbox_max_bytes as f64)).unwrap_or_default();
                enqueue(&o, &cfg, &meta, &agent, &mut seq, &mut outbox, std::mem::take(&mut samples), xb, &mut ext);
                sampler.commit_net();
            }
            return 0;
        }
        let now = Instant::now();
        if now >= next_tick {
            let mut s = sampler.sample(now_f64());
            if let Some(x) = ext.as_mut() {
                s.xray = x.on_sample(s.ts);
            }
            samples.push(s);
            if let Some(x) = ext.as_mut() {
                x.tick();
            }
            next_tick += interval;
            if next_tick <= now {
                // 进程被挂起或时钟跳变：跳过错过的周期，不补采
                let behind = now.duration_since(next_tick).as_secs() / cfg.sample_interval_s + 1;
                next_tick += interval * behind as u32;
            }
            if samples.len() >= cfg.batch_samples as usize {
                let xb = ext.as_mut().map(|x| x.on_batch(outbox.pending_bytes() as f64 / cfg.outbox_max_bytes as f64)).unwrap_or_default();
                enqueue(&o, &cfg, &meta, &agent, &mut seq, &mut outbox, std::mem::take(&mut samples), xb, &mut ext);
                sampler.commit_net();
            } else {
                continue;
            }
        }
        if outbox.pending() == 0 || Instant::now() < next_push {
            continue;
        }
        // 上报：最旧的先发
        if pusher.is_none() {
            match cfg.read_token() {
                Ok(t) => pusher = Some(Pusher::new(cfg.url(o.kind), &t, cfg.http_timeout_s)),
                Err(e) => {
                    let d = backoff.next_delay(&mut rng).max(Duration::from_secs(30));
                    warn!("token_unavailable", "err={} retry_in_s={}", crate::log::q(&e), d.as_secs());
                    last_err = Some(e);
                    next_push = Instant::now() + d;
                    write_status(&dir, &outbox, last_ok, &last_err, seq.seq, rejected);
                    continue;
                }
            }
        }
        let p = pusher.as_ref().unwrap();
        let started = Instant::now();
        let mut sent = 0u32;
        loop {
            let rec = match outbox.peek() {
                Ok(Some(r)) => r,
                Ok(None) => break,
                Err(e) => {
                    error!("outbox_read_failed", "err={}", crate::log::q(&e.to_string()));
                    next_push = Instant::now() + Duration::from_secs(60);
                    break;
                }
            };
            match p.send(&rec) {
                Outcome::Accepted { duplicate } => {
                    let _ = outbox.ack();
                    sent += 1;
                    last_ok = now_f64();
                    last_err = None;
                    if backoff.fails > 0 {
                        info!("push_recovered", "after_fails={}", backoff.fails);
                    }
                    backoff.reset();
                    if duplicate {
                        info!("push_duplicate", "note=already_accepted");
                    }
                }
                Outcome::Rejected(why) => {
                    rejected += 1;
                    let _ = outbox.ack();
                    error!("push_rejected", "dropped_batch=1 rejected_total={rejected} reason={}", crate::log::q(&why));
                    last_err = Some(why);
                }
                Outcome::Retry { reason, after_s } => {
                    let d = match after_s {
                        Some(s) => Duration::from_secs(s.clamp(1, 600)),
                        None => backoff.next_delay(&mut rng),
                    };
                    next_push = Instant::now() + d;
                    warn!(
                        "push_failed",
                        "reason={} retry_in_s={:.1} pending={} fails={}",
                        crate::log::q(&reason),
                        d.as_secs_f64(),
                        outbox.pending(),
                        backoff.fails
                    );
                    last_err = Some(reason);
                    // 令牌可能已更换：下次重新读取
                    pusher = None;
                    break;
                }
            }
            // 补传时一次最多 200 条或 20 s，剩下的下个周期接着发
            if sent >= 200 || started.elapsed() > Duration::from_secs(20) {
                next_push = Instant::now() + Duration::from_secs(1);
                break;
            }
        }
        if sent > 0 {
            info!("push_ok", "sent={sent} pending={} dropped={} seq={}", outbox.pending(), outbox.dropped(), seq.seq);
        }
        write_status(&dir, &outbox, last_ok, &last_err, seq.seq, rejected);
    }
}

fn wait_until(t: Instant, now: Instant) -> bool {
    if t <= now {
        return false;
    }
    util::wait_signal(t - now)
}

#[allow(clippy::too_many_arguments)]
fn batch(
    o: &Opts,
    cfg: &Config,
    meta: &hostmon::Meta,
    agent: &str,
    seq: u64,
    samples: Vec<hostmon::Sample>,
    pending: usize,
    dropped: u64,
    extra: Vec<(&'static str, Value)>,
) -> Value {
    let mut b = json!({
        "v": 1,
        "kind": o.kind,
        "host_id": cfg.host_id,
        "boot_id": meta.boot_id,
        "seq": seq,
        "agent": agent,
        "created_at": util::round2(now_f64()),
        "meta": {"kernel": meta.kernel, "os": meta.os, "ncpu": meta.ncpu},
        "outbox": {"pending": pending, "dropped": dropped},
        "samples": samples,
    });
    for (k, v) in extra {
        b[k] = v;
    }
    b
}

#[allow(clippy::too_many_arguments)]
fn enqueue(
    o: &Opts,
    cfg: &Config,
    meta: &hostmon::Meta,
    agent: &str,
    seq: &mut SeqState,
    outbox: &mut Outbox,
    samples: Vec<hostmon::Sample>,
    extra: Vec<(&'static str, Value)>,
    ext: &mut Option<Box<dyn Extension>>,
) {
    let n = seq.next();
    let b = batch(o, cfg, meta, agent, n, samples, outbox.pending(), outbox.dropped(), extra);
    let bytes = serde_json::to_vec(&b).unwrap();
    match outbox.push(&bytes) {
        Ok(()) => {
            if let Some(x) = ext.as_mut() {
                x.commit();
            }
        }
        Err(e) => error!("outbox_write_failed", "seq={n} bytes={} err={}", bytes.len(), crate::log::q(&e.to_string())),
    }
}

fn write_status(dir: &std::path::Path, ob: &Outbox, last_ok: f64, last_err: &Option<String>, seq: u64, rejected: u64) {
    let v = json!({
        "ts": util::round2(now_f64()),
        "last_ok": util::round2(last_ok),
        "last_error": last_err,
        "pending": ob.pending(),
        "pending_bytes": ob.pending_bytes(),
        "dropped": ob.dropped(),
        "rejected": rejected,
        "seq": seq,
    });
    let _ = util::write_atomic(&dir.join("status.json"), v.to_string().as_bytes());
}
