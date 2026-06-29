//! Emit C code from v2 IR (port of `recompiler/v2/codegen.py`).
//!
//! The Python module's ~6 process-global setters become fields on an immutable
//! `EmitCtx` threaded by `&`; the collector/take_* globals become a returned
//! `EmitOutcome` so emit is pure and rayon-safe.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::insn::{Insn, Mode};
use crate::ir::{
    AluOp, Call, IROp, MoveDir, Reg, Return, SegKind, SegRef, ShiftOp, Value,
};
use crate::rom::{addr_in_reloc_region, RelocRegion};
use crate::widths;

// ── Immutable emit context (replaces the Python process globals) ─────────────

/// Immutable-after-build bundle of the emit-affecting inputs the Python kept in
/// process globals (`set_rom_size`, `set_reloc_regions`, `set_name_resolver`,
/// `set_valid_variants`, `set_force_variant_at`).
#[derive(Debug, Clone, Default)]
pub struct EmitCtx {
    pub rom_size: usize,
    pub reloc_regions: Vec<RelocRegion>,
    /// 24-bit addr -> friendly C name, ALREADY expanded with LoROM mirrors.
    pub name_resolver: HashMap<u32, String>,
    /// 24-bit target -> surviving (m, x) set. Empty => emit all four.
    pub valid_variants: HashMap<u32, BTreeSet<(u8, u8)>>,
    /// JSR/JSL site pc24 -> pinned (m, x). Empty in the controlled oracle env.
    pub force_variant_at: HashMap<u32, (u8, u8)>,
}

impl EmitCtx {
    /// Build a name resolver expanded with LoROM bank-mirror aliases, mirroring
    /// `codegen.set_name_resolver`.
    pub fn set_name_resolver(&mut self, name_map: &HashMap<u32, String>) {
        let mut expanded: HashMap<u32, String> = HashMap::new();
        for (&pc24, name) in name_map {
            expanded.insert(pc24, name.clone());
        }
        for (&pc24, name) in name_map {
            let bank = (pc24 >> 16) & 0xFF;
            if bank < 0x40 || (0x80..0xC0).contains(&bank) {
                let mirror_bank = bank ^ 0x80;
                let mirror_pc24 = (mirror_bank << 16) | (pc24 & 0xFFFF);
                if !name_map.contains_key(&mirror_pc24) {
                    expanded.entry(mirror_pc24).or_insert_with(|| name.clone());
                }
            }
        }
        self.name_resolver = expanded;
    }

    pub fn get_name_for_pc(&self, pc24: u32) -> Option<&str> {
        self.name_resolver.get(&(pc24 & 0xFFFFFF)).map(|s| s.as_str())
    }

    fn addr_in_reloc_region(&self, addr_24: u32) -> bool {
        if self.reloc_regions.is_empty() {
            return false;
        }
        let bank = (addr_24 >> 16) & 0xFF;
        let pc16 = addr_24 & 0xFFFF;
        addr_in_reloc_region(bank, pc16, &self.reloc_regions).is_some()
    }

    /// Public wrapper (the emit driver needs this for cross-bank JML routing).
    pub fn is_invalid_lorom_call_target_pub(&self, addr_24: u32) -> bool {
        self.is_invalid_lorom_call_target(addr_24)
    }

    fn is_invalid_lorom_call_target(&self, addr_24: u32) -> bool {
        if self.addr_in_reloc_region(addr_24) {
            return false;
        }
        let pc = addr_24 & 0xFFFF;
        if pc < 0x8000 {
            return true;
        }
        if self.rom_size > 0 {
            let canon_bank = (addr_24 >> 16) & 0x7F;
            let offset = canon_bank as usize * 0x8000 + (pc as usize - 0x8000);
            if offset >= self.rom_size {
                return true;
            }
        }
        false
    }
}

/// Returned data the Python kept in process-global collectors / `take_*`.
#[derive(Debug, Clone, Default)]
pub struct EmitOutcome {
    pub unresolved_call_targets: HashSet<(u32, u8, u8)>,
    pub rejected_call_targets: HashSet<u32>,
}

// ── Variant suffix + dispatch helpers ────────────────────────────────────────

const MX_VARIANTS: [(u8, u8); 4] = [(0, 0), (0, 1), (1, 0), (1, 1)];

pub fn variant_suffix(m: u8, x: u8) -> String {
    format!("_M{}X{}", m & 1, x & 1)
}

impl EmitCtx {
    pub fn valid_variant_list(&self, addr_24: u32) -> Vec<(u8, u8)> {
        if self.valid_variants.is_empty() {
            return MX_VARIANTS.to_vec();
        }
        let a = addr_24 & 0xFFFFFF;
        let mut s = self.valid_variants.get(&a);
        if s.is_none() {
            let bank = (a >> 16) & 0xFF;
            if bank < 0x40 || (0x80..0xC0).contains(&bank) {
                s = self.valid_variants.get(&(a ^ 0x800000));
            }
        }
        match s {
            None => MX_VARIANTS.to_vec(),
            Some(set) if set.is_empty() => MX_VARIANTS.to_vec(),
            Some(set) => MX_VARIANTS.iter().copied().filter(|mx| set.contains(mx)).collect(),
        }
    }
}

fn nearest_survivor(survivors: &[(u8, u8)], m: u8, x: u8) -> Option<(u8, u8)> {
    let mut best: Option<(u8, u8)> = None;
    let mut best_cost: Option<i32> = None;
    for &(sm, sx) in survivors {
        let cost = (if sm == m { 0 } else { 2 }) + (if sx == x { 0 } else { 1 });
        if best_cost.is_none() || cost < best_cost.unwrap() {
            best_cost = Some(cost);
            best = Some((sm, sx));
        }
    }
    best
}

impl EmitCtx {
    pub fn variant_dispatch_case_lines(
        &self,
        addr_24: u32,
        base_name: &str,
        indent: &str,
        pre_call: Option<&[String]>,
    ) -> Vec<String> {
        let survivors = self.valid_variant_list(addr_24);
        let survivor_set: HashSet<(u8, u8)> = survivors.iter().copied().collect();
        let mut lines: Vec<String> = Vec::new();

        let pre = match pre_call {
            Some(p) if !p.is_empty() => format!("{} ", p.join(" ")),
            _ => String::new(),
        };
        let mut emit = |label: String, name: String, comment: &str| {
            lines.push(format!("{indent}{label}: {pre}_r = {name}(cpu); break;{comment}"));
        };

        for &(m, x) in MX_VARIANTS.iter() {
            let idx = (m << 1) | x;
            if survivor_set.contains(&(m, x)) {
                emit(format!("case {idx}"), format!("{base_name}{}", variant_suffix(m, x)), "");
            } else if !survivors.is_empty() {
                let (sm, sx) = nearest_survivor(&survivors, m, x).unwrap();
                emit(
                    format!("case {idx}"),
                    format!("{base_name}{}", variant_suffix(sm, sx)),
                    &format!("  /* M{m}X{x} pruned -> nearest survivor M{sm}X{sx} */"),
                );
            }
        }
        if !survivors.is_empty() {
            let (sm, sx) = nearest_survivor(&survivors, 0, 0).unwrap();
            emit("default".to_string(), format!("{base_name}{}", variant_suffix(sm, sx)), "");
        } else {
            lines.push(format!("{indent}default: _r = RECOMP_RETURN_NORMAL; break;"));
        }
        lines
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn v(value: Value) -> String {
    format!("_v{}", value.vid)
}

fn ctype(width: u8) -> &'static str {
    if width == 1 { "uint8" } else { "uint16" }
}

fn reg(r: Reg) -> &'static str {
    match r {
        Reg::A => "cpu->A",
        Reg::B => "((uint8)((cpu->A >> 8) & 0xFF))",
        Reg::X => "cpu->X",
        Reg::Y => "cpu->Y",
        Reg::S => "cpu->S",
        Reg::D => "cpu->D",
        Reg::Db => "cpu->DB",
        Reg::Pb => "cpu->PB",
        Reg::P => "cpu->P",
        Reg::M => "cpu->m_flag",
        Reg::Xf => "cpu->x_flag",
        Reg::E => "cpu->emulation",
        Reg::N => "cpu->_flag_N",
        Reg::V => "cpu->_flag_V",
        Reg::Zf => "cpu->_flag_Z",
        Reg::C => "cpu->_flag_C",
        Reg::I => "cpu->_flag_I",
        Reg::Df => "cpu->_flag_D",
    }
}

/// Public reg-field accessor (used by emit_function's cond-branch rewrite).
pub fn reg_field(r: Reg) -> &'static str {
    reg(r)
}

// ── emitter_helpers port ─────────────────────────────────────────────────────

fn push_byte(val_expr: &str) -> Vec<String> {
    vec![
        format!("cpu_write8(cpu, 0x00, cpu->S, {val_expr});"),
        "cpu->S = (uint16)(cpu->S - 1);".to_string(),
    ]
}

fn push_word(val_expr: &str) -> Vec<String> {
    vec![
        "cpu->S = (uint16)(cpu->S - 1);".to_string(),
        format!("cpu_write16(cpu, 0x00, cpu->S, {val_expr});"),
        "cpu->S = (uint16)(cpu->S - 1);".to_string(),
    ]
}

fn pop_byte_assign(target_decl: &str) -> Vec<String> {
    vec![
        "cpu->S = (uint16)(cpu->S + 1);".to_string(),
        format!("{target_decl} = cpu_read8(cpu, 0x00, cpu->S);"),
    ]
}

fn pop_word_assign(target_decl: &str) -> Vec<String> {
    vec![
        "cpu->S = (uint16)(cpu->S + 1);".to_string(),
        format!("{target_decl} = cpu_read16(cpu, 0x00, cpu->S);"),
        "cpu->S = (uint16)(cpu->S + 1);".to_string(),
    ]
}

fn stack_op_traced(op_id: &str, delta: i32, body: Vec<String>) -> Vec<String> {
    let mut out = vec!["{".to_string(), "  uint16 _old_s = cpu->S;".to_string()];
    for ln in body {
        out.push(format!("  {ln}"));
    }
    out.push(format!("  cpu_trace_stack_op(cpu, 0, {op_id}, _old_s, {delta});"));
    out.push("}".to_string());
    out
}

fn modify_p_via_mirrors(mask: u8, kind: &str) -> Vec<String> {
    let (modify, px_kind, px_label) = match kind {
        "rep" => (format!("cpu->P = (uint8)(cpu->P & ~{mask:#04x});"), 0, "REP"),
        "sep" => (format!("cpu->P = (uint8)(cpu->P | {mask:#04x});"), 1, "SEP"),
        _ => panic!("kind must be 'rep' or 'sep', got {kind:?}"),
    };
    vec![
        "uint8 _old_p = cpu->P;".to_string(),
        "cpu_mirrors_to_p(cpu);".to_string(),
        modify,
        "cpu_p_to_mirrors(cpu);".to_string(),
        format!("cpu_trace_px_record(cpu, 0, {px_kind} /*{px_label}*/, _old_p, cpu->P);"),
    ]
}

fn call_with_pb_save(target_bank: u32, callee_name: &str) -> Vec<String> {
    vec![
        "uint8 _saved_pb = cpu->PB;".to_string(),
        format!("cpu_trace_pb_change(cpu, 0, _saved_pb, {target_bank:#04x}, CPU_TR_JSL);"),
        format!("cpu->PB = {target_bank:#04x};"),
        format!("RecompReturn _r = {callee_name}(cpu);"),
        "cpu_trace_pb_change(cpu, 0, cpu->PB, _saved_pb, CPU_TR_RTL);".to_string(),
        "cpu->PB = _saved_pb;".to_string(),
        "if (_r != RECOMP_RETURN_NORMAL) {".to_string(),
        "  cpu_trace_event(cpu, 0, CPU_TR_NLR_PROPAGATE, (uint8)_r, 0);".to_string(),
        "  cpu_trace_mark_nlr_exit(BD_EXIT_KIND_SKIP_PROPAGATION);".to_string(),
        "  return (RecompReturn)((int)_r - 1);".to_string(),
        "}".to_string(),
    ]
}

// ── SegRef → C address expressions ───────────────────────────────────────────

/// Resolve a SegRef into (bank_expr, addr_expr). Mirrors `_segref_addr_expr`.
pub fn segref_addr_expr(seg: &SegRef) -> (String, String) {
    let idx = match seg.index {
        Some(Reg::X) => " + cpu->X",
        Some(Reg::Y) => " + cpu->Y",
        _ => "",
    };
    let off = seg.offset;
    match seg.kind {
        SegKind::Direct => ("0x7E".to_string(), format!("(uint16)(cpu->D + {off:#06x}{idx})")),
        SegKind::AbsBank => {
            if seg.index.is_none() {
                ("cpu->DB".to_string(), format!("(uint16)({off:#06x})"))
            } else {
                let idx_reg = if seg.index == Some(Reg::X) { "cpu->X" } else { "cpu->Y" };
                let eff24 = format!(
                    "(((uint32)cpu->DB << 16) + (uint32){off:#06x} + (uint32){idx_reg})"
                );
                (format!("(uint8)(({eff24}) >> 16)"), format!("(uint16)({eff24})"))
            }
        }
        SegKind::Long => {
            let bank = seg.bank.unwrap_or(0) as u32;
            if seg.index.is_none() {
                (format!("{bank:#04x}"), format!("(uint16)({off:#06x})"))
            } else {
                let base24 = (bank << 16) | (seg.offset as u32 & 0xFFFF);
                let idx_reg = if seg.index == Some(Reg::X) { "cpu->X" } else { "cpu->Y" };
                let eff24 = format!("((uint32){base24:#08x} + (uint32){idx_reg})");
                (format!("(uint8)(({eff24}) >> 16)"), format!("(uint16)({eff24})"))
            }
        }
        SegKind::Stack => ("0x00".to_string(), format!("(uint16)(cpu->S + {off:#06x})")),
        SegKind::DpIndirect => {
            let ptr_addr = format!("(uint16)(cpu->D + {off:#06x})");
            ("cpu->DB".to_string(), format!("(uint16)(cpu_read16(cpu, 0x00, {ptr_addr}){idx})"))
        }
        SegKind::DpIndirectLong => {
            let ptr_addr = format!("(uint16)(cpu->D + {off:#06x})");
            let bank_expr = format!("cpu_read8(cpu, 0x00, (uint16)({ptr_addr} + 2))");
            let addr_expr = format!("(uint16)(cpu_read16(cpu, 0x00, {ptr_addr}){idx})");
            (bank_expr, addr_expr)
        }
        SegKind::AbsIndirect => (
            "cpu->PB".to_string(),
            format!("cpu_read16(cpu, cpu->PB, (uint16){off:#06x})"),
        ),
        SegKind::AbsIndirectX => (
            "cpu->PB".to_string(),
            format!("cpu_read16(cpu, cpu->PB, (uint16)({off:#06x} + cpu->X))"),
        ),
        SegKind::AbsIndirectLong => {
            let addr = format!("(uint16){off:#06x}");
            (
                format!("cpu_read8(cpu, 0x00, (uint16)({addr} + 2))"),
                format!("cpu_read16(cpu, 0x00, {addr})"),
            )
        }
        SegKind::DpIndirectX => {
            let ptr_addr = format!("(uint16)(cpu->D + {off:#06x} + cpu->X)");
            ("cpu->DB".to_string(), format!("cpu_read16(cpu, 0x00, {ptr_addr})"))
        }
        SegKind::StackRelIndirectY => {
            let ptr_addr = format!("(uint16)(cpu->S + {off:#06x})");
            (
                "cpu->DB".to_string(),
                format!("(uint16)(cpu_read16(cpu, 0x00, {ptr_addr}) + cpu->Y)"),
            )
        }
    }
}

// ── Per-op handlers (pure) ───────────────────────────────────────────────────

fn emit_read(seg: &SegRef, width: u8, out: Value) -> Vec<String> {
    let (bank, addr) = segref_addr_expr(seg);
    vec![format!(
        "{} {} = {}(cpu, {bank}, {addr});",
        widths::ctype(width),
        v(out),
        widths::read_fn(width)
    )]
}

fn emit_write(seg: &SegRef, src: Value, width: u8) -> Vec<String> {
    let (bank, addr) = segref_addr_expr(seg);
    vec![format!("{}(cpu, {bank}, {addr}, {});", widths::write_fn(width), v(src))]
}

fn emit_readreg(r: Reg, out: Value) -> Vec<String> {
    match r {
        Reg::A => vec![format!("uint16 {} = cpu_read_a16(cpu);", v(out))],
        Reg::X => vec![format!("uint16 {} = cpu_read_x16(cpu);", v(out))],
        Reg::Y => vec![format!("uint16 {} = cpu_read_y16(cpu);", v(out))],
        _ => vec![format!("uint16 {} = (uint16){};", v(out), reg(r))],
    }
}

fn emit_writereg(r: Reg, src: Value) -> Vec<String> {
    let s = v(src);
    match r {
        Reg::A => vec![format!("cpu_write_a_m(cpu, (uint16)({s}));")],
        Reg::X => vec![format!("cpu_write_x_x(cpu, (uint16)({s}));")],
        Reg::Y => vec![format!("cpu_write_y_x(cpu, (uint16)({s}));")],
        _ => vec![format!("{} = {s};", reg(r))],
    }
}

fn emit_consti(value: i64, width: u8, out: Value) -> Vec<String> {
    vec![format!("{} {} = {value:#x};", ctype(width), v(out))]
}

fn emit_alu(op: AluOp, lhs: Value, rhs: Value, width: u8, out: Option<Value>) -> Vec<String> {
    let tname = match out {
        Some(o) => format!("_t{}", o.vid),
        None => format!("_tc{}_{}", lhs.vid, rhs.vid),
    };
    let mut lines: Vec<String> = Vec::new();
    let lhs_m = widths::masked(&v(lhs), width);
    let rhs_m = widths::masked(&v(rhs), width);
    let ct = widths::ctype(width);
    match op {
        AluOp::Add => {
            lines.push(format!(
                "uint32 {tname} = (uint32){lhs_m} + (uint32){rhs_m} + cpu->_flag_C;"
            ));
            if let Some(o) = out {
                lines.push(format!("{ct} {} = ({ct}){tname};", v(o)));
            }
            lines.push(widths::set_carry_from_overflow(&tname, width, "add"));
            if let Some(o) = out {
                lines.push(widths::set_v_adc(&lhs_m, &rhs_m, &v(o), width));
            }
        }
        AluOp::Sub => {
            lines.push(format!(
                "uint32 {tname} = (uint32){lhs_m} - (uint32){rhs_m} - (1 - cpu->_flag_C);"
            ));
            if let Some(o) = out {
                lines.push(format!("{ct} {} = ({ct}){tname};", v(o)));
            }
            lines.push(widths::set_carry_from_overflow(&tname, width, "sub"));
            if let Some(o) = out {
                lines.push(widths::set_v_sbc(&lhs_m, &rhs_m, &v(o), width));
            }
        }
        AluOp::And => {
            lines.push(format!("{ct} {} = ({ct})({} & {});", v(out.unwrap()), v(lhs), v(rhs)));
        }
        AluOp::Or => {
            lines.push(format!("{ct} {} = ({ct})({} | {});", v(out.unwrap()), v(lhs), v(rhs)));
        }
        AluOp::Xor => {
            lines.push(format!("{ct} {} = ({ct})({} ^ {});", v(out.unwrap()), v(lhs), v(rhs)));
        }
        AluOp::Cmp => {
            lines.push(format!("uint32 {tname} = (uint32){lhs_m} - (uint32){rhs_m};"));
            lines.push(format!("cpu->_flag_C = ({lhs_m} >= {rhs_m}) ? 1 : 0;"));
            lines.extend(widths::set_nz_no_p(&format!("({ct}){tname}"), width));
            return lines;
        }
    }
    if let Some(o) = out {
        lines.extend(widths::set_nz_no_p(&v(o), width));
    }
    lines
}

fn emit_shift(op: ShiftOp, src: Value, width: u8, out: Value) -> Vec<String> {
    let src_m = widths::masked(&v(src), width);
    let sign = widths::sign_bit(width);
    let out_v = v(out);
    let out_t = widths::ctype(width);
    match op {
        ShiftOp::Asl => {
            let mut l = vec![
                format!("{out_t} {out_v} = ({out_t})({src_m} << 1);"),
                widths::set_carry_from_bit(&src_m, sign),
            ];
            l.extend(widths::set_nz_no_p(&out_v, width));
            l
        }
        ShiftOp::Lsr => {
            let mut l = vec![
                format!("{out_t} {out_v} = ({out_t})({src_m} >> 1);"),
                widths::set_carry_from_bit(&src_m, "1"),
            ];
            l.extend(widths::set_nz_no_p(&out_v, width));
            l
        }
        ShiftOp::Rol => {
            let mut l = vec![
                format!("{out_t} {out_v} = ({out_t})(({src_m} << 1) | cpu->_flag_C);"),
                widths::set_carry_from_bit(&src_m, sign),
            ];
            l.extend(widths::set_nz_no_p(&out_v, width));
            l
        }
        ShiftOp::Ror => {
            let bits = width as u32 * 8;
            let mut l = vec![
                format!(
                    "{out_t} {out_v} = ({out_t})(({src_m} >> 1) | ((uint{bits})cpu->_flag_C << {}));",
                    bits - 1
                ),
                widths::set_carry_from_bit(&src_m, "1"),
            ];
            l.extend(widths::set_nz_no_p(&out_v, width));
            l
        }
    }
}

fn emit_increg(r: Reg, delta_i: i8) -> Vec<String> {
    let field = reg(r);
    let delta = if delta_i == 1 { "1" } else { "-1" };
    match r {
        Reg::A => {
            let mut lines = vec![
                "if (cpu->m_flag) {".to_string(),
                format!("  uint8 _lo8 = ({}) + ({delta});", widths::low_byte(field)),
                format!("  {field} = {};", widths::preserve_high(field, "_lo8")),
            ];
            for s in widths::set_nz_no_p("_lo8", 1) {
                lines.push(format!("  {s}"));
            }
            lines.push("} else {".to_string());
            lines.push(format!("  {field} = (uint16)(({field}) + ({delta}));"));
            for s in widths::set_nz_no_p(field, 2) {
                lines.push(format!("  {s}"));
            }
            lines.push("}".to_string());
            lines
        }
        Reg::X | Reg::Y => {
            let mut lines = vec![
                "if (cpu->x_flag) {".to_string(),
                format!("  uint8 _lo8 = ({}) + ({delta});", widths::low_byte(field)),
                format!(
                    "  {field} = {};  /* x=1 zeros high byte (hw contract) */",
                    widths::zero_extend_lo("_lo8")
                ),
            ];
            for s in widths::set_nz_no_p("_lo8", 1) {
                lines.push(format!("  {s}"));
            }
            lines.push("} else {".to_string());
            lines.push(format!("  {field} = (uint16)(({field}) + ({delta}));"));
            for s in widths::set_nz_no_p(field, 2) {
                lines.push(format!("  {s}"));
            }
            lines.push("}".to_string());
            lines
        }
        _ => {
            let mut l = vec![format!("{field} = ({field}) + ({delta});")];
            l.extend(widths::set_nz_no_p(field, 2));
            l
        }
    }
}

fn emit_incmem(seg: &SegRef, width: u8, delta_i: i8) -> Vec<String> {
    let (bank, addr) = segref_addr_expr(seg);
    let delta = if delta_i == 1 { "+1" } else { "-1" };
    let ct = widths::ctype(width);
    let mut lines = vec![
        "{".to_string(),
        format!("  {ct} _im = {}(cpu, {bank}, {addr});", widths::read_fn(width)),
        format!("  _im = ({ct})(_im {delta});"),
        format!("  {}(cpu, {bank}, {addr}, _im);", widths::write_fn(width)),
    ];
    for s in widths::set_nz_no_p("_im", width) {
        lines.push(format!("  {s}"));
    }
    lines.push("}".to_string());
    lines
}

fn emit_bittest(operand: Value, width: u8) -> Vec<String> {
    let sign = widths::sign_bit(width);
    let overflow = widths::overflow_bit(width);
    let ct = widths::ctype(width);
    let a_m = widths::masked("cpu->A", width);
    let operand_m = widths::masked(&v(operand), width);
    vec![
        "{".to_string(),
        format!("  {ct} _bt = ({ct})({a_m} & {operand_m});"),
        "  cpu->_flag_Z = (_bt == 0) ? 1 : 0;".to_string(),
        format!("  cpu->_flag_N = (({operand_m} & {sign}) != 0) ? 1 : 0;"),
        format!("  cpu->_flag_V = (({operand_m} & {overflow}) != 0) ? 1 : 0;"),
        "}".to_string(),
    ]
}

fn emit_bitsetmem(seg: &SegRef, width: u8) -> Vec<String> {
    let (bank, addr) = segref_addr_expr(seg);
    let ct = widths::ctype(width);
    vec![
        "{".to_string(),
        format!("  {ct} _m = {}(cpu, {bank}, {addr});", widths::read_fn(width)),
        "  cpu->_flag_Z = ((_m & cpu->A) == 0) ? 1 : 0;".to_string(),
        format!("  {}(cpu, {bank}, {addr}, ({ct})(_m | cpu->A));", widths::write_fn(width)),
        "}".to_string(),
    ]
}

fn emit_bitclearmem(seg: &SegRef, width: u8) -> Vec<String> {
    let (bank, addr) = segref_addr_expr(seg);
    let ct = widths::ctype(width);
    vec![
        "{".to_string(),
        format!("  {ct} _m = {}(cpu, {bank}, {addr});", widths::read_fn(width)),
        "  cpu->_flag_Z = ((_m & cpu->A) == 0) ? 1 : 0;".to_string(),
        format!("  {}(cpu, {bank}, {addr}, ({ct})(_m & ~cpu->A));", widths::write_fn(width)),
        "}".to_string(),
    ]
}

fn emit_setflag(flag: Reg, value: u8) -> Vec<String> {
    let mask = match flag {
        Reg::C => Some("0x01"),
        Reg::Zf => Some("0x02"),
        Reg::I => Some("0x04"),
        Reg::Df => Some("0x08"),
        Reg::Xf => Some("0x10"),
        Reg::M => Some("0x20"),
        Reg::V => Some("0x40"),
        Reg::N => Some("0x80"),
        _ => None,
    };
    let mut lines = vec![format!("{} = {value};", reg(flag))];
    if let Some(m) = mask {
        if value != 0 {
            lines.push(format!("cpu->P = (uint8)(cpu->P | {m});"));
        } else {
            lines.push(format!("cpu->P = (uint8)(cpu->P & ~{m});"));
        }
    }
    lines
}

fn emit_setnz(src: Value, width: u8) -> Vec<String> {
    widths::set_nz(&widths::masked(&v(src), width), width)
}

fn emit_repflags(mask: u8) -> Vec<String> {
    let mut out = vec!["{".to_string()];
    for s in modify_p_via_mirrors(mask, "rep") {
        out.push(format!("  {s}"));
    }
    out.push("}".to_string());
    out
}

fn emit_sepflags(mask: u8) -> Vec<String> {
    let mut out = vec!["{".to_string()];
    for s in modify_p_via_mirrors(mask, "sep") {
        out.push(format!("  {s}"));
    }
    out.push("}".to_string());
    out
}

fn emit_xce() -> Vec<String> {
    vec![
        "{".to_string(),
        "  uint8 _old_p = cpu->P;".to_string(),
        "  uint8 _t = cpu->emulation;".to_string(),
        "  cpu->emulation = cpu->_flag_C;".to_string(),
        "  cpu->_flag_C = _t;".to_string(),
        "  if (cpu->emulation) { cpu->m_flag = 1; cpu->x_flag = 1; cpu_mirrors_to_p(cpu); }".to_string(),
        "  cpu_trace_px_record(cpu, 0, 7 /*XCE*/, _old_p, cpu->P);".to_string(),
        "}".to_string(),
    ]
}

fn emit_xba() -> Vec<String> {
    vec![
        "{".to_string(),
        "  uint16 _old = cpu->A;".to_string(),
        "  cpu->A = (uint16)(((_old & 0xFF) << 8) | ((_old >> 8) & 0xFF));".to_string(),
        "  cpu->_flag_Z = ((cpu->A & 0xFF) == 0) ? 1 : 0;".to_string(),
        "  cpu->_flag_N = ((cpu->A & 0x80) != 0) ? 1 : 0;".to_string(),
        "}".to_string(),
    ]
}

fn emit_pushreg(r: Reg, static_m: Option<u8>, static_x: Option<u8>) -> Vec<String> {
    let field = reg(r);
    match r {
        Reg::P => {
            let mut out = vec!["cpu_mirrors_to_p(cpu);".to_string()];
            out.extend(stack_op_traced(
                "CPU_STACK_OP_PHP",
                -1,
                push_byte(&format!("(uint8)({field})")),
            ));
            out.push("cpu_trace_event(cpu, 0, CPU_TR_PHP, cpu->P, 0);".to_string());
            out.push("cpu_trace_px_record(cpu, 0, 4 /*PHP*/, cpu->P, cpu->P);".to_string());
            out
        }
        Reg::Db => {
            let mut out = stack_op_traced(
                "CPU_STACK_OP_PHB",
                -1,
                push_byte(&format!("(uint8)({field})")),
            );
            out.push("cpu_trace_event(cpu, 0, CPU_TR_PHB, cpu->DB, cpu->DB);".to_string());
            out
        }
        Reg::Pb => {
            let mut out = stack_op_traced(
                "CPU_STACK_OP_PHK",
                -1,
                push_byte(&format!("(uint8)({field})")),
            );
            out.push("cpu_trace_event(cpu, 0, CPU_TR_PHK, cpu->PB, cpu->PB);".to_string());
            out
        }
        Reg::D => stack_op_traced("CPU_STACK_OP_PHD", -2, push_word(field)),
        Reg::A => {
            let lo = widths::low_byte(field);
            match static_m {
                Some(1) => {
                    let mut out = vec!["{ uint16 _old_s = cpu->S;".to_string()];
                    for s in push_byte(&lo) {
                        out.push(format!("  {s}"));
                    }
                    out.push("  cpu_trace_stack_op(cpu, 0, CPU_STACK_OP_PHA, _old_s, -1); }".to_string());
                    out
                }
                Some(0) => {
                    let mut out = vec!["{ uint16 _old_s = cpu->S;".to_string()];
                    for s in push_word(field) {
                        out.push(format!("  {s}"));
                    }
                    out.push("  cpu_trace_stack_op(cpu, 0, CPU_STACK_OP_PHA, _old_s, -2); }".to_string());
                    out
                }
                _ => {
                    let mut out = vec![
                        "{ uint16 _old_s = cpu->S;".to_string(),
                        "  if (cpu->m_flag) {".to_string(),
                    ];
                    for s in push_byte(&lo) {
                        out.push(format!("    {s}"));
                    }
                    out.push("    cpu_trace_stack_op(cpu, 0, CPU_STACK_OP_PHA, _old_s, -1);".to_string());
                    out.push("  } else {".to_string());
                    for s in push_word(field) {
                        out.push(format!("    {s}"));
                    }
                    out.push("    cpu_trace_stack_op(cpu, 0, CPU_STACK_OP_PHA, _old_s, -2);".to_string());
                    out.push("  } }".to_string());
                    out
                }
            }
        }
        Reg::X | Reg::Y => {
            let op_id = if r == Reg::X { "CPU_STACK_OP_PHX" } else { "CPU_STACK_OP_PHY" };
            let lo = widths::low_byte(field);
            match static_x {
                Some(1) => {
                    let mut out = vec!["{ uint16 _old_s = cpu->S;".to_string()];
                    for s in push_byte(&lo) {
                        out.push(format!("  {s}"));
                    }
                    out.push(format!("  cpu_trace_stack_op(cpu, 0, {op_id}, _old_s, -1); }}"));
                    out
                }
                Some(0) => {
                    let mut out = vec!["{ uint16 _old_s = cpu->S;".to_string()];
                    for s in push_word(field) {
                        out.push(format!("  {s}"));
                    }
                    out.push(format!("  cpu_trace_stack_op(cpu, 0, {op_id}, _old_s, -2); }}"));
                    out
                }
                _ => {
                    let mut out = vec![
                        "{ uint16 _old_s = cpu->S;".to_string(),
                        "  if (cpu->x_flag) {".to_string(),
                    ];
                    for s in push_byte(&lo) {
                        out.push(format!("    {s}"));
                    }
                    out.push(format!("    cpu_trace_stack_op(cpu, 0, {op_id}, _old_s, -1);"));
                    out.push("  } else {".to_string());
                    for s in push_word(field) {
                        out.push(format!("    {s}"));
                    }
                    out.push(format!("    cpu_trace_stack_op(cpu, 0, {op_id}, _old_s, -2);"));
                    out.push("  } }".to_string());
                    out
                }
            }
        }
        _ => vec![format!("/* TODO PushReg({}) */", reg_dbg(r))],
    }
}

fn emit_pullreg(r: Reg, static_m: Option<u8>, static_x: Option<u8>) -> Vec<String> {
    let field = reg(r);
    let p_sync = "cpu->P = (uint8)((cpu->P & ~0x82) | (cpu->_flag_Z ? 0x02 : 0) | (cpu->_flag_N ? 0x80 : 0));";
    match r {
        Reg::P => {
            let mut out = vec!["{ uint8 _old_p = cpu->P; uint16 _old_s = cpu->S;".to_string()];
            for s in pop_byte_assign(field) {
                out.push(format!("  {s}"));
            }
            out.push("  cpu_p_to_mirrors(cpu);".to_string());
            out.push("  cpu_trace_stack_op(cpu, 0, CPU_STACK_OP_PLP, _old_s, +1);".to_string());
            out.push("  cpu_trace_event(cpu, 0, CPU_TR_PLP, _old_p, cpu->P);".to_string());
            out.push("  cpu_trace_px_record(cpu, 0, 2 /*PLP*/, _old_p, cpu->P); }".to_string());
            out
        }
        Reg::Db => {
            let mut out = vec!["{ uint8 _old_db = cpu->DB; uint16 _old_s = cpu->S;".to_string()];
            for s in pop_byte_assign(field) {
                out.push(format!("  {s}"));
            }
            for s in widths::set_nz(field, 1) {
                out.push(format!("  {s}"));
            }
            out.push("  cpu_trace_stack_op(cpu, 0, CPU_STACK_OP_PLB, _old_s, +1);".to_string());
            out.push("  cpu_trace_db_change(cpu, 0, _old_db, cpu->DB, CPU_TR_PLB); }".to_string());
            out
        }
        Reg::Pb => {
            let mut out = vec!["{ uint8 _old_pb = cpu->PB; uint16 _old_s = cpu->S;".to_string()];
            for s in pop_byte_assign(field) {
                out.push(format!("  {s}"));
            }
            for s in widths::set_nz(field, 1) {
                out.push(format!("  {s}"));
            }
            out.push("  cpu_trace_stack_op(cpu, 0, CPU_STACK_OP_PLB, _old_s, +1);".to_string());
            out.push("  cpu_trace_pb_change(cpu, 0, _old_pb, cpu->PB, CPU_TR_PB_WRITE); }".to_string());
            out
        }
        Reg::D => {
            let mut body = pop_word_assign(field);
            body.extend(widths::set_nz(field, 2));
            stack_op_traced("CPU_STACK_OP_PLD", 2, body)
        }
        Reg::A => match static_m {
            Some(1) => {
                let mut out = vec!["{ uint16 _old_s = cpu->S;".to_string()];
                for s in pop_byte_assign("uint8 _v") {
                    out.push(format!("  {s}"));
                }
                out.push(format!("  {field} = {};", widths::preserve_high(field, "_v")));
                for s in widths::set_nz_no_p("_v", 1) {
                    out.push(format!("  {s}"));
                }
                out.push("  cpu_trace_stack_op(cpu, 0, CPU_STACK_OP_PLA, _old_s, +1);".to_string());
                out.push(format!("  {p_sync} }}"));
                out
            }
            Some(0) => {
                let mut out = vec!["{ uint16 _old_s = cpu->S;".to_string()];
                for s in pop_word_assign(field) {
                    out.push(format!("  {s}"));
                }
                for s in widths::set_nz_no_p(field, 2) {
                    out.push(format!("  {s}"));
                }
                out.push("  cpu_trace_stack_op(cpu, 0, CPU_STACK_OP_PLA, _old_s, +2);".to_string());
                out.push(format!("  {p_sync} }}"));
                out
            }
            _ => {
                let mut out = vec![
                    "{ uint16 _old_s = cpu->S;".to_string(),
                    "  if (cpu->m_flag) {".to_string(),
                ];
                for s in pop_byte_assign("uint8 _v") {
                    out.push(format!("    {s}"));
                }
                out.push(format!("    {field} = {};", widths::preserve_high(field, "_v")));
                for s in widths::set_nz_no_p("_v", 1) {
                    out.push(format!("    {s}"));
                }
                out.push("    cpu_trace_stack_op(cpu, 0, CPU_STACK_OP_PLA, _old_s, +1);".to_string());
                out.push("  } else {".to_string());
                for s in pop_word_assign(field) {
                    out.push(format!("    {s}"));
                }
                for s in widths::set_nz_no_p(field, 2) {
                    out.push(format!("    {s}"));
                }
                out.push("    cpu_trace_stack_op(cpu, 0, CPU_STACK_OP_PLA, _old_s, +2);".to_string());
                out.push("  }".to_string());
                out.push(format!("  {p_sync} }}"));
                out
            }
        },
        Reg::X | Reg::Y => {
            let op_id = if r == Reg::X { "CPU_STACK_OP_PLX" } else { "CPU_STACK_OP_PLY" };
            match static_x {
                Some(1) => {
                    let mut out = vec!["{ uint16 _old_s = cpu->S;".to_string()];
                    for s in pop_byte_assign("uint8 _v") {
                        out.push(format!("  {s}"));
                    }
                    out.push(format!(
                        "  {field} = {};  /* x=1 zeros high byte (hw contract) */",
                        widths::zero_extend_lo("_v")
                    ));
                    for s in widths::set_nz_no_p("_v", 1) {
                        out.push(format!("  {s}"));
                    }
                    out.push(format!("  cpu_trace_stack_op(cpu, 0, {op_id}, _old_s, +1);"));
                    out.push(format!("  {p_sync} }}"));
                    out
                }
                Some(0) => {
                    let mut out = vec!["{ uint16 _old_s = cpu->S;".to_string()];
                    for s in pop_word_assign(field) {
                        out.push(format!("  {s}"));
                    }
                    for s in widths::set_nz_no_p(field, 2) {
                        out.push(format!("  {s}"));
                    }
                    out.push(format!("  cpu_trace_stack_op(cpu, 0, {op_id}, _old_s, +2);"));
                    out.push(format!("  {p_sync} }}"));
                    out
                }
                _ => {
                    let mut out = vec![
                        "{ uint16 _old_s = cpu->S;".to_string(),
                        "  if (cpu->x_flag) {".to_string(),
                    ];
                    for s in pop_byte_assign("uint8 _v") {
                        out.push(format!("    {s}"));
                    }
                    out.push(format!(
                        "    {field} = {};  /* x=1 zeros high byte (hw contract) */",
                        widths::zero_extend_lo("_v")
                    ));
                    for s in widths::set_nz_no_p("_v", 1) {
                        out.push(format!("    {s}"));
                    }
                    out.push(format!("    cpu_trace_stack_op(cpu, 0, {op_id}, _old_s, +1);"));
                    out.push("  } else {".to_string());
                    for s in pop_word_assign(field) {
                        out.push(format!("    {s}"));
                    }
                    for s in widths::set_nz_no_p(field, 2) {
                        out.push(format!("    {s}"));
                    }
                    out.push(format!("    cpu_trace_stack_op(cpu, 0, {op_id}, _old_s, +2);"));
                    out.push("  }".to_string());
                    out.push(format!("  {p_sync} }}"));
                    out
                }
            }
        }
        _ => vec![format!("/* TODO PullReg({}) */", reg_dbg(r))],
    }
}

fn emit_transfer(src: Reg, dst: Reg) -> Vec<String> {
    let src_f = reg(src);
    let dst_f = reg(dst);
    if dst == Reg::S {
        return vec![
            "{ uint16 _old_s = cpu->S;".to_string(),
            format!("  {dst_f} = {src_f};"),
            "  /* trace_event uses extra0/extra1 for old/new S high bytes */".to_string(),
            "  cpu_trace_event(cpu, 0, CPU_TR_DB_WRITE,".to_string(),
            "                  (uint8)(_old_s >> 8), cpu->S); }".to_string(),
        ];
    }
    let flag: Option<&str> = match dst {
        Reg::A => Some("cpu->m_flag"),
        Reg::X | Reg::Y => Some("cpu->x_flag"),
        _ => None,
    };
    match flag {
        None => {
            let mut l = vec![format!("{dst_f} = {src_f};")];
            l.extend(widths::set_nz(dst_f, 2));
            l
        }
        Some(flag) => {
            let dst_8bit = if dst == Reg::X || dst == Reg::Y {
                format!(
                    "{dst_f} = {};  /* x=1 zeros high byte (hw contract) */",
                    widths::zero_extend_lo("_v")
                )
            } else {
                format!("{dst_f} = {};", widths::preserve_high(dst_f, "_v"))
            };
            let mut lines = vec![
                format!("if ({flag}) {{"),
                format!("  uint8 _v = {};", widths::low_byte(src_f)),
                format!("  {dst_8bit}"),
            ];
            for s in widths::set_nz_no_p("_v", 1) {
                lines.push(format!("  {s}"));
            }
            lines.push("} else {".to_string());
            lines.push(format!("  {dst_f} = (uint16)({src_f});"));
            for s in widths::set_nz_no_p(dst_f, 2) {
                lines.push(format!("  {s}"));
            }
            lines.push("}".to_string());
            lines.push(
                "cpu->P = (uint8)((cpu->P & ~0x82) | (cpu->_flag_Z ? 0x02 : 0) | (cpu->_flag_N ? 0x80 : 0));"
                    .to_string(),
            );
            lines
        }
    }
}

fn emit_stop(wait: bool) -> Vec<String> {
    if wait {
        vec!["/* WAI: wait for interrupt — runtime hook */".to_string()]
    } else {
        vec!["/* STP: halt — runtime hook */".to_string()]
    }
}

fn emit_break(cop: bool) -> Vec<String> {
    if cop {
        vec!["/* COP: software interrupt */".to_string()]
    } else {
        vec!["/* BRK: software interrupt */".to_string()]
    }
}

fn emit_pea_per_pei(seg: &SegRef) -> Vec<String> {
    let off = seg.offset;
    match seg.kind {
        SegKind::AbsBank => vec![
            "{ uint16 _old_s = cpu->S;".to_string(),
            "  cpu->S = (uint16)(cpu->S - 1);".to_string(),
            format!("  cpu_write16(cpu, 0x00, cpu->S, (uint16){off:#06x});"),
            "  cpu->S = (uint16)(cpu->S - 1);".to_string(),
            "  cpu_trace_stack_op(cpu, 0, CPU_STACK_OP_PEA, _old_s, -2); }".to_string(),
        ],
        SegKind::DpIndirect => vec![
            "{ uint16 _old_s = cpu->S;".to_string(),
            format!("  uint16 _peival = cpu_read16(cpu, 0x00, (uint16)(cpu->D + {off:#06x}));"),
            "  cpu->S = (uint16)(cpu->S - 1);".to_string(),
            "  cpu_write16(cpu, 0x00, cpu->S, _peival);".to_string(),
            "  cpu->S = (uint16)(cpu->S - 1);".to_string(),
            "  cpu_trace_stack_op(cpu, 0, CPU_STACK_OP_PEI, _old_s, -2); }".to_string(),
        ],
        _ => vec!["/* TODO PushEffectiveAddress unsupported kind */".to_string()],
    }
}

fn emit_blockmove(direction: MoveDir, src_bank: u8, dst_bank: u8) -> Vec<String> {
    let delta = if direction == MoveDir::Mvn { "+1" } else { "-1" };
    let et = if direction == MoveDir::Mvn { "CPU_TR_MVN" } else { "CPU_TR_MVP" };
    let src_bank = src_bank as u32;
    let dst_bank = dst_bank as u32;
    vec![
        "{".to_string(),
        format!("  uint8 _src_b = {src_bank:#04x};"),
        format!("  uint8 _dst_b = {dst_bank:#04x};"),
        "  uint8 _old_db = cpu->DB;".to_string(),
        format!("  cpu_trace_event(cpu, 0, {et}, _src_b, _dst_b);"),
        "  while (cpu->A != 0xFFFF) {".to_string(),
        "    uint8 _b = cpu_read8(cpu, _src_b, cpu->X);".to_string(),
        "    cpu_write8(cpu, _dst_b, cpu->Y, _b);".to_string(),
        format!("    cpu->X = (uint16)(cpu->X {delta});"),
        format!("    cpu->Y = (uint16)(cpu->Y {delta});"),
        "    cpu->A = (uint16)(cpu->A - 1);".to_string(),
        "  }".to_string(),
        "  cpu->DB = _dst_b;".to_string(),
        format!("  cpu_trace_db_change(cpu, 0, _old_db, _dst_b, {et});"),
        "}".to_string(),
    ]
}

fn emit_push(src: Value, width: u8) -> Vec<String> {
    if width == 1 {
        stack_op_traced("CPU_STACK_OP_PHA", -1, push_byte(&format!("(uint8){}", v(src))))
    } else {
        stack_op_traced("CPU_STACK_OP_PHA", -2, push_word(&v(src)))
    }
}

fn emit_pull(out: Value, width: u8) -> Vec<String> {
    if width == 1 {
        stack_op_traced("CPU_STACK_OP_PLA", 1, pop_byte_assign(&format!("uint8 {}", v(out))))
    } else {
        stack_op_traced("CPU_STACK_OP_PLA", 2, pop_word_assign(&format!("uint16 {}", v(out))))
    }
}

fn reg_dbg(r: Reg) -> String {
    // Python prints `Reg.X` style via the enum's repr; only used in dead TODO
    // arms that never fire for real opcodes. Use the value string.
    format!("Reg.{}", r.as_str())
}

// ── Return / Call / dispatch (ctx-aware) ─────────────────────────────────────

fn emit_return(op: &Return) -> Vec<String> {
    if op.interrupt {
        return vec![
            "cpu_trace_event(cpu, 0, CPU_TR_RTI, 0, 0);".to_string(),
            "{ cpu->S = (uint16)(cpu->S + 1); cpu->P = cpu_read8(cpu, 0x00, cpu->S); cpu_p_to_mirrors(cpu);".to_string(),
            "  cpu->S = (uint16)(cpu->S + 2);  /* pull + discard PC */".to_string(),
            "  if (!cpu->emulation) cpu->S = (uint16)(cpu->S + 1);  /* native: pull + discard PB */".to_string(),
            "  cpu_trace_px_record(cpu, 0, 3 /*RTI*/, cpu->P, cpu->P);".to_string(),
            "  return RECOMP_RETURN_NORMAL; /* RTI: popped interrupt frame */ }".to_string(),
        ];
    }
    let label_inner = if op.long { "RTL" } else { "RTS" };
    let src24 = op.source_pc24.unwrap_or(0) & 0xFFFFFF;
    let mut lines = vec![
        format!("{{ uint16 _ret_s = cpu->S;  /* {label_inner} pop hardware return frame */"),
        "  cpu->S = (uint16)(cpu->S + 1);".to_string(),
        "  uint16 _rpcl = (uint16)cpu_read8(cpu, 0x00, cpu->S);".to_string(),
        "  cpu->S = (uint16)(cpu->S + 1);".to_string(),
        "  uint16 _rpch = (uint16)cpu_read8(cpu, 0x00, cpu->S);".to_string(),
    ];
    if op.long {
        lines.push("  cpu->S = (uint16)(cpu->S + 1);".to_string());
        lines.push("  uint8 _rpb = cpu_read8(cpu, 0x00, cpu->S);".to_string());
    } else {
        lines.push("  uint8 _rpb = cpu->PB;".to_string());
    }
    let frame_sz = if op.long { 3 } else { 2 };
    lines.extend([
        "  uint32 _rpc = (uint32)((((_rpch << 8) | _rpcl) + 1) & 0xFFFFu);".to_string(),
        "  uint32 _rpc24 = ((uint32)_rpb << 16) | _rpc;".to_string(),
        "#if SNESRECOMP_TRACE".to_string(),
        format!("  dbg_rts_trace(cpu, 0x{src24:06x}u, _entry_s, _ret_s, _rpc24, (uint8)_hrv);"),
        "#endif".to_string(),
        "  if (_hrv && _ret_s == _entry_s) {".to_string(),
        format!("    return RECOMP_RETURN_NORMAL;  /* {label_inner} host return */ }}"),
        "  if (_ret_s != _entry_s && !cpu_dispatch_has_entry(cpu, _rpc24)) {".to_string(),
        "    int _anc_skip = cpu_resolve_ancestor_skip(_ret_s);".to_string(),
        "    if (_anc_skip >= 0) {".to_string(),
        "      cpu_trace_mark_nlr_exit(BD_EXIT_KIND_TRAMPOLINE);".to_string(),
        format!("      return (RecompReturn)_anc_skip;  /* {label_inner} return-to-ancestor */ }}"),
        "  }".to_string(),
        "  cpu_trace_mark_nlr_exit(BD_EXIT_KIND_TRAMPOLINE);".to_string(),
        format!(
            "  return cpu_dispatch_pc_from(cpu, _rpc24, (uint16)(_entry_s + {frame_sz}u), 0x{src24:06x}u);  /* {label_inner} dispatch */ }}"
        ),
    ]);
    lines
}

fn emit_return_frame_push(op: &Call) -> Vec<String> {
    let site = op.source_pc24.map(|s| s & 0xFFFFFF);
    let hrv = "  cpu->host_return_valid = 1;  /* paired host caller */";
    if op.long {
        let ret16 = site.map(|s| (s + 3) & 0xFFFF).unwrap_or(0xFFFF);
        let pbr = site.map(|s| (s >> 16) & 0xFF).unwrap_or(0xFF);
        return vec![
            "  /* JSL return frame -> cpu->S (Option-1) */".to_string(),
            format!("  cpu_write8(cpu, 0x00, cpu->S, 0x{pbr:02x}); cpu->S = (uint16)(cpu->S - 1);"),
            format!(
                "  cpu_write8(cpu, 0x00, cpu->S, 0x{:02x}); cpu->S = (uint16)(cpu->S - 1);",
                (ret16 >> 8) & 0xFF
            ),
            format!(
                "  cpu_write8(cpu, 0x00, cpu->S, 0x{:02x}); cpu->S = (uint16)(cpu->S - 1);",
                ret16 & 0xFF
            ),
            hrv.to_string(),
        ];
    }
    let ret16 = site.map(|s| (s + 2) & 0xFFFF).unwrap_or(0xFFFF);
    vec![
        "  /* JSR return frame -> cpu->S (Option-1) */".to_string(),
        format!(
            "  cpu_write8(cpu, 0x00, cpu->S, 0x{:02x}); cpu->S = (uint16)(cpu->S - 1);",
            (ret16 >> 8) & 0xFF
        ),
        format!(
            "  cpu_write8(cpu, 0x00, cpu->S, 0x{:02x}); cpu->S = (uint16)(cpu->S - 1);",
            ret16 & 0xFF
        ),
        hrv.to_string(),
    ]
}

impl EmitCtx {
    fn base_name_for(&self, addr: u32) -> String {
        if let Some(n) = self.name_resolver.get(&addr) {
            n.clone()
        } else {
            let bank = (addr >> 16) & 0xFF;
            let pc = addr & 0xFFFF;
            format!("bank_{bank:02X}_{pc:04X}")
        }
    }

    pub fn emit_call(&self, op: &Call, outcome: &mut EmitOutcome) -> Vec<String> {
        if op.indirect {
            if let (Some(src), Some(tb)) = (op.source_pc24, op.table_base) {
                return vec![format!(
                    "/* Call indirect SUPPRESSED: JSR (${:04X},X) at ${:06X} — cfg-required-dispatch-or-kill, no indirect_call_table authorisation */",
                    tb, src
                )];
            }
            return vec!["/* Call indirect SUPPRESSED — caller dispatches */".to_string()];
        }
        let target = match op.target {
            None => return vec!["/* Call: target unknown — caller dispatches */".to_string()],
            Some(t) => t,
        };
        let addr = target & 0xFFFFFF;
        if self.is_invalid_lorom_call_target(addr) && !self.name_resolver.contains_key(&addr) {
            outcome.rejected_call_targets.insert(addr);
            return vec![format!(
                "/* Call: target ${addr:06X} not a valid LoROM code address and no cfg name — skipped (decoder followed garbage operand past an RTS) */"
            )];
        }
        let base_name = self.base_name_for(addr);
        let target_bank = (addr >> 16) & 0xFF;
        for (em, ex) in self.valid_variant_list(addr) {
            outcome.unresolved_call_targets.insert((addr, em, ex));
        }
        // Pinned variant (force_variant_at). Empty in oracle env.
        let pinned = op.source_pc24.and_then(|s| self.force_variant_at.get(&(s & 0xFFFFFF)).copied());
        if let Some((pm, px)) = pinned {
            let pinned_name = format!("{base_name}{}", variant_suffix(pm, px));
            let src = op.source_pc24.unwrap();
            if op.long {
                return vec![
                    "{".to_string(),
                    "  uint8 _saved_pb = cpu->PB;".to_string(),
                    format!("  cpu_trace_pb_change(cpu, 0, _saved_pb, {target_bank:#04x}, CPU_TR_JSL);"),
                    format!("  cpu->PB = {target_bank:#04x};"),
                    format!("  RecompReturn _r = {pinned_name}(cpu);  /* cfg force_variant_at ${src:06X} -> M{pm}X{px} */"),
                    "  cpu_trace_pb_change(cpu, 0, cpu->PB, _saved_pb, CPU_TR_RTL);".to_string(),
                    "  cpu->PB = _saved_pb;".to_string(),
                    "  if (_r != RECOMP_RETURN_NORMAL) {".to_string(),
                    "    cpu_trace_event(cpu, 0, CPU_TR_NLR_PROPAGATE, (uint8)_r, 0);".to_string(),
                    "    cpu_trace_mark_nlr_exit(BD_EXIT_KIND_SKIP_PROPAGATION);".to_string(),
                    "    return (RecompReturn)((int)_r - 1);".to_string(),
                    "  }".to_string(),
                    "}".to_string(),
                ];
            }
            return vec![
                "{".to_string(),
                format!("  RecompReturn _r = {pinned_name}(cpu);  /* cfg force_variant_at ${src:06X} -> M{pm}X{px} */"),
                "  if (_r != RECOMP_RETURN_NORMAL) {".to_string(),
                "    cpu_trace_event(cpu, 0, CPU_TR_NLR_PROPAGATE, (uint8)_r, 0);".to_string(),
                "    cpu_trace_mark_nlr_exit(BD_EXIT_KIND_SKIP_PROPAGATION);".to_string(),
                "    return (RecompReturn)((int)_r - 1);".to_string(),
                "  }".to_string(),
                "}".to_string(),
            ];
        }
        if op.long {
            let mut lines = vec!["{".to_string(), "  uint16 _call_s = cpu->S;".to_string()];
            lines.extend(emit_return_frame_push(op));
            lines.extend([
                "  uint8 _saved_pb = cpu->PB;".to_string(),
                format!("  cpu_trace_pb_change(cpu, 0, _saved_pb, {target_bank:#04x}, CPU_TR_JSL);"),
                format!("  cpu->PB = {target_bank:#04x};"),
                "  RecompReturn _r;".to_string(),
                "  switch (((cpu->m_flag & 1) << 1) | (cpu->x_flag & 1)) {".to_string(),
            ]);
            lines.extend(self.variant_dispatch_case_lines(addr, &base_name, "    ", None));
            lines.extend([
                "  }".to_string(),
                "  cpu_trace_pb_change(cpu, 0, cpu->PB, _saved_pb, CPU_TR_RTL);".to_string(),
                "  cpu->PB = _saved_pb;".to_string(),
                "  if (_r != RECOMP_RETURN_NORMAL) {".to_string(),
                "    cpu_trace_event(cpu, 0, CPU_TR_NLR_PROPAGATE, (uint8)_r, 0);".to_string(),
                "    cpu_trace_mark_nlr_exit(BD_EXIT_KIND_SKIP_PROPAGATION);".to_string(),
                "    return (RecompReturn)((int)_r - 1);".to_string(),
                "  }".to_string(),
                "  cpu->S = _call_s;  /* stack-neutrality restore (see _call_s above) */".to_string(),
                "}".to_string(),
            ]);
            return lines;
        }
        let mut lines = vec!["{".to_string(), "  uint16 _call_s = cpu->S;".to_string()];
        lines.extend(emit_return_frame_push(op));
        lines.extend([
            "  RecompReturn _r;".to_string(),
            "  switch (((cpu->m_flag & 1) << 1) | (cpu->x_flag & 1)) {".to_string(),
        ]);
        lines.extend(self.variant_dispatch_case_lines(addr, &base_name, "    ", None));
        lines.extend([
            "  }".to_string(),
            "  if (_r != RECOMP_RETURN_NORMAL) {".to_string(),
            "    cpu_trace_event(cpu, 0, CPU_TR_NLR_PROPAGATE, (uint8)_r, 0);".to_string(),
            "    cpu_trace_mark_nlr_exit(BD_EXIT_KIND_SKIP_PROPAGATION);".to_string(),
            "    return (RecompReturn)((int)_r - 1);".to_string(),
            "  }".to_string(),
            "  cpu->S = _call_s;  /* stack-neutrality restore (see _call_s above) */".to_string(),
            "}".to_string(),
        ]);
        lines
    }

    pub fn emit_dispatch(&self, insn: &Insn, outcome: &mut EmitOutcome) -> Vec<String> {
        let bank = (insn.addr >> 16) & 0xFF;
        let entries = insn.dispatch_entries.as_deref().unwrap_or(&[]);
        let kind = insn.dispatch_kind.as_deref().unwrap_or("short");
        let n = entries.len();
        let em = 1u8;
        let ex = 1u8;
        let suffix = variant_suffix(em, ex);
        let mut lines = vec!["{ /* JSL dispatch — short=2B / long=3B table */".to_string()];
        lines.push(format!("  static const uint16 _disp_n = {n};"));
        lines.push(format!("  uint16 _idx = (uint16){};", widths::masked("cpu->A", 1)));
        lines.push("  if (_idx >= _disp_n) { return RECOMP_RETURN_NORMAL; /* dispatch OOB */ }".to_string());
        lines.push("  {".to_string());
        lines.push("    uint8 _old_p = cpu->P;".to_string());
        lines.push("    cpu_mirrors_to_p(cpu);".to_string());
        lines.push("    cpu->P = (uint8)(cpu->P | 0x30);".to_string());
        lines.push("    cpu_p_to_mirrors(cpu);".to_string());
        lines.push("    cpu_trace_px_record(cpu, 0, 1 /*SEP*/, _old_p, cpu->P);".to_string());
        lines.push("  }".to_string());
        lines.push("  cpu->host_return_valid = 0;  /* dispatch-trampoline target */".to_string());
        lines.push("  switch (_idx) {".to_string());
        for (i, &e) in entries.iter().enumerate() {
            if e == 0 {
                lines.push(format!("    case {i}: break;  /* null entry */"));
                continue;
            }
            let (target_bank, local_pc, tgt_addr) = if kind == "long" {
                ((e >> 16) & 0xFF, e & 0xFFFF, e)
            } else {
                (bank, e & 0xFFFF, (bank << 16) | (e & 0xFFFF))
            };
            let base_name = match self.name_resolver.get(&tgt_addr) {
                Some(n) => n.clone(),
                None => format!("bank_{target_bank:02X}_{local_pc:04X}"),
            };
            outcome.unresolved_call_targets.insert((tgt_addr, em, ex));
            let name = format!("{base_name}{suffix}");
            let env = call_with_pb_save(target_bank, &name);
            lines.push(format!("    case {i}: {{"));
            for stmt in env {
                lines.push(format!("      {stmt}"));
            }
            lines.push("    } break;".to_string());
        }
        lines.push("    default: break;".to_string());
        lines.push("  }".to_string());
        lines.push("  return RECOMP_RETURN_NORMAL; /* dispatch is a terminator */".to_string());
        lines.push("}".to_string());
        lines
    }

    pub fn emit_indirect_dispatch(
        &self,
        insn: &Insn,
        local_labels: Option<&HashSet<String>>,
        outcome: &mut EmitOutcome,
    ) -> Vec<String> {
        let entries = insn.dispatch_entries.as_deref().unwrap_or(&[]);
        let mut idx_reg = insn.dispatch_idx_reg.unwrap_or('X');
        if idx_reg != 'X' && idx_reg != 'Y' {
            idx_reg = 'X';
        }
        let n = entries.len();
        let site_pc24 = insn.addr & 0xFFFFFF;
        let is_jsr = insn.mnem == "JSR";
        let is_rts_stack_dispatch = insn.dispatch_terminal;
        let (em, ex) = if is_rts_stack_dispatch {
            (1u8, 1u8)
        } else {
            (insn.m_flag & 1, insn.x_flag & 1)
        };
        let suffix = variant_suffix(em, ex);

        let comment = if is_rts_stack_dispatch {
            "RTS-stack dispatch terminator: cfg-resolved target list"
        } else if is_jsr {
            "indirect dispatch call: cfg-resolved target list"
        } else {
            "indirect dispatch terminator: cfg-resolved target list"
        };
        let mut lines = vec![format!("{{ /* {comment} */")];
        if is_jsr {
            let iret16 = (site_pc24 + 2) & 0xFFFF;
            lines.push(format!(
                "  cpu_write8(cpu, 0x00, cpu->S, 0x{:02x}); cpu->S = (uint16)(cpu->S - 1);",
                (iret16 >> 8) & 0xFF
            ));
            lines.push(format!(
                "  cpu_write8(cpu, 0x00, cpu->S, 0x{:02x}); cpu->S = (uint16)(cpu->S - 1);",
                iret16 & 0xFF
            ));
            lines.push("  cpu->host_return_valid = 1;  /* indirect JSR call */".to_string());
        } else {
            lines.push("  cpu->host_return_valid = _hrv;  /* JMP/JML indirect tail dispatch */".to_string());
        }

        let idx_field = if idx_reg == 'X' { 'X' } else { 'Y' };
        let kind = insn.dispatch_kind.as_deref().unwrap_or("short");
        let entry_size = if kind == "long" { 3 } else { 2 };
        let table_bases = &insn.dispatch_table_bases;

        if insn.mode == Mode::Indir && table_bases.len() == 1 {
            let ptr = insn.operand & 0xFFFF;
            lines.push(format!(
                "  uint16 _target = cpu_read16(cpu, cpu->PB, (uint16)0x{ptr:04x});  /* absolute indirect dispatch: switch on the loaded pointer */"
            ));
            if is_rts_stack_dispatch {
                lines.push("  {".to_string());
                for stmt in modify_p_via_mirrors(0x30, "sep") {
                    lines.push(format!("    {stmt}"));
                }
                lines.push("  }".to_string());
            }
            lines.push("  switch (_target) {".to_string());
            let mut seen_cases: HashSet<u32> = HashSet::new();
            for &e in entries.iter() {
                if e == 0 {
                    continue;
                }
                let target_bank = (e >> 16) & 0xFF;
                let local_pc = e & 0xFFFF;
                let tgt_addr = e & 0xFFFFFF;
                let case_value = if kind == "long" { tgt_addr } else { local_pc };
                if seen_cases.contains(&case_value) {
                    continue;
                }
                seen_cases.insert(case_value);
                let base_name = match self.name_resolver.get(&tgt_addr) {
                    Some(n) => n.clone(),
                    None => format!("bank_{target_bank:02X}_{local_pc:04X}"),
                };
                lines.push(format!("    case 0x{case_value:04x}: {{"));
                if is_rts_stack_dispatch {
                    outcome.unresolved_call_targets.insert((tgt_addr, em, ex));
                    let name = format!("{base_name}{suffix}");
                    for stmt in call_with_pb_save(target_bank, &name) {
                        lines.push(format!("      {stmt}"));
                    }
                } else {
                    for (em_v, ex_v) in self.valid_variant_list(tgt_addr) {
                        outcome.unresolved_call_targets.insert((tgt_addr, em_v, ex_v));
                    }
                    lines.push("      uint8 _saved_pb = cpu->PB;".to_string());
                    lines.push(format!(
                        "      cpu_trace_pb_change(cpu, 0, _saved_pb, {target_bank:#04x}, CPU_TR_JSL);"
                    ));
                    lines.push(format!("      cpu->PB = {target_bank:#04x};"));
                    lines.push("      RecompReturn _r;".to_string());
                    lines.push(
                        "      switch (((cpu->m_flag & 1) << 1) | (cpu->x_flag & 1)) {".to_string(),
                    );
                    lines.extend(self.variant_dispatch_case_lines(tgt_addr, &base_name, "        ", None));
                    lines.push("      }".to_string());
                    lines.push(
                        "      cpu_trace_pb_change(cpu, 0, cpu->PB, _saved_pb, CPU_TR_RTL);".to_string(),
                    );
                    lines.push("      cpu->PB = _saved_pb;".to_string());
                    lines.push("      if (_r != RECOMP_RETURN_NORMAL) {".to_string());
                    lines.push(
                        "        cpu_trace_event(cpu, 0, CPU_TR_NLR_PROPAGATE, (uint8)_r, 0);".to_string(),
                    );
                    lines.push(
                        "        cpu_trace_mark_nlr_exit(BD_EXIT_KIND_SKIP_PROPAGATION);".to_string(),
                    );
                    lines.push("        return (RecompReturn)((int)_r - 1);".to_string());
                    lines.push("      }".to_string());
                }
                if is_jsr {
                    lines.push("      break;".to_string());
                } else {
                    lines.push("      return RECOMP_RETURN_NORMAL;".to_string());
                }
                lines.push("    }".to_string());
            }
            lines.push("    default: break;".to_string());
            lines.push("  }".to_string());
            if is_jsr {
                lines.push("  /* fall through to post-JSR block */".to_string());
            } else {
                lines.push(format!(
                    "  return cpu_trace_dispatch_oob(cpu, 0x{site_pc24:06x}, _target);"
                ));
            }
            lines.push("}".to_string());
            return lines;
        }

        if table_bases.len() >= 2 {
            lines.push(format!(
                "  uint16 _idx = (uint16)(cpu->{idx_field} & 0xFFFF);  /* parallel byte tables: register already holds logical index */"
            ));
        } else {
            lines.push(format!(
                "  uint16 _idx = (uint16)((cpu->{idx_field} & 0xFFFF) / {entry_size});  /* entry_size={entry_size} ({kind}); ASL[*N] + TAX in asm => {idx_field} is byte offset, divide back to logical index */"
            ));
        }
        if is_rts_stack_dispatch {
            lines.push("  {".to_string());
            for stmt in modify_p_via_mirrors(0x30, "sep") {
                lines.push(format!("    {stmt}"));
            }
            lines.push("  }".to_string());
        }
        lines.push(format!("  static const uint16 _disp_n = {n};"));
        lines.push("  if (_idx >= _disp_n) {".to_string());
        if is_jsr {
            lines.push(format!(
                "    (void)cpu_trace_dispatch_oob(cpu, 0x{site_pc24:06x}, _idx);"
            ));
        } else {
            lines.push(format!(
                "    return cpu_trace_dispatch_oob(cpu, 0x{site_pc24:06x}, _idx);"
            ));
        }
        lines.push("  }".to_string());
        let inline_loop = insn.inline_dispatch_loop;
        let site_bank = (site_pc24 >> 16) & 0xFF;
        lines.push("  switch (_idx) {".to_string());
        for (i, &e) in entries.iter().enumerate() {
            if e == 0 {
                if is_jsr {
                    lines.push(format!("    case {i}: break; /* null entry */"));
                } else {
                    lines.push(format!("    case {i}: return RECOMP_RETURN_NORMAL; /* null entry */"));
                }
                continue;
            }
            let target_bank = (e >> 16) & 0xFF;
            let local_pc = e & 0xFFFF;
            let tgt_addr = e & 0xFFFFFF;
            let base_name = match self.name_resolver.get(&tgt_addr) {
                Some(n) => n.clone(),
                None => format!("bank_{target_bank:02X}_{local_pc:04X}"),
            };
            if inline_loop && !is_jsr && target_bank == site_bank {
                let glabel = format!("L_{local_pc:04X}_M{em}X{ex}");
                let present = match local_labels {
                    None => true,
                    Some(set) => set.contains(&glabel),
                };
                if present {
                    lines.push(format!(
                        "    case {i}: goto {glabel}; /* inline_dispatch_loop -> local handler ${local_pc:04X} */"
                    ));
                    continue;
                }
            }
            lines.push(format!("    case {i}: {{"));
            if is_rts_stack_dispatch {
                outcome.unresolved_call_targets.insert((tgt_addr, em, ex));
                let name = format!("{base_name}{suffix}");
                let env = call_with_pb_save(target_bank, &name);
                for stmt in env {
                    if !is_jsr && stmt.ends_with(&format!("{name}(cpu);")) {
                        lines.push("      cpu_tailcall_inherit_return_context(_entry_s, _hrv);".to_string());
                    }
                    lines.push(format!("      {stmt}"));
                }
            } else {
                for (em_v, ex_v) in self.valid_variant_list(tgt_addr) {
                    outcome.unresolved_call_targets.insert((tgt_addr, em_v, ex_v));
                }
                lines.push("      uint8 _saved_pb = cpu->PB;".to_string());
                lines.push(format!(
                    "      cpu_trace_pb_change(cpu, 0, _saved_pb, {target_bank:#04x}, CPU_TR_JSL);"
                ));
                lines.push(format!("      cpu->PB = {target_bank:#04x};"));
                lines.push("      RecompReturn _r;".to_string());
                lines.push("      switch (((cpu->m_flag & 1) << 1) | (cpu->x_flag & 1)) {".to_string());
                let pre: Option<Vec<String>> = if !is_jsr {
                    Some(vec!["cpu_tailcall_inherit_return_context(_entry_s, _hrv);".to_string()])
                } else {
                    None
                };
                lines.extend(self.variant_dispatch_case_lines(
                    tgt_addr,
                    &base_name,
                    "        ",
                    pre.as_deref(),
                ));
                lines.push("      }".to_string());
                lines.push(
                    "      cpu_trace_pb_change(cpu, 0, cpu->PB, _saved_pb, CPU_TR_RTL);".to_string(),
                );
                lines.push("      cpu->PB = _saved_pb;".to_string());
                lines.push("      if (_r != RECOMP_RETURN_NORMAL) {".to_string());
                lines.push(
                    "        cpu_trace_event(cpu, 0, CPU_TR_NLR_PROPAGATE, (uint8)_r, 0);".to_string(),
                );
                lines.push("        cpu_trace_mark_nlr_exit(BD_EXIT_KIND_SKIP_PROPAGATION);".to_string());
                lines.push("        return (RecompReturn)((int)_r - 1);".to_string());
                lines.push("      }".to_string());
            }
            if is_jsr {
                lines.push("      break;".to_string());
            } else {
                lines.push("      return RECOMP_RETURN_NORMAL;".to_string());
            }
            lines.push("    }".to_string());
        }
        lines.push("    default: break; /* unreachable: gated above */".to_string());
        lines.push("  }".to_string());
        if is_jsr {
            lines.push("  /* fall through to post-JSR block */".to_string());
        } else {
            lines.push(format!(
                "  return cpu_trace_dispatch_oob(cpu, 0x{site_pc24:06x}, _idx);"
            ));
        }
        lines.push("}".to_string());
        lines
    }
}

// ── emit_op dispatch ─────────────────────────────────────────────────────────

/// Lower a single IR op to C lines. Pure ops use no ctx; Call uses ctx+outcome.
/// Control-flow ops (CondBranch/Goto/IndirectGoto) are handled at block level,
/// matching the Python `_emit_*` stubs.
pub fn emit_op(ctx: &EmitCtx, op: &IROp, outcome: &mut EmitOutcome) -> Vec<String> {
    let lines: Vec<String> = match op {
        IROp::Read { seg, width, out } => emit_read(seg, *width, *out),
        IROp::Write { seg, src, width } => emit_write(seg, *src, *width),
        IROp::ReadReg { reg: r, out } => emit_readreg(*r, *out),
        IROp::WriteReg { reg: r, src } => emit_writereg(*r, *src),
        IROp::ConstI { value, width, out } => emit_consti(*value, *width, *out),
        IROp::Alu { op, lhs, rhs, width, out } => emit_alu(*op, *lhs, *rhs, *width, *out),
        IROp::Shift { op, src, width, out } => emit_shift(*op, *src, *width, *out),
        IROp::IncReg { reg: r, delta } => emit_increg(*r, *delta),
        IROp::IncMem { seg, width, delta } => emit_incmem(seg, *width, *delta),
        IROp::BitTest { operand, width } => emit_bittest(*operand, *width),
        IROp::BitSetMem { seg, width } => emit_bitsetmem(seg, *width),
        IROp::BitClearMem { seg, width } => emit_bitclearmem(seg, *width),
        IROp::SetFlag { flag, value } => emit_setflag(*flag, *value),
        IROp::SetNZ { src, width } => emit_setnz(*src, *width),
        IROp::RepFlags { mask } => emit_repflags(*mask),
        IROp::SepFlags { mask } => emit_sepflags(*mask),
        IROp::Xce => emit_xce(),
        IROp::Xba => emit_xba(),
        IROp::Push { src, width } => emit_push(*src, *width),
        IROp::Pull { width, out } => emit_pull(*out, *width),
        IROp::PushReg { reg: r, static_m, static_x } => emit_pushreg(*r, *static_m, *static_x),
        IROp::PullReg { reg: r, static_m, static_x } => emit_pullreg(*r, *static_m, *static_x),
        IROp::BlockMove { direction, src_bank, dst_bank } => {
            emit_blockmove(*direction, *src_bank, *dst_bank)
        }
        IROp::CondBranch { .. } => {
            vec!["if (/* take branch — caller fills label */) { }".to_string()]
        }
        IROp::Goto => vec!["/* Goto — caller fills label */".to_string()],
        IROp::IndirectGoto { seg } => {
            let (bank, addr) = segref_addr_expr(seg);
            vec![format!("/* IndirectGoto: target = ({bank}, {addr}) — caller dispatches */")]
        }
        IROp::Call(c) => ctx.emit_call(c, outcome),
        IROp::Return(r) => emit_return(r),
        IROp::Transfer { src, dst } => emit_transfer(*src, *dst),
        IROp::Nop => vec!["/* NOP */".to_string()],
        IROp::Break { cop } => emit_break(*cop),
        IROp::Stop { wait } => emit_stop(*wait),
        IROp::PushEffectiveAddress { seg } => emit_pea_per_pei(seg),
    };
    lines.into_iter().filter(|l| !l.is_empty()).collect()
}
