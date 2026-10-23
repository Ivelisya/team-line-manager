//! 探测点：probe ls / add / install / set / rm / primary / central / upgrade。
//! 探测点是放在靠近客户的地方（如香港）的一台小机器，替中控测线路机的客户入口通不通；「主探测来源」决定线路机卡片的心跳格与
//! relay_unreachable 告警看哪一路（中控或某个探测点）。安装有两种：给 --host 由中控 SSH 登录装好（等首次上报）；
//! 不给就打印一次性安装命令（30 分钟、只能用一次），在探测点上用 root 执行。

use crate::cmds::{num, st, Ctx};
use crate::ops::confirm;
use crate::table;
use crate::resolve;
use anyhow::{anyhow, bail};
use serde_json::{json, Value};
use std::time::Duration;

fn state_cn(s: &str) -> &str {
    match s {
        "online" => "在线",
        "blind" => "在线（探测无效）",
        "offline" => "离线",
        "pending" => "等待安装",
        "disabled" => "已停用",
        s => s,
    }
}

fn install_cn(s: &str) -> &str {
    match s {
        "running" => "安装中",
        "failed" => "安装失败",
        "done" => "已安装",
        s => s,
    }
}


/// 找一个探测点：id、id 前缀（至少 6 位）、名称（不分大小写）；规则同 `resolve::named`
fn find(all: &[Value], q: &str) -> anyhow::Result<Value> {
    let name = |p: &Value| st(p, "/name");
    resolve::named(all, q, "探测点", &[&name], &name).cloned().map_err(|e| anyhow!("{e}（先 fleetctl probe ls 看列表）"))
}

fn ms(v: &Value, p: &str) -> String { num(v, p).map_or_else(|| "-".into(), |x| format!("{x:.0} ms")) }

fn row(p: &Value) -> Vec<String> {
    let mut state = state_cn(&st(p, "/state")).to_string();
    if let Some(s) = p["install"]["state"].as_str().filter(|s| *s != "done") {
        state.push_str(&format!("（{}）", install_cn(s)));
    }
    if p["upgrade"].is_object() {
        state.push_str("（升级中）");
    }
    if let Some(f) = upgrade_failed(p) {
        state.push_str(&format!("（{f}）"));
    }
    let relays = p["relays"].as_array().map(|rs| rs.iter().map(|r| format!("{} {}", st(r, "/name"), ms(r, "/avg_ms"))).collect::<Vec<_>>().join("，")).unwrap_or_default();
    vec![
        st(p, "/name"),
        if p["primary"] == true { "是".into() } else { "".into() },
        state,
        table::ts(&st(p, "/last_seen_at")),
        p["version"].as_str().unwrap_or("-").to_string(),
        p["last_ip"].as_str().unwrap_or("-").to_string(),
        if relays.is_empty() { "-".into() } else { relays },
        st(p, "/id").chars().take(13).collect(),
    ]
}

/// 上次升级失败（中控记的 upgrade_failed）：「升级到 X 失败，已回到 Y」
fn upgrade_failed(p: &Value) -> Option<String> {
    let f = p["upgrade_failed"].as_object()?;
    let v = |k: &str| f.get(k).and_then(Value::as_str).unwrap_or("?").to_string();
    Some(format!("升级到 {} 失败，已回到 {}", v("to"), v("from")))
}

const HEAD: [&str; 8] = ["名称", "主来源", "状态", "最近上报", "版本", "来源地址", "近 1 小时平均延迟", "id"];

/// 主探测来源的说法：「香港探测点」「中控（代替离线的香港探测点）」……
fn source_line(src: &Value) -> String {
    let mut s = format!("主探测来源：{}；此刻用的是：{}", st(src, "/primary_name"), st(src, "/effective/label"));
    s.push_str(if src["central_enabled"] == true { "；中控自己的检查：开" } else { "；中控自己的检查：关" });
    s
}

pub fn ls(ctx: &Ctx) -> anyhow::Result<u8> {
    let all = ctx.list("/probe-points")?;
    let src = ctx.c.get("/settings/reach-source")?;
    let data = json!({"probe_points": all, "source": src});
    ctx.emit(&data, || {
        let mut s = source_line(&src);
        if all.is_empty() {
            s.push_str("\n还没有探测点（fleetctl probe add <名称> --host <IP> 装一台）");
            return s;
        }
        let rows: Vec<Vec<String>> = all.iter().map(row).collect();
        s.push_str(&format!("\n\n{}", table::render(&HEAD, &rows)));
        for p in &all {
            if let Some(e) = p["install"]["error"].as_str() {
                s.push_str(&format!("\n\n「{}」安装失败：{e}", st(p, "/name")));
            }
            if let Some(exp) = p["bootstrap_expires_at"].as_str() {
                s.push_str(&format!("\n「{}」的安装命令还没用，{} 前有效", st(p, "/name"), table::ts(exp)));
            }
        }
        s
    });
    Ok(0)
}

/// 等经 SSH 的安装结束（含首次上报），边等边打印步骤；失败退出码 1
fn follow_install(ctx: &Ctx, id: &str) -> anyhow::Result<u8> {
    let mut last = String::new();
    for _ in 0..400 {
        let p = ctx.c.get(&format!("/probe-points/{id}"))?;
        let ins = &p["install"];
        let now = format!("{} {}", st(ins, "/state"), st(ins, "/step"));
        if now != last && !ctx.json {
            if let Some(step) = ins["step"].as_str() {
                eprintln!("{step}…");
            }
            last = now;
        }
        match ins["state"].as_str() {
            Some("done") => {
                ctx.emit(&p, || format!("「{}」装好了，已收到首次上报（版本 {}）。设为主探测来源：fleetctl probe primary {}", st(&p, "/name"), st(&p, "/version"), st(&p, "/name")));
                return Ok(0);
            }
            Some("failed") => {
                if ctx.json {
                    eprintln!("{}", json!({"error": {"code": "install_failed", "message": ins["error"], "status": null}}));
                } else {
                    eprintln!("安装失败：{}", st(ins, "/error"));
                }
                return Ok(1);
            }
            _ => std::thread::sleep(Duration::from_millis(1500)),
        }
    }
    bail!("等了 10 分钟安装还没结束：fleetctl probe ls 看进度")
}

fn print_bootstrap(ctx: &Ctx, d: &Value, name: &str) {
    ctx.emit(d, || {
        format!(
            "在探测点「{name}」上用 root 执行（{} 前有效，只能用一次；要能访问中控）：\n\n  {}\n\n执行后半分钟内 fleetctl probe ls 显示在线；过期了用 fleetctl probe install {name} 重新生成",
            table::ts(&st(d, "/bootstrap/expires_at")),
            st(d, "/bootstrap/cmd")
        )
    });
}

/// 探测点的安装方式：给 --host 由中控 SSH 登录装（中控用自己的密钥登录 root）；不给就打印一次性安装命令
#[derive(clap::Args)]
pub struct InstallArgs {
    /// 探测点的 IP 或主机名（中控 SSH 连得到的）；不给就打印一次性安装命令（30 分钟内有效、只能用一次），在探测点上用 root 执行
    #[arg(long)]
    pub host: Option<String>,
    /// 探测点的 SSH 端口（默认 22）
    #[arg(long)]
    pub ssh_port: Option<u16>,
    /// 主机指纹和中控记下的不一样时（确认是重装过系统）重新记录
    #[arg(long)]
    pub reset_host_key: bool,
    /// 不等安装结束（默认等到收到首次上报）
    #[arg(long)]
    pub no_wait: bool,
}

fn ssh_body(o: &InstallArgs) -> anyhow::Result<Option<Value>> {
    match o.host.as_deref() {
        Some(h) => Ok(Some(json!({"host": h, "port": o.ssh_port.unwrap_or(22), "reset_host_key": o.reset_host_key}))),
        None if o.ssh_port.is_some() || o.reset_host_key => bail!("--ssh-port / --reset-host-key 要和 --host 一起用"),
        None => Ok(None),
    }
}

pub fn add(ctx: &Ctx, name: &str, note: Option<&str>, o: &InstallArgs) -> anyhow::Result<u8> {
    let mut body = json!({"name": name, "note": note.unwrap_or("")});
    if let Some(s) = ssh_body(o)? {
        body["ssh"] = s;
    }
    let d = ctx.c.post("/probe-points", &body)?;
    let id = st(&d, "/id");
    let Some(host) = o.host.as_deref() else {
        print_bootstrap(ctx, &d, name);
        return Ok(0);
    };
    if o.no_wait {
        ctx.emit(&d, || format!("已新建「{name}」，中控正在登录 {host} 安装：fleetctl probe ls 看进度"));
        return Ok(0);
    }
    follow_install(ctx, &id)
}

/// 重装：给 --host 由中控经 SSH 重装（换新令牌），否则打印新的一次性安装命令
pub fn install(ctx: &Ctx, q: &str, o: &InstallArgs) -> anyhow::Result<u8> {
    let p = find(&ctx.list("/probe-points")?, q)?;
    let (id, name) = (st(&p, "/id"), st(&p, "/name"));
    let mut body = json!({});
    if let Some(s) = ssh_body(o)? {
        body["ssh"] = s;
    }
    let d = ctx.c.post(&format!("/probe-points/{id}/install"), &body)?;
    let Some(host) = o.host.as_deref() else {
        print_bootstrap(ctx, &d, &name);
        return Ok(0);
    };
    if o.no_wait {
        ctx.emit(&d, || format!("中控正在登录 {host} 重装「{name}」：fleetctl probe ls 看进度"));
        return Ok(0);
    }
    follow_install(ctx, &id)
}

pub fn set(ctx: &Ctx, q: &str, enable: Option<bool>, name: Option<&str>, note: Option<&str>) -> anyhow::Result<u8> {
    if enable.is_none() && name.is_none() && note.is_none() {
        bail!("没有要改的：给 --enable / --disable / --name / --note 之一");
    }
    let p = find(&ctx.list("/probe-points")?, q)?;
    let mut body = json!({});
    if let Some(e) = enable {
        body["enabled"] = json!(e);
    }
    if let Some(n) = name {
        body["name"] = json!(n);
    }
    if let Some(n) = note {
        body["note"] = json!(n);
    }
    let d = ctx.c.patch(&format!("/probe-points/{}", st(&p, "/id")), &body)?;
    ctx.emit(&d, || format!("已修改：{} · {} · 备注 {}", st(&d, "/name"), state_cn(&st(&d, "/state")), st(&d, "/note")));
    Ok(0)
}

pub fn rm(ctx: &Ctx, q: &str, yes: bool) -> anyhow::Result<u8> {
    let p = find(&ctx.list("/probe-points")?, q)?;
    let name = st(&p, "/name");
    confirm(&format!("确认删除探测点「{name}」？它的令牌立刻作废，它测的检查记录一起删掉"), yes)?;
    let d = ctx.c.delete(&format!("/probe-points/{}", st(&p, "/id")))?;
    ctx.emit(&d, || format!("已删除「{name}」。探测点机器上的东西中控没有动，不再用它的话在它上面用 root 执行：\n  {}", st(&d, "/cleanup")));
    Ok(0)
}

/// 主探测来源：探测点名称 / id，或 central（中控）
pub fn primary(ctx: &Ctx, q: &str) -> anyhow::Result<u8> {
    let (body, what) = if q.eq_ignore_ascii_case("central") || q == "中控" {
        (json!({"primary": "central", "central_enabled": true}), "中控".to_string())
    } else {
        let p = find(&ctx.list("/probe-points")?, q)?;
        (json!({"primary": st(&p, "/id")}), st(&p, "/label"))
    };
    let d = ctx.c.put("/settings/reach-source", &body)?;
    ctx.emit(&d, || format!("主探测来源改成了{what}。线路机卡片的心跳格与「客户入口连不上」告警都看它。\n{}", source_line(&d)));
    Ok(0)
}

/// 中控自己的检查开 / 关（主来源是中控时不能关）
pub fn central(ctx: &Ctx, on: bool) -> anyhow::Result<u8> {
    let d = ctx.c.put("/settings/reach-source", &json!({"central_enabled": on}))?;
    ctx.emit(&d, || source_line(&d));
    Ok(0)
}

/// 升级：中控记下 dist 里的程序，探测点 30 秒内自己下载替换、重启；等到它报上新版本（最多 3 分钟）
pub fn upgrade(ctx: &Ctx, q: &str, no_wait: bool) -> anyhow::Result<u8> {
    let p = find(&ctx.list("/probe-points")?, q)?;
    let (id, name) = (st(&p, "/id"), st(&p, "/name"));
    let d = ctx.c.post(&format!("/probe-points/{id}/upgrade"), &json!({}))?;
    let to = st(&d, "/version");
    if no_wait {
        ctx.emit(&d, || format!("已让「{name}」升级到 {to}：它下次取目标（30 秒内）时自己下载替换"));
        return Ok(0);
    }
    if !ctx.json {
        eprintln!("等「{name}」自己下载替换（{} → {to}）…", st(&d, "/from"));
    }
    for _ in 0..120 {
        std::thread::sleep(Duration::from_millis(1500));
        let p = ctx.c.get(&format!("/probe-points/{id}"))?;
        if p["upgrade_failed"]["sha256"] == d["sha256"] {
            bail!("「{name}」{}：新版本没能上报，探测点自己换回了旧版本。在它上面看日志（OpenRC：tail /var/log/fleet-probe.log；systemd：journalctl -u fleet-probe），修好后发新版本（换掉中控 dist 里的 fleet-agent）再 probe upgrade：同一个版本它不会再换，中控也会拒绝", upgrade_failed(&p).unwrap_or_default());
        }
        if p["upgrade"].is_null() && p["state"] == "online" {
            ctx.emit(&p, || format!("「{name}」已升级到 {}", st(&p, "/version")));
            return Ok(0);
        }
    }
    bail!("3 分钟内「{name}」没换上新版本：在它上面看日志（OpenRC：tail /var/log/fleet-probe.log；systemd：journalctl -u fleet-probe）。升级请求还留着；下载后 5 分钟还没报上新版本，或探测点报告这个版本之前换上后跑不起来过，中控记成升级失败、不再下发（probe ls 里看）")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probes() -> Vec<Value> {
        vec![
            json!({"id": "01a162a9-39c4-752c-b647-e61f34ef48bd", "name": "香港", "state": "online"}),
            json!({"id": "01a162a9-4000-752c-b647-e61f34ef48bd", "name": "东京", "state": "offline"}),
        ]
    }

    #[test]
    fn find_by_id_prefix_and_name() {
        let all = probes();
        assert_eq!(find(&all, "东京").unwrap()["state"], "offline");
        assert!(find(&all, "01a162a9").unwrap_err().to_string().contains("匹配到 2 个探测点"));
    }

    #[test]
    fn list_row_wording() {
        let p = json!({
            "id": "01a162a9-39c4-752c-b647-e61f34ef48bd", "name": "香港", "primary": true, "state": "online", "last_seen_at": "2026-10-22T08:10:54Z",
            "version": "0.1.0+abc", "last_ip": "47.0.0.1", "install": {"state": "done"}, "upgrade": null,
            "relays": [{"name": "线路机1", "avg_ms": 158.4}, {"name": "线路机2", "avg_ms": null}]
        });
        assert_eq!(row(&p), ["香港", "是", "在线", "2026-10-22 16:10:54", "0.1.0+abc", "47.0.0.1", "线路机1 158 ms，线路机2 -", "01a162a9-39c4"]);
        let q = json!({"id": "x", "name": "东京", "state": "pending", "install": {"state": "failed"}, "upgrade": {"sha256": "a"}, "relays": []});
        assert_eq!(row(&q)[1..3], ["", "等待安装（安装失败）（升级中）"]);
        let b = json!({"id": "y", "name": "新加坡", "state": "blind", "relays": []});
        assert_eq!(row(&b)[2], "在线（探测无效）");
        let f = json!({"id": "z", "name": "首尔", "state": "online", "upgrade": null, "upgrade_failed": {"sha256": "a", "to": "0.2.0+new", "from": "0.1.0+old", "at": "2026-10-22T08:10:54Z"}, "relays": []});
        assert_eq!(row(&f)[2], "在线（升级到 0.2.0+new 失败，已回到 0.1.0+old）");
    }

    #[test]
    fn ssh_options_need_host() {
        let o = InstallArgs { host: None, ssh_port: Some(2222), reset_host_key: false, no_wait: false };
        assert!(ssh_body(&o).is_err());
        let o = InstallArgs { host: Some("47.0.0.1".into()), ssh_port: None, ..o };
        assert_eq!(ssh_body(&o).unwrap().unwrap(), json!({"host": "47.0.0.1", "port": 22, "reset_host_key": false}));
    }
}
