//! Test: Supply cap vulnerability fix for `process_transfer_mint_authority`.
//!
//! This test verifies that the supply cap break (CVE-2024-???) via the
//! `transfer_mint_authority` instruction is properly gated on genesis finalization.
//!
//! ## Vulnerability
//!
//! Prior to fix `ba17443`, the controller could:
//! 1. Rotate the COIN mint authority away from the rewards PDA before genesis finalization.
//! 2. Mint tokens directly using `spl_token::mint_to`.
//! 3. Rotate the authority back.
//! 4. Trigger genesis distribution – bypassing the `minted_supply == 0` gate because
//!    direct SPL mints do not update `cfg.minted_supply`.
//!
//! Result: final supply = `PRE_MINT + reward_supply` exceeding the cap.
//!
//! ## Fix
//!
//! `process_transfer_mint_authority` now checks `GenesisConfig::is_finalized()` before
//! allowing any authority transfer. This test verifies that the check is enforced.

use {
    solana_program_test::*,
    solana_sdk::{
        instruction::{AccountMeta, Instruction},
        pubkey::Pubkey,
        signature::{Keypair, Signer},
        transaction::Transaction,
        transport::TransportError,
    },
    kora_program::{
        error::KoraError,
        instruction::{
            genesis_deposit,
            init_coin_config_with_delay,
            init_futarchy_percolator_market,
            init_genesis_bootstrap,
            kickstart_genesis_market,
            transfer_mint_authority,
        },
        state::GenesisConfig,
    },
    tracing::{debug, error, info, span, Level, Span},
    anyhow::{Context, Result},
};

mod test_env;
use test_env::TestEnv;

/// Constants for test configuration.
const AIRDROP_AMOUNT: u64 = 10_000_000_000;
const GENESIS_DEPOSIT_AMOUNT: u64 = 5;
const REWARD_SUPPLY: u64 = 100;
const DELAY_SLOTS: u64 = 50;

/// Instruction tag for `transfer_mint_authority`.
const TRANSFER_MINT_AUTHORITY_TAG: u8 = 10;

/// Constructs a `transfer_mint_authority` instruction for testing.
///
/// This function builds the instruction as the program expects it, using the
/// correct accounts and data layout.
///
/// # Arguments
///
/// * `mint_authority` – The current mint authority (simulated signer in test).
/// * `new_authority` – The new mint authority to set.
/// * `genesis_config` – The genesis config account.
///
/// # Returns
///
/// An `Instruction` ready to be included in a transaction.
fn build_transfer_mint_authority_instruction(
    mint_authority: &Pubkey,
    new_authority: &Pubkey,
    genesis_config: &Pubkey,
) -> Instruction {
    let accounts = vec![
        AccountMeta::new(*mint_authority, true),     // signer: current authority
        AccountMeta::new(*genesis_config, false),     // read-only check
        AccountMeta::new(*new_authority, false),      // new authority
    ];
    let data = vec![TRANSFER_MINT_AUTHORITY_TAG];
    Instruction {
        program_id: kora_program::id(),
        accounts,
        data,
    }
}

/// Validates that the `transfer_mint_authority` instruction is rejected prior to
/// genesis finalization, preventing the supply cap vulnerability described above.
///
/// # Errors
///
/// Returns an `anyhow::Error` if any setup or assertion fails.
#[tokio::test]
#[tracing::instrument(skip_all, fields(test = "poc_dao_supply_cap_break_via_transfer_mint_authority"))]
async fn poc_dao_supply_cap_break_via_transfer_mint_authority() -> Result<()> {
    // -------------------------------------------------------------------------
    // Setup: create test environment and initialize program state
    // -------------------------------------------------------------------------
    debug!("Initializing test environment");
    let mut env = TestEnv::new()
        .await
        .context("Failed to create TestEnv")?;
    let ctx: &mut ProgramTestContext = &mut env.svm;

    info!("Initializing coin config with delay = {} slots", DELAY_SLOTS);
    env.init_coin_config_with_delay(DELAY_SLOTS)
        .await
        .context("init_coin_config_with_delay failed")?;

    info!("Initializing genesis bootstrap with reward_supply = {}", REWARD_SUPPLY);
    env.init_genesis_bootstrap(REWARD_SUPPLY)
        .await
        .context("init_genesis_bootstrap failed")?;

    // Create and fund two non‑trivial accounts for genesis deposits
    let alice = Keypair::new();
    let bob = Keypair::new();

    debug!("Airdropping {} lamports to alice and bob", AIRDROP_AMOUNT);
    for kp in [&alice, &bob] {
        ctx.banks_client
            .airdrop(&kp.pubkey(), AIRDROP_AMOUNT)
            .await
            .with_context(|| format!("Airdrop failed for key {}", kp.pubkey()))?;
    }

    info!("Making genesis deposits of {} tokens each", GENESIS_DEPOSIT_AMOUNT);
    env.genesis_deposit(&alice, GENESIS_DEPOSIT_AMOUNT)
        .await
        .context("genesis_deposit alice failed")?;
    env.genesis_deposit(&bob, GENESIS_DEPOSIT_AMOUNT)
        .await
        .context("genesis_deposit bob failed")?;

    // Percolator market is required before kickstart
    debug!("Initializing Futarchy percolator market");
    let (slab, percolator_vault) = env
        .init_futarchy_percolator_market()
        .await
        .context("init_futarchy_percolator_market failed")?;

    info!("Kickstarting genesis market");
    env.kickstart_genesis_market(&slab, &percolator_vault)
        .await
        .context("kickstart_genesis_market failed")?;

    // -------------------------------------------------------------------------
    // Exploit attempt: transfer mint authority before genesis finalization
    // -------------------------------------------------------------------------
    info!("Attempting to transfer mint authority before genesis finalization");

    let controller = Keypair::new();
    let mint_authority_pda: Pubkey = env.coin_mint_authority_pda;
    let genesis_config_pubkey: Pubkey = env.genesis_config_pubkey;

    // Build the instruction: the current mint authority (PDA) is the signer,
    // but we simulate a CPI call where the controller signs for the PDA via a
    // program-derived seed. In this test we sign with an arbitrary keypair to
    // exercise the program logic; the program will check the signer's authority
    // and the genesis finalization status.
    let instruction: Instruction = build_transfer_mint_authority_instruction(
        &mint_authority_pda,
        &controller.pubkey(),
        &genesis_config_pubkey,
    );

    let blockhash = ctx
        .banks_client
        .get_latest_blockhash()
        .await
        .context("Failed to get latest blockhash")?;

    let mut transaction = Transaction::new_with_payer(&[instruction], Some(&controller.pubkey()));
    // In the real exploit the signer would be the rewards PDA via CPI.
    // Here we sign with the controller keypair for test isolation.
    transaction.sign(&[&controller, &env.fee_payer], blockhash);

    debug!("Processing transaction");
    let result = ctx
        .banks_client
        .process_transaction(transaction)
        .await;

    // -------------------------------------------------------------------------
    // Assert: the instruction must be rejected because genesis is not finalized
    // -------------------------------------------------------------------------
    match result {
        Err(TransportError::TransactionError(err)) => {
            let error_msg = err.to_string();
            // Ensure the error matches the expected `GenesisNotFinalized`.
            // The exact error code depends on the program implementation.
            assert!(
                error_msg.contains("GenesisNotFinalized")
                    || error_msg.contains("GenesisNotFinalizedError"),
                "Expected error to contain 'GenesisNotFinalized', got: {}",
                error_msg
            );
            info!("Transaction correctly rejected with GenesisNotFinalized error");
        }
        Ok(_) => {
            error!("Transaction succeeded unexpectedly – vulnerability may exist");
            anyhow::bail!(
                "transfer_mint_authority succeeded before genesis finalization; \
                 supply cap is still breakable"
            );
        }
        other => {
            error!("Unexpected result: {:?}", other);
            anyhow::bail!("Unexpected transport error variant: {:?}", other);
        }
    }

    // -------------------------------------------------------------------------
    // Verify: mint authority remains unchanged
    // -------------------------------------------------------------------------
    debug!("Verifying mint authority has not been transferred");
    let coin_mint = env
        .get_coin_mint()
        .await
        .context("Failed to fetch coin mint")?;

    assert_eq!(
        coin_mint.mint_authority,
        Some(mint_authority_pda),
        "Mint authority should still be the rewards PDA after a rejected transfer"
    );

    // Optionally verify genesis is not finalized after the attempt.
    let genesis_config = env
        .get_genesis_config()
        .await
        .context("Failed to fetch genesis config")?;
    assert!(
        !genesis_config.is_finalized(),
        "Genesis config should remain non-finalized because the attack was blocked"
    );

    info!("All assertions passed – supply cap vulnerability is fixed");
    Ok(())
}