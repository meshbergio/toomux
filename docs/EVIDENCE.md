# The evidence behind the numbers

The README leads with one comparison: the same workload, before and with toomux.
This page says what was measured, how the two sides were made comparable, what the
result does and doesn't show, and how to measure your own.

<!-- TODO(maintainer): add the date ranges of the "before" and "with toomux" periods
     and how the 33,026 calls on each side were chosen (first N calls of each period,
     a random sample, or another rule). -->

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

## How the two sides were made comparable

- **Matched, not scaled.** Exactly 33,026 calls on each side. Nothing is
  extrapolated from a smaller sample to a larger one.
- **Same prices on both sides.** Every call on both sides is repriced at the same
  Claude Opus 5.5 list rates, so a change in which models were used gets no credit
  for the difference. "Normalized cost" means that repriced figure. It is a
  translation into API prices, not an invoice: the workload ran on subscription
  plans.
- **Everything counted.** Main sessions and every subagent call are included on
  both sides.

## What it shows, and what it doesn't

It shows the change where the mechanism predicts it: far less old context carried
from call to call (cache reads and cache rewrites fell by half or more) while fresh
input stayed flat. The work each call did didn't shrink; the baggage did.

It doesn't prove handover is the only cause. It is one person's workload, not a
controlled benchmark across Claude Code users, and the two periods are different
weeks of different work. Your numbers will differ with how long your sessions run,
how often you step away (which lets the cache expire), and how much you use
subagents.

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
