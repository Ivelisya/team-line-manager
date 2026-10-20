//! relayd（观察模式）：本机主机遥测 + Xray 只读统计（QueryStats reset=false、进程、连接数），
//! 内嵌 L1/L2 探测（fleet-probe）、访问日志 tail（fleet.access.v1），上报到 {endpoint}/v2/ingest/relay；
//! 管理模式（fleet.relayspec.v1）只在 fleetd 下发期望状态时才改 Xray（见 manage.rs）。
use fleet_edge::config::Config;
use fleet_edge::relay::Relay;
use fleet_edge::runner::{self, Extension};

fn main() {
    let o = runner::parse_args("relay", "relayd");
    let cfg = match Config::load(&o.config) {
        Ok(c) => c,
        Err(e) => {
            fleet_edge::error!("config_invalid", "err={}", fleet_edge::log::q(&e));
            std::process::exit(1)
        }
    };
    let Some(xc) = cfg.xray.clone() else {
        fleet_edge::error!("config_invalid", "err={}", fleet_edge::log::q("relayd 需要 [xray] 段"));
        std::process::exit(1)
    };
    let state = if o.once { None } else { Some(cfg.state_dir()) };
    let xcfg_path = xc.config.clone();
    let (xc_bin, xc_match) = (xc.bin.clone(), xc.proc_match.clone());
    let collector = match fleet_edge::xray::Collector::new(xc, state.clone()) {
        Ok(c) => c,
        Err(e) => {
            fleet_edge::error!("xray_init_failed", "err={}", fleet_edge::log::q(&e));
            std::process::exit(1)
        }
    };
    let pcfg = cfg.probe.clone().unwrap_or_default();
    let xproc = (std::path::Path::new(&xc_bin).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(), xc_match);
    let probe = pcfg.enabled.then(|| {
        let mut p = fleet_edge::probe::ProbeLink::new(pcfg, xcfg_path, &cfg.state_dir(), &cfg.host_id);
        p.xray_proc = xproc;
        p.relay_self = cfg.relay.clone();
        p
    });
    let acfg = cfg.access.clone().unwrap_or_default();
    let access = acfg.enabled.then(|| {
        let mut t = fleet_edge::accesslog::AccessTail::new(state.as_deref());
        t.max_events = acfg.max_events.clamp(1, 2000);
        (t, acfg.path.clone())
    });
    let mcfg = cfg.manage.clone().unwrap_or_default();
    let manage = (mcfg.enabled && !o.once).then(|| fleet_edge::manage::Manager::new(mcfg, &cfg.host_id, &cfg.endpoint, &cfg.token_file, &cfg.state_dir(), cfg.http_timeout_s));
    let ext: Box<dyn Extension> = Box::new(Relay { legacy: Some(fleet_edge::relay::LegacyFiles::new()), xray: collector, probe, access, manage, once: o.once });
    std::process::exit(runner::run(o, Some(ext)));
}
