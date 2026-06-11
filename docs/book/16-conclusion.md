# まとめと発展課題

## 作ったものの全体像

約 2,000 行の Rust と 200 行の JS で、次が動くようになりました。

- ES Modules 形式（`export default { fetch }`）の worker を実行する
  V8 ベースのランタイム
- Request / Response / Headers / console / setTimeout / fetch の Web API シム
- 自作イベントループ（microtask + 非同期 op + ハング検知）
- Host/Path ルーティングによるマルチテナント（1 プロセス・複数 Isolate）
- LRU プールと cold start 計測（実測 約 0.7ms）
- CPU 制限（watchdog + terminate、await は無料）と
  メモリ制限（heap limit + Isolate 破棄）
- worker 内からの外部 fetch（tokio + reqwest との接続）

## 学びの棚卸し

各章で得た「持ち帰れる知見」を 1 行ずつに圧縮します。

**V8 編**
- Isolate = ヒープと GC の単位、1 スレッド占有。Context = globalThis の単位
- Local はスコープ縛り、Global は GC ルート。Rust ではこの規約が型になる
- 素の V8 には何もない。ランタイムが生やした API の総体が攻撃面
- microtask の flush は埋め込み側が握れる（Explicit）。
  「resolve は予約、checkpoint で実行」
- 現行 rusty_v8 は Pin + マクロのスコープ API。古い記事は通用しない

**ランタイム編**
- ESM は compile → instantiate → evaluate。evaluate は常に Promise を返す
- ランタイム = 少数の Rust op + 大量の JS（bootstrap）。
  境界には平坦な値だけを通す
- イベントループ = checkpoint と「op 完了待ちの recv」の交互運転。
  pending カウンタがあればハング検知までできる
- 汎用の op 基盤を先に作れば、Promise.all も新しい op も「タダで」乗る

**システム設計編**
- ブロッキングな JS と非同期な HTTP は物理的にスレッドを分け、
  チャネルだけで会話させる
- テナントをスレッドに固定すれば、並行性バグは「起き得ない構造」になる
- enter/exit、Drop 順、コールバック data の寿命 — unsafe の規約は
  構造体の形と公開 API に閉じ込める
- CPU は再利用・メモリは破棄、世代カウンタで誤爆防止、
  「キャンセル済みの完了報告」は無視 — 失敗系の設計が本体

## 本家 workerd との距離

正直に測っておきます。本プロジェクトが触れなかった主要な領域:

| 領域 | 本家 | 本書 |
|---|---|---|
| ストリーミング | ReadableStream / TransformStream、body は流れる | 全バッファ |
| バインディング | KV, R2, D1, Durable Objects, Queues... | env の文字列変数のみ |
| Wasm | 対応 | 非対応 |
| Spectre 対策 | タイマー粗粒度化、プロセス隔離の併用など多層 | なし（議論のみ） |
| スケジューラ | リクエスト中ロック + 高度なスケジューリング | 1 スレッド 1 リクエストの直列 |
| JS 互換性 | Web 標準への厳密な準拠（WPT） | 「動く範囲」の最小シム |

それでも、**第 2 章で挙げた Isolate モデルの本質 — ミリ秒未満の起動、
高集積、ランタイムが安全性を担う構造 — はすべて手元のコードで再現
できました**。toy の価値は「本物の議論が読めるようになること」です。
いまの読者なら workerd の設計ドキュメントや deno_core のソースが
「知っている話」として読めるはずです。

## 発展課題

実装の続きに興味がある読者向けに、設計のあたりだけ付けておきます。

### 1. 動的デプロイ API（難易度: 低）

`PUT /__admin/workers/{name}` でスクリプトを差し替える。
ルートテーブルを `ArcSwap<RouteTable>` にして無停止で差し替え、
全 worker スレッドに「このテナントの warm isolate を捨てろ」という
`Msg::Invalidate(tenant)` を流すだけ。プールの evict パスがそのまま使える。

### 2. V8 snapshot による cold start 短縮（難易度: 中）

`Isolate::snapshot_creator` で bootstrap 評価済みの Context を blob に
焼き、`CreateParams::snapshot_blob` で復元する。罠は Rust ops の関数
ポインタ: snapshot には関数ポインタを保存できないため、
`external_references` として**作成時と復元時に同一順で**登録する必要が
ある。deno 流の解は「op に触らない純 JS 部分だけを snapshot し、
`__ops` の結線は復元後に行う」二段構成。build.rs で blob を生成して
`include_bytes!` する。

### 3. body ストリーミング（難易度: 高）

JS 側に ReadableStream のシムを書き、op を「pull 型」
（`op_read_chunk() -> Promise<chunk | null>` を繰り返す）に拡張する。
Rust 側は hyper/reqwest の body チャネルと接続。第 8 章の op テーブルは
そのまま使えるが、Body 系クラスの全面書き直しになる。

### 4. 厳密な CPU 時間計測（難易度: 中）

現在の「JS スライスの wall-clock 合計」を、スレッド CPU 時計で置き換える。
Linux なら `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` を worker スレッド
自身がスライス前後で読むだけでよい（watchdog は安全網として残す）。
プリエンプトされた時間が課金されなくなる。

### 5. import 対応（難易度: 中）

第 6 章で述べた ModuleMap 方式。instantiate 前に
`get_module_requests()` で依存を列挙 → 再帰 compile → 登録、
resolve callback はマップ参照のみ。specifier の正規化は `url` crate。
動的 `import()` は `set_host_import_module_dynamically_callback` で
Promise を返し、イベントループでロード完了時に resolve する。

### 6. waitUntil / ctx（難易度: 低〜中）

現在 `ctx` は空オブジェクト。`ctx.waitUntil(promise)` は「レスポンス
返却後もこの Promise の完了까지 isolate を生かす」という意味論で、
イベントループを「target 解決後も waitUntil 群が捌けるまで回す」よう
拡張すれば実装できる。

## おわりに

「Cloudflare Workers のようなもの」は、蓋を開ければ
**V8 という強力なエンジンに、規律あるグルーコードを書く仕事**でした。
イベントループも、リソース制限も、一つひとつは数十行のコードです。
しかしその数十行の置き場所と順序 — どのスレッドで、どの寿命で、
失敗したら誰の責任で — を決めるために、V8 と Rust と OS の境界条件を
すべて理解する必要がありました。

インフラのコードを読む・書く力は、こういう「境界の理解」の積み重ねです。
次は本物を読んでみてください。workerd の `src/workerd/jsg/`、deno_core の
`runtime/`。きっと、知っている顔がたくさん並んでいます。
