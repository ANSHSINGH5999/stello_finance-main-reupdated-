#![no_std]

//! # sXLM Multi-Collateral Lending Contract
//!
//! Borrowers may deposit any whitelisted Stellar asset as collateral to borrow XLM.
//!
//! ## Supported collateral assets (example)
//! | Asset | Symbol | Collateral Factor | Notes                       |
//! |-------|--------|-------------------|-----------------------------|
//! | sXLM  | sXLM   | 75 %              | Liquid staking receipt      |
//! | USDC  | USDC   | 90 %              | Circle USD on Stellar       |
//! | EURC  | EURC   | 88 %              | Circle EUR on Stellar       |
//! | yXLM  | yXLM   | 70 %              | Yield-bearing XLM wrapper   |
//!
//! ## Price oracle
//! Each collateral asset has an admin-settable XLM price (scaled by `RATE_PRECISION = 1e7`).
//! For sXLM the price is the staking exchange rate updated by the backend after every reward
//! snapshot.  External assets (USDC, EURC) are priced by the Stellar DEX TWAP reported
//! by the backend oracle keeper.
//!
//! ## Health factor
//! ```
//! HF = Σ(user_balance[a] × price[a] × cf_bps[a]) / (BPS × RATE_PRECISION × total_borrowed)
//! ```
//! When HF < 1.0 the position is liquidatable.
//!
//! ## Liquidation
//! Liquidator repays the full XLM debt and receives pro-rata seizure of every collateral
//! asset the borrower holds, plus the global liquidation bonus.

use soroban_sdk::{contract, contractimpl, contracttype, token, Address, BytesN, Env, Vec};

const BPS_DENOMINATOR: i128 = 10_000;
const RATE_PRECISION: i128 = 10_000_000; // 1e7
const DEFAULT_LIQUIDATION_BONUS_BPS: i128 = 500; // 5% bonus on seized collateral
const MAX_ASSETS: u32 = 16; // maximum number of supported collateral assets

// ---------- TTL constants ----------
const INSTANCE_LIFETIME_THRESHOLD: u32 = 100_800; // ~7 days
const INSTANCE_BUMP_AMOUNT: u32 = 518_400;        // ~30 days
const USER_LIFETIME_THRESHOLD: u32 = 518_400;     // ~30 days
const USER_BUMP_AMOUNT: u32 = 3_110_400;          // ~180 days

// ---------------------------------------------------------------------------
// Storage layout
// ---------------------------------------------------------------------------

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    Admin,
    /// The sXLM token address (first supported collateral).
    SxlmToken,
    NativeToken,
    /// sXLM → XLM exchange rate (scaled by RATE_PRECISION). Also the oracle price for sXLM.
    ExchangeRate,
    /// Global liquidation threshold (used to determine when positions become liquidatable).
    LiquidationThresholdBps,
    BorrowRateBps,
    LiquidationBonusBps,
    Initialized,
    /// Global total XLM borrowed across all users.
    TotalBorrowed,
    /// Ordered list of all whitelisted collateral asset addresses.
    AssetList,
    // Per collateral asset
    AssetEnabled(Address),
    AssetCFBps(Address),
    /// Oracle price of 1 unit of asset in XLM (scaled by RATE_PRECISION = 1e7).
    AssetPrice(Address),
    TotalAssetCollateral(Address),
    // Per user, per asset
    UserAssetBalance(Address, Address),
    // Per user
    UserBorrowed(Address),
    // Interest accounting
    TotalAccruedInterest,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn extend_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
}

fn extend_user_data(env: &Env, user: &Address, asset: &Address) {
    let key = DataKey::UserAssetBalance(user.clone(), asset.clone());
    if env.storage().persistent().has(&key) {
        env.storage()
            .persistent()
            .extend_ttl(&key, USER_LIFETIME_THRESHOLD, USER_BUMP_AMOUNT);
    }
    let bor_key = DataKey::UserBorrowed(user.clone());
    if env.storage().persistent().has(&bor_key) {
        env.storage()
            .persistent()
            .extend_ttl(&bor_key, USER_LIFETIME_THRESHOLD, USER_BUMP_AMOUNT);
    }
}

fn read_admin(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::Admin).unwrap()
}

fn read_sxlm_token(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::SxlmToken).unwrap()
}

fn read_native_token(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::NativeToken).unwrap()
}

fn read_exchange_rate(env: &Env) -> i128 {
    env.storage()
        .instance()
        .get(&DataKey::ExchangeRate)
        .unwrap_or(RATE_PRECISION)
}

fn read_i128_instance(env: &Env, key: &DataKey) -> i128 {
    env.storage().instance().get(key).unwrap_or(0)
}

fn write_i128_instance(env: &Env, key: &DataKey, val: i128) {
    env.storage().instance().set(key, &val);
}

fn read_user_asset_balance(env: &Env, user: &Address, asset: &Address) -> i128 {
    let key = DataKey::UserAssetBalance(user.clone(), asset.clone());
    let val: i128 = env.storage().persistent().get(&key).unwrap_or(0);
    if val > 0 {
        env.storage()
            .persistent()
            .extend_ttl(&key, USER_LIFETIME_THRESHOLD, USER_BUMP_AMOUNT);
    }
    val
}

fn write_user_asset_balance(env: &Env, user: &Address, asset: &Address, val: i128) {
    let key = DataKey::UserAssetBalance(user.clone(), asset.clone());
    env.storage().persistent().set(&key, &val);
    env.storage()
        .persistent()
        .extend_ttl(&key, USER_LIFETIME_THRESHOLD, USER_BUMP_AMOUNT);
}

fn read_user_borrowed(env: &Env, user: &Address) -> i128 {
    let key = DataKey::UserBorrowed(user.clone());
    let val: i128 = env.storage().persistent().get(&key).unwrap_or(0);
    if val > 0 {
        env.storage()
            .persistent()
            .extend_ttl(&key, USER_LIFETIME_THRESHOLD, USER_BUMP_AMOUNT);
    }
    val
}

fn write_user_borrowed(env: &Env, user: &Address, val: i128) {
    let key = DataKey::UserBorrowed(user.clone());
    env.storage().persistent().set(&key, &val);
    env.storage()
        .persistent()
        .extend_ttl(&key, USER_LIFETIME_THRESHOLD, USER_BUMP_AMOUNT);
}

fn read_asset_list(env: &Env) -> Vec<Address> {
    env.storage()
        .instance()
        .get(&DataKey::AssetList)
        .unwrap_or(Vec::new(env))
}

fn write_asset_list(env: &Env, list: &Vec<Address>) {
    env.storage().instance().set(&DataKey::AssetList, list);
}

/// Oracle price of `asset` in XLM, scaled by RATE_PRECISION.
/// For sXLM this falls back to the ExchangeRate.
fn asset_price(env: &Env, asset: &Address) -> i128 {
    let sxlm = read_sxlm_token(env);
    if asset == &sxlm {
        return read_exchange_rate(env);
    }
    env.storage()
        .instance()
        .get(&DataKey::AssetPrice(asset.clone()))
        .unwrap_or(RATE_PRECISION) // default 1:1 if oracle not set
}

/// Collateral factor for `asset` in basis points.
fn asset_cf_bps(env: &Env, asset: &Address) -> i128 {
    env.storage()
        .instance()
        .get(&DataKey::AssetCFBps(asset.clone()))
        .unwrap_or(7000) // 70% default
}

/// Compute the user's total weighted collateral value across all supported assets.
/// Returns value in XLM stroops (with BPS and RATE_PRECISION already divided out).
fn total_weighted_collateral(env: &Env, user: &Address, factor_bps_key: fn(&Env, &Address) -> i128) -> i128 {
    let assets = read_asset_list(env);
    let mut total: i128 = 0;
    for asset in assets.iter() {
        let balance = read_user_asset_balance(env, user, &asset);
        if balance <= 0 {
            continue;
        }
        let price = asset_price(env, &asset);
        let cf = factor_bps_key(env, &asset);
        // weighted_value = balance * price * cf / (BPS * RATE_PRECISION)
        total += balance * price * cf / (BPS_DENOMINATOR * RATE_PRECISION);
    }
    total
}

/// Health factor using the global liquidation threshold per-asset vs borrow.
/// Returns HF scaled by RATE_PRECISION (1e7 = 1.0).
fn compute_health_factor(env: &Env, user: &Address) -> i128 {
    let borrowed = read_user_borrowed(env, user);
    if borrowed == 0 {
        return i128::MAX;
    }
    let lt_bps: i128 = env.storage()
        .instance()
        .get(&DataKey::LiquidationThresholdBps)
        .unwrap_or(8000);

    let assets = read_asset_list(env);
    let mut weighted: i128 = 0;
    for asset in assets.iter() {
        let balance = read_user_asset_balance(env, user, &asset);
        if balance <= 0 {
            continue;
        }
        let price = asset_price(env, &asset);
        // Use liquidation threshold (not collateral factor) for liquidation check
        weighted += balance * price * lt_bps / (BPS_DENOMINATOR * RATE_PRECISION);
    }
    // HF = weighted / borrowed — we scale by RATE_PRECISION to express as 1e7 = 1.0
    weighted * RATE_PRECISION / borrowed
}

/// Max XLM borrowable for a user given their current collateral (uses collateral factor).
fn max_borrow(env: &Env, user: &Address) -> i128 {
    total_weighted_collateral(env, user, asset_cf_bps)
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct LendingContract;

#[contractimpl]
impl LendingContract {
    // -----------------------------------------------------------------------
    // Lifecycle
    // -----------------------------------------------------------------------

    pub fn initialize(
        env: Env,
        admin: Address,
        sxlm_token: Address,
        native_token: Address,
        collateral_factor_bps: u32,
        liquidation_threshold_bps: u32,
        borrow_rate_bps: u32,
    ) {
        let already: bool = env.storage().instance().get(&DataKey::Initialized).unwrap_or(false);
        assert!(!already, "already initialized");
        env.storage().instance().set(&DataKey::Initialized, &true);
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::SxlmToken, &sxlm_token);
        env.storage().instance().set(&DataKey::NativeToken, &native_token);
        env.storage().instance().set(&DataKey::LiquidationThresholdBps, &(liquidation_threshold_bps as i128));
        env.storage().instance().set(&DataKey::BorrowRateBps, &(borrow_rate_bps as i128));
        env.storage().instance().set(&DataKey::LiquidationBonusBps, &DEFAULT_LIQUIDATION_BONUS_BPS);
        env.storage().instance().set(&DataKey::ExchangeRate, &RATE_PRECISION);
        write_i128_instance(&env, &DataKey::TotalBorrowed, 0);

        // Register sXLM as the first collateral asset
        let mut asset_list = Vec::new(&env);
        asset_list.push_back(sxlm_token.clone());
        write_asset_list(&env, &asset_list);
        env.storage().instance().set(&DataKey::AssetEnabled(sxlm_token.clone()), &true);
        env.storage().instance().set(&DataKey::AssetCFBps(sxlm_token.clone()), &(collateral_factor_bps as i128));
        write_i128_instance(&env, &DataKey::TotalAssetCollateral(sxlm_token), 0);

        extend_instance(&env);
    }

    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) {
        read_admin(&env).require_auth();
        env.deployer().update_current_contract_wasm(new_wasm_hash);
    }

    pub fn bump_instance(env: Env) {
        extend_instance(&env);
    }

    // -----------------------------------------------------------------------
    // Asset management (admin)
    // -----------------------------------------------------------------------

    /// Add a new collateral asset.  Admin-only.
    ///
    /// * `asset`          — Stellar asset contract address
    /// * `cf_bps`         — Collateral factor (e.g., 9000 for USDC at 90%)
    /// * `initial_price`  — Oracle price of 1 unit in XLM × 1e7 (e.g., 1e7 for USDC if 1 USDC = 1 XLM)
    pub fn add_collateral_asset(
        env: Env,
        admin: Address,
        asset: Address,
        cf_bps: u32,
        initial_price: i128,
    ) {
        admin.require_auth();
        assert!(admin == read_admin(&env), "only admin");
        assert!(cf_bps > 0 && cf_bps <= 10_000, "invalid cf_bps");
        assert!(initial_price > 0, "price must be positive");

        let already_enabled: bool = env
            .storage()
            .instance()
            .get(&DataKey::AssetEnabled(asset.clone()))
            .unwrap_or(false);
        assert!(!already_enabled, "asset already registered");

        let mut list = read_asset_list(&env);
        assert!(list.len() < MAX_ASSETS, "max collateral assets reached");
        list.push_back(asset.clone());
        write_asset_list(&env, &list);

        env.storage().instance().set(&DataKey::AssetEnabled(asset.clone()), &true);
        env.storage().instance().set(&DataKey::AssetCFBps(asset.clone()), &(cf_bps as i128));
        env.storage().instance().set(&DataKey::AssetPrice(asset.clone()), &initial_price);
        write_i128_instance(&env, &DataKey::TotalAssetCollateral(asset.clone()), 0);

        extend_instance(&env);

        env.events().publish(
            (soroban_sdk::symbol_short!("add_ast"),),
            (asset, cf_bps, initial_price),
        );
    }

    /// Update oracle price for a collateral asset.  Admin-only.
    ///
    /// Called by the backend oracle keeper after every DEX TWAP refresh.
    /// For sXLM, use `update_exchange_rate` instead.
    pub fn update_asset_price(env: Env, admin: Address, asset: Address, price: i128) {
        admin.require_auth();
        assert!(admin == read_admin(&env), "only admin");
        assert!(price > 0, "price must be positive");
        let sxlm = read_sxlm_token(&env);
        assert!(asset != sxlm, "use update_exchange_rate for sXLM");

        env.storage().instance().set(&DataKey::AssetPrice(asset.clone()), &price);
        extend_instance(&env);

        env.events().publish(
            (soroban_sdk::symbol_short!("pr_upd"),),
            (asset, price),
        );
    }

    /// Update per-asset collateral factor.  Admin-only.  Governable parameter.
    pub fn update_asset_cf(env: Env, admin: Address, asset: Address, new_cf_bps: u32) {
        admin.require_auth();
        assert!(admin == read_admin(&env), "only admin");
        assert!(new_cf_bps > 0 && new_cf_bps <= 10_000, "invalid cf_bps");
        let enabled: bool = env
            .storage()
            .instance()
            .get(&DataKey::AssetEnabled(asset.clone()))
            .unwrap_or(false);
        assert!(enabled, "asset not registered");
        env.storage().instance().set(&DataKey::AssetCFBps(asset.clone()), &(new_cf_bps as i128));
        extend_instance(&env);
    }

    /// Disable an asset so new deposits are rejected.  Admin-only.
    /// Existing positions are unaffected; users may still withdraw and liquidations still occur.
    pub fn disable_collateral_asset(env: Env, admin: Address, asset: Address) {
        admin.require_auth();
        assert!(admin == read_admin(&env), "only admin");
        let sxlm = read_sxlm_token(&env);
        assert!(asset != sxlm, "cannot disable sXLM");
        env.storage().instance().set(&DataKey::AssetEnabled(asset.clone()), &false);
        extend_instance(&env);
    }

    // -----------------------------------------------------------------------
    // Existing admin setters (kept for governance backward-compat)
    // -----------------------------------------------------------------------

    /// Update sXLM → XLM exchange rate. Used as the oracle price for sXLM.
    pub fn update_exchange_rate(env: Env, rate: i128) {
        read_admin(&env).require_auth();
        assert!(rate > 0, "rate must be positive");
        extend_instance(&env);
        env.storage().instance().set(&DataKey::ExchangeRate, &rate);
        env.events().publish((soroban_sdk::symbol_short!("er_upd"),), rate);
    }

    /// Update sXLM collateral factor.  Kept for backward-compat with governance.
    pub fn update_collateral_factor(env: Env, new_cf_bps: u32) {
        read_admin(&env).require_auth();
        assert!(new_cf_bps > 0 && new_cf_bps <= 10_000, "invalid cf");
        extend_instance(&env);
        let sxlm = read_sxlm_token(&env);
        env.storage().instance().set(&DataKey::AssetCFBps(sxlm), &(new_cf_bps as i128));
        env.events().publish((soroban_sdk::symbol_short!("cf_upd"),), new_cf_bps);
    }

    pub fn update_liquidation_threshold(env: Env, new_lt_bps: u32) {
        read_admin(&env).require_auth();
        assert!(new_lt_bps > 0 && new_lt_bps <= 10_000, "invalid lt");
        extend_instance(&env);
        env.storage().instance().set(&DataKey::LiquidationThresholdBps, &(new_lt_bps as i128));
    }

    pub fn update_borrow_rate(env: Env, new_rate_bps: u32) {
        read_admin(&env).require_auth();
        extend_instance(&env);
        env.storage().instance().set(&DataKey::BorrowRateBps, &(new_rate_bps as i128));
    }

    // -----------------------------------------------------------------------
    // Core lending functions
    // -----------------------------------------------------------------------

    /// Deposit any whitelisted Stellar asset as collateral.
    ///
    /// `asset` must have been registered via `add_collateral_asset`.
    pub fn deposit_collateral(env: Env, user: Address, asset: Address, amount: i128) {
        user.require_auth();
        assert!(amount > 0, "amount must be positive");
        extend_instance(&env);

        let enabled: bool = env
            .storage()
            .instance()
            .get(&DataKey::AssetEnabled(asset.clone()))
            .unwrap_or(false);
        assert!(enabled, "collateral asset not enabled");

        // Transfer asset from user to contract
        token::Client::new(&env, &asset).transfer(
            &user,
            &env.current_contract_address(),
            &amount,
        );

        // Update user balance
        let current = read_user_asset_balance(&env, &user, &asset);
        write_user_asset_balance(&env, &user, &asset, current + amount);

        // Update global total
        let total_key = DataKey::TotalAssetCollateral(asset.clone());
        let total = read_i128_instance(&env, &total_key);
        write_i128_instance(&env, &total_key, total + amount);

        extend_user_data(&env, &user, &asset);

        env.events().publish(
            (soroban_sdk::symbol_short!("deposit"),),
            (user, asset, amount),
        );
    }

    /// Withdraw collateral, provided the position remains healthy.
    pub fn withdraw_collateral(env: Env, user: Address, asset: Address, amount: i128) {
        user.require_auth();
        assert!(amount > 0, "amount must be positive");
        extend_instance(&env);

        let current = read_user_asset_balance(&env, &user, &asset);
        assert!(current >= amount, "insufficient collateral balance");

        // Simulate withdrawal and check health factor
        write_user_asset_balance(&env, &user, &asset, current - amount);

        let borrowed = read_user_borrowed(&env, &user);
        if borrowed > 0 {
            let new_hf = compute_health_factor(&env, &user);
            assert!(new_hf >= RATE_PRECISION, "withdrawal would make position unhealthy");
        }

        // Update global total
        let total_key = DataKey::TotalAssetCollateral(asset.clone());
        let total = read_i128_instance(&env, &total_key);
        write_i128_instance(&env, &total_key, total - amount);

        // Transfer asset back to user
        token::Client::new(&env, &asset).transfer(
            &env.current_contract_address(),
            &user,
            &amount,
        );

        env.events().publish(
            (soroban_sdk::symbol_short!("withdraw"),),
            (user, asset, amount),
        );
    }

    /// Borrow XLM against deposited multi-asset collateral.
    pub fn borrow(env: Env, user: Address, xlm_amount: i128) {
        user.require_auth();
        assert!(xlm_amount > 0, "amount must be positive");
        extend_instance(&env);

        let current_borrowed = read_user_borrowed(&env, &user);
        let new_borrowed = current_borrowed + xlm_amount;
        let max = max_borrow(&env, &user);
        assert!(new_borrowed <= max, "borrow exceeds collateral limit");

        // Pool solvency check
        let native = read_native_token(&env);
        let pool_balance = token::Client::new(&env, &native).balance(&env.current_contract_address());
        assert!(pool_balance >= xlm_amount, "insufficient pool liquidity");

        write_user_borrowed(&env, &user, new_borrowed);
        let total = read_i128_instance(&env, &DataKey::TotalBorrowed);
        write_i128_instance(&env, &DataKey::TotalBorrowed, total + xlm_amount);

        token::Client::new(&env, &native).transfer(
            &env.current_contract_address(),
            &user,
            &xlm_amount,
        );

        env.events().publish(
            (soroban_sdk::symbol_short!("borrow"),),
            (user, xlm_amount),
        );
    }

    /// Repay XLM debt.
    pub fn repay(env: Env, user: Address, xlm_amount: i128) {
        user.require_auth();
        assert!(xlm_amount > 0, "amount must be positive");
        extend_instance(&env);

        let borrowed = read_user_borrowed(&env, &user);
        let repay_amount = if xlm_amount > borrowed { borrowed } else { xlm_amount };

        let native = read_native_token(&env);
        token::Client::new(&env, &native).transfer(
            &user,
            &env.current_contract_address(),
            &repay_amount,
        );

        write_user_borrowed(&env, &user, borrowed - repay_amount);
        let total = read_i128_instance(&env, &DataKey::TotalBorrowed);
        write_i128_instance(&env, &DataKey::TotalBorrowed, total - repay_amount);

        env.events().publish(
            (soroban_sdk::symbol_short!("repay"),),
            (user, repay_amount),
        );
    }

    /// Liquidate an unhealthy multi-asset position.
    ///
    /// The liquidator repays the borrower's entire XLM debt and receives a
    /// pro-rata share of each collateral asset, scaled by the liquidation bonus.
    ///
    /// Returns the total XLM debt repaid.
    pub fn liquidate(env: Env, liquidator: Address, borrower: Address) -> i128 {
        liquidator.require_auth();
        extend_instance(&env);

        let borrowed = read_user_borrowed(&env, &borrower);
        assert!(borrowed > 0, "no debt to liquidate");

        let hf = compute_health_factor(&env, &borrower);
        assert!(hf < RATE_PRECISION, "position is healthy, cannot liquidate");

        // Liquidator repays full XLM debt
        let native = read_native_token(&env);
        token::Client::new(&env, &native).transfer(
            &liquidator,
            &env.current_contract_address(),
            &borrowed,
        );

        // Clear debt
        write_user_borrowed(&env, &borrower, 0);
        let total = read_i128_instance(&env, &DataKey::TotalBorrowed);
        write_i128_instance(&env, &DataKey::TotalBorrowed, total - borrowed);

        let bonus_bps = env
            .storage()
            .instance()
            .get(&DataKey::LiquidationBonusBps)
            .unwrap_or(DEFAULT_LIQUIDATION_BONUS_BPS);

        // Seize pro-rata collateral from each asset type
        let assets = read_asset_list(&env);
        for asset in assets.iter() {
            let balance = read_user_asset_balance(&env, &borrower, &asset);
            if balance <= 0 {
                continue;
            }

            // Compute how much of this asset the liquidator receives.
            // debt_value_in_asset = borrowed * RATE_PRECISION / asset_price
            // seize_amount = debt_value_in_asset * (BPS + bonus_bps) / BPS
            // But cap at user's entire balance of this asset.
            let price = asset_price(&env, &asset);
            let debt_in_asset = borrowed * RATE_PRECISION / price;
            let seize = debt_in_asset * (BPS_DENOMINATOR + bonus_bps) / BPS_DENOMINATOR;
            let collateral_to_send = if seize > balance { balance } else { seize };

            if collateral_to_send <= 0 {
                continue;
            }

            let remaining = balance - collateral_to_send;
            write_user_asset_balance(&env, &borrower, &asset, remaining);

            let total_key = DataKey::TotalAssetCollateral(asset.clone());
            let tot = read_i128_instance(&env, &total_key);
            write_i128_instance(&env, &total_key, tot - collateral_to_send);

            token::Client::new(&env, &asset).transfer(
                &env.current_contract_address(),
                &liquidator,
                &collateral_to_send,
            );

            env.events().publish(
                (soroban_sdk::symbol_short!("liq"),),
                (liquidator.clone(), borrower.clone(), asset, collateral_to_send),
            );
        }

        borrowed
    }

    // -----------------------------------------------------------------------
    // Views
    // -----------------------------------------------------------------------

    /// Returns the user's balance of a specific collateral asset, and total XLM borrowed.
    pub fn get_position(env: Env, user: Address, asset: Address) -> (i128, i128) {
        extend_instance(&env);
        extend_user_data(&env, &user, &asset);
        (
            read_user_asset_balance(&env, &user, &asset),
            read_user_borrowed(&env, &user),
        )
    }

    /// Returns the user's total XLM borrowed.
    pub fn get_user_borrowed(env: Env, user: Address) -> i128 {
        extend_instance(&env);
        read_user_borrowed(&env, &user)
    }

    /// Returns the user's entire multi-asset collateral position as a list of (asset, balance) pairs.
    pub fn get_full_position(env: Env, user: Address) -> Vec<(Address, i128)> {
        extend_instance(&env);
        let assets = read_asset_list(&env);
        let mut result = Vec::new(&env);
        for asset in assets.iter() {
            let balance = read_user_asset_balance(&env, &user, &asset);
            if balance > 0 {
                result.push_back((asset, balance));
            }
        }
        result
    }

    /// Health factor scaled by RATE_PRECISION (1e7 = 1.0). Uses liquidation threshold.
    pub fn health_factor(env: Env, user: Address) -> i128 {
        extend_instance(&env);
        compute_health_factor(&env, &user)
    }

    /// Maximum XLM that `user` can borrow given current multi-asset collateral and collateral factors.
    pub fn max_borrow_amount(env: Env, user: Address) -> i128 {
        extend_instance(&env);
        let already = read_user_borrowed(&env, &user);
        let capacity = max_borrow(&env, &user);
        if capacity > already { capacity - already } else { 0 }
    }

    /// List all registered collateral assets.
    pub fn get_asset_list(env: Env) -> Vec<Address> {
        extend_instance(&env);
        read_asset_list(&env)
    }

    /// Per-asset config: (cf_bps, price, enabled).
    pub fn get_asset_config(env: Env, asset: Address) -> (i128, i128, bool) {
        extend_instance(&env);
        let cf = asset_cf_bps(&env, &asset);
        let price = asset_price(&env, &asset);
        let enabled: bool = env
            .storage()
            .instance()
            .get(&DataKey::AssetEnabled(asset))
            .unwrap_or(false);
        (cf, price, enabled)
    }

    pub fn total_borrowed(env: Env) -> i128 {
        extend_instance(&env);
        read_i128_instance(&env, &DataKey::TotalBorrowed)
    }

    pub fn total_asset_collateral(env: Env, asset: Address) -> i128 {
        extend_instance(&env);
        read_i128_instance(&env, &DataKey::TotalAssetCollateral(asset))
    }

    pub fn get_exchange_rate(env: Env) -> i128 {
        extend_instance(&env);
        read_exchange_rate(&env)
    }

    pub fn get_collateral_factor(env: Env) -> i128 {
        extend_instance(&env);
        let sxlm = read_sxlm_token(&env);
        asset_cf_bps(&env, &sxlm)
    }

    pub fn get_liquidation_threshold(env: Env) -> i128 {
        extend_instance(&env);
        env.storage()
            .instance()
            .get(&DataKey::LiquidationThresholdBps)
            .unwrap_or(8000)
    }

    pub fn get_borrow_rate(env: Env) -> i128 {
        extend_instance(&env);
        read_i128_instance(&env, &DataKey::BorrowRateBps)
    }

    pub fn get_liquidation_bonus(env: Env) -> i128 {
        extend_instance(&env);
        env.storage()
            .instance()
            .get(&DataKey::LiquidationBonusBps)
            .unwrap_or(DEFAULT_LIQUIDATION_BONUS_BPS)
    }

    pub fn get_pool_balance(env: Env) -> i128 {
        extend_instance(&env);
        let native = read_native_token(&env);
        token::Client::new(&env, &native).balance(&env.current_contract_address())
    }

    // Kept for backward-compat with keeper harvest
    pub fn total_accrued_interest(env: Env) -> i128 {
        extend_instance(&env);
        read_i128_instance(&env, &DataKey::TotalAccruedInterest)
    }

    pub fn harvest_interest(env: Env) -> i128 {
        read_admin(&env).require_auth();
        extend_instance(&env);
        let accrued = read_i128_instance(&env, &DataKey::TotalAccruedInterest);
        write_i128_instance(&env, &DataKey::TotalAccruedInterest, 0);
        accrued
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{token::StellarAssetClient, Env};

    struct TestEnv {
        env: Env,
        contract: Address,
        sxlm: Address,
        native: Address,
        usdc: Address,
        user: Address,
        liquidator: Address,
        admin: Address,
    }

    fn setup() -> TestEnv {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let user = Address::generate(&env);
        let liquidator = Address::generate(&env);

        let sxlm = env.register_stellar_asset_contract_v2(Address::generate(&env)).address();
        let native = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let usdc = env.register_stellar_asset_contract_v2(Address::generate(&env)).address();

        let contract = env.register_contract(None, LendingContract);
        let client = LendingContractClient::new(&env, &contract);
        client.initialize(&admin, &sxlm, &native, &7500, &8000, &500);

        // Mint initial balances
        StellarAssetClient::new(&env, &sxlm).mint(&user, &100_000_0000000);
        StellarAssetClient::new(&env, &usdc).mint(&user, &100_000_0000000);
        StellarAssetClient::new(&env, &native).mint(&contract, &500_000_0000000); // pool XLM
        StellarAssetClient::new(&env, &native).mint(&liquidator, &200_000_0000000);

        TestEnv { env, contract, sxlm, native, usdc, user, liquidator, admin }
    }

    #[test]
    fn test_initialize_and_list() {
        let t = setup();
        let client = LendingContractClient::new(&t.env, &t.contract);
        let assets = client.get_asset_list();
        assert_eq!(assets.len(), 1);
        assert_eq!(assets.get(0).unwrap(), t.sxlm);
    }

    #[test]
    fn test_add_collateral_asset() {
        let t = setup();
        let client = LendingContractClient::new(&t.env, &t.contract);
        // USDC at 90% CF, price 1:1 with XLM initially
        client.add_collateral_asset(&t.admin, &t.usdc, &9000u32, &RATE_PRECISION);
        let (cf, price, enabled) = client.get_asset_config(&t.usdc);
        assert_eq!(cf, 9000);
        assert_eq!(price, RATE_PRECISION);
        assert!(enabled);
        assert_eq!(client.get_asset_list().len(), 2);
    }

    #[test]
    fn test_deposit_sxlm_and_borrow() {
        let t = setup();
        let client = LendingContractClient::new(&t.env, &t.contract);

        client.deposit_collateral(&t.user, &t.sxlm, &10_000_0000000);
        let (col, bor) = client.get_position(&t.user, &t.sxlm);
        assert_eq!(col, 10_000_0000000);
        assert_eq!(bor, 0);

        // Max borrow = 1000 * 1.0 * 75% = 750 XLM
        client.borrow(&t.user, &7_500_000_000);
        let (_, bor2) = client.get_position(&t.user, &t.sxlm);
        assert_eq!(bor2, 7_500_000_000);
    }

    #[test]
    fn test_multi_asset_borrow_capacity() {
        let t = setup();
        let client = LendingContractClient::new(&t.env, &t.contract);
        // Add USDC at 90% CF, 1:1 price
        client.add_collateral_asset(&t.admin, &t.usdc, &9000u32, &RATE_PRECISION);

        // Deposit 1000 sXLM (CF=75%) + 500 USDC (CF=90%)
        client.deposit_collateral(&t.user, &t.sxlm, &10_000_0000000);
        client.deposit_collateral(&t.user, &t.usdc, &5_000_0000000);

        // max_borrow = 1000 * 0.75 + 500 * 0.90 = 750 + 450 = 1200 XLM
        let max = client.max_borrow_amount(&t.user);
        assert_eq!(max, 1_200_0000000);

        client.borrow(&t.user, &1_200_0000000);
        assert_eq!(client.get_user_borrowed(&t.user), 1_200_0000000);
    }

    #[test]
    #[should_panic(expected = "borrow exceeds collateral limit")]
    fn test_borrow_exceeds_multi_asset_limit() {
        let t = setup();
        let client = LendingContractClient::new(&t.env, &t.contract);
        client.deposit_collateral(&t.user, &t.sxlm, &10_000_0000000);
        // 75% of 1000 = 750 max, try 800
        client.borrow(&t.user, &8_000_000_000);
    }

    #[test]
    fn test_withdraw_collateral() {
        let t = setup();
        let client = LendingContractClient::new(&t.env, &t.contract);
        client.deposit_collateral(&t.user, &t.sxlm, &10_000_0000000);
        client.withdraw_collateral(&t.user, &t.sxlm, &5_000_0000000);
        let (col, _) = client.get_position(&t.user, &t.sxlm);
        assert_eq!(col, 5_000_0000000);
    }

    #[test]
    #[should_panic(expected = "withdrawal would make position unhealthy")]
    fn test_withdraw_unhealthy() {
        let t = setup();
        let client = LendingContractClient::new(&t.env, &t.contract);
        client.deposit_collateral(&t.user, &t.sxlm, &10_000_0000000);
        client.borrow(&t.user, &7_500_000_000); // max
        client.withdraw_collateral(&t.user, &t.sxlm, &1_000_000_000); // any withdrawal breaks HF
    }

    #[test]
    fn test_health_factor_calculation() {
        let t = setup();
        let client = LendingContractClient::new(&t.env, &t.contract);
        client.deposit_collateral(&t.user, &t.sxlm, &10_000_0000000);
        client.borrow(&t.user, &5_000_0000000);
        // HF uses LT (8000) not CF (7500)
        // HF = (1000 * 1e7 * 8000/10000) / 500 = (1000 * 8e6) / 500 = 1.6e7
        let hf = client.health_factor(&t.user);
        assert_eq!(hf, 16_000_000); // 1.6 × 1e7
    }

    #[test]
    fn test_liquidation_multi_asset() {
        let t = setup();
        let client = LendingContractClient::new(&t.env, &t.contract);
        // Add USDC at 90% CF but give it a LOW LT by deploying a separate instance
        // to trigger liquidation easily, use a fresh contract with LT=50%
        let contract2 = t.env.register_contract(None, LendingContract);
        let c2 = LendingContractClient::new(&t.env, &contract2);
        let sxlm2 = t.env.register_stellar_asset_contract_v2(Address::generate(&t.env)).address();
        let native2 = t.env.register_stellar_asset_contract_v2(Address::generate(&t.env)).address();
        c2.initialize(&t.admin, &sxlm2, &native2, &7500, &4000u32, &500);

        let u2 = Address::generate(&t.env);
        StellarAssetClient::new(&t.env, &sxlm2).mint(&u2, &100_000_0000000);
        StellarAssetClient::new(&t.env, &native2).mint(&contract2, &500_000_0000000);
        StellarAssetClient::new(&t.env, &native2).mint(&t.liquidator, &200_000_0000000);

        c2.deposit_collateral(&u2, &sxlm2, &10_000_0000000);
        c2.borrow(&u2, &7_500_000_000); // max at 75% CF
        // HF = 10000 * 1e7 * 4000/10000 / 7500 = 4000 * 1e7 / 7500 ≈ 5_333_333 < 1e7
        // → liquidatable

        let repaid = c2.liquidate(&t.liquidator, &u2);
        assert_eq!(repaid, 7_500_000_000);
        assert_eq!(c2.get_user_borrowed(&u2), 0);
    }

    #[test]
    fn test_repay() {
        let t = setup();
        let client = LendingContractClient::new(&t.env, &t.contract);
        StellarAssetClient::new(&t.env, &t.native).mint(&t.user, &50_000_0000000);
        client.deposit_collateral(&t.user, &t.sxlm, &10_000_0000000);
        client.borrow(&t.user, &5_000_0000000);
        client.repay(&t.user, &3_000_0000000);
        assert_eq!(client.get_user_borrowed(&t.user), 2_000_0000000);
    }

    #[test]
    fn test_update_asset_price_affects_borrow_capacity() {
        let t = setup();
        let client = LendingContractClient::new(&t.env, &t.contract);
        client.add_collateral_asset(&t.admin, &t.usdc, &9000u32, &RATE_PRECISION); // USDC @ 1:1 XLM

        client.deposit_collateral(&t.user, &t.usdc, &10_000_0000000);
        let max_before = client.max_borrow_amount(&t.user);
        assert_eq!(max_before, 9_000_0000000); // 90% of 1000 = 900 XLM

        // Update USDC price to 1.2 XLM
        client.update_asset_price(&t.admin, &t.usdc, &12_000_000i128);
        let max_after = client.max_borrow_amount(&t.user);
        assert_eq!(max_after, 10_800_0000000); // 90% of 1200 = 1080 XLM
    }
}
