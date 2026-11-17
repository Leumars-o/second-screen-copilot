#!/usr/bin/env python3
"""Regenerates bench/fixtures/latency/calls.json (scripted stage times, fixed seed).

Usage: python3 bench/scripts/gen_latency_fixture.py > bench/fixtures/latency/calls.json
The numbers are plausible, not measured: they exist so the harness has something to replay.
"""
import json
import random
import sys

random.seed(42)
QS = [
    "What is your pricing?", "Do you integrate with Salesforce?", "How long is onboarding?",
    "Is there a discount for annual plans?", "Can we get a demo next week?", "What about SSO?",
    "How do you handle data retention?", "Who else uses this in fintech?", "Can it run on-prem?",
    "What happens if we exceed the seat limit?",
]


def utt(q, local):
    r = random.randint
    return {"question": q, "stage_ms": {
        "transcript_final": r(250, 600) if local else r(120, 320),
        "trigger_fired": r(5, 40),
        "decision_done": r(250, 700) if local else r(150, 420),
        "context_ready": r(10, 40),
        "model_request": r(1, 6),
        "first_token": r(400, 1300) if local else r(220, 700),
        "frame_sent": r(3, 15),
        "rtt_ms": r(20, 80),
        "phone_delta_ms": r(15, 60),
    }}


calls = [
    {"id": "cloud-call-1", "mode": "cloud", "utterances": [utt(q, False) for q in QS]},
    {"id": "cloud-call-2", "mode": "cloud", "utterances": [utt(q, False) for q in reversed(QS)]},
    {"id": "local-call-1", "mode": "local", "utterances": [utt(q, True) for q in QS[:8]]},
]
json.dump(calls, sys.stdout, indent=1)
sys.stdout.write("\n")
