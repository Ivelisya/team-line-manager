//! 声明式应用：渲染 → 对比运行态算计划 →（确有改动才）xray -test → 经 API 热增删（不重启）→ 核对运行态 → 最后原子写配置文件。
//! 任一步失败按相反顺序回滚，回报 ok=false。Xray 没运行或骨架变了才走重启路径。

use super::cli::{Runtime, XrayCli};
use super::ctl;
use super::plan::{plan, verify, Step};
use super::render::{render, skeleton, RenderCtx, Rendered};
use crate::config::Config;
use anyhow::{Context, Result};
use fleet_proto::{tags, ApplyReport, DesiredState, EntryApply};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

pub struct Applier {
    pub cfg: Arc<Config>,
    pub cli: XrayCli,
    lock: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct Outcome {
    error: Option<String>,
    /// 本次涉及改动的入口
    changed: HashSet<Uuid>,
    /// 出错的那一步对应的入口
    culprit: HashSet<Uuid>,
    /// 走的是"会断开全部客户连接的重启路径"（Xray 在运行、骨架配置变了）
    disruptive_restart: bool,
}

/// 一次应用的结果：回报 + 是否走了会断开客户的重启路径
pub struct ApplyResult {
    pub report: ApplyReport,
    pub disruptive_restart: bool,
}

impl Outcome {
    fn fail(error: impl Into<String>) -> Self {
        Self { error: Some(error.into()), ..Default::default() }
    }
}

fn ids_of_tag(t: &str) -> Option<Uuid> {
    tags::parse_entry_id(t)
}

impl Applier {
    pub fn new(cfg: Arc<Config>) -> Self {
        let cli = XrayCli {
            bin: cfg.xray_bin.clone(),
            api: cfg.xray_api.clone(),
            asset_dir: cfg.xray_asset_dir.clone(),
            tmp_dir: cfg.data_dir.join("tmp"),
        };
        Self { cfg, cli, lock: tokio::sync::Mutex::new(()) }
    }

    pub fn ctx(&self) -> RenderCtx {
        RenderCtx { access_log: self.cfg.access_log.to_string_lossy().into(), api_addr: self.cfg.xray_api.clone() }
    }

    /// 磁盘上的现有配置（Agent 是唯一写者）
    fn read_prev(&self) -> Option<Rendered> {
        let raw = std::fs::read(&self.cfg.xray_config).ok()?;
        match serde_json::from_slice(&raw) {
            Ok(v) => Some(Rendered::from_full(v)),
            Err(e) => {
                tracing::warn!("现有 xray 配置无法解析，按空骨架处理：{e}");
                None
            }
        }
    }

    /// 写配置（含 fsync）放到阻塞线程，不占异步运行时
    async fn write_config(&self, full: &serde_json::Value) -> Result<()> {
        let mut data = serde_json::to_vec_pretty(full)?;
        data.push(b'\n');
        let path = self.cfg.xray_config.clone();
        tokio::task::spawn_blocking(move || crate::util::write_atomic(&path, &data, true)).await.context("写配置任务异常终止")?
    }

    /// `xray -test` 没过就返回对应的失败结果（只在真要写配置 / 改运行态之前才跑，它是个重子进程）
    async fn test_failed(&self, cand: &Rendered, ids: &HashSet<Uuid>) -> Option<Outcome> {
        let e = self.cli.test_config(&cand.full).await.err()?;
        Some(Outcome { error: Some(format!("xray -test 未通过：{e:#}")), changed: ids.clone(), ..Default::default() })
    }

    /// 启动时：清掉上次遗留的临时文件（含完整 xray 配置，有 REALITY 私钥）；配置缺失则写骨架；Xray 没运行则拉起
    pub async fn ensure_boot(&self) {
        let _g = self.lock.lock().await;
        let _ = std::fs::remove_dir_all(&self.cli.tmp_dir);
        if !self.cfg.xray_config.exists() {
            let sk = skeleton(&self.ctx());
            match self.write_config(&sk.full).await {
                Ok(()) => tracing::info!("已写入空骨架 {}", self.cfg.xray_config.display()),
                Err(e) => tracing::error!("写骨架失败：{e:#}"),
            }
        }
        if self.cli.lsi().await.is_err() {
            tracing::warn!("Xray API 不可达，尝试 systemctl start {}", self.cfg.xray_service);
            if let Err(e) = ctl::systemctl(&["start", &self.cfg.xray_service]).await {
                tracing::error!("启动 Xray 失败：{e:#}");
            }
        }
    }

    /// 轻量对齐检查（只读，不跑 xray -test）：磁盘上的 xray.json 等于期望渲染结果，且运行态核对通过。
    /// 返回 Err(不一致的原因)。
    pub async fn check_aligned(&self, d: &DesiredState) -> Result<(), String> {
        let _g = self.lock.lock().await;
        let cand = render(d, &self.ctx()).map_err(|e| format!("渲染失败：{e:#}"))?;
        match self.read_prev() {
            Some(p) if p.full == cand.full => {}
            Some(_) => return Err("磁盘上的 xray.json 与期望状态不一致".into()),
            None => return Err("xray.json 缺失或无法解析".into()),
        }
        let rt = self.cli.runtime().await.map_err(|e| format!("无法读取 Xray 运行态：{e:#}"))?;
        verify(&rt, &cand)
    }

    /// 等正在进行的应用（持有锁）结束，最多等 `max`。关停时用，免得在 apply 中途 abort 任务。
    pub async fn wait_idle(&self, max: Duration) -> bool {
        tokio::time::timeout(max, self.lock.lock()).await.is_ok()
    }

    pub async fn apply(&self, d: &DesiredState) -> ApplyReport {
        self.apply_detailed(d).await.report
    }

    pub async fn apply_detailed(&self, d: &DesiredState) -> ApplyResult {
        let _g = self.lock.lock().await;
        let t0 = Instant::now();
        let out = self.apply_inner(d).await;
        let disruptive_restart = out.disruptive_restart;
        let ok = out.error.is_none();
        let enabled: Vec<_> = d.entries.iter().filter(|e| e.enabled).collect();
        let entries = d
            .entries
            .iter()
            .map(|e| {
                let (eok, err) = match &out.error {
                    None => (true, None),
                    Some(m) if out.culprit.contains(&e.id) => (false, Some(m.clone())),
                    Some(m) if out.changed.contains(&e.id) => (false, Some(format!("已回滚：{m}"))),
                    // 没有任何入口被点名（渲染/校验阶段失败）→ 所有启用入口都没生效
                    Some(m) if out.changed.is_empty() && out.culprit.is_empty() && enabled.iter().any(|x| x.id == e.id) => (false, Some(m.clone())),
                    Some(_) => (true, None),
                };
                EntryApply { id: e.id, ok: eok, error: err }
            })
            .collect();
        tracing::info!(generation = d.generation, ok, ms = t0.elapsed().as_millis() as u64, "应用完成");
        ApplyResult { report: ApplyReport { generation: d.generation, ok, error: out.error, entries }, disruptive_restart }
    }

    async fn apply_inner(&self, d: &DesiredState) -> Outcome {
        let ctx = self.ctx();
        let cand = match render(d, &ctx) {
            Ok(c) => c,
            Err(e) => return Outcome::fail(format!("渲染失败：{e:#}")),
        };
        let prev_loaded = self.read_prev();
        let static_changed = prev_loaded.as_ref().is_some_and(|p| p.static_part != cand.static_part);
        let prev = prev_loaded.clone().unwrap_or_else(|| skeleton(&ctx));
        let all_ids: HashSet<Uuid> = d.entries.iter().filter(|e| e.enabled).map(|e| e.id).collect();

        // 先看运行态、算计划，确有写配置 / 重启 / 改运行态的动作时才跑 xray -test（幂等应用和启动对齐不该白起一个 xray 子进程）
        let rt = match self.cli.runtime().await {
            Ok(rt) if !static_changed => rt,
            Ok(_) => {
                if let Some(out) = self.test_failed(&cand, &all_ids).await {
                    return out; // 测试没过不算重启路径失败（没重启过，不走小时级闸门）
                }
                tracing::warn!("Xray 骨架配置变化，只能重启生效");
                let mut out = self.restart_apply(&cand, prev_loaded.as_ref(), all_ids).await;
                out.disruptive_restart = true;
                return out;
            }
            Err(e) => {
                // 只有确认 Xray 没在运行才走重启路径；"在运行但查询出错"绝不重启（重启会断开所有客户连接）
                if self.xray_is_down(&e).await {
                    if let Some(out) = self.test_failed(&cand, &all_ids).await {
                        return out;
                    }
                    tracing::warn!("Xray 未运行（{e:#}），写配置并启动");
                    return self.restart_apply(&cand, prev_loaded.as_ref(), all_ids).await;
                }
                return Outcome { error: Some(format!("Xray 在运行但 API 查询失败，未做任何改动：{e:#}")), changed: all_ids, culprit: HashSet::new(), ..Default::default() };
            }
        };

        let steps = plan(&rt, &prev, &cand);
        let changed: HashSet<Uuid> = steps.iter().filter_map(|s| ids_of_tag(&s.tag)).collect();
        if steps.is_empty() {
            if prev_loaded.as_ref().map(|p| &p.full) == Some(&cand.full) {
                return Outcome::default(); // 幂等：什么都不用做
            }
            if let Some(out) = self.test_failed(&cand, &all_ids).await {
                return out;
            }
            return match self.write_config(&cand.full).await {
                Ok(()) => Outcome::default(),
                Err(e) => Outcome { error: Some(format!("写配置失败：{e:#}")), changed, culprit: HashSet::new(), ..Default::default() },
            };
        }
        if let Some(out) = self.test_failed(&cand, &all_ids).await {
            return out;
        }
        tracing::info!(steps = steps.len(), "经 Xray API 应用变更");

        // 逐步生效
        for (i, s) in steps.iter().enumerate() {
            tracing::info!("  {}", s.op.describe());
            if let Err(e) = self.cli.exec(&s.op).await {
                let culprit = ids_of_tag(&s.tag).into_iter().collect();
                let msg = self.rollback(&steps[..=i], &rt, &format!("{}：{e:#}", s.op.describe())).await;
                return Outcome { error: Some(msg), changed, culprit, ..Default::default() };
            }
        }
        // 核对运行态
        let verdict = match self.cli.runtime().await {
            Ok(after) => verify(&after, &cand),
            Err(e) => Err(format!("无法读取运行态：{e:#}")),
        };
        if let Err(why) = verdict {
            let msg = self.rollback(&steps, &rt, &format!("运行态核对失败：{why}")).await;
            return Outcome { error: Some(msg), changed, culprit: HashSet::new(), ..Default::default() };
        }
        // 最后写文件
        if let Err(e) = self.write_config(&cand.full).await {
            let msg = self.rollback(&steps, &rt, &format!("写配置失败：{e:#}")).await;
            return Outcome { error: Some(msg), changed, culprit: HashSet::new(), ..Default::default() };
        }
        Outcome { error: None, changed, culprit: HashSet::new(), ..Default::default() }
    }

    /// API 连不上，且 systemd 里该服务也不是 active → 才认为 Xray 没在运行
    async fn xray_is_down(&self, err: &anyhow::Error) -> bool {
        let msg = format!("{err:#}");
        let dial_failed = msg.contains("failed to dial") || msg.contains("connection refused");
        dial_failed && ctl::systemctl(&["is-active", "--quiet", &self.cfg.xray_service]).await.is_err()
    }

    /// 按相反顺序撤回已执行的步骤（失败的那一步也撤一次，忽略"本来就没生效"的报错），再核对是否回到应用前
    async fn rollback(&self, done: &[Step], before: &Runtime, cause: &str) -> String {
        tracing::error!("应用失败，开始回滚：{cause}");
        let mut undo_errors = Vec::new();
        let last = done.len().saturating_sub(1);
        for (i, s) in done.iter().enumerate().rev() {
            match &s.undo {
                Some(u) => {
                    if let Err(e) = self.cli.exec(u).await {
                        // 最后一步（失败的那步）可能根本没生效，撤不掉是正常的
                        if i != last {
                            undo_errors.push(format!("{}：{e:#}", u.describe()));
                        }
                    }
                }
                None => undo_errors.push(format!("{}：旧对象内容未知，无法撤回", s.op.describe())),
            }
        }
        let back = match self.cli.runtime().await {
            Ok(rt) => sorted(&rt) == sorted(before),
            Err(_) => false,
        };
        if undo_errors.is_empty() && back {
            format!("{cause}；已撤回，运行态回到应用前")
        } else {
            format!("{cause}；撤回时出错 {undo_errors:?}，运行态未能确认回到应用前，请人工核对")
        }
    }

    /// Xray 没在运行 / 骨架变化：写配置 → systemctl restart → 等就绪 → 核对
    async fn restart_apply(&self, cand: &Rendered, prev: Option<&Rendered>, ids: HashSet<Uuid>) -> Outcome {
        let old_bytes = std::fs::read(&self.cfg.xray_config).ok();
        let fail = |m: String| Outcome { error: Some(m), changed: ids.clone(), culprit: HashSet::new(), ..Default::default() };
        if let Err(e) = self.write_config(&cand.full).await {
            return fail(format!("写配置失败：{e:#}"));
        }
        let res: Result<()> = async {
            ctl::systemctl(&["restart", &self.cfg.xray_service]).await?;
            let t0 = Instant::now();
            loop {
                match self.cli.runtime().await {
                    Ok(rt) => return verify(&rt, cand).map_err(anyhow::Error::msg),
                    Err(e) if t0.elapsed() > Duration::from_secs(30) => return Err(e).context("重启后 30s 内 Xray API 未就绪"),
                    Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
                }
            }
        }
        .await;
        match res {
            Ok(()) => Outcome { changed: ids, ..Default::default() },
            Err(e) => {
                // 恢复旧配置并尽力拉起旧版本
                if let (Some(b), Some(_)) = (old_bytes, prev) {
                    let _ = crate::util::write_atomic(&self.cfg.xray_config, &b, true);
                    let _ = ctl::systemctl(&["restart", &self.cfg.xray_service]).await;
                }
                fail(format!("重启应用失败：{e:#}"))
            }
        }
    }
}

fn sorted(r: &Runtime) -> (Vec<String>, Vec<String>, Vec<(String, String)>) {
    let mut a = r.inbounds.clone();
    let mut b = r.outbounds.clone();
    a.sort();
    b.sort();
    (a, b, r.rules.clone())
}
