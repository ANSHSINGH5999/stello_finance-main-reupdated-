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
  Keypair,
} from "@stellar/stellar-sdk";
import { config } from "../../config/index.js";
import { PrismaClient } from "@prisma/client";
import {
  callSetCooldownPeriod,
  callUpdateCollateralFactor,
  callUpdateBorrowRate,
  callUpdateLiquidationThreshold,
  callSetLpProtocolFeeBps,
} from "../../staking-engine/contractClient.js";

// ---------------------------------------------------------------------------
// Zod schemas
// ---------------------------------------------------------------------------

const createProposalSchema = z.object({
  userAddress: z.string().min(56).max(56),
  paramKey: z.string().min(1),
  newValue: z.string().min(1),
});

const voteSchema = z.object({
  userAddress: z.string().min(56).max(56),
  proposalId: z.number().int().min(0),
  support: z.boolean(),
});

const executeSchema = z.object({
  userAddress: z.string().min(56).max(56),
  proposalId: z.number().int().min(0),
});

const finalizeSchema = z.object({
  proposalId: z.number().int().min(0),
});

const cancelSchema = z.object({
  proposalId: z.number().int().min(0),
});

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Build and simulate a user-facing transaction, returning raw XDR for wallet signing. */
async function buildContractTx(
  server: rpc.Server,
  contractId: string,
  method: string,
  args: ReturnType<typeof nativeToScVal>[],
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
    const errStr = String(simResult.error);
    if (errStr.includes("UnreachableCodeReached")) {
      if (method === "vote") throw new Error("You need sXLM to vote. Stake XLM first to receive sXLM, then vote.");
      if (method === "create_proposal") throw new Error("You need at least 100 sXLM to create a proposal. Stake XLM first.");
      if (method === "execute_proposal") throw new Error("Proposal cannot be queued — voting period may not be over, quorum not met, already queued, or it already executed.");
      if (method === "finalize_proposal") throw new Error("Proposal cannot be finalized — timelock delay not elapsed or already finalized.");
    }
    throw new Error(`Simulation failed: ${simResult.error}`);
  }

  const preparedTx = rpc.assembleTransaction(tx, simResult).build();
  return {
    xdr: preparedTx.toXDR(),
    networkPassphrase: config.stellar.networkPassphrase,
  };
}

/** Simulate a read-only contract view call. */
async function queryContractView(
  server: rpc.Server,
  contractId: string,
  method: string,
  args: ReturnType<typeof nativeToScVal>[]
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

/**
 * Execute an admin-signed transaction on a contract.
 * Used by the keeper to finalize timelock operations without requiring a user wallet.
 */
async function executeAdminTx(
  server: rpc.Server,
  contractId: string,
  method: string,
  args: ReturnType<typeof nativeToScVal>[]
): Promise<string> {
  const keypair = Keypair.fromSecret(config.admin.secretKey);
  const account = await server.getAccount(keypair.publicKey());

  const contract = new Contract(contractId);
  const op = contract.call(method, ...args);

  const tx = new TransactionBuilder(account, {
    fee: BASE_FEE,
    networkPassphrase: config.stellar.networkPassphrase,
  })
    .addOperation(op)
    .setTimeout(300)
    .build();

  const prepared = await server.prepareTransaction(tx);
  prepared.sign(keypair);

  const result = await server.sendTransaction(prepared);
  if (result.status === "ERROR") {
    throw new Error(`${method} failed: ${JSON.stringify(result.errorResult)}`);
  }

  // Poll for confirmation
  for (let i = 0; i < 30; i++) {
    const status = await server.getTransaction(result.hash);
    if (status.status === "SUCCESS") return result.hash;
    if (status.status === "FAILED") throw new Error(`Transaction ${result.hash} failed`);
    await new Promise((r) => setTimeout(r, 2000));
  }

  throw new Error(`Transaction ${result.hash} not confirmed after 30 attempts`);
}

/**
 * Apply a governance parameter change to the relevant on-chain contract.
 * Called by the keeper after a timelock operation is finalized.
 */
async function applyGovernanceParam(paramKey: string, newValue: string): Promise<void> {
  const value = parseInt(newValue, 10);
  if (isNaN(value)) {
    console.warn(`[Governance] Cannot apply param "${paramKey}": invalid value "${newValue}"`);
    return;
  }

  try {
    switch (paramKey) {
      case "cooldown_period":
        await callSetCooldownPeriod(value);
        console.log(`[Governance] Applied cooldown_period = ${value}`);
        break;
      case "collateral_factor":
        await callUpdateCollateralFactor(value);
        console.log(`[Governance] Applied collateral_factor = ${value} bps`);
        break;
      case "borrow_rate_bps":
        await callUpdateBorrowRate(value);
        console.log(`[Governance] Applied borrow_rate_bps = ${value}`);
        break;
      case "liquidation_threshold":
        await callUpdateLiquidationThreshold(value);
        console.log(`[Governance] Applied liquidation_threshold = ${value} bps`);
        break;
      case "lp_protocol_fee_bps":
        await callSetLpProtocolFeeBps(value);
        console.log(`[Governance] Applied lp_protocol_fee_bps = ${value}`);
        break;
      default:
        console.log(`[Governance] Param "${paramKey}" has no direct contract call — governance-only param`);
    }
  } catch (err) {
    console.error(`[Governance] Failed to apply param "${paramKey}" = "${newValue}":`, err);
  }
}

/** Convert a raw op_id (Uint8Array or similar) to a lowercase hex string for DB storage. */
function opIdToHex(raw: unknown): string {
  if (raw instanceof Uint8Array) {
    return Buffer.from(raw).toString("hex");
  }
  if (Array.isArray(raw)) {
    return Buffer.from(raw as number[]).toString("hex");
  }
  return String(raw);
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

export const governanceRoutes: FastifyPluginAsync<{ prisma: PrismaClient }> = async (
  fastify,
  opts
) => {
  const { prisma } = opts;
  const server = new rpc.Server(config.stellar.rpcUrl);
  const govContractId = config.contracts.governanceContractId;
  const timelockContractId = config.contracts.timelockContractId;

  // -------------------------------------------------------------------------
  // POST /governance/create-proposal
  // -------------------------------------------------------------------------

  fastify.post("/governance/create-proposal", async (request, reply) => {
    try {
      const body = createProposalSchema.parse(request.body);

      const result = await buildContractTx(
        server,
        govContractId,
        "create_proposal",
        [
          new Address(body.userAddress).toScVal(),
          nativeToScVal(body.paramKey, { type: "string" }),
          nativeToScVal(body.newValue, { type: "string" }),
        ],
        body.userAddress
      );

      const votingPeriodLedgers = 17_280; // ~24 h
      await prisma.governanceProposal.create({
        data: {
          proposer: body.userAddress,
          paramKey: body.paramKey,
          newValue: body.newValue,
          status: "active",
          expiresAt: new Date(Date.now() + votingPeriodLedgers * 5 * 1000),
        },
      });

      return result;
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "Create proposal failed";
      reply.status(400).send({ error: message });
    }
  });

  // -------------------------------------------------------------------------
  // POST /governance/vote
  // -------------------------------------------------------------------------

  fastify.post("/governance/vote", async (request, reply) => {
    try {
      const body = voteSchema.parse(request.body);

      const sxlmRaw = await queryContractView(
        server,
        config.contracts.sxlmTokenContractId,
        "balance",
        [new Address(body.userAddress).toScVal()]
      );
      const sxlmBalance = BigInt(sxlmRaw ?? 0);
      if (sxlmBalance <= BigInt(0)) {
        return reply.status(400).send({
          error: "You have no sXLM to vote with. Stake XLM first to receive sXLM, then vote.",
        });
      }

      const result = await buildContractTx(
        server,
        govContractId,
        "vote",
        [
          new Address(body.userAddress).toScVal(),
          nativeToScVal(BigInt(body.proposalId), { type: "u64" }),
          nativeToScVal(body.support, { type: "bool" }),
        ],
        body.userAddress
      );

      return result;
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "Vote failed";
      reply.status(400).send({ error: message });
    }
  });

  // -------------------------------------------------------------------------
  // POST /governance/execute
  // Validates a passed proposal and enters it into the 48 h timelock queue.
  // Returns XDR for user wallet signing.
  // -------------------------------------------------------------------------

  fastify.post("/governance/execute", async (request, reply) => {
    try {
      const body = executeSchema.parse(request.body);

      const result = await buildContractTx(
        server,
        govContractId,
        "execute_proposal",
        [nativeToScVal(BigInt(body.proposalId), { type: "u64" })],
        body.userAddress
      );

      // Mark DB proposal as queued — the actual op_id will be captured by the
      // event listener or the next /governance/proposals sync.
      const dbProposal = await prisma.governanceProposal.findFirst({
        where: { id: body.proposalId + 1 },
      });
      if (dbProposal) {
        await prisma.governanceProposal.update({
          where: { id: dbProposal.id },
          data: { status: "queued", queuedAt: new Date() },
        });
      }

      return result;
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "Queueing proposal failed";
      reply.status(400).send({ error: message });
    }
  });

  // -------------------------------------------------------------------------
  // POST /governance/record-timelock-op
  // Called by the frontend/wallet after the execute_proposal transaction confirms,
  // to persist the returned op_id in the DB alongside the eta ledger.
  // -------------------------------------------------------------------------

  fastify.post("/governance/record-timelock-op", async (request, reply) => {
    const recordSchema = z.object({
      proposalId: z.number().int().min(0),
      opId: z.string().min(1),        // hex-encoded
      etaLedger: z.number().int().min(0),
      expiryLedger: z.number().int().min(0),
    });

    try {
      const body = recordSchema.parse(request.body);
      const dbProposal = await prisma.governanceProposal.findFirst({
        where: { id: body.proposalId + 1 },
      });
      if (!dbProposal) {
        return reply.status(404).send({ error: "Proposal not found in DB" });
      }

      await prisma.governanceProposal.update({
        where: { id: dbProposal.id },
        data: {
          timelockOpId: body.opId,
          etaLedger: body.etaLedger,
          status: "queued",
        },
      });

      await prisma.timelockOperation.create({
        data: {
          opId: body.opId,
          proposalId: dbProposal.id,
          paramKey: dbProposal.paramKey,
          newValue: dbProposal.newValue,
          status: "queued",
          etaLedger: body.etaLedger,
          expiryLedger: body.expiryLedger,
        },
      });

      return { success: true };
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "Record timelock op failed";
      reply.status(400).send({ error: message });
    }
  });

  // -------------------------------------------------------------------------
  // POST /governance/timelock-execute
  // Keeper-facing: executes a ready timelock operation on-chain using the admin
  // keypair, then calls finalize_proposal on governance, then applies params.
  // -------------------------------------------------------------------------

  fastify.post("/governance/timelock-execute", async (request, reply) => {
    const body = finalizeSchema.parse(request.body);

    try {
      const dbProposal = await prisma.governanceProposal.findFirst({
        where: { id: body.proposalId + 1 },
        include: { timelockOperation: true },
      });

      if (!dbProposal || !dbProposal.timelockOpId) {
        return reply.status(404).send({ error: "Proposal not found or not yet queued in timelock" });
      }

      if (dbProposal.status === "executed") {
        return reply.status(400).send({ error: "Proposal already finalized" });
      }

      if (!timelockContractId) {
        return reply.status(503).send({ error: "Timelock contract not configured" });
      }

      // Step 1: Execute the timelock operation (marks it Executed in the timelock contract).
      const opIdBytes = Buffer.from(dbProposal.timelockOpId, "hex");
      const opIdScVal = nativeToScVal(opIdBytes, { type: "bytes" });

      const timelockExecHash = await executeAdminTx(
        server,
        timelockContractId,
        "execute_operation",
        [
          new Address(config.admin.publicKey).toScVal(),
          opIdScVal,
        ]
      );
      console.log(`[Governance] timelock::execute_operation tx: ${timelockExecHash}`);

      // Step 2: Finalize the governance proposal (stores param on governance contract).
      const finalizeHash = await executeAdminTx(
        server,
        govContractId,
        "finalize_proposal",
        [nativeToScVal(BigInt(body.proposalId), { type: "u64" })]
      );
      console.log(`[Governance] finalize_proposal tx: ${finalizeHash}`);

      // Step 3: Apply the parameter change to the relevant protocol contract.
      await applyGovernanceParam(dbProposal.paramKey, dbProposal.newValue);

      // Step 4: Update DB state.
      await prisma.governanceProposal.update({
        where: { id: dbProposal.id },
        data: { status: "executed", finalizedAt: new Date() },
      });

      if (dbProposal.timelockOperation) {
        await prisma.timelockOperation.update({
          where: { id: dbProposal.timelockOperation.id },
          data: { status: "executed", executedAt: new Date() },
        });
      }

      return {
        success: true,
        timelockTxHash: timelockExecHash,
        finalizeTxHash: finalizeHash,
        paramKey: dbProposal.paramKey,
        newValue: dbProposal.newValue,
      };
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "Timelock execution failed";
      reply.status(400).send({ error: message });
    }
  });

  // -------------------------------------------------------------------------
  // POST /governance/cancel
  // Guardian or admin cancels a queued proposal.
  // Guardian calls the timelock directly; admin calls via governance.cancel_proposal.
  // -------------------------------------------------------------------------

  fastify.post("/governance/cancel", async (request, reply) => {
    const cancelBody = cancelSchema.parse(request.body);
    const adminKey = request.headers["x-admin-key"] as string | undefined;

    if (adminKey !== config.jwt.secret && adminKey !== config.admin.secretKey) {
      return reply.status(403).send({ error: "Forbidden: invalid admin key" });
    }

    try {
      const dbProposal = await prisma.governanceProposal.findFirst({
        where: { id: cancelBody.proposalId + 1 },
        include: { timelockOperation: true },
      });

      if (!dbProposal) {
        return reply.status(404).send({ error: "Proposal not found" });
      }

      if (dbProposal.status !== "queued") {
        return reply.status(400).send({ error: "Proposal is not in the timelock queue" });
      }

      // Cancel via governance.cancel_proposal (admin role in timelock).
      const txHash = await executeAdminTx(
        server,
        govContractId,
        "cancel_proposal",
        [
          new Address(config.admin.publicKey).toScVal(),
          nativeToScVal(BigInt(cancelBody.proposalId), { type: "u64" }),
        ]
      );

      await prisma.governanceProposal.update({
        where: { id: dbProposal.id },
        data: { status: "cancelled" },
      });

      if (dbProposal.timelockOperation) {
        await prisma.timelockOperation.update({
          where: { id: dbProposal.timelockOperation.id },
          data: { status: "cancelled", cancelledAt: new Date() },
        });
      }

      console.log(`[Governance] Proposal ${cancelBody.proposalId} cancelled. tx: ${txHash}`);
      return { success: true, txHash };
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "Cancellation failed";
      reply.status(400).send({ error: message });
    }
  });

  // -------------------------------------------------------------------------
  // GET /governance/timelock-queue
  // Lists all operations currently in the timelock queue with their status.
  // -------------------------------------------------------------------------

  fastify.get("/governance/timelock-queue", async () => {
    const ops = await prisma.timelockOperation.findMany({
      orderBy: { queuedAt: "desc" },
      include: { proposal: true },
    });

    const SECONDS_PER_LEDGER = 5;
    const now = Date.now();

    return {
      operations: ops.map((op: {
        opId: string;
        proposalId: number;
        paramKey: string;
        newValue: string;
        status: string;
        etaLedger: number;
        expiryLedger: number;
        queuedAt: Date;
        executedAt: Date | null;
        cancelledAt: Date | null;
      }) => {
        const ledgersRemaining = Math.max(0, op.etaLedger - (now / (SECONDS_PER_LEDGER * 1000)));
        const readyAt = new Date(op.queuedAt.getTime() + 48 * 60 * 60 * 1000);
        return {
          opId: op.opId,
          proposalId: op.proposalId - 1,
          paramKey: op.paramKey,
          newValue: op.newValue,
          status: op.status,
          etaLedger: op.etaLedger,
          expiryLedger: op.expiryLedger,
          estimatedReadyAt: readyAt.toISOString(),
          isReady: op.status === "queued" && now >= readyAt.getTime(),
          queuedAt: op.queuedAt.toISOString(),
          executedAt: op.executedAt?.toISOString() ?? null,
          cancelledAt: op.cancelledAt?.toISOString() ?? null,
        };
      }),
      total: ops.length,
    };
  });

  // -------------------------------------------------------------------------
  // GET /governance/proposals
  // -------------------------------------------------------------------------

  fastify.get("/governance/proposals", async () => {
    try {
      const proposalCount = await queryContractView(
        server,
        govContractId,
        "proposal_count",
        []
      );

      const count = Number(proposalCount ?? 0);
      const proposals = [];

      for (let i = 0; i < Math.min(count, 50); i++) {
        try {
          const proposal = await queryContractView(
            server,
            govContractId,
            "get_proposal",
            [nativeToScVal(BigInt(i), { type: "u64" })]
          );

          if (proposal) {
            const voteCount = await queryContractView(
              server,
              govContractId,
              "get_vote_count",
              [nativeToScVal(BigInt(i), { type: "u64" })]
            );

            const endLedger = proposal.end_ledger ?? 0;
            const startLedger = proposal.start_ledger ?? 0;
            const votesForBig = BigInt(voteCount?.[0] ?? 0);
            const votesAgainstBig = BigInt(voteCount?.[1] ?? 0);

            let status = "active";
            if (proposal.executed) {
              status = "executed";
            } else if (proposal.queued) {
              status = "queued";
            } else if (endLedger > 0) {
              const expectedDurationMs = (endLedger - startLedger) * 5 * 1000;
              const elapsedMs = Date.now() - (startLedger * 5 * 1000);
              if (elapsedMs > expectedDurationMs) {
                status = votesForBig > votesAgainstBig ? "passed" : "rejected";
              }
            }

            proposals.push({
              id: i,
              proposer: proposal.proposer?.toString() ?? "",
              paramKey: proposal.param_key ?? "",
              newValue: proposal.new_value ?? "",
              votesFor: (voteCount?.[0] ?? 0).toString(),
              votesAgainst: (voteCount?.[1] ?? 0).toString(),
              startLedger,
              endLedger,
              executed: proposal.executed ?? false,
              queued: proposal.queued ?? false,
              status,
            });

            // Sync to DB
            const existing = await prisma.governanceProposal.findFirst({
              where: { proposer: proposal.proposer?.toString() ?? "", paramKey: proposal.param_key ?? "" },
            });
            if (existing) {
              await prisma.governanceProposal.update({
                where: { id: existing.id },
                data: {
                  votesFor: BigInt(voteCount?.[0] ?? 0),
                  votesAgainst: BigInt(voteCount?.[1] ?? 0),
                  status: proposal.executed ? "executed" : proposal.queued ? "queued" : "active",
                },
              });
            }
          }
        } catch {
          // Skip on individual proposal query failure
        }
      }

      if (proposals.length === 0) {
        return await buildProposalsFromDb(prisma);
      }

      return { proposals, total: proposals.length };
    } catch {
      return await buildProposalsFromDb(prisma);
    }
  });

  // -------------------------------------------------------------------------
  // GET /governance/proposals/:id
  // -------------------------------------------------------------------------

  fastify.get("/governance/proposals/:id", async (request, reply) => {
    const { id } = request.params as { id: string };
    const proposalId = parseInt(id, 10);

    try {
      const proposal = await queryContractView(
        server,
        govContractId,
        "get_proposal",
        [nativeToScVal(proposalId, { type: "u64" })]
      );

      if (!proposal) {
        return reply.status(404).send({ error: "Proposal not found" });
      }

      const voteCount = await queryContractView(
        server,
        govContractId,
        "get_vote_count",
        [nativeToScVal(proposalId, { type: "u64" })]
      );

      // Enrich with timelock data from DB if available
      const dbRecord = await prisma.governanceProposal.findFirst({
        where: { id: proposalId + 1 },
        include: { timelockOperation: true },
      });

      return {
        id: proposalId,
        proposer: proposal.proposer?.toString() ?? "",
        paramKey: proposal.param_key ?? "",
        newValue: proposal.new_value ?? "",
        votesFor: (voteCount?.[0] ?? 0).toString(),
        votesAgainst: (voteCount?.[1] ?? 0).toString(),
        startLedger: proposal.start_ledger ?? 0,
        endLedger: proposal.end_ledger ?? 0,
        executed: proposal.executed ?? false,
        queued: proposal.queued ?? false,
        status: proposal.executed ? "executed" : proposal.queued ? "queued" : "active",
        // Timelock details from DB
        timelockOpId: dbRecord?.timelockOpId ?? null,
        etaLedger: dbRecord?.timelockOperation?.etaLedger ?? null,
        expiryLedger: dbRecord?.timelockOperation?.expiryLedger ?? null,
        timelockStatus: dbRecord?.timelockOperation?.status ?? null,
        estimatedReadyAt: dbRecord?.timelockOperation
          ? new Date(dbRecord.timelockOperation.queuedAt.getTime() + 48 * 60 * 60 * 1000).toISOString()
          : null,
      };
    } catch {
      const dbProposal = await prisma.governanceProposal.findFirst({
        where: { id: proposalId + 1 },
        include: { timelockOperation: true },
      });
      if (!dbProposal) {
        return reply.status(404).send({ error: "Proposal not found" });
      }
      return {
        id: proposalId,
        proposer: dbProposal.proposer,
        paramKey: dbProposal.paramKey,
        newValue: dbProposal.newValue,
        votesFor: dbProposal.votesFor.toString(),
        votesAgainst: dbProposal.votesAgainst.toString(),
        status: dbProposal.status,
        expiresAt: dbProposal.expiresAt.toISOString(),
        timelockOpId: dbProposal.timelockOpId ?? null,
        timelockStatus: dbProposal.timelockOperation?.status ?? null,
      };
    }
  });

  // -------------------------------------------------------------------------
  // GET /governance/params
  // -------------------------------------------------------------------------

  fastify.get("/governance/params", async () => {
    const paramKeys = [
      { key: "protocol_fee_bps", defaultValue: "1000", description: "Protocol fee in basis points (10% = 1000)" },
      { key: "cooldown_period", defaultValue: "17280", description: "Withdrawal cooldown in ledgers (~24h)" },
      { key: "collateral_factor", defaultValue: "7000", description: "Lending collateral factor in bps (70%)" },
      { key: "borrow_rate_bps", defaultValue: "400", description: "Lending borrow rate in basis points (4% = 400)" },
      { key: "liquidation_threshold", defaultValue: "8000", description: "Liquidation threshold in bps (80% = 8000)" },
      { key: "lp_protocol_fee_bps", defaultValue: "5", description: "LP pool protocol fee in basis points (5 = 0.05% of swap input)" },
      { key: "lp_mining_rate", defaultValue: "0", description: "LP liquidity mining reward rate in stroops per ledger" },
      { key: "buffer_safety_factor", defaultValue: "250", description: "Liquidity buffer safety factor (2.5x)" },
    ];

    const params = await Promise.all(
      paramKeys.map(async ({ key, defaultValue, description }) => {
        let currentValue = defaultValue;
        try {
          const onChainValue = await queryContractView(
            server,
            govContractId,
            "get_param",
            [nativeToScVal(key, { type: "string" })]
          );
          if (onChainValue && String(onChainValue) !== "") {
            currentValue = String(onChainValue);
          }
        } catch {
          // Use default if on-chain query fails
        }
        return { key, currentValue, description };
      })
    );

    return { params };
  });
};

// ---------------------------------------------------------------------------
// Helper: build proposals response from DB
// ---------------------------------------------------------------------------

async function buildProposalsFromDb(prisma: PrismaClient) {
  const dbProposals = await prisma.governanceProposal.findMany({
    orderBy: { createdAt: "desc" },
    include: { timelockOperation: true },
  });
  return {
    proposals: dbProposals.map((p: {
      id: number;
      proposer: string;
      paramKey: string;
      newValue: string;
      votesFor: bigint;
      votesAgainst: bigint;
      status: string;
      expiresAt: Date;
      timelockOpId: string | null;
      timelockOperation: { etaLedger: number; status: string } | null;
    }) => ({
      id: p.id - 1,
      proposer: p.proposer,
      paramKey: p.paramKey,
      newValue: p.newValue,
      votesFor: p.votesFor.toString(),
      votesAgainst: p.votesAgainst.toString(),
      status: p.status,
      expiresAt: p.expiresAt.toISOString(),
      timelockOpId: p.timelockOpId ?? null,
      etaLedger: p.timelockOperation?.etaLedger ?? null,
      timelockStatus: p.timelockOperation?.status ?? null,
    })),
    total: dbProposals.length,
  };
}
