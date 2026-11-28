use super::*;
use crate::{hlsl::translate, ShaderExtract, Stage};

mod fixture {
    include!("../../../examples/support/wave_guard_fixture.rs");
}

fn insn(mnemonic: &str, enc: Enc, dst: Vec<Opnd>, src: Vec<Opnd>) -> Insn {
    Insn { offset: 0, dwords: 1, enc, op: 0, raw: vec![0], mnemonic: mnemonic.into(), dst, src, aux: Default::default() }
}

fn program(mut ops: Vec<Insn>) -> Vec<Insn> {
    // Something that tells the wave's width.
    ops.insert(0, scalar("s_mov_b32", 100, vec![Opnd::ExecLo]));
    ops.push(Insn { op: 1, ..insn("s_endpgm", Enc::Sopp, vec![], vec![]) });
    for (i, op) in ops.iter_mut().enumerate() { op.offset = i * 4; }
    ops
}

fn scalar(mnemonic: &str, dst: u8, src: Vec<Opnd>) -> Insn {
    insn(mnemonic, if src.len() == 1 { Enc::Sop1 } else { Enc::Sop2 }, vec![Opnd::Sgpr(dst)], src)
}

fn compare() -> Insn {
    insn("v_cmp_gt_u32", Enc::Vopc, vec![Opnd::VccLo], vec![Opnd::Vgpr(0), Opnd::Vgpr(1)])
}

fn count(source: Opnd) -> Insn { scalar("s_bcnt1_i32_b32", 20, vec![source]) }

fn branch(op: u32, over: i64) -> Insn {
    let mut branch = insn("s_cbranch", Enc::Sopp, vec![], vec![]);
    (branch.op, branch.aux.imm) = (op, over);
    branch
}

/// Index of the definitions a program's counts take their ballots from.
fn ballots(ops: Vec<Insn>) -> Option<Vec<(usize, u8)>> {
    let found = find(&program(ops)).filter(|found| !found.uses.is_empty())?;
    Some(found.defs.iter().flat_map(|(&at, regs)| regs.iter().map(move |&r| (at, r))).collect())
}

#[test]
fn a_mask_is_captured_where_it_is_computed() {
    assert_eq!(ballots(vec![compare(), count(Opnd::VccLo)]), Some(vec![(1, 106)]));
    // Through copies and EXEC, past the point where EXEC is narrowed.
    let saved = ballots(vec![
        compare(),
        scalar("s_and_saveexec_b32", 10, vec![Opnd::VccLo]),
        scalar("s_mov_b32", 11, vec![Opnd::ExecLo]),
        insn("v_cmpx_eq_u32", Enc::Vopc, vec![Opnd::ExecLo], vec![Opnd::Sgpr(9), Opnd::Vgpr(1)]),
        count(Opnd::Sgpr(11)),
    ]);
    assert_eq!(saved, Some(vec![(3, 11)]));
    // Both of the masks that can reach the count, one of them empty.
    let gathered = ballots(vec![
        scalar("s_mov_b32", 11, vec![Opnd::ConstInt(0)]),
        compare(),
        scalar("s_or_b32", 11, vec![Opnd::Sgpr(11), Opnd::VccLo]),
        count(Opnd::Sgpr(11)),
        insn("s_cmp_lt_u32", Enc::Sopc, vec![Opnd::Scc], vec![Opnd::Sgpr(22), Opnd::ConstInt(3)]),
        branch(5, -5),
    ]);
    assert_eq!(gathered, Some(vec![(3, 11)]));
}

#[test]
fn numbers_are_not_masks() {
    for ops in [
        vec![scalar("s_mov_b32", 11, vec![Opnd::Sgpr(0)]), count(Opnd::Sgpr(11))],
        vec![scalar("s_mov_b32", 11, vec![Opnd::Literal(0xff)]), count(Opnd::Sgpr(11))],
        vec![scalar("s_add_u32", 11, vec![Opnd::Sgpr(0), Opnd::Sgpr(1)]), count(Opnd::Sgpr(11))],
        // Every lane, running or not.
        vec![scalar("s_mov_b32", 11, vec![Opnd::ConstInt(-1)]), count(Opnd::Sgpr(11))],
        // Part number.
        vec![compare(), scalar("s_and_b32", 11, vec![Opnd::VccLo, Opnd::Sgpr(0)]), count(Opnd::Sgpr(11))],
        // A mask on one path only.
        vec![
            scalar("s_mov_b32", 11, vec![Opnd::Sgpr(0)]),
            branch(4, 2),
            compare(),
            scalar("s_mov_b32", 11, vec![Opnd::VccLo]),
            count(Opnd::Sgpr(11)),
        ],
    ] {
        assert_eq!(ballots(ops), None);
    }
}

/// The same in a wave of 64 lanes.
fn wide(mut ops: Vec<Insn>) -> Option<Vec<(usize, u8)>> {
    ops.insert(0, scalar("s_mov_b64", 98, vec![Opnd::ExecLo]));
    ops.push(Insn { op: 1, ..insn("s_endpgm", Enc::Sopp, vec![], vec![]) });
    for (i, op) in ops.iter_mut().enumerate() { op.offset = i * 4; }
    let found = find(&ops).filter(|found| !found.uses.is_empty())?;
    Some(found.defs.iter().flat_map(|(&at, regs)| regs.iter().map(move |&r| (at, r))).collect())
}

fn count64(source: Opnd) -> Insn { scalar("s_bcnt1_i32_b64", 20, vec![source]) }

/// A VOP3 operation whose carry or borrow goes to `mask`.
fn carry(mnemonic: &str, mask: u8) -> Insn {
    let mut ins = insn(mnemonic, Enc::Vop3, vec![Opnd::Vgpr(3)], vec![Opnd::Vgpr(0), Opnd::Vgpr(1)]);
    ins.raw[0] = u32::from(mask) << 8;
    ins
}

#[test]
fn both_words_of_a_mask_come_from_the_same_place() {
    assert_eq!(wide(vec![compare(), count64(Opnd::VccLo)]), Some(vec![(1, 106)]));
    // The second word alone.
    let mut second = compare();
    (second.mnemonic, second.dst) = ("v_mbcnt_hi_u32_b32".into(), vec![Opnd::Vgpr(4)]);
    second.src = vec![Opnd::VccHi, Opnd::Vgpr(3)];
    assert_eq!(wide(vec![compare(), second]), Some(vec![(1, 106)]));
    for ops in [
        // The second word is a number, always or on one path.
        vec![compare(), scalar("s_mov_b32", 107, vec![Opnd::Sgpr(0)]), count64(Opnd::VccLo)],
        vec![compare(), branch(4, 1), scalar("s_mov_b32", 107, vec![Opnd::Sgpr(0)]), count64(Opnd::VccLo)],
        // Two halves of different masks.
        vec![compare(), scalar("s_mov_b32", 10, vec![Opnd::VccLo]), scalar("s_mov_b32", 11, vec![Opnd::VccLo]), count64(Opnd::Sgpr(10))],
        // A pair that starts in the middle of a mask.
        vec![
            compare(),
            scalar("s_mov_b64", 10, vec![Opnd::VccLo]),
            scalar("s_mov_b64", 12, vec![Opnd::VccLo]),
            count64(Opnd::Sgpr(11)),
        ],
    ] {
        assert_eq!(wide(ops), None);
    }
}

#[test]
fn every_scalar_write_is_seen() {
    let first = |enc, dst| insn("v_readfirstlane_b32", enc, vec![dst], vec![Opnd::Vgpr(0)]);
    for op in [
        first(Enc::Vop1, Opnd::Sgpr(11)),
        first(Enc::Vop3, Opnd::Vgpr(11)),
        insn("v_readlane_b32", Enc::Vop3, vec![Opnd::Vgpr(11)], vec![Opnd::Vgpr(0), Opnd::ConstInt(0)]),
        carry("v_mad_u64_u32", 11),
    ] {
        let kept = vec![compare(), scalar("s_mov_b32", 11, vec![Opnd::VccLo]), count(Opnd::Sgpr(11))];
        assert_eq!(ballots(kept.clone()), Some(vec![(2, 11)]));
        let mut replaced = kept;
        replaced.insert(2, op);
        assert_eq!(ballots(replaced), None);
    }
    // A carry is a mask of its own.
    assert_eq!(ballots(vec![compare(), carry("v_add_co_u32", 106), count(Opnd::VccLo)]), Some(vec![(2, 106)]));
    assert_eq!(wide(vec![compare(), carry("v_add_co_u32", 10), count64(Opnd::Sgpr(10))]), Some(vec![(2, 10)]));
    let mut short = carry("v_add_co_ci_u32", 0);
    short.enc = Enc::Vop2;
    assert_eq!(ballots(vec![compare(), short, count(Opnd::VccLo)]), Some(vec![(2, 106)]));
}

#[test]
fn a_pair_is_not_a_mask_of_32_lanes() {
    // Two masks moved at once, and one mask with its neighbour.
    let pair = |from, read| ballots(vec![
        compare(),
        scalar("s_mov_b32", 12, vec![Opnd::VccLo]),
        scalar("s_mov_b32", 13, vec![Opnd::VccLo]),
        scalar("s_mov_b64", 10, vec![from]),
        count(read),
    ]);
    assert_eq!(pair(Opnd::Sgpr(12), Opnd::Sgpr(10)), None);
    assert_eq!(pair(Opnd::Sgpr(12), Opnd::Sgpr(11)), None);
    assert_eq!(ballots(vec![compare(), count64(Opnd::VccLo)]), None);
    // Widths that contradict each other.
    assert_eq!(wide(vec![compare(), scalar("s_mov_b32", 99, vec![Opnd::ExecLo]), count(Opnd::VccLo)]), None);
    // The second word of EXEC does not exist in 32 lanes.
    assert_eq!(ballots(vec![compare(), scalar("s_mov_b32", 99, vec![Opnd::ExecHi]), count(Opnd::VccLo)]), None);
}

#[test]
fn lanes_that_parted_are_not_counted() {
    let gather = || scalar("s_or_b32", 11, vec![Opnd::Sgpr(11), Opnd::VccLo]);
    let empty = || scalar("s_mov_b32", 11, vec![Opnd::ConstInt(0)]);
    // A branch on EXEC or VCC takes the whole wave, so no lanes part.
    for op in [6, 7, 8, 9] {
        assert_eq!(ballots(vec![empty(), compare(), branch(op, 1), gather(), count(Opnd::Sgpr(11))]), Some(vec![(1, 11), (4, 11)]), "{op}");
    }
    // The condition code of a mask operation, and of a comparison with a mask.
    let code = |op: Insn| ballots(vec![empty(), compare(), op, branch(5, 1), gather(), count(Opnd::Sgpr(11))]);
    assert_eq!(code(scalar("s_and_b32", 30, vec![Opnd::VccLo, Opnd::ExecLo])), None);
    let compared = |o| insn("s_cmp_eq_u32", Enc::Sopc, vec![Opnd::Scc], vec![o, Opnd::ConstInt(0)]);
    assert_eq!(code(compared(Opnd::VccLo)), None);
    assert!(code(compared(Opnd::Sgpr(0))).is_some());
    let saved = vec![compare(), branch(8, 1), scalar("s_mov_b32", 11, vec![Opnd::ExecLo]), branch(8, 0), count(Opnd::Sgpr(11))];
    assert_eq!(ballots(saved), None, "entry value");
    let inside = |op| ballots(vec![compare(), branch(op, 2), scalar("s_mov_b32", 11, vec![Opnd::ExecLo]), count(Opnd::Sgpr(11))]);
    assert_eq!(inside(8), Some(vec![(3, 11)]));
    assert_eq!(inside(6), Some(vec![(3, 11)]));
    // Some left the program after a branch on a mask's condition code.
    let left = vec![compare(), scalar("s_and_b32", 30, vec![Opnd::VccLo, Opnd::ExecLo]), branch(5, 1),
        Insn { op: 1, ..insn("s_endpgm", Enc::Sopp, vec![], vec![]) },
        scalar("s_mov_b32", 11, vec![Opnd::ConstInt(0)]), gather(), count(Opnd::Sgpr(11))];
    assert_eq!(ballots(left), None);
}

#[test]
fn partial_waves_keep_the_old_translation() {
    let words = [0xbe8c_037e, 0x7d82_00ff, 1000, 0xbe94_0f6a, 0x7e14_0214, 0xbf81_0000u32];
    for (threads, whole) in [([64, 1, 1], true), ([8, 8, 1], true), ([16, 1, 1], false), ([33, 1, 1], false)] {
        let (mut ex, _, _): (ShaderExtract, _, _) = fixture::shader(32);
        ex.isa = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        ex.isa_data = ex.isa.clone();
        ex.psv_threads = Some(threads);
        let shader = translate(&ex).unwrap();
        assert_eq!(shader.hlsl.contains("WaveActiveBallot"), whole, "{threads:?}");
        assert_eq!(shader.required_wave_size, whole.then_some(32), "{threads:?}");
    }
}

#[test]
fn counts_read_the_ballot() {
    // v_cmp_lt_u32 vcc, 1000, v0; s_bcnt1 s20, vcc; v_mov v10, s20
    let words = |bcnt: u32| vec![0x7d82_00ff, 1000, bcnt, 0x7e14_0214];
    for (width, bcnt, save) in [(32, 0xbe94_0f6a, 0xbe8c_037eu32), (64, 0xbe94_106a, 0xbe8c_047e)] {
        let (mut ex, _, _): (ShaderExtract, _, _) = fixture::shader(width);
        assert!(matches!(ex.stage, Stage::Compute));
        let words = [vec![save], words(bcnt), vec![0xbf81_0000]].concat();
        ex.isa = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        ex.isa_data = ex.isa.clone();
        let shader = translate(&ex).unwrap();
        assert!(shader.hlsl.contains("WaveActiveBallot((vcc_lo & 1u) != 0u)"), "{}", shader.hlsl);
        let wanted = if width == 32 { "countbits(tb_bal106)" } else { "(countbits(tb_bal106) + countbits(tb_bal107))" };
        assert!(shader.hlsl.contains(wanted), "{}", shader.hlsl);
        // Every word is declared, and the wave is the guest's.
        assert!(shader.hlsl.contains("tb_bal106 = 0u"), "{}", shader.hlsl);
        assert_eq!(shader.hlsl.contains("tb_bal107 = 0u"), width == 64);
        assert_eq!(shader.required_wave_size, Some(width));
        assert_eq!(shader.profile, "cs_6_5");
    }
}

#[test]
fn branches_on_exec_and_vcc_are_decided_by_the_whole_wave() {
    // s_mov_b32 s12, exec_lo; v_cmp_lt_u32 vcc, 1000, v0; s_and_saveexec_b32 s20, vcc_lo;
    // s_cbranch_execz (or vccz) over s_mov_b32 s22, 2; s_mov_b32 exec_lo, s20; v_mov_b32 v10, s22
    let words = |branch: u32| [0xbe8c_037e, 0x7d82_00ff, 1000, 0xbe94_3c6a, branch, 0xbe96_0382, 0xbefe_0314, 0x7e14_0216, 0xbf81_0000u32];
    for (branch, whole, lane) in [
        (0xbf88_0001, "!WaveActiveAnyTrue((exec_lo & 1u) != 0u)", "(exec_lo & 1u) == 0u"),
        (0xbf86_0001, "!WaveActiveAnyTrue((vcc_lo & 1u) != 0u)", "(vcc_lo & 1u) == 0u"),
    ] {
        // A partial wave keeps each lane's own decision, which is then the same.
        for (threads, wave) in [([64, 1, 1], true), ([16, 1, 1], false)] {
            let (mut ex, _, _): (ShaderExtract, _, _) = fixture::shader(32);
            ex.isa = words(branch).iter().flat_map(|w| w.to_le_bytes()).collect();
            ex.isa_data = ex.isa.clone();
            ex.psv_threads = Some(threads);
            let shader = translate(&ex).unwrap();
            assert_eq!(shader.hlsl.contains(whole), wave, "{}", shader.hlsl);
            assert_eq!(shader.hlsl.contains(lane), !wave, "{}", shader.hlsl);
            assert_eq!(shader.required_wave_size, wave.then_some(32));
        }
    }
}

#[test]
fn a_loop_ends_when_the_whole_wave_says_so() {
    // s_mov_b32 s12, exec_lo; v_cmp_lt_u32 vcc, 1000, v0; s_and_saveexec_b32 s20, vcc_lo;
    // s_cbranch_execz over the loop; loop: v_cmp_lt_u32 vcc, 1000, v0; s_cbranch_vccz loop;
    // s_mov_b32 exec_lo, s20; v_mov_b32 v10, s22
    let words = [
        0xbe8c_037e, 0x7d82_00ff, 1000, 0xbe94_3c6a, 0xbf88_0003, 0x7d82_00ff, 1000, 0xbf86_fffd, 0xbefe_0314,
        0x7e14_0216, 0xbf81_0000u32,
    ];
    // A lane the compare left out of EXEC enters the loop with its wave, where
    // the branch before it is the wave's. Its own VCC bit never sets: the loop
    // must end on the wave's, or that lane keeps the wave in it.
    for (threads, wave) in [([64, 1, 1], true), ([16, 1, 1], false)] {
        let (mut ex, _, _): (ShaderExtract, _, _) = fixture::shader(32);
        ex.isa = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        ex.isa_data = ex.isa.clone();
        ex.psv_threads = Some(threads);
        let shader = translate(&ex).unwrap();
        let entered = if wave { "!WaveActiveAnyTrue((exec_lo & 1u) != 0u)" } else { "(exec_lo & 1u) == 0u" };
        assert!(shader.hlsl.contains(entered), "{}", shader.hlsl);
        let latch = if wave { "tb_c5 = (!WaveActiveAnyTrue((vcc_lo & 1u) != 0u));" } else { "tb_c5 = ((vcc_lo & 1u) == 0u);" };
        assert!(shader.hlsl.contains(latch), "{}", shader.hlsl);
    }
}
