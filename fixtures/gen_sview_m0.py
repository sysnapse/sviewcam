#!/usr/bin/env python3
"""產生 `fixtures/sview-m0.jsonl`：合成（非真機）SView 會話重放樣本。

刻意不從真機錄製內容生成，避免把任何權杖／個資帶進 repo。fixture 只用來驗
「重連狀態機 + depacket + 序號缺口統計」是否照預期工作；真機行為仍須真機窗驗
（TICKET-20261115-01 標準 3–5）。

用法：python3 fixtures/gen_sview_m0.py > fixtures/sview-m0.jsonl
"""
import base64
import json
import sys

SSRC = 0x11223344
TOKEN_PLACEHOLDER = "01234567890123456789012345678901"

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
    "sprop-parameter-sets=AAAAAWdNADKWNUBQAWtNwEBAQIA=,AAAAAWjuMbI=;\r\n"
    "a=control:trackID=0\r\n"
    "a=x-dimensions:2560,1440\r\n"
    "a=SViewWifi:" + TOKEN_PLACEHOLDER + "\r\n"
)


def rtsp(status, cseq, headers=None, body=""):
    h = [f"RTSP/1.0 {status} OK", f"CSeq: {cseq}"]
    for k, v in (headers or []):
        h.append(f"{k}: {v}")
    if body:
        h.append(f"Content-Length: {len(body.encode())}")
    return "\r\n".join(h) + "\r\n\r\n" + body


def responses():
    return [
        rtsp(200, 1, [("Public", "OPTIONS,DESCRIBE,SETUP,PLAY,PAUSE,TEARDOWN")]),
        rtsp(200, 2, [("Content-Base", "rtsp://192.168.1.1/test.264/"),
                      ("Content-Type", "application/sdp"),
                      ("Server", "sunshine/1.11")], SDP),
        rtsp(200, 3, [("Session", "fixture-session;timeout=60"),
                      ("Transport", "RTP/AVP;unicast;client_port=20000-20001;"
                                    "destination=192.168.245.65;source=192.168.245.96;server_port=0-0")]),
        rtsp(200, 4, [("Session", "fixture-session"), ("Range", "npt=0-")]),
    ]


def rtp(seq, ts, payload, marker=False):
    h = bytearray([0x80, (0x80 if marker else 0) | 96])
    h += seq.to_bytes(2, "big") + ts.to_bytes(4, "big") + SSRC.to_bytes(4, "big")
    return bytes(h) + payload


def single(seq, ts, nal):
    return rtp(seq, ts, nal)


def fu_a(seq_start, ts, nal, frag):
    """把 NAL 切成 FU-A，回傳 (封包清單, 下一個 seq)。"""
    body = nal[1:]
    out = []
    chunks = [body[i:i + frag] for i in range(0, len(body), frag)]
    for i, chunk in enumerate(chunks):
        hdr = 0
        if i == 0:
            hdr |= 0x80
        if i == len(chunks) - 1:
            hdr |= 0x40
        hdr |= nal[0] & 0x1F
        out.append(rtp(seq_start + i, ts, bytes([0x7C, hdr]) + chunk))
    return out, seq_start + len(chunks)


def conn(lines, index, packets, end, t_start, rst_at):
    lines.append({"type": "conn", "index": index})
    t = t_start
    for r in responses():
        lines.append({"type": "rtsp_s2c", "t_ms": t, "data": r})
        t += 1
    for i, p in enumerate(packets):
        lines.append({"type": "rtp", "t_ms": t_start + 500 + i * 40,
                      "data": base64.b64encode(p).decode()})
    if end == "rst":
        lines.append({"type": "rst", "t_ms": rst_at})
    else:
        lines.append({"type": "end", "t_ms": rst_at})
    return lines


def main():
    lines = []
    lines.append({
        "type": "meta",
        "name": "sview-m0-synthetic",
        "note": "合成樣本（非真機錄製）：2 次 RST 重連、1 個序號缺口",
        "expected": {"connections": 3, "reconnects": 2, "seq_gaps": 1, "nals": None},
    })

    nals = 0

    # 連線 1：single → FU-A → 16 個 single，相機 RST
    p1 = [single(1, 1000, b"\x65\x01\x02")]
    nals += 1
    frags, nxt = fu_a(2, 2000, b"\x65\xaa\xbb\xcc\xdd\xee", 2)
    p1 += frags
    nals += 1
    for i in range(16):
        p1.append(single(nxt + i, 3000 + i, bytes([0x41, i])))
    nals += 16
    conn(lines, 0, p1, "rst", 1000, 61000)

    # 連線 2：序號重置 → 序號缺口 4 → RST
    p2 = [single(1, 1000, b"\x65\x0a")]
    nals += 1
    frags, nxt = fu_a(2, 2000, b"\x65\x11\x22\x33\x44\x55", 2)
    p2 += frags
    nals += 1
    for i in range(3):
        p2.append(single(nxt + i, 3000 + i, bytes([0x41, i])))
    nals += 3
    gap_from = nxt + 3
    for i in range(9):
        p2.append(single(gap_from + 4 + i, 4000 + i, bytes([0x41, 0x10 + i])))
    nals += 9
    conn(lines, 1, p2, "rst", 62000, 122000)

    # 連線 3：正常結束
    p3 = [single(1 + i, 5000 + i, bytes([0x41, i])) for i in range(10)]
    nals += 10
    conn(lines, 2, p3, "end", 123000, 130000)

    lines[0]["expected"]["nals"] = nals
    for line in lines:
        sys.stdout.write(json.dumps(line, ensure_ascii=False, separators=(",", ":")) + "\n")


if __name__ == "__main__":
    main()
