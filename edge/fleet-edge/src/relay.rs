//! relayd 的扩展：Xray 只读统计 + 内嵌探测 + 访问日志 tail + 管理模式。
use crate::accesslog::AccessTail;
use crate::manage::Manager;
use crate::probe::ProbeLink;
use crate::runner::Extension;
use crate::xray::Collector;
use serde_json::{json, Value};

/// 过渡期：旧线路机侧定时器写的两个状态文件，mtime 变化时原样转发（只读），供中控与旧系统逐字节对照
pub struct LegacyFiles {
    pub files: Vec<(&'static str, String)>, // (键名, 路径)
    sent: std::collections::HashMap<&'static str, f64>,
    staged: Vec<(&'static str, f64)>,
    pub skipped_oversize: u64,
}

impl LegacyFiles {
    pub fn new() -> LegacyFiles {
        LegacyFiles {
            files: vec![("probe", "/var/lib/komari-relay-probe/status.json".into()), ("paths", "/var/lib/team-ops-bridge/path-status.json".into())],
            sent: Default::default(),
            staged: vec![],
            skipped_oversize: 0,
        }
    }

    fn batch(&mut self) -> Option<Value> {
        self.staged.clear();
        let mut out = serde_json::Map::new();
        for (k, path) in &self.files {
            let Ok(md) = std::fs::metadata(path) else { continue };
            let mt = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64()).unwrap_or(0.0);
            if self.sent.get(k).is_some_and(|m| *m == mt) {
                continue;
            }
            if md.len() > 256 * 1024 {
                self.skipped_oversize += 1;
                self.staged.push((k, mt));
                continue;
            }
            let Ok(v) = std::fs::read(path).map_err(|_| ()).and_then(|b| serde_json::from_slice::<Value>(&b).map_err(|_| ())) else { continue };
            out.insert((*k).to_string(), v);
            out.insert(format!("{k}_mtime"), json!(mt));
            self.staged.push((k, mt));
        }
        if out.is_empty() {
            return None;
        }
        out.insert("skipped_oversize".into(), json!(self.skipped_oversize));
        Some(Value::Object(out))
    }

    fn commit(&mut self) {
        for (k, mt) in self.staged.drain(..) {
            self.sent.insert(k, mt);
        }
    }
}

impl Default for LegacyFiles {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Relay {
    pub legacy: Option<LegacyFiles>,
    pub xray: Collector,
    pub probe: Option<ProbeLink>,
    pub access: Option<(AccessTail, Option<String>)>, // (tail, 配置里写死的路径)
    pub manage: Option<Manager>,
    pub once: bool,
}

impl Extension for Relay {
    fn start(&mut self) {
        if let Some(p) = self.probe.as_mut() {
            p.start();
        }
        if let Some(m) = self.manage.as_mut() {
            m.self_check();
            m.periodic_check(&self.xray);
        }
    }

    fn tick(&mut self) {
        if let Some(m) = self.manage.as_mut() {
            m.tick(&mut self.xray);
            m.periodic_check(&self.xray);
        }
    }

    fn on_sample(&mut self, ts: f64) -> Option<Value> {
        self.xray.on_sample(ts)
    }

    fn on_batch(&mut self, pressure: f64) -> Vec<(&'static str, Value)> {
        let mut v = self.xray.on_batch(pressure);
        if let Some((_, x)) = v.iter_mut().find(|(k, _)| *k == "xray") {
            x["spec_generation"] = json!(self.manage.as_ref().map(|m| m.generation).unwrap_or(0));
            if let Some(m) = &self.manage {
                x["manage"] = m.state_json();
            }
            if !self.xray.notes.is_empty() {
                x["ledger_notes"] = Value::Array(std::mem::take(&mut self.xray.notes));
            }
        }
        if let Some(p) = self.probe.as_mut() {
            if self.once {
                v.push(("probe", p.describe()));
            } else if let Some(b) = p.batch() {
                v.push(("probe", b));
            }
        }
        if let Some(l) = self.legacy.as_mut() {
            if let Some(x) = l.batch() {
                v.push(("legacy_probe", x));
            }
        }
        if let Some((t, fixed)) = self.access.as_mut() {
            t.set_path(fixed.clone().or_else(|| self.xray.access_log.clone()));
            let bh: Vec<&str> = self.xray.blackholes.iter().map(|s| s.as_str()).collect();
            if let Some(a) = t.read(&bh, pressure > 0.5) {
                v.push(("access", a));
            }
        }
        v
    }

    fn commit(&mut self) {
        self.xray.commit();
        if let Some(p) = self.probe.as_mut() {
            p.commit();
        }
        if let Some((t, _)) = self.access.as_mut() {
            t.commit();
        }
        if let Some(l) = self.legacy.as_mut() {
            l.commit();
        }
    }
}
