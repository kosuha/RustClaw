---
name: btc-regime-allocator
description: Design and run a BTC spot regime-based allocation strategy (weekly rebalance) that uses global index breadth + BTC trend/volatility + RSS/news classification to choose risk-on/neutral/risk-off behavior and a cash floor, targeting max drawdown around -20%.
---

# BTC Regime Allocator (Spot, Weekly)

Use this skill when the user wants a **“situation-adaptive” BTC spot strategy** (not one fixed setup): analyze **market regime** using **BTC (daily/weekly)** + **global equity indices** + **macro/news** and output **what to do this week**, especially **cash floor** and **allowed actions**.

## Defaults (from this group)

- **Asset / venue**: BTC **spot only** (no leverage/derivatives).
- **Cadence**: **weekly rebalance**; recommended run time **Monday 10:00 KST**.
- **Goal**: **drawdown defense**, target max drawdown roughly **-20%** (policy-driven, not guaranteed).
- **Data**: free sources; **Yahoo Finance first**, **Stooq fallback**.
- **News**: **RSS/headline ingestion + classification**, English included.

## Output (what you should produce)

1) **Regime**: `Risk-On | Neutral | Risk-Off | Crisis` + a **score** (0–100).
2) **Why**: 3–6 bullets: index breadth/trend, BTC trend/vol, key headlines.
3) **Policy**:
   - **Cash floor / range** (e.g., `cash >= 60%`).
   - **Allowed actions** this week: `buy`, `rebalance-only`, `no new buys`, `risk-reduction`.
4) **Weekly plan**: concrete “if/then” checklist for the next 7 days.

If the user also wants execution on Upbit, provide **an execution plan** first; do **not** place real orders without an explicit confirmation phrase from the user.

## Workflow (weekly run)

### 1) Pull data

- BTC: daily + weekly candles (at least 1–2 years daily; 3–5 years weekly).
- Global indices basket (see `references/index_universe.md`).
- Optional macro proxies (if available): USD (DXY proxy), rates proxy, volatility proxy.

If a requested index is not accessible as an index series, allow a **transparent proxy** (document it) and keep the rest index-based.

### 2) Compute signals (simple, robust)

Compute each of the following:

- **BTC trend**: price vs `MA200D` (and `MA50D` as secondary).
- **BTC momentum**: 1M / 3M / 6M total return (daily closes).
- **BTC volatility**: ATR(14) or realized vol; translate to a “risk” score.
- **Index breadth**: fraction of indices in uptrend (e.g., above MA200D) and median momentum.

### 3) Ingest & classify news (RSS/headlines)

- Pull RSS items from the source list in `references/rss_sources.md`.
- Classify each headline into categories such as:
  - `macro_event` (FOMC/CPI/jobs), `rates`, `usd`, `equity_risk`
  - `crypto_regulation`, `etf_flows`, `exchange_risk`, `security_hack`, `stablecoin_risk`
- Assign **severity** (`low/med/high`) and produce a **news penalty**:
  - Penalty reduces the regime score and/or clamps max exposure for the week.

Keep this conservative: news is a **filter/penalty**, not a precise timing tool.

### 4) Combine to a regime score (0–100)

Use a weighted sum that can be tuned later, but start with stable defaults:

- `index_breadth` + `index_momentum`
- `btc_trend` + `btc_momentum`
- subtract `btc_vol_risk`
- subtract `news_penalty`

Then map score to regime (default bands):
- `80–100`: Risk-On
- `55–79`: Neutral
- `30–54`: Risk-Off
- `0–29`: Crisis

### 5) Apply policy (cash floor + allowed actions)

Set a **cash floor** based on regime (defaults; tune later):
- Risk-On: cash `10–25%`
- Neutral: cash `30–55%`
- Risk-Off: cash `60–85%`
- Crisis: cash `85–95%`

Then add clamps for drawdown defense (MDD -20% target):
- If BTC is below MA200D **and** index breadth is poor → bump cash floor + tighten “no new buys”.
- If high-severity news triggers (exchange/security/regulation) → temporary clamp to `Risk-Off/Crisis` behavior until next weekly run (or manual override).

### 6) Produce weekly report + action checklist

Always include:
- The **single most important rule** for the week (e.g., “no new buys until trend recovers”).
- Rebalance instructions (if applicable): what to reduce/increase, and the max buy budget.

## When data fails

- If index data is missing: fall back to available regions + document coverage gap.
- If RSS fetch fails: run without news penalty and mark “news unavailable”.
- Never hallucinate numeric values; output “unknown” and proceed with partial signals.

## References

- Index basket + symbol mapping: `references/index_universe.md`
- RSS source list + category mapping: `references/rss_sources.md`
- Scoring + policy knobs: `references/scoring_policy.md`
- Output format conventions: `references/output_format.md`
