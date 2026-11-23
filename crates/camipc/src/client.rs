//! 同步客戶端（給 `camctl` 與整合測試用）。伺服端是 tokio，客戶端刻意用 std。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use serde_json::Value;

use crate::proto::{ErrorObj, Response, PROTO_VERSION};

/// 一條控制連線。每次 [`Client::call`] 用遞增 id，回應對 id。
pub struct Client {
    stream: BufReader<UnixStream>,
    next_id: u64,
}

impl Client {
    pub fn connect(path: &Path) -> std::io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        Ok(Self {
            stream: BufReader::new(stream),
            next_id: 1,
        })
    }

    /// 送一個請求並讀一行回應。
    pub fn call(&mut self, method: &str, params: Value) -> std::io::Result<Response> {
        self.call_with_version(PROTO_VERSION, method, params)
    }

    /// 指定 `proto_version`（測試版本不符用）。
    pub fn call_with_version(
        &mut self,
        version: u32,
        method: &str,
        params: Value,
    ) -> std::io::Result<Response> {
        let id = self.next_id;
        self.next_id += 1;
        let line = serde_json::json!({
            "id": id,
            "proto_version": version,
            "method": method,
            "params": params,
        });
        self.send_line(&line.to_string())
    }

    /// 送原始一行並讀回應（測試壞 JSON／超長行用）。
    pub fn send_raw(&mut self, line: &str) -> std::io::Result<Response> {
        self.send_line(line)
    }

    /// 只送不讀（測試客戶端中途斷線用）。
    pub fn write_only(&mut self, line: &str) -> std::io::Result<()> {
        let mut text = line.to_string();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        self.stream.get_mut().write_all(text.as_bytes())?;
        self.stream.get_mut().flush()
    }

    fn send_line(&mut self, line: &str) -> std::io::Result<Response> {
        self.write_only(line)?;
        let mut buf = String::new();
        self.stream.read_line(&mut buf)?;
        if buf.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "camd 在回應前關閉連線",
            ));
        }
        serde_json::from_str(buf.trim_end())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

/// 把回應轉成 `Result`：`error` 轉 [`ErrorObj`]。
pub fn into_result(mut resp: Response) -> Result<Value, ErrorObj> {
    if let Some(error) = resp.error.take() {
        return Err(error);
    }
    match resp.result.take() {
        Some(result) => Ok(result),
        None => Err(ErrorObj::new(
            crate::proto::ErrorCode::Internal,
            format!("回應缺少 result：{resp:?}"),
        )),
    }
}
