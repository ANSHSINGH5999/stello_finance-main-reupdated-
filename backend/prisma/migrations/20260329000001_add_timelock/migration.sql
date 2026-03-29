-- Add timelock fields to governance_proposals
ALTER TABLE "governance_proposals"
  ADD COLUMN IF NOT EXISTS "timelockOpId"  TEXT,
  ADD COLUMN IF NOT EXISTS "queuedAt"      TIMESTAMP(3),
  ADD COLUMN IF NOT EXISTS "etaLedger"     INTEGER,
  ADD COLUMN IF NOT EXISTS "finalizedAt"   TIMESTAMP(3);

-- CreateTable: timelock_operations
CREATE TABLE "timelock_operations" (
    "id"           SERIAL NOT NULL,
    "opId"         TEXT NOT NULL,
    "proposalId"   INTEGER NOT NULL,
    "paramKey"     TEXT NOT NULL,
    "newValue"     TEXT NOT NULL,
    "status"       TEXT NOT NULL DEFAULT 'queued',
    "etaLedger"    INTEGER NOT NULL,
    "expiryLedger" INTEGER NOT NULL,
    "queuedAt"     TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "executedAt"   TIMESTAMP(3),
    "cancelledAt"  TIMESTAMP(3),

    CONSTRAINT "timelock_operations_pkey" PRIMARY KEY ("id")
);

-- Unique constraints
ALTER TABLE "timelock_operations"
  ADD CONSTRAINT "timelock_operations_opId_key"       UNIQUE ("opId"),
  ADD CONSTRAINT "timelock_operations_proposalId_key"  UNIQUE ("proposalId");

-- Indexes for keeper queries
CREATE INDEX "timelock_operations_status_idx"    ON "timelock_operations"("status");
CREATE INDEX "timelock_operations_etaLedger_idx" ON "timelock_operations"("etaLedger");

-- Foreign key: timelock_operations.proposalId → governance_proposals.id
ALTER TABLE "timelock_operations"
  ADD CONSTRAINT "timelock_operations_proposalId_fkey"
  FOREIGN KEY ("proposalId") REFERENCES "governance_proposals"("id")
  ON DELETE RESTRICT ON UPDATE CASCADE;
