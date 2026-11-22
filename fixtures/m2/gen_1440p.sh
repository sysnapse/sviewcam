#!/bin/sh
# 產生 k14 管線證據用的 2560x1440 H.264 Annex B。
#
# 這不是 SView 相機的錄影：餵入端是 ffmpeg 的 testsrc2 合成畫面，只證明
# 「真實可解碼的 1440p 走過 ring → export → ffprobe」這條管線。真機證據由 k10 負責。
# 只產 10 秒；測試再把同一段 Annex B 重播 19 次（每次從 IDR 開始）湊出 190 秒時間軸，
# 避免在 CI 上花幾分鐘編 4750 個 1440p 幀。
set -eu
out=${1:-k14-1440p.h264}
ffmpeg -hide_banner -nostdin -loglevel error -y \
  -f lavfi -i testsrc2=size=2560x1440:rate=25 \
  -t 10 -c:v libx264 -preset ultrafast -g 50 -pix_fmt yuv420p -b:v 2M \
  -f h264 "$out"
