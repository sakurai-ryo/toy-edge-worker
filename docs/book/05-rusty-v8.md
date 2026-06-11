# rusty_v8 の現行 API と M0: eval

この章では rusty_v8（crate `v8`）の現行 API の読み書きを身につけ、
最初のマイルストーン **M0: `tew eval "1+2*3"` → `7`** を完成させます。

## 最重要の注意: スコープ API は全面改訂されている

Web 上の rusty_v8 の記事（そして古い deno_core のコード）には、
次のようなコードが大量にあります。

```rust
// ❌ 古い API。現行バージョンではコンパイルできない
let scope = &mut v8::HandleScope::new(&mut isolate);
let context = v8::Context::new(scope);
let scope = &mut v8::ContextScope::new(scope, context);
```

現行（本書の v8 = "149" 系）では、スコープは **`Pin` で固定してから
初期化する**方式に変わり、それを隠すための**マクロ**が提供されています。

```rust
// ✅ 現行 API
v8::scope!(let scope, &mut isolate);              // HandleScope
let context = v8::Context::new(scope, Default::default());
let scope = &mut v8::ContextScope::new(scope, context);
v8::tc_scope!(let tc, scope);                     // TryCatch スコープ
```

なぜこうなったのか。V8 のスコープは「**必ずスタック上にあり、作られた逆順で
壊される**」ことを前提に実装されています（スコープ同士が親へのポインタを
持つ）。ムーブが起きると壊れるため、Rust 側では `Pin` で「この値はもう
動かない」ことを保証してから V8 に登録する必要があります。`v8::scope!`
マクロの中身は概ね次と等価です:

```rust
// v8::scope!(let scope, &mut isolate) の展開イメージ
let mut scope = v8::HandleScope::new(&mut isolate); // 未初期化のスコープ
let mut scope = {
    let pinned = unsafe { std::pin::Pin::new_unchecked(&mut scope) };
    pinned.init()                                   // ピン留めしてから V8 に登録
};
let scope = &mut scope;                             // &mut PinnedRef<HandleScope>
```

### スコープの型と渡し方

マクロが束縛する `scope` の型は `&mut v8::PinScope<'s, 'i, C>`
（実体は `PinnedRef<HandleScope>`）です。覚えるべき実用ルールは 3 つ:

1. **関数にスコープを渡すときの型は `&mut v8::PinScope`**。
   `C`（Context の有無）はデフォルトで `Context` なので、Context 入りの
   スコープを受け取る関数はこれで書ける

   ```rust
   fn my_helper(scope: &mut v8::PinScope) -> ... { ... }
   ```

2. **TryCatch スコープ（`tc`）はそのまま `PinScope` として渡せる**。
   `Deref` 実装があるため、`v8::Script::compile(tc, ...)` のように
   スコープを要求する API に `tc` を直接渡せる

3. **スコープを跨ぐ値は `Global` にする**。`tc_scope!` のブロック内で
   作った Local を外に持ち出そうとするとライフタイムで詰むことがある。
   本プロジェクトでは「**ヘルパ関数間の受け渡しはすべて `v8::Global`**」
   という割り切りで統一した（コストは無視できる。複雑なライフタイム
   パズルを解く時間のほうが高い）

主要マクロ早見:

| マクロ | 作るもの | 用途 |
|---|---|---|
| `v8::scope!(let s, isolate)` | HandleScope | Local の台帳 |
| `v8::scope!(let s, ctx_scope)` | HandleScope | ContextScope から Context 付きスコープを得る |
| `v8::tc_scope!(let tc, s)` | TryCatch | JS 例外を拾う |
| `v8::callback_scope!(unsafe s, context)` | CallbackScope | V8 からのコールバック内でスコープを復元（第 6 章） |
| `v8::scope_with_context!(let s, isolate, global_ctx)` | HandleScope+ContextScope | `Global<Context>` から一発で |

一次資料はリポジトリ同梱の例です。何か迷ったら必ずここに戻ります:
`~/.cargo/registry/src/*/v8-149.*/examples/{hello_world.rs, process.rs}`

## M0 を作る

### workspace

```toml
# Cargo.toml（抜粋）
[workspace]
members = ["crates/edged", "crates/runtime", "crates/pool", "crates/router", "crates/protocol"]

[workspace.dependencies]
v8 = "149"          # 4 週ごとにメジャーが上がるため固定必須

[profile.dev]
debug = 1           # v8 のリンクが重いのでデバッグ情報を抑える
```

v8 crate は初回ビルド時に **ビルド済み静的ライブラリ（数十 MB）を GitHub
Releases から自動ダウンロード**します。macOS arm64 / Linux x64 / aarch64
などはこれで済み、V8 のソースビルドは不要です。

### プラットフォーム初期化 — プロセスで 1 回だけ

V8 を使う前に、プロセス全体で 1 回だけプラットフォーム（スレッドプール等）
を初期化します。2 回呼ぶと落ちるので `Once` で守ります。

```rust
// crates/runtime/src/init.rs
use std::sync::Once;

static INIT: Once = Once::new();

pub fn init() {
    INIT.call_once(|| {
        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
    });
}
```

### eval の実装

第 4 章の概念を総動員した、最小の「JS 実行」です。

```rust
// crates/runtime/src/lib.rs（抜粋）
pub fn eval(code: &str) -> Result<String> {
    init();

    // 1. Isolate を作る（new した時点でこのスレッドに enter される）
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());

    // 2. HandleScope → Context → ContextScope → TryCatch を積む
    v8::scope!(let scope, &mut isolate);
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    v8::tc_scope!(let tc, scope);

    // 3. ソース文字列を JS の String に → コンパイル → 実行
    let source = v8::String::new(tc, code)
        .ok_or_else(|| anyhow!("source too long"))?;
    let Some(script) = v8::Script::compile(tc, source, None) else {
        return Err(anyhow!("compile error: {}", exception_message!(tc)));
    };
    let Some(result) = script.run(tc) else {
        return Err(anyhow!("runtime error: {}", exception_message!(tc)));
    };

    // 4. 結果を Rust の String へ
    let result = result.to_string(tc)
        .ok_or_else(|| anyhow!("failed to stringify result"))?;
    Ok(result.to_rust_string_lossy(tc))
}
```

読みどころ:

- **失敗はすべて `None`**。`compile` も `run` も、JS 例外が起きると `None`
  を返し、例外値は TryCatch（`tc`）に入る。Rust の `?` 文化と違い
  「戻り値が空 + サイドチャネルに例外」という V8 流のエラー伝達に慣れる
  必要がある
- `v8::String::new` が `Option` なのは「長すぎる文字列」で失敗しうるから
- `to_rust_string_lossy` で V8 の String（UTF-16 ベース）を Rust の
  String（UTF-8）に変換する

### 例外メッセージの取り出しとマクロ

`tc.exception()` は例外値（多くは `Error` オブジェクト）を返します。
ここで 1 つ Rust 上の問題に当たります。`tc_scope!` が束縛する値の完全な型は

```text
&mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_, v8::Context>>>
```

のような多段ジェネリクスで、**ヘルパ関数の引数型として書くのが極めて
つらい**。ジェネリクスで受けようとするとトレイト境界のパズルになります
（実際に最初の実装はコンパイルエラーになりました）。

解決は「関数ではなくマクロにする」です。マクロは型を書かなくてよいので:

```rust
macro_rules! exception_message {
    ($tc:expr) => {
        match $tc.exception() {
            Some(exception) => $crate::worker::format_exception($tc, exception),
            None => "unknown error".to_string(),
        }
    };
}
```

`format_exception`（例外値 → 人間可読文字列。`Error.stack` があれば優先）は
`&mut v8::PinScope` を取る普通の関数で、`$tc` は Deref 強制で渡ります。
「**型が書けないところだけマクロ、書けるところは関数**」が rusty_v8 と
付き合う現実解です。

### 動かす

```sh
$ cargo run -p edged -- eval "1+2*3"
7
$ cargo run -p edged -- eval "[...Array(5)].map((_, i) => i * i).join(',')"
0,1,4,9,16
$ cargo run -p edged -- eval "nonexistent()"
Error: runtime error: ReferenceError: nonexistent is not defined
```

3 つ目の例で、TryCatch 経由のエラー報告が機能していることも確認できます。

## この章のまとめ

- rusty_v8 のスコープは `Pin` ベース + マクロの現行 API で書く。
  古い記事のコードは動かない
- スコープを取る関数は `&mut v8::PinScope`、スコープを跨ぐ値は `Global`
- V8 のエラーは「`None` + TryCatch」で受ける
- 型が書けない場所はマクロで逃がす

ここまでで「JS 文字列を 1 回実行する」ができました。次章から、これを
「worker スクリプトをロードして fetch ハンドラを呼べる」ランタイムに
育てていきます。
