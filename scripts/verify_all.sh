#!/bin/sh
# TICKET-20261115-01 的單一驗證入口（第 3 輪重送前必跑）。
# 用法（devbox 內、repo 根目錄，cargo 需在 PATH）：sh scripts/verify_all.sh
# 任一步非 0 即停並原樣報出 rc；全部通過才允許 push。
#
# 注意：devbox 的 rust 模板（alpine）內**沒有 python3**，也裝不了（非 root、apk 被拒）。
# 故本腳本只用 sh＋cargo；`fixtures/gen_sview_m0.py` 要重生 fixture 請在自己有 python3 的
# 機器上跑，再 commit 生成的 .jsonl。
set -u

export PATH="/usr/local/cargo/bin:$PATH"

step() { echo; echo "=== $* ==="; }
fail() { echo "STOP: $1（rc=$2）"; exit "$2"; }

step "1/4 cargo test --workspace"
cargo test --workspace || fail "workspace tests" $?

step "2/4 replay 重放合成 fixture（含宣告值檢查）"
cargo run -q -p replay -- --input fixtures/sview-m0.jsonl || fail "replay" $?

step "3/4 變異測試 harness（good 全綠＋5 個 mutant 全部由指定測試檢出）"
sh scripts/mutant_harness.sh || fail "mutant harness" $?

step "4/4 工作樹狀態（harness 與測試必須是 tracked 內容）"
git status --short

echo
echo "ALL GREEN"
