# リクエスト 1 件の旅

部品が揃いました。ESM ロード（第 6 章）、Web API（第 7 章）、
イベントループ（第 8 章）を 1 本に繋ぎ、
「`WorkerRequest` を入れると `WorkerResponse` が出てくる」
`Worker` 構造体を完成させます。M1 のゴールです。

## Worker 構造体 — 何を保持するか

Worker は「1 つの worker スクリプトを評価済みの Isolate」です。
リクエストのたびに使い回すので、評価結果への参照を `Global` で持ちます。

```rust
// crates/runtime/src/worker.rs（抜粋）
pub struct Worker {
    context: v8::Global<v8::Context>,
    fetch_handler: v8::Global<v8::Function>,        // default export の fetch
    build_request: v8::Global<v8::Function>,        // bootstrap のヘルパ
    serialize_response: v8::Global<v8::Function>,   // 〃
    env: v8::Global<v8::Object>,                    // fetch の第 2 引数
    op_rx: mpsc::Receiver<LoopEvent>,               // イベントループの待機点
    wall_timeout: Duration,
    cpu_budget: Option<Duration>,                   // 第 13 章
    isolate_handle: v8::IsolateHandle,              // 〃
    mem_hit: Arc<AtomicBool>,                       // 第 14 章
    isolate: v8::OwnedIsolate,                      // ★ 最後に宣言
    _mem_ctx: Option<Box<MemCtx>>,                  // 第 14 章
}
```

### ★ フィールドの宣言順は仕様である

Rust の構造体は **宣言順にフィールドが drop されます**。そして
`v8::Global` の drop は Isolate がまだ生きていることを要求します
（V8 側のハンドル台帳から自分を消すため）。逆順にすると、破棄済み
Isolate に触ってクラッシュします。

> **規約: Global 群を上に、`OwnedIsolate` を一番下に書く。**
> この並びは見た目の整理ではなく、メモリ安全性の一部。

## Worker::new — cold start でやること全部

```rust
pub fn new(script: &str, script_url: &str, config: WorkerConfig)
    -> Result<Self, WorkerError>
{
    crate::init();                                    // platform 初期化（Once）

    let mut isolate = v8::Isolate::new(params);       // ヒープ上限は第 14 章
    isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);

    let (tx, op_rx) = mpsc::channel();
    isolate.set_slot(Rc::new(RefCell::new(OpState::new(tx, config.tokio.clone()))));

    let (context, fetch_handler, build_request, serialize_response, env) = {
        v8::scope!(let hs, &mut isolate);
        let context = v8::Context::new(hs, Default::default());
        let context_global = v8::Global::new(hs, context);
        let cs = &mut v8::ContextScope::new(hs, context);
        v8::scope!(let scope, cs);

        ops::install_ops(scope);                          // ① __ops を生やす
        run_script(scope, BOOTSTRAP_JS, "tew:bootstrap.js")?; // ② bootstrap 評価
        let (build_request, serialize_response) = take_runtime_helpers(scope)?; // ③
        let env = build_env(scope, &config.env);          // ④ env オブジェクト

        let mut meter = CpuMeter::new(isolate_handle.clone(), config.cpu_budget);
        let module = compile_and_instantiate(scope, script, script_url)?; // ⑤
        evaluate_module(scope, &op_rx, &module, config.wall_timeout, &mut meter)?; // ⑥
        let fetch_handler = extract_fetch_handler(scope, &module)?;       // ⑦

        (context_global, fetch_handler, build_request, serialize_response, env)
    };

    unsafe { isolate.exit() };   // park（第 11 章で解説）

    Ok(Self { /* ... */ })
}
```

この関数が **cold start の正体**です。第 12 章で計測すると、
①〜⑦ 全部で約 0.7ms（プロセス初回のみ V8 初期化が乗って約 5ms）でした。

`env` は workers.toml の `env = { GREETING = "hi" }` を JS オブジェクトに
変換したものです。Cloudflare Workers の environment variables 相当で、
ハンドラの第 2 引数として渡します。

## Worker::handle — リクエスト処理の本体

5 ステップです。各ステップの間に第 8 章のイベントループが挟まります。

```
(1) WorkerRequest → JS の Request   （buildRequest ヘルパ呼び出し）
(2) fetch(request, env, ctx) を呼ぶ → 戻り値を Promise に正規化
(3) イベントループで Promise の解決を待つ → Response（JS 値）
(4) serializeResponse ヘルパ呼び出し → これも async なので再度ループ
(5) [status, headers, body] を Rust 値に変換 → WorkerResponse
```

### (1) Rust から JS の Request を作る

Rust 側で `Request` クラスのコンストラクタを直接探して呼ぶこともでき
ますが、引数の正規化ロジックを JS 側に集約したいので、bootstrap に
用意した `buildRequest(url, method, headersArr, bodyAb)` ヘルパを呼びます。
**「Rust → JS の境界は、平坦な値（文字列・配列・ArrayBuffer）だけを
渡す」**のが方針です。複雑なオブジェクトの組み立てはどちらか一方の
世界でやる（ここでは JS）。

```rust
let js_request = {
    v8::tc_scope!(let tc, scope);
    let build = v8::Local::new(tc, &self.build_request);
    let url: v8::Local<v8::Value> = v8::String::new(tc, &req.url).unwrap().into();
    let method: v8::Local<v8::Value> = v8::String::new(tc, &req.method).unwrap().into();
    let headers: v8::Local<v8::Value> = headers_to_js(tc, &req.headers).into();
    let body: v8::Local<v8::Value> = if req.body.is_empty() {
        v8::undefined(tc).into()
    } else {
        ops::bytes_to_array_buffer(tc, req.body.clone()).into()
    };
    let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
    let Some(js_request) = meter.run(|| build.call(tc, recv, &[url, method, headers, body]))
    else {
        return Err(WorkerError::Internal(format!(
            "buildRequest failed: {}", exception_message!(tc))));
    };
    v8::Global::new(tc, js_request)
};
```

`call` の第 2 引数 `recv` は JS の `this` です。普通の関数呼び出しなので
`undefined` を渡します。

### (2) ハンドラ呼び出しと Promise 正規化

```rust
let result_promise = {
    v8::tc_scope!(let tc, scope);
    let handler = v8::Local::new(tc, &self.fetch_handler);
    // env, ctx を用意して...
    let Some(ret) = meter.run(|| handler.call(tc, recv, &[js_request, env, ctx])) else {
        if tc.has_terminated() {
            return Err(WorkerError::Terminated);   // 制限による強制終了（第 13 章）
        }
        return Err(WorkerError::Script(exception_message!(tc)));  // 同期 throw
    };
    let promise = match v8::Local::<v8::Promise>::try_from(ret) {
        Ok(promise) => promise,
        Err(_) => {
            // 同期で Response を返すハンドラも許容（Promise.resolve 相当）
            let resolver = v8::PromiseResolver::new(tc).unwrap();
            resolver.resolve(tc, ret);
            resolver.get_promise(tc)
        }
    };
    v8::Global::new(tc, promise)
};

let response = run_event_loop_until_settled(
    scope, &self.op_rx, &result_promise, deadline, &mut meter)?;
```

2 つの分岐に意味があります。

- **同期 throw と async reject は別経路で届く。**
  `fetch() { throw new Error() }`（async でない関数の throw）は
  `call` が `None` を返し TryCatch に入る。`async fetch() { throw }` は
  call 自体は成功して Rejected な Promise が返る。両方を正しく
  `WorkerError::Script` に落とす
- **非 Promise の戻り値も包んで一本化。** `fetch() { return new Response("x") }`
  のような同期ハンドラも、resolver で包めば以降の処理は完全に同じになる

### (4)(5) Response の取り出し

`serializeResponse`（bootstrap 側）は Response を検証して
`[status, [...headers], await resp.arrayBuffer()]` の 3 要素配列に潰します。
**body の読み出しが async**（Body の意味論に従う）なので、この呼び出しも
Promise を返し、もう一度イベントループを回します。

返ってきた配列を Rust 側で分解すれば完成です:

```rust
let triple = v8::Local::<v8::Array>::try_from(triple)?;
let status = triple.get_index(scope, 0).and_then(|v| v.uint32_value(scope)).unwrap_or(200) as u16;
let headers = triple.get_index(scope, 1).map(|v| js_headers_to_vec(scope, v)).unwrap_or_default();
let body = triple.get_index(scope, 2).and_then(ops::value_to_bytes).unwrap_or_default();
Ok(WorkerResponse { status, headers, body })
```

## エラー経路の全体図

handle() を通るエラーは、最終的に HTTP ステータスに対応します
（HTTP 化は次章）。設計時に整理した対応表です:

| worker の挙動 | 検出箇所 | WorkerError | HTTP |
|---|---|---|---|
| `throw`（同期） | call が None + TryCatch | Script | 500 |
| reject / async throw | Promise が Rejected | Script | 500 |
| Response 以外を return | serializeResponse が throw | Script | 500 |
| 解決しない Promise を await | pending_ops == 0 | Hung | 500 |
| 遅すぎる（I/O 込み） | recv_timeout 超過 | Timeout | 503 |
| CPU 食いすぎ | watchdog → terminate | CpuExceeded | 503 |
| メモリ食いすぎ | heap limit → terminate | MemExceeded | 503 |

ストリーミング（body を少しずつ返す）は本実装では扱いません。body は
リクエストもレスポンスも全バッファです。ReadableStream を導入する場合の
拡張方針は第 16 章で触れます。

## 動かして確かめる

```sh
# 正常系（POST body・ヘッダ・env すべて通る）
$ cargo run -p edged -- invoke /tmp/echo_test.js \
    --url /echo --method post --body 'hello body' --header 'Content-Type: text/plain'
HTTP 201
method=POST ct=text/plain body=hello body

# 同期 throw → スタックトレース付きで報告
$ cargo run -p edged -- invoke /tmp/err_test.js
Error: script error: Error: boom
    at fetch (file:///private/tmp/err_test.js:2:25)
```

## この章のまとめ

- Worker = 評価済み Isolate + Global ハンドル束。**フィールド宣言順が
  安全性の一部**（Global → isolate の順で drop）
- Rust⇔JS の境界では平坦な値だけを渡し、オブジェクト組み立ては
  bootstrap のヘルパ（JS 側）に寄せる
- 同期 throw / async reject / 非 Promise 戻り値、すべての経路を
  Promise 1 本に正規化してからイベントループに渡す
- エラーは「誰の責任か」で分類し、HTTP ステータスへ写像する

これで「1 つの worker を 1 回呼ぶ」が完成しました。第 IV 部では
これを HTTP サーバに載せ、複数テナントに拡張します。
