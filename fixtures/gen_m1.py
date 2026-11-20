#!/usr/bin/env python3
"""產生 `fixtures/m1/*.jsonl`：M1 環形緩衝／索引／復原的合成 fixture。

刻意不從真機錄製內容生成（避免把權杖、序號、個資帶進 repo）。串流本體用
`synth_stream` 讓 replay 在重放時確定性合成 RTP，而不是把 GB 級 base64 塞進 JSONL。

用法：
    python3 fixtures/gen_m1.py            # 寫入 fixtures/m1/
    python3 fixtures/gen_m1.py --stdout ac1_six_min   # 印出單一 fixture
"""
import argparse
import base64
import json
import pathlib
import sys

SSRC = 0x11223344
TOKEN_PLACEHOLDER = "01234567890123456789012345678901"

# 與 rtsp_core::replay::SynthSpec::SPS / PPS 一致的參數集
SPS = bytes.fromhex("674D003296354050016B4DC040404080")
PPS = bytes.fromhex("68EE31B2")

SDP = (
    "v=0\r\n"
    "o=-2209971456 2209971456 IN IP4 192.168.1.1\r\n"
    "s=Unnamed\r\n"
    "c=IN IP4 192.168.1.11\r\n"
    "t=0 0\r\n"
    "a=recvonly\r\n"
    "m=video 5004 RTP/AVP 96\r\n"
    "a=rtpmap:96 H264/90000\r\n"
    "a=fmtp:96 packetization-mode=1;profile-level-id=640029;"
    "sprop-parameter-sets="
    + base64.b64encode(b"\x00\x00\x00\x01" + SPS).decode()
    + ","
    + base64.b64encode(b"\x00\x00\x00\x01" + PPS).decode()
    + ";\r\n"
    "a=control:trackID=0\r\n"
    "a=x-dimensions:2560,1440\r\n"
    "a=SViewWifi:" + TOKEN_PLACEHOLDER + "\r\n"
)


def rtsp(cseq, headers=None, body=""):
    h = ["RTSP/1.0 200 OK", f"CSeq: {cseq}"]
    for k, v in (headers or []):
        h.append(f"{k}: {v}")
    if body:
        h.append(f"Content-Length: {len(body.encode())}")
    return "\r\n".join(h) + "\r\n\r\n" + body


def responses():
    return [
        rtsp(1, [("Public", "OPTIONS,DESCRIBE,SETUP,PLAY,PAUSE,TEARDOWN")]),
        rtsp(2, [("Content-Base", "rtsp://192.168.1.1/test.264/"),
                 ("Content-Type", "application/sdp"),
                 ("Server", "sunshine/1.11")], SDP),
        rtsp(3, [("Session", "fixture-session;timeout=60"),
                 ("Transport", "RTP/AVP;unicast;client_port=20000-20001;"
                               "destination=192.168.245.65;source=192.168.245.96;server_port=0-0")]),
        rtsp(4, [("Session", "fixture-session"), ("Range", "npt=0-")]),
    ]


def line(obj):
    return json.dumps(obj, ensure_ascii=False, separators=(",", ":"))


def session_lines(conn_index, t_base=1000):
    """一段連線：4 個 RTSP 回應。"""
    out = [line({"type": "conn", "index": conn_index})]
    for i, resp in enumerate(responses()):
        out.append(line({"type": "rtsp_s2c", "t_ms": t_base + i, "data": resp}))
    return out


def synth(t_ms, duration_ms, bitrate_bps, fps=25, gop_ms=2000):
    return line({
        "type": "synth_stream",
        "t_ms": t_ms,
        "duration_ms": duration_ms,
        "bitrate_bps": bitrate_bps,
        "fps": fps,
        "gop_ms": gop_ms,
    })


def fixture(name, note, duration_ms, bitrate_bps, seconds_rst=None):
    """單段或「RST 後重連」兩段的 fixture。"""
    out = [line({"type": "meta", "name": name, "note": note})]
    out += session_lines(0, 1000)
    if seconds_rst is None:
        out.append(synth(1500, duration_ms, bitrate_bps))
        out.append(line({"type": "end"}))
        return out

    # 第一段：開流到 RST；第二段：重連後續流（中間是斷流空窗）
    burst_ms = seconds_rst * 1000
    out.append(synth(1500, burst_ms - 1500, bitrate_bps))
    out.append(line({"type": "rst", "t_ms": burst_ms}))
    out += session_lines(1, burst_ms + 3000)
    out.append(synth(burst_ms + 3500, duration_ms, bitrate_bps))
    out.append(line({"type": "end"}))
    return out


FIXTURES = {
    # AC1：開流 6.5 分鐘、X=5 → 保留窗 (300s, 302s]
    "ac1_six_min": (
        "M1 AC1：6.5 分鐘 @500 kbps，保留窗 5 分鐘",
        390_000,
        500_000,
        None,
    ),
    # AC2：20 分鐘、X=20 → 夾到 15 分鐘並只警告一次
    "ac2_twenty_min": (
        "M1 AC2：20 分鐘 @200 kbps，要求保留 20 分鐘（應夾到 15）",
        1_200_000,
        200_000,
        None,
    ),
    # AC3：20 分鐘 @60 kbps，測試會在段寫到一半時 SIGKILL
    "ac3_kill": (
        "M1 AC3：kill -9 前的長串流 @60 kbps",
        1_200_000,
        60_000,
        None,
    ),
    # AC4：100 秒後 RST，空窗約 3.5 秒後續流 60 秒
    "ac4_gap": (
        "M1 AC4：RST 重連，gap 前後不得串成同一段",
        60_000,
        500_000,
        100,
    ),
    # AC5：30 分鐘 @12 Mbps（只給 #[ignore] 跑）
    "ac5_thirty_min": (
        "M1 AC5：30 分鐘 @12 Mbps，保留窗 15 分鐘（磁碟上限 1.62e9 bytes）",
        1_800_000,
        12_000_000,
        None,
    ),
}


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("--stdout", metavar="NAME", help="只印出指定 fixture")
    ap.add_argument("--out", default="fixtures/m1", help="輸出目錄")
    args = ap.parse_args(argv)

    if args.stdout:
        name = args.stdout
        note, duration, bitrate, rst = FIXTURES[name]
        sys.stdout.write("\n".join(fixture(name, note, duration, bitrate, rst)) + "\n")
        return 0

    outdir = pathlib.Path(args.out)
    outdir.mkdir(parents=True, exist_ok=True)
    for name, (note, duration, bitrate, rst) in FIXTURES.items():
        text = "\n".join(fixture(name, note, duration, bitrate, rst)) + "\n"
        (outdir / f"{name}.jsonl").write_text(text, encoding="utf-8")
        print(f"{outdir / (name + '.jsonl')}: {len(text)} bytes")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
