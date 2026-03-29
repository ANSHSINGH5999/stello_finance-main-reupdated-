export interface OraclePrice {
  pair: string;
  periodLedgers: number;
  spotPrice: number;
  twapPrice: number;
  priceScale: number;
}

export interface SwapQuote {
  side: "xlm_to_sxlm" | "sxlm_to_xlm";
  amountIn: number;
  amountInRaw: string;
  amountOut: number;
  amountOutRaw: string;
  feeBps: number;
  midPrice: number;
  effectivePrice: number;
  priceImpactPct: number;
}

export interface PoolStats {
  reserveXlm: number;
  reserveSxlm: number;
  totalLpSupply: number;
  price: number;
  feeBps: number;
  tvl: number;
}

export interface LpPosition {
  wallet: string;
  lpTokens: number;
  lpTokensRaw: string;
  sharePercent: number;
  xlmShare: number;
  sxlmShare: number;
}

export interface MiningStats {
  miningRatePerLedger: number;
  miningRatePerLedgerXlm: number;
  totalFunded: number;
  totalClaimed: number;
  remainingRewards: number;
}

export interface PendingRewards {
  wallet: string;
  pendingRewards: number;
  pendingRewardsRaw: string;
}

export interface UnsignedTx {
  xdr: string;
  networkPassphrase: string;
}

export interface StelloClientConfig {
  /** Base URL of the Stello API. Defaults to http://localhost:3001/api */
  apiUrl?: string;
}
