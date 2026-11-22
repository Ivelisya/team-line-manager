//! 面板的响应类型 v1/web/src/lib/types.gen.ts 由这里从 Rust 类型生成（ts-rs，derive 照 serde 的 rename / skip / flatten / tag）。
//!
//! - 重新生成：`FLEET_TYPES_GEN=write cargo test -p fleet-server --test types_gen`
//! - 核对（不带环境变量）：文件和现在生成的不一样就失败。CI 的 rust-lint 跑 ci/check-types-gen.sh（重新生成后 git diff 不许有差别）
//!
//! 根类型列在 `ROOTS` 里，它们引用到的类型（字段里的结构体、枚举）自动带上；整数（含 i64 / u64）一律是 number（JSON 里就是数字），
//! 时间字段在 Rust 里逐个标了 `#[ts(type = "string")]`（RFC 3339；Date 是 YYYY-MM-DD）。不需要数据库。

use std::collections::BTreeMap;
use std::path::PathBuf;
use ts_rs::{Config, TypeVisitor, TS};

/// 按名字收集声明（同名只收一次），顺着字段类型往下走
struct Collect<'a> {
    cfg: &'a Config,
    decls: BTreeMap<String, String>,
}

impl TypeVisitor for Collect<'_> {
    fn visit<T: TS + 'static + ?Sized>(&mut self) {
        // 没有自己声明的（Vec、Option、HashMap、数字、字符串……）只看它的类型参数
        if T::output_path().is_none() {
            T::visit_generics(self);
            return;
        }
        let name = T::ident(self.cfg);
        if self.decls.contains_key(&name) {
            return;
        }
        let docs = T::docs().unwrap_or_default();
        self.decls.insert(name, format!("{docs}export {}", T::decl(self.cfg)));
        T::visit_dependencies(self);
    }
}

macro_rules! roots {
    ($c:expr; $($t:ty),* $(,)?) => { $( $c.visit::<$t>(); )* };
}

fn generate() -> String {
    let cfg = Config::new().with_large_int("number");
    let mut c = Collect { cfg: &cfg, decls: BTreeMap::new() };
    roots!(c;
        // 机器视图（GET /machines、/machines/{id}）及其里面的全部类型
        fleet_server::views::MachineView,
        // 总览（GET /overview、实时推送的 overview）与首页的另外几个接口
        fleet_server::views::Overview,
        fleet_server::api::misc::OverviewSeries,
        fleet_server::api::misc::ProbeBeats,
        fleet_server::api::misc::MetaOut,
        fleet_server::ping::PingBeats,
        // 告警、静默、作业（列表与详情在这些行上另加字段，见 docs/API.md）
        fleet_server::api::incidents::IncidentOut,
        fleet_server::api::incidents::Silence,
        fleet_server::api::jobs::JobOut,
        fleet_server::api::jobs::StepOut,
        fleet_server::api::jobs::LogOut,
    );
    let mut out = String::from(
        "// 由 fleet-server 生成（crates/fleet-server/tests/types_gen.rs，ts-rs），不要手改。\n\
         // 重新生成：cd v1 && FLEET_TYPES_GEN=write cargo test -p fleet-server --test types_gen\n\
         // 时间字段是 RFC 3339 字符串（日期是 YYYY-MM-DD），整数都是 number。\n",
    );
    for d in c.decls.values() {
        out.push('\n');
        out.push_str(d.trim_end());
        out.push('\n');
    }
    out
}

fn target() -> PathBuf { PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../web/src/lib/types.gen.ts") }

#[test]
fn types_gen_is_up_to_date() {
    let want = generate();
    let path = target();
    if std::env::var("FLEET_TYPES_GEN").as_deref() == Ok("write") {
        std::fs::write(&path, &want).unwrap();
        return;
    }
    let have = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        have == want,
        "{} 和 Rust 类型对不上：cd v1 && FLEET_TYPES_GEN=write cargo test -p fleet-server --test types_gen 重新生成后一起提交",
        path.display()
    );
}
