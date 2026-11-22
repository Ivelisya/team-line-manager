//! 软路由 OpenClash 的另外几种运维（原来运维在 `fleetctl ssh router` 里手写 shell 做的）：
//! - 只重启 OpenClash（作业 router_restart）：不改任何配置，重启、等 API 起来、逐个测 fleet 出口的延迟，报哪些回来了。
//! - 看配置（只读，`GET /routers/{id}/config`、`/config/backups`、`/config/diff`）：经隧道读源配置（同 replace::read_script，顶层 `secret:`
//!   行在路由上先删掉），中控解析出 出口 / 策略组 / 规则 → 出口；列路由上 /etc/fleet-backup/openclash/ 里的备份；当前配置和某份备份逐行比较
//!   （路由上没有 diff，在中控算）。比较前两边都按 [`mask`] 隐去凭据。
//! - 出口改名跟上面板编号（作业 router_rename）与来源 IP 分流规则（作业 router_rules）：都走「中控算、路由上照做」——
//!   作业动手前读源配置，中控按行算出要改的几行（第几行、现在是什么、改成什么，[`Patch`]），改完的整份再用 YAML 解析一遍、和预期的结构逐项核对；
//!   路由上的脚本（[`patch_script`]）只在配置（去掉 secret 行）的 sha256 还是中控读到的那份时动手，逐行核对后替换，替换完的 sha256 必须等于中控算出的，
//!   中间被人改过、awk 行为不同都一行不写。备份、清理旧备份、路由上的锁、后台跑（重启 OpenClash 会掐断隧道）、取回结果都同 routers::replace。
//!   改完重启 OpenClash、等 API 起来、核对（改名：新名字都在、旧名字都没了、select 组按新名字选回原来那个出口；规则：目标出口测得通、规则里有这些网段），
//!   不行整份换回备份再重启（持着路由上的锁，这几秒里没有别的作业写它）。
//!
//! 凭据（uuid、password、公钥、short-id…）不出路由以外的地方：改名、规则只动名字行与规则行；看配置只给出口的 name / type / server / port；
//! 逐行比较的两边先 [`mask`]，凭据换成「<已隐去·标记>」，同一次比较里值相同标记相同、值变了标记不同（标记由这次请求的随机盐算，不能反推）。

use super::clash;
use super::collect::Snapshot;
use super::replace::{lock_vars, stamp, KEEP_DAYS, MAX_BACKUPS, NO_CONFIG, WRAPPER};
use crate::jobs::scripts::vars;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::IpAddr;
use time::OffsetDateTime;
use uuid::Uuid;

// ── 路由上的脚本 ──────────────────────────────────────────────────────────

/// 重启、改配置行共用的开头（在 main 里）：工作目录、mihomo API、按字节百分号编码、等 API、测一个出口的延迟（同 replace 的 SCRIPT）
const PRELUDE: &str = r#"
set -u
umask 077
W=$(mktemp -d /tmp/fleet-replace.XXXXXX) || exit 2
TMP=
cleanup() { rm -rf "$W"; if [ -n "$TMP" ]; then rm -f "$TMP"; fi; }
trap cleanup EXIT
trap 'exit 1' INT TERM
say(){ echo "fleet：$*" >&2; }
oc_restart() { /etc/init.d/openclash restart >/dev/null 2>&1; }
PORT=$(uci -q get openclash.config.cn_port 2>/dev/null || true)
SECRET=$(uci -q get openclash.config.dashboard_password 2>/dev/null || true)
if [ -n "$SECRET" ]; then printf 'Authorization: Bearer %s\n' "$SECRET" > "$W/auth"; else : > "$W/auth"; fi
api() { curl -sS -m "$1" -H @"$W/auth" "http://127.0.0.1:$PORT$2" 2>&1; }
enc() { printf '%s' "$1" | LC_ALL=C awk 'BEGIN { for (i = 1; i < 256; i++) ord[sprintf("%c", i)] = i } { for (j = 1; j <= length($0); j++) printf "%%%02X", ord[substr($0, j, 1)] }'; }
wait_api() { _t=0; while [ "$_t" -lt "$WAIT_API" ]; do if api 3 /version | grep -q '"version"'; then return 0; fi; sleep 2; _t=$((_t + 2)); done; return 1; }
delay_of() {
  _e=$(enc "$1"); _try=0
  while [ "$_try" -lt "$TRIES" ]; do
    _ms=$(api 10 "/proxies/$_e/delay?url=$DELAY_URL&timeout=5000" | sed -n 's/.*"delay":[ ]*\([0-9][0-9]*\).*/\1/p')
    if [ -n "$_ms" ] && [ "$_ms" -gt 0 ]; then echo "$_ms"; return 0; fi
    _try=$((_try + 1)); sleep "$PAUSE"
  done
  return 1
}
"#;

/// 只重启 OpenClash：配置一行不动。输出 restarted=1（重启命令成功了）、delay_<i>=<毫秒> / dead_<i>=1（第 i 个出口测通 / 测了 TRIES 次都不通）、restart=ok。
/// 退出码：2 = 没有重启（路由上没有 OpenClash 的 init 脚本）；3 = 重启命令失败或 API 没起来（OpenClash 可能停着，没有可改回的）；BUSY 同 replace
const RESTART_MAIN: &str = r#"
[ -x /etc/init.d/openclash ] || { say "路由上没有 /etc/init.d/openclash（没装 OpenClash？），没有重启"; exit 2; }
if ! oc_restart; then say "重启 OpenClash 失败（/etc/init.d/openclash restart 出错），OpenClash 可能没在运行"; exit 3; fi
echo restarted=1
if ! wait_api; then say "OpenClash 重启后 $WAIT_API 秒内 API（127.0.0.1:${PORT:-?}）没有起来"; exit 3; fi
i=1
while [ "$i" -le "$N" ]; do
  eval "name=\$NAME_$i"
  if ms=$(delay_of "$name"); then echo "delay_$i=$ms"; else echo "dead_$i=1"; fi
  i=$((i + 1))
done
echo restart=ok
"#;

/// 改配置行：见模块说明。输出 backup=…、changed=<改了几处>、reselected=<组>（按新名字选回去的 select 组）、delay_<i>=…、sha=…、patch=ok | none。
/// 退出码同 replace 的 SCRIPT：2 = 没动配置（sha256 对不上、行对不上、备份失败、API 调不通记不下组的选择）；3 = 写了但重启 / API 不行；4 = 核对不过；3、4 已整份换回
const PATCH_MAIN: &str = r#"
CFG=$(uci -q get openclash.config.config_path 2>/dev/null || true)
if [ -z "$CFG" ] || [ ! -f "$CFG" ]; then say "OpenClash 的源配置（uci openclash.config.config_path）不存在：${CFG:-没有设置}"; exit 2; fi
BKDIR=/etc/fleet-backup/openclash
BASE=$(basename "$CFG")
BK="$BKDIR/$BASE.$STAMP"
emit_sha() { _h=$(sha256sum "$CFG" 2>/dev/null | awk '{print $1}'); [ -z "$_h" ] || echo "sha=$_h"; }
# 中控读到的那份（顶层 secret 行去掉后）的 sha256
view_sha() { sed '/^secret[[:space:]]*:/d' "$1" | sha256sum | awk '{print $1}'; }
cur=$(view_sha "$CFG")
if [ "$cur" = "$AFTER" ]; then say "配置已经是这次要改成的样子，没有改动"; emit_sha; echo patch=none; exit 0; fi
if [ "$cur" != "$BEFORE" ]; then say "配置在中控读过之后又被改过（sha256 对不上），这次没有动配置：重新发起"; exit 2; fi
i=1
while [ "$i" -le "$N" ]; do export "EL_$i" "EX_$i" "EY_$i"; i=$((i + 1)); done
export N
TAB=$(printf '\t')
# select 组现在选的出口要改名的：改完按新名字选回去。$W/sel 每行「组<TAB>新名字<TAB>原来的名字」
: > "$W/sel"
if [ "$NM" -gt 0 ] && [ "$NSG" -gt 0 ]; then
  api 3 /version | grep -q '"version"' || { say "OpenClash 的 API（127.0.0.1:${PORT:-?}）现在调不通，记不下策略组现在选的出口，这次没有动配置"; exit 2; }
  j=1
  while [ "$j" -le "$NSG" ]; do
    eval "g=\$SG_$j"
    now=$(api 5 "/proxies/$(enc "$g")" | sed -n 's/.*"now":"\([^"]*\)".*/\1/p')
    k=1
    while [ "$k" -le "$NM" ]; do
      eval "o=\$MO_$k; nw=\$MN_$k"
      if [ -n "$now" ] && [ "$now" = "$o" ]; then printf '%s\t%s\t%s\n' "$g" "$nw" "$o" >> "$W/sel"; fi
      k=$((k + 1))
    done
    j=$((j + 1))
  done
fi
cat > "$W/patch.awk" <<'FLEET_AWK'
BEGIN { n = ENVIRON["N"] + 0; for (i = 1; i <= n; i++) at[ENVIRON["EL_" i] + 0] = i; ln = 0; done = 0; errs = "" }
# 行号按去掉顶层 secret 行后的算（中控读到的就是去掉后的）；secret 行原样留着
$0 ~ /^secret[[:space:]]*:/ { print; next }
{
  ln++
  i = at[ln]
  if (!i) { print; next }
  cr = ($0 ~ /\r$/) ? "\r" : ""
  L = $0; sub(/\r$/, "", L)
  if ((L "") != (ENVIRON["EX_" i] "")) { errs = errs "fleet：第 " ln " 行现在是「" L "」，不是中控读到的样子\n"; print; next }
  m = split(ENVIRON["EY_" i], Y, "\n")
  for (k = 1; k <= m; k++) print Y[k] cr
  done++
}
END {
  if (done != n) errs = errs "fleet：要改的 " n " 处只核对上 " done " 处\n"
  if (errs != "") { printf("%s", errs) > ENVIRON["ERRF"]; exit 2 }
}
FLEET_AWK
TMP=$(mktemp "$CFG.fleet-tmp.XXXXXX") && cp -p "$CFG" "$TMP" || { say "在 $(dirname "$CFG") 里建临时文件失败，配置没有改动"; exit 2; }
if ! ERRF="$W/err" awk -f "$W/patch.awk" "$CFG" > "$TMP"; then rm -f "$TMP"; cat "$W/err" >&2 2>/dev/null; say "配置没有改动"; exit 2; fi
if [ "$(view_sha "$TMP")" != "$AFTER" ]; then rm -f "$TMP"; say "路由上改出来的配置和中控算的不一样（sha256 对不上），配置没有改动"; exit 2; fi
if ! { mkdir -p "$BKDIR" && cp -p "$CFG" "$BK"; }; then rm -f "$TMP"; say "备份 $CFG 失败，配置没有改动"; exit 2; fi
echo "backup=$BK"
ls "$BKDIR" 2>/dev/null | awk -v b="$BASE." -v k="$KEEP" -v max="$MAXBK" -v pin=" $PIN " '
  index($0, b) == 1 { n++; f[n] = $0 }
  END { for (i = 1; i <= n; i++) { s = substr(f[i], length(b) + 1); if (!index(pin, " " s " ") && (s < k || i <= n - max)) print f[i] } }
' | while read -r f; do rm -f "$BKDIR/$f"; done
mv -f "$TMP" "$CFG" || { rm -f "$TMP"; say "写入 $CFG 失败，配置没有改动"; exit 2; }
TMP=
echo "changed=$N"
now_of() { api 5 "/proxies/$(enc "$1")" | sed -n 's/.*"now":"\([^"]*\)".*/\1/p'; }
sel_put() {
  _b=$(printf '%s' "$2" | sed 's/\\/\\\\/g; s/"/\\"/g')
  curl -sS -m 5 -H @"$W/auth" -X PUT -H 'Content-Type: application/json' -d "{\"name\":\"$_b\"}" "http://127.0.0.1:$PORT/proxies/$(enc "$1")" >/dev/null 2>&1
  [ "$(now_of "$1")" = "$2" ]
}
undo() {
  if cp -p "$BK" "$CFG" && oc_restart; then
    echo rolled_back=1
    if wait_api; then
      while IFS="$TAB" read -r g nw o; do sel_put "$g" "$o" || say "换回后策略组「$g」没能选回「$o」"; done < "$W/sel"
    fi
  else
    say "换回原配置也失败了：备份在 $BK，需要人工处理"
  fi
  emit_sha
}
if ! oc_restart; then say "改好后重启 OpenClash 失败"; undo; exit 3; fi
if ! wait_api; then say "OpenClash 重启后 $WAIT_API 秒内 API（127.0.0.1:${PORT:-?}）没有起来"; undo; exit 3; fi
while IFS="$TAB" read -r g nw o; do
  if sel_put "$g" "$nw"; then echo "reselected=$g"; else say "策略组「$g」原来选着「$o」，改名后没能选回「$nw」"; undo; exit 4; fi
done < "$W/sel"
j=1
while [ "$j" -le "$NX" ]; do
  eval "x=\$CX_$j"
  if ! api 5 "/proxies/$(enc "$x")" | grep -q '"name"'; then say "重启后 OpenClash 里没有「$x」（新配置没有生效）"; undo; exit 4; fi
  j=$((j + 1))
done
j=1
while [ "$j" -le "$NG" ]; do
  eval "x=\$CG_$j"
  if api 5 "/proxies/$(enc "$x")" | grep -q '"name"'; then say "重启后 OpenClash 里还有「$x」（新配置没有生效）"; undo; exit 4; fi
  j=$((j + 1))
done
j=1
while [ "$j" -le "$ND" ]; do
  eval "x=\$CD_$j"
  if ms=$(delay_of "$x"); then echo "delay_$j=$ms"; else say "改完后出口「$x」测了 $TRIES 次都不通"; undo; exit 4; fi
  j=$((j + 1))
done
if [ "$NR" -gt 0 ]; then
  api 10 /rules > "$W/rules"
  j=1
  while [ "$j" -le "$NR" ]; do
    eval "x=\$CR_$j"
    if ! grep -qF "\"$x\"" "$W/rules"; then say "重启后 OpenClash 的规则里没有「$x」（新配置没有生效）"; undo; exit 4; fi
    j=$((j + 1))
  done
fi
emit_sha
echo patch=ok
"#;

/// 拼成一份交给 `sh -s` 的脚本：参数赋值 + 路由上的锁的参数 + `main() { 开头 主体 }` + 外层（replace::WRAPPER：后台跑、结果文件、锁）
fn assemble(kv: &[(String, String)], body: &str) -> String {
    let kv: Vec<(&str, &str)> = kv.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut s = vars(&kv) + &lock_vars();
    s.push_str("\nmain() {\n");
    s.push_str(PRELUDE);
    s.push_str(body);
    s.push_str("}\n");
    s.push_str(WRAPPER);
    s
}

fn kvs(pairs: &[(&str, String)]) -> Vec<(String, String)> { pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect() }

/// 重启脚本：`exits` 是重启后要测延迟的出口（fleet 出口）。`now` 只用来给结果文件起名（取回结果时用同一个）
pub fn restart_script(exits: &[String], now: OffsetDateTime) -> String {
    let mut kv = kvs(&[
        ("MODE", "restart".into()),
        ("STAMP", stamp(now)),
        ("N", exits.len().to_string()),
        ("DELAY_URL", super::collect::delay_url_param()),
        ("WAIT_API", "120".into()),
        ("PAUSE", "3".into()),
        ("TRIES", "3".into()),
    ]);
    kv.extend(exits.iter().enumerate().map(|(i, n)| (format!("NAME_{}", i + 1), n.clone())));
    assemble(&kv, RESTART_MAIN)
}

/// 中控算好的「改哪几行」：`before` / `after` 是改前 / 改后（去掉顶层 secret 行的）配置的 sha256，`text` 是改后的整份（核对、测试用）
#[derive(Debug, Clone)]
pub struct Patch {
    pub edits: Vec<LineEdit>,
    pub before: String,
    pub after: String,
    pub text: String,
}

/// 一处：第 `line` 行（从 1 起，按去掉顶层 secret 行后的算）现在是 `was`，换成 `now`（可以是几行，`\n` 分隔：插在它后面的连它自己一起写）
#[derive(Debug, Clone, PartialEq)]
pub struct LineEdit {
    pub line: usize,
    pub was: String,
    pub now: String,
}

/// 重启后要核对的
#[derive(Debug, Default, Clone)]
pub struct Checks {
    /// 改名：(旧名, 新名)——select 组现在选着旧名的，改完按新名选回去
    pub remap: Vec<(String, String)>,
    /// 配置里的 select 组（要记下现在选的出口的）
    pub select_groups: Vec<String>,
    /// 重启后必须有的出口 / 组名
    pub exist: Vec<String>,
    /// 重启后必须没有的
    pub gone: Vec<String>,
    /// 重启后要测得通的出口
    pub delay: Vec<String>,
    /// 重启后 /rules 里要有的规则内容（网段）
    pub rules: Vec<String>,
}

/// 改配置行的脚本。备份的命名、清理同 replace::apply_script（`pins` 是还在用的备份的时间戳）
pub fn patch_script(p: &Patch, c: &Checks, now: OffsetDateTime, pins: &[String]) -> String {
    let mut kv = kvs(&[
        ("MODE", "patch".into()),
        ("STAMP", stamp(now)),
        ("KEEP", stamp(now - time::Duration::days(KEEP_DAYS))),
        ("MAXBK", MAX_BACKUPS.to_string()),
        ("PIN", pins.join(" ")),
        ("DELAY_URL", super::collect::delay_url_param()),
        ("WAIT_API", "120".into()),
        ("PAUSE", "3".into()),
        ("TRIES", "3".into()),
        ("BEFORE", p.before.clone()),
        ("AFTER", p.after.clone()),
        ("N", p.edits.len().to_string()),
    ]);
    for (i, e) in p.edits.iter().enumerate() {
        let i = i + 1;
        kv.push((format!("EL_{i}"), e.line.to_string()));
        kv.push((format!("EX_{i}"), e.was.clone()));
        kv.push((format!("EY_{i}"), e.now.clone()));
    }
    let mut list = |n: &str, p: &str, xs: &[String]| {
        kv.push((n.to_string(), xs.len().to_string()));
        kv.extend(xs.iter().enumerate().map(|(i, x)| (format!("{p}_{}", i + 1), x.clone())));
    };
    list("NSG", "SG", &c.select_groups);
    list("NX", "CX", &c.exist);
    list("NG", "CG", &c.gone);
    list("ND", "CD", &c.delay);
    list("NR", "CR", &c.rules);
    kv.push(("NM".into(), c.remap.len().to_string()));
    for (i, (o, n)) in c.remap.iter().enumerate() {
        kv.push((format!("MO_{}", i + 1), o.clone()));
        kv.push((format!("MN_{}", i + 1), n.clone()));
    }
    assemble(&kv, PATCH_MAIN)
}

/// 列路由上的备份（/etc/fleet-backup/openclash/ 下的文件）：输出 config=<源配置路径>，每份一行 bk=<文件名> <字节数>。没有源配置以 NO_CONFIG 退出
pub fn backups_script() -> String {
    vars(&[("MODE", "backups")])
        + &format!(
            "CFG=$(uci -q get openclash.config.config_path 2>/dev/null || true)\n\
             if [ -z \"$CFG\" ]; then echo \"fleet：OpenClash 的源配置（uci openclash.config.config_path）没有设置\" >&2; exit {NO_CONFIG}; fi\n\
             echo \"config=$CFG\"\n\
             for f in /etc/fleet-backup/openclash/*; do [ -f \"$f\" ] || continue; echo \"bk=$(basename \"$f\") $(wc -c < \"$f\" | tr -d ' ')\"; done\n"
        )
}

/// 没有这份备份的退出码
pub const NO_BACKUP: i32 = 4;

/// 读一份备份（顶层 secret 行在路由上先删掉，最多多取 1 字节，同 read_script）。`name` 是完整文件名或只有时间戳（按现在的源配置的文件名补全），
/// 先用 [`backup_name_ok`] 核对过
pub fn backup_read_script(name: &str) -> String {
    vars(&[("MODE", "backup"), ("NAME", name)])
        + &format!(
            "CFG=$(uci -q get openclash.config.config_path 2>/dev/null || true)\n\
             case \"$NAME\" in */* | .*) echo \"fleet：备份名不对\" >&2; exit 2 ;; *.*) F=\"/etc/fleet-backup/openclash/$NAME\" ;; *) F=\"/etc/fleet-backup/openclash/$(basename \"${{CFG:-config.yaml}}\").$NAME\" ;; esac\n\
             [ -f \"$F\" ] || {{ echo \"fleet：路由上没有备份 $F（fleetctl router config <路由> --backups 看有哪些）\" >&2; exit {NO_BACKUP}; }}\n\
             sed '/^secret[[:space:]]*:/d' \"$F\" | head -c {}\n",
            clash::MAX_CONFIG + 1
        )
}

/// 备份名：`<文件名>.<时间戳>` 或只有时间戳（[`stamp`] 的格式 20261023-073012Z），只许字母、数字、`._-`，不以 `.` 开头
pub fn backup_name_ok(n: &str) -> bool {
    let tail = n.rsplit('.').next().unwrap_or("");
    let b = tail.as_bytes();
    let stamp_ok = b.len() == 16 && b[8] == b'-' && b[15] == b'Z' && b[..8].iter().chain(&b[9..15]).all(u8::is_ascii_digit);
    stamp_ok && !n.starts_with('.') && n.len() <= 200 && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// `backups_script` 的输出 → [{name, size, stamp, current（是现在这份源配置的备份）}]，新的在前；第二项是源配置路径
pub fn parse_backups(stdout: &str) -> (Vec<Value>, Option<String>) {
    let cfg = stdout.lines().find_map(|l| l.strip_prefix("config=")).map(String::from);
    let base = cfg.as_deref().and_then(|c| c.rsplit('/').next()).unwrap_or("");
    let mut v: Vec<Value> = stdout
        .lines()
        .filter_map(|l| l.strip_prefix("bk="))
        .filter_map(|l| {
            let (name, size) = l.trim_end().rsplit_once(' ')?;
            let name = name.trim_end();
            let stamp = name.rsplit('.').next().filter(|_| backup_name_ok(name))?;
            Some(json!({"name": name, "size": size.trim().parse::<i64>().ok(), "stamp": stamp, "current": !base.is_empty() && name.starts_with(&format!("{base}."))}))
        })
        .collect();
    v.sort_by(|a, b| b["stamp"].as_str().cmp(&a["stamp"].as_str()).then(a["name"].as_str().cmp(&b["name"].as_str())));
    (v, cfg)
}

/// 读路由上的东西的时长上限（几百 KB 经隧道，同 replace::READ_TIMEOUT）
pub const READ_TIMEOUT: std::time::Duration = super::replace::READ_TIMEOUT;

/// 经隧道跑一段只读的脚本（读源配置 / 列备份 / 读备份），返回 stdout。失败返回 (退出码, 原因)：255 是 ssh 自己失败（连不上、登录不了），
/// 别的是脚本自己说的原因（`fleet：` 开头的行）。超过 1 MiB 的不要
pub async fn read_remote(st: &crate::state::AppState, id: Uuid, port: i32, script: &str) -> Result<String, (i32, String)> {
    let o = crate::ssh::exec(super::tunnel::via_tunnel(&st.cfg, id, port), script, READ_TIMEOUT).await.map_err(|e| (-1, format!("{e:#}")))?;
    if o.code == 255 {
        return Err((255, super::tunnel::explain(&o, true)));
    }
    if !o.ok() {
        let own: Vec<&str> = o.stderr.lines().filter_map(|l| l.strip_prefix("fleet：")).collect();
        let why = if own.is_empty() { format!("退出码 {}：{}", o.code, o.stderr.trim().chars().take(200).collect::<String>()) } else { own.join("；") };
        return Err((o.code, why));
    }
    if o.stdout.len() > clash::MAX_CONFIG {
        return Err((0, "配置文件超过 1 MiB，不解析".into()));
    }
    Ok(o.stdout)
}

// ── 按行读 YAML（只认 OpenClash 配置里的这几种写法）──────────────────────────

/// 一行：`bare` 去掉了行尾的 \n 与一个 \r（同 awk 的 sub(/\r$/, "")）
#[derive(Debug, Clone, Copy)]
struct Ln<'a> {
    bare: &'a str,
    cr: bool,
}

fn split_lines(text: &str) -> Vec<Ln<'_>> {
    text.split_inclusive('\n')
        .map(|l| {
            let l = l.strip_suffix('\n').unwrap_or(l);
            match l.strip_suffix('\r') {
                Some(b) => Ln { bare: b, cr: true },
                None => Ln { bare: l, cr: false },
            }
        })
        .collect()
}

/// 第一个不是空格的位置；全是空格（空行）为 None
fn ind(s: &str) -> Option<usize> { s.find(|c| c != ' ') }
fn is_comment(s: &str) -> bool { s.trim_start().starts_with('#') }
fn isdash(s: &str, d: usize) -> bool {
    let b = s.as_bytes();
    b.get(d) == Some(&b'-') && (b.len() == d + 1 || b[d + 1] == b' ')
}

/// 去掉引号与行尾注释（同 awk 的 unq）
fn unq(v: &str) -> String {
    let v = v.trim_start_matches([' ', '\t']);
    match v.chars().next() {
        Some(q @ ('"' | '\'')) => {
            let r = &v[1..];
            r.find(q).map_or(r, |i| &r[..i]).to_string()
        }
        _ => cut_comment(v).trim_end().to_string(),
    }
}

/// 去掉行尾注释（`#` 前面是空白的才算）
fn cut_comment(v: &str) -> &str {
    let b = v.as_bytes();
    (0..b.len()).find(|&i| b[i] == b'#' && i > 0 && (b[i - 1] == b' ' || b[i - 1] == b'\t')).map_or(v, |i| &v[..i])
}

/// 一个标量值的几段：`lead`（前面的空白）+ 值（`quote` 引着的 `inner`）+ `trail`（后面的空白与注释）
#[derive(Debug)]
struct Scalar<'a> {
    lead: &'a str,
    quote: Option<char>,
    inner: &'a str,
    trail: &'a str,
}

/// 认不出（转义、流式写法、锚点等）为 None
fn scalar(v: &str) -> Option<Scalar<'_>> {
    let rest = v.trim_start_matches([' ', '\t']);
    let lead = &v[..v.len() - rest.len()];
    match rest.chars().next() {
        Some(q @ ('"' | '\'')) => {
            let body = &rest[1..];
            let i = body.find(q)?;
            let (inner, trail) = (&body[..i], &body[i + 1..]);
            // 单引号里的 '' 与双引号里的 \ 是转义：不处理
            if (q == '\'' && trail.starts_with('\'')) || (q == '"' && inner.contains('\\')) {
                return None;
            }
            let t = trail.trim_start_matches([' ', '\t']);
            if !(t.is_empty() || t.starts_with('#')) {
                return None;
            }
            Some(Scalar { lead, quote: Some(q), inner, trail })
        }
        Some(c) if "{[&*!|>%@`".contains(c) => None,
        Some(_) => {
            let val = cut_comment(rest).trim_end_matches([' ', '\t']);
            Some(Scalar { lead, quote: None, inner: val, trail: &rest[val.len()..] })
        }
        None => None,
    }
}

/// 不加引号写出来 YAML 还读成同一个字符串
fn plain_ok(v: &str) -> bool {
    let low = v.to_ascii_lowercase();
    !v.is_empty()
        && v.trim() == v
        && !v.starts_with(|c| "-?:,[]{}#&*!|>'\"%@`".contains(c))
        && !v.contains(": ")
        && !v.contains(" #")
        && !v.ends_with(':')
        && !v.contains(['\t', '\n', '\r'])
        && v.parse::<f64>().is_err()
        && !low.starts_with("0x")
        && !low.starts_with("0o")
        && !matches!(low.as_str(), "true" | "false" | "null" | "~" | "yes" | "no" | "on" | "off" | "y" | "n" | ".inf" | ".nan")
}

/// 按原来的引号写回 `new`（原来没引号、新值不加引号读不对的，加单引号）
fn render(s: &Scalar, new: &str) -> String {
    let v = match s.quote {
        Some('\'') => format!("'{}'", new.replace('\'', "''")),
        Some(_) => format!("\"{}\"", new.replace('\\', "\\\\").replace('"', "\\\"")),
        None if plain_ok(new) => new.to_string(),
        None => format!("'{}'", new.replace('\'', "''")),
    };
    format!("{}{v}{}", s.lead, s.trail)
}

fn word_char(c: char) -> bool { c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') }

/// `w` 作为一个完整的词在 `s` 里出现几次（前后不是字母、数字、`_ . -`，同 remove 脚本的 refers）
fn refs(s: &str, w: &str) -> usize {
    if w.is_empty() {
        return 0;
    }
    let (mut n, mut from) = (0, 0);
    while let Some(p) = s[from..].find(w) {
        let i = from + p;
        let before = s[..i].chars().next_back();
        let after = s[i + w.len()..].chars().next();
        if !before.is_some_and(word_char) && !after.is_some_and(word_char) {
            n += 1;
        }
        from = i + w.len();
    }
    n
}

/// 顶层的一段：键、键所在的行、段的结束（不含）
struct Block {
    key: String,
    head: usize,
    end: usize,
}

fn blocks(ls: &[Ln]) -> Vec<Block> {
    let mut out: Vec<Block> = vec![];
    for (i, l) in ls.iter().enumerate() {
        let s = l.bare;
        if ind(s) == Some(0) && !is_comment(s) && !isdash(s, 0) {
            if let Some(b) = out.last_mut() {
                b.end = i;
            }
            let key = s.split(':').next().unwrap_or("").trim().trim_matches(['"', '\'']).to_string();
            out.push(Block { key, head: i, end: ls.len() });
        }
    }
    out
}

/// 段里的列表项：每项 (第一行, 结束（不含）)。列表的缩进按第一个 `- ` 定；比它浅的非注释行之后不再算
fn items(ls: &[Ln], b: &Block) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = vec![];
    let mut col: Option<usize> = None;
    for (i, l) in ls.iter().enumerate().take(b.end).skip(b.head + 1) {
        let s = l.bare;
        let Some(d) = ind(s) else { continue };
        if is_comment(s) {
            continue;
        }
        if col.is_none() && isdash(s, d) {
            col = Some(d);
        }
        let Some(c) = col else { continue };
        if d == c && isdash(s, d) {
            if let Some(last) = out.last_mut() {
                last.1 = i;
            }
            out.push((i, b.end));
        } else if d < c {
            if let Some(last) = out.last_mut() {
                last.1 = i;
            }
            break;
        }
    }
    out
}

/// 列表项里字段那一层的缩进（`- name: x` 的 name 那一列，或第一行只有 `-` 时下一行的缩进）
fn field_col(ls: &[Ln], (start, end): (usize, usize)) -> Option<usize> {
    let s = ls[start].bare;
    let d = ind(s)?;
    let after = &s[d + 1..];
    match after.find(|c| c != ' ') {
        Some(j) => Some(d + 1 + j),
        None => (start + 1..end).find_map(|k| ind(ls[k].bare).filter(|&x| x > d && !is_comment(ls[k].bare))),
    }
}

/// 第 k 行是不是字段那一层的「键: 值」：(键, 值（冒号之后的原文）, 值在行里的起点)
fn field<'a>(ls: &[Ln<'a>], k: usize, start: usize, col: usize) -> Option<(&'a str, &'a str, usize)> {
    let s = ls[k].bare;
    if is_comment(s) || s.len() <= col {
        return None;
    }
    if k != start && ind(s) != Some(col) {
        return None;
    }
    let rest = &s[col..];
    let j = rest.find(':')?;
    let key = &rest[..j];
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    Some((key, &rest[j + 1..], col + j + 1))
}

/// 规则的几段（逗号分隔，去掉两边空白）：(类型, 目标的下标)。MATCH 的目标是第二段；别的是最后一段，去掉 no-resolve、src 这类选项
fn rule_target(parts: &[&str]) -> Option<usize> {
    if parts.len() < 2 {
        return None;
    }
    if parts[0].trim().eq_ignore_ascii_case("MATCH") {
        return Some(1);
    }
    let mut m = parts.len() - 1;
    while m > 1 && matches!(parts[m].trim().to_ascii_lowercase().as_str(), "no-resolve" | "src") {
        m -= 1;
    }
    Some(m)
}

/// 规则 `rule` 的目标换成 `new`（那一段两边的空白原样）；没有目标为 None
pub fn retarget_rule(rule: &str, new: &str) -> Option<String> {
    let parts: Vec<&str> = rule.split(',').collect();
    let m = rule_target(&parts)?;
    let p = parts[m];
    let core = p.trim();
    let lead = &p[..p.len() - p.trim_start().len()];
    let tail = &p[p.trim_end().len()..];
    if core.is_empty() {
        return None;
    }
    let mut out: Vec<String> = parts.iter().map(|s| s.to_string()).collect();
    out[m] = format!("{lead}{new}{tail}");
    Some(out.join(","))
}

/// 规则拆开：{type, payload, target, options}
fn rule_view(rule: &str) -> (String, String, String, Vec<String>) {
    let parts: Vec<&str> = rule.split(',').collect();
    let kind = parts.first().map(|s| s.trim().to_string()).unwrap_or_default();
    match rule_target(&parts) {
        Some(m) => (kind, parts[1..m].iter().map(|s| s.trim()).collect::<Vec<_>>().join(","), parts[m].trim().to_string(), parts[m + 1..].iter().map(|s| s.trim().to_string()).collect()),
        None => (kind, String::new(), String::new(), vec![]),
    }
}

fn sha_hex(s: &str) -> String { hex::encode(crate::crypto::sha256(s.as_bytes())) }

/// 按 `edits`（行下标 → 改成的内容）拼出改后的整份：同 patch 脚本里的 awk，每行都以 \n 结尾，原来是 CRLF 的行每行补 \r
fn apply(ls: &[Ln], edits: &BTreeMap<usize, String>) -> String {
    let mut out = String::new();
    for (i, l) in ls.iter().enumerate() {
        let cr = if l.cr { "\r" } else { "" };
        match edits.get(&i) {
            Some(n) => {
                for y in n.split('\n') {
                    out.push_str(y);
                    out.push_str(cr);
                    out.push('\n');
                }
            }
            None => {
                out.push_str(l.bare);
                out.push_str(cr);
                out.push('\n');
            }
        }
    }
    out
}

fn to_patch(text: &str, ls: &[Ln], edits: BTreeMap<usize, String>) -> Patch {
    let new = apply(ls, &edits);
    Patch {
        edits: edits.iter().map(|(&i, n)| LineEdit { line: i + 1, was: ls[i].bare.to_string(), now: n.clone() }).collect(),
        before: sha_hex(text),
        after: sha_hex(&new),
        text: new,
    }
}

/// 整份按通用 YAML 读成 JSON 值（核对改前改后的结构用）
fn yaml_value(text: &str) -> Result<Value, String> {
    serde_saphyr::from_str::<Value>(text).map_err(|e| format!("配置文件解析不了：{}", e.to_string().lines().next().unwrap_or("").chars().take(200).collect::<String>()))
}

// ── 出口改名 ──────────────────────────────────────────────────────────────

/// 一个出口改名：`server:port` 是中控核对过的它现在指向的（执行时配置里同名的出口还指向这里才改）
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Rename {
    pub old: String,
    pub new: String,
    pub server: String,
    pub port: u16,
}

/// 改名要改的行：`proxies` 里出口的 `name:`（与别的出口的 `dialer-proxy:`）、`proxy-groups` 各组 `proxies:` 列表里的成员、`rules` 的目标。
/// 几个名字可以互相换（07→04、10→07 一次改完）。认不出、改了会出问题的都返回原因、一行不改：配置里别处还提到这些名字（dns 的 `#名字`、
/// 流式写法、注释以外的任何地方）、一行里提到不止一个、出口不止一个同名或找不到、指向的不是核对过的地址、新名字已经有出口或策略组在用。
/// 改完的整份解析一遍，和「改前的结构按名字换过」逐项比，不一样也不改。返回 (要改的行, 配置里的 select 组)
pub fn rename_patch(text: &str, map: &[Rename]) -> Result<(Patch, Vec<String>), Vec<String>> {
    let mut errs: Vec<String> = vec![];
    let olds: HashMap<&str, &Rename> = map.iter().map(|r| (r.old.as_str(), r)).collect();
    let news: HashSet<&str> = map.iter().map(|r| r.new.as_str()).collect();
    if olds.len() != map.len() || news.len() != map.len() || map.iter().any(|r| r.old == r.new || r.new.trim().is_empty()) {
        return Err(vec!["改名的对应不对（旧名或新名重复、没变、为空）".into()]);
    }
    let names: Vec<&str> = olds.keys().copied().chain(news.iter().copied()).collect::<HashSet<_>>().into_iter().collect();
    let ls = split_lines(text);
    let bs = blocks(&ls);
    let mut edits: BTreeMap<usize, String> = BTreeMap::new();
    let mut handled: HashSet<usize> = HashSet::new();
    let mut hits: HashMap<&str, usize> = HashMap::new();
    let mut groups_select: Vec<String> = vec![];
    let line_no = |i: usize| i + 1;
    for b in &bs {
        let inline = ls[b.head].bare.split_once(':').map(|x| x.1).unwrap_or("");
        let inline = unq(inline);
        if !inline.is_empty() && matches!(b.key.as_str(), "proxies" | "proxy-groups" | "rules") && names.iter().any(|n| refs(&inline, n) > 0) {
            errs.push(format!("顶层的 {} 是流式写法（第 {} 行），认不出", b.key, line_no(b.head)));
            continue;
        }
        match b.key.as_str() {
            "proxies" => {
                for it in items(&ls, b) {
                    let Some(col) = field_col(&ls, it) else { continue };
                    if ls[it.0].bare[col..].starts_with('{') {
                        continue; // 流式写法：提到名字的由最后的检查拦下
                    }
                    let (mut name, mut srv, mut port) = (None, None, None);
                    for k in it.0..it.1 {
                        let Some((key, val, at)) = field(&ls, k, it.0, col) else { continue };
                        match key {
                            "name" => name = Some((k, val, at)),
                            "server" => srv = Some(unq(val)),
                            "port" => port = unq(val).parse::<u16>().ok(),
                            "dialer-proxy" => {
                                if let Some(r) = olds.get(unq(val).as_str()) {
                                    match scalar(val) {
                                        Some(s) => {
                                            edits.insert(k, format!("{}{}", &ls[k].bare[..at], render(&s, &r.new)));
                                            handled.insert(k);
                                        }
                                        None => errs.push(format!("第 {} 行的 dialer-proxy 写法认不出", line_no(k))),
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    let Some((k, val, at)) = name else { continue };
                    let v = unq(val);
                    if let Some(r) = olds.get(v.as_str()) {
                        *hits.entry(r.old.as_str()).or_default() += 1;
                        let addr_ok = srv.as_deref().is_some_and(|s| clash::host_eq(s, &r.server)) && port == Some(r.port);
                        if !addr_ok {
                            errs.push(format!(
                                "出口「{v}」现在指向 {}:{}，不是核对过的 {}:{}（之后又被改过）：重新发起",
                                srv.as_deref().unwrap_or("?"),
                                port.map_or("?".into(), |p| p.to_string()),
                                r.server,
                                r.port
                            ));
                            continue;
                        }
                        match scalar(val) {
                            Some(s) => {
                                edits.insert(k, format!("{}{}", &ls[k].bare[..at], render(&s, &r.new)));
                                handled.insert(k);
                            }
                            None => errs.push(format!("第 {} 行出口「{v}」的名字写法认不出（转义、锚点…）", line_no(k))),
                        }
                    } else if news.contains(v.as_str()) {
                        errs.push(format!("已经有出口叫「{v}」（不在这次改名里）：两个同名就分不清了，先处理它"));
                    }
                }
            }
            "proxy-groups" => {
                for it in items(&ls, b) {
                    let Some(col) = field_col(&ls, it) else { continue };
                    if ls[it.0].bare[col..].starts_with('{') {
                        continue;
                    }
                    let mut gname = String::new();
                    let mut gtype = String::new();
                    let mut members: Option<usize> = None;
                    for k in it.0..it.1 {
                        let s = ls[k].bare;
                        let Some(d) = ind(s) else { continue };
                        if is_comment(s) {
                            continue;
                        }
                        if let Some(pl) = members {
                            if isdash(s, d) && d >= pl {
                                let v = &s[d + 1..];
                                if let Some(r) = olds.get(unq(v).as_str()) {
                                    match scalar(v) {
                                        Some(sc) => {
                                            edits.insert(k, format!("{}{}", &s[..d + 1], render(&sc, &r.new)));
                                            handled.insert(k);
                                        }
                                        None => errs.push(format!("第 {} 行策略组成员的写法认不出", line_no(k))),
                                    }
                                }
                                continue;
                            }
                            if d <= pl {
                                members = None;
                            }
                        }
                        let Some((key, val, _)) = field(&ls, k, it.0, col) else { continue };
                        match key {
                            "name" => gname = unq(val),
                            "type" => gtype = unq(val).to_ascii_lowercase(),
                            "proxies" if unq(val).is_empty() => members = Some(col),
                            _ => {}
                        }
                    }
                    if names.contains(&gname.as_str()) {
                        errs.push(format!("有个策略组也叫「{gname}」：和出口同名，改名会弄混，先在路由上改掉"));
                    }
                    if gtype == "select" && !gname.is_empty() {
                        groups_select.push(gname);
                    }
                }
            }
            "rules" => {
                for it in items(&ls, b) {
                    let k = it.0;
                    let s = ls[k].bare;
                    let Some(d) = ind(s) else { continue };
                    let v = &s[d + 1..];
                    let rule = unq(v);
                    let parts: Vec<&str> = rule.split(',').collect();
                    let Some(m) = rule_target(&parts) else { continue };
                    let Some(r) = olds.get(parts[m].trim()) else { continue };
                    match scalar(v).filter(|sc| sc.inner == rule) {
                        Some(sc) => match retarget_rule(sc.inner, &r.new) {
                            Some(n) => {
                                edits.insert(k, format!("{}{}", &s[..d + 1], render(&sc, &n)));
                                handled.insert(k);
                            }
                            None => errs.push(format!("第 {} 行的规则认不出目标", line_no(k))),
                        },
                        None => errs.push(format!("第 {} 行的规则写法认不出（转义、多行…）", line_no(k))),
                    }
                }
            }
            _ => {}
        }
    }
    for r in map {
        match hits.get(r.old.as_str()).copied().unwrap_or(0) {
            1 => {}
            0 => errs.push(format!("配置的 proxies 里没有出口「{}」（或它是流式写法 {{name: …}}，认不出）", r.old)),
            n => errs.push(format!("配置里有 {n} 个名为「{}」的出口，不知道改哪个", r.old)),
        }
    }
    // 别处还提到这些名字（dns 的 #名字、流式写法、一行里提到两个…）：认不出怎么改，不改
    let section = |i: usize| bs.iter().rev().find(|b| b.head <= i).map_or("开头".to_string(), |b| format!("{} 下", b.key));
    for (i, l) in ls.iter().enumerate() {
        if is_comment(l.bare) {
            continue;
        }
        let total: usize = names.iter().map(|n| refs(l.bare, n)).sum();
        let want = usize::from(handled.contains(&i));
        if total != want {
            let which: Vec<&str> = names.iter().copied().filter(|n| refs(l.bare, n) > 0).collect();
            errs.push(format!("第 {} 行（{}）还提到「{}」，认不出怎么改：先在路由上改掉这处", line_no(i), section(i), which.join("」「")));
        }
    }
    if !errs.is_empty() {
        errs.dedup();
        return Err(errs);
    }
    let p = to_patch(text, &ls, edits);
    // 改完的整份：结构应当正好是改前的按名字换过
    let old = yaml_value(text).map_err(|e| vec![e])?;
    let new = yaml_value(&p.text).map_err(|e| vec![format!("改出来的配置{e}")])?;
    let rn = |v: &mut Value| {
        if let Some(r) = v.as_str().and_then(|s| olds.get(s)) {
            *v = Value::String(r.new.clone());
        }
    };
    let mut want = old.clone();
    for x in want.get_mut("proxies").and_then(Value::as_array_mut).into_iter().flatten() {
        if let Some(v) = x.get_mut("name") {
            rn(v);
        }
        if let Some(v) = x.get_mut("dialer-proxy") {
            rn(v);
        }
    }
    for g in want.get_mut("proxy-groups").and_then(Value::as_array_mut).into_iter().flatten() {
        for m in g.get_mut("proxies").and_then(Value::as_array_mut).into_iter().flatten() {
            rn(m);
        }
    }
    for r in want.get_mut("rules").and_then(Value::as_array_mut).into_iter().flatten() {
        let Some(s) = r.as_str() else { continue };
        let parts: Vec<&str> = s.split(',').collect();
        if let Some(t) = rule_target(&parts).and_then(|m| olds.get(parts[m].trim())) {
            if let Some(n) = retarget_rule(s, &t.new) {
                *r = Value::String(n);
            }
        }
    }
    if want != new {
        return Err(vec!["改出来的配置解析回来和预期的不一样（写法特殊），没有改：把这台路由的配置交给开发看".into()]);
    }
    Ok((p, groups_select))
}

/// 出口名跟上面板编号：名字末尾的数字换成节点现在的编号，位数不少于原来的（节点09 + 8 → 节点08，EXIT_9 + 12 → EXIT_12）。末尾没有数字为 None
pub fn follow_name(exit: &str, number: i32) -> Option<String> {
    let digits = exit.chars().rev().take_while(char::is_ascii_digit).count();
    if digits == 0 || number < 0 {
        return None;
    }
    let head = &exit[..exit.len() - digits];
    Some(format!("{head}{number:0digits$}"))
}

/// 一个出口指向的节点：编号、名字，是否直出入口 / 在备用团队
#[derive(Debug, Clone)]
pub struct NodeNum {
    pub name: String,
    pub number: Option<i32>,
    pub direct: bool,
    pub spare: bool,
}

/// 按最近一次采集算出的改名
#[derive(Debug, Default)]
pub struct RenamePlan {
    pub map: Vec<Rename>,
    /// 要改的：{exit, to, entry, node}
    pub rows: Vec<Value>,
    /// 名字对不上面板、但不改的：{exit, entry, why}
    pub skipped: Vec<Value>,
    /// 不点名时，从没对上过入口的出口（同 router sync 的 untrusted）
    pub untrusted: Vec<Value>,
    /// 点名的出口从没对上过入口、或指向别的团队 / 直出 / 备用团队的节点：要 force
    pub need_force: Vec<Value>,
    pub blockers: Vec<String>,
}

/// 改名计划。只动「已建立对应」的 fleet 出口（同 router sync：中控作业写过的，或曾经参数一致的；对不上面板入口的从来不动）：
/// 名字末尾的编号和它指向的节点现在的编号不一样的，改成跟上（[`follow_name`]）。不点名时从没对上过入口的列进 `untrusted`、
/// 节点属于别的团队（`foreign`：入口 → 原因，见 api::router_ops 的 foreign_why）的列进 `skipped`；点名的这两种要 `force`。
/// 直出入口、备用团队的节点、名字末尾没有编号的不改（写原因）
pub fn rename_plan(snap: &Snapshot, nodes: &HashMap<Uuid, NodeNum>, foreign: &HashMap<Uuid, &str>, only: &[String], force: bool) -> RenamePlan {
    let mut p = RenamePlan::default();
    for name in only {
        match snap.exits.iter().find(|x| &x.name == name) {
            None => p.blockers.push(format!("最近一次采集里没有出口「{name}」")),
            Some(x) if x.entry.is_none() => p.blockers.push(format!("出口「{name}」不是面板的入口，不跟面板改名")),
            Some(_) => {}
        }
    }
    for x in &snap.exits {
        let (Some(er), Some(srv), Some(port)) = (x.entry.as_ref(), x.server.as_deref(), x.port) else { continue };
        if er.lifecycle == "removed" || (!only.is_empty() && !only.contains(&x.name)) {
            continue;
        }
        let named = only.contains(&x.name);
        let skip = |p: &mut RenamePlan, why: String| {
            if named {
                p.blockers.push(format!("出口「{}」：{why}", x.name));
            }
            p.skipped.push(json!({"exit": x.name, "entry": er.name, "why": why}));
        };
        let Some(n) = nodes.get(&er.node_id) else {
            skip(&mut p, "对应的节点已经不在了".into());
            continue;
        };
        if n.direct {
            skip(&mut p, "指向直出入口，没有节点编号可跟".into());
            continue;
        }
        if n.spare {
            skip(&mut p, format!("指向的 {} 在备用团队，编号不是客户的：先替换（fleetctl replace）或改指向（fleetctl repoint）", n.name));
            continue;
        }
        let Some(num) = n.number else {
            skip(&mut p, format!("{} 没有编号", n.name));
            continue;
        };
        let Some(to) = follow_name(&x.name, num) else {
            skip(&mut p, format!("出口名末尾没有编号，不知道怎么跟 {}", n.name));
            continue;
        };
        if to == x.name {
            continue;
        }
        let why = if !x.linked { Some("从没对上过这个入口（可能是路由上有人加的）") } else { foreign.get(&er.id).copied() };
        if let Some(why) = why {
            if !named {
                if x.linked {
                    p.skipped.push(json!({"exit": x.name, "entry": er.name, "why": format!("指向{why}，名字不按这台路由的团队编号；确认要改就 --exit 点名并加 --force")}));
                } else {
                    p.untrusted.push(json!({"exit": x.name, "entry": er.name, "to": to}));
                }
                continue;
            }
            p.need_force.push(json!({"exit": x.name, "entry": er.name, "to": to, "why": why}));
            if !force {
                p.blockers.push(format!("出口「{}」{why}（{}）；确认要改名的话加 --force", x.name, er.name));
                continue;
            }
        }
        p.rows.push(json!({"exit": x.name, "to": to, "entry": er.name, "node": n.name}));
        p.map.push(Rename { old: x.name.clone(), new: to, server: srv.to_string(), port });
    }
    // 新名字：不能重复，不能是别的（不在这次改名里的）出口的名字；改名的几个不能指向同一个地址（执行时按地址认出口）
    let olds: HashSet<&str> = p.map.iter().map(|r| r.old.as_str()).collect();
    let mut seen: HashMap<&str, &str> = HashMap::new();
    for r in &p.map {
        if let Some(prev) = seen.insert(r.new.as_str(), r.old.as_str()) {
            p.blockers.push(format!("出口「{prev}」和「{}」都要改成「{}」：它们指向编号相同的节点，先看对应的入口对不对", r.old, r.new));
        }
        if !olds.contains(r.new.as_str()) && snap.exits.iter().any(|x| x.name == r.new) {
            p.blockers.push(format!("「{}」要改成「{}」，但已经有出口叫「{}」（不在这次改名里）：先处理那个出口", r.old, r.new, r.new));
        }
    }
    let mut addr: HashMap<(String, u16), &str> = HashMap::new();
    for r in &p.map {
        if let Some(prev) = addr.insert((r.server.to_ascii_lowercase(), r.port), r.old.as_str()) {
            p.blockers.push(format!("出口「{prev}」和「{}」指向同一个地址 {}:{}，执行时认不出谁是谁：分两次改（--exit 点名）", r.old, r.server, r.port));
        }
    }
    p
}

// ── 来源 IP 分流规则 ──────────────────────────────────────────────────────

/// 一条「这些来源地址 → 这个出口」
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Assign {
    /// 运维给的写法（CIDR、单个 IP、IP 段 a-b）
    pub spec: String,
    pub to: String,
}

/// 来源写法 → 一个或几个网段（规范成网络地址）。设备编号段（B001-B100）面板里没有到地址的对应，不认
pub fn parse_spec(spec: &str) -> Result<Vec<ipnetwork::IpNetwork>, String> {
    let s = spec.trim();
    if s.is_empty() {
        return Err("来源为空".into());
    }
    if s.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && !s.contains(':') {
        return Err(format!(
            "「{s}」像是设备编号：面板里没有设备编号到地址的对应（设备编号是现场的约定，中控不知道 B001 是哪个 IP），先换成 CIDR（10.31.5.0/24）、单个 IP 或 IP 段（10.31.5.101-10.31.5.200）"
        ));
    }
    if let Some((a, b)) = s.split_once('-') {
        let (a, b): (std::net::Ipv4Addr, std::net::Ipv4Addr) = match (a.trim().parse(), b.trim().parse()) {
            (Ok(a), Ok(b)) => (a, b),
            _ => return Err(format!("「{s}」不是 IPv4 的段（写成 10.31.5.101-10.31.5.200）")),
        };
        let (mut lo, hi) = (u32::from(a) as u64, u32::from(b) as u64);
        if lo > hi {
            return Err(format!("「{s}」前面的地址比后面的大"));
        }
        let mut out = vec![];
        while lo <= hi {
            let mut size = if lo == 0 { 32 } else { (lo.trailing_zeros()).min(32) };
            while size > 0 && lo + (1u64 << size) - 1 > hi {
                size -= 1;
            }
            out.push(ipnetwork::IpNetwork::V4(ipnetwork::Ipv4Network::new(std::net::Ipv4Addr::from(lo as u32), (32 - size) as u8).map_err(|e| e.to_string())?));
            lo += 1u64 << size;
        }
        if out.len() > 16 {
            return Err(format!("「{s}」要拆成 {} 条规则，太碎了：换成几段 CIDR", out.len()));
        }
        return Ok(out);
    }
    let net: ipnetwork::IpNetwork = match s.parse::<IpAddr>() {
        Ok(ip) => ipnetwork::IpNetwork::new(ip, if ip.is_ipv4() { 32 } else { 128 }).map_err(|e| e.to_string())?,
        Err(_) => s.parse().map_err(|_| format!("「{s}」不是 CIDR、IP 或 IP 段"))?,
    };
    Ok(vec![canon(net)])
}

fn canon(n: ipnetwork::IpNetwork) -> ipnetwork::IpNetwork { ipnetwork::IpNetwork::new(n.network(), n.prefix()).unwrap_or(n) }

fn overlaps(a: &ipnetwork::IpNetwork, b: &ipnetwork::IpNetwork) -> bool { a.is_ipv4() == b.is_ipv4() && (a.contains(b.network()) || b.contains(a.network())) }

/// 配置里的一条规则行（rules 段的列表项）
struct RuleLine<'a> {
    line: usize,
    /// 第几条规则（从 0 起）
    idx: usize,
    prefix: &'a str,
    sc: Scalar<'a>,
    rule: String,
}

fn rule_lines<'a>(ls: &[Ln<'a>], bs: &[Block]) -> Result<Vec<RuleLine<'a>>, String> {
    let Some(b) = bs.iter().find(|b| b.key == "rules") else { return Err("配置里没有 rules 段".into()) };
    if !unq(ls[b.head].bare.split_once(':').map_or("", |x| x.1)).is_empty() {
        return Err("顶层的 rules 是流式写法，认不出".into());
    }
    let mut out = vec![];
    for (idx, it) in items(ls, b).into_iter().enumerate() {
        let s = ls[it.0].bare;
        let d = ind(s).unwrap_or(0);
        // 一条规则跨了几行（折行的标量）：认不出
        if (it.0 + 1..it.1).any(|k| ind(ls[k].bare).is_some() && !is_comment(ls[k].bare)) {
            return Err(format!("第 {} 行的规则跨了几行，认不出", it.0 + 1));
        }
        let v = &s[d + 1..];
        let sc = scalar(v).ok_or_else(|| format!("第 {} 行的规则写法认不出（转义、流式…）", it.0 + 1))?;
        let rule = sc.inner.to_string();
        out.push(RuleLine { line: it.0, idx, prefix: &s[..d + 1], sc, rule });
    }
    Ok(out)
}

fn src_net(rule: &str) -> Option<ipnetwork::IpNetwork> {
    let parts: Vec<&str> = rule.split(',').collect();
    (parts.len() >= 3 && parts[0].trim().eq_ignore_ascii_case("SRC-IP-CIDR")).then(|| parts[1].trim().parse().ok().map(canon)).flatten()
}

/// 改规则要改的行与计划的明细 [{spec, cidr, to, op（add 新加 / retarget 改目标 / same 已经是）, from, line}]。
/// - 同一个网段已经有一条 SRC-IP-CIDR 规则：改它的目标（位置不动）；已经指向 `to` 的不动。
/// - 没有的：新加在现有 SRC-IP-CIDR 规则的最后一条后面（写法照那一条：缩进、引号、no-resolve 之类的选项）。要求现有的 SRC-IP-CIDR 规则连在一起、
///   在兜底 MATCH 前面，新网段和现有的（以及这次的几条之间）不重叠——重叠的谁先谁后会改变现有设备走哪个出口，不替人决定。
/// - 配置里还没有 SRC-IP-CIDR 规则：不知道该插在哪（插在最前面，这些设备的国内流量也会走出口；插在后面可能被前面的规则先匹配走），拒绝。
/// - 目标要是配置里的出口或策略组。
///
/// 改完的整份解析一遍，rules 应当正好是改前的加上 / 改掉这几条、别的一项不差
pub fn rules_patch(text: &str, assigns: &[Assign]) -> Result<(Patch, Vec<Value>), Vec<String>> {
    let mut errs: Vec<String> = vec![];
    let old = yaml_value(text).map_err(|e| vec![e])?;
    let mut known: HashSet<String> = HashSet::new();
    for k in ["proxies", "proxy-groups"] {
        known.extend(old.get(k).and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.get("name").and_then(Value::as_str)).map(String::from));
    }
    let ls = split_lines(text);
    let bs = blocks(&ls);
    let rules = rule_lines(&ls, &bs).map_err(|e| vec![e])?;
    let src: Vec<(&RuleLine, ipnetwork::IpNetwork)> = rules.iter().filter_map(|r| src_net(&r.rule).map(|n| (r, n))).collect();
    let mut wanted: Vec<(String, ipnetwork::IpNetwork, String)> = vec![];
    for a in assigns {
        if !known.contains(&a.to) {
            errs.push(format!("配置里没有出口或策略组「{}」", a.to));
            continue;
        }
        if a.to.contains(',') || a.to.contains('\'') {
            errs.push(format!("「{}」带逗号或引号，写不进规则", a.to));
            continue;
        }
        match parse_spec(&a.spec) {
            Ok(ns) => wanted.extend(ns.into_iter().map(|n| (a.spec.clone(), n, a.to.clone()))),
            Err(e) => errs.push(e),
        }
    }
    for (i, (s1, n1, _)) in wanted.iter().enumerate() {
        for (s2, n2, _) in &wanted[i + 1..] {
            if overlaps(n1, n2) {
                errs.push(format!("这次给的 {s1}（{n1}）和 {s2}（{n2}）重叠"));
            }
        }
    }
    let mut edits: BTreeMap<usize, String> = BTreeMap::new();
    let mut rows = vec![];
    let mut inserts: Vec<String> = vec![];
    let mut expect: Vec<Value> = old.get("rules").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut inserted: Vec<Value> = vec![];
    let anchor = src.last().map(|(r, _)| *r);
    let render_rule = |tpl: &RuleLine, rule: &str| -> String {
        let v = match tpl.sc.quote {
            Some('\'') => format!("'{}'", rule.replace('\'', "''")),
            Some(q) => format!("{q}{rule}{q}"),
            None if plain_ok(rule) => rule.to_string(),
            None => format!("'{}'", rule.replace('\'', "''")),
        };
        format!("{}{}{v}", tpl.prefix, tpl.sc.lead)
    };
    for (spec, net, to) in &wanted {
        let same: Vec<&(&RuleLine, ipnetwork::IpNetwork)> = src.iter().filter(|(_, n)| n == net).collect();
        let clash: Vec<String> = src.iter().filter(|(_, n)| n != net && overlaps(n, net)).map(|(r, n)| format!("第 {} 行的 {n}（→ {}）", r.line + 1, rule_view(&r.rule).2)).collect();
        if !clash.is_empty() {
            errs.push(format!("{spec}（{net}）和现有的规则重叠：{}。重叠的谁先谁后会改变现有设备走哪个出口，先在路由上理顺", clash.join("、")));
            continue;
        }
        match same.as_slice() {
            [] => {}
            [(r, _)] => {
                let from = rule_view(&r.rule).2;
                if &from == to {
                    rows.push(json!({"spec": spec, "cidr": net.to_string(), "to": to, "op": "same", "from": from, "line": r.line + 1}));
                    continue;
                }
                let Some(n) = retarget_rule(&r.rule, to) else {
                    errs.push(format!("第 {} 行的规则认不出目标", r.line + 1));
                    continue;
                };
                edits.insert(r.line, render_rule(r, &n) + r.sc.trail);
                expect[r.idx] = Value::String(n);
                rows.push(json!({"spec": spec, "cidr": net.to_string(), "to": to, "op": "retarget", "from": from, "line": r.line + 1}));
                continue;
            }
            _ => {
                errs.push(format!("已经有 {} 条 {net} 的规则，不知道改哪条", same.len()));
                continue;
            }
        }
        let Some(a) = anchor else {
            errs.push(format!(
                "配置里还没有 SRC-IP-CIDR 规则，不知道 {spec} 该插在哪（插在最前面，这些设备的国内流量也走出口；插在后面可能先被别的规则匹配走）：要先定一个位置"
            ));
            continue;
        };
        // 新加的那一条照最后一条 SRC-IP-CIDR 规则的写法：选项（no-resolve…）、逗号后面有没有空格
        let parts: Vec<&str> = a.rule.split(',').collect();
        let opts: String = rule_target(&parts).map(|m| parts[m + 1..].iter().map(|o| format!(",{o}")).collect()).unwrap_or_default();
        let sep = if a.rule.contains(", ") { ", " } else { "," };
        let rule = format!("SRC-IP-CIDR{sep}{net}{sep}{to}{opts}");
        inserts.push(render_rule(a, &rule));
        inserted.push(Value::String(rule));
        rows.push(json!({"spec": spec, "cidr": net.to_string(), "to": to, "op": "add", "from": null, "line": null}));
    }
    if let Some(a) = anchor.filter(|_| !inserts.is_empty()) {
        // 现有的 SRC-IP-CIDR 规则不连在一起、或在兜底 MATCH 后面：插在哪都可能改变现有设备的走向
        let idxs: Vec<usize> = src.iter().map(|(r, _)| r.idx).collect();
        if idxs.windows(2).any(|w| w[1] != w[0] + 1) {
            errs.push("现有的 SRC-IP-CIDR 规则不连在一起（中间夹着别的规则），不知道新的该插在哪：先在路由上把它们挪到一起".into());
        }
        if rules.iter().any(|r| r.idx < a.idx && rule_view(&r.rule).0.eq_ignore_ascii_case("MATCH")) {
            errs.push("SRC-IP-CIDR 规则在兜底 MATCH 后面（不会生效），先在路由上理顺".into());
        }
        let cur = edits.remove(&a.line).unwrap_or_else(|| a_line(&ls, a));
        edits.insert(a.line, std::iter::once(cur).chain(inserts.iter().cloned()).collect::<Vec<_>>().join("\n"));
        for (k, v) in inserted.into_iter().enumerate() {
            expect.insert(a.idx + 1 + k, v);
        }
    }
    if !errs.is_empty() {
        return Err(errs);
    }
    let p = to_patch(text, &ls, edits);
    if p.edits.is_empty() {
        return Ok((p, rows));
    }
    let new = yaml_value(&p.text).map_err(|e| vec![format!("改出来的配置{e}")])?;
    let mut want = old;
    want["rules"] = Value::Array(expect);
    if want != new {
        return Err(vec!["改出来的配置解析回来和预期的不一样（写法特殊），没有改：把这台路由的配置交给开发看".into()]);
    }
    Ok((p, rows))
}

fn a_line(ls: &[Ln], a: &RuleLine) -> String { ls[a.line].bare.to_string() }

// ── 配置的结构（只读）────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
#[serde(default)]
struct VCfg {
    proxies: Vec<VProxy>,
    #[serde(rename = "proxy-groups")]
    groups: Vec<VGroup>,
    rules: Vec<String>,
    #[serde(rename = "proxy-providers")]
    providers: Option<serde_json::Map<String, Value>>,
    #[serde(rename = "rule-providers")]
    rule_providers: Option<serde_json::Map<String, Value>>,
    mode: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct VProxy {
    name: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    server: Option<String>,
    port: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct VGroup {
    name: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    proxies: Vec<String>,
    #[serde(rename = "use")]
    uses: Vec<String>,
}

/// 内置的目标
fn builtin(t: &str) -> bool { matches!(t.to_ascii_uppercase().as_str(), "DIRECT" | "REJECT" | "REJECT-DROP" | "PASS" | "COMPATIBLE" | "GLOBAL") }

/// 配置的结构：出口（name / type / server / port，和快照里对上的入口、参数是否一致），策略组（成员、现在选的、有没有规则用），
/// 规则按目标汇总（每个目标几条、各类型几条；SRC-IP-CIDR 的网段全列），另列全部 SRC-IP-CIDR 规则（来源 IP → 出口，第几行）。凭据一律不给
pub fn structure(text: &str, snap: Option<&Snapshot>) -> Result<Value, String> {
    let cfg: VCfg = serde_saphyr::from_str(text).map_err(|e| format!("配置文件解析不了：{}", e.to_string().lines().next().unwrap_or("").chars().take(200).collect::<String>()))?;
    let exits = snap.map(|s| &s.exits[..]).unwrap_or_default();
    let groups_now = snap.map(|s| &s.groups[..]).unwrap_or_default();
    let proxies: Vec<Value> = cfg
        .proxies
        .iter()
        .filter_map(|p| {
            let name = p.name.as_deref()?.trim();
            let x = exits.iter().find(|x| x.name == name);
            Some(json!({
                "name": name, "type": p.kind, "server": p.server, "port": p.port.as_deref().and_then(|s| s.trim().parse::<u16>().ok()),
                "entry": x.and_then(|x| x.entry.as_ref()).map(|e| json!({"id": e.id, "name": e.name, "node_name": e.node_name})),
                "sync": x.map(|x| x.sync.clone()), "linked": x.map(|x| x.linked), "alive": x.and_then(|x| x.alive), "delay_ms": x.and_then(|x| x.delay_ms),
            }))
        })
        .collect();
    let pnames: HashSet<&str> = cfg.proxies.iter().filter_map(|p| p.name.as_deref()).map(str::trim).collect();
    let gnames: HashSet<&str> = cfg.groups.iter().filter_map(|g| g.name.as_deref()).map(str::trim).collect();
    let kind_of = |t: &str| {
        if pnames.contains(t) {
            "exit"
        } else if gnames.contains(t) {
            "group"
        } else if builtin(t) {
            "builtin"
        } else {
            "unknown"
        }
    };
    let groups: Vec<Value> = cfg
        .groups
        .iter()
        .filter_map(|g| {
            let name = g.name.as_deref()?.trim();
            let now = groups_now.iter().find(|x| x.name == name);
            Some(json!({"name": name, "type": g.kind, "proxies": g.proxies, "use": g.uses, "now": now.and_then(|x| x.now.clone()), "used": now.and_then(|x| x.used)}))
        })
        .collect();
    // 规则按目标汇总，按第一次出现的顺序
    let mut order: Vec<String> = vec![];
    // 目标 → (几条, 各类型几条, SRC-IP-CIDR 的网段)
    type Tally = (usize, BTreeMap<String, usize>, Vec<String>);
    let mut by: HashMap<String, Tally> = HashMap::new();
    let mut src = vec![];
    let mut matched: Option<String> = None;
    for (i, r) in cfg.rules.iter().enumerate() {
        let (kind, payload, target, _) = rule_view(r);
        if kind.eq_ignore_ascii_case("MATCH") {
            matched = Some(target.clone());
        }
        let e = by.entry(target.clone()).or_insert_with(|| {
            order.push(target.clone());
            Default::default()
        });
        e.0 += 1;
        *e.1.entry(kind.to_ascii_uppercase()).or_default() += 1;
        if kind.eq_ignore_ascii_case("SRC-IP-CIDR") {
            e.2.push(payload.clone());
            src.push(json!({"index": i + 1, "cidr": payload, "to": target, "to_kind": kind_of(&target)}));
        }
    }
    let rule_targets: Vec<Value> = order
        .iter()
        .map(|t| {
            let (n, kinds, cidrs) = &by[t];
            json!({"target": t, "kind": kind_of(t), "rules": n, "types": kinds, "src_cidrs": cidrs})
        })
        .collect();
    Ok(json!({
        "mode": cfg.mode,
        "proxies": proxies,
        "groups": groups,
        "rules_total": cfg.rules.len(),
        "rule_targets": rule_targets,
        "src_rules": src,
        "match": matched,
        "proxy_providers": cfg.providers.map(|m| m.keys().cloned().collect::<Vec<_>>()).unwrap_or_default(),
        "rule_providers": cfg.rule_providers.map(|m| m.keys().cloned().collect::<Vec<_>>()).unwrap_or_default(),
    }))
}

// ── 隐去凭据、逐行比较 ────────────────────────────────────────────────────

/// 变更时间线（changes.rs）算作「凭据」、只记「变了」不记值的字段（clash.rs 的 Creds：uuid、password、flow、servername / sni、reality-opts 的 public-key / short-id）。
/// 逐行比较时这些的值同样隐去；另外隐去名字带 password / secret / token / private / pre-shared / psk / auth / passphrase 的、username、user、key、
/// 订阅地址的路径与查询串、URL 里的「用户:密码@」
pub const CRED_KEYS: &[&str] = &["uuid", "password", "flow", "servername", "sni", "public-key", "short-id"];

fn secret_key(k: &str) -> bool {
    let k = k.trim().trim_matches(['"', '\'']).to_ascii_lowercase();
    CRED_KEYS.contains(&k.as_str())
        || ["password", "passwd", "secret", "token", "private", "pre-shared", "psk", "auth", "passphrase"].iter().any(|w| k.contains(w))
        || matches!(k.as_str(), "username" | "user" | "key" | "certificate" | "ca-str" | "obfs-param")
}

/// 一个值换成「<已隐去·标记>」：标记 = sha256(盐 + 值) 的前 8 位（盐每次请求随机，比较的两边用同一个）
fn tag(salt: &[u8], v: &str) -> String {
    let mut b = salt.to_vec();
    b.extend_from_slice(unq(v).as_bytes());
    format!("<已隐去·{}>", &hex::encode(crate::crypto::sha256(&b))[..8])
}

/// URL 里的「用户:密码@」与查询串换成标记；`path`：路径也换（订阅地址，路径里常带令牌）
fn mask_url(v: &str, salt: &[u8], path: bool) -> String {
    let Some(i) = v.find("://") else { return v.to_string() };
    let (scheme, rest) = v.split_at(i + 3);
    let end = rest.find(['\'', '"', ' ', ',', '}', ']']).unwrap_or(rest.len());
    let (url, tail) = rest.split_at(end);
    let (auth, hostpath) = match url.find('@').filter(|&a| url.find('/').is_none_or(|s| a < s)) {
        Some(a) => (Some(&url[..a]), &url[a + 1..]),
        None => (None, url),
    };
    let (host, more) = hostpath.split_at(hostpath.find(['/', '?', '#']).unwrap_or(hostpath.len()));
    let more = if more.is_empty() {
        String::new()
    } else if path {
        format!("/{}", tag(salt, more))
    } else {
        match more.find('?') {
            Some(q) => format!("{}?{}", &more[..q], tag(salt, &more[q..])),
            None => more.to_string(),
        }
    };
    let auth = auth.map(|a| format!("{}@", tag(salt, a))).unwrap_or_default();
    format!("{scheme}{auth}{host}{more}{tail}")
}

/// 流式映射里的凭据（`{name: a, uuid: b, reality-opts: {public-key: c}}`）
fn mask_flow(s: &str, salt: &[u8]) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(c) = rest.find(':') {
        let (head, tail) = rest.split_at(c);
        let key_start = head.rfind(['{', ',', ' ']).map_or(0, |i| i + 1);
        let key = &head[key_start..];
        out.push_str(head);
        out.push(':');
        rest = &tail[1..];
        if secret_key(key) {
            let t = rest.trim_start();
            let lead = &rest[..rest.len() - t.len()];
            let end = match t.chars().next() {
                Some(q @ ('"' | '\'')) => t[1..].find(q).map_or(t.len(), |i| i + 2),
                Some('{') => 0,
                _ => t.find([',', '}']).unwrap_or(t.len()),
            };
            if end > 0 {
                out.push_str(lead);
                out.push_str(&tag(salt, t[..end].trim_end()));
                out.push_str(&t[..end][t[..end].trim_end().len()..]);
                rest = &t[end..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 隐去配置里的凭据（逐行，见 [`CRED_KEYS`]）：键值行的值、块写法（`key:` 下面更深的几行、`|` / `>`）整块、流式映射里的、URL 里的；注释原样
pub fn mask(text: &str, salt: &[u8]) -> String {
    let mut out = String::with_capacity(text.len());
    // 隐去整块：块的键所在的缩进，比它深的行都隐去
    let mut block: Option<usize> = None;
    for l in split_lines(text) {
        let s = l.bare;
        let d = ind(s);
        let line = match (d, block) {
            (Some(d), Some(b)) if d > b => {
                let body = &s[d..];
                if is_comment(body) {
                    s.to_string()
                } else if isdash(s, d) {
                    format!("{}- {}", &s[..d], tag(salt, body[1..].trim()))
                } else {
                    format!("{}{}", &s[..d], tag(salt, body))
                }
            }
            _ => {
                block = None;
                mask_line(s, salt, &mut block)
            }
        };
        out.push_str(&line);
        if l.cr {
            out.push('\r');
        }
        out.push('\n');
    }
    out
}

fn mask_line(s: &str, salt: &[u8], block: &mut Option<usize>) -> String {
    let Some(d) = ind(s) else { return s.to_string() };
    if is_comment(s) {
        return s.to_string();
    }
    let mut col = d;
    if isdash(s, d) {
        col = d + 1 + s[d + 1..].find(|c| c != ' ').unwrap_or(s.len() - d - 1);
    }
    let rest = &s[col..];
    if rest.starts_with('{') {
        return format!("{}{}", &s[..col], mask_flow(rest, salt));
    }
    let key_like = |j: usize| j > 0 && !rest[j..].starts_with("://") && rest[..j].chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '"' | '\'' | '.' | ' '));
    let Some(j) = rest.find(':').filter(|&j| key_like(j)) else {
        // 列表里的一项（订阅地址、DNS 地址…）：URL 里的用户名密码与查询串
        return if rest.contains("://") { format!("{}{}", &s[..col], mask_url(rest, salt, false)) } else { s.to_string() };
    };
    let (key, val) = (&rest[..j], &rest[j + 1..]);
    let v = val.trim_start();
    let lead = &val[..val.len() - v.len()];
    let head = &s[..col + j + 1];
    if secret_key(key) {
        if v.is_empty() || is_comment(v) || v.starts_with('|') || v.starts_with('>') {
            *block = Some(col);
            return format!("{head}{lead}{}", if v.is_empty() { "" } else { "<已隐去>" });
        }
        let c = cut_comment(v);
        let body = c.trim_end();
        return format!("{head}{lead}{}{}", tag(salt, body), &v[body.len()..]);
    }
    if v.starts_with('{') {
        return format!("{head}{lead}{}", mask_flow(v, salt));
    }
    if v.contains("://") {
        let url_key = key.trim().eq_ignore_ascii_case("url");
        return format!("{head}{lead}{}", mask_url(v, salt, url_key));
    }
    s.to_string()
}

/// 逐行比较的一步
#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Eq(usize, usize),
    Del(usize),
    Ins(usize),
}

/// Myers 差异（先去掉共同的头尾）；改动超过 `max_d` 行为 None
fn diff_ops(a: &[&str], b: &[&str], max_d: usize) -> Option<Vec<Op>> {
    let pre = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..].iter().rev().zip(b[pre..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (am, bm) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let (n, m) = (am.len() as isize, bm.len() as isize);
    let max = (n + m) as usize;
    let off = max as isize + 1;
    let mut v = vec![0isize; 2 * max + 3];
    let mut trace: Vec<Vec<isize>> = vec![];
    let mut found = None;
    'outer: for d in 0..=max.min(max_d) as isize {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && v[(off + k - 1) as usize] < v[(off + k + 1) as usize]) { v[(off + k + 1) as usize] } else { v[(off + k - 1) as usize] + 1 };
            let mut y = x - k;
            while x < n && y < m && am[x as usize] == bm[y as usize] {
                x += 1;
                y += 1;
            }
            v[(off + k) as usize] = x;
            if x >= n && y >= m {
                found = Some(d);
                break 'outer;
            }
            k += 2;
        }
    }
    let d_end = found?;
    // 回溯
    let mut ops = vec![];
    let (mut x, mut y) = (n, m);
    for d in (1..=d_end).rev() {
        let v = &trace[d as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && v[(off + k - 1) as usize] < v[(off + k + 1) as usize]) { k + 1 } else { k - 1 };
        let px = v[(off + prev_k) as usize];
        let py = px - prev_k;
        while x > px && y > py {
            x -= 1;
            y -= 1;
            ops.push(Op::Eq(x as usize, y as usize));
        }
        if x == px {
            y -= 1;
            ops.push(Op::Ins(y as usize));
        } else {
            x -= 1;
            ops.push(Op::Del(x as usize));
        }
    }
    while x > 0 && y > 0 {
        x -= 1;
        y -= 1;
        ops.push(Op::Eq(x as usize, y as usize));
    }
    ops.reverse();
    let shift = |o: Op| match o {
        Op::Eq(i, j) => Op::Eq(i + pre, j + pre),
        Op::Del(i) => Op::Del(i + pre),
        Op::Ins(j) => Op::Ins(j + pre),
    };
    let mut all: Vec<Op> = (0..pre).map(|i| Op::Eq(i, i)).collect();
    all.extend(ops.into_iter().map(shift));
    all.extend((0..suf).map(|t| Op::Eq(a.len() - suf + t, b.len() - suf + t)));
    Some(all)
}

/// 最多比这么多行的改动
pub const MAX_DIFF: usize = 3000;

/// 两份配置（都已隐去凭据）逐行比较：`old` 是备份，`new` 是现在的。返回 {identical, added, removed, hunks:[{old_start, old_lines, new_start, new_lines, lines:["␠…", "-…", "+…"]}], text（统一格式）}
pub fn diff(old: &str, new: &str, old_name: &str, new_name: &str) -> Result<Value, String> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let ops = diff_ops(&a, &b, MAX_DIFF).ok_or_else(|| format!("两份配置差了 {MAX_DIFF} 行以上，不逐行列了（多半是换了一整份配置）"))?;
    let (added, removed) = (ops.iter().filter(|o| matches!(o, Op::Ins(_))).count(), ops.iter().filter(|o| matches!(o, Op::Del(_))).count());
    const CTX: usize = 3;
    let changed: Vec<usize> = ops.iter().enumerate().filter(|(_, o)| !matches!(o, Op::Eq(..))).map(|(i, _)| i).collect();
    let mut hunks = vec![];
    let mut text = if changed.is_empty() { String::new() } else { format!("--- {old_name}\n+++ {new_name}\n") };
    let mut i = 0;
    while i < changed.len() {
        let start = changed[i].saturating_sub(CTX);
        let mut end = changed[i] + CTX + 1;
        while i + 1 < changed.len() && changed[i + 1] <= end + CTX {
            i += 1;
            end = changed[i] + CTX + 1;
        }
        let end = end.min(ops.len());
        let seg = &ops[start..end];
        let old_start = seg.iter().find_map(|o| match o {
            Op::Eq(x, _) | Op::Del(x) => Some(*x + 1),
            Op::Ins(_) => None,
        });
        let new_start = seg.iter().find_map(|o| match o {
            Op::Eq(_, y) | Op::Ins(y) => Some(*y + 1),
            Op::Del(_) => None,
        });
        let lines: Vec<String> = seg
            .iter()
            .map(|o| match o {
                Op::Eq(x, _) => format!(" {}", a[*x]),
                Op::Del(x) => format!("-{}", a[*x]),
                Op::Ins(y) => format!("+{}", b[*y]),
            })
            .collect();
        let ol = seg.iter().filter(|o| !matches!(o, Op::Ins(_))).count();
        let nl = seg.iter().filter(|o| !matches!(o, Op::Del(_))).count();
        let (os, ns) = (old_start.unwrap_or(0), new_start.unwrap_or(0));
        text.push_str(&format!("@@ -{os},{ol} +{ns},{nl} @@\n"));
        for l in &lines {
            text.push_str(l);
            text.push('\n');
        }
        hunks.push(json!({"old_start": os, "old_lines": ol, "new_start": ns, "new_lines": nl, "lines": lines}));
        i += 1;
    }
    Ok(json!({"identical": changed.is_empty(), "added": added, "removed": removed, "hunks": hunks, "text": text}))
}

// ── 改名之后库里跟着改的 ──────────────────────────────────────────────────

/// 改名成功之后：「已建立的对应」（router_exit_link）按新名字记（几个名字互换也对）；出口的历史指标（router_exit_metric、小时汇总）跟着出口走，
/// 改记到新名字下——新名字下已经有别的（早已不在配置里的出口留下的）历史时不挪，返回 false（作业写一行警告）
pub async fn after_rename(db: &sqlx::PgPool, router: Uuid, map: &[Rename]) -> sqlx::Result<bool> {
    let olds: Vec<&str> = map.iter().map(|r| r.old.as_str()).collect();
    let news: Vec<&str> = map.iter().map(|r| r.new.as_str()).collect();
    let mut tx = db.begin().await?;
    let links: Vec<(String, Uuid, OffsetDateTime, String)> =
        sqlx::query_as("delete from router_exit_link where router_id = $1 and exit = any($2) returning exit, entry_id, since, source").bind(router).bind(&olds).fetch_all(&mut *tx).await?;
    for (exit, entry, since, source) in links {
        let Some(r) = map.iter().find(|r| r.old == exit) else { continue };
        sqlx::query("insert into router_exit_link (router_id, exit, entry_id, since, source) values ($1, $2, $3, $4, $5) on conflict do nothing")
            .bind(router)
            .bind(&r.new)
            .bind(entry)
            .bind(since)
            .bind(source)
            .execute(&mut *tx)
            .await?;
    }
    // 新名字（不在旧名里的）下已经有历史：挪过去会混在一起，不挪
    let fresh: Vec<&str> = news.iter().copied().filter(|n| !olds.contains(n)).collect();
    let taken: bool = sqlx::query_scalar(
        "select exists (select 1 from router_exit_metric where router_id = $1 and exit = any($2)) or exists (select 1 from router_exit_metric_hourly where router_id = $1 and exit = any($2))",
    )
    .bind(router)
    .bind(&fresh)
    .fetch_one(&mut *tx)
    .await?;
    if !taken {
        // 先挪到带前缀的临时名字再去掉前缀：几个名字互换时一步改会撞主键
        const TMP: &str = "\u{1}fleet-rename\u{1}";
        for t in ["router_exit_metric", "router_exit_metric_hourly"] {
            sqlx::query(&format!("update {t} x set exit = $4 || m.new from unnest($2::text[], $3::text[]) m(old, new) where x.router_id = $1 and x.exit = m.old"))
                .bind(router)
                .bind(&olds)
                .bind(&news)
                .bind(TMP)
                .execute(&mut *tx)
                .await?;
            sqlx::query(&format!("update {t} set exit = substr(exit, length($2) + 1) where router_id = $1 and starts_with(exit, $2)")).bind(router).bind(TMP).execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    Ok(!taken)
}

#[cfg(test)]
mod tests {
    use super::super::collect::{Exit, Group};
    use super::*;

    /// 现场的写法：块格式、顶格的列表、name 不在第一行、带引号的名字、select 组、规则（含带引号与 no-resolve 的）、dns 里的 #组名、secret 行已在路由上去掉
    const CFG: &str = "mixed-port: 7890\n\
dns:\n  nameserver:\n  - https://1.1.1.1/dns-query#手动选择\n\
proxies:\n\
- name: 节点07\n  type: vless\n  server: 198.51.100.2\n  port: 20007\n  uuid: 11111111-1111-4111-8111-111111111111\n  servername: www.cloudflare.com\n  reality-opts:\n    public-key: OLD_PBK\n    short-id: abcd\n\
- type: anytls\n  name: \"节点10\"\n  server: relay.test\n  port: 40010\n  password: 22222222-2222-4222-8222-222222222222\n  sni: www.apple.com\n\
- name: 节点05\n  type: vless\n  server: 198.51.100.2\n  port: 20005\n\
- name: 机场\n  type: ss\n  server: 203.0.113.9\n  port: 8388\n  password: pw\n  dialer-proxy: 节点10\n\
proxy-groups:\n\
- name: 手动选择\n  type: select\n  proxies:\n  - 节点07\n  - '节点10'\n  - DIRECT\n\
- name: 自动\n  type: url-test\n  proxies:\n    - 节点05\n    - 节点07\n\
rules:\n\
- SRC-IP-CIDR,10.31.1.7/32,节点07\n\
- 'SRC-IP-CIDR,10.31.1.10/32,节点10,no-resolve'\n\
- SRC-IP-CIDR,10.31.1.5/32,节点05\n\
- DOMAIN-SUFFIX,cn,DIRECT\n\
- MATCH,手动选择\n";

    fn rn(old: &str, new: &str, server: &str, port: u16) -> Rename { Rename { old: old.into(), new: new.into(), server: server.into(), port } }

    /// 07 → 04、10 → 07 一次改完（互换名字）：出口名、组成员（带引号的照样带）、规则目标、别的出口的 dialer-proxy 都跟着改，别的一个字不动；
    /// 改完解析回来和预期的结构一致；sha256 按每行补 \n 算
    #[test]
    fn rename_swaps_names_everywhere() {
        let map = [rn("节点07", "节点04", "198.51.100.2", 20007), rn("节点10", "节点07", "relay.test", 40010)];
        let (p, sel) = rename_patch(CFG, &map).unwrap();
        assert_eq!(sel, ["手动选择"]);
        let want = CFG
            .replacen("- name: 节点07\n", "- name: 节点04\n", 1)
            .replacen("  name: \"节点10\"\n", "  name: \"节点07\"\n", 1)
            .replacen("  dialer-proxy: 节点10\n", "  dialer-proxy: 节点07\n", 1)
            .replacen("  - 节点07\n  - '节点10'\n", "  - 节点04\n  - '节点07'\n", 1)
            .replacen("    - 节点07\n", "    - 节点04\n", 1)
            .replacen("- SRC-IP-CIDR,10.31.1.7/32,节点07\n", "- SRC-IP-CIDR,10.31.1.7/32,节点04\n", 1)
            .replacen("- 'SRC-IP-CIDR,10.31.1.10/32,节点10,no-resolve'\n", "- 'SRC-IP-CIDR,10.31.1.10/32,节点07,no-resolve'\n", 1);
        assert_eq!(p.text, want);
        assert_eq!(p.edits.len(), 8);
        assert_eq!(p.before, sha_hex(CFG));
        assert_eq!(p.after, sha_hex(&want));
        let e = &p.edits[0];
        assert_eq!((e.line, e.was.as_str(), e.now.as_str()), (6, "- name: 节点07", "- name: 节点04"));
        assert!(!p.edits.iter().any(|e| e.now.contains("1111") || e.now.contains("OLD_PBK")), "凭据所在的行不碰");
        // CRLF：每行原样保留 \r
        let crlf = CFG.replace('\n', "\r\n");
        let (p, _) = rename_patch(&crlf, &map).unwrap();
        assert_eq!(p.text, want.replace('\n', "\r\n"));
    }

    /// 认不出、改了会出问题的一行不改：地址变了、同名两个、找不到、新名字已有出口 / 策略组、dns 里还提到、流式写法、一行提到两个
    #[test]
    fn rename_refuses() {
        let r = |cfg: &str, map: &[Rename]| rename_patch(cfg, map).unwrap_err().join("；");
        let ok = [rn("节点07", "节点04", "198.51.100.2", 20007)];
        assert!(r(CFG, &[rn("节点07", "节点04", "198.51.100.2", 20099)]).contains("现在指向 198.51.100.2:20007，不是核对过的 198.51.100.2:20099"));
        assert!(r(&CFG.replace("- name: 节点05", "- name: 节点07"), &ok).contains("有 2 个名为「节点07」"));
        assert!(r(CFG, &[rn("节点99", "节点98", "x", 1)]).contains("没有出口「节点99」"));
        assert!(r(CFG, &[rn("节点07", "节点05", "198.51.100.2", 20007)]).contains("已经有出口叫「节点05」"));
        assert!(r(CFG, &[rn("节点07", "手动选择", "198.51.100.2", 20007)]).contains("策略组也叫「手动选择」"));
        let dns = CFG.replace("#手动选择", "#节点07");
        assert!(r(&dns, &ok).contains("第 4 行（dns 下）还提到「节点07」"), "{}", r(&dns, &ok));
        let flow = CFG.replace("- name: 节点05\n  type: vless\n  server: 198.51.100.2\n  port: 20005\n", "- {name: 节点05, type: vless, server: x, port: 1, dialer-proxy: 节点07}\n");
        assert!(r(&flow, &ok).contains("还提到「节点07」"));
        let two = CFG.replace("- DOMAIN-SUFFIX,cn,DIRECT", "- DOMAIN-SUFFIX,节点07,节点07");
        assert!(r(&two, &ok).contains("还提到"));
        // 注释里提到的不管
        let commented = CFG.replace("rules:\n", "rules:\n# 节点07 是主力\n");
        assert!(rename_patch(&commented, &ok).is_ok());
    }

    #[test]
    fn follow_names() {
        assert_eq!(follow_name("节点09", 8).as_deref(), Some("节点08"));
        assert_eq!(follow_name("节点09", 12).as_deref(), Some("节点12"));
        assert_eq!(follow_name("EXIT_5", 10).as_deref(), Some("EXIT_10"));
        assert_eq!(follow_name("节点100", 7).as_deref(), Some("节点007"));
        assert_eq!(follow_name("香港", 3), None);
    }

    fn exit(name: &str, port: u16, node: Uuid, linked: bool) -> Exit {
        Exit {
            name: name.into(),
            server: Some("relay.test".into()),
            port: Some(port),
            entry: Some(clash::EntryRef { id: Uuid::now_v7(), name: format!("{name} 的入口"), node_id: node, relay_id: Uuid::now_v7(), node_name: String::new(), relay_name: String::new(), port: port as i32, lifecycle: "active".into() }),
            sync: "ok".into(),
            linked,
            ..Default::default()
        }
    }

    /// 编号压缩后（07–12 → 04–09）：已建立对应的出口改成跟上面板；从没对上过的不点名不动；备用、直出、没编号的写原因；新名字撞上别的出口时拦下
    #[test]
    fn rename_plan_follows_fleet_numbers() {
        let ids: Vec<Uuid> = (0..6).map(|_| Uuid::now_v7()).collect();
        let node = |i: usize, number: Option<i32>, spare: bool, direct: bool| (ids[i], NodeNum { name: format!("团队2-{:02}", number.unwrap_or(0)), number, spare, direct });
        let nodes: HashMap<Uuid, NodeNum> = [node(0, Some(4), false, false), node(1, Some(5), false, false), node(2, Some(6), false, false), node(3, Some(1), true, false), node(4, None, false, true), node(5, Some(3), false, false)].into_iter().collect();
        let snap = Snapshot {
            exits: vec![exit("节点07", 1, ids[0], true), exit("节点08", 2, ids[1], true), exit("节点09", 3, ids[2], false), exit("节点11", 4, ids[3], true), exit("直连", 5, ids[4], true), exit("节点03", 6, ids[5], true)],
            ..Default::default()
        };
        let p = rename_plan(&snap, &nodes, &HashMap::new(), &[], false);
        assert!(p.blockers.is_empty(), "{:?}", p.blockers);
        assert_eq!(p.map.iter().map(|r| (r.old.as_str(), r.new.as_str())).collect::<Vec<_>>(), [("节点07", "节点04"), ("节点08", "节点05")]);
        assert_eq!(p.untrusted[0]["exit"], "节点09");
        let why: Vec<&str> = p.skipped.iter().filter_map(|s| s["why"].as_str()).collect();
        assert!(why[0].contains("在备用团队") && why[1].contains("直出入口"), "{why:?}");
        // 点名从没对上过的：要 force
        let p = rename_plan(&snap, &nodes, &HashMap::new(), &["节点09".into()], false);
        assert!(p.blockers[0].contains("从没对上过") && p.map.is_empty(), "{:?}", p.blockers);
        let p = rename_plan(&snap, &nodes, &HashMap::new(), &["节点09".into()], true);
        assert_eq!((p.map[0].new.as_str(), p.blockers.len()), ("节点06", 0));
        // 新名字已经有出口用着（它不在这次改名里）
        let mut s2 = snap.clone();
        s2.exits.push(Exit { name: "节点04".into(), ..Default::default() });
        assert!(rename_plan(&s2, &nodes, &HashMap::new(), &[], false).blockers[0].contains("已经有出口叫「节点04」"));
    }

    #[test]
    fn specs_and_ranges() {
        let s = |x: &str| parse_spec(x).map(|v| v.iter().map(|n| n.to_string()).collect::<Vec<_>>());
        assert_eq!(s("10.31.5.7").unwrap(), ["10.31.5.7/32"]);
        assert_eq!(s("10.31.5.9/24").unwrap(), ["10.31.5.0/24"]);
        assert_eq!(s("10.31.5.101-10.31.5.110").unwrap(), ["10.31.5.101/32", "10.31.5.102/31", "10.31.5.104/30", "10.31.5.108/31", "10.31.5.110/32"]);
        assert_eq!(s("10.31.5.0-10.31.5.255").unwrap(), ["10.31.5.0/24"]);
        assert!(s("B001-B100").unwrap_err().contains("设备编号"));
        assert!(s("10.0.0.9-10.0.0.1").is_err() && s("x.y").is_err());
        assert!(s("10.0.0.1-10.0.255.254").unwrap_err().contains("太碎"));
    }

    /// 规则：同网段的改目标（位置、写法不动）；没有的照最后一条 SRC-IP-CIDR 的写法加在它后面；已经是的不动；重叠、目标不存在、没有 SRC-IP-CIDR 规则的拒绝
    #[test]
    fn rules_assign() {
        let a = |spec: &str, to: &str| Assign { spec: spec.into(), to: to.into() };
        let (p, rows) = rules_patch(CFG, &[a("10.31.1.7", "节点05"), a("10.31.2.0-10.31.2.3", "节点10"), a("10.31.1.5/32", "节点05")]).unwrap();
        let ops: Vec<(&str, &str)> = rows.iter().map(|r| (r["cidr"].as_str().unwrap(), r["op"].as_str().unwrap())).collect();
        assert_eq!(ops, [("10.31.1.7/32", "retarget"), ("10.31.2.0/30", "add"), ("10.31.1.5/32", "same")]);
        let want = CFG
            .replacen("- SRC-IP-CIDR,10.31.1.7/32,节点07\n", "- SRC-IP-CIDR,10.31.1.7/32,节点05\n", 1)
            .replacen("- SRC-IP-CIDR,10.31.1.5/32,节点05\n", "- SRC-IP-CIDR,10.31.1.5/32,节点05\n- SRC-IP-CIDR,10.31.2.0/30,节点10\n", 1);
        assert_eq!(p.text, want);
        assert_eq!(p.edits.len(), 2);
        // 都已经是：一行不改
        let (p, _) = rules_patch(CFG, &[a("10.31.1.5", "节点05")]).unwrap();
        assert!(p.edits.is_empty());
        let e = |cfg: &str, xs: &[Assign]| rules_patch(cfg, xs).unwrap_err().join("；");
        assert!(e(CFG, &[a("10.31.1.0/24", "节点05")]).contains("和现有的规则重叠"));
        assert!(e(CFG, &[a("10.31.9.1", "没有")]).contains("没有出口或策略组「没有」"));
        assert!(e(CFG, &[a("10.31.9.0/24", "节点05"), a("10.31.9.7", "节点07")]).contains("重叠"));
        let none = CFG.replace("- SRC-IP-CIDR,10.31.1.7/32,节点07\n- 'SRC-IP-CIDR,10.31.1.10/32,节点10,no-resolve'\n- SRC-IP-CIDR,10.31.1.5/32,节点05\n", "");
        assert!(e(&none, &[a("10.31.9.1", "节点05")]).contains("还没有 SRC-IP-CIDR 规则"));
        let apart = CFG.replace("- SRC-IP-CIDR,10.31.1.5/32,节点05\n- DOMAIN-SUFFIX,cn,DIRECT\n", "- DOMAIN-SUFFIX,cn,DIRECT\n- SRC-IP-CIDR,10.31.1.5/32,节点05\n");
        assert!(e(&apart, &[a("10.31.9.1", "节点05")]).contains("不连在一起"));
        // 带引号、no-resolve 的那条在最后：新加的照它写
        let quoted = CFG.replace("- SRC-IP-CIDR,10.31.1.5/32,节点05\n", "");
        let (p, _) = rules_patch(&quoted, &[a("10.31.3.3", "节点07")]).unwrap();
        assert!(p.text.contains("- 'SRC-IP-CIDR,10.31.1.10/32,节点10,no-resolve'\n- 'SRC-IP-CIDR,10.31.3.3/32,节点07,no-resolve'\n"), "{}", p.text);
    }

    /// 结构：出口带快照里对上的入口；组带现在选的；规则按目标汇总，SRC-IP-CIDR 全列；没有凭据
    #[test]
    fn structure_view() {
        let snap = Snapshot { groups: vec![Group { name: "手动选择".into(), kind: "Selector".into(), now: Some("节点10".into()), all: vec![], used: Some(true) }], ..Default::default() };
        let v = structure(CFG, Some(&snap)).unwrap();
        assert_eq!(v["proxies"].as_array().unwrap().len(), 4);
        assert_eq!(v["proxies"][1], json!({"name": "节点10", "type": "anytls", "server": "relay.test", "port": 40010, "entry": null, "sync": null, "linked": null, "alive": null, "delay_ms": null}));
        assert_eq!(v["groups"][0]["now"], "节点10");
        assert_eq!(v["rules_total"], 5);
        assert_eq!(v["match"], "手动选择");
        assert_eq!(v["src_rules"][1], json!({"index": 2, "cidr": "10.31.1.10/32", "to": "节点10", "to_kind": "exit"}));
        assert_eq!(v["rule_targets"][3], json!({"target": "DIRECT", "kind": "builtin", "rules": 1, "types": {"DOMAIN-SUFFIX": 1}, "src_cidrs": []}));
        let s = v.to_string();
        assert!(!s.contains("1111") && !s.contains("OLD_PBK") && !s.contains("2222") && !s.contains("\"pw\""), "{s}");
    }

    /// 隐去凭据：变更时间线算凭据的那几个字段、名字像密码 / 令牌的、块写法、流式映射、订阅地址的路径；同一次比较里同值同标记、值变了标记不同
    #[test]
    fn masking() {
        let salt = b"salt";
        let m = mask(CFG, salt);
        for leak in ["11111111-1111", "OLD_PBK", "abcd", "22222222-2222", "www.cloudflare.com", "www.apple.com", ": pw"] {
            assert!(!m.contains(leak), "{leak}\n{m}");
        }
        assert!(m.contains("  server: 198.51.100.2\n  port: 20007\n  uuid: <已隐去·") && m.contains("- name: 节点07\n  type: vless\n"), "{m}");
        assert_eq!(m.lines().count(), CFG.lines().count());
        // 同值同标记；别的盐标记不同
        let tagged = |s: &str| s.lines().find(|l| l.contains("uuid:")).unwrap().to_string();
        assert_eq!(tagged(&mask(CFG, salt)), tagged(&m));
        assert_ne!(tagged(&mask(CFG, b"other")), tagged(&m));
        let rotated = CFG.replacen("uuid: 11111111-1111-4111-8111-111111111111", "uuid: 33333333-3333-4333-8333-333333333333", 1);
        assert_ne!(tagged(&mask(&rotated, salt)), tagged(&m));
        let more = "secret: abc\nauthentication:\n  - \"user:pass\"\nproxy-providers:\n  airport:\n    url: https://sub.example/api/v1/client/subscribe?token=SECRET123\n    path: ./a.yaml\n\
proxies:\n  - {name: a, type: vless, server: s, port: 1, uuid: U-123, reality-opts: {public-key: PK9, short-id: 'ab'}}\n\
  - name: b\n    type: socks5\n    username: someone\n    password: \"p w\" # 注释\n    private-key: |\n      LINE1\n      LINE2\n    port: 2\n\
dns:\n  nameserver:\n    - https://u:p@dns.example/dns-query?x=1\n";
        let m = mask(more, salt);
        for leak in ["abc", "user:pass", "SECRET123", "client/subscribe", "U-123", "PK9", "'ab'", "someone", "p w", "LINE1", "LINE2", "u:p@"] {
            assert!(!m.contains(leak), "{leak}\n{m}");
        }
        assert!(m.contains("url: https://sub.example/<已隐去·") && m.contains("{name: a, type: vless, server: s, port: 1, uuid: <已隐去·") && m.contains("# 注释\n") && m.contains("    port: 2\n"), "{m}");
        assert!(m.contains("https://<已隐去·") && m.contains("@dns.example/dns-query?<已隐去·"), "{m}");
    }

    /// 逐行比较：统一格式的块、行号；完全一样时 identical；差太多不列
    #[test]
    fn line_diff() {
        let a = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        let b = "a\nb\nX\nd\ne\nf\ng\nh\ni\nj\nk\n";
        let d = diff(a, b, "备份", "现在").unwrap();
        assert_eq!((d["identical"].as_bool(), d["added"].as_u64(), d["removed"].as_u64()), (Some(false), Some(2), Some(1)));
        assert_eq!(d["hunks"].as_array().unwrap().len(), 2);
        assert_eq!(d["text"], "--- 备份\n+++ 现在\n@@ -1,6 +1,6 @@\n a\n b\n-c\n+X\n d\n e\n f\n@@ -8,3 +8,4 @@\n h\n i\n j\n+k\n");
        assert_eq!(diff(a, a, "x", "y").unwrap()["identical"], true);
        assert_eq!(diff("", "a\n", "x", "y").unwrap()["text"], "--- x\n+++ y\n@@ -0,0 +1,1 @@\n+a\n");
        let big_a: String = (0..4000).map(|i| format!("{i}\n")).collect();
        let big_b: String = (0..4000).map(|i| format!("x{i}\n")).collect();
        assert!(diff(&big_a, &big_b, "x", "y").is_err());
        // 随便几组：按改动重放能得到新的那份
        for (x, y) in [("a\nb\n", "b\na\n"), ("1\n2\n3\n", ""), ("", ""), ("p\nq\nr\ns\n", "q\ns\nt\n")] {
            let xa: Vec<&str> = x.lines().collect();
            let yb: Vec<&str> = y.lines().collect();
            let ops = diff_ops(&xa, &yb, 100).unwrap();
            let rebuilt: Vec<&str> = ops.iter().filter_map(|o| match o { Op::Eq(_, j) | Op::Ins(j) => Some(yb[*j]), Op::Del(_) => None }).collect();
            assert_eq!(rebuilt, yb);
            let kept: Vec<&str> = ops.iter().filter_map(|o| match o { Op::Eq(i, _) | Op::Del(i) => Some(xa[*i]), Op::Ins(_) => None }).collect();
            assert_eq!(kept, xa);
        }
    }

    #[test]
    fn backups_listing() {
        assert!(backup_name_ok("c.yaml.20261023-073012Z") && backup_name_ok("20261023-073012Z"));
        for bad in ["../x.20261023-073012Z", ".c.20261023-073012Z", "c.yaml", "c/y.20261023-073012Z", "c.yaml.2026102-073012Z", "c y.20261023-073012Z"] {
            assert!(!backup_name_ok(bad), "{bad}");
        }
        let (v, cfg) = parse_backups("config=/etc/openclash/config/c.yaml\nbk=c.yaml.20261020-000000Z 1200\nbk=c.yaml.20261023-073012Z  999\nbk=other.yaml.20261021-000000Z 5\nbk=junk 1\n");
        assert_eq!(cfg.as_deref(), Some("/etc/openclash/config/c.yaml"));
        assert_eq!(v.iter().map(|x| x["name"].as_str().unwrap()).collect::<Vec<_>>(), ["c.yaml.20261023-073012Z", "other.yaml.20261021-000000Z", "c.yaml.20261020-000000Z"]);
        assert_eq!((v[0]["size"].as_i64(), v[0]["current"].as_bool(), v[1]["current"].as_bool()), (Some(999), Some(true), Some(false)));
    }

    #[cfg(unix)]
    mod scripts {
        use super::super::super::replace::tests::{at, shells, Fake};
        use super::*;

        const S: &str = "20261023-010000Z";

        /// curl 桩：出口的延迟、组现在选的（GET 回 group.<组>，PUT 把 -d 的内容记进 put.log 并改掉它）、/proxies/<名字> 有没有、/rules
        fn api(delay: &[(&str, u32)], groups: &[&str], names: &[&str], rules: &str) -> String {
            let e = |n: &str| n.bytes().map(|b| format!("%{b:02X}")).collect::<String>();
            let mut c = String::new();
            for (n, ms) in delay {
                c.push_str(&format!("*/proxies/{}/delay*) echo '{{\"delay\":{ms}}}';;\n", e(n)));
            }
            for g in groups {
                let ge = e(g);
                c.push_str(&format!(
                    "*/proxies/{ge}) case \"$*\" in *PUT*) p=; b=; for a; do [ \"$p\" = -d ] && b=$a; p=$a; done; echo \"{g} $b\" >> @D@/put.log; printf '%s' \"$b\" | sed 's/.*\"name\":\"\\(.*\\)\"}}/\\1/' > '@D@/group.{ge}';; *) echo \"{{\\\"name\\\":\\\"{g}\\\",\\\"now\\\":\\\"$(cat '@D@/group.{ge}')\\\",\\\"type\\\":\\\"Selector\\\"}}\";; esac;;\n"
                ));
            }
            for n in names {
                c.push_str(&format!("*/proxies/{}) echo '{{\"name\":\"{n}\"}}';;\n", e(n)));
            }
            c.push_str(&format!("*/rules) echo '{rules}';;\n"));
            c
        }

        /// 重启：配置一行不动，重启一次，测通的报延迟、测不通的报 dead；没有 init 脚本不重启；API 起不来退出码 3
        #[test]
        fn restart_only() {
            for bb in shells() {
                let f = Fake::new(CFG, bb);
                let o = f.run(&restart_script(&["节点07".into(), "节点10".into()], at(S)), &[("节点07", 55)]);
                assert_eq!((o.code, o.kv("restarted"), o.kv("delay_1"), o.kv("dead_2"), o.kv("restart")), (0, Some("1"), Some("55"), Some("1"), Some("ok")), "busybox={bb} {o:?}");
                assert_eq!(f.read("etc/openclash/config/c.yaml"), CFG);
                assert_eq!(f.read("restart.log").lines().count(), 1);
                assert!(!f.path("etc/fleet-backup").exists());
                f.assert_no_leftovers();
                let o = f.run(&restart_script(&[], at(S)).replace("WAIT_API='120'", "WAIT_API='0'"), &[]);
                assert_eq!((o.code, o.kv("restarted")), (3, Some("1")), "{o:?}");
                std::fs::remove_file(f.path("etc/init.d/openclash")).unwrap();
                let o = f.run(&restart_script(&[], at(S)), &[]);
                assert!(o.code == 2 && o.stderr.contains("init.d/openclash（没装 OpenClash？），没有重启"), "{o:?}");
            }
        }

        fn checks_for(map: &[Rename], sel: Vec<String>) -> Checks {
            Checks {
                remap: map.iter().map(|r| (r.old.clone(), r.new.clone())).collect(),
                select_groups: sel,
                exist: map.iter().map(|r| r.new.clone()).collect(),
                gone: map.iter().filter(|r| !map.iter().any(|x| x.new == r.old)).map(|r| r.old.clone()).collect(),
                ..Default::default()
            }
        }

        /// 改名：路由上照中控算的改（secret 行原样留着、行号按去掉后的算），备份、重启一次；select 组原来选着改名的出口的，按新名字选回去；
        /// 重跑（已经改好）不写不重启；配置在中控读过之后又改过的一行不写
        #[test]
        fn patch_renames_and_reselects() {
            let map = [rn("节点07", "节点04", "198.51.100.2", 20007), rn("节点10", "节点07", "relay.test", 40010)];
            let (p, sel) = rename_patch(CFG, &map).unwrap();
            let c = checks_for(&map, sel);
            let on_router = format!("secret: s3\n{CFG}");
            for bb in shells() {
                let f = Fake::new(&on_router, bb);
                std::fs::write(f.path(&format!("group.{}", "手动选择".bytes().map(|b| format!("%{b:02X}")).collect::<String>())), "节点10").unwrap();
                let curl = api(&[], &["手动选择"], &["节点04", "节点07"], "");
                let o = f.run_curl(&patch_script(&p, &c, at(S), &[]), &curl);
                assert_eq!((o.code, o.kv("patch"), o.kv("changed"), o.kv("reselected")), (0, Some("ok"), Some("8"), Some("手动选择")), "busybox={bb} {o:?}");
                assert_eq!(f.read("etc/openclash/config/c.yaml"), format!("secret: s3\n{}", p.text), "busybox={bb}");
                assert_eq!(std::fs::read_to_string(f.path(&format!("etc/fleet-backup/openclash/c.yaml.{S}"))).unwrap(), on_router);
                assert_eq!(f.read("put.log"), "手动选择 {\"name\":\"节点07\"}\n", "原来选的节点10 改名成了节点07");
                assert_eq!(f.read("restart.log").lines().count(), 1);
                f.assert_no_leftovers();
                // 重跑：已经是改好的样子
                let o = f.run_curl(&patch_script(&p, &c, at("20261023-020000Z"), &[]), &curl);
                assert_eq!((o.code, o.kv("patch")), (0, Some("none")), "{o:?}");
                assert_eq!(f.read("restart.log").lines().count(), 1);
                // 中控读过之后又被改过
                let f = Fake::new(&on_router.replace("mixed-port: 7890", "mixed-port: 7891"), bb);
                let o = f.run_curl(&patch_script(&p, &c, at(S), &[]), &curl);
                assert!(o.code == 2 && o.stderr.contains("sha256 对不上"), "{o:?}");
                assert!(!f.path("etc/fleet-backup").exists() && f.read("restart.log").is_empty());
            }
        }

        /// 重启后核对不过（新名字没生效 / 组选不回去）：整份换回、再重启，原来的选择选回去，退出码 4
        #[test]
        fn patch_rolls_back_when_not_effective() {
            let map = [rn("节点07", "节点04", "198.51.100.2", 20007)];
            let (p, sel) = rename_patch(CFG, &map).unwrap();
            let c = checks_for(&map, sel);
            for bb in shells() {
                let f = Fake::new(CFG, bb);
                let o = f.run_curl(&patch_script(&p, &c, at(S), &[]), &api(&[], &[], &[], ""));
                assert_eq!((o.code, o.kv("rolled_back")), (4, Some("1")), "busybox={bb} {o:?}");
                assert!(o.stderr.contains("重启后 OpenClash 里没有「节点04」"), "{}", o.stderr);
                assert_eq!(f.read("etc/openclash/config/c.yaml"), CFG);
                assert_eq!(f.read("restart.log").lines().count(), 2);
                f.assert_no_leftovers();
            }
        }

        /// 规则：照中控算的插行、改目标；重启后目标出口测得通、/rules 里有这些网段才算；规则里没有就换回
        #[test]
        fn patch_rules() {
            let (p, rows) = rules_patch(CFG, &[Assign { spec: "10.31.2.0/30".into(), to: "节点10".into() }]).unwrap();
            assert_eq!(rows[0]["op"], "add");
            let c = Checks { delay: vec!["节点10".into()], rules: vec!["10.31.2.0/30".into()], ..Default::default() };
            for bb in shells() {
                let f = Fake::new(CFG, bb);
                let o = f.run_curl(&patch_script(&p, &c, at(S), &[]), &api(&[("节点10", 40)], &[], &[], "{\"rules\":[{\"type\":\"SrcIPCIDR\",\"payload\":\"10.31.2.0/30\",\"proxy\":\"节点10\"}]}"));
                assert_eq!((o.code, o.kv("patch"), o.kv("delay_1")), (0, Some("ok"), Some("40")), "busybox={bb} {o:?}");
                assert_eq!(f.read("etc/openclash/config/c.yaml"), p.text);
                let f = Fake::new(CFG, bb);
                let o = f.run_curl(&patch_script(&p, &c, at(S), &[]), &api(&[("节点10", 40)], &[], &[], "{\"rules\":[]}"));
                assert_eq!((o.code, o.kv("rolled_back")), (4, Some("1")), "{o:?}");
                assert_eq!(f.read("etc/openclash/config/c.yaml"), CFG);
            }
        }

        /// 备份列表与读备份（secret 行不出路由）
        #[test]
        fn backup_scripts() {
            for bb in shells() {
                let f = Fake::new(CFG, bb);
                let dir = f.path("etc/fleet-backup/openclash");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join(format!("c.yaml.{S}")), "secret: x\nmode: rule\n").unwrap();
                let o = f.run(&backups_script(), &[]);
                assert_eq!(o.code, 0, "{o:?}");
                let (v, cfg) = parse_backups(&o.stdout);
                assert!(cfg.unwrap().ends_with("/etc/openclash/config/c.yaml"));
                assert_eq!((v[0]["name"].as_str(), v[0]["size"].as_i64()), (Some(format!("c.yaml.{S}").as_str()), Some(21)));
                let o = f.run(&backup_read_script(S), &[]);
                assert_eq!((o.code, o.stdout.as_str()), (0, "mode: rule\n"), "{o:?}");
                let o = f.run(&backup_read_script("c.yaml.20200101-000000Z"), &[]);
                assert!(o.code == NO_BACKUP && o.stderr.contains("路由上没有备份"), "{o:?}");
            }
        }
    }
}
