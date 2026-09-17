# ccusage-adapter-dsh

The DeepSeek Harness (`dsh`) adapter turns DSH session logs into shared ccusage
entries and focused daily, monthly, and session reports.

## Data source

- `${DSH_HOME:-~/.dsh}/sessions/**/session.jsonl[.zstd]` (v0)
- `${DSH_HOME:-~/.dsh}/sessions/**/session.vN.jsonl[.zstd]` (v1-v3)

DSH preserves older physical generations after migration. The adapter groups
files by session directory and reads only the highest generation, matching DSH's
own selection rule. A session whose newest generation is newer than this adapter
supports is skipped rather than read from a stale predecessor.

## Accounting

Each settled request attempt becomes one usage entry. A final
`assistant/message` replaces the stream usage for that attempt; retries with
their own usage remain separate billed attempts. Cache read and write counters
are disjoint input buckets. `reasoningTokens` is an output subset and
`totalTokens` is validation metadata, so neither is added again.

DSH does not persist a precomputed USD amount. `auto` and `calculate` therefore
use model pricing, while `display` reports zero cost. Private provider aliases
usually need a `pricingOverrides` entry.
