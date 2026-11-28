#!/usr/bin/env python3
"""Check on a real GPU that lane masks read as numbers hold the whole wave's lanes.

Each synthetic program counts a mask's lanes, finds its first one or counts
those before each lane, some of them after EXEC was narrowed to one elected
lane. The reference models the console. Each program runs in waves of its own
width, where the host has them, and each 64-lane program also runs in the form a
32-lane host uses, with two console lanes per thread. Only the translator's input
and output are instrumented. No game data is needed.
"""
import argparse
import json
from pathlib import Path
import re
import struct
import subprocess
import sys
from tempfile import TemporaryDirectory

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "tools"))
from check_lockstep_gpu import LIMIT, branch, label, words_of  # noqa: E402
from check_wave_guard import cases  # noqa: E402
from frosty_gpu_guest import run_gpu_guest  # noqa: E402
from project_tuxbox.dxil_store import COMPUTE_FLAGS  # noqa: E402
from project_tuxbox.inspect import digest  # noqa: E402

# Results go to v10, v11 and v12. s12 holds the entry EXEC and v14 the lane's index.
WAVE_OPTIONS = {32: (), 64: ("-mattr=-wavefrontsize32,+wavefrontsize64",)}
# A running sum along each row of sixteen lanes.
ROW_SCAN = ["v_add_nc_u32_dpp v5, v5, v5 row_shr:1 row_mask:0xf bank_mask:0xf bound_ctrl:1",
            *(f"v_add_nc_u32_dpp v5, v5, v5 row_shr:{n} row_mask:0xf bank_mask:0xf" for n in (2, 4, 8))]
WAVE32 = {
    # A compare's own mask.
    "compare": ["v_cmp_lt_u32_e32 vcc_lo, 0x3e8, v0", "s_bcnt1_i32_b32 s20, vcc_lo", "v_mov_b32_e32 v10, s20",
                "v_mbcnt_lo_u32_b32 v11, vcc_lo, 0", "s_ff1_i32_b32 s21, vcc_lo", "v_mov_b32_e32 v12, s21"],
    # A copy of EXEC, read after one lane of it was elected.
    "elected": ["v_mbcnt_lo_u32_b32 v14, -1, 0", "v_cmp_lt_u32_e32 vcc_lo, 0x3e8, v0",
                "s_and_saveexec_b32 s10, vcc_lo", branch("s_cbranch_execz", "end"),
                "s_mov_b32 s11, exec_lo", "v_mbcnt_lo_u32_b32 v11, s11, 0",
                "s_ff1_i32_b32 s21, s11", "v_mov_b32_e32 v12, s21",
                "v_cmpx_eq_u32_e32 s21, v14", "s_bcnt1_i32_b32 s20, s11", "v_mov_b32_e32 v10, s20",
                label("end")],
    # A mask gathered over a loop.
    "gathered": ["s_mov_b32 s11, 0", "s_mov_b32 s22, 0", label("again"),
                 "v_and_b32_e32 v15, 3, v0", "v_cmp_eq_u32_e32 vcc_lo, s22, v15", "s_or_b32 s11, s11, vcc_lo",
                 "s_add_u32 s22, s22, 1", "s_cmp_lt_u32 s22, 3", branch("s_cbranch_scc1", "again"),
                 "s_bcnt1_i32_b32 s20, s11", "v_mov_b32_e32 v10, s20", "v_mbcnt_lo_u32_b32 v11, s11, 0",
                 "v_mov_b32_e32 v12, 0"],
    # The lanes left out, and a branch on their number.
    "others": ["v_cmp_lt_u32_e32 vcc_lo, 0x3e8, v0", "s_andn2_b32 s11, exec_lo, vcc_lo",
               "s_bcnt1_i32_b32 s20, s11", branch("s_cbranch_scc0", "end"), "v_mov_b32_e32 v10, s20",
               "v_mbcnt_lo_u32_b32 v11, s11, 0", "v_mov_b32_e32 v12, 1", label("end")],
    # A loop that ends on VCC, entered by a wave that left lanes outside EXEC, then a loop
    # every lane runs. A lane that stayed in the first loop has no iterations left for it.
    "latch": ["v_cmp_lt_u32_e32 vcc_lo, 0x3e8, v0", "s_bcnt1_i32_b32 s20, vcc_lo",
              "s_and_saveexec_b32 s10, vcc_lo", branch("s_cbranch_execz", "out"),
              label("again"), "v_add_nc_u32_e32 v11, 1, v11", "v_cmp_lt_u32_e32 vcc_lo, 2, v11",
              branch("s_cbranch_vccz", "again"),
              label("out"), "s_mov_b32 exec_lo, s10", "s_mov_b32 s22, 0",
              label("more"), "v_add_nc_u32_e32 v10, 1, v10", "s_add_u32 s22, s22, 1", "s_cmp_lt_u32 s22, 3",
              branch("s_cbranch_scc1", "more"), "v_mov_b32_e32 v12, s20"],
}
WAVE64 = {
    "compare": ["v_cmp_lt_u32_e32 vcc, 0x3e8, v0", "s_bcnt1_i32_b64 s20, vcc", "v_mov_b32_e32 v10, s20",
                "v_mbcnt_lo_u32_b32 v11, vcc_lo, 0", "v_mbcnt_hi_u32_b32 v11, vcc_hi, v11",
                "s_ff1_i32_b64 s21, vcc", "v_mov_b32_e32 v12, s21"],
    "elected": ["v_mbcnt_lo_u32_b32 v14, -1, 0", "v_mbcnt_hi_u32_b32 v14, -1, v14",
                "v_cmp_lt_u32_e32 vcc, 0x3e8, v0", "s_and_saveexec_b64 s[10:11], vcc", branch("s_cbranch_execz", "end"),
                "s_mov_b64 s[14:15], exec", "v_mbcnt_lo_u32_b32 v11, s14, 0", "v_mbcnt_hi_u32_b32 v11, s15, v11",
                "s_ff1_i32_b64 s21, s[14:15]", "v_mov_b32_e32 v12, s21",
                "v_cmpx_eq_u32_e32 s21, v14", "s_bcnt1_i32_b64 s20, s[14:15]", "v_mov_b32_e32 v10, s20",
                label("end")],
    "gathered": ["s_mov_b64 s[14:15], 0", "s_mov_b32 s22, 0", label("again"),
                 "v_and_b32_e32 v15, 3, v0", "v_cmp_eq_u32_e32 vcc, s22, v15", "s_or_b64 s[14:15], s[14:15], vcc",
                 "s_add_u32 s22, s22, 1", "s_cmp_lt_u32 s22, 3", branch("s_cbranch_scc1", "again"),
                 "s_bcnt1_i32_b64 s20, s[14:15]", "v_mov_b32_e32 v10, s20",
                 "v_mbcnt_lo_u32_b32 v11, s14, 0", "v_mbcnt_hi_u32_b32 v11, s15, v11", "v_mov_b32_e32 v12, 0"],
    "others": ["v_cmp_lt_u32_e32 vcc, 0x3e8, v0", "s_andn2_b64 s[14:15], exec, vcc",
               "s_bcnt1_i32_b64 s20, s[14:15]", branch("s_cbranch_scc0", "end"), "v_mov_b32_e32 v10, s20",
               "v_mbcnt_lo_u32_b32 v11, s14, 0", "v_mbcnt_hi_u32_b32 v11, s15, v11", "v_mov_b32_e32 v12, 1",
               label("end")],
    "latch": ["v_cmp_lt_u32_e32 vcc, 0x3e8, v0", "s_bcnt1_i32_b64 s20, vcc",
              "s_and_saveexec_b64 s[10:11], vcc", branch("s_cbranch_execz", "out"),
              label("again"), "v_add_nc_u32_e32 v11, 1, v11", "v_cmp_lt_u32_e32 vcc, 2, v11",
              branch("s_cbranch_vccz", "again"),
              label("out"), "s_mov_b64 exec, s[10:11]", "s_mov_b32 s22, 0",
              label("more"), "v_add_nc_u32_e32 v10, 1, v10", "s_add_u32 s22, s22, 1", "s_cmp_lt_u32 s22, 3",
              branch("s_cbranch_scc1", "more"), "v_mov_b32_e32 v12, s20"],
    # The exclusive prefix sum of the active lanes' values, taken twice in a loop. The lower
    # half's total goes round through lane 31.
    "scan": ["v_cmp_lt_u32_e32 vcc, 0x3e8, v0", "s_bcnt1_i32_b64 s20, vcc", "v_mov_b32_e32 v10, s20",
             "v_mov_b32_e32 v11, 0", "s_mov_b32 s22, 0", label("again"), "s_or_saveexec_b64 s[14:15], -1",
             "v_cndmask_b32_e64 v4, 0, v0, s[14:15]", "v_mov_b32_e32 v5, v4", *ROW_SCAN,
             "s_mov_b32 exec_lo, 0xffff0000", "s_mov_b32 exec_hi, 0xffff0000",
             "v_permlanex16_b32 v6, v5, -1, -1 op_sel:[1,0]", "v_add_nc_u32_e32 v5, v6, v5",
             "v_readlane_b32 s16, v5, 31", "s_mov_b32 exec_lo, 0", "s_mov_b32 exec_hi, -1",
             "v_add_nc_u32_e32 v5, s16, v5", "s_mov_b32 exec_lo, -1", "s_mov_b32 exec_hi, -1",
             "v_sub_nc_u32_e32 v5, v5, v4", "s_mov_b64 exec, s[14:15]", "v_add_nc_u32_e32 v12, v5, v12",
             "s_add_u32 s22, s22, 1", "s_cmp_lt_u32 s22, 2", branch("s_cbranch_scc1", "again")],
}


def program(body, width):
    size = "64" if width == 64 else "32"
    exec_, saved = ("exec", "s[12:13]") if width == 64 else ("exec_lo", "s12")
    return [f"s_mov_b{size} {saved}, {exec_}", *body, f"s_mov_b{size} {exec_}, {saved}", "s_endpgm"]


def model(name, active, values, host):
    """v10, v11 and v12 of every lane, with the group run as waves of `host` lanes."""
    rows = []
    for start in range(0, len(values), host):
        lanes = range(host)
        on = [active[start + i] for i in lanes]
        value = values[start:start + host]
        over = [on[i] and value[i] > LIMIT for i in lanes]
        mask = {"compare": over, "elected": over, "gathered": [on[i] and value[i] & 3 < 3 for i in lanes],
                "others": [on[i] and not over[i] for i in lanes], "scan": over, "latch": over}[name]
        count, first = sum(mask), mask.index(True) if any(mask) else 0xffffffff
        for i in lanes:
            rank = sum(mask[:i])
            if not on[i] or not any(mask) and name in ("elected", "others"):
                rows.append((0, 0, 0))
            elif name == "compare":
                rows.append((count, rank, first))
            elif name == "latch":
                # Three turns of the second loop for every lane; three of the first for its own.
                rows.append((3, 3 if mask[i] else 0, count))
            elif name == "scan":
                rows.append((count, 0, 2 * sum(value[j] for j in range(i) if on[j]) & 0xffffffff))
            elif name == "elected":
                rows.append((count if i == first else 0, rank, first) if mask[i] else (0, 0, 0))
            else:
                rows.append((count, rank, int(name == "others")))
    return rows


def instrument(hlsl, width):
    start = "    v0 = gtid.x; v1 = gtid.y; v2 = gtid.z;"
    load = ("    uint input = output[gtid.x];\n    exec_lo = input >> 31;\n"
            "    v0 = input & 0x7fffffffu; v1 = gtid.y; v2 = gtid.z;")
    if hlsl.count(start) != 1 or "WaveActiveBallot" not in hlsl or f"[WaveSize({width})]" not in hlsl:
        raise ValueError("fixture must take a ballot in waves of its own width")
    stores = (f"    output[{width}u + gtid.x] = v10 | (v11 << 8u) | (WaveGetLaneCount() << 16u);\n"
              f"    output[{2 * width}u + gtid.x] = v12;\n")
    hlsl = hlsl.replace(start, load)
    end = hlsl.rstrip().rfind("}")
    return "RWBuffer<uint> output : register(u0);\n" + hlsl[:end] + stores + "}\n"


def instrument_paired(hlsl):
    """Two console lanes per thread: lane `l` in one half and `l + 32` in the other."""
    start = re.search(r"^    s\d+ = gid\.x; s\d+ = gid\.y; s\d+ = gid\.z;\n", hlsl, re.M)
    if not start or "tb_pair" not in hlsl or "[WaveSize(32)]" not in hlsl or "WaveActiveBallot" not in hlsl:
        raise ValueError("fixture must take the paired translation")
    lanes = [f"(tb_first + tb_pair + {32 * half}u)" for half in (0, 1)]
    load = "".join(f"    uint input{half} = output[{lanes[half]}];\n    v0__[{half}] = input{half} & 0x7fffffffu;\n"
                   for half in (0, 1))
    load += ("    exec_lo = WaveActiveBallot((input0 >> 31) != 0u).x;\n"
             "    exec_hi = WaveActiveBallot((input1 >> 31) != 0u).x;\n")
    stores = ""
    for half in (0, 1):
        # A register the program never touches is not declared.
        low, high, last = (f"v{n}__[{half}]" if re.search(rf"\bv{n}__\b", hlsl) else "0u" for n in (10, 11, 12))
        stores += (f"    output[64u + {lanes[half]}] = {low} | ({high} << 8u) | ((WaveGetLaneCount() * 2u) << 16u);\n"
                   f"    output[128u + {lanes[half]}] = {last};\n")
    hlsl = hlsl[:start.end()] + load + hlsl[start.end():]
    end = hlsl.rstrip().rfind("}")
    return "RWBuffer<uint> output : register(u0);\n" + hlsl[:end] + stores + "}\n"


def inputs(width):
    yield from cases(width)
    yield "below", (1 << width) - 1, list(range(width))
    yield "one_above", (1 << width) - 1, [LIMIT + 1 if lane == 17 else lane for lane in range(width)]
    yield "some_above", (1 << width) - 1, [LIMIT - 20 + lane for lane in range(width)]
    yield "inactive_above", 0x0000ffff, [lane if lane < 16 else LIMIT + lane for lane in range(width)]


def seed_for(mask, values):
    width = len(values)
    seed = [0x76540000 + i for i in range(384)]
    seed[96:96 + width] = [v | (((mask >> lane) & 1) << 31) for lane, v in enumerate(values)]
    return seed


def difference(name, seed, mask, values, data):
    """The first word that is wrong, if any."""
    if len(data) != 4 * len(seed):
        return "no readback"
    actual, expected, width = struct.unpack("<384I", data), list(seed), len(values)
    host = actual[96 + width] >> 16
    if host != width:
        return f"host wave of {host}"
    rows = model(name, [bool((mask >> lane) & 1) for lane in range(width)], values, host)
    for lane, (count, rank, last) in enumerate(rows):
        expected[96 + width + lane] = count | rank << 8 | host << 16
        expected[96 + 2 * width + lane] = last
    return next((dict(word=i, got=a, wanted=b) for i, (a, b) in enumerate(zip(actual, expected)) if a != b), None)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("emitter", "llvm-mc", "dxc", "dxv", "provider", "proton", "prefix", "compiler-dir", "work",
                 "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--adapter", required=True)
    args = parser.parse_args(argv)
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    with TemporaryDirectory(dir=work) as tmp:
        trial = Path(tmp)
        trials, shaders = {}, {}
        native = set()
        for form, width, bodies in (("32", 32, WAVE32), ("64", 64, WAVE64), ("64-paired", 64, WAVE64)):
            paired = form == "64-paired"
            for name, body in bodies.items():
                words = words_of(program(body, width), args.llvm_mc, *WAVE_OPTIONS[width])
                emitted = subprocess.check_output(
                    [args.emitter.resolve(), form, *(f"{word:08x}" for word in words)], text=True, timeout=30)
                shader = f"{name}{width}" + ("paired" if paired else "")
                source = trial / f"{shader}.hlsl"
                source.write_text(instrument_paired(emitted) if paired else instrument(emitted, width))
                blob = trial / f"{shader}.cso"
                subprocess.run([args.dxc, "-T", "cs_6_6", "-E", "main", *(COMPUTE_FLAGS if paired else ()), "-Fo", blob,
                                source], check=True, timeout=30, capture_output=True)
                subprocess.run([args.dxv, blob], check=True, timeout=30, capture_output=True)
                shaders[shader] = dict(hlsl_sha256=digest(source), dxil_sha256=digest(blob))
                for case, mask, values in inputs(width):
                    trial_name = f"{shader}_{case}"
                    trials[trial_name] = name, seed_for(mask, values), mask, values
                    (trial / f"{trial_name}.seed").write_bytes(struct.pack("<384I", *trials[trial_name][1]))
                    (trial / f"{trial_name}.cso").hardlink_to(blob)
                    if form == "64":
                        (trial / f"{trial_name}.minwave").write_text("64\n")
                        native.add(trial_name)
        (trial / "cases.txt").write_text("".join(f"{name} 0 uint 1\n" for name in trials))
        exe, status, fields = run_gpu_guest(
            ROOT / "tools/reference/independent-shaders/compute.cpp", "ballots.exe", trial, work,
            args.proton.resolve(), args.prefix, args.provider, args.compiler_dir)
        # Only a host that reports narrower waves may leave the native 64-lane programs out.
        narrow = int(fields.get("wave_lanes_max", 0)) in range(1, 64)
        skipped = {trial_name for trial_name in native if narrow and fields.get(f"{trial_name}_skipped") == "64"}
        wrong = {}
        for trial_name, (name, seed, mask, values) in trials.items():
            if trial_name in skipped:
                continue
            path = trial / f"{trial_name}.bin"
            found = difference(name, seed, mask, values, path.read_bytes() if path.is_file() else b"")
            if found:
                wrong[trial_name] = found
        passed = status == 0 and fields.get("adapter") == args.adapter and not wrong
        report = dict(passed=passed, scope="Synthetic programs reading lane masks as numbers",
                      adapter=fields.get("adapter"), guest_exit=status, cases=len(trials) - len(skipped),
                      compared_words=(len(trials) - len(skipped)) * 384, wrong=wrong, shaders=shaders,
                      native_wave64_cases_not_run=len(skipped), wave_lanes_max=fields.get("wave_lanes_max"),
                      emitter_sha256=digest(args.emitter), guest_sha256=digest(exe))
        if fields.get("error"):
            report["error"] = fields["error"]
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + "\n")
    for name, detail in wrong.items():
        print(f"WRONG {name}: {detail}")
    if not passed:
        print(f"FAIL: {len(wrong)} of {len(trials) - len(skipped)} cases, guest exit {status}")
        return 1
    ran = len(trials) - len(skipped)
    note = f", {len(skipped)} native wave64 cases not run (host has no 64-lane waves)" if skipped else ""
    print(f"PASS: {ran} cases, {ran * 384} words, adapter {fields['adapter']}{note}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
