# DeepSeek Harness Data Source

ccusage reads DeepSeek Harness (`dsh`) session logs and includes their token
usage in unified reports. Focused DSH reports support daily, monthly, and
session views.

```bash
ccusage dsh daily
ccusage dsh monthly
ccusage dsh session
```

## Data Source

The adapter scans `${DSH_HOME:-~/.dsh}/sessions/`. `DSH_HOME` may contain a
comma-separated list of roots.

DSH keeps immutable session generations during format migrations:

```text
session.jsonl.zstd       # v0
session.v1.jsonl.zstd
session.v2.jsonl.zstd
session.v3.jsonl.zstd
```

Plain `.jsonl` variants are also supported. ccusage groups files by session
directory and reads only the highest generation, matching DSH's selection
rule. It currently supports session formats v0 through v3. If the newest file
for a session uses a later format, that session is skipped instead of falling
back to stale data.

## Token Accounting

One report entry represents one settled model request attempt. Within an
attempt, the final `assistant/message` usage replaces the stream usage; it is
not counted twice. Retried requests that report usage remain separate attempts,
so provider-billed retry tokens are preserved.

The fields map as follows:

| DSH field          | ccusage bucket       |
| ------------------ | -------------------- |
| `inputTokens`      | Uncached input       |
| `cacheReadTokens`  | Cache read input     |
| `cacheWriteTokens` | Cache creation input |
| `outputTokens`     | Output               |

`reasoningTokens` is a subset of output and is not added again. `totalTokens`
is used to validate a usage sample, not as another token bucket.

## Cost Calculation

DSH does not store a precomputed USD cost. Consequently, `auto` and
`calculate` estimate cost from the recorded model and ccusage pricing, while
`display` reports zero cost.

Provider-specific aliases such as an internal gateway model may not exist in
the built-in pricing data. Configure a `pricingOverrides` entry keyed by the raw
model name when that happens:

```json
{
	"dsh": {
		"defaults": {
			"pricingOverrides": {
				"internal-deepseek-model": {
					"inputCostPerToken": 0.000001,
					"outputCostPerToken": 0.000002
				}
			}
		}
	}
}
```

## Related Guides

- [All Sources](/guide/all-reports)
- [Cost Modes](/guide/cost-modes)
- [Configuration Files](/guide/config-files)
- [Environment Variables](/guide/environment-variables)
