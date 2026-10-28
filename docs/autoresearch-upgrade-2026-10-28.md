# Autoresearch upgrade — 2026-10-28

This change updates the local jsh controller and jagent validator together.
Existing frozen campaigns, author-input V1/V2 and edit-plan V1 keep their wire
formats. New model configuration is optional. No campaign is reinitialized.

## Experiment reliability and memory

- Bridge context, reports and pre-execution incumbent checks use one evidence
  projector. A stage needs its exact ordered integer seeds and valid numeric
  metrics/guardrails. Partial/mismatched evidence never becomes an average, and
  confirmation is never exposed without screening.
- The incumbent's scored record remains in the 16-trial model window, alongside
  the latest 15 trials when it would otherwise disappear. Omitted counts remain
  accurate; the context and hypothesis byte bounds are unchanged.
- Stop/deadline checks cover the interval between checkout and process launch,
  result validation, and the final selection boundary. Invalid incumbent evidence
  is rejected before charging or launching another trial.
- jagent checks score/status consistency, stage-specific failure categories,
  distinct feedback trial IDs and admission before bound observations.

## Model integration and reports

New auto configurations can specify `sampling` and `token_limit_field`. Sampling
accepts bounded temperature/top_p/top_k/seed values; null omits a parameter from
the HTTP body. The token field can be `max_tokens` or `max_completion_tokens`,
both using the same persistent output reservation. Older configurations retain
their original request defaults. See [model setup](autoresearch-auto.md).

`auto-report` now exposes paired-seed comparisons against each trial's actual
historical incumbent: sample count, candidate/reference means, signed improvement,
min/max improvement, improved-seed count and the strict threshold result.
Missing stages remain absent. These descriptive values do not change promotion
or claim statistical significance. See [report semantics](autoresearch-observe.md).

## Source-read performance

One `git ls-tree -l -z` query checks paths, regular-file modes and all blob sizes;
one `git cat-file --batch` retrieves the selected content. Size validation happens
before blob reads. Batch object IDs, lengths, separators and UTF-8 are checked,
and source order and exact line endings are preserved.

Local benchmark: eight files, 20,576 bytes, four warmup iterations and 20 measured
iterations per implementation, with alternating order:

| Source snapshot | Before | After |
| --- | ---: | ---: |
| Git processes | 24 | 2 |
| Median elapsed | 124.202 ms | 11.847 ms |

This is approximately 10.5× for source reading on this host, not model inference
or total research throughput. The script first checks that both implementations
produce identical snapshots. [Recorded result](performance/research_sources_2026-10-28.json).

```sh
python3 scripts/bench_research_sources.py --before-ref 4ca2639 --samples 20
```

## Validation and dependency integration

Tests cover malformed/duplicate/reordered seeds, guardrails, incumbent retention,
stop during preparation/validation/selection, source bounds and batch integrity,
historical comparisons, strict model endpoints, and old frozen configurations.
The HTTP fixtures are local deterministic servers, not live Flint measurements.

Validation completed on this host:

- jagent: 446 tests passed; research suites also pass on Rust 1.86.0.
- jsh: core-only and AI-enabled research CLI/bridge/model-loop integration passed.
- All 18 new Python regression cases pass against the compiled binary, including
  legacy ledger preflight and strict model-parameter handling. Together with five
  new jagent cases, this adds 23 regression tests.
- Formatting and strict all-target Clippy pass in both repositories; jsh Clippy
  is checked with all features and the local jagent override.

```sh
# jsh
python3 -m unittest discover -s tests -p 'test_research*.py' -q
cargo test --locked --no-default-features --test research_cli --test research_bridge --test research_auto

# jagent
cargo test --locked
cargo +1.86.0 test --locked --test research --test research_author --test research_feedback --test research_protocol_version
cargo clippy --locked --all-targets -- -D warnings
```

The release dependency is pinned to jagent `9fd9eab99ab21a1d88ac786b063ce40508429c9f`, published through the GitHub Git Data API. The earlier local-override validation is followed by a normal locked build against that published revision. No path patch is required for consumers.
