import { FastifyPluginAsync } from "fastify";
import { z } from "zod";
import {
  rpc,
  Contract,
  Address,
  nativeToScVal,
  TransactionBuilder,
  BASE_FEE,
} from "@stellar/stellar-sdk";
import { config } from "../../config/index.js";

// ---------------------------------------------------------------------------
// Schemas
// ---------------------------------------------------------------------------

const stellarToEvmSchema = z.object({
  senderAddress: z.string().min(56).max(56),
  evmRecipient: z.string().regex(/^0x[0-9a-fA-F]{40}$/, "Invalid EVM address"),
  amount: z.number().positive().min(1),
  targetChainId: z.number().int().positive(),
});

const evmToStellarSchema = z.object({
  stellarRecipient: z.string().min(56).max(56),
  amount: z.number().positive().min(1),
  sourceChainId: z.number().int().positive(),
  evmSender: z.string().regex(/^0x[0-9a-fA-F]{40}$/, "Invalid EVM address"),
});

const bridgeSubmitSchema = z.object({
  signedXdr: z.string().min(1),
});

// EVM chain info (display only — real minting is done by the relayer)
const CHAIN_NAMES: Record<number, string> = {
  1: "Ethereum",
  42161: "Arbitrum",
  11155111: "Sepolia",
};

// ---------------------------------------------------------------------------
// Route plugin
// ---------------------------------------------------------------------------

export const bridgeRoutes: FastifyPluginAsync = async (fastify) => {
  const BRIDGE_CONTRACT_ID = process.env["BRIDGE_CONTRACT_ID"] ?? "";
  const server = new rpc.Server(config.stellar.rpcUrl);

  // -------------------------------------------------------------------------
  // POST /bridge/stellar-to-evm
  // Builds an unsigned Stellar tx that calls bridge_to_evm on the bridge contract.
  // The user signs this with Freighter; the relayer then mints wsXLM on EVM.
  // -------------------------------------------------------------------------

  fastify.post("/bridge/stellar-to-evm", async (request, reply) => {
    try {
      const body = stellarToEvmSchema.parse(request.body);

      if (!BRIDGE_CONTRACT_ID) {
        // Bridge contract not deployed on this environment — return a
        // "not yet available" response so the UI shows a clear message.
        return reply.status(503).send({
          error: "Bridge contract is not yet deployed on this network. Feature coming soon.",
        });
      }

      const amountStroops = BigInt(Math.floor(body.amount * 1e7));
      const evmRecipientBytes = Buffer.from(
        body.evmRecipient.replace("0x", ""),
        "hex"
      );

      const contract = new Contract(BRIDGE_CONTRACT_ID);
      const op = contract.call(
        "bridge_to_evm",
        new Address(body.senderAddress).toScVal(),
        nativeToScVal(evmRecipientBytes, { type: "bytes" }),
        nativeToScVal(amountStroops, { type: "i128" }),
        nativeToScVal(body.targetChainId, { type: "u32" })
      );

      const account = await server.getAccount(body.senderAddress);
      const tx = new TransactionBuilder(account, {
        fee: BASE_FEE,
        networkPassphrase: config.stellar.networkPassphrase,
      })
        .addOperation(op)
        .setTimeout(300)
        .build();

      const simResult = await server.simulateTransaction(tx);

      if (rpc.Api.isSimulationError(simResult)) {
        return reply.status(400).send({
          error: `Simulation failed: ${simResult.error}`,
        });
      }

      const preparedTx = rpc.assembleTransaction(tx, simResult).build();

      return {
        xdr: preparedTx.toXDR(),
        networkPassphrase: config.stellar.networkPassphrase,
        amount: body.amount,
        evmRecipient: body.evmRecipient,
        targetChain: CHAIN_NAMES[body.targetChainId] ?? `Chain ${body.targetChainId}`,
      };
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "Bridge transaction build failed";
      reply.status(400).send({ error: message });
    }
  });

  // -------------------------------------------------------------------------
  // POST /bridge/evm-to-stellar
  // Returns calldata for the user to call burnForStellar on the wsXLM EVM contract.
  // The relayer monitors BridgeBackInitiated events and releases sXLM on Stellar.
  // -------------------------------------------------------------------------

  fastify.post("/bridge/evm-to-stellar", async (request, reply) => {
    try {
      const body = evmToStellarSchema.parse(request.body);

      const chainName = CHAIN_NAMES[body.sourceChainId] ?? `Chain ${body.sourceChainId}`;
      const amountWei = BigInt(Math.floor(body.amount * 1e18)).toString();

      // ABI-encode the calldata for burnForStellar(stellarRecipient, amount)
      // The frontend wallet (MetaMask/WalletConnect) will use this to send the EVM tx.
      // Format: function burnForStellar(string stellarRecipient, uint256 amount)
      const stellarRecipientHex = Buffer.from(body.stellarRecipient, "utf8").toString("hex").padEnd(128, "0");
      const amountHex = BigInt(amountWei).toString(16).padStart(64, "0");
      // Function selector for burnForStellar(string,uint256)
      const selector = "0x" + Buffer.from("burnForStellar(string,uint256)").toString("hex").slice(0, 8);

      return {
        calldata: `${selector}${stellarRecipientHex}${amountHex}`,
        evmSender: body.evmSender,
        stellarRecipient: body.stellarRecipient,
        amount: body.amount,
        sourceChain: chainName,
        evmTxHash: `pending_${Date.now()}`,
        note: `Call burnForStellar on the wsXLM contract on ${chainName}. The relayer will release sXLM to your Stellar address within ~3 minutes.`,
      };
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "EVM bridge calldata build failed";
      reply.status(400).send({ error: message });
    }
  });

  // -------------------------------------------------------------------------
  // POST /bridge/submit
  // Submit a user-signed Stellar bridge transaction XDR.
  // Identical to /staking/submit but prefixed under /bridge for clarity.
  // -------------------------------------------------------------------------

  fastify.post("/bridge/submit", async (request, reply) => {
    try {
      const body = bridgeSubmitSchema.parse(request.body);

      const rpcResponse = await fetch(config.stellar.rpcUrl, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          jsonrpc: "2.0",
          id: 1,
          method: "sendTransaction",
          params: { transaction: body.signedXdr },
        }),
      });

      const rpcResult = await rpcResponse.json() as {
        result?: { hash: string; status: string; errorResultXdr?: string };
        error?: { message: string };
      };

      if (rpcResult.error) {
        return reply.status(400).send({ error: `RPC error: ${rpcResult.error.message}` });
      }

      if (!rpcResult.result) {
        return reply.status(400).send({ error: "No result from RPC" });
      }

      const { hash, status } = rpcResult.result;

      if (status === "ERROR") {
        return reply.status(400).send({
          error: `Transaction rejected: ${rpcResult.result.errorResultXdr ?? "unknown error"}`,
        });
      }

      return { txHash: hash, status };
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "Bridge submit failed";
      reply.status(400).send({ error: message });
    }
  });

  // -------------------------------------------------------------------------
  // GET /bridge/status/:txHash
  // Check the status of a bridge transaction by its Stellar tx hash.
  // Queries the Stellar RPC directly.
  // -------------------------------------------------------------------------

  fastify.get("/bridge/status/:txHash", async (request, reply) => {
    const { txHash } = request.params as { txHash: string };

    if (!txHash || txHash.length < 10) {
      return reply.status(400).send({ error: "Invalid transaction hash" });
    }

    try {
      // Check Stellar RPC for tx status
      const rpcResponse = await fetch(config.stellar.rpcUrl, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          jsonrpc: "2.0",
          id: 1,
          method: "getTransaction",
          params: { hash: txHash },
        }),
      });

      const rpcResult = await rpcResponse.json() as {
        result?: { status: string; ledger?: number };
        error?: { message: string };
      };

      if (rpcResult.error) {
        return { txHash, status: "pending", message: "Transaction not yet found" };
      }

      const txStatus = rpcResult.result?.status ?? "NOT_FOUND";

      let bridgeStatus: string;
      if (txStatus === "SUCCESS") {
        bridgeStatus = "relaying"; // Stellar tx confirmed, relayer will now mint on EVM
      } else if (txStatus === "FAILED") {
        bridgeStatus = "failed";
      } else {
        bridgeStatus = "pending";
      }

      return {
        txHash,
        status: bridgeStatus,
        stellarStatus: txStatus,
        ledger: rpcResult.result?.ledger ?? null,
      };
    } catch (err: unknown) {
      return { txHash, status: "pending", message: "Could not fetch status" };
    }
  });
};
