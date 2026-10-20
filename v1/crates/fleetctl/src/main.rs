//! fleetctl：团队线路管理的运维命令行。人看表格，AI 加 `--json` 拿原样 JSON。
//! 退出码：0 成功；1 失败（错误信息在 stderr）；2 命令行参数不对。exec 单台时沿用远端命令的退出码。

mod access;
mod client;
mod cmds;
mod config;
mod ops;
mod resolve;
mod table;

use clap::{Args, Parser, Subcommand};
use client::{ApiError, Client, Cred};
use cmds::Ctx;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "fleetctl",
    version,
    about = "团队线路管理 运维命令行",
    after_help = "连接配置：环境变量 FLEET_URL、FLEET_TOKEN，或 `fleetctl login` 写入的配置文件。\n<机器> 可以写 机器 id、显示名（团队1-06）、线路机名、IP。\n加 --json 输出 API 返回的 data 原样 JSON；错误一律走 stderr 且退出码非 0。"
)]
struct Cli {
    /// 输出原样 JSON（给脚本和 AI）
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args)]
struct KeyAuthArgs {
    /// 用这个用户名登录会话（密钥管理只认会话；密码来自 FLEET_PASSWORD 或 --password-stdin）
    #[arg(long, env = "FLEET_USER")]
    user: Option<String>,
    /// 从标准输入读密码
    #[arg(long)]
    password_stdin: bool,
}

#[derive(Subcommand)]
enum KeysCmd {
    /// 列出 API 密钥
    Ls {
        #[command(flatten)]
        auth: KeyAuthArgs,
    },
    /// 创建 API 密钥（token 只显示一次）
    Create {
        name: String,
        /// 权限，逗号分隔：read,write,secrets,exec
        #[arg(long, default_value = "read")]
        scopes: String,
        /// 有效天数（不填则永不过期）
        #[arg(long)]
        days: Option<i64>,
        #[command(flatten)]
        auth: KeyAuthArgs,
    },
    /// 撤销密钥（按名称、前缀或 id）
    Revoke {
        which: String,
        #[command(flatten)]
        auth: KeyAuthArgs,
    },
}

#[derive(Args)]
struct ProvisionArgs {
    /// 批量文件（JSON 数组或 {"items":[…]}；条目里 team/relay/provider/plan 可直接写名字）
    #[arg(long)]
    file: Option<PathBuf>,
    /// vps | proxy | relay
    #[arg(long)]
    kind: Option<String>,
    #[arg(long)]
    ip: Option<String>,
    #[arg(long)]
    ssh_port: Option<u16>,
    /// 团队名（节点）
    #[arg(long)]
    team: Option<String>,
    /// 团队内编号（不填取最小空号）
    #[arg(long)]
    number: Option<i32>,
    /// 线路机名（relay）
    #[arg(long)]
    label: Option<String>,
    /// 节点要挂的线路机
    #[arg(long)]
    relay: Option<String>,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    plan: Option<String>,
    #[arg(long)]
    service_ref: Option<String>,
    #[arg(long)]
    note: Option<String>,
    /// vps 的 SS 端口
    #[arg(long)]
    ss_port: Option<u16>,
    /// proxy 的上游：socks5://用户:密码@主机:端口
    #[arg(long)]
    upstream: Option<String>,
    /// 首次登录用的 root 密码放在哪个环境变量里（不填则用中控的密钥）
    #[arg(long)]
    ssh_password_env: Option<String>,
    /// 到期日 YYYY-MM-DD
    #[arg(long)]
    due: Option<String>,
    #[arg(long)]
    price_cents: Option<i64>,
    /// 只校验，不提交
    #[arg(long)]
    dry_run: bool,
    /// 跳过确认
    #[arg(long)]
    yes: bool,
    /// 提交后跟随作业日志直到结束
    #[arg(long)]
    follow: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// 保存中控地址与 API 密钥到配置文件
    Login {
        #[arg(long)]
        url: String,
        #[arg(long)]
        token: String,
    },
    /// 当前凭据的身份与权限
    Whoami,
    /// 总览 + 进行中的告警
    Status,
    /// 机器列表
    Ls {
        #[arg(long)]
        team: Option<String>,
        /// relay | vps | proxy
        #[arg(long)]
        kind: Option<String>,
        /// 健康状态（ok/degraded/down/unknown/maintenance）或生命周期（active/provisioning/…）
        #[arg(long)]
        status: Option<String>,
        /// 只看已退役
        #[arg(long)]
        retired: bool,
    },
    /// 机器详情
    Show { machine: String },
    /// 时序指标
    Series {
        machine: String,
        /// 1h | 6h | 24h | 7d | 30d
        #[arg(long, default_value = "24h")]
        range: String,
        /// host | traffic | probe
        #[arg(long, default_value = "host")]
        metric: String,
    },
    /// 告警
    Incidents {
        /// 含已恢复的
        #[arg(long)]
        all: bool,
    },
    /// 确认告警
    Ack { id: String },
    /// 作业列表
    Jobs {
        #[arg(long)]
        active: bool,
        #[arg(long, default_value_t = 30)]
        limit: i64,
    },
    /// 作业详情；--follow 实时跟日志，结束时按成败给退出码
    Job {
        id: String,
        #[arg(long)]
        follow: bool,
    },
    /// 重试失败的作业
    Retry { id: String },
    /// 取消进行中的作业
    Cancel { id: String },
    /// 上架：先 check 再提交（单台参数或 --file 批量）
    Provision(ProvisionArgs),
    /// 退役一台机器（危险）
    Retire {
        machine: String,
        #[arg(long)]
        reason: Option<String>,
        /// 不清理机器上的服务
        #[arg(long)]
        no_cleanup: bool,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        follow: bool,
    },
    /// 彻底删除已退役的机器（危险，不可恢复）
    Purge {
        machine: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        follow: bool,
    },
    /// 改名 = 改团队/编号/线路机名/备注
    Rename {
        machine: String,
        #[arg(long)]
        team: Option<String>,
        #[arg(long)]
        number: Option<i32>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// 团队编号压缩成连续；不加 --yes 只预览
    Renumber {
        team: String,
        #[arg(long)]
        yes: bool,
    },
    /// 升级 agent（单台，或 --all）
    UpgradeAgent {
        machine: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        follow: bool,
    },
    /// 入口列表（可只看某台机器相关的）
    Entries { machine: Option<String> },
    /// 输出入口的分享链接（vless://…；需要 secrets 权限，记审计）
    Share {
        /// 入口 id、`线路机名:端口` 或节点显示名
        entry: String,
    },
    /// 入口启用/停用/轮换凭据
    Entry {
        /// enable | disable | rotate
        action: String,
        entry: String,
        #[arg(long)]
        yes: bool,
    },
    /// 在机器上执行命令（需要 exec 权限，全程审计）。多台：逗号分隔或 --team，并发 4。
    /// 单个参数原样当脚本（可含管道）；多个参数会逐个加引号。
    Exec {
        /// 机器，可逗号分隔多台
        machine: Option<String>,
        #[arg(long)]
        team: Option<String>,
        /// 超时秒数（1-600）
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        /// 从文件读脚本
        #[arg(long)]
        script: Option<PathBuf>,
        /// `--` 之后是要执行的命令
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// 机器上的服务日志（journalctl）
    Logs {
        machine: String,
        /// fleet-agent | fleet-xray | ssserver
        #[arg(long, default_value = "fleet-agent")]
        unit: String,
        #[arg(long, default_value_t = 200)]
        lines: u32,
    },
    /// 中控自身日志
    ServerLogs {
        #[arg(long, default_value_t = 200)]
        lines: u32,
    },
    /// API 密钥管理
    Keys {
        #[command(subcommand)]
        cmd: KeysCmd,
    },
    /// 审计日志
    Audit {
        /// 只看某台机器
        #[arg(long)]
        target: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// 访问监控：哪个节点访问了哪些域名/IP
    Access {
        /// summary（按节点汇总）| targets（域名与 IP）| new（新出现的目标）| sources（来源地址）| events（原始记录）
        #[arg(default_value = "targets")]
        view: String,
        /// 只看某台机器（id / 显示名 / IP）
        #[arg(long)]
        node: Option<String>,
        #[arg(long)]
        team: Option<String>,
        /// 1h | 24h | 7d | 30d（events 固定取最近记录）
        #[arg(long, default_value = "24h")]
        window: String,
        /// 搜索域名（events / sources 也匹配来源 IP）
        #[arg(long)]
        search: Option<String>,
        /// 只看被阻止的连接
        #[arg(long)]
        blocked: bool,
        /// domain | ip
        #[arg(long)]
        kind: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// 订阅实时事件流
    Watch {
        /// 收到 N 条后退出
        #[arg(long)]
        count: Option<usize>,
        /// 也显示 30 秒一次的 tick
        #[arg(long)]
        ticks: bool,
    },
}

fn make_ctx(json: bool) -> anyhow::Result<Ctx> {
    let (url, token) = config::load()?;
    Ok(Ctx { c: Client::new(&url, Cred::Bearer(token))?, base: url.trim_end_matches('/').to_string(), json })
}

fn login(url: &str, token: &str, json: bool) -> anyhow::Result<u8> {
    let c = Client::new(url, Cred::Bearer(token.to_string()))?;
    // 先验证再保存，免得存进去一把用不了的
    let me = c.get("/auth/me")?;
    let p = config::save(url.trim_end_matches('/'), token)?;
    if json {
        cmds::outln(serde_json::json!({"saved": p.display().to_string(), "identity": me}).to_string());
    } else {
        cmds::outln(format!("已保存到 {}（身份 {}，权限 {}）", p.display(), cmds::st(&me, "/username"), me["scopes"].as_array().map(|a| a.iter().filter_map(|s| s.as_str()).collect::<Vec<_>>().join(",")).unwrap_or_default()));
    }
    Ok(0)
}

fn run(cli: Cli) -> anyhow::Result<u8> {
    let json = cli.json;
    if let Cmd::Login { url, token } = &cli.cmd {
        return login(url, token, json);
    }
    let ctx = make_ctx(json)?;
    let x = &ctx;
    match cli.cmd {
        Cmd::Login { .. } => unreachable!(),
        Cmd::Whoami => cmds::whoami(x),
        Cmd::Status => cmds::status(x),
        Cmd::Ls { team, kind, status, retired } => cmds::ls(x, team.as_deref(), kind.as_deref(), status.as_deref(), retired),
        Cmd::Show { machine } => cmds::show(x, &machine),
        Cmd::Series { machine, range, metric } => cmds::series(x, &machine, &range, &metric),
        Cmd::Incidents { all } => cmds::incidents(x, all),
        Cmd::Ack { id } => cmds::ack(x, &id),
        Cmd::Jobs { active, limit } => cmds::jobs(x, active, limit),
        Cmd::Job { id, follow } => cmds::job(x, &id, follow),
        Cmd::Retry { id } => cmds::job_action(x, &id, "retry"),
        Cmd::Cancel { id } => cmds::job_action(x, &id, "cancel"),
        Cmd::Provision(a) => ops::provision(
            x,
            ops::ProvisionOpts {
                file: a.file,
                kind: a.kind,
                ip: a.ip,
                ssh_port: a.ssh_port,
                team: a.team,
                number: a.number,
                label: a.label,
                relay: a.relay,
                provider: a.provider,
                plan: a.plan,
                service_ref: a.service_ref,
                note: a.note,
                ss_port: a.ss_port,
                upstream: a.upstream,
                ssh_password_env: a.ssh_password_env,
                due: a.due,
                price_cents: a.price_cents,
                dry_run: a.dry_run,
                yes: a.yes,
                follow: a.follow,
            },
        ),
        Cmd::Retire { machine, reason, no_cleanup, yes, follow } => ops::retire(x, &machine, reason.as_deref(), no_cleanup, yes, follow),
        Cmd::Purge { machine, yes, follow } => ops::purge(x, &machine, yes, follow),
        Cmd::Rename { machine, team, number, label, note } => ops::rename(x, &machine, team.as_deref(), number, label.as_deref(), note.as_deref()),
        Cmd::Renumber { team, yes } => ops::renumber(x, &team, yes),
        Cmd::UpgradeAgent { machine, all, yes, follow } => ops::upgrade_agent(x, machine.as_deref(), all, yes, follow),
        Cmd::Entries { machine } => cmds::entries(x, machine.as_deref()),
        Cmd::Share { entry } => ops::share(x, &entry),
        Cmd::Entry { action, entry, yes } => ops::entry_action(x, &action, &entry, yes),
        Cmd::Exec { machine, team, timeout, script, command } => ops::exec(x, ops::ExecOpts { machine: machine.as_deref(), team: team.as_deref(), timeout, script: script.as_deref(), command: &command }),
        Cmd::Logs { machine, unit, lines } => cmds::logs(x, &machine, &unit, lines),
        Cmd::ServerLogs { lines } => cmds::server_logs(x, lines),
        Cmd::Keys { cmd } => {
            let ka = |a: KeyAuthArgs| ops::KeyAuth { user: a.user, password_stdin: a.password_stdin };
            match cmd {
                KeysCmd::Ls { auth } => ops::keys_ls(x, &ka(auth)),
                KeysCmd::Create { name, scopes, days, auth } => ops::keys_create(x, &ka(auth), &name, &scopes, days),
                KeysCmd::Revoke { which, auth } => ops::keys_revoke(x, &ka(auth), &which),
            }
        }
        Cmd::Audit { target, limit } => cmds::audit(x, target.as_deref(), limit),
        Cmd::Access { view, node, team, window, search, blocked, kind, limit } => access::access(
            x,
            access::AccessOpts { view: &view, node: node.as_deref(), team: team.as_deref(), window: &window, search: search.as_deref(), blocked, kind: kind.as_deref(), limit },
        ),
        Cmd::Watch { count, ticks } => cmds::watch(x, count, ticks),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            if json {
                let (code, status) = match e.downcast_ref::<ApiError>() {
                    Some(a) => (a.code.clone(), Some(a.status)),
                    None => ("error".to_string(), None),
                };
                let msg = match e.downcast_ref::<ApiError>() {
                    Some(a) if !e.to_string().contains('\n') => a.message.clone(),
                    _ => format!("{e:#}"),
                };
                eprintln!("{}", serde_json::json!({"error": {"code": code, "message": msg, "status": status}}));
            } else {
                eprintln!("错误：{e:#}");
            }
            ExitCode::from(1)
        }
    }
}
