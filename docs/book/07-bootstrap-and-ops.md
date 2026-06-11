# Web API シムと ops 設計

素の Context には `console` も `Response` もありません。この章では
worker から見える世界 — Web API のサブセット — を作ります。
ここでの設計判断が、ランタイム全体の形を決めます。

## 設計方針: どこまで JS で書き、どこから Rust にするか

グローバル API の実装には 2 つの選択肢があります。

1. **Rust 側で `ObjectTemplate` / `FunctionTemplate` を組んで生やす**
   — V8 の API でクラスを 1 つ作るのは大量のボイラープレート
2. **JS で書いたブートストラップスクリプトを起動時に評価する**
   — クラスの定義は普通の JS。Rust が関与するのは「JS では書けない部分」だけ

本プロジェクトの線引きはこうです:

> **「I/O・時間・バイト列変換」だけが Rust（ops）、
> それ以外の仕様準拠ロジックは全部 JS（bootstrap.js）**

| 実装場所 | 内容 | 理由 |
|---|---|---|
| bootstrap.js | Headers / Request / Response / Body の意味論、console のフォーマット、fetch の引数正規化 | ただの JS ロジック。JS で書くほうが 1/10 の行数で書けて Web 仕様とも突き合わせやすい |
| Rust ops | `print`（stdout）、`timer`（時間）、`fetch`（ネットワーク）、`encodeUtf8` / `decodeUtf8`（バイト列変換） | JS からは原理的に触れない OS リソース。文字コード変換は正確さと速度のため |

Deno も同じ構造です（`ext/*/js` に大量の JS、ops は I/O だけ）。
**ランタイム = 「ちょっとのネイティブ口 + たくさんの JS」**という構図は
現代の JS ランタイムに共通しています。

## Rust の関数を JS に生やす — ops

### FunctionCallback の形

rusty_v8 で JS から呼べる関数はこの形をしています:

```rust
fn op_print(
    scope: &mut v8::PinScope,            // コールバック用に復元されたスコープ
    args: v8::FunctionCallbackArguments, // JS 側の引数
    _rv: v8::ReturnValue,                // JS への戻り値を書く場所
) {
    let text = args.get(0)
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_default();
    print!("{text}");
    std::io::stdout().flush().ok();
}
```

- `args.get(i)` は範囲外でも `undefined` を返す（JS の関数呼び出しと同じ）
- 戻り値は `rv.set(value)` で書く。書かなければ `undefined`
- ここでも**キャプチャ付きクロージャは不可**。状態は slot から取る

### __ops オブジェクトに束ねて生やす

関数を 1 つずつ `globalThis` に生やすのではなく、`__ops` という 1 つの
オブジェクトに束ねます。後で**まとめて隠す**ためです。

```rust
// crates/runtime/src/ops.rs（抜粋）
pub fn install_ops(scope: &mut v8::PinScope) {
    let context = scope.get_current_context();
    let global = context.global(scope);
    let ops = v8::Object::new(scope);

    set_fn(scope, ops, "print", op_print);
    set_fn(scope, ops, "timer", op_timer);
    set_fn(scope, ops, "fetch", op_fetch);
    set_fn(scope, ops, "encodeUtf8", op_encode_utf8);
    set_fn(scope, ops, "decodeUtf8", op_decode_utf8);

    let key = v8::String::new(scope, "__ops").unwrap();
    global.set(scope, key.into(), ops.into());
}

fn set_fn(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    name: &str,
    callback: impl v8::MapFnTo<v8::FunctionCallback>,
) {
    let func = v8::Function::new(scope, callback).unwrap();
    let key = v8::String::new(scope, name).unwrap();
    obj.set(scope, key.into(), func.into());
}
```

## bootstrap.js — IIFE で受け取り、評価後に消す

bootstrap.js は `include_str!` で Rust バイナリに埋め込み、Context 作成
直後（ユーザーコードより前）に classic script として評価します。
全体は 1 つの IIFE です。

```js
// crates/runtime/js/bootstrap.js（骨格）
(({ print, timer, fetch: opFetch, encodeUtf8, decodeUtf8 }) => {
  class Headers { /* ... */ }
  class Body { /* ... */ }
  class Request extends Body { /* ... */ }
  class Response extends Body { /* ... */ }
  class TextEncoder { /* ... */ }
  class TextDecoder { /* ... */ }
  const console = { log: (...a) => print(a.map(inspect).join(" ") + "\n"), ... };
  const setTimeout = (cb, ms = 0, ...args) => {
    timer(Math.max(0, Number(ms))).then(() => cb(...args));
  };
  const fetch = async (input, init) => { /* 第 15 章 */ };

  // ランタイム内部用フック（Rust が取り出して delete する）
  globalThis.__runtime = {
    buildRequest: (url, method, headersArr, bodyAb) =>
      new Request(url, { method, headers: headersArr, body: bodyAb }),
    serializeResponse: async (resp) => {
      if (!(resp instanceof Response)) {
        throw new TypeError("fetch handler must return a Response");
      }
      const body = resp.bodyUsed ? new ArrayBuffer(0) : await resp.arrayBuffer();
      return [resp.status, [...resp.headers], body];
    },
  };

  Object.assign(globalThis, {
    Headers, Request, Response, TextEncoder, TextDecoder,
    console, setTimeout, fetch,
  });
})(globalThis.__ops);
delete globalThis.__ops;     // ← ユーザーコードから ops を隠す
```

### 隠蔽の 2 段構え

worker のユーザーコードに生のネイティブ関数を触らせたくありません
（`__ops.print` を直接呼ばれるのは行儀の問題ですが、将来 op が増えたとき
の攻撃面になります）。そこで:

1. **`__ops`**: IIFE の引数として bootstrap に渡したら、最後の行で
   `delete globalThis.__ops`。bootstrap のクラスたちはクロージャ変数として
   ops を握り続けるが、グローバルからは見えない
2. **`__runtime`**: Rust 側が評価直後に `buildRequest` /
   `serializeResponse` を `Global<Function>` として取り出し、その後
   `global.delete()` する

```rust
// crates/runtime/src/worker.rs（抜粋）
fn take_runtime_helpers(scope: &mut v8::PinScope)
    -> Result<(v8::Global<v8::Function>, v8::Global<v8::Function>), WorkerError>
{
    let context = scope.get_current_context();
    let global = context.global(scope);

    let runtime_key = v8::String::new(scope, "__runtime").unwrap();
    let runtime = global.get(scope, runtime_key.into())
        .and_then(|v| v8::Local::<v8::Object>::try_from(v).ok())
        .ok_or_else(|| WorkerError::Internal("__runtime not found after bootstrap".into()))?;
    // buildRequest / serializeResponse を Global 化 ...
    global.delete(scope, runtime_key.into());   // ユーザーコードから不可視に
    Ok((build_request, serialize_response))
}
```

評価順は厳密にこうなります:

```
Context 作成
 → install_ops()            … __ops を生やす
 → bootstrap.js を評価       … Web API 定義、__runtime を生やす、__ops を消す
 → __runtime を回収して消す
 → ユーザーモジュールを評価  … この時点でグローバルは Web API だけ
```

## Web API シムの中身 — 仕様の「気持ち」を最小実装する

完全な Fetch 標準（WHATWG）の実装は壮大なので、worker を書くのに必要な
意味論だけを抜き出します。それでも学びの多いポイントがいくつかあります。

### Headers — 大文字小文字の正規化と append

HTTP ヘッダ名は大文字小文字を区別しません。`Map` をラップして名前を
lowercase に正規化し、`append` は同名ヘッダをカンマ結合します
（Fetch 標準の挙動）。

```js
class Headers {
  #map = new Map();
  append(name, value) {
    name = normalizeName(name);
    const cur = this.#map.get(name);
    this.#map.set(name, cur === undefined ? String(value) : cur + ", " + value);
  }
  get(name) { return this.#map.get(normalizeName(name)) ?? null; }
  *[Symbol.iterator]() { yield* this.#map.entries(); }
  // set / has / delete / entries / keys / values / forEach ...
}
```

`[Symbol.iterator]` を実装してあるので `[...headers]` や
`Object.fromEntries(headers)` がそのまま動きます。

### Body — 「一度しか読めない」の再現

`Request` / `Response` の body は **一度読んだら使用済み**になります
（ストリームだから）。本実装の body はただの `Uint8Array` ですが、
`bodyUsed` の意味論は再現します。`text()` / `json()` / `arrayBuffer()` が
**async である**ことも踏襲します — 中身は即値で resolve するだけですが、
インターフェースを本物と揃えておけば worker のコードはそのまま本家でも
動きます。

```js
class Body {
  #bytes; #used = false;
  #consume() {
    if (this.#used) throw new TypeError("body already used");
    this.#used = true;
    return this.#bytes ?? new Uint8Array(0);
  }
  async arrayBuffer() { const b = this.#consume(); return b.buffer.slice(...); }
  async text() { return decodeUtf8(await this.arrayBuffer()); }
  async json() { return JSON.parse(await this.text()); }
}
```

### setTimeout — op との合成

`setTimeout` は「`timer(ms)`（Promise を返す Rust op）に `.then` を繋ぐ
だけ」の薄いラッパです。タイマー ID と `clearTimeout` は未対応
（toy の割り切り）。op 側の仕組みは第 8 章・第 15 章で詳述します。

## バイト列の往復 — ArrayBuffer と `Vec<u8>`

ops の引数・戻り値でバイナリを渡すための変換ヘルパを `ops.rs` に
用意しました。方向によって手段が違います。

**Rust → JS（コピーなし）**: `Vec<u8>` の所有権を BackingStore に移譲して
ArrayBuffer 化。ゼロコピーです。

```rust
pub fn bytes_to_array_buffer<'s>(
    scope: &v8::PinScope<'s, '_>,
    bytes: Vec<u8>,
) -> v8::Local<'s, v8::ArrayBuffer> {
    let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
    v8::ArrayBuffer::with_backing_store(scope, &store)
}
```

**JS → Rust（コピーあり）**: ArrayBuffer（または TypedArray）の中身を
`Vec<u8>` にコピー。JS 側がいつ書き換えるか分からないので、所有権を
明確にするためコピーします。

```rust
pub fn value_to_bytes(value: v8::Local<v8::Value>) -> Option<Vec<u8>> {
    if let Ok(buffer) = v8::Local::<v8::ArrayBuffer>::try_from(value) {
        // BackingStore の生ポインタから copy_nonoverlapping
    } else if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(value) {
        // view.copy_contents(&mut bytes)
    } else { None }
}
```

UTF-8 変換（`encodeUtf8` / `decodeUtf8`）を JS で書かず Rust op にしたのも
同じ理由です。V8 の `String` は内部表現が UTF-16/Latin-1 で、正しい UTF-8
変換を JS で手書きするのは間違いやすく遅い。`to_rust_string_lossy` と
`String::from_utf8_lossy` に任せるのが正確です。

## ハマりどころ: "use strict" と IIFE

最初の bootstrap.js は冒頭にこう書いていました:

```js
(({ print, timer, encodeUtf8, decodeUtf8 }) => {
  "use strict";   // ❌ SyntaxError!
```

これは **`SyntaxError: Illegal 'use strict' directive in function with
non-simple parameter list`** になります。分割代入を引数に持つ関数では
`"use strict"` ディレクティブが書けない、という ES2016 からの仕様です。
普段 module（常に strict）で書いていると忘れている罠でした。
classic script として評価する bootstrap ならではの一撃です。

## この章のまとめ

- ランタイム = 少数の Rust ops + 大半は JS（bootstrap.js）。線引きは
  「I/O・時間・バイト列変換だけ Rust」
- ops は `__ops` に束ねて渡し、`delete` で隠す。内部ヘルパも
  `Global<Function>` として回収後に `delete`
- Web API は意味論（ヘッダ正規化・bodyUsed・async インターフェース）を
  最小実装。インターフェースを本物と揃えれば worker コードに互換性が出る
- バイナリは Rust→JS はゼロコピー、JS→Rust はコピー

これで worker の世界に API が生えました。しかし `async fetch` の Promise を
解決する仕組みがまだありません。次章、いよいよイベントループです。
