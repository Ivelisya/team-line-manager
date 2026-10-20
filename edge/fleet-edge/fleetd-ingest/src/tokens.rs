//! agent 令牌：`fleetd agent-token issue <host_id> [--name N]`（只打印一次明文）/ `revoke <host_id> [--name N]` / `list`。
//! 数据库里只存 sha256。以 root 运行而数据库属于其他用户时拒绝执行（否则 WAL 文件会变成 root 所有，fleetd 打不开）。
use crate::{now, sha256_hex, Ingest};
use rusqlite::params;
use serde_json::{json, Value};
use std::io::Read;
use std::os::unix::fs::MetadataExt;

pub fn new_token() -> Result<String, String> {
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).map_err(|e| format!("读取随机数失败：{e}"))?;
    Ok(format!("fe1_{}", hex::encode(b)))
}

pub fn valid_host_id(h: &str) -> bool {
    !h.is_empty() && h.len() <= 64 && !h.chars().any(|c| c.is_control() || c == '/' || c.is_whitespace())
}

impl Ingest {
    pub fn issue_token(&self, host: &str, name: &str) -> Result<String, String> {
        if !valid_host_id(host) {
            return Err("host_id 无效（不能为空、含空白或 /，≤ 64 字节）".into());
        }
        let t = new_token()?;
        let h = sha256_hex(&t);
        // 令牌写入要落盘（库平时是 synchronous=NORMAL）
        self.with_db(|c| {
            crate::commit_pending(c);
            let _ = c.execute_batch("PRAGMA synchronous=FULL");
            let r = c.execute("INSERT INTO agent_tokens(host_id, name, sha256, created_at) VALUES(?1,?2,?3,?4)", params![host, name, h, now()]);
            let _ = c.execute_batch("PRAGMA synchronous=NORMAL");
            r
        })
        .map_err(|e| e.to_string())?;
        Ok(t)
    }

    pub fn revoke_tokens(&self, host: &str, name: Option<&str>) -> Result<usize, String> {
        self.with_db(|c| {
            crate::commit_pending(c);
            let _ = c.execute_batch("PRAGMA synchronous=FULL");
            let r = match name {
                Some(n) => c.execute("UPDATE agent_tokens SET revoked_at=?3 WHERE host_id=?1 AND name=?2 AND revoked_at IS NULL", params![host, n, now()]),
                None => c.execute("UPDATE agent_tokens SET revoked_at=?2 WHERE host_id=?1 AND revoked_at IS NULL", params![host, now()]),
            };
            let _ = c.execute_batch("PRAGMA synchronous=NORMAL");
            r
        })
        .map_err(|e| e.to_string())
    }

    pub fn list_tokens(&self) -> Vec<Value> {
        self.with_db(|c| {
            let mut st = c
                .prepare("SELECT t.host_id, t.name, substr(t.sha256,1,12), t.created_at, t.revoked_at, h.received_at FROM agent_tokens t LEFT JOIN host_latest h ON h.host_id=t.host_id ORDER BY t.host_id, t.id")
                .unwrap();
            st.query_map([], |r| {
                Ok(json!({"host_id": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?, "sha256_prefix": r.get::<_, String>(2)?,
                    "created_at": r.get::<_, f64>(3)?, "revoked_at": r.get::<_, Option<f64>>(4)?, "last_received": r.get::<_, Option<f64>>(5)?}))
            })
            .unwrap()
            .flatten()
            .collect()
        })
    }
}

fn euid() -> Option<u32> {
    std::fs::metadata("/proc/self").ok().map(|m| m.uid())
}

/// 命令行入口。`args` 是 `agent-token` 之后的参数。返回进程退出码。
pub fn cli(cfg: crate::IngestCfg, args: &[String]) -> i32 {
    let usage = "用法：agent-token issue <host_id> [--name N] | revoke <host_id> [--name N] | list";
    if let (Some(0), Ok(md)) = (euid(), std::fs::metadata(&cfg.db_path)) {
        if md.uid() != 0 {
            eprintln!("数据库 {} 属于 uid {}，请用该用户执行（例如 runuser -u fleetd -- …），不要用 root", cfg.db_path.display(), md.uid());
            return 2;
        }
    }
    let ing = match Ingest::open(cfg) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let mut name: Option<String> = None;
    let mut pos = vec![];
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--name" {
            name = it.next().cloned();
        } else {
            pos.push(a.clone());
        }
    }
    match (pos.first().map(|s| s.as_str()), pos.get(1)) {
        (Some("issue"), Some(h)) => match ing.issue_token(h, name.as_deref().unwrap_or("default")) {
            Ok(t) => {
                // 明文只出现这一次，只写 stdout（便于管道传给 rollout），说明写 stderr
                println!("{t}");
                eprintln!("已为 {h} 签发令牌（只显示这一次，数据库只存 sha256）");
                0
            }
            Err(e) => {
                eprintln!("签发失败：{e}");
                1
            }
        },
        (Some("revoke"), Some(h)) => match ing.revoke_tokens(h, name.as_deref()) {
            Ok(n) => {
                println!("已撤销 {h} 的 {n} 个令牌");
                if n == 0 {
                    3
                } else {
                    0
                }
            }
            Err(e) => {
                eprintln!("撤销失败：{e}");
                1
            }
        },
        (Some("list"), _) => {
            for t in ing.list_tokens() {
                println!("{t}");
            }
            0
        }
        _ => {
            eprintln!("{usage}");
            2
        }
    }
}
