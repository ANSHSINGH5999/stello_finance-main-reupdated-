#![no_std]

//! # sXLM Governance Contract
//!
//! On-chain parameter governance for the sXLM liquid staking protocol.
//!
//! ## Lifecycle of a proposal
//!
//! ```text
//! create_proposal()  →  vote()  →  execute_proposal()  →  [48 h timelock]  →  finalize_proposal()
//!                                       (enters timelock queue)              (applies param on-chain)
//! ```
//!
//! `execute_proposal` validates quorum / majority and enqueues the change in the
//! companion Timelock contract.  Only after the mandatory delay may anyone call
//! `finalize_proposal`, which cross-checks the Timelock state and writes the
//! parameter value on-chain.
//!
//! At any point before finalization the guardian (or admin) may call
//! `cancel_proposal` to abort the change via the Timelock contract.

use soroban_sdk::{contract, contractimpl, contracttype, token, Address, Bytes, BytesN, Env, IntoVal, String, Symbol};

const BPS_DENOMINATOR: i128 = 10_000;
const MIN_PROPOSAL_BALANCE: i128 = 100_0000000; // 100 sXLM minimum to create a proposal

// ---------------------------------------------------------------------------
// TTL constants (mirror timelock contract)
// ---------------------------------------------------------------------------
const INSTANCE_LIFETIME_THRESHOLD: u32 = 100_800; // ~7 days
const INSTANCE_BUMP_AMOUNT: u32 = 518_400;        // bump to ~30 days
const PROPOSAL_LIFETIME_THRESHOLD: u32 = 518_400; // ~30 days
const PROPOSAL_BUMP_AMOUNT: u32 = 3_110_400;      // bump to ~180 days

// ---------------------------------------------------------------------------
// Storage keys
// ---------------------------------------------------------------------------

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    Admin,
    SxlmToken,
    /// Address of the companion Timelock contract.
    TimelockContract,
    VotingPeriodLedgers,
    QuorumBps,
    Initialized,
    ProposalCount,
    Proposal(u64),
    Vote(u64, Address),
    /// Approved governance parameter values (set by finalize_proposal).
    Param(String),
    /// Total sXLM supply reference for quorum calculation (set by admin).
    ReferenceSupply,
    /// Maps proposal_id → timelock op_id (BytesN<32>).
    ProposalOpId(u64),
}

// ---------------------------------------------------------------------------
// Proposal
// ---------------------------------------------------------------------------

#[derive(Clone)]
#[contracttype]
pub struct Proposal {
    pub id: u64,
    pub proposer: Address,
    pub param_key: String,
    pub new_value: String,
    pub votes_for: i128,
    pub votes_against: i128,
    pub start_ledger: u32,
    pub end_ledger: u32,
    /// True once the proposal has been finalized (param applied on-chain).
    pub executed: bool,
    /// True once the proposal has passed voting and entered the timelock queue.
    pub queued: bool,
}

// ---------------------------------------------------------------------------
// Storage helpers
// ---------------------------------------------------------------------------

fn extend_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
}

fn extend_proposal(env: &Env, id: u64) {
    let key = DataKey::Proposal(id);
    if env.storage().persistent().has(&key) {
        env.storage()
            .persistent()
            .extend_ttl(&key, PROPOSAL_LIFETIME_THRESHOLD, PROPOSAL_BUMP_AMOUNT);
    }
}

fn extend_vote(env: &Env, proposal_id: u64, voter: &Address) {
    let key = DataKey::Vote(proposal_id, voter.clone());
    if env.storage().persistent().has(&key) {
        env.storage()
            .persistent()
            .extend_ttl(&key, PROPOSAL_LIFETIME_THRESHOLD, PROPOSAL_BUMP_AMOUNT);
    }
}

fn read_admin(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::Admin).unwrap()
}

fn read_sxlm_token(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::SxlmToken).unwrap()
}

fn read_voting_period(env: &Env) -> u32 {
    env.storage()
        .instance()
        .get(&DataKey::VotingPeriodLedgers)
        .unwrap_or(17_280u32) // ~24 hours
}

fn read_quorum_bps(env: &Env) -> i128 {
    env.storage()
        .instance()
        .get(&DataKey::QuorumBps)
        .unwrap_or(1000) // 10%
}

fn next_proposal_id(env: &Env) -> u64 {
    let id: u64 = env
        .storage()
        .instance()
        .get(&DataKey::ProposalCount)
        .unwrap_or(0);
    env.storage()
        .instance()
        .set(&DataKey::ProposalCount, &(id + 1));
    id
}

fn read_proposal(env: &Env, id: u64) -> Proposal {
    let key = DataKey::Proposal(id);
    let proposal: Proposal = env.storage().persistent().get(&key).unwrap();
    env.storage()
        .persistent()
        .extend_ttl(&key, PROPOSAL_LIFETIME_THRESHOLD, PROPOSAL_BUMP_AMOUNT);
    proposal
}

fn write_proposal(env: &Env, proposal: &Proposal) {
    let key = DataKey::Proposal(proposal.id);
    env.storage().persistent().set(&key, proposal);
    env.storage()
        .persistent()
        .extend_ttl(&key, PROPOSAL_LIFETIME_THRESHOLD, PROPOSAL_BUMP_AMOUNT);
}

fn has_voted(env: &Env, proposal_id: u64, voter: &Address) -> bool {
    let key = DataKey::Vote(proposal_id, voter.clone());
    let val: bool = env.storage().persistent().get(&key).unwrap_or(false);
    if val {
        env.storage()
            .persistent()
            .extend_ttl(&key, PROPOSAL_LIFETIME_THRESHOLD, PROPOSAL_BUMP_AMOUNT);
    }
    val
}

fn set_voted(env: &Env, proposal_id: u64, voter: &Address) {
    let key = DataKey::Vote(proposal_id, voter.clone());
    env.storage().persistent().set(&key, &true);
    env.storage()
        .persistent()
        .extend_ttl(&key, PROPOSAL_LIFETIME_THRESHOLD, PROPOSAL_BUMP_AMOUNT);
}

/// Read the timelock contract address. Panics if not configured.
fn read_timelock(env: &Env) -> Address {
    env.storage()
        .instance()
        .get(&DataKey::TimelockContract)
        .unwrap_or_else(|| panic!("timelock contract not configured"))
}

/// Compute the operation ID the timelock will assign.
/// Mirrors timelock::compute_op_id: SHA-256(proposal_id_be || eta_be).
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
pub struct GovernanceContract;

#[contractimpl]
impl GovernanceContract {
    // -----------------------------------------------------------------------
    // Lifecycle
    // -----------------------------------------------------------------------

    /// One-time initialization.
    ///
    /// `timelock_contract` may be set to admin's own address as a placeholder and
    /// updated later via `set_timelock_contract` once the timelock is deployed.
    pub fn initialize(
        env: Env,
        admin: Address,
        sxlm_token: Address,
        voting_period_ledgers: u32,
        quorum_bps: u32,
    ) {
        let already: bool = env.storage().instance().get(&DataKey::Initialized).unwrap_or(false);
        assert!(!already, "already initialized");

        env.storage().instance().set(&DataKey::Initialized, &true);
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::SxlmToken, &sxlm_token);
        env.storage()
            .instance()
            .set(&DataKey::VotingPeriodLedgers, &voting_period_ledgers);
        env.storage()
            .instance()
            .set(&DataKey::QuorumBps, &(quorum_bps as i128));
        env.storage().instance().set(&DataKey::ReferenceSupply, &0i128);
        extend_instance(&env);
    }

    /// Set or update the companion Timelock contract address.  Admin-only.
    ///
    /// Call this after deploying the timelock if it was not available at
    /// initialization time.
    pub fn set_timelock_contract(env: Env, admin: Address, timelock: Address) {
        admin.require_auth();
        assert!(admin == read_admin(&env), "only admin");
        env.storage()
            .instance()
            .set(&DataKey::TimelockContract, &timelock);
        extend_instance(&env);

        env.events().publish(
            (soroban_sdk::symbol_short!("tl_set"),),
            timelock,
        );
    }

    /// Upgrade the contract WASM.  Admin-only.
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) {
        let admin = read_admin(&env);
        admin.require_auth();
        env.deployer().update_current_contract_wasm(new_wasm_hash);
    }

    /// Bump instance TTL — callable by anyone to keep contract alive.
    pub fn bump_instance(env: Env) {
        extend_instance(&env);
    }

    /// Set the reference total supply used for quorum calculation.  Admin-only.
    pub fn set_reference_supply(env: Env, supply: i128) {
        let admin = read_admin(&env);
        admin.require_auth();
        assert!(supply >= 0, "supply must be non-negative");
        extend_instance(&env);
        env.storage().instance().set(&DataKey::ReferenceSupply, &supply);
    }

    // -----------------------------------------------------------------------
    // Proposal creation
    // -----------------------------------------------------------------------

    /// Create a new governance proposal.  Proposer must hold ≥ 100 sXLM.
    pub fn create_proposal(
        env: Env,
        proposer: Address,
        param_key: String,
        new_value: String,
    ) -> u64 {
        proposer.require_auth();
        extend_instance(&env);

        let sxlm = read_sxlm_token(&env);
        let balance = token::Client::new(&env, &sxlm).balance(&proposer);
        assert!(
            balance >= MIN_PROPOSAL_BALANCE,
            "insufficient sXLM to create proposal"
        );

        let id = next_proposal_id(&env);
        let current_ledger = env.ledger().sequence();
        let voting_period = read_voting_period(&env);

        let proposal = Proposal {
            id,
            proposer: proposer.clone(),
            param_key: param_key.clone(),
            new_value: new_value.clone(),
            votes_for: 0,
            votes_against: 0,
            start_ledger: current_ledger,
            end_ledger: current_ledger + voting_period,
            executed: false,
            queued: false,
        };

        write_proposal(&env, &proposal);

        env.events().publish(
            (soroban_sdk::symbol_short!("propose"),),
            (id, proposer, param_key),
        );

        id
    }

    // -----------------------------------------------------------------------
    // Voting
    // -----------------------------------------------------------------------

    /// Vote on a proposal.  Vote weight equals the voter's sXLM balance.
    pub fn vote(env: Env, voter: Address, proposal_id: u64, support: bool) {
        voter.require_auth();
        extend_instance(&env);
        extend_vote(&env, proposal_id, &voter);

        let mut proposal = read_proposal(&env, proposal_id);

        let current_ledger = env.ledger().sequence();
        assert!(current_ledger <= proposal.end_ledger, "voting period has ended");
        assert!(!has_voted(&env, proposal_id, &voter), "already voted");

        let sxlm = read_sxlm_token(&env);
        let weight = token::Client::new(&env, &sxlm).balance(&voter);
        assert!(weight > 0, "no sXLM to vote with");

        if support {
            proposal.votes_for += weight;
        } else {
            proposal.votes_against += weight;
        }

        set_voted(&env, proposal_id, &voter);
        write_proposal(&env, &proposal);

        env.events().publish(
            (soroban_sdk::symbol_short!("voted"),),
            (proposal_id, voter, support, weight),
        );
    }

    // -----------------------------------------------------------------------
    // Timelock queue entry
    // -----------------------------------------------------------------------

    /// Validate that a proposal passed and enqueue it in the Timelock contract.
    ///
    /// Can be called by anyone once the voting period has ended and the proposal
    /// has achieved quorum with more votes-for than votes-against.
    ///
    /// Returns the timelock operation ID.
    pub fn execute_proposal(env: Env, proposal_id: u64) -> BytesN<32> {
        extend_instance(&env);
        extend_proposal(&env, proposal_id);

        let mut proposal = read_proposal(&env, proposal_id);

        assert!(!proposal.executed, "proposal already finalized");
        assert!(!proposal.queued, "proposal already in timelock queue");

        let current_ledger = env.ledger().sequence();
        assert!(current_ledger > proposal.end_ledger, "voting period not ended");

        let total_votes = proposal.votes_for + proposal.votes_against;
        assert!(total_votes > 0, "no votes cast");

        let quorum_bps = read_quorum_bps(&env);
        let reference_supply: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ReferenceSupply)
            .unwrap_or(0);
        if reference_supply > 0 {
            let min_votes_required = reference_supply * quorum_bps / BPS_DENOMINATOR;
            assert!(total_votes >= min_votes_required, "quorum not met");
        }

        assert!(proposal.votes_for > proposal.votes_against, "proposal did not pass");

        // Enqueue in timelock — cross-contract call.
        // The timelock verifies caller == admin (this contract's address) via require_auth.
        let timelock_addr = read_timelock(&env);
        let self_addr = env.current_contract_address();

        let op_id: BytesN<32> = env.invoke_contract(
            &timelock_addr,
            &Symbol::new(&env, "queue_operation"),
            soroban_sdk::vec![
                &env,
                self_addr.into_val(&env),
                proposal_id.into_val(&env),
                proposal.param_key.clone().into_val(&env),
                proposal.new_value.clone().into_val(&env),
            ],
        );

        // Persist the timelock op_id so finalize_proposal can retrieve it.
        let op_id_key = DataKey::ProposalOpId(proposal_id);
        env.storage().persistent().set(&op_id_key, &op_id);
        env.storage().persistent().extend_ttl(
            &op_id_key,
            PROPOSAL_LIFETIME_THRESHOLD,
            PROPOSAL_BUMP_AMOUNT,
        );

        proposal.queued = true;
        write_proposal(&env, &proposal);

        env.events().publish(
            (soroban_sdk::symbol_short!("queued"),),
            (proposal_id, op_id.clone()),
        );

        op_id
    }

    // -----------------------------------------------------------------------
    // Finalization (after timelock delay)
    // -----------------------------------------------------------------------

    /// Apply the parameter change after the timelock delay has elapsed.
    ///
    /// Anyone may call this.  The call cross-checks the Timelock contract to
    /// verify the operation has been executed there first.
    pub fn finalize_proposal(env: Env, proposal_id: u64) {
        extend_instance(&env);
        extend_proposal(&env, proposal_id);

        let mut proposal = read_proposal(&env, proposal_id);

        assert!(proposal.queued, "proposal not in timelock queue");
        assert!(!proposal.executed, "proposal already finalized");

        // Retrieve the stored timelock op_id.
        let op_id_key = DataKey::ProposalOpId(proposal_id);
        let op_id: BytesN<32> = env
            .storage()
            .persistent()
            .get(&op_id_key)
            .unwrap_or_else(|| panic!("timelock op id not found"));

        // Cross-call timelock to verify execution.
        let timelock_addr = read_timelock(&env);
        let is_executed: bool = env.invoke_contract(
            &timelock_addr,
            &Symbol::new(&env, "is_executed"),
            soroban_sdk::vec![&env, op_id.into_val(&env)],
        );
        assert!(is_executed, "timelock operation not yet executed");

        // Write the approved parameter value on-chain.
        let param_key = DataKey::Param(proposal.param_key.clone());
        env.storage()
            .persistent()
            .set(&param_key, &proposal.new_value);
        env.storage().persistent().extend_ttl(
            &param_key,
            PROPOSAL_LIFETIME_THRESHOLD,
            PROPOSAL_BUMP_AMOUNT,
        );

        proposal.executed = true;
        write_proposal(&env, &proposal);

        env.events().publish(
            (soroban_sdk::symbol_short!("finalized"),),
            (proposal_id, proposal.param_key, proposal.new_value),
        );
    }

    // -----------------------------------------------------------------------
    // Emergency cancellation
    // -----------------------------------------------------------------------

    /// Cancel a queued proposal via the Timelock contract.
    ///
    /// Callable by the admin (protocol admin key) which is also the
    /// timelock admin.  Guardians may cancel directly through the Timelock
    /// contract using their own address.
    pub fn cancel_proposal(env: Env, admin: Address, proposal_id: u64) {
        admin.require_auth();
        assert!(admin == read_admin(&env), "only admin may cancel via governance");

        let proposal = read_proposal(&env, proposal_id);
        assert!(proposal.queued, "proposal is not in timelock queue");
        assert!(!proposal.executed, "proposal already finalized");

        let op_id: BytesN<32> = env
            .storage()
            .persistent()
            .get(&DataKey::ProposalOpId(proposal_id))
            .unwrap_or_else(|| panic!("timelock op id not found"));

        // Governance contract is the timelock admin — passes itself as canceller.
        let timelock_addr = read_timelock(&env);
        let self_addr = env.current_contract_address();

        env.invoke_contract::<()>(
            &timelock_addr,
            &Symbol::new(&env, "cancel_operation"),
            soroban_sdk::vec![
                &env,
                self_addr.into_val(&env),
                op_id.clone().into_val(&env),
            ],
        );

        env.events().publish(
            (soroban_sdk::symbol_short!("cancelled"),),
            (proposal_id, op_id),
        );
    }

    // -----------------------------------------------------------------------
    // Views
    // -----------------------------------------------------------------------

    pub fn get_proposal(env: Env, id: u64) -> Proposal {
        extend_instance(&env);
        read_proposal(&env, id)
    }

    pub fn proposal_count(env: Env) -> u64 {
        extend_instance(&env);
        env.storage()
            .instance()
            .get(&DataKey::ProposalCount)
            .unwrap_or(0)
    }

    pub fn get_vote_count(env: Env, id: u64) -> (i128, i128) {
        extend_instance(&env);
        let proposal = read_proposal(&env, id);
        (proposal.votes_for, proposal.votes_against)
    }

    /// Read the op_id stored for a queued proposal.
    pub fn get_proposal_op_id(env: Env, proposal_id: u64) -> BytesN<32> {
        extend_instance(&env);
        env.storage()
            .persistent()
            .get(&DataKey::ProposalOpId(proposal_id))
            .unwrap_or_else(|| panic!("no op id for proposal"))
    }

    /// Read an approved governance parameter value.
    pub fn get_param(env: Env, key: String) -> String {
        extend_instance(&env);
        let param_key = DataKey::Param(key);
        let val: String = env
            .storage()
            .persistent()
            .get(&param_key)
            .unwrap_or(String::from_str(&env, ""));
        if env.storage().persistent().has(&param_key) {
            env.storage().persistent().extend_ttl(
                &param_key,
                PROPOSAL_LIFETIME_THRESHOLD,
                PROPOSAL_BUMP_AMOUNT,
            );
        }
        val
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::{token::StellarAssetClient, Env, String};

    // -----------------------------------------------------------------------
    // Minimal timelock mock
    // -----------------------------------------------------------------------
    // In real deployment the full timelock contract is used. For unit tests we
    // register a thin mock that exercises the governance contract logic in isolation.

    mod mock_timelock {
        use soroban_sdk::{contract, contractimpl, contracttype, BytesN, Env, String};

        #[derive(Clone)]
        #[contracttype]
        pub enum TlKey {
            Executed(BytesN<32>),
            OpId,
        }

        #[contract]
        pub struct MockTimelock;

        #[contractimpl]
        impl MockTimelock {
            /// Always returns a deterministic op_id derived from proposal_id.
            pub fn queue_operation(
                env: Env,
                _caller: soroban_sdk::Address,
                proposal_id: u64,
                _param_key: String,
                _new_value: String,
            ) -> BytesN<32> {
                let mut data = soroban_sdk::Bytes::new(&env);
                for b in proposal_id.to_be_bytes().iter() {
                    data.push_back(*b);
                }
                // Use fixed eta=0 for simplicity in tests
                for _b in 0u32.to_be_bytes().iter() {
                    data.push_back(0u8);
                }
                env.crypto().sha256(&data)
            }

            pub fn is_executed(env: Env, op_id: BytesN<32>) -> bool {
                env.storage()
                    .persistent()
                    .get::<TlKey, bool>(&TlKey::Executed(op_id))
                    .unwrap_or(false)
            }

            /// Test helper: mark op as executed.
            pub fn mock_execute(env: Env, op_id: BytesN<32>) {
                env.storage()
                    .persistent()
                    .set(&TlKey::Executed(op_id), &true);
            }

            pub fn cancel_operation(
                _env: Env,
                _canceller: soroban_sdk::Address,
                _op_id: BytesN<32>,
            ) {
                // no-op in mock
            }
        }
    }

    fn setup_test() -> (Env, Address, Address, Address, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let proposer = Address::generate(&env);
        let voter = Address::generate(&env);

        let sxlm_id = env
            .register_stellar_asset_contract_v2(Address::generate(&env))
            .address();
        let timelock_id = env.register_contract(None, mock_timelock::MockTimelock);
        let contract_id = env.register_contract(None, GovernanceContract);

        let client = GovernanceContractClient::new(&env, &contract_id);
        client.initialize(&admin, &sxlm_id, &100, &1000); // 100 ledgers voting, 10% quorum
        client.set_timelock_contract(&admin, &timelock_id);

        // Mint sXLM
        let sxlm_admin = StellarAssetClient::new(&env, &sxlm_id);
        sxlm_admin.mint(&proposer, &10_000_0000000);
        sxlm_admin.mint(&voter, &5_000_0000000);

        (env, contract_id, timelock_id, sxlm_id, proposer, voter)
    }

    #[test]
    fn test_initialize() {
        let (env, contract_id, _, _, _, _) = setup_test();
        let client = GovernanceContractClient::new(&env, &contract_id);
        assert_eq!(client.proposal_count(), 0);
    }

    #[test]
    fn test_create_proposal() {
        let (env, contract_id, _, _, proposer, _) = setup_test();
        let client = GovernanceContractClient::new(&env, &contract_id);

        let id = client.create_proposal(
            &proposer,
            &String::from_str(&env, "protocol_fee_bps"),
            &String::from_str(&env, "500"),
        );
        assert_eq!(id, 0);
        assert_eq!(client.proposal_count(), 1);

        let p = client.get_proposal(&0);
        assert_eq!(p.votes_for, 0);
        assert!(!p.executed);
        assert!(!p.queued);
    }

    #[test]
    fn test_vote() {
        let (env, contract_id, _, _, proposer, voter) = setup_test();
        let client = GovernanceContractClient::new(&env, &contract_id);

        client.create_proposal(
            &proposer,
            &String::from_str(&env, "protocol_fee_bps"),
            &String::from_str(&env, "500"),
        );

        client.vote(&voter, &0, &true);

        let (votes_for, votes_against) = client.get_vote_count(&0);
        assert_eq!(votes_for, 5_000_0000000);
        assert_eq!(votes_against, 0);
    }

    #[test]
    #[should_panic(expected = "already voted")]
    fn test_double_vote() {
        let (env, contract_id, _, _, proposer, voter) = setup_test();
        let client = GovernanceContractClient::new(&env, &contract_id);

        client.create_proposal(
            &proposer,
            &String::from_str(&env, "protocol_fee_bps"),
            &String::from_str(&env, "500"),
        );

        client.vote(&voter, &0, &true);
        client.vote(&voter, &0, &false); // should panic
    }

    #[test]
    fn test_execute_proposal_enters_timelock() {
        let (env, contract_id, _timelock_id, _, proposer, voter) = setup_test();
        let client = GovernanceContractClient::new(&env, &contract_id);

        client.create_proposal(
            &proposer,
            &String::from_str(&env, "collateral_factor"),
            &String::from_str(&env, "7500"),
        );

        client.vote(&proposer, &0, &true);
        client.vote(&voter, &0, &true);

        env.ledger().with_mut(|li| li.sequence_number += 101);

        let _op_id = client.execute_proposal(&0);

        let p = client.get_proposal(&0);
        assert!(p.queued);
        assert!(!p.executed); // not finalized yet — still in timelock
    }

    #[test]
    fn test_finalize_proposal_stores_param() {
        let (env, contract_id, timelock_id, _, proposer, voter) = setup_test();
        let client = GovernanceContractClient::new(&env, &contract_id);
        let tl_client = mock_timelock::MockTimelockClient::new(&env, &timelock_id);

        client.create_proposal(
            &proposer,
            &String::from_str(&env, "collateral_factor"),
            &String::from_str(&env, "7500"),
        );

        client.vote(&proposer, &0, &true);
        client.vote(&voter, &0, &true);

        env.ledger().with_mut(|li| li.sequence_number += 101);

        let op_id = client.execute_proposal(&0);

        // Simulate timelock executing the operation
        tl_client.mock_execute(&op_id);

        client.finalize_proposal(&0);

        let p = client.get_proposal(&0);
        assert!(p.executed);

        let value = client.get_param(&String::from_str(&env, "collateral_factor"));
        assert_eq!(value, String::from_str(&env, "7500"));
    }

    #[test]
    #[should_panic(expected = "timelock operation not yet executed")]
    fn test_cannot_finalize_before_timelock_executes() {
        let (env, contract_id, _, _, proposer, voter) = setup_test();
        let client = GovernanceContractClient::new(&env, &contract_id);

        client.create_proposal(
            &proposer,
            &String::from_str(&env, "collateral_factor"),
            &String::from_str(&env, "7500"),
        );

        client.vote(&proposer, &0, &true);
        client.vote(&voter, &0, &true);

        env.ledger().with_mut(|li| li.sequence_number += 101);

        client.execute_proposal(&0);
        // Do NOT call tl_client.mock_execute — timelock not yet done
        client.finalize_proposal(&0); // should panic
    }

    #[test]
    #[should_panic(expected = "voting period not ended")]
    fn test_execute_too_early() {
        let (env, contract_id, _, _, proposer, voter) = setup_test();
        let client = GovernanceContractClient::new(&env, &contract_id);

        client.create_proposal(
            &proposer,
            &String::from_str(&env, "fee"),
            &String::from_str(&env, "100"),
        );

        client.vote(&proposer, &0, &true);
        client.vote(&voter, &0, &true);

        // Don't advance ledger
        client.execute_proposal(&0);
    }

    #[test]
    #[should_panic(expected = "proposal did not pass")]
    fn test_execute_failed_proposal() {
        let (env, contract_id, _, _, proposer, voter) = setup_test();
        let client = GovernanceContractClient::new(&env, &contract_id);

        client.create_proposal(
            &proposer,
            &String::from_str(&env, "fee"),
            &String::from_str(&env, "100"),
        );

        client.vote(&proposer, &0, &false); // 10k against
        client.vote(&voter, &0, &true); // 5k for

        env.ledger().with_mut(|li| li.sequence_number += 101);

        client.execute_proposal(&0); // should panic
    }

    #[test]
    fn test_vote_against() {
        let (env, contract_id, _, _, proposer, voter) = setup_test();
        let client = GovernanceContractClient::new(&env, &contract_id);

        client.create_proposal(
            &proposer,
            &String::from_str(&env, "fee"),
            &String::from_str(&env, "100"),
        );

        client.vote(&voter, &0, &false);

        let (votes_for, votes_against) = client.get_vote_count(&0);
        assert_eq!(votes_for, 0);
        assert_eq!(votes_against, 5_000_0000000);
    }

    #[test]
    fn test_get_param_default() {
        let (env, contract_id, _, _, _, _) = setup_test();
        let client = GovernanceContractClient::new(&env, &contract_id);
        let val = client.get_param(&String::from_str(&env, "nonexistent"));
        assert_eq!(val, String::from_str(&env, ""));
    }

    #[test]
    fn test_cancel_proposal() {
        let (env, contract_id, _, _, proposer, voter) = setup_test();
        let admin = Address::generate(&env);
        // Re-setup with explicit admin to test cancel
        let sxlm_id = env
            .register_stellar_asset_contract_v2(Address::generate(&env))
            .address();
        let timelock_id = env.register_contract(None, mock_timelock::MockTimelock);
        let gov_id = env.register_contract(None, GovernanceContract);
        let client = GovernanceContractClient::new(&env, &gov_id);
        client.initialize(&admin, &sxlm_id, &100, &1000);
        client.set_timelock_contract(&admin, &timelock_id);

        let sxlm_admin = StellarAssetClient::new(&env, &sxlm_id);
        sxlm_admin.mint(&proposer, &10_000_0000000);
        sxlm_admin.mint(&voter, &5_000_0000000);

        client.create_proposal(
            &proposer,
            &String::from_str(&env, "collateral_factor"),
            &String::from_str(&env, "9900"), // "malicious"
        );

        client.vote(&proposer, &0, &true);
        client.vote(&voter, &0, &true);

        env.ledger().with_mut(|li| li.sequence_number += 101);
        client.execute_proposal(&0);

        // Admin cancels the queued proposal
        client.cancel_proposal(&admin, &0);

        let p = client.get_proposal(&0);
        assert!(p.queued);    // still marked queued in governance state
        assert!(!p.executed); // was not finalized
    }
}
