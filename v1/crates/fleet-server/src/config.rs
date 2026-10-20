use anyhow::Context;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "d_listen")]
    pub listen: String,
    pub database_url: String,
    /// 32 字节随机数（二进制文件），用于加密库里的秘密字段
    pub secret_key_file: PathBuf,
    /// agent 访问中控用的地址（写进节点和线路机的 agent 配置）
    pub public_url: String,
    /// 前端构建产物目录；不存在则只提供 API
    #[serde(default)]
    pub web_dir: Option<PathBuf>,
    /// 中控登录机器用的私钥（上架、下线）
    pub ssh_key: PathBuf,
    /// 中控自己维护的 known_hosts（上架预检时记录指纹，之后严格校验）
    pub known_hosts: PathBuf,
    /// 推送到机器上的二进制：fleet-agent、ssserver、xray
    pub dist_dir: PathBuf,
    /// 位于反向代理之后时信任 X-Forwarded-For
    #[serde(default)]
    pub trust_forwarded_for: bool,
    #[serde(default)]
    pub retention: Retention,
    #[serde(default)]
    pub numbering: Numbering,
    #[serde(default = "d_workers")]
    pub job_workers: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Retention {
    /// 原始时序（主机指标、入口流量、探测）保留天数
    pub raw_days: u32,
    /// 访问记录保留天数
    pub access_days: u32,
}
impl Default for Retention {
    fn default() -> Self { Self { raw_days: 14, access_days: 7 } }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Numbering {
    /// 节点退役后自动把该团队编号压缩成 01..N（编号连续，数量一眼可见）
    pub auto_compact: bool,
}
impl Default for Numbering {
    fn default() -> Self { Self { auto_compact: true } }
}

fn d_listen() -> String { "127.0.0.1:8080".into() }
fn d_workers() -> usize { 8 }

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path).with_context(|| format!("读取配置 {}", path.display()))?;
        let c: Config = toml::from_str(&s).with_context(|| format!("解析配置 {}", path.display()))?;
        Ok(c)
    }
}
