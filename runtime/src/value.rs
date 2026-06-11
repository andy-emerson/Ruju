//! Boxing and unboxing, built on the object model and the bootstrapped types.
//!
//! A boxed value is a real tagged heap object whose header points at its
//! DataType, mirroring Julia's `jl_box_int64` and the singleton `jl_nothing`.
//! The allocation size is driven by the type's declared layout size, as in
//! `jl_gc_alloc(..., jl_datatype_size(type), type)`.

use crate::object::{self, Value};
use crate::types::{self, id};

/// Box an `Int64`: allocate a tagged `Int64` object and store the payload.
pub fn box_int(x: i64) -> Value {
    let t = types::builtin(id::INT64);
    let v = object::alloc(t, types::size_of(t) as usize);
    if v.is_null() {
        return Value::NULL;
    }
    unsafe {
        *object::data_ptr::<i64>(v) = x;
    }
    v
}

/// Read the `Int64` payload of `v`. The caller guarantees `v` is a non-null
/// `Int64`.
pub fn unbox_int(v: Value) -> i64 {
    unsafe { *object::data_ptr::<i64>(v) }
}

/// Box a `Float64` (an 8-byte tagged object).
pub fn box_float64(x: f64) -> Value {
    let t = types::builtin(id::FLOAT64);
    let v = object::alloc(t, types::size_of(t) as usize);
    if v.is_null() {
        return Value::NULL;
    }
    unsafe {
        *object::data_ptr::<f64>(v) = x;
    }
    v
}

/// Read the `Float64` payload of `v`. The caller guarantees `v` is a non-null
/// `Float64`.
pub fn unbox_float64(v: Value) -> f64 {
    unsafe { *object::data_ptr::<f64>(v) }
}

/// Box a `Bool` (a one-byte tagged object).
pub fn box_bool(b: bool) -> Value {
    let t = types::builtin(id::BOOL);
    let v = object::alloc(t, types::size_of(t) as usize);
    if v.is_null() {
        return Value::NULL;
    }
    unsafe {
        *object::data_ptr::<u8>(v) = b as u8;
    }
    v
}

/// Read the `Bool` payload of `v`. The caller guarantees `v` is a non-null
/// `Bool`.
pub fn unbox_bool(v: Value) -> bool {
    unsafe { *object::data_ptr::<u8>(v) != 0 }
}

/// The `nothing` singleton (the sole instance of `Nothing`).
#[allow(dead_code)] // exposed via the C ABI; first Rust consumer is the interpreter
pub fn nothing() -> Value {
    Value(types::nothing_instance())
}
