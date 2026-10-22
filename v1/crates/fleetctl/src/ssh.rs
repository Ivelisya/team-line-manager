//! `fleetctl ssh`：在中控上用中控自己的 SSH 密钥直接登录机器或软路由（交互式，或 `--` 后带一条命令），
//! 进程整个换成 ssh（终端、信号、退出码都是 ssh 的）。只在中控上能用：私钥只在那里，root 才读得了。
//! 主机指纹只认中控记下的（StrictHostKeyChecking=yes）；登录前经 API 记一条审计（哪把密钥、哪台、带没带命令，命令内容不记），
//! 记不上就不登录。ssh 选项同 fleet-server ssh.rs 的 Target：不读 ~/.ssh/config 与系统 known_hosts，要用的选项全在这里给出。

use crate::client::ApiError;
use crate::cmds::Ctx;
use crate::resolve::Want;
use anyhow::{anyhow, bail, Context};
use serde_json::{json, Value};
use std::path::Path;

/// 中控登录机器 / 软路由的私钥与记下的主机指纹（同 deploy/central/install.sh 与 fleet.toml 里的路径）
pub const KEY: &str = "/etc/fleet/ssh/id_ed25519";
pub const KNOWN_HOSTS: &str = "/var/lib/fleet/known_hosts";
/// 软路由的主机指纹：经反向隧道第一次连上时中控记下，主机字段是别名 router-<id>（隧道都在 127.0.0.1 上，只能按别名区分）
pub const ROUTER_KNOWN_HOSTS: &str = "/var/lib/fleet/router_known_hosts";

#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    Machine(String),
    Router(String),
}

/// `ssh <机器>` / `ssh router <名称>`
pub fn parse_target(args: &[String]) -> anyhow::Result<Target> {
    match args {
        [m] => Ok(Target::Machine(m.clone())),
        [r, name] if r == "router" => Ok(Target::Router(name.clone())),
        _ => bail!("用法：fleetctl ssh <机器> [-- 命令…]，或 fleetctl ssh router <名称> [-- 命令…]"),
    }
}

/// 读不到中控的私钥：不在中控上，或不是 root
pub fn check_key(path: &Path) -> anyhow::Result<()> {
    match std::fs::File::open(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("这台机器上没有中控的 SSH 密钥 {}：fleetctl ssh 只能在中控上用。先 ssh 进中控（root），在那里运行同样的命令", path.display())
        }
        Err(e) => bail!("读不了中控的 SSH 密钥 {}（{e}）：fleetctl ssh 要以 root 在中控上运行（ssh root@中控，或 sudo -i）", path.display()),
    }
}

/// 机器与软路由共用的选项：只用中控的密钥、只认给定的 known_hosts，主机指纹不符或没有记录就拒绝
fn common(known_hosts: &str) -> Vec<String> {
    let mut a: Vec<String> = ["-F", "/dev/null", "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes", "-i", KEY].map(String::from).to_vec();
    for o in ["StrictHostKeyChecking=yes", "UpdateHostKeys=no", "ConnectTimeout=10", "ServerAliveInterval=10", "ServerAliveCountMax=3", "LogLevel=ERROR", "GlobalKnownHostsFile=/dev/null"] {
        a.extend(["-o".to_string(), o.to_string()]);
    }
    a.extend(["-o".to_string(), format!("UserKnownHostsFile={known_hosts}")]);
    a
}

/// `--` 把选项与目标分开：命令以 - 开头也不会被 ssh 当成选项
fn finish(mut a: Vec<String>, port: u16, dest: String, command: Option<&str>) -> Vec<String> {
    a.extend(["-p".to_string(), port.to_string(), "--".to_string(), dest]);
    a.extend(command.map(String::from));
    a
}

/// 登录机器：中控密钥 + 中控 known_hosts
pub fn machine_args(ip: &str, port: u16, command: Option<&str>) -> Vec<String> { finish(common(KNOWN_HOSTS), port, format!("root@{ip}"), command) }

/// 登录软路由：经它连到中控的反向隧道（127.0.0.1:<隧道端口>），主机指纹按别名 router-<id> 查
pub fn router_args(id: &str, tunnel_port: u16, command: Option<&str>) -> Vec<String> {
    let mut a = common(ROUTER_KNOWN_HOSTS);
    a.extend(["-o".to_string(), format!("HostKeyAlias=router-{id}")]);
    finish(a, tunnel_port, "root@127.0.0.1".into(), command)
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str { v.get(k).and_then(Value::as_str).unwrap_or("") }

fn port_of(v: &Value, k: &str) -> Option<u16> { v.get(k).and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok()) }

/// 机器（完整视图）的 SSH 地址；同机 IP 节点没有自己的 SSH，用宿主机的（`host` 是宿主机的完整视图，同中控的远程执行）
pub fn endpoint(m: &Value, host: Option<&Value>) -> anyhow::Result<(String, u16)> {
    let v = host.unwrap_or(m);
    let ip = Some(s(v, "ip")).filter(|ip| !ip.is_empty()).ok_or_else(|| anyhow!("{} 没有 IP", s(v, "name")))?;
    let port = port_of(v, "ssh_port").ok_or_else(|| anyhow!("{} 没有 SSH 端口", s(v, "name")))?;
    Ok((ip.to_string(), port))
}

/// 按 id（可前缀，至少 6 位）或名称找软路由；歧义时报错并列出候选
pub fn find_router<'a>(list: &'a [Value], q: &str) -> Result<&'a Value, String> {
    let q = q.trim();
    let ql = q.to_lowercase();
    let id = |r: &Value| s(r, "id").to_lowercase();
    let mut hits: Vec<&Value> = list.iter().filter(|r| id(r) == ql || s(r, "name") == q).collect();
    if hits.is_empty() && q.len() >= 6 && q.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        hits = list.iter().filter(|r| id(r).starts_with(&ql)).collect();
    }
    match hits.len() {
        1 => Ok(hits[0]),
        0 if list.is_empty() => Err(format!("没有软路由「{q}」：中控上还没有接入任何软路由")),
        0 => Err(format!("没有软路由「{q}」，现有：{}", list.iter().map(|r| s(r, "name")).collect::<Vec<_>>().join("、"))),
        n => Err(format!("「{q}」匹配到 {n} 台软路由，请用 id 指定：\n{}", hits.iter().map(|r| format!("  {}  (id {})", s(r, "name"), s(r, "id"))).collect::<Vec<_>>().join("\n"))),
    }
}

/// 登录前记审计（POST /audit/ssh）；记不上就不登录，同 exec「先有记录再动手」
fn audit(ctx: &Ctx, kind: &str, id: &str, command: bool) -> anyhow::Result<()> {
    ctx.c.post("/audit/ssh", &json!({"kind": kind, "id": id, "command": command})).map(|_| ()).context("登录前写审计失败，没有登录")
}

/// `target` 是 `ssh` 后面、`--` 之前的参数；`command` 是 `--` 之后的（空 = 交互式）
pub fn run(ctx: &Ctx, target: &[String], command: &[String]) -> anyhow::Result<u8> {
    let target = parse_target(target)?;
    // 同 exec：一个参数原样当脚本，多个参数逐个加引号
    let command = match command {
        [] => None,
        c => {
            let c = crate::ops::join_command(c);
            if c.trim().is_empty() {
                bail!("`--` 后面的命令是空的");
            }
            Some(c)
        }
    };
    let args = match target {
        Target::Machine(q) => {
            let (id, name) = ctx.find_id(&q, Want::Live)?;
            let m = ctx.c.get(&format!("/machines/{id}"))?;
            if s(&m, "kind") == "proxy" {
                bail!("{name} 是家宽代理，没有 SSH");
            }
            let host = m["host_machine"]["id"].as_str().map(|h| ctx.c.get(&format!("/machines/{h}"))).transpose()?;
            let (ip, port) = endpoint(&m, host.as_ref())?;
            if let (Some(h), false) = (&host, ctx.json) {
                eprintln!("{name} 是同机 IP 节点，登录的是宿主机 {}（{ip}）", s(h, "name"));
            }
            audit(ctx, "machine", &id, command.is_some())?;
            machine_args(&ip, port, command.as_deref())
        }
        Target::Router(q) => {
            let list = ctx.c.get("/routers").map_err(|e| match e.downcast_ref::<ApiError>() {
                Some(a) if a.status == 404 => anyhow!("这个中控还不能管理软路由（GET /routers 404），先升级中控"),
                _ => e,
            })?;
            let r = find_router(list.as_array().map(Vec::as_slice).unwrap_or_default(), &q).map_err(|e| anyhow!(e))?;
            let (id, name) = (s(r, "id").to_string(), s(r, "name").to_string());
            let port = port_of(r, "tunnel_port").ok_or_else(|| anyhow!("软路由「{name}」没有隧道端口"))?;
            match s(r, "state") {
                "pending" => bail!("软路由「{name}」还没接入过：中控还没记下它的主机指纹，在路由上执行接入命令、等面板显示在线后再登录"),
                "offline" if !ctx.json => eprintln!("软路由「{name}」现在离线（隧道没连上），多半登录不了"),
                _ => {}
            }
            audit(ctx, "router", &id, command.is_some())?;
            router_args(&id, port, command.as_deref())
        }
    };
    hand_over(args)
}

/// 把当前进程换成 ssh：交互式时终端直接交给它，退出码就是 ssh 的（带命令时 = 远端命令的，ssh 自己失败是 255）
#[cfg(unix)]
fn hand_over(args: Vec<String>) -> anyhow::Result<u8> {
    use std::os::unix::process::CommandExt;
    let e = std::process::Command::new("ssh").args(&args).exec();
    bail!("启动 ssh 失败：{e}（中控上要有 openssh-client）")
}

/// 只在中控（Linux）上用；这里只为能编译，走不到（找不到中控的私钥就先报错了）
#[cfg(not(unix))]
fn hand_over(args: Vec<String>) -> anyhow::Result<u8> {
    let st = std::process::Command::new("ssh").args(&args).status().context("启动 ssh")?;
    Ok(st.code().map_or(255, |c| c.clamp(0, 255) as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> { a.iter().map(|s| s.to_string()).collect() }

    /// 选项 `-o K=V` 的值
    fn opt<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
        args.windows(2).find_map(|w| (w[0] == "-o").then(|| w[1].strip_prefix(&format!("{key}="))).flatten())
    }

    #[test]
    fn parses_machine_and_router_targets() {
        assert_eq!(parse_target(&v(&["团队1-03"])).unwrap(), Target::Machine("团队1-03".into()));
        assert_eq!(parse_target(&v(&["router", "团队2-路由A"])).unwrap(), Target::Router("团队2-路由A".into()));
        assert!(parse_target(&v(&["团队1-03", "多余"])).unwrap_err().to_string().contains("用法"));
        assert!(parse_target(&[]).is_err());
    }

    #[test]
    fn machine_args_pin_central_key_and_known_hosts() {
        let a = machine_args("203.0.113.5", 2222, Some("uname -a"));
        assert_eq!(a[..2], v(&["-F", "/dev/null"]));
        assert!(a.windows(2).any(|w| w == v(&["-i", KEY])));
        assert_eq!(opt(&a, "StrictHostKeyChecking"), Some("yes"));
        assert_eq!(opt(&a, "UserKnownHostsFile"), Some(KNOWN_HOSTS));
        assert_eq!(opt(&a, "GlobalKnownHostsFile"), Some("/dev/null"));
        assert_eq!(opt(&a, "BatchMode"), Some("yes"));
        assert_eq!(opt(&a, "HostKeyAlias"), None);
        // 端口、--、目标、命令（一个参数）在最后
        assert_eq!(a[a.len() - 5..], v(&["-p", "2222", "--", "root@203.0.113.5", "uname -a"]));
        // 交互式：没有命令
        let a = machine_args("203.0.113.5", 22, None);
        assert_eq!(a[a.len() - 4..], v(&["-p", "22", "--", "root@203.0.113.5"]));
    }

    #[test]
    fn ip_node_logs_into_its_host() {
        let host = json!({"name": "团队1-03", "ip": "198.51.100.3", "ssh_port": 2222, "host_machine": null});
        let node = json!({"name": "团队1-52", "ip": "198.51.100.66", "ssh_port": 22, "host_machine": {"id": "h", "name": "团队1-03"}});
        let (ip, port) = endpoint(&node, Some(&host)).unwrap();
        assert_eq!((ip.as_str(), port), ("198.51.100.3", 2222), "同机 IP 节点连宿主机的地址和端口");
        let a = machine_args(&ip, port, Some("ip -4 addr"));
        assert_eq!(a[a.len() - 5..], v(&["-p", "2222", "--", "root@198.51.100.3", "ip -4 addr"]));
        assert_eq!(endpoint(&host, None).unwrap(), ("198.51.100.3".to_string(), 2222));
        assert!(endpoint(&json!({"name": "x", "ip": "1.2.3.4"}), None).unwrap_err().to_string().contains("没有 SSH 端口"));
    }

    #[test]
    fn router_args_use_tunnel_and_alias() {
        let id = "0193a5b0-0000-7000-8000-000000000001";
        let a = router_args(id, 23001, Some("logread | tail -20"));
        assert_eq!(opt(&a, "HostKeyAlias"), Some("router-0193a5b0-0000-7000-8000-000000000001"));
        assert_eq!(opt(&a, "UserKnownHostsFile"), Some(ROUTER_KNOWN_HOSTS));
        assert_eq!(opt(&a, "StrictHostKeyChecking"), Some("yes"));
        assert!(a.windows(2).any(|w| w == v(&["-i", KEY])));
        assert_eq!(a[a.len() - 5..], v(&["-p", "23001", "--", "root@127.0.0.1", "logread | tail -20"]));
    }

    #[test]
    fn finds_router_by_name_or_id() {
        let rs = vec![
            json!({"id": "0193a5b0-0000-7000-8000-000000000001", "name": "团队2-路由A", "tunnel_port": 23001, "state": "online"}),
            json!({"id": "0193a5b0-0000-7000-8000-000000000002", "name": "团队2-路由B", "tunnel_port": 23002, "state": "pending"}),
        ];
        assert_eq!(find_router(&rs, "团队2-路由B").unwrap()["tunnel_port"], 23002);
        assert_eq!(find_router(&rs, "0193A5B0-0000-7000-8000-000000000001").unwrap()["name"], "团队2-路由A");
        assert!(find_router(&rs, "0193a5b0").unwrap_err().contains("匹配到 2 台"));
        assert!(find_router(&rs, "团队1-路由").unwrap_err().contains("现有：团队2-路由A、团队2-路由B"));
        assert!(find_router(&[], "x").unwrap_err().contains("还没有接入任何软路由"));
    }

    #[test]
    fn missing_key_says_to_ssh_into_central_first() {
        let e = check_key(Path::new("/nonexistent/fleet/ssh/id_ed25519")).unwrap_err().to_string();
        assert!(e.contains("只能在中控上用") && e.contains("先 ssh 进中控") && e.contains("/nonexistent/fleet/ssh/id_ed25519"), "{e}");
        let f = std::env::temp_dir().join(format!("fleetctl-key-{}", std::process::id()));
        std::fs::write(&f, "k").unwrap();
        assert!(check_key(&f).is_ok());
        let _ = std::fs::remove_file(&f);
    }
}
