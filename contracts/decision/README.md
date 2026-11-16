# Decision contract (v1)

- `questions.json`: the five questions asked in one call per trigger (spec: "What it decides on each trigger").
- `decision.schema.json`: the **normalised** result every `DecisionModel` adapter must return (Rust: `ssc_core::decision::Decision`).
- Input stays under 500 tokens: last "them" question + screen diff (trimmed to fit) + profile ids.

**Not verified:** the TypeSafe `/v1/systemone` request/response wire shape. W1-MODELS confirms it from the provider docs (Build Sheet §9), records fixtures under `crates/ssc-router/tests/fixtures/`, and maps it into this normalised shape. One adapter serves Jev (cloud) and Tev1 4B (Ollama) by changing `base_url` and `model`.

**Safety:** the decision has no field that can change mode, redaction or data routing. `fixtures/invalid/routing_field.json` must be rejected.
