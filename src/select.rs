//! Account selection — the ONE implementation shared by launcher and watcher.
//! (The bash version had this logic three times, as divergent jq programs.)
//!
//! Two predicates and one ordering, all threshold-parameterised:
//!
//! - [`usable`]  — fail OPEN: may this account keep serving? Unknown usage
//!   (mid-poll `null`) excuses an account, it never condemns it.
//! - [`electable`] — fail CLOSED: may this account be *chosen* (borrowed for
//!   a mapped dir, rotated onto, counted as a fallback)? Requires health we
//!   can actually see, on every meter it exposes — a blind hand-off is worse
//!   than the next tier.
//! - [`rank`] — windowed quota first, spend-billed last. Real dollars are the
//!   last resort, in the launcher exactly as in the watcher.

use crate::cswap::{Account, List, UsageStatus};

/// Window headroom: status ok, and if windowed, both windows below the
/// threshold. This alone is the watcher's notion of "not exhausted" — a
/// spend cap never rotates a live session away, it only stops new routing.
pub fn windows_ok(a: &Account, thr: f64) -> bool {
    a.usage_status == UsageStatus::Ok && (!a.windowed() || a.worst().is_some_and(|w| w < thr))
}

/// May this account keep serving? Spend/unknown meters fail open.
pub fn usable(a: &Account, thr: f64, spend_thr: f64) -> bool {
    windows_ok(a, thr) && a.spend_pct().is_none_or(|p| p < spend_thr)
}

/// May this account be chosen? Same thresholds, but usage must be KNOWN.
/// Does NOT check `disabled`: a disabled account still serves its own mapped
/// dirs, so callers that must not borrow it filter that themselves.
pub fn electable(a: &Account, thr: f64, spend_thr: f64) -> bool {
    a.usage.is_some() && usable(a, thr, spend_thr)
}

/// Sort key: windowed accounts (subscription quota) before spend-billed ones,
/// each by how much is already used.
fn rank(a: &Account) -> (u8, f64) {
    if a.windowed() {
        (0, a.worst().unwrap_or(0.0))
    } else {
        (1, a.spend_pct().unwrap_or(0.0))
    }
}

fn best<'a>(mut candidates: impl Iterator<Item = &'a Account>) -> Option<&'a Account> {
    candidates.next().map(|first| {
        candidates.fold(first, |best, a| {
            let (ka, kb) = (rank(a), rank(best));
            if ka.0 < kb.0 || (ka.0 == kb.0 && ka.1 < kb.1) {
                a
            } else {
                best
            }
        })
    })
}

/// Mapped dir whose pinned account is dry: any other enabled electable
/// account may serve it, freshest windowed quota first.
pub fn pick_alt<'a>(
    list: &'a List,
    skip_email: &str,
    thr: f64,
    spend_thr: f64,
) -> Option<&'a Account> {
    best(
        list.accounts
            .iter()
            .filter(|a| a.email != skip_email && !a.disabled && electable(a, thr, spend_thr)),
    )
}

/// Unmapped dir with a dry default login: launch anyway if any enabled
/// account could replace it — the cswap-auto daemon moves the default login,
/// not us.
pub fn any_enabled_electable(list: &List, thr: f64, spend_thr: f64) -> bool {
    list.accounts
        .iter()
        .any(|a| !a.disabled && electable(a, thr, spend_thr))
}

/// Watcher: target account number for an exhausted session account, or
/// `None` (healthy, unknown, or nothing left — hold).
///
/// Exhausted = status not ok, or a window at/over the threshold; a spend cap
/// alone never evicts a live session.
pub fn pick_target(list: &List, current: i64, thr: f64, spend_thr: f64) -> Option<i64> {
    if windows_ok(list.by_number(current)?, thr) {
        return None;
    }
    best(
        list.accounts
            .iter()
            .filter(|a| a.number != current && !a.disabled && electable(a, thr, spend_thr)),
    )
    .map(|a| a.number)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Same fixture as the old tests/pick_target_test.sh:
    // 1 = windowed near-dry, 2 = windowed fresh, 3 = dead+disabled,
    // 4 = spend-only, 5 = unknown usage.
    fn fixture() -> List {
        serde_json::from_str(
            r#"{"schemaVersion":1,"activeAccountNumber":1,"accounts":[
 {"number":1,"email":"a@sia.com","active":true,"usageStatus":"ok",
  "usage":{"fiveHour":{"pct":97.0,"resetsAt":"x"},"sevenDay":{"pct":40.0,"resetsAt":"x"}}},
 {"number":2,"email":"b@qm.io","active":false,"usageStatus":"ok",
  "usage":{"fiveHour":{"pct":10.0,"resetsAt":"x"},"sevenDay":{"pct":20.0,"resetsAt":"x"}}},
 {"number":3,"email":"c@dead.io","active":false,"usageStatus":"token_expired","usage":null,"disabled":true},
 {"number":4,"email":"d@api.com","active":false,"usageStatus":"ok",
  "usage":{"spend":{"used":14.12,"limit":100.0,"pct":14.12}}},
 {"number":5,"email":"e@unknown.io","active":false,"usageStatus":"ok","usage":null}
]}"#,
        )
        .unwrap()
    }

    const THR: f64 = 95.0;
    const STHR: f64 = 97.0;

    fn set_5h(list: &mut List, idx: usize, pct: f64) {
        list.accounts[idx]
            .usage
            .as_mut()
            .unwrap()
            .five_hour
            .as_mut()
            .unwrap()
            .pct = pct;
    }

    fn set_spend(list: &mut List, idx: usize, pct: f64) {
        list.accounts[idx]
            .usage
            .as_mut()
            .unwrap()
            .spend
            .as_mut()
            .unwrap()
            .pct = Some(pct);
    }

    #[test]
    fn exhausted_prefers_windowed_over_spend() {
        assert_eq!(pick_target(&fixture(), 1, THR, STHR), Some(2));
    }

    #[test]
    fn healthy_stays_put() {
        assert_eq!(pick_target(&fixture(), 2, THR, STHR), None);
    }

    #[test]
    fn dead_token_goes_to_best_windowed() {
        assert_eq!(pick_target(&fixture(), 3, THR, STHR), Some(2));
    }

    #[test]
    fn spend_is_last_resort_and_unknown_never_picked() {
        let mut list = fixture();
        set_5h(&mut list, 1, 99.0);
        assert_eq!(pick_target(&list, 1, THR, STHR), Some(4));
    }

    #[test]
    fn nothing_left_holds() {
        let mut list = fixture();
        set_5h(&mut list, 1, 99.0);
        list.accounts[3].usage_status = UsageStatus::NotOk;
        assert_eq!(pick_target(&list, 1, THR, STHR), None);
    }

    #[test]
    fn spend_capped_account_is_not_a_target() {
        let mut list = fixture();
        set_5h(&mut list, 1, 99.0);
        set_spend(&mut list, 3, 98.0);
        assert_eq!(pick_target(&list, 1, THR, STHR), None);
    }

    #[test]
    fn spend_cap_alone_never_evicts_a_session() {
        let mut list = fixture();
        set_spend(&mut list, 3, 98.0);
        // Account 4 is capped but its session holds (no windows breached).
        assert_eq!(pick_target(&list, 4, THR, STHR), None);
    }

    #[test]
    fn unknown_session_account_holds() {
        assert_eq!(pick_target(&fixture(), 42, THR, STHR), None);
    }

    #[test]
    fn usable_fails_open_electable_fails_closed() {
        let mut list = fixture();
        assert!(usable(&list.accounts[0], 99.0, STHR)); // 97 < launcher's 99
        assert!(!usable(&list.accounts[0], 95.0, STHR));
        assert!(!usable(&list.accounts[2], 99.0, STHR)); // dead token
        assert!(usable(&list.accounts[3], 99.0, STHR)); // spend-only, low
        // Unknown usage: keeps serving, never gets chosen.
        assert!(usable(&list.accounts[4], 99.0, STHR));
        assert!(!electable(&list.accounts[4], 99.0, STHR));
        // At/over the spend threshold the account stops counting either way.
        set_spend(&mut list, 3, 98.0);
        assert!(!usable(&list.accounts[3], 99.0, STHR));
        assert!(!electable(&list.accounts[3], 99.0, STHR));
    }

    #[test]
    fn alt_pick_prefers_windowed_quota_over_real_dollars() {
        let list = fixture();
        // Borrowing for a dir pinned to account 2: windowed 1 (97% but under
        // the launcher's 99 line) beats the spend-billed 4, and the
        // unknown-usage 5 is never borrowed.
        assert_eq!(pick_alt(&list, "b@qm.io", 99.0, STHR).unwrap().number, 1);
        // Freshest windowed quota wins when several qualify.
        assert_eq!(pick_alt(&list, "zz@none", 99.0, STHR).unwrap().number, 2);
    }

    #[test]
    fn alt_pick_falls_to_spend_only_when_no_window_has_room() {
        let mut list = fixture();
        set_5h(&mut list, 0, 99.5);
        set_5h(&mut list, 1, 99.5);
        assert_eq!(pick_alt(&list, "zz@none", 99.0, STHR).unwrap().number, 4);
    }

    #[test]
    fn any_enabled_electable_ignores_disabled_and_unknown() {
        let mut list = fixture();
        assert!(any_enabled_electable(&list, 99.0, STHR));
        for a in &mut list.accounts {
            if !a.disabled {
                a.usage = None; // every enabled account mid-poll
            }
        }
        assert!(!any_enabled_electable(&list, 99.0, STHR));
    }
}
