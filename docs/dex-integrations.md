# sXLM DEX Integration Guide

This guide covers everything needed to list the `sXLM/XLM` pair on a third-party DEX
(StellarX, Lumenswap, etc.) or to build custom routing on top of the Stello liquidity pool.

---

## TypeScript SDK

Install the SDK from the monorepo (or a published npm registry):

```bash
npm install @stello/dex-sdk
```

```typescript
import { StelloClient } from "@stello/dex-sdk";

const client = new StelloClient({
  apiUrl: "https://api.stello.finance/api", // production
  // apiUrl: "http://localhost:3001/api",   // local dev
});

// TWAP price (1-hour window)
const price = await client.getTwapPrice(720);
console.log(price.twapPrice); // e.g. 1.0018

// Swap quote
const quote = await client.getSwapQuote("xlm_to_sxlm", 100);
console.log(quote.amountOut, quote.priceImpactPct);

// Build and sign a swap tx
const tx = await client.buildSwapXlmToSxlm(walletAddress, 100, quote.amountOut * 0.99);
// sign tx.xdr with Freighter, then submit via Stellar RPC
```

---

## REST API

Base URL:

```
https://api.stello.finance/api
```

All endpoints are public and read-only unless otherwise noted.

---

### TWAP Oracle

```
GET /liquidity/oracle?periodLedgers=720
```

| Param | Default | Description |
|---|---|---|
| `periodLedgers` | 720 | Averaging window (720 ledgers ≈ 1 h at 5 s/ledger) |

Response:

```json
{
  "pair": "sXLM/XLM",
  "periodLedgers": 720,
  "spotPrice": 1.0021,
  "twapPrice": 1.0018,
  "priceScale": 10000000
}
```

- `spotPrice` and `twapPrice` are XLM per sXLM.
- Use `periodLedgers=8640` (≈ 12 h) for a manipulation-resistant price feed.
- The data source is the on-chain LP pool TWAP accumulator.

---

### Swap Quote

```
GET /liquidity/quote?side=xlm_to_sxlm&amount=100
GET /liquidity/quote?side=sxlm_to_xlm&amount=100
```

Response:

```json
{
  "side": "xlm_to_sxlm",
  "amountIn": 100,
  "amountInRaw": "1000000000",
  "amountOut": 99.12,
  "amountOutRaw": "991200000",
  "feeBps": 30,
  "midPrice": 0.998,
  "effectivePrice": 1.0088,
  "priceImpactPct": 1.08
}
```

- `*Raw` fields use 7-decimal stroop precision.
- Use `priceImpactPct` to warn users about large trades.
- This is for display only — not a settlement guarantee.

---

### Pool Statistics

```
GET /liquidity/pool-stats
```

Response:

```json
{
  "reserveXlm": 50000.0,
  "reserveSxlm": 49975.0,
  "totalLpSupply": 49987.5,
  "price": 1.0005,
  "feeBps": 30,
  "tvl": 99975.0
}
```

---

### LP Position

```
GET /liquidity/position/:walletAddress
```

Response:

```json
{
  "wallet": "GABC...",
  "lpTokens": 1000.0,
  "lpTokensRaw": "10000000000",
  "sharePercent": 2.0,
  "xlmShare": 999.5,
  "sxlmShare": 998.0
}
```

---

### Liquidity Mining

```
GET /liquidity/mining-stats
```

Response:

```json
{
  "miningRatePerLedger": 1000,
  "miningRatePerLedgerXlm": 0.0001,
  "totalFunded": 50000.0,
  "totalClaimed": 1234.5,
  "remainingRewards": 48765.5
}
```

```
GET /liquidity/pending-rewards/:walletAddress
```

Response:

```json
{
  "wallet": "GABC...",
  "pendingRewards": 12.345,
  "pendingRewardsRaw": "123450000"
}
```

---

### Transaction Builders

These endpoints return unsigned XDR. The caller signs with their Stellar wallet
(Freighter, Albedo, etc.) and submits directly to the Stellar RPC.

| Endpoint | Description |
|---|---|
| `POST /liquidity/add` | Add liquidity |
| `POST /liquidity/remove` | Remove liquidity |
| `POST /liquidity/swap-xlm-to-sxlm` | Swap XLM → sXLM |
| `POST /liquidity/swap-sxlm-to-xlm` | Swap sXLM → XLM |
| `POST /liquidity/claim-rewards` | Claim mining rewards |
| `POST /liquidity/fund-rewards` | Fund mining rewards pool |

All require `userAddress` (56-char Stellar public key) and relevant amount fields.

---

## On-chain LP Pool Views

External integrators can also query the LP contract directly:

| Method | Returns |
|---|---|
| `get_reserves()` | `[xlm_stroops, sxlm_stroops]` |
| `get_price()` | `i128` in stroop scale (divide by 1e7) |
| `get_twap(period_ledgers: u32)` | `i128` TWAP price |
| `total_lp_supply()` | `i128` total LP tokens |
| `get_mining_rate()` | `i128` stroops per ledger |
| `get_mining_stats()` | `[total_funded, total_claimed]` |
| `get_pending_rewards(user: Address)` | `i128` pending rewards |

---

## Liquidity Mining Program

The mining rate is a governance parameter (`lp_mining_rate`) and can be changed
through a governance proposal. To activate mining:

1. Fund the rewards pool via `POST /liquidity/fund-rewards`
2. Create a governance proposal to set `lp_mining_rate` to desired stroops/ledger
3. After the proposal passes and the 48-hour timelock elapses, the keeper auto-applies the rate

LP token holders earn rewards proportional to their pool share and can claim
at any time via `POST /liquidity/claim-rewards`.

---

## Current Limitations

- No multi-hop routing (single sXLM/XLM pair only).
- Asset metadata is minimal; integrating apps should map contract IDs to known symbols.
- The npm package is part of this monorepo; a separate published package is planned.
