//! 显示名只在读取时计算，从不作为身份。

/// 节点显示名：`团队1-06`
pub fn node_name(team: &str, number: i32) -> String { format!("{team}-{number:02}") }

/// 机器显示名：线路机用 label，节点用 团队-编号
pub fn display_name(kind: &str, label: Option<&str>, team: Option<&str>, number: Option<i32>) -> String {
    match (kind, label, team, number) {
        ("relay", Some(l), _, _) => l.to_string(),
        (_, _, Some(t), Some(n)) => node_name(t, n),
        (_, Some(l), _, _) => l.to_string(),
        _ => "未命名".to_string(),
    }
}

/// 线路机直出入口（入口的节点就是线路机自己）的显示名：`线路机2-直出`。入口备注、分享链接 #后的名字、入口列表的节点列都用它
pub fn direct_name(relay: &str) -> String { format!("{relay}-直出") }

/// 入口的显示名（备注 / 分享链接名）：节点的显示名；直出入口是 `线路机名-直出`
pub fn entry_name(direct: bool, kind: &str, label: Option<&str>, team: Option<&str>, number: Option<i32>) -> String {
    let n = display_name(kind, label, team, number);
    if direct {
        direct_name(&n)
    } else {
        n
    }
}

/// 重新编号：按现有编号顺序压缩成 1..n。返回 (machine_id, 旧号, 新号)，只含有变化的。
/// 新号 ≤ 旧号，所以按新号升序逐条更新不会撞唯一索引。
pub fn compact_numbers<I: Clone>(mut nodes: Vec<(I, i32)>) -> Vec<(I, i32, i32)> {
    nodes.sort_by_key(|(_, n)| *n);
    nodes
        .into_iter()
        .enumerate()
        .filter_map(|(i, (id, old))| {
            let new = i as i32 + 1;
            (new != old).then_some((id, old, new))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(node_name("团队1", 6), "团队1-06");
        assert_eq!(node_name("团队2", 120), "团队2-120");
        assert_eq!(display_name("relay", Some("线路机2"), None, None), "线路机2");
        assert_eq!(display_name("vps", None, Some("团队1"), Some(3)), "团队1-03");
        assert_eq!(entry_name(true, "relay", Some("线路机2"), None, None), "线路机2-直出");
        assert_eq!(entry_name(false, "vps", None, Some("团队1"), Some(3)), "团队1-03");
    }

    #[test]
    fn compaction_moves_down_only() {
        let m = compact_numbers(vec![("a", 1), ("c", 5), ("b", 3), ("d", 9)]);
        assert_eq!(m, vec![("b", 3, 2), ("c", 5, 3), ("d", 9, 4)]);
        assert!(m.iter().all(|(_, o, n)| n < o));
        assert!(compact_numbers(vec![("a", 1), ("b", 2)]).is_empty());
    }
}
