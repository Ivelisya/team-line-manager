//! 网页终端用的伪终端（PTY）：中控本地开一对主从端，子进程（`ssh -tt`）以从端为控制终端跑在自己的会话里，中控经主端读写。
//!
//! 不引第三方 PTY 库：libc 已经是依赖，开 PTY、设控制终端、改窗口大小合起来几十行；读写直接挂在 tokio 的 AsyncFd 上
//! （非阻塞，调用方不读就不读，背压经内核的 PTY 缓冲一路传到 ssh 和远端）。portable-pty 自带阻塞的读写线程、
//! pty-process 的接口这几个版本一直在变，为这几十行多一个要过安全审查的依赖不值得。
//!
//! - 主从两端都带 O_CLOEXEC 打开：别的线程此刻在起子进程（远程执行、作业的 ssh）时不会把它们带过去——
//!   从端被别的进程拿着，ssh 退出后这边就一直读不到 EOF
//! - 子进程 setsid 自成会话与进程组（进程组号 = pid），再把已经接在 0/1/2 上的从端设为控制终端（TIOCSCTTY）。
//!   改窗口大小（TIOCSWINSZ）时内核给前台进程组发 SIGWINCH，ssh 收到后把新大小转给远端
//! - 结束时给整个进程组先发 SIGTERM，等不到再 SIGKILL（`kill_group`）

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::process::{Child, Command};

/// PTY 的主端
pub struct Pty {
    fd: AsyncFd<OwnedFd>,
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

fn set_size(fd: libc::c_int, cols: u16, rows: u16) -> io::Result<()> {
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: fd 是打开着的 PTY，ws 在调用期间有效
    cvt(unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws as *const libc::winsize) }).map(|_| ())
}

impl Pty {
    /// 开一对 PTY，窗口 `cols` 列 × `rows` 行。返回主端与从端（从端交给 [`spawn`]）
    pub fn open(cols: u16, rows: u16) -> io::Result<(Pty, OwnedFd)> {
        // SAFETY: 都是对刚打开的 fd 的系统调用；fd 一拿到就交给 OwnedFd，出错返回时自动关闭
        let (master, slave) = unsafe {
            let master = OwnedFd::from_raw_fd(cvt(libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC))?);
            let m = master.as_raw_fd();
            cvt(libc::grantpt(m))?;
            cvt(libc::unlockpt(m))?;
            let mut name = [0 as libc::c_char; 64];
            let r = libc::ptsname_r(m, name.as_mut_ptr(), name.len());
            if r != 0 {
                return Err(io::Error::from_raw_os_error(r));
            }
            let slave = OwnedFd::from_raw_fd(cvt(libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC))?);
            let fl = cvt(libc::fcntl(m, libc::F_GETFL))?;
            cvt(libc::fcntl(m, libc::F_SETFL, fl | libc::O_NONBLOCK))?;
            (master, slave)
        };
        set_size(master.as_raw_fd(), cols, rows)?;
        // SAFETY: OwnedFd 在 AsyncFd 存活期间一直开着、始终是这个主端
        let fd = unsafe { AsyncFd::register(master) }.map_err(|e| e.into_parts().1)?;
        Ok((Pty { fd }, slave))
    }

    /// 改窗口大小（子进程收到 SIGWINCH）
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> { set_size(self.fd.as_raw_fd(), cols, rows) }

    /// 读一段输出；0 = 从端已经全部关闭（子进程退出了：Linux 上这时读主端得到 EIO）
    pub async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut g = self.fd.readable().await?;
            let r = g.try_io(|fd| {
                // SAFETY: buf 是有效的可写缓冲区
                let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
            });
            match r {
                Ok(Ok(n)) => return Ok(n),
                Ok(Err(e)) if e.raw_os_error() == Some(libc::EIO) => return Ok(0),
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                Ok(Err(e)) => return Err(e),
                // 其实还不可读：try_io 已清掉就绪标记，接着等
                Err(_) => {}
            }
        }
    }

    /// 把输入全部写进去：子进程不读时就等着（不在这里攒）
    pub async fn write_all(&self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let mut g = self.fd.writable().await?;
            let r = g.try_io(|fd| {
                // SAFETY: data 是有效的只读缓冲区
                let n = unsafe { libc::write(fd.as_raw_fd(), data.as_ptr().cast(), data.len()) };
                if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
            });
            match r {
                Ok(Ok(n)) => data = &data[n..],
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                Ok(Err(e)) => return Err(e),
                Err(_) => {}
            }
        }
        Ok(())
    }
}

/// 在 PTY 里起子进程：从端接到标准输入、输出、错误；子进程 setsid 自成会话（进程组号 = pid），再把从端设为控制终端。
/// 这里拿着的从端随 `cmd` 一起关掉，之后只有子进程拿着它：子进程一退出，主端就读到 EOF
pub fn spawn(mut cmd: Command, slave: OwnedFd) -> io::Result<Child> {
    let out = slave.try_clone()?;
    let err = slave.try_clone()?;
    cmd.stdin(Stdio::from(slave)).stdout(Stdio::from(out)).stderr(Stdio::from(err)).kill_on_drop(true);
    // SAFETY: 闭包在 fork 之后、exec 之前运行，只调用异步信号安全的 setsid 与 ioctl；此时 0/1/2 已经是从端
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY, 0 as libc::c_int) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
}

/// 结束子进程所在的整个进程组：先 SIGTERM（ssh 收到后断开连接，远端的 shell 随之收到 SIGHUP），`grace` 内没退出再 SIGKILL。
/// 只在子进程还没被回收时发信号：回收之后 pid（即进程组号）可能已被系统复用
pub async fn kill_group(child: &mut Child, grace: Duration) -> io::Result<ExitStatus> {
    if let Some(s) = child.try_wait()? {
        return Ok(s);
    }
    if let Some(pid) = child.id() {
        let pgid = -(pid as libc::pid_t);
        // SAFETY: 只是发信号；子进程还没被回收，这个进程组仍是它的
        unsafe { libc::kill(pgid, libc::SIGTERM) };
        if let Ok(r) = tokio::time::timeout(grace, child.wait()).await {
            return r;
        }
        // SAFETY: 同上（wait 超时，说明还没退出、没被回收）
        unsafe { libc::kill(pgid, libc::SIGKILL) };
    }
    child.wait().await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一直读到输出里出现 `want`（最多 5 秒），返回读到的全部
    async fn read_until(p: &Pty, want: &str) -> String {
        let mut all = Vec::new();
        let mut buf = [0u8; 4096];
        let r = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let n = p.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                all.extend_from_slice(&buf[..n]);
                if String::from_utf8_lossy(&all).contains(want) {
                    break;
                }
            }
        })
        .await;
        let s = String::from_utf8_lossy(&all).into_owned();
        assert!(r.is_ok() && s.contains(want), "等不到 {want:?}，读到：{s:?}");
        s
    }

    fn sh() -> Command {
        let mut c = Command::new("sh");
        c.env_clear().env("PATH", "/usr/bin:/bin").env("PS1", "$ ");
        c
    }

    /// 每条命令都等上一条的输出到了再发，免得后一条的输出被前一次读走。等的标记都不会出现在 PTY 回显的命令里
    #[tokio::test]
    async fn shell_sees_size_resize_and_own_session() {
        let (p, slave) = Pty::open(100, 30).unwrap();
        let mut child = spawn(sh(), slave).unwrap();
        let pid = child.id().unwrap();
        p.write_all(b"stty size\n").await.unwrap();
        read_until(&p, "30 100").await;
        // 自成会话：会话号（/proc/<pid>/stat 第 6 列）就是自己的 pid
        p.write_all(b"echo SID$(cut -d' ' -f6 /proc/$$/stat)END\n").await.unwrap();
        read_until(&p, &format!("SID{pid}END")).await;
        // 改大小：子进程看到新的
        p.resize(120, 40).unwrap();
        p.write_all(b"stty size\n").await.unwrap();
        read_until(&p, "40 120").await;
        // 退出：主端读到 EOF，退出码照常拿得到
        p.write_all(b"exit 3\n").await.unwrap();
        let mut buf = [0u8; 4096];
        let eof = tokio::time::timeout(Duration::from_secs(5), async {
            while p.read(&mut buf).await.unwrap() > 0 {}
        })
        .await;
        assert!(eof.is_ok(), "子进程退出后读不到 EOF");
        assert_eq!(child.wait().await.unwrap().code(), Some(3));
    }

    /// 子进程是会话首进程、PTY 是它的控制终端：改窗口大小时它（前台进程组）收到 SIGWINCH——ssh 就靠这个把新大小转给远端
    #[tokio::test]
    async fn resize_sends_sigwinch_to_child() {
        let (p, slave) = Pty::open(80, 24).unwrap();
        let mut c = sh();
        // 28 = SIGWINCH（x86_64 / aarch64 Linux）
        c.args(["-c", "trap 'echo GOT-WINCH' 28; echo READY; while :; do sleep 0.1; done"]);
        let mut child = spawn(c, slave).unwrap();
        read_until(&p, "READY").await;
        p.resize(132, 50).unwrap();
        read_until(&p, "GOT-WINCH").await;
        kill_group(&mut child, Duration::from_secs(2)).await.unwrap();
    }

    #[tokio::test]
    async fn kill_group_ends_children_too() {
        let (p, slave) = Pty::open(80, 24).unwrap();
        let mut c = sh();
        // 后台的 sleep 与 sh 同一个进程组（非交互的 sh 不做作业控制）
        c.args(["-c", "sleep 300 & echo BG$!END; wait"]);
        let mut child = spawn(c, slave).unwrap();
        let out = read_until(&p, "END").await;
        let bg: u32 = out.split("BG").nth(1).and_then(|s| s.split("END").next()).unwrap().parse().unwrap();
        let st = kill_group(&mut child, Duration::from_secs(2)).await.unwrap();
        assert!(!st.success());
        // 后台的 sleep 也没了（或只剩等着被 init 回收的僵尸）
        let gone = |pid: u32| match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(s) => s.rsplit(')').next().is_some_and(|r| r.trim_start().starts_with('Z')),
        };
        for _ in 0..50 {
            if gone(bg) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("后台进程 {bg} 还在");
    }
}
