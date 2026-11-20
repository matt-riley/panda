# Synthetic compaction comparison

Both strategies in each pair use the same generated Pi branch. The default uses an HTTP Jev fixture. The provider baseline exercises Panda’s existing summarizer with Jev unconfigured. All requests go to loopback. The keyless pair has no Jev credential on either path. These observations describe the scripted services.

The fixtures port the upstream cases for real read ranges, repeated handoffs, oversized handoff rejection, unavailable judging and mid-run cancellation. The adjacent provider regressions also check masking before tool scoring truncation. [Pinned safe-compact tests](https://github.com/matt-riley/pi-extensions/blob/2b31ab39412dd30971019107d23e6ac4c9056744/packages/safe-compact/test/safe-compact.test.mjs) · [Rust scoring transport tests](../crates/providers/tests/jev.rs)

The independent oracle checks two standing instructions verbatim, one exact error, and each file path with its actual read range. It counts a contradictory operating-system claim and an unsupported ownership claim independently. The provider fixture deliberately emits both calibration claims when the source contains the checked fact. File lists alone do not count as recoverable line pointers. The cancellation fixture establishes a prior checkpoint, then cancels a second compaction. The native strategy completes one priced judgment before its next request is cancelled. The baseline cancels its single provider request. It verifies the original checkpoint and transcript survive. A cancelled checkpoint conveys no new claims. Bytes measure the stored UTF-8 summary. Requests include failed calls and exclude setup and ordinary turn calls. Retry counts identify scheduled repeat requests. Latency measures local elapsed time across the fixture’s compaction workflow. Cost uses synthetic USD reported by the loopback services, with no external charges. A dash means the client received no priced response.

| Fixture | Strategy | Instructions / 2 | Contradicted | Uncheckable | Exact error / 1 | Pointers | Bytes | Requests | Retries | Latency ms | Reported USD |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| representative | provider | 1 | 1 | 1 | 1 | 0/3 | 107 | 1 | 0 | 51 | 0.000010 |
| representative | default | 2 | 0 | 0 | 1 | 3/3 | 993 | 11 | 0 | 391 | 0.000011 |
| tool-heavy | provider | 1 | 1 | 1 | 1 | 0/24 | 107 | 2 | 1 | 72 | 0.000010 |
| tool-heavy | default | 2 | 0 | 0 | 1 | 24/24 | 2390 | 53 | 0 | 745 | 0.000053 |
| repeated | provider | 1 | 1 | 1 | 1 | 0/3 | 107 | 2 | 0 | 171 | 0.000020 |
| repeated | default | 2 | 0 | 0 | 1 | 3/3 | 1894 | 20 | 0 | 682 | 0.000020 |
| keyless | provider | 1 | 1 | 1 | 1 | 0/3 | 107 | 1 | 0 | 38 | 0.000010 |
| keyless | default | 1 | 1 | 1 | 1 | 0/3 | 107 | 1 | 0 | 41 | 0.000010 |
| unavailable | provider | 1 | 1 | 1 | 1 | 0/3 | 107 | 1 | 0 | 37 | 0.000010 |
| unavailable | default | 1 | 1 | 1 | 1 | 0/3 | 107 | 10 | 0 | 120 | 0.000010 |
| rejected | provider | 1 | 1 | 1 | 1 | 0/3 | 107 | 2 | 0 | 45 | 0.000020 |
| rejected | default | 1 | 1 | 1 | 1 | 0/3 | 107 | 15 | 0 | 398 | 0.000033 |
| cancelled | provider | 0 | 0 | 0 | 0 | 0/3 | 0 | 1 | 0 | 91 | — |
| cancelled | default | 0 | 0 | 0 | 0 | 0/3 | 0 | 2 | 0 | 133 | 0.000001 |

Reproduce with `cargo build --locked -p panda-cli --bin panda` followed by `python3 crates/cli/tests/compaction_comparison.py target/debug/panda`. Pass `--report docs/compaction-comparison.md` to refresh this report. Wall-clock measurements vary by host.
