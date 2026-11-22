#!/bin/sh
# 下載「本機測試用」的 arm64 靜態 ffmpeg（釘死版本與 sha256）。
#
# CI 一律用 ubuntu-latest 的系統 ffmpeg，不執行這個腳本；這只是給沒有 ffmpeg
# 的 arm64 devbox 重現 AC①/③/④ 用。產物在 .tools/ffmpeg/，不進 git。
set -eu

VERSION=7.0.2
URL="https://johnvansickle.com/ffmpeg/releases/ffmpeg-${VERSION}-arm64-static.tar.xz"
SHA256=f4149bb2b0784e30e99bdda85471c9b5930d3402014e934a5098b41d0f7201b1
DEST=${1:-.tools/ffmpeg}

case "$(uname -m)" in
  aarch64|arm64) ;;
  *) echo "此腳本只提供 arm64 靜態版；$(uname -m) 非 arm64 請自備 ffmpeg（或改用系統 ffmpeg）" >&2; exit 2 ;;
esac

mkdir -p "$DEST"
if [ -x "$DEST/ffmpeg" ] && [ -x "$DEST/ffprobe" ]; then
  echo "已存在：$DEST/ffmpeg"
  exit 0
fi

TMP=$(mktemp)
trap 'rm -f "$TMP"' EXIT
if command -v curl >/dev/null 2>&1; then
  curl -fsSL "$URL" -o "$TMP"
else
  wget -q -O "$TMP" "$URL"
fi
echo "$SHA256  $TMP" | sha256sum -c -
tar -xJf "$TMP" -C "$DEST" --strip-components=1
"$DEST/ffmpeg" -version | head -1
