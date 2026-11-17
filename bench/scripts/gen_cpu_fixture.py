#!/usr/bin/env python3
"""Regenerates bench/fixtures/cpu/samples.json: SYNTHETIC CPU samples, fixed seed.

  python3 bench/scripts/gen_cpu_fixture.py > bench/fixtures/cpu/samples.json

Format: [{"phase": "idle"|"capturing", "samples": [[wall_ms, cumulative_cpu_ms], ...]}].
Idle averages ~0.8% of a core, capturing ~6%. Not a measurement.
"""
import json
import random
import sys

random.seed(7)


def phase(name, seconds, lo, hi, cpu0):
    out, cpu = [[0, cpu0]], cpu0
    for s in range(1, seconds + 1):
        cpu += random.randint(lo, hi)  # ms of CPU in this 1 s interval
        out.append([s * 1000, cpu])
    return {"phase": name, "samples": out}


json.dump([phase("idle", 60, 2, 14, 1500), phase("capturing", 60, 30, 90, 9000)], sys.stdout)
sys.stdout.write("\n")
