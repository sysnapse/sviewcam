#!/bin/sh
# 變異測試 harness（TICKET-20261115-01 驗收標準 2）
#
# 目的：證明「重連測試有鑑別力」，不是只證明「測試會綠」。
# 做法：把 crates/rtsp-core/src/session.rs 的重連決策逐個改成已知錯誤的實作（mutant），
#       每次在副本上跑 `cargo test -p rtsp-core --no-fail-fast`，斷言：
#         1. good（未變異）→ 全綠；
#         2. 每個 mutant → 必須出現 `test result: FAILED`，而且**指定那個測試**要紅才算檢出；
#            紅在別處不算，否則「測試根本沒碰到這條規則」也会被誤判成檢出。
#       編譯不過**不算檢出**（另報 COMPILE_ERROR，exit 4），避免「改壞到編不過」冒充「測試抓到」。
#
# 對應 protocol 文檔 §11.2（2026-11-15 14:3x 裁定為三類 RST 觸發）：
#   naive     斷流後不重連（把 RST 當正常結束）      → 定時回收／中途 RST 兩類都要紅
#   mutantA   退避失效（斷流後立即重連，不睡 backoff） → 中途 RST 加倍退避
#   mutantB   不容忍序號重置（重連後新會話 seq 從 1 起會被誤報缺包）→ 合成 RST 重連
#   mutantC   不分類（把定時回收當成中途 RST，白等退避）→ 定時回收退避必為 0
#   mutantD   空窗收口點退回 PLAY 200（N1 前的行為）→ 空窗量成 RST→PLAY 而非 RST→首個 RTP
#   mutantE   每次 RST 都重開空窗（R8 前的行為，覆寫已開的空窗）→ 連續停機只量到最後一段
#   mutantF   reconnects 遞增時機搬回握手（SVW-19 前的行為）→ 零 RTP 會話被記成已重連成功
#   mutantG   延後強切的上限退回 force_cut（k24 前的立即硬切行為）→ 4000 ms 就切在 P 幀
#
# 只動 session.rs 一個檔案、一個缺陷一版，mutant 一律從 .bak 重建，結束還原並以 sha256 核對。
# POSIX sh＋busybox 工具即可（devbox 的 rust 模板沒有 python3，也不得假設能裝套件）。
#
# 用法：  sh scripts/mutant_harness.sh              # good + 7 mutants
#         sh scripts/mutant_harness.sh naive        # 只跑指定 mutant
# 結束碼：0 全部檢出；1 有 mutant 逃逸或 good 不綠；3 原始碼還原失敗；4 harness／注入／編譯錯誤。

set -eu

HITS=""

cd "$(dirname "$0")/.."
# devbox 的 rust 模板（alpine）不在非互動 shell 的 PATH 裡放 cargo，自己補上。
command -v cargo >/dev/null 2>&1 || PATH="/usr/local/cargo/bin:$PATH"
export PATH
SRC=crates/rtsp-core/src/session.rs
SRC_RING=crates/camcore/src/ring.rs
BAK=/tmp/mutant-harness.orig.rs
BAK_RING=/tmp/mutant-harness.orig-ring.rs
LOGS=/tmp/mutant-harness-logs
TARGET=${1:-all}

[ -f "$SRC" ] || { echo "找不到 $SRC（要在 repo 根跑）"; exit 4; }
[ -f "$SRC_RING" ] || { echo "找不到 $SRC_RING（要在 repo 根跑）"; exit 4; }
mkdir -p "$LOGS"
cp "$SRC" "$BAK"
cp "$SRC_RING" "$BAK_RING"
SUM=$(sha256sum "$SRC" | cut -d' ' -f1)
SUM_RING=$(sha256sum "$SRC_RING" | cut -d' ' -f1)

restore() {
    cp "$BAK" "$SRC"
    cp "$BAK_RING" "$SRC_RING"
    NOW=$(sha256sum "$SRC" | cut -d' ' -f1)
    NOW_RING=$(sha256sum "$SRC_RING" | cut -d' ' -f1)
    if [ "$NOW" != "$SUM" ] || [ "$NOW_RING" != "$SUM_RING" ]; then
        echo "！！原始碼還原失敗：session.rs 或 ring.rs 已與跑之前的 sha256 不一致，請手動 git checkout"
        exit 3
    fi
}
trap restore EXIT INT TERM

# ---- 注入（每次從 $BAK 重建，確保只改一件事）------------------------------------
# 字面取代，不用正則：Rust 的程式碼片段全是 regex 特殊字元（括號、星號、`&&`），
# busybox awk 還會因 `{}` 直接報 "Invalid contents of {}"。
subline() {
    awk -v old="$2" -v new="$3" '
        { i = index($0, old)
          if (i > 0) { $0 = substr($0, 1, i - 1) new substr($0, i + length(old)) }
          print }' "$1"
}
anchor_check() {
    # 注入前先確認錨點在原始碼裡恰好一個；找不到就停，不得讓 mutant 靜默變成 good。
    case "$1" in
        naive|mutantA)
            N=$(grep -c '^        clock\.sleep(delay)\.await;$' "$BAK" || true)
            [ "$N" = 1 ] || { echo "錨點失效：$1 的 sleep(delay) 有 $N 處（應為 1）"; exit 4; } ;;
        mutantB)
            N=$(grep -cF '} else if d > 1 && d < 0x4000 {' "$BAK" || true)
            [ "$N" = 1 ] || { echo "錨點失效：mutantB 的缺口窗口有 $N 處（應為 1）"; exit 4; } ;;
        mutantC)
            N=$(grep -c '^            RstClass::TimedRecycle$' "$BAK" || true)
            [ "$N" = 1 ] || { echo "錨點失效：mutantC 的分桶行有 $N 處（應為 1）"; exit 4; } ;;
        mutantD)
            N=$(grep -cF '// N1（PM 2026-11-15 R7 裁定）' "$BAK" || true)
            [ "$N" = 1 ] || { echo "錨點失效：mutantD 的 N1 收口點註解有 $N 處（應為 1）"; exit 4; } ;;
        mutantE)
            N=$(grep -c '^        if gap_open_at.is_none() {$' "$BAK" || true)
            [ "$N" = 1 ] || { echo "錨點失效：mutantE 的「已開空窗不覆寫」有 $N 處（應為 1）"; exit 4; } ;;
        mutantG)
            # 延後強切的上限：正常是 4×force_cut，變異退回 force_cut＝立刻硬切。
            N=$(grep -c 'let limit = self\.cfg\.force_cut_limit_ms();' "$BAK" || true)
            [ "$N" = 1 ] || { echo "錨點失效：mutantG 的 force_cut_limit_ms 有 $N 處（應為 1）"; exit 4; } ;;
        mutantF)
            # 遞增時機的兩個錨點：握手成功的 `let session_is_reconnect = …` 與首個 RTP 分支的
            # `if session_is_reconnect {`（各自恰好一個）。
            N=$(grep -c '^        let session_is_reconnect = rep\.connects > 1;$' "$BAK" || true)
            [ "$N" = 1 ] || { echo "錨點失效：mutantF 的 session_is_reconnect 宣告有 $N 處（應為 1）"; exit 4; }
            M=$(grep -c '^                                if session_is_reconnect {$' "$BAK" || true)
            [ "$M" = 1 ] || { echo "錨點失效：mutantF 的「首個 RTP 才遞增」有 $M 處（應為 1）"; exit 4; } ;;
    esac
}

inject() {
    anchor_check "$1"
    case "$1" in
        naive)
            subline "$BAK" '        clock.sleep(delay).await;' \
                '        finished = "rst without reconnect".into();
        break '"'"'outer;' > "$SRC"
            N=$(grep -c "rst without reconnect" "$SRC" || true)
            [ "$N" = 1 ] || { echo "注入失敗：naive（找到 $N 處，應為 1）"; exit 4; }
            ;;
        mutantA)
            grep -v '^        clock\.sleep(delay)\.await;$' "$BAK" > "$SRC"
            N=$(grep -c "clock.sleep(delay)" "$SRC" || true)
            [ "$N" = 0 ] || { echo "注入失敗：mutantA 沒刪掉 sleep（$N 處）"; exit 4; }
            ;;
        mutantB)
            # 不容忍向後跳：把「向前缺口」的窗口條件改成「只要不連號就算缺口」。
            # 正確實作對 d>=0xC000（向後）只記 out_of_order；此變異會把它記成丟包與缺口。
            subline "$BAK" '} else if d > 1 && d < 0x4000 {' '} else if d != 1 {' > "$SRC"
            N=$(grep -c "} else if d != 1 {" "$SRC" || true)
            [ "$N" = 1 ] || { echo "注入失敗：mutantB（$N 處，應為 1）"; exit 4; }
            ;;
        mutantC)
            # 整行相等才換：三個 match arm 的字面是 `            RstClass::TimedRecycle => ...`，
            # 用 subline（前綴取代）會把它們一併改到，那就不是「只改一件事」。
            awk '{ if ($0 == "            RstClass::TimedRecycle") print "            RstClass::MidStream"; else print }' "$BAK" > "$SRC"
            # 注入點只有一處：分桶那行（`RstClass::TimedRecycle` 獨立成行）。改後該行應消失，
            # 而 match arm 的 `RstClass::TimedRecycle =>` 三處要原封不動，證明只改了判別、沒改動作。
            N=$(grep -c '^            RstClass::TimedRecycle$' "$SRC" || true)
            [ "$N" = 0 ] || { echo "注入失敗：mutantC 仍留有分桶行（$N）"; exit 4; }
            M=$(grep -c 'RstClass::TimedRecycle =>' "$SRC" || true)
            [ "$M" = 3 ] || { echo "注入失敗：mutantC 波及 match arm（$M 個，應為 3）"; exit 4; }
            ;;
        mutantD)
            # N1 前的行為：在 PLAY 段就把 gap 收口（take gap_open_at），
            # 於是空窗量到 RST→PLAY（本 fixture 為 1003 ms），而非 RST→首個 RTP（1500 ms）。
            # 單行注入：整段收口邏輯壓成一行，插在 PLAY 段 `backoff = ...` 之後的 N1 註解之前。
            ins='if let Some(at) = gap_open_at.take() { let now = clock.now_ms(); let ms = now.saturating_sub(at); rep.gaps.push(StreamGap { at_ms: at.saturating_sub(t0), ms, reason: gap_reason.clone() }); rep.max_gap_ms = rep.max_gap_ms.max(ms); on_event(Event::Reconnected { after_ms: now.saturating_sub(t0), gap_ms: ms }); }'
            awk -v ins="$ins" '
                { if (index($0, "// N1（PM 2026-11-15 R7 裁定）") > 0) print "        " ins
                  print }' "$BAK" > "$SRC"
            N=$(grep -c 'gap_open_at.take()' "$SRC" || true)
            [ "$N" = 2 ] || { echo "注入失敗：mutantD 應有 2 處 take（PLAY 與首個 RTP），實得 $N"; exit 4; }
            ;;
        mutantG)
            # k24 前的行為：上限等於 force_cut，段滿 force_cut 就立刻硬切在 P 幀。
            subline "$BAK" 'let limit = self.cfg.force_cut_limit_ms();' \
                'let limit = self.cfg.force_cut_ms;' > "$SRC"
            N=$(grep -c 'let limit = self\.cfg\.force_cut_ms;' "$SRC" || true)
            [ "$N" = 1 ] || { echo "注入失敗：mutantG 上限沒改成 force_cut（$N 處，應為 1）"; exit 4; }
            M=$(grep -c 'force_cut_limit_ms()' "$SRC" || true)
            [ "$M" = 0 ] || { echo "注入失敗：mutantG 仍呼叫 force_cut_limit_ms（$M 處）"; exit 4; } ;;
        mutantF)
            # SVW-19 前的行為：握手成功就 `rep.reconnects += 1`，首個 RTP 分支不再遞增。
            # 於是「PLAY 200 但永不送 RTP」的會話也被記成重連成功（零 RTP 情境 5 段連線得 4）。
            ins='        if session_is_reconnect { rep.reconnects += 1; }'
            awk -v ins="$ins" '
                { if ($0 == "        let session_is_reconnect = rep.connects > 1;") { print; print ins; next }
                  if ($0 == "                                if session_is_reconnect {") { print "                                if false {"; next }
                  print }' "$BAK" > "$SRC"
            N=$(grep -c '^        if session_is_reconnect { rep\.reconnects += 1; }$' "$SRC" || true)
            [ "$N" = 1 ] || { echo "注入失敗：mutantF 握手處沒有遞增（$N 處，應為 1）"; exit 4; }
            M=$(grep -c '^                                if false {$' "$SRC" || true)
            [ "$M" = 1 ] || { echo "注入失敗：mutantF 首個 RTP 分支仍在遞增（$M 處，應為 1）"; exit 4; } ;;
        mutantE)
            # R8 前的行為：每次 RST 都 gap_open_at = Some(now)／gap_reason = reason，
            # 於是「RST → PLAY 成功但零 RTP → RST」的前段停機被覆寫掉，空窗只量最後一段。
            awk '{ if ($0 == "        if gap_open_at.is_none() {") print "        if true {"; else print }' "$BAK" > "$SRC"
            N=$(grep -c '^        if gap_open_at.is_none() {$' "$SRC" || true)
            [ "$N" = 0 ] || { echo "注入失敗：mutantE 仍留有 is_none 判斷（$N）"; exit 4; }
            M=$(grep -c '^        if gap_open_at.is_none() {$' "$SRC" || true)
            [ "$M" = 0 ] || { echo "注入失敗：mutantE 注入無效"; exit 4; } ;;
        good)
            cp "$BAK" "$SRC"
            ;;
        *)
            echo "unknown mutant: $1"; exit 4 ;;
    esac
}

# 指定測試（fn 名字，| 分隔任一即算命中）。這是「檢出」的定義，不是「有任何測試紅」。
require_of() {
    case "$1" in
        naive)   echo 'timed_recycle_reconnects_immediately_with_zero_backoff|midstream_rst_resets_backoff_after_rtp_success' ;;
        mutantA) echo 'midstream_rst_doubles_when_reconnect_gets_no_rtp|immediate_reject_uses_ladder_then_stops_after_five' ;;
        mutantB) echo 'backward_seq_jump_within_session_is_not_counted_as_loss' ;;
        mutantC) echo 'timed_recycle_reconnects_immediately_with_zero_backoff' ;;
        mutantD) echo 'replays_fixture_and_matches_declared_expectations|gap_is_measured_from_rst_to_first_rtp' ;;
        mutantE) echo 'midstream_rst_doubles_when_reconnect_gets_no_rtp' ;;
        mutantF) echo 'play_ok_without_any_rtp_records_no_gap_and_no_reconnected' ;;
        mutantG) echo 'ring::tests::forced_cut_defers_until_idr_without_losing_frames' ;;
        good)    echo '' ;;
    esac
}

# ---- 跑一輪 --------------------------------------------------------------------
run_one() {
    NAME=$1
    LOG="$LOGS/$NAME.log"
    case "$NAME" in
        mutantG) SRC=crates/camcore/src/ring.rs; BAK=/tmp/mutant-harness.orig-ring.rs ;;
        *)       SRC=crates/rtsp-core/src/session.rs; BAK=/tmp/mutant-harness.orig.rs ;;
    esac
    inject "$NAME"
    if [ "$NAME" = good ]; then
        { cargo test -p rtsp-core --no-fail-fast
          cargo test -p camcore --lib --no-fail-fast; } >"$LOG" 2>&1 || true
    elif [ "$NAME" = mutantG ]; then
        cargo test -p camcore --lib 'ring::tests::forced_cut_defers_until_idr_without_losing_frames' --no-fail-fast >"$LOG" 2>&1 || true
    else
        cargo test -p rtsp-core --no-fail-fast >"$LOG" 2>&1 || true
    fi
    if grep -qE 'error\[E[0-9]+\]|error: could not compile|error: unexpected' "$LOG"; then
        echo "  $NAME: COMPILE_ERROR（注入後編譯不過，不算檢出）— 見 $LOG"
        HITS="$HITS $NAME:compile_error"
        return
    fi
    if grep -q 'test result: FAILED' "$LOG"; then
        VERDICT=failed
    elif grep -q 'test result: ok' "$LOG"; then
        VERDICT=ok
    else
        echo "  $NAME: 讀不到 test result（harness 自身問題）— 見 $LOG"
        HITS="$HITS $NAME:noresult"
        return
    fi
    REQ=$(require_of "$NAME")
    if [ "$NAME" = good ]; then
        if [ "$VERDICT" = ok ]; then
            PASSED=$(grep -o 'test result: ok\..*' "$LOG" | awk '{s+=$4} END {print s+0}')
            echo "  good: 全綠（$PASSED 個測試通過）"
            HITS="$HITS good:ok"
        else
            echo "  good: 不綠！未變異的原始碼就有測試失敗，後續檢出無意義 — 見 $LOG"
            HITS="$HITS good:NOTGREEN"
        fi
        return
    fi
    if [ "$VERDICT" = failed ] && grep -E "^test ($REQ) \.\.\. FAILED" "$LOG" >/dev/null; then
        WHICH=$(grep -E "^test ($REQ) \.\.\. FAILED" "$LOG" | head -1 | sed 's/^test \([^ ]*\).*/\1/')
        echo "  $NAME: 檢出（指定測試紅：$WHICH）"
        HITS="$HITS $NAME:caught"
    elif [ "$VERDICT" = failed ]; then
        echo "  $NAME: 有測試紅，但不是指定的那個（$REQ）→ 不算檢出"
        HITS="$HITS $NAME:wrongtest"
    else
        echo "  $NAME: 逃逸——注入錯誤實作後測試仍全綠，代表這條規則沒有被測到"
        HITS="$HITS $NAME:escaped"
    fi
}

echo "== 變異測試 harness（§11.2 三類 RST 觸發）=="
echo "repo: $(pwd)"
cargo --version

if [ "$TARGET" = all ]; then
    for m in good naive mutantA mutantB mutantC mutantD mutantE mutantF mutantG; do
        echo "-- $m"
        run_one "$m"
    done
else
    echo "-- $TARGET"
    run_one "$TARGET"
fi

echo
echo "== 結果 =="
echo "$HITS" | tr ' ' '\n' | sed '/^$/d'

RC=0
echo "$HITS" | grep -q 'NOTGREEN' && RC=1
echo "$HITS" | grep -qE 'escaped|wrongtest' && RC=1
echo "$HITS" | grep -qE 'compile_error|noresult' && RC=4
[ "$RC" = 0 ] || echo "結論：未達標（exit $RC）"
[ "$RC" = 0 ] && echo "結論：baseline 全綠，7 個 mutant 全部由指定測試檢出"
exit "$RC"
