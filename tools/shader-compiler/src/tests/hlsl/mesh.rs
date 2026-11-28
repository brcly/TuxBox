use super::*;

#[test]
fn launch_requires_the_packed_group_and_wave64_lane_definitions() {
    let words: [u32; 10] = [
        0xd7650000, 0x000100c1, // v0 = mbcnt_lo(-1, 0)
        0x7e000505, // s0 = readfirstlane(v5)
        0x9381ff03, 0x00040018, // s1 = s3[27:24]
        0xd7660000, 0x000200c1, // v0 = mbcnt_hi(-1, v0)
        0x900a8800, // s10 = s0 >> 8
        0xd7460001, 0x04010c01, // v1 = (s1 << 6) + v0
    ];
    let check = |words: &[u32]| {
        let bytes = words
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect::<Vec<_>>();
        validate_launch(&disassemble(&bytes), 8, Opnd::Vgpr(1))
    };
    assert!(check(&words).is_ok());
    let mut wrong_wave = words;
    wrong_wave[4] = 0x00040017;
    assert!(check(&wrong_wave).is_err());
    // An s_load_dwordx4 into s[0:3] clobbers both intermediate values.
    let mut clobbered = words.to_vec();
    clobbered.splice(7..7, [0xf408000d, 0xfa000000]);
    assert!(check(&clobbered).is_err());
    let mut wrong_group = words;
    wrong_group[7] = 0x900a8700;
    assert!(check(&wrong_group).is_err());
}

#[test]
fn only_uniform_values_can_bypass_a_wave_broadcast() {
    let validate = |words: &[u32]| {
        let bytes = words
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect::<Vec<_>>();
        { let body = disassemble(&bytes); validate_lane_ops(&body, &waterfall_reads(&body), false, false) }
    };
    // Entry v5 holds the same packed group identifier in every lane.
    assert!(validate(&[0x7e000505]).is_ok());
    // Counting preceding lanes overwrites v5 with a lane-dependent value.
    assert!(validate(&[0xd7650005, 0x000100c1, 0x7e000505]).is_err());
    // A new constant in v5 is safe to broadcast.
    assert!(validate(&[0x7e0a0281, 0x7e000505]).is_ok());
    // Shared memory would require synchronization among logical Xbox waves.
    assert!(validate(&[0xd8380100, 0x00020100]).is_err());
}

#[test]
fn a_scalar_add_reads_the_number_a_multiply_left_in_vcc() {
    let validate = |words: &[u32]| {
        let bytes = words.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<_>>();
        let body = disassemble(&bytes);
        validate_lane_ops(&body, &waterfall_reads(&body), true, false)
    };
    let multiply = 0x936a_0100; // s_mul_i32 vcc_lo, s0, s1
    let add = [0x8102_ff6a, 0x80]; // s_add_i32 s2, vcc_lo, 128
    let compare = 0x7d84_0000; // v_cmp_eq_u32 vcc, s0, v0
    let branch = 0xbf84_0000; // s_cbranch_scc0 to the next instruction
    assert_eq!(validate(&[multiply, add[0], add[1]]), Ok(()));
    // VCC as a compare left it holds one bit per lane.
    assert!(validate(&[compare, add[0], add[1]]).is_err());
    assert!(validate(&[multiply, compare, add[0], add[1]]).is_err());
    // Nothing proves what VCC holds on another path into the add.
    assert!(validate(&[add[0], add[1]]).is_err());
    assert!(validate(&[multiply, branch, add[0], add[1]]).is_err());
}

#[test]
fn class5_mesh_pointer_is_eight_above_the_user_location() {
    assert_class5_mesh_pointer();
}

#[test]
fn class5_descriptor_load_uses_the_guest_heap_index() {
    assert_class5_descriptor_load();
}

/// One register holds both output counts, so the compiler checks it once and
/// combines each mask with itself. Assembled with llvm-mc for wave64.
const ONE_COUNT: [u32; 70] = [
    0x7e3c0280, 0xbf068014, 0x8580807e, 0xb5940080, 0x8582807e, 0x88800000,
    0x88820202, 0x88800002, 0xd5010001, 0x00010280, 0x7e000501, 0x87000081,
    0xbf068100, 0xbe80047e, 0x8582807e, 0xd5010001, 0x00090214, 0x8984c102,
    0x7e0c0501, 0x7da40080, 0xbf88000d, 0x8f078c06, 0x8aea027e, 0x88060607,
    0xbefc0306, 0xbf900009, 0xbf8c0000, 0xbf870006, 0xbf8a0000, 0x7e027081,
    0xf80008c0, 0x00000000, 0xf8000941, 0x00000001, 0xbf8cff0f, 0xbefe0400,
    0xbf8a0000, 0x7d880014, 0x8780046a, 0xbe862400, 0xbf880012, 0xd5480001,
    0x02291513, 0xd5480002, 0x02292913, 0x363026ff, 0x000003ff, 0x7d860214,
    0xd4c30002, 0x00020414, 0xd4c30000, 0x00023014, 0x8882026a, 0x88800200,
    0xd5010001, 0x0001ff13, 0x80000000, 0xf8000941, 0x00000001, 0xbf8cff0f,
    0xbefe0406, 0x7d880014, 0x8780046a, 0xbe802400, 0xbf880004, 0xf80008cf,
    0x33323130, 0xf800020f, 0x1211100f, 0xbf810000,
];

#[test]
fn one_count_for_vertices_and_primitives_is_recognized() {
    let words = ONE_COUNT;
    let recognize = |words: &[u32]| {
        let bytes = words.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<_>>();
        super::super::mesh_exports::recognize(&disassemble(&bytes), 128, None)
    };
    let found = recognize(&words).unwrap_or_else(|reason| panic!("{reason}"));
    assert_eq!((found.vertices, found.primitives), (Opnd::Sgpr(20), Opnd::Sgpr(20)));
    assert_eq!(found.lane_index, Opnd::Vgpr(0));
    assert_eq!((found.primitive_exports.len(), found.vertex_exports.len()), (0, 2));
    // A larger count limit is a different program.
    let mut limit = words;
    assert_eq!(limit[3], 0xb5940080);
    limit[3] = 0xb5940081;
    assert!(recognize(&limit).is_err());
    // The self-combinations must be of the mask itself.
    let mut mixed = words;
    assert_eq!(mixed[5], 0x88800000);
    mixed[5] = 0x88800200;
    assert!(recognize(&mixed).is_err());
}

const HALF: u32 = 0x7e0e1501; // v_cvt_f16_f32 v7, v1: bits 31:16 keep the launch value
const LOW: u32 = 0x7e101707; // v_cvt_f32_f16 v8, v7: reads bits 15:0
const WIDEN: u32 = 0x7e0e1707; // v_cvt_f32_f16 v7, v7: a full write
const COPY: u32 = 0x7e100307; // v_mov_b32 v8, v7: reads all 32 bits
const END: u32 = 0xbf810000; // s_endpgm
const OPEN: u32 = 0xbeac047e; // s_mov_b64 s[44:45], exec
const GUARD: u32 = 0x7da8002e; // v_cmpx_gt_u32 s46, v0: the vertex count against the lane
const CLOSE: u32 = 0xbefe042c; // s_mov_b64 exec, s[44:45]
const SAVE: u32 = 0xbe84047e; // s_mov_b64 s[4:5], exec
const RESTORE: u32 = 0xbefe0404; // s_mov_b64 exec, s[4:5]
const NARROW: u32 = 0x7da40280; // v_cmpx_eq_u32 0, v1

fn bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// `body` in the block the vertex-count guard opens, then `after`.
fn guarded(body: &[u32], after: &[u32]) -> Vec<u32> {
    let mut words = vec![OPEN, GUARD, 0xbf880000 | body.len() as u32]; // s_cbranch_execz to CLOSE
    words.extend(body);
    words.push(CLOSE);
    words.extend(after);
    words.push(END);
    words
}

/// Whether v7 keeps only unread launch bits, with the export sequence from word
/// `start` and its vertex exports from word `vertices`.
fn kept_from(words: &[u32], start: usize, vertices: usize) -> bool {
    let insns = disassemble(&bytes(words));
    let at = |word: usize| insns.iter().position(|op| op.offset == 4 * word).unwrap_or(insns.len());
    let exports = super::mesh_exports::MeshExports {
        start: at(start),
        vertex_start: at(vertices),
        lane_index: Opnd::Vgpr(0),
        vertices: Opnd::Sgpr(46),
        primitives: Opnd::Sgpr(48),
        connectivity: Opnd::Vgpr(19),
        guarded_connectivity: Opnd::Vgpr(1),
        connectivity_without_primitive: Opnd::Sgpr(48),
        vertex_exports: Vec::new(),
        primitive_exports: Vec::new(),
    };
    kept_half_unread(&insns, 7, &exports)
}

/// The same, for a program without an export sequence.
fn kept(words: &[u32]) -> bool {
    kept_from(words, words.len(), words.len())
}

#[test]
fn a_half_result_hides_only_launch_bits_no_reader_takes() {
    assert!(kept(&guarded(&[HALF, LOW], &[])));
    assert!(kept(&guarded(&[HALF, WIDEN, COPY], &[])));
    assert!(kept(&guarded(&[HALF, 0xd7110008, 0x00020307], &[])), "v_pack_b32_f16 v8, v7, v1");
    assert!(kept(&guarded(&[HALF, 0xe0341000, 0x80000206, LOW], &[])), "a load addressed by v6 alone");
    assert!(kept(&guarded(&[HALF, 0xd7600007, 0x00010100, LOW], &[])), "v_readlane_b32 writes s7");
    assert!(!kept(&guarded(&[HALF, COPY], &[])));
    assert!(!kept(&guarded(&[COPY, HALF, LOW], &[])), "the launch value is read first");
    assert!(!kept(&guarded(&[HALF, 0xd7110808, 0x00020307], &[])), "op_sel reads the kept high half");
    assert!(!kept(&guarded(&[HALF, 0x560e0501], &[])), "v_fmac_f32 accumulates into v7");
    assert!(!kept(&guarded(&[HALF, 0xe0741000, 0x80000600], &[])), "buffer_store_dwordx2 v[6:7]");
    assert!(!kept(&guarded(&[HALF, 0xe0303000, 0x80000206, LOW], &[])), "an idxen offen address v[6:7]");
    // A path around the full write, or lanes it skips, still see the launch bits.
    assert!(!kept(&guarded(&[HALF, 0xbf840001, WIDEN, COPY], &[])), "s_cbranch_scc0 past the full write");
    assert!(!kept(&guarded(&[HALF, NARROW, WIDEN, COPY], &[])), "v_cmpx_eq_u32 before the full write");
}

#[test]
fn a_half_result_is_kept_only_behind_the_vertex_count_guard() {
    // The recorded shape: the block narrows further and opens another for the
    // primitive count after it closes.
    let nested = [HALF, SAVE, NARROW, 0xbf880001, LOW, RESTORE, LOW];
    let primitives = [0xbe80047e, 0x7da80030, 0xbefe0400]; // s_mov_b64 s[0:1], exec; v_cmpx_gt_u32 s48, v0; s_mov_b64 exec, s[0:1]
    assert!(kept(&guarded(&nested, &primitives)));
    // Without the guard, or with one that tests another count or lane, lanes past
    // the vertex count never execute the 16-bit write.
    assert!(!kept(&[HALF, LOW, END]));
    assert!(!kept(&guarded(&[HALF, LOW], &[]).iter().map(|&w| if w == GUARD { 0x7da80030 } else { w }).collect::<Vec<_>>()), "s48 counts primitives");
    assert!(!kept(&guarded(&[HALF, LOW], &[]).iter().map(|&w| if w == GUARD { 0x7da8022e } else { w }).collect::<Vec<_>>()), "v1 is not the lane");
    assert!(!kept(&guarded(&[0x812e812e, HALF, LOW], &[])), "s_add_i32 s46, s46, 1 after the guard");
    assert!(!kept(&guarded(&[HALF, LOW], &[0x7e000301])), "v_mov_b32 v0, v1 after the guard");
    // (A) A branch from before the 16-bit write to after the full write.
    assert!(!kept(&[0xbf840002, HALF, WIDEN, COPY, END]), "s_cbranch_scc0 +2 without a guard");
    assert!(!kept(&guarded(&[0xbf840002, HALF, WIDEN, COPY], &[])), "s_cbranch_scc0 +2 inside the block");
    // (B) An s_and_saveexec_b64 before the 16-bit write, restored after the full write.
    let and_saveexec = 0xbe84246a; // s_and_saveexec_b64 s[4:5], vcc
    assert!(!kept(&[and_saveexec, HALF, WIDEN, RESTORE, COPY, END]));
    assert!(!kept(&guarded(&[and_saveexec, HALF, WIDEN, RESTORE, COPY], &[])));
    // (C) The same with v_cmpx.
    assert!(!kept(&[SAVE, NARROW, HALF, WIDEN, RESTORE, COPY, END]));
    assert!(!kept(&guarded(&[SAVE, NARROW, HALF, WIDEN, RESTORE, COPY], &[])));
    // (D) EXEC narrowed around the 16-bit write only, then a low-half read.
    assert!(!kept(&[SAVE, NARROW, HALF, RESTORE, LOW, END]));
    assert!(!kept(&guarded(&[SAVE, NARROW, HALF, RESTORE, LOW], &[])));
    // EXEC widens again inside the block.
    assert!(!kept(&guarded(&[HALF, CLOSE, LOW], &[])), "the block's own restore, early");
    assert!(!kept(&guarded(&[HALF, 0xbefe04c1, LOW], &[])), "s_mov_b64 exec, -1");
    // After the block every lane runs, and lanes past the count hold the launch value.
    assert!(!kept(&guarded(&[HALF], &[LOW])));
    // s_branch from after the block back to the copy inside it.
    assert!(!kept(&guarded(&[HALF, WIDEN, COPY], &[0xbf82fffd])));
}

#[test]
fn a_kept_half_reaches_only_the_vertex_exports() {
    // An export sequence from word 6: a primitive part, then the vertex exports from word 7.
    let program = |primitive: u32| guarded(&[HALF, WIDEN], &[primitive, 0xf80008cf, 0x33323107]); // exp pos0 v7, v49, v50, v51 done
    assert!(kept_from(&program(0xbf800000), 6, 7), "s_nop");
    assert!(!kept_from(&program(COPY), 6, 7), "the primitive exports run lanes past the vertex count");
}

#[test]
fn a_guarded_half_result_in_a_recognized_program() {
    // The guard, the 16-bit write and the full write, then a 128-vertex export
    // sequence with the vertex count in s46 and the primitive count in s48, as in
    // the recorded programs. Its position export reads v7.
    let mut words = vec![OPEN, GUARD, 0xbf880002, HALF, WIDEN, CLOSE];
    words.extend(separate_count_exports());
    let check = |words: &[u32]| {
        let insns = disassemble(&bytes(words));
        let exports = super::mesh_exports::recognize(&insns, 128, None).unwrap();
        assert_eq!((exports.vertices, exports.lane_index), (Opnd::Sgpr(46), Opnd::Vgpr(0)));
        kept_half_unread(&insns, 7, &exports)
    };
    assert!(check(&words));
    let mut primitive_guard = words.clone();
    primitive_guard[1] = 0x7da80030; // v_cmpx_gt_u32 s48, v0
    assert!(!check(&primitive_guard));
}

#[test]
fn a_guarded_half_result_ahead_of_one_shared_count() {
    // One register, s20, is both counts. The vertex exports still test it against
    // v0, so a guard on it keeps the same lanes they run; no other register could
    // stand for the vertex count. The position export reads v7.
    let guard = 0x7da80014; // v_cmpx_gt_u32 s20, v0
    let program = |block: &[u32], after: &[u32], exports: &[u32]| {
        let mut words = vec![OPEN, guard, 0xbf880000 | block.len() as u32];
        words.extend(block);
        words.push(CLOSE);
        words.extend(after);
        words.extend(exports);
        let insns = disassemble(&bytes(&words));
        let found = super::mesh_exports::recognize(&insns, 128, None).unwrap_or_else(|reason| panic!("{reason}"));
        assert_eq!((found.vertices, found.primitives, found.lane_index), (Opnd::Sgpr(20), Opnd::Sgpr(20), Opnd::Vgpr(0)));
        (insns, found)
    };
    let kept = |(insns, found): (Vec<Insn>, super::mesh_exports::MeshExports)| kept_half_unread(&insns, 7, &found);
    let mut position = ONE_COUNT;
    assert_eq!(position[66], 0x33323130);
    position[66] = 0x33323107; // exp pos0 v7, v49, v50, v51 done
    // The allocation's s_cmpk_gt_u32 s20, 0x80 reads the count and does not write it.
    assert!(kept(program(&[HALF, WIDEN], &[], &position)));
    assert!(kept(program(&[HALF, LOW, WIDEN], &[], &position)));
    // A guard on a register that is not the count, or on another lane.
    let other = |(mut insns, found): (Vec<Insn>, super::mesh_exports::MeshExports), word: u32| {
        insns[1] = disassemble(&bytes(&[word])).remove(0);
        insns[1].offset = 4;
        kept_half_unread(&insns, 7, &found)
    };
    assert!(!other(program(&[HALF, WIDEN], &[], &position), GUARD), "s46 is not the count");
    assert!(!other(program(&[HALF, WIDEN], &[], &position), 0x7da80214), "v_cmpx_gt_u32 s20, v1");
    // The count rewritten between the guard and the exports.
    assert!(!kept(program(&[HALF, WIDEN], &[0x81148114], &position)), "s_add_i32 s20, s20, 1");
    // Lanes past the count read the register after the block.
    assert!(!kept(program(&[HALF], &[LOW], &position)));
    assert!(!kept(program(&[HALF, WIDEN], &[0x7e100507], &position)), "v_readfirstlane_b32 s8, v7");
    // A swizzle takes the register whole, from other lanes.
    assert!(!kept(program(&[HALF, 0xd8d40000, 0x08000007, WIDEN], &[], &position)), "ds_swizzle_b32 v8, v7");
    // The primitive part reads v7 as the packed triangle. With one count its lanes
    // are those of the guard, but the proof stops at the vertex exports.
    let mut triangle = position;
    assert_eq!((triangle[42], triangle[44], triangle[45], triangle[55]), (0x02291513, 0x02292913, 0x363026ff, 0x0001ff13));
    (triangle[42], triangle[44], triangle[45], triangle[55]) = (0x02291507, 0x02292907, 0x36300eff, 0x0001ff07);
    assert!(!kept(program(&[HALF, WIDEN], &[], &triangle)));
}

/// A 128-vertex export sequence with separate counts, assembled with
/// `llvm-mc -mcpu=gfx1030 -mattr=+wavefrontsize64`.
fn separate_count_exports() -> Vec<u32> {
    vec![
        0xbf06802e, // s_cmp_eq_u32 s46, 0
        0xb0060080, // s_movk_i32 s6, 0x80
        0x8580807e, // s_cselect_b64 s[0:1], exec, 0
        0xbf068030, // s_cmp_eq_u32 s48, 0
        0x8582807e, // s_cselect_b64 s[2:3], exec, 0
        0xbf0a2e06, // s_cmp_lt_u32 s6, s46
        0x8584807e, // s_cselect_b64 s[4:5], exec, 0
        0xbf0a3006, // s_cmp_lt_u32 s6, s48
        0x8586807e, // s_cselect_b64 s[6:7], exec, 0
        0x88800200, // s_or_b64 s[0:1], s[0:1], s[2:3]
        0x88820604, // s_or_b64 s[2:3], s[4:5], s[6:7]
        0x88800002, // s_or_b64 s[0:1], s[2:3], s[0:1]
        0xd5010001, 0x00010280, // v_cndmask_b32_e64 v1, 0, 1, s[0:1]
        0x7e000501, // v_readfirstlane_b32 s0, v1
        0x87000081, // s_and_b32 s0, 1, s0
        0xbf068100, // s_cmp_eq_u32 s0, 1
        0xbe80047e, // s_mov_b64 s[0:1], exec
        0x8582807e, // s_cselect_b64 s[2:3], exec, 0
        0xd5010002, 0x0009022e, // v_cndmask_b32_e64 v2, s46, 1, s[2:3]
        0xd5010003, 0x00090230, // v_cndmask_b32_e64 v3, s48, 1, s[2:3]
        0x8984c102, // s_xor_b64 s[4:5], s[2:3], -1
        0x7e0c0502, // v_readfirstlane_b32 s6, v2
        0x7e0e0503, // v_readfirstlane_b32 s7, v3
        0x7da40080, // v_cmpx_eq_u32_e32 0, v0
        0xbf88000d, // s_cbranch_execz restore
        0x8f078c07, // s_lshl_b32 s7, s7, 12
        0x8aea027e, // s_andn2_b64 vcc, exec, s[2:3]
        0x88060607, // s_or_b32 s6, s7, s6
        0xbefc0306, // s_mov_b32 m0, s6
        0xbf900009, // s_sendmsg sendmsg(MSG_GS_ALLOC_REQ)
        0xbf8c0000, // s_waitcnt vmcnt(0) expcnt(0) lgkmcnt(0)
        0xbf870006, // s_cbranch_vccnz restore
        0xbf8a0000, // s_barrier
        0x7e027081, // v_bfrev_b32_e32 v1, 1
        0xf80008c0, 0x00000000, // exp pos0 off, off, off, off done
        0xf8000941, 0x00000001, // exp prim v1, off, off, off done
        0xbf8cff0f, // s_waitcnt expcnt(0)
        0xbefe0400, // s_mov_b64 exec, s[0:1]
        0xbf8a0000, // s_barrier
        0x7d880030, // v_cmp_gt_u32_e32 vcc, s48, v0
        0x8780046a, // s_and_b64 s[0:1], vcc, s[4:5]
        0xbe862400, // s_and_saveexec_b64 s[6:7], s[0:1]
        0xbf880012, // s_cbranch_execz vertices
        0xd5480001, 0x02291513, // v_bfe_u32 v1, v19, 10, 10
        0xd5480002, 0x02292913, // v_bfe_u32 v2, v19, 20, 10
        0x363026ff, 0x000003ff, // v_and_b32_e32 v24, 0x3ff, v19
        0x7d86022e, // v_cmp_le_u32_e32 vcc, s46, v1
        0xd4c30002, 0x0002042e, // v_cmp_le_u32_e64 s[2:3], s46, v2
        0xd4c30000, 0x0002302e, // v_cmp_le_u32_e64 s[0:1], s46, v24
        0x8882026a, // s_or_b64 s[2:3], vcc, s[2:3]
        0x88800200, // s_or_b64 s[0:1], s[0:1], s[2:3]
        0xd5010001, 0x0001ff13, 0x80000000, // v_cndmask_b32_e64 v1, v19, 0x80000000, s[0:1]
        0xf8000941, 0x00000001, // exp prim v1, off, off, off done
        0xbf8cff0f, // s_waitcnt expcnt(0)
        0xbefe0406, // s_mov_b64 exec, s[6:7]
        0x7d88002e, // v_cmp_gt_u32_e32 vcc, s46, v0
        0x8780046a, // s_and_b64 s[0:1], vcc, s[4:5]
        0xbe802400, // s_and_saveexec_b64 s[0:1], s[0:1]
        0xbf880002, // s_cbranch_execz end
        0xf80008cf, 0x33323107, // exp pos0 v7, v49, v50, v51 done: v7 from the 16-bit block
        0xbf810000, // s_endpgm
    ]
}

#[test]
fn sampler_heap_binding_uses_the_reaching_pointer() {
    use tuxbox_dxbc::mesh_table::MeshTableBinding;

    let words = [
        0xbe99_0380u32, // s_mov_b32 s25, 0
        0xbf82_0001, // s_branch past the write to s24
        0xbe98_0380, // s_mov_b32 s24, 0
        0xf408_050c, 0xfa00_0410, // s_load_dwordx4 s[20:23], s[24:25], 0x410
        0xf40c_0808, 0xfa00_0000, // s_load_dwordx8 s[32:39], s[16:17], 0
        0xf09c_0108, 0x00a8_0401, // image_sample_lz v4, v[1:2], s[32:39], s[20:23]
        0xbf81_0000,
    ];
    let emit = |words: [u32; 10]| -> Result<String, String> {
        let isa: Vec<_> = words.into_iter().flat_map(u32::to_le_bytes).collect();
        let ex = heap_extract(&[[6, 0, 1, 0, 16]], 0x800, &isa);
        let insns = disassemble(&isa);
        let mut em = Emitter::new(&ex, insns.clone());
        em.mesh_bindings = Some(super::mesh_bindings::MeshBindings {
            tables: [((16, 0), MeshTableBinding {
                parameter: 0, kind: 0, register: 1, space: 0, stride_words: None,
            })].into(),
            raw: Default::default(),
            constants: Default::default(),
            replacements: Default::default(),
        });
        em.resolve_resources()?;
        em.emit_insn(5, &insns[5], false, None)?;
        let mut hlsl = String::new();
        em.heap_sampler_locals(&mut hlsl);
        hlsl.push_str(&em.body);
        Ok(hlsl)
    };
    let hlsl = emit(words).unwrap();
    assert!(hlsl.contains("SamplerState samp65 = SamplerDescriptorHeap[65];"));
    assert!(hlsl.contains(".SampleLevel(samp65,"));

    let mut clobbered = words;
    clobbered[1] = 0xbf84_0001; // s_cbranch_scc0: the write is now reachable
    let reason = emit(clobbered).unwrap_err();
    assert!(reason.contains("base s24 is not a root table pointer"));
}

#[test]
fn validate_launch_ignores_readlane_sgpr_destination_matching_lane_vgpr() {
    let words = [
        0xd765_0002u32, 0x0001_00c1, // v_mbcnt_lo_u32_b32 v2, -1, 0
        0x7e00_0505,                 // v_readfirstlane_b32 s0, v5
        0x9381_ff03, 0x0004_0018,    // s_bfe_u32 s1, s3, 0x00040018
        0xd766_0002, 0x0002_04c1,    // v_mbcnt_hi_u32_b32 v2, -1, v2
        0x9016_8700,                 // s_lshr_b32 s22, s0, 7
        0xd746_0002, 0x0409_0c01,    // v_lshl_add_u32 v2, s1, 6, v2
        0xd760_0002, 0x0001_0100,    // v_readlane_b32 s2, v0, 0 (writes s2, not v2)
    ];
    let isa: Vec<u8> = words.into_iter().flat_map(u32::to_le_bytes).collect();
    let body = disassemble(&isa);
    super::validate_launch(&body, 7, Opnd::Vgpr(2)).expect("v_readlane_b32 writes SGPR, not lane_index VGPR");
}

#[test]
fn wave_cull_andn2_b64_sets_scc_from_lane_bit() {
    let words = [
        0x8a82_007eu32, // s_andn2_b64 s[2:3], exec, s[0:1]
        0xbf81_0000,    // s_endpgm
    ];
    let isa: Vec<u8> = words.into_iter().flat_map(u32::to_le_bytes).collect();
    let ex = heap_extract(&[], 0x800, &isa);
    let insns = disassemble(&isa);

    let mut cull = Emitter::new(&ex, insns.clone());
    cull.wave_cull = true;
    cull.hoist_wave_exchanges = true;
    cull.emit_insn(0, &insns[0], false, None).unwrap();
    assert!(
        cull.body.contains("scc = ((t0l & 1u) != 0u) ? 1u : 0u;"),
        "wave_cull s_andn2_b64 must test only bit 0 of the lane mask: {}",
        cull.body
    );

    let mut plain = Emitter::new(&ex, insns.clone());
    plain.wave_cull = false;
    plain.emit_insn(0, &insns[0], false, None).unwrap();
    assert!(
        plain.body.contains("scc = ((t0l | t0h) != 0u) ? 1u : 0u;"),
        "non-wave_cull s_andn2_b64 must keep full 64-bit scc test: {}",
        plain.body
    );
}
