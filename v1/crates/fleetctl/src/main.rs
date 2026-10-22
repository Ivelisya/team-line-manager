//! fleetctl：团队线路管理的运维命令行。人看表格，AI 加 `--json` 拿 JSON。
//! 退出码：0 成功；1 失败（错误信息在 stderr）；2 命令行参数不对。exec 单台时沿用远端命令的退出码。

mod access;
mod billing;
mod client;
mod cmds;
mod config;
mod ipintel;
mod lifecycle;
mod meta;
mod ops;
mod policy;
mod resolve;
mod routers;
mod settings;
mod silence;
mod ssh;
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
        /// 开始时间，组织时区（中控的 org_timezone，默认北京时间；同表格）：2026-10-23 02:00（别的时区写带时区的 2026-10-23T02:00:00+09:00）；不填 = 现在，最多提前 90 天
        #[arg(long)]
        start: Option<String>,
        /// 时长：30m / 4h / 2d，可组合（1h30m）；最长 30 天
        #[arg(long = "for", value_name = "DURATION")]
        dur: String,
        /// 标成维护：卡片显示「维护」，期间暂停健康判定（针对整台机器，不能和 --rule 一起用）
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
}

#[derive(Subcommand)]
enum SubscriptionCmd {
    /// 改订阅：先读现有的，只改给了的字段再整份提交；还没有订阅时新建，要给 --due（没有套餐的机器还要 --price）。
    /// 价格、币种、周期、流量上限默认跟随套餐，账单时区、提前提醒、流量重置日跟随供应商；给了值 = 单独设置，给 none = 改回跟随
    Set {
        machine: String,
        /// 价格，如 39.46（币种的主单位，最多两位小数）；none = 跟随套餐
        #[arg(long)]
        price: Option<String>,
        /// 币种代码，如 USD / CNY；none = 跟随套餐
        #[arg(long)]
        currency: Option<String>,
        /// 计费周期：monthly | quarterly | semiannual | yearly；none = 跟随套餐
        #[arg(long)]
        cycle: Option<String>,
        /// 到期日 YYYY-MM-DD（账单时区的日期）。续费请用 renew，会留续费记录
        #[arg(long)]
        due: Option<String>,
        /// 到期自动续费：true | false（只影响提醒：自动续费的到期前不提醒，过期才告警；不会真的扣款）
        #[arg(long)]
        auto_renew: Option<bool>,
        /// 到期前几天开始提醒（0–60）；none = 跟随供应商
        #[arg(long)]
        remind_days: Option<String>,
        /// 账单时区（IANA 名）：到期与流量周期按它的日期算；none = 跟随供应商
        #[arg(long)]
        tz: Option<String>,
        /// 流量上限 GB（供应商的十进制 GB；0 = 不限）；none = 跟随套餐（同机 IP 节点没有套餐 = 与宿主机共用）
        #[arg(long)]
        traffic_gb: Option<String>,
        /// 流量每月几号重置（1–31，小月没有这一天按月末）；none = 跟随供应商（按账单日 = 到期日几号，或按自然月 = 1 号）
        #[arg(long)]
        reset_day: Option<String>,
        /// 按供应商面板校准本周期已用：填面板上的 GB 数（如 901.6），之后的用量按网卡流量累加；none = 清掉校准。
        /// 周期中途接入或和面板差得多时用；进入下个周期自动失效
        #[arg(long)]
        used: Option<String>,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum TeamCmd {
    /// 团队列表：排序、在用节点数、是否备用
    Ls,
    /// 设为 / 取消备用团队。备用团队里在用的机器订阅过期满 1 天（账单时区的「到期日 + 1 天」过完）后，中控自动退役并彻底删除
    Set {
        team: String,
        /// true = 设为备用（设为备用要确认：已经过期满 1 天的机器一小时内就会被删）；false = 取消
        #[arg(long, action = clap::ArgAction::Set, value_name = "true|false")]
        spare: bool,
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
        /// 只看已退役
        #[arg(long)]
        retired: bool,
        /// 只列 N 天内到期（含已过期）的在用机器，按到期日排，表格加「到期」列（自动续费的也列出并标明）
        #[arg(long, value_name = "DAYS", conflicts_with = "retired")]
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
        /// 只看这类作业：provision（各类上架）| restore（各类恢复）| attach_entry | move_entry | replace | upgrade_agent | retire | purge
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
    /// 团队：列表（含是否备用）、设为 / 取消备用团队
    Team {
        #[command(subcommand)]
        cmd: TeamCmd,
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
    Audit {
        /// 只看某台机器
        #[arg(long)]
        target: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
        /// 翻页：只看编号小于它的记录（上一页末尾提示的编号）
        #[arg(long)]
        before: Option<i64>,
        /// 分类：auth（登录与账户）| exec（远程执行）| machine（机器与入口）| config（设置与告警）
        #[arg(long)]
        cat: Option<String>,
        /// 搜索标题、摘要、操作人
        #[arg(short, long)]
        q: Option<String>,
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
        /// 1h | 24h | 7d | 30d，不能比汇总的保留期长（见 settings retention）；events 固定取最近记录
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
        /// 24h | 7d | 30d，不能比汇总的保留期长（见 settings retention）
        #[arg(long, default_value = "24h")]
        window: String,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// IP 情报离线库状态（加载了几个、版本、缺什么）
    IpintelStatus,
    /// 面板设置：访问记录保留期（settings retention）
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
    /// 替换节点：新节点接替旧节点。软路由上指向旧节点的出口改成新节点的入口（备份、重启 OpenClash、测延迟，失败全部换回），
    /// 旧节点去备用团队（或 --to 指定的团队），新节点接旧编号。不加 --yes 只打印预览
    Replace {
        /// 旧节点（id / 显示名 / IP）
        old: String,
        /// 新节点
        #[arg(long = "with")]
        new: String,
        /// 旧节点去哪个团队（默认：同团队的备用团队）
        #[arg(long)]
        to: Option<String>,
        /// 旧节点在那个团队里的编号（默认第一个空号）
        #[arg(long)]
        number: Option<i32>,
        /// 不改软路由，只交换编号
        #[arg(long)]
        no_routers: bool,
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
        /// 家宽代理：socks5://用户:密码@主机:端口。不给沿用退役前的上游；上游有用户名时要给（密码退役时已清掉）
        #[arg(long)]
        upstream: Option<String>,
        /// VPS 节点的公网（NAT 映射）SS 端口：不给沿用退役前的（预检里能看到），都没有为 8388
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
}

/// 一键部署的目标（中控要能直接连到路由的 SSH）。密码只从标准输入读，不收参数
#[derive(Args)]
struct RouterSshArgs {
    /// 路由的 IP 或主机名（中控连得到的）；给了就一键部署
    #[arg(long)]
    host: Option<String>,
    /// 路由的 SSH 端口（默认 22）
    #[arg(long)]
    port: Option<u16>,
    /// 登录用户（默认 root）
    #[arg(long)]
    user: Option<String>,
    /// 从标准输入读一行密码（不给就用中控自己的密钥登录：路由上已经授权过中控公钥时）
    #[arg(long)]
    password_stdin: bool,
}

impl RouterSshArgs {
    fn opts(&self) -> routers::SshOpts<'_> {
        routers::SshOpts { host: self.host.as_deref(), port: self.port, user: self.user.as_deref(), password_stdin: self.password_stdin }
    }
}

#[derive(Subcommand)]
enum RouterCmd {
    /// 一台软路由的详情：系统、接口、OpenClash、出口（对应的 fleet 入口、参数是否一致、延迟、连接）、客户端
    Show {
        /// 软路由（id / id 前缀 / 名称）
        router: String,
    },
    /// 新建：不给 --host 打印接入命令（在路由上用 root 执行，30 分钟内有效）；给 --host 就一键部署（中控登录路由装好隧道）
    Add {
        name: String,
        #[arg(long)]
        team: Option<String>,
        #[arg(long)]
        note: Option<String>,
        #[command(flatten)]
        ssh: RouterSshArgs,
        /// 一键部署要确认
        #[arg(long)]
        yes: bool,
        /// 一键部署：等它结束（失败退出码 1）
        #[arg(long)]
        follow: bool,
    },
    /// 重新生成接入命令：现在的隧道密钥立刻作废、隧道换端口（危险，要 --yes）
    Bootstrap {
        router: String,
        #[arg(long)]
        yes: bool,
    },
    /// 一键部署（重新部署）：中控用账号密码（或自己的密钥）登录路由装好隧道
    Deploy {
        router: String,
        #[command(flatten)]
        ssh: RouterSshArgs,
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
        Cmd::Ls { team, kind, status, retired, due } => cmds::ls(x, team.as_deref(), kind.as_deref(), status.as_deref(), retired, due),
        Cmd::Show { machine } => cmds::show(x, &machine),
        Cmd::Series { machine, range, metric } => cmds::series(x, &machine, &range, &metric),
        Cmd::Uptime { machine, days } => cmds::uptime(x, &machine, days),
        Cmd::Billing { machine } => billing::billing(x, &machine),
        Cmd::Renew { machine, to, cycles, amount, note, yes } => {
            billing::renew(x, billing::RenewOpts { machine: &machine, to: to.as_deref(), cycles, amount: amount.as_deref(), note: note.as_deref(), yes })
        }
        Cmd::Subscription { cmd } => match cmd {
            SubscriptionCmd::Set { machine, price, currency, cycle, due, auto_renew, remind_days, tz, traffic_gb, reset_day, used, yes } => billing::subscription_set(
                x,
                billing::SubSetOpts {
                    machine: &machine,
                    price: price.as_deref(),
                    currency: currency.as_deref(),
                    cycle: cycle.as_deref(),
                    due: due.as_deref(),
                    auto_renew,
                    remind_days: remind_days.as_deref(),
                    tz: tz.as_deref(),
                    traffic_gb: traffic_gb.as_deref(),
                    reset_day: reset_day.as_deref(),
                    used: used.as_deref(),
                    yes,
                },
            ),
        },
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
        Cmd::Jobs { active, kind, limit } => cmds::jobs(x, active, kind.as_deref(), limit),
        Cmd::Job { id, follow } => cmds::job(x, &id, follow),
        Cmd::Retry { id, reset_host_key, password_env } => cmds::job_retry(x, &id, reset_host_key, password_env.as_deref()),
        Cmd::Cancel { id } => cmds::job_cancel(x, &id),
        Cmd::Provision(a) => ops::provision(x, *a),
        Cmd::Retire { machine, reason, no_cleanup, yes, follow } => ops::retire(x, &machine, reason.as_deref(), no_cleanup, yes, follow),
        Cmd::Purge { machine, yes, follow } => ops::purge(x, &machine, yes, follow),
        Cmd::Rename { machine, team, number, label, note, country } => {
            ops::rename(x, &machine, team.as_deref(), number, label.as_deref(), note.as_deref(), country.as_deref())
        }
        Cmd::Renumber { team, yes } => ops::renumber(x, &team, yes),
        Cmd::Team { cmd } => match cmd {
            TeamCmd::Ls => ops::team_ls(x),
            TeamCmd::Set { team, spare, yes } => ops::team_set(x, &team, spare, yes),
        },
        Cmd::UpgradeAgent { machine, all, yes, follow } => ops::upgrade_agent(x, machine.as_deref(), all, yes, follow),
        Cmd::Entries { machine } => cmds::entries(x, machine.as_deref()),
        Cmd::Share { entry } => ops::share(x, &entry),
        Cmd::Entry { action, entry, yes, follow } => ops::entry_action(x, action, &entry, yes, follow),
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
        Cmd::Audit { target, limit, before, cat, q } => cmds::audit(x, cmds::AuditOpts { target: target.as_deref(), limit, before, cat: cat.as_deref(), q: q.as_deref() }),
        Cmd::Access { view, node, team, window, search, blocked, kind, limit } => access::access(
            x,
            access::AccessOpts { view: &view, node: node.as_deref(), team: team.as_deref(), window: &window, search: search.as_deref(), blocked, kind: kind.as_deref(), limit },
        ),
        Cmd::Ip { ips } => ipintel::ip(x, &ips),
        Cmd::Sources { machine, window, limit } => ipintel::sources(x, &machine, &window, limit),
        Cmd::IpintelStatus => ipintel::status(x),
        Cmd::Settings { cmd: SettingsCmd::Retention { raw, hourly, yes } } => settings::retention(x, raw, hourly, yes),
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
        Cmd::Replace { old, new, to, number, no_routers, yes, follow } => {
            lifecycle::replace(x, lifecycle::ReplaceOpts { old: &old, new: &new, to: to.as_deref(), number, no_routers, yes, follow })
        }
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
        Cmd::Routers { team } => routers::ls(x, team.as_deref()),
        Cmd::Router { cmd } => match cmd {
            RouterCmd::Show { router } => routers::show(x, &router),
            RouterCmd::Add { name, team, note, ssh, yes, follow } => {
                routers::add(x, routers::AddOpts { name: &name, team: team.as_deref(), note: note.as_deref(), ssh: ssh.opts(), yes, follow })
            }
            RouterCmd::Bootstrap { router, yes } => routers::bootstrap(x, &router, yes),
            RouterCmd::Deploy { router, ssh, yes, follow } => routers::deploy(x, &router, ssh.opts(), yes, follow),
            RouterCmd::Edit { router, name, team, note } => routers::edit(x, &router, name.as_deref(), team.as_deref(), note.as_deref()),
            RouterCmd::Rm { router, yes } => routers::rm(x, &router, yes),
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

    /// `entry` 的动作由 clap 校验：写错是参数错误（退出码 2），不发请求
    #[test]
    fn entry_action_is_value_enum() {
        let parse = |a: &[&str]| Cli::try_parse_from(["fleetctl", "entry"].iter().chain(a)).map(|c| matches!(c.cmd, Cmd::Entry { action: crate::ops::EntryAction::AddDirect, .. }));
        assert_eq!(parse(&["add-direct", "线路机2"]).ok(), Some(true));
        assert_eq!(parse(&["rotate", "线路机2:20001", "--yes"]).ok(), Some(false));
        let e = parse(&["enabel", "线路机2:20001"]).unwrap_err();
        assert_eq!((e.kind(), e.exit_code()), (clap::error::ErrorKind::InvalidValue, 2));
    }

    /// `team set` 的 --spare 要明写 true / false（不是开关）
    #[test]
    fn team_set_spare_takes_value() {
        let parse = |a: &[&str]| match Cli::try_parse_from(["fleetctl", "team", "set"].iter().chain(a)).map(|c| c.cmd) {
            Ok(Cmd::Team { cmd: super::TeamCmd::Set { team, spare, yes } }) => Ok((team, spare, yes)),
            Ok(_) => panic!("不是 team set"),
            Err(e) => Err(e.kind()),
        };
        assert_eq!(parse(&["备用", "--spare", "true", "--yes"]), Ok(("备用".into(), true, true)));
        assert_eq!(parse(&["备用", "--spare", "false"]), Ok(("备用".into(), false, false)));
        assert!(parse(&["备用", "--spare"]).is_err() && parse(&["备用"]).is_err() && parse(&["备用", "--spare", "yes"]).is_err());
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
