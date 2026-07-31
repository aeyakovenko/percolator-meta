use litesvm::LiteSVM;
use solana_sdk::{
    account::Account,
    clock::Clock,
    compute_budget::ComputeBudgetInstruction,
    instruction::{AccountMeta, Instruction},
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use std::path::PathBuf;

use percolator_prog::ix::Instruction as PIx;

const DEPOSIT: u64 = 100;
const MAINTENANCE_FEE_PER_SLOT: u128 = 200;
const CRANKER_SHARE_BPS: u16 = 1_000;
const START_SLOT: u64 = 100;
const FEE_SLOT: u64 = START_SLOT + 1;
const ATA_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    retry_accepted: bool,
    later_wallet_balance: u64,
    cranker_withdrawn: u64,
    retained_insurance: u128,
}

fn perc_id() -> Pubkey {
    percolator_prog::id()
}

fn perc_so() -> PathBuf {
    if let Ok(target) = std::env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(target).join("deploy/percolator_prog.so");
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/deploy/percolator_prog.so")
}

fn pix(accounts: Vec<AccountMeta>, ix: PIx) -> Instruction {
    Instruction {
        program_id: perc_id(),
        accounts,
        data: ix.encode(),
    }
}

fn send(svm: &mut LiteSVM, signers: &[&Keypair], ix: Instruction) -> Result<(), String> {
    svm.expire_blockhash();
    let blockhash = svm.latest_blockhash();
    svm.send_transaction(Transaction::new_signed_with_payer(
        &[ix],
        Some(&signers[0].pubkey()),
        signers,
        blockhash,
    ))
    .map(|_| ())
    .map_err(|error| format!("{:?}", error.err))
}

fn token_account_data(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Vec<u8> {
    let mut data = vec![0u8; spl_token::state::Account::LEN];
    data[0..32].copy_from_slice(mint.as_ref());
    data[32..64].copy_from_slice(owner.as_ref());
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    data[108] = 1;
    data
}

fn set_token(svm: &mut LiteSVM, key: &Pubkey, mint: &Pubkey, owner: &Pubkey, amount: u64) {
    svm.set_account(
        *key,
        Account {
            lamports: 2_000_000,
            data: token_account_data(mint, owner, amount),
            owner: spl_token::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

fn token_amount(svm: &LiteSVM, token: &Pubkey) -> u64 {
    spl_token::state::Account::unpack(&svm.get_account(token).unwrap().data)
        .unwrap()
        .amount
}

fn create_mint(svm: &mut LiteSVM, payer: &Keypair, authority: &Pubkey) -> Pubkey {
    let mint = Keypair::new();
    send(
        svm,
        &[payer, &mint],
        solana_sdk::system_instruction::create_account(
            &payer.pubkey(),
            &mint.pubkey(),
            svm.minimum_balance_for_rent_exemption(spl_token::state::Mint::LEN),
            spl_token::state::Mint::LEN as u64,
            &spl_token::ID,
        ),
    )
    .unwrap();
    send(
        svm,
        &[payer],
        spl_token::instruction::initialize_mint(&spl_token::ID, &mint.pubkey(), authority, None, 6)
            .unwrap(),
    )
    .unwrap();
    mint.pubkey()
}

fn vault_authority(market: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"vault", market.as_ref()], &perc_id()).0
}

fn canonical_vault(authority: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[authority.as_ref(), spl_token::ID.as_ref(), mint.as_ref()],
        &ATA_PROGRAM_ID,
    )
    .0
}

fn init_portfolio(svm: &mut LiteSVM, payer: &Keypair, owner: &Keypair, market: &Pubkey) -> Keypair {
    let portfolio = Keypair::new();
    let portfolio_len = percolator_prog::state::portfolio_account_len_for_market_slots(1).unwrap();
    send(
        svm,
        &[payer, &portfolio],
        solana_sdk::system_instruction::create_account(
            &payer.pubkey(),
            &portfolio.pubkey(),
            svm.minimum_balance_for_rent_exemption(portfolio_len),
            portfolio_len as u64,
            &perc_id(),
        ),
    )
    .unwrap();
    send(
        svm,
        &[payer, owner],
        pix(
            vec![
                AccountMeta::new_readonly(owner.pubkey(), true),
                AccountMeta::new(*market, false),
                AccountMeta::new(portfolio.pubkey(), false),
            ],
            PIx::InitPortfolio,
        ),
    )
    .unwrap();
    portfolio
}

fn run(retry: bool) -> Outcome {
    let mut svm =
        LiteSVM::new().with_compute_budget(solana_program_runtime::compute_budget::ComputeBudget {
            compute_unit_limit: 1_400_000,
            heap_size: 256 * 1024,
            ..solana_program_runtime::compute_budget::ComputeBudget::default()
        });
    svm.add_program_from_file(perc_id(), perc_so()).unwrap();

    let cranker = Keypair::new();
    let creator = Keypair::new();
    let victim = Keypair::new();
    for signer in [&cranker, &creator, &victim] {
        svm.airdrop(&signer.pubkey(), 100_000_000_000).unwrap();
    }
    svm.set_sysvar(&Clock {
        slot: START_SLOT,
        unix_timestamp: START_SLOT as i64,
        ..Clock::default()
    });

    let mint_authority = Keypair::new();
    let collateral_mint = create_mint(&mut svm, &cranker, &mint_authority.pubkey());
    let market = Keypair::new();
    let market_len = percolator_prog::state::market_account_len_for_capacity(1).unwrap();
    let market_rent = svm.minimum_balance_for_rent_exemption(market_len);
    send(
        &mut svm,
        &[&cranker, &market],
        solana_sdk::system_instruction::create_account(
            &cranker.pubkey(),
            &market.pubkey(),
            market_rent,
            market_len as u64,
            &perc_id(),
        ),
    )
    .unwrap();
    send(
        &mut svm,
        &[&cranker, &creator],
        pix(
            vec![
                AccountMeta::new_readonly(creator.pubkey(), true),
                AccountMeta::new(market.pubkey(), false),
                AccountMeta::new_readonly(collateral_mint, false),
            ],
            PIx::InitMarket {
                max_portfolio_assets: 1,
                h_min: 0,
                h_max: 10,
                initial_price: 1_000_000,
                min_nonzero_mm_req: 1,
                min_nonzero_im_req: 2,
                maintenance_margin_bps: 10_000,
                initial_margin_bps: 10_000,
                max_trading_fee_bps: 10_000,
                trade_fee_base_bps: 0,
                liquidation_fee_bps: 0,
                liquidation_fee_cap: 0,
                min_liquidation_abs: 0,
                max_price_move_bps_per_slot: 10_000,
                max_accrual_dt_slots: 1,
                max_abs_funding_e9_per_slot: 0,
                min_funding_lifetime_slots: 1,
                max_account_b_settlement_chunks: 1,
                max_bankrupt_close_chunks: 1,
                max_bankrupt_close_lifetime_slots: 100,
                public_b_chunk_atoms: percolator::MAX_VAULT_TVL,
                maintenance_fee_per_slot: MAINTENANCE_FEE_PER_SLOT,
            },
        ),
    )
    .unwrap();
    send(
        &mut svm,
        &[&cranker, &creator],
        pix(
            vec![
                AccountMeta::new_readonly(creator.pubkey(), true),
                AccountMeta::new(market.pubkey(), false),
            ],
            PIx::UpdateMaintenanceFeePolicy {
                cranker_share_bps: CRANKER_SHARE_BPS,
            },
        ),
    )
    .unwrap();

    let authority = vault_authority(&market.pubkey());
    let vault = canonical_vault(&authority, &collateral_mint);
    set_token(&mut svm, &vault, &collateral_mint, &authority, 0);
    let victim_portfolio = init_portfolio(&mut svm, &cranker, &victim, &market.pubkey());
    let cranker_portfolio = init_portfolio(&mut svm, &cranker, &cranker, &market.pubkey());

    let victim_source = Pubkey::new_unique();
    let cranker_destination = Pubkey::new_unique();
    set_token(
        &mut svm,
        &victim_source,
        &collateral_mint,
        &victim.pubkey(),
        DEPOSIT,
    );
    set_token(
        &mut svm,
        &cranker_destination,
        &collateral_mint,
        &cranker.pubkey(),
        0,
    );

    let deposit = || {
        pix(
            vec![
                AccountMeta::new_readonly(victim.pubkey(), true),
                AccountMeta::new(market.pubkey(), false),
                AccountMeta::new(victim_portfolio.pubkey(), false),
                AccountMeta::new(victim_source, false),
                AccountMeta::new(vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            PIx::Deposit {
                amount: u128::from(DEPOSIT),
            },
        )
    };
    let blockhash = svm.latest_blockhash();
    let first_deposit = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_399_999),
            deposit(),
        ],
        Some(&cranker.pubkey()),
        &[&cranker, &victim],
        blockhash,
    );
    let retry_deposit = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            deposit(),
        ],
        Some(&cranker.pubkey()),
        &[&cranker, &victim],
        blockhash,
    );
    svm.send_transaction(first_deposit)
        .expect("the victim authorizes one deposit");
    assert_eq!(token_amount(&svm, &victim_source), 0);

    // Model an unrelated transfer that replenishes the same wallet token account
    // while the second signed retry remains within the recent-blockhash window.
    set_token(
        &mut svm,
        &victim_source,
        &collateral_mint,
        &victim.pubkey(),
        DEPOSIT,
    );
    svm.set_sysvar(&Clock {
        slot: FEE_SLOT,
        unix_timestamp: FEE_SLOT as i64,
        ..Clock::default()
    });
    let market_before_retry = svm.get_account(&market.pubkey()).unwrap();
    let portfolio_before_retry = svm.get_account(&victim_portfolio.pubkey()).unwrap();
    let source_before_retry = svm.get_account(&victim_source).unwrap();
    let vault_before_retry = svm.get_account(&vault).unwrap();
    let retry_accepted = if retry {
        let accepted = svm.send_transaction(retry_deposit).is_ok();
        if !accepted {
            assert_eq!(
                svm.get_account(&market.pubkey()).unwrap(),
                market_before_retry
            );
            assert_eq!(
                svm.get_account(&victim_portfolio.pubkey()).unwrap(),
                portfolio_before_retry
            );
            assert_eq!(
                svm.get_account(&victim_source).unwrap(),
                source_before_retry
            );
            assert_eq!(svm.get_account(&vault).unwrap(), vault_before_retry);
        }
        accepted
    } else {
        false
    };

    send(
        &mut svm,
        &[&cranker],
        pix(
            vec![
                AccountMeta::new(market.pubkey(), false),
                AccountMeta::new(victim_portfolio.pubkey(), false),
                AccountMeta::new(cranker_portfolio.pubkey(), false),
            ],
            PIx::SyncMaintenanceFee { now_slot: FEE_SLOT },
        ),
    )
    .expect("the public cranker charges the configured elapsed fee");

    let cranker_capital = percolator_prog::state::read_portfolio(
        &svm.get_account(&cranker_portfolio.pubkey()).unwrap().data,
    )
    .unwrap()
    .capital
    .get();
    send(
        &mut svm,
        &[&cranker],
        pix(
            vec![
                AccountMeta::new_readonly(cranker.pubkey(), true),
                AccountMeta::new(market.pubkey(), false),
                AccountMeta::new(cranker_portfolio.pubkey(), false),
                AccountMeta::new(cranker_destination, false),
                AccountMeta::new(vault, false),
                AccountMeta::new_readonly(authority, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            PIx::Withdraw {
                amount: cranker_capital,
            },
        ),
    )
    .expect("the cranker withdraws the configured reward");

    Outcome {
        retry_accepted,
        later_wallet_balance: token_amount(&svm, &victim_source),
        cranker_withdrawn: token_amount(&svm, &cranker_destination),
        retained_insurance: percolator_accounting::read_asset_insurance_remaining(
            &svm.get_account(&market.pubkey()).unwrap().data,
            0,
        )
        .unwrap(),
    }
}

#[test]
#[ignore = "upstream percolator-prog#388: signed deposits need a portfolio action nonce"]
fn public_deposit_retry_is_one_shot() {
    let control = run(false);
    let retry = run(true);

    assert!(
        !retry.retry_accepted,
        "the retained deposit variant must reject: control={control:?} retry={retry:?}"
    );
    assert_eq!(
        retry.later_wallet_balance, control.later_wallet_balance,
        "a distinct presigned retry must not pull a later wallet payment: control={control:?} retry={retry:?}"
    );
    assert_eq!(
        retry.cranker_withdrawn, control.cranker_withdrawn,
        "a distinct presigned retry must not create a second cranker payout"
    );
    assert_eq!(
        retry.retained_insurance, control.retained_insurance,
        "a distinct presigned retry must not confiscate a later wallet payment into insurance"
    );
}
