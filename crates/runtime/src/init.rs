use std::sync::Once;

static INIT: Once = Once::new();

/// V8 プラットフォームを初期化する。プロセス全体で 1 回だけ実行される。
pub fn init() {
    INIT.call_once(|| {
        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
    });
}
