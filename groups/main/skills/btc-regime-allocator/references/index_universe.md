# Index Universe (Global)

Purpose: a **small, stable global basket** to infer risk regime via **breadth/trend/momentum**.

Constraints (this group):
- Prefer **index series** over ETFs.
- Use **free sources**: try **Yahoo Finance first**, then **Stooq** fallback.
- If an index is not realistically accessible, allow a **transparent proxy** (document it) rather than dropping global coverage.

## Recommended basket (v0)

- **US**: S&P 500, Nasdaq 100/Composite
- **EU**: STOXX Europe (600 preferred; otherwise a major pan-EU index)
- **UK**: FTSE 100
- **JP**: Nikkei 225
- **CN**: CSI 300 (or Shanghai Composite if needed)
- **HK**: Hang Seng
- **KR**: KOSPI
- **Global / EM**: MSCI World/ACWI + MSCI EM (may require proxies)

## Symbol mapping approach

Do not hardcode a single symbol and assume it works.

1) Maintain a **candidate list** per index (Yahoo symbols vary).
2) Fetch (daily) and accept the first candidate that returns a valid series (e.g., `>= 300` rows).
3) If all Yahoo candidates fail, try Stooq (search by name; Stooq code formats vary).
4) If both fail for “Global / EM”, allow a proxy and record it in the weekly report.

## Candidate symbols (fill/validate during implementation)

Use this as a starting point; verify availability programmatically.

## Stooq fallback (concrete)

Stooq provides simple CSV endpoints (no key):
- Daily CSV: `https://stooq.com/q/d/l/?s=<symbol>&i=d`
- Weekly CSV: `https://stooq.com/q/d/l/?s=<symbol>&i=w`

Verified in this environment (v0 examples):
- S&P 500 proxy index: `^spx`
- Nasdaq Composite proxy index: `^ndq`
- Germany DAX: `^dax`
- UK (FTSE 100 proxy): `^ukx`
- Hang Seng: `^hsi`
- KOSPI: `^kospi`
- Shanghai Composite: `^shc`

If Stooq doesn’t have a target index (common), use Yahoo candidates or a transparent proxy.

### US
- S&P 500: Yahoo candidates: `^GSPC`
- Nasdaq: Yahoo candidates: `^IXIC` (Composite), `^NDX` (Nasdaq 100)

### Europe / UK
- Germany DAX: Yahoo candidates: `^GDAXI`
- Euro area / pan-Europe: Yahoo candidates vary (verify); examples include `^STOXX50E` (Euro Stoxx 50)
- UK FTSE 100: Yahoo candidates: `^FTSE`

### Asia
- Japan Nikkei 225: Yahoo candidates: `^N225`
- China CSI 300: Yahoo candidates: `000300.SS` (verify) / alternatives if unavailable
- Shanghai Composite: Yahoo candidates: `000001.SS` (verify)
- Hong Kong Hang Seng: Yahoo candidates: `^HSI`
- Korea KOSPI: Yahoo candidates: `^KS11`

### Global / Emerging (often hard as true indices)
- MSCI World / ACWI / EM: may not be available as free index series everywhere.
  - If missing, allow proxy ETFs (only for these): `ACWI` (global), `EEM` (EM) and mark them as proxies.
