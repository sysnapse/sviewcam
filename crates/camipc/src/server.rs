//! 每連線一個 task 的 Unix socket 伺服器；協定邏輯在 [`crate::proto`]。
//!
//! 連線之間互不影響：某條連線壞掉、太慢或送超長行，只關那一條，camd 繼續服務。

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};

use crate::proto::{dispatch, ErrorCode, ErrorObj, Response, RpcHandler, MAX_LINE_BYTES};

/// 回應寫入逾時：慢客戶端不讀就把連線丟掉，不能卡住整條連線的 task。
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// 持續 accept，直到 listener 出錯（或呼叫端 select 結束）。
pub async fn serve(listener: UnixListener, handler: Arc<dyn RpcHandler>) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    handle_connection(stream, handler).await;
                });
            }
            Err(_) => {
                // accept 失敗（例如 EMFILE）不應該讓 camd 直接死；短暫退避後再試。
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

async fn handle_connection(stream: UnixStream, handler: Arc<dyn RpcHandler>) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    loop {
        let mut buf = Vec::new();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }

        // 行長檢查在 UTF-8 之前：超長行連解析都不做，直接結構化錯誤＋關閉本連線。
        if buf.len() > MAX_LINE_BYTES + 1 {
            let resp = Response::err(
                serde_json::Value::Null,
                ErrorObj::new(
                    ErrorCode::LineTooLong,
                    format!("請求行超過 {MAX_LINE_BYTES} bytes，關閉連線"),
                ),
            );
            let _ = write_line(&mut write_half, &resp).await;
            break;
        }

        let line = match std::str::from_utf8(&buf) {
            Ok(text) => text.trim_end_matches(['\n', '\r']),
            Err(_) => {
                let resp = Response::err(
                    serde_json::Value::Null,
                    ErrorObj::new(ErrorCode::BadRequest, "請求行不是合法 UTF-8"),
                );
                if write_line(&mut write_half, &resp).await.is_err() {
                    break;
                }
                continue;
            }
        };

        let (resp, close) = dispatch(handler.as_ref(), line);
        if write_line(&mut write_half, &resp).await.is_err() {
            break;
        }
        if close {
            break;
        }
    }
}

async fn write_line(writer: &mut OwnedWriteHalf, resp: &Response) -> std::io::Result<()> {
    let mut line = resp.to_line();
    line.push('\n');
    match tokio::time::timeout(WRITE_TIMEOUT, async {
        writer.write_all(line.as_bytes()).await?;
        writer.flush().await
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "回應寫入逾時",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Method;
    use serde_json::{json, Value};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct Pong;

    impl RpcHandler for Pong {
        fn handle(&self, method: Method, _params: &Value) -> Result<Value, ErrorObj> {
            Ok(json!({ "method": method.as_str() }))
        }
    }

    async fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().expect("socketpair")
    }

    #[tokio::test]
    async fn long_line_closes_only_that_connection() {
        let (server_side, mut client_side) = pair().await;

        // 先在另一條連線證明服務還活著。
        let (server_side2, mut client_side2) = pair().await;
        tokio::spawn(handle_connection(server_side2, Arc::new(Pong)));
        client_side2
            .write_all(b"{\"id\":1,\"proto_version\":1,\"method\":\"ping\"}\n")
            .await
            .unwrap();
        let mut buf = vec![0u8; 256];
        let n = client_side2.read(&mut buf).await.unwrap();
        let first: Response = serde_json::from_slice(&buf[..n]).unwrap();
        assert!(first.result.is_some(), "正常連線應先成功");

        tokio::spawn(handle_connection(server_side, Arc::new(Pong)));
        let huge = vec![b'x'; MAX_LINE_BYTES + 10];
        client_side.write_all(&huge).await.unwrap();
        client_side.write_all(b"\n").await.unwrap();

        let mut resp_buf = Vec::new();
        client_side.read_to_end(&mut resp_buf).await.unwrap();
        let text = String::from_utf8_lossy(&resp_buf);
        assert!(text.contains("LINE_TOO_LONG"), "實得：{text}");

        // 第二條連線仍能服務。
        client_side2
            .write_all(b"{\"id\":2,\"proto_version\":1,\"method\":\"ping\"}\n")
            .await
            .unwrap();
        let n = client_side2.read(&mut buf).await.unwrap();
        let second: Response = serde_json::from_slice(&buf[..n]).unwrap();
        assert_eq!(second.id, json!(2));
        assert!(second.result.is_some(), "超長行不應影響其他連線");
    }
}
