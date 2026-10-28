//! 执行命令的审计不留凭据原文（P-21）：新写的在 `db::audit_insert` 打码；存量（含规则升级前打的）由启动时的 `db::sync_audit_redact`
//! 用同一份 Rust 规则重新打码，按 `detail.redact_version` 只处理旧版本的

use crate::common;
use fleet_server::api::audit::{redact_command, REDACT_VERSION};
use fleet_server::db;
use serde_json::{json, Value};

const CMDS: &[&str] = &[
    "PASSWORD=xxx ./deploy.sh",
    "mysql -uroot -phunter2 db",
    "curl -H \"Authorization: Bearer eyJabc def\" https://u:pw@h/x",
    "echo hunter2 | passwd --stdin root",
    "uptime",
];

fn sha(s: &str) -> String { hex::encode(<sha2::Sha256 as sha2::Digest>::digest(s.as_bytes())) }

#[tokio::test]
async fn startup_reredacts_stored_exec_commands_like_new_writes() {
    let (pool, _schema) = common::db_until(i64::MAX).await;
    for (i, c) in CMDS.iter().enumerate() {
        // 旧写法：原文进 detail 和 search、原文的无盐 sha256；第一条假装是旧版规则打过的
        let v = if i == 0 { json!(0) } else { Value::Null };
        sqlx::query("insert into audit_log (actor, action, target_kind, detail, search) values ('a', 'exec', 'machine', $1, $2)")
            .bind(json!({"name": "n", "command": c, "exit_code": 0, "command_sha256": sha(c), "redact_version": v}))
            .bind(format!("执行命令 n · {c} · 退出码 0"))
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("insert into audit_log (actor, action, detail) values ('a', 'ssh.login', '{\"command\": true}')").execute(&pool).await.unwrap();

    assert_eq!(db::sync_audit_redact(&pool).await.unwrap(), CMDS.len() as u64);
    let rows: Vec<(Value, String)> = sqlx::query_as("select detail, search from audit_log where action = 'exec' order by id").fetch_all(&pool).await.unwrap();
    for (c, (d, search)) in CMDS.iter().zip(&rows) {
        let m = redact_command(c);
        assert_eq!(d["command"], m, "{c}");
        assert_eq!(d["command_sha256"], sha(&m));
        assert_eq!(d["redact_version"], REDACT_VERSION);
        assert!(!search.contains(c) || m == *c, "{search}");
    }
    let plain: i64 = sqlx::query_scalar("select count(*) from audit_log where detail::text ~ '(xxx|hunter2|:pw@|eyJabc)' or search ~ '(xxx|hunter2|:pw@|eyJabc)'").fetch_one(&pool).await.unwrap();
    assert_eq!(plain, 0, "历史里不该再有明文");
    let ssh: Value = sqlx::query_scalar("select detail from audit_log where action = 'ssh.login'").fetch_one(&pool).await.unwrap();
    assert_eq!(ssh, json!({"command": true}));
    // 已是当前版本的不再处理
    assert_eq!(db::sync_audit_redact(&pool).await.unwrap(), 0);

    // 新写入：同样打码、带版本
    let id = db::audit_insert(&pool, "a", "exec", "machine", None, json!({"name": "n", "command": "PASSWORD=xxx run", "started": true})).await.unwrap();
    let (d, search): (Value, String) = sqlx::query_as("select detail, search from audit_log where id = $1").bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(d["command"], "PASSWORD=*** run");
    assert_eq!(d["redact_version"], REDACT_VERSION);
    assert!(!d.to_string().contains("xxx") && !search.contains("xxx"), "{d} {search}");
}

/// 复审第二轮：重脱敏按库里记下的版本（app_setting）判断，版本没变直接跳过、不再每次启动全表扫；分批提交，超过一批的也都处理完
#[tokio::test]
async fn reredact_runs_in_batches_and_skips_when_version_unchanged() {
    let (pool, _schema) = common::db_until(i64::MAX).await;
    let old = |n: i64| {
        let pool = pool.clone();
        async move {
            sqlx::query("insert into audit_log (actor, action, target_kind, detail, search) select 'a', 'exec', 'machine', jsonb_build_object('name', 'n', 'command', 'PASSWORD=xxx run ' || g), 'x' from generate_series(1, $1) g")
                .bind(n)
                .execute(&pool)
                .await
                .unwrap();
        }
    };
    old(2500).await;
    assert_eq!(db::sync_audit_redact(&pool).await.unwrap(), 2500, "超过一批（1000 条）的也都处理完");
    let plain: i64 = sqlx::query_scalar("select count(*) from audit_log where detail::text like '%xxx%'").fetch_one(&pool).await.unwrap();
    assert_eq!(plain, 0);
    let v: i64 = sqlx::query_scalar("select (value #>> '{}')::bigint from app_setting where key = 'audit_redact_version'").fetch_one(&pool).await.unwrap();
    assert_eq!(v, REDACT_VERSION, "记下已同步到的版本");
    // 版本没变：不再扫表（库里就算混进一条旧版本的也不碰——新写入都走 audit_insert，一定是当前版本）
    old(1).await;
    assert_eq!(db::sync_audit_redact(&pool).await.unwrap(), 0, "版本没变直接跳过");
}
