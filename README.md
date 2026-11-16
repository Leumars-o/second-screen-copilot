# Second Screen Copilot

Real-time call copilot: a PC agent reads the screen and the call, and cue answers stream to your phone.

- Product: `Second Screen Copilot — Product Spec.md`
- Build plan (agent swarm): `Second Screen Copilot — Build Sheet.md`
- Contracts (frozen at tag `contracts-v1`): `contracts/`, `crates/ssc-core/`

## Develop

Needs Rust stable (rustfmt, clippy), Node 20+, pnpm 10.

```sh
pnpm install
scripts/check.sh                    # full gate
scripts/check.sh crates/ssc-screen  # one owned path
```
