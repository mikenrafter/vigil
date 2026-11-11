//! Early attention (specs/auto-interval.md AI5): when a watch has run longer
//! than its owner expected, or its inputs have gone quiet while its process
//! still runs, the call ends early with one `attention` event.
//!
//! RED phase: stubs only. Tests below and in `auto_interval_tests` define the
//! contract.

/// AI5: "1.5x or 9 minutes over, whichever comes first" (build-loop budget rule).
pub const OVERDUE_EXTRA_CAP_S: u64 = 540;
/// AI5 / D3: the stall limit applied to a long window when the watch sets
/// neither `expect_s` nor `stall_s`.
pub const DEFAULT_STALL_S: u64 = 900;
/// AI5 (A1): a call window above this is "long".
pub const LONG_WINDOW_S: u64 = 270;

/// The elapsed time since create at which `overdue` fires, in whole seconds:
/// `min(1.5 x expect_s, expect_s + 540)` (A15: inclusive; odd `expect_s` is
/// not pinned).
pub fn overdue_after_s(expect_s: u64) -> u64 {
    expect_s // RED: wrong on purpose
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overdue_threshold_is_one_and_a_half_times_or_nine_minutes_over_whichever_is_first() {
        // why: AI5 / D2 (the build-loop budget rule). Hand-computed oracle,
        //      all expect_s even so 1.5x is exact (A15):
        //        60   -> min(90, 600)     = 90
        //        100  -> min(150, 640)    = 150
        //        1000 -> min(1500, 1540)  = 1500
        //        1078 -> min(1617, 1618)  = 1617   (1.5x still smaller by 1)
        //        1080 -> min(1620, 1620)  = 1620   (crossover)
        //        1082 -> min(1623, 1622)  = 1622   (+540 smaller by 1)
        //        2000 -> min(3000, 2540)  = 2540
        //        3600 -> min(5400, 4140)  = 4140
        //      The two neighbours of the crossover kill a swapped min/max and
        //      a mistyped 540.
        // proves: CLAIM(AI5-threshold)
        let cases = [(60, 90), (100, 150), (1000, 1500), (1078, 1617), (1080, 1620), (1082, 1622), (2000, 2540), (3600, 4140)];
        for (expect_s, expected) in cases {
            assert_eq!(overdue_after_s(expect_s), expected, "expect_s {expect_s}");
        }
    }

    #[test]
    fn the_shipped_attention_constants_are_the_spec_numbers() {
        // why: AI5 / D3 fix 540 s, 900 s and the 270 s "long" line. A change
        //      detector on purpose: each is a spec number, not an
        //      implementation choice.
        // proves: CLAIM(AI5-threshold)
        assert_eq!((OVERDUE_EXTRA_CAP_S, DEFAULT_STALL_S, LONG_WINDOW_S), (540, 900, 270));
    }
}
