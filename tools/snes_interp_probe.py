#!/usr/bin/env python3
"""snes_interp_probe.py — AOT-vs-interpreter execution-coverage probe for
snesrecomp games.

snesrecomp v2 is LLE-first: a 65816 interpreter is the always-compiled-in
baseline and AOT-recompiled C is the fast tier on top. Code with no exact
AOT body silently tiers down to the interpreter. This probe answers "is
the game mostly statically recompiled, or silently interpreting?" from the
runtime's always-on counters — no pausing (RULE 0), no lossy ring sampling.

Primary signal: the `interp_stats` TCP command (added to runner/debug_server.c):
  dispatch_total, found1 (AOT), found0 (interp), found0_pct,
  tier_hits, tier2_sites, tier2_clean, tier2_bail (bail = interp step-cap
  = correctness risk, should be 0).

Snapshots interp_stats before/after a driven nav phase to get BOTH the
cumulative rate and the rate DURING gameplay (the delta). Also mines the
runtime's [tier2] stderr log for the distinct clean/bail gap breakdown.

Requires the runtime built with -DSNESRECOMP_ENABLE_TRACE=ON (TCP server)
and launched with the ROM as a positional arg (skips the GUI launcher).
Ports: SMW 4377 / Zelda 4378 / MMX 4379. Text protocol: `cmd arg=val\n`.
"""
from __future__ import annotations

import json
import re
import socket
import sys
import time
from pathlib import Path

# SNES pad bits (debug_server.c parse_controller_mask).
BTN = {"b": 0x1, "y": 0x2, "select": 0x4, "start": 0x8, "up": 0x10,
       "down": 0x20, "left": 0x40, "right": 0x80, "a": 0x100, "x": 0x200,
       "l": 0x400, "r": 0x800}
PORTS = {"smw": 4377, "zelda": 4378, "mmx": 4379}


class Runtime:
    def __init__(self, port, host="127.0.0.1"):
        self.port, self.host = port, host

    def cmd(self, line):
        last = None
        for _ in range(6):
            try:
                with socket.create_connection((self.host, self.port), timeout=15) as s:
                    s.sendall((line + "\n").encode())
                    s.settimeout(15)
                    buf = b""
                    while not buf.endswith(b"\n"):
                        d = s.recv(1 << 16)
                        if not d:
                            break
                        buf += d
                return buf.decode(errors="replace").strip()
            except (OSError, ConnectionError) as e:
                last = e
                time.sleep(0.25)
        raise RuntimeError(f"cmd {line!r} failed: {last}")

    def jcmd(self, line):
        return json.loads(self.cmd(line))

    def alive(self):
        try:
            return self.jcmd("ping").get("ok", False)
        except (RuntimeError, ValueError):
            return False

    def mask(self, *btns):
        m = 0
        for b in btns:
            m |= BTN[b]
        return m

    def press(self, *btns, hold=0.12, gap=0.22):
        self.cmd("set_controller 0x%04x" % self.mask(*btns))
        time.sleep(hold)
        self.cmd("clear_controller")
        time.sleep(gap)

    def hold(self, mask, dur):
        self.cmd("set_controller 0x%04x" % mask)
        time.sleep(dur)
        self.cmd("clear_controller")

    def shot(self, bmp_path):
        return self.cmd("screenshot " + str(bmp_path))

    def stats(self):
        return self.jcmd("interp_stats")


def run_steps(rt: Runtime, steps, shotdir: Path):
    """steps: list of ('press',(btns),), ('hold',mask,dur), ('wait',s),
    ('shot',name)."""
    for st in steps:
        op = st[0]
        if op == "press":
            rt.press(*st[1], hold=st[2] if len(st) > 2 else 0.12)
        elif op == "hold":
            rt.hold(st[1], st[2])
        elif op == "wait":
            time.sleep(st[1])
        elif op == "shot":
            try:
                rt.shot(shotdir / f"{st[1]}.bmp")
            except RuntimeError:
                pass


def scan_tier2_log(log_path):
    """Distinct clean/bail interp gap sites + kind breakdown from the
    always-on [tier2] stderr stream."""
    if not log_path or not Path(log_path).exists():
        return None
    txt = Path(log_path).read_text(errors="ignore")
    lines = re.findall(r"INTERP GAP #(\d+)\s+(\w+)\s+.*\b(clean|bail)\b", txt)
    if not lines:
        return None
    kinds, clean, bail = {}, 0, 0
    for _, kind, res in lines:
        kinds[kind] = kinds.get(kind, 0) + 1
        if res == "clean":
            clean += 1
        else:
            bail += 1
    return {"distinct_sites": len(lines), "clean_sites": clean,
            "bail_sites": bail, "kinds": kinds}


def build_report(game, base, final, tier2log):
    """base/final: interp_stats snapshots around the driven nav phase."""
    d_total = final["dispatch_total"] - base["dispatch_total"]
    d_f0 = final["found0"] - base["found0"]
    during_pct = round(100 * d_f0 / d_total, 3) if d_total else 0.0
    # Mode-independent headline: interp guest-cycles / total guest-cycles over
    # the play window. Works in HLE and LLE and when dispatch_total==0.
    d_icyc = final.get("interp_cycles", 0) - base.get("interp_cycles", 0)
    d_mcyc = final.get("master_cycles", 0) - base.get("master_cycles", 0)
    d_iins = final.get("interp_insns", 0) - base.get("interp_insns", 0)
    cyc_pct = round(100 * d_icyc / d_mcyc, 4) if d_mcyc else 0.0
    rep = {
        "game": game,
        "cumulative": {
            "dispatch_total": final["dispatch_total"],
            "found1": final["found1"], "found0": final["found0"],
            "found0_pct": final["found0_pct"],
            "tier_hits": final["tier_hits"],
            "tier2_sites": final["tier2_sites"],
            "tier2_clean": final["tier2_clean"],
            "tier2_bail": final["tier2_bail"],
        },
        "during_nav": {
            "dispatch_delta": d_total, "found0_delta": d_f0,
            "found0_pct": during_pct,
            "interp_cycles": d_icyc, "master_cycles": d_mcyc,
            "interp_insns": d_iins,
            "interp_cycle_pct": cyc_pct,     # <- mode-independent headline
        },
        "tier2_log": tier2log,
        "verdict": _verdict(final, cyc_pct),
    }
    return rep


def _verdict(final, cyc_pct):
    """cyc_pct = interp guest-cycles as % of total during the play window."""
    if final["tier2_bail"] > 0:
        return (f"RISK — {final['tier2_bail']} interp BAIL(s): interpreter hit "
                "its step cap; investigate (possible correctness gap)")
    if cyc_pct < 1.0:
        return (f"MOSTLY STATIC — {cyc_pct}% of guest cycles ran on the "
                "interpreter; ~{:.1f}% ran as AOT C.".format(100 - cyc_pct))
    if cyc_pct < 15.0:
        return (f"MIXED — {cyc_pct}% of guest cycles interpreted; notable AOT "
                "gap. See tier2 worklist for promotion targets.")
    return (f"INTERP-HEAVY — {cyc_pct}% of guest cycles ran on the interpreter; "
            "static coverage is NOT carrying execution in this mode.")


def render_md(rep):
    c, n = rep["cumulative"], rep["during_nav"]
    L = [f"# {rep['game']} — AOT vs interpreter coverage", "",
         f"**{rep['verdict']}**", "",
         "## During driven gameplay (the meaningful window)",
         f"- **interp guest-cycles: {n.get('interp_cycle_pct',0)}%** "
         f"of {n.get('master_cycles',0):,} total  "
         f"(interp ran {n.get('interp_insns',0):,} instrs) "
         f"— mode-independent headline",
         f"- indirect-dispatch path: {n['dispatch_delta']:,} dispatches, "
         f"{n['found0_delta']:,} to interp ({n['found0_pct']}%)", "",
         "## Cumulative (boot -> end)",
         f"- dispatch_total: {c['dispatch_total']:,}",
         f"- found1 (AOT): {c['found1']:,}   found0 (interp): {c['found0']:,} "
         f"({c['found0_pct']}%)",
         f"- interp tier-down hits: {c['tier_hits']:,}",
         f"- distinct interp gap sites: {c['tier2_sites']:,} "
         f"(clean summed {c['tier2_clean']:,}, **bail {c['tier2_bail']:,}**)"]
    if rep.get("tier2_log"):
        t = rep["tier2_log"]
        L += ["", "## tier2 stderr gap log",
              f"- distinct sites: {t['distinct_sites']} "
              f"(clean {t['clean_sites']}, bail {t['bail_sites']})",
              f"- kinds: {t['kinds']}"]
    return "\n".join(L)


# Per-game nav scripts. attract -> menu -> into gameplay. Refine per game.
def nav_smw(shotdir):
    return [("wait", 4), ("shot", "0_boot"),
            ("press", ("start",)), ("press", ("start",)), ("wait", 1),
            ("shot", "1_title"),
            ("press", ("a",)), ("wait", 1),            # 1P game -> file select
            ("press", ("a",)), ("wait", 1.5),          # select file
            ("press", ("a",)), ("press", ("a",)), ("wait", 2),  # intro -> map
            ("press", ("a",)), ("press", ("a",)), ("wait", 1.5),
            ("press", ("a",)), ("wait", 3), ("shot", "2_level")]


def nav_play(shotdir):
    """Generic ~15s gameplay farm: run right + jump."""
    R, Y, B, A, L = BTN["right"], BTN["y"], BTN["b"], BTN["a"], BTN["left"]
    seq = [R | Y, R | Y | B, R | Y, B, R | B, L | Y, R | Y | B, R | Y | A]
    return [("hold", seq[i % len(seq)], 0.5) for i in range(24)]


NAVS = {"smw": nav_smw}


def main(argv=None):
    import argparse
    ap = argparse.ArgumentParser()
    ap.add_argument("--game", required=True, choices=PORTS)
    ap.add_argument("--port", type=int)
    ap.add_argument("--shotdir", type=Path, default=Path("shots"))
    ap.add_argument("--log", type=Path, help="runtime stderr log ([tier2] lines)")
    ap.add_argument("--out", type=Path, help="report basename")
    ap.add_argument("--play-only", action="store_true",
                    help="skip menu nav; assume already in gameplay")
    args = ap.parse_args(argv)
    port = args.port or PORTS[args.game]
    rt = Runtime(port)
    if not rt.alive():
        print(f"runtime not reachable on {port}", file=sys.stderr)
        return 2
    args.shotdir.mkdir(parents=True, exist_ok=True)
    if not args.play_only and args.game in NAVS:
        run_steps(rt, NAVS[args.game](args.shotdir), args.shotdir)
    base = rt.stats()                              # baseline before play window
    run_steps(rt, nav_play(args.shotdir), args.shotdir)
    rt.shot(args.shotdir / "3_play.bmp")
    final = rt.stats()
    rep = build_report(args.game, base, final, scan_tier2_log(args.log))
    print(json.dumps(rep, indent=2))
    print("\n" + render_md(rep))
    if args.out:
        args.out.with_suffix(".json").write_text(json.dumps(rep, indent=2))
        args.out.with_suffix(".md").write_text(render_md(rep))
    return 0


if __name__ == "__main__":
    sys.exit(main())
