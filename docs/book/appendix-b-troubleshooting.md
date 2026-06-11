# 付録 B: ハマりどころ集

開発中に実際に踏んだ・設計で先回りした罠のカタログです。
症状から引けるように書いています。

## ビルド・環境

### 古い記事のコードがコンパイルできない

**症状**: `HandleScope::new(&mut isolate)` が型エラー、
`the method exists but its trait bounds were not satisfied` など。

**原因**: rusty_v8 のスコープ API は Pin + マクロ方式に全面改訂された。
Web 上のサンプル（〜2023 年頃）はほぼ全滅。

**対処**: `v8::scope!` / `tc_scope!` / `callback_scope!` を使う（第 5 章）。
迷ったら `~/.cargo/registry/src/*/v8-*/examples/process.rs` が正解集。

### TryCatch を受け取るヘルパ関数の型が書けない

**症状**: `tc` を関数に渡したいが、引数型
`&mut PinnedRef<TryCatch<HandleScope<...>>>` のトレイト境界が解決できない。

**対処**: 型を書かずに済む**マクロ**にする（`exception_message!`）。
あるいは tc から `exception()` で値だけ取り出し、`&mut v8::PinScope` を
取る関数に値を渡す。「型が書けない場所だけマクロ」が現実解（第 5 章）。

### v8 のダウンロード/リンクが遅い

- 初回ビルドは static lib（数十 MB）のダウンロードが走る。
  プロキシ環境では `RUSTY_V8_MIRROR` / `RUSTY_V8_ARCHIVE` を使う
- v8 依存は 1 crate（runtime）に閉じ込め、`[profile.dev] debug = 1` で
  リンクを軽くする（workspace 直下の Cargo.toml 参照）

## JS・bootstrap

### SyntaxError: Illegal 'use strict' directive...

**症状**: bootstrap.js の評価が
`Illegal 'use strict' directive in function with non-simple parameter list`。

**原因**: 分割代入引数を持つ関数には `"use strict"` を書けない（ES2016）。
`(({ print, timer }) => { "use strict"; ... })` がまさにこれ。

**対処**: ディレクティブを消す（または引数を単純にする）。
ESM は常時 strict だが、bootstrap は classic script なので踏む。

### export を含むスクリプトが SyntaxError

**原因**: `v8::Script::compile`（classic script）に ESM を渡している。

**対処**: `ScriptOrigin` の `is_module: true` +
`script_compiler::compile_module`（第 6 章）。

### obj.get() が None にならない

**症状**: 存在しないプロパティの検出が効かない。

**原因**: `get` は無いプロパティでも `Some(undefined)` を返す。
`None` は「例外発生」の意味。

**対処**: `.filter(|v| !v.is_undefined())` を挟む（第 6 章）。

## ライフタイム・寿命

### スコープを抜けた Local を使ってしまう設計になる

**対処**: ヘルパ関数間の受け渡しは全部 `v8::Global` に統一する。
ライフタイムパズルを解くより安くて安全（第 5 章）。

### drop 順のクラッシュ

**症状**: Worker の drop でクラッシュ／assert。

**原因と対処**（第 9・11・14 章）:
1. `Global` は Isolate より先に drop する → 構造体で Global 群を
   isolate フィールドより**上**に宣言
2. `OwnedIsolate` の drop はカレントであることを要求 → park 方式なら
   `Drop for Worker` で `enter()` してから
3. `near_heap_limit_callback` の data は Isolate より長生き →
   `Box<MemCtx>` を isolate フィールドより**下**に宣言

### RefCell の二重借用パニック

**症状**: `already borrowed: BorrowMutError`（イベントループ内）。

**原因**: op の Receiver を `RefCell<OpState>` の中に入れると、
`recv` でブロック中も borrow が生き、完了処理の `borrow_mut` と衝突。

**対処**: 「待つもの（rx）」は RefCell の外（Worker のフィールド）に
出す（第 8 章）。

## 実行時

### worker が無限ループするとサーバ全体が固まる

**原因**: JS を tokio ワーカスレッドで実行している。

**対処**: JS は専用 std::thread + チャネル（第 10 章）。
それでも該当スレッドは占有されるので watchdog（第 13 章）を入れる。

### terminate が次のリクエストを誤爆する

**症状**: 制限内で完了したのに、直後のリクエストが Terminated になる。

**原因**: Disarm がチャネル配達中に watchdog が発火する競合。

**対処**: 世代カウンタ。Disarm は世代一致時のみ有効（第 13 章）。
併せて、リクエスト開始時に `is_execution_terminating` を見て
`cancel_terminate_execution` で掃除するのも防御になる。

### ヒープ上限でプロセスごと落ちる

**症状**: `Fatal JavaScript out of memory` でプロセス abort。

**原因**: heap_limits を設定しただけ、または near_heap_limit_callback で
上限を据え置いた（巻き戻り中の割り当てで再ヒット）。

**対処**: コールバックで terminate を要求しつつ
**上限を +8MiB して返す**（第 14 章）。

### 遅れて届く op 完了で panic / 誤動作

**症状**: タイムアウト後・terminate 後に古い fetch の結果が届いて壊れる。

**対処**: 完了通知は「resolver テーブルに op_id が無ければ無視」。
abort 時はテーブルと pending カウンタをクリアするだけでよい（第 8 章）。

## 運用・デバッグ

### Address already in use (os error 48)

**症状**: テスト中、ルーティングが古い・404 が返る・設定が反映されない。

**原因**: 前回のサーバプロセスが残っていて、新プロセスは bind に失敗して
いた（そして curl は**古いプロセス**に繋がっていた）。開発中に実際に
これで 10 分溶かした。

**対処**: `pkill -f "target/debug/tew"` してから起動。
「設定が反映されない」と思ったらまずポートの主を `lsof -i :8787` で確認。

### Host ルーティングをローカルで試したい

DNS をいじる必要はない。`curl -H "Host: hello.localhost" http://127.0.0.1:8787/`
で十分（router はポートを除去して比較する）。
