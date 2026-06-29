//! The ONLY place width-dependent C-string literals live in the v2 codegen
//! (port of `recompiler/v2/widths.py`).
//!
//! Functions take only the IR's `width` (1 or 2) and raw operand-name strings;
//! no IR types, no decoder dependency. Centralizing here is what lets the
//! width-literal lint stay mechanical.

/// Operand mask for a width-bound op.
pub fn op_mask(width: u8) -> &'static str {
    if width == 1 { "0xFF" } else { "0xFFFF" }
}

/// High bit position for N-flag derivation.
pub fn sign_bit(width: u8) -> &'static str {
    if width == 1 { "0x80" } else { "0x8000" }
}

/// Bit one past the high bit — ADC/SBC carry-out detection.
pub fn carry_bit(width: u8) -> &'static str {
    if width == 1 { "0x100" } else { "0x10000" }
}

/// V-flag bit position for BIT mem (bit 6 in 8-bit, bit 14 in 16-bit).
pub fn overflow_bit(width: u8) -> &'static str {
    if width == 1 { "0x40" } else { "0x4000" }
}

/// C type name to hold a width-bound value.
pub fn ctype(width: u8) -> &'static str {
    if width == 1 { "uint8" } else { "uint16" }
}

/// Wrap a raw C expression in `(expr & op_mask)`.
pub fn masked(expr: &str, width: u8) -> String {
    format!("({expr} & {})", op_mask(width))
}

/// Canonical N/Z mirror update PLUS the `cpu->P` packed-flag refresh.
pub fn set_nz(src_expr: &str, width: u8) -> Vec<String> {
    let sign = sign_bit(width);
    vec![
        format!("cpu->_flag_Z = (({src_expr}) == 0) ? 1 : 0;"),
        format!("cpu->_flag_N = ((({src_expr}) & {sign}) != 0) ? 1 : 0;"),
        "cpu->P = (uint8)((cpu->P & ~0x82) | \
         (cpu->_flag_Z ? 0x02 : 0) | (cpu->_flag_N ? 0x80 : 0));"
            .to_string(),
    ]
}

/// Same as `set_nz` but omits the `cpu->P` update (deferred flush). Use sparingly.
pub fn set_nz_no_p(src_expr: &str, width: u8) -> Vec<String> {
    let sign = sign_bit(width);
    vec![
        format!("cpu->_flag_Z = (({src_expr}) == 0) ? 1 : 0;"),
        format!("cpu->_flag_N = ((({src_expr}) & {sign}) != 0) ? 1 : 0;"),
    ]
}

/// `cpu->_flag_C = ((src & bit_mask) != 0) ? 1 : 0;`
pub fn set_carry_from_bit(src_expr: &str, bit_mask: &str) -> String {
    format!("cpu->_flag_C = (({src_expr}) & {bit_mask}) ? 1 : 0;")
}

/// Carry derivation from a uint32 temp produced by ADD ("add") or SUB ("sub").
pub fn set_carry_from_overflow(temp_var: &str, width: u8, polarity: &str) -> String {
    let cb = carry_bit(width);
    match polarity {
        "add" => format!("cpu->_flag_C = ({temp_var} & {cb}) ? 1 : 0;"),
        "sub" => format!("cpu->_flag_C = ({temp_var} & {cb}) ? 0 : 1;"),
        other => panic!("polarity must be 'add' or 'sub', got {other:?}"),
    }
}

/// Two's-complement overflow flag for ADC.
pub fn set_v_adc(lhs_m: &str, rhs_m: &str, out_v: &str, width: u8) -> String {
    let sign = sign_bit(width);
    format!(
        "cpu->_flag_V = ((({lhs_m} ^ {out_v}) & ({rhs_m} ^ {out_v}) & {sign}) != 0) ? 1 : 0;"
    )
}

/// Two's-complement overflow flag for SBC.
pub fn set_v_sbc(lhs_m: &str, rhs_m: &str, out_v: &str, width: u8) -> String {
    let sign = sign_bit(width);
    format!(
        "cpu->_flag_V = ((({lhs_m} ^ {rhs_m}) & ({lhs_m} ^ {out_v}) & {sign}) != 0) ? 1 : 0;"
    )
}

/// A in m=1: keep the existing high byte (B register), replace the low byte.
pub fn preserve_high(field: &str, lo_byte_expr: &str) -> String {
    format!("(uint16)(({field} & 0xFF00) | (({lo_byte_expr}) & 0xFF))")
}

/// X/Y in x=1: hardware-zero the high byte.
pub fn zero_extend_lo(lo_byte_expr: &str) -> String {
    format!("(uint16)(({lo_byte_expr}) & 0xFF)")
}

/// Cast `(uint8)(field & 0xFF)` — extract low byte from 16-bit storage.
pub fn low_byte(field: &str) -> String {
    format!("(uint8)({field} & 0xFF)")
}

/// C function name for a width-bound memory read.
pub fn read_fn(width: u8) -> &'static str {
    if width == 1 { "cpu_read8" } else { "cpu_read16" }
}

/// C function name for a width-bound memory write.
pub fn write_fn(width: u8) -> &'static str {
    if width == 1 { "cpu_write8" } else { "cpu_write16" }
}
