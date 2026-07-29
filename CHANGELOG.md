# Changelog

## [0.1.1](https://github.com/forkwright/epistole/compare/v0.1.0...v0.1.1) (2026-07-29)


### Bug Fixes

* **release:** keep Cargo.lock in lockstep with the package version ([#76](https://github.com/forkwright/epistole/issues/76)) ([7c9090d](https://github.com/forkwright/epistole/commit/7c9090d2065c18153c6851fa1c0723e47f9a67d0)), closes [#75](https://github.com/forkwright/epistole/issues/75)

## [0.1.0](https://github.com/forkwright/epistole/compare/v0.0.1...v0.1.0) (2026-07-28)


### Features

* **_llm:** add T0 corpus per [#667](https://github.com/forkwright/epistole/issues/667) / [#673](https://github.com/forkwright/epistole/issues/673) fleet rollout ([#11](https://github.com/forkwright/epistole/issues/11)) ([e53eeac](https://github.com/forkwright/epistole/commit/e53eeac0a2a5244f2142ce8a1fa1631fd7436ac9))
* **archive:** render sends ledger ([e0b6e80](https://github.com/forkwright/epistole/commit/e0b6e807295d72689e2400789ef587b18a61f44d))
* **epistole:** Phase 0 substrate scaffold ([b2bc1e2](https://github.com/forkwright/epistole/commit/b2bc1e2bbaada031918009276ff1eb276744de17))
* **epistole:** Phase 1 security hardening ([#3](https://github.com/forkwright/epistole/issues/3)) ([dfca221](https://github.com/forkwright/epistole/commit/dfca221469037554b16f649751f0831353d480a8))
* **epistole:** Phase 1.5 — security audit remediation (closes 13 findings) ([#6](https://github.com/forkwright/epistole/issues/6)) ([17bfcad](https://github.com/forkwright/epistole/commit/17bfcad6895750b9d4cc225ae0fd4e9b69de3361))
* **epistole:** Phase 1.5.1 — re-audit followups (closes 5 findings) ([#7](https://github.com/forkwright/epistole/issues/7)) ([66bf568](https://github.com/forkwright/epistole/commit/66bf568b7a7469a49b392ed13aa9a28d327e2dc7))
* **epistole:** Phase 1.5.2 — codex-mm reaudit followups (closes 4 findings) ([#8](https://github.com/forkwright/epistole/issues/8)) ([7df123a](https://github.com/forkwright/epistole/commit/7df123ace192e116758d7e5cbeea9eecae6e9b48))


### Bug Fixes

* **ci:** osv-scanner job permissions — drop security-events:write ([#54](https://github.com/forkwright/epistole/issues/54)) ([#56](https://github.com/forkwright/epistole/issues/56)) ([f430a3a](https://github.com/forkwright/epistole/commit/f430a3ae713278241bb2af4bcd32268f47366b04))
* **deploy:** UMask=0077 missing from committed unit (was applied live) ([#5](https://github.com/forkwright/epistole/issues/5)) ([871868c](https://github.com/forkwright/epistole/commit/871868cb65aa3695f9f381877516c75fe0d43040))
* **deps:** bump ammonia to 4.1.4 for RUSTSEC-2026-0213 ([#71](https://github.com/forkwright/epistole/issues/71)) ([8edb121](https://github.com/forkwright/epistole/commit/8edb1219812ddbb6b7fc304845fb477ee344a9f7))
* **deps:** clear RUSTSEC-2026-0190/-0193/-0204 + yanked crates via lockfile bumps ([#50](https://github.com/forkwright/epistole/issues/50)) ([a21bcad](https://github.com/forkwright/epistole/commit/a21bcadca4ec94fc3930e814d6ee5b80b1ccfd5e))
* **docs:** CLAUDE.md/AGENTS.md forge-vs-GitHub hosting claim is stale ([#51](https://github.com/forkwright/epistole/issues/51)) ([#55](https://github.com/forkwright/epistole/issues/55)) ([b0c987e](https://github.com/forkwright/epistole/commit/b0c987ea4dd0a4432440040884007c15a719bfc3))
* **store:** broken-symlink coverage in canonicalize_with_nonexistent ([#33](https://github.com/forkwright/epistole/issues/33)) ([#9](https://github.com/forkwright/epistole/issues/9)) ([88518b5](https://github.com/forkwright/epistole/commit/88518b56787877ac429d89a21c03fc6d3701cc0e))
* **subscribe:** stop persisting pending subscribers ([#15](https://github.com/forkwright/epistole/issues/15)) ([e4859bf](https://github.com/forkwright/epistole/commit/e4859bfd0c01920239bfc4a0fc728deb7796f268))

## Changelog
