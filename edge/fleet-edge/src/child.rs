//! 短命子进程（`xray api …`、`xray run -test`、`xray version`）：带超时运行、边跑边读输出、**无论成败都回收**。
//!
//! 两个坑（2026-09-24 central/line1 每 10 min 多一个 `xray <defunct>` 的根因）：
//! 1. 只 `try_wait` 不读管道：管道缓冲 64 KiB，`xray api lsi` 在 central 输出 67 KiB，子进程卡在 write 上永远退不出 → 超时。
//! 2. 超时后 `Child::kill()`：relayd 的 seccomp 禁止 kill 系列系统调用，返回 EPERM，随后 `Child` 被丢弃、从不 wait →
//!    读端关闭后 xray 写 stdout 得到 EPIPE，Go 运行时想 raise(SIGPIPE) 也被 seccomp 拦下，只好 exit(2) → 僵尸。
//!
//! 做法：stdout/stderr 各由一个读线程持续读走（有上限，超出部分读了丢掉，管道不会满）；
//! 子进程由一个“持有线程” fork 并带 `PR_SET_PDEATHSIG(SIGKILL)`（与 pathcheck 常驻客户端同一招），
//! 超时 → 让持有线程退出 → 内核直接给子进程发 SIGKILL（不经 kill 系统调用）→ `wait()` 回收。
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// stdout 最多保留 16 MiB、stderr 最多 256 KiB；超出部分照样读走（不让管道满），只是不存
pub const MAX_STDOUT: usize = 16 << 20;
pub const MAX_STDERR: usize = 256 << 10;

#[derive(Debug)]
pub struct Captured {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug)]
pub enum RunError {
    /// fork/exec 失败（没有子进程需要回收）
    Spawn(std::io::Error),
    /// 超时：子进程已被 SIGKILL 并回收
    Timeout,
    /// 等待子进程时出错（极少见）；子进程已尽力回收
    Wait(std::io::Error),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Spawn(e) => write!(f, "启动失败：{e}"),
            RunError::Timeout => write!(f, "超时"),
            RunError::Wait(e) => write!(f, "等待子进程：{e}"),
        }
    }
}

fn reader<R: Read + Send + 'static>(src: Option<R>, cap: usize) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    let Some(mut src) = src else {
        let _ = tx.send(vec![]);
        return rx;
    };
    let spawned = std::thread::Builder::new().name("child-pipe".into()).stack_size(64 * 1024).spawn(move || {
        let mut keep = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match src.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let room = cap.saturating_sub(keep.len());
                    keep.extend_from_slice(&buf[..n.min(room)]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let _ = tx.send(keep);
    });
    if spawned.is_err() {
        // 起不了线程（TasksMax 满）：读端随闭包一起被丢弃，子进程写入会得到 EPIPE 并退出，仍由 run() 回收
        let (tx2, rx2) = mpsc::channel();
        let _ = tx2.send(vec![]);
        return rx2;
    }
    rx
}

/// 在子进程里（exec 之前）：创建它的线程退出时内核发 SIGKILL
fn set_pdeathsig() -> std::io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// 运行 `cmd`（stdin=/dev/null，stdout/stderr 采集），最多等 `timeout`。返回之前子进程一定已被回收。
pub fn run(mut cmd: Command, timeout: Duration) -> Result<Captured, RunError> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    unsafe {
        cmd.pre_exec(set_pdeathsig);
    }
    let (ctx, crx) = mpsc::channel::<std::io::Result<Child>>();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let holder = std::thread::Builder::new()
        .name("child-holder".into())
        .stack_size(64 * 1024)
        .spawn(move || match cmd.spawn() {
            Ok(c) => {
                // 主线程一直在 recv，send 不会失败；失败也只是 Child 被丢弃 → 本线程退出 → SIGKILL
                if ctx.send(Ok(c)).is_ok() {
                    let _ = stop_rx.recv(); // 等停止信号（或发送端被丢弃）
                }
            }
            Err(e) => {
                let _ = ctx.send(Err(e));
            }
        })
        .map_err(RunError::Spawn)?;
    let mut child = match crx.recv() {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            let _ = holder.join();
            return Err(RunError::Spawn(e));
        }
        Err(_) => {
            let _ = holder.join();
            return Err(RunError::Spawn(std::io::Error::other("持有线程异常退出")));
        }
    };
    let out_rx = reader::<ChildStdout>(child.stdout.take(), MAX_STDOUT);
    let err_rx = reader::<ChildStderr>(child.stderr.take(), MAX_STDERR);
    let t0 = Instant::now();
    let mut sleep = Duration::from_millis(5);
    let waited = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Ok(Some(s)),
            Ok(None) if t0.elapsed() < timeout => {
                std::thread::sleep(sleep.min(timeout.saturating_sub(t0.elapsed())).max(Duration::from_millis(1)));
                sleep = (sleep * 2).min(Duration::from_millis(50));
            }
            Ok(None) => break Ok(None),
            Err(e) => break Err(e),
        }
    };
    // 持有线程退出：子进程若还活着，内核此刻发 SIGKILL
    drop(stop_tx);
    let _ = holder.join();
    let status = match waited {
        Ok(Some(s)) => Ok(s),
        // 超时或 try_wait 出错：SIGKILL 已发出，阻塞 wait 回收（SIGKILL 不可忽略，很快返回）
        Ok(None) => {
            let _ = child.wait();
            Err(RunError::Timeout)
        }
        Err(e) => {
            let _ = child.wait();
            Err(RunError::Wait(e))
        }
    };
    // 子进程已退出，管道写端随之关闭；xray 不 fork，读线程马上 EOF。以防万一最多再等 2 s（等不到就放弃输出，不影响回收）
    let stdout = out_rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
    let stderr = err_rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
    Ok(Captured { status: status?, stdout, stderr })
}

/// 本进程当前的僵尸子进程（测试与自检用）：/proc/self/task/*/children 里状态为 Z 的 pid
pub fn zombie_children() -> Vec<u32> {
    let mut out = vec![];
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else { return out };
    for t in tasks.flatten() {
        let Ok(s) = std::fs::read_to_string(t.path().join("children")) else { continue };
        for pid in s.split_whitespace().filter_map(|p| p.parse::<u32>().ok()) {
            if std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|st| st.rsplit_once(')').is_some_and(|(_, r)| r.trim_start().starts_with('Z'))) {
                out.push(pid);
            }
        }
    }
    out
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    // 僵尸检查看的是整个测试进程的子进程：串行跑，免得看到别的测试里“刚退出、还没 try_wait 到”的瞬时僵尸
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn sh(script: &str) -> Command {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", script]);
        c
    }

    fn assert_reaped(pid_hint: &str) {
        // 同进程里其他测试并行跑也会有短暂的子进程，但它们都会被回收；这里只要求没有僵尸
        let z = zombie_children();
        assert!(z.is_empty(), "{pid_hint}: 还有僵尸子进程 {z:?}");
    }

    #[test]
    fn big_output_does_not_block() {
        let _g = serial();
        // 200 KiB 远超 64 KiB 管道缓冲：只 try_wait 不读会卡到超时
        let r = run(sh("head -c 204800 /dev/zero"), Duration::from_secs(10)).unwrap();
        assert!(r.status.success());
        assert_eq!(r.stdout.len(), 204800);
        assert_reaped("big_output");
    }

    #[test]
    fn stderr_and_exit_code() {
        let _g = serial();
        let r = run(sh("echo out; echo err >&2; exit 3"), Duration::from_secs(10)).unwrap();
        assert_eq!(r.status.code(), Some(3));
        assert_eq!(r.stdout, b"out\n");
        assert_eq!(r.stderr, b"err\n");
    }

    #[test]
    fn timeout_kills_and_reaps_without_kill_syscall() {
        let _g = serial();
        let t0 = Instant::now();
        let r = run(sh("exec sleep 30"), Duration::from_millis(300));
        assert!(matches!(r, Err(RunError::Timeout)), "{r:?}");
        assert!(t0.elapsed() < Duration::from_secs(5), "超时后应立刻被 SIGKILL：{:?}", t0.elapsed());
        assert_reaped("timeout");
    }

    #[test]
    fn timeout_with_large_output() {
        let _g = serial();
        // 大量输出 + 之后不退出：超时后照样 SIGKILL 并回收，读线程拿到的输出受上限约束
        let r = run(sh("head -c 1048576 /dev/zero; exec sleep 30"), Duration::from_millis(500));
        assert!(matches!(r, Err(RunError::Timeout)), "{r:?}");
        assert_reaped("large_output_timeout");
    }

    #[test]
    fn stdout_cap_keeps_draining() {
        let _g = serial();
        let r = run(sh(&format!("head -c {} /dev/zero", MAX_STDOUT + 100_000)), Duration::from_secs(20)).unwrap();
        assert!(r.status.success());
        assert_eq!(r.stdout.len(), MAX_STDOUT);
    }

    #[test]
    fn spawn_error() {
        let _g = serial();
        let r = run(Command::new("/nonexistent/xray"), Duration::from_secs(1));
        assert!(matches!(r, Err(RunError::Spawn(_))));
    }
}
