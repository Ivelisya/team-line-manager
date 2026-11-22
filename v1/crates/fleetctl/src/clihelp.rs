//! 命令行本身的两件事，都直接从 clap 的定义生成（不会和真实参数对不上）：
//! - 参数写错时：带了 `--json` 就在 stderr 给统一的错误 JSON（`{"error":{"code":"usage",…}}`），退出码仍是 2；
//!   `--help` / `--version` 照旧原样输出。
//! - `fleetctl help --all [--json] [命令…]`：一次列出全部命令与子命令的说明、参数、选项（给脚本和 AI，省得逐个 --help）。
//!   clap 自带的 `help` 子命令不收选项，所以在交给 clap 之前先认出这一种写法；`fleetctl help <命令>` 仍归 clap。

use clap::builder::PossibleValue;
use clap::error::ErrorKind;
use clap::{Arg, ArgAction, Command};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::process::ExitCode;

/// 命令行里有没有 `--json`（`--` 之后是远端命令之类的原样参数，不算）
pub fn wants_json(argv: &[OsString]) -> bool { argv.iter().skip(1).take_while(|a| *a != "--").any(|a| a == "--json") }

/// clap 解析失败：`--help` / `--version`（以及不带 --json 时的一切）照 clap 原样输出并退出；
/// 带 --json 时 stderr 打一行错误 JSON，退出码同 clap（参数错误是 2）
pub fn usage_error(e: clap::Error, argv: &[OsString]) -> ExitCode {
    if !wants_json(argv) || matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
        e.exit()
    }
    eprintln!("{}", usage_json(&e));
    ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(2))
}

fn usage_json(e: &clap::Error) -> Value {
    let text = e.render().to_string();
    let msg = text.trim();
    let msg = msg.strip_prefix("error: ").unwrap_or(msg);
    json!({"error": {"code": "usage", "message": msg, "status": null}})
}

/// 认出 `fleetctl [--json] help --all [--json] [命令…]`：是就打印命令一览并返回退出码，不是返回 None（交给 clap）
pub fn catalogue_request(argv: &[OsString], cmd: impl FnOnce() -> Command) -> Option<ExitCode> {
    let json = wants_json(argv);
    let rest: Vec<&OsString> = argv.iter().skip(1).filter(|a| *a != "--json").collect();
    if rest.first().map(|a| a.as_os_str()) != Some("help".as_ref()) || !rest.iter().any(|a| *a == "--all") {
        return None;
    }
    let mut path = Vec::new();
    let mut bad = None;
    for a in &rest[1..] {
        if *a == "--all" {
            continue;
        }
        match a.to_str() {
            Some(s) if !s.starts_with('-') => path.push(s.to_string()),
            _ => bad = Some(a.to_string_lossy().into_owned()),
        }
    }
    let root = cmd();
    let res = match bad {
        Some(b) => Err(format!("help --all 只认 --json 和命令名（如 help --all router），不认「{b}」")),
        None => catalogue(&root, &path),
    };
    match res {
        Ok(v) => {
            crate::cmds::outln(if json { v.to_string() } else { render_text(&v) });
            Some(ExitCode::SUCCESS)
        }
        Err(msg) => {
            if json {
                eprintln!("{}", json!({"error": {"code": "usage", "message": msg, "status": null}}));
            } else {
                eprintln!("错误：{msg}");
            }
            Some(ExitCode::from(2))
        }
    }
}

/// 命令一览：`{"global_flags":[…],"commands":[{"path","about","args","flags","subcommands","subcommand_required"}…]}`。
/// 参数 / 选项的字段：name、short、value（带值时的占位名）、required、multiple、possible_values、default、env、help；
/// 空的（null、false、空数组）不写。
/// `path` 给了就只列这个命令和它下面的（`["router"]` = router 与 router 的各子命令）
pub fn catalogue(root: &Command, path: &[String]) -> Result<Value, String> {
    let mut start = root;
    for (i, p) in path.iter().enumerate() {
        start = start.get_subcommands().find(|c| c.get_name() == p || c.get_all_aliases().any(|a| a == p)).ok_or_else(|| format!("没有这个命令：{}", path[..=i].join(" ")))?;
    }
    let mut out = Vec::new();
    let prefix: Vec<String> = path.iter().take(path.len().saturating_sub(1)).cloned().collect();
    if path.is_empty() {
        for c in root.get_subcommands().filter(|c| !c.is_hide_set()) {
            walk(c, &[], &mut out);
        }
    } else {
        walk(start, &prefix, &mut out);
    }
    let global: Vec<Value> = root.get_arguments().filter(|a| a.is_global_set() && !a.is_hide_set()).map(flag_json).collect();
    Ok(json!({"global_flags": global, "commands": out}))
}

fn walk(c: &Command, parent: &[String], out: &mut Vec<Value>) {
    let mut path = parent.to_vec();
    path.push(c.get_name().to_string());
    let args: Vec<&Arg> = c.get_arguments().filter(|a| !a.is_hide_set() && !a.is_global_set() && !is_builtin(a)).collect();
    let subs: Vec<&Command> = c.get_subcommands().filter(|s| !s.is_hide_set()).collect();
    out.push(compact(json!({
        "path": path.join(" "),
        "about": c.get_about().map(|s| s.to_string()),
        "args": args.iter().filter(|a| a.is_positional()).map(|a| positional_json(a)).collect::<Vec<_>>(),
        "flags": args.iter().filter(|a| !a.is_positional()).map(|a| flag_json(a)).collect::<Vec<_>>(),
        "subcommands": subs.iter().map(|s| s.get_name()).collect::<Vec<_>>(),
        "subcommand_required": c.is_subcommand_required_set(),
    })));
    for s in subs {
        walk(s, &path, out);
    }
}

/// clap 在 build 时才加的 --help / --version（没 build 的定义里本来没有，以防万一）
fn is_builtin(a: &Arg) -> bool { matches!(a.get_action(), ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version) }

fn value_name(a: &Arg) -> String {
    a.get_value_names().and_then(|v| v.first()).map(|s| s.to_string()).unwrap_or_else(|| a.get_id().as_str().to_uppercase())
}

fn multiple(a: &Arg) -> bool { matches!(a.get_action(), ArgAction::Append | ArgAction::Count) || a.get_num_args().is_some_and(|n| n.max_values() > 1) }

/// 取值清单（开关型的 --yes 之类不列 true / false）
fn possible(a: &Arg) -> Vec<String> {
    if !a.get_action().takes_values() {
        return Vec::new();
    }
    a.get_possible_values().iter().filter(|p| !p.is_hide_set()).map(PossibleValue::get_name).map(String::from).collect() }

fn defaults(a: &Arg) -> Vec<String> { a.get_default_values().iter().map(|v| v.to_string_lossy().into_owned()).collect() }

fn positional_json(a: &Arg) -> Value {
    compact(json!({
        "name": a.get_id().as_str(),
        "value": value_name(a),
        "required": a.is_required_set(),
        "multiple": multiple(a),
        "possible_values": possible(a),
        "default": defaults(a),
        "help": a.get_help().map(|s| s.to_string()),
    }))
}

fn flag_json(a: &Arg) -> Value {
    let name = match (a.get_long(), a.get_short()) {
        (Some(l), _) => format!("--{l}"),
        (None, Some(s)) => format!("-{s}"),
        (None, None) => a.get_id().as_str().to_string(),
    };
    compact(json!({
        "name": name,
        "short": a.get_short().map(|s| format!("-{s}")),
        "value": a.get_action().takes_values().then(|| value_name(a)),
        "required": a.is_required_set(),
        "multiple": multiple(a),
        "possible_values": possible(a),
        "default": defaults(a),
        "env": a.get_env().map(|e| e.to_string_lossy().into_owned()),
        "help": a.get_help().map(|s| s.to_string()),
    }))
}

/// 去掉空的字段（null、false、[]）：一览有一百多个命令，全写上要多一倍字符（AI 按字符数截断输出）
fn compact(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.retain(|_, x| !(x.is_null() || *x == Value::Bool(false) || x.as_array().is_some_and(|a| a.is_empty())));
    }
    v
}

/// 人看的版本：每个命令一行用法 + 一行说明，下面缩进列参数与选项
fn render_text(v: &Value) -> String {
    let s = |x: &Value| x.as_str().unwrap_or("").to_string();
    let mut out = String::from("fleetctl 全部命令（--json 得到同样内容的 JSON）\n全局选项：");
    let g: Vec<String> = v["global_flags"].as_array().into_iter().flatten().map(|f| format!("{}  {}", s(&f["name"]), s(&f["help"]))).collect();
    out.push_str(&g.join("；"));
    out.push('\n');
    for c in v["commands"].as_array().into_iter().flatten() {
        let mut usage = format!("fleetctl {}", s(&c["path"]));
        for a in c["args"].as_array().into_iter().flatten() {
            let mut n = format!("<{}>", s(&a["value"]));
            if a["multiple"] == true {
                n.push_str("...");
            }
            usage.push(' ');
            usage.push_str(&if a["required"] == true { n } else { format!("[{n}]") });
        }
        if c["flags"].as_array().is_some_and(|f| !f.is_empty()) {
            usage.push_str(" [选项]");
        }
        if c["subcommands"].as_array().is_some_and(|f| !f.is_empty()) {
            usage.push_str(if c["subcommand_required"] == true { " <子命令>" } else { " [子命令]" });
        }
        out.push('\n');
        out.push_str(&usage);
        out.push('\n');
        if let Some(a) = c["about"].as_str() {
            out.push_str(&format!("    {}\n", a.lines().next().unwrap_or("")));
        }
        let rows: Vec<(String, String)> = c["args"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|a| (format!("<{}>", s(&a["value"])), a))
            .chain(c["flags"].as_array().into_iter().flatten().map(|f| {
                let mut n = s(&f["name"]);
                if let Some(sh) = f["short"].as_str().filter(|sh| !n.starts_with(*sh)) {
                    n = format!("{sh}, {n}");
                }
                if let Some(val) = f["value"].as_str() {
                    n.push_str(&format!(" <{val}>"));
                }
                (n, f)
            }))
            .map(|(n, a)| (n, detail(a)))
            .collect();
        let w = rows.iter().map(|(n, _)| n.chars().count()).max().unwrap_or(0);
        for (n, d) in rows {
            let pad = " ".repeat(w - n.chars().count());
            out.push_str(format!("      {n}{pad}  {d}").trim_end());
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}

fn detail(a: &Value) -> String {
    let list = |k: &str| a[k].as_array().map(|v| v.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" | ")).unwrap_or_default();
    let mut d = a["help"].as_str().and_then(|h| h.lines().next()).unwrap_or("").to_string();
    if a["required"] == true {
        d.push_str(" [必填]");
    }
    if a["multiple"] == true {
        d.push_str(" [可多个]");
    }
    let p = list("possible_values");
    if !p.is_empty() {
        d.push_str(&format!(" [取值：{p}]"));
    }
    let df = list("default");
    if !df.is_empty() {
        d.push_str(&format!(" [默认：{df}]"));
    }
    if let Some(e) = a["env"].as_str() {
        d.push_str(&format!(" [环境变量：{e}]"));
    }
    d.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser, Subcommand};

    #[derive(Parser)]
    #[command(name = "t")]
    struct T {
        /// 输出 JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        cmd: C,
    }

    #[derive(Subcommand)]
    enum C {
        /// 列出
        Ls {
            /// 团队
            #[arg(long, short)]
            team: Option<String>,
            #[arg(long, value_parser = ["a", "b"], default_value = "a")]
            kind: String,
            /// 确认
            #[arg(long)]
            yes: bool,
        },
        /// 一组
        Grp {
            #[command(subcommand)]
            cmd: G,
        },
    }

    #[derive(Subcommand)]
    enum G {
        /// 显示
        Show { name: String, rest: Vec<String> },
    }

    fn os(a: &[&str]) -> Vec<OsString> { a.iter().map(OsString::from).collect() }

    #[test]
    fn json_flag_detection_stops_at_double_dash() {
        assert!(wants_json(&os(&["t", "--json", "ls"])) && wants_json(&os(&["t", "ls", "--json"])));
        assert!(!wants_json(&os(&["t", "ssh", "x", "--", "--json"])) && !wants_json(&os(&["t", "ls"])));
    }

    #[test]
    fn catalogue_lists_every_command_with_args() {
        let v = catalogue(&T::command(), &[]).unwrap();
        let paths: Vec<&str> = v["commands"].as_array().unwrap().iter().map(|c| c["path"].as_str().unwrap()).collect();
        assert_eq!(paths, ["ls", "grp", "grp show"]);
        assert_eq!(v["global_flags"][0]["name"], "--json");
        let ls = &v["commands"][0];
        assert_eq!(ls["about"], "列出");
        assert_eq!((ls["flags"][0]["name"].as_str(), ls["flags"][0]["short"].as_str(), ls["flags"][0]["value"].as_str()), (Some("--team"), Some("-t"), Some("TEAM")));
        assert_eq!((ls["flags"][1]["possible_values"].clone(), ls["flags"][1]["default"].clone()), (json!(["a", "b"]), json!(["a"])));
        // 开关不带值、不列 true / false
        assert_eq!((ls["flags"][2]["name"].as_str(), ls["flags"][2].get("value"), ls["flags"][2].get("possible_values")), (Some("--yes"), None, None));
        // 子命令里不重复列全局的 --json，也没有 clap 自己加的 --help
        assert!(ls["flags"].as_array().unwrap().iter().all(|f| f["name"] != "--json" && f["name"] != "--help"));
        let show = &v["commands"][2];
        assert_eq!((show["args"][0]["required"].as_bool(), show["args"][1]["multiple"].as_bool()), (Some(true), Some(true)));
        assert_eq!(v["commands"][1]["subcommands"], json!(["show"]));
        // 只看一组；没有的命令报错
        let g = catalogue(&T::command(), &["grp".into()]).unwrap();
        assert_eq!(g["commands"].as_array().unwrap().iter().map(|c| c["path"].as_str().unwrap()).collect::<Vec<_>>(), ["grp", "grp show"]);
        assert!(catalogue(&T::command(), &["nope".into()]).unwrap_err().contains("nope"));
        let text = render_text(&v);
        assert!(text.contains("fleetctl grp show <NAME> [<REST>...]") && text.contains("[取值：a | b] [默认：a]"), "{text}");
    }

    #[test]
    fn catalogue_request_only_takes_help_all() {
        assert!(catalogue_request(&os(&["t", "help"]), T::command).is_none());
        assert!(catalogue_request(&os(&["t", "help", "ls"]), T::command).is_none());
        assert!(catalogue_request(&os(&["t", "ls", "--all"]), T::command).is_none());
        // help --all 后面跟了别的选项：自己报参数错误（退出码 2），不交给 clap 报一句看不懂的「unrecognized subcommand '--all'」
        assert_eq!(catalogue_request(&os(&["t", "help", "--all", "--bogus"]), T::command), Some(ExitCode::from(2)));
    }

    #[test]
    fn usage_error_json_shape() {
        let e = T::try_parse_from(["t", "ls", "--nope"]).err().unwrap();
        let v = usage_json(&e);
        assert_eq!((v["error"]["code"].as_str(), v["error"]["status"].is_null()), (Some("usage"), true));
        let m = v["error"]["message"].as_str().unwrap();
        assert!(m.contains("--nope") && !m.starts_with("error:"), "{m}");
        assert_eq!(e.exit_code(), 2);
    }
}
