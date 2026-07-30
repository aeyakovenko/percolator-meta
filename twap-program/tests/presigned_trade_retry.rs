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

const DEPOSIT: u64 = 1_000_000;
const INITIAL_PRICE: u64 = 1_000_000;
const MOVED_PRICE: u64 = INITIAL_PRICE / 2;
const START_SLOT: u64 = 100;
const MOVE_SLOT: u64 = 110;
const ATA_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    retry_accepted: bool,
    victim_loss: u128,
    counterparty_gain: u128,
    crystallized_loss: u128,
    counterparty_withdrawn: u64,
    victim_withdrawn: u64,
    vault_remaining: u64,
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

fn token_amount(svm: &LiteSVM, key: &Pubkey) -> u64 {
    spl_token::state::Account::unpack(&svm.get_account(key).unwrap().data)
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

fn portfolio_capital(svm: &LiteSVM, portfolio: &Pubkey) -> u128 {
    percolator_prog::state::read_portfolio(&svm.get_account(portfolio).unwrap().data)
        .unwrap()
        .capital
        .get()
}

fn crystallized_loss(svm: &LiteSVM, portfolio: &Pubkey) -> u128 {
    let data = svm.get_account(portfolio).unwrap().data;
    u128::from_le_bytes(data[196..212].try_into().unwrap())
}

fn run(retry: bool, batch: bool) -> Outcome {
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
    let counterparty = Keypair::new();
    let victim = Keypair::new();
    for signer in [&payer, &creator, &oracle, &counterparty, &victim] {
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

    let counterparty_portfolio = Keypair::new();
    let victim_portfolio = Keypair::new();
    let portfolio_len = percolator_prog::state::portfolio_account_len_for_market_slots(1).unwrap();
    let portfolio_rent = svm.minimum_balance_for_rent_exemption(portfolio_len);
    for (owner, portfolio) in [
        (&counterparty, &counterparty_portfolio),
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

    let position_q = -((percolator::POS_SCALE / 4) as i128);
    let trade = || {
        let trade = if batch {
            PIx::BatchTradeNoCpi {
                legs: vec![percolator_prog::ix::BatchTradeLeg {
                    asset_index: 0,
                    size_q: position_q,
                    exec_price: INITIAL_PRICE,
                    fee_bps: 0,
                }],
            }
        } else {
            PIx::TradeNoCpi {
                asset_index: 0,
                size_q: position_q,
                exec_price: INITIAL_PRICE,
                fee_bps: 0,
            }
        };
        pix(
            vec![
                AccountMeta::new_readonly(counterparty.pubkey(), true),
                AccountMeta::new_readonly(victim.pubkey(), true),
                AccountMeta::new(market.pubkey(), false),
                AccountMeta::new(counterparty_portfolio.pubkey(), false),
                AccountMeta::new(victim_portfolio.pubkey(), false),
            ],
            trade,
        )
    };
    // Model two presigned fee-bump/retry variants that are simultaneously valid.
    // The trade intent is byte-identical; only the compute-unit limit differs, so
    // the transactions have distinct signatures under one recent blockhash.
    let trade_blockhash = svm.latest_blockhash();
    let first_trade = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_399_999),
            trade(),
        ],
        Some(&payer.pubkey()),
        &[&payer, &counterparty, &victim],
        trade_blockhash,
    );
    let retry_trade = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            trade(),
        ],
        Some(&payer.pubkey()),
        &[&payer, &counterparty, &victim],
        trade_blockhash,
    );
    svm.send_transaction(first_trade)
        .expect("the victim authorizes one matched trade");
    let retry_accepted = retry && svm.send_transaction(retry_trade).is_ok();

    svm.set_sysvar(&Clock {
        slot: MOVE_SLOT,
        unix_timestamp: MOVE_SLOT as i64,
        ..Clock::default()
    });
    send(
        &mut svm,
        &[&payer, &oracle],
        pix(
            vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(market.pubkey(), false),
            ],
            PIx::PushAuthMark {
                asset_index: 0,
                now_slot: MOVE_SLOT,
                mark_e6: MOVED_PRICE,
            },
        ),
    )
    .expect("the independent oracle moves against the victim");

    let crank = |portfolio: Pubkey| {
        pix(
            vec![
                AccountMeta::new_readonly(payer.pubkey(), true),
                AccountMeta::new(market.pubkey(), false),
                AccountMeta::new(portfolio, false),
            ],
            PIx::PermissionlessCrank {
                now_slot: MOVE_SLOT,
                observations: vec![percolator_prog::ix::CrankObservationHint {
                    asset_index: 0,
                    oracle_accounts: 0,
                }],
            },
        )
    };
    for portfolio in [
        counterparty_portfolio.pubkey(),
        counterparty_portfolio.pubkey(),
        victim_portfolio.pubkey(),
        victim_portfolio.pubkey(),
    ] {
        send(&mut svm, &[&payer], crank(portfolio)).unwrap();
    }

    let accepted_fills = 1i128 + i128::from(retry_accepted);
    let flatten = if batch {
        PIx::BatchTradeNoCpi {
            legs: vec![percolator_prog::ix::BatchTradeLeg {
                asset_index: 0,
                size_q: -position_q * accepted_fills,
                exec_price: MOVED_PRICE,
                fee_bps: 0,
            }],
        }
    } else {
        PIx::TradeNoCpi {
            asset_index: 0,
            size_q: -position_q * accepted_fills,
            exec_price: MOVED_PRICE,
            fee_bps: 0,
        }
    };
    send(
        &mut svm,
        &[&payer, &counterparty, &victim],
        pix(
            vec![
                AccountMeta::new_readonly(counterparty.pubkey(), true),
                AccountMeta::new_readonly(victim.pubkey(), true),
                AccountMeta::new(market.pubkey(), false),
                AccountMeta::new(counterparty_portfolio.pubkey(), false),
                AccountMeta::new(victim_portfolio.pubkey(), false),
            ],
            flatten,
        ),
    )
    .expect("the users flatten at the independently moved mark");
    for portfolio in [counterparty_portfolio.pubkey(), victim_portfolio.pubkey()] {
        send(&mut svm, &[&payer], crank(portfolio)).unwrap();
    }

    let victim_loss = u128::from(DEPOSIT)
        .checked_sub(portfolio_capital(&svm, &victim_portfolio.pubkey()))
        .unwrap();
    let counterparty_before = portfolio_capital(&svm, &counterparty_portfolio.pubkey());
    send(
        &mut svm,
        &[&payer, &counterparty],
        pix(
            vec![
                AccountMeta::new_readonly(counterparty.pubkey(), true),
                AccountMeta::new(market.pubkey(), false),
                AccountMeta::new(counterparty_portfolio.pubkey(), false),
            ],
            PIx::ConvertReleasedPnl { amount: u128::MAX },
        ),
    )
    .expect("the counterparty converts the victim's released loss");
    let counterparty_gain = portfolio_capital(&svm, &counterparty_portfolio.pubkey())
        .checked_sub(counterparty_before)
        .unwrap();
    let counterparty_capital = portfolio_capital(&svm, &counterparty_portfolio.pubkey());
    let victim_capital = portfolio_capital(&svm, &victim_portfolio.pubkey());
    let counterparty_destination = Pubkey::new_unique();
    let victim_destination = Pubkey::new_unique();
    set_token(
        &mut svm,
        &counterparty_destination,
        &collateral_mint,
        &counterparty.pubkey(),
        0,
    );
    set_token(
        &mut svm,
        &victim_destination,
        &collateral_mint,
        &victim.pubkey(),
        0,
    );
    for (owner, portfolio, destination, amount) in [
        (
            &counterparty,
            counterparty_portfolio.pubkey(),
            counterparty_destination,
            counterparty_capital,
        ),
        (
            &victim,
            victim_portfolio.pubkey(),
            victim_destination,
            victim_capital,
        ),
    ] {
        send(
            &mut svm,
            &[&payer, owner],
            pix(
                vec![
                    AccountMeta::new_readonly(owner.pubkey(), true),
                    AccountMeta::new(market.pubkey(), false),
                    AccountMeta::new(portfolio, false),
                    AccountMeta::new(destination, false),
                    AccountMeta::new(vault, false),
                    AccountMeta::new_readonly(authority, false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                ],
                PIx::Withdraw { amount },
            ),
        )
        .expect("flat owner withdraws all spendable capital");
    }
    let counterparty_withdrawn = token_amount(&svm, &counterparty_destination);
    let victim_withdrawn = token_amount(&svm, &victim_destination);
    let vault_remaining = token_amount(&svm, &vault);
    assert_eq!(
        counterparty_withdrawn
            .checked_add(victim_withdrawn)
            .and_then(|amount| amount.checked_add(vault_remaining))
            .unwrap(),
        2 * DEPOSIT + 1,
        "both user deposits plus the one backing atom remain exactly conserved",
    );

    Outcome {
        retry_accepted,
        victim_loss,
        counterparty_gain,
        crystallized_loss: crystallized_loss(&svm, &victim_portfolio.pubkey()),
        counterparty_withdrawn,
        victim_withdrawn,
        vault_remaining,
    }
}

#[test]
#[ignore = "upstream percolator-prog#384: signed trade deltas need a portfolio action nonce"]
fn public_trade_retry_is_one_shot() {
    let outcomes = [true, false].map(|batch| (batch, run(false, batch), run(true, batch)));

    for (batch, control, retry) in outcomes {
        assert_eq!(
            retry.counterparty_withdrawn, control.counterparty_withdrawn,
            "batch={batch}: a distinct presigned retry must not increase spendable counterparty withdrawal: control={control:?} retry={retry:?}"
        );
        assert_eq!(
            retry.victim_withdrawn, control.victim_withdrawn,
            "batch={batch}: a distinct presigned retry must not reduce the victim's spendable withdrawal"
        );
        assert_eq!(
            retry.victim_loss, control.victim_loss,
            "batch={batch}: a distinct presigned retry must not increase victim loss: control={control:?} retry={retry:?}"
        );
        assert_eq!(
            retry.counterparty_gain, control.counterparty_gain,
            "batch={batch}: a distinct presigned retry must not create a second counterparty payout"
        );
    }
}
