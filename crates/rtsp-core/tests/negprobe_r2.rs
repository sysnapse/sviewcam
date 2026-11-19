//! Verifier 第二輪負向驗證用（臨時分支 verify/negprobe-r2，勿合併）。
//! 目的：在 head 4c39746（cache key 已綁版控 Cargo.lock）上確認 CI 仍會因為一個紅測試而失敗，
//! 而不是只看 log 紅、workflow 仍然 success。

#[test]
fn negprobe_r2_must_be_red_in_ci() {
    panic!("negprobe-r2：這個測試故意失敗，用來驗證 CI 會紅");
}
