#![no_std]

//! # sXLM Timelock Contract
//!
//! Enforces a mandatory delay between governance proposal approval and on-chain parameter
//! application.  Passed proposals enter this queue; only after the configured delay
//! (default 48 h ≈ 34 560 ledgers at 5 s/ledger) may they be finalized.
//!
//! ## Roles
//! - **Admin**    — The governance contract.  The only address permitted to enqueue
//!                  operations.  Also holds the emergency-cancel right.
//! - **Guardian** — A fast-response multisig / security council.  May cancel malicious
//!                  or erroneous operations before they execute.
//!
//! ## Progressive decentralization roadmap
//! 1. Phase 1 (now)    — Admin key + timelock.  Guardian is a multisig.
//! 2. Phase 2          — Transfer admin role to governance contract itself; admin key
//!                        no longer needed for day-to-day governance.
//! 3. Phase 3          — Increase delay, reduce guardian power over time as protocol
//!                        matures.
//! 4. Phase 4 (target) — No admin key.  Governance contract is the sole admin.
//!                        Guardian may only cancel, not modify, operations.

use soroban_sdk::{
    contract, contractimpl, contracttype, symbol_short,
    Address, Bytes, BytesN, Env, String, Symbol,
};

// ---------------------------------------------------------------------------
// Timing constants
// ---------------------------------------------------------------------------

/// Default timelock delay: ~48 hours at 5 seconds per ledger.
const DEFAULT_DELAY_LEDGERS: u32 = 34_560;

/// Grace period after `eta` during which the operation must be executed.
/// After this window the operation expires and can no longer run (~7 days).
const GRACE_PERIOD_LEDGERS: u32 = 120_960;

// ---------------------------------------------------------------------------
// TTL constants (mirror governance contract)
// ---------------------------------------------------------------------------
const INSTANCE_LIFETIME_THRESHOLD: u32 = 100_800; // ~7 days
const INSTANCE_BUMP_AMOUNT: u32 = 518_400;         // ~30 days
const OP_LIFETIME_THRESHOLD: u32 = 518_400;        // ~30 days
const OP_BUMP_AMOUNT: u32 = 3_110_400;             // ~180 days

// ---------------------------------------------------------------------------
// Storage layout
// ---------------------------------------------------------------------------

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    Initialized,
    /// The governance contract address.  Only this address may queue operations.
    Admin,
    /// Emergency guardian — may cancel queued operations.
    Guardian,
    /// Timelock delay in ledgers.
    DelayLedgers,
    /// Individual queued operation.
    Operation(BytesN<32>),
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq)]
#[contracttype]
pub enum OperationStatus {
    Queued,
    Executed,
    Cancelled,
}

#[derive(Clone)]
#[contracttype]
pub struct QueuedOperation {
    /// Deterministic identifier: SHA-256(proposal_id || eta_ledger).
    pub id: BytesN<32>,
    /// The governance proposal that generated this operation.
    pub proposal_id: u64,
    /// The protocol parameter key being changed (e.g. "collateral_factor").
    pub param_key: String,
    /// The new value to be applied (e.g. "7500" for 75 % in bps).
    pub new_value: String,
    /// Ledger at which the operation was queued.
    pub queued_at_ledger: u32,
    /// Earliest ledger at which the operation may be executed.
    pub eta_ledger: u32,
    /// Latest ledger by which the operation must be executed (eta + grace period).
    pub expiry_ledger: u32,
    /// Current lifecycle state.
    pub status: OperationStatus,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn extend_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
}

fn read_admin(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::Admin).unwrap()
}

fn read_guardian(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::Guardian).unwrap()
}

fn read_delay(env: &Env) -> u32 {
    env.storage()
        .instance()
        .get(&DataKey::DelayLedgers)
        .unwrap_or(DEFAULT_DELAY_LEDGERS)
}

/// Deterministic operation ID: SHA-256 over (proposal_id as big-endian u64) || (eta as big-endian u32).
fn compute_op_id(env: &Env, proposal_id: u64, eta: u32) -> BytesN<32> {
    let mut data = Bytes::new(env);
    for b in proposal_id.to_be_bytes().iter() {
        data.push_back(*b);
    }
    for b in eta.to_be_bytes().iter() {
        data.push_back(*b);
    }
    env.crypto().sha256(&data).into()
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct TimelockContract;

#[contractimpl]
impl TimelockContract {
    // -----------------------------------------------------------------------
    // Lifecycle
    // -----------------------------------------------------------------------

    /// One-time initialization.
    ///
    /// * `admin`          — The governance contract address (enqueuing authority).
    /// * `guardian`       — Emergency cancellation authority.
    /// * `delay_ledgers`  — Override the default 48 h delay (0 = use default).
    pub fn initialize(env: Env, admin: Address, guardian: Address, delay_ledgers: u32) {
        let already: bool = env
            .storage()
            .instance()
            .get(&DataKey::Initialized)
            .unwrap_or(false);
        assert!(!already, "already initialized");

        env.storage().instance().set(&DataKey::Initialized, &true);
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Guardian, &guardian);

        let delay = if delay_ledgers == 0 {
            DEFAULT_DELAY_LEDGERS
        } else {
            delay_ledgers
        };
        env.storage().instance().set(&DataKey::DelayLedgers, &delay);
        extend_instance(&env);
    }

    /// Bump instance TTL — callable by anyone to keep contract alive.
    pub fn bump_instance(env: Env) {
        extend_instance(&env);
    }

    // -----------------------------------------------------------------------
    // Core timelock operations
    // -----------------------------------------------------------------------

    /// Enqueue a governance operation.
    ///
    /// Only the admin (governance contract) may call this.
    /// Returns the deterministic operation ID.
    pub fn queue_operation(
        env: Env,
        caller: Address,
        proposal_id: u64,
        param_key: String,
        new_value: String,
    ) -> BytesN<32> {
        caller.require_auth();

        let admin = read_admin(&env);
        assert!(caller == admin, "only governance may queue operations");

        let delay = read_delay(&env);
        let current_ledger = env.ledger().sequence();
        let eta = current_ledger + delay;
        let expiry = eta + GRACE_PERIOD_LEDGERS;

        let op_id = compute_op_id(&env, proposal_id, eta);

        assert!(
            !env.storage()
                .persistent()
                .has(&DataKey::Operation(op_id.clone())),
            "operation already queued"
        );

        let operation = QueuedOperation {
            id: op_id.clone(),
            proposal_id,
            param_key: param_key.clone(),
            new_value: new_value.clone(),
            queued_at_ledger: current_ledger,
            eta_ledger: eta,
            expiry_ledger: expiry,
            status: OperationStatus::Queued,
        };

        env.storage()
            .persistent()
            .set(&DataKey::Operation(op_id.clone()), &operation);
        env.storage().persistent().extend_ttl(
            &DataKey::Operation(op_id.clone()),
            OP_LIFETIME_THRESHOLD,
            OP_BUMP_AMOUNT,
        );

        extend_instance(&env);

        env.events().publish(
            (symbol_short!("tl_queue"),),
            (op_id.clone(), proposal_id, param_key, eta),
        );

        op_id
    }

    /// Execute a queued operation after the delay has elapsed.
    ///
    /// Anyone may call this once `current_ledger >= eta_ledger`.
    /// Returns the operation data so the caller can apply parameter changes.
    pub fn execute_operation(env: Env, executor: Address, op_id: BytesN<32>) -> QueuedOperation {
        executor.require_auth();

        let mut op: QueuedOperation = env
            .storage()
            .persistent()
            .get(&DataKey::Operation(op_id.clone()))
            .unwrap_or_else(|| panic!("operation not found"));

        assert!(op.status == OperationStatus::Queued, "operation not queued");

        let current = env.ledger().sequence();
        assert!(current >= op.eta_ledger, "timelock delay not elapsed");
        assert!(current <= op.expiry_ledger, "operation expired");

        op.status = OperationStatus::Executed;
        env.storage()
            .persistent()
            .set(&DataKey::Operation(op_id.clone()), &op);
        env.storage().persistent().extend_ttl(
            &DataKey::Operation(op_id.clone()),
            OP_LIFETIME_THRESHOLD,
            OP_BUMP_AMOUNT,
        );

        extend_instance(&env);

        env.events().publish(
            (symbol_short!("tl_exec"),),
            (op_id, op.proposal_id),
        );

        op
    }

    /// Cancel a queued operation before it executes.
    ///
    /// Only the guardian or admin (governance) may call this.  Use for emergency
    /// cancellation of malicious or erroneous proposals.
    pub fn cancel_operation(env: Env, canceller: Address, op_id: BytesN<32>) {
        canceller.require_auth();

        let admin = read_admin(&env);
        let guardian = read_guardian(&env);
        assert!(
            canceller == admin || canceller == guardian,
            "only guardian or admin may cancel"
        );

        let mut op: QueuedOperation = env
            .storage()
            .persistent()
            .get(&DataKey::Operation(op_id.clone()))
            .unwrap_or_else(|| panic!("operation not found"));

        assert!(op.status == OperationStatus::Queued, "operation not queued");

        op.status = OperationStatus::Cancelled;
        env.storage()
            .persistent()
            .set(&DataKey::Operation(op_id.clone()), &op);
        env.storage().persistent().extend_ttl(
            &DataKey::Operation(op_id.clone()),
            OP_LIFETIME_THRESHOLD,
            OP_BUMP_AMOUNT,
        );

        extend_instance(&env);

        env.events().publish(
            (symbol_short!("tl_cncl"),),
            (op_id, op.proposal_id),
        );
    }

    // -----------------------------------------------------------------------
    // Admin configuration
    // -----------------------------------------------------------------------

    /// Update the timelock delay.  Only callable by admin (governance).
    ///
    /// Note: for a fully decentralized protocol, changes to the delay should
    /// themselves be timelocked — enforce this at the governance layer.
    pub fn set_delay(env: Env, admin: Address, new_delay_ledgers: u32) {
        admin.require_auth();
        assert!(admin == read_admin(&env), "only admin may set delay");
        assert!(new_delay_ledgers > 0, "delay must be positive");

        env.storage()
            .instance()
            .set(&DataKey::DelayLedgers, &new_delay_ledgers);
        extend_instance(&env);

        env.events()
            .publish((symbol_short!("tl_dlay"),), new_delay_ledgers);
    }

    /// Rotate the guardian address.  Only callable by admin (governance).
    pub fn set_guardian(env: Env, admin: Address, new_guardian: Address) {
        admin.require_auth();
        assert!(admin == read_admin(&env), "only admin may set guardian");

        env.storage()
            .instance()
            .set(&DataKey::Guardian, &new_guardian);
        extend_instance(&env);

        env.events()
            .publish((symbol_short!("tl_grd"),), new_guardian);
    }

    // -----------------------------------------------------------------------
    // Views
    // -----------------------------------------------------------------------

    pub fn get_operation(env: Env, op_id: BytesN<32>) -> QueuedOperation {
        env.storage()
            .persistent()
            .get(&DataKey::Operation(op_id))
            .unwrap_or_else(|| panic!("operation not found"))
    }

    pub fn get_delay(env: Env) -> u32 {
        read_delay(&env)
    }

    pub fn get_guardian(env: Env) -> Address {
        read_guardian(&env)
    }

    /// Returns true when a queued operation has passed its delay and is ready to execute.
    pub fn is_operation_ready(env: Env, op_id: BytesN<32>) -> bool {
        match env
            .storage()
            .persistent()
            .get::<DataKey, QueuedOperation>(&DataKey::Operation(op_id))
        {
            Some(op) => {
                let current = env.ledger().sequence();
                op.status == OperationStatus::Queued
                    && current >= op.eta_ledger
                    && current <= op.expiry_ledger
            }
            None => false,
        }
    }

    /// Returns true when an operation has been successfully executed.
    /// Used by the governance contract's `finalize_proposal` to verify on-chain.
    pub fn is_executed(env: Env, op_id: BytesN<32>) -> bool {
        match env
            .storage()
            .persistent()
            .get::<DataKey, QueuedOperation>(&DataKey::Operation(op_id))
        {
            Some(op) => op.status == OperationStatus::Executed,
            None => false,
        }
    }

    /// Returns true when an operation has been cancelled.
    pub fn is_cancelled(env: Env, op_id: BytesN<32>) -> bool {
        match env
            .storage()
            .persistent()
            .get::<DataKey, QueuedOperation>(&DataKey::Operation(op_id))
        {
            Some(op) => op.status == OperationStatus::Cancelled,
            None => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::Env;

    fn setup() -> (Env, Address, Address, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let guardian = Address::generate(&env);
        let executor = Address::generate(&env);
        let contract_id = env.register_contract(None, TimelockContract);

        let client = TimelockContractClient::new(&env, &contract_id);
        client.initialize(&admin, &guardian, &0); // 0 → default 34 560 ledgers

        (env, contract_id, admin, guardian, executor)
    }

    #[test]
    fn test_initialize() {
        let (env, contract_id, _admin, guardian, _executor) = setup();
        let client = TimelockContractClient::new(&env, &contract_id);
        assert_eq!(client.get_delay(), DEFAULT_DELAY_LEDGERS);
        assert_eq!(client.get_guardian(), guardian);
    }

    #[test]
    fn test_queue_and_execute() {
        let (env, contract_id, admin, _guardian, executor) = setup();
        let client = TimelockContractClient::new(&env, &contract_id);

        let op_id = client.queue_operation(
            &admin,
            &0u64,
            &soroban_sdk::String::from_str(&env, "collateral_factor"),
            &soroban_sdk::String::from_str(&env, "7500"),
        );

        // Cannot execute before delay
        assert!(!client.is_operation_ready(&op_id));

        // Advance ledger past eta
        env.ledger().with_mut(|li| {
            li.sequence_number += DEFAULT_DELAY_LEDGERS + 1;
        });

        assert!(client.is_operation_ready(&op_id));
        assert!(!client.is_executed(&op_id));

        let op = client.execute_operation(&executor, &op_id);
        assert_eq!(op.proposal_id, 0u64);
        assert_eq!(op.param_key, soroban_sdk::String::from_str(&env, "collateral_factor"));
        assert_eq!(op.new_value, soroban_sdk::String::from_str(&env, "7500"));
        assert!(client.is_executed(&op_id));
        assert!(!client.is_operation_ready(&op_id));
    }

    #[test]
    #[should_panic(expected = "timelock delay not elapsed")]
    fn test_cannot_execute_before_delay() {
        let (env, contract_id, admin, _guardian, executor) = setup();
        let client = TimelockContractClient::new(&env, &contract_id);

        let op_id = client.queue_operation(
            &admin,
            &1u64,
            &soroban_sdk::String::from_str(&env, "borrow_rate_bps"),
            &soroban_sdk::String::from_str(&env, "400"),
        );

        // Do NOT advance ledger
        client.execute_operation(&executor, &op_id);
    }

    #[test]
    #[should_panic(expected = "operation expired")]
    fn test_cannot_execute_after_expiry() {
        let (env, contract_id, admin, _guardian, executor) = setup();
        let client = TimelockContractClient::new(&env, &contract_id);

        let op_id = client.queue_operation(
            &admin,
            &2u64,
            &soroban_sdk::String::from_str(&env, "protocol_fee_bps"),
            &soroban_sdk::String::from_str(&env, "500"),
        );

        // Advance past both delay AND grace period
        env.ledger().with_mut(|li| {
            li.sequence_number += DEFAULT_DELAY_LEDGERS + GRACE_PERIOD_LEDGERS + 1;
        });

        client.execute_operation(&executor, &op_id);
    }

    #[test]
    fn test_guardian_cancel() {
        let (env, contract_id, admin, guardian, _executor) = setup();
        let client = TimelockContractClient::new(&env, &contract_id);

        let op_id = client.queue_operation(
            &admin,
            &3u64,
            &soroban_sdk::String::from_str(&env, "collateral_factor"),
            &soroban_sdk::String::from_str(&env, "9500"), // malicious value
        );

        client.cancel_operation(&guardian, &op_id);
        assert!(client.is_cancelled(&op_id));
    }

    #[test]
    fn test_admin_cancel() {
        let (env, contract_id, admin, _guardian, _executor) = setup();
        let client = TimelockContractClient::new(&env, &contract_id);

        let op_id = client.queue_operation(
            &admin,
            &4u64,
            &soroban_sdk::String::from_str(&env, "borrow_rate_bps"),
            &soroban_sdk::String::from_str(&env, "9999"),
        );

        client.cancel_operation(&admin, &op_id);
        assert!(client.is_cancelled(&op_id));
        assert!(!client.is_executed(&op_id));
    }

    #[test]
    #[should_panic(expected = "operation not queued")]
    fn test_cannot_execute_cancelled_op() {
        let (env, contract_id, admin, guardian, executor) = setup();
        let client = TimelockContractClient::new(&env, &contract_id);

        let op_id = client.queue_operation(
            &admin,
            &5u64,
            &soroban_sdk::String::from_str(&env, "collateral_factor"),
            &soroban_sdk::String::from_str(&env, "9900"),
        );

        client.cancel_operation(&guardian, &op_id);

        env.ledger().with_mut(|li| {
            li.sequence_number += DEFAULT_DELAY_LEDGERS + 1;
        });

        client.execute_operation(&executor, &op_id); // should panic
    }

    #[test]
    #[should_panic(expected = "only guardian or admin may cancel")]
    fn test_random_cannot_cancel() {
        let (env, contract_id, admin, _guardian, _executor) = setup();
        let client = TimelockContractClient::new(&env, &contract_id);
        let rando = Address::generate(&env);

        let op_id = client.queue_operation(
            &admin,
            &6u64,
            &soroban_sdk::String::from_str(&env, "collateral_factor"),
            &soroban_sdk::String::from_str(&env, "7500"),
        );

        client.cancel_operation(&rando, &op_id);
    }

    #[test]
    fn test_set_delay() {
        let (env, contract_id, admin, _guardian, _executor) = setup();
        let client = TimelockContractClient::new(&env, &contract_id);

        client.set_delay(&admin, &50_000u32);
        assert_eq!(client.get_delay(), 50_000u32);
    }

    #[test]
    fn test_set_guardian() {
        let (env, contract_id, admin, _guardian, _executor) = setup();
        let client = TimelockContractClient::new(&env, &contract_id);

        let new_guardian = Address::generate(&env);
        client.set_guardian(&admin, &new_guardian);
        assert_eq!(client.get_guardian(), new_guardian);
    }

    #[test]
    #[should_panic(expected = "operation already queued")]
    fn test_no_duplicate_queue() {
        let (env, contract_id, admin, _guardian, _executor) = setup();
        let client = TimelockContractClient::new(&env, &contract_id);

        client.queue_operation(
            &admin,
            &7u64,
            &soroban_sdk::String::from_str(&env, "collateral_factor"),
            &soroban_sdk::String::from_str(&env, "7500"),
        );
        // Same proposal_id → same eta → same hash → duplicate
        client.queue_operation(
            &admin,
            &7u64,
            &soroban_sdk::String::from_str(&env, "collateral_factor"),
            &soroban_sdk::String::from_str(&env, "7500"),
        );
    }
}
