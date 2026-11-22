//! `fleet-server ipintel-fetch [目录]`：下载 / 更新 IP 情报离线库（fleet-ipintel-fetch.timer 每月一次，User=fleet；也可手动跑）。
//! 取代 deploy/ipintel/fetch-dbs.sh，来源、文件名、目录布局、MANIFEST 相同，但更严：
//!
//! | 库 | 来源 | 校验 |
//! |---|---|---|
//! | ip2region_v4.xdb / ip2region_v6.xdb | raw.githubusercontent.com/lionsoul2014/ip2region/master/data/ | GitHub contents API 公布的 size 与 git blob sha1，**拿不到就失败**（脚本退回只查大小） |
//! | dbip-city-lite-YYYY-MM.mmdb、dbip-asn-lite-YYYY-MM.mmdb | download.db-ip.com/free/…mmdb.gz | db-ip.com 下载页公布的 SHA1SUM（对解压后的 .mmdb），**解析不到、下载页已不是这个月的文件都失败**（脚本只警告）；gzip CRC |
//! | ip2asn-combined.tsv | iptoasn.com/data/ip2asn-combined.tsv.gz | **官方不公布校验和**：只能核对 Content-Length、gzip CRC、行数 ≥ 30 万、首行格式 |
//!
//! 每个库都先下到库目录里的 `.fetch.XXXXXX`，校验和对上后再用 fleet-ipintel 自己的加载器真正打开一遍（`Dbs::open`）、
//! 抽查一个已知地址查得到，才 rename 进库目录：fleet-server mmap 着这些库，**只换目录项、不改写或截断正在用的文件**；
//! fleet-server 每 5 分钟比对库文件的大小与 mtime（fleet_ipintel::service::signature），变了自动重新打开，不用重启或发信号。
//! 任何一个库失败都保留旧文件、退出码非 0，其余通过校验的照常更新。本机月份的 DB-IP 还没发布时（每月 1 日前后）退回上个月。
use super::{set_mode, WorkDir};
use anyhow::{bail, Context};
use fleet_ipintel::{Dbs, Lists};
use sha1::Digest as _;
use std::io::{Read, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const UA: &str = "fleet-ipintel-fetch/2 (+team-line-manager)";

/// 下载源（测试换成本机的假服务）
#[derive(Clone, Debug)]
pub struct Sources {
    pub github_api: String,
    pub github_raw: String,
    pub dbip_download: String,
    pub dbip_page: String,
    pub iptoasn: String,
    /// 只许 https（脚本的 `curl --proto '=https'`）；测试的假服务是 http
    pub https_only: bool,
    /// 暂时性错误（连不上、超时、408 / 429 / 5xx）重试几次、隔多久（脚本的 `--retry 3 --retry-delay 10`）
    pub retries: u32,
    pub retry_delay: Duration,
}

impl Default for Sources {
    fn default() -> Self {
        Sources {
            github_api: "https://api.github.com".into(),
            github_raw: "https://raw.githubusercontent.com".into(),
            dbip_download: "https://download.db-ip.com".into(),
            dbip_page: "https://db-ip.com".into(),
            iptoasn: "https://iptoasn.com".into(),
            https_only: true,
            retries: 3,
            retry_delay: Duration::from_secs(10),
        }
    }
}

/// 大小下限（同脚本；测试用小的假库时调小）
#[derive(Clone, Debug)]
pub struct Limits {
    pub min_dbip_city: u64,
    pub min_dbip_asn: u64,
    pub min_iptoasn_rows: usize,
    /// DB-IP 只留最新几个月（约 137 MB 一个月）
    pub keep_dbip: usize,
}

impl Default for Limits {
    fn default() -> Self { Limits { min_dbip_city: 60_000_000, min_dbip_asn: 4_000_000, min_iptoasn_rows: 300_000, keep_dbip: 1 } }
}

/// 这一轮的结果
#[derive(Debug, Default)]
pub struct Report {
    /// 换上了新文件的库
    pub updated: Vec<String>,
    /// 已是最新、没换的
    pub current: Vec<String>,
    /// 失败的（旧文件保留）
    pub failed: Vec<String>,
    /// 来源不公布校验和、只做了格式检查就换上的
    pub unverified: Vec<String>,
    /// 做完后库目录里能加载的库数（满分 fleet_ipintel::EXPECTED_DBS）
    pub loaded: usize,
}

fn log(s: impl AsRef<str>) { tracing::info!("{}", s.as_ref()); }

struct Http {
    c: reqwest::Client,
    retries: u32,
    delay: Duration,
}

fn transient(s: reqwest::StatusCode) -> bool { matches!(s.as_u16(), 408 | 429 | 500 | 502 | 503 | 504) }

impl Http {
    fn new(src: &Sources) -> anyhow::Result<Http> {
        let c = reqwest::Client::builder()
            .user_agent(UA)
            .connect_timeout(Duration::from_secs(20))
            .https_only(src.https_only)
            .min_tls_version(reqwest::tls::Version::TLS_1_2)
            // .gz 原样下载：不让客户端按 Content-Encoding 自动解压（Content-Length 要和收到的字节数对得上）
            .no_gzip()
            .build()
            .context("建 HTTP 客户端")?;
        Ok(Http { c, retries: src.retries, delay: src.retry_delay })
    }

    /// GET，暂时性错误重试；返回成功的响应。404 之类不重试，直接报错
    async fn get(&self, url: &str, accept: Option<&str>, timeout: Duration) -> anyhow::Result<reqwest::Response> {
        let mut last = None;
        for i in 0..=self.retries {
            if i > 0 {
                tokio::time::sleep(self.delay).await;
            }
            let mut rq = self.c.get(url).timeout(timeout);
            if let Some(a) = accept {
                rq = rq.header(reqwest::header::ACCEPT, a);
            }
            match rq.send().await {
                Ok(r) if r.status().is_success() => return Ok(r),
                Ok(r) if transient(r.status()) => last = Some(anyhow::anyhow!("{url}：HTTP {}", r.status())),
                Ok(r) => bail!("{url}：HTTP {}", r.status()),
                Err(e) => last = Some(anyhow::Error::new(e).context(url.to_string())),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("{url}：下载失败")))
    }

    async fn text(&self, url: &str, accept: Option<&str>) -> anyhow::Result<String> {
        let r = self.get(url, accept, Duration::from_secs(60)).await?;
        r.text().await.with_context(|| url.to_string())
    }

    /// 下载到文件（流式写盘，不进内存）；返回 (收到的字节数, 响应头里的 Content-Length)。传输中断整个重下
    async fn to_file(&self, url: &str, out: &Path) -> anyhow::Result<(u64, Option<u64>)> {
        let mut last = None;
        for i in 0..=self.retries {
            if i > 0 {
                tokio::time::sleep(self.delay).await;
            }
            let mut r = match self.get(url, None, Duration::from_secs(1800)).await {
                Ok(r) => r,
                Err(e) => return Err(e),
            };
            let want = r.content_length();
            let mut f = std::fs::File::create(out).with_context(|| format!("建 {}", out.display()))?;
            let mut n = 0u64;
            let res: anyhow::Result<()> = async {
                while let Some(chunk) = r.chunk().await? {
                    f.write_all(&chunk)?;
                    n += chunk.len() as u64;
                }
                f.sync_all()?;
                Ok(())
            }
            .await;
            match res {
                Ok(()) => return Ok((n, want)),
                Err(e) => last = Some(e.context(format!("{url}：传输中断（收到 {n} 字节）"))),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("{url}：下载失败")))
    }
}

fn hash_file<D: sha1::Digest>(mut h: D, p: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// git 的对象哈希：sha1("blob <大小>\0" + 内容)，GitHub contents API 公布的 sha 就是它
pub fn git_blob_sha1(p: &Path) -> std::io::Result<String> {
    let len = std::fs::metadata(p)?.len();
    let mut h = sha1::Sha1::new();
    h.update(format!("blob {len}\0").as_bytes());
    hash_file(h, p)
}

pub fn sha1_file(p: &Path) -> std::io::Result<String> { hash_file(sha1::Sha1::new(), p) }

/// gzip 解压（流式；gzip 尾部的 CRC32 与长度不对、截断都报错，同 `gzip -t`）
pub fn gunzip(src: &Path, dst: &Path) -> anyhow::Result<u64> {
    let mut d = flate2::read::MultiGzDecoder::new(std::io::BufReader::new(std::fs::File::open(src)?));
    let mut o = std::fs::File::create(dst)?;
    let n = std::io::copy(&mut d, &mut o).with_context(|| format!("{} 损坏（gzip 解不开或 CRC 不对）", src.display()))?;
    o.sync_all()?;
    Ok(n)
}

/// db-ip.com 下载页里「… Lite MMDB」按钮前最近的 SHA1SUM（与 fetch-dbs.sh 的 sed + awk 逐条相同：
/// 去掉标签、按空白切词；`SHA1SUM` 的下一个词记作哈希；词窗口里倒数第 1、2、4 个是 `MMDB`、`Lite`、`to` 时取当前哈希）
pub fn dbip_page_sha1(html: &str) -> Option<String> {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                text.push(' ');
            }
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    let mut it = text.split_whitespace();
    // w[0..4] = awk 的 p1..p4（p1 最新）
    let (mut w, mut sha) = ([""; 4], "");
    while let Some(t) = it.next() {
        if t == "SHA1SUM" {
            sha = it.next().unwrap_or("");
            continue;
        }
        w.rotate_right(1);
        w[0] = t;
        if w[0] == "MMDB" && w[1] == "Lite" && w[3] == "to" && sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Some(sha.to_string());
        }
    }
    None
}

/// iptoasn 首行：`起\t止\tAS 号\t…`（同脚本的 grep -E）
pub fn iptoasn_first_line_ok(line: &str) -> bool {
    let f: Vec<&str> = line.splitn(4, '\t').collect();
    let addr = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit() || c == '.' || c == ':');
    f.len() == 4 && addr(f[0]) && addr(f[1]) && !f[2].is_empty() && f[2].bytes().all(|b| b.is_ascii_digit())
}

/// (这个月, 上个月)，`YYYY-MM`（UTC，同脚本的 date -u）
pub fn months(today: time::Date) -> (String, String) {
    let m0 = format!("{:04}-{:02}", today.year(), today.month() as u8);
    let (y, m) = if today.month() == time::Month::January { (today.year() - 1, time::Month::December) } else { (today.year(), today.month().previous()) };
    (m0, format!("{y:04}-{:02}", m as u8))
}

/// 哪一种库、文件名（库目录里）
#[derive(Clone, Copy, Debug)]
enum Kind {
    X4,
    X6,
    City,
    Asn,
    I2a,
}

/// 用 fleet-ipintel 的加载器打开候选文件并抽查一个已知地址：放在只有它的目录里，按正式文件名
fn validate(kind: Kind, file: &Path, lim: &Limits) -> anyhow::Result<String> {
    let dir = file.parent().context("候选文件的目录")?;
    let dbs = Dbs::open(dir, Lists::builtin());
    let ip = |s: &str| s.parse::<IpAddr>().expect("地址");
    let errs = || dbs.errors.join("；");
    let r = match kind {
        Kind::X4 | Kind::X6 => {
            let (x, probe) = match kind {
                Kind::X4 => (dbs.x4.as_ref(), "223.5.5.5"),
                _ => (dbs.x6.as_ref(), "2400:3200::1"),
            };
            let x = x.with_context(|| format!("加载失败：{}", errs()))?;
            if x.v6 != matches!(kind, Kind::X6) || !matches!(x.structure, 2 | 3) {
                bail!("不是对应地址族的 xdb（structure={}，v6={}）", x.structure, x.v6);
            }
            let reg = dbs.lookup(ip(probe), None).dbs.ip2region.unwrap_or_default();
            if reg.raw.is_none() || (reg.country.is_none() && reg.country_name.is_none()) {
                bail!("抽查 {probe} 查不到");
            }
            format!("structure {}，抽查 {probe} → {}", x.structure, reg.raw.unwrap_or_default())
        }
        Kind::City => {
            let r = dbs.city.as_ref().with_context(|| format!("加载失败：{}", errs()))?;
            let c = dbs.lookup(ip("8.8.8.8"), None).dbs.dbip_city.unwrap_or_default();
            let cc = c.country.context("抽查 8.8.8.8 查不到国家")?;
            format!("build_epoch {}，抽查 8.8.8.8 → {cc}", r.metadata().build_epoch)
        }
        Kind::Asn => {
            let r = dbs.asn.as_ref().with_context(|| format!("加载失败：{}", errs()))?;
            let a = dbs.lookup(ip("8.8.8.8"), None).dbs.dbip_asn.unwrap_or_default();
            let asn = a.asn.filter(|a| *a > 0).context("抽查 8.8.8.8 查不到 ASN")?;
            format!("build_epoch {}，抽查 8.8.8.8 → AS{asn}", r.metadata().build_epoch)
        }
        Kind::I2a => {
            let r = dbs.i2a.as_ref().with_context(|| format!("加载失败：{}", errs()))?;
            if r.rows < lim.min_iptoasn_rows {
                bail!("行数太少（{}）", r.rows);
            }
            let a = r.lookup(ip("8.8.8.8")).filter(|a| a.asn > 0).context("抽查 8.8.8.8 查不到 ASN")?;
            format!("{} 行，抽查 8.8.8.8 → AS{}", r.rows, a.asn)
        }
    };
    Ok(r)
}

/// 候选文件挪进自己的检查目录（按正式名）、校验、再 rename 进库目录（0640）。rename 只换目录项：映射着旧文件的进程不受影响
fn check_and_install(kind: Kind, cand: &Path, name: &str, tmp: &Path, dest: &Path, lim: &Limits) -> anyhow::Result<()> {
    let cdir = tmp.join(format!("check-{name}"));
    super::mkdir_mode(&cdir, 0o700)?;
    let f = cdir.join(name);
    std::fs::rename(cand, &f)?;
    let what = validate(kind, &f, lim).with_context(|| format!("{name} 用 fleet-ipintel 打开校验不过"))?;
    log(format!("{name} 加载通过（{what}）"));
    set_mode(&f, 0o640)?;
    let bytes = std::fs::metadata(&f)?.len();
    std::fs::rename(&f, dest.join(name)).with_context(|| format!("换上 {name}"))?;
    let _ = std::fs::remove_dir(&cdir);
    log(format!("已更新 {name}（{bytes} 字节）"));
    Ok(())
}

/// GitHub contents API 列出的 (sha, size)
fn github_meta(listing: &serde_json::Value, name: &str) -> Option<(String, u64)> {
    let e = listing.as_array()?.iter().find(|e| e.get("name").and_then(|n| n.as_str()) == Some(name))?;
    let sha = e.get("sha")?.as_str()?.to_ascii_lowercase();
    let size = e.get("size")?.as_u64()?;
    (sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit())).then_some((sha, size))
}

async fn ip2region(http: &Http, src: &Sources, tmp: &Path, dest: &Path, lim: &Limits, rep: &mut Report) {
    let api = format!("{}/repos/lionsoul2014/ip2region/contents/data?ref=master", src.github_api);
    let listing = match http.text(&api, Some("application/vnd.github+json")).await.and_then(|t| serde_json::from_str::<serde_json::Value>(&t).context("contents API 回的不是 JSON")) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("失败：拿不到 GitHub 公布的哈希（{e:#}；API 限流？），ip2region 两个库都不更新（不接受没核对过的文件）");
            None
        }
    };
    for (kind, name) in [(Kind::X4, "ip2region_v4.xdb"), (Kind::X6, "ip2region_v6.xdb")] {
        let Some(listing) = &listing else {
            rep.failed.push(format!("{name}：拿不到 GitHub 公布的哈希"));
            continue;
        };
        let r: anyhow::Result<bool> = async {
            let (want_sha, want_size) = github_meta(listing, name).with_context(|| format!("contents API 里没有 {name} 的 sha / size"))?;
            let live = dest.join(name);
            if live.is_file() && git_blob_sha1(&live)? == want_sha {
                log(format!("{name} 已是最新（git blob sha1 {want_sha}）"));
                return Ok(false);
            }
            let out = tmp.join(name);
            http.to_file(&format!("{}/lionsoul2014/ip2region/master/data/{name}", src.github_raw), &out).await.context("下载失败")?;
            let (got_size, got_sha) = (std::fs::metadata(&out)?.len(), git_blob_sha1(&out)?);
            if got_size != want_size || got_sha != want_sha {
                bail!("校验不符：size {got_size}/{want_size} sha {got_sha}/{want_sha}（下载期间仓库更新了？下次再试）");
            }
            log(format!("{name} 校验通过（git blob sha1 {got_sha}）"));
            check_and_install(kind, &out, name, tmp, dest, lim)?;
            Ok(true)
        }
        .await;
        match r {
            Ok(true) => rep.updated.push(name.into()),
            Ok(false) => rep.current.push(name.into()),
            Err(e) => {
                tracing::warn!("失败：{name}：{e:#}");
                rep.failed.push(format!("{name}：{e:#}"));
            }
        }
    }
}

async fn dbip(http: &Http, src: &Sources, tmp: &Path, dest: &Path, lim: &Limits, today: time::Date, rep: &mut Report) {
    let (m0, m1) = months(today);
    for (kind, k) in [(Kind::City, "city"), (Kind::Asn, "asn")] {
        let page_url = format!("{}/db/download/ip-to-{k}-lite", src.dbip_page);
        let mut done = false;
        let mut errors = vec![];
        for m in [&m0, &m1] {
            let base = format!("dbip-{k}-lite-{m}");
            let name = format!("{base}.mmdb");
            if dest.join(&name).is_file() {
                log(format!("{name} 已是最新"));
                rep.current.push(name);
                done = true;
                break;
            }
            let gz = tmp.join(format!("{name}.gz"));
            if let Err(e) = http.to_file(&format!("{}/free/{name}.gz", src.dbip_download), &gz).await {
                log(format!("{base} 还没发布或下载失败（{e:#}），试上个月"));
                let _ = std::fs::remove_file(&gz);
                continue;
            }
            let r: anyhow::Result<()> = async {
                let out = tmp.join(&name);
                gunzip(&gz, &out)?;
                std::fs::remove_file(&gz)?;
                // 下载页：每个下载按钮前面是它的 SHA1SUM（对解压后的 .mmdb）；拿不到、对不上、已不是这个月的都不接受
                let html = http.text(&page_url, None).await.context("下载页取不到，核对不了 SHA1")?;
                if !html.contains(&format!("{name}.gz")) {
                    bail!("下载页公布的已不是 {m} 的文件，核对不了 SHA1（下次再试）");
                }
                let want = dbip_page_sha1(&html).context("下载页没解析到 SHA1（页面改版了？），不接受没核对过的文件")?;
                let got = sha1_file(&out)?;
                if got != want {
                    bail!("SHA1 不符：{got} ≠ {want}");
                }
                log(format!("{name} SHA1 校验通过（{got}，与 db-ip.com 下载页一致）"));
                let min = if matches!(kind, Kind::City) { lim.min_dbip_city } else { lim.min_dbip_asn };
                let sz = std::fs::metadata(&out)?.len();
                if sz < min {
                    bail!("太小（{sz} 字节）");
                }
                check_and_install(kind, &out, &name, tmp, dest, lim)
            }
            .await;
            match r {
                Ok(()) => {
                    rep.updated.push(name.clone());
                    // 只保留最近 keep_dbip 个月（被删的那个 fleet-server 可能还映射着：unlink 不影响已映射的 inode）
                    let mut olds: Vec<String> = std::fs::read_dir(dest)
                        .map(|rd| rd.filter_map(|e| e.ok()?.file_name().into_string().ok()).filter(|n| n.starts_with(&format!("dbip-{k}-lite-")) && n.ends_with(".mmdb")).collect())
                        .unwrap_or_default();
                    olds.sort_unstable_by(|a, b| b.cmp(a));
                    for old in olds.into_iter().skip(lim.keep_dbip.max(1)) {
                        if std::fs::remove_file(dest.join(&old)).is_ok() {
                            log(format!("删除旧库 {old}"));
                        }
                    }
                    done = true;
                    break;
                }
                Err(e) => {
                    tracing::warn!("失败：{name}：{e:#}");
                    errors.push(format!("{name}：{e:#}"));
                }
            }
        }
        if !done {
            if errors.is_empty() {
                errors.push(format!("DB-IP {k}：{m0} 与 {m1} 都下载不到"));
            }
            tracing::warn!("失败：DB-IP {k} 没有可用的版本");
            rep.failed.extend(errors);
        } else {
            // 这个月的失败了、退回上个月的（已在库目录里）：仍算失败，下次再试
            rep.failed.extend(errors);
        }
    }
}

async fn iptoasn(http: &Http, src: &Sources, tmp: &Path, dest: &Path, lim: &Limits, rep: &mut Report) {
    let name = "ip2asn-combined.tsv";
    let r: anyhow::Result<()> = async {
        let gz = tmp.join(format!("{name}.gz"));
        let (got, want) = http.to_file(&format!("{}/data/{name}.gz", src.iptoasn), &gz).await.context("下载失败")?;
        match want {
            Some(w) if w != got => bail!("{name}.gz 长度不符：{got} ≠ {w}"),
            Some(_) => {}
            None => bail!("{name}.gz 的响应没有 Content-Length：官方不公布校验和，连长度都核对不了，不接受"),
        }
        let out = tmp.join(name);
        gunzip(&gz, &out)?;
        std::fs::remove_file(&gz)?;
        let mut first = vec![];
        std::io::BufRead::read_until(&mut std::io::BufReader::new(std::fs::File::open(&out)?.take(4096)), b'\n', &mut first)?;
        if !iptoasn_first_line_ok(String::from_utf8_lossy(&first).trim_end_matches(['\n', '\r'])) {
            bail!("{name} 首行格式不对");
        }
        check_and_install(Kind::I2a, &out, name, tmp, dest, lim)
    }
    .await;
    match r {
        Ok(()) => {
            log(format!("注意：iptoasn.com 不公布校验和，{name} 只核对了 Content-Length、gzip CRC、行数与首行格式"));
            rep.unverified.push(name.into());
            rep.updated.push(name.into());
        }
        Err(e) => {
            tracing::warn!("失败：{name}：{e:#}");
            rep.failed.push(format!("{name}：{e:#}"));
        }
    }
}

/// 库目录的清单（每个库一行「sha256  字节数  文件名」），先写临时文件再 rename
fn write_manifest(dest: &Path, tmp: &Path, now: time::OffsetDateTime) -> anyhow::Result<()> {
    let ts = now.format(&time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z"))?;
    let mut s = format!("# fleet-server ipintel-fetch {ts}\n");
    let mut dbip: Vec<String> = std::fs::read_dir(dest)?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with("dbip-") && n.contains("-lite-") && n.ends_with(".mmdb"))
        .collect();
    dbip.sort();
    let names: Vec<String> = ["ip2region_v4.xdb".to_string(), "ip2region_v6.xdb".to_string()].into_iter().chain(dbip).chain(["ip2asn-combined.tsv".to_string()]).collect();
    for n in names {
        let p = dest.join(&n);
        if p.is_file() {
            s.push_str(&format!("{}  {}  {n}\n", super::sha256_file(&p)?, std::fs::metadata(&p)?.len()));
        }
    }
    let t = tmp.join("MANIFEST");
    std::fs::write(&t, s)?;
    set_mode(&t, 0o640)?;
    std::fs::rename(&t, dest.join("MANIFEST"))?;
    Ok(())
}

/// 更新一轮。只有建不了目录这类问题返回 Err；各个库的失败记在 `Report::failed`
pub async fn run(dest: &Path, src: &Sources, lim: &Limits, now: time::OffsetDateTime) -> anyhow::Result<Report> {
    if !dest.is_dir() {
        std::fs::create_dir_all(dest).with_context(|| format!("建库目录 {}", dest.display()))?;
        set_mode(dest, 0o750)?;
    }
    let work = WorkDir::new(dest, ".fetch.")?;
    let tmp: PathBuf = work.path().to_path_buf();
    let http = Http::new(src)?;
    let mut rep = Report::default();
    ip2region(&http, src, &tmp, dest, lim, &mut rep).await;
    dbip(&http, src, &tmp, dest, lim, now.date(), &mut rep).await;
    iptoasn(&http, src, &tmp, dest, lim, &mut rep).await;
    if let Err(e) = write_manifest(dest, &tmp, now) {
        tracing::warn!("写 MANIFEST 失败：{e:#}");
        rep.failed.push(format!("MANIFEST：{e:#}"));
    }
    drop(work);
    let dbs = Dbs::open(dest, Lists::builtin());
    rep.loaded = dbs.loaded();
    let total: u64 = std::fs::read_dir(dest).map(|rd| rd.filter_map(|e| e.ok()?.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()).unwrap_or(0);
    log(format!("库目录 {}：{}，能加载 {}/{} 个库", dest.display(), super::human_size(total), rep.loaded, fleet_ipintel::EXPECTED_DBS));
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::{StatusCode, Uri};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[test]
    fn page_sha1_like_the_script() {
        let html = r#"<html><body>
            <div class="card"><dl><dt>MD5SUM</dt><dd>0123456789abcdef0123456789abcdef</dd>
            <dt>SHA1SUM</dt><dd>1111111111111111111111111111111111111111</dd></dl>
            <a href="https://download.db-ip.com/free/dbip-city-lite-2026-11.csv.gz">Download IP to City Lite CSV</a>
            <dl><dt>SHA1SUM</dt><dd>aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa</dd></dl>
            <a href="https://download.db-ip.com/free/dbip-city-lite-2026-11.mmdb.gz">Download IP to City Lite MMDB</a>
            </body></html>"#;
        assert_eq!(dbip_page_sha1(html).as_deref(), Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        // 没有 MMDB 按钮、哈希不是 40 位小写十六进制：解析不到
        assert_eq!(dbip_page_sha1("<dt>SHA1SUM</dt><dd>abc</dd> IP to City Lite MMDB"), None);
        assert_eq!(dbip_page_sha1("<dt>SHA1SUM</dt><dd>aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa</dd> IP to City Lite CSV"), None);
        assert_eq!(dbip_page_sha1(""), None);
    }

    #[test]
    fn month_rollover() {
        assert_eq!(months(time::macros::date!(2026 - 11 - 03)), ("2026-11".into(), "2026-10".into()));
        assert_eq!(months(time::macros::date!(2027 - 01 - 01)), ("2027-01".into(), "2026-12".into()));
    }

    #[test]
    fn first_line_format() {
        assert!(iptoasn_first_line_ok("1.0.0.0\t1.0.0.255\t13335\tUS\tCLOUDFLARENET"));
        assert!(iptoasn_first_line_ok("2001:200::\t2001:200:ffff::\t2500\tJP\tWIDE"));
        assert!(!iptoasn_first_line_ok("<html>"));
        assert!(!iptoasn_first_line_ok("1.0.0.0\t1.0.0.255\tAS13335\tUS\tX"));
    }

    #[test]
    fn git_blob_hash() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("f");
        std::fs::write(&p, b"hello\n").unwrap();
        // git hash-object 对 "hello\n" 的结果
        assert_eq!(git_blob_sha1(&p).unwrap(), "ce013625030ba8dba906f756967f9e9ca394464a");
    }

    // ── 假库与假下载源 ──

    /// 最小的 MaxMind DB（IPv4 树，record_size 24）：只有 8.8.8.0/24 一条，数据是给定的 map
    fn mmdb(db_type: &str, data: Vec<u8>) -> Vec<u8> {
        let target: u32 = u32::from(std::net::Ipv4Addr::new(8, 8, 8, 0));
        let depth = 24u32;
        let node_count = depth;
        let mut tree = vec![];
        for i in 0..depth {
            let bit = (target >> (31 - i)) & 1;
            let next = if i + 1 == depth { node_count + 16 } else { i + 1 };
            let (l, r) = if bit == 0 { (next, node_count) } else { (node_count, next) };
            tree.extend_from_slice(&l.to_be_bytes()[1..]);
            tree.extend_from_slice(&r.to_be_bytes()[1..]);
        }
        let mut out = tree;
        out.extend_from_slice(&[0u8; 16]);
        out.extend_from_slice(&data);
        out.extend_from_slice(b"\xab\xcd\xefMaxMind.com");
        out.extend(map(vec![
            (s("binary_format_major_version"), u(5, 2)),
            (s("binary_format_minor_version"), u(5, 0)),
            (s("build_epoch"), u(9, 1_790_000_000)),
            (s("database_type"), s(db_type)),
            (s("description"), map(vec![(s("en"), s("test"))])),
            (s("ip_version"), u(5, 4)),
            (s("languages"), arr(vec![s("en")])),
            (s("node_count"), u(6, node_count as u64)),
            (s("record_size"), u(5, 24)),
        ]));
        out
    }
    /// 控制字节：类型 1–7 放在高 3 位，扩展类型放下一个字节；大小 < 29 直接放低 5 位，29–284 写 29 再跟一个字节
    fn ctrl(t: u8, size: usize) -> Vec<u8> {
        assert!(size < 285);
        let (low, extra) = if size < 29 { (size as u8, None) } else { (29u8, Some((size - 29) as u8)) };
        let mut v = if t <= 7 { vec![(t << 5) | low] } else { vec![low, t - 7] };
        v.extend(extra);
        v
    }
    fn s(x: &str) -> Vec<u8> {
        let mut v = ctrl(2, x.len());
        v.extend_from_slice(x.as_bytes());
        v
    }
    fn u(t: u8, n: u64) -> Vec<u8> {
        let b = n.to_be_bytes();
        let b: Vec<u8> = b.iter().copied().skip_while(|x| *x == 0).collect();
        let mut v = ctrl(t, b.len());
        v.extend(b);
        v
    }
    fn map(kv: Vec<(Vec<u8>, Vec<u8>)>) -> Vec<u8> {
        let mut v = ctrl(7, kv.len());
        for (k, x) in kv {
            v.extend(k);
            v.extend(x);
        }
        v
    }
    fn arr(xs: Vec<Vec<u8>>) -> Vec<u8> {
        let mut v = ctrl(11, xs.len());
        for x in xs {
            v.extend(x);
        }
        v
    }
    fn city_mmdb() -> Vec<u8> {
        mmdb("DBIP-City-Lite", map(vec![(s("country"), map(vec![(s("iso_code"), s("US")), (s("names"), map(vec![(s("en"), s("United States"))]))]))]))
    }
    fn asn_mmdb() -> Vec<u8> {
        mmdb("DBIP-ASN-Lite", map(vec![(s("autonomous_system_number"), u(6, 15169)), (s("autonomous_system_organization"), s("Google LLC"))]))
    }
    fn xdb(v6: bool) -> Vec<u8> {
        use fleet_ipintel::testutil::build_xdb;
        let ip = |x: &str| x.parse::<IpAddr>().unwrap();
        if v6 {
            build_xdb(true, &[(ip("2400:3200::"), ip("2400:3200:ffff:ffff:ffff:ffff:ffff:ffff"), "中国|浙江省|杭州市|阿里云|CN")])
        } else {
            build_xdb(false, &[(ip("223.5.0.0"), ip("223.5.255.255"), "中国|浙江省|杭州市|阿里云|CN")])
        }
    }
    fn gz(b: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(b).unwrap();
        e.finish().unwrap()
    }
    fn blob_sha(b: &[u8]) -> String {
        let mut h = sha1::Sha1::new();
        h.update(format!("blob {}\0", b.len()).as_bytes());
        h.update(b);
        hex::encode(h.finalize())
    }
    fn sha1_hex(b: &[u8]) -> String { hex::encode(sha1::Sha1::digest(b)) }

    type Files = Arc<Mutex<HashMap<String, (u16, Vec<u8>)>>>;

    async fn serve(files: Files) -> String {
        async fn h(State(f): State<Files>, uri: Uri) -> (StatusCode, Vec<u8>) {
            let f = crate::lock(&f);
            match f.get(uri.path()) {
                Some((st, b)) => (StatusCode::from_u16(*st).unwrap(), b.clone()),
                None => (StatusCode::NOT_FOUND, b"not found".to_vec()),
            }
        }
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, axum::Router::new().fallback(h).with_state(files)).await.unwrap() });
        format!("http://{addr}")
    }

    fn page(name: &str, sha: &str) -> Vec<u8> {
        format!(
            "<dl><dt>SHA1SUM</dt><dd>{sha}</dd></dl><a href=\"https://download.db-ip.com/free/{name}.gz\">Download IP to City Lite MMDB</a>"
        )
        .into_bytes()
    }

    fn tsv() -> Vec<u8> { fleet_ipintel::testutil::TSV.as_bytes().to_vec() }

    /// 一套全都对得上的假源（11 月的 DB-IP 已发布）
    fn good_files() -> HashMap<String, (u16, Vec<u8>)> {
        let (x4, x6, city, asn) = (xdb(false), xdb(true), city_mmdb(), asn_mmdb());
        let listing = serde_json::json!([
            {"name": "ip2region_v4.xdb", "sha": blob_sha(&x4), "size": x4.len()},
            {"name": "ip2region_v6.xdb", "sha": blob_sha(&x6), "size": x6.len()},
            {"name": "README.md", "sha": "0".repeat(40), "size": 1},
        ]);
        let mut m = HashMap::new();
        m.insert("/repos/lionsoul2014/ip2region/contents/data".into(), (200, listing.to_string().into_bytes()));
        m.insert("/lionsoul2014/ip2region/master/data/ip2region_v4.xdb".into(), (200, x4));
        m.insert("/lionsoul2014/ip2region/master/data/ip2region_v6.xdb".into(), (200, x6));
        m.insert("/free/dbip-city-lite-2026-11.mmdb.gz".into(), (200, gz(&city)));
        m.insert("/free/dbip-asn-lite-2026-11.mmdb.gz".into(), (200, gz(&asn)));
        m.insert("/db/download/ip-to-city-lite".into(), (200, page("dbip-city-lite-2026-11.mmdb", &sha1_hex(&city))));
        m.insert("/db/download/ip-to-asn-lite".into(), (200, page("dbip-asn-lite-2026-11.mmdb", &sha1_hex(&asn))));
        m.insert("/data/ip2asn-combined.tsv.gz".into(), (200, gz(&tsv())));
        m
    }

    fn sources(base: &str) -> Sources {
        Sources {
            github_api: base.into(),
            github_raw: base.into(),
            dbip_download: base.into(),
            dbip_page: base.into(),
            iptoasn: base.into(),
            https_only: false,
            retries: 1,
            retry_delay: Duration::from_millis(10),
        }
    }
    fn small() -> Limits { Limits { min_dbip_city: 10, min_dbip_asn: 10, min_iptoasn_rows: 3, keep_dbip: 1 } }
    const NOW: time::OffsetDateTime = time::macros::datetime!(2026-11-03 20:20:00 UTC);

    fn names(d: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn fresh_install_all_verified_and_loadable() {
        let files: Files = Arc::new(Mutex::new(good_files()));
        let base = serve(files.clone()).await;
        let t = tempfile::tempdir().unwrap();
        let dest = t.path().join("ipintel");
        let rep = run(&dest, &sources(&base), &small(), NOW).await.unwrap();
        assert!(rep.failed.is_empty(), "{:?}", rep.failed);
        assert_eq!(rep.updated, ["ip2region_v4.xdb", "ip2region_v6.xdb", "dbip-city-lite-2026-11.mmdb", "dbip-asn-lite-2026-11.mmdb", "ip2asn-combined.tsv"]);
        assert_eq!(rep.unverified, ["ip2asn-combined.tsv"]);
        assert_eq!(rep.loaded, 5);
        assert_eq!(
            names(&dest),
            ["MANIFEST", "dbip-asn-lite-2026-11.mmdb", "dbip-city-lite-2026-11.mmdb", "ip2asn-combined.tsv", "ip2region_v4.xdb", "ip2region_v6.xdb"],
            "临时目录不留"
        );
        let man = std::fs::read_to_string(dest.join("MANIFEST")).unwrap();
        assert!(man.starts_with("# fleet-server ipintel-fetch 2026-11-03T20:20:00Z\n"), "{man}");
        assert_eq!(man.lines().count(), 6);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dest.join("ip2region_v4.xdb")).unwrap().permissions().mode() & 0o777, 0o640);
        }
        // 再跑一次：ip2region 哈希没变、DB-IP 这个月的已在，都不换（mtime 不变，fleet-server 不重载）；iptoasn 没有哈希，照常换
        let m = std::fs::metadata(dest.join("ip2region_v4.xdb")).unwrap().modified().unwrap();
        let rep = run(&dest, &sources(&base), &small(), NOW).await.unwrap();
        assert!(rep.failed.is_empty(), "{:?}", rep.failed);
        assert_eq!(rep.current, ["ip2region_v4.xdb", "ip2region_v6.xdb", "dbip-city-lite-2026-11.mmdb", "dbip-asn-lite-2026-11.mmdb"]);
        assert_eq!(rep.updated, ["ip2asn-combined.tsv"]);
        assert_eq!(std::fs::metadata(dest.join("ip2region_v4.xdb")).unwrap().modified().unwrap(), m);
    }

    /// 换上新库时正在用的旧文件（fleet-server mmap 着）不被改写：旧 inode 的内容原样，目录项指向新文件
    #[cfg(unix)]
    #[tokio::test]
    async fn live_file_replaced_by_rename_not_in_place() {
        use std::os::unix::fs::MetadataExt;
        let mut f = good_files();
        let t = tempfile::tempdir().unwrap();
        let dest = t.path().join("ipintel");
        std::fs::create_dir_all(&dest).unwrap();
        let old = xdb(false)[..].to_vec();
        let mut old_marked = old.clone();
        old_marked[4] ^= 1; // 另一个 created_at：内容不同
        std::fs::write(dest.join("ip2region_v4.xdb"), &old_marked).unwrap();
        let held = std::fs::File::open(dest.join("ip2region_v4.xdb")).unwrap();
        let old_ino = held.metadata().unwrap().ino();
        f.remove("/free/dbip-city-lite-2026-11.mmdb.gz");
        let base = serve(Arc::new(Mutex::new(f))).await;
        let _ = run(&dest, &sources(&base), &small(), NOW).await.unwrap();
        let new_ino = std::fs::metadata(dest.join("ip2region_v4.xdb")).unwrap().ino();
        assert_ne!(old_ino, new_ino, "换的是目录项");
        let mut buf = vec![];
        let mut held = held;
        held.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, old_marked, "旧 inode 没被改写或截断");
    }

    #[tokio::test]
    async fn unverifiable_or_bad_files_keep_old_ones() {
        let t = tempfile::tempdir().unwrap();
        let dest = t.path().join("ipintel");
        std::fs::create_dir_all(&dest).unwrap();
        for n in ["ip2region_v4.xdb", "ip2region_v6.xdb", "ip2asn-combined.tsv", "dbip-city-lite-2026-10.mmdb"] {
            std::fs::write(dest.join(n), b"OLD").unwrap();
        }
        let mut f = good_files();
        // GitHub API 限流：两个 xdb 都不换（脚本会退回只查大小然后换上）
        f.insert("/repos/lionsoul2014/ip2region/contents/data".into(), (403, b"rate limited".to_vec()));
        // city：下载页解析不到 SHA1（脚本只警告然后换上）
        f.insert("/db/download/ip-to-city-lite".into(), (200, b"<html>redesigned <a href=\"/free/dbip-city-lite-2026-11.mmdb.gz\">MMDB</a></html>".to_vec()));
        // asn：SHA1 对不上
        f.insert("/db/download/ip-to-asn-lite".into(), (200, page("dbip-asn-lite-2026-11.mmdb", &"b".repeat(40))));
        // iptoasn：压缩包截断
        let mut g = gz(&tsv());
        g.truncate(g.len() - 6);
        f.insert("/data/ip2asn-combined.tsv.gz".into(), (200, g));
        let base = serve(Arc::new(Mutex::new(f))).await;
        let rep = run(&dest, &sources(&base), &small(), NOW).await.unwrap();
        assert!(rep.updated.is_empty(), "{:?}", rep.updated);
        let all = rep.failed.join("\n");
        for want in ["ip2region_v4.xdb：拿不到 GitHub 公布的哈希", "ip2region_v6.xdb：拿不到", "dbip-city-lite-2026-11.mmdb：下载页没解析到 SHA1", "dbip-asn-lite-2026-11.mmdb：SHA1 不符", "ip2asn-combined.tsv"] {
            assert!(all.contains(want), "缺「{want}」：\n{all}");
        }
        for n in ["ip2region_v4.xdb", "ip2region_v6.xdb", "ip2asn-combined.tsv", "dbip-city-lite-2026-10.mmdb"] {
            assert_eq!(std::fs::read(dest.join(n)).unwrap(), b"OLD", "{n} 保留旧文件");
        }
        assert!(!names(&dest).iter().any(|n| n.starts_with(".fetch.")));
    }

    /// 校验和对得上但文件打不开 / 抽查不到（比如上游传了个错的文件）：也不换
    #[tokio::test]
    async fn checksum_ok_but_not_loadable_is_rejected() {
        let t = tempfile::tempdir().unwrap();
        let dest = t.path().join("ipintel");
        let mut f = good_files();
        // v4 的位置放了 v6 的库（哈希按它自己算，对得上）
        let x6 = xdb(true);
        let listing = serde_json::json!([
            {"name": "ip2region_v4.xdb", "sha": blob_sha(&x6), "size": x6.len()},
            {"name": "ip2region_v6.xdb", "sha": blob_sha(&x6), "size": x6.len()},
        ]);
        f.insert("/repos/lionsoul2014/ip2region/contents/data".into(), (200, listing.to_string().into_bytes()));
        f.insert("/lionsoul2014/ip2region/master/data/ip2region_v4.xdb".into(), (200, x6));
        // city 的位置放了 asn 库：SHA1 按它算、对得上，但抽查查不到国家
        let asn = asn_mmdb();
        f.insert("/free/dbip-city-lite-2026-11.mmdb.gz".into(), (200, gz(&asn)));
        f.insert("/db/download/ip-to-city-lite".into(), (200, page("dbip-city-lite-2026-11.mmdb", &sha1_hex(&asn))));
        // iptoasn 不是 TSV
        f.insert("/data/ip2asn-combined.tsv.gz".into(), (200, gz(b"<html>maintenance</html>\n")));
        let base = serve(Arc::new(Mutex::new(f))).await;
        let rep = run(&dest, &sources(&base), &small(), NOW).await.unwrap();
        assert_eq!(rep.updated, ["ip2region_v6.xdb", "dbip-asn-lite-2026-11.mmdb"]);
        let all = rep.failed.join("\n");
        assert!(all.contains("ip2region_v4.xdb 用 fleet-ipintel 打开校验不过"), "{all}");
        assert!(all.contains("dbip-city-lite-2026-11.mmdb 用 fleet-ipintel 打开校验不过") && all.contains("查不到国家"), "{all}");
        assert!(all.contains("首行格式不对"), "{all}");
        assert!(!dest.join("ip2region_v4.xdb").exists() && !dest.join("ip2asn-combined.tsv").exists());
    }

    /// 本月的 DB-IP 还没发布（404）：退回上个月；换上后只留最新一个月
    #[tokio::test]
    async fn dbip_falls_back_to_last_month_and_prunes() {
        let t = tempfile::tempdir().unwrap();
        let dest = t.path().join("ipintel");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("dbip-asn-lite-2026-09.mmdb"), b"OLD").unwrap();
        let mut f = good_files();
        let asn = asn_mmdb();
        f.remove("/free/dbip-asn-lite-2026-11.mmdb.gz");
        f.insert("/free/dbip-asn-lite-2026-10.mmdb.gz".into(), (200, gz(&asn)));
        f.insert("/db/download/ip-to-asn-lite".into(), (200, page("dbip-asn-lite-2026-10.mmdb", &sha1_hex(&asn))));
        let base = serve(Arc::new(Mutex::new(f))).await;
        let rep = run(&dest, &sources(&base), &small(), NOW).await.unwrap();
        assert!(rep.failed.is_empty(), "{:?}", rep.failed);
        assert!(rep.updated.contains(&"dbip-asn-lite-2026-10.mmdb".to_string()));
        assert!(dest.join("dbip-asn-lite-2026-10.mmdb").exists() && !dest.join("dbip-asn-lite-2026-09.mmdb").exists());
    }

    /// 暂时性错误（503）重试后成功；Content-Length 由服务端给出、和收到的对得上
    #[tokio::test]
    async fn retries_transient_errors() {
        let files: Files = Arc::new(Mutex::new(good_files()));
        // 第一次 503，之后正常：用一个计数的路由包一层
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h2 = hits.clone();
        let f2 = files.clone();
        let app = axum::Router::new().fallback(move |uri: Uri| {
            let (hits, files) = (h2.clone(), f2.clone());
            async move {
                if uri.path() == "/data/ip2asn-combined.tsv.gz" && hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    return (StatusCode::SERVICE_UNAVAILABLE, b"busy".to_vec());
                }
                match crate::lock(&files).get(uri.path()) {
                    Some((st, b)) => (StatusCode::from_u16(*st).unwrap(), b.clone()),
                    None => (StatusCode::NOT_FOUND, vec![]),
                }
            }
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let t = tempfile::tempdir().unwrap();
        let rep = run(&t.path().join("d"), &sources(&base), &small(), NOW).await.unwrap();
        assert!(rep.failed.is_empty(), "{:?}", rep.failed);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn https_only_in_production() {
        let s = Sources::default();
        assert!(s.https_only);
        for u in [&s.github_api, &s.github_raw, &s.dbip_download, &s.dbip_page, &s.iptoasn] {
            assert!(u.starts_with("https://"), "{u}");
        }
    }
}
