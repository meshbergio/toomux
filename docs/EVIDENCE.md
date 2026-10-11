# The evidence behind the numbers

The README leads with one comparison: the same workload, before and with toomux.
This page says what was measured, how the two sides were made comparable, what the
result does and doesn't show, and how to measure your own.

## What was measured

Every Claude Code API call in one person's real workload: main sessions and every
subagent, across two Max 20x accounts. Usage comes from Claude Code's own
transcripts (the `usage` block on each assistant entry), not from estimates.

| | Before | With toomux | Change |
|---|---:|---:|---:|
| Calls | 33,026 | 33,026 | matched |
| Context per call | 309,783 | 147,188 | −52.5% |
| Normalized cost per 1,000 calls | $131.59 | $60.09 | −54.3% |
| Calls above 400k context | 9,194 (27.84%) | 51 (0.154%) | −99.45% rate |
| Output tokens per normalized dollar | 6,763 | 12,295 | +81.8% |

Where the difference came from:

| | Before | With toomux | Change |
|---|---:|---:|---:|
| Cache reads | 9.90B | 4.77B | −51.8% |
| 5-minute cache writes | 298M | 67.8M | −77.3% |
| Cache rewritten after expiring mid-turn | 225M | 19.4M | −91.4% |
| Cache rewritten after expiring between turns | 16.1M | 2.47M | −84.6% |
| Cache writes from normal growth | 66.1M | 61.1M | −7.7% |
| Fresh input per call | 68.7k | 67.9k | −1.2% |

## The two periods

Times are AEST (UTC+10), where the work ran.

| | From | To | Calls |
|---|---|---|---:|
| Before | 2026-09-25, about 14:40 | 2026-09-29, 22:18 | 33,026 |
| With toomux | 2026-09-30, about 00:05 | 2026-10-01, about 22:45 | 33,026 |

**How each side was chosen: by count, back from a fixed point.**

- **Before** is the 33,026 calls immediately preceding toomux's first handover
  (22:18 on 29 September, the first entry in toomux's handover log). No handover
  had happened yet.
- **With toomux** is the 33,026 most recent calls when the comparison was made, on
  2 October.
- The roughly 1,500 calls between the first handover and the start of the "with"
  side belong to neither. Nothing was picked or dropped by hand.

The two periods sit a day apart: the same person, projects and habits. The "with"
side covers fewer days because more calls ran per day that week (about 17,000 a day
against about 7,700). Counting calls rather than days keeps the two sides the same
size despite that.

## How the two sides were made comparable

- **Matched, not scaled.** Exactly 33,026 calls on each side. Nothing is
  extrapolated from a smaller sample to a larger one.
- **Same prices on both sides.** Every call on both sides is repriced at the same
  Claude Opus 5.5 list rates (per million tokens: input $4, output $20, cache read
  $0.20, 5-minute cache write $5, 1-hour cache write $8), so a change in which models
  were used gets no credit for the difference. "Normalized cost" means that repriced figure. It is a
  translation into API prices, not an invoice: the workload ran on subscription
  plans.
- **Everything counted.** Main sessions and every subagent call are included on
  both sides.

## What it shows, and what it doesn't

It shows the change where the mechanism predicts it: far less old context carried
from call to call (cache reads and cache rewrites fell by half or more) while fresh
input stayed flat. The work each call did didn't shrink; the baggage did.

Every figure is per API call, not per task. The comparison doesn't measure what
it cost to finish a given piece of work: a handover adds calls of its own (the
brief, the fresh session reading it in), and those calls are counted on the "with"
side like any other.

It doesn't prove handover is the only cause. It is one person's workload, not a
controlled benchmark across Claude Code users, and the two periods are different
weeks of different work. Your numbers will differ with how long your sessions run,
how often you step away (which lets the cache expire), and how much you use
subagents.

## Reproducing it

The figures come from `toomux tokens`, run over each window: a report since the
window's start, minus a report since its end.

```sh
python3 - <<'PY'
import json, subprocess
RATES = dict(read=0.20, write_5m=5.0, write_1h=8.0, input=4.0, output=20.0)  # $/M, Opus 5.5
KEYS = ["calls", "context", "over_limit", "output", *RATES]
def since(t):
    d = json.loads(subprocess.run(["toomux", "tokens", "--since", t, "--json"],
                                  capture_output=True, text=True, check=True).stdout)
    return {k: d["main"][k] + d["subagents"][k] for k in KEYS}
def window(start, end):
    a, b = since(start), since(end)
    w = {k: a[k] - b[k] for k in KEYS}
    usd = sum(w[k] * r for k, r in RATES.items()) / 1e6
    print(f"{start} -> {end}: {w['calls']:,} calls, {w['context'] / w['calls']:,.0f} context/call, "
          f"${usd / w['calls'] * 1000:.2f} per 1,000 calls, {w['over_limit']:,} above 400k, "
          f"{w['output'] / usd:,.0f} output tokens per $")
window("2026-09-25 14:40", "2026-09-29 22:18")  # before
window("2026-09-30 00:05", "2026-10-02 00:00")  # with toomux
PY
```

Rerun against the same machine on 11 October, it gives $131.58 and $60.09 per 1,000
calls, 309,829 and 147,170 context per call, 9,204 and 51 calls above 400k, and
6,763 and 12,295 output tokens per dollar. That is within 0.1% of the published
figures, and identical on the "with" side's cost, 400k count and output per dollar.
The small drift is transcripts that were archived or moved between accounts after
the comparison was first run (a rerun can move by a few calls for the same reason).
Window edges are at minute resolution, so a side lands within a dozen calls of
33,026 rather than exactly on it.

## Measure your own

`toomux tokens` reads your own Claude Code transcripts and reports where the tokens
went: by kind (fresh input, cache reads, cache writes by reason, output), by session
and subagent, and priced at list rates.

```sh
toomux tokens --since 7d
toomux tokens --since 7d --json   # for your own analysis
```

Run it before installing toomux's hooks for a baseline, and again after a week of
use. Observed usage and estimated savings are reported separately and never added
together: what toomux avoided carrying forward is an estimate, what your sessions
used is not.
