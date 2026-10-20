//! nodeagent：落地节点主机遥测（只读），每 10 s 采样、每 30 s 上报一批到 {endpoint}/v2/ingest/node。
fn main() {
    let o = fleet_edge::runner::parse_args("node", "nodeagent");
    std::process::exit(fleet_edge::runner::run(o, None));
}
