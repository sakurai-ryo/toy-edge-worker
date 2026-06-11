# Isolate プールとコールドスタート

M4 です。前章の素朴な HashMap キャッシュを、容量制限つきの LRU プールに
進化させ、cold start を計測できるようにします。Isolate モデルの売りである
「ミリ秒未満の起動」を、自分のコードで実測する章です。

## なぜプールか — warm/cold の経済学

Isolate は軽いとはいえ、無限には持てません。1 つの warm Isolate は
ヒープ・コンパイル済みコード・Global ハンドルを抱えています。
テナント数がメモリを超えたら、誰かを「忘れる」必要があります。

| 状態 | 意味 | レイテンシ |
|---|---|---|
| cold | Isolate が存在しない。生成から始める | +0.7ms 程度（実測・後述） |
| warm | 評価済み Isolate がプールにある | +0（取り出すだけ） |
| condemned | 壊れた Isolate（第 14 章）。破棄待ち | — |

方針は「**メモリが許す限り warm を維持し、溢れたら LRU で evict**」。
Cloudflare も同じで、彼らのドキュメントには「worker は使われなくなると
evict されうる。次のリクエストは cold start」と明記されています。

## `Pool<W>` — v8 に依存しない LRU

プールは `pool` crate に置きます。ポイントはジェネリクスです。

```rust
// crates/pool/src/lib.rs（抜粋）
pub struct Pool<W> {
    capacity: usize,
    entries: HashMap<String, Entry<W>>,
    tick: u64,                  // LRU 用の論理時計
}

struct Entry<W> {
    worker: W,
    last_used: u64,
}

impl<W> Pool<W> {
    pub fn get(&mut self, tenant: &str) -> Option<&mut W> {
        self.tick += 1;
        let tick = self.tick;
        self.entries.get_mut(tenant).map(|entry| {
            entry.last_used = tick;     // touch
            &mut entry.worker
        })
    }

    /// 容量超過なら最古のエントリを evict して返す
    pub fn insert(&mut self, tenant: String, worker: W) -> Option<(String, W)> {
        self.tick += 1;
        let evicted = if self.entries.len() >= self.capacity {
            self.evict_lru()
        } else { None };
        self.entries.insert(tenant, Entry { worker, last_used: self.tick });
        evicted
    }

    fn evict_lru(&mut self) -> Option<(String, W)> {
        let oldest = self.entries.iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(tenant, _)| tenant.clone())?;
        let entry = self.entries.remove(&oldest)?;
        Some((oldest, entry.worker))
    }
}
```

設計メモ:

- **論理時計（tick）**: `Instant::now()` ではなく単調増加のカウンタ。
  時刻は要らない、順序だけあればいい
- **線形探索の LRU**: `lru` crate を使わず `min_by_key` で全走査。
  per-thread のエントリ数は `isolates_max / worker_threads`（既定 16/4 = 4）
  程度なので、賢い双方向リストは過剰。**遅くなってから賢くする**
- **`Pool<u32>` でテストできる**: v8 非依存にしたおかげで、LRU の
  挙動テストが整数で書ける

```rust
#[test]
fn lru_evicts_oldest() {
    let mut pool: Pool<u32> = Pool::new(2);
    pool.insert("a".into(), 1);
    pool.insert("b".into(), 2);
    pool.get("a");                              // a に触れて b を最古にする
    let evicted = pool.insert("c".into(), 3);
    assert_eq!(evicted, Some(("b".into(), 2))); // b が追い出される
}
```

evict された Worker は worker スレッド上で drop されます。第 11 章の
`Drop for Worker`（enter してから破棄）がここで効きます。evict 時点では
どの Isolate もカレントでない（リクエスト間）ので、規律どおりです。

## プールは per-thread、メトリクスは共有

プール自体は worker スレッドごとに持ちます（Isolate はスレッドを
跨げないので当然そうなる）。一方、観測はプロセス全体でしたいので、
メトリクスだけ `Arc<Metrics>`（内部は `Mutex<HashMap<String, TenantMetrics>>`）
で共有します。

```rust
#[derive(Default, Clone, Serialize)]
pub struct TenantMetrics {
    pub requests: u64,
    pub errors: u64,
    pub cold_starts: u64,
    pub warm_hits: u64,
    pub evictions: u64,
    pub cold_start_ms_last: f64,
    pub cold_start_ms_max: f64,
    pub cold_start_ms_sum: f64,
    pub terminations_cpu: u64,    // 第 13 章
    pub terminations_mem: u64,    // 第 14 章
}
```

`GET /__admin/metrics` でこの HashMap をそのまま JSON にして返します。
worker スレッドのホットパスに入る計測は `Mutex` 1 回の lock ですが、
JS 実行のコスト（ms 級）に対して無視できます。

worker スレッドのループはこう変わりました:

```rust
Msg::Work(item) => {
    metrics.with(tenant, |m| m.requests += 1);

    if workers.get(tenant).is_some() {
        metrics.with(tenant, |m| m.warm_hits += 1);
    } else {
        let worker = cold_start(index, &tenants, tenant, &metrics, &tokio_handle)?;
        if let Some((evicted, _)) = workers.insert(tenant.clone(), worker) {
            metrics.with(&evicted, |m| m.evictions += 1);
        }
    }
    let worker = workers.get(tenant).expect("worker just inserted");
    let result = worker.handle(&item.request);
    // ...
}
```

`cold_start` 関数が `Worker::new` を `Instant` で挟んで計測する場所です。

## 実測する

`isolates_max` を絞った設定で 3 テナントを叩き、eviction を強制します
（worker_threads = 1、容量 = 2）。

```sh
$ curl -s http://127.0.0.1:8788/hello   > /dev/null   # cold hello
$ curl -s http://127.0.0.1:8788/echo/x  > /dev/null   # cold echo
$ curl -s http://127.0.0.1:8788/hello   > /dev/null   # warm hello（echo が最古に）
$ curl -s http://127.0.0.1:8788/third   > /dev/null   # cold third → echo を evict
$ curl -s http://127.0.0.1:8788/echo/x  > /dev/null   # cold echo（再）→ hello を evict
```

サーバログ:

```
[worker-0] cold start tenant=hello (5.2ms)     ← プロセス初回は V8 初期化込み
[worker-0] cold start tenant=echo (0.7ms)
[worker-0] cold start tenant=third (0.8ms)
[worker-0] evicted tenant=echo (LRU, capacity=2)
[worker-0] cold start tenant=echo (0.6ms)
[worker-0] evicted tenant=hello (LRU, capacity=2)
```

`/__admin/metrics`（抜粋）:

```json
{
  "hello": { "requests": 2, "cold_starts": 1, "warm_hits": 1, "evictions": 1,
             "cold_start_ms_last": 5.23 },
  "echo":  { "requests": 2, "cold_starts": 2, "warm_hits": 0, "evictions": 1,
             "cold_start_ms_last": 0.65 }
}
```

LRU の動きが教科書どおりであること（hello に触れたので echo が先に
追い出される）、そして cold start の実測値が読み取れます。

### cold start 0.7ms の内訳と意味

cold start（`Worker::new`）の中身は第 9 章で見たとおり:
Isolate 生成 → Context + bootstrap.js 評価 → ESM compile →
instantiate + evaluate → ハンドラ取り出し。これが合計 **約 0.7ms** です
（プロセス最初の 1 回だけは V8 プラットフォーム初期化が乗って約 5ms）。

前作 toy-lambda-runtime の microVM cold start は数百 ms のオーダーでした。
**3 桁の差**です。第 2 章で述べた「エッジで全テナントを賄えるのは
Isolate だけ」という主張が、自分の実装でも再現されたことになります。

さらに縮めたければ V8 snapshot（bootstrap 評価済みのヒープを焼き込む）
という技がありますが、0.7ms の大半は ESM の compile/evaluate であり、
toy の規模では割に合わないので発展課題（第 16 章）としました。

## この章のまとめ

- warm/cold/condemned の 3 状態。メモリの許す限り warm 維持、溢れたら LRU
- プールは per-thread（Isolate の都合）、メトリクスはプロセス共有
- LRU は論理時計 + 線形探索で十分。v8 非依存にして整数でテスト
- cold start 実測 約 0.7ms。microVM 比で 3 桁速い —— Isolate モデルの
  存在理由を数字で確認した
