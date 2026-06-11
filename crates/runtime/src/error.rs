use std::fmt;

/// Worker 実行で起きるエラー。HTTP 層でのステータス変換を見越して種別を持つ。
#[derive(Debug)]
pub enum WorkerError {
    /// ユーザースクリプト由来の例外（コンパイルエラー・throw・Promise reject）
    Script(String),
    /// 解決される見込みのない Promise を待っている（pending op が無いのに pending）
    Hung,
    /// wall-clock タイムアウト
    Timeout,
    /// terminate_execution による強制終了（未分類。通常は呼び出し側で
    /// CpuExceeded / MemExceeded に分類される）
    Terminated,
    /// CPU 時間制限超過（watchdog による terminate）。Isolate は再利用可
    CpuExceeded,
    /// ヒープ上限超過（near-heap-limit による terminate）。Isolate は要破棄
    MemExceeded,
    /// ランタイム内部の不整合
    Internal(String),
}

impl fmt::Display for WorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkerError::Script(msg) => write!(f, "script error: {msg}"),
            WorkerError::Hung => write!(f, "promise will never resolve"),
            WorkerError::Timeout => write!(f, "wall-clock timeout exceeded"),
            WorkerError::Terminated => write!(f, "execution terminated"),
            WorkerError::CpuExceeded => write!(f, "worker exceeded CPU time limit"),
            WorkerError::MemExceeded => write!(f, "worker exceeded memory limit"),
            WorkerError::Internal(msg) => write!(f, "internal error: {msg}"),
        }
    }
}

impl std::error::Error for WorkerError {}
