//! リソース制限: CPU 時間（watchdog + terminate_execution）とメモリ（heap limit）。
//!
//! # CPU 時間
//! プロセス共通の watchdog スレッドに (IsolateHandle, deadline, 世代) を送って
//! arm し、JS 実行スライスが終わったら disarm する。期限超過で
//! `terminate_execution()`。世代カウンタは「disarm が間に合わず次のリクエストを
//! 誤って terminate する」事故を防ぐ。
//!
//! CPU 計測は「JS を実行しているスライスの wall-clock 合計」で近似する。
//! イベントループが op 完了を待ってブロックしている時間（= await 中）は
//! スライス外なので予算を消費しない（Cloudflare Workers の
//! 「await は CPU 時間に入らない」挙動の再現）。
//!
//! # メモリ
//! `CreateParams::heap_limits` + `add_near_heap_limit_callback`。コールバックで
//! terminate を要求しつつ、heap limit を一時的に引き上げて返す（引き上げないと
//! 例外巻き戻し中の割り当てで V8 が process abort する）。メモリ超過した
//! Isolate は再利用せず破棄する。

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

enum WatchdogMsg {
    Arm {
        handle: v8::IsolateHandle,
        deadline: Instant,
        generation: u64,
    },
    Disarm {
        generation: u64,
    },
}

/// プロセスで 1 本だけ立てる watchdog スレッドへの送信口。
pub struct Watchdog {
    tx: mpsc::Sender<WatchdogMsg>,
    generation: AtomicU64,
}

impl Watchdog {
    pub fn global() -> &'static Watchdog {
        static WATCHDOG: OnceLock<Watchdog> = OnceLock::new();
        WATCHDOG.get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            std::thread::Builder::new()
                .name("watchdog".into())
                .spawn(move || watchdog_main(rx))
                .expect("failed to spawn watchdog thread");
            Watchdog {
                tx,
                generation: AtomicU64::new(0),
            }
        })
    }

    /// deadline までに disarm されなければ terminate する。世代を返す。
    pub fn arm(&self, handle: v8::IsolateHandle, deadline: Instant) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send(WatchdogMsg::Arm {
            handle,
            deadline,
            generation,
        });
        generation
    }

    pub fn disarm(&self, generation: u64) {
        let _ = self.tx.send(WatchdogMsg::Disarm { generation });
    }
}

fn watchdog_main(rx: mpsc::Receiver<WatchdogMsg>) {
    let mut armed: Option<(v8::IsolateHandle, Instant, u64)> = None;
    loop {
        let timeout = armed
            .as_ref()
            .map(|(_, deadline, _)| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_secs(3600));
        match rx.recv_timeout(timeout) {
            Ok(WatchdogMsg::Arm {
                handle,
                deadline,
                generation,
            }) => armed = Some((handle, deadline, generation)),
            Ok(WatchdogMsg::Disarm { generation }) => {
                if armed
                    .as_ref()
                    .is_some_and(|(_, _, armed_gen)| *armed_gen == generation)
                {
                    armed = None;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some((handle, deadline, _)) = armed.take() {
                    // recv_timeout は早起きすることがあるので期限を確認してから撃つ
                    if Instant::now() >= deadline {
                        handle.terminate_execution();
                    } else {
                        armed = Some((handle, deadline, 0));
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// リクエスト 1 件分の CPU 予算。JS 実行スライスを `run` で包むと
/// スライスの所要時間が予算から差し引かれ、残額で watchdog が arm される。
pub struct CpuMeter {
    handle: v8::IsolateHandle,
    budget: Option<Duration>,
    used: Duration,
}

impl CpuMeter {
    pub fn new(handle: v8::IsolateHandle, budget: Option<Duration>) -> Self {
        Self {
            handle,
            budget,
            used: Duration::ZERO,
        }
    }

    /// JS 実行スライスを watchdog の監視下で実行する。
    pub fn run<R>(&mut self, f: impl FnOnce() -> R) -> R {
        let Some(budget) = self.budget else {
            return f();
        };
        let remaining = budget.saturating_sub(self.used);
        let watchdog = Watchdog::global();
        let started = Instant::now();
        let generation = watchdog.arm(self.handle.clone(), started + remaining);
        let result = f();
        watchdog.disarm(generation);
        self.used += started.elapsed();
        result
    }
}

/// near-heap-limit コールバックに渡すコンテキスト。
/// Worker が Box で所有し、Isolate 破棄まで生かす。
pub struct MemCtx {
    pub handle: v8::IsolateHandle,
    pub hit: Arc<AtomicBool>,
}

/// heap limit 到達時に呼ばれる。terminate を要求し、巻き戻しの猶予として
/// limit を 8MiB 引き上げて返す。
pub unsafe extern "C" fn near_heap_limit_callback(
    data: *mut c_void,
    current_heap_limit: usize,
    _initial_heap_limit: usize,
) -> usize {
    let ctx = unsafe { &*(data as *const MemCtx) };
    ctx.hit.store(true, Ordering::SeqCst);
    ctx.handle.terminate_execution();
    current_heap_limit + 8 * 1024 * 1024
}
