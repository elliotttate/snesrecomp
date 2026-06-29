//! Basic-block CFG keyed by (PC, M, X) (port of `recompiler/v2/cfg.py`).
//!
//! Block identity is `DecodeKey` — the same key the decoder uses — so the same
//! PC reached under different (m, x) yields two distinct blocks, never merged.
//!
//! Determinism note: the Python iterated a `set` of leaders (hash order); here
//! `blocks` is a `BTreeMap` keyed by `DecodeKey`, so block iteration order is
//! deterministic. Labels are by PC, so this affects only textual layout, not
//! correctness.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::decoder::{DecodeKey, DecodedInsn, FunctionDecodeGraph};

/// Mnemonics that END a basic block: their successors start new blocks.
fn is_block_ender(mnem: &str) -> bool {
    matches!(
        mnem,
        "RTS" | "RTL" | "RTI" | "STP" | "WAI" | "BRK" // terminators
            | "BRA" | "BRL"                            // unconditional branch
            | "BPL" | "BMI" | "BVC" | "BVS" | "BCC" | "BCS" | "BNE" | "BEQ" // cond
            | "JMP" | "JSR" | "JSL"                    // control transfer
    )
}

/// A basic block keyed by `entry` (a DecodeKey).
#[derive(Debug, Clone)]
pub struct V2Block {
    pub entry: DecodeKey,
    pub insns: Vec<DecodedInsn>,
    pub successors: Vec<DecodeKey>,
    pub predecessors: Vec<DecodeKey>,
}

impl V2Block {
    pub fn last(&self) -> &DecodedInsn {
        self.insns.last().expect("V2Block has no insns")
    }
}

/// Control-flow graph for one function, keyed by DecodeKey.
#[derive(Debug, Clone)]
pub struct V2Cfg {
    pub entry: DecodeKey,
    pub blocks: BTreeMap<DecodeKey, V2Block>,
    pub dominators: BTreeMap<DecodeKey, DecodeKey>, // idom map (self for entry)
    pub dominance_frontier: BTreeMap<DecodeKey, BTreeSet<DecodeKey>>,
}

/// Block leaders: function entry, successors of block-ender insns, and join
/// points (multiple predecessors).
fn identify_leaders(
    graph: &FunctionDecodeGraph,
    preds: &HashMap<DecodeKey, Vec<DecodeKey>>,
) -> BTreeSet<DecodeKey> {
    let mut leaders: BTreeSet<DecodeKey> = BTreeSet::new();
    if let Some(e) = &graph.entry {
        leaders.insert(e.clone());
    }
    for di in graph.insns() {
        if is_block_ender(di.insn.mnem) {
            for s in &di.successors {
                if graph.contains(s) {
                    leaders.insert(s.clone());
                }
            }
        }
    }
    for (key, ps) in preds {
        if ps.len() > 1 && graph.contains(key) {
            leaders.insert(key.clone());
        }
    }
    leaders
}

/// For each leader, walk forward through the decode graph (following the single
/// canonical successor for non-control-flow insns) until a block-ender or
/// another leader.
fn build_blocks(
    graph: &FunctionDecodeGraph,
    leaders: &BTreeSet<DecodeKey>,
) -> BTreeMap<DecodeKey, V2Block> {
    let mut blocks: BTreeMap<DecodeKey, V2Block> = BTreeMap::new();

    for leader in leaders {
        if !graph.contains(leader) {
            continue;
        }
        let mut block_insns: Vec<DecodedInsn> = Vec::new();
        let mut seen: BTreeSet<DecodeKey> = BTreeSet::new();
        let mut cur = leader.clone();
        loop {
            let di = match graph.get(&cur) {
                Some(d) => d,
                None => break,
            };
            if seen.contains(&cur) {
                break; // defensive against unexpected cycles
            }
            seen.insert(cur.clone());
            block_insns.push(di.clone());

            if is_block_ender(di.insn.mnem) {
                break;
            }
            // Non-control-flow insn: exactly one canonical fall-through.
            if di.successors.len() != 1 {
                break;
            }
            let nxt = di.successors[0].clone();
            if leaders.contains(&nxt) && nxt != *leader {
                break;
            }
            cur = nxt;
        }

        if block_insns.is_empty() {
            continue;
        }
        let successors = block_insns.last().unwrap().successors.clone();
        blocks.insert(
            leader.clone(),
            V2Block {
                entry: leader.clone(),
                insns: block_insns,
                successors,
                predecessors: Vec::new(),
            },
        );
    }

    // Wire predecessors (iterate in deterministic key order).
    let succ_pairs: Vec<(DecodeKey, Vec<DecodeKey>)> =
        blocks.iter().map(|(k, b)| (k.clone(), b.successors.clone())).collect();
    for (entry_key, succs) in succ_pairs {
        for s in succs {
            if let Some(b) = blocks.get_mut(&s) {
                b.predecessors.push(entry_key.clone());
            }
        }
    }

    blocks
}

/// Iterative Cooper-Harvey-Kennedy dominator computation. idom[entry]==entry;
/// unreachable nodes omitted.
fn compute_dominators(
    blocks: &BTreeMap<DecodeKey, V2Block>,
    entry: &DecodeKey,
) -> BTreeMap<DecodeKey, DecodeKey> {
    if !blocks.contains_key(entry) {
        return BTreeMap::new();
    }

    // Post-order via iterative DFS (matches the Python recursive dfs that
    // appends a node after visiting its successors).
    let mut order: Vec<DecodeKey> = Vec::new();
    let mut visited: BTreeSet<DecodeKey> = BTreeSet::new();
    // Stack of (node, next-successor-index).
    let mut stack: Vec<(DecodeKey, usize)> = Vec::new();
    visited.insert(entry.clone());
    stack.push((entry.clone(), 0));
    while let Some((node, idx)) = stack.last().cloned() {
        let blk = blocks.get(&node);
        let succs: &[DecodeKey] = blk.map(|b| b.successors.as_slice()).unwrap_or(&[]);
        if idx < succs.len() {
            stack.last_mut().unwrap().1 += 1;
            let s = succs[idx].clone();
            if blocks.contains_key(&s) && !visited.contains(&s) {
                visited.insert(s.clone());
                stack.push((s, 0));
            }
        } else {
            order.push(node);
            stack.pop();
        }
    }

    let rpo: Vec<DecodeKey> = order.iter().rev().cloned().collect();
    let rpo_index: HashMap<DecodeKey, usize> =
        rpo.iter().enumerate().map(|(i, n)| (n.clone(), i)).collect();

    let mut idom: BTreeMap<DecodeKey, DecodeKey> = BTreeMap::new();
    idom.insert(entry.clone(), entry.clone());

    let intersect = |idom: &BTreeMap<DecodeKey, DecodeKey>,
                     b1: &DecodeKey,
                     b2: &DecodeKey|
     -> DecodeKey {
        let mut f1 = b1.clone();
        let mut f2 = b2.clone();
        while f1 != f2 {
            while rpo_index[&f1] > rpo_index[&f2] {
                f1 = idom[&f1].clone();
            }
            while rpo_index[&f2] > rpo_index[&f1] {
                f2 = idom[&f2].clone();
            }
        }
        f1
    };

    let mut changed = true;
    while changed {
        changed = false;
        for node in rpo.iter().skip(1) {
            let blk = &blocks[node];
            let processed: Vec<DecodeKey> = blk
                .predecessors
                .iter()
                .filter(|p| idom.contains_key(*p))
                .cloned()
                .collect();
            if processed.is_empty() {
                continue;
            }
            let mut new_idom = processed[0].clone();
            for p in &processed[1..] {
                new_idom = intersect(&idom, p, &new_idom);
            }
            if idom.get(node) != Some(&new_idom) {
                idom.insert(node.clone(), new_idom);
                changed = true;
            }
        }
    }

    idom
}

/// Cytron 1991 dominance-frontier algorithm.
fn compute_dominance_frontier(
    blocks: &BTreeMap<DecodeKey, V2Block>,
    idom: &BTreeMap<DecodeKey, DecodeKey>,
) -> BTreeMap<DecodeKey, BTreeSet<DecodeKey>> {
    let mut df: BTreeMap<DecodeKey, BTreeSet<DecodeKey>> =
        blocks.keys().map(|b| (b.clone(), BTreeSet::new())).collect();

    for (node, blk) in blocks {
        if blk.predecessors.len() < 2 {
            continue;
        }
        for p in &blk.predecessors {
            if !idom.contains_key(p) {
                continue;
            }
            let mut runner = p.clone();
            while Some(&runner) != idom.get(node) && idom.contains_key(&runner) {
                df.get_mut(&runner).unwrap().insert(node.clone());
                let next_runner = idom[&runner].clone();
                if next_runner == runner {
                    break;
                }
                runner = next_runner;
            }
        }
    }

    df
}

/// Build a V2Cfg from a v2-decoded function graph.
pub fn build_cfg(graph: &FunctionDecodeGraph) -> V2Cfg {
    let entry = graph.entry.clone().expect("graph has no entry");

    // Reverse-edge map: which keys cite each key as a successor?
    let mut preds: HashMap<DecodeKey, Vec<DecodeKey>> = HashMap::new();
    for di in graph.insns() {
        for s in &di.successors {
            preds.entry(s.clone()).or_default().push(di.key.clone());
        }
    }

    let leaders = identify_leaders(graph, &preds);
    let blocks = build_blocks(graph, &leaders);
    let idom = compute_dominators(&blocks, &entry);
    let df = compute_dominance_frontier(&blocks, &idom);

    V2Cfg { entry, blocks, dominators: idom, dominance_frontier: df }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::{decode_function, DecodeEnv};

    fn rom_at_8000(bytes: &[u8]) -> Vec<u8> {
        // bank 0 $8000 maps to ROM offset 0 (LoROM), so code goes at the start.
        let mut rom = bytes.to_vec();
        rom.resize(0x10000, 0);
        rom
    }

    #[test]
    fn single_block_linear_rts() {
        // LDA #$01 ; RTS  -> one block, entry == only leader.
        let rom = rom_at_8000(&[0xA9, 0x01, 0x60]);
        let env = DecodeEnv::default();
        let g = decode_function(&rom, 0x00, 0x8000, 1, 1, None, &env);
        let cfg = build_cfg(&g);
        assert_eq!(cfg.blocks.len(), 1);
        let b = cfg.blocks.values().next().unwrap();
        assert_eq!(b.insns.len(), 2);
        assert_eq!(b.last().insn.mnem, "RTS");
        assert_eq!(cfg.dominators[&cfg.entry], cfg.entry);
    }

    #[test]
    fn diamond_has_dominance_frontier() {
        // BEQ skips one insn: entry block, taken/fall blocks, join block.
        //   $8000 BEQ $8004    (F0 02)
        //   $8002 LDA #$00     (A9 00)
        //   $8004 RTS          (60)
        let rom = rom_at_8000(&[0xF0, 0x02, 0xA9, 0x00, 0x60]);
        let env = DecodeEnv::default();
        let g = decode_function(&rom, 0x00, 0x8000, 1, 1, None, &env);
        let cfg = build_cfg(&g);
        // entry block ($8000), fall block ($8002), join block ($8004).
        assert!(cfg.blocks.len() >= 3);
        // The join block ($8004) has 2 predecessors.
        let join = cfg.blocks.values().find(|b| (b.entry.pc & 0xFFFF) == 0x8004).unwrap();
        assert_eq!(join.predecessors.len(), 2);
        // Every block is dominated by entry.
        assert!(cfg.dominators.contains_key(&cfg.entry));
    }
}
