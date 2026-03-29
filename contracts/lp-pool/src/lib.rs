#![no_std]

use soroban_sdk::{contract, contractimpl, contracttype, token, vec, Address, BytesN, Env, Vec};

const BPS_DENOMINATOR: i128 = 10_000;
const RATE_PRECISION: i128 = 10_000_000; // 1e7

/// Maximum TWAP samples stored in the circular buffer.
const TWAP_MAX_SAMPLES: u32 = 100;

// ---------- TTL constants ----------
const INSTANCE_LIFETIME_THRESHOLD: u32 = 100_800; // ~7 days
const INSTANCE_BUMP_AMOUNT: u32 = 518_400;        // bump to ~30 days
const LP_LIFETIME_THRESHOLD: u32 = 518_400;       // ~30 days
const LP_BUMP_AMOUNT: u32 = 3_110_400;            // bump to ~180 days

// ===================================================================
// DataKey — all storage keys
// ===================================================================
#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    // Core pool state
    Admin,
    SxlmToken,
    NativeToken,
    FeeBps,
    Initialized,
    ReserveXlm,
    ReserveSxlm,
    TotalLpSupply,
    LpBalance(Address),
    ProtocolFeeBps,
    AccruedProtocolFees,

    // TWAP oracle
    TwapSamples,       // Vec<TwapSample>

    // Liquidity mining
    MiningRate,               // i128 — reward tokens per ledger for the entire pool (scaled by RATE_PRECISION)
    MiningRewardVault,        // i128 — XLM available for distribution
    RewardPerLpStored,        // i128 — cumulative reward per LP unit (scaled by RATE_PRECISION)
    LastRewardLedger,         // u32
    UserRewardPerLpPaid(Address), // i128
    UserPendingRewards(Address),  // i128
}

// ===================================================================
// TWAP sample struct
// ===================================================================
#[derive(Clone)]
#[contracttype]
pub struct TwapSample {
    pub ledger: u32,
    pub price:  i128, // reserve_xlm * RATE_PRECISION / reserve_sxlm
}

// ===================================================================
// Storage helpers
// ===================================================================

fn extend_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
}

fn extend_lp_balance(env: &Env, user: &Address) {
    let key = DataKey::LpBalance(user.clone());
    if env.storage().persistent().has(&key) {
        env.storage()
            .persistent()
            .extend_ttl(&key, LP_LIFETIME_THRESHOLD, LP_BUMP_AMOUNT);
    }
}

fn read_i128(env: &Env, key: &DataKey) -> i128 {
    env.storage().instance().get(key).unwrap_or(0)
}

fn write_i128(env: &Env, key: &DataKey, val: i128) {
    env.storage().instance().set(key, &val);
}

fn read_u32(env: &Env, key: &DataKey) -> u32 {
    env.storage().instance().get(key).unwrap_or(0u32)
}

fn write_u32(env: &Env, key: &DataKey, val: u32) {
    env.storage().instance().set(key, &val);
}

fn read_sxlm_token(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::SxlmToken).unwrap()
}

fn read_native_token(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::NativeToken).unwrap()
}

fn read_fee_bps(env: &Env) -> i128 {
    env.storage()
        .instance()
        .get(&DataKey::FeeBps)
        .unwrap_or(30)
}

fn read_admin(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::Admin).unwrap()
}

fn read_lp_balance(env: &Env, user: &Address) -> i128 {
    let key = DataKey::LpBalance(user.clone());
    let val: i128 = env.storage().persistent().get(&key).unwrap_or(0);
    if val > 0 {
        env.storage()
            .persistent()
            .extend_ttl(&key, LP_LIFETIME_THRESHOLD, LP_BUMP_AMOUNT);
    }
    val
}

fn write_lp_balance(env: &Env, user: &Address, val: i128) {
    let key = DataKey::LpBalance(user.clone());
    env.storage().persistent().set(&key, &val);
    env.storage()
        .persistent()
        .extend_ttl(&key, LP_LIFETIME_THRESHOLD, LP_BUMP_AMOUNT);
}

fn read_user_reward_paid(env: &Env, user: &Address) -> i128 {
    let key = DataKey::UserRewardPerLpPaid(user.clone());
    env.storage().persistent().get(&key).unwrap_or(0i128)
}

fn write_user_reward_paid(env: &Env, user: &Address, val: i128) {
    let key = DataKey::UserRewardPerLpPaid(user.clone());
    env.storage().persistent().set(&key, &val);
    env.storage()
        .persistent()
        .extend_ttl(&key, LP_LIFETIME_THRESHOLD, LP_BUMP_AMOUNT);
}

fn read_user_pending_rewards(env: &Env, user: &Address) -> i128 {
    let key = DataKey::UserPendingRewards(user.clone());
    env.storage().persistent().get(&key).unwrap_or(0i128)
}

fn write_user_pending_rewards(env: &Env, user: &Address, val: i128) {
    let key = DataKey::UserPendingRewards(user.clone());
    env.storage().persistent().set(&key, &val);
    env.storage()
        .persistent()
        .extend_ttl(&key, LP_LIFETIME_THRESHOLD, LP_BUMP_AMOUNT);
}

// ===================================================================
// TWAP helpers
// ===================================================================

fn read_twap_samples(env: &Env) -> Vec<TwapSample> {
    env.storage()
        .instance()
        .get(&DataKey::TwapSamples)
        .unwrap_or_else(|| vec![env])
}

fn write_twap_samples(env: &Env, samples: &Vec<TwapSample>) {
    env.storage().instance().set(&DataKey::TwapSamples, samples);
}

/// Push a new price sample into the circular buffer.
/// Called after every swap so the buffer tracks price history.
fn record_twap_sample(env: &Env, reserve_xlm: i128, reserve_sxlm: i128) {
    if reserve_sxlm == 0 {
        return;
    }
    let price = reserve_xlm * RATE_PRECISION / reserve_sxlm;
    let current_ledger = env.ledger().sequence();

    let mut samples = read_twap_samples(env);

    // If the last sample is from the same ledger, just update it (no manipulation)
    let len = samples.len();
    if len > 0 {
        let last = samples.get(len - 1).unwrap();
        if last.ledger == current_ledger {
            // Replace in place — rebuild because Vec has no set
            let mut new_samples: Vec<TwapSample> = vec![env];
            for i in 0..(len - 1) {
                new_samples.push_back(samples.get(i).unwrap());
            }
            new_samples.push_back(TwapSample { ledger: current_ledger, price });
            write_twap_samples(env, &new_samples);
            return;
        }
    }

    // Drop oldest sample if at capacity
    if samples.len() >= TWAP_MAX_SAMPLES {
        let mut trimmed: Vec<TwapSample> = vec![env];
        for i in 1..samples.len() {
            trimmed.push_back(samples.get(i).unwrap());
        }
        samples = trimmed;
    }

    samples.push_back(TwapSample { ledger: current_ledger, price });
    write_twap_samples(env, &samples);
}

// ===================================================================
// Liquidity mining helpers
// ===================================================================

/// Accrue global rewards up to current ledger, then update user's earned amount.
/// Must be called BEFORE any LP balance change.
fn accrue_rewards(env: &Env, user: &Address) {
    let current_ledger = env.ledger().sequence();
    let last_reward_ledger = read_u32(env, &DataKey::LastRewardLedger);
    let total_lp = read_i128(env, &DataKey::TotalLpSupply);
    let mining_rate = read_i128(env, &DataKey::MiningRate);
    let mut reward_per_lp = read_i128(env, &DataKey::RewardPerLpStored);

    // Advance global accumulator
    if total_lp > 0 && mining_rate > 0 && current_ledger > last_reward_ledger {
        let elapsed = (current_ledger - last_reward_ledger) as i128;
        // total rewards this period / total_lp  (both scaled by RATE_PRECISION)
        let delta = elapsed * mining_rate / total_lp;
        reward_per_lp += delta;
        write_i128(env, &DataKey::RewardPerLpStored, reward_per_lp);
    }
    write_u32(env, &DataKey::LastRewardLedger, current_ledger);

    // Credit user
    let user_lp = read_lp_balance(env, user);
    let paid = read_user_reward_paid(env, user);
    let pending = read_user_pending_rewards(env, user);
    let earned = user_lp * (reward_per_lp - paid) / RATE_PRECISION;
    if earned > 0 {
        write_user_pending_rewards(env, user, pending + earned);
    }
    write_user_reward_paid(env, user, reward_per_lp);
}

/// Integer square root using Newton's method.
fn isqrt(n: i128) -> i128 {
    if n <= 0 {
        return 0;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

// ===================================================================
// Contract
// ===================================================================

#[contract]
pub struct LpPoolContract;

#[contractimpl]
impl LpPoolContract {
    // ------------------------------------------------------------------
    // Initialization & admin
    // ------------------------------------------------------------------

    pub fn initialize(
        env: Env,
        admin: Address,
        sxlm_token: Address,
        native_token: Address,
        fee_bps: u32,
    ) {
        let already: bool = env.storage().instance().get(&DataKey::Initialized).unwrap_or(false);
        if already {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Initialized, &true);
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::SxlmToken, &sxlm_token);
        env.storage().instance().set(&DataKey::NativeToken, &native_token);
        env.storage().instance().set(&DataKey::FeeBps, &(fee_bps as i128));
        write_i128(&env, &DataKey::ProtocolFeeBps, 5);
        write_i128(&env, &DataKey::AccruedProtocolFees, 0);
        write_i128(&env, &DataKey::MiningRate, 0);
        write_i128(&env, &DataKey::MiningRewardVault, 0);
        write_i128(&env, &DataKey::RewardPerLpStored, 0);
        write_u32(&env, &DataKey::LastRewardLedger, env.ledger().sequence());
        extend_instance(&env);
    }

    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) {
        let admin = read_admin(&env);
        admin.require_auth();
        env.deployer().update_current_contract_wasm(new_wasm_hash);
    }

    pub fn bump_instance(env: Env) {
        extend_instance(&env);
    }

    // ------------------------------------------------------------------
    // Liquidity provision
    // ------------------------------------------------------------------

    /// Add liquidity to the pool. Returns LP tokens minted.
    pub fn add_liquidity(env: Env, user: Address, xlm_amount: i128, sxlm_amount: i128) -> i128 {
        user.require_auth();
        assert!(xlm_amount > 0 && sxlm_amount > 0, "amounts must be positive");
        extend_instance(&env);

        // Accrue rewards before balance changes
        accrue_rewards(&env, &user);

        let reserve_xlm = read_i128(&env, &DataKey::ReserveXlm);
        let reserve_sxlm = read_i128(&env, &DataKey::ReserveSxlm);
        let total_lp = read_i128(&env, &DataKey::TotalLpSupply);

        let (actual_xlm, actual_sxlm, lp_minted) = if total_lp == 0 {
            (xlm_amount, sxlm_amount, isqrt(xlm_amount * sxlm_amount))
        } else {
            let lp_from_xlm = xlm_amount * total_lp / reserve_xlm;
            let lp_from_sxlm = sxlm_amount * total_lp / reserve_sxlm;
            if lp_from_xlm < lp_from_sxlm {
                let needed_sxlm = lp_from_xlm * reserve_sxlm / total_lp;
                (xlm_amount, needed_sxlm, lp_from_xlm)
            } else {
                let needed_xlm = lp_from_sxlm * reserve_xlm / total_lp;
                (needed_xlm, sxlm_amount, lp_from_sxlm)
            }
        };
        assert!(lp_minted > 0, "insufficient liquidity minted");
        assert!(actual_xlm > 0 && actual_sxlm > 0, "zero deposit");

        let native = read_native_token(&env);
        let sxlm = read_sxlm_token(&env);
        token::Client::new(&env, &native).transfer(&user, &env.current_contract_address(), &actual_xlm);
        token::Client::new(&env, &sxlm).transfer(&user, &env.current_contract_address(), &actual_sxlm);

        write_i128(&env, &DataKey::ReserveXlm, reserve_xlm + actual_xlm);
        write_i128(&env, &DataKey::ReserveSxlm, reserve_sxlm + actual_sxlm);
        write_i128(&env, &DataKey::TotalLpSupply, total_lp + lp_minted);

        let user_lp = read_lp_balance(&env, &user);
        write_lp_balance(&env, &user, user_lp + lp_minted);

        env.events().publish(
            (soroban_sdk::symbol_short!("add_liq"),),
            (user, actual_xlm, actual_sxlm, lp_minted),
        );

        lp_minted
    }

    /// Remove liquidity from the pool. Returns (xlm_out, sxlm_out).
    pub fn remove_liquidity(env: Env, user: Address, lp_amount: i128) -> (i128, i128) {
        user.require_auth();
        assert!(lp_amount > 0, "amount must be positive");
        extend_instance(&env);

        // Accrue rewards before balance changes
        accrue_rewards(&env, &user);

        let user_lp = read_lp_balance(&env, &user);
        assert!(user_lp >= lp_amount, "insufficient LP balance");

        let reserve_xlm = read_i128(&env, &DataKey::ReserveXlm);
        let reserve_sxlm = read_i128(&env, &DataKey::ReserveSxlm);
        let total_lp = read_i128(&env, &DataKey::TotalLpSupply);

        let xlm_out = lp_amount * reserve_xlm / total_lp;
        let sxlm_out = lp_amount * reserve_sxlm / total_lp;

        assert!(xlm_out > 0 && sxlm_out > 0, "insufficient output");

        write_i128(&env, &DataKey::ReserveXlm, reserve_xlm - xlm_out);
        write_i128(&env, &DataKey::ReserveSxlm, reserve_sxlm - sxlm_out);
        write_i128(&env, &DataKey::TotalLpSupply, total_lp - lp_amount);
        write_lp_balance(&env, &user, user_lp - lp_amount);

        let native = read_native_token(&env);
        let sxlm = read_sxlm_token(&env);
        token::Client::new(&env, &native).transfer(&env.current_contract_address(), &user, &xlm_out);
        token::Client::new(&env, &sxlm).transfer(&env.current_contract_address(), &user, &sxlm_out);

        env.events().publish(
            (soroban_sdk::symbol_short!("rm_liq"),),
            (user, lp_amount, xlm_out, sxlm_out),
        );

        (xlm_out, sxlm_out)
    }

    // ------------------------------------------------------------------
    // Swaps
    // ------------------------------------------------------------------

    /// Swap XLM for sXLM. Returns sXLM received.
    pub fn swap_xlm_to_sxlm(env: Env, user: Address, xlm_amount: i128, min_out: i128) -> i128 {
        user.require_auth();
        assert!(xlm_amount > 0, "amount must be positive");
        extend_instance(&env);

        let fee_bps = read_fee_bps(&env);
        let amount_after_fee = xlm_amount * (BPS_DENOMINATOR - fee_bps) / BPS_DENOMINATOR;

        let reserve_xlm = read_i128(&env, &DataKey::ReserveXlm);
        let reserve_sxlm = read_i128(&env, &DataKey::ReserveSxlm);
        assert!(reserve_xlm > 0 && reserve_sxlm > 0, "pool has no liquidity");

        let sxlm_out = reserve_sxlm - (reserve_xlm * reserve_sxlm) / (reserve_xlm + amount_after_fee);
        assert!(sxlm_out > 0 && sxlm_out < reserve_sxlm, "insufficient liquidity");
        assert!(sxlm_out >= min_out, "slippage: output below minimum");

        let native = read_native_token(&env);
        let sxlm = read_sxlm_token(&env);
        token::Client::new(&env, &native).transfer(&user, &env.current_contract_address(), &xlm_amount);
        token::Client::new(&env, &sxlm).transfer(&env.current_contract_address(), &user, &sxlm_out);

        let total_fee = xlm_amount - amount_after_fee;
        let protocol_fee_bps = read_i128(&env, &DataKey::ProtocolFeeBps);
        let protocol_cut = total_fee * protocol_fee_bps / fee_bps;

        let new_reserve_xlm = reserve_xlm + xlm_amount - protocol_cut;
        let new_reserve_sxlm = reserve_sxlm - sxlm_out;

        write_i128(&env, &DataKey::ReserveXlm, new_reserve_xlm);
        write_i128(&env, &DataKey::ReserveSxlm, new_reserve_sxlm);

        let accrued = read_i128(&env, &DataKey::AccruedProtocolFees);
        write_i128(&env, &DataKey::AccruedProtocolFees, accrued + protocol_cut);

        // Record TWAP sample after reserves are updated
        record_twap_sample(&env, new_reserve_xlm, new_reserve_sxlm);

        env.events().publish(
            (soroban_sdk::symbol_short!("swap"),),
            (user, xlm_amount, sxlm_out),
        );

        sxlm_out
    }

    /// Swap sXLM for XLM. Returns XLM received.
    pub fn swap_sxlm_to_xlm(env: Env, user: Address, sxlm_amount: i128, min_out: i128) -> i128 {
        user.require_auth();
        assert!(sxlm_amount > 0, "amount must be positive");
        extend_instance(&env);

        let fee_bps = read_fee_bps(&env);
        let amount_after_fee = sxlm_amount * (BPS_DENOMINATOR - fee_bps) / BPS_DENOMINATOR;

        let reserve_xlm = read_i128(&env, &DataKey::ReserveXlm);
        let reserve_sxlm = read_i128(&env, &DataKey::ReserveSxlm);
        assert!(reserve_xlm > 0 && reserve_sxlm > 0, "pool has no liquidity");

        let xlm_out = reserve_xlm - (reserve_xlm * reserve_sxlm) / (reserve_sxlm + amount_after_fee);
        assert!(xlm_out > 0 && xlm_out < reserve_xlm, "insufficient liquidity");
        assert!(xlm_out >= min_out, "slippage: output below minimum");

        let native = read_native_token(&env);
        let sxlm = read_sxlm_token(&env);
        token::Client::new(&env, &sxlm).transfer(&user, &env.current_contract_address(), &sxlm_amount);
        token::Client::new(&env, &native).transfer(&env.current_contract_address(), &user, &xlm_out);

        let new_reserve_sxlm = reserve_sxlm + sxlm_amount;
        let new_reserve_xlm = reserve_xlm - xlm_out;

        write_i128(&env, &DataKey::ReserveSxlm, new_reserve_sxlm);
        write_i128(&env, &DataKey::ReserveXlm, new_reserve_xlm);

        // Record TWAP sample after reserves are updated
        record_twap_sample(&env, new_reserve_xlm, new_reserve_sxlm);

        env.events().publish(
            (soroban_sdk::symbol_short!("swap"),),
            (user, sxlm_amount, xlm_out),
        );

        xlm_out
    }

    // ------------------------------------------------------------------
    // Protocol fee management
    // ------------------------------------------------------------------

    pub fn collect_protocol_fees(env: Env) -> i128 {
        let admin = read_admin(&env);
        admin.require_auth();
        extend_instance(&env);

        let accrued = read_i128(&env, &DataKey::AccruedProtocolFees);
        if accrued <= 0 {
            return 0;
        }

        let native = read_native_token(&env);
        token::Client::new(&env, &native).transfer(&env.current_contract_address(), &admin, &accrued);
        write_i128(&env, &DataKey::AccruedProtocolFees, 0);

        env.events().publish(
            (soroban_sdk::symbol_short!("pf_col"),),
            (admin, accrued),
        );

        accrued
    }

    pub fn set_protocol_fee_bps(env: Env, bps: u32) {
        let admin = read_admin(&env);
        admin.require_auth();
        extend_instance(&env);
        write_i128(&env, &DataKey::ProtocolFeeBps, bps as i128);
    }

    // ------------------------------------------------------------------
    // TWAP oracle
    // ------------------------------------------------------------------

    /// Returns the TWAP price of sXLM in XLM (scaled by RATE_PRECISION = 1e7).
    /// `period_ledgers` is the look-back window (e.g. 720 ≈ 1 hour at 5s/ledger).
    /// Falls back to spot price when fewer than 2 samples exist in the window.
    pub fn get_twap(env: Env, period_ledgers: u32) -> i128 {
        extend_instance(&env);
        let samples = read_twap_samples(&env);
        let len = samples.len();

        if len == 0 {
            // No history — return spot price
            return Self::get_price(env);
        }

        let current_ledger = env.ledger().sequence();
        let cutoff = current_ledger.saturating_sub(period_ledgers);

        // Collect samples inside the window
        let mut weighted_sum: i128 = 0;
        let mut total_weight: i128 = 0;
        let mut prev_ledger: u32 = 0;
        let mut prev_price: i128 = 0;
        let mut first = true;

        for i in 0..len {
            let s = samples.get(i).unwrap();
            if s.ledger < cutoff {
                // Save as boundary seed even if outside window
                prev_ledger = s.ledger;
                prev_price = s.price;
                first = false;
                continue;
            }
            // This sample is inside the window
            if first {
                // No prior sample — weight starts from cutoff
                let weight = (s.ledger.saturating_sub(cutoff)) as i128;
                // Use spot at cutoff as the starting price (best approximation)
                weighted_sum += prev_price * weight;
                total_weight += weight;
                first = false;
            } else {
                let weight = (s.ledger - prev_ledger) as i128;
                weighted_sum += prev_price * weight;
                total_weight += weight;
            }
            prev_ledger = s.ledger;
            prev_price = s.price;
        }

        // Add tail segment from last sample to current ledger
        if !first && prev_ledger < current_ledger {
            let weight = (current_ledger - prev_ledger) as i128;
            weighted_sum += prev_price * weight;
            total_weight += weight;
        }

        if total_weight == 0 {
            // Only one sample or very short window — return that sample's price
            return samples.get(len - 1).unwrap().price;
        }

        weighted_sum / total_weight
    }

    /// Returns all stored TWAP samples (for indexer / frontend display).
    pub fn get_twap_samples(env: Env) -> Vec<TwapSample> {
        extend_instance(&env);
        read_twap_samples(&env)
    }

    // ------------------------------------------------------------------
    // Liquidity mining
    // ------------------------------------------------------------------

    /// Set the per-ledger mining reward rate. Governance-controlled.
    /// `rate` is XLM stroops per ledger distributed across all LP holders.
    /// Callable by admin (governance contract after timelock).
    pub fn set_mining_rate(env: Env, admin: Address, rate: i128) {
        admin.require_auth();
        let stored_admin = read_admin(&env);
        assert!(admin == stored_admin, "not admin");
        extend_instance(&env);

        // Accrue with old rate before changing — use contract address as dummy
        // (global accumulator only, no user balance)
        let current_ledger = env.ledger().sequence();
        let last = read_u32(&env, &DataKey::LastRewardLedger);
        let total_lp = read_i128(&env, &DataKey::TotalLpSupply);
        let old_rate = read_i128(&env, &DataKey::MiningRate);
        let mut reward_per_lp = read_i128(&env, &DataKey::RewardPerLpStored);

        if total_lp > 0 && old_rate > 0 && current_ledger > last {
            let elapsed = (current_ledger - last) as i128;
            reward_per_lp += elapsed * old_rate / total_lp;
            write_i128(&env, &DataKey::RewardPerLpStored, reward_per_lp);
        }
        write_u32(&env, &DataKey::LastRewardLedger, current_ledger);
        write_i128(&env, &DataKey::MiningRate, rate);

        env.events().publish(
            (soroban_sdk::symbol_short!("mine_rt"),),
            (rate,),
        );
    }

    /// Fund the mining reward vault. Anyone can top up.
    /// Transfers `amount` of native XLM from `funder` into the pool's vault.
    pub fn fund_mining_rewards(env: Env, funder: Address, amount: i128) {
        funder.require_auth();
        assert!(amount > 0, "amount must be positive");
        extend_instance(&env);

        let native = read_native_token(&env);
        token::Client::new(&env, &native).transfer(&funder, &env.current_contract_address(), &amount);

        let vault = read_i128(&env, &DataKey::MiningRewardVault);
        write_i128(&env, &DataKey::MiningRewardVault, vault + amount);

        env.events().publish(
            (soroban_sdk::symbol_short!("mine_fd"),),
            (funder, amount),
        );
    }

    /// Claim accumulated mining rewards. Transfers XLM from the reward vault.
    pub fn claim_mining_rewards(env: Env, user: Address) -> i128 {
        user.require_auth();
        extend_instance(&env);

        // Accrue up to current ledger for this user
        accrue_rewards(&env, &user);

        let pending = read_user_pending_rewards(&env, &user);
        if pending <= 0 {
            return 0;
        }

        let vault = read_i128(&env, &DataKey::MiningRewardVault);
        let claimable = if pending > vault { vault } else { pending };
        assert!(claimable > 0, "reward vault empty");

        write_user_pending_rewards(&env, &user, pending - claimable);
        write_i128(&env, &DataKey::MiningRewardVault, vault - claimable);

        let native = read_native_token(&env);
        token::Client::new(&env, &native).transfer(&env.current_contract_address(), &user, &claimable);

        env.events().publish(
            (soroban_sdk::symbol_short!("mine_cl"),),
            (user, claimable),
        );

        claimable
    }

    // ------------------------------------------------------------------
    // Views
    // ------------------------------------------------------------------

    pub fn accrued_protocol_fees(env: Env) -> i128 {
        extend_instance(&env);
        read_i128(&env, &DataKey::AccruedProtocolFees)
    }

    pub fn protocol_fee_bps(env: Env) -> i128 {
        extend_instance(&env);
        read_i128(&env, &DataKey::ProtocolFeeBps)
    }

    /// Returns (reserve_xlm, reserve_sxlm).
    pub fn get_reserves(env: Env) -> (i128, i128) {
        extend_instance(&env);
        (
            read_i128(&env, &DataKey::ReserveXlm),
            read_i128(&env, &DataKey::ReserveSxlm),
        )
    }

    /// Returns spot price of sXLM in XLM (scaled by RATE_PRECISION = 1e7).
    pub fn get_price(env: Env) -> i128 {
        extend_instance(&env);
        let reserve_xlm = read_i128(&env, &DataKey::ReserveXlm);
        let reserve_sxlm = read_i128(&env, &DataKey::ReserveSxlm);
        if reserve_sxlm == 0 {
            return RATE_PRECISION; // 1:1 default
        }
        reserve_xlm * RATE_PRECISION / reserve_sxlm
    }

    pub fn get_lp_balance(env: Env, user: Address) -> i128 {
        extend_instance(&env);
        extend_lp_balance(&env, &user);
        read_lp_balance(&env, &user)
    }

    pub fn total_lp_supply(env: Env) -> i128 {
        extend_instance(&env);
        read_i128(&env, &DataKey::TotalLpSupply)
    }

    /// Returns pending (unclaimed) mining rewards for a user.
    pub fn get_pending_rewards(env: Env, user: Address) -> i128 {
        extend_instance(&env);
        let current_ledger = env.ledger().sequence();
        let last = read_u32(&env, &DataKey::LastRewardLedger);
        let total_lp = read_i128(&env, &DataKey::TotalLpSupply);
        let mining_rate = read_i128(&env, &DataKey::MiningRate);
        let mut reward_per_lp = read_i128(&env, &DataKey::RewardPerLpStored);

        // Project forward without writing
        if total_lp > 0 && mining_rate > 0 && current_ledger > last {
            let elapsed = (current_ledger - last) as i128;
            reward_per_lp += elapsed * mining_rate / total_lp;
        }

        let user_lp = read_lp_balance(&env, &user);
        let paid = read_user_reward_paid(&env, &user);
        let pending = read_user_pending_rewards(&env, &user);
        let earned = user_lp * (reward_per_lp - paid) / RATE_PRECISION;
        pending + earned.max(0)
    }

    /// Returns (mining_rate, reward_vault_balance, reward_per_lp_stored).
    pub fn get_mining_stats(env: Env) -> (i128, i128, i128) {
        extend_instance(&env);
        (
            read_i128(&env, &DataKey::MiningRate),
            read_i128(&env, &DataKey::MiningRewardVault),
            read_i128(&env, &DataKey::RewardPerLpStored),
        )
    }
}

// ===================================================================
// Tests
// ===================================================================

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger as _};
    use soroban_sdk::{token::StellarAssetClient, Env};

    fn setup_test() -> (Env, Address, Address, Address, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        let sxlm_id = env.register_stellar_asset_contract_v2(Address::generate(&env)).address();
        let native_id = env.register_stellar_asset_contract_v2(Address::generate(&env)).address();

        let contract_id = env.register_contract(None, LpPoolContract);
        let client = LpPoolContractClient::new(&env, &contract_id);
        client.initialize(&admin, &sxlm_id, &native_id, &30);

        StellarAssetClient::new(&env, &sxlm_id).mint(&user, &1_000_000_0000000);
        StellarAssetClient::new(&env, &native_id).mint(&user, &1_000_000_0000000);

        (env, contract_id, sxlm_id, native_id, user, admin)
    }

    #[test]
    fn test_initialize() {
        let (env, contract_id, _, _, _, _) = setup_test();
        let client = LpPoolContractClient::new(&env, &contract_id);
        let (rx, rs) = client.get_reserves();
        assert_eq!(rx, 0);
        assert_eq!(rs, 0);
        assert_eq!(client.total_lp_supply(), 0);
    }

    #[test]
    fn test_add_liquidity_first() {
        let (env, contract_id, _, _, user, _) = setup_test();
        let client = LpPoolContractClient::new(&env, &contract_id);

        let lp = client.add_liquidity(&user, &10_000_0000000, &10_000_0000000);
        assert!(lp > 0);
        assert_eq!(client.get_lp_balance(&user), lp);
        assert_eq!(client.total_lp_supply(), lp);

        let (rx, rs) = client.get_reserves();
        assert_eq!(rx, 10_000_0000000);
        assert_eq!(rs, 10_000_0000000);
    }

    #[test]
    fn test_add_and_remove_liquidity() {
        let (env, contract_id, _, _, user, _) = setup_test();
        let client = LpPoolContractClient::new(&env, &contract_id);

        let lp = client.add_liquidity(&user, &10_000_0000000, &10_000_0000000);
        let (xlm_out, sxlm_out) = client.remove_liquidity(&user, &(lp / 2));
        assert!(xlm_out > 0);
        assert!(sxlm_out > 0);
    }

    #[test]
    fn test_swap_xlm_to_sxlm() {
        let (env, contract_id, _, _, user, _) = setup_test();
        let client = LpPoolContractClient::new(&env, &contract_id);

        client.add_liquidity(&user, &100_000_0000000, &100_000_0000000);
        let sxlm_out = client.swap_xlm_to_sxlm(&user, &1_000_0000000, &0);
        assert!(sxlm_out > 0);
        assert!(sxlm_out < 1_000_0000000);
    }

    #[test]
    fn test_swap_sxlm_to_xlm() {
        let (env, contract_id, _, _, user, _) = setup_test();
        let client = LpPoolContractClient::new(&env, &contract_id);

        client.add_liquidity(&user, &100_000_0000000, &100_000_0000000);
        let xlm_out = client.swap_sxlm_to_xlm(&user, &1_000_0000000, &0);
        assert!(xlm_out > 0);
        assert!(xlm_out < 1_000_0000000);
    }

    #[test]
    fn test_get_price() {
        let (env, contract_id, _, _, user, _) = setup_test();
        let client = LpPoolContractClient::new(&env, &contract_id);

        client.add_liquidity(&user, &100_000_0000000, &100_000_0000000);
        let price = client.get_price();
        assert_eq!(price, 10_000_000); // 1:1
    }

    #[test]
    fn test_price_changes_after_swap() {
        let (env, contract_id, _, _, user, _) = setup_test();
        let client = LpPoolContractClient::new(&env, &contract_id);

        client.add_liquidity(&user, &100_000_0000000, &100_000_0000000);
        client.swap_xlm_to_sxlm(&user, &10_000_0000000, &0);

        let price = client.get_price();
        assert!(price > 10_000_000);
    }

    #[test]
    fn test_constant_product_invariant() {
        let (env, contract_id, _, _, user, _) = setup_test();
        let client = LpPoolContractClient::new(&env, &contract_id);

        client.add_liquidity(&user, &100_000_0000000, &100_000_0000000);
        let (rx0, rs0) = client.get_reserves();
        let k_before = rx0 * rs0;

        client.swap_xlm_to_sxlm(&user, &5_000_0000000, &0);
        let (rx1, rs1) = client.get_reserves();
        let k_after = rx1 * rs1;

        assert!(k_after >= k_before);
    }

    #[test]
    fn test_protocol_fee_collection() {
        let (env, contract_id, _, native_id, user, admin) = setup_test();
        let client = LpPoolContractClient::new(&env, &contract_id);

        client.add_liquidity(&user, &100_000_0000000, &100_000_0000000);
        assert_eq!(client.accrued_protocol_fees(), 0);
        assert_eq!(client.protocol_fee_bps(), 5);

        client.swap_xlm_to_sxlm(&user, &10_000_0000000, &0);

        let accrued = client.accrued_protocol_fees();
        assert!(accrued > 0);
        assert_eq!(accrued, 5_0000000);

        let admin_before = token::Client::new(&env, &native_id).balance(&admin);
        let collected = client.collect_protocol_fees();
        let admin_after = token::Client::new(&env, &native_id).balance(&admin);

        assert_eq!(collected, accrued);
        assert_eq!(admin_after - admin_before, accrued);
        assert_eq!(client.accrued_protocol_fees(), 0);
    }

    #[test]
    fn test_sxlm_to_xlm_no_protocol_fee() {
        let (env, contract_id, _, _, user, _) = setup_test();
        let client = LpPoolContractClient::new(&env, &contract_id);

        client.add_liquidity(&user, &100_000_0000000, &100_000_0000000);
        client.swap_sxlm_to_xlm(&user, &10_000_0000000, &0);
        assert_eq!(client.accrued_protocol_fees(), 0);
    }

    // ------------------------------------------------------------------
    // TWAP tests
    // ------------------------------------------------------------------

    #[test]
    fn test_twap_records_samples_on_swap() {
        let (env, contract_id, _, _, user, _) = setup_test();
        let client = LpPoolContractClient::new(&env, &contract_id);

        client.add_liquidity(&user, &100_000_0000000, &100_000_0000000);

        // No samples before first swap
        assert_eq!(client.get_twap_samples().len(), 0);

        client.swap_xlm_to_sxlm(&user, &1_000_0000000, &0);
        assert_eq!(client.get_twap_samples().len(), 1);

        // Advance ledger and do another swap
        env.ledger().with_mut(|li| li.sequence_number += 100);
        client.swap_sxlm_to_xlm(&user, &500_0000000, &0);
        assert_eq!(client.get_twap_samples().len(), 2);
    }

    #[test]
    fn test_twap_equals_spot_with_one_sample() {
        let (env, contract_id, _, _, user, _) = setup_test();
        let client = LpPoolContractClient::new(&env, &contract_id);

        client.add_liquidity(&user, &100_000_0000000, &100_000_0000000);
        client.swap_xlm_to_sxlm(&user, &1_000_0000000, &0);

        // With a single sample, TWAP should equal that sample's price
        let twap = client.get_twap(&720);
        let spot = client.get_price();
        // They should be very close (TWAP includes tail segment to current ledger)
        assert!(twap > 0);
        // Both should be > 1:1 since we swapped XLM in
        assert!(twap > 10_000_000);
        assert_eq!(spot, twap); // only one sample, tail weight = 0
    }

    #[test]
    fn test_twap_time_weighted_average() {
        let (env, contract_id, _, _, user, _) = setup_test();
        let client = LpPoolContractClient::new(&env, &contract_id);

        client.add_liquidity(&user, &100_000_0000000, &100_000_0000000);

        // Swap 1: creates sample at current ledger (price slightly > 1e7)
        client.swap_xlm_to_sxlm(&user, &10_000_0000000, &0);
        let price_after_swap1 = client.get_price();

        // Advance 600 ledgers (~50 min) then swap sXLM→XLM
        env.ledger().with_mut(|li| li.sequence_number += 600);
        client.swap_sxlm_to_xlm(&user, &10_000_0000000, &0);
        let price_after_swap2 = client.get_price();

        // Advance 120 more ledgers then get TWAP over 720-ledger window
        env.ledger().with_mut(|li| li.sequence_number += 120);
        let twap = client.get_twap(&720);

        // TWAP should be between the two prices (time-weighted)
        let low = price_after_swap1.min(price_after_swap2);
        let high = price_after_swap1.max(price_after_swap2);
        assert!(twap >= low && twap <= high, "TWAP {twap} not in range [{low}, {high}]");
    }

    // ------------------------------------------------------------------
    // Liquidity mining tests
    // ------------------------------------------------------------------

    #[test]
    fn test_liquidity_mining_basic() {
        let (env, contract_id, _, native_id, user, admin) = setup_test();
        let client = LpPoolContractClient::new(&env, &contract_id);

        // Fund the reward vault (1000 XLM)
        StellarAssetClient::new(&env, &native_id).mint(&admin, &1_000_0000000);
        client.fund_mining_rewards(&admin, &1_000_0000000);
        let (_, vault, _) = client.get_mining_stats();
        assert_eq!(vault, 1_000_0000000);

        // Set mining rate: 1_000_000 reward units per ledger for the whole pool
        client.set_mining_rate(&admin, &1_000_000);

        // User provides liquidity
        client.add_liquidity(&user, &100_000_0000000, &100_000_0000000);

        // Advance 100 ledgers
        env.ledger().with_mut(|li| li.sequence_number += 100);

        // Check pending rewards
        let pending = client.get_pending_rewards(&user);
        assert!(pending > 0, "should have accrued rewards");

        // Claim
        let user_balance_before = token::Client::new(&env, &native_id).balance(&user);
        let claimed = client.claim_mining_rewards(&user);
        let user_balance_after = token::Client::new(&env, &native_id).balance(&user);

        assert!(claimed > 0);
        assert_eq!(user_balance_after - user_balance_before, claimed);
        assert_eq!(client.get_pending_rewards(&user), 0);
    }

    #[test]
    fn test_mining_rewards_proportional_to_lp_share() {
        let (env, contract_id, sxlm_id, native_id, user, admin) = setup_test();
        let user2 = Address::generate(&env);
        StellarAssetClient::new(&env, &sxlm_id).mint(&user2, &1_000_000_0000000);
        StellarAssetClient::new(&env, &native_id).mint(&user2, &1_000_000_0000000);

        let client = LpPoolContractClient::new(&env, &contract_id);

        StellarAssetClient::new(&env, &native_id).mint(&admin, &10_000_0000000);
        client.fund_mining_rewards(&admin, &10_000_0000000);
        client.set_mining_rate(&admin, &10_000_000); // higher rate for clear numbers

        // user1 provides 2x the liquidity of user2
        client.add_liquidity(&user, &200_000_0000000, &200_000_0000000);
        client.add_liquidity(&user2, &100_000_0000000, &100_000_0000000);

        env.ledger().with_mut(|li| li.sequence_number += 300);

        let pending1 = client.get_pending_rewards(&user);
        let pending2 = client.get_pending_rewards(&user2);

        assert!(pending1 > 0 && pending2 > 0);
        // user1 has ~2x the LP share, should earn ~2x rewards
        // (within rounding tolerance)
        let ratio = pending1 * 100 / pending2;
        assert!(ratio >= 190 && ratio <= 210, "ratio {ratio} should be ~200");
    }

    #[test]
    fn test_set_mining_rate_governance() {
        let (env, contract_id, _, _, _, admin) = setup_test();
        let client = LpPoolContractClient::new(&env, &contract_id);

        client.set_mining_rate(&admin, &5_000_000);
        let (rate, _, _) = client.get_mining_stats();
        assert_eq!(rate, 5_000_000);

        // Update rate
        client.set_mining_rate(&admin, &2_000_000);
        let (rate2, _, _) = client.get_mining_stats();
        assert_eq!(rate2, 2_000_000);
    }

    #[test]
    fn test_rewards_stop_when_vault_empty() {
        let (env, contract_id, _, native_id, user, admin) = setup_test();
        let client = LpPoolContractClient::new(&env, &contract_id);

        // Fund with small amount
        StellarAssetClient::new(&env, &native_id).mint(&admin, &100);
        client.fund_mining_rewards(&admin, &100);
        client.set_mining_rate(&admin, &1_000_000);

        client.add_liquidity(&user, &100_000_0000000, &100_000_0000000);

        env.ledger().with_mut(|li| li.sequence_number += 1000);

        // Should claim up to vault balance
        let claimed = client.claim_mining_rewards(&user);
        assert_eq!(claimed, 100); // capped by vault
        let (_, vault, _) = client.get_mining_stats();
        assert_eq!(vault, 0);
    }
}
