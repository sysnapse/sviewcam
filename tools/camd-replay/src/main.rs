//! `camd-replay`：**測試用**的假相機（RTSP/1.0 + UDP RTP）。
//!
//! 這是給 M3 的 AC①／②／⑤ 用的獨立 process 幀來源：產品路徑的 `camd` 完全不知道
//! 它的存在，只看到一個正常 `rtsp://127.0.0.1:<port>/test.264`。RTP payload 由
//! [`rtsp_core::SynthSpec`] 確定性合成（與 fixture 的 `synth_stream` 同一條組包路徑），
//! 因此每跑一次的位元組完全相同。
//!
//! 刻意不做的事：不驗權杖、不做 SETUP 的 destination 檢查、不支援 UDP 心跳回應
//! （camd 的心跳送給 13000，本程式不監聽，UDP 丟包即可）。它只服務測試。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use rtsp_core::mock::default_sdp;
use rtsp_core::SynthSpec;

/// 假相機的命令列參數。
#[derive(Parser, Debug)]
#[command(
    name = "camd-replay",
    about = "測試用假相機（RTSP/1.0 + UDP RTP，確定性合成 H.264）"
)]
struct Args {
    /// 監聽位址；埠 0 表示由系統挑一個
    #[arg(long, default_value = "127.0.0.1:0")]
    listen: String,

    /// 幀率
    #[arg(long, default_value_t = 25)]
    fps: u32,

    /// GOP 長度（毫秒）
    #[arg(long, default_value_t = 2000)]
    gop_ms: u64,

    /// 目標碼率（bps）
    #[arg(long, default_value_t = 500_000)]
    bitrate_bps: u64,

    /// 把實際監聽位址寫進這個檔（測試用；一行文字）
    #[arg(long)]
    port_file: Option<PathBuf>,

    /// 直接送真實的 H.264 Annex B 檔（測試用）。給定時忽略合成 bitstream；
    /// 這條路徑讓 AC 的「已落盤段可播」能用真的可解碼視訊驗證。
    #[arg(long)]
    h264: Option<PathBuf>,
}

/// 一條控制連線的狀態。
struct Conn {
    stream: TcpStream,
    buf: Vec<u8>,
    session: String,
    cseq: u32,
    rtp_target: Option<std::net::SocketAddr>,
    stop: Arc<AtomicBool>,
    playing: bool,
}

fn main() {
    let args = Args::parse();
    let spec = SynthSpec {
        start_ms: 0,
        duration_ms: 0,
        bitrate_bps: args.bitrate_bps,
        fps: args.fps,
        gop_ms: args.gop_ms,
    };
    let listener = match TcpListener::bind(&args.listen) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("錯誤：無法監聽 {}：{e}", args.listen);
            std::process::exit(2);
        }
    };
    let addr = listener.local_addr().expect("local_addr");
    if let Some(path) = &args.port_file {
        if let Err(e) = std::fs::write(path, format!("{}", addr.port())) {
            eprintln!("錯誤：寫入 port file 失敗（{}）：{e}", path.display());
            std::process::exit(2);
        }
    }
    println!("camd-replay 已啟動：rtsp://{addr}/test.264");
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    let h264 = match &args.h264 {
        Some(path) => match load_annexb(path) {
            Ok(frames) if !frames.is_empty() => Some(Arc::new(frames)),
            Ok(_) => {
                eprintln!("錯誤：{} 裡沒有 H.264 frame", path.display());
                std::process::exit(2);
            }
            Err(e) => {
                eprintln!("錯誤：讀不了 {}：{e}", path.display());
                std::process::exit(2);
            }
        },
        None => None,
    };

    let epoch = Instant::now();
    let frame = Arc::new(AtomicU64::new(0));
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let spec = spec.clone();
                let frame = Arc::clone(&frame);
                let h264 = h264.clone();
                std::thread::spawn(move || {
                    if let Err(e) = serve(stream, spec, h264, epoch, frame) {
                        eprintln!("camd-replay：連線結束：{e}");
                    }
                });
            }
            Err(e) => eprintln!("camd-replay：accept 失敗：{e}"),
        }
    }
}

fn serve(
    stream: TcpStream,
    spec: SynthSpec,
    h264: Option<Arc<Vec<Vec<Vec<u8>>>>>,
    epoch: Instant,
    frame: Arc<AtomicU64>,
) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    let mut conn = Conn {
        stream,
        buf: Vec::new(),
        session: "0".to_string(),
        cseq: 0,
        rtp_target: None,
        stop: Arc::new(AtomicBool::new(false)),
        playing: false,
    };
    loop {
        let Some(request) = read_request(&mut conn)? else {
            // 控制連線斷了（camd 被 kill -9 或關掉）：把這條連線的 RTP 送流一起停掉，
            // 否則舊的送流 thread 會和新連線的 thread 同時灌同一個埠。
            conn.stop.store(true, Ordering::SeqCst);
            return Ok(());
        };
        let method = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().next())
            .unwrap_or("")
            .to_string();
        if let Some(cseq) = request
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("cseq:"))
            .and_then(|line| line.split(':').nth(1))
            .and_then(|value| value.trim().parse::<u32>().ok())
        {
            conn.cseq = cseq;
        }
        match method.as_str() {
            "OPTIONS" => respond(
                &mut conn,
                "Public: OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN",
                "",
            )?,
            "DESCRIBE" => {
                let sdp = default_sdp("camd-replay");
                let headers = format!(
                    "Content-Type: application/sdp\r\nContent-Length: {}",
                    sdp.len()
                );
                respond(&mut conn, &headers, &sdp)?;
            }
            "SETUP" => {
                let client_port = parse_client_port(&request).unwrap_or(5004);
                let udp = UdpSocket::bind("127.0.0.1:0")?;
                let server_port = udp.local_addr()?.port();
                conn.rtp_target = Some(format!("127.0.0.1:{client_port}").parse().unwrap());
                conn.session = format!("{}", std::process::id());
                let extra = format!(
                    "Transport: RTP/AVP;unicast;client_port={client_port}-{};server_port={server_port};ssrc=0x11223344\r\nSession: {};timeout=60",
                    client_port + 1,
                    conn.session
                );
                // server_port 只是回給客戶端的資訊；實際送流在 PLAY 另開 socket。
                drop(udp);
                respond(&mut conn, &extra, "")?;
            }
            "PLAY" => {
                respond(&mut conn, "", "")?;
                if let Some(target) = conn.rtp_target {
                    if !conn.playing {
                        conn.playing = true;
                        let spec = spec.clone();
                        let h264 = h264.clone();
                        let stop = Arc::clone(&conn.stop);
                        let frame = Arc::clone(&frame);
                        std::thread::spawn(move || {
                            stream_rtp(spec, h264, target, stop, epoch, frame)
                        });
                    }
                }
            }
            "TEARDOWN" => {
                respond(&mut conn, "", "")?;
                conn.stop.store(true, Ordering::SeqCst);
                conn.playing = false;
                return Ok(());
            }
            _ => {
                let body = write_response(&mut conn, "501 Not Implemented", "", "");
                if let Err(e) = body {
                    return Err(e);
                }
                return Ok(());
            }
        }
    }
}

fn parse_client_port(request: &str) -> Option<u16> {
    let lower = request.to_ascii_lowercase();
    let idx = lower.find("client_port=")?;
    let rest = &request[idx + "client_port=".len()..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn read_request(conn: &mut Conn) -> std::io::Result<Option<String>> {
    loop {
        if let Some(end) = find_headers_end(&conn.buf) {
            let request = String::from_utf8_lossy(&conn.buf[..end]).to_string();
            conn.buf.drain(..end);
            return Ok(Some(request));
        }
        let mut chunk = [0u8; 4096];
        let n = conn.stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        conn.buf.extend_from_slice(&chunk[..n]);
    }
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|pos| pos + 4)
}

fn respond(conn: &mut Conn, headers: &str, body: &str) -> std::io::Result<()> {
    write_response(conn, "200 OK", headers, body)
}

/// 寫一個完整 RTSP 回應：狀態列＋CSeq＋（可選）headers＋空行＋body。
fn write_response(conn: &mut Conn, status: &str, headers: &str, body: &str) -> std::io::Result<()> {
    let mut text = format!("RTSP/1.0 {status}\r\nCSeq: {}\r\n", conn.cseq);
    let headers = headers.trim_end_matches(['\r', '\n']);
    if !headers.is_empty() {
        text.push_str(headers);
        text.push_str("\r\n");
    }
    text.push_str("\r\n");
    text.push_str(body);
    conn.stream.write_all(text.as_bytes())?;
    conn.stream.flush()
}

/// 依實際時間送流：斷線期間時間照樣前進，重連後 timestamp 會出現真正的空窗
/// （和真相機一致）。`frame` 是跨連線共用的計數器，避免重連後 timestamp 倒退。
fn stream_rtp(
    spec: SynthSpec,
    h264: Option<Arc<Vec<Vec<Vec<u8>>>>>,
    target: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    epoch: Instant,
    frame: Arc<AtomicU64>,
) {
    let udp = match UdpSocket::bind("127.0.0.1:0") {
        Ok(udp) => udp,
        Err(e) => {
            eprintln!("camd-replay：RTP socket 綁定失敗：{e}");
            return;
        }
    };
    let period = Duration::from_millis(1000 / spec.fps as u64);
    let trace = std::env::var_os("CAMD_REPLAY_TRACE").is_some();
    let mut seq: u16 = 0;
    if trace {
        eprintln!(
            "trace: stream start target={target} wall_ms={} frame={}",
            epoch.elapsed().as_millis(),
            frame.load(Ordering::SeqCst)
        );
    }
    // 時間戳只跟前一個 session 的進度有關（模擬相機持續產生影格、camd 錯過的空窗
    // 就是真的錯過），但不可以用 epoch 追趕：重連後若一次補送，camd 收到的時間戳
    // 會比實際快，索引就會出現「同一秒兩段」的假象。故每個 session 從現在的 frame
    // 計數起算，以真實 25 fps 的節奏送。
    let start_index = frame.load(Ordering::SeqCst);
    let start = Instant::now();
    let mut index = start_index;
    while !stop.load(Ordering::SeqCst) {
        let packets = match &h264 {
            Some(frames) => {
                let frame_nals = &frames[(index as usize) % frames.len()];
                let ticks = (index as u32).wrapping_mul(90_000 / spec.fps.max(1));
                annexb_packets(frame_nals, ticks, &mut seq)
            }
            None => spec.frame_packets(index, &mut seq),
        };
        for packet in packets {
            if udp.send_to(&packet, target).is_err() {
                return;
            }
        }
        index += 1;
        frame.store(index, Ordering::SeqCst);
        let due = start + period * (index - start_index) as u32;
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
    }
    if trace {
        eprintln!(
            "trace: stream stop wall_ms={} frame={}",
            epoch.elapsed().as_millis(),
            frame.load(Ordering::SeqCst)
        );
    }
}

/// 把 Annex B 檔切成「一個 frame 一組 NAL」：非 VCL（SPS/PPS/SEI/AUD）歸到下一幀，
/// 遇到 VCL（type 1/5）且當前幀已經有 VCL 就開新幀。
fn load_annexb(path: &std::path::Path) -> Result<Vec<Vec<Vec<u8>>>, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let mut nals: Vec<Vec<u8>> = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let start = i + 3;
            // 找下一個 start code（4 byte 形式也吃）
            let mut j = start;
            while j + 3 <= data.len() {
                if data[j] == 0 && data[j + 1] == 0 && data[j + 2] == 1 {
                    break;
                }
                j += 1;
            }
            let end = j.min(data.len());
            if end > start {
                nals.push(data[start..end].to_vec());
            }
            i = end;
        } else {
            i += 1;
        }
    }
    let mut frames: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut pending: Vec<Vec<u8>> = Vec::new();
    let mut has_slice = false;
    for nal in nals {
        let nal_type = nal.first().map(|b| b & 0x1f).unwrap_or(0);
        let is_vcl = nal_type == 1 || nal_type == 5;
        if is_vcl && has_slice {
            frames.push(std::mem::take(&mut pending));
            has_slice = false;
        }
        pending.push(nal);
        if is_vcl {
            has_slice = true;
        }
    }
    if !pending.is_empty() {
        frames.push(pending);
    }
    Ok(frames)
}

/// 把一幀的 NAL 打成 RTP 封包：小 NAL 走 single NAL，大的走 FU-A；marker 在最後一個。
fn annexb_packets(nals: &[Vec<u8>], ticks: u32, seq: &mut u16) -> Vec<Vec<u8>> {
    const SSRC: u32 = 0x1122_3344;
    const MAX_PAYLOAD: usize = 1400;
    let mut packets = Vec::new();
    let count = nals.len();
    for (i, nal) in nals.iter().enumerate() {
        let last_nal = i + 1 == count;
        if nal.len() <= MAX_PAYLOAD {
            let mut packet = Vec::with_capacity(12 + nal.len());
            push_header(&mut packet, *seq, ticks, last_nal, SSRC);
            *seq = seq.wrapping_add(1);
            packet.extend_from_slice(nal);
            packets.push(packet);
        } else {
            // FU-A：indicator + header + payload 切片。
            let nal_type = nal[0] & 0x1f;
            let nri = nal[0] & 0x60;
            let mut offset = 1;
            let mut first = true;
            while offset < nal.len() {
                let end = (offset + MAX_PAYLOAD - 2).min(nal.len());
                let mut packet = Vec::with_capacity(14 + (end - offset));
                let marker = last_nal && end == nal.len();
                push_header(&mut packet, *seq, ticks, marker, SSRC);
                *seq = seq.wrapping_add(1);
                packet.push((nri | 28) as u8);
                let mut fu = nal_type;
                if first {
                    fu |= 0x80;
                }
                if end == nal.len() {
                    fu |= 0x40;
                }
                packet.push(fu);
                packet.extend_from_slice(&nal[offset..end]);
                packets.push(packet);
                offset = end;
                first = false;
            }
        }
    }
    packets
}

fn push_header(packet: &mut Vec<u8>, seq: u16, ticks: u32, marker: bool, ssrc: u32) {
    packet.push(0x80);
    packet.push(if marker { 96 | 0x80 } else { 96 });
    packet.extend_from_slice(&seq.to_be_bytes());
    packet.extend_from_slice(&ticks.to_be_bytes());
    packet.extend_from_slice(&ssrc.to_be_bytes());
}
