import dotenv from "dotenv";

dotenv.config();

function requireEnv(key: string, fallback?: string): string {
  const value = process.env[key] ?? fallback;
  if (value === undefined) {
    throw new Error(`Missing required environment variable: ${key}`);
  }
  return value;
}

function assertProductionConfig(nodeEnv: string, jwtSecret: string, adminSecretKey: string): void {
  if (nodeEnv !== "production") return;

  if (jwtSecret === "sxlm-dev-jwt-secret-change-in-production") {
    throw new Error("Insecure JWT_SECRET detected in production");
  }

  if (!adminSecretKey) {
    throw new Error("Missing ADMIN_SECRET_KEY in production");
  }
}

export type CollateralAssetOracle =
  | {
      type: "dex";
      assetCode: string;
      assetIssuer: string;
    }
  | {
      type: "fixed";
      price: number;
    };

export type CollateralAssetDefinition = {
  assetId: string;
  symbol: string;
  collateralFactorBps: number;
  initialPrice?: number;
  oracle?: CollateralAssetOracle;
};

function parseCollateralAssetOracle(value: unknown): CollateralAssetOracle | undefined {
  if (!value || typeof value !== "object") {
    return undefined;
  }
  const raw = value as Record<string, unknown>;
  const explicitType = typeof raw.type === "string" ? raw.type : undefined;

  if (explicitType === "fixed") {
    const price = typeof raw.price === "number" && raw.price > 0 ? raw.price : undefined;
    if (price) {
      return { type: "fixed", price };
    }
    return undefined;
  }

  const assetCode = typeof raw.assetCode === "string" && raw.assetCode.trim() ? raw.assetCode.trim() : undefined;
  const assetIssuer = typeof raw.assetIssuer === "string" && raw.assetIssuer.trim() ? raw.assetIssuer.trim() : undefined;
  if (assetCode && assetIssuer) {
    if (explicitType && explicitType !== "dex") {
      return undefined;
    }
    return { type: "dex", assetCode, assetIssuer };
  }

  return undefined;
}

function parseCollateralAssetsConfig(value: string): CollateralAssetDefinition[] {
  if (!value) return [];
  try {
    const raw = JSON.parse(value);
    if (!Array.isArray(raw)) return [];

    const parsed: CollateralAssetDefinition[] = [];
    for (const entry of raw) {
      if (!entry || typeof entry !== "object") continue;
      const row = entry as Record<string, unknown>;
      const assetId =
        typeof row.assetId === "string" && row.assetId.trim()
          ? row.assetId.trim()
          : typeof row.asset === "string" && row.asset.trim()
            ? row.asset.trim()
            : "";
      if (!assetId) continue;

      const cf = Number(row.collateralFactorBps ?? row.cfBps ?? row.cf);
      if (!Number.isFinite(cf) || cf <= 0 || cf > 10000) continue;

      const symbol =
        typeof row.symbol === "string" && row.symbol.trim()
          ? row.symbol.trim()
          : assetId.slice(0, 6);

      const initialPrice = typeof row.initialPrice === "number" && row.initialPrice > 0
        ? row.initialPrice
        : undefined;

      const oracle = parseCollateralAssetOracle(row.oracle ?? row);
      parsed.push({
        assetId,
        symbol,
        collateralFactorBps: Math.floor(cf),
        initialPrice,
        oracle,
      });
    }

    return parsed;
  } catch (err) {
    console.warn("[Config] COLLATERAL_ASSETS parse error:", err);
    return [];
  }
}

export const config = {
  stellar: {
    rpcUrl: requireEnv("STELLAR_RPC_URL", "https://mainnet.sorobanrpc.com"),
    networkPassphrase: requireEnv(
      "STELLAR_NETWORK_PASSPHRASE",
      "Public Global Stellar Network ; September 2015"
    ),
    horizonUrl: requireEnv(
      "STELLAR_HORIZON_URL",
      "https://horizon.stellar.org"
    ),
  },

  contracts: {
    sxlmTokenContractId: requireEnv(
      "SXLM_TOKEN_CONTRACT_ID",
      "CCGFHMW3NZD5Z7ATHYHZSEG6ABCJADUHP5HIAWFPR37CP4VGNEDQO7FJ"
    ),
    stakingContractId: requireEnv(
      "STAKING_CONTRACT_ID",
      "CDYXKWVDGEVA6OSIGN7GRAPPRN6AKID35OJL5ZZQIBCMECZ35KGL45PS"
    ),
    lendingContractId: requireEnv(
      "LENDING_CONTRACT_ID",
      "CAOWXZ6BWA2ZYY7GHD75OFKADKUJS4WCKPDYGGXULQWFJRB55TXAQNJG"
    ),
    lpPoolContractId: requireEnv(
      "LP_POOL_CONTRACT_ID",
      "CAW2DRMOI3CCJWKVMEUWYJUEQHXB4S4DR72HNL2DWQCMQQUH3LFFVLHV"
    ),
    governanceContractId: requireEnv(
      "GOVERNANCE_CONTRACT_ID",
      "CB7LV3FBQ7US26GVC7SM7RMX22IEEHAEUL7V3TDDWM32DHA5TDFDDEP4"
    ),
    timelockContractId: requireEnv(
      "TIMELOCK_CONTRACT_ID",
      ""
    ),
  },

  guardian: {
    // The guardian address may cancel malicious proposals directly on the timelock.
    // Leave empty on testnet; set to a multisig address in production.
    address: process.env["GUARDIAN_ADDRESS"] ?? "",
  },

  server: {
    port: parseInt(requireEnv("PORT", "3001"), 10),
    host: requireEnv("HOST", "0.0.0.0"),
    nodeEnv: requireEnv("NODE_ENV", "development"),
  },

  database: {
    url: requireEnv(
      "DATABASE_URL",
      "postgresql://sxlm:sxlm_password@localhost:5432/sxlm_protocol"
    ),
  },

  redis: {
    url: requireEnv("REDIS_URL", "redis://localhost:6379"),
  },

  admin: {
    secretKey: requireEnv("ADMIN_SECRET_KEY", ""),
    // Fallback to known active mainnet account used for read-only Soroban simulations
    publicKey: requireEnv("ADMIN_PUBLIC_KEY", "GDWXTIIROGCVBSNQMBJFH6HOWQ4YSRVMKSUS53CH6MP56WSWD6J4VZ5N"),
  },

  jwt: {
    secret: requireEnv("JWT_SECRET", "sxlm-dev-jwt-secret-change-in-production"),
    expiresIn: requireEnv("JWT_EXPIRES_IN", "24h"),
  },

  webhooks: {
    governanceUrl: process.env["GOVERNANCE_WEBHOOK_URL"] ?? "",
    slackUrl: process.env["SLACK_WEBHOOK_URL"] ?? "",
  },

  collateralAssets: parseCollateralAssetsConfig(process.env["COLLATERAL_ASSETS"] ?? "[]"),

  protocol: {
    unbondingPeriodMs: 7 * 24 * 60 * 60 * 1000, // 7 days
    liquidityBufferPercent: 5, // 5% of TVL — fallback minimum
    liquidityBufferSafetyFactor: 2.5, // α in Required Buffer = D × α
    liquidityBufferLookbackDays: 7, // Days to average withdrawal demand
    minStakeAmount: BigInt(10_000_000), // 1 XLM in stroops
    maxStakeAmount: BigInt(100_000_000_000_000), // 10M XLM in stroops
    rebalanceThreshold: 0.1, // 10% deviation triggers rebalance
    validatorMinUptime: 0.95, // 95%
    exchangeRateRefreshIntervalMs: 60_000, // 1 minute
    rewardSnapshotIntervalMs: 5 * 60_000, // 5 minutes
    withdrawalPollIntervalMs: 30_000, // 30 seconds
  },
} as const;

assertProductionConfig(config.server.nodeEnv, config.jwt.secret, config.admin.secretKey);

export type Config = typeof config;
