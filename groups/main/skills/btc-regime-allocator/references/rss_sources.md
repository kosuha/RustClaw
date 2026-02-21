# RSS / Headline Sources (English + KR)

Goal: keep a diversified headline stream to apply **conservative penalties/filters** (not timing).

Implementation note:
- Prefer sources with stable RSS/Atom feeds.
- Store `(name, feed_url, default_categories, weight)` and validate periodically.
- If a feed disappears, replace it—do not silently drop the category.

## Categories (for classification)

- `macro_event`: FOMC, CPI, PCE, Jobs, GDP, central bank decisions
- `rates`: yields, policy rate guidance
- `usd`: dollar strength/liquidity stress
- `equity_risk`: broad risk-on/off tone in global equities
- `crypto_regulation`: legislation, enforcement, court rulings
- `etf_flows`: ETF approvals/flows
- `exchange_risk`: CEX issues (halts/withdrawals/insolvency)
- `security_hack`: hacks/exploits/incidents
- `stablecoin_risk`: depegs/reserve issues

## Verified feed list (v0)

All `feed_url` entries below were verified to return RSS/Atom successfully in this environment.

### Macro / official

| name | feed_url | default_categories | weight | notes |
|---|---|---:|---:|---|
| Fed press releases | `https://www.federalreserve.gov/feeds/press_all.xml` | `macro_event,rates` | 1.0 | policy surprises / guidance |
| Fed speeches | `https://www.federalreserve.gov/feeds/speeches.xml` | `macro_event,rates` | 0.8 | slower-moving, but useful |
| BLS major indicators | `https://www.bls.gov/feed/bls_latest.rss` | `macro_event` | 1.0 | CPI/jobs context |
| BEA updates | `https://www.bea.gov/rss/rss.xml` | `macro_event` | 0.8 | GDP/PCE context |
| ECB press | `https://www.ecb.europa.eu/rss/press.html` | `macro_event,rates` | 0.8 | EU policy risk |
| Bank of England news | `https://www.bankofengland.co.uk/rss/news` | `macro_event,rates` | 0.6 | UK policy risk |
| SEC press releases | `https://www.sec.gov/news/pressreleases.rss` | `crypto_regulation` | 0.7 | enforcement headlines |

### Markets (broad / sentiment)

| name | feed_url | default_categories | weight | notes |
|---|---|---:|---:|---|
| CNBC top news | `https://www.cnbc.com/id/100003114/device/rss/rss.html` | `equity_risk,rates,usd` | 0.6 | high volume; use as sentiment only |
| Investing.com world news | `https://www.investing.com/rss/news_25.rss` | `equity_risk,usd,rates` | 0.5 | noisy; keep low weight |

### Crypto (news / narrative)

| name | feed_url | default_categories | weight | notes |
|---|---|---:|---:|---|
| CoinDesk | `https://www.coindesk.com/arc/outboundfeeds/rss/` | `etf_flows,crypto_regulation,exchange_risk,stablecoin_risk` | 0.7 | general crypto news |
| Decrypt | `https://decrypt.co/feed` | `etf_flows,crypto_regulation` | 0.5 | narrative, medium noise |
| Cointelegraph | `https://cointelegraph.com/rss` | `etf_flows,crypto_regulation` | 0.3 | noisy; keep but down-weight |
| Investing.com crypto | `https://www.investing.com/rss/news_301.rss` | `etf_flows` | 0.3 | very noisy; low weight |

### Crypto (risk / security / institutions)

| name | feed_url | default_categories | weight | notes |
|---|---|---:|---:|---|
| Chainalysis blog | `https://www.chainalysis.com/blog/feed/` | `security_hack,exchange_risk,stablecoin_risk` | 0.7 | slower but higher signal |
| Kraken blog | `https://blog.kraken.com/feed` | `exchange_risk` | 0.4 | exchange notices/insights |
| Chainlink blog | `https://blog.chain.link/rss/` | `security_hack` | 0.3 | ecosystem/security notes |

## How to use without scripts

When running a weekly review manually:
- Read only the **top N** (e.g., 30–80) headlines per week.
- Penalize only **clearly material** items (prefer `med` over `high`).
- Record the top 3 headlines you used for penalties so the decision is auditable.

