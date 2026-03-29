/**
 * Keeper Bot
 *
 * Runs on a schedule to keep the protocol healthy:
 *
 * Every 6 hours:
 *   1. Harvest accrued lending interest from the lending contract → admin wallet
 *   2. Pipe harvested interest to staking.add_rewards() → raises sXLM exchange rate
 *   3. Bump TTL on all 5 contracts so they never expire
 *
 * Every 24 hours:
 *   4. Recalibrate the staking exchange rate (sanity check)
 *
 * The reward engine (reward-engine/index.ts) handles simulated APR-based distributions
 * independently. This keeper handles REAL yield from lending fees.
 */

import {
  rpc,
  Contract,
  Address,
  nativeToScVal,
  scValToNative,
  Keypair,
  TransactionBuilder,
  BASE_FEE,
} from "@stellar/stellar-sdk";
import { config } from "../config/index.js";
import {
  callAddRewards,
  callWithdrawFees,
  callCollectProtocolFees,
  getLpAccruedProtocolFees,
  getTreasuryBalance,
  callSetCooldownPeriod,
  callUpdateCollateralFactor,
  callUpdateBorrowRate,
  callUpdateLiquidationThreshold,
  callSetLpProtocolFeeBps,
  callSetLpMiningRate,
} from "../staking-engine/contractClient.js";
import { PrismaClient } from "@prisma/client";

const KEEPER_INTERVAL_MS = 6 * 60 * 60 * 1000;         // 6 hours
const TTL_BUMP_INTERVAL_MS = 24 * 60 * 60 * 1000;       // 24 hours
const RECALIBRATE_INTERVAL_MS = 24 * 60 * 60 * 1000;    // 24 hours
const TREASURY_RECYCLE_INTERVAL_MS = 24 * 60 * 60 * 1000;
const TREASURY_RECYCLE_THRESHOLD = BigInt(100_0000000);  // 100 XLM in stroops
const ASSET_ORACLE_INTERVAL_MS = 60_000; // 1 minute cadence for collateral oracle updates
const RATE_PRECISION = 10_000_000; // match lending contract precision

// Check for executable timelock operations every 10 minutes.
const TIMELOCK_POLL_INTERVAL_MS = 10 * 60 * 1000;

let keeperInterval: ReturnType<typeof setInterval> | null = null;
let ttlInterval: ReturnType<typeof setInterval> | null = null;
let recalibrateInterval: ReturnType<typeof setInterval> | null = null;
let treasuryRecycleInterval: ReturnType<typeof setInterval> | null = null;
let timelockInterval: ReturnType<typeof setInterval> | null = null;
let assetPriceInterval: ReturnType<typeof setInterval> | null = null;
let assetPriceUpdateRunning = false;

export class KeeperBot {
  private server: rpc.Server;
  private prisma: PrismaClient;

  constructor(prisma: PrismaClient) {
    this.server = new rpc.Server(config.stellar.rpcUrl);
    this.prisma = prisma;
  }

  async initialize(): Promise<void> {
    console.log("[KeeperBot] Initializing...");

    try {
      await this.ensureCollateralAssetsConfigured();
    } catch (err) {
      console.error("[KeeperBot] Collateral asset setup failed:", err);
    }

    // Run immediately on startup (non-blocking — don't delay server start)
    this.runHarvestCycle().catch((err) =>
      console.error("[KeeperBot] Initial harvest cycle failed:", err)
    );
    this.bumpAllContractTTLs().catch((err) =>
      console.error("[KeeperBot] Initial TTL bump failed:", err)
    );

    try {
      await this.updateCollateralAssetPrices();
    } catch (err) {
      console.error("[KeeperBot] Initial asset price update failed:", err);
    }

    assetPriceInterval = setInterval(async () => {
      try {
        await this.updateCollateralAssetPrices();
      } catch (err) {
        console.error("[KeeperBot] Asset price update failed:", err);
      }
    }, ASSET_ORACLE_INTERVAL_MS);

    // Schedule harvest cycle every 6h
    keeperInterval = setInterval(async () => {
      try {
        await this.runHarvestCycle();
      } catch (err) {
        console.error("[KeeperBot] Harvest cycle error:", err);
      }
    }, KEEPER_INTERVAL_MS);

    // Schedule TTL bumps every 24h
    ttlInterval = setInterval(async () => {
      try {
        await this.bumpAllContractTTLs();
      } catch (err) {
        console.error("[KeeperBot] TTL bump error:", err);
      }
    }, TTL_BUMP_INTERVAL_MS);

    // Schedule recalibration every 24h
    recalibrateInterval = setInterval(async () => {
      try {
        await this.recalibrateStakingRate();
      } catch (err) {
        console.error("[KeeperBot] Recalibrate error:", err);
      }
    }, RECALIBRATE_INTERVAL_MS);

    // Schedule treasury recycling every 24h
    treasuryRecycleInterval = setInterval(async () => {
      try {
        await this.recycleTreasury();
      } catch (err) {
        console.error("[KeeperBot] Treasury recycle error:", err);
      }
    }, TREASURY_RECYCLE_INTERVAL_MS);

    // Poll timelock operations every 10 minutes and auto-execute ready ones.
    timelockInterval = setInterval(async () => {
      try {
        await this.processTimelockQueue();
      } catch (err) {
        console.error("[KeeperBot] Timelock poll error:", err);
      }
    }, TIMELOCK_POLL_INTERVAL_MS);

    console.log(
      `[KeeperBot] Running — harvest every ${KEEPER_INTERVAL_MS / 3_600_000}h, ` +
      `TTL bump every ${TTL_BUMP_INTERVAL_MS / 3_600_000}h, ` +
      `timelock poll every ${TIMELOCK_POLL_INTERVAL_MS / 60_000}m`
    );
  }

  async shutdown(): Promise<void> {
    if (keeperInterval) { clearInterval(keeperInterval); keeperInterval = null; }
    if (ttlInterval) { clearInterval(ttlInterval); ttlInterval = null; }
    if (recalibrateInterval) { clearInterval(recalibrateInterval); recalibrateInterval = null; }
    if (treasuryRecycleInterval) { clearInterval(treasuryRecycleInterval); treasuryRecycleInterval = null; }
    if (timelockInterval) { clearInterval(timelockInterval); timelockInterval = null; }
    if (assetPriceInterval) { clearInterval(assetPriceInterval); assetPriceInterval = null; }
    console.log("[KeeperBot] Shut down");
  }

  // ============================================================
  // Core: harvest lending interest and pipe to staking rewards
  // ============================================================

  async runHarvestCycle(): Promise<void> {
    console.log("[KeeperBot] Starting harvest cycle...");

    // Step 1: Check how much interest has accrued on the lending contract
    const pendingInterest = await this.queryLendingAccruedInterest();

    // Log LP pool stats for transparency
    await this.logLpPoolStats();

    // Step 2: Collect LP protocol fees
    let lpProtocolFees = BigInt(0);
    try {
      const accrued = await getLpAccruedProtocolFees();
      if (accrued > BigInt(0)) {
        await callCollectProtocolFees();
        lpProtocolFees = accrued;
        console.log(`[KeeperBot] Collected ${Number(lpProtocolFees) / 1e7} XLM in LP protocol fees`);
      }
    } catch (err) {
      console.warn("[KeeperBot] LP protocol fee collection failed:", err);
    }

    // Step 3: Harvest lending interest
    let harvested = BigInt(0);
    if (pendingInterest > BigInt(0)) {
      console.log(
        `[KeeperBot] Pending interest: ${Number(pendingInterest) / 1e7} XLM`
      );
      harvested = await this.harvestLendingInterest(pendingInterest);
      if (harvested > BigInt(0)) {
        console.log(`[KeeperBot] Harvested ${Number(harvested) / 1e7} XLM from lending`);
      }
    }

    // Step 4: Pipe total yield to add_rewards
    const totalYield = harvested + lpProtocolFees;
    if (totalYield <= BigInt(0)) {
      console.log("[KeeperBot] No yield to distribute");
      return;
    }

    try {
      await callAddRewards(totalYield);
      console.log(
        `[KeeperBot] add_rewards called with ${Number(totalYield) / 1e7} XLM (lending: ${Number(harvested) / 1e7}, LP fees: ${Number(lpProtocolFees) / 1e7}) — sXLM rate will increase`
      );
    } catch (err) {
      console.error("[KeeperBot] add_rewards failed:", err);
      console.error(
        `[KeeperBot] MANUAL ACTION REQUIRED: call add_rewards with ${totalYield} stroops`
      );
    }
  }

  // ============================================================
  // Query accrued interest from lending contract
  // ============================================================

  private async queryLendingAccruedInterest(): Promise<bigint> {
    try {
      const result = await this.simulateView(
        config.contracts.lendingContractId,
        "total_accrued_interest",
        []
      );
      return result != null ? BigInt(result as string | number | bigint) : BigInt(0);
    } catch (err) {
      console.warn("[KeeperBot] Could not query accrued interest:", err);
      return BigInt(0);
    }
  }

  // ============================================================
  // Call harvest_interest() on lending contract
  // ============================================================

  private async harvestLendingInterest(pendingBefore: bigint): Promise<bigint> {
    try {
      const hash = await this.executeAdminCall(
        config.contracts.lendingContractId,
        "harvest_interest",
        []
      );
      console.log(`[KeeperBot] harvest_interest tx: ${hash}`);

      // The contract harvests min(pending, pool_balance).
      // Re-query after harvest to see how much is left; the difference is what was harvested.
      const pendingAfter = await this.queryLendingAccruedInterest();
      const harvested = pendingBefore > pendingAfter
        ? pendingBefore - pendingAfter
        : pendingBefore; // fallback if query fails

      return harvested;
    } catch (err) {
      console.error("[KeeperBot] harvest_interest failed:", err);
      return BigInt(0);
    }
  }

  // ============================================================
  // Bump TTL on all 5 contracts
  // ============================================================

  // ============================================================
  // Timelock queue processor — auto-executes ready operations
  // ============================================================

  async processTimelockQueue(): Promise<void> {
    if (!config.contracts.timelockContractId) {
      return; // Timelock not yet deployed
    }

    const readyOps = await this.prisma.timelockOperation.findMany({
      where: { status: "queued" },
      include: { proposal: true },
    });

    const now = Date.now();
    const TIMELOCK_DELAY_MS = 48 * 60 * 60 * 1000; // 48 h

    for (const op of readyOps) {
      const estimatedReadyAt = new Date(op.queuedAt.getTime() + TIMELOCK_DELAY_MS);
      if (now < estimatedReadyAt.getTime()) {
        continue; // Not yet ready
      }

      console.log(
        `[KeeperBot] Timelock op ready: ${op.opId} (proposal ${op.proposalId}, ${op.paramKey} = ${op.newValue})`
      );

      try {
        // Step 1: Execute the timelock operation on-chain.
        const opIdBytes = Buffer.from(op.opId, "hex");
        const timelockExecHash = await this.executeAdminCall(
          config.contracts.timelockContractId,
          "execute_operation",
          [
            new Address(Keypair.fromSecret(config.admin.secretKey).publicKey()).toScVal(),
            nativeToScVal(opIdBytes, { type: "bytes" }),
          ]
        );
        console.log(`[KeeperBot] timelock::execute_operation tx: ${timelockExecHash}`);

        // Step 2: Finalize governance proposal (writes param to governance contract).
        const proposalOnChainId = op.proposalId - 1; // DB is 1-indexed
        const finalizeHash = await this.executeAdminCall(
          config.contracts.governanceContractId,
          "finalize_proposal",
          [nativeToScVal(BigInt(proposalOnChainId), { type: "u64" })]
        );
        console.log(`[KeeperBot] governance::finalize_proposal tx: ${finalizeHash}`);

        // Step 3: Apply the parameter to the relevant protocol contract.
        await this.applyGovernanceParam(op.paramKey, op.newValue);

        // Step 4: Update DB.
        await this.prisma.timelockOperation.update({
          where: { id: op.id },
          data: { status: "executed", executedAt: new Date() },
        });
        await this.prisma.governanceProposal.update({
          where: { id: op.proposalId },
          data: { status: "executed", finalizedAt: new Date() },
        });

        console.log(`[KeeperBot] Proposal ${proposalOnChainId} finalized: ${op.paramKey} = ${op.newValue}`);
      } catch (err) {
        console.error(`[KeeperBot] Failed to process timelock op ${op.opId}:`, err);
      }
    }
  }

  /** Apply a governance-approved parameter change to the relevant protocol contract. */
  private async applyGovernanceParam(paramKey: string, newValue: string): Promise<void> {
    const value = parseInt(newValue, 10);
    if (isNaN(value)) {
      console.warn(`[KeeperBot] Cannot apply param "${paramKey}": invalid value`);
      return;
    }
    try {
      switch (paramKey) {
        case "cooldown_period":
          await callSetCooldownPeriod(value);
          break;
        case "collateral_factor":
          await callUpdateCollateralFactor(value);
          break;
        case "borrow_rate_bps":
          await callUpdateBorrowRate(value);
          break;
        case "liquidation_threshold":
          await callUpdateLiquidationThreshold(value);
          break;
        case "lp_protocol_fee_bps":
          await callSetLpProtocolFeeBps(value);
          break;
        case "lp_mining_rate":
          await callSetLpMiningRate(value);
          break;
        default:
          console.log(`[KeeperBot] Param "${paramKey}" is governance-only, no contract call needed`);
      }
      console.log(`[KeeperBot] Applied protocol param: ${paramKey} = ${value}`);
    } catch (err) {
      console.error(`[KeeperBot] Failed to apply param "${paramKey}":`, err);
    }
  }

  async bumpAllContractTTLs(): Promise<void> {
    const contracts = [
      { name: "sXLM Token",  id: config.contracts.sxlmTokenContractId },
      { name: "Staking",     id: config.contracts.stakingContractId },
      { name: "Lending",     id: config.contracts.lendingContractId },
      { name: "LP Pool",     id: config.contracts.lpPoolContractId },
      { name: "Governance",  id: config.contracts.governanceContractId },
      ...(config.contracts.timelockContractId
        ? [{ name: "Timelock", id: config.contracts.timelockContractId }]
        : []),
    ];

    for (const c of contracts) {
      try {
        await this.executeAdminCall(c.id, "bump_instance", []);
        console.log(`[KeeperBot] TTL bumped: ${c.name}`);
      } catch (err) {
        console.error(`[KeeperBot] TTL bump failed for ${c.name}:`, err);
        // Non-fatal: log and continue
      }
    }
  }

  // ============================================================
  // Recalibrate staking exchange rate (sanity check)
  // ============================================================

  async recalibrateStakingRate(): Promise<void> {
    try {
      await this.executeAdminCall(
        config.contracts.stakingContractId,
        "recalibrate_rate",
        []
      );
      console.log("[KeeperBot] Staking rate recalibrated");
    } catch (err) {
      console.error("[KeeperBot] Recalibrate failed:", err);
    }
  }

  // ============================================================
  // LP Pool stats logging (fees go to LPs via constant product k growth)
  // ============================================================

  private async logLpPoolStats(): Promise<void> {
    try {
      const reserves = await this.simulateView(
        config.contracts.lpPoolContractId,
        "get_reserves",
        []
      );

      const arr = reserves as [string | number | bigint, string | number | bigint] | null;
      const xlm = Number(arr?.[0] ?? 0) / 1e7;
      const sxlm = Number(arr?.[1] ?? 0) / 1e7;
      const k = xlm * sxlm;

      const accruedFees = await getLpAccruedProtocolFees().catch(() => BigInt(0));

      console.log(
        `[KeeperBot] LP Pool: reserve_xlm=${xlm.toFixed(2)}, reserve_sxlm=${sxlm.toFixed(2)}, k=${k.toFixed(2)}, accrued_protocol_fees=${Number(accruedFees) / 1e7} XLM`
      );
    } catch (err) {
      console.warn("[KeeperBot] Could not query LP pool stats:", err);
    }
  }

  // ============================================================
  // Collateral asset management helpers
  // ============================================================

  private async ensureCollateralAssetsConfigured(): Promise<void> {
    if (!config.contracts.lendingContractId || config.collateralAssets.length === 0) return;

    for (const asset of config.collateralAssets) {
      if (asset.assetId === config.contracts.sxlmTokenContractId) {
        continue;
      }

      try {
        const existing = await this.getAssetConfig(asset.assetId);
        if (existing?.enabled) {
          continue;
        }

        const initialPrice = asset.initialPrice ?? 1;
        const scaledPrice = BigInt(Math.max(1, Math.round(initialPrice * RATE_PRECISION)));

        await this.executeAdminCall(
          config.contracts.lendingContractId,
          "add_collateral_asset",
          [
            new Address(config.admin.publicKey).toScVal(),
            new Address(asset.assetId).toScVal(),
            nativeToScVal(asset.collateralFactorBps, { type: "u32" }),
            nativeToScVal(scaledPrice, { type: "i128" }),
          ]
        );
        console.log(`[KeeperBot] Registered collateral asset ${asset.symbol}`);
      } catch (err) {
        console.error(`[KeeperBot] Failed to register collateral asset ${asset.symbol}:`, err);
      }
    }
  }

  private async updateCollateralAssetPrices(): Promise<void> {
    if (assetPriceUpdateRunning || !config.contracts.lendingContractId) return;
    if (config.collateralAssets.length === 0) return;
    assetPriceUpdateRunning = true;

    try {
      for (const asset of config.collateralAssets) {
        if (asset.assetId === config.contracts.sxlmTokenContractId) {
          continue;
        }
        if (!asset.oracle) {
          continue;
        }

        try {
          const price = await this.fetchOraclePrice(asset);
          if (!price) {
            continue;
          }

          const scaledPrice = BigInt(Math.max(1, Math.round(price * RATE_PRECISION)));
          const current = await this.getAssetConfig(asset.assetId);
          if (!current?.enabled) {
            continue;
          }
          if (current.price === scaledPrice) {
            continue;
          }

          await this.executeAdminCall(
            config.contracts.lendingContractId,
            "update_asset_price",
            [
              new Address(config.admin.publicKey).toScVal(),
              new Address(asset.assetId).toScVal(),
              nativeToScVal(scaledPrice, { type: "i128" }),
            ]
          );
          console.log(`[KeeperBot] Oracle price updated: ${asset.symbol} ≈ ${price.toFixed(6)} XLM`);
        } catch (err) {
          console.error(`[KeeperBot] Price sync failed for ${asset.symbol}:`, err);
        }
      }
    } finally {
      assetPriceUpdateRunning = false;
    }
  }

  private async getAssetConfig(assetId: string): Promise<{ price: bigint; enabled: boolean } | null> {
    if (!config.contracts.lendingContractId) return null;
    const result = await this.simulateView(
      config.contracts.lendingContractId,
      "get_asset_config",
      [new Address(assetId).toScVal()]
    );
    if (!Array.isArray(result)) return null;

    const priceRaw = result[1] ?? 0;
    const enabledRaw = result[2] ?? false;
    return {
      price: BigInt(priceRaw ?? 0),
      enabled: Boolean(enabledRaw),
    };
  }

  private async fetchOraclePrice(
    asset: (typeof config.collateralAssets)[number]
  ): Promise<number | null> {
    if (!asset.oracle) return null;
    if (asset.oracle.type === "fixed") {
      return asset.oracle.price;
    }
    return this.fetchDexPrice(asset.oracle.assetCode, asset.oracle.assetIssuer, asset.symbol);
  }

  private async fetchDexPrice(assetCode: string, assetIssuer: string, label: string): Promise<number | null> {
    try {
      const orderBookUrl = new URL("order_book", config.stellar.horizonUrl);
      const assetType = assetCode.length <= 4 ? "credit_alphanum4" : "credit_alphanum12";
      orderBookUrl.searchParams.set("selling_asset_type", assetType);
      orderBookUrl.searchParams.set("selling_asset_code", assetCode);
      orderBookUrl.searchParams.set("selling_asset_issuer", assetIssuer);
      orderBookUrl.searchParams.set("buying_asset_type", "native");

      const response = await fetch(orderBookUrl.toString());
      if (!response.ok) {
        console.warn(`[KeeperBot] Order book request failed for ${label}: ${response.status}`);
        return null;
      }

      const data = (await response.json()) as {
        bids?: Array<{ price: string }>;
        asks?: Array<{ price: string }>;
      };

      const bestAsk = data.asks && data.asks[0] ? Number(data.asks[0].price) : 0;
      const bestBid = data.bids && data.bids[0] ? Number(data.bids[0].price) : 0;
      if (!bestAsk && !bestBid) {
        console.warn(`[KeeperBot] Order book had no bids/asks for ${label}`);
        return null;
      }

      const price = bestAsk && bestBid ? (bestAsk + bestBid) / 2 : bestAsk || bestBid;
      if (!price || Number.isNaN(price)) {
        return null;
      }
      return price;
    } catch (err) {
      console.error(`[KeeperBot] Order book fetch failed for ${label}:`, err);
      return null;
    }
  }

  // ============================================================
  // Treasury recycling: withdraw protocol fees and pipe back as rewards
  // ============================================================

  async recycleTreasury(): Promise<void> {
    try {
      const treasuryBal = await getTreasuryBalance();

      if (treasuryBal < TREASURY_RECYCLE_THRESHOLD) {
        console.log(
          `[KeeperBot] Treasury balance ${Number(treasuryBal) / 1e7} XLM below threshold (${Number(TREASURY_RECYCLE_THRESHOLD) / 1e7} XLM) — skipping recycle`
        );
        return;
      }

      console.log(`[KeeperBot] Recycling treasury: ${Number(treasuryBal) / 1e7} XLM`);

      await callWithdrawFees(treasuryBal);
      console.log(`[KeeperBot] Withdrew ${Number(treasuryBal) / 1e7} XLM from treasury`);

      await callAddRewards(treasuryBal);
      console.log(
        `[KeeperBot] Recycled ${Number(treasuryBal) / 1e7} XLM treasury → add_rewards — stakers get ~100% of yield`
      );
    } catch (err) {
      console.error("[KeeperBot] Treasury recycle failed:", err);
    }
  }

  // ============================================================
  // Helpers
  // ============================================================

  private async simulateView(
    contractId: string,
    method: string,
    args: ReturnType<typeof nativeToScVal>[]
  ): Promise<unknown> {
    const contract = new Contract(contractId);
    const op = contract.call(method, ...args);

    const keypair = Keypair.fromSecret(config.admin.secretKey);
    const account = await this.server.getAccount(keypair.publicKey());

    const tx = new TransactionBuilder(account, {
      fee: BASE_FEE,
      networkPassphrase: config.stellar.networkPassphrase,
    })
      .addOperation(op)
      .setTimeout(30)
      .build();

    const simResult = await this.server.simulateTransaction(tx);

    if (rpc.Api.isSimulationSuccess(simResult) && simResult.result) {
      return scValToNative(simResult.result.retval);
    }
    return null;
  }

  private async executeAdminCall(
    contractId: string,
    method: string,
    args: ReturnType<typeof nativeToScVal>[]
  ): Promise<string> {
    const keypair = Keypair.fromSecret(config.admin.secretKey);
    const account = await this.server.getAccount(keypair.publicKey());

    const contract = new Contract(contractId);
    const op = contract.call(method, ...args);

    const tx = new TransactionBuilder(account, {
      fee: BASE_FEE,
      networkPassphrase: config.stellar.networkPassphrase,
    })
      .addOperation(op)
      .setTimeout(300)
      .build();

    const preparedTx = await this.server.prepareTransaction(tx);
    preparedTx.sign(keypair);

    const result = await this.server.sendTransaction(preparedTx);
    if (result.status === "ERROR") {
      throw new Error(`${contractId}::${method} failed: ${JSON.stringify(result.errorResult)}`);
    }

    await this.pollTransaction(result.hash);
    return result.hash;
  }

  private async pollTransaction(
    hash: string,
    maxAttempts = 30,
    intervalMs = 2000
  ): Promise<void> {
    for (let attempt = 0; attempt < maxAttempts; attempt++) {
      try {
        // Use raw JSON-RPC fetch to avoid SDK XDR parse errors ("Bad union switch")
        // that occur when the SDK version is behind the current protocol version.
        const response = await fetch(config.stellar.rpcUrl, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            jsonrpc: "2.0",
            id: 1,
            method: "getTransaction",
            params: { hash },
          }),
        });

        const result = await response.json() as {
          result?: { status: string; ledger?: number; errorResultXdr?: string };
          error?: { message: string };
        };

        const status = result.result?.status;

        if (status === "SUCCESS") {
          return;
        }

        if (status === "FAILED") {
          throw new Error(`Transaction ${hash} failed: ${result.result?.errorResultXdr ?? "unknown"}`);
        }

        // NOT_FOUND or still pending — wait and retry
      } catch (err: unknown) {
        // If this is a FAILED error we re-threw above, propagate it
        if (err instanceof Error && err.message.includes("failed:")) {
          throw err;
        }
        // Otherwise (network error, parse error) log and continue polling
        console.warn(`[KeeperBot] pollTransaction attempt ${attempt + 1} error: ${err instanceof Error ? err.message : err}`);
      }

      await new Promise((resolve) => setTimeout(resolve, intervalMs));
    }

    // Timed out — treat as non-fatal for keeper operations (TTL bumps, etc.)
    console.warn(`[KeeperBot] Transaction ${hash} not confirmed after ${maxAttempts} attempts — treating as submitted`);
  }
}
