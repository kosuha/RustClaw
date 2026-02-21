# Scoring + Policy (v0 defaults)

This file contains the tunable knobs. Keep them stable; prefer changing **weights** over adding complexity.

## Regime score (0–100)

Start from 50 and add/subtract components:

- `+ index_breadth_score` (0..+20)
- `+ index_momentum_score` (0..+10)
- `+ btc_trend_score` (0..+15)
- `+ btc_momentum_score` (0..+10)
- `- btc_vol_risk` (0..-15)
- `- news_penalty` (0..-30)

Clamp to `[0, 100]`.

### Suggested component definitions (robust, simple)

- `index_breadth_score`:
  - compute share of indices above MA200D
  - map: `0% -> 0`, `50% -> 10`, `100% -> 20`

- `index_momentum_score`:
  - median 3M return across indices
  - map: strong negative -> 0, flat -> 5, strong positive -> 10

- `btc_trend_score`:
  - `+15` if BTC close > MA200D
  - `+7` if BTC close > MA50D but <= MA200D
  - `0` otherwise

- `btc_momentum_score`:
  - use 1M/3M/6M returns; cap at +10

- `btc_vol_risk`:
  - higher realized vol or ATR% -> larger penalty up to 15

## Regime bands

- `80–100`: Risk-On
- `55–79`: Neutral
- `30–54`: Risk-Off
- `0–29`: Crisis

## Cash policy (cash floor / range)

- Risk-On: cash `10–25%`
- Neutral: cash `30–55%`
- Risk-Off: cash `60–85%`
- Crisis: cash `85–95%`

## Drawdown defense clamps (MDD -20% target)

Hard clamps (defaults):
- If `BTC < MA200D` AND breadth is weak → at least **Risk-Off** behavior.
- If `high` severity `exchange_risk/security_hack/crypto_regulation` headline hits → temporary **Crisis clamp** until next weekly run (unless user overrides).

## News severity -> penalty (conservative)

Per headline (cap the total penalty each run):
- `low`: `-3`
- `med`: `-8`
- `high`: `-15` (also triggers clamp candidates)

Total `news_penalty` cap: `-30`.

### Keyword heuristics (starter set)

Use these as weak signals; prefer conservative `med` over `high` unless it’s clearly material.

- `macro_event`: `FOMC`, `Fed decision`, `CPI`, `PCE`, `jobs report`, `nonfarm payrolls`, `GDP`
- `rates`: `yields`, `Treasury`, `rate hike`, `rate cut`, `dot plot`, `tightening`, `easing`
- `usd`: `dollar surge`, `liquidity`, `funding stress`, `swap line`
- `equity_risk`: `selloff`, `risk-off`, `volatility spike`, `crash`, `circuit breaker`
- `crypto_regulation`: `SEC`, `CFTC`, `lawsuit`, `ban`, `approval`, `enforcement`
- `etf_flows`: `ETF inflows`, `ETF outflows`, `approval`, `redemption`, `issuance`
- `exchange_risk`: `withdrawal halt`, `insolvency`, `bankruptcy`, `proof of reserves`, `audit issue`
- `security_hack`: `hack`, `exploit`, `drained`, `breach`, `stolen`
- `stablecoin_risk`: `depeg`, `reserves`, `redemption pause`, `peg stability`

### Clamp triggers (starter set)

Trigger a *temporary Crisis clamp candidate* when any headline is classified as `high` severity in:
- `exchange_risk`
- `security_hack`
- `crypto_regulation` (major enforcement/action)
- `stablecoin_risk` (depeg/reserve failure)

## Weekly rebalance rule

- Run: **Monday 10:00 KST**
- Act only if target allocation differs from current by more than a **deadband**:
  - suggested: `±5%` of portfolio value (to reduce churn)
