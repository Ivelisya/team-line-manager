//! 执行命令的审计不留凭据原文（P-21）：新写的在 `db::audit_insert` 打码，存量由迁移 0097 用同一组规则打码，两边结果一致

use crate::common;
use fleet_server::api::audit::redact_command;
use fleet_server::db;
use serde_json::{json, Value};

const CMDS: &[&str] = &[
    "PASSWORD=xxx ./deploy.sh",
    "export DB_PASS='a b c'; mysql_token=\"q\" x",
    "curl --password hunter2 -u x --token=abc",
    "curl -H \"Authorization: Bearer eyJabc.def\" https://u:pw@h/x",
    "echo QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlq | base64 -d",
    "uptime",
];

fn sha(s: &str) -> String { hex::encode(<sha2::Sha256 as sha2::Digest>::digest(s.as_bytes())) }

#[tokio::test]
async fn migration_masks_stored_exec_commands_like_new_writes() {
    let (pool, _schema) = common::db_until(93).await;
    for c in CMDS {
        // 迁移前的写法：命令原文进 detail 和 search
        sqlx::query("insert into audit_log (actor, action, target_kind, detail, search) values ('a', 'exec', 'machine', $1, $2)")
            .bind(json!({"name": "n", "command": c, "exit_code": 0}))
            .bind(format!("执行命令 n · {c} · 退出码 0"))
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("insert into audit_log (actor, action, detail) values ('a', 'ssh.login', '{\"command\": true}')").execute(&pool).await.unwrap();
    common::migrator(None).run(&pool).await.unwrap();

    let rows: Vec<(Value, String)> = sqlx::query_as("select detail, search from audit_log where action = 'exec' order by id").fetch_all(&pool).await.unwrap();
    for (c, (d, search)) in CMDS.iter().zip(&rows) {
        assert_eq!(d["command"], redact_command(c), "{c}");
        assert_eq!(d["command_sha256"], sha(c));
        assert!(search.contains(&redact_command(c)), "{search}");
    }
    let plain: i64 = sqlx::query_scalar("select count(*) from audit_log where detail::text ~ '(xxx|hunter2|:pw@|eyJabc)' or search ~ '(xxx|hunter2|:pw@|eyJabc)'").fetch_one(&pool).await.unwrap();
    assert_eq!(plain, 0, "历史里不该再有明文");
    let ssh: Value = sqlx::query_scalar("select detail from audit_log where action = 'ssh.login'").fetch_one(&pool).await.unwrap();
    assert_eq!(ssh, json!({"command": true}));

    // 新写入：同样打码
    let id = db::audit_insert(&pool, "a", "exec", "machine", None, json!({"name": "n", "command": "PASSWORD=xxx run", "started": true})).await.unwrap();
    let (d, search): (Value, String) = sqlx::query_as("select detail, search from audit_log where id = $1").bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(d["command"], "PASSWORD=*** run");
    assert!(!d.to_string().contains("xxx") && !search.contains("xxx"), "{d} {search}");
}
