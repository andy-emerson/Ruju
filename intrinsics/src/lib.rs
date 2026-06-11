//! Ruju runtime intrinsics.
//!
//! Intrinsics are the primitive arithmetic, memory, and type operations the
//! runtime and (later) AOT-compiled code build on. They are pure and free of
//! runtime state, so this crate is `no_std`. The skeleton provides a single
//! integer-add intrinsic; the full set will mirror Julia's `Core.Intrinsics`.
#![no_std]

/// Two's-complement integer addition (`add_int`).
#[inline]
pub fn add_int(a: i64, b: i64) -> i64 {
    a.wrapping_add(b)
}

/// Two's-complement integer subtraction (`sub_int`).
#[inline]
pub fn sub_int(a: i64, b: i64) -> i64 {
    a.wrapping_sub(b)
}

/// Two's-complement integer multiplication (`mul_int`).
#[inline]
pub fn mul_int(a: i64, b: i64) -> i64 {
    a.wrapping_mul(b)
}

/// Signed integer less-than (`slt_int`).
#[inline]
pub fn slt_int(a: i64, b: i64) -> bool {
    a < b
}

/// Signed integer less-than-or-equal (`sle_int`).
#[inline]
pub fn sle_int(a: i64, b: i64) -> bool {
    a <= b
}

/// Bitwise integer equality (`eq_int`).
#[inline]
pub fn eq_int(a: i64, b: i64) -> bool {
    a == b
}

/// Two's-complement negation (`neg_int`).
#[inline]
pub fn neg_int(a: i64) -> i64 {
    a.wrapping_neg()
}

/// Checked signed division (`checked_sdiv_int`): `None` is Julia's
/// `DivideError` — division by zero, or `typemin ÷ -1`
/// (`runtime_intrinsics.c:1251`). The unchecked `sdiv_int` is hardware
/// division whose error cases Julia guards before reaching it.
#[inline]
pub fn checked_sdiv_int(a: i64, b: i64) -> Option<i64> {
    if b == 0 || (a == i64::MIN && b == -1) {
        None
    } else {
        Some(a / b)
    }
}

/// Checked signed remainder (`checked_srem_int`): `None` on division by
/// zero; `typemin % -1` is 0, not an error (as in Julia).
#[inline]
pub fn checked_srem_int(a: i64, b: i64) -> Option<i64> {
    if b == 0 {
        None
    } else if a == i64::MIN && b == -1 {
        Some(0)
    } else {
        Some(a % b)
    }
}

/// Bitwise and (`and_int`).
#[inline]
pub fn and_int(a: i64, b: i64) -> i64 {
    a & b
}

/// Bitwise or (`or_int`).
#[inline]
pub fn or_int(a: i64, b: i64) -> i64 {
    a | b
}

/// Bitwise xor (`xor_int`).
#[inline]
pub fn xor_int(a: i64, b: i64) -> i64 {
    a ^ b
}

/// Bitwise not (`not_int`).
#[inline]
pub fn not_int(a: i64) -> i64 {
    !a
}

/// Shift left (`shl_int`): a count at or beyond the width yields 0; the
/// count is treated as unsigned (`shl_op`, `runtime_intrinsics.c:1569`).
#[inline]
pub fn shl_int(a: i64, b: i64) -> i64 {
    if (b as u64) >= 64 {
        0
    } else {
        ((a as u64) << b) as i64
    }
}

/// Logical shift right (`lshr_int`): zero-fill; a count at or beyond the
/// width yields 0 (`lshr_op`, `runtime_intrinsics.c:1571`).
#[inline]
pub fn lshr_int(a: i64, b: i64) -> i64 {
    if (b as u64) >= 64 {
        0
    } else {
        ((a as u64) >> b) as i64
    }
}

/// Arithmetic shift right (`ashr_int`): sign-fill; a negative or
/// out-of-width count yields the sign word (`ashr_op`,
/// `runtime_intrinsics.c:1573`).
#[inline]
pub fn ashr_int(a: i64, b: i64) -> i64 {
    if !(0..64).contains(&b) {
        a >> 63
    } else {
        a >> b
    }
}

/// Unsigned less-than (`ult_int`).
#[inline]
pub fn ult_int(a: i64, b: i64) -> bool {
    (a as u64) < (b as u64)
}

/// Unsigned less-than-or-equal (`ule_int`).
#[inline]
pub fn ule_int(a: i64, b: i64) -> bool {
    (a as u64) <= (b as u64)
}

/// Signed integer to double (`sitofp`).
#[inline]
pub fn sitofp(a: i64) -> f64 {
    a as f64
}

/// Double to signed integer (`fptosi`): the C casts and leaves out-of-range
/// input implementation-defined ("an arbitrary value" per Julia's
/// `unsafe_trunc`); this port picks Rust's saturating cast (NaN → 0).
#[inline]
pub fn fptosi(a: f64) -> i64 {
    a as i64
}

/// IEEE-754 double addition (`add_float`).
#[inline]
pub fn add_float(a: f64, b: f64) -> f64 {
    a + b
}

/// IEEE-754 double division (`div_float`): total — x/0 is ±Inf or NaN.
#[inline]
pub fn div_float(a: f64, b: f64) -> f64 {
    a / b
}

/// IEEE-754 double negation (`neg_float`).
#[inline]
pub fn neg_float(a: f64) -> f64 {
    -a
}

/// Floating remainder (`rem_float` = `fmod`, `runtime_intrinsics.c:1363`);
/// Rust's `%` on floats is fmod.
#[inline]
pub fn rem_float(a: f64, b: f64) -> f64 {
    a % b
}

/// IEEE-754 double subtraction (`sub_float`).
#[inline]
pub fn sub_float(a: f64, b: f64) -> f64 {
    a - b
}

/// IEEE-754 double multiplication (`mul_float`).
#[inline]
pub fn mul_float(a: f64, b: f64) -> f64 {
    a * b
}

/// IEEE-754 double less-than (`lt_float`).
#[inline]
pub fn lt_float(a: f64, b: f64) -> bool {
    a < b
}

/// IEEE-754 double less-than-or-equal (`le_float`).
#[inline]
pub fn le_float(a: f64, b: f64) -> bool {
    a <= b
}

/// IEEE-754 double equality (`eq_float`).
#[inline]
pub fn eq_float(a: f64, b: f64) -> bool {
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_int_semantics() {
        assert_eq!(add_int(2, 3), 5);
        assert_eq!(add_int(i64::MAX, 1), i64::MIN); // wrapping
        assert_eq!(sub_int(3, 5), -2);
        assert_eq!(mul_int(6, 7), 42);
        assert!(slt_int(-1, 0) && !slt_int(0, 0));
        assert!(eq_int(9, 9) && !eq_int(9, 8));
        assert_eq!(neg_int(i64::MIN), i64::MIN); // wrapping
    }

    #[test]
    fn division_and_remainder() {
        assert_eq!(checked_sdiv_int(7, 2), Some(3)); // truncating
        assert_eq!(checked_sdiv_int(-7, 2), Some(-3));
        assert_eq!(checked_sdiv_int(1, 0), None); // DivideError
        assert_eq!(checked_sdiv_int(i64::MIN, -1), None); // DivideError
        assert_eq!(checked_srem_int(7, 2), Some(1));
        assert_eq!(checked_srem_int(-7, 2), Some(-1)); // sign of dividend
        assert_eq!(checked_srem_int(1, 0), None);
        assert_eq!(checked_srem_int(i64::MIN, -1), Some(0)); // not an error
    }

    #[test]
    fn shifts_match_julia_overflow_semantics() {
        assert_eq!(shl_int(1, 10), 1024);
        assert_eq!(shl_int(1, 64), 0); // count >= width
        assert_eq!(shl_int(1, -1), 0); // negative count is huge unsigned
        assert_eq!(lshr_int(-8, 1), 0x7FFF_FFFF_FFFF_FFFC); // zero-fill
        assert_eq!(lshr_int(-1, 64), 0);
        assert_eq!(ashr_int(-8, 1), -4); // sign-fill
        assert_eq!(ashr_int(-8, 100), -1); // out-of-width: sign word
        assert_eq!(ashr_int(8, 100), 0);
        assert_eq!(ashr_int(-8, -1), -1); // negative count: sign word
    }

    #[test]
    fn float_and_conversions() {
        assert_eq!(div_float(1.0, 2.0), 0.5);
        assert_eq!(div_float(1.0, 0.0), f64::INFINITY); // total, no error
        assert_eq!(rem_float(5.5, 2.0), 1.5); // fmod
        assert_eq!(neg_float(1.5), -1.5);
        assert_eq!(sitofp(7), 7.0);
        assert_eq!(fptosi(3.9), 3); // truncates toward zero
        assert_eq!(fptosi(-3.9), -3);
        assert!(ult_int(1, -1) && !ult_int(-1, 1)); // -1 is huge unsigned
        assert!(ule_int(0, 0));
    }
}
