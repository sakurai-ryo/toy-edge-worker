# CPU 時間制限 — watchdog と terminate

M5 前半です。マルチテナントで最も恐ろしいコードはこれです。

```js
export default {
  async fetch() {
    while (true) {}     // examples/workers/loop.js
  },
};
```

第 10 章の設計により、これでプロセス全体は死にません（死ぬのは worker
スレッド 1 本の時間だけ）。しかしそのテナントのスレッドは永遠に占有され、
同じスレッドに割り当てられた他テナントも巻き添えになります。
**実行中の JS を外から殺す**手段が必要です。

## TerminateExecution — V8 の緊急停止ボタン

V8 にはこのための API が最初から用意されています。

- `isolate.thread_safe_handle()` → `v8::IsolateHandle` を得る。
  これは **Send + Sync** で、他スレッドに渡せる（Isolate 本体は渡せない
  ことと対照的）
- `handle.terminate_execution()` — **どのスレッドからでも呼べる**。
  実行中の JS は最も近い機会に中断される
- 中断は「catch 不能の例外」として現れる。JS の `try/catch` でも
  `finally` でも握り潰せない。Rust 側では `call`/`run` が `None` を返し、
  TryCatch の `has_terminated()` が true になる
- `cancel_terminate_execution()` でフラグを下ろせば、**Isolate は再利用可能**

「外部スレッドから安全に呼べる」のが肝です。JS を実行中のスレッドは
ブロックしているので、自分では自分を止められません。**止める係の
別スレッド = watchdog** が必要になります。

## watchdog スレッド

設計: プロセスに 1 本だけ watchdog スレッドを立て、worker スレッドが
JS 実行の前後に Arm（監視開始）/ Disarm（解除）メッセージを送ります。
期限までに Disarm が来なければ terminate します。

```rust
// crates/runtime/src/limits.rs（抜粋）
enum WatchdogMsg {
    Arm { handle: v8::IsolateHandle, deadline: Instant, generation: u64 },
    Disarm { generation: u64 },
}

fn watchdog_main(rx: mpsc::Receiver<WatchdogMsg>) {
    let mut armed: Option<(v8::IsolateHandle, Instant, u64)> = None;
    loop {
        let timeout = armed.as_ref()
            .map(|(_, deadline, _)| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_secs(3600));
        match rx.recv_timeout(timeout) {
            Ok(WatchdogMsg::Arm { handle, deadline, generation }) =>
                armed = Some((handle, deadline, generation)),
            Ok(WatchdogMsg::Disarm { generation }) => {
                if armed.as_ref().is_some_and(|(_, _, g)| *g == generation) {
                    armed = None;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some((handle, deadline, _)) = armed.take() {
                    if Instant::now() >= deadline {
                        handle.terminate_execution();
                    } else {
                        armed = Some((handle, deadline, 0)); // 早起きしただけ
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}
```

watchdog 自体も `recv_timeout` で眠る省エネ設計です。「Arm されていない
ときは 1 時間眠る」「Arm 中は期限まで眠る」。

### 世代カウンタ — 誤爆の防止

`generation` が無いとどうなるか。

```
worker: Arm(リクエスト1) ──▶ watchdog
worker: JS 実行（49ms で完了、ぎりぎりセーフ）
worker: Disarm 送信 ──── (チャネルで配達中) ────▶
watchdog: 50ms 経過、terminate 発射！   ← Disarm が間に合わなかった
worker: 次のリクエスト 2 の JS を開始
        → リクエスト 2 が無実の罪で terminate される
```

メッセージは非同期に配達されるので、「解除したつもり」と「発射」が
すれ違う競合が原理的にあります。Arm のたびに単調増加の世代番号を払い出し、
**Disarm は世代が一致したときだけ有効**にすることで、古い Disarm が
新しい Arm を消したり、その逆が起きたりしなくなります。
（terminate 自体のすれ違いは、後述の「terminate 検知とフラグ下ろし」で
吸収されます。）

分散システムで言う fencing token と同型の、小さくて美しいパターンです。

## 「CPU 時間」をどう測るか — await はタダ、の実装

Cloudflare Workers の CPU 制限は「**JS を実行している時間**」だけを
数えます。`await fetch(...)` でネットワークを待つ時間はいくら長くても
無料です。これを正しく実装しないと、50ms の CPU 予算では外部 API を
1 回呼んだだけでタイムアウトしてしまいます。

理想は OS のスレッド CPU 時計（`clock_gettime(CLOCK_THREAD_CPUTIME_ID)`）
ですが、「他スレッド（watchdog）から worker スレッドの CPU 時計を読む」
のは macOS では Mach API が必要で煩雑です。本プロジェクトは構造を利用した
近似を採りました。

> **観察**: 第 8 章のイベントループでは、JS が実行される区間が
> 明確に分かれている。(a) ハンドラの `call`、(b) microtask checkpoint、
> (c) bootstrap ヘルパの `call`。**それ以外の時間（`recv_timeout` で
> op を待っている時間 = await 中）は、JS は 1 ミリも動いていない。**

つまり「JS 実行スライスの wall-clock の合計」が CPU 時間の良い近似に
なります。これを `CpuMeter` として実装します。

```rust
pub struct CpuMeter {
    handle: v8::IsolateHandle,
    budget: Option<Duration>,    // 例: 50ms
    used: Duration,              // 消費済み
}

impl CpuMeter {
    /// JS 実行スライスを watchdog の監視下で実行する
    pub fn run<R>(&mut self, f: impl FnOnce() -> R) -> R {
        let Some(budget) = self.budget else { return f(); };
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
```

使う側（第 8・9 章で既に登場していた `meter.run(...)`）:

```rust
// ハンドラ呼び出し
let Some(ret) = meter.run(|| handler.call(tc, recv, &args)) else { ... };

// イベントループの microtask checkpoint
meter.run(|| scope.perform_microtask_checkpoint());
```

動きを図にするとこうなります。

```
CPU 予算 50ms のリクエストで、10ms 計算 → 200ms の fetch を await → 10ms 計算

スライス1: arm(残50ms) ── JS 10ms ── disarm   used=10ms
await 中 : （armされていない。watchdog は眠っている。200ms 経っても無罪）
スライス2: arm(残40ms) ── JS 10ms ── disarm   used=20ms
→ 完走。wall-clock は 220ms でも CPU 会計は 20ms
```

`loop.js` の場合はスライス 1 が终わらないので、50ms 後に watchdog が
terminate を撃ちます。

> **近似の限界**: スライス内の wall-clock は「OS にプリエンプトされた
> 時間」も含むため、マシンが過負荷だと実 CPU 時間より多く数えます。
> 厳密化（スレッド CPU 時計のサンプリング）は第 16 章の発展課題です。

## terminate されたあとの後始末

terminate は 3 箇所で検知されます。

1. `handler.call` が `None` + `tc.has_terminated()` → `WorkerError::Terminated`
2. イベントループ先頭の `scope.is_execution_terminating()` → 同上
3. （第 14 章）メモリ起因も同じ経路に合流する

検知後の後始末は `Worker::handle` のラッパで一元化します。

```rust
pub fn handle(&mut self, req: &WorkerRequest) -> Result<WorkerResponse, WorkerError> {
    unsafe { self.isolate.enter() };
    let mut result = self.handle_inner(req);
    if matches!(result, Err(WorkerError::Terminated)) {
        result = Err(self.classify_termination());
    }
    unsafe { self.isolate.exit() };
    result
}

fn classify_termination(&self) -> WorkerError {
    // terminate フラグを下ろす（下ろせば Isolate は再利用できる）
    self.isolate.cancel_terminate_execution();
    // 未完了の op は破棄（遅れて届く完了通知は op_id 不一致で無視される）
    ops::get_op_state(&self.isolate).borrow_mut().abort_all();
    if self.mem_hit.load(Ordering::SeqCst) {
        WorkerError::MemExceeded      // 第 14 章
    } else {
        WorkerError::CpuExceeded
    }
}
```

ポイントは **CPU 超過の Isolate は再利用してよい**ことです。
`cancel_terminate_execution()` でフラグを下ろせば、次のリクエストは
普通に処理できます。グローバル変数が中途半端な状態かもしれませんが、
それは「warm Isolate には前のリクエストの状態が残る」という Workers の
公認セマンティクス（第 2 章）の範囲内です。

サーバ側はエラー種別をメトリクスに刻み、HTTP 503 を返します。

## 動かす

```sh
$ time curl -s -w "%{http_code}" http://127.0.0.1:8787/loop
worker exceeded CPU time limit
503    real 0m0.100s        # cpu_ms=50 + 諸経費で ~100ms で殺せた

$ curl -s -w "%{http_code}" http://127.0.0.1:8787/loop   # 2 回目
503                          # warm のまま再利用されている（metrics で確認可）

$ curl -s http://127.0.0.1:8787/__admin/metrics | jq .loop
{ "requests": 2, "cold_starts": 1, "warm_hits": 1, "terminations_cpu": 2, ... }
```

`warm_hits: 1` が「terminate 後も Isolate が再利用された」証拠です。
一方、第 15 章の proxy worker は cpu_ms=50 のまま 200ms 超の外部 fetch を
完走します — await が課金されていない証拠です。

## この章のまとめ

- 実行中の JS は `IsolateHandle::terminate_execution()` で外から殺せる。
  catch 不能、`cancel` すれば Isolate は再利用可
- 殺す係は watchdog スレッド。Arm/Disarm + **世代カウンタ**で誤爆を防ぐ
- CPU 会計は「JS 実行スライスの wall-clock 合計」で近似。
  イベントループの構造上、**await 中は自然に無料になる**
- terminate 後は op テーブルを破棄し、原因（CPU/メモリ）を分類して報告
