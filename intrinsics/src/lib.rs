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

/// IEEE-754 double addition (`add_float`).
#[inline]
pub fn add_float(a: f64, b: f64) -> f64 {
    a + b
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
    }
}
