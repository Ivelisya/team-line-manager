//! fleetctl：团队线路管理的运维命令行。人看表格，AI 加 `--json` 拿 JSON。
//! 退出码：0 成功；1 失败（错误信息在 stderr）；2 命令行参数不对。exec 单台时沿用远端命令的退出码。

mod access;
mod billing;
mod client;
mod cmds;
mod config;
mod ipintel;
mod lifecycle;
mod ops;
mod policy;
mod resolve;
mod silence;
mod table;

use clap::{Args, Parser, Subcommand};
use client::{Client, Cred};
use cmds::Ctx;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "fleetctl",
    version,
    about = "团队线路管理 运维命令行",
    after_help = "连接配置：环境变量 FLEET_URL、FLEET_TOKEN，或 `fleetctl login` 写入的配置文件。\n<机器> 可以写 机器 id、显示名（团队1-06）、线路机名、IP。\n加 --json 输出一行 JSON：ls / entries / incidents / policy ls / silence 是与表格同列的精简对象（单台完整视图用 show、policy show），billing 是订阅、流量额度与最近续费记录合成的对象，其余是 API 返回的 data 原样；错误一律走 stderr 且退出码非 0。"
)]
struct Cli {
    /// 输出 JSON（给脚本和 AI）
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

#[derive(Subcommand)]
enum PolicyCmd {
    /// 所有团队/节点的访问策略：生效值与草稿
    Ls,
    /// 某个团队或节点的策略详情
    Show {
        /// 团队名，或节点（显示名 / id / IP）
        who: String,
    },
    /// 设置草稿（不生效，publish 才应用到线路机）。不写 --domains / --note 就沿用现有的
    Set {
        who: String,
        /// observe | allowlist | denylist | blocked | inherit（仅节点）
        #[arg(long)]
        mode: String,
        /// 域名，逗号或空白分隔；`=example.com` 为精确匹配，否则含子域
        #[arg(long)]
        domains: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// 模拟：草稿生效后最近 24 小时每个节点会被阻止多少连接
    Preview,
    /// 发布全部草稿（先预览，再确认）
    Publish {
        #[arg(long)]
        yes: bool,
    },
    /// 丢弃全部草稿
    Discard {
        #[arg(long)]
        yes: bool,
    },
    /// 发布记录（最近 30 个）与各线路机的应用状态
    Versions,
    /// 把历史版本作为新版本发布
    Rollback {
        /// 版本号（3 或 v3）或版本 id
        version: String,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum SilenceCmd {
    /// 进行中与已预约的静默（含维护窗口），进行中的在前
    Ls,
    /// 新建静默：一台机器，或 --all（全部机器，要 --yes）。--start 预约开始时间
    Add {
        /// 机器（id / 显示名 / 线路机名 / IP）
        machine: Option<String>,
        /// 静默全部机器
        #[arg(long)]
        all: bool,
        /// 只静默这条告警规则（规则 key，如 agent_offline；不填 = 全部规则）
        #[arg(long)]
        rule: Option<String>,
        /// 开始时间，北京时间（同表格）：2026-10-23 02:00（别的时区写带时区的 2026-10-23T02:00:00+09:00）；不填 = 现在，最多提前 90 天
        #[arg(long)]
        start: Option<String>,
        /// 时长：30m / 4h / 2d，可组合（1h30m）；最长 30 天
        #[arg(long = "for", value_name = "DURATION")]
        dur: String,
        /// 标成维护：卡片显示「维护中」，期间暂停健康判定（针对整台机器，不能和 --rule 一起用）
        #[arg(long)]
        maintenance: bool,
        #[arg(long)]
        comment: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// 删除静默（id 前缀，见 silence ls 的 id 列）：进行中的提前结束、告警即刻恢复，已预约的取消
    Rm { id: String },
}

#[derive(Subcommand)]
enum SubscriptionCmd {
    /// 改订阅：先读现有的，只改给了的字段再整份提交；还没有订阅时新建，要给 --price 和 --due
    Set {
        machine: String,
        /// 价格，如 39.46（币种的主单位，最多两位小数）
        #[arg(long)]
        price: Option<String>,
        /// 币种代码，如 USD / CNY（新建默认 USD）
        #[arg(long)]
        currency: Option<String>,
        /// 计费周期：monthly | quarterly | semiannual | yearly（新建默认 monthly）
        #[arg(long)]
        cycle: Option<String>,
        /// 到期日 YYYY-MM-DD（账单时区的日期）。续费请用 renew，会留续费记录
        #[arg(long)]
        due: Option<String>,
        /// 到期自动续费：true | false（只影响提醒：自动续费的到期前不提醒，过期才告警；不会真的扣款）
        #[arg(long)]
        auto_renew: Option<bool>,
        /// 到期前几天开始提醒（0–60，新建默认 7）
        #[arg(long)]
        remind_days: Option<i64>,
        /// 账单时区（IANA 名，新建默认 Asia/Shanghai）：到期与流量周期按它的日期算
        #[arg(long)]
        tz: Option<String>,
        /// 流量上限 GiB（0 = 不限）；none = 不单独设，按套餐（同机 IP 节点 = 与宿主机共用）
        #[arg(long)]
        traffic_gb: Option<String>,
        /// 流量每月几号重置（1–31，小月没有这一天按月末；新建默认 1）；none = 不设，按北京时间的自然月
        #[arg(long)]
        reset_day: Option<String>,
        #[arg(long)]
        yes: bool,
    },
}

fn keys_user(c: &KeysCmd) -> Option<&str> {
    match c {
        KeysCmd::Ls { auth } | KeysCmd::Create { auth, .. } | KeysCmd::Revoke { auth, .. } => auth.user.as_deref(),
    }
}

#[derive(Args)]
struct ProvisionArgs {
    /// 批量文件（JSON 数组或 {"items":[…]}；条目里 team/relay/provider/plan/host 可直接写名字）
    #[arg(long)]
    file: Option<PathBuf>,
    /// vps | proxy | relay
    #[arg(long)]
    kind: Option<String>,
    #[arg(long)]
    ip: Option<String>,
    /// 同机 IP 节点：加在这台已有的落地机（宿主机）上，用它的 SSH 与 agent；--ip 是新的出口 IP（要已配在宿主机网卡上），
    /// 不用填 --ssh-port / --ssh-password-env / --ss-port（SS 端口由中控分配），供应商取宿主机的
    #[arg(long)]
    host: Option<String>,
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
    /// vps 的公网（NAT 映射）SS 端口；节点上 ssserver 固定监听 8388，非 NAT 机不要填
    #[arg(long)]
    ss_port: Option<u16>,
    /// 家宽代理（proxy）的上游：socks5://用户:密码@主机:端口
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
    /// 近 N 天可用率：每天的 TCP 与出口探测成功率（按机器的账单时区分天，最后一天是今天）
    Uptime {
        machine: String,
        /// 天数（1-90）
        #[arg(long, default_value_t = 30)]
        days: i64,
    },
    /// 费用：订阅（价格、到期、自动续费、提醒、账单时区、流量上限与本周期已用）与最近的续费记录
    Billing { machine: String },
    /// 登记续费：到期日顺延（默认按订阅周期推一个周期）、金额默认取订阅价格；只记账，不会向供应商付款
    Renew {
        machine: String,
        /// 新到期日 YYYY-MM-DD（不写则按订阅周期推 --cycles 个周期，目标月没有那一天时落到月末）
        #[arg(long, conflicts_with = "cycles")]
        to: Option<String>,
        /// 按订阅的计费周期推几个周期（一次付了几期）
        #[arg(long, default_value_t = 1)]
        cycles: u32,
        /// 实付金额，如 39.46（币种同订阅；不写 = 订阅价格 × 周期数，给 --to 时是一个周期的价格）
        #[arg(long)]
        amount: Option<String>,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// 订阅：费用、到期日、提醒、账单时区、流量上限（看用 billing）
    Subscription {
        #[command(subcommand)]
        cmd: SubscriptionCmd,
    },
    /// 告警
    Incidents {
        /// 含已恢复的
        #[arg(long)]
        all: bool,
        /// 只看某台机器（id / 显示名 / 线路机名 / IP，含已退役的）
        #[arg(long)]
        machine: Option<String>,
    },
    /// 确认告警
    Ack { id: String },
    /// 静默与维护窗口（可预约）：维护前先开维护窗口，免得告警刷屏
    Silence {
        #[command(subcommand)]
        cmd: SilenceCmd,
    },
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
    Retry {
        id: String,
        /// 机器重装过、主机指纹变了：清掉已记录的指纹，按现在的重新记录
        #[arg(long)]
        reset_host_key: bool,
        /// root 密码放在哪个环境变量里（作业报「中控密钥登录不了，也没有可用的 root 密码」时用）
        #[arg(long)]
        password_env: Option<String>,
    },
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
    /// 入口启用/停用/轮换凭据；add-direct 给线路机加直出入口（从线路机自己的 IP 出去，每台至多一条）
    Entry {
        /// enable | disable | rotate | add-direct
        action: String,
        /// 入口（id、`线路机名:端口`、节点显示名）；add-direct 时是线路机
        entry: String,
        #[arg(long)]
        yes: bool,
        /// add-direct：提交后跟随作业直到结束
        #[arg(long)]
        follow: bool,
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
        /// 翻页：只看编号小于它的记录（上一页末尾提示的编号）
        #[arg(long)]
        before: Option<i64>,
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
    /// 查 IP 的画像（运营商、地点、接入方式）：一个 IP 给详细判定理由，多个给表格；只查中控本地离线库
    Ip {
        #[arg(required = true)]
        ips: Vec<String>,
    },
    /// 节点（或线路机名下全部节点）的来源画像分布：运营商 / 城市 / 接入方式、top 来源、学习期
    Sources {
        machine: String,
        /// 24h | 7d | 30d
        #[arg(long, default_value = "24h")]
        window: String,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// IP 情报离线库状态（加载了几个、版本、缺什么）
    IpintelStatus,
    /// 访问策略：团队/节点的域名黑白名单（草稿 → 预览 → 发布 → 回滚）
    Policy {
        #[command(subcommand)]
        cmd: PolicyCmd,
    },
    /// 订阅实时事件流
    Watch {
        /// 收到 N 条后退出
        #[arg(long)]
        count: Option<usize>,
        /// 也显示实时指标（metrics，最多每 3 秒一条）
        #[arg(long)]
        metrics: bool,
    },
    /// 换线路机：把节点的入口换到另一台线路机（新入口验收后才摘旧入口，不断网；分享链接会变）
    Move {
        /// 节点（id / 显示名 / IP）
        node: String,
        /// 目标线路机
        #[arg(long)]
        to: String,
        /// 节点有多个入口时，指定换掉哪台线路机上的
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        follow: bool,
    },
    /// 搬空线路机：为它上面每个在用入口的节点各建一个换线路机作业（线路机退役前用）
    Evacuate {
        /// 要搬空的线路机
        relay: String,
        /// 搬到哪台线路机
        #[arg(long)]
        to: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        follow: bool,
    },
    /// 恢复已退役的机器（保持原 id；先预检再提交，重新走上架收敛）
    Restore {
        /// 已退役的机器（id / 显示名 / IP）
        machine: String,
        /// 改到别的团队（默认原团队）
        #[arg(long)]
        team: Option<String>,
        /// 指定团队内编号（默认接在末尾）
        #[arg(long)]
        number: Option<i32>,
        /// 节点要接入的线路机
        #[arg(long)]
        relay: Option<String>,
        /// 线路机恢复时改名（原名被占用时）
        #[arg(long)]
        label: Option<String>,
        /// 家宽代理：socks5://用户:密码@主机:端口（退役时出站参数已清掉）
        #[arg(long)]
        upstream: Option<String>,
        /// vps 的公网（NAT 映射）SS 端口；节点上 ssserver 固定监听 8388，非 NAT 机不要填
        #[arg(long)]
        ss_port: Option<u16>,
        /// root 密码放在哪个环境变量里（预检说中控密钥登录不了时必填）
        #[arg(long)]
        password_env: Option<String>,
        #[arg(long)]
        reason: Option<String>,
        /// 机器重装过、主机指纹变了：允许覆盖已记录的指纹
        #[arg(long)]
        reset_host_key: bool,
        /// 预检不试 SSH 登录（只做库内检查）
        #[arg(long)]
        no_probe: bool,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        follow: bool,
    },
}

/// `session_login`：命令自己会用用户名密码登录（keys --user），没有 API 密钥也放行，只要有中控地址
fn make_ctx(json: bool, session_login: bool) -> anyhow::Result<Ctx> {
    let (url, token) = match config::load() {
        Ok(v) => v,
        Err(e) => match config::load_url() {
            Some(url) if session_login => (url, String::new()),
            _ => return Err(e),
        },
    };
    Ok(Ctx::new(Client::new(&url, Cred::Bearer(token))?, url.trim_end_matches('/').to_string(), json))
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
    let session_login = matches!(&cli.cmd, Cmd::Keys { cmd } if keys_user(cmd).is_some());
    let ctx = make_ctx(json, session_login)?;
    let x = &ctx;
    match cli.cmd {
        Cmd::Login { .. } => unreachable!(),
        Cmd::Whoami => cmds::whoami(x),
        Cmd::Status => cmds::status(x),
        Cmd::Ls { team, kind, status, retired } => cmds::ls(x, team.as_deref(), kind.as_deref(), status.as_deref(), retired),
        Cmd::Show { machine } => cmds::show(x, &machine),
        Cmd::Series { machine, range, metric } => cmds::series(x, &machine, &range, &metric),
        Cmd::Uptime { machine, days } => cmds::uptime(x, &machine, days),
        Cmd::Billing { machine } => billing::billing(x, &machine),
        Cmd::Renew { machine, to, cycles, amount, note, yes } => {
            billing::renew(x, billing::RenewOpts { machine: &machine, to: to.as_deref(), cycles, amount: amount.as_deref(), note: note.as_deref(), yes })
        }
        Cmd::Subscription { cmd: SubscriptionCmd::Set { machine, price, currency, cycle, due, auto_renew, remind_days, tz, traffic_gb, reset_day, yes } } => billing::subscription_set(
            x,
            billing::SubSetOpts {
                machine: &machine,
                price: price.as_deref(),
                currency: currency.as_deref(),
                cycle: cycle.as_deref(),
                due: due.as_deref(),
                auto_renew,
                remind_days,
                tz: tz.as_deref(),
                traffic_gb: traffic_gb.as_deref(),
                reset_day: reset_day.as_deref(),
                yes,
            },
        ),
        Cmd::Incidents { all, machine } => cmds::incidents(x, all, machine.as_deref()),
        Cmd::Ack { id } => cmds::ack(x, &id),
        Cmd::Silence { cmd } => match cmd {
            SilenceCmd::Ls => silence::ls(x),
            SilenceCmd::Add { machine, all, rule, start, dur, maintenance, comment, yes } => silence::add(
                x,
                silence::AddOpts { machine: machine.as_deref(), all, rule: rule.as_deref(), start: start.as_deref(), dur: &dur, maintenance, comment: comment.as_deref(), yes },
            ),
            SilenceCmd::Rm { id } => silence::rm(x, &id),
        },
        Cmd::Jobs { active, limit } => cmds::jobs(x, active, limit),
        Cmd::Job { id, follow } => cmds::job(x, &id, follow),
        Cmd::Retry { id, reset_host_key, password_env } => cmds::job_retry(x, &id, reset_host_key, password_env.as_deref()),
        Cmd::Cancel { id } => cmds::job_cancel(x, &id),
        Cmd::Provision(a) => ops::provision(
            x,
            ops::ProvisionOpts {
                file: a.file,
                kind: a.kind,
                ip: a.ip,
                host: a.host,
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
        Cmd::Entry { action, entry, yes, follow } if action == "add-direct" => ops::add_direct(x, &entry, yes, follow),
        Cmd::Entry { action, entry, yes, .. } => ops::entry_action(x, &action, &entry, yes),
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
        Cmd::Audit { target, limit, before } => cmds::audit(x, target.as_deref(), limit, before),
        Cmd::Access { view, node, team, window, search, blocked, kind, limit } => access::access(
            x,
            access::AccessOpts { view: &view, node: node.as_deref(), team: team.as_deref(), window: &window, search: search.as_deref(), blocked, kind: kind.as_deref(), limit },
        ),
        Cmd::Ip { ips } => ipintel::ip(x, &ips),
        Cmd::Sources { machine, window, limit } => ipintel::sources(x, &machine, &window, limit),
        Cmd::IpintelStatus => ipintel::status(x),
        Cmd::Policy { cmd } => match cmd {
            PolicyCmd::Ls => policy::ls(x),
            PolicyCmd::Show { who } => policy::show(x, &who),
            PolicyCmd::Set { who, mode, domains, note } => policy::set(x, policy::SetOpts { who: &who, mode: &mode, domains: domains.as_deref(), note: note.as_deref() }),
            PolicyCmd::Preview => policy::preview(x),
            PolicyCmd::Publish { yes } => policy::publish(x, yes),
            PolicyCmd::Discard { yes } => policy::discard(x, yes),
            PolicyCmd::Versions => policy::versions(x),
            PolicyCmd::Rollback { version, yes } => policy::rollback(x, &version, yes),
        },
        Cmd::Watch { count, metrics } => cmds::watch(x, count, metrics),
        Cmd::Move { node, to, from, yes, follow } => lifecycle::mv(x, &node, &to, from.as_deref(), yes, follow),
        Cmd::Evacuate { relay, to, yes, follow } => lifecycle::evacuate(x, &relay, &to, yes, follow),
        Cmd::Restore { machine, team, number, relay, label, upstream, ss_port, password_env, reason, reset_host_key, no_probe, yes, follow } => lifecycle::restore(
            x,
            lifecycle::RestoreOpts {
                machine: &machine,
                team: team.as_deref(),
                number,
                relay: relay.as_deref(),
                label: label.as_deref(),
                upstream: upstream.as_deref(),
                ss_port,
                password_env: password_env.as_deref(),
                reason: reason.as_deref(),
                reset_host_key,
                no_probe,
                yes,
                follow,
            },
        ),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            if json {
                eprintln!("{}", serde_json::json!({"error": cmds::error_json(&e)}));
            } else {
                eprintln!("错误：{e:#}");
            }
            ExitCode::from(1)
        }
    }
}
