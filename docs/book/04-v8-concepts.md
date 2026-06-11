# V8 埋め込みの基礎概念

この章は座学です。コードを書く前に、V8 を埋め込むうえで避けて通れない
5 つの概念 — **Isolate / Context / Handle / HandleScope / microtask** —
を整理します。ここを曖昧にしたまま進むと、後の章のライフタイムや
`unsafe` の意味が分からなくなります。

## Isolate — JS の宇宙、ヒープの単位

```
┌─ Isolate ────────────────────────────────┐
│  ヒープ（GC される JS オブジェクトの住処）  │
│  ┌─ Context A ─────┐ ┌─ Context B ─────┐ │
│  │ globalThis      │ │ globalThis      │ │
│  │ Object, Array.. │ │ Object, Array.. │ │
│  └─────────────────┘ └─────────────────┘ │
│  microtask queue / pending exceptions /  │
│  埋め込み側 slot（後述）...               │
└──────────────────────────────────────────┘
```

- Isolate は**ヒープと GC の単位**。Isolate を破棄すれば、その中の
  すべてのオブジェクトがまとめて消える（個別の後始末は不要）
- **スレッド規律**: 1 つの Isolate に同時に触れるのは 1 スレッドだけ。
  V8 はこれを「現在このスレッドに enter されている Isolate」という
  スレッドローカルなスタックで管理する。複数スレッドから同時に触ると
  即クラッシュではなく**未定義動作**（運が良ければ DCHECK で落ちる）
- 生成はそれなりに軽い（本プロジェクトの実測で 1ms 未満）が、
  タダではない。だからプール（第 12 章）に意味がある

rusty_v8 では `v8::Isolate::new(params)` が `v8::OwnedIsolate` を返します。
これは「所有権を持つ Isolate」で、drop すると V8 側の Isolate が破棄されます。

## Context — globalThis の単位

Context は「グローバルオブジェクト一式」です。`Object` や `Array` といった
組み込みも Context ごとに別インスタンスです（だから iframe 間で
`instanceof` が効かない、という Web 開発の小ネタはここに由来します）。

本プロジェクトは 1 Isolate に 1 Context しか作りませんが、概念としては
分かれていることを覚えておいてください。多くの V8 API は「現在の Context」
を要求し、rusty_v8 ではそれが `ContextScope` で表現されます。

## Handle と GC — なぜ生ポインタではだめか

V8 の GC は **moving GC** です。生きているオブジェクトをヒープ内で
移動（コンパクション）させます。つまり JS オブジェクトへの生ポインタを
持っていると、GC のあとには別の場所を指すゴミになります。

そこで V8 は**ハンドル**という間接参照を使います。GC はハンドルが指す先を
知っていて、オブジェクトを動かすときにハンドルの中身を書き換えてくれます。
ハンドルには 2 種類あります。

### `Local<T>` — スコープに紐づく短命ハンドル

`v8::Local<'s, T>` は「現在の HandleScope が生きている間だけ有効」な
ハンドルです。Rust ではライフタイム `'s` で表現されます。

### HandleScope — Local をまとめて無効化する仕組み

HandleScope は Local の「台帳」です。スコープを抜けると、そのスコープで
作られた Local は一括で無効になり、GC はそれらを根（root）とみなさなく
なります。C++ では:

```cpp
{
  v8::HandleScope scope(isolate);
  v8::Local<v8::String> s = v8::String::NewFromUtf8(...);
  // scope が生きている間だけ s は有効
}  // ← ここで s は無効。以後使ったら未定義動作
```

C++ ではこの規約はプログラマの注意力だけで守られています。Rust
（rusty_v8）では、`Local<'s, T>` が `'s` でスコープに縛られているため、
**スコープより長生きする Local はコンパイルエラー**になります。
V8 埋め込みを Rust でやる最大のうまみがここです。

### `Global<T>` — スコープを越えて保持する長命ハンドル

「リクエストをまたいで fetch ハンドラの Function を持っておきたい」など、
スコープより長く保持したいときは `v8::Global<T>` を使います。

- `v8::Global::new(scope, local)` で Local から作る
- Global は **GC ルート**になる。持っている限りそのオブジェクトは
  回収されない（= 持ちすぎ・消し忘れはメモリリーク）
- 使うときは `v8::Local::new(scope, &global)` で現在のスコープの
  Local に「降ろして」から使う

本プロジェクトでの使い分けの実例:

| 保持するもの | 種類 | 理由 |
|---|---|---|
| 関数の引数・戻り値・一時値 | `Local` | その場で使い捨て |
| Context（リクエスト間で保持） | `Global<Context>` | Worker 構造体に置く |
| fetch ハンドラの Function | `Global<Function>` | 〃 |
| 非同期 op の PromiseResolver | `Global<PromiseResolver>` | op 完了（別ターン）まで生かす |

## 例外と TryCatch

JS の `throw` は V8 内部では「pending exception をセットして特別な値を
返す」操作です。埋め込み API では「失敗 = 空の戻り値（rusty_v8 では
`None`）+ pending exception」という形で現れます。

例外を拾うには **TryCatch スコープ**を張ります。スコープ内で起きた例外は
TryCatch に捕まり、`exception()` で例外値を取り出せます。捕まえなければ
外側へ伝播します。

```rust
v8::tc_scope!(let tc, scope);
let result = some_js_call(tc);     // None が返ったら…
if result.is_none() {
    let exc = tc.exception();      // 例外値（Error オブジェクト等）を取得
}
```

注意: **`TerminateExecution`（第 13 章）による強制終了も「例外っぽい中断」
として現れます**が、これは catch できない特殊な状態です。TryCatch では
`tc.has_terminated()` で区別します。JS 側の `try {} catch {}` でも
握り潰せないので、無限ループを `try` で包んでも逃げられません。

## microtask — await の正体

`Promise.then` や `await` の続き（継続）は、**microtask キュー**に積まれます。
ブラウザや Node.js では「現在の実行が終わったら自動的に flush」されますが、
**素の V8 では、いつ flush するかは埋め込み側が決めます**。

```rust
isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
// ...
scope.perform_microtask_checkpoint();  // ここで初めて .then の中身が走る
```

`MicrotasksPolicy` には Auto（JS 呼び出しから戻るたびに自動 flush）も
ありますが、本プロジェクトは **Explicit** を選びます。イベントループ
（第 8 章）が「いつ JS が走るか」を完全に制御するためです。これは
deno_core と同じ選択です。

重要な帰結: `resolver.resolve(value)` を呼んでも、**その場では何も
起きません**。`await` の続きが走るのは、次に `perform_microtask_checkpoint()`
を呼んだときです。「resolve = 予約、checkpoint = 実行」というリズムが
イベントループの基本拍になります。

## Isolate の slot — 埋め込み側のデータ置き場

V8 のコールバック（JS から呼ばれる Rust 関数）は、自由な引数を取れません。
「この Isolate に紐づく Rust 側の状態」をコールバックから取り出すために、
Isolate には任意の型の値を 1 つずつ格納できる **slot** があります。

```rust
isolate.set_slot(Rc::new(RefCell::new(OpState::new(...))));
// コールバック内で:
let state = scope.get_slot::<Rc<RefCell<OpState>>>().unwrap().clone();
```

`Rc<RefCell<...>>` なのは、worker スレッド内でしか触らない（`Send` 不要）
一方、コールバックとイベントループの双方から可変アクセスしたいからです。
deno_core の `OpState` も同じ構造をしています。

## この章のまとめ

- Isolate = ヒープと GC の単位。1 スレッドからしか触れない
- Context = globalThis の単位
- Local はスコープ縛りの短命ハンドル、Global はスコープを越える長命ハンドル
  （= GC ルート）。Rust ではこの規約がライフタイムで強制される
- 例外は TryCatch で拾う。terminate は catch 不能
- microtask の flush タイミングは埋め込み側が握れる（Explicit）
- slot で Rust 側の状態をコールバックに渡す

次章で、これらが rusty_v8 の実際の API でどう書けるかを見ます。
