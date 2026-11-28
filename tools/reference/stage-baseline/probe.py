"""Run one compiler tool and record its wall time, CPU time and peak memory.

The per-stage baseline puts a stub in front of xshader, DXC and DXV that runs
`python -I probe.py LOG NAME TOOL ARGS...`. The tool is called exactly as the
caller asked; this script only waits for it with wait4 and appends one JSON
line to LOG. A start line comes first, so a tool the caller kills at its
timeout still leaves a record. Only the standard library is used, and no
shader text is recorded: the output file's size and the HLSL wave size.
"""
import ctypes
import json
import os
import re
import signal
import sys
import time

PR_SET_PDEATHSIG = 1
# Options worth keeping: the ones that select a compile variant. Paths are dropped.
VALUED = {"-T", "-E", "--stencil-target"}


def options(args):
    kept, take = [], False
    for arg in args:
        if take:
            kept.append(arg)
            take = False
        elif arg.startswith("-") and not arg.startswith(("-Fo", "--hlsl-out", "--profile-out")):
            kept.append(arg)
            take = arg in VALUED
    return kept


def output_of(args):
    for flag in ("--hlsl-out", "-Fo"):
        if flag in args and args.index(flag) + 1 < len(args):
            return args[args.index(flag) + 1]
    return None


def append(log, record):
    line = (json.dumps(record, sort_keys=True) + "\n").encode()
    fd = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    try:
        os.write(fd, line)
    finally:
        os.close(fd)


def main(argv):
    log, name, tool, *args = argv
    record = {"tool": name, "options": options(args), "pid": os.getpid()}
    append(log, {**record, "event": "start", "at": time.time()})
    parent = os.getpid()
    started = time.monotonic()
    child = os.fork()
    if child == 0:
        # The caller kills this process at its timeout; take the tool with it.
        ctypes.CDLL(None).prctl(PR_SET_PDEATHSIG, signal.SIGKILL)
        if os.getppid() != parent:
            os._exit(125)
        try:
            os.execv(tool, [tool, *args])
        except OSError:
            os._exit(127)
    _, status, usage = os.wait4(child, 0)
    seconds = time.monotonic() - started
    code = os.waitstatus_to_exitcode(status)
    record.update(event="end", at=time.time(), seconds=round(seconds, 4), exit=code,
                  cpu_seconds=round(usage.ru_utime + usage.ru_stime, 4),
                  # ru_maxrss is in KiB on Linux.
                  peak_rss_kib=usage.ru_maxrss)
    output = output_of(args)
    if output and os.path.isfile(output):
        record["output_bytes"] = os.path.getsize(output)
        if name == "xshader":
            with open(output, "rb") as stream:
                found = re.search(rb"\[WaveSize\((\d+)\)\]", stream.read())
            record["wave_size"] = int(found[1]) if found else None
    append(log, record)
    return code if code >= 0 else 128 - code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
