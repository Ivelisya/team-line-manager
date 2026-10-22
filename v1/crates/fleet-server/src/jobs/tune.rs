//! 系统调优档 tune-v1：上架时紧跟系统基线固定执行（converge / converge_relay），已有机器用作业 `tune` 重做。
//! 调优档就是这里的代码常量，跟中控版本走：改任何一项值都升版本号（tune-v2 …），实验室验证、部署中控后 `fleetctl tune --all` 重做；
//! 回滚 = 发新档把值改回去再重做。选值依据与每一项的理由见 docs/DESIGN.md §3「系统调优」。
//!
//! 远端脚本逐键直接写 `/proc/sys/…` 再读回，只把读回一致的键写进 `/etc/sysctl.d/99-zz-fleet-tune.conf`（容器里写不了的键不进文件，
//! 开机时也不报错）。逐键结果就是核对凭据，原样存进步骤结果（`parse`）。

use serde_json::{json, Value};

/// 当前调优档。`machine.tune_profile` 记机器上最近一次成功应用的档；和它不一样就是「旧档」
pub const PROFILE: &str = "tune-v1";

/// 机器的角色：定哪些键适用，线路机另带入口端口池（`relay.port_min-port_max`）
#[derive(Clone, Copy, Debug)]
pub enum Role {
    Node,
    Relay { port_min: i32, port_max: i32 },
}

impl Role {
    /// 本角色在用的服务单元：核对它的 LimitNOFILE
    fn unit(self) -> &'static str {
        match self {
            Role::Node => "fleet-ssserver",
            Role::Relay { .. } => "fleet-xray",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum For {
    Both,
    Node,
    Relay,
}

pub struct Knob {
    pub key: &'static str,
    pub val: &'static str,
    /// 「至少」语义：现值已经不小于目标就保持不动（内核按内存自动给的、别人调过的，不往下改）；否则是「等于」
    pub at_least: bool,
    only: For,
}

const fn k(key: &'static str, val: &'static str, at_least: bool, only: For) -> Knob { Knob { key, val, at_least, only } }

/// 线路机的 ip_local_reserved_ports 目标值是渲染时代入的入口端口池
const RESERVED: &str = "@reserved@";

/// 顺序就是应用顺序：ip_local_reserved_ports 要先于 ip_local_port_range 写
pub const KNOBS: &[Knob] = &[
    k("net.core.default_qdisc", "fq", false, For::Both),
    k("net.ipv4.tcp_congestion_control", "bbr", false, For::Both),
    k("net.core.rmem_max", "16777216", true, For::Both),
    k("net.core.wmem_max", "16777216", true, For::Both),
    k("net.ipv4.tcp_rmem", "4096 131072 16777216", false, For::Both),
    k("net.ipv4.tcp_wmem", "4096 16384 16777216", false, For::Both),
    k("net.ipv4.tcp_notsent_lowat", "131072", false, For::Both),
    k("net.ipv4.tcp_slow_start_after_idle", "0", false, For::Both),
    k("net.ipv4.tcp_mtu_probing", "1", false, For::Both),
    k("net.core.somaxconn", "4096", true, For::Both),
    k("net.ipv4.tcp_max_syn_backlog", "8192", true, For::Both),
    k("net.core.netdev_max_backlog", "16384", true, For::Both),
    k("net.ipv4.tcp_fin_timeout", "30", false, For::Both),
    // 键存在（conntrack 模块已加载）才设；表满会直接丢新连接
    k("net.netfilter.nf_conntrack_max", "131072", true, For::Node),
    k("net.netfilter.nf_conntrack_max", "262144", true, For::Relay),
    k("net.ipv4.ip_local_reserved_ports", RESERVED, false, For::Relay),
    k("net.ipv4.ip_local_port_range", "10240 65535", false, For::Relay),
    k("net.ipv4.tcp_tw_reuse", "1", false, For::Relay),
];

/// 本角色适用的键（目标值已代入）：(符号, 键, 目标值)，符号 `=` / `>=`
pub fn knobs(role: Role) -> Vec<(&'static str, &'static str, String)> {
    KNOBS
        .iter()
        .filter(|x| matches!((x.only, role), (For::Both, _) | (For::Node, Role::Node) | (For::Relay, Role::Relay { .. })))
        .map(|x| {
            let val = match (x.val, role) {
                (RESERVED, Role::Relay { port_min, port_max }) => format!("{port_min}-{port_max}"),
                (v, _) => v.to_string(),
            };
            (if x.at_least { ">=" } else { "=" }, x.key, val)
        })
        .collect()
}

/// 远端脚本模板。键表以 heredoc 嵌入；脚本的 stdin 就是脚本本身（bash -s），会读标准输入的命令一律 `</dev/null`。
/// 只有写文件失败才退出非 0；键缺失 / 写不进、时间没同步上都不失败，记在输出里
const TUNE: &str = r##"
set -u
PS=/proc/sys
CONF=/etc/sysctl.d/99-zz-fleet-tune.conf
ct=0; command -v systemd-detect-virt >/dev/null && systemd-detect-virt -cq && ct=1
modprobe tcp_bbr 2>/dev/null </dev/null || true
install -d -m 755 /etc/sysctl.d || exit 1
printf '%s\n' '# fleet __PROFILE__（中控生成，勿手改）' > $CONF.tmp || exit 1
while read -r op key val; do
  p=$PS/$(echo "$key" | tr . /)
  [ -e "$p" ] || { echo "tune.$key=missing"; continue; }
  cur=$(tr -s ' \t' ' ' < "$p" 2>/dev/null)
  if [ "$cur" != "$val" ] && [ "$op" = '>=' ] && [ "$cur" -ge "$val" ] 2>/dev/null; then echo "tune.$key=kept:$cur"; continue; fi
  st=ok
  if [ "$cur" != "$val" ]; then
    { printf '%s' "$val" > "$p"; } 2>/dev/null
    got=$(tr -s ' \t' ' ' < "$p" 2>/dev/null)
    [ "$got" = "$val" ] || { echo "tune.$key=readonly:$got"; continue; }
    st=applied
  fi
  echo "tune.$key=$st"
  printf '%s = %s\n' "$key" "$val" >> $CONF.tmp || exit 1
done <<'KNOBS'
__KNOBS__
KNOBS
if cmp -s $CONF.tmp $CONF; then rm -f $CONF.tmp; else mv $CONF.tmp $CONF || exit 1; fi
rm -f /etc/sysctl.d/90-fleet-bbr.conf
J=/etc/systemd/journald.conf.d/90-fleet.conf
install -d -m 755 /etc/systemd/journald.conf.d || exit 1
printf '%s\n' '# fleet __PROFILE__（中控生成，勿手改）' '[Journal]' 'SystemMaxUse=300M' > $J.tmp || exit 1
if cmp -s $J.tmp $J; then rm -f $J.tmp; echo journald=same
else mv $J.tmp $J || exit 1; systemctl restart systemd-journald 2>/dev/null </dev/null; echo journald=changed; fi
ntp_ok() { timedatectl show -p NTPSynchronized --value 2>/dev/null </dev/null; }
ntp=container
if [ $ct = 0 ]; then
  if [ "$(ntp_ok)" = yes ]; then ntp=synced
  else
    if ! command -v chronyd >/dev/null && ! command -v ntpd >/dev/null && [ ! -x /lib/systemd/systemd-timesyncd ] && [ ! -x /usr/lib/systemd/systemd-timesyncd ]; then
      export DEBIAN_FRONTEND=noninteractive
      timeout 120 apt-get update -qq >/dev/null 2>&1 </dev/null || true
      timeout 240 apt-get install -y -qq systemd-timesyncd >/dev/null 2>&1 </dev/null || true
    fi
    timedatectl set-ntp true 2>/dev/null </dev/null || true
    ntp=unsynced
    for _ in $(seq 30); do if [ "$(ntp_ok)" = yes ]; then ntp=synced; break; fi; sleep 1; done
  fi
fi
echo "ntp=$ntp"
echo "nofile=$(systemctl show -p LimitNOFILE --value __UNIT__ 2>/dev/null </dev/null)"
echo "mem_mb=$(awk '/MemTotal/{print int($2/1024)}' /proc/meminfo)"
echo "swap_mb=$(awk '/SwapTotal/{print int($2/1024)}' /proc/meminfo)"
echo "container=$ct"
echo "tune_profile=__PROFILE__"
"##;

/// 渲染本角色的调优脚本
pub fn script(role: Role) -> String {
    let table: String = knobs(role).iter().map(|(op, key, val)| format!("{op} {key} {val}\n")).collect();
    TUNE.replace("__PROFILE__", PROFILE).replace("__UNIT__", role.unit()).replace("__KNOBS__", table.trim_end())
}

/// 单个键的结果
enum Status<'a> {
    Ok,
    Applied,
    Kept,
    Missing,
    Readonly(&'a str),
}

fn knob_status(s: &str) -> Option<Status<'_>> {
    Some(match s.split_once(':') {
        None => match s {
            "ok" => Status::Ok,
            "applied" => Status::Applied,
            "missing" => Status::Missing,
            _ => return None,
        },
        Some(("kept", _)) => Status::Kept,
        Some(("readonly", got)) => Status::Readonly(got),
        Some(_) => return None,
    })
}

/// 解析后的脚本输出：`result` 存进步骤结果（可核对的凭据），`warns` 逐条记作业警告
pub struct Report {
    pub result: Value,
    pub warns: Vec<String>,
}

/// 解析脚本输出。结果形如 `{"profile","applied":[…],"ok":[…],"kept":[…],"skipped":[{"key","why":"missing|readonly","got"}],
/// "journald","ntp","nofile","container","mem_mb","swap_mb","bbr"}`；`bbr` = 拥塞控制是否已是 bbr（兼容上架结果里原来的 `bbr` 字段）。
/// 缺失 / 写不进 / 保持不动都不算失败：容器里（`container`）不记警告，非容器机器上写不进的键记一条（可能是供应商限制）
pub fn parse(stdout: &str) -> Report {
    let kv = |name: &str| stdout.lines().rev().find_map(|l| l.strip_prefix(name).and_then(|r| r.strip_prefix('='))).unwrap_or("");
    let container = kv("container") == "1";
    let (mut applied, mut ok, mut kept, mut skipped): (Vec<&str>, Vec<&str>, Vec<&str>, Vec<Value>) = Default::default();
    let (mut readonly, mut cc) = (Vec::new(), None);
    for l in stdout.lines() {
        let Some((key, st)) = l.strip_prefix("tune.").and_then(|r| r.split_once('=')) else { continue };
        let status = knob_status(st);
        if key == "net.ipv4.tcp_congestion_control" {
            cc = Some(matches!(status, Some(Status::Ok | Status::Applied)));
        }
        match status {
            Some(Status::Applied) => applied.push(key),
            Some(Status::Ok) => ok.push(key),
            Some(Status::Kept) => kept.push(key),
            Some(Status::Missing) => skipped.push(json!({"key": key, "why": "missing", "got": null})),
            Some(Status::Readonly(got)) => {
                skipped.push(json!({"key": key, "why": "readonly", "got": got}));
                readonly.push(key);
            }
            None => {}
        }
    }
    let bbr = cc.unwrap_or(false);
    let mut warns = vec![];
    if !bbr {
        warns.push("拥塞控制没能切到 BBR（容器里属正常）".to_string());
    }
    readonly.retain(|k| *k != "net.ipv4.tcp_congestion_control");
    if !container && !readonly.is_empty() {
        warns.push(format!("这台机器上 {} 写不进（可能是供应商限制），保持原值", readonly.join("、")));
    }
    if kv("ntp") == "unsynced" {
        warns.push("系统时钟没有同步：已启用时间同步，30 秒内还没同步上".to_string());
    }
    let nofile = kv("nofile");
    if nofile.parse::<u64>().is_ok_and(|n| n < 65536) {
        warns.push(format!("服务的 LimitNOFILE 是 {nofile}，低于 65536（单元被改过？）"));
    }
    let num = |name: &str| kv(name).parse::<i64>().ok();
    let result = json!({
        "profile": kv("tune_profile"),
        "applied": applied, "ok": ok, "kept": kept, "skipped": skipped,
        "journald": kv("journald"), "ntp": kv("ntp"), "nofile": nofile,
        "container": container, "mem_mb": num("mem_mb"), "swap_mb": num("swap_mb"),
        "bbr": bbr,
    });
    Report { result, warns }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELAY: Role = Role::Relay { port_min: 20000, port_max: 29999 };

    #[test]
    fn relay_script_reserves_the_entry_port_pool_before_the_port_range() {
        let s = script(RELAY);
        let (res, range) = (s.find("= net.ipv4.ip_local_reserved_ports 20000-29999\n").unwrap(), s.find("= net.ipv4.ip_local_port_range 10240 65535\n").unwrap());
        assert!(res < range, "先保留入口端口池，再放宽本地端口范围");
        assert!(s.contains(">= net.netfilter.nf_conntrack_max 262144\n") && !s.contains("nf_conntrack_max 131072"));
        assert!(s.contains("tcp_tw_reuse 1\n") && s.contains("show -p LimitNOFILE --value fleet-xray "));
        assert!(!s.contains("__") && !s.contains(RESERVED));
        // 入口端口池按这台线路机自己的来
        let s = script(Role::Relay { port_min: 21000, port_max: 21999 });
        assert!(s.contains("= net.ipv4.ip_local_reserved_ports 21000-21999\n"));
    }

    #[test]
    fn node_script_has_no_relay_only_keys() {
        let s = script(Role::Node);
        for key in ["ip_local_reserved_ports", "ip_local_port_range", "tcp_tw_reuse", "nf_conntrack_max 262144"] {
            assert!(!s.contains(key), "{key}");
        }
        assert!(s.contains(">= net.netfilter.nf_conntrack_max 131072\n") && s.contains("= net.ipv4.tcp_rmem 4096 131072 16777216\n"));
        assert!(s.contains(">= net.core.rmem_max 16777216\n") && s.contains("show -p LimitNOFILE --value fleet-ssserver "));
        assert!(!s.contains("__"));
    }

    #[test]
    fn knobs_are_unique_per_role() {
        for role in [Role::Node, RELAY] {
            let keys: Vec<_> = knobs(role).iter().map(|x| x.1).collect();
            let mut u = keys.clone();
            u.sort();
            u.dedup();
            assert_eq!(u.len(), keys.len(), "{role:?}");
        }
    }

    #[test]
    fn parse_reports_every_key_and_warns_only_where_it_matters() {
        let out = "tune.net.ipv4.tcp_congestion_control=ok\ntune.net.core.rmem_max=applied\ntune.net.core.somaxconn=kept:65535\n\
                   tune.net.netfilter.nf_conntrack_max=missing\ntune.net.ipv4.tcp_fin_timeout=readonly:\n\
                   journald=changed\nntp=synced\nnofile=65536\nmem_mb=3888\nswap_mb=0\ncontainer=0\ntune_profile=tune-v1\n";
        let r = parse(out);
        let j = &r.result;
        assert_eq!((j["profile"].as_str(), j["bbr"].as_bool(), j["mem_mb"].as_i64(), j["container"].as_bool()), (Some("tune-v1"), Some(true), Some(3888), Some(false)));
        assert_eq!((&j["applied"], &j["ok"], &j["kept"]), (&json!(["net.core.rmem_max"]), &json!(["net.ipv4.tcp_congestion_control"]), &json!(["net.core.somaxconn"])));
        assert_eq!(j["skipped"], json!([{"key": "net.netfilter.nf_conntrack_max", "why": "missing", "got": null}, {"key": "net.ipv4.tcp_fin_timeout", "why": "readonly", "got": ""}]));
        assert_eq!(r.warns, ["这台机器上 net.ipv4.tcp_fin_timeout 写不进（可能是供应商限制），保持原值"]);
        // 容器里：写不进不记警告（BBR 没切上、时钟用宿主机的照常只是结果）
        let r = parse("tune.net.ipv4.tcp_congestion_control=readonly:cubic\ntune.net.ipv4.tcp_fin_timeout=readonly:60\nntp=container\nnofile=1048576\ncontainer=1\ntune_profile=tune-v1\n");
        assert_eq!((r.result["bbr"].as_bool(), r.warns.len()), (Some(false), 1));
        // 时钟没同步上、单元的文件数限制被改低
        let r = parse("ntp=unsynced\nnofile=4096\ncontainer=0\ntune_profile=tune-v1\n");
        assert!(r.warns.iter().any(|w| w.contains("时钟")) && r.warns.iter().any(|w| w.contains("4096")), "{:?}", r.warns);
    }
}
