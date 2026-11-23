//! `camipc` — SViewCam camd 控制協議（M3 切片 1）。
//!
//! 版本化 JSON-RPC 行協議，走 Unix domain socket。伺服端（[`server::serve`]）用
//! tokio，客戶端（[`client::Client`]）同步；協議型別與 dispatch 在 [`proto`]。
//!
//! 時間基準：索引的 `t_start`／`t_end`（串流毫秒）是唯一權威；`uptime_ms` 是
//! host 時間，兩者不得互換。

pub mod client;
pub mod path;
pub mod proto;
pub mod server;

pub use client::Client;
pub use path::resolve_socket;
pub use proto::{
    dispatch, ErrorCode, ErrorObj, Method, Response, RpcHandler, MAX_LINE_BYTES, PROTO_VERSION,
};
