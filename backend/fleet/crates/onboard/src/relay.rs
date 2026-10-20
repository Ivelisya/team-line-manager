//! 线路机接入（P13）：一批节点合成一次变更，经 Xray API 热加（HandlerService AddOutbound/AddInbound、
//! RoutingService AddRule append），全程不重启 Xray；成功后把同样的对象原子落盘到 /etc/fleet/xray/config.json。
//!
//! 顺序（线路机默认转发是 direct，先有入口后有规则会把流量从线路机 IP 放出去）：
//!   全部 AddOutbound → 全部 AddRule(append) → 端口三重检查 → 全部 AddInbound → 运行态核对 → 落盘。
//! 远端脚本在 flock 下执行，并用配置文件 sha256 做 CAS；中途失败只撤销本次加上的对象（逆序），已有对象不碰。
//! 崩溃恢复：运行态已有、文件还没有 → 重跑时跳过 API、只补落盘；两边都有 → 什么也不做（幂等）。

use crate::config::RelayCfg;
use crate::guard;
use crate::model::{ErrClass, StepErr};
use crate::ssh::{sq, Remote};
use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Entry {
    pub row_id: String,
    pub name: String,
    pub port: u16,
    pub inbound: Value,
    pub outbound: Value,
    pub rule: Value,
    pub inbound_tag: String,
    pub outbound_tag: String,
    pub rule_tag: String,
}

pub struct EntrySecrets<'a> {
    pub uuid: &'a str,
    pub private_key: &'a str,
    pub short_id: &'a str,
    pub ss_password: &'a str,
}

pub fn outbound_tag(ip: &str, ss_public_port: u16, nat: bool) -> String {
    let base = format!("ss-{}", ip.replace('.', "-"));
    if nat { format!("{base}-{ss_public_port}") } else { base }
}

/// 入口对象沿用旧 provision.py add_inbound 的参数（xtls-rprx-vision、target/sni www.cloudflare.com、minClientVer 1.8.2、
/// sniffing http/tls/quic），这些决定了与 mihomo 客户端的兼容性，不要改。
pub fn build_entry(rc: &RelayCfg, row_id: &str, name: &str, port: u16, email: &str, ip: &str, ss_public_port: u16, nat: bool, method: &str, s: &EntrySecrets) -> Entry {
    let itag = format!("inbound-{port}");
    let otag = outbound_tag(ip, ss_public_port, nat);
    let rtag = format!("r-inbound-{port}");
    let inbound = json!({
        "listen": "0.0.0.0", "port": port, "protocol": "vless", "tag": itag,
        "settings": {"clients": [{"id": s.uuid, "email": email, "flow": "xtls-rprx-vision"}], "decryption": "none", "fallbacks": []},
        "streamSettings": {"network": "tcp", "security": "reality",
            "tcpSettings": {"acceptProxyProtocol": false, "header": {"type": "none"}},
            "realitySettings": {"show": false, "xver": 0, "target": format!("{}:443", rc.sni), "serverNames": [rc.sni],
                "privateKey": s.private_key, "minClientVer": "1.8.2", "maxClientVer": "", "maxTimeDiff": 0, "shortIds": [s.short_id]}},
        "sniffing": {"enabled": true, "destOverride": ["http", "tls", "quic"], "metadataOnly": false, "routeOnly": false}
    });
    let outbound = json!({"tag": otag, "protocol": "shadowsocks",
        "settings": {"servers": [{"address": ip, "port": ss_public_port, "method": method, "password": s.ss_password}]}});
    let rule = json!({"type": "field", "ruleTag": rtag, "inboundTag": [itag], "outboundTag": otag});
    Entry { row_id: row_id.into(), name: name.into(), port, inbound, outbound, rule, inbound_tag: itag, outbound_tag: otag, rule_tag: rtag }
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub config: Value,
    pub sha: String,
    pub pid: String,
    pub start: String,
    pub uptime: u64,
    pub rt_inbounds: BTreeSet<String>,
    pub rt_outbounds: BTreeSet<String>,
    pub rt_rules: BTreeSet<String>,
    pub listening: BTreeSet<u16>,
}

impl Snapshot {
    pub fn file_ports(&self) -> Vec<u16> {
        self.config["inbounds"].as_array().into_iter().flatten().filter_map(|i| i["port"].as_u64().map(|p| p as u16)).collect()
    }
    pub fn entry_count(&self) -> usize {
        self.config["inbounds"].as_array().map(|a| a.iter().filter(|i| i["tag"] != "api").count()).unwrap_or(0)
    }
    pub fn outbound_targets(&self) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        for o in self.config["outbounds"].as_array().into_iter().flatten() {
            for s in o["settings"]["servers"].as_array().into_iter().flatten() {
                if let (Some(a), Some(p)) = (s["address"].as_str(), s["port"].as_u64()) {
                    m.insert(format!("{a}:{p}"), o["tag"].as_str().unwrap_or("").to_string());
                }
            }
        }
        m
    }
}

pub fn relay_remote(rc: &RelayCfg, ssh_bin: &str) -> Remote {
    Remote {
        ssh_bin: ssh_bin.into(),
        key: rc.ssh_key.clone(),
        known_hosts: rc.known_hosts.clone(),
        host: rc.host.clone(),
        port: rc.ssh_port,
        user: rc.ssh_user.clone(),
        control_dir: Some(crate::ssh::control_dir()),
    }
}

fn head(rc: &RelayCfg) -> String {
    format!(
        "CFG={}; X={}; API={}; ASSET={}\nPID_OF() {{ pgrep -o -f -- \"run -c $CFG\" 2>/dev/null || true; }}\n\
         if [ \"$X\" = auto ]; then XP0=$(PID_OF); X=$(readlink -f /proc/${{XP0:-0}}/exe 2>/dev/null || true); [ -x \"$X\" ] || {{ echo \"==ERR 找不到正在运行的 Xray（run -c $CFG）\"; exit 3; }}; fi\n\
         [ \"$ASSET\" = auto ] && ASSET=$(dirname \"$X\")\n\
         ST_OF() {{ [ -n \"$1\" ] && awk '{{print $22}}' /proc/$1/stat 2>/dev/null || true; }}\n\
         UP_OF() {{ \"$X\" api statssys --server=\"$API\" 2>/dev/null | tr -d ' \\n' | grep -o '\"Uptime\":[0-9]*' | cut -d: -f2 || true; }}\n\
         TAGS() {{ grep -o \"\\\"$1\\\": *\\\"[^\\\"]*\\\"\" | sed 's/.*: *\"//;s/\"$//'; }}\n",
        sq(&rc.config), sq(&rc.xray_bin), sq(&rc.api), sq(&rc.asset_dir)
    )
}

fn sections(out: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    let mut cur: Option<String> = None;
    for l in out.lines() {
        if let Some(k) = l.strip_prefix("==") {
            cur = Some(k.split_whitespace().next().unwrap_or("").to_string());
            m.entry(cur.clone().unwrap()).or_insert_with(String::new);
            if let Some(rest) = k.split_once(' ').map(|x| x.1) {
                let e = m.get_mut(cur.as_ref().unwrap()).unwrap();
                e.push_str(rest);
                e.push('\n');
            }
            continue;
        }
        if let Some(k) = &cur {
            let e = m.get_mut(k).unwrap();
            e.push_str(l);
            e.push('\n');
        }
    }
    m
}

fn set_of(s: Option<&String>) -> BTreeSet<String> {
    s.map(|x| x.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()).unwrap_or_default()
}

/// 只读快照：配置文件（原文）、sha256、Xray 进程号与启动时刻、运行态三类 tag、监听端口。
pub async fn snapshot(r: &Remote, rc: &RelayCfg) -> Result<Snapshot, StepErr> {
    let script = format!(
        "{}set -u\n[ -r \"$CFG\" ] || {{ echo \"==ERR 读不到 $CFG\"; exit 3; }}\n\
         echo ==SHA; sha256sum \"$CFG\" | cut -d' ' -f1\n\
         P=$(PID_OF); echo ==PID; echo \"$P\"; echo ==START; ST_OF \"$P\"; echo; echo ==UPTIME; UP_OF; echo\n\
         echo ==LSI; \"$X\" api lsi --server=\"$API\" --isOnlyTags=true 2>/dev/null | TAGS tag || echo '==APIERR lsi'\n\
         echo ==LSO; \"$X\" api lso --server=\"$API\" 2>/dev/null | TAGS tag || echo '==APIERR lso'\n\
         echo ==LSR; \"$X\" api lsrules --server=\"$API\" 2>/dev/null | TAGS ruleTag || echo '==APIERR lsrules'\n\
         echo ==LISTEN; ss -Htln 2>/dev/null | awk '{{print $4}}' | sed 's/.*://' | sort -un\n\
         echo ==CONFIG; base64 -w0 \"$CFG\"; echo\n",
        head(rc)
    );
    let out = r.run(&script, Duration::from_secs(40)).await?;
    let s = sections(&out.stdout);
    if let Some(e) = s.get("ERR") {
        return Err(StepErr { class: ErrClass::Relay, msg: format!("线路机 {}：{}", rc.name, e.trim()) });
    }
    if !out.ok() {
        return Err(StepErr { class: ErrClass::Relay, msg: format!("线路机 {} 快照失败 rc={}：{}", rc.name, out.code, out.stderr.trim()) });
    }
    if let Some(e) = s.get("APIERR") {
        return Err(StepErr { class: ErrClass::Relay, msg: format!("线路机 {} 的 Xray API（{}）不可用：{}", rc.name, rc.api, e.trim()) });
    }
    let raw = base64::engine::general_purpose::STANDARD
        .decode(s.get("CONFIG").map(|x| x.trim()).unwrap_or(""))
        .map_err(|e| StepErr { class: ErrClass::Relay, msg: format!("配置读回解码失败：{e}") })?;
    let config: Value = serde_json::from_slice(&raw).map_err(|e| StepErr { class: ErrClass::Relay, msg: format!("{} 不是合法 JSON：{e}", rc.config) })?;
    let rt_inbounds = set_of(s.get("LSI"));
    if rt_inbounds.is_empty() {
        return Err(StepErr { class: ErrClass::Relay, msg: format!("线路机 {} 的 Xray 运行态没有任何入口（API 不通或 Xray 没在跑）", rc.name) });
    }
    Ok(Snapshot {
        config,
        sha: s.get("SHA").map(|x| x.trim().to_string()).unwrap_or_default(),
        pid: s.get("PID").map(|x| x.trim().to_string()).unwrap_or_default(),
        start: s.get("START").map(|x| x.trim().to_string()).unwrap_or_default(),
        uptime: s.get("UPTIME").and_then(|x| x.trim().parse().ok()).unwrap_or(0),
        rt_inbounds,
        rt_outbounds: set_of(s.get("LSO")),
        rt_rules: set_of(s.get("LSR")),
        listening: s.get("LISTEN").map(|x| x.lines().filter_map(|l| l.trim().parse().ok()).collect()).unwrap_or_default(),
    })
}

#[derive(Debug)]
pub enum MemberResult {
    Attached,
    Conflict(String),
}

#[derive(Debug)]
pub struct AttachReport {
    pub members: Vec<(String, MemberResult)>,
    pub report: Value,
}

fn b64(v: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(v)
}
fn pretty(v: &Value) -> String {
    // 与 fleet-cutover 的 jdump 一致（indent=2、不转义中文、末尾换行），已有对象的字节不变。
    serde_json::to_string_pretty(v).unwrap() + "\n"
}

fn find_by_tag<'a>(cfg: &'a Value, arr: &str, tag: &str) -> Option<&'a Value> {
    cfg[arr].as_array()?.iter().find(|x| x["tag"].as_str() == Some(tag))
}
fn find_rule<'a>(cfg: &'a Value, rt: &str) -> Option<&'a Value> {
    cfg["routing"]["rules"].as_array()?.iter().find(|x| x["ruleTag"].as_str() == Some(rt))
}

/// 一次线路机批次。crash_after_api=true 只用于演练「API 已加、未落盘」时崩溃。
pub async fn attach_batch(r: &Remote, rc: &RelayCfg, entries: &[Entry], crash_after_api: bool) -> Result<AttachReport, StepErr> {
    let mut last_err = None;
    for cas_try in 0..3 {
        let snap = snapshot(r, rc).await?;
        let mut cand = snap.config.clone();
        let mut members = vec![];
        let mut go: Vec<&Entry> = vec![];
        let targets = snap.outbound_targets();
        for e in entries {
            let fi = find_by_tag(&snap.config, "inbounds", &e.inbound_tag);
            let fo = find_by_tag(&snap.config, "outbounds", &e.outbound_tag);
            let fr = find_rule(&snap.config, &e.rule_tag);
            let conflict = if fi.is_some_and(|x| x != &e.inbound) {
                Some(format!("{} 上已有同名入口 {} 且内容不同", rc.name, e.inbound_tag))
            } else if fo.is_some_and(|x| x != &e.outbound) {
                Some(format!("{} 上已有同名转发 {} 且内容不同", rc.name, e.outbound_tag))
            } else if fr.is_some_and(|x| x != &e.rule) {
                Some(format!("{} 上已有同名规则 {} 且内容不同", rc.name, e.rule_tag))
            } else if fi.is_none() && snap.file_ports().contains(&e.port) {
                Some(format!("端口 {} 已被 {} 上别的入口占用", e.port, rc.name))
            } else if fi.is_none() && !snap.rt_inbounds.contains(&e.inbound_tag) && snap.listening.contains(&e.port) {
                Some(format!("端口 {} 在 {} 上已被别的进程监听", e.port, rc.name))
            } else {
                let addr = format!("{}:{}", e.outbound["settings"]["servers"][0]["address"].as_str().unwrap_or(""), e.outbound["settings"]["servers"][0]["port"]);
                match targets.get(&addr) {
                    Some(t) if t != &e.outbound_tag => Some(format!("{addr} 已经挂在 {} 的转发 {t} 上", rc.name)),
                    _ => None,
                }
            };
            if let Some(c) = conflict {
                members.push((e.row_id.clone(), MemberResult::Conflict(c)));
                continue;
            }
            if fo.is_none() {
                cand["outbounds"].as_array_mut().ok_or_else(|| StepErr { class: ErrClass::Relay, msg: "配置没有 outbounds 数组".into() })?.push(e.outbound.clone());
            }
            if fr.is_none() {
                cand["routing"]["rules"].as_array_mut().ok_or_else(|| StepErr { class: ErrClass::Relay, msg: "配置没有 routing.rules 数组".into() })?.push(e.rule.clone());
            }
            if fi.is_none() {
                cand["inbounds"].as_array_mut().ok_or_else(|| StepErr { class: ErrClass::Relay, msg: "配置没有 inbounds 数组".into() })?.push(e.inbound.clone());
            }
            go.push(e);
        }
        let diff = guard::append_only(&snap.config, &cand).map_err(|m| StepErr { class: ErrClass::Bug, msg: format!("diff 守卫拒绝：{m}") })?;
        let cand_text = pretty(&cand);
        let cand_sha = hex::encode(Sha256::digest(cand_text.as_bytes()));
        let fp_before = guard::inbound_fingerprints(&snap.config);

        let mut sc = head(rc);
        sc.push_str("set -u\numask 077\n");
        sc.push_str(&format!("EXPECT={}\nCRASH={}\n", sq(&snap.sha), if crash_after_api { "1" } else { "0" }));
        sc.push_str(
            "exec 9>\"${FLEET_SPEC_LOCK:-/run/lock/fleet-xray-config.lock}\"; flock -w 60 9 || { echo '==ERR 拿不到线路机配置锁'; exit 20; }\n\
             [ \"$(sha256sum \"$CFG\" | cut -d' ' -f1)\" = \"$EXPECT\" ] || { echo '==CAS'; exit 17; }\n\
             T=$(mktemp -d /dev/shm/fonb.XXXXXX 2>/dev/null || mktemp -d); trap 'rm -rf \"$T\"' EXIT\n",
        );
        sc.push_str(&format!("echo {} | base64 -d > \"$T/cand.json\"\n", b64(cand_text.as_bytes())));
        sc.push_str(
            "XRAY_LOCATION_ASSET=\"$ASSET\" \"$X\" run -test -c \"$T/cand.json\" > \"$T/test.log\" 2>&1 || { echo '==ERR xray -test 不通过'; tail -c 1200 \"$T/test.log\"; exit 18; }\n\
             P0=$(PID_OF); S0=$(ST_OF \"$P0\"); U0=$(UP_OF)\n\
             \"$X\" api lsi --server=\"$API\" --isOnlyTags=true 2>/dev/null | TAGS tag > \"$T/lsi\"\n\
             \"$X\" api lso --server=\"$API\" 2>/dev/null | TAGS tag > \"$T/lso\"\n\
             \"$X\" api lsrules --server=\"$API\" 2>/dev/null | TAGS ruleTag > \"$T/lsr\"\n\
             ADDED=''; SKIPPED=''\n\
             undo() { for a in $(echo $ADDED | tr ' ' '\\n' | tac); do k=${a%%:*}; t=${a#*:}; case $k in\n\
               i) \"$X\" api rmi --server=\"$API\" \"$t\" >/dev/null 2>&1 ;; r) \"$X\" api rmrules --server=\"$API\" \"$t\" >/dev/null 2>&1 ;; o) \"$X\" api rmo --server=\"$API\" \"$t\" >/dev/null 2>&1 ;; esac; echo \"==UNDO $a\"; done; }\n\
             fail() { echo \"==ERR $1\"; cat \"$T/out\" 2>/dev/null | tail -c 600; undo; exit $2; }\n\
             listening() { ss -Htln \"sport = :$1\" 2>/dev/null | grep -q .; }\n",
        );
        // ① 转发
        for e in &go {
            sc.push_str(&format!(
                "if grep -qx -- {t} \"$T/lso\"; then SKIPPED=\"$SKIPPED o:{tr}\"; else echo {j} | base64 -d > \"$T/o.json\"; \"$X\" api ado --server=\"$API\" \"$T/o.json\" > \"$T/out\" 2>&1 || fail 'AddOutbound {tr}' 21; ADDED=\"$ADDED o:{tr}\"; fi\n",
                t = sq(&e.outbound_tag), tr = e.outbound_tag, j = b64(json!({"outbounds": [e.outbound]}).to_string().as_bytes())
            ));
        }
        // ② 规则（一定带 -append：不带会整表替换！）
        for e in &go {
            sc.push_str(&format!(
                "if grep -qx -- {t} \"$T/lsr\"; then SKIPPED=\"$SKIPPED r:{tr}\"; else echo {j} | base64 -d > \"$T/r.json\"; \"$X\" api adrules --server=\"$API\" -append \"$T/r.json\" > \"$T/out\" 2>&1 || fail 'AddRule {tr}' 21; ADDED=\"$ADDED r:{tr}\"; fi\n",
                t = sq(&e.rule_tag), tr = e.rule_tag, j = b64(json!({"routing": {"rules": [e.rule]}}).to_string().as_bytes())
            ));
        }
        // ③ 入口（端口第三重检查：系统监听表）
        for e in &go {
            sc.push_str(&format!(
                "if grep -qx -- {t} \"$T/lsi\"; then SKIPPED=\"$SKIPPED i:{tr}\"; else if listening {p}; then : > \"$T/out\"; fail '端口 {p} 已被占用' 22; fi; echo {j} | base64 -d > \"$T/i.json\"; \"$X\" api adi --server=\"$API\" \"$T/i.json\" > \"$T/out\" 2>&1 || fail 'AddInbound {tr}' 21; ADDED=\"$ADDED i:{tr}\"; fi\n",
                t = sq(&e.inbound_tag), tr = e.inbound_tag, p = e.port, j = b64(json!({"inbounds": [e.inbound]}).to_string().as_bytes())
            ));
        }
        // ④ 运行态核对
        sc.push_str(
            "\"$X\" api lsi --server=\"$API\" --isOnlyTags=true 2>/dev/null | TAGS tag > \"$T/lsi2\"\n\
             \"$X\" api lso --server=\"$API\" 2>/dev/null | TAGS tag > \"$T/lso2\"\n\
             \"$X\" api lsrules --server=\"$API\" 2>/dev/null | TAGS ruleTag > \"$T/lsr2\"\n",
        );
        for e in &go {
            sc.push_str(&format!(
                "grep -qx -- {i} \"$T/lsi2\" && grep -qx -- {o} \"$T/lso2\" && grep -qx -- {r} \"$T/lsr2\" || {{ : > \"$T/out\"; fail '运行态核对 {it}' 23; }}\n\
                 for n in 1 2 3 4 5 6 7 8 9 10; do listening {p} && break; sleep 0.2; done; listening {p} || {{ : > \"$T/out\"; fail '端口 {p} 未监听' 23; }}\n",
                i = sq(&e.inbound_tag), o = sq(&e.outbound_tag), r = sq(&e.rule_tag), it = e.inbound_tag, p = e.port
            ));
        }
        sc.push_str(
            "[ \"$CRASH\" = 1 ] && { echo \"==CRASH API 已加（$ADDED），未落盘\"; exit 99; }\n\
             # ⑤ 落盘：备份 → 同目录临时文件（保持属主/权限）→ 原子改名\n\
             B=\"$(dirname \"$CFG\")/onboard-backups\"; mkdir -p \"$B\"; cp -p \"$CFG\" \"$B/$(basename \"$CFG\").$(date +%Y%m%dT%H%M%S).$$\"\n\
             cp \"$T/cand.json\" \"$CFG.onboard-tmp\"; chmod --reference=\"$CFG\" \"$CFG.onboard-tmp\"; chown --reference=\"$CFG\" \"$CFG.onboard-tmp\" 2>/dev/null || true\n\
             sync \"$CFG.onboard-tmp\" 2>/dev/null || true; mv -f \"$CFG.onboard-tmp\" \"$CFG\"\n\
             P1=$(PID_OF); S1=$(ST_OF \"$P1\"); U1=$(UP_OF)\n\
             echo \"==RESULT {\\\"pid_before\\\":\\\"$P0\\\",\\\"start_before\\\":\\\"$S0\\\",\\\"uptime_before\\\":\\\"$U0\\\",\\\"pid_after\\\":\\\"$P1\\\",\\\"start_after\\\":\\\"$S1\\\",\\\"uptime_after\\\":\\\"$U1\\\",\\\"added\\\":\\\"$ADDED\\\",\\\"skipped\\\":\\\"$SKIPPED\\\",\\\"sha_after\\\":\\\"$(sha256sum \"$CFG\" | cut -d' ' -f1)\\\"}\"\n",
        );
        let out = r.run(&sc, Duration::from_secs(120)).await?;
        let s = sections(&out.stdout);
        match out.code {
            0 => {}
            17 => {
                last_err = Some(StepErr { class: ErrClass::Relay, msg: format!("{} 的配置在读取后被别人改了（CAS 第 {} 次）", rc.name, cas_try + 1) });
                continue;
            }
            99 => {
                return Err(StepErr { class: ErrClass::Bug, msg: format!("CRASH_INJECTED {}", s.get("CRASH").map(|x| x.trim()).unwrap_or("")) });
            }
            c => {
                let msg = s.get("ERR").map(|x| x.trim().to_string()).unwrap_or_else(|| out.stderr.trim().to_string());
                let undone = s.get("UNDO").map(|x| x.trim().replace('\n', " ")).unwrap_or_default();
                return Err(StepErr { class: ErrClass::Relay, msg: format!("{} 接入失败（rc={c}）：{msg}{}", rc.name, if undone.is_empty() { String::new() } else { format!("；已撤销本次加上的 {undone}") }) });
            }
        }
        let res: Value = s.get("RESULT").and_then(|x| serde_json::from_str(x.trim()).ok()).unwrap_or(json!({}));
        // 读回复核：落盘内容 = 候选；已有入口指纹不变；运行态齐全
        let after = snapshot(r, rc).await?;
        let mut problems = vec![];
        if after.sha != cand_sha {
            problems.push(format!("落盘 sha {} ≠ 候选 {}", &after.sha, &cand_sha));
        }
        if let Err(m) = guard::append_only(&snap.config, &after.config) {
            problems.push(format!("读回后 diff 守卫：{m}"));
        }
        let fp_after = guard::inbound_fingerprints(&after.config);
        let changed: Vec<_> = fp_before.iter().filter(|(k, v)| fp_after.get(*k) != Some(v)).map(|(k, _)| k.clone()).collect();
        if !changed.is_empty() {
            problems.push(format!("已有入口指纹变了：{changed:?}"));
        }
        for e in &go {
            if !after.rt_inbounds.contains(&e.inbound_tag) || !after.rt_outbounds.contains(&e.outbound_tag) || !after.rt_rules.contains(&e.rule_tag) {
                problems.push(format!("运行态缺 {}", e.inbound_tag));
            }
        }
        let restarted = res["pid_before"] != res["pid_after"] || res["start_before"] != res["start_after"] || after.pid != snap.pid || after.start != snap.start;
        if !problems.is_empty() {
            return Err(StepErr { class: ErrClass::Relay, msg: format!("{} 接入后复核失败：{}", rc.name, problems.join("；")) });
        }
        for e in &go {
            members.push((e.row_id.clone(), MemberResult::Attached));
        }
        let report = json!({
            "relay": rc.name, "entries_before": snap.entry_count(), "entries_after": after.entry_count(),
            "diff": diff, "config_sha_before": snap.sha, "config_sha_after": after.sha,
            "xray": {"pid_before": snap.pid, "start_before": snap.start, "uptime_before_s": snap.uptime,
                     "pid_after": after.pid, "start_after": after.start, "uptime_after_s": after.uptime, "restarted": restarted},
            "existing_inbounds_unchanged": fp_before.len(), "api_added": res["added"], "api_skipped_already_present": res["skipped"],
            "method": "xray api ado / adrules -append / adi（HandlerService + RoutingService），未重启",
            "xray_reloads": 0, "xray_full_restart": restarted as i32,
        });
        return Ok(AttachReport { members, report });
    }
    Err(last_err.unwrap_or(StepErr { class: ErrClass::Relay, msg: "CAS 重试用完".into() }))
}

/// 补偿：从线路机撤掉一个入口（先删入口，再删规则，最后删转发——入口在时不能先删规则，否则流量会走默认 direct）。
pub async fn detach(r: &Remote, rc: &RelayCfg, e: &Entry) -> Result<Value, StepErr> {
    for _ in 0..3 {
        let snap = snapshot(r, rc).await?;
        let mut cand = snap.config.clone();
        if let Some(a) = cand["inbounds"].as_array_mut() {
            a.retain(|i| i["tag"].as_str() != Some(&e.inbound_tag));
        }
        if let Some(a) = cand["outbounds"].as_array_mut() {
            a.retain(|o| o["tag"].as_str() != Some(&e.outbound_tag));
        }
        if let Some(a) = cand["routing"]["rules"].as_array_mut() {
            a.retain(|x| x["ruleTag"].as_str() != Some(&e.rule_tag));
        }
        guard::remove_only(&snap.config, &cand, &e.inbound_tag, &e.outbound_tag, &e.rule_tag).map_err(|m| StepErr { class: ErrClass::Bug, msg: m })?;
        let text = pretty(&cand);
        let mut sc = head(rc);
        sc.push_str(&format!("set -u\numask 077\nEXPECT={}\n", sq(&snap.sha)));
        sc.push_str(
            "exec 9>\"${FLEET_SPEC_LOCK:-/run/lock/fleet-xray-config.lock}\"; flock -w 60 9 || exit 20\n\
             [ \"$(sha256sum \"$CFG\" | cut -d' ' -f1)\" = \"$EXPECT\" ] || { echo '==CAS'; exit 17; }\n\
             T=$(mktemp -d /dev/shm/fonb.XXXXXX 2>/dev/null || mktemp -d); trap 'rm -rf \"$T\"' EXIT\n",
        );
        sc.push_str(&format!("echo {} | base64 -d > \"$T/cand.json\"\n", b64(text.as_bytes())));
        sc.push_str(&format!(
            "XRAY_LOCATION_ASSET=\"$ASSET\" \"$X\" run -test -c \"$T/cand.json\" >/dev/null 2>&1 || {{ echo '==ERR xray -test 不通过'; exit 18; }}\n\
             \"$X\" api rmi --server=\"$API\" {i} >/dev/null 2>&1 || true\n\
             \"$X\" api rmrules --server=\"$API\" {r} >/dev/null 2>&1 || true\n\
             \"$X\" api rmo --server=\"$API\" {o} >/dev/null 2>&1 || true\n\
             B=\"$(dirname \"$CFG\")/onboard-backups\"; mkdir -p \"$B\"; cp -p \"$CFG\" \"$B/$(basename \"$CFG\").$(date +%Y%m%dT%H%M%S).$$\"\n\
             cp \"$T/cand.json\" \"$CFG.onboard-tmp\"; chmod --reference=\"$CFG\" \"$CFG.onboard-tmp\"; chown --reference=\"$CFG\" \"$CFG.onboard-tmp\" 2>/dev/null || true; mv -f \"$CFG.onboard-tmp\" \"$CFG\"\n\
             echo ==OK\n",
            i = sq(&e.inbound_tag), r = sq(&e.rule_tag), o = sq(&e.outbound_tag)
        ));
        let out = r.run(&sc, Duration::from_secs(60)).await?;
        match out.code {
            0 => return Ok(json!({"relay": rc.name, "removed": [e.inbound_tag, e.rule_tag, e.outbound_tag]})),
            17 => continue,
            c => return Err(StepErr { class: ErrClass::Relay, msg: format!("{} 撤销入口失败 rc={c}：{}", rc.name, out.stdout.trim()) }),
        }
    }
    Err(StepErr { class: ErrClass::Relay, msg: "CAS 重试用完".into() })
}

/// 在线路机上临时起一个 xray（UDS 监听，不占端口、不写固定路径，并发安全），经 SS 转发访问出口核对 URL，返回出口 IP。
pub async fn check_outbound(r: &Remote, rc: &RelayCfg, outbound: &Value, url: &str) -> Result<String, StepErr> {
    let (host, port) = url_host_port(url);
    let mut ob = outbound.clone();
    ob["tag"] = json!("out");
    let cfg = json!({"log": {"loglevel": "warning"},
        "inbounds": [{"listen": "@@SOCK@@", "protocol": "dokodemo-door", "settings": {"address": host, "port": port, "network": "tcp"}, "tag": "in"}],
        "outbounds": [ob]});
    let script = format!(
        "{}set -u\nXP=''; T=$(mktemp -d /dev/shm/fonb-chk.XXXXXX 2>/dev/null || mktemp -d); trap 'kill $XP 2>/dev/null; rm -rf \"$T\"' EXIT\n\
         echo {} | base64 -d | sed \"s#@@SOCK@@#$T/s.sock#\" > \"$T/c.json\"\n\
         \"$X\" run -c \"$T/c.json\" > \"$T/log\" 2>&1 & XP=$!\n\
         for n in $(seq 1 50); do [ -S \"$T/s.sock\" ] && break; sleep 0.1; done\n\
         curl -sS -m 15 --unix-socket \"$T/s.sock\" {} 2>&1 | grep -E '^(ip|loc)=' || {{ echo '==ERR 出口核对请求失败'; tail -c 400 \"$T/log\"; exit 4; }}\n",
        head(rc),
        b64(cfg.to_string().as_bytes()),
        sq(url)
    );
    let out = r.run(&script, Duration::from_secs(40)).await?;
    parse_trace_ip(&out.stdout).ok_or_else(|| StepErr { class: ErrClass::Transient, msg: format!("出口核对没拿到 IP：{}", out.stdout.trim().chars().take(300).collect::<String>()) })
}

pub fn parse_trace_ip(s: &str) -> Option<String> {
    s.lines().find_map(|l| l.trim().strip_prefix("ip=").map(|x| x.trim().to_string()))
}

pub fn url_host_port(url: &str) -> (String, u16) {
    let (scheme, rest) = url.split_once("://").unwrap_or(("https", url));
    let hostport = rest.split('/').next().unwrap_or("");
    match hostport.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => (h.to_string(), p.parse().unwrap()),
        _ => (hostport.to_string(), if scheme == "http" { 80 } else { 443 }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn url_parse() {
        assert_eq!(url_host_port("https://www.cloudflare.com/cdn-cgi/trace"), ("www.cloudflare.com".into(), 443));
        assert_eq!(url_host_port("http://11.99.0.200:8080/"), ("11.99.0.200".into(), 8080));
        assert_eq!(parse_trace_ip("fl=1\nip=1.2.3.4\nts=1"), Some("1.2.3.4".into()));
    }
    #[test]
    fn sections_parse() {
        let s = sections("==SHA\nabc\n==LSI\na\nb\n==ERR boom\nmore\n");
        assert_eq!(s["SHA"].trim(), "abc");
        assert_eq!(set_of(s.get("LSI")).len(), 2);
        assert!(s["ERR"].contains("boom"));
    }
}
