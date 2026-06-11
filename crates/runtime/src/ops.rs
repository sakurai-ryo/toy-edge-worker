//! Rust ⇔ JS のネイティブ関数（ops）。
//!
//! 方針: I/O・時間・バイト列変換だけを Rust op にし、Web API の仕様準拠
//! ロジックは bootstrap.js（純 JS）に寄せる。
//!
//! 非同期 op は「PromiseResolver を作って Promise を即 return し、完了を
//! channel 経由でイベントループへ通知して resolve する」モデル。

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

/// 非同期 op の完了通知。worker スレッドのイベントループが受信する。
pub enum LoopEvent {
    OpComplete {
        op_id: u64,
        result: Result<OpResult, String>,
    },
}

/// 非同期 op の成功値。
pub enum OpResult {
    Timer,
    Fetch {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
}

/// Isolate slot に `Rc<RefCell<OpState>>` として保持される op の進行状態。
pub struct OpState {
    pub tx: mpsc::Sender<LoopEvent>,
    pub resolvers: HashMap<u64, v8::Global<v8::PromiseResolver>>,
    pub pending_ops: u64,
    next_op_id: u64,
    /// サブリクエスト fetch の I/O を実行する tokio ランタイムへの入口。
    /// None の場合 fetch() は使えない（即 reject される）
    tokio: Option<tokio::runtime::Handle>,
    /// reqwest クライアント（コネクションプール共有のため 1 isolate に 1 つ）
    http: reqwest::Client,
}

impl OpState {
    pub fn new(tx: mpsc::Sender<LoopEvent>, tokio: Option<tokio::runtime::Handle>) -> Self {
        Self {
            tx,
            resolvers: HashMap::new(),
            pending_ops: 0,
            next_op_id: 0,
            tokio,
            http: reqwest::Client::new(),
        }
    }

    /// resolver を登録して op_id を払い出す。
    fn register(&mut self, resolver: v8::Global<v8::PromiseResolver>) -> u64 {
        let op_id = self.next_op_id;
        self.next_op_id += 1;
        self.resolvers.insert(op_id, resolver);
        self.pending_ops += 1;
        op_id
    }

    /// terminate 時に未完了の op を全て破棄する。
    /// 遅れて届く完了通知は「テーブルに無い op_id → 無視」で処理される。
    pub fn abort_all(&mut self) {
        self.resolvers.clear();
        self.pending_ops = 0;
    }
}

pub fn get_op_state(isolate: &v8::Isolate) -> Rc<RefCell<OpState>> {
    isolate
        .get_slot::<Rc<RefCell<OpState>>>()
        .expect("OpState not set on isolate")
        .clone()
}

/// `globalThis.__ops` にネイティブ関数群を生やす。bootstrap.js 評価前に呼ぶ。
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

/// print(str): 標準出力へ書く（console.log の実体）
fn op_print(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let text = args
        .get(0)
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_default();
    print!("{text}");
    std::io::stdout().flush().ok();
}

/// timer(ms): Promise を返し、ms 経過後に resolve される（setTimeout の実体）。
/// 最初の「pending op」としてイベントループの動作確認に使う。
fn op_timer(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let ms = args.get(0).number_value(scope).unwrap_or(0.0).max(0.0) as u64;

    let resolver = v8::PromiseResolver::new(scope).unwrap();
    rv.set(resolver.get_promise(scope).into());

    let state = get_op_state(scope);
    let resolver = v8::Global::new(scope, resolver);
    let (op_id, tx) = {
        let mut st = state.borrow_mut();
        (st.register(resolver), st.tx.clone())
    };

    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(ms));
        let _ = tx.send(LoopEvent::OpComplete {
            op_id,
            result: Ok(OpResult::Timer),
        });
    });
}

/// fetch(url, method, headersArr, bodyAb?) -> Promise<{status, headers, body}>
///
/// サブリクエストの心臓部。
/// 1. PromiseResolver を作って Promise を即 return する
/// 2. tokio に reqwest の実行を依頼する（worker スレッドはブロックしない）
/// 3. 完了したら LoopEvent としてイベントループへ通知され、
///    complete_op が resolver を resolve する
fn op_fetch(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let url = args
        .get(0)
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_default();
    let method = args
        .get(1)
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_else(|| "GET".into());
    let headers = js_header_pairs(scope, args.get(2));
    let body = value_to_bytes(args.get(3));

    let resolver = v8::PromiseResolver::new(scope).unwrap();
    rv.set(resolver.get_promise(scope).into());

    let state = get_op_state(scope);
    let (op_id, tx, handle, client) = {
        let mut st = state.borrow_mut();
        let Some(handle) = st.tokio.clone() else {
            drop(st);
            let message =
                v8::String::new(scope, "fetch is not available in this runtime").unwrap();
            let exception = v8::Exception::error(scope, message);
            resolver.reject(scope, exception);
            return;
        };
        let resolver = v8::Global::new(scope, resolver);
        (st.register(resolver), st.tx.clone(), handle, st.http.clone())
    };

    handle.spawn(async move {
        let result = do_fetch(client, url, method, headers, body).await;
        // worker スレッドは op_rx.recv でブロック中 → この send が wake になる
        let _ = tx.send(LoopEvent::OpComplete { op_id, result });
    });
}

async fn do_fetch(
    client: reqwest::Client,
    url: String,
    method: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
) -> Result<OpResult, String> {
    let method: reqwest::Method = method.parse().map_err(|_| format!("invalid method: {method}"))?;
    let mut request = client.request(method, &url);
    for (key, value) in headers {
        request = request.header(key, value);
    }
    if let Some(body) = body {
        request = request.body(body);
    }

    let response = request.send().await.map_err(|err| err.to_string())?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(key, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (key.as_str().to_string(), v.to_string()))
        })
        .collect();
    let body = response
        .bytes()
        .await
        .map_err(|err| err.to_string())?
        .to_vec();

    Ok(OpResult::Fetch {
        status,
        headers,
        body,
    })
}

/// JS の [[k, v], ...] 形式のヘッダ配列を Vec に変換する。
fn js_header_pairs(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Vec<(String, String)> {
    let Ok(array) = v8::Local::<v8::Array>::try_from(value) else {
        return Vec::new();
    };
    let mut pairs = Vec::with_capacity(array.length() as usize);
    for i in 0..array.length() {
        let Some(pair) = array.get_index(scope, i) else {
            continue;
        };
        let Ok(pair) = v8::Local::<v8::Array>::try_from(pair) else {
            continue;
        };
        let text = |scope: &mut v8::PinScope, index: u32| {
            pair.get_index(scope, index)
                .and_then(|v| v.to_string(scope))
                .map(|s| s.to_rust_string_lossy(scope))
        };
        if let (Some(key), Some(value)) = (text(scope, 0), text(scope, 1)) {
            pairs.push((key, value));
        }
    }
    pairs
}

/// encodeUtf8(str) -> ArrayBuffer
fn op_encode_utf8(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let text = args
        .get(0)
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_default();
    let buffer = bytes_to_array_buffer(scope, text.into_bytes());
    rv.set(buffer.into());
}

/// decodeUtf8(ArrayBuffer | TypedArray) -> str
fn op_decode_utf8(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(bytes) = value_to_bytes(args.get(0)) else {
        let msg = v8::String::new(scope, "decodeUtf8 expects an ArrayBuffer or view").unwrap();
        let exception = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exception);
        return;
    };
    let text = String::from_utf8_lossy(&bytes);
    rv.set(v8::String::new(scope, &text).unwrap().into());
}

/// Rust のバイト列を JS の ArrayBuffer にする（コピーなし、所有権移譲）。
pub fn bytes_to_array_buffer<'s>(
    scope: &v8::PinScope<'s, '_>,
    bytes: Vec<u8>,
) -> v8::Local<'s, v8::ArrayBuffer> {
    let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
    v8::ArrayBuffer::with_backing_store(scope, &store)
}

/// JS の ArrayBuffer / TypedArray からバイト列を取り出す（コピー）。
pub fn value_to_bytes(value: v8::Local<v8::Value>) -> Option<Vec<u8>> {
    if let Ok(buffer) = v8::Local::<v8::ArrayBuffer>::try_from(value) {
        let store = buffer.get_backing_store();
        let len = store.byte_length();
        let mut bytes = vec![0u8; len];
        if let Some(data) = store.data() {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    data.as_ptr() as *const u8,
                    bytes.as_mut_ptr(),
                    len,
                );
            }
        }
        Some(bytes)
    } else if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(value) {
        let mut bytes = vec![0u8; view.byte_length()];
        view.copy_contents(&mut bytes);
        Some(bytes)
    } else {
        None
    }
}
