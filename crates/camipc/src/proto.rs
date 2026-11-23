//! 協議 v1：型別、版本檢查與 method dispatch。
//!
//! 時間基準只有一條：索引的 `t_start`／`t_end`（串流毫秒，M1 起點 1500 那種）。
//! `uptime_ms` 是 host 時間，兩者不得互換，也不得用 uptime 換算 `from`／`to`。
//!
//! 每行一個 JSON 物件；請求含 `id`、`proto_version`、`method`、`params`，
//! 回應含 `id` 與 `result` 或 `error`。`proto_version` 在 method dispatch **之前**
//! 檢查，`ping` 也一樣，沒有例外。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// 現行協議版本。
pub const PROTO_VERSION: u32 = 1;

/// 單行上限（bytes，不含換行）。超過回 [`ErrorCode::LineTooLong`] 並關閉該連線。
pub const MAX_LINE_BYTES: usize = 65_536;

/// 結構化錯誤碼。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    /// JSON 壞、缺必要欄位、行不是合法 UTF-8。
    BadRequest,
    /// `method` 不在 `status`／`list_segments`／`ping` 之內。
    UnknownMethod,
    /// `params` 型別或範圍不對（例如 `from > to`、`limit` 超界）。
    BadParams,
    /// `proto_version` 與 [`PROTO_VERSION`] 不符。
    ProtoVersion,
    /// 單行超過 [`MAX_LINE_BYTES`]。
    LineTooLong,
    /// camd 內部錯誤（例如索引讀取失敗）。
    Internal,
}

impl ErrorCode {
    /// 穩定的字串形式（與 serde 輸出一致），測試與日誌用。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BadRequest => "BAD_REQUEST",
            Self::UnknownMethod => "UNKNOWN_METHOD",
            Self::BadParams => "BAD_PARAMS",
            Self::ProtoVersion => "PROTO_VERSION",
            Self::LineTooLong => "LINE_TOO_LONG",
            Self::Internal => "INTERNAL",
        }
    }
}

/// 錯誤物件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorObj {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorObj {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// 一行回應。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObj>,
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, error: ErrorObj) -> Self {
        Self {
            id,
            result: None,
            error: Some(error),
        }
    }

    /// 序列化成一行（不含換行）。
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("response serialises")
    }
}

/// 支援的方法。只有這三個；`subscribe` 等未列入。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Status,
    ListSegments,
    Ping,
}

impl Method {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "status" => Some(Self::Status),
            "list_segments" => Some(Self::ListSegments),
            "ping" => Some(Self::Ping),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::ListSegments => "list_segments",
            Self::Ping => "ping",
        }
    }
}

/// camd 端要實作的 domain 邏輯；版本檢查與錯誤包裝由 [`dispatch`] 負責。
pub trait RpcHandler: Send + Sync {
    fn handle(&self, method: Method, params: &Value) -> Result<Value, ErrorObj>;
}

#[derive(Debug, Deserialize)]
struct RawRequest {
    id: Option<Value>,
    proto_version: Option<u32>,
    method: Option<String>,
    #[serde(default)]
    params: Value,
}

/// 處理一行請求，回傳（回應, 是否關閉連線）。
///
/// 版本檢查在 dispatch 之前；`LIST_SEGMENTS` 之類的參數檢查由 handler 做。
pub fn dispatch(handler: &dyn RpcHandler, line: &str) -> (Response, bool) {
    let raw: RawRequest = match serde_json::from_str(line) {
        Ok(raw) => raw,
        Err(e) => {
            return (
                Response::err(
                    Value::Null,
                    ErrorObj::new(ErrorCode::BadRequest, format!("無法解析 JSON：{e}")),
                ),
                false,
            );
        }
    };
    let id = raw.id.unwrap_or(Value::Null);

    match raw.proto_version {
        None => {
            return (
                Response::err(
                    id,
                    ErrorObj::new(
                        ErrorCode::BadRequest,
                        format!("缺少 proto_version（現行 {PROTO_VERSION}）"),
                    ),
                ),
                false,
            );
        }
        Some(v) if v != PROTO_VERSION => {
            return (
                Response::err(
                    id,
                    ErrorObj::new(
                        ErrorCode::ProtoVersion,
                        format!("協議版本不符：收到 {v}，支援 {PROTO_VERSION}"),
                    )
                    .with_data(json!({ "expected": PROTO_VERSION, "got": v })),
                ),
                false,
            );
        }
        Some(_) => {}
    }

    let name = match raw.method.as_deref() {
        Some(name) => name,
        None => {
            return (
                Response::err(id, ErrorObj::new(ErrorCode::BadRequest, "缺少 method")),
                false,
            );
        }
    };
    let method = match Method::parse(name) {
        Some(method) => method,
        None => {
            return (
                Response::err(
                    id,
                    ErrorObj::new(ErrorCode::UnknownMethod, format!("未知方法：{name}")),
                ),
                false,
            );
        }
    };

    match handler.handle(method, &raw.params) {
        Ok(result) => (Response::ok(id, result), false),
        Err(error) => (Response::err(id, error), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    impl RpcHandler for Echo {
        fn handle(&self, method: Method, _params: &Value) -> Result<Value, ErrorObj> {
            Ok(json!({ "method": method.as_str() }))
        }
    }

    #[test]
    fn version_check_runs_before_ping() {
        let (resp, close) = dispatch(&Echo, r#"{"id":1,"proto_version":99,"method":"ping"}"#);
        assert!(!close);
        assert_eq!(resp.id, json!(1));
        let err = resp.error.unwrap();
        assert_eq!(err.code, ErrorCode::ProtoVersion);
        assert_eq!(err.data.unwrap(), json!({ "expected": 1, "got": 99 }));
    }

    #[test]
    fn missing_version_is_bad_request() {
        let (resp, _) = dispatch(&Echo, r#"{"id":2,"method":"status"}"#);
        assert_eq!(resp.error.unwrap().code, ErrorCode::BadRequest);
    }

    #[test]
    fn unknown_method_is_structured_error() {
        let (resp, _) = dispatch(&Echo, r#"{"id":3,"proto_version":1,"method":"subscribe"}"#);
        assert_eq!(resp.error.unwrap().code, ErrorCode::UnknownMethod);
    }

    #[test]
    fn malformed_json_keeps_connection_open() {
        let (resp, close) = dispatch(&Echo, "{not json");
        assert_eq!(resp.id, Value::Null);
        assert_eq!(resp.error.unwrap().code, ErrorCode::BadRequest);
        assert!(!close);
    }

    #[test]
    fn valid_request_reaches_handler() {
        let (resp, _) = dispatch(&Echo, r#"{"id":"a","proto_version":1,"method":"ping"}"#);
        assert_eq!(resp.id, json!("a"));
        assert_eq!(resp.result.unwrap(), json!({ "method": "ping" }));
        assert!(resp.error.is_none());
    }
}
