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

const amountSchema = z.object({
  userAddress: z.string().min(56).max(56),
  amount: z.number().positive(),
});

const assetAmountSchema = amountSchema.extend({
  asset: z.string().min(56).max(56),
});

const liquidateSchema = z.object({
  liquidatorAddress: z.string().min(56).max(56),
  borrowerAddress: z.string().min(56).max(56),
});

// Higher inclusion fee to avoid txINSUFFICIENT_FEE when simulation
// slightly underestimates resource costs. assembleTransaction adds
// minResourceFee on top of this, so the total is well above the minimum.
const SOROBAN_FEE = "2000000"; // 0.2 XLM

function formatAssetLabel(assetId: string, sxlmTokenId: string): string {
  if (assetId === sxlmTokenId) {
    return "sXLM";
  }

  return `${assetId.slice(0, 4)}...${assetId.slice(-4)}`;
}

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
    fee: SOROBAN_FEE,
    networkPassphrase: config.stellar.networkPassphrase,
  })
    .addOperation(op)
    .setTimeout(300)
    .build();

  const simResult = await server.simulateTransaction(tx);

  if (rpc.Api.isSimulationError(simResult)) {
    const errStr = String(simResult.error);
    // Translate common WASM trap errors into human-readable messages
    if (errStr.includes("UnreachableCodeReached")) {
      if (method === "deposit_collateral") {
        throw new Error("Insufficient sXLM balance. Stake XLM first to receive sXLM, then deposit it as collateral.");
      }
      if (method === "withdraw_collateral") {
        throw new Error("Withdrawal would make your position unhealthy, or you have no collateral deposited.");
      }
      if (method === "borrow") {
        throw new Error("Borrow exceeds your collateral limit. Deposit more sXLM or reduce the borrow amount.");
      }
      if (method === "repay") {
        throw new Error("Repay amount exceeds your outstanding debt.");
      }
      if (method === "liquidate") {
        throw new Error("This position cannot be liquidated — it may already be healthy or have no debt.");
      }
    }
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

export const lendingRoutes: FastifyPluginAsync<{ prisma: PrismaClient }> = async (
  fastify,
  opts
) => {
  const { prisma } = opts;
  const server = new rpc.Server(config.stellar.rpcUrl);
  const lendingContractId = config.contracts.lendingContractId;

  function classifyRisk(healthFactor: number): {
    riskLevel: "safe" | "warning" | "critical";
    recommendation: string;
  } {
    if (healthFactor <= 0) {
      return {
        riskLevel: "safe",
        recommendation: "No active debt position detected.",
      };
    }

    if (healthFactor < 1.0) {
      return {
        riskLevel: "critical",
        recommendation: "Position is liquidatable. Repay debt or add collateral immediately.",
      };
    }

    if (healthFactor < 1.5) {
      return {
        riskLevel: "warning",
        recommendation: "Health factor is low. Consider adding collateral or repaying part of your debt.",
      };
    }

    return {
      riskLevel: "safe",
      recommendation: "Position health is stable.",
    };
  }

  /**
   * POST /lending/deposit-collateral
   * Build unsigned tx: deposit sXLM as collateral.
   */
  fastify.post("/lending/deposit-collateral", async (request, reply) => {
    try {
      const body = assetAmountSchema.parse(request.body);
      const stroops = BigInt(Math.floor(body.amount * 1e7));
      const assetId = body.asset;

      // Pre-flight: check user has enough of the selected collateral asset before simulating
      const assetBalanceRaw = await queryContractView(
        server,
        assetId,
        "balance",
        [new Address(body.userAddress).toScVal()]
      );
      const assetBalance = BigInt(assetBalanceRaw ?? 0);
      if (assetBalance < stroops) {
        const available = (Number(assetBalance) / 1e7).toFixed(7);
        const assetLabel = formatAssetLabel(assetId, config.contracts.sxlmTokenContractId);
        return reply.status(400).send({
          error: `Insufficient ${assetLabel} balance. You have ${available} ${assetLabel} but tried to deposit ${body.amount} ${assetLabel}.`,
        });
      }

      const result = await buildContractTx(
        server,
        lendingContractId,
        "deposit_collateral",
        [
          new Address(body.userAddress).toScVal(),
          new Address(assetId).toScVal(),
          nativeToScVal(stroops, { type: "i128" }),
        ],
        body.userAddress
      );

      return result;
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : (err as { message?: string })?.message ?? "Deposit failed";
      reply.status(400).send({ error: message });
    }
  });

  /**
   * POST /lending/withdraw-collateral
   * Build unsigned tx: withdraw sXLM collateral.
   */
  fastify.post("/lending/withdraw-collateral", async (request, reply) => {
    try {
      const body = assetAmountSchema.parse(request.body);
      const stroops = BigInt(Math.floor(body.amount * 1e7));

      const result = await buildContractTx(
        server,
        lendingContractId,
        "withdraw_collateral",
        [
          new Address(body.userAddress).toScVal(),
          new Address(body.asset).toScVal(),
          nativeToScVal(stroops, { type: "i128" }),
        ],
        body.userAddress
      );

      return result;
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : (err as { message?: string })?.message ?? "Withdraw failed";
      reply.status(400).send({ error: message });
    }
  });

  /**
   * POST /lending/borrow
   * Build unsigned tx: borrow XLM against sXLM collateral.
   */
  fastify.post("/lending/borrow", async (request, reply) => {
    try {
      const body = amountSchema.parse(request.body);
      const stroops = BigInt(Math.floor(body.amount * 1e7));

      // Pre-flight: check pool has enough XLM liquidity
      const poolBalRaw = await queryContractView(server, lendingContractId, "get_pool_balance", []);
      const poolBalance = BigInt(poolBalRaw ?? 0);
      if (poolBalance < stroops) {
        const available = (Number(poolBalance) / 1e7).toFixed(7);
        return reply.status(400).send({
          error: `Insufficient pool liquidity. Pool has ${available} XLM available but you tried to borrow ${body.amount} XLM.`,
        });
      }

      const result = await buildContractTx(
        server,
        lendingContractId,
        "borrow",
        [
          new Address(body.userAddress).toScVal(),
          nativeToScVal(stroops, { type: "i128" }),
        ],
        body.userAddress
      );

      return result;
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : (err as { message?: string })?.message ?? "Borrow failed";
      reply.status(400).send({ error: message });
    }
  });

  /**
   * POST /lending/repay
   * Build unsigned tx: repay borrowed XLM.
   */
  fastify.post("/lending/repay", async (request, reply) => {
    try {
      const body = amountSchema.parse(request.body);
      const stroops = BigInt(Math.floor(body.amount * 1e7));

      const result = await buildContractTx(
        server,
        lendingContractId,
        "repay",
        [
          new Address(body.userAddress).toScVal(),
          nativeToScVal(stroops, { type: "i128" }),
        ],
        body.userAddress
      );

      return result;
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : (err as { message?: string })?.message ?? "Repay failed";
      reply.status(400).send({ error: message });
    }
  });

  /**
   * POST /lending/liquidate
   * Build unsigned tx: liquidate an unhealthy position.
   */
  fastify.post("/lending/liquidate", async (request, reply) => {
    try {
      const body = liquidateSchema.parse(request.body);

      const result = await buildContractTx(
        server,
        lendingContractId,
        "liquidate",
        [
          new Address(body.liquidatorAddress).toScVal(),
          new Address(body.borrowerAddress).toScVal(),
        ],
        body.liquidatorAddress
      );

      return result;
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : (err as { message?: string })?.message ?? "Liquidation failed";
      reply.status(400).send({ error: message });
    }
  });

  /**
   * GET /lending/position/:wallet
   * Query on-chain position via contract view + sync to DB.
   */
  fastify.get("/lending/position/:wallet", async (request, reply) => {
    try {
      const { wallet } = request.params as { wallet: string };

      const [assetListRaw, fullPositionRaw, healthFactor, maxBorrowRaw, erRaw] = await Promise.all([
        queryContractView(server, lendingContractId, "get_asset_list", []),
        queryContractView(server, lendingContractId, "get_full_position", [new Address(wallet).toScVal()]),
        queryContractView(server, lendingContractId, "health_factor", [new Address(wallet).toScVal()]),
        queryContractView(server, lendingContractId, "max_borrow_amount", [new Address(wallet).toScVal()]),
        queryContractView(server, lendingContractId, "get_exchange_rate", []),
      ]);

      const assetList = Array.isArray(assetListRaw)
        ? assetListRaw.map((asset) => String(asset))
        : [];
      const fullPosition = Array.isArray(fullPositionRaw)
        ? fullPositionRaw as Array<[string, string | number | bigint]>
        : [];

      const [borrowedRaw, assetConfigs] = await Promise.all([
        queryContractView(server, lendingContractId, "get_user_borrowed", [new Address(wallet).toScVal()]),
        Promise.all(
          assetList.map(async (assetId) => {
            const configRaw = await queryContractView(
              server,
              lendingContractId,
              "get_asset_config",
              [new Address(assetId).toScVal()]
            );

            return {
              assetId,
              collateralFactorBps: Number(configRaw?.[0] ?? (assetId === config.contracts.sxlmTokenContractId ? 7000 : 0)),
              price: Number(configRaw?.[1] ?? (assetId === config.contracts.sxlmTokenContractId ? 10_000_000 : 0)) / 1e7,
              enabled: Boolean(configRaw?.[2] ?? false),
              symbol: formatAssetLabel(assetId, config.contracts.sxlmTokenContractId),
            };
          })
        ),
      ]);

      const borrowed = BigInt(borrowedRaw ?? 0);
      const hf = healthFactor !== null ? Number(healthFactor) / 1e7 : 0;
      const maxBorrow = Number(maxBorrowRaw ?? 0) / 1e7;

      const assets = fullPosition.map(([assetId, balanceRaw]) => {
        const balance = BigInt(balanceRaw ?? 0);
        const configForAsset = assetConfigs.find((asset) => asset.assetId === String(assetId));
        return {
          assetId: String(assetId),
          symbol: configForAsset?.symbol ?? formatAssetLabel(String(assetId), config.contracts.sxlmTokenContractId),
          balance: Number(balance) / 1e7,
          balanceRaw: balance.toString(),
          collateralFactorBps: configForAsset?.collateralFactorBps ?? 0,
          price: configForAsset?.price ?? 0,
          enabled: configForAsset?.enabled ?? false,
          collateralValueXlm: (Number(balance) / 1e7) * (configForAsset?.price ?? 0),
        };
      });

      const totalCollateralXlm = assets.reduce((sum, asset) => sum + asset.collateralValueXlm, 0);
      const sxlmPosition = assets.find((asset) => asset.assetId === config.contracts.sxlmTokenContractId);

      // Sync aggregate position to the existing DB shape for backwards compatibility.
      if (assets.length > 0 || borrowed > 0n) {
        const existing = await prisma.collateralPosition.findFirst({
          where: { wallet },
        });
        if (existing) {
          await prisma.collateralPosition.update({
            where: { id: existing.id },
            data: {
              sxlmDeposited: BigInt(sxlmPosition?.balanceRaw ?? "0"),
              xlmBorrowed: borrowed,
              healthFactor: hf,
              updatedAt: new Date(),
            },
          });
        } else {
          await prisma.collateralPosition.create({
            data: {
              wallet,
              sxlmDeposited: BigInt(sxlmPosition?.balanceRaw ?? "0"),
              xlmBorrowed: borrowed,
              healthFactor: hf,
            },
          });
        }
      }

      return {
        wallet,
        collateralAssets: assets,
        sxlmDeposited: sxlmPosition?.balance ?? 0,
        sxlmDepositedRaw: sxlmPosition?.balanceRaw ?? "0",
        totalCollateralXlm,
        xlmBorrowed: Number(borrowed) / 1e7,
        xlmBorrowedRaw: borrowed.toString(),
        healthFactor: hf,
        maxBorrow,
        assetConfigs,
        exchangeRate: Number(erRaw ?? 10_000_000) / 1e7,
      };
    } catch (err: unknown) {
      // Fallback to DB if contract query fails
      const { wallet } = request.params as { wallet: string };
      const dbPosition = await prisma.collateralPosition.findFirst({
        where: { wallet },
        orderBy: { updatedAt: "desc" },
      });
      return dbPosition
        ? {
            wallet,
            collateralAssets: [
              {
                assetId: config.contracts.sxlmTokenContractId,
                symbol: "sXLM",
                balance: Number(dbPosition.sxlmDeposited) / 1e7,
                balanceRaw: dbPosition.sxlmDeposited.toString(),
                collateralFactorBps: 7000,
                price: 1,
                enabled: true,
                collateralValueXlm: Number(dbPosition.sxlmDeposited) / 1e7,
              },
            ],
            sxlmDeposited: Number(dbPosition.sxlmDeposited) / 1e7,
            sxlmDepositedRaw: dbPosition.sxlmDeposited.toString(),
            totalCollateralXlm: Number(dbPosition.sxlmDeposited) / 1e7,
            xlmBorrowed: Number(dbPosition.xlmBorrowed) / 1e7,
            xlmBorrowedRaw: dbPosition.xlmBorrowed.toString(),
            healthFactor: dbPosition.healthFactor,
            maxBorrow: 0, // cannot compute without on-chain CF/ER
            assetConfigs: [
              {
                assetId: config.contracts.sxlmTokenContractId,
                symbol: "sXLM",
                collateralFactorBps: 7000,
                price: 1,
                enabled: true,
              },
            ],
            exchangeRate: 1,
          }
        : {
            wallet,
            collateralAssets: [],
            sxlmDeposited: 0,
            sxlmDepositedRaw: "0",
            totalCollateralXlm: 0,
            xlmBorrowed: 0,
            xlmBorrowedRaw: "0",
            healthFactor: 0,
            maxBorrow: 0,
            assetConfigs: [],
            exchangeRate: 1,
          };
    }
  });

  /**
   * GET /lending/stats
   * Query on-chain lending stats.
   */
  fastify.get("/lending/stats", async () => {
    try {
      const [assetListRaw, totalBorrowedRaw, ltBpsRaw, borrowRateBpsRaw, poolBalanceRaw] =
        await Promise.all([
          queryContractView(server, lendingContractId, "get_asset_list", []),
          queryContractView(server, lendingContractId, "total_borrowed", []),
          queryContractView(server, lendingContractId, "get_liquidation_threshold", []),
          queryContractView(server, lendingContractId, "get_borrow_rate", []),
          queryContractView(server, lendingContractId, "get_pool_balance", []),
        ]);

      const assetList = Array.isArray(assetListRaw)
        ? assetListRaw.map((asset) => String(asset))
        : [];
      const assetStats = await Promise.all(
        assetList.map(async (assetId) => {
          const [totalAssetCollateralRaw, configRaw] = await Promise.all([
            queryContractView(server, lendingContractId, "total_asset_collateral", [new Address(assetId).toScVal()]),
            queryContractView(server, lendingContractId, "get_asset_config", [new Address(assetId).toScVal()]),
          ]);

          const totalAssetCollateral = BigInt(totalAssetCollateralRaw ?? 0);
          const price = Number(configRaw?.[1] ?? (assetId === config.contracts.sxlmTokenContractId ? 10_000_000 : 0)) / 1e7;

          return {
            assetId,
            symbol: formatAssetLabel(assetId, config.contracts.sxlmTokenContractId),
            totalCollateral: Number(totalAssetCollateral) / 1e7,
            totalCollateralRaw: totalAssetCollateral.toString(),
            collateralFactorBps: Number(configRaw?.[0] ?? (assetId === config.contracts.sxlmTokenContractId ? 7000 : 0)),
            price,
            enabled: Boolean(configRaw?.[2] ?? false),
            collateralValueXlm: (Number(totalAssetCollateral) / 1e7) * price,
          };
        })
      );

      const totalCollateralXlm = assetStats.reduce((sum, asset) => sum + asset.collateralValueXlm, 0);
      const tb = Number(totalBorrowedRaw ?? 0);

      return {
        collateralAssets: assetStats,
        totalCollateral: totalCollateralXlm,
        totalCollateralRaw: assetStats
          .reduce((sum, asset) => sum + BigInt(asset.totalCollateralRaw), 0n)
          .toString(),
        totalBorrowed: tb / 1e7,
        totalBorrowedRaw: (totalBorrowedRaw ?? 0).toString(),
        poolBalance: Number(poolBalanceRaw ?? 0) / 1e7,
        liquidationThresholdBps: Number(ltBpsRaw ?? 8000),
        borrowRateBps: Number(borrowRateBpsRaw ?? 500),
        utilizationRate: totalCollateralXlm > 0 ? (tb / 1e7) / totalCollateralXlm : 0,
      };
    } catch {
      return {
        collateralAssets: [],
        totalCollateral: 0,
        totalCollateralRaw: "0",
        totalBorrowed: 0,
        totalBorrowedRaw: "0",
        poolBalance: 0,
        liquidationThresholdBps: 8000,
        borrowRateBps: 500,
        utilizationRate: 0,
      };
    }
  });

  fastify.get("/lending/assets", async () => {
    try {
      const assetListRaw = await queryContractView(server, lendingContractId, "get_asset_list", []);
      const assetList = Array.isArray(assetListRaw)
        ? assetListRaw.map((asset) => String(asset))
        : [];

      const assets = await Promise.all(
        assetList.map(async (assetId) => {
          const configRaw = await queryContractView(
            server,
            lendingContractId,
            "get_asset_config",
            [new Address(assetId).toScVal()]
          );

          return {
            assetId,
            symbol: formatAssetLabel(assetId, config.contracts.sxlmTokenContractId),
            collateralFactorBps: Number(configRaw?.[0] ?? (assetId === config.contracts.sxlmTokenContractId ? 7000 : 0)),
            price: Number(configRaw?.[1] ?? (assetId === config.contracts.sxlmTokenContractId ? 10_000_000 : 0)) / 1e7,
            enabled: Boolean(configRaw?.[2] ?? false),
            isDefault: assetId === config.contracts.sxlmTokenContractId,
          };
        })
      );

      return { assets };
    } catch {
      return {
        assets: [
          {
            assetId: config.contracts.sxlmTokenContractId,
            symbol: "sXLM",
            collateralFactorBps: 7000,
            price: 1,
            enabled: true,
            isDefault: true,
          },
        ],
      };
    }
  });

  /**
   * GET /lending/alerts/:wallet
   * Returns lending risk classification based on health factor.
   */
  fastify.get("/lending/alerts/:wallet", async (request) => {
    const { wallet } = request.params as { wallet: string };

    try {
      const healthFactorRaw = await queryContractView(
        server,
        lendingContractId,
        "health_factor",
        [new Address(wallet).toScVal()]
      );

      const healthFactor = Number(healthFactorRaw ?? 0) / 1e7;
      const classification = classifyRisk(healthFactor);

      return {
        wallet,
        healthFactor,
        ...classification,
        source: "chain",
        timestamp: new Date().toISOString(),
      };
    } catch {
      const dbPosition = await prisma.collateralPosition.findFirst({
        where: { wallet },
        orderBy: { updatedAt: "desc" },
      });

      const healthFactor = dbPosition?.healthFactor ?? 0;
      const classification = classifyRisk(healthFactor);

      return {
        wallet,
        healthFactor,
        ...classification,
        source: "db",
        timestamp: new Date().toISOString(),
      };
    }
  });
};
