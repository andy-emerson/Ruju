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

/// Box a `Bool`: return the `true`/`false` permbox (`jl_box_bool` returns
/// `jl_true`/`jl_false` and never allocates).
pub fn box_bool(b: bool) -> Value {
    let bs = types::builtins();
    Value(if b { bs.true_instance } else { bs.false_instance })
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

// --- remaining primitive boxings ---------------------------------------------
//
// One box/unbox pair per primitive width, as `jl_box_*`/`jl_unbox_*`
// (`datatype.c`). No permbox caches yet (Julia pre-boxes small ints, all
// Int8/UInt8, and ASCII Chars — recorded in `design/implementation.md`).
// Consumers arrive with the conversion intrinsics and wider source literals;
// until then these serve tests and the ABI.

macro_rules! box_prim {
    ($box_name:ident, $unbox_name:ident, $ty:ty, $type_id:expr) => {
        #[allow(dead_code)]
        pub fn $box_name(x: $ty) -> Value {
            let t = types::builtin($type_id);
            let v = object::alloc(t, types::size_of(t) as usize);
            if v.is_null() {
                return Value::NULL;
            }
            unsafe {
                *object::data_ptr::<$ty>(v) = x;
            }
            v
        }

        /// Read the payload. The caller guarantees the value's type.
        #[allow(dead_code)]
        pub fn $unbox_name(v: Value) -> $ty {
            unsafe { *object::data_ptr::<$ty>(v) }
        }
    };
}

box_prim!(box_int8, unbox_int8, i8, id::INT8);
box_prim!(box_int16, unbox_int16, i16, id::INT16);
box_prim!(box_int32, unbox_int32, i32, id::INT32);
box_prim!(box_uint8, unbox_uint8, u8, id::UINT8);
box_prim!(box_uint16, unbox_uint16, u16, id::UINT16);
box_prim!(box_uint32, unbox_uint32, u32, id::UINT32);
box_prim!(box_uint64, unbox_uint64, u64, id::UINT64);
box_prim!(box_float32, unbox_float32, f32, id::FLOAT32);
// A Char's payload is Julia's 4-byte representation, stored as the raw u32.
box_prim!(box_char, unbox_char, u32, id::CHAR);
