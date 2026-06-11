# イベントループを自作する

本書の最重要章です。`async fetch(request) { await ... }` が動くためには
誰かが microtask を回し、非同期処理の完了を Promise に届けなければ
なりません。Node.js なら libuv、Deno なら deno_core の
`JsRuntime::run_event_loop` がやっている仕事 — それを自分で書きます。

## イベントループとは何を「ループ」しているのか

JS の非同期は 2 層構造です。

1. **microtask 層**（V8 の内側）: `await` の継続、`.then` のコールバック。
   `perform_microtask_checkpoint()` を呼べば V8 が勝手に全部流してくれる
2. **イベント層**（V8 の外側）: タイマー満了、ネットワーク I/O 完了など
   「外の世界の出来事」。**V8 は一切関知しない**。埋め込み側が検知して、
   対応する Promise を resolve してやる必要がある

イベントループとは、この 2 層を交互に駆動する装置です:

```
┌────────────────────────────────────────────┐
│ microtask を全部流す（JS が走る）            │
│   ↓                                        │
│ 目的の Promise は settle した？ → Yes: 終了  │
│   ↓ No                                     │
│ 外の世界の完了通知を待つ（ブロック）          │
│   ↓ 届いた                                  │
│ 対応する resolver を resolve（予約だけ）      │
└──── 先頭に戻る ─────────────────────────────┘
```

## 非同期 op のモデル

「外の世界」との接続は **op（operation）** という形に統一します。
deno_core から借りた語彙です。第 7 章の `timer` op を例にします。

```
JS:   await __ops.timer(10)
       │
Rust: op_timer コールバック
       │ 1. PromiseResolver を作る
       │ 2. resolver を Global 化して resolvers テーブルに登録（op_id 払い出し）
       │ 3. pending_ops += 1
       │ 4. 完了を届けるスレッド/タスクを起動
       │ 5. Promise を JS に即 return  ← JS はこの Promise を await する
       ▼
（10ms 後、別スレッド）
       │ tx.send(LoopEvent::OpComplete { op_id, result })
       ▼
worker スレッドのイベントループ:
       │ rx.recv() で受信
       │ resolvers から op_id で resolver を取り出して resolve（予約）
       │ 次周の perform_microtask_checkpoint() で await の続きが走る
       ▼
JS:   await が返ってくる
```

### op の状態 — OpState

```rust
// crates/runtime/src/ops.rs（抜粋）
pub enum LoopEvent {
    OpComplete { op_id: u64, result: Result<OpResult, String> },
}

pub enum OpResult {
    Timer,
    Fetch { status: u16, headers: Vec<(String, String)>, body: Vec<u8> },
}

pub struct OpState {
    pub tx: mpsc::Sender<LoopEvent>,     // 完了を届ける口（クローンして配る）
    pub resolvers: HashMap<u64, v8::Global<v8::PromiseResolver>>,
    pub pending_ops: u64,
    next_op_id: u64,
    tokio: Option<tokio::runtime::Handle>,  // 第 15 章
    http: reqwest::Client,                  // 第 15 章
}
```

`OpState` は `isolate.set_slot(Rc<RefCell<OpState>>)` で Isolate に
ぶら下げ、op コールバックから `scope.get_slot()` で取り出します
（第 4 章の slot の出番です）。

一方 **`Receiver` は slot に入れず Worker 構造体に直接持たせます**。
これは設計上の急所です:

> イベントループは `rx.recv()` でブロックします。もし Receiver が
> `RefCell` の中にあると、recv でブロックしている間 `borrow()` を
> 保持し続けることになり、受信後に resolver を取り出す
> `borrow_mut()` と衝突してパニックします。
> **「待つもの（rx）」と「触るもの（テーブル）」は別の場所に置く。**

### timer op の実装

`timer` は最小の op です。完了通知のスレッドが `std::thread::spawn` で
済むため、tokio なしで動きます（イベントループの単体検証に最適）。

```rust
fn op_timer(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments,
            mut rv: v8::ReturnValue) {
    let ms = args.get(0).number_value(scope).unwrap_or(0.0).max(0.0) as u64;

    let resolver = v8::PromiseResolver::new(scope).unwrap();
    rv.set(resolver.get_promise(scope).into());     // Promise を即 return

    let state = get_op_state(scope);
    let resolver = v8::Global::new(scope, resolver); // ★ Local はこの関数と共に死ぬ
    let (op_id, tx) = {
        let mut st = state.borrow_mut();
        (st.register(resolver), st.tx.clone())
    };

    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(ms));
        let _ = tx.send(LoopEvent::OpComplete { op_id, result: Ok(OpResult::Timer) });
    });
}
```

★の行が第 4 章の Global の典型的な使い所です。resolver はこのコールバック
関数を抜けたあと（タイマー満了後）に使うので、Local のままでは持てません。

## イベントループ本体

```rust
// crates/runtime/src/worker.rs（抜粋・注釈追加）
fn run_event_loop_until_settled(
    scope: &mut v8::PinScope,
    op_rx: &mpsc::Receiver<LoopEvent>,
    target: &v8::Global<v8::Promise>,
    deadline: Instant,
    meter: &mut CpuMeter,                 // 第 13 章で登場。今は素通しと思ってよい
) -> Result<v8::Global<v8::Value>, WorkerError> {
    let state = ops::get_op_state(scope);
    loop {
        // (1) microtask を全部流す（ここで JS が走る）
        meter.run(|| scope.perform_microtask_checkpoint());

        // (2) terminate（CPU/メモリ制限）されていないか
        if scope.is_execution_terminating() {
            state.borrow_mut().abort_all();
            return Err(WorkerError::Terminated);
        }

        // (3) 目的の Promise の状態を確認
        let promise = v8::Local::new(scope, target);
        match promise.state() {
            v8::PromiseState::Fulfilled => {
                let value = promise.result(scope);
                return Ok(v8::Global::new(scope, value));
            }
            v8::PromiseState::Rejected => {
                let exception = promise.result(scope);
                return Err(WorkerError::Script(format_exception(scope, exception)));
            }
            v8::PromiseState::Pending => {}
        }

        // (4) 進行中の op が無いのに Pending なら、永遠に解決しない
        if state.borrow().pending_ops == 0 {
            return Err(WorkerError::Hung);
        }

        // (5) op の完了を待つ（= イベントループの唯一の待機点）
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            state.borrow_mut().abort_all();
            return Err(WorkerError::Timeout);
        }
        match op_rx.recv_timeout(timeout) {
            Ok(LoopEvent::OpComplete { op_id, result }) => {
                complete_op(scope, &state, op_id, result);
                // 溜まっている完了通知は非ブロッキングで吸い出す
                while let Ok(LoopEvent::OpComplete { op_id, result }) = op_rx.try_recv() {
                    complete_op(scope, &state, op_id, result);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                state.borrow_mut().abort_all();
                return Err(WorkerError::Timeout);
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(WorkerError::Internal("op channel disconnected".into()));
            }
        }
    }
}
```

### 設計の読みどころ

**Promise はポーリングで見る。** `.then` でコールバックを仕込む方式も
可能ですが、「ループの先頭で `promise.state()` を見る」ほうが圧倒的に
単純です。state は `Pending / Fulfilled / Rejected` の 3 値で、
settle 済みなら `promise.result()` で値（または例外）が取れます。

**ブロッキング recv がイベントループの「待機点」。** epoll に相当する
ものが、ここでは単なる `mpsc::recv_timeout` です。op の完了通知が
唯一の起床要因なので、これで十分です。CPU は一切無駄になりません
（待っている間、worker スレッドは眠っている）。

**(4) のハング検知は自作ならではの明快さ。**

```js
await new Promise(() => {});   // 誰も resolve しない Promise
```

このコードは、resolver がどの op テーブルにも登録されないため
`pending_ops == 0` のまま Pending になり、即座に
`WorkerError::Hung`（"promise will never resolve"）で検出されます。
タイムアウトまで待つ必要すらありません。deno_core にも同じ検査があり、
イベントループが「自分が何を待っているか」を完全に把握しているからこそ
できる芸当です。

**resolve は「予約」にすぎない。** `complete_op` の中で
`resolver.resolve(value)` を呼んでも JS は 1 行も走りません
（`MicrotasksPolicy::Explicit` のため）。実際に `await` の続きが走るのは
ループ先頭の checkpoint。「**イベント処理と JS 実行を分離する**」のが
Explicit ポリシーの意味です。

### complete_op — resolver への配達

```rust
fn complete_op(scope: &mut v8::PinScope, state: &Rc<RefCell<OpState>>,
               op_id: u64, result: Result<OpResult, String>) {
    let resolver = {
        let mut st = state.borrow_mut();
        let Some(resolver) = st.resolvers.remove(&op_id) else {
            return;   // terminate で破棄済みの op が遅れて完了した → 無視
        };
        st.pending_ops -= 1;
        resolver
    };
    let resolver = v8::Local::new(scope, &resolver);
    match result {
        Ok(OpResult::Timer) => { resolver.resolve(scope, v8::undefined(scope).into()); }
        Ok(OpResult::Fetch { .. }) => { /* JS オブジェクトを組んで resolve（第 15 章） */ }
        Err(message) => { /* TypeError を作って reject */ }
    }
}
```

「テーブルに無い op_id は黙って無視」という一行に注目してください。
terminate やタイムアウトで `abort_all()`（テーブル全クリア）したあとも、
飛行中だったタイマースレッドや fetch タスクは完了通知を送ってきます。
それを安全に受け流すための取り決めです。**非同期システムでは
「キャンセルした仕事の完了報告」が必ず遅れて届く** — この前提を
データ構造に織り込んでおくと、あちこちの競合が一気に消えます。

## deno_core との対応表

自作した部品は、deno_core では次に対応します（答え合わせ用）:

| 本書 | deno_core |
|---|---|
| `run_event_loop_until_settled` | `JsRuntime::run_event_loop` + `resolve_value` |
| `OpState`（slot 内） | `OpState` + `OpCtx` |
| `LoopEvent` チャネル | ops の完了を運ぶ `tokio` タスク群と FuturesUnordered |
| `pending_ops == 0` で Hung | `has_pending_ops` 等によるイベントループ完了判定 |
| `MicrotasksPolicy::Explicit` + checkpoint | 同じ |

deno_core は tokio に深く統合されていて完了通知が Future ベースですが、
本書は「素の mpsc チャネル」で同じ構造を作りました。骨格が同じであること
を確認しながら deno_core のソースを読むと、よい復習になります。

## 動かす

M1 の完成形デモです。microtask（`await Promise.resolve()`）と
op（`setTimeout`）の両方が 1 つの worker の中で動きます。

```sh
$ cargo run -p edged -- invoke examples/workers/hello.js --url /foo --env GREETING=hi
GET http://localhost/foo
HTTP 200
content-type: application/json

{"hello":"world","url":"http://localhost/foo","greeting":"hi"}

# ハング検知
$ cargo run -p edged -- invoke /tmp/hung_test.js
Error: promise will never resolve
```

## この章のまとめ

- イベントループ = microtask 層（checkpoint で V8 に任せる）と
  イベント層（op の完了通知を recv で待つ）を交互に回す装置
- 非同期 op = resolver を Global で預かり、完了通知を channel で受け、
  resolve を「予約」する仕組み。`Promise.all` も自然に動く
- pending_ops カウンタで「永遠に解決しない await」を即検知できる
- 「待つもの（rx）」と「触るもの（RefCell のテーブル）」は分離する
- 遅れて届く完了通知は「テーブルに無い → 無視」で受け流す
