/// TryCatch スコープから例外メッセージを取り出す。
/// （tc_scope! が生成する型はジェネリクスで名指ししにくいためマクロにしている）
macro_rules! exception_message {
    ($tc:expr) => {
        match $tc.exception() {
            Some(exception) => $crate::worker::format_exception($tc, exception),
            None => "unknown error".to_string(),
        }
    };
}

mod error;
mod init;
mod limits;
mod ops;
mod worker;

pub use error::WorkerError;
pub use init::init;
pub use worker::{Worker, WorkerConfig};

use anyhow::{anyhow, Result};

/// JS 式を新しい Isolate で評価して結果を文字列で返す（M0 のデモ用）。
pub fn eval(code: &str) -> Result<String> {
    init();

    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let scope, &mut isolate);
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    v8::tc_scope!(let tc, scope);

    let source =
        v8::String::new(tc, code).ok_or_else(|| anyhow!("source too long"))?;
    let Some(script) = v8::Script::compile(tc, source, None) else {
        return Err(anyhow!("compile error: {}", exception_message!(tc)));
    };
    let Some(result) = script.run(tc) else {
        return Err(anyhow!("runtime error: {}", exception_message!(tc)));
    };

    let result = result
        .to_string(tc)
        .ok_or_else(|| anyhow!("failed to stringify result"))?;
    Ok(result.to_rust_string_lossy(tc))
}
