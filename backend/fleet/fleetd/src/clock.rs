//! 时钟抽象：生产用系统时钟，测试用可拨动的假时钟（告警状态机、摘要时刻、日历天数都只从这里取时间）。
use std::sync::Mutex;

pub trait Clock: Send + Sync {
    /// Unix 秒（带小数）。
    fn now(&self) -> f64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> f64 {
        let d = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        d.as_secs_f64()
    }
}

pub struct FakeClock(Mutex<f64>);

impl FakeClock {
    pub fn new(t: f64) -> Self {
        FakeClock(Mutex::new(t))
    }
    pub fn set(&self, t: f64) {
        *self.0.lock().unwrap() = t;
    }
    pub fn advance(&self, dt: f64) {
        *self.0.lock().unwrap() += dt;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> f64 {
        *self.0.lock().unwrap()
    }
}
