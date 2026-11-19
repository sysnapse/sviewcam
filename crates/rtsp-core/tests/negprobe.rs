//! Verifier 負向驗證用（臨時分支 verify/negprobe-0x4001，勿合併）。
//! 目的：確認 CI 的 `cargo test --workspace` 步驟真的會因為一個紅測試而失敗，
//! 而不是只在 log 裡紅、workflow 仍然 success。

#[test]
fn negprobe_must_be_red_in_ci() {
    panic!("negprobe：這個測試故意失敗，用來驗證 CI 會紅");
}
