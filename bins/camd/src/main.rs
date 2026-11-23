//! `camd` 進入點。exit code：0 正常收工；1 已有 camd 在執行；2 參數／環境／復原錯誤。

use std::path::PathBuf;
use std::process::ExitCode;

use camd::{Options, StartupError};
use clap::Parser;

/// SViewCam 常駐服務（M3 切片 1）：拉流、寫段、索引、控制 socket。
#[derive(Parser, Debug)]
#[command(
    name = "camd",
    about = "SViewCam 常駐服務（M3 切片 1）",
    version,
    after_help = "控制 socket：--socket > $SVCAM_SOCKET > $XDG_RUNTIME_DIR/sviewcam/camd.sock\n> $HOME/.local/state/sviewcam/camd.sock；協議 v1（status／list_segments／ping）。"
)]
struct Args {
    /// 錄影資料目錄（段檔與 index.sqlite）
    #[arg(long)]
    dir: PathBuf,

    /// 控制 socket 路徑（預設依 SVCAM_SOCKET／XDG_RUNTIME_DIR／HOME 解析）
    #[arg(long)]
    socket: Option<PathBuf>,

    /// RTSP URI
    #[arg(long, default_value = "rtsp://192.168.1.1:554/test.264")]
    uri: String,

    /// 讀取 SViewWifi 權杖的環境變數名稱（權杖不落盤、不進日誌）
    #[arg(long, default_value = "SVIEW_TOKEN")]
    token_env: String,

    /// 保留窗分鐘數（預設 5，上限 15）
    #[arg(long)]
    retain_minutes: Option<u64>,

    /// 寫進 MKV 的視訊寬
    #[arg(long, default_value_t = 2560)]
    width: u32,

    /// 寫進 MKV 的視訊高
    #[arg(long, default_value_t = 1440)]
    height: u32,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let token = std::env::var(&args.token_env).unwrap_or_default();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("錯誤：建立 tokio runtime 失敗：{e}");
            return ExitCode::from(2);
        }
    };

    let options = Options {
        dir: args.dir,
        socket: args.socket,
        uri: args.uri,
        token,
        retention_minutes: args.retain_minutes,
        width: args.width,
        height: args.height,
    };

    match runtime.block_on(camd::run(options)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(StartupError { code, message }) => {
            eprintln!("錯誤：{message}");
            ExitCode::from(code)
        }
    }
}
