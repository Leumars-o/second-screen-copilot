#!/usr/bin/env python3
"""Regenerates the SYNTHETIC decision example set and its recorded-response fixtures.

  python3 bench/scripts/gen_decision_fixtures.py

Writes:
  bench/data/snippets.synthetic.jsonl          16 synthetic labelled snippets (format example)
  bench/fixtures/decision/jev.jsonl            synthetic /v1/systemone responses, one per snippet
  bench/fixtures/decision/tev1.jsonl           (same, with a few deliberate mistakes)

Nothing here is a real call or a real model output. The real set (bench/data/snippets.jsonl,
~200 items) is labelled by a human from real calls (Build Sheet §9).
"""
import json
import os

HERE = os.path.dirname(os.path.abspath(__file__))
BENCH = os.path.dirname(HERE)
P2 = ["sales", "support"]

# (id, question, screen_diff, profiles, needs_answer, intent, tier, profile, urgency)
ITEMS = [
    ("syn-001", "So what would this cost us for forty seats?", "Deal: Contoso, 40 seats", P2, True, "pricing", "fast", "sales", "high"),
    ("syn-002", "Honestly it feels expensive compared to what we use now.", "", P2, True, "objection", "fast", "sales", "high"),
    ("syn-003", "Does the API support webhooks with retries and signing?", "API docs: Webhooks", P2, True, "technical", "strong", "support", "medium"),
    ("syn-004", "Can we do Thursday at three instead?", "Calendar: Thu 15:00 free", P2, True, "scheduling", "fast", "sales", "medium"),
    ("syn-005", "How was your weekend, by the way?", "", P2, False, "small_talk", "none", "sales", "low"),
    ("syn-006", "Okay, let me share my screen.", "", P2, False, "none", "none", "sales", "low"),
    ("syn-007", "Abeg, how much be the annual plan?", "Pricing page", P2, True, "pricing", "fast", "sales", "high"),
    ("syn-008", "We tried something like this before and it didn't stick.", "", P2, True, "objection", "fast", "sales", "medium"),
    ("syn-009", "Is the data encrypted at rest, and which key management do you use?", "Security questionnaire, row 14", P2, True, "technical", "strong", "support", "high"),
    ("syn-010", "Sorry, you cut out for a second there.", "", P2, False, "none", "none", "sales", "low"),
    ("syn-011", "Do you people get discount for NGOs?", "", P2, True, "pricing", "fast", "sales", "medium"),
    ("syn-012", "Can someone from your team join our kickoff next Monday?", "", P2, True, "scheduling", "fast", "sales", "low"),
    ("syn-013", "Why does the export keep timing out on large workspaces?", "Ticket #4821: export timeout", P2, True, "technical", "strong", "support", "high"),
    ("syn-014", "Great, thanks, that's really helpful.", "", P2, False, "small_talk", "none", "sales", "low"),
    ("syn-015", "What's the difference between your team and enterprise tiers?", "Pricing page", ["sales"], True, "pricing", "fast", "sales", "medium"),
    ("syn-016", "Our security team will block anything without SSO.", "", P2, True, "objection", "fast", "support", "high"),
]

INTENTS = ["pricing", "objection", "technical", "scheduling", "small_talk", "none"]
TIERS = ["fast", "strong", "none"]
URG = ["low", "medium", "high"]

# Deliberate disagreements so accuracy is not trivially 100%: (model, id) -> overrides.
WRONG = {
    ("tev1", "syn-002"): {"intent": "pricing"},
    ("tev1", "syn-003"): {"tier": "fast"},
    ("tev1", "syn-006"): {"needs": True},
    ("tev1", "syn-007"): {"intent": "none", "needs": False, "tier": "none"},
    ("tev1", "syn-012"): {"urgency": "medium"},
    ("tev1", "syn-016"): {"profile": "sales"},
    ("jev", "syn-008"): {"urgency": "high"},
    ("jev", "syn-016"): {"profile": "sales"},
}
# Recorded latency (ms) for the loopback replay; synthetic.
DELAY = {"jev": [18, 22, 25, 19, 30, 21, 24, 26, 20, 23, 28, 22, 19, 27, 21, 24],
         "tev1": [35, 42, 38, 45, 40, 37, 50, 39, 41, 44, 36, 43, 47, 38, 40, 46]}


def choice(options, pick, p=0.8):
    rest = (1.0 - p) / (len(options) - 1)
    return {"type": "choice", "choice": pick,
            "probabilities": {o: round(p if o == pick else rest, 4) for o in options}}


def response(model_name, item, model_key):
    (iid, _q, _s, profiles, needs, intent, tier, profile, urgency) = item
    w = WRONG.get((model_key, iid), {})
    needs = w.get("needs", needs)
    answers = {
        "needs_answer": {"type": "noul", "noul": 0.9 if needs else 0.1},
        "intent": choice(INTENTS, w.get("intent", intent)),
        "answer_tier": choice(TIERS, w.get("tier", tier)),
        "urgency": choice(URG, w.get("urgency", urgency)),
    }
    if len(profiles) >= 2:
        answers["profile"] = choice(profiles, w.get("profile", profile))
    return {"model": model_name, "answers": answers, "usage": {"input_tokens": 120, "output_tokens": 0}}


def main():
    with open(os.path.join(BENCH, "data", "snippets.synthetic.jsonl"), "w") as f:
        for it in ITEMS:
            (iid, q, s, profiles, needs, intent, tier, profile, urgency) = it
            f.write(json.dumps({
                "id": iid, "synthetic": True,
                "source": "SYNTHETIC example written for the format; not from a real call",
                "question": q, "screen_diff": s, "profiles": profiles,
                "label": {"needs_answer": needs, "intent": intent, "answer_tier": tier,
                          "profile": profile, "urgency": urgency},
                "labeller": "synthetic", "notes": "",
            }) + "\n")
    for key, name in (("jev", "jev-1.14.0"), ("tev1", "tev1:4b")):
        with open(os.path.join(BENCH, "fixtures", "decision", f"{key}.jsonl"), "w") as f:
            for i, it in enumerate(ITEMS):
                f.write(json.dumps({"id": it[0], "delay_ms": DELAY[key][i], "synthetic": True,
                                    "response": response(name, it, key)}) + "\n")


main()
