//! fleetctl：团队线路管理的运维命令行。人看表格，AI 加 `--json` 拿 JSON。
//! 退出码：0 成功；1 失败（错误信息在 stderr）；2 命令行参数不对。exec 单台时沿用远端命令的退出码。

mod access;
mod billing;
mod catalog;
mod client;
mod cmds;
mod config;
mod fx;
mod ipintel;
mod lifecycle;
mod meta;
mod ops;
mod ping;
mod policy;
mod probe;
mod recycle;
mod resolve;
mod routers;
mod settings;
mod silence;
mod ssh;
mod table;

use clap::{Args, Parser, Subcommand};
use client::{Client, Cred};
use cmds::Ctx;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "fleetctl",
    version,
    about = "团队线路管理 运维命令行",
    after_help = "连接配置：环境变量 FLEET_URL、FLEET_TOKEN，或 `fleetctl login` 写入的配置文件。\n<机器> 可以写 机器 id、显示名（团队1-06）、线路机名、IP。\n加 --json 输出一行 JSON：ls / entries / incidents / policy ls / silence 是与表格同列的精简对象（单台完整视图用 show、policy show），billing 是订阅、流量额度与最近续费记录合成的对象，router exits 是按出口算好的统计，provider / plan 的 add、set 是改完后的那一项（同 ls 的一项），其余是 API 返回的 data 原样；错误一律走 stderr 且退出码非 0。"
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
    /// 创建 API 密钥（token 只显示一次）。中控要再核对一次当前密码，用 --user 登录的那个；开了两步验证的账户到网页上建
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
    Set(policy::SetOpts),
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
    /// 新建静默：一台机器、--router 一台软路由、--probe 一个探测点，或 --all（全部，要 --yes）。--start 预约开始时间
    Add(silence::AddOpts),
    /// 删除静默（id 前缀，见 silence ls 的 id 列）：进行中的提前结束、告警即刻恢复，已预约的取消
    Rm { id: String },
}

#[derive(Subcommand)]
enum SettingsCmd {
    /// 访问记录保留期：不带参数显示当前的；--raw / --hourly 改（只给一个时另一个不变）。调短后超出的访问记录几分钟内删掉，找不回来
    Retention {
        /// 原始访问记录（每一条：时间、来源 IP、目标）保留天数，1–90
        #[arg(long)]
        raw: Option<i64>,
        /// 汇总（小时汇总、新目标、来源画像）保留天数，1–90、不短于 --raw；访问页最长能看这么久
        #[arg(long)]
        hourly: Option<i64>,
        #[arg(long)]
        yes: bool,
    },
    /// 汇率（计费统一折成 USD）：不带参数看各币种的汇率与来源；<币种> --usd / --per-usd 手动设置（自动更新不再改它），
    /// <币种> --auto 取消手动、改回自动更新的值；--refresh 立即从汇率源更新（默认每天自动一次）
    Fx(fx::FxOpts),
}

#[derive(Subcommand)]
enum SubscriptionCmd {
    /// 改订阅：先读现有的，只改给了的字段再整份提交；还没有订阅时新建，要给 --due（没有套餐的机器还要 --price）。
    /// 价格、币种、周期、流量上限默认跟随套餐，账单时区、提前提醒、流量重置日跟随供应商；给了值 = 单独设置，给 none = 改回跟随
    Set(billing::SubSetOpts),
}

#[derive(Subcommand)]
enum TeamCmd {
    /// 团队列表：排序、在用节点数、哪个是备用团队（只有一个，退役的机器放这里）
    Ls,
    /// 改团队名（备用团队也能改）：团队里的机器显示名跟着变，线路机备注刷新，节点不断线
    Set {
        team: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum RecycleCmd {
    /// 回收站列表（只读），回收时间倒序
    Ls {
        /// fleet（面板里退役的）| legacy（旧面板导入的）
        #[arg(long)]
        source: Option<String>,
        /// 搜名字（含回收时的名字）、IP、团队、供应商、套餐、供应商编号
        #[arg(short, long)]
        q: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: i64,
    },
    /// 一条的详情（id、退役前或回收时的名字、IP）；--json 带回收时的快照
    Show { which: String },
    /// 旧面板的退役机器导入回收站（文件：{"items": [...]} 或数组）。不加 --yes 只预演
    Import {
        #[arg(long)]
        file: std::path::PathBuf,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum ProviderCmd {
    /// 供应商列表：缩写、账单时区、流量计量与重置、提前提醒天数、套餐数
    Ls,
    /// 新建供应商（上架前先确认供应商存在，没有就建）：只给 --code 就行，其余默认跟随组织时区、流量取大、自然月、提前 7 天提醒
    Add {
        name: String,
        #[command(flatten)]
        args: catalog::ProviderArgs,
    },
    /// 改供应商：只改给了的项（--tz none 改回跟随组织时区、--panel-url none 清空）；时区、流量计量与重置、提醒天数立即对名下没单独设置的机器生效
    Set {
        /// 供应商（id / 名称 / 缩写）
        provider: String,
        #[arg(long)]
        name: Option<String>,
        #[command(flatten)]
        args: catalog::ProviderArgs,
        #[arg(long)]
        yes: bool,
    },
    /// 删供应商：还有套餐（含已归档的）或机器（含已退役的）时中控拒绝
    Rm {
        provider: String,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum PlanCmd {
    /// 套餐列表（默认不含已归档的）：供应商、规格、价格 / 周期、在用机器数
    Ls {
        /// 只看这家供应商的
        #[arg(long)]
        provider: Option<String>,
        /// 含已归档的
        #[arg(long)]
        all: bool,
    },
    /// 新建套餐（上架前先确认套餐存在，没有就建）：--provider、--price（主单位，如 299.99）、--currency、--cycle 必填
    Add {
        name: String,
        #[command(flatten)]
        args: catalog::PlanArgs,
    },
    /// 改套餐：只改给了的项（规格给 none 清空）；价格、币种、周期、流量上限立即对用它、没单独设置的机器生效。不再上架的 --archive
    Set {
        /// 套餐（id / 名称；重名时写「供应商/套餐」）
        plan: String,
        #[arg(long)]
        name: Option<String>,
        #[command(flatten)]
        args: catalog::PlanArgs,
        /// 归档（上架时不再列出，已用着的机器不受影响）
        #[arg(long, conflicts_with = "unarchive")]
        archive: bool,
        /// 取消归档
        #[arg(long)]
        unarchive: bool,
        #[arg(long)]
        yes: bool,
    },
    /// 删套餐：还有机器（含已退役的）用着时中控拒绝，不再上架的用 set --archive
    Rm {
        plan: String,
        #[arg(long)]
        yes: bool,
    },
}

fn keys_user(c: &KeysCmd) -> Option<&str> {
    match c {
        KeysCmd::Ls { auth } | KeysCmd::Create { auth, .. } | KeysCmd::Revoke { auth, .. } => auth.user.as_deref(),
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// 保存中控地址与 API 密钥到配置文件（密钥从标准输入读一行：`fleetctl login --url <地址> < 密钥文件`）
    Login {
        #[arg(long)]
        url: String,
        /// API 密钥。写在命令行会进 shell 历史与进程列表，不给就从标准输入读
        #[arg(long)]
        token: Option<String>,
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
        /// 只列 N 天内到期（含已过期）的在用机器，按到期日排，表格加「到期」列（自动续费的也列出并标明）
        #[arg(long, value_name = "DAYS")]
        due: Option<i64>,
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
    Renew(billing::RenewOpts),
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
        /// 只看这类作业：provision（各类上架）| attach_entry | move_entry | replace | upgrade_agent | tune | retire | purge
        #[arg(long)]
        kind: Option<String>,
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
        /// 机器重装过、主机指纹变了：清掉已记录的指纹，按现在的重新记录（只有因主机指纹不符失败的作业能用；同机 IP 节点对宿主机的作业用）
        #[arg(long)]
        reset_host_key: bool,
        /// root 密码放在哪个环境变量里（作业报「中控密钥登录不了，也没有可用的 root 密码」时用）
        #[arg(long)]
        password_env: Option<String>,
    },
    /// 取消进行中的作业（升级 agent 只能在排队时取消，「全部升级」里等它的跟着取消）
    Cancel { id: String },
    /// 上架：先 check 再提交（单台参数或 --file 批量）
    // 装箱：上架参数最多，免得整个 Cmd 跟着它变大（clippy large_enum_variant）
    Provision(Box<ops::ProvisionOpts>),
    /// 退役：默认移进备用团队（照常运行，订阅过期满 1 天后自动回收；移出备用 = 撤销）。--now 立即回收（摘入口、清理、撤令牌、进回收站）
    Retire {
        machine: String,
        /// 立即回收，不等到期（线路机、没有订阅、已过期满 1 天、没上线的、本来就在备用里的只能这样）
        #[arg(long)]
        now: bool,
        #[arg(long)]
        reason: Option<String>,
        /// 立即回收时不清理机器上的服务
        #[arg(long)]
        no_cleanup: bool,
        #[arg(long)]
        yes: bool,
        /// 跟作业进度（只对 --now 有用）
        #[arg(long)]
        follow: bool,
    },
    /// 回收站（只读）：列表、详情、旧面板导入
    Recycle {
        #[command(subcommand)]
        cmd: RecycleCmd,
    },
    /// 改名 = 改团队/编号/线路机名/备注；也改国家（名字旁的国旗）
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
        /// ISO 3166-1 两位代码（如 US、JP）；auto = 改回按 IP 自动
        #[arg(long)]
        country: Option<String>,
    },
    /// 团队编号压缩成连续；不加 --yes 只预览
    Renumber {
        team: String,
        #[arg(long)]
        yes: bool,
    },
    /// 团队：列表（含哪个是备用团队）、改名
    Team {
        #[command(subcommand)]
        cmd: TeamCmd,
    },
    /// 供应商：列表、新建、修改、删除（上架前先确认供应商存在）
    Provider {
        #[command(subcommand)]
        cmd: ProviderCmd,
    },
    /// 套餐：列表、新建、修改（含归档）、删除（上架前先确认套餐存在）
    Plan {
        #[command(subcommand)]
        cmd: PlanCmd,
    },
    /// 升级 agent（单台，或 --all：先升一台做金丝雀，成功后线路机逐台、VPS 节点并发）；新版本起不来、没上报或线路机 Xray 不正常自动换回旧版本
    UpgradeAgent {
        machine: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        follow: bool,
    },
    /// 重做系统调优（内核网络参数、日志体积、时间同步；不重启 ssserver / Xray、不断客户连接）：机器名（可逗号分隔多台），或 --team / --all：
    /// 先做一台（线路机优先）并跟到结束，成功后其余逐台提交，有进行中作业的跳过并列出；--all 要确认（--yes）。
    /// --rollback：撤销指定机器的调优（按机器上第一次调优前记下的原值写回，删掉调优写的 sysctl 配置），要确认（--yes）
    Tune {
        machine: Option<String>,
        #[arg(long, conflicts_with_all = ["machine", "all"])]
        team: Option<String>,
        #[arg(long, conflicts_with = "machine")]
        all: bool,
        #[arg(long, requires = "machine", conflicts_with_all = ["team", "all"])]
        rollback: bool,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        follow: bool,
    },
    /// 重新下发节点的 SS 配置（升级中控后把新的 ssserver 配置推到存量节点，例如出站目标黑名单）：配置有变化才重启 ssserver（该节点的连接断开几秒），同机 IP 节点随宿主机一起刷，最后经入口验收出口。
    /// 机器名（可逗号分隔多台），或 --team / --all（只含在用的 VPS 节点）：先做一台并跟到结束，成功后其余逐台提交，有进行中作业的跳过并列出；--all 要确认（--yes）
    SsRefresh {
        machine: Option<String>,
        #[arg(long, conflicts_with_all = ["machine", "all"])]
        team: Option<String>,
        #[arg(long, conflicts_with = "machine")]
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
        #[arg(value_enum)]
        action: ops::EntryAction,
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
    Exec(ops::ExecOpts),
    /// 在中控上直接登录机器（同机 IP 节点登录宿主机）或软路由：不带命令是交互式，`--` 后带命令则执行完退出、退出码同远端。
    /// 只在中控上能用（用中控的 SSH 密钥，要 root）；主机指纹只认中控记下的；每次登录记审计（不记命令内容，要留命令全文用 exec）
    Ssh {
        /// <机器>（id / 显示名 / 线路机名 / IP），或 `router <软路由名称>`
        #[arg(required = true, num_args = 1..=2, value_name = "机器 | router 名称")]
        target: Vec<String>,
        /// `--` 之后是要在远端执行的命令（单个参数原样当脚本，可含管道）
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
    Audit(cmds::AuditOpts),
    /// 访问监控：哪个节点访问了哪些域名/IP
    Access(access::AccessOpts),
    /// 查 IP 的画像（运营商、地点、接入方式）：一个 IP 给详细判定理由，多个给表格；只查中控本地离线库
    Ip {
        #[arg(required = true)]
        ips: Vec<String>,
    },
    /// 节点（或线路机名下全部节点）的来源画像分布：运营商 / 城市 / 接入方式、top 来源、学习期
    Sources {
        machine: String,
        /// 24h | 7d | 30d，不能比汇总的保留期长（见 settings retention）
        #[arg(long, default_value = "24h")]
        window: String,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// IP 情报离线库状态（加载了几个、版本、缺什么）
    IpintelStatus,
    /// 面板设置：访问记录保留期（settings retention）、汇率（settings fx）
    Settings {
        #[command(subcommand)]
        cmd: SettingsCmd,
    },
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
    /// 换线路机：把节点的入口换到另一台线路机（新入口验收后才摘旧入口，不断网；分享链接会变）。
    /// 软路由上指向旧入口的出口在摘旧入口之前改成新入口（备份、重启 OpenClash、测延迟，失败全部换回、旧入口不摘）
    Move {
        /// 节点（id / 显示名 / IP）
        node: String,
        /// 目标线路机
        #[arg(long)]
        to: String,
        /// 节点有多个入口时，指定换掉哪台线路机上的
        #[arg(long)]
        from: Option<String>,
        /// 不改软路由，只换入口（路由上指向旧入口的出口摘掉后就断了）
        #[arg(long)]
        no_routers: bool,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        follow: bool,
    },
    /// 搬空线路机：为它上面每个在用入口的节点各建一个换线路机作业（线路机退役前用）；软路由出口跟着改，同 move
    Evacuate {
        /// 要搬空的线路机
        relay: String,
        /// 搬到哪台线路机
        #[arg(long)]
        to: String,
        /// 不改软路由，只换入口
        #[arg(long)]
        no_routers: bool,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        follow: bool,
    },
    /// 搬迁建议：线路机按近 7 天的速度重置前会用满时，挑出搬走后能撑到重置日的最少入口和目标线路机（默认额度余量最多的），
    /// 打印建议与可直接执行的 move 命令；只读，不执行
    Rebalance {
        /// 流量吃紧的线路机
        relay: String,
        /// 搬到哪台线路机（默认：在用、非故障、额度余量最多的）
        #[arg(long)]
        target: Option<String>,
        /// 不动的节点（可重复），如分享给别人的入口所在的节点
        #[arg(long)]
        exclude: Vec<String>,
    },
    /// 替换节点：新节点接替旧节点。软路由上指向旧节点的出口改成新节点的入口（备份、重启 OpenClash、测延迟，失败全部换回），
    /// 旧节点去备用团队（或 --to 指定的团队），新节点接旧编号。不加 --yes 只打印预览
    Replace(lifecycle::ReplaceOpts),
    /// 改出口指向：软路由上指向旧入口的出口改成新入口（备份、重启 OpenClash、测延迟，失败全部换回），两个入口都不动。
    /// 如直出出口换线路机（repoint 线路机1-直出 --to 线路机3-直出）、节点出问题时临时指到备用节点。不加 --yes 只打印预览
    Repoint {
        /// 旧入口（入口 id、`线路机名:端口`、节点显示名如「团队2-11」「线路机1-直出」）
        from: String,
        /// 新入口
        #[arg(long)]
        to: String,
        /// 只改这台软路由（可重复；不给 = 所有指向旧入口的）
        #[arg(long = "router")]
        routers: Vec<String>,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        follow: bool,
    },
    /// 软路由列表：状态、出口通 / 总、客户端、CPU、内存、WAN 速率
    Routers {
        /// 只看这个团队的
        #[arg(long)]
        team: Option<String>,
    },
    /// 软路由：详情、接入（接入命令或一键部署）、重新生成接入命令、改名、删除
    Router {
        #[command(subcommand)]
        cmd: RouterCmd,
    },
    /// 探测点：从靠近客户的地方（家里 PVE、香港）测线路机的客户入口；探测顺序决定心跳格与「客户入口连不上」告警
    Probe {
        #[command(subcommand)]
        cmd: ProbeCmd,
    },
    /// 三网 PING 的全局目标（8 个槽位）：不给子命令显示；set 改一个槽位，mode 改方式。线路机每分钟往外测一轮
    PingTargets {
        #[command(subcommand)]
        cmd: Option<PingTargetsCmd>,
    },
    /// 一台线路机的三网 PING：不给子命令显示目标与当前读数；set / reset / mode 改这台的单独设置
    Ping {
        /// 线路机（id / 名称 / IP）
        relay: String,
        #[command(subcommand)]
        cmd: Option<PingCmd>,
    },
}

#[derive(Subcommand)]
enum PingTargetsCmd {
    /// 改一个槽位：--target "" 清空（不测）
    Set {
        /// 槽位 1–8
        slot: u8,
        #[arg(long)]
        name: Option<String>,
        /// host、host:port、[IPv6]:port；TCP 不写端口连 80
        #[arg(long)]
        target: Option<String>,
    },
    /// 方式：tcp（建连，默认）/ icmp（系统 ping，用不了时退回 TCP）
    Mode {
        #[arg(value_parser = ["tcp", "icmp"])]
        mode: String,
    },
}

#[derive(Subcommand)]
enum PingCmd {
    /// 这台单独设置一个槽位（--target "" = 这台不测它）
    Set {
        slot: u8,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        target: Option<String>,
    },
    /// 改回跟随全局：给槽位只改那一个，不给全部
    Reset { slot: Option<u8> },
    /// 这台的方式：tcp / icmp / global（跟随全局）
    Mode {
        #[arg(value_parser = ["tcp", "icmp", "global"])]
        mode: String,
    },
}

#[derive(Subcommand)]
enum ProbeCmd {
    /// 列表：状态、在探测顺序里排第几、最近上报、版本、近 1 小时到各线路机的平均延迟；探测顺序与此刻实际用的那一路
    Ls,
    /// 新建并安装：给 --host 由中控 SSH 装好、等首次上报；不给就打印一次性安装命令
    Add {
        name: String,
        #[arg(long)]
        note: Option<String>,
        #[command(flatten)]
        install: probe::InstallArgs,
    },
    /// 重装（换新令牌）：给 --host 由中控 SSH 重装；不给就打印新的一次性安装命令
    Install {
        /// 探测点（id / id 前缀 / 名称）
        probe: String,
        #[command(flatten)]
        install: probe::InstallArgs,
    },
    /// 启用 / 停用（停用的拿到空目标、不测；主来源不能停用）、改名、改备注
    Set {
        probe: String,
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        #[arg(long)]
        disable: bool,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// 删除：令牌作废、检查记录一起删；探测点机器上的东西要自己清（会打印命令）。主来源不能删
    Rm {
        probe: String,
        #[arg(long)]
        yes: bool,
    },
    /// 设探测顺序：按顺序用第一个能用的探测点（第一个是主探测来源），都用不了时由中控代测；只写 central = 主来源是中控（同时打开中控自己的检查）
    Order {
        /// 探测点（id / id 前缀 / 名称），按优先顺序；或只写 central
        #[arg(required = true, num_args = 1..)]
        sources: Vec<String>,
    },
    /// 中控自己的检查开 / 关（主来源是中控时不能关）
    Central {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// 升级成中控 dist 里的 fleet-agent：探测点 30 秒内自己下载、核对、替换、重启；默认等它报上新版本。
    /// 这个版本在它上面换上后跑不起来过（自己换回了旧版本）时中控拒绝（409）：修好后发新版本再升级
    Upgrade {
        probe: String,
        #[arg(long)]
        no_wait: bool,
    },
}

#[derive(Subcommand)]
enum RouterCmd {
    /// 一台软路由的详情：系统、接口、OpenClash、出口（对应的 fleet 入口、参数是否一致、延迟、连接）、客户端
    Show {
        /// 软路由（id / id 前缀 / 名称）
        router: String,
    },
    /// 新建：不给 --host 打印接入命令（在路由上用 root 执行，30 分钟内有效）；给 --host 就一键部署（中控登录路由装好隧道）
    Add(routers::AddOpts),
    /// 重新生成接入命令：现在的隧道密钥立刻作废、隧道换端口（危险，要 --yes）
    Bootstrap {
        router: String,
        #[arg(long)]
        yes: bool,
    },
    /// 出口历史：每个出口在这段时间里的测试次数、失败次数、成功率、连续两轮失败的次数、延迟中位数与最大值、当前连接数，末行合计
    Exits {
        router: String,
        /// 1h（原始点）| 6h | 24h（5 分钟一点）| 7d | 30d（小时汇总：看不出连续失败）
        #[arg(long, default_value = "24h")]
        range: String,
    },
    /// 一键部署（重新部署）：中控用账号密码（或自己的密钥）登录路由装好隧道
    Deploy {
        router: String,
        #[command(flatten)]
        ssh: routers::SshOpts,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        follow: bool,
    },
    /// 改名称 / 团队（none 清掉）/ 备注
    Edit {
        router: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        team: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// 删除：撤销隧道公钥，状态与指标一起删掉；路由上中控加的东西要在路由上自己清（会打印命令）
    Rm {
        router: String,
        #[arg(long)]
        yes: bool,
        /// 有没做完的作业改过它的出口时也删（路由确实回不来了）：作业不再改回它、能收尾，它上面被改过的出口要自己处理
        #[arg(long)]
        force: bool,
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
    meta::init(&url, &token);
    Ok(Ctx::new(Client::new(&url, Cred::Bearer(token))?, url.trim_end_matches('/').to_string(), json))
}

/// 明文 http 且主机不是本机 / 内网地址：密钥会在公网上裸传
fn insecure_http(url: &str) -> bool {
    use std::net::IpAddr;
    let Ok(u) = reqwest::Url::parse(url) else { return false };
    let Some(host) = u.host_str().filter(|_| u.scheme() == "http") else { return false };
    let Ok(ip) = host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() else {
        return !host.eq_ignore_ascii_case("localhost");
    };
    !match ip {
        IpAddr::V4(a) => a.is_loopback() || a.is_private() || a.is_link_local(),
        IpAddr::V6(a) => a.is_loopback() || a.is_unique_local() || a.is_unicast_link_local(),
    }
}

fn login(url: &str, token: Option<&str>, json: bool) -> anyhow::Result<u8> {
    use std::io::{BufRead, IsTerminal, Write};
    let token = match token {
        Some(t) => t.trim().to_string(),
        None => {
            if std::io::stdin().is_terminal() {
                eprint!("API 密钥：");
                let _ = std::io::stderr().flush();
            }
            let mut l = String::new();
            std::io::stdin().lock().read_line(&mut l)?;
            // Windows PowerShell 5 写的 UTF-8 文件开头带 BOM
            l.trim().trim_start_matches('\u{feff}').to_string()
        }
    };
    if token.is_empty() {
        anyhow::bail!("没有拿到 API 密钥：从标准输入给一行，或加 --token");
    }
    if insecure_http(url) {
        eprintln!("警告：{url} 是明文 http 且不是本机或内网地址，API 密钥会在网络上明文传输；请改用 https://");
    }
    let token = token.as_str();
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
        return login(url, token.as_deref(), json);
    }
    if let Cmd::Ssh { target, command } = &cli.cmd {
        // 不在中控上时先说这个，而不是「缺少配置」：中控上的 fleetctl 由部署配好
        ssh::check_key(std::path::Path::new(ssh::KEY))?;
        return ssh::run(&make_ctx(json, false)?, target, command);
    }
    let session_login = matches!(&cli.cmd, Cmd::Keys { cmd } if keys_user(cmd).is_some());
    let ctx = make_ctx(json, session_login)?;
    let x = &ctx;
    match cli.cmd {
        Cmd::Login { .. } | Cmd::Ssh { .. } => unreachable!(),
        Cmd::Whoami => cmds::whoami(x),
        Cmd::Status => cmds::status(x),
        Cmd::Ls { team, kind, status, due } => cmds::ls(x, team.as_deref(), kind.as_deref(), status.as_deref(), due),
        Cmd::Show { machine } => cmds::show(x, &machine),
        Cmd::Series { machine, range, metric } => cmds::series(x, &machine, &range, &metric),
        Cmd::Uptime { machine, days } => cmds::uptime(x, &machine, days),
        Cmd::Billing { machine } => billing::billing(x, &machine),
        Cmd::Renew(o) => billing::renew(x, o),
        Cmd::Subscription { cmd: SubscriptionCmd::Set(o) } => billing::subscription_set(x, o),
        Cmd::Incidents { all, machine } => cmds::incidents(x, all, machine.as_deref()),
        Cmd::Ack { id } => cmds::ack(x, &id),
        Cmd::Silence { cmd } => match cmd {
            SilenceCmd::Ls => silence::ls(x),
            SilenceCmd::Add(o) => silence::add(x, o),
            SilenceCmd::Rm { id } => silence::rm(x, &id),
        },
        Cmd::Jobs { active, kind, limit } => cmds::jobs(x, active, kind.as_deref(), limit),
        Cmd::Job { id, follow } => cmds::job(x, &id, follow),
        Cmd::Retry { id, reset_host_key, password_env } => cmds::job_retry(x, &id, reset_host_key, password_env.as_deref()),
        Cmd::Cancel { id } => cmds::job_cancel(x, &id),
        Cmd::Provision(a) => ops::provision(x, *a),
        Cmd::Retire { machine, now, reason, no_cleanup, yes, follow } => ops::retire(x, &machine, now, reason.as_deref(), no_cleanup, yes, follow),
        Cmd::Recycle { cmd } => match cmd {
            RecycleCmd::Ls { source, q, limit } => recycle::ls(x, source.as_deref(), q.as_deref(), limit),
            RecycleCmd::Show { which } => recycle::show(x, &which),
            RecycleCmd::Import { file, yes } => recycle::import(x, &file, yes),
        },
        Cmd::Rename { machine, team, number, label, note, country } => {
            ops::rename(x, &machine, team.as_deref(), number, label.as_deref(), note.as_deref(), country.as_deref())
        }
        Cmd::Renumber { team, yes } => ops::renumber(x, &team, yes),
        Cmd::Team { cmd } => match cmd {
            TeamCmd::Ls => ops::team_ls(x),
            TeamCmd::Set { team, name, yes } => ops::team_set(x, &team, &name, yes),
        },
        Cmd::Provider { cmd } => match cmd {
            ProviderCmd::Ls => catalog::provider_ls(x),
            ProviderCmd::Add { name, args } => catalog::provider_add(x, &name, &args),
            ProviderCmd::Set { provider, name, args, yes } => catalog::provider_set(x, &provider, name.as_deref(), &args, yes),
            ProviderCmd::Rm { provider, yes } => catalog::provider_rm(x, &provider, yes),
        },
        Cmd::Plan { cmd } => match cmd {
            PlanCmd::Ls { provider, all } => catalog::plan_ls(x, provider.as_deref(), all),
            PlanCmd::Add { name, args } => catalog::plan_add(x, &name, &args),
            PlanCmd::Set { plan, name, args, archive, unarchive, yes } => {
                let ar = if archive { Some(true) } else if unarchive { Some(false) } else { None };
                catalog::plan_set(x, &plan, name.as_deref(), &args, ar, yes)
            }
            PlanCmd::Rm { plan, yes } => catalog::plan_rm(x, &plan, yes),
        },
        Cmd::UpgradeAgent { machine, all, yes, follow } => ops::upgrade_agent(x, machine.as_deref(), all, yes, follow),
        Cmd::Tune { machine, team, all, rollback, yes, follow } => ops::tune(x, machine.as_deref(), team.as_deref(), all, rollback, yes, follow),
        Cmd::SsRefresh { machine, team, all, yes, follow } => ops::ss_refresh(x, machine.as_deref(), team.as_deref(), all, yes, follow),
        Cmd::Entries { machine } => cmds::entries(x, machine.as_deref()),
        Cmd::Share { entry } => ops::share(x, &entry),
        Cmd::Entry { action, entry, yes, follow } => ops::entry_action(x, action, &entry, yes, follow),
        Cmd::Exec(o) => ops::exec(x, o),
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
        Cmd::Audit(o) => cmds::audit(x, o),
        Cmd::Access(o) => access::access(x, o),
        Cmd::Ip { ips } => ipintel::ip(x, &ips),
        Cmd::Sources { machine, window, limit } => ipintel::sources(x, &machine, &window, limit),
        Cmd::IpintelStatus => ipintel::status(x),
        Cmd::Settings { cmd: SettingsCmd::Retention { raw, hourly, yes } } => settings::retention(x, raw, hourly, yes),
        Cmd::Settings { cmd: SettingsCmd::Fx(o) } => fx::fx(x, o),
        Cmd::Policy { cmd } => match cmd {
            PolicyCmd::Ls => policy::ls(x),
            PolicyCmd::Show { who } => policy::show(x, &who),
            PolicyCmd::Set(o) => policy::set(x, o),
            PolicyCmd::Preview => policy::preview(x),
            PolicyCmd::Publish { yes } => policy::publish(x, yes),
            PolicyCmd::Discard { yes } => policy::discard(x, yes),
            PolicyCmd::Versions => policy::versions(x),
            PolicyCmd::Rollback { version, yes } => policy::rollback(x, &version, yes),
        },
        Cmd::Watch { count, metrics } => cmds::watch(x, count, metrics),
        Cmd::Rebalance { relay, target, exclude } => lifecycle::rebalance(x, &relay, target.as_deref(), &exclude),
        Cmd::Move { node, to, from, no_routers, yes, follow } => lifecycle::mv(x, &node, &to, from.as_deref(), no_routers, yes, follow),
        Cmd::Evacuate { relay, to, no_routers, yes, follow } => lifecycle::evacuate(x, &relay, &to, no_routers, yes, follow),
        Cmd::Replace(o) => lifecycle::replace(x, o),
        Cmd::Repoint { from, to, routers, yes, follow } => lifecycle::repoint(x, &from, &to, &routers, yes, follow),
        Cmd::Routers { team } => routers::ls(x, team.as_deref()),
        Cmd::Router { cmd } => match cmd {
            RouterCmd::Show { router } => routers::show(x, &router),
            RouterCmd::Exits { router, range } => routers::exits(x, &router, &range),
            RouterCmd::Add(o) => routers::add(x, o),
            RouterCmd::Bootstrap { router, yes } => routers::bootstrap(x, &router, yes),
            RouterCmd::Deploy { router, ssh, yes, follow } => routers::deploy(x, &router, ssh, yes, follow),
            RouterCmd::Edit { router, name, team, note } => routers::edit(x, &router, name.as_deref(), team.as_deref(), note.as_deref()),
            RouterCmd::Rm { router, yes, force } => routers::rm(x, &router, yes, force),
        },
        Cmd::Probe { cmd } => match cmd {
            ProbeCmd::Ls => probe::ls(x),
            ProbeCmd::Add { name, note, install } => probe::add(x, &name, note.as_deref(), &install),
            ProbeCmd::Install { probe: q, install } => probe::install(x, &q, &install),
            ProbeCmd::Set { probe: q, enable, disable, name, note } => {
                let en = if enable { Some(true) } else if disable { Some(false) } else { None };
                probe::set(x, &q, en, name.as_deref(), note.as_deref())
            }
            ProbeCmd::Rm { probe: q, yes } => probe::rm(x, &q, yes),
            ProbeCmd::Order { sources } => probe::order(x, &sources),
            ProbeCmd::Central { state } => probe::central(x, state == "on"),
            ProbeCmd::Upgrade { probe: q, no_wait } => probe::upgrade(x, &q, no_wait),
        },
        Cmd::PingTargets { cmd } => match cmd {
            None => ping::targets(x),
            Some(PingTargetsCmd::Set { slot, name, target }) => ping::targets_set(x, slot, name.as_deref(), target.as_deref()),
            Some(PingTargetsCmd::Mode { mode }) => ping::targets_mode(x, &mode),
        },
        Cmd::Ping { relay, cmd } => match cmd {
            None => ping::show(x, &relay),
            Some(PingCmd::Set { slot, name, target }) => ping::set(x, &relay, slot, name.as_deref(), target.as_deref()),
            Some(PingCmd::Reset { slot }) => ping::reset(x, &relay, slot),
            Some(PingCmd::Mode { mode }) => ping::mode(x, &relay, &mode),
        },
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

#[cfg(test)]
mod tests {
    use super::{insecure_http, Cli, Cmd};
    use clap::Parser;

    /// `ssh` 的命令行：目标 1–2 个参数（`router <名称>`），`--` 之后原样是远端命令
    #[test]
    fn ssh_command_line() {
        let parse = |a: &[&str]| match Cli::try_parse_from(["fleetctl"].iter().chain(a)).map(|c| c.cmd) {
            Ok(Cmd::Ssh { target, command }) => Ok((target, command)),
            Ok(_) => panic!("不是 ssh"),
            Err(e) => Err(e.kind()),
        };
        assert_eq!(parse(&["ssh", "团队1-03"]), Ok((vec!["团队1-03".into()], vec![])));
        assert_eq!(parse(&["--json", "ssh", "团队1-03", "--", "df", "-h"]), Ok((vec!["团队1-03".into()], vec!["df".into(), "-h".into()])));
        assert_eq!(parse(&["ssh", "router", "团队2-路由A", "--", "logread | tail"]), Ok((vec!["router".into(), "团队2-路由A".into()], vec!["logread | tail".into()])));
        assert!(parse(&["ssh"]).is_err());
        assert!(parse(&["ssh", "router", "a", "b"]).is_err(), "目标最多两个参数");
    }

    /// 命令行定义自洽（参数重名、互斥写反等 clap 在这里断言）
    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn plain_http_warns_only_for_public_hosts() {
        for u in ["http://192.168.31.60:8080", "http://127.0.0.1:8080", "http://localhost:8080", "http://10.1.2.3", "http://[::1]:8080", "http://[fd00::1]/", "https://fleet.kismet613.com", "https://8.8.8.8"] {
            assert!(!insecure_http(u), "{u}");
        }
        for u in ["http://fleet.kismet613.com", "http://8.8.8.8:8080", "http://[2001:db8::1]/"] {
            assert!(insecure_http(u), "{u}");
        }
    }
}
