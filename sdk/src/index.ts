/**
 * @stello/dex-sdk
 *
 * Public SDK for integrating the sXLM/XLM trading pair into third-party DEXes
 * (StellarX, Lumenswap, etc.).
 *
 * Quick start:
 *   import { StelloClient } from "@stello/dex-sdk";
 *   const client = new StelloClient({ apiUrl: "https://api.stello.finance/api" });
 *   const price = await client.getTwapPrice();
 *   const quote = await client.getSwapQuote("xlm_to_sxlm", 100);
 */

export type {
  OraclePrice,
  SwapQuote,
  PoolStats,
  LpPosition,
  MiningStats,
  PendingRewards,
  UnsignedTx,
  StelloClientConfig,
} from "./types.js";

import type {
  OraclePrice,
  SwapQuote,
  PoolStats,
  LpPosition,
  MiningStats,
  PendingRewards,
  UnsignedTx,
  StelloClientConfig,
} from "./types.js";

const DEFAULT_API_URL = "http://localhost:3001/api";

export class StelloClient {
  private readonly apiUrl: string;

  constructor(config: StelloClientConfig = {}) {
    this.apiUrl = (config.apiUrl ?? DEFAULT_API_URL).replace(/\/$/, "");
  }

  // ---------------------------------------------------------------------------
  // Oracle
  // ---------------------------------------------------------------------------

  /**
   * Get the current spot and TWAP price for the sXLM/XLM pair.
   *
   * @param periodLedgers Number of ledgers to average over (default 720 ≈ 1 hour).
   *   Use a larger window (e.g. 8640 ≈ 12 hours) for a manipulation-resistant price.
   */
  async getTwapPrice(periodLedgers = 720): Promise<OraclePrice> {
    return this.get<OraclePrice>(`/liquidity/oracle?periodLedgers=${periodLedgers}`);
  }

  // ---------------------------------------------------------------------------
  // Quotes & routing
  // ---------------------------------------------------------------------------

  /**
   * Get a swap quote for the sXLM/XLM pair.
   *
   * @param side  "xlm_to_sxlm" or "sxlm_to_xlm"
   * @param amount  Amount in the input token (human-readable, e.g. 100 XLM)
   *
   * The returned `amountOut` uses 7-decimal Stellar stroop precision.
   * Use `priceImpactPct` to warn users about large trades.
   */
  async getSwapQuote(
    side: "xlm_to_sxlm" | "sxlm_to_xlm",
    amount: number
  ): Promise<SwapQuote> {
    return this.get<SwapQuote>(`/liquidity/quote?side=${side}&amount=${amount}`);
  }

  // ---------------------------------------------------------------------------
  // Pool data
  // ---------------------------------------------------------------------------

  /** Get current pool reserves, TVL, and fee rate. */
  async getPoolStats(): Promise<PoolStats> {
    return this.get<PoolStats>("/liquidity/pool-stats");
  }

  /**
   * Get LP position for a given Stellar wallet address.
   *
   * Returns the LP token balance, pool share percentage, and the user's
   * proportional XLM and sXLM holdings.
   */
  async getLpPosition(walletAddress: string): Promise<LpPosition> {
    return this.get<LpPosition>(`/liquidity/position/${walletAddress}`);
  }

  // ---------------------------------------------------------------------------
  // Liquidity mining
  // ---------------------------------------------------------------------------

  /** Get global liquidity mining stats (rate, funded, claimed). */
  async getMiningStats(): Promise<MiningStats> {
    return this.get<MiningStats>("/liquidity/mining-stats");
  }

  /**
   * Get pending unclaimed mining rewards for a wallet.
   *
   * Returns the reward amount in XLM (human-readable).
   */
  async getPendingRewards(walletAddress: string): Promise<PendingRewards> {
    return this.get<PendingRewards>(`/liquidity/pending-rewards/${walletAddress}`);
  }

  // ---------------------------------------------------------------------------
  // Transaction builders (return unsigned XDR for wallet signing)
  // ---------------------------------------------------------------------------

  /**
   * Build an unsigned transaction to add liquidity.
   *
   * Sign the returned `xdr` with Freighter (or any Stellar wallet) and submit
   * via the Stellar RPC or the `/liquidity/submit` endpoint.
   */
  async buildAddLiquidity(
    userAddress: string,
    xlmAmount: number,
    sxlmAmount: number
  ): Promise<UnsignedTx> {
    return this.post<UnsignedTx>("/liquidity/add", {
      userAddress,
      xlmAmount,
      sxlmAmount,
    });
  }

  /** Build an unsigned transaction to remove liquidity. */
  async buildRemoveLiquidity(
    userAddress: string,
    lpAmount: number
  ): Promise<UnsignedTx> {
    return this.post<UnsignedTx>("/liquidity/remove", { userAddress, lpAmount });
  }

  /** Build an unsigned transaction to swap XLM for sXLM. */
  async buildSwapXlmToSxlm(
    userAddress: string,
    amount: number,
    minOut = 0
  ): Promise<UnsignedTx> {
    return this.post<UnsignedTx>("/liquidity/swap-xlm-to-sxlm", {
      userAddress,
      amount,
      minOut,
    });
  }

  /** Build an unsigned transaction to swap sXLM for XLM. */
  async buildSwapSxlmToXlm(
    userAddress: string,
    amount: number,
    minOut = 0
  ): Promise<UnsignedTx> {
    return this.post<UnsignedTx>("/liquidity/swap-sxlm-to-xlm", {
      userAddress,
      amount,
      minOut,
    });
  }

  /** Build an unsigned transaction to claim pending liquidity mining rewards. */
  async buildClaimRewards(userAddress: string): Promise<UnsignedTx> {
    return this.post<UnsignedTx>("/liquidity/claim-rewards", { userAddress });
  }

  /** Build an unsigned transaction to fund the liquidity mining rewards pool. */
  async buildFundRewards(
    userAddress: string,
    amount: number
  ): Promise<UnsignedTx> {
    return this.post<UnsignedTx>("/liquidity/fund-rewards", {
      userAddress,
      amount,
    });
  }

  // ---------------------------------------------------------------------------
  // Internal helpers
  // ---------------------------------------------------------------------------

  private async get<T>(path: string): Promise<T> {
    const res = await fetch(`${this.apiUrl}${path}`);
    const data = await res.json() as T | { error: string };
    if (!res.ok) {
      throw new Error((data as { error: string }).error ?? `HTTP ${res.status}`);
    }
    return data as T;
  }

  private async post<T>(path: string, body: unknown): Promise<T> {
    const res = await fetch(`${this.apiUrl}${path}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    const data = await res.json() as T | { error: string };
    if (!res.ok) {
      throw new Error((data as { error: string }).error ?? `HTTP ${res.status}`);
    }
    return data as T;
  }
}
