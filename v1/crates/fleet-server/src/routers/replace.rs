//! 替换节点时改软路由：把 OpenClash 源配置里指向旧节点入口的出口（vless 代理块）改成新节点入口的参数，名字、分组、规则都不动。
//! 作业步骤在 jobs/steps/replace.rs，这里是路由上执行的脚本（POSIX sh + awk：OpenWrt 是 busybox ash，awk 也是 busybox 的）。
//!
//! - 参数经脚本开头的 `K='v'` 赋值（`vars`，单引号转义）随 stdin 交给 `sh -s`，不上命令行；凭据（uuid、公钥、short-id）只在中控与路由之间，
//!   脚本不回显它们，作业日志、错误里只写出口名与 server:port。改回也带这组参数（只用来核对「现在仍是本作业写的」）。
//! - 临时文件每次独立：工作目录 `mktemp -d /tmp/fleet-replace.XXXXXX`、配置旁的临时文件 `mktemp $CFG.fleet-tmp.XXXXXX`，退出（含被信号终止）时删。
//!   外层超时后中控杀掉本地 ssh、路由上的旧脚本可能还在跑，它和紧接着的改回不会抢同一个临时文件。
//! - 改的是源配置（`uci get openclash.config.config_path`，OpenClash 重启时由它生成运行配置）。
//!   团队1 旁路由（team2 那台）上的看门服务会用模板重新生成配置，但它以当前配置为准、会自动写回模板，不用特殊处理。
//! - 只改「- name: 出口名」（名字带不带引号都认；name 不在第一行也认）这个列表项里、字段那一层的 server / port / uuid / servername（或 sni），
//!   和 reality-opts 下的 public-key / short-id，一共 6 行；每个出口恰好找到一处、6 个字段各一行，且它现在指向旧入口（或已经是新入口：重跑），
//!   改动行数对得上才写，否则一行不写。流式写法（`- {name: …}`）认不出，按「写法认不出」拒绝。
//! - 写之前备份到 /etc/fleet-backup/openclash/<文件名>.<时间>（时间由中控给，回滚按它找；按文件名里的时间删：早于 KEEP_DAYS 天的、
//!   超出最近 MAX_BACKUPS 份的删，但中控给的 PIN（还在用的作业引用的备份）一份不删）；
//!   写完重启 OpenClash，等 API 起来后经 mihomo 逐个测改过的出口（同采集的 generate_204，各试 3 次）。重启失败、API 不起来、测不通：
//!   脚本自己改回再重启，输出 rolled_back=1 并以非 0 退出。中控对之前已改好的路由按时间戳跑回滚（MODE=rollback）。
//! - 改回只动这次改过的出口：现在还是这次写入的那组参数（server、port、uuid、servername、public-key、short-id 全部对上）的，
//!   那 6 行换回备份里的原值（原样照抄）；已经被别人改走的不动，说明写到 stderr。不整份换回：同一台路由上别的作业（或人）之后改的出口不会被抹掉。备份不在了时，没有出口指向这次改成的地址
//!   就是没什么可改回（rollback=none），有的话报错。
//!
//! 输出（`key=value` 行）：backup=<备份路径>、changed=<改了几行>、delay_<i>=<毫秒>、replace=ok；回滚：rollback=ok | none。
//! 退出码：2 = 没有动配置（认不出、找不到、指向别处、备份失败）；3 = 写了但重启 / API 不行；4 = 测不通。2 以外的都已经尝试改回。

use crate::jobs::scripts::vars;
use std::time::Duration;

/// 一台路由改配置 + 重启 + 等 API（最多 120 秒）+ 测延迟的总时长上限
pub const APPLY_TIMEOUT: Duration = Duration::from_secs(420);
/// 一台路由换回备份并重启
pub const ROLLBACK_TIMEOUT: Duration = Duration::from_secs(240);

/// 一个出口要改成的样子。`old_*` 是它现在应当指向的旧入口（核对用，不改）
#[derive(Clone)]
pub struct Edit {
    pub exit: String,
    pub old_server: String,
    pub old_port: u16,
    pub server: String,
    pub port: u16,
    pub uuid: String,
    pub sni: String,
    pub pbk: String,
    pub sid: String,
}

/// 凭据不打印
impl std::fmt::Debug for Edit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Edit({} {}:{} → {}:{}，凭据…)", self.exit, self.old_server, self.old_port, self.server, self.port)
    }
}

/// 备份的时间戳（文件名里用；中控按它回滚）：UTC，`20261023-073012Z`
pub fn stamp(t: time::OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!("{:04}{:02}{:02}-{:02}{:02}{:02}Z", t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute(), t.second())
}

/// 备份留几天：搬空时一台路由一天改几十次，也要让换回失败、等人工处理的作业来得及处理
pub const KEEP_DAYS: i64 = 7;

/// 备份最多留几份（每份是一整份源配置，最大 1 MiB，路由的闪存有限）：超出的从最早的删起
pub const MAX_BACKUPS: usize = 30;

/// 改配置的脚本。`now` 是这次的时间：备份按它命名（[`stamp`]，回滚按它找），早于它 KEEP_DAYS 天的、超出 MAX_BACKUPS 份的备份删掉；
/// `pins` 是还在用的备份的时间戳（失败 / 需人工处理的作业引用的），一份不删
pub fn apply_script(edits: &[Edit], now: time::OffsetDateTime, pins: &[String]) -> String {
    let mut kv: Vec<(String, String)> = vec![
        ("MODE".into(), "apply".into()),
        ("STAMP".into(), stamp(now)),
        ("KEEP".into(), stamp(now - time::Duration::days(KEEP_DAYS))),
        ("MAXBK".into(), MAX_BACKUPS.to_string()),
        ("PIN".into(), pins.join(" ")),
        ("N".into(), edits.len().to_string()),
        ("DELAY_URL".into(), super::collect::delay_url_param()),
        ("WAIT_API".into(), "120".into()),
        ("PAUSE".into(), "3".into()),
    ];
    for (i, e) in edits.iter().enumerate() {
        let i = i + 1;
        for (k, v) in [
            ("NAME", e.exit.clone()),
            ("OSRV", e.old_server.clone()),
            ("OPORT", e.old_port.to_string()),
            ("SRV", e.server.clone()),
            ("PORT", e.port.to_string()),
            ("UUID", e.uuid.clone()),
            ("SNI", e.sni.clone()),
            ("PBK", e.pbk.clone()),
            ("SID", e.sid.clone()),
        ] {
            kv.push((format!("{k}_{i}"), v));
        }
    }
    let kv: Vec<(&str, &str)> = kv.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    vars(&kv) + SCRIPT
}

/// 把这次改过的出口改回 `stamp` 那份备份里的值并重启 OpenClash。`exits` 是这次写入的样子（`old_*` 不用）：
/// 路由上的出口整组参数都还是它，才算「现在仍是这次写的」
pub fn rollback_script(stamp: &str, exits: &[Edit]) -> String {
    let mut kv: Vec<(String, String)> = vec![("MODE".into(), "rollback".into()), ("STAMP".into(), stamp.into()), ("N".into(), exits.len().to_string())];
    for (i, e) in exits.iter().enumerate() {
        let i = i + 1;
        kv.extend([
            (format!("NAME_{i}"), e.exit.clone()),
            (format!("SRV_{i}"), e.server.clone()),
            (format!("PORT_{i}"), e.port.to_string()),
            (format!("UUID_{i}"), e.uuid.clone()),
            (format!("SNI_{i}"), e.sni.clone()),
            (format!("PBK_{i}"), e.pbk.clone()),
            (format!("SID_{i}"), e.sid.clone()),
        ]);
    }
    let kv: Vec<(&str, &str)> = kv.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    vars(&kv) + SCRIPT
}

/// 读配置（几百 KB 经隧道）的时长上限
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// 读源配置：作业动手前按它重新确定要改哪些出口。顶层 `secret:` 行在路由上先删掉、最多多取 1 字节（同采集的 fetch_config）
pub fn read_script() -> String {
    vars(&[("MODE", "read")])
        + &format!(
            "CFG=$(uci -q get openclash.config.config_path 2>/dev/null || true)\n\
             if [ -z \"$CFG\" ] || [ ! -f \"$CFG\" ]; then echo \"fleet：OpenClash 的源配置（uci openclash.config.config_path）不存在：${{CFG:-没有设置}}\" >&2; exit 2; fi\n\
             sed '/^secret[[:space:]]*:/d' \"$CFG\" | head -c {}\n",
            super::clash::MAX_CONFIG + 1
        )
}

/// 路由上的脚本。整段包在函数里（sh 读完整个函数才执行，里面的命令读 stdin 吃不到后面的脚本），见 collect.rs 的 WRAPPER
pub const SCRIPT: &str = r#"
main() {
set -u
umask 077
W=$(mktemp -d /tmp/fleet-replace.XXXXXX) || exit 2
TMP=
cleanup() { rm -rf "$W"; if [ -n "$TMP" ]; then rm -f "$TMP"; fi; }
trap cleanup EXIT
trap 'exit 1' HUP INT TERM PIPE
say() { echo "fleet：$*" >&2; }
CFG=$(uci -q get openclash.config.config_path 2>/dev/null || true)
if [ -z "$CFG" ] || [ ! -f "$CFG" ]; then say "OpenClash 的源配置（uci openclash.config.config_path）不存在：${CFG:-没有设置}"; exit 2; fi
BKDIR=/etc/fleet-backup/openclash
BASE=$(basename "$CFG")
BK="$BKDIR/$BASE.$STAMP"
# 配置旁的临时文件（每次独立）：建好并复制现有配置的内容与权限
mktmp() { TMP=$(mktemp "$CFG.fleet-tmp.XXXXXX") && cp -p "$CFG" "$TMP"; }
oc_restart() { /etc/init.d/openclash restart >/dev/null 2>&1; }
i=1
while [ "$i" -le "$N" ]; do
  export "NAME_$i" "OSRV_$i" "OPORT_$i" "SRV_$i" "PORT_$i" "UUID_$i" "SNI_$i" "PBK_$i" "SID_$i"
  i=$((i + 1))
done
export N
cat > "$W/edit.awk" <<'FLEET_AWK'
function ind(s,   i) { i = match(s, /[^ ]/); return i ? i - 1 : -1 }
function unq(v,   c, i) {
  sub(/\r$/, "", v); sub(/^[ \t]+/, "", v)
  c = substr(v, 1, 1)
  if (c == "\"" || c == "'") { v = substr(v, 2); i = index(v, c); return i ? substr(v, 1, i - 1) : v }
  sub(/[ \t]+#.*$/, "", v); sub(/[ \t]+$/, "", v)
  return v
}
# YAML 里会被当成数字、布尔、空值或带特殊字符的加单引号（short-id 一律加）
function q(v, force) {
  if (force || v !~ /^[A-Za-z0-9][A-Za-z0-9._-]*$/ || v ~ /^[-+]?[0-9]+(\.[0-9]*)?([eE][-+]?[0-9]+)?$/ || v ~ /^0[xXoObB]/ || tolower(v) ~ /^(true|false|null|yes|no|on|off|y|n)$/)
    return "'" v "'"
  return v
}
function low(s) { return tolower(s) }
function isdash(s, d) { return substr(s, d + 1, 1) == "-" && (length(s) == d + 1 || substr(s, d + 2, 1) == " ") }
# 第 k 行是不是列表项字段那一层的「键: 值」：是则 FK / FV / FP（行首到键之前）
function field(k,   L, j, rest) {
  L = buf[k]; FK = ""
  if (F < 0) return 0
  if (k == 1) { if (length(L) <= F) return 0 }
  else if (ind(L) != F) return 0
  rest = substr(L, F + 1); FP = substr(L, 1, F)
  j = index(rest, ":"); if (!j) return 0
  FK = substr(rest, 1, j - 1); FV = substr(rest, j + 1)
  if (FK !~ /^[A-Za-z0-9_-]+$/) { FK = ""; return 0 }
  return 1
}
function bad(m) { errs = errs "fleet：" m "\n" }
function note(m) { notes = notes "fleet：" m "\n" }
# 改回时出口的 6 个字段按这些名字记（servername 与 sni 记成 sni）
function fkey(k) { return (k == "servername") ? "sni" : k }
function isf(k) { return k == "server" || k == "port" || k == "uuid" || k == "sni" }
function isro(k) { return k == "public-key" || k == "short-id" }
# 备份里出口 i 的 6 个字段：值原样记下（含引号、注释），改回时照抄
function grab(i,   k, L, d, ro, rest, j, key, v) {
  ro = 0
  for (k = 1; k <= cnt; k++) {
    L = buf[k]; sub(/\r$/, "", L)
    if (field(k)) {
      ro = (FK == "reality-opts" && unq(FV) == "")
      v = FV; sub(/\r$/, "", v); key = fkey(FK)
      if (isf(key)) { ov[i, key] = v; og[i, key]++ }
      continue
    }
    d = ind(L)
    if (ro && d > F && L !~ /^[ ]*#/) {
      rest = substr(L, d + 1); j = index(rest, ":"); key = j ? substr(rest, 1, j - 1) : ""
      if (isro(key)) { ov[i, key] = substr(rest, j + 1); og[i, key]++ }
    }
  }
}
# 配置里出口 i 现在还是这次改成的（SRV_i:PORT_i）：6 行换回备份里的值。已经是原值的不动；被别人改走的不动，记一条说明
function restore(i,   k, L, d, ro, rest, j, key, cr, cs, cp, cu, cn, cb, ci, nl, mine, n, K, keep, ch) {
  cs = ""; cp = ""; cu = ""; cn = ""; cb = ""; ci = ""; ro = 0
  for (k = 1; k <= cnt; k++) {
    L = buf[k]
    if (field(k)) {
      ro = (FK == "reality-opts" && unq(FV) == "")
      if (FK == "server") cs = unq(FV)
      else if (FK == "port") cp = unq(FV)
      else if (FK == "uuid") cu = unq(FV)
      else if (FK == "servername" || FK == "sni") cn = unq(FV)
      continue
    }
    d = ind(L)
    if (ro && d > F && L !~ /^[ ]*#/) {
      rest = substr(L, d + 1); j = index(rest, ":"); key = j ? substr(rest, 1, j - 1) : ""
      if (key == "public-key") cb = unq(substr(rest, j + 1))
      else if (key == "short-id") ci = unq(substr(rest, j + 1))
    }
  }
  # 「现在仍是这次写的」：写入的整组参数都对得上（只比 server:port 的话，别的作业之后把同一个地址配上别的凭据也会被换回）
  mine = (low(cs) == low(ENVIRON["SRV_" i]) && cp == ENVIRON["PORT_" i] && low(cu) == low(ENVIRON["UUID_" i]) && cn == ENVIRON["SNI_" i] && cb == ENVIRON["PBK_" i] && ci == ENVIRON["SID_" i])
  if (NOBK) {
    if (mine) bad("改之前的备份不在了，出口「" nm[i] "」现在指向这次改成的 " cs ":" cp "，换不回")
    return
  }
  if (bhits[i] != 1) { bad("改之前的备份里" (bhits[i] ? "有 " bhits[i] " 个" : "没有") "名为「" nm[i] "」的出口，换不回"); return }
  if (low(cs) == low(unq(ov[i, "server"])) && cp == unq(ov[i, "port"])) return
  if (!mine) {
    if (low(cs) == low(ENVIRON["SRV_" i]) && cp == ENVIRON["PORT_" i]) note("出口「" nm[i] "」还指向这次改成的 " cs ":" cp "，但 uuid、servername、public-key 或 short-id 已经不是这次写的（之后又被改过），没有改回")
    else note("出口「" nm[i] "」现在指向 " cs ":" cp "，不是这次改成的 " ENVIRON["SRV_" i] ":" ENVIRON["PORT_" i] "（之后又被改过），没有改回")
    return
  }
  split("server port uuid sni public-key short-id", K, " ")
  for (n = 1; n <= 6; n++) if (og[i, K[n]] != 1) { bad("改之前的备份里出口「" nm[i] "」的写法认不出（" K[n] " 有 " (og[i, K[n]] + 0) " 行），换不回"); return }
  ro = 0; ch = 0; split("", seen)
  for (k = 1; k <= cnt; k++) keep[k] = buf[k]
  for (k = 1; k <= cnt; k++) {
    L = buf[k]; cr = (L ~ /\r$/) ? "\r" : ""; nl = ""
    if (field(k)) {
      ro = (FK == "reality-opts" && unq(FV) == ""); key = fkey(FK)
      if (isf(key)) nl = FP FK ":" ov[i, key] cr
    } else {
      d = ind(L)
      if (ro && d > F && L !~ /^[ ]*#/) {
        rest = substr(L, d + 1); j = index(rest, ":"); key = j ? substr(rest, 1, j - 1) : ""
        if (isro(key)) nl = substr(L, 1, d) key ":" ov[i, key] cr
      }
    }
    if (nl == "") continue
    seen[key]++
    if (nl != buf[k]) { buf[k] = nl; ch++ }
  }
  for (n = 1; n <= 6; n++) if (seen[K[n]] != 1) {
    for (k = 1; k <= cnt; k++) buf[k] = keep[k]
    bad("出口「" nm[i] "」现在的写法认不出（" K[n] " 有 " (seen[K[n]] + 0) " 行），没有改回")
    return
  }
  diff += ch
}
function edit(i,   k, L, d, ro, rest, j, key, cr, cs, cp) {
  ro = 0; cs = ""; cp = ""; split("", seen)
  for (k = 1; k <= cnt; k++) {
    L = buf[k]; cr = (L ~ /\r$/) ? "\r" : ""
    if (field(k)) {
      ro = (FK == "reality-opts" && unq(FV) == "")
      if (FK == "server") { cs = unq(FV); buf[k] = FP "server: " q(ENVIRON["SRV_" i], 0) cr; seen["server"]++ }
      else if (FK == "port") { cp = unq(FV); buf[k] = FP "port: " (ENVIRON["PORT_" i] + 0) cr; seen["port"]++ }
      else if (FK == "uuid") { buf[k] = FP "uuid: " q(ENVIRON["UUID_" i], 0) cr; seen["uuid"]++ }
      else if (FK == "servername" || FK == "sni") { buf[k] = FP FK ": " q(ENVIRON["SNI_" i], 0) cr; seen["sni"]++ }
      continue
    }
    d = ind(L)
    if (ro && d > F && L !~ /^[ ]*#/) {
      rest = substr(L, d + 1); j = index(rest, ":"); key = j ? substr(rest, 1, j - 1) : ""
      if (key == "public-key") { buf[k] = substr(L, 1, d) "public-key: " q(ENVIRON["PBK_" i], 0) cr; seen["pbk"]++ }
      else if (key == "short-id") { buf[k] = substr(L, 1, d) "short-id: " q(ENVIRON["SID_" i], 1) cr; seen["sid"]++ }
    }
  }
  if (seen["server"] != 1 || seen["port"] != 1 || seen["uuid"] != 1 || seen["sni"] != 1 || seen["pbk"] != 1 || seen["sid"] != 1) {
    bad("出口「" nm[i] "」的写法认不出：server、port、uuid、servername、reality-opts 的 public-key 与 short-id 应各有一行，实际 " \
      (seen["server"] + 0) "、" (seen["port"] + 0) "、" (seen["uuid"] + 0) "、" (seen["sni"] + 0) "、" (seen["pbk"] + 0) "、" (seen["sid"] + 0))
    return
  }
  if (!((low(cs) == low(ENVIRON["OSRV_" i]) && cp == ENVIRON["OPORT_" i]) || (low(cs) == low(ENVIRON["SRV_" i]) && cp == ENVIRON["PORT_" i]))) {
    bad("出口「" nm[i] "」现在指向 " cs ":" cp "，不是旧节点的入口（" ENVIRON["OSRV_" i] ":" ENVIRON["OPORT_" i] "），不改")
    return
  }
  changed += 6
}
# 一个列表项读完：是要改的出口就改，原样（或改后）输出
function item(   k, d, d2, j, v, i, who) {
  if (cnt == 0) return
  d = ind(buf[1]); F = -1
  if (isdash(buf[1], d)) { j = match(substr(buf[1], d + 2), /[^ ]/); if (j) F = d + j }
  if (F < 0) for (k = 2; k <= cnt; k++) { d2 = ind(buf[k]); if (d2 > d && buf[k] !~ /^[ ]*#/) { F = d2; break } }
  who = 0
  for (k = 1; k <= cnt; k++) if (field(k) && FK == "name") { v = unq(FV); for (i = 1; i <= n; i++) if (v == nm[i]) who = i }
  if (who && pass == 1) { bhits[who]++; if (bhits[who] == 1) grab(who) }
  else if (who) { hits[who]++; if (hits[who] == 1) { if (REV) restore(who); else edit(who) } }
  if (pass == 2) for (k = 1; k <= cnt; k++) print buf[k]
  cnt = 0
}
# REV=1（改回）：先读备份（第一个文件，只记出口的原值、不输出），再读配置（最后一个文件）输出改回后的样子
BEGIN {
  n = ENVIRON["N"] + 0; for (i = 1; i <= n; i++) { nm[i] = ENVIRON["NAME_" i]; hits[i] = 0; bhits[i] = 0 }
  REV = (ENVIRON["REV"] == "1"); NOBK = REV && ARGV[1] == "/dev/null"; pass = REV ? 1 : 2
  sec = 0; I = -1; cnt = 0; changed = 0; diff = 0; errs = ""; notes = ""
}
{
  np = (!REV || FILENAME == ARGV[ARGC - 1]) ? 2 : 1
  if (np != pass) { if (sec) item(); sec = 0; I = -1; cnt = 0; pass = np }
  L = $0; bare = L; sub(/\r$/, "", bare); d = ind(bare)
  if (!sec) {
    if (pass == 2) print L
    if (bare ~ /^proxies:[ \t]*(#.*)?$/) { sec = 1; I = -1 }
    next
  }
  if (d < 0 || bare ~ /^[ ]*#/) { buf[++cnt] = L; next }
  if (I < 0 && isdash(bare, d)) I = d
  if (I < 0 || d < I || (d == I && !isdash(bare, d))) { item(); sec = 0; if (pass == 2) print L; next }
  if (d == I) item()
  buf[++cnt] = L
}
END {
  if (sec) item()
  for (i = 1; i <= n; i++) {
    if (REV && hits[i] == 0) note("配置里已经没有出口「" nm[i] "」，不用改回")
    else if (hits[i] == 0) bad("配置的 proxies 里没有出口「" nm[i] "」（或它是流式写法 {name: …}，认不出）")
    else if (hits[i] > 1) bad("配置里有 " hits[i] " 个名为「" nm[i] "」的出口，不知道" (REV ? "改回" : "改") "哪个")
  }
  printf("%d", REV ? diff : changed) > ENVIRON["STATF"]
  if (REV) printf("%s", notes) > ENVIRON["NOTEF"]
  if (errs != "") { printf("%s", errs) > ENVIRON["ERRF"]; exit 2 }
}
FLEET_AWK
# 改回这次改过的出口（见 restore）：$1 是改之前的备份，空 = 备份不在了。没有要改的行就不写、不重启
revert() {
  rm -f "$W/err" "$W/stat" "$W/note"
  mktmp || { say "在 $(dirname "$CFG") 里建临时文件失败，没有改回"; return 3; }
  if ! REV=1 ERRF="$W/err" STATF="$W/stat" NOTEF="$W/note" awk -f "$W/edit.awk" "${1:-/dev/null}" "$CFG" > "$TMP"; then
    rm -f "$TMP"; cat "$W/err" >&2 2>/dev/null; say "没有改回"; return 2
  fi
  cat "$W/note" >&2 2>/dev/null
  if [ "$(cat "$W/stat" 2>/dev/null)" = 0 ]; then rm -f "$TMP"; return 0; fi
  mv -f "$TMP" "$CFG" || { rm -f "$TMP"; say "写入 $CFG 失败，没有改回"; return 3; }
  oc_restart || { say "已改回 $CFG，但重启 OpenClash 失败"; return 3; }
}
undo() {
  if revert "$BK"; then echo rolled_back=1; else say "改回原配置也失败了：备份在 $BK，需要人工处理"; fi
}
if [ "$MODE" = rollback ]; then
  if [ -f "$BK" ]; then revert "$BK" || exit $?; echo rollback=ok; exit 0; fi
  revert "" || exit $?
  echo rollback=none
  exit 0
fi
rm -f "$W/err" "$W/stat"
mktmp || { say "在 $(dirname "$CFG") 里建临时文件失败，没有改动"; exit 2; }
if ! ERRF="$W/err" STATF="$W/stat" awk -f "$W/edit.awk" "$CFG" > "$TMP"; then
  rm -f "$TMP"; cat "$W/err" >&2 2>/dev/null; say "配置没有改动"; exit 2
fi
changed=$(cat "$W/stat" 2>/dev/null || true)
if [ "$changed" != "$((N * 6))" ]; then rm -f "$TMP"; say "改动的行数不对（应为 $((N * 6))，实际 ${changed:-0}），配置没有改动"; exit 2; fi
if ! { mkdir -p "$BKDIR" && cp -p "$CFG" "$BK"; }; then rm -f "$TMP"; say "备份 $CFG 失败，配置没有改动"; exit 2; fi
echo "backup=$BK"
# 按文件名里的时间删（cp -p 保留了配置的修改时间，文件时间不能用）：早于 KEEP 的，和超出最近 MAXBK 份的；PIN 里的时间戳一份不删
ls "$BKDIR" 2>/dev/null | awk -v b="$BASE." -v k="$KEEP" -v max="$MAXBK" -v pin=" $PIN " '
  index($0, b) == 1 { n++; f[n] = $0 }
  END { for (i = 1; i <= n; i++) { s = substr(f[i], length(b) + 1); if (!index(pin, " " s " ") && (s < k || i <= n - max)) print f[i] } }
' | while read -r f; do rm -f "$BKDIR/$f"; done
mv -f "$TMP" "$CFG" || { rm -f "$TMP"; say "写入 $CFG 失败，配置没有改动"; exit 2; }
echo "changed=$changed"
if ! oc_restart; then say "改好配置后重启 OpenClash 失败"; undo; exit 3; fi
PORT=$(uci -q get openclash.config.cn_port 2>/dev/null || true)
SECRET=$(uci -q get openclash.config.dashboard_password 2>/dev/null || true)
if [ -n "$SECRET" ]; then printf 'Authorization: Bearer %s\n' "$SECRET" > "$W/auth"; else : > "$W/auth"; fi
api() { curl -sS -m "$1" -H @"$W/auth" "http://127.0.0.1:$PORT$2" 2>&1; }
# 出口名按字节百分号编码（放进 /proxies/<名字>/delay）。有的 OpenWrt 的 busybox 没有 od，用 awk 查字节表
enc() { printf '%s' "$1" | LC_ALL=C awk 'BEGIN { for (i = 1; i < 256; i++) ord[sprintf("%c", i)] = i } { for (j = 1; j <= length($0); j++) printf "%%%02X", ord[substr($0, j, 1)] }'; }
t=0; up=0
while [ "$t" -lt "$WAIT_API" ]; do
  if api 3 /version | grep -q '"version"'; then up=1; break; fi
  sleep 2; t=$((t + 2))
done
if [ "$up" != 1 ]; then say "OpenClash 重启后 $WAIT_API 秒内 API（127.0.0.1:${PORT:-?}）没有起来"; undo; exit 3; fi
i=1
while [ "$i" -le "$N" ]; do
  eval "name=\$NAME_$i"
  e=$(enc "$name"); ms=; try=0
  while [ "$try" -lt 3 ]; do
    ms=$(api 10 "/proxies/$e/delay?url=$DELAY_URL&timeout=5000" | sed -n 's/.*"delay":[ ]*\([0-9][0-9]*\).*/\1/p')
    if [ -n "$ms" ] && [ "$ms" -gt 0 ]; then break; fi
    ms=; try=$((try + 1)); sleep "$PAUSE"
  done
  if [ -z "$ms" ]; then say "出口「$name」换成新入口后测了 3 次都不通"; undo; exit 4; fi
  echo "delay_$i=$ms"
  i=$((i + 1))
done
echo replace=ok
}
main </dev/null
"#;

#[cfg(test)]
mod tests {
    use super::*;

    const UUID_NEW: &str = "22222222-2222-4222-8222-222222222222";

    fn edit(exit: &str) -> Edit {
        Edit {
            exit: exit.into(),
            old_server: "198.51.100.2".into(),
            old_port: 20003,
            server: "198.51.100.2".into(),
            port: 20020,
            uuid: UUID_NEW.into(),
            sni: "www.apple.com".into(),
            pbk: "NEW_PBK-abc_123".into(),
            sid: "01234567".into(),
        }
    }

    /// 出口 节点02：relay.test:20004 → relay.test:20021
    fn edit2() -> Edit { Edit { exit: "节点02".into(), old_server: "relay.test".into(), old_port: 20004, server: "relay.test".into(), port: 20021, ..edit("节点02") } }

    /// 时间戳（[`stamp`] 的格式）对应的时间
    fn at(s: &str) -> time::OffsetDateTime {
        let n = |a: usize, b: usize| s[a..b].parse::<u8>().unwrap();
        let y: i32 = s[..4].parse().unwrap();
        time::Date::from_calendar_date(y, time::Month::try_from(n(4, 6)).unwrap(), n(6, 8)).unwrap().with_hms(n(9, 11), n(11, 13), n(13, 15)).unwrap().assume_utc()
    }

    const S: &str = "20261023-010000Z";

    #[test]
    fn script_carries_parameters_but_debug_hides_creds() {
        let s = apply_script(&[edit("节点01"), Edit { exit: "it's".into(), ..edit("x") }], at("20261023-073012Z"), &["20261020-000001Z".into(), "20261021-000002Z".into()]);
        assert!(s.starts_with("MODE='apply'\nSTAMP='20261023-073012Z'\nKEEP='20261016-073012Z'\nMAXBK='30'\nPIN='20261020-000001Z 20261021-000002Z'\nN='2'\nDELAY_URL='https%3A%2F%2Fwww.gstatic.com%2Fgenerate_204'\n"), "{}", &s[..200]);
        assert!(s.contains("NAME_1='节点01'\n") && s.contains(&format!("UUID_1='{UUID_NEW}'\n")) && s.contains("NAME_2='it'\\''s'\n"));
        assert!(s.trim_end().ends_with("main </dev/null"));
        // 结束标记不能被参数里的内容提前结束；脚本本身不回显凭据
        assert_eq!(SCRIPT.matches("\nFLEET_AWK\n").count(), 1);
        for leak in ["$UUID_", "$PBK_", "$SID_", "$SECRET\""] {
            assert!(!SCRIPT.lines().any(|l| l.trim_start().starts_with("echo") && l.contains(leak)), "{leak}");
        }
        let dbg = format!("{:?}", edit("节点01"));
        assert!(!dbg.contains(UUID_NEW) && !dbg.contains("NEW_PBK") && !dbg.contains("01234567") && dbg.contains("198.51.100.2:20020"), "{dbg}");
        assert_eq!(stamp(time::macros::datetime!(2026-10-23 15:30:12 +8)), "20261023-073012Z");
        let r = rollback_script("S", &[edit("节点01")]);
        assert!(r.starts_with("MODE='rollback'\nSTAMP='S'\nN='1'\nNAME_1='节点01'\nSRV_1='198.51.100.2'\nPORT_1='20020'\nUUID_1='"), "{}", &r[..120]);
        assert!(r.contains("PBK_1='NEW_PBK-abc_123'\nSID_1='01234567'\n"), "改回带整组参数");
        assert!(read_script().starts_with("MODE='read'\nCFG=$(uci -q get openclash.config.config_path") && read_script().ends_with("| head -c 1048577\n"));
    }

    /// 现场的写法：列表项顶格（`- name:` 在第 0 列）、字段缩进 2、reality-opts 下缩进 4；外加带引号的名字、不改的同名字段、注释、CRLF
    #[cfg(unix)]
    const CONFIG: &str = "mixed-port: 7890\n\
proxies:\n\
- name: 节点01\n  type: vless\n  server: 198.51.100.2\n  port: 20003\n  uuid: 11111111-1111-4111-8111-111111111111\n  network: tcp\n  tls: true\n  udp: true\n  flow: xtls-rprx-vision\n  servername: www.cloudflare.com\n  reality-opts:\n    public-key: OLD_PBK\n    short-id: abcd\n  client-fingerprint: chrome\n\
# 第二个：名字带引号、name 不在第一行\n\
- type: vless\n  name: \"节点02\"\n  server: relay.test\n  port: 20004\n  uuid: 11111111-1111-4111-8111-111111111111\n  servername: www.cloudflare.com\n  reality-opts:\n    public-key: OLD_PBK\n    short-id: 'ef'\n\
- name: 别人的\n  type: ss\n  server: 203.0.113.9\n  port: 8388\n  servername: x\n\
proxy-groups:\n\
- name: 节点01组\n  type: select\n  proxies:\n  - 节点01\n\
rules:\n- MATCH,节点01组\n";

    /// 在本机 sh（+ 本机 awk，或 busybox）里真跑：uci / curl 是桩，/etc 与 /tmp/fleet-replace 换到临时目录，OpenClash 的 init 脚本记下调用
    #[cfg(unix)]
    struct Fake {
        dir: tempfile::TempDir,
        busybox: bool,
    }

    #[cfg(unix)]
    impl Fake {
        fn new(config: &str, busybox: bool) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let p = |s: &str| dir.path().join(s);
            std::fs::create_dir_all(p("etc/openclash/config")).unwrap();
            std::fs::create_dir_all(p("etc/init.d")).unwrap();
            std::fs::write(p("etc/openclash/config/c.yaml"), config).unwrap();
            std::fs::set_permissions(p("etc/openclash/config/c.yaml"), std::fs::Permissions::from_mode(0o640)).unwrap();
            std::fs::write(p("etc/init.d/openclash"), format!("#!/bin/sh\necho \"$1 $(md5sum < {}/etc/openclash/config/c.yaml | cut -c1-8)\" >> {}/restart.log\n", dir.path().display(), dir.path().display())).unwrap();
            std::fs::set_permissions(p("etc/init.d/openclash"), std::fs::Permissions::from_mode(0o755)).unwrap();
            Fake { dir, busybox }
        }

        /// 脚本退出后，临时工作目录与配置旁的临时文件都不留
        fn assert_no_leftovers(&self) {
            let names = |d: std::path::PathBuf| std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect::<Vec<_>>();
            assert_eq!(names(self.path("etc/openclash/config")), ["c.yaml"]);
            assert!(!names(self.dir.path().to_path_buf()).iter().any(|n| n.starts_with("w.")), "工作目录没删");
        }

        fn path(&self, s: &str) -> std::path::PathBuf { self.dir.path().join(s) }
        fn read(&self, s: &str) -> String { std::fs::read_to_string(self.path(s)).unwrap_or_default() }

        /// 跑一段脚本。`delay`：出口名 → 延迟（不在里面的测不通）
        fn run(&self, script: &str, delay: &[(&str, u32)]) -> crate::ssh::Output {
            let d = self.dir.path().display().to_string();
            let cases: String = delay.iter().map(|(n, ms)| {
                let enc: String = n.bytes().map(|b| format!("%{b:02X}")).collect();
                format!("*/proxies/{enc}/delay*) echo '{{\"delay\":{ms}}}';;\n")
            }).collect();
            let stubs = format!(
                "uci() {{ case \"$3\" in openclash.config.config_path) echo {d}/etc/openclash/config/c.yaml;; openclash.config.cn_port) echo 9090;; openclash.config.dashboard_password) echo s3cret;; esac; }}\n\
                 curl() {{ for a; do u=$a; done; echo \"$u\" >> {d}/curl.log; case $u in */version) echo '{{\"version\":\"alpha\"}}';;\n{cases} *) echo '{{\"message\":\"timeout\"}}';; esac; }}\n\
                 sleep() {{ :; }}\n{}",
                if self.busybox { "awk() { busybox awk \"$@\"; }\n" } else { "" }
            );
            let body = script.replace("/etc/", &format!("{d}/etc/")).replace("/tmp/fleet-replace", &format!("{d}/w")).replace("PAUSE='3'", "PAUSE='0'");
            let (sh, arg) = if self.busybox { ("busybox", vec!["sh", "-c"]) } else { ("sh", vec!["-c"]) };
            let out = std::process::Command::new(sh).args(arg).arg(format!("{stubs}{body}")).output().unwrap();
            crate::ssh::Output { code: out.status.code().unwrap_or(-1), stdout: String::from_utf8_lossy(&out.stdout).into_owned(), stderr: String::from_utf8_lossy(&out.stderr).into_owned() }
        }
    }

    #[cfg(unix)]
    fn shells() -> Vec<bool> {
        let bb = std::process::Command::new("busybox").arg("awk").arg("BEGIN{}").output().is_ok_and(|o| o.status.success());
        if bb { vec![false, true] } else { vec![false] }
    }

    #[cfg(unix)]
    #[test]
    fn apply_edits_only_the_named_exits() {
        for bb in shells() {
            // 读配置：原样取回，顶层 secret 行不出路由
            let f = Fake::new(&format!("secret: s3cret\n{CONFIG}"), bb);
            let o = f.run(&read_script(), &[]);
            assert_eq!((o.code, o.stdout.as_str()), (0, CONFIG), "busybox={bb} {o:?}");
            let f = Fake::new(CONFIG, bb);
            let o = f.run(&apply_script(&[edit("节点01"), edit2()], at("20261023-073012Z"), &[]), &[("节点01", 88), ("节点02", 95)]);
            assert_eq!(o.code, 0, "busybox={bb} {o:?}");
            assert_eq!((o.kv("changed"), o.kv("delay_1"), o.kv("delay_2"), o.kv("replace")), (Some("12"), Some("88"), Some("95"), Some("ok")), "{o:?}");
            let bk = format!("{}/etc/fleet-backup/openclash/c.yaml.20261023-073012Z", f.dir.path().display());
            assert_eq!(o.kv("backup"), Some(bk.as_str()));
            assert_eq!(std::fs::read_to_string(&bk).unwrap(), CONFIG, "备份是改之前的");
            let want = CONFIG
                .replacen("  port: 20003\n  uuid: 11111111-1111-4111-8111-111111111111", &format!("  port: 20020\n  uuid: {UUID_NEW}"), 1)
                .replacen("  servername: www.cloudflare.com\n  reality-opts:\n    public-key: OLD_PBK\n    short-id: abcd", "  servername: www.apple.com\n  reality-opts:\n    public-key: NEW_PBK-abc_123\n    short-id: '01234567'", 1)
                .replacen("  port: 20004\n  uuid: 11111111-1111-4111-8111-111111111111\n  servername: www.cloudflare.com\n  reality-opts:\n    public-key: OLD_PBK\n    short-id: 'ef'",
                    &format!("  port: 20021\n  uuid: {UUID_NEW}\n  servername: www.apple.com\n  reality-opts:\n    public-key: NEW_PBK-abc_123\n    short-id: '01234567'"), 1);
            assert_eq!(f.read("etc/openclash/config/c.yaml"), want, "busybox={bb}");
            // 名字、分组、规则、别的出口都没动；权限保留；重启了一次；凭据没进输出
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(f.path("etc/openclash/config/c.yaml")).unwrap().permissions().mode() & 0o777, 0o640);
            assert_eq!(f.read("restart.log").lines().count(), 1);
            assert!(!o.stdout.contains(UUID_NEW) && !o.stderr.contains(UUID_NEW) && !o.stdout.contains("NEW_PBK"));
            let curl = f.read("curl.log");
            assert!(curl.contains("http://127.0.0.1:9090/proxies/%E8%8A%82%E7%82%B9%30%31/delay?url=https%3A%2F%2Fwww.gstatic.com%2Fgenerate_204&timeout=5000"), "{curl}");
            // 重跑（已经是新入口）：照样通过，又备份一份
            let o = f.run(&apply_script(&[edit("节点01")], at("20261023-080000Z"), &[]), &[("节点01", 70)]);
            assert_eq!((o.code, o.kv("changed")), (0, Some("6")), "{o:?}");
            assert_eq!(f.read("etc/openclash/config/c.yaml"), want);
            // 改回第一次改之前：这次改过的两个出口换回备份里的原值
            let back = [edit("节点01"), edit2()];
            let o = f.run(&rollback_script("20261023-073012Z", &back), &[]);
            assert_eq!((o.code, o.kv("rollback")), (0, Some("ok")), "{o:?}");
            assert_eq!(f.read("etc/openclash/config/c.yaml"), CONFIG);
            assert_eq!(f.read("restart.log").lines().count(), 3);
            // 已经是原样：不写、不重启；备份不在了、也没有出口指向这次改成的：没什么可改回
            assert_eq!(f.run(&rollback_script("20261023-073012Z", &back), &[]).kv("rollback"), Some("ok"));
            assert_eq!(f.read("restart.log").lines().count(), 3);
            assert_eq!(f.run(&rollback_script("19990101-000000Z", &back), &[]).kv("rollback"), Some("none"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn indented_list_and_crlf() {
        for bb in shells() {
            let cfg = "proxies:\r\n  - name: '节点01'   # 主力\r\n    type: vless\r\n    server: 198.51.100.2\r\n    port: 20003\r\n    uuid: x\r\n    sni: a.com\r\n    reality-opts:\r\n      public-key: P\r\n      short-id: s\r\n  - name: 节点03\r\n    server: 1.1.1.1\r\n    port: 1\r\nrules: []\r\n";
            let f = Fake::new(cfg, bb);
            let o = f.run(&apply_script(&[edit("节点01")], at(S), &[]), &[("节点01", 50)]);
            assert_eq!(o.code, 0, "busybox={bb} {o:?}");
            let want = cfg
                .replace("    port: 20003\r\n    uuid: x\r\n    sni: a.com\r\n", &format!("    port: 20020\r\n    uuid: {UUID_NEW}\r\n    sni: www.apple.com\r\n"))
                .replace("      public-key: P\r\n      short-id: s\r\n", "      public-key: NEW_PBK-abc_123\r\n      short-id: '01234567'\r\n");
            assert_eq!(f.read("etc/openclash/config/c.yaml"), want);
            let o = f.run(&rollback_script(S, &[edit("节点01")]), &[]);
            assert_eq!((o.code, f.read("etc/openclash/config/c.yaml")), (0, cfg.to_string()), "busybox={bb} {o:?}");
        }
    }

    /// 改回只动这次改过、现在还是这次改成的出口：之后别人改走的不动（写明原因）；备份按文件名里的时间留 7 天；
    /// 备份不在了而出口还指向这次改成的：报错、一行不写
    #[cfg(unix)]
    #[test]
    fn revert_keeps_later_edits_by_others() {
        for bb in shells() {
            let f = Fake::new(CONFIG, bb);
            let bk = |s: &str| f.path(&format!("etc/fleet-backup/openclash/{s}"));
            std::fs::create_dir_all(bk("")).unwrap();
            for old in ["c.yaml.20261001-000000Z", "c.yaml.20261020-000000Z", "other.yaml.20261001-000000Z"] {
                std::fs::write(bk(old), "x").unwrap();
            }
            assert_eq!(f.run(&apply_script(&[edit("节点01"), edit2()], at(S), &[]), &[("节点01", 1), ("节点02", 1)]).code, 0);
            assert!(!bk("c.yaml.20261001-000000Z").exists() && bk("c.yaml.20261020-000000Z").exists() && bk("other.yaml.20261001-000000Z").exists());
            // 之后别的作业把 节点02 改去了别处
            let b = Edit { old_server: "relay.test".into(), old_port: 20021, server: "198.51.100.9".into(), port: 20030, uuid: "33333333-3333-4333-8333-333333333333".into(), ..edit("节点02") };
            assert_eq!(f.run(&apply_script(&[b], at("20261023-020000Z"), &[]), &[("节点02", 1)]).code, 0);
            let after_b = f.read("etc/openclash/config/c.yaml");
            let o = f.run(&rollback_script(S, &[edit("节点01"), edit2()]), &[]);
            assert_eq!((o.code, o.kv("rollback")), (0, Some("ok")), "busybox={bb} {o:?}");
            assert!(o.stderr.contains("出口「节点02」现在指向 198.51.100.9:20030，不是这次改成的 relay.test:20021"), "{}", o.stderr);
            let want = after_b
                .replacen(&format!("  port: 20020\n  uuid: {UUID_NEW}"), "  port: 20003\n  uuid: 11111111-1111-4111-8111-111111111111", 1)
                .replacen("  servername: www.apple.com\n  reality-opts:\n    public-key: NEW_PBK-abc_123\n    short-id: '01234567'", "  servername: www.cloudflare.com\n  reality-opts:\n    public-key: OLD_PBK\n    short-id: abcd", 1);
            assert_eq!(f.read("etc/openclash/config/c.yaml"), want, "busybox={bb}");
            assert!(want.contains("port: 20030"));
            // 备份不在了，出口还指向这次改成的
            assert_eq!(f.run(&apply_script(&[edit("节点01")], at("20261023-030000Z"), &[]), &[("节点01", 1)]).code, 0);
            std::fs::remove_file(bk("c.yaml.20261023-030000Z")).unwrap();
            let before = f.read("etc/openclash/config/c.yaml");
            let o = f.run(&rollback_script("20261023-030000Z", &[edit("节点01")]), &[]);
            assert_eq!(o.code, 2, "{o:?}");
            assert!(o.stderr.contains("改之前的备份不在了，出口「节点01」现在指向这次改成的 198.51.100.2:20020"), "{}", o.stderr);
            assert_eq!(f.read("etc/openclash/config/c.yaml"), before);
        }
    }

    /// 备份最多留 MAX_BACKUPS 份（7 天内的也一样，从最早的删起），pins 里的不删；别的配置的备份不动
    #[cfg(unix)]
    #[test]
    fn backups_are_capped_but_pinned_ones_stay() {
        for bb in shells() {
            let f = Fake::new(CONFIG, bb);
            let bk = |s: &str| f.path(&format!("etc/fleet-backup/openclash/{s}"));
            std::fs::create_dir_all(bk("")).unwrap();
            let old = |n: usize| format!("c.yaml.20261020-0000{n:02}Z");
            for n in 0..35 {
                std::fs::write(bk(&old(n)), "x").unwrap();
            }
            std::fs::write(bk("other.yaml.20261020-000000Z"), "x").unwrap();
            let pin = old(1)["c.yaml.".len()..].to_string();
            assert_eq!(f.run(&apply_script(&[edit("节点01")], at(S), &[pin]), &[("节点01", 1)]).code, 0);
            // 35 份旧的 + 这次的 = 36 份：最早的 6 份里没被钉住的 5 份删掉
            let gone = |n: usize| !bk(&old(n)).exists();
            assert!(gone(0) && (2..=5).all(gone) && bk(&old(1)).exists() && bk(&old(6)).exists() && bk(&format!("c.yaml.{S}")).exists());
            assert!(bk("other.yaml.20261020-000000Z").exists());
            assert_eq!(std::fs::read_dir(bk("")).unwrap().count(), 30 + 1 + 1);
        }
    }

    /// 之后别的作业把同一个 server:port 配上了别的凭据：不是本作业写的，不换回（只比 server:port 会把别人的改动换掉）
    #[cfg(unix)]
    #[test]
    fn revert_requires_all_written_params() {
        for bb in shells() {
            let f = Fake::new(CONFIG, bb);
            assert_eq!(f.run(&apply_script(&[edit("节点01")], at(S), &[]), &[("节点01", 1)]).code, 0);
            let other = Edit { old_server: "198.51.100.2".into(), old_port: 20020, uuid: "33333333-3333-4333-8333-333333333333".into(), pbk: "OTHER_PBK".into(), ..edit("节点01") };
            assert_eq!(f.run(&apply_script(&[other], at("20261023-020000Z"), &[]), &[("节点01", 1)]).code, 0);
            let (before, restarts) = (f.read("etc/openclash/config/c.yaml"), f.read("restart.log").lines().count());
            let o = f.run(&rollback_script(S, &[edit("节点01")]), &[]);
            assert_eq!((o.code, o.kv("rollback")), (0, Some("ok")), "busybox={bb} {o:?}");
            assert!(o.stderr.contains("出口「节点01」还指向这次改成的 198.51.100.2:20020，但 uuid、servername、public-key 或 short-id 已经不是这次写的"), "{}", o.stderr);
            assert_eq!((f.read("etc/openclash/config/c.yaml"), f.read("restart.log").lines().count()), (before, restarts), "busybox={bb}");
        }
    }

    /// 认不出、找不到、指向别处、同名两个：一行不写、不备份、不重启
    #[cfg(unix)]
    #[test]
    fn refuses_without_touching_anything() {
        let flow = "proxies:\n  - {name: 节点01, type: vless, server: 198.51.100.2, port: 20003}\n";
        let elsewhere = CONFIG.replace("  server: 198.51.100.2\n  port: 20003", "  server: 203.0.113.50\n  port: 443");
        let dup = CONFIG.replace("name: 别人的", "name: 节点01");
        let no_ro = CONFIG.replace("  reality-opts:\n    public-key: OLD_PBK\n    short-id: abcd\n", "");
        for (cfg, want) in [
            (flow, "没有出口「节点01」"),
            (elsewhere.as_str(), "现在指向 203.0.113.50:443"),
            (dup.as_str(), "有 2 个名为「节点01」"),
            (no_ro.as_str(), "写法认不出"),
            (CONFIG, "没有出口「节点09」"),
        ] {
            for bb in shells() {
                let f = Fake::new(cfg, bb);
                let name = if want.contains("节点09") { "节点09" } else { "节点01" };
                let o = f.run(&apply_script(&[edit(name)], at(S), &[]), &[("节点01", 1)]);
                assert_eq!(o.code, 2, "{want} busybox={bb} {o:?}");
                assert!(o.stderr.contains(want) && o.stderr.contains("配置没有改动"), "{want}: {}", o.stderr);
                assert_eq!(f.read("etc/openclash/config/c.yaml"), cfg);
                assert!(!f.path("etc/fleet-backup").exists() && f.read("restart.log").is_empty());
                f.assert_no_leftovers();
            }
        }
    }

    /// 测不通：自己换回原配置、再重启一次，退出码 4；API 起不来：退出码 3
    #[cfg(unix)]
    #[test]
    fn rolls_back_itself_when_exit_is_dead() {
        for bb in shells() {
            let f = Fake::new(CONFIG, bb);
            let o = f.run(&apply_script(&[edit("节点01")], at(S), &[]), &[]);
            assert_eq!((o.code, o.kv("rolled_back")), (4, Some("1")), "{o:?}");
            assert!(o.stderr.contains("出口「节点01」换成新入口后测了 3 次都不通"), "{}", o.stderr);
            assert_eq!(f.read("etc/openclash/config/c.yaml"), CONFIG);
            let restarts = f.read("restart.log");
            assert_eq!(restarts.lines().count(), 2, "{restarts}");
            assert_eq!(f.read("curl.log").matches("/delay").count(), 3);
            let f = Fake::new(CONFIG, bb);
            let o = f.run(&apply_script(&[edit("节点01")], at(S), &[]).replace("WAIT_API='120'", "WAIT_API='0'"), &[("节点01", 5)]);
            assert_eq!((o.code, o.kv("rolled_back")), (3, Some("1")), "{o:?}");
            assert_eq!(f.read("etc/openclash/config/c.yaml"), CONFIG);
        }
    }
}
