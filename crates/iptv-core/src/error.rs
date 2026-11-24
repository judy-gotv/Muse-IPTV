//! 领域错误（§2：`AppError` 映射状态码；业务层只抛错误码 + 参数，
//! 序列化层按 locale 渲染 message，§12.5）。
// 作者: Passwd Word

use iptv_dto::ErrorCode;
use thiserror::Error;

/// 业务错误：错误码 + 结构化参数（供 i18n 渲染，如 used/max）。
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("{code:?}: {message}")]
    Coded {
        code: ErrorCode,
        message: String,
        args: Vec<(String, serde_json::Value)>,
    },
    #[error("internal: {0}")]
    Internal(String),
}

impl CoreError {
    pub fn coded(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Coded {
            code,
            message: message.into(),
            args: vec![],
        }
    }
    pub fn with_arg(mut self, key: &str, value: serde_json::Value) -> Self {
        if let Self::Coded { args, .. } = &mut self {
            args.push((key.to_string(), value));
        }
        self
    }
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Coded { code, .. } => *code,
            Self::Internal(_) => ErrorCode::Internal,
        }
    }
    pub fn message(&self) -> String {
        match self {
            Self::Coded { message, .. } => message.clone(),
            Self::Internal(m) => m.clone(),
        }
    }
}

impl From<anyhow::Error> for CoreError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e.to_string())
    }
}
