//! Structured branches and loops, and the block dispatcher.
use super::*;
use crate::hlsl::coverage;

impl<'a> Emitter<'a> {
    /// Control flow.
    ///
    /// * Forward branches use skip flags: `jT` is set when a branch to instruction T is
    ///   taken, and every instruction before T that lies after such a branch is wrapped in
    ///   `if (!(jT || ...))`. This handles arbitrary forward-only CFGs (if/else, breaks).
    /// * A backward branch at B to header L becomes `do { ... } while (cond)` around
    ///   [L..=B]. Flags for labels inside the body are reset each iteration; flags for
    ///   labels past B (loop exits) terminate the loop and keep skipping until their label.
    ///   Loops must be properly nested, have one back-edge per header, and must not be
    ///   entered from outside except through the header — otherwise `Err`.
    pub(in crate::hlsl) fn emit_body(&mut self, is_ps: bool) -> Result<(), String> {
        self.emit_program(is_ps)?;
        coverage::record(&self.insns);
        Ok(())
    }

    fn emit_program(&mut self, is_ps: bool) -> Result<(), String> {
        let mut off2idx: HashMap<usize, usize> = self.insns.iter().enumerate().map(|(i, x)| (x.offset, i)).collect();
        // A branch to the address just past the last instruction leaves the program (same as
        // reaching the final s_endpgm).
        if let Some(l) = self.insns.last() {
            off2idx.insert(l.offset + l.dwords * 4, self.insns.len());
        }
        let prog_end = self.insns.last().map_or(0, |l| l.offset + l.dwords * 4);
        let mut target: HashMap<usize, usize> = HashMap::new(); // forward: branch idx -> target idx
        let mut back: HashMap<usize, usize> = HashMap::new(); // backward: branch idx B -> header idx L
        let n_insns = self.insns.len();
        for (i, ins) in self.insns.iter().enumerate() {
            if ins.enc == Enc::Sopp && ins.op == 1 && i + 1 < n_insns {
                target.insert(i, n_insns); // early s_endpgm -> end of program
                continue;
            }
            if ins.enc == Enc::Sopp && Self::branch_cond(ins.op).is_some() {
                let t = branch_pc(ins);
                let ti = if t >= 0 && t as usize >= prog_end {
                        self.insns.len() // at or beyond the end of the program (s_code_end padding): exit
                    } else {
                        *off2idx
                            .get(&(t as usize))
                            .ok_or_else(|| format!("branch at {:#x} targets mid-instruction {t:#x}", ins.offset))?
                    };
                if ti <= i {
                    back.insert(i, ti);
                } else {
                    target.insert(i, ti);
                }
            }
        }
        // Validate loop structure.
        // Several back-edges to one header = `continue`s. The last one (highest address)
        // closes the loop; each earlier one becomes a forward jump to that closing branch
        // which also forces another iteration (see the `tb_skip == B` at the loop bottom).
        let mut heads: HashMap<usize, usize> = HashMap::new(); // L -> B
        for (&b, &l) in &back {
            let e = heads.entry(l).or_insert(b);
            *e = (*e).max(b);
        }
        let continues: Vec<(usize, usize)> =
            back.iter().filter(|(&b, &l)| heads[&l] != b).map(|(&b, &l)| (b, heads[&l])).collect();
        for (b, closing) in continues {
            back.remove(&b);
            target.insert(b, closing);
        }
        // Sorted: `heads` is a HashMap, and the order of `loops` decides the order of the
        // emitted flag declarations. Iteration order would make the HLSL (and with it the
        // compiled-shader cache key) differ between runs.
        let mut loops: Vec<(usize, usize)> = heads.iter().map(|(&l, &b)| (l, b)).collect();
        loops.sort_unstable();
        for &(l1, b1) in &loops {
            for &(l2, b2) in &loops {
                let disjoint = b1 < l2 || b2 < l1;
                let nested = (l1 <= l2 && b2 <= b1) || (l2 <= l1 && b1 <= b2);
                if !(disjoint || nested) {
                    // Interleaved loops cannot be expressed as nested do/while: fall back
                    // to the general block dispatcher.
                    if self.insns.iter().any(is_quad_exchange) && !self.pixel_wave {
                        return Err("DPP quad exchange with interleaved loops".into());
                    }
                    return self.emit_body_dispatch(is_ps, 0);
                }
            }
            // A forward branch from before the header into the body (the usual rotated
            // `while`: jump straight to the condition) is handled by the skip flags: the
            // flag stays set on the first iteration, so everything before the label is
            // skipped, and in-body flags are cleared at the bottom of each iteration.
        }
        let labels: BTreeSet<usize> = target.values().copied().collect();
        // The earliest forward branch to each label: a label is pending at `i` when
        // some branch before `i` targets it, which this decides without rescanning
        // every branch for every instruction.
        let mut earliest: HashMap<usize, usize> = HashMap::new();
        for (&branch, &label) in &target {
            earliest.entry(label).and_modify(|first| *first = (*first).min(branch)).or_insert(branch);
        }
        // Loops that hold a group rendezvous iterate as a group (see `group_loops`).
        let grouped = if self.hoist_wave_exchanges || self.page_waves {
            group_loops::select(&loops, |i| self.group_exchange(i, &self.insns[i]).is_ok_and(|code| code.is_some())
                || wave_cull::guarded_vote(self, i, &self.insns[i], &[]).is_some())
        } else {
            BTreeSet::new()
        };
        if !labels.is_empty() || !grouped.is_empty() {
            // See `skip_to`.
            self.line("uint tb_skip = 0u;");
        }
        if !loops.is_empty() {
            self.line("uint tb_guard = 0u;");
            for &(_, b) in &loops {
                self.line(&format!("bool tb_c{b} = false;"));
            }
            for &b in &grouped {
                group_loops::declare(self, b);
            }
        }
        let mut group_depth = 0usize;

        let insns = self.insns.clone();
        let view_cull = self.view_cull;
        let probe_trace = insns.iter().any(|op| matches!(op.mnemonic.as_str(),"tb_radiance_query" | "tb_reflection_far_query" | super::super::scarlett_rays::MNEMONIC));
        if view_cull.is_some() { self.line("uint cull_view = 0u; bool cull_candidate = false; uint3 cull_record = 0;"); }
        if insns.iter().any(|op| op.mnemonic == "tb_view_occluded") {
            self.line("bool hidden = false; uint hidden_id = 0u, hidden_view = 0u;");
        }
        let group_size = compute_group(self.ex).into_iter().fold(1u32, u32::saturating_mul);
        let lds_waits = if self.is_cs && (2..=64).contains(&group_size) {
            lds_order::exchange_waits(&insns, &loops)
        } else {
            BTreeSet::new()
        };
        let restored_waits = if self.is_cs && group_size == 64 {
            lds_order::restored_write_waits(&insns, &loops)
        } else {
            BTreeSet::new()
        };
        let mut open = false;
        let mut cur: Vec<usize> = vec![];
        let mut prior_lds_write = None;
        for (i, ins) in insns.iter().enumerate() {
            // One guest wave exchanges halfwords through LDS in instruction order.
            // Host invocations must finish reading before another lane overwrites
            // the tile, and finish publishing writes before other lanes read.
            // Keep the rendezvous outside the translated guest branch guard.
            if self.single_wave32 && ins.enc == Enc::Ds {
                let write = ins.mnemonic.starts_with("ds_write");
                if prior_lds_write.replace(write).is_some_and(|was_write| was_write != write) {
                    if !back.is_empty() {
                        return Err("single-wave LDS exchange inside a loop".into());
                    }
                    self.end_guard(&mut open); cur.clear();
                    self.line("GroupMemoryBarrierWithGroupSync();");
                }
            }
            // Reconverge both host subgroups after each guest view, including
            // invocations whose guest EXEC mask branched over the queue append.
            if let Some(program) = view_cull.filter(|p| p.join == ins.offset) {
                self.end_guard(&mut open);
                cur.clear();
                program.append(self);
            }
            if let Some(&b) = heads.get(&i) {
                self.end_guard(&mut open);
                cur.clear();
                if grouped.contains(&b) {
                    group_loops::enter(self, b);
                    group_depth += 1;
                }
                self.line("do {");
                // The back edge can store the index after a use. Bind again inside the loop.
                self.drop_heap_handles();
                self.loop_depth += 1;
                self.in_accepted_swizzle_loop_stack.push(self.accepted_swizzle_loops.contains(&b));
                self.line(&format!("tb_c{b} = false;"));
            }
            let mut active: Vec<usize> =
                labels.iter().copied().filter(|&t| t > i && earliest.get(&t).is_some_and(|&b| b < i)).collect();
            if group_depth != 0 {
                // A thread done with an enclosing group loop skips all of its body.
                active.push(group_loops::SKIP_ALL);
            }
            if let Some(code) = self.group_exchange(i, ins)? {
                self.end_guard(&mut open);
                cur.clear();
                self.line(&code);
            }
            if let Some(code) = wave_cull::guarded_vote(self, i, ins, &active) {
                // Every invocation reaches the barriers; the guard decides its vote.
                self.end_guard(&mut open);
                cur.clear();
                self.line(&code);
                self.hoisted_votes.insert(i);
            }
            let mesh_condition = self.hoist_wave_exchanges && self.ex.stage == Stage::Mesh
                && reads_scc(ins) && !self.in_loop(i) && scc_from_mask(&insns, i);
            if mesh_condition {
                // Mask operations set SCC if any bit in the guest wave is
                // set. Vote outside the forward branch guards so that every
                // workgroup invocation reaches the barriers.
                self.end_guard(&mut open);
                cur.clear();
                self.line(&format!("uint2 mesh_scc{i} = tb_page_ballot(tb_wave_tid, scc != 0u);"));
            }
            // Exited guest waves no longer count towards S_BARRIER. Their host
            // threads still belong to the group and must participate in the HLSL
            // barrier. Only lift the barrier if every enclosing skip exits the
            // program and no loop could change how many times a wave reaches it.
            if self.is_cs && ins.mnemonic == "s_barrier" && loops.is_empty()
                && active.iter().all(|&t| t == insns.len() || (t + 1 == insns.len() && insns[t].mnemonic == "s_endpgm"))
            {
                active.clear();
            }
            if self.compute_swizzle && exchanges(ins) && !active.is_empty() {
                self.end_guard(&mut open);
                cur.clear();
                self.emit_compute_swizzle(i, ins, &active)?;
                self.hoisted_swizzle = Some(i);
            }
            if ins.mnemonic == "tb_first_exec_lane" && !self.exec_snapshots.sites.contains_key(&i) {
                self.end_guard(&mut open);
                cur.clear();
                let mut condition=String::from("(exec_lo & 1u) != 0u");
                for &t in &active { write!(condition, " && {}", not_skipping(t)).unwrap(); }
                self.line(&format!("uint election{i}base = tb_lane & ~63u;"));
                self.line(&format!("if ((tb_lane & 63u)==0) tb_swizzle[election{i}base]=0xffffffffu;"));
                self.line("GroupMemoryBarrierWithGroupSync();");
                self.line(&format!("if ({condition}) InterlockedMin(tb_swizzle[election{i}base],tb_lane & 63u);"));
                self.line("GroupMemoryBarrierWithGroupSync();");
                self.line(&format!("uint election{i}=tb_swizzle[election{i}base];"));
                self.line("GroupMemoryBarrierWithGroupSync();");
            }
            if ins.mnemonic == "tb_indexed_readlane" && !active.is_empty() {
                self.end_guard(&mut open);
                cur.clear();
                self.emit_indexed_read(i, ins, &active)?;
                self.hoisted_swizzle = Some(i);
            }
            if self.micropoly_vote == Some(i) {
                // Retired triangles contribute zero and still participate in both
                // barriers. The count precedes either rasterization loop.
                self.end_guard(&mut open);
                micropoly::emit_vote(self, ins, &active)?;
                return Ok(());
            }
            if is_quad_exchange(ins) {
                if self.loop_depth != 0 && (!self.pixel_wave || is_row_move(ins)) {
                    return Err("DPP16 exchange inside backward control flow".into());
                }
                if open {
                    self.end_guard(&mut open);
                    cur.clear();
                }
                if self.pixel_wave && !is_row_move(ins) {
                    self.emit_pixel_quad_exchange(i, ins)?;
                } else if self.straight_pixel_quad.is_some() {
                    // Prefix analysis proves converged FI=1 axis moves with full row/bank masks.
                    let source = r(&ins.src[0])?;
                    let axis = if ins.aux.imm == 0xb1 { "X" } else { "Y" };
                    self.line(&format!("uint q{i} = QuadReadAcross{axis}({source});"));
                } else {
                    let source = r(&ins.src[0])?;
                    if ins.op == OP_DPP_BINARY {
                        // All host lanes participate before the guest destination guards.
                        let perm = ins.aux.imm;
                        self.line(&format!("uint q{i}lane = WaveGetLaneIndex();"));
                        let dpp8 = group_extrema::is_dpp8(ins);
                        let lane = if dpp8 {
                            format!("(q{i}lane & ~7u) | (({perm}u >> (3u * (q{i}lane & 7u))) & 7u)")
                        } else if perm == 0x140 {
                            format!("q{i}lane ^ 15u")
                        } else if (0x121..=0x12f).contains(&perm) {
                            format!("(q{i}lane & ~15u) | ((q{i}lane - {}u) & 15u)", perm & 15)
                        } else { format!("(q{i}lane & ~3u) | (({perm}u >> (2u * (q{i}lane & 3u))) & 3u)") };
                        if dpp8 || ins.raw[1] & (1 << 18) != 0 {
                            self.line(&format!("uint q{i} = WaveReadLaneAt({source}, {lane});"));
                        } else if ins.raw[1] & (1 << 19) != 0 {
                            self.line(&format!("uint q{i} = WaveReadLaneAt((exec_lo & 1u) != 0u ? {source} : 0u, {lane});"));
                        } else {
                            self.line(&format!("uint2 q{i}pair = WaveReadLaneAt(uint2({source}, exec_lo & 1u), {lane});"));
                            self.line(&format!("uint q{i} = q{i}pair.x;"));
                            self.line(&format!("bool q{i}active = q{i}pair.y != 0u;"));
                        }
                    } else {
                        let selector = ins.aux.imm;
                        let lane = match ins.op {
                            OP_QUAD_MOVE => {
                                self.line(&format!("uint q{i}lane = WaveGetLaneIndex();"));
                                format!("(q{i}lane & ~3u) | (({selector}u >> (2u * (q{i}lane & 3u))) & 3u)")
                            }
                            OP_ROW_SHARE_MOVE => format!("(WaveGetLaneIndex() & ~15u) | {selector}u"),
                            OP_ROW_ROR_MOVE => format!("(WaveGetLaneIndex() & ~15u) | ((WaveGetLaneIndex() - {selector}u) & 15u)"),
                            OP_ROW_SHIFT_MOVE => {
                                let sign = if selector < 0 { "-" } else { "+" };
                                self.line(&format!("uint q{i}offset = (WaveGetLaneIndex() & 15u) {sign} {}u;", selector.abs()));
                                // Keep the wave read in range even for lanes that will discard its value.
                                format!("(WaveGetLaneIndex() & ~15u) | (q{i}offset & 15u)")
                            }
                            _ => format!("WaveGetLaneIndex() ^ {selector}u"),
                        };
                        let fetch_inactive = ins.raw[1] & (1 << 18) != 0;
                        let bound_ctrl = ins.raw[1] & (1 << 19) != 0;
                        let lane = if is_row_move(ins) || !fetch_inactive {
                            self.line(&format!("uint q{i}src = {lane};"));
                            format!("q{i}src")
                        } else { lane };
                        if fetch_inactive {
                            self.line(&format!("uint q{i} = WaveReadLaneAt({source}, {lane});"));
                        } else if bound_ctrl {
                            self.line(&format!("uint q{i} = WaveReadLaneAt((exec_lo & 1u) != 0u ? {source} : 0u, {lane});"));
                        } else {
                            // An inactive source suppresses the write to this lane's destination.
                            let dst = r(&ins.dst[0])?;
                            self.line(&format!("uint2 q{i}pair = WaveReadLaneAt(uint2({source}, exec_lo & 1u), {lane});"));
                            self.line(&format!("uint q{i} = q{i}pair.y != 0u ? q{i}pair.x : {dst};"));
                        }
                        if ins.op == OP_ROW_SHIFT_MOVE {
                            let fallback = if bound_ctrl { "0u".to_string() } else { r(&ins.dst[0])? };
                            self.line(&format!("if (q{i}offset >= 16u) q{i} = {fallback};"));
                        }
                    }
                }
            }
            if restored_waits.contains(&i) {
                self.end_guard(&mut open);
                cur.clear();
                self.line("GroupMemoryBarrierWithGroupSync();");
            }
            if active != cur || (open && heads.contains_key(&i)) {
                self.end_guard(&mut open);
                // Every pending target lies past the nearest one, so one comparison
                // decides for all of them.
                if let Some(&nearest) = active.iter().min() {
                    self.line(&format!("if {} {{", not_skipping(nearest)));
                    open = true;
                }
                cur = active;
            }
            for o in ins.src.iter().chain(ins.dst.iter()) {
                self.touch(o);
            }
            if mesh_condition {
                self.line(&format!("scc = uint(any(mesh_scc{i}));"));
            }
            if lds_waits.contains(&i) && !restored_waits.contains(&i) {
                self.line("GroupMemoryBarrierWithGroupSync();");
            }
            if self.packed.as_ref().is_some_and(|p| p.device_sync_before.contains(&i)) {
                // Publish every coarse luminance texel before this group takes its ticket.
                self.line("AllMemoryBarrierWithGroupSync();");
            }
            self.emit_insn(i, ins, is_ps, target.get(&i).copied())
                .map_err(|e| format!("{e} [{} @ {:#x}]", ins.mnemonic, ins.offset))?;
            if open && target.get(&i).is_some_and(|t| cur.contains(t)) {
                // Another branch to an enclosing target just updated its flag.
                // The following instruction must reevaluate the guard.
                self.end_guard(&mut open);
                cur.clear();
            }
            if back.contains_key(&i) {
                let head = back[&i];
                // A loop whose body holds any group rendezvous must keep every lane
                // together until the last one is done: a per-thread exit that leaves
                // the loop early makes the lane stop hitting the barriers the other
                // lanes wait on. Only the loops the pipeline proved safe for a shared
                // exchange get the whole-wave vote: other loops keep their per-thread
                // condition, exactly as they emitted before this feature existed.
                let vote = self.accepted_swizzle_loops.contains(&i) && self.insns[head..=i].iter().enumerate().any(|(j, ins2)| {
                    let idx = head + j;
                    exchanges(ins2)
                        || ins2.mnemonic == "s_barrier"
                        || ins2.mnemonic == "tb_first_exec_lane"
                        || ins2.mnemonic == "tb_indexed_readlane"
                        || (self.single_wave32 && ins2.enc == Enc::Ds)
                        || lds_waits.contains(&idx)
                        || restored_waits.contains(&idx)
                });
                if vote {
                    // Evaluate the back-edge condition unconditionally: the while
                    // condition below votes over it, which must not be guarded.
                    self.end_guard(&mut open);
                    cur.clear();
                }
                // Evaluate the back-edge condition inside the guard (a skipped branch is not taken).
                // Where the whole wave decides branches on EXEC and VCC, it decides this one
                // too. A lane outside EXEC enters the loop with its wave: on its own bit it
                // would stay until the iteration cap, and keep the wave there with it.
                let cond = mask_ballots::branch(self, i, ins.op).unwrap_or_else(|| {
                    wave_cull::branch_condition(self, ins, Self::branch_cond(ins.op).unwrap_or("false"))
                });
                self.line(&format!("tb_c{i} = ({cond});"));
                if labels.contains(&i) {
                    // A `continue` jumped here: iterate again regardless of the condition.
                    self.line(&format!("tb_c{i} = tb_c{i} || (tb_skip == {i}u);"));
                }
                self.end_guard(&mut open);
                cur.clear();
                // Forget targets inside this loop body before the next iteration. A
                // target past the latch stays pending and leaves the loop.
                if labels.range(head + 1..=i).next().is_some() {
                    self.line(&format!("tb_skip *= uint(tb_skip > {i}u);"));
                }
                let exits = labels.range(i + 1..).next().is_some();
                // A mesh loop without a rendezvous may leave early by itself:
                // the group loops keep their own count.
                let guard = if (self.wave_cull && self.ex.stage != Stage::Mesh) || self.light_grid || probe_trace
                    || self.wave_bounded_latches.contains(&i)
                    || self.packed.as_ref().is_some_and(|p| p.copy_backedges.contains(&i) || p.bounded_backedges.contains(&i))
                {
                    ""
                } else {
                    " && ++tb_guard < 65536u"
                };
                if view_cull.is_some_and(|p| p.back == ins.offset) {
                    // The guest iterates cb0[12] views. Retired lanes must
                    // participate in the barriers for each remaining view.
                    self.line("} while (++cull_view < cb0[3].x);");
                } else if grouped.contains(&i) {
                    let exit = if exits { format!(" && tb_skip <= {i}u") } else { String::new() };
                    group_loops::close(self, i, &format!("tb_c{i}{exit}"));
                    group_depth -= 1;
                } else if vote {
                    // Keep the whole wave in the loop while any lane still needs to
                    // run; finished lanes stay in the loop, masked, and still hit the barriers.
                    let exit = if exits { format!(" && tb_skip <= {i}u") } else { String::new() };
                    self.line(&format!("}} while (WaveActiveAnyTrue(tb_c{i}{exit}){guard});"));
                } else {
                    let exit = if exits { format!(" && tb_skip <= {i}u") } else { String::new() };
                    self.line(&format!("}} while (tb_c{i}{exit}{guard});"));
                }
                self.loop_depth = self.loop_depth.saturating_sub(1);
                self.in_accepted_swizzle_loop_stack.pop();
                self.drop_heap_handles();
            }
        }
        if open {
            self.end_guard(&mut open);
        }
        Ok(())
    }

    /// The rendezvous emitted at `i` outside every guest branch guard.
    fn group_exchange(&self, i: usize, ins: &Insn) -> Result<Option<String>, String> {
        if self.page_waves {
            return page_waves::rendezvous(i, ins);
        }
        if !self.hoist_wave_exchanges {
            return Ok(None);
        }
        let local = self.own_lane_reads.contains(&i)
            || (matches!(ins.mnemonic.as_str(), "s_cbranch_execz" | "s_cbranch_vccz" | "s_cbranch_vccnz") && self.in_loop(i));
        if local { Ok(None) } else { wave_cull::exchange(i, ins, self.ex.stage == Stage::Mesh) }
    }

    /// Follow guest basic blocks from `entry`. Each block sets the next block id
    /// (`tb_pc`). The id one past the last instruction terminates the program.
    /// (Implicit-derivative sampling cannot live inside this loop — FXC rejects it, and the
    /// shader then fails to compile rather than silently changing meaning.)
    pub(in crate::hlsl) fn emit_pixel_quad_exchange(&mut self, i: usize, ins: &Insn) -> Result<(), String> {
        let mut source = r(&ins.src[0])?;
        let mut ty = "uint";
        let ctrl = ins.raw[1];
        let check_exec = ctrl & (1 << 18) == 0;
        let preserve = check_exec && ctrl & (1 << 19) == 0;
        if check_exec {
            // Pixel entry initializes the per-invocation EXEC bit even on helper
            // lanes. Only trust it after an explicit full-wave reset; CMPX may
            // then disable individual lanes, including covered pixels.
            let prefix = &self.insns[..i];
            let mut preceding = prefix.iter().rev();
            let mut reset = preceding.next();
            if reset.is_some_and(|op| op.mnemonic.starts_with("v_cmpx_")) {
                reset = preceding.next();
            }
            let full_exec = reset.is_some_and(|op| op.mnemonic == "s_mov_b64"
                && op.dst == [Opnd::ExecLo] && op.src == [Opnd::ConstInt(-1)]);
            if !full_exec || self.dispatched || self.insns.iter().any(|op| op.enc == Enc::Sopp && Self::branch_cond(op.op).is_some()) {
                return Err(format!("pixel DPP16 FI=0 at {:#x} requires a proven source EXEC mask", ins.offset));
            }
            if preserve {
                ty = "uint2";
                source = format!("uint2({source}, exec_lo & 1u)");
            } else {
                source = format!("((exec_lo & 1u) != 0u ? {source} : 0u)");
            }
        }
        let perm = ins.aux.imm;
        // Quad operations exchange values with helper lanes. Lane bits are x, y.
        self.line(&format!("uint q{i}lane = WaveGetLaneIndex() & 3u, q{i}src = (({perm}u >> (2u * q{i}lane)) & 3u) ^ q{i}lane;"));
        self.line(&format!("{ty} q{i}x = QuadReadAcrossX({source}), q{i}y = QuadReadAcrossY({source}), q{i}d = QuadReadAcrossDiagonal({source});"));
        let selected = if preserve { format!("q{i}pair") } else { format!("q{i}") };
        self.line(&format!("{ty} {selected} = q{i}src == 0u ? {source} : q{i}src == 1u ? q{i}x : q{i}src == 2u ? q{i}y : q{i}d;"));
        if preserve {
            if ins.op == OP_QUAD_MOVE {
                let dst = r(&ins.dst[0])?;
                self.line(&format!("uint q{i} = q{i}pair.y != 0u ? q{i}pair.x : {dst};"));
            } else {
                self.line(&format!("uint q{i} = q{i}pair.x; bool q{i}active = q{i}pair.y != 0u;"));
            }
        }
        Ok(())
    }

    pub(in crate::hlsl) fn emit_body_dispatch(&mut self, is_ps: bool, entry: usize) -> Result<(), String> {
        // Per-invocation dispatch cannot guarantee whole-wave arrival at a vote.
        if !self.reduction_execz_guards.is_empty() {
            return Err(lockstep::DISPATCHED_VOTE.into());
        }
        if !self.wave_bounded_latches.is_empty() {
            return Err("bounded wave reduction in a block-dispatched program".into());
        }
        self.dispatched = true;
        let n = self.insns.len();
        let mut off2idx: HashMap<usize, usize> = self.insns.iter().enumerate().map(|(i, x)| (x.offset, i)).collect();
        if let Some(l) = self.insns.last() {
            off2idx.insert(l.offset + l.dwords * 4, n);
        }
        let prog_end = self.insns.last().map_or(0, |l| l.offset + l.dwords * 4);
        let mut leaders = BTreeSet::from([entry]);
        let mut tgt: HashMap<usize, usize> = HashMap::new();
        for (i, ins) in self.insns.iter().enumerate().skip(entry) {
            if ins.enc != Enc::Sopp {
                continue;
            }
            if Self::branch_cond(ins.op).is_some() {
                let t = branch_pc(ins);
                let ti = if t >= 0 && t as usize >= prog_end {
                        self.insns.len() // at or beyond the end of the program (s_code_end padding): exit
                    } else {
                        *off2idx
                            .get(&(t as usize))
                            .ok_or_else(|| format!("branch at {:#x} targets mid-instruction {t:#x}", ins.offset))?
                    };
                tgt.insert(i, ti);
                leaders.insert(ti);
                leaders.insert(i + 1);
            } else if ins.op == 1 {
                leaders.insert(i + 1);
            }
        }
        // A guest wave reaches S_BARRIER as a whole: lanes that left a loop early wait in
        // EXEC for the rest. Here each lane follows its own blocks, so the barrier starts
        // one that a lane enters only when its wave has caught up.
        let barriers: Vec<usize> = (entry..n)
            .filter(|&i| self.is_cs && self.insns[i].enc == Enc::Sopp && self.insns[i].op == 0x0a)
            .collect();
        leaders.extend(&barriers);
        let starts: Vec<usize> = leaders.into_iter().filter(|&s| s < n).collect();
        self.loop_depth = 0;
        if entry == 0 {
            self.line("uint tb_pc = 0u; uint tb_guard = 0u;");
            self.line(&format!("[loop] while (tb_pc != {n}u && tb_guard++ < 1000000u) {{"));
        } else {
            self.line(&format!("uint tb_pc = {entry}u; [loop] while (tb_pc != {n}u) {{"));
        }
        self.loop_depth += 1;
        // Blocks are tested in program order on the live block id, so one pass over them
        // follows every forward jump and only a jump back needs another. Many blocks are
        // nested by range, which skips most tests. (FXC mishandles a `switch` whose selector
        // is assigned inside its cases.)
        const RANGE: usize = 24;
        let spans: &[usize] = match starts.len() {
            0..=RANGE => &[],
            blocks if blocks <= RANGE * RANGE => &[RANGE],
            _ => &[RANGE * RANGE, RANGE],
        };
        if !barriers.is_empty() {
            let at = barriers.iter().map(|i| format!("tb_pc == {i}u")).collect::<Vec<_>>().join(" || ");
            self.line(&format!("bool tb_wait = {at};"));
            // When lanes stand at different barriers the earliest goes first.
            self.line("bool tb_busy = WaveActiveAnyTrue(!tb_wait); uint tb_low = WaveActiveMin(tb_pc);");
            self.line("tb_wait = tb_wait && (tb_busy || tb_pc != tb_low);");
            // The barrier a lane may enter in this pass. One it reaches later waits for the next.
            self.line("uint tb_sel = tb_wait ? 0xfffffffeu : tb_pc;");
        }
        let insns = self.insns.clone();
        for (b, &start) in starts.iter().enumerate() {
            let end = starts.get(b + 1).copied().unwrap_or(n);
            for &span in spans {
                if b % span == 0 {
                    let to = starts.get(b + span).copied().unwrap_or(n);
                    self.line(&format!("if (tb_pc - {start}u < {}u) {{", to - start));
                }
            }
            let entered = if barriers.contains(&start) { format!(" && tb_sel == {start}u") } else { String::new() };
            self.line(&format!("if (tb_pc == {start}u{entered}) {{"));
            self.drop_heap_handles();
            for i in start..end {
                let ins = &insns[i];
                for o in ins.src.iter().chain(ins.dst.iter()) {
                    self.touch(o);
                }
                if is_quad_exchange(ins) && self.pixel_wave && !is_row_move(ins) {
                    self.emit_pixel_quad_exchange(i, ins)?;
                }
                let is_ctl = ins.enc == Enc::Sopp && (Self::branch_cond(ins.op).is_some() || ins.op == 1);
                if !is_ctl || ins.mnemonic == "s_swappc_b64" {
                    self.emit_insn(i, ins, is_ps, None)
                        .map_err(|e| format!("{e} [{} @ {:#x}]", ins.mnemonic, ins.offset))?;
                }
            }
            let last = &insns[end - 1];
            let next = if last.enc == Enc::Sopp && last.op == 1 {
                format!("{n}u")
            } else if let (Some(c), Some(&t)) = (
                if last.enc == Enc::Sopp { Self::branch_cond(last.op) } else { None },
                tgt.get(&(end - 1)),
            ) {
                if self.wave_cull { wave_cull::prepare_insn(self, end - 1, last)?; }
                let whole_wave = mask_ballots::branch(self, end - 1, last.op);
                let c = wave_cull::branch_condition(self, last, whole_wave.as_deref().unwrap_or(c));
                if last.op == 2 { format!("{t}u") } else { format!("({c}) ? {t}u : {end}u") }
            } else {
                format!("{end}u")
            };
            self.line(&format!("tb_pc = {next}; }}"));
            for &span in spans.iter().rev() {
                if b % span == span - 1 || b + 1 == starts.len() { self.line("}"); }
            }
        }
        self.line("}");
        self.drop_heap_handles();
        self.loop_depth = self.loop_depth.saturating_sub(1);
        Ok(())
    }

    /// Take a forward branch to the instruction at `target` when `condition` holds.
    ///
    /// `tb_skip` is the farthest target this invocation has branched to, and it
    /// skips everything before that instruction. One variable keeps large programs
    /// from carrying a flag per target through every loop and join, which costs DXC
    /// much of a large compile. Compute programs update it without a branch, which
    /// keeps the control flow of an unoptimised compile small.
    pub(in crate::hlsl) fn skip_to(&mut self, target: usize, condition: Option<&str>) {
        let statement = match condition {
            None => format!("tb_skip = max(tb_skip, {target}u);"),
            Some(c) if self.ex.stage == Stage::Compute => format!("tb_skip = max(tb_skip, uint({c}) * {target}u);"),
            Some(c) => format!("if ({c}) tb_skip = max(tb_skip, {target}u);"),
        };
        self.line(&statement);
    }
}

/// Whether this invocation runs the instructions before `target`, as far as the
/// forward branches to it are concerned.
pub(in crate::hlsl) fn not_skipping(target: usize) -> String {
    format!("(tb_skip < {target}u)")
}
