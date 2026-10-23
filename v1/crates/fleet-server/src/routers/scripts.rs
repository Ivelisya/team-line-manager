//! 路由上执行的脚本（POSIX sh：OpenWrt 是 busybox ash，没有 bash）。参数经开头的 `K='v'` 赋值注入（单引号转义，见 `vars`），
//! 秘密（隧道私钥）只在 stdin / 接入脚本的正文里，不上命令行。
//!
//! 接入脚本只新增：/etc/fleet-tunnel/（隧道私钥、中控 sshd 的主机公钥）、/etc/init.d/fleet-tunnel、/etc/dropbear/authorized_keys 里
//! 中控的登录公钥（已有就不再加；文件不存在才新建 0600）。不动 root 密码、dropbear / LuCI 监听、OpenClash、防火墙、原有的 router-tunnel。
//! 两种接入共用：「在路由上执行命令」一次跑完整份（[`Bootstrap::full`]）；「一键部署」分两步跑 [`Bootstrap::keys`]、[`Bootstrap::service`]。

use crate::jobs::scripts::vars;
use uuid::Uuid;

/// 先查路由有没有要用的东西：OpenSSH 客户端（隧道要 -R / ExitOnForwardFailure / HostKeyAlias，dropbear 的 dbclient 不行）、dropbear 目录
pub const PRECHECK: &str = r#"
set -eu
if ! [ -x /usr/bin/ssh ] || ! /usr/bin/ssh -V 2>&1 | grep -q OpenSSH; then
  echo "fleet：路由上没有 OpenSSH 客户端（/usr/bin/ssh），先装上再接入：opkg update && opkg install openssh-client" >&2; exit 2
fi
[ -d /etc/dropbear ] || { echo "fleet：没有 /etc/dropbear：不是用 dropbear 的 OpenWrt？" >&2; exit 2; }
command -v ucode >/dev/null 2>&1 || echo "fleet：注意，路由上没有 ucode，中控采集不了状态（OpenWrt 23.05 以后都有）" >&2
"#;

/// 写隧道私钥（先写成 id_ed25519.new，启动隧道时才换上：这一步之后失败的话，跑着的旧隧道断线重连也还用旧密钥）、中控 sshd 的主机公钥，
/// 把中控登录公钥加进 dropbear 的 authorized_keys。需要 TUNNEL_KEY、CENTRAL_HOSTKEY、CENTRAL_PUBKEY
pub const KEYS: &str = r#"
set -eu
umask 077
mkdir -p /etc/fleet-tunnel
chmod 700 /etc/fleet-tunnel
printf '%s\n' "$TUNNEL_KEY" > /etc/fleet-tunnel/id_ed25519.tmp
mv -f /etc/fleet-tunnel/id_ed25519.tmp /etc/fleet-tunnel/id_ed25519.new
printf 'fleet-central %s\n' "$CENTRAL_HOSTKEY" > /etc/fleet-tunnel/known_hosts.tmp
mv -f /etc/fleet-tunnel/known_hosts.tmp /etc/fleet-tunnel/known_hosts
AK=/etc/dropbear/authorized_keys
[ -f "$AK" ] || : > "$AK"
KEYPART=$(printf '%s' "$CENTRAL_PUBKEY" | cut -d' ' -f1,2)
if ! grep -qF "$KEYPART" "$AK"; then
  if [ -s "$AK" ] && [ -n "$(tail -c 1 "$AK")" ]; then echo >> "$AK"; fi
  printf '%s\n' "$CENTRAL_PUBKEY" >> "$AK"
fi
echo fleet_keys=ok
"#;

/// 写 /etc/init.d/fleet-tunnel（procd，断了 10 秒后重连，一直重试），和新的隧道私钥一起换上（换上后输出 fleet_switched=1），再启动。
/// 需要 SSH_HOST、SSH_PORT、TUNNEL_PORT
pub const SERVICE: &str = r#"
set -eu
umask 022
cat > /etc/init.d/fleet-tunnel.tmp <<EOF
#!/bin/sh /etc/rc.common
# fleet：到中控的反向 SSH 隧道（中控接入时写入，与原有的 router-tunnel 并存）。中控经它登录本机、只读采集状态。
# 面板上删掉这台路由后可以停用并删掉：/etc/init.d/fleet-tunnel disable; /etc/init.d/fleet-tunnel stop; rm -rf /etc/init.d/fleet-tunnel /etc/fleet-tunnel
START=99
USE_PROCD=1

start_service() {
	procd_open_instance
	procd_set_param command /usr/bin/ssh -N -i /etc/fleet-tunnel/id_ed25519 -o BatchMode=yes -o IdentitiesOnly=yes \\
		-o UserKnownHostsFile=/etc/fleet-tunnel/known_hosts -o StrictHostKeyChecking=yes -o HostKeyAlias=fleet-central \\
		-o ExitOnForwardFailure=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3 -o ConnectTimeout=15 \\
		-p $SSH_PORT -R 127.0.0.1:$TUNNEL_PORT:127.0.0.1:22 fleet-tunnel@$SSH_HOST
	procd_set_param respawn 3600 10 0
	procd_set_param stderr 1
	procd_close_instance
}
EOF
chmod 755 /etc/init.d/fleet-tunnel.tmp
mv -f /etc/fleet-tunnel/id_ed25519.new /etc/fleet-tunnel/id_ed25519
mv -f /etc/init.d/fleet-tunnel.tmp /etc/init.d/fleet-tunnel
echo fleet_switched=1
/etc/init.d/fleet-tunnel enable
/etc/init.d/fleet-tunnel restart
echo fleet_service=ok
"#;

/// 一份接入要用的参数
pub struct Bootstrap<'a> {
    pub router_id: Uuid,
    pub tunnel_port: i32,
    pub ssh_host: &'a str,
    pub ssh_port: u16,
    /// 隧道私钥（OpenSSH 格式）
    pub tunnel_key: &'a str,
    /// 中控 sshd 的主机公钥「ssh-ed25519 base64」
    pub central_hostkey: &'a str,
    /// 中控登录公钥（一整行）
    pub central_pubkey: &'a str,
}

impl Bootstrap<'_> {
    /// 第一步：写密钥（一键部署的「上传密钥」）
    pub fn keys(&self) -> String {
        vars(&[("TUNNEL_KEY", self.tunnel_key), ("CENTRAL_HOSTKEY", self.central_hostkey), ("CENTRAL_PUBKEY", self.central_pubkey)]) + PRECHECK + KEYS
    }

    /// 第二步：装好 fleet-tunnel 并启动（一键部署的「启动隧道」）
    pub fn service(&self) -> String {
        vars(&[("SSH_HOST", self.ssh_host), ("SSH_PORT", &self.ssh_port.to_string()), ("TUNNEL_PORT", &self.tunnel_port.to_string())]) + SERVICE
    }

    /// 「在路由上执行命令」取走的整份接入脚本
    pub fn full(&self) -> String {
        format!(
            "#!/bin/sh\n# fleet 软路由接入脚本（一次性，中控生成；路由 {}，隧道端口 {}）。只新增：/etc/fleet-tunnel/、/etc/init.d/fleet-tunnel、\n\
             # /etc/dropbear/authorized_keys 里中控的登录公钥；不动 root 密码、dropbear / LuCI 监听、OpenClash、防火墙、原有的 router-tunnel\n\
             {}{}\necho \"fleet：已接入，隧道已启动。等面板显示在线（约半分钟）\"\n",
            self.router_id,
            self.tunnel_port,
            self.keys(),
            self.service()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b() -> Bootstrap<'static> {
        Bootstrap {
            router_id: Uuid::nil(),
            tunnel_port: 23001,
            ssh_host: "fleet.test",
            ssh_port: 2222,
            tunnel_key: "-----BEGIN OPENSSH PRIVATE KEY-----\nAAA'B\n-----END OPENSSH PRIVATE KEY-----",
            central_hostkey: "ssh-ed25519 HOSTKEY",
            central_pubkey: "ssh-ed25519 LOGINKEY fleet-central@test",
        }
    }

    #[test]
    fn parts_and_full_script() {
        let s = b();
        let keys = s.keys();
        // 单引号转义：私钥里出现 ' 也不会断开赋值
        assert!(keys.starts_with("TUNNEL_KEY='-----BEGIN OPENSSH PRIVATE KEY-----\nAAA'\\''B\n"), "{keys}");
        assert!(keys.contains("CENTRAL_HOSTKEY='ssh-ed25519 HOSTKEY'") && keys.contains("echo fleet_keys=ok"));
        let svc = s.service();
        assert!(svc.starts_with("SSH_HOST='fleet.test'\nSSH_PORT='2222'\nTUNNEL_PORT='23001'\n"), "{svc}");
        assert!(svc.contains("-p $SSH_PORT -R 127.0.0.1:$TUNNEL_PORT:127.0.0.1:22 fleet-tunnel@$SSH_HOST") && svc.contains("echo fleet_service=ok"));
        // 新私钥与 init 脚本一起换上，之后才重启
        let (sw, rs) = (svc.find("echo fleet_switched=1").unwrap(), svc.find("/etc/init.d/fleet-tunnel restart").unwrap());
        assert!(svc.find("mv -f /etc/fleet-tunnel/id_ed25519.new /etc/fleet-tunnel/id_ed25519").unwrap() < sw && sw < rs);
        let full = s.full();
        assert!(full.starts_with("#!/bin/sh\n") && full.contains(&keys) && full.contains(&svc) && full.trim_end().ends_with("（约半分钟）\""));
        // 不改路由现有的配置：没有改密码、改 dropbear / uci / 防火墙的命令
        for bad in ["passwd", "uci set", "uci commit", "/etc/config/", "fw4", "iptables", "nft "] {
            assert!(!full.contains(bad), "{bad}");
        }
    }

    /// 在本机用 sh 跑一遍（路径换到临时目录）：写出的文件、init 脚本里的启动命令都对，authorized_keys 只加一次、不动原有的行
    /// 只跑了写密钥这一步（启动隧道失败）：现在的私钥、init 脚本不动，跑着的旧隧道断线重连也还是旧的
    #[cfg(unix)]
    #[test]
    fn keys_step_alone_keeps_current_tunnel() {
        let dir = std::env::temp_dir().join(format!("fleet-bs-{}", Uuid::new_v4().simple()));
        let root = dir.display().to_string();
        std::fs::create_dir_all(dir.join("etc/dropbear")).unwrap();
        std::fs::create_dir_all(dir.join("etc/fleet-tunnel")).unwrap();
        std::fs::write(dir.join("etc/fleet-tunnel/id_ed25519"), "OLD KEY\n").unwrap();
        let out = std::process::Command::new("sh").arg("-c").arg(b().keys().replace("/etc/", &format!("{root}/etc/"))).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let read = |p: &str| std::fs::read_to_string(dir.join(p)).unwrap();
        assert_eq!(read("etc/fleet-tunnel/id_ed25519"), "OLD KEY\n");
        assert!(read("etc/fleet-tunnel/id_ed25519.new").contains("AAA'B"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn script_runs_and_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("fleet-bs-{}", Uuid::new_v4().simple()));
        let root = dir.display().to_string();
        std::fs::create_dir_all(dir.join("etc/dropbear")).unwrap();
        std::fs::create_dir_all(dir.join("etc/init.d")).unwrap();
        std::fs::write(dir.join("etc/dropbear/authorized_keys"), "ssh-rsa EXISTING user@pc").unwrap();
        // rc.common 换成记下调用的桩；/usr/bin/ssh 用本机的（要是 OpenSSH）
        std::fs::write(dir.join("etc/rc.common"), format!("echo \"$2\" >> {root}/rc.log\n")).unwrap();
        let s = b();
        let script = format!("{}\n{}", s.keys(), s.service()).replace("/etc/", &format!("{root}/etc/"));
        for _ in 0..2 {
            let out = std::process::Command::new("sh").arg("-c").arg(&script).output().unwrap();
            assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        }
        let read = |p: &str| std::fs::read_to_string(dir.join(p)).unwrap();
        assert_eq!(read("etc/fleet-tunnel/id_ed25519"), "-----BEGIN OPENSSH PRIVATE KEY-----\nAAA'B\n-----END OPENSSH PRIVATE KEY-----\n");
        assert!(!dir.join("etc/fleet-tunnel/id_ed25519.new").exists(), "启动隧道时换上了");
        assert_eq!(read("etc/fleet-tunnel/known_hosts"), "fleet-central ssh-ed25519 HOSTKEY\n");
        assert_eq!(read("etc/dropbear/authorized_keys"), "ssh-rsa EXISTING user@pc\nssh-ed25519 LOGINKEY fleet-central@test\n");
        let init = read("etc/init.d/fleet-tunnel");
        assert!(init.contains("-p 2222 -R 127.0.0.1:23001:127.0.0.1:22 fleet-tunnel@fleet.test"), "{init}");
        assert!(init.contains("-o StrictHostKeyChecking=yes -o HostKeyAlias=fleet-central \\\n"), "{init}");
        assert_eq!(read("rc.log"), "enable\nrestart\nenable\nrestart\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
