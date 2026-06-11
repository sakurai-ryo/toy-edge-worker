# メモリ制限 — heap limit との攻防

M5 後半です。もう 1 つの資源、メモリを守ります。対戦相手はこれです。

```js
// examples/workers/membomb.js
export default {
  async fetch() {
    const chunks = [];
    while (true) {
      chunks.push(new Array(1024 * 1024).fill(Math.random()));
    }
  },
};
```

制限がなければ、この worker はプロセスの（= **全テナントの**）メモリを
食い尽くし、最後は OOM killer がプロセスごと殺します。Isolate モデルでは
ヒープはテナントごとに分かれているので、「この Isolate のヒープは
64MB まで」という上限を掛けられます。

## heap_limits — 上限の設定

上限は Isolate 生成時に `CreateParams` で指定します。

```rust
// crates/runtime/src/worker.rs（抜粋）
let mut params = v8::CreateParams::default();
if let Some(limit) = config.heap_limit {
    params = params.heap_limits(0, limit);     // (initial, max) バイト
}
let mut isolate = v8::Isolate::new(params);
```

これだけで「上限に達したら止まる」が手に入る — と思いきや、
そうは問屋が卸しません。**V8 はヒープ上限に達すると、既定では
プロセスごと abort します**（`Fatal JavaScript out of memory`）。
ブラウザならタブクラッシュで済みますが、マルチテナントのサーバで
プロセス abort は全テナント死です。絶対に避けなければなりません。

## near_heap_limit_callback — abort までの最後の砦

V8 は abort の前に、埋め込み側へ最後の相談をしてくれます。それが
**NearHeapLimitCallback** です。「ヒープが上限に近い。どうする？
新しい上限を返せ」という形のコールバックで、ここが唯一の介入点です。

```rust
// crates/runtime/src/limits.rs（抜粋）
pub struct MemCtx {
    pub handle: v8::IsolateHandle,
    pub hit: Arc<AtomicBool>,
}

pub unsafe extern "C" fn near_heap_limit_callback(
    data: *mut c_void,
    current_heap_limit: usize,
    _initial_heap_limit: usize,
) -> usize {
    let ctx = unsafe { &*(data as *const MemCtx) };
    ctx.hit.store(true, Ordering::SeqCst);      // ① 原因フラグを立てる
    ctx.handle.terminate_execution();           // ② 実行を止める（要求）
    current_heap_limit + 8 * 1024 * 1024        // ③ 上限を 8MiB 引き上げて返す
}
```

3 行それぞれに意味があります。

### ② terminate — 止めるのは結局これ

メモリを止める手段も、CPU と同じ `terminate_execution()` です。
コールバックは V8 の GC パスから呼ばれるため、ここで直接何かを破棄する
ことはできません。「止まってくれ」と要求するだけです。

### ③ 上限引き上げ — 直感に反するが必須

terminate を要求したのに、なぜ上限を**上げる**のか。

terminate は「最も近い機会」に効きますが、その瞬間までの間、JS は
まだ動いています。さらに、**例外で巻き戻る過程やエラーオブジェクトの
構築自体がヒープを割り当てる**ことがあります。上限を据え置く
（`current_heap_limit` をそのまま返す）と、巻き戻り中の割り当てが
再び上限に当たり、今度こそ V8 は abort します。

「+8MiB」は、terminate が完了するまでの**脱出用の酸素ボンベ**です。
この Isolate はどうせ直後に破棄する（後述）ので、一時的な超過は
問題になりません。deno_core も同じ構造のコールバックを実装しています。

### ① 原因フラグ — CPU か、メモリか

terminate 検知のコードパス（第 13 章の `classify_termination`）は
CPU とメモリで共通です。区別のために、コールバックで
`Arc<AtomicBool>` のフラグを立てておき、後で読みます。

```rust
fn classify_termination(&self) -> WorkerError {
    self.isolate.cancel_terminate_execution();
    ops::get_op_state(&self.isolate).borrow_mut().abort_all();
    if self.mem_hit.load(Ordering::SeqCst) {
        WorkerError::MemExceeded     // ← コールバックがフラグを立てていた
    } else {
        WorkerError::CpuExceeded
    }
}
```

`AtomicBool` なのは、コールバックが V8 の GC スレッド文脈から呼ばれる
可能性を考慮しての保守的な選択です。

## コールバックに渡すデータの寿命 — Box と宣言順ふたたび

`add_near_heap_limit_callback(callback, data)` の `data` は生ポインタです。
V8 は **Isolate が生きている限り**このポインタを保持し、コールバックの
たびに渡してきます。つまり `MemCtx` は Isolate より長生きでなければ
なりません。

```rust
let mem_ctx = config.heap_limit.map(|_| {
    let ctx = Box::new(MemCtx { handle: isolate_handle.clone(), hit: mem_hit.clone() });
    isolate.add_near_heap_limit_callback(
        limits::near_heap_limit_callback,
        &*ctx as *const MemCtx as *mut c_void,
    );
    ctx       // Box を Worker のフィールドに保管して寿命を保証する
});
```

そして Worker 構造体（第 9 章）のフィールド順をもう一度見てください。

```rust
pub struct Worker {
    // ... Global 群 ...
    isolate: v8::OwnedIsolate,
    _mem_ctx: Option<Box<MemCtx>>,   // ← isolate の「後」に宣言 = 後に drop
}
```

Global 群は isolate より**先に**、`_mem_ctx` は isolate より**後に**
drop されるよう並んでいます。1 つの構造体のフィールド順に、
2 種類の寿命制約がエンコードされているわけです。Rust で FFI 境界の
リソースを扱うときの典型的な形です。

## メモリ超過した Isolate は破棄する

CPU 超過（第 13 章）では Isolate を再利用しました。メモリ超過では
**破棄**します。理由:

- ヒープは膨らんだまま（`chunks` 配列が GC ルートから切れたとしても、
  上限は +8MiB された状態で、ヒープの統計も荒れている）
- 「メモリを食い尽くす寸前だった Isolate」を信用して使い続けるのは
  リスクに見合わない。0.7ms で作り直せるのだから捨てるほうが安い

サーバ側（worker スレッド）の分岐:

```rust
Err(WorkerError::MemExceeded) => {
    metrics.with(tenant, |m| { m.errors += 1; m.terminations_mem += 1; });
    workers.remove(tenant);    // プールから外して drop（= isolate 破棄）
    println!("[worker-{index}] heap limit hit tenant={tenant} (isolate discarded)");
}
```

`workers.remove` で Worker が drop され、第 11 章の Drop 規律
（enter → Global → isolate → MemCtx の順で破棄）が走ります。
次のリクエストは cold start からやり直しです。これは Cloudflare Workers の
「Exceeded Memory エラー後は Isolate がリセットされる」挙動と同じです。

## 動かす

```sh
$ curl -s -w "%{http_code}" http://127.0.0.1:8787/membomb
worker exceeded memory limit
503

$ curl -s -w "%{http_code}" http://127.0.0.1:8787/membomb    # 2 回目も同じ
503

$ curl -s http://127.0.0.1:8787/__admin/metrics | jq .membomb
{
  "requests": 2,
  "cold_starts": 2,        ← 毎回 cold start（破棄されている証拠）
  "warm_hits": 0,
  "terminations_mem": 2
}
```

サーバログ:

```
[worker-1] cold start tenant=membomb (0.7ms)
[worker-1] heap limit hit tenant=membomb (isolate discarded)
[worker-1] cold start tenant=membomb (0.7ms)        ← 作り直し
[worker-1] heap limit hit tenant=membomb (isolate discarded)
```

loop.js（CPU 超過 → `warm_hits` が増える）との対比がメトリクスに
きれいに現れます。**「CPU は再利用、メモリは破棄」**という方針の違いが
観測値で確認できました。

## この章のまとめ

- ヒープ上限は `CreateParams::heap_limits`。ただし既定の上限到達は
  **プロセス abort** — マルチテナントでは許されない
- `near_heap_limit_callback` で (1) フラグ (2) terminate 要求
  (3) **上限を一時引き上げ**（巻き戻り中の割り当てで abort しないため）
- コールバックの data は Isolate より長生きさせる。Box + フィールド
  宣言順で寿命をエンコード
- CPU 超過は Isolate 再利用、メモリ超過は破棄して cold start からやり直し
