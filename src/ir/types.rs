use std::fmt;
use std::fmt::Formatter;
use wasm_encoder::AbstractHeapType;
use wasmparser::{ConstExpr, GlobalType, Operator, RefType, ValType};

#[derive(Debug, Clone)]
pub struct Global {
    pub ty: GlobalType,
    // TODO: We might want to build our own representation of econstant expression
    // seee https://docs.rs/walrus/latest/src/walrus/const_expr.rs.html#13-22
    pub init_expr: InitExpr,
}

#[derive(Debug, Clone)]
pub struct DataSegment<'a> {
    /// The kind of data segment.
    pub kind: DataSegmentKind<'a>,
    /// The data of the data segment.
    pub data: &'a [u8],
}

/// The kind of data segment.
#[derive(Debug, Clone)]
pub enum DataSegmentKind<'a> {
    /// The data segment is passive.
    Passive,
    /// The data segment is active.
    Active {
        /// The memory index for the data segment.
        memory_index: u32,
        /// The initialization operator for the data segment.
        offset_expr: wasmparser::ConstExpr<'a>,
    },
}

#[derive(Debug, Clone)]
pub enum ElementKind<'a> {
    Passive,
    Active {
        table_index: Option<u32>,
        offset_expr: wasmparser::ConstExpr<'a>,
    },
    Declared,
}

#[derive(Debug, Clone)]
pub enum ElementItems<'a> {
    Functions(Vec<u32>),
    ConstExprs {
        ty: RefType,
        exprs: Vec<wasmparser::ConstExpr<'a>>,
    },
}

#[derive(Debug, Clone, Eq, PartialEq)]
/// The type of instrumentation to be applied to an instruction.
pub enum InstrumentType {
    InstrumentBefore,
    InstrumentAfter,
    InstrumentAlternate,
    NotInstrumented,
}

impl fmt::Display for InstrumentType {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match *self {
            InstrumentType::InstrumentBefore => write!(f, "Instrument Before"),
            InstrumentType::InstrumentAfter => write!(f, "Instrument After"),
            InstrumentType::InstrumentAlternate => write!(f, "Instrument Alternate"),
            InstrumentType::NotInstrumented => write!(f, "Not Instrumented"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Body<'a> {
    /// Local variables of the function, given as tuples of (# of locals, type).
    /// Note that these do not include the function parameters which are given
    /// indices before the locals. So if a function has 2 parameters and a local
    /// defined here then local indices 0 and 1 will refer to the parameters and
    /// index 2 will refer to the local here.
    pub locals: Vec<(u32, ValType)>,
    pub instructions: Vec<(Operator<'a>, InstrumentType)>,
}

/// A constant which is produced in WebAssembly, typically used in global
/// initializers or element/data offsets.
#[derive(Debug, Copy, Clone)]
pub enum InitExpr {
    /// An immediate constant value
    Value(Value),
    /// A constant value referenced by the global specified
    Global(u32),
    /// A null reference
    RefNull(RefType),
    /// A function initializer
    RefFunc(u32),
}

impl InitExpr {
    pub(crate) fn eval(init: &ConstExpr) -> InitExpr {
        use wasmparser::Operator::*;
        let mut reader = init.get_operators_reader();
        let val = match reader.read().unwrap() {
            I32Const { value } => InitExpr::Value(Value::I32(value)),
            I64Const { value } => InitExpr::Value(Value::I64(value)),
            F32Const { value } => InitExpr::Value(Value::F32(f32::from_bits(value.bits()))),
            F64Const { value } => InitExpr::Value(Value::F64(f64::from_bits(value.bits()))),
            V128Const { value } => InitExpr::Value(Value::V128(v128_to_u128(&value))),
            GlobalGet { global_index } => InitExpr::Global(global_index),
            // Marking nullable as true as its a null reference
            RefNull { hty } => InitExpr::RefNull(RefType::new(true, hty).unwrap()),
            RefFunc { function_index } => InitExpr::RefFunc(function_index),
            _ => panic!("invalid constant expression"),
        };
        match reader.read().unwrap() {
            End => {}
            _ => panic!("invalid constant expression"),
        }
        reader.ensure_end().unwrap();
        val
    }

    pub(crate) fn to_wasmencoder_type(self) -> wasm_encoder::ConstExpr {
        match self {
            InitExpr::Value(v) => match v {
                Value::I32(v) => wasm_encoder::ConstExpr::i32_const(v),
                Value::I64(v) => wasm_encoder::ConstExpr::i64_const(v),
                Value::F32(v) => wasm_encoder::ConstExpr::f32_const(v),
                Value::F64(v) => wasm_encoder::ConstExpr::f64_const(v),
                Value::V128(v) => wasm_encoder::ConstExpr::v128_const(v as i128),
            },
            InitExpr::Global(g) => wasm_encoder::ConstExpr::global_get(g),
            InitExpr::RefNull(ty) => wasm_encoder::ConstExpr::ref_null(if ty.is_func_ref() {
                wasm_encoder::HeapType::Abstract {
                    shared: false,
                    ty: AbstractHeapType::Func,
                }
            } else if ty.is_extern_ref() {
                wasm_encoder::HeapType::Abstract {
                    shared: false,
                    ty: AbstractHeapType::Extern,
                }
            } else {
                unreachable!()
            }),
            InitExpr::RefFunc(f) => wasm_encoder::ConstExpr::ref_func(f),
        }
    }
}

/// Constant values that can show up in WebAssembly
#[derive(Debug, Clone, Copy)]
pub enum Value {
    /// A constant 32-bit integer
    I32(i32),
    /// A constant 64-bit integer
    I64(i64),
    /// A constant 32-bit float
    F32(f32),
    /// A constant 64-bit float
    F64(f64),
    /// A constant 128-bit vector register
    V128(u128),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Value::I32(i) => i.fmt(f),
            Value::I64(i) => i.fmt(f),
            Value::F32(i) => i.fmt(f),
            Value::F64(i) => i.fmt(f),
            Value::V128(i) => i.fmt(f),
        }
    }
}

#[allow(clippy::identity_op)]
pub(crate) fn v128_to_u128(value: &wasmparser::V128) -> u128 {
    let n = value.bytes();
    ((n[0] as u128) << 0)
        | ((n[1] as u128) << 8)
        | ((n[2] as u128) << 16)
        | ((n[3] as u128) << 24)
        | ((n[4] as u128) << 32)
        | ((n[5] as u128) << 40)
        | ((n[6] as u128) << 48)
        | ((n[7] as u128) << 56)
        | ((n[8] as u128) << 64)
        | ((n[9] as u128) << 72)
        | ((n[10] as u128) << 80)
        | ((n[11] as u128) << 88)
        | ((n[12] as u128) << 96)
        | ((n[13] as u128) << 104)
        | ((n[14] as u128) << 112)
        | ((n[15] as u128) << 120)
}