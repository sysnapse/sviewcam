use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// 對端關閉／重設連線（相機在開流後約 60–70 秒會主動 RST，文檔 §10）。
    #[error("link closed: {0}")]
    Closed(String),

    /// 重放資料已用盡：不是錯誤，代表 fixture 正常結束。
    #[error("end of input")]
    EndOfInput,

    #[error("protocol: {0}")]
    Protocol(String),

    #[error("rtsp status {status}: {body}")]
    Status { status: u16, body: String },

    #[error("SViewWifi 權杖缺失：{0}")]
    TokenMissing(String),
}

pub type Result<T> = std::result::Result<T, Error>;
