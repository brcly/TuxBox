#!/usr/bin/env python3
"""Run a fixed set of synthetic provider regression controls offline.

The supplied build artifacts must come from the intended revision. The
source Proton prefix is copied and never modified. Logs remain local.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
from tempfile import TemporaryDirectory
from threading import Lock
from time import monotonic

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from project_tuxbox.inspect import digest  # noqa: E402
from project_tuxbox.jobs import check_jobs  # noqa: E402
# The launcher builds its Vulkan layer on first use; one case at a time.
LAYER_BUILD = Lock()

CASES = {
    "root-cbv-window": ("d3d12-root-cbv-window", (), "d3d12-root-cbv-window", ("words=120/120","window=65536","offsets=0,256,65280,65536,65792")),
    "root-cbv-offsets": ("d3d12-root-cbv-window", ("--offsets",), "d3d12-root-cbv-offsets", ("offsets=17/17","step=16","words=408/408")),
    "tables": ("d3d12-tables", (), "d3d12-tables", ("words=16/16", "placed-srv=16/16")),
    "heaps": ("d3d12-heaps", (), "d3d12-heaps", ("words=256/256", "volume=45/45")),
    "alias-deferred": ("d3d12-copy-resource", ("--alias-deferred",), "d3d12-alias-deferred", ("overlaps=2", "old=verified", "refreshed=verified", "no_cpu_wait=1")),
    "copy-resource": ("d3d12-copy-resource", (), "d3d12-copy-resource", ("placed_dst=verified", "cpu_writeback=verified", "overlap_mips=4", "pending_reader=verified")),
    "placement": ("d3d12-placement", (), "d3d12-placement", ("R8-texels=262144", "rejected=3")),
    "render-heap": ("d3d12-placement", ("--render-heap",), "d3d12-render-heap", ("slot=133", "kept=65536/65536")),
    "stream": ("d3d12-draw", ("--stream",), "d3d12-stream", ("slot=86", "rejected=4")),
    "mesh": ("d3d12-mesh", (), "d3d12-mesh", ("pixels=256/256", "guards=32", "rejected=5")),
    "frame": ("xbox-frame", (), "xbox-frame", ("pixels=962/962",)),
    "indirect": ("d3d12-indirect", (), "d3d12-indirect", ("list=59", "words=64/64")),
    "indirect-scarlett": ("d3d12-indirect", ("--scarlett",), "d3d12-indirect", ("list=61", "words=64/64")),
    "buffer-remap": ("d3d12-buffer-import", ("--remap",), "d3d12-buffer-remap", ("words=768/768", "identical=1", "replacement=1")),
    "buffer-layout": ("d3d12-buffer-import", ("--layout",), "d3d12-buffer-layout", ("slot=133", "words=512/512", "VA-offsets=0,4")),
    "buffer-import": ("d3d12-buffer-import", (), "d3d12-buffer-import", ("pages=2", "words=1536/1536", "passes=2", "aliases=2")),
    "buffer-import-reject": ("d3d12-buffer-import", ("--reject",), "d3d12-buffer-import-reject", ("rejected=1",)),
    "buffer-unmap-alive": ("d3d12-buffer-import", ("--unmap-alive",), "d3d12-buffer-unmap", ("mode=alive",)),
    "buffer-unmap-released": ("d3d12-buffer-import", ("--unmap-released",), "d3d12-buffer-unmap", ("mode=released", "later-submits=ok")),
    "null-usage": ("d3d12-null-usage", (), "d3d12-null-usage", ("words=64/64", "recorded=64", "valid=2")),
    "null-disabled": ("d3d12-null-usage", ("--reject",), "d3d12-null-disabled", ("rejected=1", "valid=1")),
    "address-uav": ("d3d12-address-uav", (), "d3d12-address-uav", ("words=10/10", "start=432", "stride=20")),
}
DEFAULT = tuple(name for name in CASES if name not in ("root-cbv-window", "root-cbv-offsets", "alias-deferred", "copy-resource", "indirect-scarlett", "mesh", "buffer-import", "buffer-import-reject", "buffer-layout", "buffer-remap", "buffer-unmap-alive", "buffer-unmap-released"))
# These run with the constant-buffer settings `tuxbox.py run` gives a game. The
# other cases keep vkd3d-proton's defaults.
LAUNCHER_CBV = ("root-cbv-offsets",)
DIGEST = hashlib.sha256(bytes.fromhex("78563412efcdab90")).hexdigest()


def imports(records):
    """Drop the records of the page cache (`page_open`, `span_hit` and so on),
    which say how an import was found. What was imported, retried or replaced
    by a stand-in stays."""
    return [record for record in records if not record[0].startswith(("page_", "span_"))]


def launcher_cbv(proton, executable):
    """The variables `tuxbox.py run` adds so 16-byte constant-buffer offsets read
    correctly, taken from the launcher's own code so the control follows it.

    The root CBV case needs both the layer and force_raw_va_cbv on NVIDIA. An
    RTX 5090 read 5 of 17 offsets with vkd3d-proton's defaults and with either alone.
    """
    sys.path.insert(0, str(ROOT))
    from project_tuxbox.console_cbv import configure_console_cbv

    # The provider beside the guest is the build under test, wherever it was built.
    before = dict(os.environ, PROTONPATH=str(proton), TUXBOX_REFERENCE_CBV="1")
    env = dict(before)
    configure_console_cbv("proton", env, executable, lambda message: None)
    return {key: value for key, value in env.items() if before.get(key) != value}


def validate(case, adapter, status, output):
    _, _, name, markers = CASES[case]
    prefix = f"{name} PASS pci={adapter.lower()} "
    lines = output.splitlines()
    # A success line from another case or adapter is contradictory, not a
    # second independent pass. Do not accept duplicate receipt fields.
    passes = [line for line in lines if re.match(r"(?:d3d12-|xbox-frame)[a-z0-9-]* PASS ", line)]
    if status or len(passes) != 1 or not passes[0].lower().startswith(prefix.lower()):
        return False, None
    fields = passes[0].split()[2:]
    keys = [field.split("=", 1)[0] for field in fields]
    if len(set(keys)) != len(keys) or any(marker not in fields for marker in markers):
        return False, None
    fallback = [line for line in lines if "TuxBox d3d12_xs: substituted null " in line]
    if case != "null-usage":
        # A compatibility control must exercise the real pipeline, not a
        # successful no-op substitution (even if its output happens to match).
        if fallback or any("TuxBox d3d12_xs: null usage rank " in line for line in lines):
            return False, passes[0]
    if case in ("buffer-import","buffer-import-reject"):
        # Proton may insert another diagnostic between the provider prefix
        # and its message. Accept an exact complete body, optionally prefixed.
        records = re.findall(r"(?m)^(?:TuxBox d3d12_xs: )?buffer backing path="
                             r"([a-z_]+) count=([0-9]+) source=place_virtual bytes=65536$", output)
        expected = ([("pool_open", str(i)) for i in range(1,4)]
                    + [("pool_hit", str(i)) for i in range(1,3)]
                    + [("imported", str(i)) for i in range(1,6)]
                    if case == "buffer-import" else [("strict_rejected", "1")])
        if sorted(records) != sorted(expected):
            return False, passes[0]
    if case == "buffer-remap":
        records = re.findall(r"(?m)^(?:TuxBox d3d12_xs: )?buffer backing path=([a-z_]+) count=([0-9]+) source=place_virtual_layout bytes=65536$", output)
        expected = [("pool_open","1"),("pool_open","2"),("pool_hit","1"),("imported","1"),("imported","2"),("imported","3")]
        if sorted(records)!=sorted(expected) or output.count("retired stale GPU import")!=1:
            return False, passes[0]
    if case == "buffer-unmap-alive":
        # Whether later submits survive is a property of the host driver, so
        # either answer passes. It must be one of the two, and stated.
        if not {"later-submits=ok", "later-submits=lost"} & set(fields):
            return False, passes[0]
    if case == "buffer-layout":
        records = re.findall(r"(?m)^(?:TuxBox d3d12_xs: )?buffer backing path="
                             r"([a-z_]+) count=([0-9]+) source=place_virtual_layout bytes=([0-9]+)$", output)
        # The host may expose different tight alignment. Require both
        # successful imports/retries and no stand-in, regardless of padding.
        if sorted((path, count) for path,count,_ in imports(records)) != [
            ("commit_retry","1"), ("commit_retry","2"), ("imported","1"), ("imported","2")]:
            return False, passes[0]
    if case == "root-cbv-window":
        records = re.findall(r"(?m)^(?:TuxBox d3d12_xs: )?buffer backing path=([a-z_]+) count=([0-9]+) source=view bytes=([0-9]+)$", output)
        if sorted(imports(records)) != [("imported","1","65536"),("imported","2","131072"),("imported","3","131072"),("imported","4","65536"),("imported","5","131072")]:
            return False, passes[0]
        if "bound with a short range" in output or "substituted null " in output:
            return False, passes[0]
    if case == "alias-deferred":
        times = re.findall(r"(?m)^alias deferred observed ms=([0-9]+)$", output)
        if len(times)!=1 or int(times[0])>1000:
            return False, passes[0]
        if output.count("placed texture refresh deferred until an overlapping write is published")!=2:
            return False, passes[0]
        if "Failed to signal event" in output or "Failed to set event" in output:
            return False, passes[0]
    if case == "null-usage":
        # Driver output may interleave; only accept complete exact records.
        summaries = [line.split("TuxBox d3d12_xs: ",1)[1] for line in output.splitlines()
                     if line.startswith("TuxBox d3d12_xs: null usage interval ")]
        ranks = [line.split("TuxBox d3d12_xs: ",1)[1] for line in output.splitlines()
                 if line.startswith("TuxBox d3d12_xs: null usage rank ")]
        expected = []
        expected_ranks = []
        for interval,count in ((1,1),(2,63)):
            expected.append(f"null usage interval recorded={count} display_bound=0 tracked=1 overflow=0 overflow_display_bound=0 compute={count} raster=0 interval={interval} trigger=recording_checkpoint")
            expected_ranks.append(f"null usage rank kind=compute CS={DIGEST} recorded={count} display_bound=0 interval={interval} trigger=recording_checkpoint")
        if (sorted(summaries)!=sorted(expected) or sorted(ranks)!=sorted(expected_ranks)
                or sorted(fallback) != sorted(
                    f"TuxBox d3d12_xs: substituted null compute pipeline for an untranslated shader ({n} so far)"
                    for n in (1,2))):
            return False, passes[0]
    return True, passes[0]


def copy_prefix(source, trial):
    """Copy the source prefix, skipping rebuilt GPU caches.

    A reflinkable filesystem would make a full copy free, but ext4 has no
    reflinks and the prefix can carry tens of GB of caches, so the copy is
    bounded to what keeps a trial disposable: registries, files and user
    data, never the caches. Those rebuild into the trial's own cache path.
    """
    ignore = shutil.ignore_patterns("shadercache", "tuxbox_shader_cache")
    shutil.copytree(source, trial, ignore=ignore, symlinks=True, dirs_exist_ok=False)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__)
    for arg in ("build-dir","proton","prefix","compiler-dir","work","output"):
        ap.add_argument("--"+arg,type=Path,required=True)
    ap.add_argument("--xmem",type=Path)
    ap.add_argument("--adapter",required=True)
    ap.add_argument("--case",choices=tuple(CASES),action="append")
    a=ap.parse_args(argv)
    if not re.fullmatch(r"[a-fA-F0-9]{4}:[a-fA-F0-9]{4}",a.adapter):
        ap.error("adapter must be a PCI vendor:device ID")
    selected=a.case or DEFAULT
    if len(set(selected))!=len(selected) or a.output.exists() or a.work.exists():
        ap.error("unique cases and fresh work/output required")
    a.work=a.work.resolve()
    a.prefix=a.prefix.resolve()
    if not a.prefix.is_dir() or a.work.is_relative_to(a.prefix) or a.output.resolve().is_relative_to(a.prefix):
        ap.error("work/output must be outside the source prefix")
    for n in ("bwrap","xvfb-run","timeout"):
        if not shutil.which(n):
            ap.error(f"missing host tool: {n}")
    files=a.proton.resolve()/"files"
    wine=files/"bin/wine"
    libs=files/"lib/wine"
    artifacts={"d3d12_xs.dll":a.build_dir/"d3d12_xs.dll",
               "d3d12.dll":libs/"vkd3d-proton/x86_64-windows/d3d12.dll",
               "d3d12core.dll":libs/"vkd3d-proton/x86_64-windows/d3d12core.dll",
               "dxgi.dll":libs/"dxvk/x86_64-windows/dxgi.dll",
               "dxcompiler.dll":a.compiler_dir/"dxcompiler.dll",
               "dxil.dll":a.compiler_dir/"dxil.dll",
               "wine":wine,"wineserver":files/"bin/wineserver"}
    artifacts.update({CASES[c][0]+".exe":a.build_dir/(CASES[c][0]+".exe") for c in selected})
    if "buffer-remap" in selected:
        if not a.xmem:
            ap.error("buffer-remap needs --xmem")
        artifacts["xmem.dll"]=a.xmem
    if any(not p.is_file() for p in (*artifacts.values(),wine)):
        ap.error("missing build, Proton or compiler artifact")
    fingerprints={k:digest(p) for k,p in artifacts.items()}
    a.work.mkdir(parents=True)
    def run_case(case):
        start=monotonic()
        name,extra,_,_=CASES[case]
        with TemporaryDirectory(dir=a.work,prefix=case+"-") as temp:
            trial=Path(temp)
            for k,source in artifacts.items():
                if k.endswith(".dll") or k==name+".exe":
                    shutil.copyfile(source,trial/k)
                    if digest(trial/k)!=fingerprints[k]:
                        raise ValueError("artifact changed during staging")
            copy_prefix(a.prefix, trial/"prefix")
            for k in ("tmp","runtime","cache"):
                (trial/k).mkdir(mode=0o700)
            env={"WINEPREFIX":trial/"prefix","WINESERVER":files/"bin/wineserver",
                 "WINEDEBUG":"-all","VKD3D_DEBUG":"warn","WINEDLLOVERRIDES":"d3d12,d3d12core,dxgi=n,b",
                 "XDG_CACHE_HOME":trial/"cache","XDG_RUNTIME_DIR":trial/"runtime","TMPDIR":trial/"tmp",
                 "VKD3D_SHADER_CACHE_PATH":trial,"TUXBOX_LOG":"stderr","TUXBOX_TRACE_PRESENT":"1",
                 "TUXBOX_NULL_PIPELINES":"1" if case=="null-usage" else "0",
                 "TUXBOX_REQUIRE_BUFFER_IMPORT":"1" if case.startswith("buffer-") else "0",
                 "TUXBOX_TRACE_BUFFER_BACKING":"1" if case.startswith("buffer-") or case=="root-cbv-window" else "0"}
            settings,failure={},None
            if case in LAUNCHER_CBV:
                try:
                    with LAYER_BUILD:
                        settings=launcher_cbv(a.proton.resolve(),trial/(name+".exe"))
                    env.update(settings)
                except (OSError,subprocess.CalledProcessError) as error:
                    failure=f"constant-buffer setup failed: {error}"
            cmd=["bwrap","--die-with-parent","--unshare-pid","--unshare-net","--ro-bind","/","/",
                 "--dev-bind","/dev","/dev","--proc","/proc","--tmpfs","/tmp","--bind",str(trial),str(trial),
                 "--unsetenv","WAYLAND_DISPLAY","--chdir",str(trial)]
            for k,v in env.items():
                cmd.extend(("--setenv",k,str(v)))
            cmd+=["--","xvfb-run","-a","timeout","-k","2s","45s",str(wine),str(trial/(name+".exe")),
                  *extra,"--adapter",a.adapter]
            log=a.work/(case+".log")
            with log.open("xb") as stream:
                try:
                    if failure:
                        # Running without the settings would test another configuration.
                        stream.write(failure.encode()+b"\n")
                        status=1
                    else:
                        q=subprocess.run(cmd,stdout=stream,stderr=subprocess.STDOUT,timeout=55)
                        status=q.returncode
                except subprocess.TimeoutExpired:
                    status=124
            # Logs are local only; the JSON receipt contains a fixed case
            # name, exit status and the known fixture's synthetic PASS line.
            ok,receipt=validate(case,a.adapter,status,log.read_text(errors="replace"))
            row=dict(case=case,passed=ok,exit=status,seconds=round(monotonic()-start,3),receipt=receipt,
                     null_fallback_enabled=case=="null-usage")
            if case in LAUNCHER_CBV:
                # The names say which route ran. Paths stay out of the receipt.
                row["launcher_cbv"]={k:settings.get(k,"") for k in ("VKD3D_CONFIG","VK_INSTANCE_LAYERS","VKD3D_DISABLE_EXTENSIONS")}
            return row,failure
    rows=[]
    # Each case runs in its own sandbox with a copied prefix and its own X server.
    with ThreadPoolExecutor(check_jobs()) as pool:
        for row,failure in pool.map(run_case,selected):
            rows.append(row)
            print(row["case"],"PASS" if row["passed"] else "FAIL",*([failure] if failure else []),flush=True)
    unchanged=fingerprints=={k:digest(p) for k,p in artifacts.items()}
    report=dict(schema=1,scope="Synthetic D3D12 provider controls; not retail execution",
                adapter=a.adapter.lower(),proton=(a.proton/"version").read_text().strip(),
                artifacts_sha256=fingerprints,unchanged=unchanged,cases=rows,
                passed=unchanged and all(row["passed"] for row in rows))
    a.output.parent.mkdir(parents=True,exist_ok=True)
    with a.output.open("x") as stream:
        json.dump(report,stream,indent=2);stream.write("\n")
    return int(not report["passed"])


if __name__=="__main__":
    raise SystemExit(main())
