use std::sync::Arc;

use super::vm::Function;

#[derive(Debug,Clone,Copy,PartialEq)]
pub struct Slot(u32);

impl Slot {
    pub const DUMMY: Self = Self(0);

    pub fn new(id: u32) -> Self {
        Self(id)
    }

    pub fn offset(&self) -> usize {
        self.0 as usize
    }
}

#[allow(non_camel_case_types)]
#[derive(Debug)]
#[repr(u16)]
pub enum Instr {
    I8_Const(Slot, i8),
    I8_Neg(Slot, Slot),
    I8_Not(Slot, Slot),
    I8_Eq(Slot, Slot, Slot),
    I8_NotEq(Slot, Slot, Slot),
    I8_Add(Slot, Slot, Slot),
    I8_Sub(Slot, Slot, Slot),
    I8_Mul(Slot, Slot, Slot),
    I8_Or(Slot, Slot, Slot),
    I8_And(Slot, Slot, Slot),
    I8_Xor(Slot, Slot, Slot),
    I8_ShiftL(Slot, Slot, Slot),
    I8_S_Lt(Slot, Slot, Slot),
    I8_S_LtEq(Slot, Slot, Slot),
    I8_S_Div(Slot, Slot, Slot),
    I8_S_Rem(Slot, Slot, Slot),
    I8_S_ShiftR(Slot, Slot, Slot),
    I8_U_Lt(Slot, Slot, Slot),
    I8_U_LtEq(Slot, Slot, Slot),
    I8_U_Div(Slot, Slot, Slot),
    I8_U_Rem(Slot, Slot, Slot),
    I8_U_ShiftR(Slot, Slot, Slot),

    I16_Const(Slot, i16),
    I16_Neg(Slot, Slot),
    I16_Not(Slot, Slot),
    I16_Eq(Slot, Slot, Slot),
    I16_NotEq(Slot, Slot, Slot),
    I16_Add(Slot, Slot, Slot),
    I16_Sub(Slot, Slot, Slot),
    I16_Mul(Slot, Slot, Slot),
    I16_Or(Slot, Slot, Slot),
    I16_And(Slot, Slot, Slot),
    I16_Xor(Slot, Slot, Slot),
    I16_ShiftL(Slot, Slot, Slot),
    I16_S_Lt(Slot, Slot, Slot),
    I16_S_LtEq(Slot, Slot, Slot),
    I16_S_Div(Slot, Slot, Slot),
    I16_S_Rem(Slot, Slot, Slot),
    I16_S_ShiftR(Slot, Slot, Slot),
    I16_U_Lt(Slot, Slot, Slot),
    I16_U_LtEq(Slot, Slot, Slot),
    I16_U_Div(Slot, Slot, Slot),
    I16_U_Rem(Slot, Slot, Slot),
    I16_U_ShiftR(Slot, Slot, Slot),

    I32_Const(Slot, i32),
    I32_Neg(Slot, Slot),
    I32_Not(Slot, Slot),
    I32_Eq(Slot, Slot, Slot),
    I32_NotEq(Slot, Slot, Slot),
    I32_Add(Slot, Slot, Slot),
    I32_Sub(Slot, Slot, Slot),
    I32_Mul(Slot, Slot, Slot),
    I32_Or(Slot, Slot, Slot),
    I32_And(Slot, Slot, Slot),
    I32_Xor(Slot, Slot, Slot),
    I32_ShiftL(Slot, Slot, Slot),
    I32_S_Lt(Slot, Slot, Slot),
    I32_S_LtEq(Slot, Slot, Slot),
    I32_S_Div(Slot, Slot, Slot),
    I32_S_Rem(Slot, Slot, Slot),
    I32_S_ShiftR(Slot, Slot, Slot),
    I32_U_Lt(Slot, Slot, Slot),
    I32_U_LtEq(Slot, Slot, Slot),
    I32_U_Div(Slot, Slot, Slot),
    I32_U_Rem(Slot, Slot, Slot),
    I32_U_ShiftR(Slot, Slot, Slot),

    I64_Const(Slot, i64),
    I64_Neg(Slot, Slot),
    I64_Not(Slot, Slot),
    I64_Eq(Slot, Slot, Slot),
    I64_NotEq(Slot, Slot, Slot),
    I64_Add(Slot, Slot, Slot),
    I64_Sub(Slot, Slot, Slot),
    I64_Mul(Slot, Slot, Slot),
    I64_Or(Slot, Slot, Slot),
    I64_And(Slot, Slot, Slot),
    I64_Xor(Slot, Slot, Slot),
    I64_ShiftL(Slot, Slot, Slot),
    I64_S_Lt(Slot, Slot, Slot),
    I64_S_LtEq(Slot, Slot, Slot),
    I64_S_Div(Slot, Slot, Slot),
    I64_S_Rem(Slot, Slot, Slot),
    I64_S_ShiftR(Slot, Slot, Slot),
    I64_U_Lt(Slot, Slot, Slot),
    I64_U_LtEq(Slot, Slot, Slot),
    I64_U_Div(Slot, Slot, Slot),
    I64_U_Rem(Slot, Slot, Slot),
    I64_U_ShiftR(Slot, Slot, Slot),

    I128_Const(Slot, Box<i128>),
    I128_Neg(Slot, Slot),
    I128_Not(Slot, Slot),
    I128_Eq(Slot, Slot, Slot),
    I128_NotEq(Slot, Slot, Slot),
    I128_Add(Slot, Slot, Slot),
    I128_Sub(Slot, Slot, Slot),
    I128_Mul(Slot, Slot, Slot),
    I128_Or(Slot, Slot, Slot),
    I128_And(Slot, Slot, Slot),
    I128_Xor(Slot, Slot, Slot),
    I128_ShiftL(Slot, Slot, Slot),
    I128_S_Lt(Slot, Slot, Slot),
    I128_S_LtEq(Slot, Slot, Slot),
    I128_S_Div(Slot, Slot, Slot),
    I128_S_Rem(Slot, Slot, Slot),
    I128_S_ShiftR(Slot, Slot, Slot),
    I128_U_Lt(Slot, Slot, Slot),
    I128_U_LtEq(Slot, Slot, Slot),
    I128_U_Div(Slot, Slot, Slot),
    I128_U_Rem(Slot, Slot, Slot),
    I128_U_ShiftR(Slot, Slot, Slot),

    Bool_Not(Slot, Slot),

    F32_Neg(Slot, Slot),
    F32_Eq(Slot, Slot, Slot),
    F32_NotEq(Slot, Slot, Slot),
    F32_Add(Slot, Slot, Slot),
    F32_Sub(Slot, Slot, Slot),
    F32_Mul(Slot, Slot, Slot),
    F32_Div(Slot, Slot, Slot),
    F32_Rem(Slot, Slot, Slot),
    F32_Lt(Slot, Slot, Slot),
    F32_LtEq(Slot, Slot, Slot),
    F32_Gt(Slot, Slot, Slot),
    F32_GtEq(Slot, Slot, Slot),

    F64_Neg(Slot, Slot),
    F64_Eq(Slot, Slot, Slot),
    F64_NotEq(Slot, Slot, Slot),
    F64_Add(Slot, Slot, Slot),
    F64_Sub(Slot, Slot, Slot),
    F64_Mul(Slot, Slot, Slot),
    F64_Div(Slot, Slot, Slot),
    F64_Rem(Slot, Slot, Slot),
    F64_Lt(Slot, Slot, Slot),
    F64_LtEq(Slot, Slot, Slot),
    F64_Gt(Slot, Slot, Slot),
    F64_GtEq(Slot, Slot, Slot),

    // Integer widening ops used for casts
    // Narrowing needs no special instructions
    I16_S_Widen_8(Slot, Slot),
    I16_U_Widen_8(Slot, Slot),

    I32_S_Widen_16(Slot, Slot),
    I32_U_Widen_16(Slot, Slot),
    I32_S_Widen_8(Slot, Slot),
    I32_U_Widen_8(Slot, Slot),

    I64_S_Widen_32(Slot, Slot),
    I64_U_Widen_32(Slot, Slot),
    I64_S_Widen_16(Slot, Slot),
    I64_U_Widen_16(Slot, Slot),
    I64_S_Widen_8(Slot, Slot),
    I64_U_Widen_8(Slot, Slot),

    I128_S_Widen_64(Slot, Slot),
    I128_U_Widen_64(Slot, Slot),
    I128_S_Widen_32(Slot, Slot),
    I128_U_Widen_32(Slot, Slot),
    I128_S_Widen_16(Slot, Slot),
    I128_U_Widen_16(Slot, Slot),
    I128_S_Widen_8(Slot, Slot),
    I128_U_Widen_8(Slot, Slot),

    // Float casts
    F32_From_F64(Slot, Slot),
    F32_From_I8_S(Slot, Slot),
    F32_From_I16_S(Slot, Slot),
    F32_From_I32_S(Slot, Slot),
    F32_From_I64_S(Slot, Slot),
    F32_From_I128_S(Slot, Slot),
    F32_From_I8_U(Slot, Slot),
    F32_From_I16_U(Slot, Slot),
    F32_From_I32_U(Slot, Slot),
    F32_From_I64_U(Slot, Slot),
    F32_From_I128_U(Slot, Slot),
    F32_Into_I8_S(Slot, Slot),
    F32_Into_I16_S(Slot, Slot),
    F32_Into_I32_S(Slot, Slot),
    F32_Into_I64_S(Slot, Slot),
    F32_Into_I128_S(Slot, Slot),
    F32_Into_I8_U(Slot, Slot),
    F32_Into_I16_U(Slot, Slot),
    F32_Into_I32_U(Slot, Slot),
    F32_Into_I64_U(Slot, Slot),
    F32_Into_I128_U(Slot, Slot),

    F64_From_F32(Slot, Slot),
    F64_From_I8_S(Slot, Slot),
    F64_From_I16_S(Slot, Slot),
    F64_From_I32_S(Slot, Slot),
    F64_From_I64_S(Slot, Slot),
    F64_From_I128_S(Slot, Slot),
    F64_From_I8_U(Slot, Slot),
    F64_From_I16_U(Slot, Slot),
    F64_From_I32_U(Slot, Slot),
    F64_From_I64_U(Slot, Slot),
    F64_From_I128_U(Slot, Slot),
    F64_Into_I8_S(Slot, Slot),
    F64_Into_I16_S(Slot, Slot),
    F64_Into_I32_S(Slot, Slot),
    F64_Into_I64_S(Slot, Slot),
    F64_Into_I128_S(Slot, Slot),
    F64_Into_I8_U(Slot, Slot),
    F64_Into_I16_U(Slot, Slot),
    F64_Into_I32_U(Slot, Slot),
    F64_Into_I64_U(Slot, Slot),
    F64_Into_I128_U(Slot, Slot),

    MovSS1(Slot, Slot),
    MovSS2(Slot, Slot),
    MovSS4(Slot, Slot),
    MovSS8(Slot, Slot),
    MovSS16(Slot, Slot),

    MovSP1(Slot, Slot),
    MovSP2(Slot, Slot),
    MovSP4(Slot, Slot),
    MovSP8(Slot, Slot),
    MovSP16(Slot, Slot),

    MovPS1(Slot, Slot),
    MovPS2(Slot, Slot),
    MovPS4(Slot, Slot),
    MovPS8(Slot, Slot),
    MovPS16(Slot, Slot),

    SlotAddr(Slot, Slot),

    Jump(i32),
    JumpF(i32, Slot),
    JumpT(i32, Slot),

    Call(Slot, Arc<Function>),

    Return,
    Bad,
    Debug(Box<String>)
}
