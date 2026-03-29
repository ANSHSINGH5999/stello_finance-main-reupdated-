import { FastifyPluginAsync } from "fastify";
import { z } from "zod";
import {
  rpc,
  Contract,
  Address,
  nativeToScVal,
  scValToNative,
  TransactionBuilder,
  BASE_FEE,
} from "@stellar/stellar-sdk";
import { config } from "../../config/index.js";
import { PrismaClient } from "@prisma/client";

const addLiquiditySchema = z.object({
  userAddress: z.string().min(56).max(56),
  xlmAmount: z.number().positive(),
  sxlmAmount: z.number().positive(),
});

const removeLiquiditySchema = z.object({
  userAddress: z.string().min(56).max(56),
  lpAmount: z.number().positive(),
});

const swapSchema = z.object({
  userAddress: z.string().min(56).max(56),
  amount: z.number().positive(),
  minOut: z.number().min(0).default(0),
});

const quoteQuerySchema = z.object({
  side: z.enum(["xlm_to_sxlm", "sxlm_to_xlm"]),
  amount: z.coerce.number().positive(),
});

const oracleQuerySchema = z.object({
  periodLedgers: z.coerce.number().int().positive().max(100_000).default(720),
});

async function buildContractTx(
  server: rpc.Server,
  contractId: string,
  method: string,
  args: any[],
  userAddress: string
) {
  const contract = new Contract(contractId);
  const op = contract.call(method, ...args);

  const account = await server.getAccount(userAddress);
  const tx = new TransactionBuilder(account, {
    fee: BASE_FEE,
    networkPassphrase: config.stellar.networkPassphrase,
  })
    .addOperation(op)
    .setTimeout(300)
    .build();

  const simResult = await server.simulateTransaction(tx);

  if (rpc.Api.isSimulationError(simResult)) {
    throw new Error(`Simulation failed: ${simResult.error}`);
  }

  const preparedTx = rpc.assembleTransaction(tx, simResult).build();
  return {
    xdr: preparedTx.toXDR(),
    networkPassphrase: config.stellar.networkPassphrase,
  };
}

async function queryContractView(
  server: rpc.Server,
  contractId: string,
  method: string,
  args: any[]
) {
  const contract = new Contract(contractId);
  const op = contract.call(method, ...args);

  const account = await server.getAccount(config.admin.publicKey);
  const tx = new TransactionBuilder(account, {
    fee: BASE_FEE,
    networkPassphrase: config.stellar.networkPassphrase,
  })
    .addOperation(op)
    .setTimeout(30)
    .build();

  const simResult = await server.simulateTransaction(tx);

  if (rpc.Api.isSimulationSuccess(simResult) && simResult.result) {
    return scValToNative(simResult.result.retval);
  }
  return null;
}

export const liquidityRoutes: FastifyPluginAsync<{ prisma: PrismaClient }> = async (
  fastify,
  opts
) => {
  const { prisma } = opts;
  const server = new rpc.Server(config.stellar.rpcUrl);
  const lpContractId = config.contracts.lpPoolContractId;

  /**
   * POST /liquidity/add
   * Build unsigned tx: add liquidity to the sXLM/XLM pool.
   */
  fastify.post("/liquidity/add", async (request, reply) => {
    try {
      const body = addLiquiditySchema.parse(request.body);
      const xlmStroops = BigInt(Math.floor(body.xlmAmount * 1e7));
      const sxlmStroops = BigInt(Math.floor(body.sxlmAmount * 1e7));

      const result = await buildContractTx(
        server,
        lpContractId,
        "add_liquidity",
        [
          new Address(body.userAddress).toScVal(),
          nativeToScVal(xlmStroops, { type: "i128" }),
          nativeToScVal(sxlmStroops, { type: "i128" }),
        ],
        body.userAddress
      );

      return result;
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "Add liquidity failed";
      reply.status(400).send({ error: message });
    }
  });

  /**
   * POST /liquidity/remove
   * Build unsigned tx: remove liquidity from the pool.
   */
  fastify.post("/liquidity/remove", async (request, reply) => {
    try {
      const body = removeLiquiditySchema.parse(request.body);
      const lpStroops = BigInt(Math.floor(body.lpAmount * 1e7));

      const result = await buildContractTx(
        server,
        lpContractId,
        "remove_liquidity",
        [
          new Address(body.userAddress).toScVal(),
          nativeToScVal(lpStroops, { type: "i128" }),
        ],
        body.userAddress
      );

      return result;
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "Remove liquidity failed";
      reply.status(400).send({ error: message });
    }
  });

  /**
   * POST /liquidity/swap-xlm-to-sxlm
   * Build unsigned tx: swap XLM for sXLM.
   */
  fastify.post("/liquidity/swap-xlm-to-sxlm", async (request, reply) => {
    try {
      const body = swapSchema.parse(request.body);
      const stroops = BigInt(Math.floor(body.amount * 1e7));

      const minOutStroops = BigInt(Math.floor(body.minOut * 1e7));

      const result = await buildContractTx(
        server,
        lpContractId,
        "swap_xlm_to_sxlm",
        [
          new Address(body.userAddress).toScVal(),
          nativeToScVal(stroops, { type: "i128" }),
          nativeToScVal(minOutStroops, { type: "i128" }),
        ],
        body.userAddress
      );

      return result;
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "Swap failed";
      reply.status(400).send({ error: message });
    }
  });

  /**
   * POST /liquidity/swap-sxlm-to-xlm
   * Build unsigned tx: swap sXLM for XLM.
   */
  fastify.post("/liquidity/swap-sxlm-to-xlm", async (request, reply) => {
    try {
      const body = swapSchema.parse(request.body);
      const stroops = BigInt(Math.floor(body.amount * 1e7));
      const minOutStroops = BigInt(Math.floor(body.minOut * 1e7));

      const result = await buildContractTx(
        server,
        lpContractId,
        "swap_sxlm_to_xlm",
        [
          new Address(body.userAddress).toScVal(),
          nativeToScVal(stroops, { type: "i128" }),
          nativeToScVal(minOutStroops, { type: "i128" }),
        ],
        body.userAddress
      );

      return result;
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "Swap failed";
      reply.status(400).send({ error: message });
    }
  });

  /**
   * GET /liquidity/position/:wallet
   * Query on-chain LP position + sync to DB.
   */
  fastify.get("/liquidity/position/:wallet", async (request) => {
    const { wallet } = request.params as { wallet: string };

    try {
      const lpBalance = await queryContractView(
        server,
        lpContractId,
        "get_lp_balance",
        [new Address(wallet).toScVal()]
      );

      const lpTokens = BigInt(lpBalance ?? 0);

      // Get reserves for share calculation
      const reserves = await queryContractView(
        server,
        lpContractId,
        "get_reserves",
        []
      );
      const totalLp = await queryContractView(
        server,
        lpContractId,
        "total_lp_supply",
        []
      );

      const totalLpBig = BigInt(totalLp ?? 0);
      const sharePercent =
        totalLpBig > 0 ? (Number(lpTokens) / Number(totalLpBig)) * 100 : 0;

      const reserveXlm = BigInt(reserves?.[0] ?? 0);
      const reserveSxlm = BigInt(reserves?.[1] ?? 0);

      // Calculate user's share of the pool
      const userXlm =
        totalLpBig > 0
          ? (lpTokens * reserveXlm) / totalLpBig
          : BigInt(0);
      const userSxlm =
        totalLpBig > 0
          ? (lpTokens * reserveSxlm) / totalLpBig
          : BigInt(0);

      // Sync to DB
      if (lpTokens > 0) {
        const existing = await prisma.lPPosition.findFirst({
          where: { wallet },
        });
        if (existing) {
          await prisma.lPPosition.update({
            where: { id: existing.id },
            data: {
              lpTokens,
              xlmDeposited: userXlm,
              sxlmDeposited: userSxlm,
              updatedAt: new Date(),
            },
          });
        } else {
          await prisma.lPPosition.create({
            data: {
              wallet,
              lpTokens,
              xlmDeposited: userXlm,
              sxlmDeposited: userSxlm,
            },
          });
        }
      }

      return {
        wallet,
        lpTokens: Number(lpTokens) / 1e7,
        lpTokensRaw: lpTokens.toString(),
        sharePercent,
        xlmShare: Number(userXlm) / 1e7,
        sxlmShare: Number(userSxlm) / 1e7,
      };
    } catch {
      // Fallback to DB
      const dbPos = await prisma.lPPosition.findFirst({
        where: { wallet },
        orderBy: { updatedAt: "desc" },
      });
      return {
        wallet,
        lpTokens: dbPos ? Number(dbPos.lpTokens) / 1e7 : 0,
        lpTokensRaw: dbPos?.lpTokens.toString() ?? "0",
        sharePercent: 0,
        xlmShare: dbPos ? Number(dbPos.xlmDeposited) / 1e7 : 0,
        sxlmShare: dbPos ? Number(dbPos.sxlmDeposited) / 1e7 : 0,
      };
    }
  });

  /**
   * GET /liquidity/pool-stats
   * Query on-chain pool stats.
   */
  fastify.get("/liquidity/pool-stats", async () => {
    try {
      const reserves = await queryContractView(
        server,
        lpContractId,
        "get_reserves",
        []
      );
      const price = await queryContractView(
        server,
        lpContractId,
        "get_price",
        []
      );
      const totalLp = await queryContractView(
        server,
        lpContractId,
        "total_lp_supply",
        []
      );

      const reserveXlm = Number(reserves?.[0] ?? 0) / 1e7;
      const reserveSxlm = Number(reserves?.[1] ?? 0) / 1e7;

      return {
        reserveXlm,
        reserveSxlm,
        totalLpSupply: Number(totalLp ?? 0) / 1e7,
        price: Number(price ?? 10_000_000) / 1e7,
        feeBps: 30,
        tvl: reserveXlm + reserveSxlm * (Number(price ?? 10_000_000) / 1e7),
      };
    } catch {
      return {
        reserveXlm: 0,
        reserveSxlm: 0,
        totalLpSupply: 0,
        price: 1.0,
        feeBps: 30,
        tvl: 0,
      };
    }
  });

  /**
   * GET /liquidity/quote?side=xlm_to_sxlm&amount=123
   * Public quote endpoint for external integrators.
   */
  fastify.get("/liquidity/quote", async (request, reply) => {
    try {
      const query = quoteQuerySchema.parse(request.query);
      const reserves = await queryContractView(server, lpContractId, "get_reserves", []);

      const reserveXlm = BigInt(reserves?.[0] ?? 0);
      const reserveSxlm = BigInt(reserves?.[1] ?? 0);
      if (reserveXlm <= 0n || reserveSxlm <= 0n) {
        return reply.status(400).send({ error: "Pool has no liquidity" });
      }

      const feeBps = 30n;
      const amountIn = BigInt(Math.floor(query.amount * 1e7));
      const amountAfterFee = amountIn * (10_000n - feeBps) / 10_000n;

      const amountOut =
        query.side === "xlm_to_sxlm"
          ? reserveSxlm - (reserveXlm * reserveSxlm) / (reserveXlm + amountAfterFee)
          : reserveXlm - (reserveXlm * reserveSxlm) / (reserveSxlm + amountAfterFee);

      const spotPrice = Number(await queryContractView(server, lpContractId, "get_price", []) ?? 10_000_000) / 1e7;
      const midPrice =
        query.side === "xlm_to_sxlm"
          ? 1 / spotPrice
          : spotPrice;
      const effectivePrice = Number(amountOut) > 0 ? query.amount / (Number(amountOut) / 1e7) : 0;

      return {
        side: query.side,
        amountIn: query.amount,
        amountInRaw: amountIn.toString(),
        amountOut: Number(amountOut) / 1e7,
        amountOutRaw: amountOut.toString(),
        feeBps: Number(feeBps),
        midPrice,
        effectivePrice,
        priceImpactPct: midPrice > 0 ? Math.max(0, ((effectivePrice - midPrice) / midPrice) * 100) : 0,
      };
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "Quote failed";
      reply.status(400).send({ error: message });
    }
  });

  /**
   * GET /liquidity/mining-stats
   * Public endpoint: returns current mining rate and total funded rewards.
   */
  fastify.get("/liquidity/mining-stats", async (_request, reply) => {
    try {
      const [rateRaw, statsRaw] = await Promise.all([
        queryContractView(server, lpContractId, "get_mining_rate", []),
        queryContractView(server, lpContractId, "get_mining_stats", []),
      ]);

      const miningRate = Number(rateRaw ?? 0);
      // get_mining_stats returns [total_funded, total_claimed] or null
      const totalFunded = Number(statsRaw?.[0] ?? 0) / 1e7;
      const totalClaimed = Number(statsRaw?.[1] ?? 0) / 1e7;

      return {
        miningRatePerLedger: miningRate,
        miningRatePerLedgerXlm: miningRate / 1e7,
        totalFunded,
        totalClaimed,
        remainingRewards: Math.max(0, totalFunded - totalClaimed),
      };
    } catch {
      return {
        miningRatePerLedger: 0,
        miningRatePerLedgerXlm: 0,
        totalFunded: 0,
        totalClaimed: 0,
        remainingRewards: 0,
      };
    }
  });

  /**
   * GET /liquidity/pending-rewards/:wallet
   * Returns pending unclaimed mining rewards for an LP.
   */
  fastify.get("/liquidity/pending-rewards/:wallet", async (request, reply) => {
    const { wallet } = request.params as { wallet: string };
    if (!wallet || wallet.length < 56) {
      return reply.status(400).send({ error: "Invalid wallet address" });
    }

    try {
      const pendingRaw = await queryContractView(
        server,
        lpContractId,
        "get_pending_rewards",
        [new Address(wallet).toScVal()]
      );

      return {
        wallet,
        pendingRewards: Number(pendingRaw ?? 0) / 1e7,
        pendingRewardsRaw: String(pendingRaw ?? 0),
      };
    } catch {
      return { wallet, pendingRewards: 0, pendingRewardsRaw: "0" };
    }
  });

  /**
   * POST /liquidity/claim-rewards
   * Build unsigned tx: claim pending liquidity mining rewards.
   */
  fastify.post("/liquidity/claim-rewards", async (request, reply) => {
    const claimSchema = z.object({ userAddress: z.string().min(56).max(56) });
    try {
      const body = claimSchema.parse(request.body);
      const result = await buildContractTx(
        server,
        lpContractId,
        "claim_mining_rewards",
        [new Address(body.userAddress).toScVal()],
        body.userAddress
      );
      return result;
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "Claim rewards failed";
      reply.status(400).send({ error: message });
    }
  });

  /**
   * POST /liquidity/fund-rewards
   * Build unsigned tx: fund the liquidity mining rewards pool.
   */
  fastify.post("/liquidity/fund-rewards", async (request, reply) => {
    const fundSchema = z.object({
      userAddress: z.string().min(56).max(56),
      amount: z.number().positive(),
    });
    try {
      const body = fundSchema.parse(request.body);
      const amountStroops = BigInt(Math.floor(body.amount * 1e7));
      const result = await buildContractTx(
        server,
        lpContractId,
        "fund_mining_rewards",
        [
          new Address(body.userAddress).toScVal(),
          nativeToScVal(amountStroops, { type: "i128" }),
        ],
        body.userAddress
      );
      return result;
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "Fund rewards failed";
      reply.status(400).send({ error: message });
    }
  });

  /**
   * GET /liquidity/oracle?periodLedgers=720
   * Public TWAP oracle endpoint for external integrators.
   */
  fastify.get("/liquidity/oracle", async (request, reply) => {
    try {
      const query = oracleQuerySchema.parse(request.query);
      const [spotRaw, twapRaw] = await Promise.all([
        queryContractView(server, lpContractId, "get_price", []),
        queryContractView(server, lpContractId, "get_twap", [nativeToScVal(query.periodLedgers, { type: "u32" })]),
      ]);

      return {
        pair: "sXLM/XLM",
        periodLedgers: query.periodLedgers,
        spotPrice: Number(spotRaw ?? 10_000_000) / 1e7,
        twapPrice: Number(twapRaw ?? spotRaw ?? 10_000_000) / 1e7,
        priceScale: 1e7,
      };
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "Oracle query failed";
      reply.status(400).send({ error: message });
    }
  });
};
