//! 已知地址抽查（公共 DNS、国内运营商 DNS、云厂商、VPN 网络）——用真实离线库。
//! 库目录来自环境变量 IPINTEL_DB_DIR（由 deploy/ipintel/fetch-dbs.sh 下载）；没设置或目录里没有库时整组跳过，
//! 不影响没装库的机器上的 `cargo test`。构造数据的单元测试在各模块的 `#[cfg(test)]` 里，不依赖真实库。
use fleet_ipintel::classify::ConnType;
use fleet_ipintel::{Dbs, Intel, Lists};
use std::net::IpAddr;
use std::path::PathBuf;

fn db_dir() -> Option<PathBuf> {
    let d = PathBuf::from(std::env::var("IPINTEL_DB_DIR").ok()?);
    d.join("ip2region_v4.xdb").exists().then_some(d)
}

macro_rules! need_dbs {
    () => {
        match db_dir() {
            Some(d) => d,
            None => {
                eprintln!("跳过：没有设置 IPINTEL_DB_DIR（离线库目录）");
                return;
            }
        }
    };
}

fn q(d: &Dbs, ip: &str) -> Intel {
    d.lookup(ip.parse::<IpAddr>().unwrap(), None)
}

#[test]
fn public_dns_and_clouds() {
    let dir = need_dbs!();
    let d = Dbs::open(&dir, Lists::builtin());
    assert_eq!(d.loaded(), 5, "errors: {:?}", d.errors);
    assert!(d.hosting_scan.len() > 500, "iptoasn 机房集合太小：{}", d.hosting_scan.len());

    let g = q(&d, "8.8.8.8");
    assert_eq!(g.asn, Some(15169));
    assert_eq!(g.country.as_deref(), Some("US"));
    assert_eq!(g.conn_type, ConnType::Datacenter);
    assert_eq!(g.sources.len(), 4);

    let cf = q(&d, "1.1.1.1");
    assert_eq!(cf.asn, Some(13335));
    assert_eq!(cf.conn_type, ConnType::Datacenter);

    let ali = q(&d, "223.5.5.5");
    assert_eq!(ali.asn, Some(37963));
    assert_eq!(ali.country.as_deref(), Some("CN"));
    assert_eq!(ali.region.as_deref(), Some("浙江省"));
    assert_eq!(ali.city.as_deref(), Some("杭州市"));
    assert_eq!(ali.conn_type, ConnType::Datacenter);
    assert_eq!(ali.geo_basis.as_deref(), Some("ip2region"));

    let tx = q(&d, "119.29.29.29");
    assert_eq!(tx.country.as_deref(), Some("CN"));
    assert_eq!(tx.conn_type, ConnType::Datacenter);

    let v6 = q(&d, "2001:4860:4860::8888");
    assert_eq!(v6.v, 6);
    assert_eq!(v6.asn, Some(15169));
    let ali6 = q(&d, "2400:3200::1");
    assert_eq!(ali6.country.as_deref(), Some("CN"));
    assert_eq!(ali6.asn, Some(37963));
    // IPv4-mapped IPv6 按 IPv4 查
    let m = q(&d, "::ffff:8.8.8.8");
    assert_eq!(m.ip, "8.8.8.8");
    assert_eq!(m.asn, Some(15169));
}

#[test]
fn cn_carriers() {
    let dir = need_dbs!();
    let d = Dbs::open(&dir, Lists::builtin());
    let zz = q(&d, "202.102.224.68"); // 河南联通 DNS
    assert_eq!(zz.isp, "中国联通");
    assert_eq!(zz.region.as_deref(), Some("河南省"));
    assert_eq!(zz.city.as_deref(), Some("郑州市"));
    assert_eq!(zz.asn, Some(4837));
    assert_eq!(zz.conn_type, ConnType::Residential);
    assert_eq!(zz.type_confidence, "low");
    assert_eq!(zz.type_alt, Some(ConnType::Mobile));
    assert_eq!(zz.coord_basis.as_deref(), Some("gazetteer"));

    let gz = q(&d, "202.96.128.86"); // 广东电信 DNS
    assert_eq!(gz.isp, "中国电信");
    assert_eq!(gz.region.as_deref(), Some("广东省"));
    assert_eq!(gz.asn, Some(4134));

    let bj = q(&d, "211.136.17.107");
    assert_eq!(bj.isp, "中国移动");
    assert_eq!(bj.asn, Some(9808));

}

#[test]
fn vpn_and_private() {
    let dir = need_dbs!();
    let d = Dbs::open(&dir, Lists::builtin());
    let m247 = q(&d, "45.87.212.1");
    assert_eq!(m247.asn, Some(9009));
    assert_eq!(m247.conn_type, ConnType::Vpn);
    let p = q(&d, "10.1.2.3");
    assert_eq!(p.conn_type, ConnType::Unknown);
    assert!(p.flags.contains(&"private".to_string()));
    assert_eq!(p.isp, "内网地址");
    let ula = q(&d, "fd00::1");
    assert!(ula.flags.contains(&"private".to_string()));
    let internal = d.lookup("8.8.4.4".parse().unwrap(), Some("r2"));
    assert_eq!(internal.internal.as_deref(), Some("r2"));
    assert!(internal.flags.contains(&"internal".to_string()));
}

#[test]
fn missing_dbs_degrade() {
    let tmp = tempfile::tempdir().unwrap();
    let d = Dbs::open(tmp.path(), Lists::builtin());
    assert_eq!(d.loaded(), 0);
    assert_eq!(d.errors.len(), 5);
    let x = d.lookup("8.8.8.8".parse().unwrap(), None);
    assert_eq!(x.asn, None);
    assert_eq!(x.conn_type, ConnType::Unknown);
    assert!(x.sources.is_empty());
}
