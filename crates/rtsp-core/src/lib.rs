//! `rtsp-core` — SViewWifi 私有 RTSP/RTP 客戶端核心（M0 spike）。
//!
//! 範圍刻意收窄（見 TICKET-20261115-01）：
//! - RTSP 控制面：OPTIONS → DESCRIBE（帶 `SViewWifi`）→ SETUP（忽略假 destination）
//!   → PLAY（URL 帶結尾斜線）→ TEARDOWN。
//! - RTP/AVP PT96 H.264 depacket（single NAL / STAP-A / FU-A）→ Annex-B。
//! - 斷流（RST）重連狀態機、序號缺口與空窗記錄。
//! - 不做：GUI、環形緩衝、MKV 寫盤、解碼播放。
//!
//! 傳輸層抽象成 [`link::Link`] / [`link::LinkFactory`]，因此同一條重連狀態機
//! 可以跑在真機（TCP 554 + UDP RTP）或錄製 fixture（[`replay`]）之上。

pub mod clock;
pub mod error;
pub mod link;
pub mod mock;
pub mod replay;
pub mod rtp;
pub mod rtsp;
pub mod sdp;
pub mod session;
pub mod sink;

pub use clock::{Clock, RealClock, VirtualClock};
pub use error::{Error, Result};
pub use link::{Link, LinkFactory};
pub use replay::{ReplayFactory, ReplayFixture};
pub use rtsp::USER_AGENT;
pub use session::{run, Event, ReconnectPolicy, Report, RstClass, SessionConfig};
pub use sink::{FileSink, NalSink, NullSink, VecSink};

/// 相機 Soft-AP 的預設 RTSP 根位址。
pub const DEFAULT_CAMERA_HOST: &str = "192.168.1.1";
/// 相機 RTSP 監聽埠（文檔 §4.1）。
pub const DEFAULT_RTSP_PORT: u16 = 554;
/// 相機串流路徑（文檔 §4.1）。
pub const DEFAULT_STREAM_PATH: &str = "/test.264";
/// 上行私有 UDP 通道（16-byte 心跳，文檔 §6）。
pub const DEFAULT_HEARTBEAT_PORT: u16 = 13000;
