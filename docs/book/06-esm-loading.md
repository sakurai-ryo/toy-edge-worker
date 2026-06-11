# ES Modules をロードする

M1 の前半戦です。worker スクリプトは ES Modules（ESM）形式なので、
`v8::Script`（classic script）ではなく `v8::Module` の API でロードします。
ESM のロードは **compile → instantiate → evaluate** の 3 段階で、
classic script より一手間多い世界です。

## なぜ Script ではだめか

```js
export default { async fetch(request) { ... } };
```

`export` は classic script では構文エラーです。`v8::Script::compile` に
このソースを渡すと `SyntaxError: Unexpected token 'export'` になります。
ESM としてコンパイルするには `ScriptOrigin` で **`is_module: true`** を
立てて `script_compiler::compile_module` を使います。

ESM を選んだ理由（Service Worker 形式との比較）:

- 現行 Cloudflare Workers の標準形式である
- グローバル汚染がない（ハンドラは export で渡す）
- module namespace / TLA（top-level await）など、V8 のモジュール機構を
  学べる — まさに本プロジェクトの目的

## 3 段階のライフサイクル

```
ソース文字列
  │ compile_module      … パースして Module オブジェクトに
  ▼
Module (Uninstantiated)
  │ instantiate_module  … import/export を解決して束縛を作る
  │                       ← ここで resolve callback が呼ばれる
  ▼
Module (Instantiated)
  │ evaluate            … トップレベルコードを実行
  ▼
Module (Evaluated)  →  get_module_namespace() で export を取り出せる
```

### compile — is_module が肝

```rust
// crates/runtime/src/worker.rs（抜粋）
fn compile_and_instantiate(
    scope: &mut v8::PinScope,
    code: &str,
    url: &str,
) -> Result<v8::Global<v8::Module>, WorkerError> {
    v8::tc_scope!(let tc, scope);
    let source_str = v8::String::new(tc, code)
        .ok_or_else(|| WorkerError::Internal("script too long".into()))?;
    let name: v8::Local<v8::Value> = v8::String::new(tc, url).unwrap().into();
    let origin = v8::ScriptOrigin::new(
        tc, name, 0, 0, false, 0, None, false, false,
        /* is_module */ true, None,
    );
    let mut source = v8::script_compiler::Source::new(source_str, Some(&origin));

    let Some(module) = v8::script_compiler::compile_module(tc, &mut source) else {
        return Err(WorkerError::Script(exception_message!(tc)));
    };
    if module.instantiate_module(tc, resolve_module_callback).is_none() {
        return Err(WorkerError::Script(exception_message!(tc)));
    }
    Ok(v8::Global::new(tc, module))
}
```

`ScriptOrigin` の `resource_name`（ここでは `file:///...` の URL）は
スタックトレースに出る「ファイル名」になります。エラーメッセージの
品質に直結するので、ちゃんとした URL を入れる価値があります。

```
Error: script error: Error: boom
    at fetch (file:///private/tmp/err_test.js:2:25)   ← これ
```

### instantiate と resolve callback — import の解決点

`instantiate_module` には **resolve callback** を渡します。モジュールが
`import ... from "specifier"` を含む場合、V8 は specifier ごとにこの
コールバックを呼び、「対応する Module をくれ」と要求してきます。

このコールバックには rusty_v8 特有の制約が 2 つあります。

```rust
fn resolve_module_callback<'s>(
    context: v8::Local<'s, v8::Context>,
    specifier: v8::Local<'s, v8::String>,
    _import_attributes: v8::Local<'s, v8::FixedArray>,
    _referrer: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Module>> {
    // SAFETY: V8 のモジュール解決中に呼ばれるため context は有効
    v8::callback_scope!(unsafe scope, context);
    let specifier = specifier.to_rust_string_lossy(scope);
    let message = v8::String::new(
        scope, &format!("import is not supported yet: {specifier}")).unwrap();
    let exception = v8::Exception::error(scope, message);
    scope.throw_exception(exception);
    None
}
```

1. **スコープが引数で渡ってこない。** V8 の C++ コールバックの生の形が
   そのまま出ている。ハンドルを作るには `v8::callback_scope!` で
   「いま V8 のコールバック中である」ことを表明してスコープを復元する。
   `unsafe` なのは「本当にコールバック中である」ことをコンパイラが
   検証できないから
2. **キャプチャ付きクロージャを渡せない**（関数ポインタに変換される
   ため）。コールバックに状態を渡したければ、Isolate の slot や
   Context 経由にするしかない

本プロジェクトの worker は単一ファイルなので、**import は「未対応」として
例外を投げる**実装にしました。throw して `None` を返すと、
`instantiate_module` 全体が `None` になり、呼び出し元の TryCatch に
例外が渡ります。

> **発展: import を本当に解決するには**
> deno_core の ModuleMap 方式が参考になります。(1) instantiate の**前に**
> 依存グラフを再帰的に辿り（`module.get_module_requests()` で specifier を
> 列挙）、すべて compile して `HashMap<解決済みURL, Global<Module>>` に登録、
> (2) resolve callback は**そのマップを引くだけ**の同期処理にする。
> 「コールバック内で I/O しない」が設計の肝です。動的 `import()` は
> さらに `set_host_import_module_dynamically_callback` で Promise を
> 返す形になります（第 16 章）。

## evaluate と top-level await

ここに現代 V8 の重要な仕様があります:
**`module.evaluate()` の戻り値は常に Promise** です（TLA を使っていなくても）。

- TLA がない場合: 即座に Fulfilled な Promise が返る
- TLA がある場合: トップレベルの await がすべて解決するまで Pending

つまり「モジュールの評価完了を待つ」こと自体が**イベントループを要求**
します。本プロジェクトでは evaluate の直後に第 8 章のイベントループで
この Promise の解決を待ちます。

```rust
fn evaluate_module(
    scope: &mut v8::PinScope,
    op_rx: &mpsc::Receiver<LoopEvent>,
    module: &v8::Global<v8::Module>,
    timeout: Duration,
    meter: &mut CpuMeter,
) -> Result<(), WorkerError> {
    let deadline = Instant::now() + timeout;
    let promise = {
        v8::tc_scope!(let tc, scope);
        let module = v8::Local::new(tc, module);
        let Some(value) = meter.run(|| module.evaluate(tc)) else {
            if tc.has_terminated() {
                return Err(WorkerError::Terminated);
            }
            return Err(WorkerError::Script(exception_message!(tc)));
        };
        let promise = v8::Local::<v8::Promise>::try_from(value)
            .map_err(|_| WorkerError::Internal("module evaluate did not return a promise".into()))?;
        v8::Global::new(tc, promise)
    };

    match run_event_loop_until_settled(scope, op_rx, &promise, deadline, meter) {
        Ok(_) => Ok(()),
        Err(err) => {
            // Errored ならモジュール側の例外を優先して報告
            let module = v8::Local::new(scope, module);
            if module.get_status() == v8::ModuleStatus::Errored {
                let exception = module.get_exception();
                return Err(WorkerError::Script(format_exception(scope, exception)));
            }
            Err(err)
        }
    }
}
```

評価に失敗したモジュールは `ModuleStatus::Errored` になり、
`get_exception()` で原因を取り出せます。Promise の reject 理由よりも
こちらのほうが情報が正確なので、優先して報告しています。

## fetch ハンドラの取り出し

評価が済んだら、module namespace から `default.fetch` を取り出して
`Global<Function>` として保持します。これが Worker の「本体」です。

```rust
fn extract_fetch_handler(
    scope: &mut v8::PinScope,
    module: &v8::Global<v8::Module>,
) -> Result<v8::Global<v8::Function>, WorkerError> {
    let module = v8::Local::new(scope, module);
    let namespace = v8::Local::<v8::Object>::try_from(module.get_module_namespace())
        .map_err(|_| WorkerError::Internal("module namespace is not an object".into()))?;

    let key = v8::String::new(scope, "default").unwrap();
    let default = namespace.get(scope, key.into())
        .filter(|v| !v.is_undefined())
        .ok_or_else(|| WorkerError::Script("script has no default export".into()))?;
    // ... default.fetch を Function として取り出して Global::new ...
}
```

細かいが重要な点: `object.get()` は「プロパティが無い」場合も
**`Some(undefined)`** を返します（`None` は例外発生時のみ）。
「無い」を検出するには `is_undefined()` の確認が必要です。
JS の `obj.missing === undefined` と同じ意味論が API にも現れています。

## エラー型の設計

この章で `WorkerError` という enum を導入しました。後の章のためにここで
全体像を示します。

```rust
pub enum WorkerError {
    Script(String),    // ユーザースクリプト由来（コンパイルエラー・throw・reject）
    Hung,              // 解決される見込みのない Promise を待っている（第 8 章）
    Timeout,           // wall-clock タイムアウト（第 8 章）
    Terminated,        // terminate された（原因未分類の中間状態。第 13 章）
    CpuExceeded,       // CPU 予算超過（第 13 章）
    MemExceeded,       // ヒープ上限超過（第 14 章）
    Internal(String),  // ランタイムのバグ・不整合
}
```

`Script` は「ユーザーが悪い」(HTTP 500 相当)、`CpuExceeded` /
`MemExceeded` / `Timeout` は「制限に当たった」(503 相当)、`Internal` は
「ランタイムが悪い」。エラーの**責任の所在**で分類するのがポイントです。

## この章のまとめ

- ESM は compile（`is_module: true`）→ instantiate（resolve callback）
  → evaluate の 3 段階
- resolve callback はスコープなし・キャプチャ不可。`callback_scope!` で復元
- `evaluate()` は常に Promise を返す。モジュール評価の完了待ちには
  イベントループが要る（TLA 対応が自然に手に入る）
- `get()` の「無い」は `Some(undefined)`。`None` ではない

ただし、ここでロードした worker の `fetch` を呼んでもまだ動きません。
`Response` も `console` も存在しないからです。次章でグローバルを作ります。
