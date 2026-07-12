//! LiteSVM e2e: fixed-supply and TWAP-funded deterministic reward epochs.
//! (10/10/40/40). Insurance + backing reward SUBLEDGER CAPITAL (Position.principal - comparable
//! base units, log tenure, soft-veto on exit); LP/trader reward residual counters; funding-payer rewards
//! cumulative Percolator funding-paid counters. Self-service flow:
//! register -> crystallize -> freeze -> claim, against mock dependency accounts at the offset-pinned
//! layouts (tests/offsets.rs pins every offset vs the real percolator/subledger structs).

use litesvm::LiteSVM;
use solana_sdk::{
    account::Account,
    clock::Clock,
    instruction::{AccountMeta, Instruction},
    program_option::COption,
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction,
    transaction::Transaction,
};
use spl_token::instruction::AuthorityType;
use std::{cell::RefCell, collections::HashMap};

fn rd_id() -> Pubkey {
    Pubkey::from(residual_distributor::ID)
}
fn dist_id() -> Pubkey {
    Pubkey::try_from("D1str1but1on11111111111111111111111111111111").unwrap()
}
const DISTRIBUTION_CLAIM_WINDOW_SLOTS: u64 = 1_000_000;
fn dist_config_pda(coin_mint: &Pubkey, authority: &Pubkey) -> Pubkey {
    let claim_window = DISTRIBUTION_CLAIM_WINDOW_SLOTS.to_le_bytes();
    Pubkey::find_program_address(
        &[b"dist_config", coin_mint.as_ref(), authority.as_ref(), &claim_window],
        &dist_id(),
    )
    .0
}
fn rd_so() -> String {
    format!(
        "{}/../target/deploy/residual_distributor.so",
        env!("CARGO_MANIFEST_DIR")
    )
}
fn subledger_so() -> String {
    format!(
        "{}/../target/deploy/subledger_program.so",
        env!("CARGO_MANIFEST_DIR")
    )
}

const COHORT_INSURANCE: u8 = 0;
const COHORT_BACKING: u8 = 1;
const COHORT_LP: u8 = 2;
const COHORT_TRADER: u8 = 3;
const COHORT_FUNDING_PAYER: u8 = 4;
const IX_INIT_REWARD_EPOCH: u8 = 6;

thread_local! {
    static REGISTERED_LINKS: RefCell<HashMap<(Pubkey, Pubkey), Vec<Pubkey>>> =
        RefCell::new(HashMap::new());
}

fn create_mint(svm: &mut LiteSVM, payer: &Keypair, authority: &Pubkey) -> Pubkey {
    let mint = Keypair::new();
    let rent = svm.minimum_balance_for_rent_exemption(spl_token::state::Mint::LEN);
    let ixs = [
        system_instruction::create_account(
            &payer.pubkey(),
            &mint.pubkey(),
            rent,
            spl_token::state::Mint::LEN as u64,
            &spl_token::ID,
        ),
        spl_token::instruction::initialize_mint(&spl_token::ID, &mint.pubkey(), authority, None, 6)
            .unwrap(),
    ];
    let tx = Transaction::new_signed_with_payer(
        &ixs,
        Some(&payer.pubkey()),
        &[payer, &mint],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).unwrap();
    mint.pubkey()
}
fn create_token_account(
    svm: &mut LiteSVM,
    payer: &Keypair,
    mint: &Pubkey,
    owner: &Pubkey,
) -> Pubkey {
    let acc = Keypair::new();
    let rent = svm.minimum_balance_for_rent_exemption(spl_token::state::Account::LEN);
    let ixs = [
        system_instruction::create_account(
            &payer.pubkey(),
            &acc.pubkey(),
            rent,
            spl_token::state::Account::LEN as u64,
            &spl_token::ID,
        ),
        spl_token::instruction::initialize_account(&spl_token::ID, &acc.pubkey(), mint, owner)
            .unwrap(),
    ];
    let tx = Transaction::new_signed_with_payer(
        &ixs,
        Some(&payer.pubkey()),
        &[payer, &acc],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).unwrap();
    acc.pubkey()
}
fn mint_to(
    svm: &mut LiteSVM,
    payer: &Keypair,
    mint: &Pubkey,
    authority: &Keypair,
    dest: &Pubkey,
    amount: u64,
) {
    let ix = spl_token::instruction::mint_to(
        &spl_token::ID,
        mint,
        dest,
        &authority.pubkey(),
        &[],
        amount,
    )
    .unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[payer, authority],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).unwrap();
}
fn revoke_mint(svm: &mut LiteSVM, payer: &Keypair, mint: &Pubkey, authority: &Keypair) {
    let ix = spl_token::instruction::set_authority(
        &spl_token::ID,
        mint,
        None,
        AuthorityType::MintTokens,
        &authority.pubkey(),
        &[],
    )
    .unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[payer, authority],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).unwrap();
}
fn token_amount(svm: &LiteSVM, acc: &Pubkey) -> u64 {
    spl_token::state::Account::unpack(&svm.get_account(acc).unwrap().data)
        .unwrap()
        .amount
}
fn set_slot(svm: &mut LiteSVM, slot: u64) {
    svm.set_sysvar(&Clock {
        slot,
        ..Default::default()
    });
}
fn send(
    svm: &mut LiteSVM,
    payer: &Keypair,
    ixs: &[Instruction],
    extra: &[&Keypair],
) -> Result<(), String> {
    svm.expire_blockhash();
    let bh = svm.latest_blockhash();
    let mut signers: Vec<&Keypair> = vec![payer];
    signers.extend_from_slice(extra);
    let tx = Transaction::new_signed_with_payer(ixs, Some(&payer.pubkey()), &signers, bh);
    svm.send_transaction(tx)
        .map(|_| ())
        .map_err(|e| format!("{:?}", e))
}

fn rd_init_data(
    supply: u64,
    emission_end: u64,
    insurance_bps: u16,
    backing_bps: u16,
    lp_bps: u16,
    finalize_window: u64,
    ins_pool: Pubkey,
    back_pool: Pubkey,
    market: Pubkey,
    extras: &[Pubkey],
    residual_fee_bps: Option<u16>,
) -> Vec<u8> {
    let mut d = vec![0u8];
    d.extend_from_slice(&supply.to_le_bytes());
    d.extend_from_slice(&emission_end.to_le_bytes());
    d.extend_from_slice(&insurance_bps.to_le_bytes());
    d.extend_from_slice(&backing_bps.to_le_bytes());
    d.extend_from_slice(&lp_bps.to_le_bytes());
    d.extend_from_slice(&finalize_window.to_le_bytes());
    d.extend_from_slice(ins_pool.as_ref());
    d.extend_from_slice(back_pool.as_ref());
    d.extend_from_slice(market.as_ref());
    d.push(extras.len() as u8);
    for e in extras {
        d.extend_from_slice(e.as_ref());
    }
    if let Some(fee_bps) = residual_fee_bps {
        d.extend_from_slice(&fee_bps.to_le_bytes());
    }
    d
}

#[allow(clippy::too_many_arguments)]
fn rd_init_data_with_funding_bps(
    supply: u64,
    emission_end: u64,
    insurance_bps: u16,
    backing_bps: u16,
    lp_bps: u16,
    funding_payer_bps: u16,
    finalize_window: u64,
    ins_pool: Pubkey,
    back_pool: Pubkey,
    market: Pubkey,
    extras: &[Pubkey],
    residual_fee_bps: u16,
) -> Vec<u8> {
    let mut d = rd_init_data(
        supply,
        emission_end,
        insurance_bps,
        backing_bps,
        lp_bps,
        finalize_window,
        ins_pool,
        back_pool,
        market,
        extras,
        Some(residual_fee_bps),
    );
    d.extend_from_slice(&funding_payer_bps.to_le_bytes());
    d
}

fn rd_init_accounts(
    payer: Pubkey,
    coin_mint: Pubkey,
    dist_config: Pubkey,
    percolator_program: Pubkey,
    subledger_program: Pubkey,
    rd_config: Pubkey,
    coin_mint_authority: Pubkey,
) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new(payer, true),
        AccountMeta::new_readonly(coin_mint, false),
        AccountMeta::new_readonly(dist_id(), false),
        AccountMeta::new_readonly(dist_config, false),
        AccountMeta::new_readonly(percolator_program, false),
        AccountMeta::new_readonly(subledger_program, false),
        AccountMeta::new(rd_config, false),
        AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        AccountMeta::new_readonly(coin_mint_authority, true),
    ]
}

#[derive(Clone, Copy)]
struct RewardEpochMarket {
    market: Pubkey,
    insurance_pool: Pubkey,
    backing_pool: Pubkey,
}

fn reward_epoch_pda(authority: &Pubkey, coin_mint: &Pubkey, epoch_id: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"rd_epoch",
            authority.as_ref(),
            coin_mint.as_ref(),
            &epoch_id.to_le_bytes(),
        ],
        &rd_id(),
    )
    .0
}

#[allow(clippy::too_many_arguments)]
fn reward_epoch_init_data(
    epoch_id: u64,
    start_slot: u64,
    emission_end_slot: u64,
    expected_reward_supply: u64,
    insurance_bps: u16,
    backing_bps: u16,
    lp_bps: u16,
    funding_payer_bps: u16,
    finalize_window: u64,
    fee_bps: u16,
    markets: &[RewardEpochMarket],
) -> Vec<u8> {
    let mut data = vec![IX_INIT_REWARD_EPOCH];
    data.extend_from_slice(&epoch_id.to_le_bytes());
    data.extend_from_slice(&start_slot.to_le_bytes());
    data.extend_from_slice(&emission_end_slot.to_le_bytes());
    data.extend_from_slice(&expected_reward_supply.to_le_bytes());
    data.extend_from_slice(&insurance_bps.to_le_bytes());
    data.extend_from_slice(&backing_bps.to_le_bytes());
    data.extend_from_slice(&lp_bps.to_le_bytes());
    data.extend_from_slice(&funding_payer_bps.to_le_bytes());
    data.extend_from_slice(&finalize_window.to_le_bytes());
    data.extend_from_slice(&fee_bps.to_le_bytes());
    data.push(markets.len() as u8);
    for scope in markets {
        data.extend_from_slice(scope.market.as_ref());
        data.extend_from_slice(scope.insurance_pool.as_ref());
        data.extend_from_slice(scope.backing_pool.as_ref());
    }
    data
}

fn reward_epoch_init_accounts(
    payer: Pubkey,
    authority: Pubkey,
    coin_mint: Pubkey,
    percolator_program: Pubkey,
    subledger_program: Pubkey,
    config: Pubkey,
    vault: Pubkey,
) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new(payer, true),
        AccountMeta::new_readonly(authority, true),
        AccountMeta::new_readonly(coin_mint, false),
        AccountMeta::new_readonly(percolator_program, false),
        AccountMeta::new_readonly(subledger_program, false),
        AccountMeta::new(config, false),
        AccountMeta::new_readonly(vault, false),
        AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
    ]
}

fn rd_config_fee_bps(svm: &LiteSVM, rd_config: Pubkey) -> u16 {
    u16::from_le_bytes(
        svm.get_account(&rd_config).unwrap().data[144..146]
            .try_into()
            .unwrap(),
    )
}

// Mock subledger Position at the pinned offsets: pool@8, owner@40, principal@72,
// withdrawn@88, start_slot@89, shares@104.
fn set_position(
    svm: &mut LiteSVM,
    key: &Pubkey,
    sub: &Pubkey,
    pool: &Pubkey,
    owner: &Pubkey,
    shares: u128,
    withdrawn: bool,
) {
    let mut data = vec![0u8; 160];
    data[..8].copy_from_slice(b"SUBPOS01");
    data[8..40].copy_from_slice(pool.as_ref());
    data[40..72].copy_from_slice(owner.as_ref());
    data[72..80].copy_from_slice(
        &u64::try_from(shares)
            .expect("mock principal fits u64")
            .to_le_bytes(),
    );
    data[88] = withdrawn as u8;
    data[104..120].copy_from_slice(&shares.to_le_bytes());
    svm.set_account(
        *key,
        Account {
            lamports: 1_000_000_000,
            data,
            owner: *sub,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

fn set_position_start_slot(svm: &mut LiteSVM, key: &Pubkey, start_slot: u64) {
    let mut account = svm.get_account(key).expect("position");
    account.data[89..97].copy_from_slice(&start_slot.to_le_bytes());
    svm.set_account(*key, account).unwrap();
}

fn set_position_principal(svm: &mut LiteSVM, key: &Pubkey, principal: u64) {
    let mut account = svm.get_account(key).expect("position");
    account.data[72..80].copy_from_slice(&principal.to_le_bytes());
    svm.set_account(*key, account).unwrap();
}
fn initialize_portfolio_header(
    data: &mut [u8],
    key: &Pubkey,
    market: &Pubkey,
    owner: &Pubkey,
) {
    data[..8].copy_from_slice(&0x5045_5243_5631_3600u64.to_le_bytes());
    data[8..10].copy_from_slice(&16u16.to_le_bytes());
    data[10] = 2;
    data[16..48].copy_from_slice(market.as_ref());
    data[48..80].copy_from_slice(key.as_ref());
    data[80..112].copy_from_slice(owner.as_ref());
    data[112..114].copy_from_slice(&1u16.to_le_bytes());
    data[114..116].copy_from_slice(&17u16.to_le_bytes());
    data[116..148].copy_from_slice(owner.as_ref());
}

// Mock Percolator PortfolioAccount at the pinned provenance/counter offsets.
fn set_portfolio(
    svm: &mut LiteSVM,
    key: &Pubkey,
    perc: &Pubkey,
    market: &Pubkey,
    owner: &Pubkey,
    received: u128,
    crystallized: u128,
) {
    let mut data = vec![0u8; 512];
    initialize_portfolio_header(&mut data, key, market, owner);
    data[196..212].copy_from_slice(&crystallized.to_le_bytes());
    data[228..244].copy_from_slice(&received.to_le_bytes());
    svm.set_account(
        *key,
        Account {
            lamports: 1_000_000_000,
            data,
            owner: *perc,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

// Like set_portfolio but also writes residual_spent@212.
fn set_portfolio_full(
    svm: &mut LiteSVM,
    key: &Pubkey,
    perc: &Pubkey,
    market: &Pubkey,
    owner: &Pubkey,
    received: u128,
    crystallized: u128,
    spent: u128,
) {
    let mut data = vec![0u8; 512];
    initialize_portfolio_header(&mut data, key, market, owner);
    data[196..212].copy_from_slice(&crystallized.to_le_bytes());
    data[212..228].copy_from_slice(&spent.to_le_bytes());
    data[228..244].copy_from_slice(&received.to_le_bytes());
    svm.set_account(
        *key,
        Account {
            lamports: 1_000_000_000,
            data,
            owner: *perc,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

fn set_portfolio_funding(
    svm: &mut LiteSVM,
    key: &Pubkey,
    perc: &Pubkey,
    market: &Pubkey,
    owner: &Pubkey,
    long_paid: u128,
    long_received: u128,
    short_paid: u128,
    short_received: u128,
) {
    let mut data = vec![0u8; 512];
    initialize_portfolio_header(&mut data, key, market, owner);
    data[244..260].copy_from_slice(&long_paid.to_le_bytes());
    data[260..276].copy_from_slice(&long_received.to_le_bytes());
    data[276..292].copy_from_slice(&short_paid.to_le_bytes());
    data[292..308].copy_from_slice(&short_received.to_le_bytes());
    svm.set_account(
        *key,
        Account {
            lamports: 1_000_000_000,
            data,
            owner: *perc,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

struct Env {
    rd_config: Pubkey,
    coin_mint: Pubkey,
    vault: Pubkey,
    mint_auth: Keypair,
    stub_sub: Pubkey,
    stub_perc: Pubkey,
    ins_pool: Pubkey,
    back_pool: Pubkey,
    market: Pubkey,
    supply: u64,
    emission_end: u64,
    finalize_window: u64,
}

// Init an rd_config (10/10/40/40) with a fully-funded rd-owned COIN vault (the self-service claim vault).
fn setup(svm: &mut LiteSVM, payer: &Keypair, supply: u64) -> Env {
    let emission_end = 2_000u64;
    let finalize_window = 500u64;
    let mint_auth = Keypair::new();
    let coin_mint = create_mint(svm, payer, &mint_auth.pubkey());
    let rd_config = Pubkey::find_program_address(&[b"rd_config", coin_mint.as_ref()], &rd_id()).0;
    let dist_config = dist_config_pda(&coin_mint, &rd_config);
    let vault = create_token_account(svm, payer, &coin_mint, &rd_config); // rd-owned claim vault
    mint_to(svm, payer, &coin_mint, &mint_auth, &vault, supply);

    let stub_sub = Pubkey::new_unique();
    let stub_perc = Pubkey::new_unique();
    let ins_pool = Pubkey::new_unique();
    let back_pool = Pubkey::new_unique();
    let market = Pubkey::new_unique();
    let d = rd_init_data(
        supply,
        emission_end,
        1_000,
        1_000,
        4_000,
        finalize_window,
        ins_pool,
        back_pool,
        market,
        &[],
        Some(0),
    );
    send(
        svm,
        payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: rd_init_accounts(
                payer.pubkey(),
                coin_mint,
                dist_config,
                stub_perc,
                stub_sub,
                rd_config,
                mint_auth.pubkey(),
            ),
            data: d,
        }],
        &[&mint_auth],
    )
    .expect("rd init");
    revoke_mint(svm, payer, &coin_mint, &mint_auth);
    Env {
        rd_config,
        coin_mint,
        vault,
        mint_auth,
        stub_sub,
        stub_perc,
        ins_pool,
        back_pool,
        market,
        supply,
        emission_end,
        finalize_window,
    }
}

fn setup_trader_reward_epoch(svm: &mut LiteSVM, payer: &Keypair, supply: u64) -> Env {
    let authority = Keypair::new();
    let mint_auth = Keypair::new();
    let coin_mint = create_mint(svm, payer, &mint_auth.pubkey());
    let epoch_id = 9_001u64;
    let rd_config = reward_epoch_pda(&authority.pubkey(), &coin_mint, epoch_id);
    let vault = create_token_account(svm, payer, &coin_mint, &rd_config);
    let stub_sub = Pubkey::new_unique();
    let stub_perc = Pubkey::new_unique();
    let market = Pubkey::new_unique();
    let emission_start = 100u64;
    let emission_end = 2_000u64;
    let finalize_window = 500u64;
    set_slot(svm, 50);
    send(
        svm,
        payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: reward_epoch_init_accounts(
                payer.pubkey(),
                authority.pubkey(),
                coin_mint,
                stub_perc,
                stub_sub,
                rd_config,
                vault,
            ),
            data: reward_epoch_init_data(
                epoch_id,
                emission_start,
                emission_end,
                0,
                0,
                0,
                0,
                0,
                finalize_window,
                0,
                &[RewardEpochMarket {
                    market,
                    insurance_pool: Pubkey::default(),
                    backing_pool: Pubkey::default(),
                }],
            ),
        }],
        &[&authority],
    )
    .expect("initialize trader-only reward epoch");
    mint_to(svm, payer, &coin_mint, &mint_auth, &vault, supply);
    revoke_mint(svm, payer, &coin_mint, &mint_auth);
    Env {
        rd_config,
        coin_mint,
        vault,
        mint_auth,
        stub_sub,
        stub_perc,
        ins_pool: Pubkey::default(),
        back_pool: Pubkey::default(),
        market,
        supply,
        emission_end,
        finalize_window,
    }
}

fn setup_funding_payer_only_with_fee_and_extras(
    svm: &mut LiteSVM,
    payer: &Keypair,
    supply: u64,
    fee_bps: u16,
    extras: &[Pubkey],
) -> Env {
    let emission_end = 2_000u64;
    let finalize_window = 500u64;
    let mint_auth = Keypair::new();
    let coin_mint = create_mint(svm, payer, &mint_auth.pubkey());
    let rd_config = Pubkey::find_program_address(&[b"rd_config", coin_mint.as_ref()], &rd_id()).0;
    let dist_config = dist_config_pda(&coin_mint, &rd_config);
    let vault = create_token_account(svm, payer, &coin_mint, &rd_config);
    mint_to(svm, payer, &coin_mint, &mint_auth, &vault, supply);

    let stub_sub = Pubkey::new_unique();
    let stub_perc = Pubkey::new_unique();
    let ins_pool = Pubkey::new_unique();
    let back_pool = Pubkey::new_unique();
    let market = Pubkey::new_unique();
    let d = rd_init_data_with_funding_bps(
        supply,
        emission_end,
        0,
        0,
        0,
        10_000,
        finalize_window,
        ins_pool,
        back_pool,
        market,
        extras,
        fee_bps,
    );
    send(
        svm,
        payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: rd_init_accounts(
                payer.pubkey(),
                coin_mint,
                dist_config,
                stub_perc,
                stub_sub,
                rd_config,
                mint_auth.pubkey(),
            ),
            data: d,
        }],
        &[&mint_auth],
    )
    .unwrap();
    revoke_mint(svm, payer, &coin_mint, &mint_auth);
    Env {
        rd_config,
        coin_mint,
        vault,
        mint_auth,
        stub_sub,
        stub_perc,
        ins_pool,
        back_pool,
        market,
        supply,
        emission_end,
        finalize_window,
    }
}

fn setup_funding_payer_only_with_fee(
    svm: &mut LiteSVM,
    payer: &Keypair,
    supply: u64,
    fee_bps: u16,
) -> Env {
    setup_funding_payer_only_with_fee_and_extras(svm, payer, supply, fee_bps, &[])
}

fn setup_funding_payer_only(svm: &mut LiteSVM, payer: &Keypair, supply: u64) -> Env {
    setup_funding_payer_only_with_fee(svm, payer, supply, 0)
}

fn setup_share_value_and_funding_split(
    svm: &mut LiteSVM,
    payer: &Keypair,
    supply: u64,
    insurance_bps: u16,
    backing_bps: u16,
    funding_payer_bps: u16,
) -> Env {
    let emission_end = 2_000u64;
    let finalize_window = 500u64;
    let mint_auth = Keypair::new();
    let coin_mint = create_mint(svm, payer, &mint_auth.pubkey());
    let rd_config = Pubkey::find_program_address(&[b"rd_config", coin_mint.as_ref()], &rd_id()).0;
    let dist_config = dist_config_pda(&coin_mint, &rd_config);
    let vault = create_token_account(svm, payer, &coin_mint, &rd_config);
    mint_to(svm, payer, &coin_mint, &mint_auth, &vault, supply);

    let stub_sub = Pubkey::new_unique();
    let stub_perc = Pubkey::new_unique();
    let ins_pool = Pubkey::new_unique();
    let back_pool = Pubkey::new_unique();
    let market = Pubkey::new_unique();
    let d = rd_init_data_with_funding_bps(
        supply,
        emission_end,
        insurance_bps,
        backing_bps,
        0,
        funding_payer_bps,
        finalize_window,
        ins_pool,
        back_pool,
        market,
        &[],
        0,
    );
    send(
        svm,
        payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: rd_init_accounts(
                payer.pubkey(),
                coin_mint,
                dist_config,
                stub_perc,
                stub_sub,
                rd_config,
                mint_auth.pubkey(),
            ),
            data: d,
        }],
        &[&mint_auth],
    )
    .expect("rd init with capital/funding split");
    revoke_mint(svm, payer, &coin_mint, &mint_auth);
    Env {
        rd_config,
        coin_mint,
        vault,
        mint_auth,
        stub_sub,
        stub_perc,
        ins_pool,
        back_pool,
        market,
        supply,
        emission_end,
        finalize_window,
    }
}

#[allow(clippy::too_many_arguments)]
fn setup_custom_split_with_fee_and_extras(
    svm: &mut LiteSVM,
    payer: &Keypair,
    supply: u64,
    insurance_bps: u16,
    backing_bps: u16,
    lp_bps: u16,
    funding_payer_bps: u16,
    fee_bps: u16,
    extras: &[Pubkey],
) -> Env {
    let emission_end = 2_000u64;
    let finalize_window = 500u64;
    let mint_auth = Keypair::new();
    let coin_mint = create_mint(svm, payer, &mint_auth.pubkey());
    let rd_config = Pubkey::find_program_address(&[b"rd_config", coin_mint.as_ref()], &rd_id()).0;
    let dist_config = dist_config_pda(&coin_mint, &rd_config);
    let vault = create_token_account(svm, payer, &coin_mint, &rd_config);
    mint_to(svm, payer, &coin_mint, &mint_auth, &vault, supply);

    let stub_sub = Pubkey::new_unique();
    let stub_perc = Pubkey::new_unique();
    let ins_pool = Pubkey::new_unique();
    let back_pool = Pubkey::new_unique();
    let market = Pubkey::new_unique();
    let d = rd_init_data_with_funding_bps(
        supply,
        emission_end,
        insurance_bps,
        backing_bps,
        lp_bps,
        funding_payer_bps,
        finalize_window,
        ins_pool,
        back_pool,
        market,
        extras,
        fee_bps,
    );
    send(
        svm,
        payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: rd_init_accounts(
                payer.pubkey(),
                coin_mint,
                dist_config,
                stub_perc,
                stub_sub,
                rd_config,
                mint_auth.pubkey(),
            ),
            data: d,
        }],
        &[&mint_auth],
    )
    .expect("custom rd init");
    revoke_mint(svm, payer, &coin_mint, &mint_auth);
    Env {
        rd_config,
        coin_mint,
        vault,
        mint_auth,
        stub_sub,
        stub_perc,
        ins_pool,
        back_pool,
        market,
        supply,
        emission_end,
        finalize_window,
    }
}

// Like setup(), but appends the OPTIONAL trailing residual_fee_bps (the anti-wash fee on LP/trader claims).
fn setup_with_fee(svm: &mut LiteSVM, payer: &Keypair, supply: u64, fee_bps: u16) -> Env {
    let emission_end = 2_000u64;
    let finalize_window = 500u64;
    let mint_auth = Keypair::new();
    let coin_mint = create_mint(svm, payer, &mint_auth.pubkey());
    let rd_config = Pubkey::find_program_address(&[b"rd_config", coin_mint.as_ref()], &rd_id()).0;
    let dist_config = dist_config_pda(&coin_mint, &rd_config);
    let vault = create_token_account(svm, payer, &coin_mint, &rd_config);
    mint_to(svm, payer, &coin_mint, &mint_auth, &vault, supply);
    let (stub_sub, stub_perc, ins_pool, back_pool, market) = (
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    );
    let d = rd_init_data(
        supply,
        emission_end,
        1_000,
        1_000,
        4_000,
        finalize_window,
        ins_pool,
        back_pool,
        market,
        &[],
        Some(fee_bps),
    );
    send(
        svm,
        payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: rd_init_accounts(
                payer.pubkey(),
                coin_mint,
                dist_config,
                stub_perc,
                stub_sub,
                rd_config,
                mint_auth.pubkey(),
            ),
            data: d,
        }],
        &[&mint_auth],
    )
    .expect("rd init with fee");
    revoke_mint(svm, payer, &coin_mint, &mint_auth);
    Env {
        rd_config,
        coin_mint,
        vault,
        mint_auth,
        stub_sub,
        stub_perc,
        ins_pool,
        back_pool,
        market,
        supply,
        emission_end,
        finalize_window,
    }
}

// LoF PROBE: reward multiplication must remain exact when a legal capital position's tenure
// multiplier makes `cohort_supply * points` exceed u128. Saturating that intermediate underpays
// the sole staker and locks the rest of the immutable COIN cohort forever.
#[test]
fn maximum_real_subledger_position_claims_its_full_reward_without_mul_overflow_loss() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    svm.add_program_from_file(subledger_program::id(), subledger_so())
        .unwrap();

    let payer = Keypair::new();
    let dao = Keypair::new();
    let owner = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    svm.airdrop(&owner.pubkey(), 10_000_000_000).unwrap();
    set_slot(&mut svm, 1);

    // Build the maximum principal through the real subledger public API.
    let collateral_authority = Keypair::new();
    let collateral_mint = create_mint(&mut svm, &payer, &collateral_authority.pubkey());
    let asset_id = 77u64;
    let no_market = Pubkey::default();
    let policy = [0u8];
    let domain = [0u8];
    let deposit_window = u64::MAX.to_le_bytes();
    let deposit_start = 0u64.to_le_bytes();
    let bootstrap_delay = 0u64.to_le_bytes();
    let pool = Pubkey::find_program_address(
        &[
            b"subledger_pool",
            collateral_mint.as_ref(),
            &asset_id.to_le_bytes(),
            no_market.as_ref(),
            no_market.as_ref(),
            no_market.as_ref(),
            &policy,
            &domain,
            &deposit_window,
            &deposit_start,
            &bootstrap_delay,
        ],
        &subledger_program::id(),
    )
    .0;
    let pool_vault = create_token_account(&mut svm, &payer, &collateral_mint, &pool);
    let mut init_pool_data = vec![0u8];
    init_pool_data.extend_from_slice(&asset_id.to_le_bytes());
    init_pool_data.push(0); // principal policy
    init_pool_data.push(0); // insurance domain
    send(
        &mut svm,
        &payer,
        &[Instruction {
            program_id: subledger_program::id(),
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new_readonly(collateral_mint, false),
                AccountMeta::new(pool, false),
                AccountMeta::new_readonly(pool_vault, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: init_pool_data,
        }],
        &[],
    )
    .expect("initialize real subledger pool");

    let owner_collateral =
        create_token_account(&mut svm, &payer, &collateral_mint, &owner.pubkey());
    mint_to(
        &mut svm,
        &payer,
        &collateral_mint,
        &collateral_authority,
        &owner_collateral,
        u64::MAX,
    );
    let position = Pubkey::find_program_address(
        &[b"subledger_position", pool.as_ref(), owner.pubkey().as_ref()],
        &subledger_program::id(),
    )
    .0;
    let mut deposit_data = vec![1u8];
    deposit_data.extend_from_slice(&u64::MAX.to_le_bytes());
    send(
        &mut svm,
        &payer,
        &[Instruction {
            program_id: subledger_program::id(),
            accounts: vec![
                AccountMeta::new(owner.pubkey(), true),
                AccountMeta::new(pool, false),
                AccountMeta::new(position, false),
                AccountMeta::new(owner_collateral, false),
                AccountMeta::new(pool_vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: deposit_data,
        }],
        &[&owner],
    )
    .expect("deposit maximum principal through subledger");
    assert_eq!(
        u64::from_le_bytes(
            svm.get_account(&position).unwrap().data[72..80]
                .try_into()
                .unwrap()
        ),
        u64::MAX,
        "the reward source is a real maximum-principal position"
    );

    // Allocate a fixed maximum COIN supply to one insurance cohort.
    let coin_authority = Keypair::new();
    let coin_mint = create_mint(&mut svm, &payer, &coin_authority.pubkey());
    let epoch_id = 9001u64;
    let rd_config = reward_epoch_pda(&dao.pubkey(), &coin_mint, epoch_id);
    let reward_vault = create_token_account(&mut svm, &payer, &coin_mint, &rd_config);
    mint_to(
        &mut svm,
        &payer,
        &coin_mint,
        &coin_authority,
        &reward_vault,
        u64::MAX,
    );
    revoke_mint(&mut svm, &payer, &coin_mint, &coin_authority);
    let market = Pubkey::new_unique();
    let stub_percolator = Pubkey::new_unique();
    let emission_start = 10u64;
    let emission_end = 14u64;
    let finalize_window = 1u64;
    send(
        &mut svm,
        &payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: reward_epoch_init_accounts(
                payer.pubkey(),
                dao.pubkey(),
                coin_mint,
                stub_percolator,
                subledger_program::id(),
                rd_config,
                reward_vault,
            ),
            data: reward_epoch_init_data(
                epoch_id,
                emission_start,
                emission_end,
                u64::MAX,
                10_000,
                0,
                0,
                0,
                finalize_window,
                0,
                &[RewardEpochMarket {
                    market,
                    insurance_pool: pool,
                    backing_pool: Pubkey::default(),
                }],
            ),
        }],
        &[&dao],
    )
    .expect("initialize maximum fixed reward epoch");
    let env = Env {
        rd_config,
        coin_mint,
        vault: reward_vault,
        mint_auth: Keypair::new(),
        stub_sub: subledger_program::id(),
        stub_perc: stub_percolator,
        ins_pool: pool,
        back_pool: Pubkey::default(),
        market,
        supply: u64::MAX,
        emission_end,
        finalize_window,
    };
    let recipient = create_token_account(&mut svm, &payer, &coin_mint, &owner.pubkey());

    set_slot(&mut svm, emission_start);
    register(
        &mut svm,
        &payer,
        &env,
        &owner,
        &owner.pubkey(),
        &position,
        COHORT_INSURANCE,
    )
    .expect("register real capital");
    set_slot(&mut svm, emission_end); // age 4 => floor(log2(age)) = 2
    crystallize(&mut svm, &payer, &env, &owner, &position).expect("crystallize maximum points");
    let stake = stake_pda_for_cohort(&env, &owner.pubkey(), &position, COHORT_INSURANCE);
    let expected_points = (u64::MAX as u128) * 2;
    assert_eq!(
        u128::from_le_bytes(
            svm.get_account(&stake).unwrap().data[176..192]
                .try_into()
                .unwrap()
        ),
        expected_points,
        "the legal point total crosses the u128 multiplication threshold"
    );

    set_slot(&mut svm, emission_end + finalize_window);
    freeze(&mut svm, &payer, &env).expect("freeze exact denominator");
    claim(
        &mut svm,
        &payer,
        &env,
        &owner,
        &recipient,
        Some(&position),
    )
    .expect("claim sole-staker allocation");
    assert_eq!(
        token_amount(&svm, &recipient),
        u64::MAX,
        "a sole staker receives the entire cohort; overflow must not strand COIN"
    );
    assert_eq!(token_amount(&svm, &reward_vault), 0);
}

// Conservation PROBE: the cohort denominator must remain the exact sum of stake points. If a
// second maximum counter were allowed to saturate the denominator at u128::MAX, both stakes would
// have `points == denominator` and could each calculate a full-cohort claim.
#[test]
fn crystallize_rejects_point_sum_overflow_instead_of_saturating_the_denominator() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup_funding_payer_only_with_fee(&mut svm, &payer, 1_000_000, 0);
    let alice = Keypair::new();
    let bob = Keypair::new();
    let alice_portfolio = Pubkey::new_unique();
    let bob_portfolio = Pubkey::new_unique();
    set_portfolio_funding(
        &mut svm,
        &alice_portfolio,
        &env.stub_perc,
        &env.market,
        &alice.pubkey(),
        0,
        0,
        0,
        0,
    );
    set_portfolio_funding(
        &mut svm,
        &bob_portfolio,
        &env.stub_perc,
        &env.market,
        &bob.pubkey(),
        0,
        0,
        0,
        0,
    );
    set_slot(&mut svm, 100);
    register(
        &mut svm,
        &payer,
        &env,
        &alice,
        &alice.pubkey(),
        &alice_portfolio,
        COHORT_FUNDING_PAYER,
    )
    .expect("register first funding payer");
    register(
        &mut svm,
        &payer,
        &env,
        &bob,
        &bob.pubkey(),
        &bob_portfolio,
        COHORT_FUNDING_PAYER,
    )
    .expect("register second funding payer");

    set_portfolio_funding(
        &mut svm,
        &alice_portfolio,
        &env.stub_perc,
        &env.market,
        &alice.pubkey(),
        u128::MAX,
        0,
        0,
        0,
    );
    set_portfolio_funding(
        &mut svm,
        &bob_portfolio,
        &env.stub_perc,
        &env.market,
        &bob.pubkey(),
        u128::MAX,
        0,
        0,
        0,
    );
    crystallize(&mut svm, &payer, &env, &alice, &alice_portfolio)
        .expect("first maximum counter fills denominator exactly");
    let config_before = svm.get_account(&env.rd_config).unwrap().data;
    let bob_stake = stake_pda_for_cohort(
        &env,
        &bob.pubkey(),
        &bob_portfolio,
        COHORT_FUNDING_PAYER,
    );
    let bob_stake_before = svm.get_account(&bob_stake).unwrap().data;

    assert!(
        crystallize(&mut svm, &payer, &env, &bob, &bob_portfolio).is_err(),
        "a second maximum stake must not saturate the denominator"
    );
    assert_eq!(
        svm.get_account(&env.rd_config).unwrap().data,
        config_before,
        "rejected overflow leaves the exact denominator unchanged"
    );
    assert_eq!(
        svm.get_account(&bob_stake).unwrap().data,
        bob_stake_before,
        "rejected overflow cannot persist a numerator outside the denominator"
    );
}

// LoF PROBE: the trader live cap scales frozen points down after spent principal rises. Its
// `points * cap_net / frozen_net` intermediate can exceed u128 even though the exact result fits.
// Saturating that intermediate underpays the irreversible claim and locks the remaining COIN.
#[test]
fn trader_live_cap_is_exact_when_its_intermediate_exceeds_u128() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64;
    let env = setup_custom_split_with_fee_and_extras(
        &mut svm,
        &payer,
        supply,
        0,
        0,
        0,
        0,
        0,
        &[],
    ); // trader receives the 100% remainder
    let owner = Keypair::new();
    let portfolio = Pubkey::new_unique();
    set_portfolio_full(
        &mut svm,
        &portfolio,
        &env.stub_perc,
        &env.market,
        &owner.pubkey(),
        0,
        0,
        0,
    );
    let recipient = create_token_account(&mut svm, &payer, &env.coin_mint, &owner.pubkey());

    set_slot(&mut svm, 100);
    register(
        &mut svm,
        &payer,
        &env,
        &owner,
        &owner.pubkey(),
        &portfolio,
        COHORT_TRADER,
    )
    .expect("register trader");

    let frozen_net = 1u128 << 125;
    set_portfolio_full(
        &mut svm,
        &portfolio,
        &env.stub_perc,
        &env.market,
        &owner.pubkey(),
        0,
        frozen_net,
        0,
    );
    set_slot(&mut svm, 104); // age 4 => multiplier 2
    crystallize(&mut svm, &payer, &env, &owner, &portfolio)
        .expect("crystallize large but representable trader points");
    let stake = stake_pda_for_cohort(&env, &owner.pubkey(), &portfolio, COHORT_TRADER);
    assert_eq!(
        u128::from_le_bytes(
            svm.get_account(&stake).unwrap().data[176..192]
                .try_into()
                .unwrap()
        ),
        1u128 << 126,
        "frozen points are exactly multiplier * frozen net"
    );

    set_slot(&mut svm, env.emission_end + env.finalize_window);
    freeze(&mut svm, &payer, &env).expect("freeze trader denominator");
    let live_net = frozen_net / 2;
    set_portfolio_full(
        &mut svm,
        &portfolio,
        &env.stub_perc,
        &env.market,
        &owner.pubkey(),
        0,
        frozen_net,
        frozen_net - live_net,
    );
    claim(
        &mut svm,
        &payer,
        &env,
        &owner,
        &recipient,
        Some(&portfolio),
    )
    .expect("claim exactly live-capped trader points");
    assert_eq!(
        token_amount(&svm, &recipient),
        supply / 2,
        "halving eligible loss pays exactly half the cohort despite a wide intermediate"
    );
    assert_eq!(
        token_amount(&svm, &env.vault),
        supply / 2,
        "only the intentionally forfeited half remains"
    );
}

#[test]
fn one_coin_mint_can_run_two_independent_dao_reward_epochs() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    let dao = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();

    let mint_auth = Keypair::new();
    let coin_mint = create_mint(&mut svm, &payer, &mint_auth.pubkey());
    let epoch_ids = [41u64, 42u64];
    let configs = epoch_ids.map(|epoch_id| reward_epoch_pda(&dao.pubkey(), &coin_mint, epoch_id));
    let vaults = configs.map(|config| create_token_account(&mut svm, &payer, &coin_mint, &config));

    let stub_sub = Pubkey::new_unique();
    let stub_perc = Pubkey::new_unique();
    let scope = RewardEpochMarket {
        market: Pubkey::new_unique(),
        insurance_pool: Pubkey::new_unique(),
        backing_pool: Pubkey::new_unique(),
    };
    let oi_only_scope = RewardEpochMarket {
        market: Pubkey::new_unique(),
        insurance_pool: Pubkey::default(),
        backing_pool: Pubkey::default(),
    };
    let start_slot = 100u64;
    let emission_end = 200u64;
    let finalize_window = 10u64;
    set_slot(&mut svm, 50);

    for ((epoch_id, config), vault) in epoch_ids.into_iter().zip(configs).zip(vaults) {
        let ix = Instruction {
            program_id: rd_id(),
            accounts: reward_epoch_init_accounts(
                payer.pubkey(),
                dao.pubkey(),
                coin_mint,
                stub_perc,
                stub_sub,
                config,
                vault,
            ),
            data: reward_epoch_init_data(
                epoch_id,
                start_slot,
                emission_end,
                0, // dynamic: freeze the COIN actually accumulated in this epoch vault
                1_000,
                1_000,
                0,
                8_000,
                finalize_window,
                0,
                &[scope, oi_only_scope],
            ),
        };
        send(&mut svm, &payer, &[ix], &[&dao]).expect("DAO initializes reward epoch");
    }

    let epoch_supplies = [1_000_000u64, 2_000_000u64];
    for (vault, supply) in vaults.into_iter().zip(epoch_supplies) {
        mint_to(&mut svm, &payer, &coin_mint, &mint_auth, &vault, supply);
    }
    revoke_mint(&mut svm, &payer, &coin_mint, &mint_auth);

    let envs =
        configs
            .into_iter()
            .zip(vaults)
            .zip(epoch_supplies)
            .map(|((rd_config, vault), supply)| Env {
                rd_config,
                coin_mint,
                vault,
                mint_auth: Keypair::new(),
                stub_sub,
                stub_perc,
                ins_pool: scope.insurance_pool,
                back_pool: scope.backing_pool,
                market: scope.market,
                supply,
                emission_end,
                finalize_window,
            });
    let envs = envs.collect::<Vec<_>>();

    let insurance_owner = Keypair::new();
    let backing_owner = Keypair::new();
    let funding_owner = Keypair::new();
    let insurance_position = Pubkey::new_unique();
    let backing_position = Pubkey::new_unique();
    let funding_portfolio = Pubkey::new_unique();
    set_position(
        &mut svm,
        &insurance_position,
        &stub_sub,
        &scope.insurance_pool,
        &insurance_owner.pubkey(),
        100,
        false,
    );
    set_position(
        &mut svm,
        &backing_position,
        &stub_sub,
        &scope.backing_pool,
        &backing_owner.pubkey(),
        100,
        false,
    );
    set_portfolio_funding(
        &mut svm,
        &funding_portfolio,
        &stub_perc,
        &oi_only_scope.market,
        &funding_owner.pubkey(),
        0,
        0,
        0,
        0,
    );

    let insurance_ata =
        create_token_account(&mut svm, &payer, &coin_mint, &insurance_owner.pubkey());
    let backing_ata = create_token_account(&mut svm, &payer, &coin_mint, &backing_owner.pubkey());
    let funding_ata = create_token_account(&mut svm, &payer, &coin_mint, &funding_owner.pubkey());

    assert!(
        register(
            &mut svm,
            &payer,
            &envs[0],
            &insurance_owner,
            &insurance_owner.pubkey(),
            &insurance_position,
            COHORT_INSURANCE,
        )
        .is_err(),
        "registration cannot begin before the immutable epoch start"
    );

    set_slot(&mut svm, start_slot);
    for env in &envs {
        register(
            &mut svm,
            &payer,
            env,
            &insurance_owner,
            &insurance_owner.pubkey(),
            &insurance_position,
            COHORT_INSURANCE,
        )
        .expect("insurance registers in epoch");
        register(
            &mut svm,
            &payer,
            env,
            &backing_owner,
            &backing_owner.pubkey(),
            &backing_position,
            COHORT_BACKING,
        )
        .expect("backing registers in epoch");
        register(
            &mut svm,
            &payer,
            env,
            &funding_owner,
            &funding_owner.pubkey(),
            &funding_portfolio,
            COHORT_FUNDING_PAYER,
        )
        .expect("funding payer registers in epoch");
    }

    set_portfolio_funding(
        &mut svm,
        &funding_portfolio,
        &stub_perc,
        &oi_only_scope.market,
        &funding_owner.pubkey(),
        7,
        100,
        3,
        100,
    );
    set_slot(&mut svm, emission_end);
    let late_owner = Keypair::new();
    let late_position = Pubkey::new_unique();
    set_position(
        &mut svm,
        &late_position,
        &stub_sub,
        &scope.insurance_pool,
        &late_owner.pubkey(),
        1_000_000,
        false,
    );
    assert!(
        register(
            &mut svm,
            &payer,
            &envs[0],
            &late_owner,
            &late_owner.pubkey(),
            &late_position,
            COHORT_INSURANCE,
        )
        .is_err(),
        "registration closes exactly at emission_end; late capital cannot dilute the epoch"
    );
    for env in &envs {
        crystallize(&mut svm, &payer, env, &insurance_owner, &insurance_position)
            .expect("insurance crystallizes");
        crystallize(&mut svm, &payer, env, &backing_owner, &backing_position)
            .expect("backing crystallizes");
        crystallize(&mut svm, &payer, env, &funding_owner, &funding_portfolio)
            .expect("funding payer crystallizes");
    }

    set_slot(&mut svm, emission_end + finalize_window);
    assert!(
        freeze_ix(
            &mut svm,
            &payer,
            envs[0].rd_config,
            coin_mint,
            envs[1].vault,
        )
        .is_err(),
        "a cranker cannot swap another epoch's same-mint vault into the one-shot freeze"
    );
    for env in &envs {
        freeze(&mut svm, &payer, env).expect("dynamic reward pool freezes");
        claim(
            &mut svm,
            &payer,
            env,
            &insurance_owner,
            &insurance_ata,
            Some(&insurance_position),
        )
        .expect("insurance claims epoch reward");
        claim(
            &mut svm,
            &payer,
            env,
            &backing_owner,
            &backing_ata,
            Some(&backing_position),
        )
        .expect("backing claims epoch reward");
        claim(&mut svm, &payer, env, &funding_owner, &funding_ata, None)
            .expect("funding payer claims epoch reward");
        assert_eq!(token_amount(&svm, &env.vault), 0);
    }

    assert_eq!(token_amount(&svm, &insurance_ata), 300_000);
    assert_eq!(token_amount(&svm, &backing_ata), 300_000);
    assert_eq!(token_amount(&svm, &funding_ata), 2_400_000);
}

// ATTACK PROBE: reward-epoch pool scopes must be globally segregated by cohort. If one
// pool appears as insurance in one market tuple and backing in another, the same owner-bound
// position gets two distinct stake PDAs and can consume both cohort allocations.
#[test]
fn reward_epoch_rejects_cross_domain_pool_alias_no_double_claim() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    let dao = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();

    let mint_auth = Keypair::new();
    let coin_mint = create_mint(&mut svm, &payer, &mint_auth.pubkey());
    let epoch_id = 43u64;
    let rd_config = reward_epoch_pda(&dao.pubkey(), &coin_mint, epoch_id);
    let vault = create_token_account(&mut svm, &payer, &coin_mint, &rd_config);
    let aliased_pool = Pubkey::new_unique();
    let scopes = [
        RewardEpochMarket {
            market: Pubkey::new_unique(),
            insurance_pool: aliased_pool,
            backing_pool: Pubkey::new_unique(),
        },
        RewardEpochMarket {
            market: Pubkey::new_unique(),
            insurance_pool: Pubkey::new_unique(),
            backing_pool: aliased_pool,
        },
    ];
    set_slot(&mut svm, 50);
    let result = send(
        &mut svm,
        &payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: reward_epoch_init_accounts(
                payer.pubkey(),
                dao.pubkey(),
                coin_mint,
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                rd_config,
                vault,
            ),
            data: reward_epoch_init_data(epoch_id, 100, 104, 0, 5_000, 5_000, 0, 0, 1, 0, &scopes),
        }],
        &[&dao],
    );
    assert!(
        result.is_err(),
        "one pool cannot source both insurance and backing reward families"
    );
    assert!(
        svm.get_account(&rd_config).is_none(),
        "a rejected alias must leave the canonical epoch PDA available"
    );
}

// The original/genesis initializer must enforce the same domain segregation as reusable epochs.
// Otherwise one position in the aliased pool can derive distinct insurance and backing stake PDAs
// and consume both COIN allocations for one principal balance.
#[test]
fn legacy_genesis_rejects_cross_domain_pool_alias_no_double_claim() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();

    let aliased_pool = Pubkey::new_unique();
    assert!(
        try_init(
            &mut svm,
            &payer,
            1_000_000,
            5_000,
            5_000,
            0,
            aliased_pool,
            aliased_pool,
            Pubkey::default(),
        )
        .is_err(),
        "one genesis pool cannot source both capital reward families"
    );
}

#[test]
fn reward_epoch_rejects_funding_points_created_after_emission_end() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    let dao = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();

    let mint_auth = Keypair::new();
    let coin_mint = create_mint(&mut svm, &payer, &mint_auth.pubkey());
    let epoch_id = 73u64;
    let rd_config = reward_epoch_pda(&dao.pubkey(), &coin_mint, epoch_id);
    let vault = create_token_account(&mut svm, &payer, &coin_mint, &rd_config);
    let stub_sub = Pubkey::new_unique();
    let stub_perc = Pubkey::new_unique();
    let market = Pubkey::new_unique();
    let start_slot = 100u64;
    let emission_end = 200u64;
    let finalize_window = 10u64;
    set_slot(&mut svm, 50);
    send(
        &mut svm,
        &payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: reward_epoch_init_accounts(
                payer.pubkey(),
                dao.pubkey(),
                coin_mint,
                stub_perc,
                stub_sub,
                rd_config,
                vault,
            ),
            data: reward_epoch_init_data(
                epoch_id,
                start_slot,
                emission_end,
                0,
                0,
                0,
                0,
                10_000,
                finalize_window,
                0,
                &[RewardEpochMarket {
                    market,
                    insurance_pool: Pubkey::default(),
                    backing_pool: Pubkey::default(),
                }],
            ),
        }],
        &[&dao],
    )
    .expect("initialize funding-only reward epoch");
    let supply = 1_000_000u64;
    mint_to(&mut svm, &payer, &coin_mint, &mint_auth, &vault, supply);
    revoke_mint(&mut svm, &payer, &coin_mint, &mint_auth);
    let env = Env {
        rd_config,
        coin_mint,
        vault,
        mint_auth: Keypair::new(),
        stub_sub,
        stub_perc,
        ins_pool: Pubkey::default(),
        back_pool: Pubkey::default(),
        market,
        supply,
        emission_end,
        finalize_window,
    };

    let farmer = Keypair::new();
    let portfolio = Pubkey::new_unique();
    set_portfolio_funding(
        &mut svm,
        &portfolio,
        &stub_perc,
        &market,
        &farmer.pubkey(),
        0,
        0,
        0,
        0,
    );
    set_slot(&mut svm, start_slot);
    register(
        &mut svm,
        &payer,
        &env,
        &farmer,
        &farmer.pubkey(),
        &portfolio,
        COHORT_FUNDING_PAYER,
    )
    .expect("register during the reward period");

    // The immutable reward period is over. Growing a cumulative counter now
    // must not let a registered portfolio mint points during the grace window.
    set_slot(&mut svm, emission_end + 1);
    set_portfolio_funding(
        &mut svm,
        &portfolio,
        &stub_perc,
        &market,
        &farmer.pubkey(),
        1_000_000,
        0,
        0,
        0,
    );
    assert!(
        crystallize(&mut svm, &payer, &env, &farmer, &portfolio).is_err(),
        "counters created after emission_end cannot enter the frozen denominator"
    );

    set_slot(&mut svm, emission_end + finalize_window);
    freeze(&mut svm, &payer, &env).expect("freeze after the grace window");
    let recipient = create_token_account(&mut svm, &payer, &coin_mint, &farmer.pubkey());
    claim(&mut svm, &payer, &env, &farmer, &recipient, None)
        .expect("zero-point stake remains consumable");
    assert_eq!(token_amount(&svm, &recipient), 0);
    assert_eq!(token_amount(&svm, &vault), supply);
}

// ATTACK PROBE: shares from distinct pools are not fungible units. Equal base-unit deposits can
// receive different share counts solely because each pool has a different loss/surplus history.
// A multi-market epoch must reward comparable capital at risk, not sum raw cross-pool shares.
#[test]
fn equal_principal_across_pools_is_not_diluted_by_unrelated_share_prices() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    let dao = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();

    let mint_auth = Keypair::new();
    let coin_mint = create_mint(&mut svm, &payer, &mint_auth.pubkey());
    let epoch_id = 77u64;
    let rd_config = reward_epoch_pda(&dao.pubkey(), &coin_mint, epoch_id);
    let vault = create_token_account(&mut svm, &payer, &coin_mint, &rd_config);
    let stub_sub = Pubkey::new_unique();
    let stub_perc = Pubkey::new_unique();
    let primary = RewardEpochMarket {
        market: Pubkey::new_unique(),
        insurance_pool: Pubkey::new_unique(),
        backing_pool: Pubkey::default(),
    };
    let second = RewardEpochMarket {
        market: Pubkey::new_unique(),
        insurance_pool: Pubkey::new_unique(),
        backing_pool: Pubkey::default(),
    };
    let supply = 1_000_000u64;
    let emission_end = 2_000u64;
    let finalize_window = 10u64;
    set_slot(&mut svm, 50);
    send(
        &mut svm,
        &payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: reward_epoch_init_accounts(
                payer.pubkey(),
                dao.pubkey(),
                coin_mint,
                stub_perc,
                stub_sub,
                rd_config,
                vault,
            ),
            data: reward_epoch_init_data(
                epoch_id,
                100,
                emission_end,
                supply,
                10_000,
                0,
                0,
                0,
                finalize_window,
                0,
                &[primary, second],
            ),
        }],
        &[&dao],
    )
    .expect("initialize two-pool reward epoch");
    mint_to(&mut svm, &payer, &coin_mint, &mint_auth, &vault, supply);
    revoke_mint(&mut svm, &payer, &coin_mint, &mint_auth);
    let env = Env {
        rd_config,
        coin_mint,
        vault,
        mint_auth: Keypair::new(),
        stub_sub,
        stub_perc,
        ins_pool: primary.insurance_pool,
        back_pool: Pubkey::default(),
        market: primary.market,
        supply,
        emission_end,
        finalize_window,
    };

    set_slot(&mut svm, 100);
    let a = Keypair::new();
    let b = Keypair::new();
    let a_pos = Pubkey::new_unique();
    let b_pos = Pubkey::new_unique();
    // Both positions risk 100 base units. Pool B's pre-existing 2x share price means the real
    // subledger mints half as many shares for the same new deposit.
    set_position(
        &mut svm,
        &a_pos,
        &stub_sub,
        &primary.insurance_pool,
        &a.pubkey(),
        100_000_000,
        false,
    );
    set_position_principal(&mut svm, &a_pos, 100);
    set_position_start_slot(&mut svm, &a_pos, 100);
    set_position(
        &mut svm,
        &b_pos,
        &stub_sub,
        &second.insurance_pool,
        &b.pubkey(),
        50_000_000,
        false,
    );
    set_position_principal(&mut svm, &b_pos, 100);
    set_position_start_slot(&mut svm, &b_pos, 100);
    register(
        &mut svm,
        &payer,
        &env,
        &a,
        &a.pubkey(),
        &a_pos,
        COHORT_INSURANCE,
    )
    .expect("register primary-pool depositor");
    register(
        &mut svm,
        &payer,
        &env,
        &b,
        &b.pubkey(),
        &b_pos,
        COHORT_INSURANCE,
    )
    .expect("register second-pool depositor");

    set_slot(&mut svm, 1_124);
    crystallize(&mut svm, &payer, &env, &a, &a_pos).expect("crystallize primary");
    crystallize(&mut svm, &payer, &env, &b, &b_pos).expect("crystallize second");
    set_slot(&mut svm, emission_end + finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");
    let a_ata = create_token_account(&mut svm, &payer, &coin_mint, &a.pubkey());
    let b_ata = create_token_account(&mut svm, &payer, &coin_mint, &b.pubkey());
    claim(&mut svm, &payer, &env, &a, &a_ata, Some(&a_pos)).expect("claim primary");
    claim(&mut svm, &payer, &env, &b, &b_ata, Some(&b_pos)).expect("claim second");
    assert_eq!(token_amount(&svm, &a_ata), 500_000);
    assert_eq!(token_amount(&svm, &b_ata), 500_000);
}

// DoS PROBE (lamport-prefund front-run brick, sweep tick D): the rd creates its rd_config (and every stake) PDA
// via create_pda. If that used a naive system create_account, a front-runner could transfer 1 lamport to the
// canonical rd_config PDA (system-owned, empty) BEFORE the genesis inits — create_account fails on a funded
// account, so the rd_config could NEVER be initialized and the ENTIRE residual distribution would be permanently
// bricked (no cohort could ever claim). The same DoS on a stake PDA would deny a single victim their share.
// distribution + gv already fixed this with a robust create; this pins the rd. init must succeed over the dust.
#[test]
fn init_is_not_bricked_by_a_lamport_prefund_of_the_rd_config_pda() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64;
    let mint_auth = Keypair::new();
    let coin_mint = create_mint(&mut svm, &payer, &mint_auth.pubkey());
    let rd_config = Pubkey::find_program_address(&[b"rd_config", coin_mint.as_ref()], &rd_id()).0;
    let dist_config = dist_config_pda(&coin_mint, &rd_config);
    let vault = create_token_account(&mut svm, &payer, &coin_mint, &rd_config);
    mint_to(&mut svm, &payer, &coin_mint, &mint_auth, &vault, supply);

    // ATTACK: a front-runner dusts the canonical rd_config PDA with lamports (system-owned, empty).
    svm.set_account(
        rd_config,
        Account {
            lamports: 1,
            data: vec![],
            owner: solana_sdk::system_program::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();

    let (stub_sub, stub_perc, ins_pool, back_pool, market) = (
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    );
    let d = rd_init_data(
        supply,
        2_000,
        1_000,
        1_000,
        4_000,
        500,
        ins_pool,
        back_pool,
        market,
        &[],
        Some(0),
    );
    let r = send(
        &mut svm,
        &payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: rd_init_accounts(
                payer.pubkey(),
                coin_mint,
                dist_config,
                stub_perc,
                stub_sub,
                rd_config,
                mint_auth.pubkey(),
            ),
            data: d,
        }],
        &[&mint_auth],
    );
    assert!(r.is_ok(), "rd init must succeed despite a lamport-prefund of the config PDA (no front-run brick): {r:?}");
    revoke_mint(&mut svm, &payer, &coin_mint, &mint_auth);
    // The config really got initialized (program-owned, sized), not left as the dusted system stub.
    let acc = svm.get_account(&rd_config).unwrap();
    assert_eq!(
        acc.owner,
        rd_id(),
        "rd_config is now program-owned (robust create adopted the dusted PDA)"
    );
}

#[test]
fn init_requires_current_coin_mint_authority_no_config_squat() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();

    let supply = 1_000_000u64;
    let mint_auth = Keypair::new();
    let coin_mint = create_mint(&mut svm, &payer, &mint_auth.pubkey());
    let rd_config = Pubkey::find_program_address(&[b"rd_config", coin_mint.as_ref()], &rd_id()).0;
    let dist_config = dist_config_pda(&coin_mint, &rd_config);
    let stub_perc = Pubkey::new_unique();
    let stub_sub = Pubkey::new_unique();
    let ins_pool = Pubkey::new_unique();
    let back_pool = Pubkey::new_unique();
    let market = Pubkey::new_unique();
    let d = rd_init_data(
        supply,
        2_000,
        1_000,
        1_000,
        4_000,
        500,
        ins_pool,
        back_pool,
        market,
        &[],
        Some(0),
    );

    let attacker = send(
        &mut svm,
        &payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new_readonly(coin_mint, false),
                AccountMeta::new_readonly(dist_id(), false),
                AccountMeta::new_readonly(dist_config, false),
                AccountMeta::new_readonly(stub_perc, false),
                AccountMeta::new_readonly(stub_sub, false),
                AccountMeta::new(rd_config, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: d.clone(),
        }],
        &[],
    );
    assert!(
        attacker.is_err(),
        "init without the current coin mint authority must not squat the canonical rd_config"
    );
    assert!(
        svm.get_account(&rd_config).is_none(),
        "rejected attacker init must not create rd_config"
    );

    let wrong_auth = Keypair::new();
    let wrong = send(
        &mut svm,
        &payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: rd_init_accounts(
                payer.pubkey(),
                coin_mint,
                dist_config,
                stub_perc,
                stub_sub,
                rd_config,
                wrong_auth.pubkey(),
            ),
            data: d.clone(),
        }],
        &[&wrong_auth],
    );
    assert!(
        wrong.is_err(),
        "a signer that is not the current coin mint authority must not initialize rd_config"
    );
    assert!(
        svm.get_account(&rd_config).is_none(),
        "wrong-authority init must not create rd_config"
    );

    send(
        &mut svm,
        &payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: rd_init_accounts(
                payer.pubkey(),
                coin_mint,
                dist_config,
                stub_perc,
                stub_sub,
                rd_config,
                mint_auth.pubkey(),
            ),
            data: d.clone(),
        }],
        &[&mint_auth],
    )
    .expect("the current mint authority can initialize");

    let later = send(
        &mut svm,
        &payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: rd_init_accounts(
                payer.pubkey(),
                coin_mint,
                dist_config,
                stub_perc,
                stub_sub,
                rd_config,
                mint_auth.pubkey(),
            ),
            data: d,
        }],
        &[&mint_auth],
    );
    assert!(
        later.is_err(),
        "after the legitimate init, rd_config remains one-shot"
    );
}

#[test]
fn init_omitted_fee_uses_default_but_explicit_zero_remains_allowed() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();

    let init_case = |svm: &mut LiteSVM, residual_fee_bps: Option<u16>| -> Pubkey {
        let mint_auth = Keypair::new();
        let coin_mint = create_mint(svm, &payer, &mint_auth.pubkey());
        let rd_config =
            Pubkey::find_program_address(&[b"rd_config", coin_mint.as_ref()], &rd_id()).0;
        let dist_config = dist_config_pda(&coin_mint, &rd_config);
        let stub_perc = Pubkey::new_unique();
        let stub_sub = Pubkey::new_unique();
        let d = rd_init_data(
            1_000_000,
            2_000,
            1_000,
            1_000,
            4_000,
            500,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            &[],
            residual_fee_bps,
        );
        send(
            svm,
            &payer,
            &[Instruction {
                program_id: rd_id(),
                accounts: rd_init_accounts(
                    payer.pubkey(),
                    coin_mint,
                    dist_config,
                    stub_perc,
                    stub_sub,
                    rd_config,
                    mint_auth.pubkey(),
                ),
                data: d,
            }],
            &[&mint_auth],
        )
        .expect("rd init");
        rd_config
    };

    let omitted = init_case(&mut svm, None);
    assert_eq!(
        rd_config_fee_bps(&svm, omitted),
        residual_distributor::DEFAULT_FEE_SUPPORT_BPS,
        "omitting the optional residual_fee_bps should use the declared default",
    );

    let explicit_zero = init_case(&mut svm, Some(0));
    assert_eq!(
        rd_config_fee_bps(&svm, explicit_zero),
        0,
        "explicit 0% fee remains an intentional no-fee config"
    );
}

// DoS PROBE (lamport-prefund of a VICTIM's stake PDA, sweep tick D): the rd_config test above pins the
// whole-system brick; this pins the per-victim variant on the REGISTER call site. A griefer can transfer 1
// lamport to a backer's deterministic stake PDA (no signature needed) to try to block their registration and
// deny them their cohort share. The robust create_pda must adopt the dusted PDA so the victim still registers.
// (Parity with subledger's dusting_a_depositors_position_pda_cannot_block_their_deposit.)
#[test]
fn register_is_not_bricked_by_a_lamport_prefund_of_the_victims_stake_pda() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);

    let victim = Keypair::new();
    let pf = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &victim.pubkey(),
        0,
        0,
    );

    // ATTACK: dust the victim's deterministic stake PDA with 1 lamport before they register.
    let stake = stake_pda(&env, &victim.pubkey(), &pf);
    svm.set_account(
        stake,
        Account {
            lamports: 1,
            data: vec![],
            owner: solana_sdk::system_program::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();

    // The victim can STILL register (robust create adopts the dusted PDA) — their share is not denied.
    register(
        &mut svm,
        &payer,
        &env,
        &victim,
        &victim.pubkey(),
        &pf,
        COHORT_LP,
    )
    .expect("register must succeed over a lamport-prefunded stake PDA (no per-victim brick)");
    assert_eq!(
        svm.get_account(&stake).unwrap().owner,
        rd_id(),
        "stake PDA adopted + program-owned despite the dust"
    );
}

// Like setup(), but configures the IL+ multi-market allow-list: `extras` are ADDITIONAL trusted-Pyth markets
// (beyond the primary `market`) the LP/trader cohorts will also accept. Returns the Env (primary market).
fn setup_with_extra_markets(
    svm: &mut LiteSVM,
    payer: &Keypair,
    supply: u64,
    extras: &[Pubkey],
) -> Env {
    let emission_end = 2_000u64;
    let finalize_window = 500u64;
    let mint_auth = Keypair::new();
    let coin_mint = create_mint(svm, payer, &mint_auth.pubkey());
    let rd_config = Pubkey::find_program_address(&[b"rd_config", coin_mint.as_ref()], &rd_id()).0;
    let dist_config = dist_config_pda(&coin_mint, &rd_config);
    let vault = create_token_account(svm, payer, &coin_mint, &rd_config);
    mint_to(svm, payer, &coin_mint, &mint_auth, &vault, supply);
    let (stub_sub, stub_perc, ins_pool, back_pool, market) = (
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    );
    let d = rd_init_data(
        supply,
        emission_end,
        1_000,
        1_000,
        4_000,
        finalize_window,
        ins_pool,
        back_pool,
        market,
        extras,
        Some(0),
    );
    send(
        svm,
        payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: rd_init_accounts(
                payer.pubkey(),
                coin_mint,
                dist_config,
                stub_perc,
                stub_sub,
                rd_config,
                mint_auth.pubkey(),
            ),
            data: d,
        }],
        &[&mint_auth],
    )
    .expect("rd init with extra markets");
    revoke_mint(svm, payer, &coin_mint, &mint_auth);
    Env {
        rd_config,
        coin_mint,
        vault,
        mint_auth,
        stub_sub,
        stub_perc,
        ins_pool,
        back_pool,
        market,
        supply,
        emission_end,
        finalize_window,
    }
}

// FREE-FARM PROBE (finding IL+ multi-market allow-list, sweep tick D): register_rejects_portfolio_from_a_foreign_market
// pins the SINGLE-market case (count 0). The IL+ extension allows up to 9 EXTRA trusted-Pyth markets, and that
// path was untested: it must (a) ACCEPT a portfolio from an allow-listed extra, and (b) still REJECT one from an
// off-list market — even though the list is now longer. An off-list market is an attacker's own auth-mark oracle
// on which crystallized_loss/received are freely manufacturable, so a leak here is a direct COIN free-farm.
#[test]
fn allow_list_accepts_a_listed_extra_market_and_still_rejects_an_off_list_market() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let extra_a = Pubkey::new_unique();
    let extra_b = Pubkey::new_unique();
    let env = setup_with_extra_markets(&mut svm, &payer, 1_000_000, &[extra_a, extra_b]);
    set_slot(&mut svm, 100);

    // (1) trader portfolio whose provenance is an allow-listed EXTRA market -> ACCEPTED.
    let t1 = Keypair::new();
    let pf1 = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &pf1,
        &env.stub_perc,
        &extra_b,
        &t1.pubkey(),
        0,
        9_000,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &t1,
        &t1.pubkey(),
        &pf1,
        COHORT_TRADER,
    )
    .expect("an allow-listed extra market must be accepted");

    // (2) trader portfolio from an OFF-list market (attacker's own auth-mark oracle) -> REJECTED, even with the
    // longer list. This is the free-farm boundary: no off-list market can mint trader/LP points.
    let t2 = Keypair::new();
    let pf2 = Pubkey::new_unique();
    let off_list = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &pf2,
        &env.stub_perc,
        &off_list,
        &t2.pubkey(),
        0,
        9_000,
    );
    assert!(
        register(
            &mut svm,
            &payer,
            &env,
            &t2,
            &t2.pubkey(),
            &pf2,
            COHORT_TRADER
        )
        .is_err(),
        "an off-list (attacker-oracle'd) market must be rejected even when extras are configured"
    );

    // (3) the PRIMARY market still counts (the extras didn't displace it).
    let t3 = Keypair::new();
    let pf3 = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &pf3,
        &env.stub_perc,
        &env.market,
        &t3.pubkey(),
        0,
        9_000,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &t3,
        &t3.pubkey(),
        &pf3,
        COHORT_TRADER,
    )
    .expect("the primary market still counts");
}

// DoS/HYGIENE PROBE (allow-list init bounds, sweep tick D): the extra-market tail is `count: u8` + count keys.
// init must bound count to MAX_EXTRA_MARKETS (=9) and reject a default or primary-duplicate extra — else a
// malformed list could over-read or admit a junk/aliased market into the trusted scope.
#[test]
fn init_rejects_a_malformed_or_overlong_extra_market_allow_list() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    // count = 10 (> MAX_EXTRA_MARKETS 9) is rejected before any key is read (no over-read).
    let over = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        setup_with_extra_markets(&mut svm, &payer, 1_000_000, &[Pubkey::new_unique(); 10])
    }));
    assert!(
        over.is_err(),
        "an allow-list of 10 extras (> MAX 9) must be rejected at init"
    );
    // a default (zero) extra key is rejected.
    let zero = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        setup_with_extra_markets(&mut svm, &payer, 1_000_000, &[Pubkey::default()])
    }));
    assert!(zero.is_err(), "a default extra market key must be rejected");
}

// FINDING NZ: the anti-wash fee is skimmed from LP/trader (PnL-flow) claims and RETAINED in the vault, but
// NOT from the capital (insurance/backing, capital-at-risk) cohorts. A sole LP staker with a 20% fee
// claims 80% of its cohort; the 20% stays locked in the vault. A sole insurance staker pays nothing.
// ANTI-WASH FEE DUST-DODGE (surface D, follow-up: last tick established the claim fee is the SOLE economic bound
// on the cross-margin delta-neutral wash). The fee = amount * fee_bps / 10000. If that FLOORS, a claim small
// enough (amount * fee_bps < 10000) rounds the fee to ZERO -- so a Sybil farmer who FRAGMENTS one farm into many
// dust stakes pays 0 fee on each and dodges the anti-wash skim entirely. This pins that fragmentation cannot
// reduce the effective fee below the intended rate (the fix CEILs the fee so every nonzero LP/trader claim pays
// >= 1 atom, making dust claims pay a >= -rate fee instead of 0). supply 100 -> trader cohort 40; 10 equal dust
// stakes each claim 4 -> floor fee 0 (DODGED) vs ceil fee 1 (>= the 20% intended).
#[test]
fn anti_wash_fee_cannot_be_dust_dodged_by_fragmentation() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 100u64; // trader cohort = 40% = 40 atoms
    let env = setup_with_fee(&mut svm, &payer, supply, 2_000); // 20% anti-wash fee
    set_slot(&mut svm, 100);

    // N trader stakes, each a stub portfolio with EQUAL crystallized loss -> each claims trader_supply/N = 4 atoms.
    let n = 10usize;
    let mut traders: Vec<(Keypair, Pubkey)> = Vec::new();
    for _ in 0..n {
        let t = Keypair::new();
        let pf = Pubkey::new_unique();
        set_portfolio(
            &mut svm,
            &pf,
            &env.stub_perc,
            &env.market,
            &t.pubkey(),
            0,
            0,
        ); // snap 0 at register
        register(&mut svm, &payer, &env, &t, &t.pubkey(), &pf, COHORT_TRADER).expect("reg trader");
        set_portfolio(
            &mut svm,
            &pf,
            &env.stub_perc,
            &env.market,
            &t.pubkey(),
            0,
            1_000,
        ); // equal crystallized loss
        traders.push((t, pf));
    }
    set_slot(&mut svm, 1_000); // tenure 900 -> floor(log2)=9 (uniform across all stakes)
    for (t, pf) in &traders {
        crystallize(&mut svm, &payer, &env, t, pf).expect("cry");
    }
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    let mut total_payout = 0u64;
    for (t, pf) in &traders {
        let ata = create_token_account(&mut svm, &payer, &env.coin_mint, &t.pubkey());
        claim(&mut svm, &payer, &env, t, &ata, Some(pf)).expect("claim");
        total_payout += token_amount(&svm, &ata);
    }
    // trader cohort = 40; the INTENDED 20% fee on the aggregate is 8, so an un-dodgeable fee leaves <= 32 paid out.
    let trader_cohort = supply as u128 * 4_000 / 10_000; // 40
    let intended_fee = (trader_cohort * 2_000 / 10_000) as u64; // 8
                                                                // FRAGMENTATION DODGE GUARD: total paid out must NOT exceed cohort - intended_fee. A floor fee lets each
                                                                // 4-atom dust claim pay 0 -> total_payout = 40 > 32 (fee fully dodged). A ceil fee makes each pay >= 1 ->
                                                                // total_payout <= 32. Pin that fragmentation cannot dodge the anti-wash skim.
    assert!(total_payout <= trader_cohort as u64 - intended_fee,
        "anti-wash fee dust-dodged via fragmentation: {n} dust claims paid out {total_payout} of the {trader_cohort} trader cohort (intended fee {intended_fee} skimmed -> should leave <= {})", trader_cohort as u64 - intended_fee);
    // The dodged/retained fee stays locked in the vault (deflationary), never over-drawn.
    assert!(
        total_payout as u128 <= trader_cohort,
        "conservation: never pays more than the cohort"
    );
}

#[test]
fn lp_trader_claim_pays_the_anti_wash_fee_share_value_cohorts_dont() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64; // lp = 40% = 400_000 ; insurance = 10% = 100_000
    let env = setup_with_fee(&mut svm, &payer, supply, 2_000); // 20% anti-wash fee
    set_slot(&mut svm, 100);
    let vault_start = token_amount(&svm, &env.vault);

    // sole LP staker (residual cohort -> fee applies). Register at slot 100, then let real time elapse
    // (residual points are time-weighted: floor(log2(tenure)) * netΔ — sole-in-cohort so the weight cancels
    // in the claim ratio, but it must be > 0).
    let lp = Keypair::new();
    let pf = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        0,
        0,
    );
    register(&mut svm, &payer, &env, &lp, &lp.pubkey(), &pf, COHORT_LP).expect("reg lp");
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        9_000,
        0,
    );
    set_slot(&mut svm, 1_000); // tenure = 900 -> floor(log2) = 9 > 0
    crystallize(&mut svm, &payer, &env, &lp, &pf).expect("cry lp");
    // sole INSURANCE staker (capital cohort -> log-time weighted, but NO claim fee).
    let ins = Keypair::new();
    let ins_pos = Pubkey::new_unique();
    set_position(
        &mut svm,
        &ins_pos,
        &env.stub_sub,
        &env.ins_pool,
        &ins.pubkey(),
        500,
        false,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &ins,
        &ins.pubkey(),
        &ins_pos,
        COHORT_INSURANCE,
    )
    .expect("reg ins");
    set_slot(&mut svm, 1_124);
    crystallize(&mut svm, &payer, &env, &ins, &ins_pos).expect("cry ins");

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    let lp_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &lp.pubkey());
    claim(&mut svm, &payer, &env, &lp, &lp_ata, None).expect("lp claim");
    assert_eq!(
        token_amount(&svm, &lp_ata),
        320_000,
        "LP claims 80% of its 400_000 cohort — 20% anti-wash fee skimmed"
    );

    let ins_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &ins.pubkey());
    claim(&mut svm, &payer, &env, &ins, &ins_ata, Some(&ins_pos)).expect("ins claim");
    assert_eq!(
        token_amount(&svm, &ins_ata),
        100_000,
        "insurance (capital-at-risk) claims its FULL 100_000 — no fee"
    );

    // The 80_000 LP fee is retained in the vault (locked, deflationary), not paid to the farmer.
    let paid_out = token_amount(&svm, &lp_ata) + token_amount(&svm, &ins_ata);
    assert_eq!(
        vault_start - token_amount(&svm, &env.vault),
        paid_out,
        "vault drained only by what was paid out"
    );
    assert_eq!(
        token_amount(&svm, &env.vault),
        supply - 320_000 - 100_000,
        "the 80_000 fee + unclaimed cohorts stay locked in the vault"
    );
}

// LIVENESS/NO-DILUTION PROBE (registered-but-never-crystallized stake, sweep tick D): a backer can register and
// then never crystallize (forgot, or ran out of finalize window) — its stake.points stay 0. Two properties must
// hold: (1) its own claim pays 0 GRACEFULLY (points_to_amount guards total_points==0, lib.rs:97 — no div-by-zero
// brick), and (2) its 0 points do NOT enter the frozen denominator, so a co-cohort staker that DID crystallize
// still takes the full cohort (the idle stake neither strands supply nor dilutes the honest claimant). All claim
// tests above crystallize first, so the zero-points path was unexercised.
#[test]
fn a_registered_but_never_crystallized_stake_claims_zero_and_does_not_dilute_the_cohort() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64; // lp cohort = 40% = 400_000 ; no anti-wash fee (setup)
    let env = setup(&mut svm, &payer, supply);
    set_slot(&mut svm, 100);

    // Staker A: registers AND crystallizes a real residual (points > 0).
    let a = Keypair::new();
    let a_pf = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &a_pf,
        &env.stub_perc,
        &env.market,
        &a.pubkey(),
        0,
        0,
    );
    register(&mut svm, &payer, &env, &a, &a.pubkey(), &a_pf, COHORT_LP).expect("reg A");
    set_portfolio(
        &mut svm,
        &a_pf,
        &env.stub_perc,
        &env.market,
        &a.pubkey(),
        9_000,
        0,
    );
    set_slot(&mut svm, 1_000);
    crystallize(&mut svm, &payer, &env, &a, &a_pf).expect("cry A");

    // Staker B: registers in the SAME cohort but NEVER crystallizes — points stay 0.
    let b = Keypair::new();
    let b_pf = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &b_pf,
        &env.stub_perc,
        &env.market,
        &b.pubkey(),
        0,
        0,
    );
    register(&mut svm, &payer, &env, &b, &b.pubkey(), &b_pf, COHORT_LP)
        .expect("reg B (never crystallizes)");

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    // A takes the FULL lp cohort — B's 0 points never entered the frozen denominator (no dilution).
    let a_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &a.pubkey());
    claim(&mut svm, &payer, &env, &a, &a_ata, None).expect("A claim");
    assert_eq!(
        token_amount(&svm, &a_ata),
        400_000,
        "the sole crystallized LP staker takes the full cohort — the idle stake did not dilute"
    );

    // B's claim pays 0 gracefully — no panic, no div-by-zero, no brick of its own slot.
    let b_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &b.pubkey());
    claim(&mut svm, &payer, &env, &b, &b_ata, None).expect("B claim succeeds (pays 0)");
    assert_eq!(
        token_amount(&svm, &b_ata),
        0,
        "a registered-but-never-crystallized stake claims 0, gracefully"
    );
}

// FREE-FARM PROBE (finding NZ, sweep): the TRADER cohort is the PRIMARY delta-neutral wash surface, so the
// anti-wash fee MUST apply to it too (the LP test above only covers COHORT_LP). claim taxes both PnL-flow
// cohorts (matches!(cohort, COHORT_LP | COHORT_TRADER)); if the trader branch dodged the fee, a wash-farmer
// would route through the trader cohort fee-free. A sole trader staker with a 20% fee claims 80% of its
// cohort; the 20% is retained in the vault.
#[test]
fn trader_cohort_claim_also_pays_the_anti_wash_fee() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64; // trader = remainder 40% = 400_000
    let env = setup_with_fee(&mut svm, &payer, supply, 2_000); // 20% anti-wash fee
    set_slot(&mut svm, 100);

    let t = Keypair::new();
    let pf = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &t.pubkey(),
        0,
        0,
    );
    register(&mut svm, &payer, &env, &t, &t.pubkey(), &pf, COHORT_TRADER).expect("reg trader");
    // crystallized loss = 9_000 (received/spent 0 -> trader counter = crystallized - spent = 9_000).
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &t.pubkey(),
        0,
        9_000,
    );
    set_slot(&mut svm, 1_000); // tenure > 0 for the time-weight
    crystallize(&mut svm, &payer, &env, &t, &pf).expect("cry trader");
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    let ata = create_token_account(&mut svm, &payer, &env.coin_mint, &t.pubkey());
    claim(&mut svm, &payer, &env, &t, &ata, None).expect("trader claim");
    assert_eq!(token_amount(&svm, &ata), 320_000, "trader claims 80% of its 400_000 cohort — the 20% anti-wash fee IS skimmed (no fee-free trader farm)");
    assert_eq!(
        token_amount(&svm, &env.vault),
        supply - 320_000,
        "the 80_000 trader fee is retained in the vault"
    );
}

// ANTI-WASH FEE AT THE 100% EXTREME (claim-layer graceful degradation): init accepts fee_support_bps == 10_000
// (the inclusive boundary, pinned at init by init_rejects_an_anti_wash_fee_above_100pct...). This pins the RUNTIME
// counterpart the init test never exercises: a real crystallize->freeze->CLAIM at 100% must degrade gracefully —
// payout = amount - fee = amount - amount = 0, the `if payout > 0` guard SKIPS the transfer (no 0-amount transfer,
// no underflow, no panic), the whole cohort payout is RETAINED in the vault (intentionally deflationary), and the
// stake is STILL marked claimed so a re-claim cannot retry. A future change to the claim fee math (dropping the
// guard, reordering amount-fee) would brick every LP/trader claim at high fees — an init-only test wouldn't catch it.
#[test]
fn trader_claim_at_a_100pct_anti_wash_fee_pays_zero_gracefully_and_still_consumes_the_stake() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64; // trader cohort = remainder 40% = 400_000
    let env = setup_with_fee(&mut svm, &payer, supply, 10_000); // 100% anti-wash fee — the inclusive max
    set_slot(&mut svm, 100);

    let t = Keypair::new();
    let pf = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &t.pubkey(),
        0,
        0,
    );
    register(&mut svm, &payer, &env, &t, &t.pubkey(), &pf, COHORT_TRADER).expect("reg trader");
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &t.pubkey(),
        0,
        9_000,
    ); // crystallized loss
    set_slot(&mut svm, 1_000);
    crystallize(&mut svm, &payer, &env, &t, &pf).expect("cry trader");
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    let ata = create_token_account(&mut svm, &payer, &env.coin_mint, &t.pubkey());
    // The claim SUCCEEDS (no panic / no underflow) even though the entire payout is skimmed.
    claim(&mut svm, &payer, &env, &t, &ata, None)
        .expect("claim at 100% fee must succeed gracefully, not revert");
    assert_eq!(
        token_amount(&svm, &ata),
        0,
        "100% anti-wash fee -> the trader receives 0 (whole payout skimmed)"
    );
    assert_eq!(
        token_amount(&svm, &env.vault),
        supply,
        "the full payout is retained in the vault (deflationary) — nothing left it"
    );
    // The stake is consumed: a re-claim cannot retry to drain the vault even though the first claim paid 0.
    assert!(
        claim(&mut svm, &payer, &env, &t, &ata, None).is_err(),
        "a zero-payout claim still consumes the stake — no re-claim retry"
    );
    assert_eq!(
        token_amount(&svm, &env.vault),
        supply,
        "vault still intact after the rejected re-claim"
    );
}

// PERMISSIONLESS CRYSTALLIZE (LP/trader, sweep tick D): share_value_crystallize_cannot_be_forced_by_a_third_party
// pins that capital (insurance/backing) crystallize is OWNER-GATED (finding KO) — a forced crystallize at a
// transient low-principal moment would grief. The COMPLEMENT, untested: LP/trader crystallize is PERMISSIONLESS — any
// cranker may finalize a staker's points, because the percolator residual counters are MONOTONIC, so a forced
// crystallize can only RAISE the netΔ, never grief. Pin that a third-party cranker successfully crystallizes a
// trader stake and the points are recorded (the owner then claims its full cohort).
#[test]
fn lp_trader_crystallize_is_permissionless_any_cranker_finalizes_a_stakers_points() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64; // trader cohort = 40% = 400_000 ; no anti-wash fee (setup)
    let env = setup(&mut svm, &payer, supply);
    set_slot(&mut svm, 100);

    let t = Keypair::new();
    let pf = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &t.pubkey(),
        0,
        0,
    );
    register(&mut svm, &payer, &env, &t, &t.pubkey(), &pf, COHORT_TRADER).expect("reg trader");
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &t.pubkey(),
        0,
        9_000,
    );
    set_slot(&mut svm, 1_000);

    // A THIRD PARTY (not the stake owner) crystallizes the trader stake — permissionless (monotonic-safe).
    let cranker = Keypair::new();
    crystallize_as(&mut svm, &payer, &env, &cranker, &t.pubkey(), &pf)
        .expect("LP/trader crystallize is permissionless — any cranker may finalize");

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");
    // The third-party crystallize recorded the points, so the sole trader staker claims its full cohort.
    let ata = create_token_account(&mut svm, &payer, &env.coin_mint, &t.pubkey());
    claim(&mut svm, &payer, &env, &t, &ata, None).expect("owner claim");
    assert_eq!(
        token_amount(&svm, &ata),
        400_000,
        "the third-party crystallize finalized the points; the sole trader claims its full cohort"
    );
}

// FREE-FARM PROBE (time-weight semantics, sweep): the log2(tenure) weight keys off `start_slot`, set at
// REGISTER (lib.rs:725) — NOT off when the residual was actually created. So two stakers with the IDENTICAL
// net residual but different registration times earn different points: an early registrant out-captures a late
// one. A farmer can pre-register a residual-EMPTY stake cheaply (just a percolator portfolio, no capital/loss),
// accrue tenure for free, then manufacture the loss late and still bank the full-tenure multiplier. This pins
// that behavior so it is not misread as a hard "position held open the whole time" lock.
//
// VERDICT: ACCEPTED LIMITATION, not a LOF/DoS and not free COIN. The multiplier only shifts RELATIVE share
// toward early committers (parity with the genesis-vote early-deposit weight); the EARNING — the net residual R
// itself — still costs real capital-at-risk + the 3bps per-trade fee + the anti-wash claim fee, all of which
// scale with farm size (Sybil-flat). Tying tenure to residual-age instead would need a per-increment ledger the
// design deliberately avoids, and ANY single anchor (register OR first-crystallize) is bypassable with a cheap
// early dust loss — so there is no clean on-chain fix; the manufacturing cost is the real bound.
#[test]
fn time_weight_rewards_registration_tenure_not_residual_age_early_over_captures() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64; // trader cohort = remainder 40% = 400_000
    let env = setup(&mut svm, &payer, supply); // no fee -> isolate the time-weight

    let r = 9_000u128; // IDENTICAL net residual for both stakers
                       // EARLY: register at slot 100 (residual-empty), manufacture R only at slot 10_000 -> tenure 9_900, log2=13.
    let early = Keypair::new();
    let early_pf = Pubkey::new_unique();
    set_slot(&mut svm, 100);
    set_portfolio(
        &mut svm,
        &early_pf,
        &env.stub_perc,
        &env.market,
        &early.pubkey(),
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &early,
        &early.pubkey(),
        &early_pf,
        COHORT_TRADER,
    )
    .expect("reg early");
    // LATE: register at slot 9_000 -> crystallize at 10_000 gives tenure 1_000, log2=9 (same R).
    let late = Keypair::new();
    let late_pf = Pubkey::new_unique();
    set_slot(&mut svm, 9_000);
    set_portfolio(
        &mut svm,
        &late_pf,
        &env.stub_perc,
        &env.market,
        &late.pubkey(),
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &late,
        &late.pubkey(),
        &late_pf,
        COHORT_TRADER,
    )
    .expect("reg late");

    // Both manufacture the SAME loss at the SAME slot, then crystallize together.
    set_slot(&mut svm, 10_000);
    set_portfolio(
        &mut svm,
        &early_pf,
        &env.stub_perc,
        &env.market,
        &early.pubkey(),
        0,
        r,
    );
    set_portfolio(
        &mut svm,
        &late_pf,
        &env.stub_perc,
        &env.market,
        &late.pubkey(),
        0,
        r,
    );
    crystallize(&mut svm, &payer, &env, &early, &early_pf).expect("cry early");
    crystallize(&mut svm, &payer, &env, &late, &late_pf).expect("cry late");
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    let early_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &early.pubkey());
    let late_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &late.pubkey());
    claim(&mut svm, &payer, &env, &early, &early_ata, None).expect("early claim");
    claim(&mut svm, &payer, &env, &late, &late_ata, None).expect("late claim");

    // points: early = 13*9_000 = 117_000, late = 9*9_000 = 81_000; denom = 198_000; cohort = 400_000.
    let early_paid = token_amount(&svm, &early_ata);
    let late_paid = token_amount(&svm, &late_ata);
    assert_eq!(
        early_paid,
        400_000u64 * 117_000 / 198_000,
        "early registrant captures the log2(9_900)=13 multiplier"
    );
    assert_eq!(
        late_paid,
        400_000u64 * 81_000 / 198_000,
        "late registrant only gets log2(1_000)=9 on the SAME residual"
    );
    // The pin: SAME residual, different registration -> early over-captures (~59% vs ~41%). The weight rewards
    // stake-tenure, not how long the loss was held; the bound is the cost to manufacture R, not the multiplier.
    assert!(
        early_paid > late_paid,
        "pre-registration over-captures vs a late registrant with identical residual"
    );
    // Conserved: the two split the whole cohort up to 1 atom of independent-floor rounding dust (stays locked).
    assert_eq!(
        early_paid + late_paid,
        399_999,
        "cohort fully shared between the two minus 1-atom floor dust"
    );
    assert!(
        400_000 - (early_paid + late_paid) <= 1,
        "at most 1 atom of rounding dust stays locked in the vault"
    );
}

// TIME-WEIGHT FLOOR (floor_log2(tenure) boundary): points = floor_log2(now - start_slot) * netΔ, and
// floor_log2(n) = 0 for n < 2 (lib.rs). So a stake CRYSTALLIZED at tenure 1 earns ZERO points despite a real
// residual — the first positive weight requires tenure >= 2 (parity with genesis-vote's age-2 vote-weight floor,
// pinned there; the rd's floor_log2 is a SEPARATE impl, so pin its boundary too). A tenure-1 stake does not dilute
// the cohort (0 points), and a tenure-2 co-staker takes the whole cohort. This also closes a JIT-capture angle:
// registering + crystallizing in the same/adjacent slot earns nothing.
#[test]
fn time_weight_floor_tenure_below_2_crystallizes_zero_points_first_positive_at_2() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64; // lp cohort = 40% = 400_000
    let env = setup(&mut svm, &payer, supply);
    set_slot(&mut svm, 100);

    // Both register at slot 100 (snap 0), same cohort, IDENTICAL residual.
    let (a, a_pf) = (Keypair::new(), Pubkey::new_unique());
    let (b, b_pf) = (Keypair::new(), Pubkey::new_unique());
    set_portfolio(
        &mut svm,
        &a_pf,
        &env.stub_perc,
        &env.market,
        &a.pubkey(),
        0,
        0,
    );
    set_portfolio(
        &mut svm,
        &b_pf,
        &env.stub_perc,
        &env.market,
        &b.pubkey(),
        0,
        0,
    );
    register(&mut svm, &payer, &env, &a, &a.pubkey(), &a_pf, COHORT_LP).expect("reg A");
    register(&mut svm, &payer, &env, &b, &b.pubkey(), &b_pf, COHORT_LP).expect("reg B");

    // A crystallizes at tenure 1 (slot 101) -> floor_log2(1) = 0 -> 0 points.
    set_portfolio(
        &mut svm,
        &a_pf,
        &env.stub_perc,
        &env.market,
        &a.pubkey(),
        9_000,
        0,
    );
    set_slot(&mut svm, 101);
    crystallize(&mut svm, &payer, &env, &a, &a_pf).expect("cry A at tenure 1");
    // B crystallizes at tenure 2 (slot 102) -> floor_log2(2) = 1 -> 9_000 points.
    set_portfolio(
        &mut svm,
        &b_pf,
        &env.stub_perc,
        &env.market,
        &b.pubkey(),
        9_000,
        0,
    );
    set_slot(&mut svm, 102);
    crystallize(&mut svm, &payer, &env, &b, &b_pf).expect("cry B at tenure 2");

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    let a_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &a.pubkey());
    let b_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &b.pubkey());
    claim(&mut svm, &payer, &env, &a, &a_ata, None).expect("A claim (succeeds, pays 0)");
    claim(&mut svm, &payer, &env, &b, &b_ata, None).expect("B claim");
    assert_eq!(
        token_amount(&svm, &a_ata),
        0,
        "tenure-1 stake earns 0 points (floor_log2(1)=0) -> claims nothing"
    );
    assert_eq!(token_amount(&svm, &b_ata), 400_000, "tenure-2 stake takes the WHOLE LP cohort — the tenure-1 stake did not dilute the denominator");
}

// FREE-FARM PROBE (net-by-spent asymmetry: churn defeats trader, only the fee bounds LP, sweep tick D): the
// TRADER counter is the NET drain `crystallized - spent` (residual_counter), so a farmer who CHURNS — recycles
// capital by closing+reopening, which spends their own crystallized budget — drives spent up to crystallized and
// nets their trader points to ZERO. But the LP counter is raw `received`, which has NO symmetric self-recovery
// term to net against, so the SAME churn leaves LP points untouched. This pins that asymmetry end-to-end against
// the real rd .so: a fully-churned position (spent == crystallized) is worth 0 in the trader cohort but FULL
// (minus only the anti-wash claim fee) in the LP cohort.
// VERDICT: ACCEPTED / BY DESIGN. The trader cohort gets two bounds (net-by-spent + the fee); the LP cohort gets
// one (the claim fee) because `received` reflects realized counterparty flow with no self-cancelling leg. So the
// claim fee is LP's SOLE on-chain bound (plus the per-trade fee, the time-weight, and cohort dilution off-chain).
// This is why the fee is mandatory and why it is NOT redundant with the spent-netting (which protects only the
// trader half). [[residual-cohort-pyth-allowlist]]
#[test]
fn churn_zeroes_a_trader_via_spent_netting_but_lp_received_is_bounded_only_by_the_claim_fee() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64; // trader = 40% = 400_000 ; lp = 40% = 400_000
    let env = setup_with_fee(&mut svm, &payer, supply, 2_000); // 20% anti-wash fee
    set_slot(&mut svm, 100);

    // A TRADER and an LP staker, each on a fresh empty portfolio (register-time snap = 0).
    let t = Keypair::new();
    let t_pf = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &t_pf,
        &env.stub_perc,
        &env.market,
        &t.pubkey(),
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &t,
        &t.pubkey(),
        &t_pf,
        COHORT_TRADER,
    )
    .expect("reg trader");
    let l = Keypair::new();
    let l_pf = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &l_pf,
        &env.stub_perc,
        &env.market,
        &l.pubkey(),
        0,
        0,
    );
    register(&mut svm, &payer, &env, &l, &l.pubkey(), &l_pf, COHORT_LP).expect("reg lp");

    // The IDENTICAL fully-churned wash counters on both: crystallized 10_000 FULLY spent (net 0), received 10_000.
    set_portfolio_full(
        &mut svm,
        &t_pf,
        &env.stub_perc,
        &env.market,
        &t.pubkey(),
        10_000,
        10_000,
        10_000,
    );
    set_portfolio_full(
        &mut svm,
        &l_pf,
        &env.stub_perc,
        &env.market,
        &l.pubkey(),
        10_000,
        10_000,
        10_000,
    );
    set_slot(&mut svm, 1_000); // tenure > 0
    crystallize(&mut svm, &payer, &env, &t, &t_pf).expect("cry trader");
    crystallize(&mut svm, &payer, &env, &l, &l_pf).expect("cry lp");
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    // TRADER: counter = crystallized - spent = 0 -> 0 points -> claims NOTHING. Churn defeated by net-by-spent.
    let t_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &t.pubkey());
    claim(&mut svm, &payer, &env, &t, &t_ata, None).expect("trader claim (zero)");
    assert_eq!(
        token_amount(&svm, &t_ata),
        0,
        "a fully-churned trader nets to 0 — spent-netting kills trader churn"
    );

    // LP: counter = received = 10_000 (spent is irrelevant to it) -> full points -> claims the WHOLE lp cohort
    // minus only the 20% anti-wash fee. The SAME churn that zeroed the trader does NOTHING to the LP cohort.
    let l_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &l.pubkey());
    claim(&mut svm, &payer, &env, &l, &l_ata, None).expect("lp claim (full minus fee)");
    assert_eq!(token_amount(&svm, &l_ata), 320_000, "the SAME churn leaves LP at full 80% of its cohort — the claim fee is LP's only on-chain bound");
}

// DoS PROBE (claim-underflow via an out-of-range anti-wash fee, sweep): claim pays `payout = amount - fee`
// with `fee = amount * fee_support_bps / 10000`. If fee_support_bps could exceed 10000, fee > amount and the
// u64 subtraction underflows -> every LP/trader claim reverts forever (a permanent fund-FREEZE on those
// cohorts). init guards `residual_fee_bps > BPS_DENOMINATOR -> reject` (lib.rs:532). This pins it: a fee bps
// over 100% is rejected at init; exactly 100% is the inclusive boundary (all skimmed, payout 0, no underflow).
#[test]
fn init_rejects_an_anti_wash_fee_above_100pct_no_claim_underflow_dos() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    // Build an rd init with a CUSTOM trailing fee_bps. Fresh coin_mint each call -> distinct rd_config.
    let try_fee = |svm: &mut LiteSVM, fee_bps: u16| -> Result<(), String> {
        let mint_auth = Keypair::new();
        let coin_mint = create_mint(svm, &payer, &mint_auth.pubkey());
        let rd_config =
            Pubkey::find_program_address(&[b"rd_config", coin_mint.as_ref()], &rd_id()).0;
        let dist_config = dist_config_pda(&coin_mint, &rd_config);
        let stub_perc = Pubkey::new_unique();
        let stub_sub = Pubkey::new_unique();
        let d = rd_init_data(
            1_000_000,
            2_000,
            1_000,
            1_000,
            4_000,
            500,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            &[],
            Some(fee_bps),
        );
        send(
            svm,
            &payer,
            &[Instruction {
                program_id: rd_id(),
                accounts: rd_init_accounts(
                    payer.pubkey(),
                    coin_mint,
                    dist_config,
                    stub_perc,
                    stub_sub,
                    rd_config,
                    mint_auth.pubkey(),
                ),
                data: d,
            }],
            &[&mint_auth],
        )
    };
    // fee > 100% -> rejected (else `payout = amount - fee` underflows -> permanent claim revert = fund freeze).
    assert!(
        try_fee(&mut svm, 10_001).is_err(),
        "anti-wash fee > 100% must be rejected (claim-underflow DoS)"
    );
    assert!(
        try_fee(&mut svm, u16::MAX).is_err(),
        "a max-u16 fee is rejected"
    );
    // fee == 100% -> accepted boundary (everything skimmed, payout 0, no underflow); fee 0 -> accepted.
    try_fee(&mut svm, 10_000).expect("exactly 100% is the inclusive boundary");
    try_fee(&mut svm, 0).expect("0% (no fee) accepted");
}

fn stake_pda(env: &Env, owner: &Pubkey, linked: &Pubkey) -> Pubkey {
    stake_pda_for_cohort(env, owner, linked, COHORT_LP)
}

fn stake_family(cohort: u8) -> u8 {
    if cohort == COHORT_TRADER {
        COHORT_LP
    } else {
        cohort
    }
}

fn stake_pda_for_cohort(env: &Env, owner: &Pubkey, linked: &Pubkey, cohort: u8) -> Pubkey {
    let family = [stake_family(cohort)];
    Pubkey::find_program_address(
        &[
            b"rd_stake",
            env.rd_config.as_ref(),
            owner.as_ref(),
            linked.as_ref(),
            &family,
        ],
        &rd_id(),
    )
    .0
}

fn portfolio_market(svm: &LiteSVM, env: &Env, portfolio: &Pubkey) -> Pubkey {
    svm.get_account(portfolio)
        .and_then(|account| {
            account
                .data
                .get(16..48)
                .map(|bytes| Pubkey::new_from_array(bytes.try_into().unwrap()))
        })
        .filter(|market| *market != Pubkey::default())
        .unwrap_or(env.market)
}

fn ensure_mock_market(svm: &mut LiteSVM, percolator: &Pubkey, market: &Pubkey) {
    if svm.get_account(market).is_some() {
        return;
    }
    let mut data = vec![0u8; 16 + 448];
    data[..8].copy_from_slice(&0x5045_5243_5631_3600u64.to_le_bytes());
    data[8..10].copy_from_slice(&16u16.to_le_bytes());
    data[10] = 1;
    svm.set_account(
        *market,
        Account {
            lamports: 1_000_000_000,
            data,
            owner: *percolator,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

fn portfolio_archive_pda(
    svm: &LiteSVM,
    env: &Env,
    owner: &Pubkey,
    portfolio: &Pubkey,
) -> Pubkey {
    let market = portfolio_market(svm, env, portfolio);
    Pubkey::find_program_address(
        &[
            b"rd_portfolio_archive",
            env.stub_perc.as_ref(),
            market.as_ref(),
            owner.as_ref(),
            portfolio.as_ref(),
        ],
        &rd_id(),
    )
    .0
}

fn retired_market_pda(percolator_program: &Pubkey, market: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"retired-market",
            percolator_program.as_ref(),
            market.as_ref(),
        ],
        &solana_sdk::pubkey!("3ueoyr1JepT2DvPxh8LrhdJZ6YsL2sT9Sm7y3TfNyfi9"),
    )
    .0
}

fn stakes_for_link(svm: &LiteSVM, env: &Env, owner: &Pubkey, linked: &Pubkey) -> Vec<Pubkey> {
    [
        COHORT_INSURANCE,
        COHORT_BACKING,
        COHORT_LP,
        COHORT_FUNDING_PAYER,
    ]
    .into_iter()
    .map(|cohort| stake_pda_for_cohort(env, owner, linked, cohort))
    .filter(|stake| svm.get_account(stake).is_some())
    .collect()
}

fn remember_registered_link(env: &Env, owner: &Pubkey, linked: &Pubkey) {
    REGISTERED_LINKS.with(|links| {
        let mut links = links.borrow_mut();
        let entry = links.entry((env.rd_config, *owner)).or_default();
        if !entry.contains(linked) {
            entry.push(*linked);
        }
    });
}

fn registered_stake_for_claim(
    svm: &LiteSVM,
    env: &Env,
    owner: &Pubkey,
    requested: Option<&Pubkey>,
) -> (Pubkey, Pubkey) {
    if let Some(linked) = requested {
        let candidates = stakes_for_link(svm, env, owner, linked);
        match candidates.as_slice() {
            [stake] => return (*stake, *linked),
            [] => {}
            _ => panic!("ambiguous reward family; pass the cohort explicitly"),
        }
    }
    REGISTERED_LINKS.with(|links| {
        let links = links.borrow();
        let candidates = links
            .get(&(env.rd_config, *owner))
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .flat_map(|linked| {
                stakes_for_link(svm, env, owner, &linked)
                    .into_iter()
                    .map(move |stake| (stake, linked))
            })
            .collect::<Vec<_>>();
        match candidates.as_slice() {
            [(stake, linked)] => (*stake, *linked),
            [] => panic!("no registered stake for owner"),
            _ => panic!("ambiguous registered stake; pass the cohort explicitly"),
        }
    })
}

fn register(
    svm: &mut LiteSVM,
    payer: &Keypair,
    env: &Env,
    owner: &Keypair,
    recipient: &Pubkey,
    linked: &Pubkey,
    cohort: u8,
) -> Result<(), String> {
    let stake = stake_pda_for_cohort(env, &owner.pubkey(), linked, cohort);
    let mut accounts = vec![
        AccountMeta::new(payer.pubkey(), true),
        AccountMeta::new_readonly(env.rd_config, false),
        AccountMeta::new_readonly(owner.pubkey(), true),
        AccountMeta::new_readonly(*recipient, false),
        AccountMeta::new_readonly(*linked, false),
        AccountMeta::new(stake, false),
        AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
    ];
    if matches!(cohort, COHORT_LP | COHORT_TRADER | COHORT_FUNDING_PAYER) {
        let market = portfolio_market(svm, env, linked);
        ensure_mock_market(svm, &env.stub_perc, &market);
        accounts.push(AccountMeta::new_readonly(
            portfolio_archive_pda(svm, env, &owner.pubkey(), linked),
            false,
        ));
        accounts.push(AccountMeta::new_readonly(market, false));
        accounts.push(AccountMeta::new_readonly(
            retired_market_pda(&env.stub_perc, &market),
            false,
        ));
    }
    if svm.get_account(&env.rd_config).unwrap().data.len() == 823 {
        let legacy_owner_stake = Pubkey::find_program_address(
            &[b"rd_stake", env.rd_config.as_ref(), owner.pubkey().as_ref()],
            &rd_id(),
        )
        .0;
        let legacy_linked_stake = Pubkey::find_program_address(
            &[
                b"rd_stake",
                env.rd_config.as_ref(),
                owner.pubkey().as_ref(),
                linked.as_ref(),
            ],
            &rd_id(),
        )
        .0;
        accounts.push(AccountMeta::new_readonly(legacy_owner_stake, false));
        accounts.push(AccountMeta::new_readonly(legacy_linked_stake, false));
    }
    let result = send(
        svm,
        payer,
        &[Instruction {
            program_id: rd_id(),
            accounts,
            data: vec![1u8, cohort],
        }],
        &[owner],
    );
    if result.is_ok() {
        remember_registered_link(env, &owner.pubkey(), linked);
    }
    result
}
// `cranker` triggers crystallize (first account, must sign). Share-value cohorts (insurance/backing)
// require it to be the stake owner (finding KO); LP/trader accept any cranker.
fn crystallize_as(
    svm: &mut LiteSVM,
    payer: &Keypair,
    env: &Env,
    cranker: &Keypair,
    owner: &Pubkey,
    linked: &Pubkey,
) -> Result<(), String> {
    let candidates = stakes_for_link(svm, env, owner, linked);
    let stake = match candidates.as_slice() {
        [stake] => *stake,
        [] => registered_stake_for_claim(svm, env, owner, None).0,
        _ => panic!("ambiguous reward family; pass the cohort explicitly"),
    };
    let cohort = svm.get_account(&stake).unwrap().data[193];
    let mut accounts = vec![
        AccountMeta::new(cranker.pubkey(), true),
        AccountMeta::new(env.rd_config, false),
        AccountMeta::new(stake, false),
        AccountMeta::new_readonly(*linked, false),
    ];
    if matches!(cohort, COHORT_LP | COHORT_TRADER | COHORT_FUNDING_PAYER) {
        let market = portfolio_market(svm, env, linked);
        accounts.push(AccountMeta::new_readonly(
            portfolio_archive_pda(svm, env, owner, linked),
            false,
        ));
        accounts.push(AccountMeta::new_readonly(
            retired_market_pda(&env.stub_perc, &market),
            false,
        ));
    }
    send(
        svm,
        payer,
        &[Instruction {
            program_id: rd_id(),
            accounts,
            data: vec![2u8],
        }],
        &[cranker],
    )
}
// Default crystallize: the owner authorizes their own (valid for every cohort).
fn crystallize(
    svm: &mut LiteSVM,
    payer: &Keypair,
    env: &Env,
    owner: &Keypair,
    linked: &Pubkey,
) -> Result<(), String> {
    crystallize_as(svm, payer, env, owner, &owner.pubkey(), linked)
}
fn crystallize_cohort(
    svm: &mut LiteSVM,
    payer: &Keypair,
    env: &Env,
    cranker: &Keypair,
    owner: &Pubkey,
    linked: &Pubkey,
    cohort: u8,
) -> Result<(), String> {
    let stake = stake_pda_for_cohort(env, owner, linked, cohort);
    let mut accounts = vec![
        AccountMeta::new(cranker.pubkey(), true),
        AccountMeta::new(env.rd_config, false),
        AccountMeta::new(stake, false),
        AccountMeta::new_readonly(*linked, false),
    ];
    if matches!(cohort, COHORT_LP | COHORT_TRADER | COHORT_FUNDING_PAYER) {
        let market = portfolio_market(svm, env, linked);
        accounts.push(AccountMeta::new_readonly(
            portfolio_archive_pda(svm, env, owner, linked),
            false,
        ));
        accounts.push(AccountMeta::new_readonly(
            retired_market_pda(&env.stub_perc, &market),
            false,
        ));
    }
    send(
        svm,
        payer,
        &[Instruction {
            program_id: rd_id(),
            accounts,
            data: vec![2u8],
        }],
        &[cranker],
    )
}
fn freeze(svm: &mut LiteSVM, payer: &Keypair, env: &Env) -> Result<(), String> {
    send(
        svm,
        payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new(env.rd_config, false),
                AccountMeta::new_readonly(env.coin_mint, false),
                AccountMeta::new(env.vault, false),
            ],
            data: vec![4u8],
        }],
        &[],
    )
}
// claim: insurance/backing append the live subledger position; LP/trader append the live portfolio and
// cumulative archive. Funding-payer claims are frozen-counter only and require no trailing account.
// `cranker` is the claim trigger (first account, must sign). Share-value cohorts (insurance/backing)
// require it to be the stake owner (finding KM); portfolio-flow cohorts accept any cranker while their live
// witness exists, and require the owner only for terminal dematerialized-witness recovery. The helper takes the
// cranker keypair explicitly so tests can model both the owner's own claim and a foreign forced claim.
fn claim_as(
    svm: &mut LiteSVM,
    payer: &Keypair,
    env: &Env,
    cranker: &Keypair,
    owner: &Pubkey,
    recipient_ata: &Pubkey,
    position: Option<&Pubkey>,
) -> Result<(), String> {
    let (stake, stake_linked) = registered_stake_for_claim(svm, env, owner, position);
    let claim_linked = position.copied().unwrap_or(stake_linked);
    let mut accounts = vec![
        AccountMeta::new(cranker.pubkey(), true),
        AccountMeta::new_readonly(env.rd_config, false),
        AccountMeta::new(stake, false),
        AccountMeta::new(env.vault, false),
        AccountMeta::new(*recipient_ata, false),
        AccountMeta::new_readonly(spl_token::ID, false),
    ];
    let cohort = svm.get_account(&stake).unwrap().data[193];
    match cohort {
        COHORT_INSURANCE | COHORT_BACKING => {
            accounts.push(AccountMeta::new_readonly(claim_linked, false));
        }
        COHORT_LP | COHORT_TRADER => {
            let market = portfolio_market(svm, env, &claim_linked);
            accounts.push(AccountMeta::new_readonly(claim_linked, false));
            accounts.push(AccountMeta::new_readonly(
                portfolio_archive_pda(svm, env, owner, &claim_linked),
                false,
            ));
            accounts.push(AccountMeta::new_readonly(
                retired_market_pda(&env.stub_perc, &market),
                false,
            ));
        }
        _ => {}
    }
    send(
        svm,
        payer,
        &[Instruction {
            program_id: rd_id(),
            accounts,
            data: vec![5u8],
        }],
        &[cranker],
    )
}
// Default claim: the owner authorizes their own claim (valid for every cohort).
fn claim(
    svm: &mut LiteSVM,
    payer: &Keypair,
    env: &Env,
    owner: &Keypair,
    recipient_ata: &Pubkey,
    position: Option<&Pubkey>,
) -> Result<(), String> {
    claim_as(
        svm,
        payer,
        env,
        owner,
        &owner.pubkey(),
        recipient_ata,
        position,
    )
}
fn claim_without_linked(
    svm: &mut LiteSVM,
    payer: &Keypair,
    env: &Env,
    owner: &Keypair,
    recipient_ata: &Pubkey,
) -> Result<(), String> {
    let (stake, _) = registered_stake_for_claim(svm, env, &owner.pubkey(), None);
    let accounts = vec![
        AccountMeta::new(owner.pubkey(), true),
        AccountMeta::new_readonly(env.rd_config, false),
        AccountMeta::new(stake, false),
        AccountMeta::new(env.vault, false),
        AccountMeta::new(*recipient_ata, false),
        AccountMeta::new_readonly(spl_token::ID, false),
    ];
    send(
        svm,
        payer,
        &[Instruction {
            program_id: rd_id(),
            accounts,
            data: vec![5u8],
        }],
        &[owner],
    )
}

fn claim_cohort(
    svm: &mut LiteSVM,
    payer: &Keypair,
    env: &Env,
    owner: &Keypair,
    recipient_ata: &Pubkey,
    linked: &Pubkey,
    cohort: u8,
) -> Result<(), String> {
    let stake = stake_pda_for_cohort(env, &owner.pubkey(), linked, cohort);
    let mut accounts = vec![
        AccountMeta::new(owner.pubkey(), true),
        AccountMeta::new_readonly(env.rd_config, false),
        AccountMeta::new(stake, false),
        AccountMeta::new(env.vault, false),
        AccountMeta::new(*recipient_ata, false),
        AccountMeta::new_readonly(spl_token::ID, false),
    ];
    match cohort {
        COHORT_INSURANCE | COHORT_BACKING => {
            accounts.push(AccountMeta::new_readonly(*linked, false));
        }
        COHORT_LP | COHORT_TRADER => {
            let market = portfolio_market(svm, env, linked);
            accounts.push(AccountMeta::new_readonly(*linked, false));
            accounts.push(AccountMeta::new_readonly(
                portfolio_archive_pda(svm, env, &owner.pubkey(), linked),
                false,
            ));
            accounts.push(AccountMeta::new_readonly(
                retired_market_pda(&env.stub_perc, &market),
                false,
            ));
        }
        _ => {}
    }
    send(
        svm,
        payer,
        &[Instruction {
            program_id: rd_id(),
            accounts,
            data: vec![5u8],
        }],
        &[owner],
    )
}

#[test]
fn funding_payer_points_go_to_paid_side_not_received_side() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64;
    let env = setup_funding_payer_only(&mut svm, &payer, supply);
    set_slot(&mut svm, 100);

    let long_payer = Keypair::new();
    let long_receiver = Keypair::new();
    let short_payer = Keypair::new();
    let short_receiver = Keypair::new();
    let lp_pf = Pubkey::new_unique();
    let lr_pf = Pubkey::new_unique();
    let sp_pf = Pubkey::new_unique();
    let sr_pf = Pubkey::new_unique();

    for (owner, pf, cohort) in [
        (&long_payer, &lp_pf, COHORT_FUNDING_PAYER),
        (&long_receiver, &lr_pf, COHORT_FUNDING_PAYER),
        (&short_payer, &sp_pf, COHORT_FUNDING_PAYER),
        (&short_receiver, &sr_pf, COHORT_FUNDING_PAYER),
    ] {
        set_portfolio_funding(
            &mut svm,
            pf,
            &env.stub_perc,
            &env.market,
            &owner.pubkey(),
            0,
            0,
            0,
            0,
        );
        register(&mut svm, &payer, &env, owner, &owner.pubkey(), pf, cohort)
            .expect("register funding stake");
    }

    set_slot(&mut svm, 1_000);
    // Long side: receiver-only activity must NOT earn funding-payer points; paid activity earns.
    set_portfolio_funding(
        &mut svm,
        &lp_pf,
        &env.stub_perc,
        &env.market,
        &long_payer.pubkey(),
        10_000,
        50_000,
        0,
        0,
    );
    set_portfolio_funding(
        &mut svm,
        &lr_pf,
        &env.stub_perc,
        &env.market,
        &long_receiver.pubkey(),
        0,
        50_000,
        0,
        0,
    );
    // Short side: receiver-only activity must NOT earn funding-payer points; paid activity earns.
    set_portfolio_funding(
        &mut svm,
        &sp_pf,
        &env.stub_perc,
        &env.market,
        &short_payer.pubkey(),
        0,
        0,
        10_000,
        40_000,
    );
    set_portfolio_funding(
        &mut svm,
        &sr_pf,
        &env.stub_perc,
        &env.market,
        &short_receiver.pubkey(),
        0,
        0,
        0,
        40_000,
    );

    crystallize(&mut svm, &payer, &env, &long_payer, &lp_pf).expect("crystallize long payer");
    crystallize(&mut svm, &payer, &env, &long_receiver, &lr_pf).expect("crystallize long receiver");
    crystallize(&mut svm, &payer, &env, &short_payer, &sp_pf).expect("crystallize short payer");
    crystallize(&mut svm, &payer, &env, &short_receiver, &sr_pf)
        .expect("crystallize short receiver");

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    let lp_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &long_payer.pubkey());
    let lr_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &long_receiver.pubkey());
    let sp_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &short_payer.pubkey());
    let sr_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &short_receiver.pubkey());
    claim(&mut svm, &payer, &env, &long_payer, &lp_ata, None).expect("long payer claim");
    claim(&mut svm, &payer, &env, &long_receiver, &lr_ata, None).expect("long receiver claim");
    claim(&mut svm, &payer, &env, &short_payer, &sp_ata, None).expect("short payer claim");
    claim(&mut svm, &payer, &env, &short_receiver, &sr_ata, None).expect("short receiver claim");

    assert_eq!(
        token_amount(&svm, &lp_ata),
        500_000,
        "funding-payer cohort rewards funding_long_paid, not funding_long_received"
    );
    assert_eq!(
        token_amount(&svm, &lr_ata),
        0,
        "long receiver-only account gets no payer points"
    );
    assert_eq!(
        token_amount(&svm, &sp_ata),
        500_000,
        "funding-payer cohort rewards funding_short_paid, not funding_short_received"
    );
    assert_eq!(
        token_amount(&svm, &sr_ata),
        0,
        "short receiver-only account gets no payer points"
    );
}

// FREE-FARM COST PROBE (funding-payer cohort): the anti-wash fee must apply to OI/funding farming exactly
// like LP/trader residual farming. Otherwise a delta-neutral pair could farm the funding-payer supply slice
// without paying the DAO fee that is supposed to be the Sybil-flat economic bound. Pins both paid-side flows
// inside the cumulative funding-payer cohort.
#[test]
fn funding_payer_claims_pay_the_anti_wash_fee_for_both_sides() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64; // funding-payer = 1_000_000
    let env = setup_funding_payer_only_with_fee(&mut svm, &payer, supply, 2_000); // 20% fee
    set_slot(&mut svm, 100);

    let long = Keypair::new();
    let short = Keypair::new();
    let long_pf = Pubkey::new_unique();
    let short_pf = Pubkey::new_unique();
    set_portfolio_funding(
        &mut svm,
        &long_pf,
        &env.stub_perc,
        &env.market,
        &long.pubkey(),
        0,
        0,
        0,
        0,
    );
    set_portfolio_funding(
        &mut svm,
        &short_pf,
        &env.stub_perc,
        &env.market,
        &short.pubkey(),
        0,
        0,
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &long,
        &long.pubkey(),
        &long_pf,
        COHORT_FUNDING_PAYER,
    )
    .expect("reg long payer");
    register(
        &mut svm,
        &payer,
        &env,
        &short,
        &short.pubkey(),
        &short_pf,
        COHORT_FUNDING_PAYER,
    )
    .expect("reg short payer");

    set_slot(&mut svm, 1_000);
    set_portfolio_funding(
        &mut svm,
        &long_pf,
        &env.stub_perc,
        &env.market,
        &long.pubkey(),
        10_000,
        0,
        0,
        0,
    );
    set_portfolio_funding(
        &mut svm,
        &short_pf,
        &env.stub_perc,
        &env.market,
        &short.pubkey(),
        0,
        0,
        10_000,
        0,
    );
    crystallize(&mut svm, &payer, &env, &long, &long_pf).expect("cry long payer");
    crystallize(&mut svm, &payer, &env, &short, &short_pf).expect("cry short payer");

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");
    let long_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &long.pubkey());
    let short_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &short.pubkey());
    claim(&mut svm, &payer, &env, &long, &long_ata, None).expect("long claim");
    claim(&mut svm, &payer, &env, &short, &short_ata, None).expect("short claim");

    assert_eq!(
        token_amount(&svm, &long_ata),
        400_000,
        "long-paid funding gets 80% of its 500k pro-rata share after fee"
    );
    assert_eq!(
        token_amount(&svm, &short_ata),
        400_000,
        "short-paid funding gets 80% of its 500k pro-rata share after fee"
    );
    assert_eq!(
        token_amount(&svm, &env.vault),
        200_000,
        "both 100k OI-farming fees stay retained in the vault"
    );
}

// FREE-FARM PROBE (funding-payer market scope): funding-paid counters from an attacker-controlled market must
// not register. This is the OI/funding counterpart to the LP/trader foreign-market allow-list tests.
#[test]
fn funding_payer_register_rejects_portfolios_from_foreign_markets() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup_funding_payer_only(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);

    let attacker = Keypair::new();
    let foreign = Pubkey::new_unique();
    let evil_long = Pubkey::new_unique();
    let evil_short = Pubkey::new_unique();
    set_portfolio_funding(
        &mut svm,
        &evil_long,
        &env.stub_perc,
        &foreign,
        &attacker.pubkey(),
        9_000_000,
        0,
        0,
        0,
    );
    set_portfolio_funding(
        &mut svm,
        &evil_short,
        &env.stub_perc,
        &foreign,
        &attacker.pubkey(),
        0,
        0,
        9_000_000,
        0,
    );
    assert!(
        register(
            &mut svm,
            &payer,
            &env,
            &attacker,
            &attacker.pubkey(),
            &evil_long,
            COHORT_FUNDING_PAYER
        )
        .is_err(),
        "funding-payer cohort rejects long-paid counters from a foreign market"
    );
    assert!(
        register(
            &mut svm,
            &payer,
            &env,
            &attacker,
            &attacker.pubkey(),
            &evil_short,
            COHORT_FUNDING_PAYER
        )
        .is_err(),
        "funding-payer cohort rejects short-paid counters from a foreign market"
    );

    let good = Pubkey::new_unique();
    set_portfolio_funding(
        &mut svm,
        &good,
        &env.stub_perc,
        &env.market,
        &attacker.pubkey(),
        0,
        0,
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &attacker,
        &attacker.pubkey(),
        &good,
        COHORT_FUNDING_PAYER,
    )
    .expect("the same owner can register a portfolio from the allow-listed market");
}

// COVERAGE PROBE (funding-payer extra market allow-list): the funding-payer cohort uses the same market
// allow-list branch as LP/trader, but the OI-farming path must also accept vetted non-primary markets.
#[test]
fn funding_payer_accepts_extra_allowlisted_market_and_rejects_off_list_market() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let extra_market = Pubkey::new_unique();
    let env = setup_funding_payer_only_with_fee_and_extras(
        &mut svm,
        &payer,
        1_000_000,
        0,
        &[extra_market],
    );
    set_slot(&mut svm, 100);

    let long = Keypair::new();
    let short = Keypair::new();
    let off_list = Keypair::new();
    let long_pf = Pubkey::new_unique();
    let short_pf = Pubkey::new_unique();
    let off_list_pf = Pubkey::new_unique();
    set_portfolio_funding(
        &mut svm,
        &long_pf,
        &env.stub_perc,
        &extra_market,
        &long.pubkey(),
        1_000,
        0,
        0,
        0,
    );
    set_portfolio_funding(
        &mut svm,
        &short_pf,
        &env.stub_perc,
        &extra_market,
        &short.pubkey(),
        0,
        0,
        1_000,
        0,
    );
    set_portfolio_funding(
        &mut svm,
        &off_list_pf,
        &env.stub_perc,
        &Pubkey::new_unique(),
        &off_list.pubkey(),
        1_000,
        0,
        0,
        0,
    );

    register(
        &mut svm,
        &payer,
        &env,
        &long,
        &long.pubkey(),
        &long_pf,
        COHORT_FUNDING_PAYER,
    )
    .expect("funding-payer accepts long-paid counters from an extra allow-listed market");
    register(
        &mut svm,
        &payer,
        &env,
        &short,
        &short.pubkey(),
        &short_pf,
        COHORT_FUNDING_PAYER,
    )
    .expect("funding-payer accepts short-paid counters from an extra allow-listed market");
    assert!(
        register(
            &mut svm,
            &payer,
            &env,
            &off_list,
            &off_list.pubkey(),
            &off_list_pf,
            COHORT_FUNDING_PAYER
        )
        .is_err(),
        "off-list funding-payer market still rejects"
    );
}

// ATTACK PROBE (funding-payer denominator inflation via substituted crystallize portfolio): funding-payer
// crystallize mutates the cohort denominator, so it must bind the portfolio to the one registered at start.
#[test]
fn funding_payer_crystallize_rejects_a_substituted_portfolio_no_denominator_inflation() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup_funding_payer_only(&mut svm, &payer, 1_000_000); // funding-payer cohort = 1_000_000
    set_slot(&mut svm, 100);

    let attacker = Keypair::new();
    let honest = Keypair::new();
    let attacker_pf = Pubkey::new_unique();
    let honest_pf = Pubkey::new_unique();
    let decoy_pf = Pubkey::new_unique();
    set_portfolio_funding(
        &mut svm,
        &attacker_pf,
        &env.stub_perc,
        &env.market,
        &attacker.pubkey(),
        0,
        0,
        0,
        0,
    );
    set_portfolio_funding(
        &mut svm,
        &honest_pf,
        &env.stub_perc,
        &env.market,
        &honest.pubkey(),
        0,
        0,
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &attacker,
        &attacker.pubkey(),
        &attacker_pf,
        COHORT_FUNDING_PAYER,
    )
    .expect("reg attacker");
    register(
        &mut svm,
        &payer,
        &env,
        &honest,
        &honest.pubkey(),
        &honest_pf,
        COHORT_FUNDING_PAYER,
    )
    .expect("reg honest");

    set_slot(&mut svm, 1_124); // tenure 1024 -> floor_log2 = 10
    set_portfolio_funding(
        &mut svm,
        &attacker_pf,
        &env.stub_perc,
        &env.market,
        &attacker.pubkey(),
        1_000,
        0,
        0,
        0,
    );
    set_portfolio_funding(
        &mut svm,
        &honest_pf,
        &env.stub_perc,
        &env.market,
        &honest.pubkey(),
        1_000,
        0,
        0,
        0,
    );
    set_portfolio_funding(
        &mut svm,
        &decoy_pf,
        &env.stub_perc,
        &env.market,
        &attacker.pubkey(),
        99_999,
        0,
        0,
        0,
    );
    assert!(crystallize(&mut svm, &payer, &env, &attacker, &decoy_pf).is_err(),
        "a substituted funding portfolio is rejected before it can inflate the funding-payer denominator");

    crystallize(&mut svm, &payer, &env, &attacker, &attacker_pf)
        .expect("attacker bound crystallize");
    crystallize(&mut svm, &payer, &env, &honest, &honest_pf).expect("honest crystallize");
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    let attacker_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &attacker.pubkey());
    let honest_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &honest.pubkey());
    claim(&mut svm, &payer, &env, &attacker, &attacker_ata, None).expect("attacker claim");
    claim(&mut svm, &payer, &env, &honest, &honest_ata, None).expect("honest claim");
    assert_eq!(
        token_amount(&svm, &attacker_ata),
        500_000,
        "attacker gets only its honest half of the funding-payer cohort"
    );
    assert_eq!(
        token_amount(&svm, &honest_ata),
        500_000,
        "honest claim is not diluted by a decoy high counter"
    );
}

// ATTACK PROBE (same-key portfolio reuse): Percolator `ClosePortfolio` dematerializes a flat portfolio and
// `InitPortfolio` can later write fresh owner/market provenance into an uninitialized account at the same key.
// The funding-payer stake binds the portfolio KEY at register, but crystallize is where the denominator is
// updated. So crystallize must re-check the registered key still has the registered owner and an allow-listed
// market; otherwise a user can register a clean key, reinitialize that same key against an attacker-controlled
// market or owner, and mint long/short funding points into this genesis.
#[test]
fn funding_payer_crystallize_rejects_same_key_reinitialized_owner_or_market() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup_funding_payer_only(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);

    let attacker = Keypair::new();
    let honest = Keypair::new();
    let attacker_pf = Pubkey::new_unique();
    let honest_pf = Pubkey::new_unique();
    set_portfolio_funding(
        &mut svm,
        &attacker_pf,
        &env.stub_perc,
        &env.market,
        &attacker.pubkey(),
        0,
        0,
        0,
        0,
    );
    set_portfolio_funding(
        &mut svm,
        &honest_pf,
        &env.stub_perc,
        &env.market,
        &honest.pubkey(),
        0,
        0,
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &attacker,
        &attacker.pubkey(),
        &attacker_pf,
        COHORT_FUNDING_PAYER,
    )
    .expect("register attacker funding payer");
    register(
        &mut svm,
        &payer,
        &env,
        &honest,
        &honest.pubkey(),
        &honest_pf,
        COHORT_FUNDING_PAYER,
    )
    .expect("register honest funding payer");

    set_slot(&mut svm, 1_124);
    let foreign_market = Pubkey::new_unique();
    set_portfolio_funding(
        &mut svm,
        &attacker_pf,
        &env.stub_perc,
        &foreign_market,
        &attacker.pubkey(),
        99_999,
        0,
        0,
        0,
    );
    assert!(
        crystallize(&mut svm, &payer, &env, &attacker, &attacker_pf).is_err(),
        "same portfolio key reinitialized to a foreign market must not mint funding-payer points"
    );

    let foreign_owner = Keypair::new();
    set_portfolio_funding(
        &mut svm,
        &attacker_pf,
        &env.stub_perc,
        &env.market,
        &foreign_owner.pubkey(),
        99_999,
        0,
        0,
        0,
    );
    assert!(
        crystallize(&mut svm, &payer, &env, &attacker, &attacker_pf).is_err(),
        "same portfolio key reinitialized to a foreign owner must not mint funding-payer points"
    );

    set_portfolio_funding(
        &mut svm,
        &attacker_pf,
        &env.stub_perc,
        &env.market,
        &attacker.pubkey(),
        1_000,
        0,
        0,
        0,
    );
    set_portfolio_funding(
        &mut svm,
        &honest_pf,
        &env.stub_perc,
        &env.market,
        &honest.pubkey(),
        1_000,
        0,
        0,
        0,
    );
    crystallize(&mut svm, &payer, &env, &attacker, &attacker_pf)
        .expect("attacker valid crystallize");
    crystallize(&mut svm, &payer, &env, &honest, &honest_pf).expect("honest crystallize");

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");
    let attacker_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &attacker.pubkey());
    let honest_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &honest.pubkey());
    claim(&mut svm, &payer, &env, &attacker, &attacker_ata, None).expect("attacker claim");
    claim(&mut svm, &payer, &env, &honest, &honest_ata, None).expect("honest claim");
    assert_eq!(
        token_amount(&svm, &attacker_ata),
        500_000,
        "attacker only receives its valid same-market/owner half"
    );
    assert_eq!(
        token_amount(&svm, &honest_ata),
        500_000,
        "honest denominator is not diluted by same-key reuse attempts"
    );
}

// LIFECYCLE PROBE (funding-payer claim after portfolio cleanup): paid-funding counters are monotonic and points
// are frozen at crystallize/freeze, so the claim must not require the Percolator portfolio to still exist. Users
// can close flat portfolios before claiming; requiring the account here strands otherwise valid COIN.
#[test]
fn funding_payer_claim_does_not_require_portfolio_after_freeze() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup_funding_payer_only(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);

    let farmer = Keypair::new();
    let pf = Pubkey::new_unique();
    set_portfolio_funding(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &farmer.pubkey(),
        0,
        0,
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &farmer,
        &farmer.pubkey(),
        &pf,
        COHORT_FUNDING_PAYER,
    )
    .expect("reg funding payer");
    set_slot(&mut svm, 1_000);
    set_portfolio_funding(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &farmer.pubkey(),
        5_000,
        0,
        0,
        0,
    );
    crystallize(&mut svm, &payer, &env, &farmer, &pf).expect("cry long payer");

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");
    let ata = create_token_account(&mut svm, &payer, &env.coin_mint, &farmer.pubkey());

    svm.set_account(
        pf,
        Account {
            lamports: 0,
            data: Vec::new(),
            owner: solana_sdk::system_program::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
    claim_without_linked(&mut svm, &payer, &env, &farmer, &ata)
        .expect("funding-payer claim after portfolio cleanup");
    assert_eq!(
        token_amount(&svm, &ata),
        1_000_000,
        "frozen funding-payer points remain claimable after portfolio cleanup"
    );
}

// PUBLIC DOS (terminal residual claim): after Percolator closes a portfolio, anyone can transfer one
// lamport to its zero-data address. Depending on runtime purge timing the address can retain its old
// Percolator owner or return to the system program; a PDA-owned portfolio has no signer that can remove
// that dust. The exact linked key still proves which frozen stake is being claimed, and neither empty
// account can carry live Percolator counters, so dust must not disable the dematerialized-witness
// fallback. Exercise both residual cohorts and owner states through the deployed SBF.
#[test]
fn lamport_dust_cannot_lock_frozen_lp_or_trader_reward() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);

    let lp = Keypair::new();
    let trader = Keypair::new();
    let lp_pf = Pubkey::new_unique();
    let trader_pf = Pubkey::new_unique();
    set_portfolio_full(
        &mut svm,
        &lp_pf,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        0,
        0,
        0,
    );
    set_portfolio_full(
        &mut svm,
        &trader_pf,
        &env.stub_perc,
        &env.market,
        &trader.pubkey(),
        0,
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &lp,
        &lp.pubkey(),
        &lp_pf,
        COHORT_LP,
    )
    .expect("register LP");
    register(
        &mut svm,
        &payer,
        &env,
        &trader,
        &trader.pubkey(),
        &trader_pf,
        COHORT_TRADER,
    )
    .expect("register trader");

    set_slot(&mut svm, 1_124);
    set_portfolio_full(
        &mut svm,
        &lp_pf,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        10_000,
        0,
        0,
    );
    set_portfolio_full(
        &mut svm,
        &trader_pf,
        &env.stub_perc,
        &env.market,
        &trader.pubkey(),
        0,
        20_000,
        0,
    );
    crystallize(&mut svm, &payer, &env, &lp, &lp_pf).expect("crystallize LP");
    crystallize(&mut svm, &payer, &env, &trader, &trader_pf)
        .expect("crystallize trader");
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    for (portfolio, owner) in [
        (lp_pf, solana_sdk::system_program::ID),
        (trader_pf, env.stub_perc),
    ] {
        svm.set_account(
            portfolio,
            Account {
                lamports: 1,
                data: Vec::new(),
                owner,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    }

    let lp_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &lp.pubkey());
    let trader_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &trader.pubkey());
    let fake_archive = Pubkey::new_unique();
    svm.set_account(
        fake_archive,
        Account {
            lamports: 1,
            data: Vec::new(),
            owner: solana_sdk::system_program::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
    let lp_stake = stake_pda_for_cohort(&env, &lp.pubkey(), &lp_pf, COHORT_LP);
    let stake_before_fake_archive = svm.get_account(&lp_stake).unwrap();
    assert!(
        send(
            &mut svm,
            &payer,
            &[Instruction {
                program_id: rd_id(),
                accounts: vec![
                    AccountMeta::new(payer.pubkey(), true),
                    AccountMeta::new_readonly(env.rd_config, false),
                    AccountMeta::new(lp_stake, false),
                    AccountMeta::new(env.vault, false),
                    AccountMeta::new(lp_ata, false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                    AccountMeta::new_readonly(lp_pf, false),
                    AccountMeta::new_readonly(fake_archive, false),
                ],
                data: vec![5u8],
            }],
            &[],
        )
        .is_err(),
        "an unrelated empty account cannot select the pre-archive claim fallback"
    );
    assert_eq!(svm.get_account(&lp_stake).unwrap(), stake_before_fake_archive);
    assert_eq!(token_amount(&svm, &lp_ata), 0);
    claim_as(
        &mut svm,
        &payer,
        &env,
        &payer,
        &lp.pubkey(),
        &lp_ata,
        None,
    )
    .expect("public LP claim survives dusted closed witness");
    claim_as(
        &mut svm,
        &payer,
        &env,
        &payer,
        &trader.pubkey(),
        &trader_ata,
        None,
    )
    .expect("public trader claim survives dusted closed witness");

    assert_eq!(token_amount(&svm, &lp_ata), 400_000);
    assert_eq!(token_amount(&svm, &trader_ata), 400_000);
    assert_eq!(token_amount(&svm, &env.vault), 200_000);
}

// PAY-UP PROBE (funding-payer claim one-sided live cap): if paid funding grows after the last crystallize,
// claim must not pay the higher live counter against the frozen denominator. Extra funding needs a re-crystallize
// before freeze; otherwise it stays out of both numerator and denominator.
#[test]
fn funding_payer_claim_never_pays_above_frozen_points_when_live_counter_grows_without_recrystallize(
) {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup_funding_payer_only(&mut svm, &payer, 1_000_000); // funding-payer cohort = 1_000_000
    set_slot(&mut svm, 100);

    let honest = Keypair::new();
    let grower = Keypair::new();
    let honest_pf = Pubkey::new_unique();
    let grower_pf = Pubkey::new_unique();
    set_portfolio_funding(
        &mut svm,
        &honest_pf,
        &env.stub_perc,
        &env.market,
        &honest.pubkey(),
        0,
        0,
        0,
        0,
    );
    set_portfolio_funding(
        &mut svm,
        &grower_pf,
        &env.stub_perc,
        &env.market,
        &grower.pubkey(),
        0,
        0,
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &honest,
        &honest.pubkey(),
        &honest_pf,
        COHORT_FUNDING_PAYER,
    )
    .expect("reg honest");
    register(
        &mut svm,
        &payer,
        &env,
        &grower,
        &grower.pubkey(),
        &grower_pf,
        COHORT_FUNDING_PAYER,
    )
    .expect("reg grower");

    set_slot(&mut svm, 1_124); // tenure 1024 -> floor_log2 = 10
    set_portfolio_funding(
        &mut svm,
        &honest_pf,
        &env.stub_perc,
        &env.market,
        &honest.pubkey(),
        1_000,
        0,
        0,
        0,
    );
    set_portfolio_funding(
        &mut svm,
        &grower_pf,
        &env.stub_perc,
        &env.market,
        &grower.pubkey(),
        1_000,
        0,
        0,
        0,
    );
    crystallize(&mut svm, &payer, &env, &honest, &honest_pf).expect("honest crystallize");
    crystallize(&mut svm, &payer, &env, &grower, &grower_pf).expect("grower crystallize");

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");
    set_portfolio_funding(
        &mut svm,
        &grower_pf,
        &env.stub_perc,
        &env.market,
        &grower.pubkey(),
        100_000,
        0,
        0,
        0,
    );

    let honest_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &honest.pubkey());
    let grower_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &grower.pubkey());
    claim(&mut svm, &payer, &env, &honest, &honest_ata, None).expect("honest claim");
    claim(&mut svm, &payer, &env, &grower, &grower_ata, None).expect("grower claim");
    assert_eq!(
        token_amount(&svm, &honest_ata),
        500_000,
        "honest frozen share is not diluted by an uncrystallized live-counter grow"
    );
    assert_eq!(
        token_amount(&svm, &grower_ata),
        500_000,
        "live counter growth after freeze does not pay above frozen points"
    );
}

// USER-EXPECTATION PROBE (cumulative funding-payer points): a single portfolio that paid funding as both
// long and short over time earns both counters through ONE stake.
#[test]
fn one_funding_payer_stake_claims_long_paid_plus_short_paid_for_one_portfolio() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup_funding_payer_only(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);

    let both = Keypair::new();
    let long_only = Keypair::new();
    let both_pf = Pubkey::new_unique();
    let long_pf = Pubkey::new_unique();
    set_portfolio_funding(
        &mut svm,
        &both_pf,
        &env.stub_perc,
        &env.market,
        &both.pubkey(),
        0,
        0,
        0,
        0,
    );
    set_portfolio_funding(
        &mut svm,
        &long_pf,
        &env.stub_perc,
        &env.market,
        &long_only.pubkey(),
        0,
        0,
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &both,
        &both.pubkey(),
        &both_pf,
        COHORT_FUNDING_PAYER,
    )
    .expect("owner registers one cumulative funding-payer stake");
    register(
        &mut svm,
        &payer,
        &env,
        &long_only,
        &long_only.pubkey(),
        &long_pf,
        COHORT_FUNDING_PAYER,
    )
    .expect("control owner registers a funding-payer stake");

    set_slot(&mut svm, 1_124); // same tenure for both portfolios
    set_portfolio_funding(
        &mut svm,
        &both_pf,
        &env.stub_perc,
        &env.market,
        &both.pubkey(),
        6_000,
        0,
        4_000,
        0,
    );
    set_portfolio_funding(
        &mut svm,
        &long_pf,
        &env.stub_perc,
        &env.market,
        &long_only.pubkey(),
        10_000,
        0,
        0,
        0,
    );
    crystallize(&mut svm, &payer, &env, &both, &both_pf).expect("crystallize cumulative payer");
    crystallize(&mut svm, &payer, &env, &long_only, &long_pf).expect("crystallize long-only payer");

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");
    let both_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &both.pubkey());
    let long_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &long_only.pubkey());
    claim(&mut svm, &payer, &env, &both, &both_ata, None).expect("both-side claim");
    claim(&mut svm, &payer, &env, &long_only, &long_ata, None).expect("long-only claim");

    assert_eq!(
        token_amount(&svm, &both_ata),
        500_000,
        "6k long-paid + 4k short-paid earns the same as 10k long-paid"
    );
    assert_eq!(
        token_amount(&svm, &long_ata),
        500_000,
        "control gets the other half of the funding-payer cohort"
    );
}

// SEMANTICS PROBE (funding-payer has no age multiplier): funding paid late should earn the same points as
// the same paid delta from an early-registered portfolio. Residual LP/trader cohorts keep log-time weighting,
// but funding-payer is just the paid accumulator delta.
#[test]
fn funding_payer_points_are_accumulator_delta_without_age_multiplier() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup_funding_payer_only(&mut svm, &payer, 1_000_000);

    let early = Keypair::new();
    let late = Keypair::new();
    let early_pf = Pubkey::new_unique();
    let late_pf = Pubkey::new_unique();

    set_slot(&mut svm, 100);
    set_portfolio_funding(
        &mut svm,
        &early_pf,
        &env.stub_perc,
        &env.market,
        &early.pubkey(),
        0,
        0,
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &early,
        &early.pubkey(),
        &early_pf,
        COHORT_FUNDING_PAYER,
    )
    .expect("register early funding payer");

    set_slot(&mut svm, 9_000);
    set_portfolio_funding(
        &mut svm,
        &late_pf,
        &env.stub_perc,
        &env.market,
        &late.pubkey(),
        0,
        0,
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &late,
        &late.pubkey(),
        &late_pf,
        COHORT_FUNDING_PAYER,
    )
    .expect("register late funding payer");

    set_slot(&mut svm, 10_000);
    set_portfolio_funding(
        &mut svm,
        &early_pf,
        &env.stub_perc,
        &env.market,
        &early.pubkey(),
        10_000,
        0,
        0,
        0,
    );
    set_portfolio_funding(
        &mut svm,
        &late_pf,
        &env.stub_perc,
        &env.market,
        &late.pubkey(),
        10_000,
        0,
        0,
        0,
    );
    crystallize(&mut svm, &payer, &env, &early, &early_pf).expect("crystallize early");
    crystallize(&mut svm, &payer, &env, &late, &late_pf).expect("crystallize late");

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");
    let early_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &early.pubkey());
    let late_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &late.pubkey());
    claim(&mut svm, &payer, &env, &early, &early_ata, None).expect("early claim");
    claim(&mut svm, &payer, &env, &late, &late_ata, None).expect("late claim");

    assert_eq!(
        token_amount(&svm, &early_ata),
        500_000,
        "early registration gives no extra funding-payer points"
    );
    assert_eq!(
        token_amount(&svm, &late_ata),
        500_000,
        "same paid accumulator delta earns the same funding-payer share"
    );
}

// NO-AGE/FREE-FARMING SWEEP: 100 deterministic LiteSVM cases where two funding payers register at
// different slots but pay the same net funding amount. Funding-payer points are raw accumulator deltas,
// so early and late registrations must pay equally unless one portfolio later pays more. The sweep also
// pins permissionless crystallize replay, claim redirection rejection, receiver-only canaries, fee edges,
// omitted linked claim accounts, extra-market allow-listing, and vault conservation.
#[test]
fn funding_payer_100_case_no_age_replay_and_claim_redirect_sweep() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();

    let splits: [(u16, u16, u16, u16); 6] = [
        (0, 0, 0, 10_000),
        (1_000, 1_000, 0, 8_000),
        (2_500, 0, 2_500, 5_000),
        (0, 500, 0, 1),
        (333, 777, 1_111, 2_222),
        (4_999, 1, 0, 5_000),
    ];
    let fees: [u16; 8] = [0, 1, 9, 333, 2_500, 7_777, 9_999, 10_000];
    let ceil_fee = |amount: u64, fee_bps: u16| -> u64 {
        (((amount as u128) * (fee_bps as u128) + 9_999) / 10_000) as u64
    };

    for case_idx in 0..100usize {
        let (insurance_bps, backing_bps, lp_bps, funding_bps) = splits[case_idx % splits.len()];
        let fee_bps = fees[(case_idx / splits.len()) % fees.len()];
        let supply = 25_001u64 + (case_idx as u64 * 211);
        let extras: Vec<Pubkey> = if case_idx % 3 == 0 {
            vec![Pubkey::new_unique(), Pubkey::new_unique()]
        } else {
            Vec::new()
        };
        let env = setup_custom_split_with_fee_and_extras(
            &mut svm,
            &payer,
            supply,
            insurance_bps,
            backing_bps,
            lp_bps,
            funding_bps,
            fee_bps,
            &extras,
        );
        let market = if extras.is_empty() {
            env.market
        } else {
            extras[case_idx % extras.len()]
        };

        let early = Keypair::new();
        let late = Keypair::new();
        let receiver = Keypair::new();
        let attacker = Keypair::new();
        svm.airdrop(&attacker.pubkey(), 1_000_000_000).unwrap();
        let early_pf = Pubkey::new_unique();
        let late_pf = Pubkey::new_unique();
        let receiver_pf = Pubkey::new_unique();
        let start_slot = 100 + case_idx as u64;
        let late_slot = start_slot + 500 + ((case_idx as u64 * 13) % 3_000);
        let final_slot = late_slot + 1 + ((case_idx as u64 * 29) % 6_000);

        let early_baseline_long = ((case_idx as u128 * 17) % 2_000) + 3;
        let early_baseline_short = ((case_idx as u128 * 19) % 2_000) + 5;
        let late_baseline_long = ((case_idx as u128 * 23) % 2_000) + 7;
        let late_baseline_short = ((case_idx as u128 * 31) % 2_000) + 11;
        let shared_delta = ((case_idx as u128 * 97) % 20_000) + 1;
        let early_long_delta = shared_delta * (((case_idx % 5) + 1) as u128) / 6;
        let early_short_delta = shared_delta - early_long_delta;
        let late_short_delta = shared_delta * (((case_idx % 7) + 1) as u128) / 8;
        let late_long_delta = shared_delta - late_short_delta;

        set_slot(&mut svm, start_slot);
        set_portfolio_funding(
            &mut svm,
            &early_pf,
            &env.stub_perc,
            &market,
            &early.pubkey(),
            early_baseline_long,
            0,
            early_baseline_short,
            0,
        );
        register(
            &mut svm,
            &payer,
            &env,
            &early,
            &early.pubkey(),
            &early_pf,
            COHORT_FUNDING_PAYER,
        )
        .expect("register early funding payer");

        crystallize_as(
            &mut svm,
            &payer,
            &env,
            &attacker,
            &early.pubkey(),
            &early_pf,
        )
        .expect("foreign cranker can crystallize unchanged early funding payer");

        set_slot(&mut svm, late_slot);
        set_portfolio_funding(
            &mut svm,
            &late_pf,
            &env.stub_perc,
            &market,
            &late.pubkey(),
            late_baseline_long,
            0,
            late_baseline_short,
            0,
        );
        set_portfolio_funding(
            &mut svm,
            &receiver_pf,
            &env.stub_perc,
            &market,
            &receiver.pubkey(),
            0,
            shared_delta + 50_000,
            0,
            shared_delta + 75_000,
        );
        register(
            &mut svm,
            &payer,
            &env,
            &late,
            &late.pubkey(),
            &late_pf,
            COHORT_FUNDING_PAYER,
        )
        .expect("register late funding payer");
        register(
            &mut svm,
            &payer,
            &env,
            &receiver,
            &receiver.pubkey(),
            &receiver_pf,
            COHORT_FUNDING_PAYER,
        )
        .expect("register receiver-only canary");

        set_slot(&mut svm, final_slot);
        set_portfolio_funding(
            &mut svm,
            &early_pf,
            &env.stub_perc,
            &market,
            &early.pubkey(),
            early_baseline_long + early_long_delta,
            90_000,
            early_baseline_short + early_short_delta,
            80_000,
        );
        set_portfolio_funding(
            &mut svm,
            &late_pf,
            &env.stub_perc,
            &market,
            &late.pubkey(),
            late_baseline_long + late_long_delta,
            70_000,
            late_baseline_short + late_short_delta,
            60_000,
        );
        set_portfolio_funding(
            &mut svm,
            &receiver_pf,
            &env.stub_perc,
            &market,
            &receiver.pubkey(),
            0,
            shared_delta + 90_000,
            0,
            shared_delta + 110_000,
        );

        crystallize_as(
            &mut svm,
            &payer,
            &env,
            &attacker,
            &early.pubkey(),
            &early_pf,
        )
        .expect("foreign cranker crystallizes early funding payer");
        crystallize(&mut svm, &payer, &env, &late, &late_pf)
            .expect("late funding payer crystallize");
        crystallize(&mut svm, &payer, &env, &receiver, &receiver_pf)
            .expect("receiver-only canary crystallize");

        let extra_delta = if case_idx % 4 == 0 {
            ((case_idx as u128 * 41) % 1_000) + 1
        } else {
            0
        };
        if extra_delta > 0 {
            set_portfolio_funding(
                &mut svm,
                &early_pf,
                &env.stub_perc,
                &market,
                &early.pubkey(),
                early_baseline_long + early_long_delta + extra_delta,
                90_000,
                early_baseline_short + early_short_delta,
                80_000,
            );
        }
        crystallize_as(
            &mut svm,
            &payer,
            &env,
            &attacker,
            &early.pubkey(),
            &early_pf,
        )
        .expect("foreign replay crystallize is idempotent except new paid delta");

        set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
        freeze(&mut svm, &payer, &env).expect("freeze");

        let early_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &early.pubkey());
        let late_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &late.pubkey());
        let receiver_ata =
            create_token_account(&mut svm, &payer, &env.coin_mint, &receiver.pubkey());
        let attacker_ata =
            create_token_account(&mut svm, &payer, &env.coin_mint, &attacker.pubkey());

        assert!(
            claim_as(
                &mut svm,
                &payer,
                &env,
                &attacker,
                &early.pubkey(),
                &attacker_ata,
                None,
            )
            .is_err(),
            "case {case_idx}: foreign claim cannot redirect payout to attacker-owned token account"
        );
        assert_eq!(
            token_amount(&svm, &attacker_ata),
            0,
            "case {case_idx}: failed redirect pays nothing"
        );

        if case_idx % 2 == 0 {
            claim_without_linked(&mut svm, &payer, &env, &early, &early_ata)
                .expect("early funding-payer claim without linked portfolio");
        } else {
            claim(&mut svm, &payer, &env, &early, &early_ata, None)
                .expect("early funding-payer claim with linked portfolio");
        }
        claim_without_linked(&mut svm, &payer, &env, &late, &late_ata)
            .expect("late funding-payer claim without linked portfolio");
        claim_without_linked(&mut svm, &payer, &env, &receiver, &receiver_ata)
            .expect("receiver-only funding-payer claim without linked portfolio");

        let funding_supply = ((env.supply as u128) * (funding_bps as u128) / 10_000) as u64;
        let early_points = shared_delta + extra_delta;
        let late_points = shared_delta;
        let total_points = early_points + late_points;
        let early_amount = ((funding_supply as u128) * early_points / total_points) as u64;
        let late_amount = ((funding_supply as u128) * late_points / total_points) as u64;
        let expected_early = early_amount - ceil_fee(early_amount, fee_bps);
        let expected_late = late_amount - ceil_fee(late_amount, fee_bps);

        assert_eq!(
            token_amount(&svm, &early_ata),
            expected_early,
            "case {case_idx}: early payout follows raw paid delta, not registration age"
        );
        assert_eq!(
            token_amount(&svm, &late_ata),
            expected_late,
            "case {case_idx}: late payout follows raw paid delta, not registration age"
        );
        assert_eq!(
            token_amount(&svm, &receiver_ata),
            0,
            "case {case_idx}: receiver-only portfolio cannot farm payer distribution"
        );
        if extra_delta == 0 {
            assert_eq!(
                token_amount(&svm, &early_ata),
                token_amount(&svm, &late_ata),
                "case {case_idx}: equal raw paid deltas pay equally despite different registration slots"
            );
        }

        let pre_fee_paid = early_amount + late_amount;
        assert!(
            pre_fee_paid <= funding_supply,
            "case {case_idx}: pre-fee funding payout cannot exceed configured slice"
        );
        assert!(
            funding_supply - pre_fee_paid <= 1,
            "case {case_idx}: only integer floor dust remains in funding slice"
        );
        let paid = token_amount(&svm, &early_ata)
            + token_amount(&svm, &late_ata)
            + token_amount(&svm, &receiver_ata)
            + token_amount(&svm, &attacker_ata);
        assert_eq!(
            token_amount(&svm, &env.vault),
            env.supply - paid,
            "case {case_idx}: vault only decreases by successful claims"
        );
    }
}

// CONFIG/FARMING SWEEP: 100 deterministic litesvm cases over funding-payer bps, mixed cohort splits, fee bps
// including dust/100% fee, primary vs extra allow-listed markets, nonzero register-time baselines, and asymmetric
// long-paid/short-paid deltas. This catches the weird branches that are easy to miss in single happy-path tests:
// receiver-only portfolios must never earn, baseline counters must not be replayed as new farming, the anti-wash
// fee must ceil on dust, and all paid COIN must be bounded by the configured funding-payer slice.
#[test]
fn funding_payer_100_case_config_sweep_preserves_payer_only_payouts() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();

    let splits: [(u16, u16, u16, u16); 6] = [
        (0, 0, 0, 10_000),
        (1_000, 1_000, 0, 8_000),
        (2_500, 0, 2_500, 5_000),
        (0, 500, 0, 1),
        (333, 777, 1_111, 2_222),
        (4_999, 1, 0, 5_000),
    ];
    let fees: [u16; 8] = [0, 1, 9, 333, 2_500, 7_777, 9_999, 10_000];

    let ceil_fee = |amount: u64, fee_bps: u16| -> u64 {
        (((amount as u128) * (fee_bps as u128) + 9_999) / 10_000) as u64
    };

    for case_idx in 0..100usize {
        let (insurance_bps, backing_bps, lp_bps, funding_bps) = splits[case_idx % splits.len()];
        let fee_bps = fees[(case_idx / splits.len()) % fees.len()];
        let supply = 10_003u64 + (case_idx as u64 * 137);
        let extras: Vec<Pubkey> = if case_idx % 4 == 0 {
            vec![Pubkey::new_unique(), Pubkey::new_unique()]
        } else {
            Vec::new()
        };
        let env = setup_custom_split_with_fee_and_extras(
            &mut svm,
            &payer,
            supply,
            insurance_bps,
            backing_bps,
            lp_bps,
            funding_bps,
            fee_bps,
            &extras,
        );
        let market = if extras.is_empty() {
            env.market
        } else {
            extras[case_idx % extras.len()]
        };

        let start_slot = 100 + case_idx as u64;
        set_slot(&mut svm, start_slot);
        let long = Keypair::new();
        let short = Keypair::new();
        let long_receiver = Keypair::new();
        let short_receiver = Keypair::new();
        let long_pf = Pubkey::new_unique();
        let short_pf = Pubkey::new_unique();
        let long_receiver_pf = Pubkey::new_unique();
        let short_receiver_pf = Pubkey::new_unique();

        let long_baseline = ((case_idx as u128 * 11) % 997) + 1;
        let short_baseline = ((case_idx as u128 * 17) % 991) + 1;
        let long_delta = ((case_idx as u128 * 37) % 5_000) + 1;
        let short_delta = ((case_idx as u128 * 53) % 7_000) + 1;

        set_portfolio_funding(
            &mut svm,
            &long_pf,
            &env.stub_perc,
            &market,
            &long.pubkey(),
            long_baseline,
            0,
            0,
            0,
        );
        set_portfolio_funding(
            &mut svm,
            &short_pf,
            &env.stub_perc,
            &market,
            &short.pubkey(),
            0,
            0,
            short_baseline,
            0,
        );
        set_portfolio_funding(
            &mut svm,
            &long_receiver_pf,
            &env.stub_perc,
            &market,
            &long_receiver.pubkey(),
            0,
            long_baseline + long_delta + 5,
            0,
            0,
        );
        set_portfolio_funding(
            &mut svm,
            &short_receiver_pf,
            &env.stub_perc,
            &market,
            &short_receiver.pubkey(),
            0,
            0,
            0,
            short_baseline + short_delta + 7,
        );

        register(
            &mut svm,
            &payer,
            &env,
            &long,
            &long.pubkey(),
            &long_pf,
            COHORT_FUNDING_PAYER,
        )
        .expect("register long payer");
        register(
            &mut svm,
            &payer,
            &env,
            &short,
            &short.pubkey(),
            &short_pf,
            COHORT_FUNDING_PAYER,
        )
        .expect("register short payer");
        register(
            &mut svm,
            &payer,
            &env,
            &long_receiver,
            &long_receiver.pubkey(),
            &long_receiver_pf,
            COHORT_FUNDING_PAYER,
        )
        .expect("register long receiver canary");
        register(
            &mut svm,
            &payer,
            &env,
            &short_receiver,
            &short_receiver.pubkey(),
            &short_receiver_pf,
            COHORT_FUNDING_PAYER,
        )
        .expect("register short receiver canary");

        set_slot(&mut svm, start_slot + 2 + ((case_idx as u64 * 19) % 4_096));
        set_portfolio_funding(
            &mut svm,
            &long_pf,
            &env.stub_perc,
            &market,
            &long.pubkey(),
            long_baseline + long_delta,
            99_000,
            0,
            0,
        );
        set_portfolio_funding(
            &mut svm,
            &short_pf,
            &env.stub_perc,
            &market,
            &short.pubkey(),
            0,
            0,
            short_baseline + short_delta,
            88_000,
        );
        set_portfolio_funding(
            &mut svm,
            &long_receiver_pf,
            &env.stub_perc,
            &market,
            &long_receiver.pubkey(),
            0,
            long_baseline + long_delta + 50_000,
            0,
            0,
        );
        set_portfolio_funding(
            &mut svm,
            &short_receiver_pf,
            &env.stub_perc,
            &market,
            &short_receiver.pubkey(),
            0,
            0,
            0,
            short_baseline + short_delta + 50_000,
        );
        crystallize(&mut svm, &payer, &env, &long, &long_pf).expect("crystallize long payer");
        crystallize(&mut svm, &payer, &env, &short, &short_pf).expect("crystallize short payer");
        crystallize(&mut svm, &payer, &env, &long_receiver, &long_receiver_pf)
            .expect("crystallize long receiver canary");
        crystallize(&mut svm, &payer, &env, &short_receiver, &short_receiver_pf)
            .expect("crystallize short receiver canary");

        set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
        freeze(&mut svm, &payer, &env).expect("freeze");

        let long_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &long.pubkey());
        let short_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &short.pubkey());
        let long_receiver_ata =
            create_token_account(&mut svm, &payer, &env.coin_mint, &long_receiver.pubkey());
        let short_receiver_ata =
            create_token_account(&mut svm, &payer, &env.coin_mint, &short_receiver.pubkey());
        claim(&mut svm, &payer, &env, &long, &long_ata, None).expect("long payer claim");
        claim(&mut svm, &payer, &env, &short, &short_ata, None).expect("short payer claim");
        claim(
            &mut svm,
            &payer,
            &env,
            &long_receiver,
            &long_receiver_ata,
            None,
        )
        .expect("long receiver canary claim");
        claim(
            &mut svm,
            &payer,
            &env,
            &short_receiver,
            &short_receiver_ata,
            None,
        )
        .expect("short receiver canary claim");

        let funding_supply = ((env.supply as u128) * (funding_bps as u128) / 10_000) as u64;
        let total_delta = long_delta + short_delta;
        let long_amount = ((funding_supply as u128) * long_delta / total_delta) as u64;
        let short_amount = ((funding_supply as u128) * short_delta / total_delta) as u64;
        let expected_long = long_amount - ceil_fee(long_amount, fee_bps);
        let expected_short = short_amount - ceil_fee(short_amount, fee_bps);

        assert_eq!(
            token_amount(&svm, &long_ata),
            expected_long,
            "case {case_idx}: long-paid payout"
        );
        assert_eq!(
            token_amount(&svm, &short_ata),
            expected_short,
            "case {case_idx}: short-paid payout"
        );
        assert_eq!(
            token_amount(&svm, &long_receiver_ata),
            0,
            "case {case_idx}: long receiver-only cannot farm"
        );
        assert_eq!(
            token_amount(&svm, &short_receiver_ata),
            0,
            "case {case_idx}: short receiver-only cannot farm"
        );
        assert!(
            long_amount + short_amount <= funding_supply,
            "case {case_idx}: pre-fee payout does not exceed funding slice"
        );
        assert!(
            funding_supply - (long_amount + short_amount) <= 1,
            "case {case_idx}: only floor dust remains in the funding slice"
        );
        let paid = token_amount(&svm, &long_ata)
            + token_amount(&svm, &short_ata)
            + token_amount(&svm, &long_receiver_ata)
            + token_amount(&svm, &short_receiver_ata);
        assert_eq!(
            token_amount(&svm, &env.vault),
            env.supply - paid,
            "case {case_idx}: vault only decreases by paid COIN"
        );
    }
}

// CONFIG COMPATIBILITY PROBE: older clients may still send the 6-byte tail
// `(fee_bps, old_long_payer_bps, old_short_payer_bps)`. The program intentionally sums the old long/short
// fields into the single cumulative funding-payer cohort. Pin both the successful parse and the overflow reject.
#[test]
fn legacy_long_short_funding_tail_maps_to_cumulative_funding_payer_bps() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();

    let make_env =
        |svm: &mut LiteSVM, old_long_bps: u16, old_short_bps: u16| -> Result<Env, String> {
            let supply = 1_000_000u64;
            let emission_end = 2_000u64;
            let finalize_window = 500u64;
            let mint_auth = Keypair::new();
            let coin_mint = create_mint(svm, &payer, &mint_auth.pubkey());
            let rd_config =
                Pubkey::find_program_address(&[b"rd_config", coin_mint.as_ref()], &rd_id()).0;
            let dist_config = dist_config_pda(&coin_mint, &rd_config);
            let vault = create_token_account(svm, &payer, &coin_mint, &rd_config);
            mint_to(svm, &payer, &coin_mint, &mint_auth, &vault, supply);
            let stub_sub = Pubkey::new_unique();
            let stub_perc = Pubkey::new_unique();
            let ins_pool = Pubkey::new_unique();
            let back_pool = Pubkey::new_unique();
            let market = Pubkey::new_unique();
            let mut d = rd_init_data(
                supply,
                emission_end,
                1_000,
                1_000,
                0,
                finalize_window,
                ins_pool,
                back_pool,
                market,
                &[],
                Some(0),
            );
            d.extend_from_slice(&old_long_bps.to_le_bytes());
            d.extend_from_slice(&old_short_bps.to_le_bytes());
            send(
                svm,
                &payer,
                &[Instruction {
                    program_id: rd_id(),
                    accounts: rd_init_accounts(
                        payer.pubkey(),
                        coin_mint,
                        dist_config,
                        stub_perc,
                        stub_sub,
                        rd_config,
                        mint_auth.pubkey(),
                    ),
                    data: d,
                }],
                &[&mint_auth],
            )?;
            revoke_mint(svm, &payer, &coin_mint, &mint_auth);
            Ok(Env {
                rd_config,
                coin_mint,
                vault,
                mint_auth,
                stub_sub,
                stub_perc,
                ins_pool,
                back_pool,
                market,
                supply,
                emission_end,
                finalize_window,
            })
        };

    let env = make_env(&mut svm, 3_000, 5_000)
        .expect("legacy 3000/5000 tail initializes as cumulative 8000");
    let funding_bps_off = 466 + 1 + 9 * 32;
    let stored_funding_bps = u16::from_le_bytes(
        svm.get_account(&env.rd_config).unwrap().data[funding_bps_off..funding_bps_off + 2]
            .try_into()
            .unwrap(),
    );
    assert_eq!(
        stored_funding_bps, 8_000,
        "legacy long+short funding bps are summed into one cumulative cohort"
    );

    set_slot(&mut svm, 100);
    let farmer = Keypair::new();
    let pf = Pubkey::new_unique();
    set_portfolio_funding(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &farmer.pubkey(),
        0,
        0,
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &farmer,
        &farmer.pubkey(),
        &pf,
        COHORT_FUNDING_PAYER,
    )
    .expect("register funding payer");
    set_slot(&mut svm, 1_000);
    set_portfolio_funding(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &farmer.pubkey(),
        1_000,
        0,
        1_000,
        0,
    );
    crystallize(&mut svm, &payer, &env, &farmer, &pf).expect("crystallize funding payer");
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");
    let ata = create_token_account(&mut svm, &payer, &env.coin_mint, &farmer.pubkey());
    claim(&mut svm, &payer, &env, &farmer, &ata, None).expect("claim funding payer");
    assert_eq!(
        token_amount(&svm, &ata),
        800_000,
        "legacy 3000+5000 tail pays the cumulative 80% slice"
    );

    assert!(
        make_env(&mut svm, 6_000, 5_000).is_err(),
        "legacy long+short funding bps above 100% is rejected"
    );
}

// GENESIS SPLIT PROBE: the intended points split can be 10% insurance + 10% backing + 80% cumulative
// funding-payer. The funding-payer stake uses ONE portfolio and earns long-paid + short-paid together.
#[test]
fn ten_ten_eighty_split_pays_insurance_backing_and_cumulative_funding() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64;
    let env = setup_share_value_and_funding_split(&mut svm, &payer, supply, 1_000, 1_000, 8_000);
    set_slot(&mut svm, 100);

    let insurance = Keypair::new();
    let backing = Keypair::new();
    let funding = Keypair::new();
    let ins_pos = Pubkey::new_unique();
    let back_pos = Pubkey::new_unique();
    let funding_pf = Pubkey::new_unique();
    set_position(
        &mut svm,
        &ins_pos,
        &env.stub_sub,
        &env.ins_pool,
        &insurance.pubkey(),
        1_000,
        false,
    );
    set_position(
        &mut svm,
        &back_pos,
        &env.stub_sub,
        &env.back_pool,
        &backing.pubkey(),
        1_000,
        false,
    );
    set_portfolio_funding(
        &mut svm,
        &funding_pf,
        &env.stub_perc,
        &env.market,
        &funding.pubkey(),
        0,
        0,
        0,
        0,
    );

    register(
        &mut svm,
        &payer,
        &env,
        &insurance,
        &insurance.pubkey(),
        &ins_pos,
        COHORT_INSURANCE,
    )
    .expect("register insurance");
    register(
        &mut svm,
        &payer,
        &env,
        &backing,
        &backing.pubkey(),
        &back_pos,
        COHORT_BACKING,
    )
    .expect("register backing");
    register(
        &mut svm,
        &payer,
        &env,
        &funding,
        &funding.pubkey(),
        &funding_pf,
        COHORT_FUNDING_PAYER,
    )
    .expect("register funding-payer");

    set_slot(&mut svm, 1_124); // capital cohorts get equal log tenure; funding stays age-neutral.
    set_portfolio_funding(
        &mut svm,
        &funding_pf,
        &env.stub_perc,
        &env.market,
        &funding.pubkey(),
        6_000,
        0,
        4_000,
        0,
    );
    crystallize(&mut svm, &payer, &env, &insurance, &ins_pos).expect("crystallize insurance");
    crystallize(&mut svm, &payer, &env, &backing, &back_pos).expect("crystallize backing");
    crystallize(&mut svm, &payer, &env, &funding, &funding_pf)
        .expect("crystallize cumulative funding");

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");
    let ins_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &insurance.pubkey());
    let back_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &backing.pubkey());
    let funding_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &funding.pubkey());
    claim(&mut svm, &payer, &env, &insurance, &ins_ata, Some(&ins_pos)).expect("insurance claim");
    claim(&mut svm, &payer, &env, &backing, &back_ata, Some(&back_pos)).expect("backing claim");
    claim(
        &mut svm,
        &payer,
        &env,
        &funding,
        &funding_ata,
        Some(&funding_pf),
    )
    .expect("funding-payer claim");

    assert_eq!(
        token_amount(&svm, &ins_ata),
        100_000,
        "insurance capital cohort receives 10%"
    );
    assert_eq!(
        token_amount(&svm, &back_ata),
        100_000,
        "backing capital cohort receives 10%"
    );
    assert_eq!(
        token_amount(&svm, &funding_ata),
        800_000,
        "one funding stake receives the 80% cumulative long-paid + short-paid slice"
    );
    assert_eq!(
        token_amount(&svm, &env.vault),
        0,
        "10/10/80 claims exhaust the fixed supply with no dust in this exact split"
    );
}

// MULTI-COHORT OWNER LOF: one wallet can legitimately be both an insurance depositor and a backing
// depositor in the same genesis. Stake PDAs must therefore be linked-account scoped, not owner-only. If the PDA
// is only `(config, owner)`, the first registration consumes the owner's only stake account and the second
// cohort cannot register, forfeiting a real reward allocation even though both positions are valid and
// separately scoped.
#[test]
fn one_owner_can_register_and_claim_insurance_and_backing_cohorts() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64;
    let env = setup_share_value_and_funding_split(&mut svm, &payer, supply, 1_000, 1_000, 8_000);
    set_slot(&mut svm, 100);

    let owner = Keypair::new();
    let ins_pos = Pubkey::new_unique();
    let back_pos = Pubkey::new_unique();
    set_position(
        &mut svm,
        &ins_pos,
        &env.stub_sub,
        &env.ins_pool,
        &owner.pubkey(),
        1_000,
        false,
    );
    set_position(
        &mut svm,
        &back_pos,
        &env.stub_sub,
        &env.back_pool,
        &owner.pubkey(),
        1_000,
        false,
    );

    register(
        &mut svm,
        &payer,
        &env,
        &owner,
        &owner.pubkey(),
        &ins_pos,
        COHORT_INSURANCE,
    )
    .expect("register insurance");
    register(
        &mut svm,
        &payer,
        &env,
        &owner,
        &owner.pubkey(),
        &back_pos,
        COHORT_BACKING,
    )
    .expect("same owner also registers backing");

    set_slot(&mut svm, 1_124);
    crystallize(&mut svm, &payer, &env, &owner, &ins_pos).expect("crystallize insurance");
    crystallize(&mut svm, &payer, &env, &owner, &back_pos).expect("crystallize backing");
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");
    let ata = create_token_account(&mut svm, &payer, &env.coin_mint, &owner.pubkey());
    claim(&mut svm, &payer, &env, &owner, &ata, Some(&ins_pos)).expect("claim insurance");
    claim(&mut svm, &payer, &env, &owner, &ata, Some(&back_pos)).expect("claim backing");
    assert_eq!(
        token_amount(&svm, &ata),
        200_000,
        "same wallet receives both the 10% insurance and 10% backing cohorts"
    );
}

// HEADLINE: all four legacy cohorts in one genesis, one staker each -> each claims its full cohort_supply
// (10/10/40/40). Insurance/backing from subledger capital; LP/trader portfolios from residual counters.
#[test]
fn full_four_way_split_pays_each_cohort_its_share() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64;
    let env = setup(&mut svm, &payer, supply);
    set_slot(&mut svm, 100);

    let (ins, back, lp, trd) = (
        Keypair::new(),
        Keypair::new(),
        Keypair::new(),
        Keypair::new(),
    );
    let ins_pos = Pubkey::new_unique();
    let back_pos = Pubkey::new_unique();
    let lp_pf = Pubkey::new_unique();
    let trd_pf = Pubkey::new_unique();
    // Insurance + backing positions (capital); LP/trader portfolios (residual counters, start 0).
    set_position(
        &mut svm,
        &ins_pos,
        &env.stub_sub,
        &env.ins_pool,
        &ins.pubkey(),
        500,
        false,
    );
    set_position(
        &mut svm,
        &back_pos,
        &env.stub_sub,
        &env.back_pool,
        &back.pubkey(),
        700,
        false,
    );
    set_portfolio(
        &mut svm,
        &lp_pf,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        0,
        0,
    );
    set_portfolio(
        &mut svm,
        &trd_pf,
        &env.stub_perc,
        &env.market,
        &trd.pubkey(),
        0,
        0,
    );

    register(
        &mut svm,
        &payer,
        &env,
        &ins,
        &ins.pubkey(),
        &ins_pos,
        COHORT_INSURANCE,
    )
    .expect("reg ins");
    register(
        &mut svm,
        &payer,
        &env,
        &back,
        &back.pubkey(),
        &back_pos,
        COHORT_BACKING,
    )
    .expect("reg back");
    register(&mut svm, &payer, &env, &lp, &lp.pubkey(), &lp_pf, COHORT_LP).expect("reg lp");
    register(
        &mut svm,
        &payer,
        &env,
        &trd,
        &trd.pubkey(),
        &trd_pf,
        COHORT_TRADER,
    )
    .expect("reg trd");

    // LP absorbs 10_000 residual_received; trader crystallizes 20_000 loss.
    set_slot(&mut svm, 1_500);
    set_portfolio(
        &mut svm,
        &lp_pf,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        10_000,
        0,
    );
    set_portfolio(
        &mut svm,
        &trd_pf,
        &env.stub_perc,
        &env.market,
        &trd.pubkey(),
        0,
        20_000,
    );
    crystallize(&mut svm, &payer, &env, &ins, &ins_pos).expect("cry ins");
    crystallize(&mut svm, &payer, &env, &back, &back_pos).expect("cry back");
    crystallize(&mut svm, &payer, &env, &lp, &lp_pf).expect("cry lp");
    crystallize(&mut svm, &payer, &env, &trd, &trd_pf).expect("cry trd");

    // Freeze after emission_end + finalize_window.
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    // Each cohort has a single staker -> claims the WHOLE cohort_supply (10/10/40/40 of 1_000_000).
    for (owner, linked, cohort, want, is_share) in [
        (&ins, &ins_pos, COHORT_INSURANCE, 100_000u64, true),
        (&back, &back_pos, COHORT_BACKING, 100_000u64, true),
        (&lp, &lp_pf, COHORT_LP, 400_000u64, false),
        (&trd, &trd_pf, COHORT_TRADER, 400_000u64, false),
    ] {
        let ata = create_token_account(&mut svm, &payer, &env.coin_mint, &owner.pubkey());
        claim(
            &mut svm,
            &payer,
            &env,
            &owner,
            &ata,
            if is_share { Some(linked) } else { None },
        )
        .expect("claim");
        assert_eq!(token_amount(&svm, &ata), want, "cohort {cohort} payout");
    }
    // Conservation: total paid == supply (10+10+40+40 = 100%).
    assert_eq!(100_000 + 100_000 + 400_000 + 400_000, supply);
}

// Capital rewards are pro-rata by principal, and the soft veto: an insurance depositor who EXITS (principal -> 0
// at claim) forfeits its COIN even if it had crystallized points; the survivor still claims its own.
#[test]
fn share_value_is_pro_rata_and_exit_forfeits() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64; // insurance cohort = 10% = 100_000
    let env = setup(&mut svm, &payer, supply);
    set_slot(&mut svm, 100);

    let (a, b) = (Keypair::new(), Keypair::new());
    let a_pos = Pubkey::new_unique();
    let b_pos = Pubkey::new_unique();
    set_position(
        &mut svm,
        &a_pos,
        &env.stub_sub,
        &env.ins_pool,
        &a.pubkey(),
        300,
        false,
    );
    set_position(
        &mut svm,
        &b_pos,
        &env.stub_sub,
        &env.ins_pool,
        &b.pubkey(),
        100,
        false,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &a,
        &a.pubkey(),
        &a_pos,
        COHORT_INSURANCE,
    )
    .expect("reg a");
    register(
        &mut svm,
        &payer,
        &env,
        &b,
        &b.pubkey(),
        &b_pos,
        COHORT_INSURANCE,
    )
    .expect("reg b");
    set_slot(&mut svm, 1_124); // equal multiplier 10; pro-rata remains 3:1
    crystallize(&mut svm, &payer, &env, &a, &a_pos).expect("cry a"); // 3_000 pts
    crystallize(&mut svm, &payer, &env, &b, &b_pos).expect("cry b"); // 1_000 pts

    // b EXITS before claim. The frozen 4_000-point denominator remains, so b's term is forfeited.
    set_position(
        &mut svm,
        &b_pos,
        &env.stub_sub,
        &env.ins_pool,
        &b.pubkey(),
        0,
        true,
    );

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    let a_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &a.pubkey());
    let b_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &b.pubkey());
    claim(&mut svm, &payer, &env, &a, &a_ata, Some(&a_pos)).expect("claim a");
    claim(&mut svm, &payer, &env, &b, &b_ata, Some(&b_pos)).expect("claim b");
    // a: 100_000 * 300/400 = 75_000. b: exited -> live principal 0 -> 0 (forfeit; its 25_000 stays in the vault).
    assert_eq!(
        token_amount(&svm, &a_ata),
        75_000,
        "a pro-rata by principal"
    );
    assert_eq!(
        token_amount(&svm, &b_ata),
        0,
        "b exited -> soft-veto forfeit"
    );
}

// ATTACK PROBE: registering dust early must not let capital deposited immediately before
// crystallization inherit the stake account's full tenure. The real subledger resets a Position's
// start_slot on every top-up, so the distributor must use that later clock for capital points.
#[test]
fn share_value_top_up_near_end_cannot_borrow_early_stake_tenure() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64; // insurance cohort = 10% = 100_000
    let env = setup(&mut svm, &payer, supply);
    set_slot(&mut svm, 100);

    let honest = Keypair::new();
    let farmer = Keypair::new();
    let honest_pos = Pubkey::new_unique();
    let farmer_pos = Pubkey::new_unique();
    set_position(
        &mut svm,
        &honest_pos,
        &env.stub_sub,
        &env.ins_pool,
        &honest.pubkey(),
        1_000_000,
        false,
    );
    set_position_start_slot(&mut svm, &honest_pos, 100);
    set_position(
        &mut svm,
        &farmer_pos,
        &env.stub_sub,
        &env.ins_pool,
        &farmer.pubkey(),
        1,
        false,
    );
    set_position_start_slot(&mut svm, &farmer_pos, 100);
    register(
        &mut svm,
        &payer,
        &env,
        &honest,
        &honest.pubkey(),
        &honest_pos,
        COHORT_INSURANCE,
    )
    .expect("register honest");
    register(
        &mut svm,
        &payer,
        &env,
        &farmer,
        &farmer.pubkey(),
        &farmer_pos,
        COHORT_INSURANCE,
    )
    .expect("register farmer");

    set_slot(&mut svm, 1_124);
    set_position(
        &mut svm,
        &farmer_pos,
        &env.stub_sub,
        &env.ins_pool,
        &farmer.pubkey(),
        1_000_000,
        false,
    );
    set_position_start_slot(&mut svm, &farmer_pos, 1_123);
    crystallize(&mut svm, &payer, &env, &honest, &honest_pos).expect("crystallize honest");
    crystallize(&mut svm, &payer, &env, &farmer, &farmer_pos).expect("crystallize farmer");

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");
    let honest_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &honest.pubkey());
    let farmer_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &farmer.pubkey());
    claim(
        &mut svm,
        &payer,
        &env,
        &honest,
        &honest_ata,
        Some(&honest_pos),
    )
    .expect("claim honest");
    claim(
        &mut svm,
        &payer,
        &env,
        &farmer,
        &farmer_ata,
        Some(&farmer_pos),
    )
    .expect("claim farmer");

    assert_eq!(token_amount(&svm, &honest_ata), 100_000);
    assert_eq!(token_amount(&svm, &farmer_ata), 0);
}

// ATTACK PROBE: a post-crystallization partial exit followed by a restoring top-up must not
// resurrect the frozen points. A top-up resets Position.start_slot, so claim must recheck that
// live clock against the slot at which the points were crystallized.
#[test]
fn share_value_post_freeze_top_up_cannot_restore_withdrawn_tenure() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64; // insurance cohort = 10% = 100_000
    let env = setup(&mut svm, &payer, supply);
    set_slot(&mut svm, 100);

    let farmer = Keypair::new();
    let farmer_pos = Pubkey::new_unique();
    set_position(
        &mut svm,
        &farmer_pos,
        &env.stub_sub,
        &env.ins_pool,
        &farmer.pubkey(),
        1_000_000,
        false,
    );
    set_position_start_slot(&mut svm, &farmer_pos, 100);
    register(
        &mut svm,
        &payer,
        &env,
        &farmer,
        &farmer.pubkey(),
        &farmer_pos,
        COHORT_INSURANCE,
    )
    .expect("register farmer");

    set_slot(&mut svm, 1_124);
    crystallize(&mut svm, &payer, &env, &farmer, &farmer_pos).expect("crystallize farmer");
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    // A real partial withdrawal leaves less live principal. A subsequent top-up can restore the
    // principal, but necessarily resets this position clock to the current slot.
    set_position(
        &mut svm,
        &farmer_pos,
        &env.stub_sub,
        &env.ins_pool,
        &farmer.pubkey(),
        1,
        false,
    );
    set_position(
        &mut svm,
        &farmer_pos,
        &env.stub_sub,
        &env.ins_pool,
        &farmer.pubkey(),
        1_000_000,
        false,
    );
    set_position_start_slot(
        &mut svm,
        &farmer_pos,
        env.emission_end + env.finalize_window + 1,
    );

    let farmer_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &farmer.pubkey());
    claim(
        &mut svm,
        &payer,
        &env,
        &farmer,
        &farmer_ata,
        Some(&farmer_pos),
    )
    .expect("claim farmer");
    assert_eq!(
        token_amount(&svm, &farmer_ata),
        0,
        "restoring principal cannot restore the old tenure"
    );
}

// SOFT-VETO PARTIAL DIRECTION (sweep tick D): the exit-forfeit test above covers a FULL exit (principal 0 +
// withdrawn=TRUE). The post-freeze-deposit test covers the inflation cap (live > frozen -> capped at frozen).
// The untested MIDDLE case is a PARTIAL post-freeze withdraw: withdrawn stays FALSE, principal drops but stays
// non-zero -> the live-cap path with a value strictly between 0 and
// frozen. The claim min-cap must then pay the LIVE reduced amount, so a
// depositor that de-risks half its capital after freeze claims half its COIN; the rest stays locked (the genuine
// partial soft-veto). Pins that the min-cap pays `live` (not the frozen snapshot, not 0) on a partial reduction.
#[test]
fn share_value_claim_partial_post_freeze_withdraw_pays_the_reduced_live_shares() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64; // insurance cohort = 10% = 100_000
    let env = setup(&mut svm, &payer, supply);
    set_slot(&mut svm, 100);

    let a = Keypair::new();
    let a_pos = Pubkey::new_unique();
    set_position(
        &mut svm,
        &a_pos,
        &env.stub_sub,
        &env.ins_pool,
        &a.pubkey(),
        300,
        false,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &a,
        &a.pubkey(),
        &a_pos,
        COHORT_INSURANCE,
    )
    .expect("reg a");
    set_slot(&mut svm, 1_124);
    crystallize(&mut svm, &payer, &env, &a, &a_pos).expect("cry a"); // frozen points = 3_000

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    // PARTIAL post-freeze withdraw: principal 300 -> 150, still live (withdrawn=false).
    set_position(
        &mut svm,
        &a_pos,
        &env.stub_sub,
        &env.ins_pool,
        &a.pubkey(),
        150,
        false,
    );

    let a_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &a.pubkey());
    claim(&mut svm, &payer, &env, &a, &a_ata, Some(&a_pos)).expect("claim a");
    // The common multiplier cancels: 100_000 * (10*150)/(10*300) = 50_000.
    assert_eq!(
        token_amount(&svm, &a_ata),
        50_000,
        "partial post-freeze withdraw pays the REDUCED live principal, not the frozen snapshot"
    );
    assert_eq!(
        token_amount(&svm, &env.vault),
        supply - 50_000,
        "the de-risked half stays locked in the vault (partial soft-veto)"
    );
}

// A permissionless terminal return is not an owner soft-veto: it must preserve
// frozen rewards for the capital that was still at risk when the cranker returned
// it. The terminal snapshot must not restore capital the owner withdrew earlier.
#[test]
fn terminal_return_preserves_only_the_remaining_frozen_capital_reward() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64;
    let env = setup(&mut svm, &payer, supply);
    set_slot(&mut svm, 100);

    let owner = Keypair::new();
    let position = Pubkey::new_unique();
    set_position(
        &mut svm,
        &position,
        &env.stub_sub,
        &env.ins_pool,
        &owner.pubkey(),
        300,
        false,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &owner,
        &owner.pubkey(),
        &position,
        COHORT_INSURANCE,
    )
    .expect("register capital");
    set_slot(&mut svm, 1_124);
    crystallize(&mut svm, &payer, &env, &owner, &position)
        .expect("crystallize 300 principal");
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    // The owner previously reduced 300 to 150; terminal cleanup then returned
    // that remaining 150 and retired the position.
    let mut terminal = svm.get_account(&position).unwrap();
    terminal.data[72..80].copy_from_slice(&0u64.to_le_bytes());
    terminal.data[80..88].copy_from_slice(&150u64.to_le_bytes());
    terminal.data[88] = 1;
    terminal.data[98] = 1;
    terminal.data[104..120].copy_from_slice(&0u128.to_le_bytes());
    svm.set_account(position, terminal).unwrap();

    let recipient = create_token_account(&mut svm, &payer, &env.coin_mint, &owner.pubkey());
    claim(
        &mut svm,
        &payer,
        &env,
        &owner,
        &recipient,
        Some(&position),
    )
    .expect("claim terminally returned capital reward");
    assert_eq!(
        token_amount(&svm, &recipient),
        50_000,
        "the terminal snapshot preserves the remaining half, not the withdrawn half"
    );
}

// ATTACK PROBE (post-freeze capital inflation): the insurance/backing claim pays
// cohort_supply * min(frozen_points, live tenure*principal) / frozen_denominator.
// The exit direction (live < frozen -> forfeit) is pinned by share_value_is_pro_rata_and_exit_forfeits. The
// UPPER direction is the over-draw vector: the cohort supply is FIXED and the denominator is FROZEN, so if the
// claim used LIVE (not min) points, a claimant who TOPS UP their subledger position AFTER freeze (live principal
// >> frozen) would mint a numerator far above their frozen contribution against the frozen denominator —
// claiming more than their share, draining the fixed cohort supply and diluting honest claimants. The min-cap
// blocks it: a post-freeze deposit can never raise the payout above the frozen-time contribution.
#[test]
fn share_value_claim_caps_at_frozen_points_post_freeze_deposit_cannot_inflate() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64; // insurance cohort = 10% = 100_000
    let env = setup(&mut svm, &payer, supply);
    set_slot(&mut svm, 100);

    let (a, b) = (Keypair::new(), Keypair::new());
    let a_pos = Pubkey::new_unique();
    let b_pos = Pubkey::new_unique();
    set_position(
        &mut svm,
        &a_pos,
        &env.stub_sub,
        &env.ins_pool,
        &a.pubkey(),
        300,
        false,
    );
    set_position(
        &mut svm,
        &b_pos,
        &env.stub_sub,
        &env.ins_pool,
        &b.pubkey(),
        100,
        false,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &a,
        &a.pubkey(),
        &a_pos,
        COHORT_INSURANCE,
    )
    .expect("reg a");
    register(
        &mut svm,
        &payer,
        &env,
        &b,
        &b.pubkey(),
        &b_pos,
        COHORT_INSURANCE,
    )
    .expect("reg b");
    set_slot(&mut svm, 1_124); // equal multiplier 10; frozen denominator = 4_000
    crystallize(&mut svm, &payer, &env, &a, &a_pos).expect("cry a");
    crystallize(&mut svm, &payer, &env, &b, &b_pos).expect("cry b");

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze"); // denominator frozen at 4_000

    // ATTACK: AFTER freeze, b tops up their subledger position 100x (100 -> 10_000 live principal), trying to
    // inflate its numerator above the frozen denominator.
    set_position(
        &mut svm,
        &b_pos,
        &env.stub_sub,
        &env.ins_pool,
        &b.pubkey(),
        10_000,
        false,
    );
    set_position_start_slot(&mut svm, &b_pos, env.emission_end + env.finalize_window + 1);

    let a_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &a.pubkey());
    let b_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &b.pubkey());
    claim(&mut svm, &payer, &env, &a, &a_ata, Some(&a_pos)).expect("claim a");
    claim(&mut svm, &payer, &env, &b, &b_ata, Some(&b_pos)).expect("claim b");
    // The top-up reset b's clock after crystallization, so it cannot inherit the frozen tenure.
    assert_eq!(
        token_amount(&svm, &b_ata),
        0,
        "post-freeze top-up cannot inherit or inflate frozen tenure"
    );
    assert_eq!(
        token_amount(&svm, &a_ata),
        75_000,
        "a unaffected: 100_000 * 300/400"
    );
    // Conservation: the fixed cohort supply is not over-drawn by the inflation attempt.
    assert_eq!(
        token_amount(&svm, &a_ata) + token_amount(&svm, &b_ata),
        75_000,
        "unearned rewards remain in the vault; no over-draw"
    );
}

// ATTACK PROBE (soft-veto bypass via a SUBSTITUTED position at claim). A capital (insurance/backing) claim
// caps the payout by LIVE principal read from a position account passed at claim time; the soft veto rests on
// that being the OWNER'S OWN bound position (so exiting it really forfeits). claim binds position.key ==
// stake.backing_ledger (src:902). Without it, an owner who EXITED their bound position (live principal 0 -> should
// forfeit) could pass a DIFFERENT high-share position to read a high live_pts -> min(frozen, high) = frozen ->
// claim the FULL COIN while their capital is no longer at risk — defeating the soft veto entirely. None of the
// capital tests pass a substituted position; this pins the bind. Real rd .so.
#[test]
fn share_value_claim_rejects_a_substituted_position_no_soft_veto_bypass() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64; // insurance cohort = 10% = 100_000
    let env = setup(&mut svm, &payer, supply);
    set_slot(&mut svm, 100);

    // a (the attacker-owner) and b (honest) both register insurance stakes; a=300, b=100 -> frozen denom 400.
    let (a, b) = (Keypair::new(), Keypair::new());
    let a_pos = Pubkey::new_unique();
    let b_pos = Pubkey::new_unique();
    set_position(
        &mut svm,
        &a_pos,
        &env.stub_sub,
        &env.ins_pool,
        &a.pubkey(),
        300,
        false,
    );
    set_position(
        &mut svm,
        &b_pos,
        &env.stub_sub,
        &env.ins_pool,
        &b.pubkey(),
        100,
        false,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &a,
        &a.pubkey(),
        &a_pos,
        COHORT_INSURANCE,
    )
    .expect("reg a");
    register(
        &mut svm,
        &payer,
        &env,
        &b,
        &b.pubkey(),
        &b_pos,
        COHORT_INSURANCE,
    )
    .expect("reg b");
    set_slot(&mut svm, 1_124);
    crystallize(&mut svm, &payer, &env, &a, &a_pos).expect("cry a"); // 3_000 pts
    crystallize(&mut svm, &payer, &env, &b, &b_pos).expect("cry b"); // 1_000 pts

    // a EXITS its bound position (live principal 0) — the soft veto must now forfeit a's claim.
    set_position(
        &mut svm,
        &a_pos,
        &env.stub_sub,
        &env.ins_pool,
        &a.pubkey(),
        0,
        true,
    );
    // A decoy position with HIGH live principal (subledger-owned so it passes the program-owner check) that a
    // will try to substitute to read a high live_pts and dodge the forfeit.
    let decoy_pos = Pubkey::new_unique();
    set_position(
        &mut svm,
        &decoy_pos,
        &env.stub_sub,
        &env.ins_pool,
        &a.pubkey(),
        9_999,
        false,
    );

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    let a_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &a.pubkey());
    // ATTACK: a claims with a 9_999-principal DECOY instead of its own empty bound position.
    assert!(claim(&mut svm, &payer, &env, &a, &a_ata, Some(&decoy_pos)).is_err(),
        "a substituted position is rejected (position.key != stake.backing_ledger) — no soft-veto bypass");
    assert_eq!(
        token_amount(&svm, &a_ata),
        0,
        "the substituted-position claim paid nothing"
    );

    // (control) claiming with the CORRECT bound (now-empty) position pays 0 — the soft veto forfeits, as designed.
    claim(&mut svm, &payer, &env, &a, &a_ata, Some(&a_pos)).expect("claim with the bound position");
    assert_eq!(
        token_amount(&svm, &a_ata),
        0,
        "a exited its capital -> soft-veto forfeit, 0 COIN"
    );
    // The honest staker b still claims its full 100/400 share with its own intact position.
    let b_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &b.pubkey());
    claim(&mut svm, &payer, &env, &b, &b_ata, Some(&b_pos)).expect("b claims");
    assert_eq!(
        token_amount(&svm, &b_ata),
        25_000,
        "b gets its honest 100_000 * 100/400"
    );
}

// ATTACK PROBE (denominator inflation via a SUBSTITUTED ledger at CRYSTALLIZE). crystallize for a capital
// (insurance/backing) stake OVERWRITES stake.points from LIVE principal in the passed ledger AND updates the
// cohort denominator (subtract-old/add-new). It binds backing_ledger == stake.backing_ledger (src:726). This
// is the crystallize-side complement of the claim-side position bind (902): without 726, an owner could
// crystallize a DECOY high-share ledger to push their points (and the frozen cohort denominator) far above
// their real bound position — DILUTING every honest claimant (payout = supply * pts / inflated_denom). The
// claim-side cap (902) would still bound the ATTACKER'S own payout, but the inflated DENOMINATOR has already
// shrunk everyone else's share. 726 keeps the denominator honest. None of the crystallize tests substitute the
// ledger; this pins it. Real rd .so.
#[test]
fn crystallize_rejects_a_substituted_ledger_no_denominator_inflation() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64; // insurance cohort = 10% = 100_000
    let env = setup(&mut svm, &payer, supply);
    set_slot(&mut svm, 100);

    let (a, b) = (Keypair::new(), Keypair::new());
    let a_pos = Pubkey::new_unique();
    let b_pos = Pubkey::new_unique();
    set_position(
        &mut svm,
        &a_pos,
        &env.stub_sub,
        &env.ins_pool,
        &a.pubkey(),
        100,
        false,
    );
    set_position(
        &mut svm,
        &b_pos,
        &env.stub_sub,
        &env.ins_pool,
        &b.pubkey(),
        100,
        false,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &a,
        &a.pubkey(),
        &a_pos,
        COHORT_INSURANCE,
    )
    .expect("reg a");
    register(
        &mut svm,
        &payer,
        &env,
        &b,
        &b.pubkey(),
        &b_pos,
        COHORT_INSURANCE,
    )
    .expect("reg b");

    set_slot(&mut svm, 1_124);
    // A decoy position with HIGH live principal (subledger-owned), which a will try to crystallize INSTEAD of
    // its bound a_pos to inflate its points + the cohort denominator.
    let decoy = Pubkey::new_unique();
    set_position(
        &mut svm,
        &decoy,
        &env.stub_sub,
        &env.ins_pool,
        &a.pubkey(),
        9_999,
        false,
    );
    assert!(
        crystallize(&mut svm, &payer, &env, &a, &decoy).is_err(),
        "a substituted ledger at crystallize is rejected (backing_ledger != stake.backing_ledger)"
    );

    // Honest crystallize of the bound ledgers -> 10 * (100 + 100) = 2_000 points.
    crystallize(&mut svm, &payer, &env, &a, &a_pos).expect("a crystallizes its bound ledger");
    crystallize(&mut svm, &payer, &env, &b, &b_pos).expect("b crystallizes its bound ledger");
    let denom = u128::from_le_bytes(
        svm.get_account(&env.rd_config).unwrap().data[174..190]
            .try_into()
            .unwrap(),
    );
    assert_eq!(
        denom, 2_000,
        "insurance denominator reflects the real bound principal, not the 9_999 decoy"
    );

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    // Both claim an UNDILUTED 50/50: 100_000 * 100/200 = 50_000 each. A decoy-inflated denominator (10_099)
    // would have starved b to ~990 — the bind prevents that.
    let a_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &a.pubkey());
    let b_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &b.pubkey());
    claim(&mut svm, &payer, &env, &a, &a_ata, Some(&a_pos)).expect("a claims");
    claim(&mut svm, &payer, &env, &b, &b_ata, Some(&b_pos)).expect("b claims");
    assert_eq!(
        token_amount(&svm, &a_ata),
        50_000,
        "a gets its honest half — no self-inflation"
    );
    assert_eq!(
        token_amount(&svm, &b_ata),
        50_000,
        "b is NOT diluted by a phantom decoy in the denominator"
    );
}

// finding KM: a capital claim must be authorized by the stake's OWN owner. claim caps the payout by
// LIVE principal, so a permissionless trigger would let an attacker force the victim's claim during a
// transient low-principal moment (mid partial-withdraw: withdrawn=false, principal reduced) and the irreversible
// claimed-flag would lock in the reduced payout. Here the attacker's forced claim is rejected, so the
// victim re-deposits and claims their FULL share themselves.
#[test]
fn share_value_claim_cannot_be_forced_by_a_third_party_at_a_low_share_moment() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64; // insurance cohort = 10% = 100_000
    let env = setup(&mut svm, &payer, supply);
    set_slot(&mut svm, 100);

    let victim = Keypair::new();
    let attacker = Keypair::new();
    let pos = Pubkey::new_unique();
    set_position(
        &mut svm,
        &pos,
        &env.stub_sub,
        &env.ins_pool,
        &victim.pubkey(),
        300,
        false,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &victim,
        &victim.pubkey(),
        &pos,
        COHORT_INSURANCE,
    )
    .expect("reg");
    set_slot(&mut svm, 1_124);
    crystallize(&mut svm, &payer, &env, &victim, &pos).expect("cry"); // 3_000 pts
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    // victim has partially withdrawn: still live (withdrawn=false), but principal is now 30.
    set_position(
        &mut svm,
        &pos,
        &env.stub_sub,
        &env.ins_pool,
        &victim.pubkey(),
        30,
        false,
    );
    let ata = create_token_account(&mut svm, &payer, &env.coin_mint, &victim.pubkey());
    // the attacker cannot force the victim's claim at the low-principal moment.
    assert!(
        claim_as(
            &mut svm,
            &payer,
            &env,
            &attacker,
            &victim.pubkey(),
            &ata,
            Some(&pos)
        )
        .is_err(),
        "a third party must not be able to force a capital claim"
    );
    assert_eq!(
        token_amount(&svm, &ata),
        0,
        "nothing was paid out by the forced attempt"
    );

    // the victim re-deposits to full principal and claims THEMSELVES -> full 100_000 (grief avoided).
    set_position(
        &mut svm,
        &pos,
        &env.stub_sub,
        &env.ins_pool,
        &victim.pubkey(),
        300,
        false,
    );
    claim(&mut svm, &payer, &env, &victim, &ata, Some(&pos)).expect("owner claims their own");
    assert_eq!(
        token_amount(&svm, &ata),
        100_000,
        "victim claims their full pro-rata share"
    );
}

// finding KO (KM parity, one step earlier): crystallize OVERWRITES a capital stake's points from the
// live principal NOW, and freeze locks that as the frozen denominator term — which the claim-time min-cap can
// only lower, never raise. So a permissionless crystallize would let an attacker force a victim's points
// down at a transient low-principal moment, then freeze to lock it. crystallize for capital cohorts is
// therefore owner-gated. Here the attacker's forced crystallize is rejected; the owner re-crystallizes at
// full principal and claims their full share.
#[test]
fn share_value_crystallize_cannot_be_forced_by_a_third_party_at_a_low_share_moment() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64; // insurance cohort = 10% = 100_000
    let env = setup(&mut svm, &payer, supply);
    set_slot(&mut svm, 100);

    let victim = Keypair::new();
    let attacker = Keypair::new();
    let pos = Pubkey::new_unique();
    set_position(
        &mut svm,
        &pos,
        &env.stub_sub,
        &env.ins_pool,
        &victim.pubkey(),
        300,
        false,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &victim,
        &victim.pubkey(),
        &pos,
        COHORT_INSURANCE,
    )
    .expect("reg");
    set_slot(&mut svm, 1_124);
    crystallize(&mut svm, &payer, &env, &victim, &pos).expect("owner crystallizes at 3_000");

    // victim mid partial-withdraw -> live principal transiently 30. The attacker tries to force the points down.
    set_position(
        &mut svm,
        &pos,
        &env.stub_sub,
        &env.ins_pool,
        &victim.pubkey(),
        30,
        false,
    );
    assert!(
        crystallize_as(&mut svm, &payer, &env, &attacker, &victim.pubkey(), &pos).is_err(),
        "a third party must not be able to force a capital crystallize"
    );

    // victim restores principal and genesis freezes; the victim claims the full 100_000 (grief avoided).
    set_position(
        &mut svm,
        &pos,
        &env.stub_sub,
        &env.ins_pool,
        &victim.pubkey(),
        300,
        false,
    );
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");
    let ata = create_token_account(&mut svm, &payer, &env.coin_mint, &victim.pubkey());
    claim(&mut svm, &payer, &env, &victim, &ata, Some(&pos)).expect("owner claims");
    assert_eq!(
        token_amount(&svm, &ata),
        100_000,
        "victim's points were not force-lowered"
    );
}

// REGISTER binds the linked account's owner (finding GY): a foreign signer cannot register against
// someone else's position/portfolio, and a position from a foreign pool is rejected (finding HG).
#[test]
fn register_rejects_foreign_owner_and_foreign_pool() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);

    let victim = Keypair::new();
    let attacker = Keypair::new();
    let pos = Pubkey::new_unique();
    set_position(
        &mut svm,
        &pos,
        &env.stub_sub,
        &env.ins_pool,
        &victim.pubkey(),
        500,
        false,
    );
    // attacker signs but the position.owner is the victim -> rejected.
    assert!(
        register(
            &mut svm,
            &payer,
            &env,
            &attacker,
            &attacker.pubkey(),
            &pos,
            COHORT_INSURANCE
        )
        .is_err(),
        "foreign owner must be rejected"
    );

    // a position in a FOREIGN pool (not the genesis insurance pool) -> rejected even for the owner.
    let foreign_pool = Pubkey::new_unique();
    let pos2 = Pubkey::new_unique();
    set_position(
        &mut svm,
        &pos2,
        &env.stub_sub,
        &foreign_pool,
        &victim.pubkey(),
        500,
        false,
    );
    assert!(
        register(
            &mut svm,
            &payer,
            &env,
            &victim,
            &victim.pubkey(),
            &pos2,
            COHORT_INSURANCE
        )
        .is_err(),
        "foreign pool must be rejected"
    );
}

// ATTACK PROBE (cross-cohort pool-scope confusion: insurance position farms the backing cohort, or vice
// versa). The insurance and backing cohorts have SEPARATE supplies and are scoped to DIFFERENT genesis pools:
// register requires position.pool == config.subledger_pool for COHORT_INSURANCE and == config.backing_pool for
// COHORT_BACKING (src:register_start, finding HG). If the scope used the wrong pool, an insurance depositor
// could register their position under the BACKING cohort (claiming the backing supply they never backed) and
// the backing cohort's denominator would be diluted by insurance positions (and symmetrically). The existing
// foreign-pool test uses a RANDOM pool (neither genesis pool) — the cross-GENESIS-pool swap (a real ins
// position declared backing, a real backing position declared insurance) was untested. Real rd .so.
#[test]
fn register_rejects_cross_cohort_pool_scope_insurance_vs_backing() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000); // insurance + backing cohorts both active (10% each)
    set_slot(&mut svm, 100);

    let (i_owner, b_owner) = (Keypair::new(), Keypair::new());
    let i_pos = Pubkey::new_unique(); // a real INSURANCE-pool position
    let b_pos = Pubkey::new_unique(); // a real BACKING-pool position
    set_position(
        &mut svm,
        &i_pos,
        &env.stub_sub,
        &env.ins_pool,
        &i_owner.pubkey(),
        500,
        false,
    );
    set_position(
        &mut svm,
        &b_pos,
        &env.stub_sub,
        &env.back_pool,
        &b_owner.pubkey(),
        500,
        false,
    );

    // (1) An insurance-pool position declared under the BACKING cohort -> pool (ins) != backing scope -> reject.
    assert!(
        register(
            &mut svm,
            &payer,
            &env,
            &i_owner,
            &i_owner.pubkey(),
            &i_pos,
            COHORT_BACKING
        )
        .is_err(),
        "an insurance-pool position cannot farm the backing cohort"
    );
    // (2) A backing-pool position declared under the INSURANCE cohort -> pool (back) != insurance scope -> reject.
    assert!(
        register(
            &mut svm,
            &payer,
            &env,
            &b_owner,
            &b_owner.pubkey(),
            &b_pos,
            COHORT_INSURANCE
        )
        .is_err(),
        "a backing-pool position cannot farm the insurance cohort"
    );
    // (control) Each position registers fine under its OWN cohort — the gate is the scope, not the owner/pool.
    register(
        &mut svm,
        &payer,
        &env,
        &i_owner,
        &i_owner.pubkey(),
        &i_pos,
        COHORT_INSURANCE,
    )
    .expect("ins pos -> ins cohort ok");
    register(
        &mut svm,
        &payer,
        &env,
        &b_owner,
        &b_owner.pubkey(),
        &b_pos,
        COHORT_BACKING,
    )
    .expect("back pos -> back cohort ok");
}

// finding IL: the LP/trader cohorts must be scoped to the ONE allow-listed (trusted-Pyth) genesis
// market. An attacker who stands up their OWN percolator market with an oracle they control can
// wash-trade to mint crystallized_loss/received at will; here that portfolio belongs to a FOREIGN
// market_group, so register rejects it for both cohorts even though the attacker owns it and the
// counters are non-zero. The same attacker's portfolio in the genesis market would register fine.
#[test]
fn register_rejects_portfolio_from_a_foreign_market() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);

    let attacker = Keypair::new();
    let foreign_market = Pubkey::new_unique(); // attacker's own market, oracle they control
                                               // attacker owns the portfolio and has manufactured a fat loss/receipt — but in a foreign market.
    let evil_pf = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &evil_pf,
        &env.stub_perc,
        &foreign_market,
        &attacker.pubkey(),
        9_000_000,
        9_000_000,
    );
    assert!(
        register(
            &mut svm,
            &payer,
            &env,
            &attacker,
            &attacker.pubkey(),
            &evil_pf,
            COHORT_TRADER
        )
        .is_err(),
        "trader cohort: a portfolio from a foreign (attacker-oracle'd) market must be rejected"
    );
    assert!(
        register(
            &mut svm,
            &payer,
            &env,
            &attacker,
            &attacker.pubkey(),
            &evil_pf,
            COHORT_LP
        )
        .is_err(),
        "lp cohort: a portfolio from a foreign market must be rejected"
    );

    // the SAME attacker, but a portfolio in the genesis (allow-listed) market -> accepted.
    let good_pf = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &good_pf,
        &env.stub_perc,
        &env.market,
        &attacker.pubkey(),
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &attacker,
        &attacker.pubkey(),
        &good_pf,
        COHORT_TRADER,
    )
    .expect("a portfolio in the allow-listed genesis market registers");
}

// ATTACK PROBE (LP/trader residual double-count / point-theft via foreign-portfolio register): the LP/trader
// cohorts bind the linked percolator PortfolioAccount to its OWNER (src:660 — OFF_PORTFOLIO_OWNER == signing
// owner). Without it, a second party B could register VICTIM A's portfolio (in the allow-listed market, real
// residual R) under B's own per-owner stake naming B recipient — crediting B points for residual B never
// generated. Because A also registers P, the SAME R would be counted TWICE in the cohort denominator, and the
// pair (A,B) — if one actor — would capture 2R/(2R+H) of the cohort instead of the fair R/(R+H). It is also a
// straight point-THEFT (B claims COIN off A's loss). The owner bind blocks it: P counts exactly once, under its
// true owner. This complements register_rejects_foreign_owner (which only covers the INSURANCE position bind,
// src:637) and register_rejects_portfolio_from_a_foreign_market (where the attacker owns the portfolio, so 660
// passes and the MARKET check does the rejecting). Proven against the real rd .so.
#[test]
fn register_lp_trader_binds_portfolio_to_its_owner_no_double_count() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);

    let victim = Keypair::new();
    let attacker = Keypair::new();
    // victim owns a portfolio in the ALLOW-LISTED genesis market with real residual (received & crystallized).
    let pf = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &victim.pubkey(),
        7_000,
        7_000,
    );

    // attacker signs as owner=attacker, naming the VICTIM's portfolio — the market is allow-listed, so the
    // ONLY thing that can reject is the portfolio-owner bind (660). Both cohorts must reject.
    assert!(
        register(
            &mut svm,
            &payer,
            &env,
            &attacker,
            &attacker.pubkey(),
            &pf,
            COHORT_LP
        )
        .is_err(),
        "LP: a non-owner cannot register the victim's portfolio (no double-count)"
    );
    assert!(
        register(
            &mut svm,
            &payer,
            &env,
            &attacker,
            &attacker.pubkey(),
            &pf,
            COHORT_TRADER
        )
        .is_err(),
        "trader: a non-owner cannot register the victim's portfolio (no point-theft)"
    );

    // The rightful owner registers it once (control) — P's residual counts exactly once, under its true owner.
    register(
        &mut svm,
        &payer,
        &env,
        &victim,
        &victim.pubkey(),
        &pf,
        COHORT_LP,
    )
    .expect("owner registers P");
    // And the attacker STILL cannot register P (its owner is the victim) — no second crediting of the same R.
    assert!(register(&mut svm, &payer, &env, &attacker, &attacker.pubkey(), &pf, COHORT_LP).is_err(),
        "even after the owner registers, a non-owner cannot re-register P to double-count its residual");
}

// DILUTION/STRAND PROBE (finding IK: default-pubkey recipient, sweep tick D): the register helpers above pin the
// GY owner-sign guard; the sibling IK guard (lib.rs:651) rejects a register whose COIN recipient is the zero
// pubkey. Such a stake would still accrue points and land in the FROZEN cohort denominator, but its claim could
// never pay out — the claim requires recipient_ata.owner == stake.recipient and nobody owns Pubkey::default() —
// so its share would sit locked forever, diluting every honest claimant in the cohort (their points/denom share
// shrinks by the dead stake's weight). Pin that a default recipient is refused up front.
#[test]
fn register_rejects_a_default_pubkey_recipient_no_unclaimable_denominator_polluting_stake() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);

    let owner = Keypair::new();
    let pf = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &owner.pubkey(),
        0,
        0,
    );

    // recipient = Pubkey::default() -> rejected at register (the guard fires BEFORE the stake PDA is created).
    assert!(register(&mut svm, &payer, &env, &owner, &Pubkey::default(), &pf, COHORT_LP).is_err(),
        "a default-pubkey recipient must be rejected (would be an unclaimable, denominator-polluting stake)");
    // The PDA is still free, so a register with a REAL recipient succeeds (the rejected attempt squatted nothing).
    register(
        &mut svm,
        &payer,
        &env,
        &owner,
        &owner.pubkey(),
        &pf,
        COHORT_LP,
    )
    .expect("a real recipient registers cleanly");
}

// LP/trader points are the Δ of the monotonic residual counter since register; claim is frozen-final
// (no live cap account), and double-claim is rejected.
#[test]
fn lp_residual_delta_and_double_claim_rejected() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64; // lp cohort = 40% = 400_000
    let env = setup(&mut svm, &payer, supply);
    set_slot(&mut svm, 100);

    let lp = Keypair::new();
    let pf = Pubkey::new_unique();
    // register at received=5_000 (pre-existing); only the Δ after register should count.
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        5_000,
        0,
    );
    register(&mut svm, &payer, &env, &lp, &lp.pubkey(), &pf, COHORT_LP).expect("reg lp");
    set_slot(&mut svm, 1_500);
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        12_000,
        0,
    ); // Δ = 7_000
    crystallize(&mut svm, &payer, &env, &lp, &pf).expect("cry lp");
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    let ata = create_token_account(&mut svm, &payer, &env.coin_mint, &lp.pubkey());
    claim(&mut svm, &payer, &env, &lp, &ata, None).expect("claim lp");
    // sole LP staker -> whole cohort supply regardless of the absolute Δ.
    assert_eq!(
        token_amount(&svm, &ata),
        400_000,
        "sole LP claims the LP cohort supply"
    );
    // double-claim rejected.
    assert!(
        claim(&mut svm, &payer, &env, &lp, &ata, None).is_err(),
        "double-claim must reject"
    );
}

// UPGRADE-INDUCED REWARD VAULT LOCK: continuous epochs appended 626 bytes to the
// legacy genesis config, growing it from 823 to 1,449 bytes without changing the
// legacy config PDA or the already-current 211-byte stake. Rejecting that exact
// pre-epoch size bricks register/crystallize/freeze/claim after program upgrade.
#[test]
fn pre_epoch_config_completes_register_crystallize_freeze_and_claim() {
    const PRE_EPOCH_CONFIG_SIZE: usize = 823;

    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64;
    let env = setup(&mut svm, &payer, supply);

    let current_config = svm.get_account(&env.rd_config).unwrap();
    assert_eq!(current_config.data.len(), 1_449, "current config fixture size");
    let lp = Keypair::new();
    let portfolio = Pubkey::new_unique();
    set_slot(&mut svm, 100);
    set_portfolio(
        &mut svm,
        &portfolio,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        5_000,
        0,
    );

    let mut unsupported_partial = current_config.clone();
    unsupported_partial.data.truncate(PRE_EPOCH_CONFIG_SIZE + 1);
    svm.set_account(env.rd_config, unsupported_partial).unwrap();
    assert!(
        register(
            &mut svm,
            &payer,
            &env,
            &lp,
            &lp.pubkey(),
            &portfolio,
            COHORT_LP,
        )
        .is_err(),
        "unknown partial layouts stay rejected"
    );
    assert!(
        svm.get_account(&stake_pda_for_cohort(
            &env,
            &lp.pubkey(),
            &portfolio,
            COHORT_LP,
        ))
        .is_none(),
        "rejected partial config created no stake"
    );

    let mut historical_config = current_config;
    historical_config.data.truncate(PRE_EPOCH_CONFIG_SIZE);
    svm.set_account(env.rd_config, historical_config).unwrap();
    // The two predecessor addresses are required as absence proofs. A third party can transfer
    // lamports to either PDA, so data-empty prefunding must not become a new registration DoS.
    let legacy_owner_stake = Pubkey::find_program_address(
        &[b"rd_stake", env.rd_config.as_ref(), lp.pubkey().as_ref()],
        &rd_id(),
    )
    .0;
    let legacy_linked_stake = Pubkey::find_program_address(
        &[
            b"rd_stake",
            env.rd_config.as_ref(),
            lp.pubkey().as_ref(),
            portfolio.as_ref(),
        ],
        &rd_id(),
    )
    .0;
    svm.airdrop(&legacy_owner_stake, 1).unwrap();
    svm.airdrop(&legacy_linked_stake, 1).unwrap();
    register(
        &mut svm,
        &payer,
        &env,
        &lp,
        &lp.pubkey(),
        &portfolio,
        COHORT_LP,
    )
    .expect("pre-epoch register remains live");

    set_slot(&mut svm, 1_500);
    set_portfolio(
        &mut svm,
        &portfolio,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        12_000,
        0,
    );
    crystallize(&mut svm, &payer, &env, &lp, &portfolio)
        .expect("pre-epoch crystallize remains live");
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("pre-epoch freeze remains live");
    assert_eq!(
        svm.get_account(&env.rd_config).unwrap().data.len(),
        PRE_EPOCH_CONFIG_SIZE,
        "compatibility never reallocates historical state"
    );

    let recipient = create_token_account(&mut svm, &payer, &env.coin_mint, &lp.pubkey());
    claim(&mut svm, &payer, &env, &lp, &recipient, None)
        .expect("pre-epoch claimant receives the frozen LP cohort");
    assert_eq!(token_amount(&svm, &recipient), 400_000, "sole LP receives the 40% cohort");
    assert_eq!(token_amount(&svm, &env.vault), 600_000, "only the earned reward leaves the vault");
    assert!(
        claim(&mut svm, &payer, &env, &lp, &recipient, None).is_err(),
        "historical stake remains one-shot"
    );
}

fn exercise_historical_frozen_stake_claim(linked_seed: bool, label: &str) {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000);

    set_slot(&mut svm, 100);
    let lp = Keypair::new();
    let portfolio = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &portfolio,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        5_000,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &lp,
        &lp.pubkey(),
        &portfolio,
        COHORT_LP,
    )
    .expect("register current fixture stake");
    set_slot(&mut svm, 1_500);
    set_portfolio(
        &mut svm,
        &portfolio,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        12_000,
        0,
    );
    crystallize(&mut svm, &payer, &env, &lp, &portfolio).expect("crystallize");

    let current_stake = stake_pda_for_cohort(&env, &lp.pubkey(), &portfolio, COHORT_LP);
    let mut historical_stake = svm.get_account(&current_stake).unwrap();
    let (historical_stake_key, historical_bump) = if linked_seed {
        Pubkey::find_program_address(
            &[
                b"rd_stake",
                env.rd_config.as_ref(),
                lp.pubkey().as_ref(),
                portfolio.as_ref(),
            ],
            &rd_id(),
        )
    } else {
        Pubkey::find_program_address(
            &[b"rd_stake", env.rd_config.as_ref(), lp.pubkey().as_ref()],
            &rd_id(),
        )
    };
    historical_stake.data[192] = historical_bump;
    svm.set_account(historical_stake_key, historical_stake).unwrap();
    svm.set_account(
        current_stake,
        Account {
            lamports: 0,
            data: vec![],
            owner: solana_sdk::system_program::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();

    // These stake schemas predate the continuous-epoch tail and therefore pair
    // with the exact 823-byte config restored by the preceding stacked fix.
    let mut historical_config = svm.get_account(&env.rd_config).unwrap();
    historical_config.data.truncate(823);
    svm.set_account(env.rd_config, historical_config).unwrap();

    let legacy_crystallize = Instruction {
        program_id: rd_id(),
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new(env.rd_config, false),
            AccountMeta::new(historical_stake_key, false),
            AccountMeta::new_readonly(portfolio, false),
        ],
        data: vec![2u8],
    };
    assert!(
        send(&mut svm, &payer, &[legacy_crystallize], &[]).is_err(),
        "{label}: historical seed cannot re-enter point accrual"
    );
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze denominators");

    let recipient = create_token_account(&mut svm, &payer, &env.coin_mint, &lp.pubkey());
    let archive = portfolio_archive_pda(&svm, &env, &lp.pubkey(), &portfolio);
    let claim_ix = Instruction {
        program_id: rd_id(),
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(env.rd_config, false),
            AccountMeta::new(historical_stake_key, false),
            AccountMeta::new(env.vault, false),
            AccountMeta::new(recipient, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(portfolio, false),
            AccountMeta::new_readonly(archive, false),
            AccountMeta::new_readonly(
                retired_market_pda(&env.stub_perc, &env.market),
                false,
            ),
        ],
        data: vec![5u8],
    };
    send(&mut svm, &payer, &[claim_ix.clone()], &[])
        .unwrap_or_else(|e| panic!("{label}: frozen historical stake can claim: {e}"));
    assert_eq!(token_amount(&svm, &recipient), 400_000, "{label}: exact LP cohort paid");
    assert_eq!(
        svm.get_account(&historical_stake_key).unwrap().data[210],
        1,
        "{label}: historical stake consumed"
    );
    assert!(
        send(&mut svm, &payer, &[claim_ix], &[]).is_err(),
        "{label}: historical stake cannot replay"
    );
}

// Frozen historical stakes may recover their already-counted reward, but only
// claim recognizes old seeds. Register and crystallize remain V2 family-scoped.
#[test]
fn frozen_owner_only_and_linked_stakes_preserve_claims_without_reopening_accrual() {
    exercise_historical_frozen_stake_claim(false, "owner-only V0 stake");
    exercise_historical_frozen_stake_claim(true, "owner+linked V1 stake");
}

fn exercise_pre_epoch_legacy_stake_blocks_parallel_registration(
    linked_seed: bool,
    already_counted: bool,
    label: &str,
) {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000);

    let owner = Keypair::new();
    let portfolio = Pubkey::new_unique();
    set_slot(&mut svm, 100);
    set_portfolio(
        &mut svm,
        &portfolio,
        &env.stub_perc,
        &env.market,
        &owner.pubkey(),
        5_000,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &owner,
        &owner.pubkey(),
        &portfolio,
        COHORT_LP,
    )
    .expect("register the pre-upgrade stake");
    if already_counted {
        set_slot(&mut svm, 1_500);
        set_portfolio(
            &mut svm,
            &portfolio,
            &env.stub_perc,
            &env.market,
            &owner.pubkey(),
            12_000,
            0,
        );
        crystallize(&mut svm, &payer, &env, &owner, &portfolio)
            .expect("crystallize the pre-upgrade stake");
    }

    let current_stake = stake_pda_for_cohort(&env, &owner.pubkey(), &portfolio, COHORT_LP);
    let mut historical_stake = svm.get_account(&current_stake).unwrap();
    let (historical_key, historical_bump) = if linked_seed {
        Pubkey::find_program_address(
            &[
                b"rd_stake",
                env.rd_config.as_ref(),
                owner.pubkey().as_ref(),
                portfolio.as_ref(),
            ],
            &rd_id(),
        )
    } else {
        Pubkey::find_program_address(
            &[b"rd_stake", env.rd_config.as_ref(), owner.pubkey().as_ref()],
            &rd_id(),
        )
    };
    historical_stake.data[192] = historical_bump;
    svm.set_account(historical_key, historical_stake).unwrap();
    svm.set_account(
        current_stake,
        Account {
            lamports: 0,
            data: vec![],
            owner: solana_sdk::system_program::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
    let mut historical_config = svm.get_account(&env.rd_config).unwrap();
    historical_config.data.truncate(823);
    svm.set_account(env.rd_config, historical_config).unwrap();

    set_slot(&mut svm, 1_600);
    let result = register(
        &mut svm,
        &payer,
        &env,
        &owner,
        &owner.pubkey(),
        &portfolio,
        COHORT_LP,
    );
    if already_counted {
        assert!(
            result.is_err(),
            "{label}: an already-counted historical stake must block a parallel family stake"
        );
        assert!(
            svm.get_account(&current_stake)
                .is_none_or(|account| account.data.is_empty()),
            "{label}: rejected parallel registration creates no current stake"
        );
    } else {
        result.unwrap_or_else(|error| {
            panic!("{label}: a zero-point predecessor can migrate: {error}")
        });
        assert!(
            svm.get_account(&current_stake)
                .is_some_and(|account| !account.data.is_empty()),
            "{label}: migration creates the current family stake"
        );
    }
}

#[test]
fn pre_epoch_historical_stake_cannot_be_registered_again_under_the_current_family_seed() {
    exercise_pre_epoch_legacy_stake_blocks_parallel_registration(
        false,
        true,
        "owner-only V0 stake",
    );
    exercise_pre_epoch_legacy_stake_blocks_parallel_registration(
        true,
        true,
        "owner+linked V1 stake",
    );
}

#[test]
fn pre_epoch_zero_point_historical_stake_can_migrate_to_the_current_family_seed() {
    exercise_pre_epoch_legacy_stake_blocks_parallel_registration(
        false,
        false,
        "owner-only V0 stake",
    );
    exercise_pre_epoch_legacy_stake_blocks_parallel_registration(
        true,
        false,
        "owner+linked V1 stake",
    );
}

// SNAP MANIPULATION (trader cohort, NON-ZERO baseline — sweep tick D free-farm): the LP delta test above and
// the churn test both register on a FRESH portfolio (snap = 0). The sharper trader-specific free-farm is to
// bring a portfolio that ALREADY carries a large crystallized loss history and try to cash it in: register
// captures `residual_snap = crystallized - spent` at register, and crystallize credits only `counter - snap`,
// so pre-registration loss must earn NOTHING. Distinctly, the trader net-by-spent must still hold ATOP that
// non-zero baseline: a counterparty recovering the POST-register loss (spent rises) drives the net counter back
// to the snap, zeroing the points — even though the snap itself is non-zero. Pins both, which neither the
// snap=0 churn test nor the LP (no-spent) delta test exercises. Real .so.
#[test]
fn trader_snap_captures_pre_existing_loss_and_spent_netting_holds_atop_a_nonzero_baseline() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000);

    let t = Keypair::new();
    let pf = Pubkey::new_unique();
    let stake = stake_pda(&env, &t.pubkey(), &pf);
    let pts = |svm: &LiteSVM| -> u128 {
        u128::from_le_bytes(
            svm.get_account(&stake).unwrap().data[176..192]
                .try_into()
                .unwrap(),
        )
    };

    // Register at a PRE-EXISTING net loss: crystallized 8_000, spent 0 -> snap = 8_000. This history is the
    // trader's prior real loss; it must NOT be claimable (only the post-register delta earns).
    set_slot(&mut svm, 100);
    set_portfolio_full(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &t.pubkey(),
        0,
        8_000,
        0,
    );
    register(&mut svm, &payer, &env, &t, &t.pubkey(), &pf, COHORT_TRADER)
        .expect("reg trader at non-zero snap");

    // A NEW real loss after register: crystallized 8_000 -> 14_000 (spent still 0) -> net counter 14_000,
    // netΔ = 14_000 - 8_000(snap) = 6_000. Crystallize at tenure = 1024 -> floor_log2 = 10.
    set_slot(&mut svm, 100 + 1024);
    set_portfolio_full(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &t.pubkey(),
        0,
        14_000,
        0,
    );
    crystallize(&mut svm, &payer, &env, &t, &pf).expect("cry post-register delta");
    assert_eq!(pts(&svm), 10 * 6_000, "points credit ONLY the post-register 6_000 delta, not the full 14_000 (snap captured the pre-existing 8_000)");

    // Now a counterparty RECOVERS the post-register loss: spent rises 0 -> 6_000 (crystallized unchanged at
    // 14_000) -> net counter = 14_000 - 6_000 = 8_000 = the snap. netΔ = 8_000 - 8_000 = 0. Re-crystallize
    // OVERWRITES the points to 0: the washed/recovered loss earns nothing even atop the non-zero baseline.
    set_slot(&mut svm, 100 + 2048);
    set_portfolio_full(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &t.pubkey(),
        0,
        14_000,
        6_000,
    );
    crystallize(&mut svm, &payer, &env, &t, &pf).expect("re-cry after recovery");
    assert_eq!(
        pts(&svm),
        0,
        "net-by-spent atop a non-zero snap: recovering the post-register loss zeroes the points"
    );

    // End to end: freeze + claim pays 0 (no real net loss remained over the registration window).
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");
    let t_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &t.pubkey());
    claim(&mut svm, &payer, &env, &t, &t_ata, None).expect("claim (zero)");
    assert_eq!(token_amount(&svm, &t_ata), 0, "a trader whose post-register loss was recovered claims nothing — no free farm from a loss-loaded portfolio");
}

// REWARD DOS PROBE: a trader can crystallize at peak loss, consume that loss after emission ends, and leave
// stale points in the frozen denominator. Claim's live cap correctly pays that stake zero, but without a
// reduce-only finalize path the stale denominator permanently dilutes every honest trader. A permissionless
// finalize-window crystallize must remove only lost eligibility and must not admit fresh post-epoch loss.
#[test]
fn finalize_window_removes_recovered_trader_loss_without_admitting_fresh_loss() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup_trader_reward_epoch(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);

    // Honest trader H: a real, UNRECOVERED 6_000 loss, crystallized at tenure 1024 (floor_log2 = 10).
    let h = Keypair::new();
    let h_pf = Pubkey::new_unique();
    set_portfolio_full(
        &mut svm,
        &h_pf,
        &env.stub_perc,
        &env.market,
        &h.pubkey(),
        0,
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &h,
        &h.pubkey(),
        &h_pf,
        COHORT_TRADER,
    )
    .expect("reg H");
    // Attacker A: an IDENTICAL 6_000 loss, same tenure.
    let a = Keypair::new();
    let a_pf = Pubkey::new_unique();
    set_portfolio_full(
        &mut svm,
        &a_pf,
        &env.stub_perc,
        &env.market,
        &a.pubkey(),
        0,
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &a,
        &a.pubkey(),
        &a_pf,
        COHORT_TRADER,
    )
    .expect("reg A");

    set_slot(&mut svm, 100 + 1024);
    set_portfolio_full(
        &mut svm,
        &h_pf,
        &env.stub_perc,
        &env.market,
        &h.pubkey(),
        0,
        6_000,
        0,
    );
    set_portfolio_full(
        &mut svm,
        &a_pf,
        &env.stub_perc,
        &env.market,
        &a.pubkey(),
        0,
        6_000,
        0,
    );
    crystallize(&mut svm, &payer, &env, &h, &h_pf).expect("cry H (net 6_000)");
    crystallize(&mut svm, &payer, &env, &a, &a_pf).expect("cry A (net 6_000)");

    // A's loss is recovered during the finalize window. The reward period is over, so this can only lower
    // existing eligibility; a third-party cranker must be able to remove A's stale denominator contribution.
    set_slot(&mut svm, 100 + 2048);
    set_portfolio_full(
        &mut svm,
        &a_pf,
        &env.stub_perc,
        &env.market,
        &a.pubkey(),
        0,
        6_000,
        6_000,
    ); // A net -> 0
    crystallize_as(&mut svm, &payer, &env, &payer, &a.pubkey(), &a_pf)
        .expect("permissionless reduce-only finalize removes recovered loss");
    let a_stake = stake_pda_for_cohort(&env, &a.pubkey(), &a_pf, COHORT_TRADER);
    let stake_points = |svm: &LiteSVM| {
        u128::from_le_bytes(
            svm.get_account(&a_stake).unwrap().data[176..192]
                .try_into()
                .unwrap(),
        )
    };
    assert_eq!(
        stake_points(&svm),
        0,
        "recovered loss leaves no denominator points"
    );

    // Fresh loss created after emission_end cannot restore the removed points.
    set_portfolio_full(
        &mut svm,
        &a_pf,
        &env.stub_perc,
        &env.market,
        &a.pubkey(),
        0,
        12_000,
        6_000,
    );
    crystallize_as(&mut svm, &payer, &env, &payer, &a.pubkey(), &a_pf)
        .expect("post-epoch finalize remains callable but reduce-only");
    assert_eq!(
        stake_points(&svm),
        0,
        "fresh post-epoch loss cannot mint reward points"
    );

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    let h_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &h.pubkey());
    let a_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &a.pubkey());
    claim(&mut svm, &payer, &env, &h, &h_ata, None).expect("H claim");
    let _ = claim(&mut svm, &payer, &env, &a, &a_ata, None); // may or may not pay
    let h_got = token_amount(&svm, &h_ata);
    let a_got = token_amount(&svm, &a_ata);
    assert_eq!(a_got, 0, "a recovered loss earns no trader reward");
    assert_eq!(
        h_got, 1_000_000,
        "stale recovered-loss points cannot lock an honest trader's cohort share"
    );
}

#[test]
fn trader_fresh_post_freeze_loss_cannot_revive_recovered_frozen_points() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000); // trader cohort = 40% = 400_000, fee = 0
    set_slot(&mut svm, 100);

    let h = Keypair::new();
    let h_pf = Pubkey::new_unique();
    set_portfolio_full(
        &mut svm,
        &h_pf,
        &env.stub_perc,
        &env.market,
        &h.pubkey(),
        0,
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &h,
        &h.pubkey(),
        &h_pf,
        COHORT_TRADER,
    )
    .expect("reg H");
    let a = Keypair::new();
    let a_pf = Pubkey::new_unique();
    set_portfolio_full(
        &mut svm,
        &a_pf,
        &env.stub_perc,
        &env.market,
        &a.pubkey(),
        0,
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &a,
        &a.pubkey(),
        &a_pf,
        COHORT_TRADER,
    )
    .expect("reg A");

    set_slot(&mut svm, 100 + 1024);
    set_portfolio_full(
        &mut svm,
        &h_pf,
        &env.stub_perc,
        &env.market,
        &h.pubkey(),
        0,
        6_000,
        0,
    );
    set_portfolio_full(
        &mut svm,
        &a_pf,
        &env.stub_perc,
        &env.market,
        &a.pubkey(),
        0,
        6_000,
        0,
    );
    crystallize(&mut svm, &payer, &env, &h, &h_pf).expect("cry H (net 6_000)");
    crystallize(&mut svm, &payer, &env, &a, &a_pf).expect("cry A (net 6_000)");

    set_slot(&mut svm, 100 + 2048);
    set_portfolio_full(
        &mut svm,
        &a_pf,
        &env.stub_perc,
        &env.market,
        &a.pubkey(),
        0,
        6_000,
        6_000,
    );

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    set_portfolio_full(
        &mut svm,
        &a_pf,
        &env.stub_perc,
        &env.market,
        &a.pubkey(),
        0,
        12_000,
        6_000,
    );
    let h_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &h.pubkey());
    let a_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &a.pubkey());
    claim(&mut svm, &payer, &env, &h, &h_ata, None).expect("H claim");
    claim(&mut svm, &payer, &env, &a, &a_ata, None).expect("A claim");

    assert_eq!(
        token_amount(&svm, &a_ata),
        0,
        "fresh post-freeze net must not revive stale points from a recovered frozen loss"
    );
    assert_eq!(
        token_amount(&svm, &h_ata),
        200_000,
        "H remains limited by the frozen denominator; A's stale share stays locked"
    );
}

// LIVE-CAP ONE-SIDEDNESS (the fix must not over-pay either, sweep): the claim live-cap scales points by
// min(1, live_net/frozen_net). It caps DOWN on a recovery (live < frozen) — but it must NEVER pay UP when the
// live net is HIGHER than the crystallized (frozen) net. If a trader takes MORE loss after crystallizing and
// does NOT re-crystallize, the new loss is in neither stake.points NOR the frozen DENOMINATOR; paying it at
// claim (live > frozen) would credit un-accounted loss and over-draw the cohort. Pin that the claim pays the
// FROZEN value (the trader must re-crystallize to get credit for the new loss).
#[test]
fn live_cap_never_pays_above_the_frozen_points_when_live_net_grew_without_recrystallize() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000); // trader cohort = 400_000, fee = 0
    set_slot(&mut svm, 100);

    // Honest co-staker H: a 6_000 loss, crystallized and LEFT as-is (frozen == live).
    let h = Keypair::new();
    let h_pf = Pubkey::new_unique();
    set_portfolio_full(
        &mut svm,
        &h_pf,
        &env.stub_perc,
        &env.market,
        &h.pubkey(),
        0,
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &h,
        &h.pubkey(),
        &h_pf,
        COHORT_TRADER,
    )
    .expect("reg H");
    // T: an IDENTICAL 6_000 loss, same crystallize, but its live net later GROWS without a re-crystallize.
    let t = Keypair::new();
    let pf = Pubkey::new_unique();
    set_portfolio_full(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &t.pubkey(),
        0,
        0,
        0,
    );
    register(&mut svm, &payer, &env, &t, &t.pubkey(), &pf, COHORT_TRADER).expect("reg T");
    set_slot(&mut svm, 100 + 1024); // tenure 1024 -> floor_log2 = 10 -> each frozen = 10 * 6_000 = 60_000
    set_portfolio_full(
        &mut svm,
        &h_pf,
        &env.stub_perc,
        &env.market,
        &h.pubkey(),
        0,
        6_000,
        0,
    );
    set_portfolio_full(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &t.pubkey(),
        0,
        6_000,
        0,
    );
    crystallize(&mut svm, &payer, &env, &h, &h_pf).expect("cry H");
    crystallize(&mut svm, &payer, &env, &t, &pf).expect("cry T");

    // T's loss GROWS to 14_000 (live net 14_000 > frozen 6_000), but T does NOT re-crystallize: the extra loss
    // is in NEITHER stake.points NOR the frozen DENOMINATOR (still 60_000 + 60_000 = 120_000).
    set_slot(&mut svm, 100 + 2048);
    set_portfolio_full(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &t.pubkey(),
        0,
        14_000,
        0,
    );

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");
    let t_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &t.pubkey());
    let h_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &h.pubkey());
    claim(&mut svm, &payer, &env, &t, &t_ata, None).expect("T claim");
    claim(&mut svm, &payer, &env, &h, &h_ata, None).expect("H claim");
    // One-sided cap: live (14_000) > frozen (6_000) -> min(1, live/frozen) = 1 -> T is paid its FROZEN 60_000,
    // NOT scaled up to the un-crystallized 14_000. So T and H split 50/50; a pay-UP bug would let T draw
    // 140_000/120_000 (capped at the whole 400_000 cohort) and starve H.
    assert_eq!(
        token_amount(&svm, &t_ata),
        200_000,
        "T paid its FROZEN points (50%), NOT the higher un-crystallized live net"
    );
    assert_eq!(
        token_amount(&svm, &h_ata),
        200_000,
        "the honest co-staker keeps its full 50% — T's grown live net did not steal from H"
    );
}

// ATTACK PROBE (CROSS-COHORT double-dip: trader-loss + LP-recovery of the SAME loss). The real percolator
// `transfer_account_residual_reward_credit` (percolator v16.rs:8312) moves a recovered trader loss by raising
// BOTH the trader's `spent` AND the recovering LP's `received` by the SAME `credit = min(trader_net,
// lp_principal)`. So an attacker owning both legs can: (1) crystallize a TRADER loss X at peak (trader points
// = X); (2) self-deal the LP recovery so trader spent -> X (net 0) and LP received -> X; (3) crystallize the LP
// leg (LP points = X) — and if the trader leg keeps its STALE points, the SAME loss X is counted in BOTH
// cohorts = 2X captured from one X loss. The claim LIVE-CAP closes it: the trader claim re-reads live_net =
// crystallized - spent = 0 < frozen X -> caps the trader payout to 0, so the points correctly MOVE to the LP
// cohort (conserved), never doubled. Real rd .so; the post-recovery counter state mirrors what the percolator
// credit transfer produces (spent_trader and received_lp raised together).
#[test]
fn cross_cohort_trader_loss_then_lp_recovery_cannot_double_dip_the_same_loss() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000); // trader 40% = 400_000, lp 40% = 400_000, fee 0
    set_slot(&mut svm, 100);

    // Attacker owns BOTH legs: a_tr (trader) and a_lp (LP).
    let a_tr = Keypair::new();
    let tr_pf = Pubkey::new_unique();
    let a_lp = Keypair::new();
    let lp_pf = Pubkey::new_unique();
    set_portfolio_full(
        &mut svm,
        &tr_pf,
        &env.stub_perc,
        &env.market,
        &a_tr.pubkey(),
        0,
        0,
        0,
    );
    set_portfolio_full(
        &mut svm,
        &lp_pf,
        &env.stub_perc,
        &env.market,
        &a_lp.pubkey(),
        0,
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &a_tr,
        &a_tr.pubkey(),
        &tr_pf,
        COHORT_TRADER,
    )
    .expect("reg trader leg");
    register(
        &mut svm,
        &payer,
        &env,
        &a_lp,
        &a_lp.pubkey(),
        &lp_pf,
        COHORT_LP,
    )
    .expect("reg LP leg");

    // (1) trader crystallizes a 6_000 loss at PEAK (tenure 1024 -> floor_log2 = 10 -> 60_000 trader points).
    set_slot(&mut svm, 100 + 1024);
    set_portfolio_full(
        &mut svm,
        &tr_pf,
        &env.stub_perc,
        &env.market,
        &a_tr.pubkey(),
        0,
        6_000,
        0,
    );
    crystallize(&mut svm, &payer, &env, &a_tr, &tr_pf).expect("cry trader (net 6_000)");

    // (2) self-dealt LP RECOVERY of that exact loss: percolator raises trader.spent AND lp.received together.
    // (3) crystallize the LP leg so it banks the recovered 6_000 as LP points (60_000). Trader leg NOT
    //     re-crystallized -> its frozen 60_000 trader points are now STALE (live net is 0).
    set_slot(&mut svm, 100 + 2048);
    set_portfolio_full(
        &mut svm,
        &tr_pf,
        &env.stub_perc,
        &env.market,
        &a_tr.pubkey(),
        0,
        6_000,
        6_000,
    ); // trader net -> 0
    set_portfolio_full(
        &mut svm,
        &lp_pf,
        &env.stub_perc,
        &env.market,
        &a_lp.pubkey(),
        6_000,
        0,
        0,
    ); // lp received -> 6_000
    crystallize(&mut svm, &payer, &env, &a_lp, &lp_pf).expect("cry LP (received 6_000)");

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    let tr_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &a_tr.pubkey());
    let lp_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &a_lp.pubkey());
    let _ = claim(&mut svm, &payer, &env, &a_tr, &tr_ata, Some(&tr_pf)); // may pay 0
    claim(&mut svm, &payer, &env, &a_lp, &lp_ata, Some(&lp_pf)).expect("LP claim");
    let tr_got = token_amount(&svm, &tr_ata);
    let lp_got = token_amount(&svm, &lp_ata);

    // The live-cap caps the trader leg to 0 (live net 0 < frozen 6_000): the SAME loss is NOT counted twice.
    assert_eq!(tr_got, 0, "DOUBLE-DIP CLOSED: the trader leg's stale points are capped to 0 once the LP recovered the loss");
    // The points correctly LANDED in the LP cohort (the recovery's beneficiary): a_lp is the sole LP staker.
    assert_eq!(lp_got, 400_000, "the recovered loss is counted ONCE, in the LP cohort where the credit transfer attributed it");
    // Total capture from the single 6_000 loss = ONE cohort (400_000), never two (800_000).
    assert_eq!(
        tr_got + lp_got,
        400_000,
        "one loss -> one cohort's worth of COIN, not double across trader+LP"
    );
}

// ATTACK PROBE (residual claim live-cap bypass via a SUBSTITUTED portfolio). The claim live-cap (src:1011)
// re-reads the BOUND portfolio to scale a trader's frozen points down by min(1, live_net/frozen_net) — that is
// what closes the stale-points wash (a loss crystallized at peak then recovered to net 0). The cap is only as
// strong as the account bind: claim requires `portfolio.key == stake.backing_ledger` (src:1013). If that bind
// regressed, a farmer who recovered their loss (bound portfolio now net 0) could append a DECOY percolator-owned
// portfolio carrying a high net and read live_net >= frozen_net, making the cap a no-op and claiming the FULL
// stale-high points — the exact free-farm the cap exists to stop. None of the residual claim tests substitute
// the portfolio (they pass the bound one or None=bound); this pins the SHARP guard. Real rd .so.
#[test]
fn residual_claim_rejects_a_substituted_portfolio_no_live_cap_bypass() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000); // trader cohort = 400_000, fee = 0
    set_slot(&mut svm, 100);

    // Attacker A: a real 6_000 loss crystallized at tenure 1024 (floor_log2 = 10 -> frozen points 60_000)...
    let a = Keypair::new();
    let a_pf = Pubkey::new_unique();
    set_portfolio_full(
        &mut svm,
        &a_pf,
        &env.stub_perc,
        &env.market,
        &a.pubkey(),
        0,
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &a,
        &a.pubkey(),
        &a_pf,
        COHORT_TRADER,
    )
    .expect("reg A");
    set_slot(&mut svm, 100 + 1024);
    set_portfolio_full(
        &mut svm,
        &a_pf,
        &env.stub_perc,
        &env.market,
        &a.pubkey(),
        0,
        6_000,
        0,
    );
    crystallize(&mut svm, &payer, &env, &a, &a_pf).expect("cry A (net 6_000)");
    // ...then RECOVERED to net 0 (crystallized 6_000, spent 6_000) without re-crystallizing — the live-cap
    // would scale A's bound claim to 0.
    set_slot(&mut svm, 100 + 2048);
    set_portfolio_full(
        &mut svm,
        &a_pf,
        &env.stub_perc,
        &env.market,
        &a.pubkey(),
        0,
        6_000,
        6_000,
    );
    // A DECOY portfolio (percolator-owned so it passes the program-owner check) carrying a high live net that A
    // will try to substitute to read live_net >= frozen_net and defeat the cap.
    let decoy_pf = Pubkey::new_unique();
    set_portfolio_full(
        &mut svm,
        &decoy_pf,
        &env.stub_perc,
        &env.market,
        &a.pubkey(),
        0,
        99_999,
        0,
    );

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    let a_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &a.pubkey());
    // ATTACK: A claims but appends the DECOY portfolio (net 99_999) instead of its own (now net 0) bound one.
    assert!(claim(&mut svm, &payer, &env, &a, &a_ata, Some(&decoy_pf)).is_err(),
        "a substituted portfolio is rejected (portfolio.key != stake.backing_ledger) — no live-cap bypass");
    assert_eq!(
        token_amount(&svm, &a_ata),
        0,
        "the substituted-portfolio claim paid nothing"
    );

    // (control) claiming with the CORRECT bound (now-recovered) portfolio caps to 0, as the live-cap designs.
    claim(&mut svm, &payer, &env, &a, &a_ata, Some(&a_pf)).expect("claim with the bound portfolio");
    assert_eq!(
        token_amount(&svm, &a_ata),
        0,
        "A recovered its loss (live net 0) -> live-cap pays 0 COIN"
    );
}

// ATTACK PROBE (same-key portfolio reuse at residual claim): a substituted-key guard is not enough if the
// registered portfolio key can be dematerialized and reinitialized with different provenance before claim.
// The residual live-cap must re-check the same key still belongs to the registered owner and allow-listed
// market; otherwise a recovered trader could replace the bound account data with a foreign-market high-net
// portfolio and make the cap pay stale frozen points.
#[test]
fn residual_claim_rejects_same_key_reinitialized_market_no_live_cap_bypass() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);

    let attacker = Keypair::new();
    let pf = Pubkey::new_unique();
    set_portfolio_full(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &attacker.pubkey(),
        0,
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &attacker,
        &attacker.pubkey(),
        &pf,
        COHORT_TRADER,
    )
    .expect("register trader");
    set_slot(&mut svm, 1_124);
    set_portfolio_full(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &attacker.pubkey(),
        0,
        6_000,
        0,
    );
    crystallize(&mut svm, &payer, &env, &attacker, &pf).expect("crystallize trader");

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");
    let ata = create_token_account(&mut svm, &payer, &env.coin_mint, &attacker.pubkey());

    set_portfolio_full(
        &mut svm,
        &pf,
        &env.stub_perc,
        &Pubkey::new_unique(),
        &attacker.pubkey(),
        0,
        99_999,
        0,
    );
    assert!(
        claim(&mut svm, &payer, &env, &attacker, &ata, Some(&pf)).is_err(),
        "same portfolio key reinitialized to a foreign market must not bypass the residual live-cap"
    );
    assert_eq!(
        token_amount(&svm, &ata),
        0,
        "same-key foreign-market claim paid nothing"
    );
}

// ATTACK PROBE (crystallize replay / denominator inflation): an LP/trader stake's points are the Δ of a
// MONOTONIC percolator counter since the register-time snapshot — `new_pts = counter - residual_snap`, and
// the cohort denominator is updated subtract-old/add-new (`slot = slot - stake.points + new_pts`,
// src:765-768). residual_snap is NOT advanced by crystallize, so the operation must be IDEMPOTENT: replaying
// crystallize with an unchanged counter re-derives the same Δ and nets zero, and a later crystallize after
// the counter moved tracks the FULL delta from register (never an accumulation of per-window deltas). If it
// instead added each call's Δ, a wash-farmer could replay-crystallize to multiply their own points and seize
// a larger slice of the (already self-capturable) LP/trader cohort. Pinned because denominator integrity is
// the only thing standing between "one miner takes their honest Δ-share" and "one miner inflates without bound".
#[test]
fn crystallize_is_idempotent_under_replay_and_tracks_full_delta_not_accumulation() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64;
    let env = setup(&mut svm, &payer, supply);
    set_slot(&mut svm, 100);

    let lp = Keypair::new();
    let pf = Pubkey::new_unique();
    let stake = stake_pda(&env, &lp.pubkey(), &pf);
    // register at received = 5_000 (pre-existing baseline — must NOT count).
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        5_000,
        0,
    );
    register(&mut svm, &payer, &env, &lp, &lp.pubkey(), &pf, COHORT_LP).expect("reg lp");

    // points@176 (u128) on the stake; lp_total_points@402 (u128) on the config — read both each step.
    let pts = |svm: &LiteSVM| -> u128 {
        u128::from_le_bytes(
            svm.get_account(&stake).unwrap().data[176..192]
                .try_into()
                .unwrap(),
        )
    };
    let denom = |svm: &LiteSVM| -> u128 {
        u128::from_le_bytes(
            svm.get_account(&env.rd_config).unwrap().data[402..418]
                .try_into()
                .unwrap(),
        )
    };

    // Points are TIME-WEIGHTED: floor(log2(now - start_slot=100)) * netΔ. register at slot 100.
    // counter 5_000 -> 12_000 (netΔ from register = 7_000). First crystallize at slot 1_000: tenure 900,
    // floor(log2(900)) = 9, so points = 9 * 7_000 = 63_000.
    set_slot(&mut svm, 1_000);
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        12_000,
        0,
    );
    crystallize(&mut svm, &payer, &env, &lp, &pf).expect("crystallize 1");
    assert_eq!(
        pts(&svm),
        9 * 7_000,
        "points = floor(log2(tenure=900)) * (counter - register snapshot)"
    );
    assert_eq!(
        denom(&svm),
        9 * 7_000,
        "cohort denominator = the single staker's weighted Δ"
    );

    // REPLAY at the SAME slot+counter — idempotent: same tenure, same netΔ -> no inflation.
    crystallize(&mut svm, &payer, &env, &lp, &pf).expect("crystallize replay (same counter)");
    crystallize(&mut svm, &payer, &env, &lp, &pf).expect("crystallize replay #2");
    assert_eq!(
        pts(&svm),
        9 * 7_000,
        "replay did NOT inflate the stake's points"
    );
    assert_eq!(
        denom(&svm),
        9 * 7_000,
        "replay did NOT inflate the cohort denominator"
    );

    // counter advances 12_000 -> 20_000 (netΔ from register = 15_000). Re-crystallize at slot 1_800: tenure
    // 1_700, floor(log2) = 10. Tracks the FULL netΔ from register (10 * 15_000), NOT a sum of per-window Δs.
    set_slot(&mut svm, 1_800);
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        20_000,
        0,
    );
    crystallize(&mut svm, &payer, &env, &lp, &pf).expect("crystallize 2");
    assert_eq!(
        pts(&svm),
        10 * 15_000,
        "points = floor(log2(tenure=1700)) * cumulative netΔ from register"
    );
    assert_eq!(
        denom(&svm),
        10 * 15_000,
        "denominator re-derived (subtract-old/add-new), still the true weighted Δ"
    );

    // Replaying after the advance is still idempotent.
    crystallize(&mut svm, &payer, &env, &lp, &pf).expect("crystallize replay #3");
    assert_eq!(
        pts(&svm),
        10 * 15_000,
        "still idempotent after the counter advance"
    );
    assert_eq!(denom(&svm), 10 * 15_000, "denominator stable under replay");
}

// claim is rejected before freeze (denominators not final).
#[test]
fn claim_before_freeze_is_rejected() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);
    let lp = Keypair::new();
    let pf = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        0,
        0,
    );
    register(&mut svm, &payer, &env, &lp, &lp.pubkey(), &pf, COHORT_LP).expect("reg");
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        1_000,
        0,
    );
    crystallize(&mut svm, &payer, &env, &lp, &pf).expect("cry");
    let ata = create_token_account(&mut svm, &payer, &env.coin_mint, &lp.pubkey());
    assert!(
        claim(&mut svm, &payer, &env, &lp, &ata, None).is_err(),
        "claim before freeze must reject"
    );
}

// init validation guards: reject zero supply, a cohort bps sum > 100%, an active insurance/backing cohort
// with no pool scope, and an active LP/trader cohort with no market_group (finding IL — else an unscoped
// genesis could mint COIN to positions from any pool / any market). init authenticates the live SPL mint
// authority, so each case uses a fresh real coin mint. Real .so.
fn try_init(
    svm: &mut LiteSVM,
    payer: &Keypair,
    supply: u64,
    ins: u16,
    back: u16,
    lp: u16,
    ins_pool: Pubkey,
    back_pool: Pubkey,
    market: Pubkey,
) -> Result<(), String> {
    try_init_with_timing(
        svm, payer, supply, 2_000, 500, ins, back, lp, ins_pool, back_pool, market,
    )
}

#[allow(clippy::too_many_arguments)]
fn try_init_with_timing(
    svm: &mut LiteSVM,
    payer: &Keypair,
    supply: u64,
    emission_end: u64,
    finalize_window: u64,
    ins: u16,
    back: u16,
    lp: u16,
    ins_pool: Pubkey,
    back_pool: Pubkey,
    market: Pubkey,
) -> Result<(), String> {
    let mint_auth = Keypair::new();
    let coin_mint = create_mint(svm, payer, &mint_auth.pubkey());
    let rd_config = Pubkey::find_program_address(&[b"rd_config", coin_mint.as_ref()], &rd_id()).0;
    let dist_config = dist_config_pda(&coin_mint, &rd_config);
    let stub_perc = Pubkey::new_unique();
    let stub_sub = Pubkey::new_unique();
    let d = rd_init_data(
        supply,
        emission_end,
        ins,
        back,
        lp,
        finalize_window,
        ins_pool,
        back_pool,
        market,
        &[],
        Some(0),
    );
    send(
        svm,
        payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: rd_init_accounts(
                payer.pubkey(),
                coin_mint,
                dist_config,
                stub_perc,
                stub_sub,
                rd_config,
                mint_auth.pubkey(),
            ),
            data: d,
        }],
        &[&mint_auth],
    )
}

#[test]
fn init_rejects_zero_supply_overallocation_and_unscoped_cohorts() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let p = Pubkey::new_unique;
    let z = Pubkey::default();

    // zero supply -> rejected.
    assert!(
        try_init(&mut svm, &payer, 0, 1_000, 1_000, 4_000, p(), p(), p()).is_err(),
        "zero supply"
    );
    // cohort bps sum > 100% (5000+4000+2000=11000) -> rejected.
    assert!(
        try_init(
            &mut svm,
            &payer,
            1_000_000,
            5_000,
            4_000,
            2_000,
            p(),
            p(),
            p()
        )
        .is_err(),
        "bps sum > 100%"
    );
    // insurance active (10000, trader=0) but NO insurance pool -> rejected.
    assert!(
        try_init(&mut svm, &payer, 1_000_000, 10_000, 0, 0, z, z, z).is_err(),
        "insurance cohort without a pool scope"
    );
    // backing active (10000, trader=0) but NO backing pool -> rejected.
    assert!(
        try_init(&mut svm, &payer, 1_000_000, 0, 10_000, 0, z, z, z).is_err(),
        "backing cohort without a pool scope"
    );
    // LP+trader active (lp 4000, trader 4000) with pools set but NO market_group -> rejected (finding IL).
    assert!(
        try_init(
            &mut svm,
            &payer,
            1_000_000,
            1_000,
            1_000,
            4_000,
            p(),
            p(),
            z
        )
        .is_err(),
        "lp/trader cohort without a market scope"
    );
    // Overflowing emission_end + finalize_window would make the permissionless one-shot freeze unreachable
    // for any practical slot, permanently blocking self-service claims.
    assert!(
        try_init_with_timing(
            &mut svm,
            &payer,
            1_000_000,
            u64::MAX - 9,
            10,
            1_000,
            1_000,
            4_000,
            p(),
            p(),
            p(),
        )
        .is_err(),
        "overflowing freeze cutoff must be rejected at init"
    );
    // A zero finalize window lets a permissionless cranker freeze immediately at emission_end,
    // before slower backers get any post-emission slot to crystallize their final points.
    assert!(
        try_init_with_timing(
            &mut svm,
            &payer,
            1_000_000,
            2_000,
            0,
            1_000,
            1_000,
            4_000,
            p(),
            p(),
            p(),
        )
        .is_err(),
        "zero finalize window must be rejected at init"
    );
    // fully-valid config -> accepted.
    try_init(
        &mut svm,
        &payer,
        1_000_000,
        1_000,
        1_000,
        4_000,
        p(),
        p(),
        p(),
    )
    .expect("a fully-scoped config initializes");
}

// ATTACK PROBE (init extra-market allow-list vetting bypass): the allow-list tail is a u8 count + that many
// trusted-market pubkeys (finding IL+). The vetting (src:489-501): count <= MAX_EXTRA_MARKETS (9), each extra
// != default and != market_group, and NO trailing bytes (502, exact-length). If any of these could be bypassed
// at init, the orchestrator's curated allow-list could be silently corrupted — an oversized count overruns the
// fixed config layout; a default extra makes `market_allowed(default)` TRUE so any uninitialized/edge portfolio
// (market_group field default) farms the COIN; a length mismatch desyncs the parse. The existing init test
// (...unscoped_cohorts) only ever sends count=0; the lp_cohort test sends a VALID 2-extra list. The vetting
// REJECTIONS were untested. Real rd .so.
#[test]
fn init_extra_market_vetting_rejects_overflow_default_duplicate_and_malformed_tail() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();

    // Build an rd init (lp 4000 / trader 4000 remainder, so the allow-list is load-bearing) with a CUSTOM
    // extra-market tail: a declared u8 count followed by the given pubkeys (which may mismatch the count to
    // exercise the exact-length check). Fresh coin_mint each call -> distinct rd_config, no reinit collision.
    let try_tail = |svm: &mut LiteSVM,
                    market_group: Pubkey,
                    declared_count: u8,
                    extras: &[Pubkey]|
     -> Result<(), String> {
        let mint_auth = Keypair::new();
        let coin_mint = create_mint(svm, &payer, &mint_auth.pubkey());
        let rd_config =
            Pubkey::find_program_address(&[b"rd_config", coin_mint.as_ref()], &rd_id()).0;
        let dist_config = dist_config_pda(&coin_mint, &rd_config);
        let mut d = vec![0u8];
        d.extend_from_slice(&1_000_000u64.to_le_bytes()); // supply
        d.extend_from_slice(&2_000u64.to_le_bytes()); // emission_end
        d.extend_from_slice(&1_000u16.to_le_bytes()); // insurance
        d.extend_from_slice(&1_000u16.to_le_bytes()); // backing
        d.extend_from_slice(&4_000u16.to_le_bytes()); // lp (trader = 4000 remainder)
        d.extend_from_slice(&500u64.to_le_bytes()); // finalize_window
        d.extend_from_slice(Pubkey::new_unique().as_ref()); // ins_pool (non-default)
        d.extend_from_slice(Pubkey::new_unique().as_ref()); // back_pool (non-default)
        d.extend_from_slice(market_group.as_ref());
        d.push(declared_count);
        for e in extras {
            d.extend_from_slice(e.as_ref());
        }
        send(
            svm,
            &payer,
            &[Instruction {
                program_id: rd_id(),
                accounts: rd_init_accounts(
                    payer.pubkey(),
                    coin_mint,
                    dist_config,
                    Pubkey::new_unique(),
                    Pubkey::new_unique(),
                    rd_config,
                    mint_auth.pubkey(),
                ),
                data: d,
            }],
            &[&mint_auth],
        )
    };
    let mg = Pubkey::new_unique(); // a real primary trusted market
    let u = Pubkey::new_unique;

    // (1) count > MAX_EXTRA_MARKETS (10 > 9) — would overrun the fixed [Pubkey; 9] config layout.
    let ten: Vec<Pubkey> = (0..10).map(|_| u()).collect();
    assert!(
        try_tail(&mut svm, mg, 10, &ten).is_err(),
        "extra_market_count > MAX (9) must be rejected"
    );
    // (2) a default-pubkey extra — would make market_allowed(default) TRUE (any unset-market portfolio farms).
    assert!(
        try_tail(&mut svm, mg, 2, &[u(), Pubkey::default()]).is_err(),
        "a default-pubkey extra market is rejected"
    );
    // (3) an extra duplicating the primary market_group.
    assert!(
        try_tail(&mut svm, mg, 2, &[u(), mg]).is_err(),
        "an extra equal to the primary market is rejected"
    );
    // (4) duplicate extras silently waste scarce allow-list slots and can leave a vetted market unlistable.
    let dup = u();
    assert!(
        try_tail(&mut svm, mg, 2, &[dup, dup]).is_err(),
        "duplicate extra markets are rejected"
    );
    // (5) declared count exceeds the supplied pubkeys (truncated payload) — take_pubkey underruns.
    assert!(
        try_tail(&mut svm, mg, 3, &[u(), u()]).is_err(),
        "count > supplied pubkeys is rejected"
    );
    // (6) trailing bytes: more pubkeys than declared — the exact-length check (502) rejects the remainder.
    assert!(
        try_tail(&mut svm, mg, 1, &[u(), u()]).is_err(),
        "extra trailing pubkeys are rejected"
    );
    // (7) boundary: count == MAX (9) with 9 distinct valid extras -> accepted.
    let nine: Vec<Pubkey> = (0..9).map(|_| u()).collect();
    try_tail(&mut svm, mg, 9, &nine)
        .expect("the maximum-size, all-distinct, all-valid allow-list initializes");
}

// CONFIG RE-INIT (un-freeze / denominator reset): the rd config is one-shot — init rejects an already-initialized
// config (data_len != 0, lib.rs:70). This is what makes FREEZE immutable: without it, an attacker could re-init the
// config AFTER freeze, resetting freeze_slot to 0 (un-freezing) and wiping the frozen cohort denominators, then
// re-open register/crystallize to inject/inflate points and over-claim the COIN. gv and distribution have explicit
// re-init-rejection tests; the rd did not. This pins it: a re-init of the FROZEN config is rejected and the config
// (freeze_slot + all four frozen denominators + the bound vault) is left byte-identical.
#[test]
fn rd_config_cannot_be_reinitialized_to_un_freeze_or_reset_denominators() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64;
    let env = setup(&mut svm, &payer, supply);

    // Freeze the config (one-shot): freeze_slot set, the four cohort denominators snapshotted, the vault bound.
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");
    let cfg_before = svm.get_account(&env.rd_config).unwrap().data.clone();
    let freeze_slot_before = u64::from_le_bytes(cfg_before[318..326].try_into().unwrap());
    assert!(
        freeze_slot_before != 0,
        "config is frozen (freeze_slot set)"
    );

    // ATTACK: re-init the SAME (now frozen) rd config — would reset freeze_slot -> 0 and wipe the denominators.
    let dist_config = dist_config_pda(&env.coin_mint, &env.rd_config);
    let mut d = vec![0u8];
    d.extend_from_slice(&supply.to_le_bytes());
    d.extend_from_slice(&env.emission_end.to_le_bytes());
    d.extend_from_slice(&1_000u16.to_le_bytes()); // insurance
    d.extend_from_slice(&1_000u16.to_le_bytes()); // backing
    d.extend_from_slice(&4_000u16.to_le_bytes()); // lp
    d.extend_from_slice(&env.finalize_window.to_le_bytes());
    d.extend_from_slice(env.ins_pool.as_ref());
    d.extend_from_slice(env.back_pool.as_ref());
    d.extend_from_slice(env.market.as_ref());
    d.extend_from_slice(&[0u8]); // extra market count
    let res = send(
        &mut svm,
        &payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new_readonly(env.coin_mint, false),
                AccountMeta::new_readonly(dist_id(), false),
                AccountMeta::new_readonly(dist_config, false),
                AccountMeta::new_readonly(env.stub_perc, false),
                AccountMeta::new_readonly(env.stub_sub, false),
                AccountMeta::new(env.rd_config, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
                AccountMeta::new_readonly(env.mint_auth.pubkey(), true),
            ],
            data: d,
        }],
        &[&env.mint_auth],
    );
    assert!(
        res.is_err(),
        "re-initializing an existing rd config must be rejected — no un-freeze / denominator reset"
    );

    // The frozen config is byte-identical: freeze_slot + all denominators + the bound vault are immutable.
    let cfg_after = svm.get_account(&env.rd_config).unwrap().data.clone();
    assert_eq!(
        cfg_after, cfg_before,
        "rd config unchanged by the rejected re-init — freeze and denominators immutable"
    );
}

// claim anti-theft (GY at the claim layer): LP/trader claim is PERMISSIONLESS (any cranker may finalize a
// backer's claim), so the cranker must NOT be able to redirect the COIN to an account it owns or can spend
// as delegate, nor pay from a decoy vault. The bound recipient + config vault are the only endpoints. Real .so.
#[test]
fn claim_cannot_be_redirected_delegated_or_paid_from_a_decoy_vault() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);

    let lp = Keypair::new();
    let pf = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        0,
        0,
    );
    register(&mut svm, &payer, &env, &lp, &lp.pubkey(), &pf, COHORT_LP).expect("register"); // recipient bound = lp
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        10_000,
        0,
    );
    set_slot(&mut svm, 1_000); // residual points are time-weighted -> need tenure > 0
    crystallize(&mut svm, &payer, &env, &lp, &pf).expect("crystallize");
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");

    let attacker = Keypair::new();
    let stake = stake_pda(&env, &lp.pubkey(), &pf);
    let rd_config = env.rd_config;
    let coin_mint = env.coin_mint;
    let real_vault = env.vault;
    let archive = portfolio_archive_pda(&svm, &env, &lp.pubkey(), &pf);
    let mut raw_claim = |svm: &mut LiteSVM,
                         cranker: &Keypair,
                         vault: Pubkey,
                         recipient_ata: Pubkey|
     -> Result<(), String> {
        send(
            svm,
            &payer,
            &[Instruction {
                program_id: rd_id(),
                accounts: vec![
                    AccountMeta::new(cranker.pubkey(), true),
                    AccountMeta::new_readonly(rd_config, false),
                    AccountMeta::new(stake, false),
                    AccountMeta::new(vault, false),
                    AccountMeta::new(recipient_ata, false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                    AccountMeta::new_readonly(pf, false), // LP/trader live-cap portfolio (stake.backing_ledger)
                    AccountMeta::new_readonly(archive, false),
                    AccountMeta::new_readonly(
                        retired_market_pda(&env.stub_perc, &env.market),
                        false,
                    ),
                ],
                data: vec![5u8],
            }],
            &[cranker],
        )
    };

    let attacker_ata = create_token_account(&mut svm, &payer, &coin_mint, &attacker.pubkey());
    // A permissionless cranker must not force the payout into a recipient-owned
    // account that delegates token spending to the cranker. Owner equality alone
    // does not prevent the delegate from taking the reward immediately afterward.
    let delegated_lp_ata = create_token_account(&mut svm, &payer, &coin_mint, &lp.pubkey());
    send(
        &mut svm,
        &payer,
        &[spl_token::instruction::approve(
            &spl_token::ID,
            &delegated_lp_ata,
            &attacker.pubkey(),
            &lp.pubkey(),
            &[],
            u64::MAX,
        )
        .unwrap()],
        &[&lp],
    )
    .expect("recipient legitimately delegates an existing COIN account");
    if raw_claim(&mut svm, &attacker, real_vault, delegated_lp_ata).is_ok() {
        let stolen = token_amount(&svm, &delegated_lp_ata);
        send(
            &mut svm,
            &payer,
            &[spl_token::instruction::transfer(
                &spl_token::ID,
                &delegated_lp_ata,
                &attacker_ata,
                &attacker.pubkey(),
                &[],
                stolen,
            )
            .unwrap()],
            &[&attacker],
        )
        .expect("the cranker spends the forced payout through its delegate authority");
        assert_eq!(token_amount(&svm, &attacker_ata), stolen);
        panic!("permissionless claim exposed the bound recipient's reward to a token delegate");
    }

    // (a) a third-party cranker redirecting to its OWN ata -> rejected (ra.owner != stake.recipient).
    assert!(
        raw_claim(&mut svm, &attacker, real_vault, attacker_ata).is_err(),
        "claim cannot be redirected to a non-recipient ata"
    );
    // (b) paying from a decoy vault -> rejected (vault.key != config.vault).
    let decoy_vault = create_token_account(&mut svm, &payer, &coin_mint, &attacker.pubkey());
    let lp_ata = create_token_account(&mut svm, &payer, &coin_mint, &lp.pubkey());
    assert!(
        raw_claim(&mut svm, &attacker, decoy_vault, lp_ata).is_err(),
        "claim cannot pay from a decoy vault"
    );
    // (control) ANY cranker may finalize the claim, but ONLY into the bound recipient from the real vault.
    raw_claim(&mut svm, &attacker, real_vault, lp_ata)
        .expect("permissionless cranker pays the bound recipient");
    assert!(
        token_amount(&svm, &lp_ata) > 0,
        "the LP backer received its share"
    );
}

// SAME-PROGRAM TYPE CONFUSION (discriminator defense): the rd Config (RDCONFG1) and Stake (RDSTAKE1) are BOTH
// rd-owned, so the `owner == program_id` checks pass for either in either slot. The ONLY thing separating them is
// the 8-byte discriminator checked in deserialize (lib.rs:286 Config, 434 Stake). Without it, passing a Stake in
// the config slot would read attacker-controlled stake bytes AS a Config (wrong vault/total_supply/denominators ->
// a crafted drain), and a Config in the stake slot would read config bytes as a Stake. The cross-PROGRAM confusion
// is pinned (register_rejects_..._cross_program 1607); this pins the same-program one. Both swaps are rejected; the
// correctly-typed claim still pays.
#[test]
fn claim_rejects_same_program_type_confusion_config_and_stake_discriminators() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);

    let lp = Keypair::new();
    let pf = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        0,
        0,
    ); // snap 0 at register
    register(&mut svm, &payer, &env, &lp, &lp.pubkey(), &pf, COHORT_LP).expect("reg lp");
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        9_000,
        0,
    ); // residual_received grows
    set_slot(&mut svm, 1_000); // tenure > 0 for the time-weight
    crystallize(&mut svm, &payer, &env, &lp, &pf).expect("cry lp");
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");
    let stake = stake_pda(&env, &lp.pubkey(), &pf);
    let ata = create_token_account(&mut svm, &payer, &env.coin_mint, &lp.pubkey());
    let archive = portfolio_archive_pda(&svm, &env, &lp.pubkey(), &pf);

    // Build a claim with arbitrary accounts in the config/stake slots (LP cohort -> no position appended).
    let claim_with =
        |svm: &mut LiteSVM, config_slot: Pubkey, stake_slot: Pubkey| -> Result<(), String> {
            send(
                svm,
                &payer,
                &[Instruction {
                    program_id: rd_id(),
                    accounts: vec![
                        AccountMeta::new(lp.pubkey(), true),
                        AccountMeta::new_readonly(config_slot, false),
                        AccountMeta::new(stake_slot, false),
                        AccountMeta::new(env.vault, false),
                        AccountMeta::new(ata, false),
                        AccountMeta::new_readonly(spl_token::ID, false),
                        AccountMeta::new_readonly(pf, false), // LP/trader live-cap portfolio (stake.backing_ledger)
                        AccountMeta::new_readonly(archive, false),
                        AccountMeta::new_readonly(
                            retired_market_pda(&env.stub_perc, &env.market),
                            false,
                        ),
                    ],
                    data: vec![5u8],
                }],
                &[&lp],
            )
        };
    // (a) STAKE account in the config slot -> Config::deserialize sees RDSTAKE1 != RDCONFG1 -> reject.
    assert!(
        claim_with(&mut svm, stake, stake).is_err(),
        "a stake account in the config slot must be rejected by the discriminator"
    );
    // (b) CONFIG account in the stake slot -> Stake::deserialize sees RDCONFG1 != RDSTAKE1 -> reject.
    assert!(
        claim_with(&mut svm, env.rd_config, env.rd_config).is_err(),
        "a config account in the stake slot must be rejected by the discriminator"
    );
    assert_eq!(
        token_amount(&svm, &ata),
        0,
        "no payout from the type-confused claims"
    );

    // The correctly-typed claim still pays the LP its cohort share.
    claim_with(&mut svm, env.rd_config, stake).expect("honest claim with correct account types");
    assert!(
        token_amount(&svm, &ata) > 0,
        "honest claim paid the LP its share"
    );
}

// ATTACK PROBE (cross-genesis claim: a stake from rd_config A claims rd_config B's COIN). claim binds
// stake.config == config_account.key (src:871). Two genesis flows can share the subledger/percolator but have
// DIFFERENT coin mints + vaults; without this bind, an attacker who earned points in a worthless genesis A
// could present A's stake against B's real (frozen, funded) config + vault and drain B's valuable COIN for
// points B never granted. The decoy-vault test uses the SAME config's stake against a fake vault; the
// cross-CONFIG case (a real stake from a different rd_config against B's real vault) was untested. Real rd .so.
#[test]
fn claim_rejects_a_stake_from_a_different_rd_config_no_cross_genesis_claim() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();

    // Genesis A: an LP staker earns real points, then A freezes.
    let env_a = setup(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);
    let lp = Keypair::new();
    let pf = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &pf,
        &env_a.stub_perc,
        &env_a.market,
        &lp.pubkey(),
        0,
        0,
    );
    register(&mut svm, &payer, &env_a, &lp, &lp.pubkey(), &pf, COHORT_LP).expect("register in A");
    set_portfolio(
        &mut svm,
        &pf,
        &env_a.stub_perc,
        &env_a.market,
        &lp.pubkey(),
        10_000,
        0,
    );
    set_slot(&mut svm, 1_000); // residual points are time-weighted -> need tenure > 0
    crystallize(&mut svm, &payer, &env_a, &lp, &pf).expect("crystallize in A");

    // Genesis B: a SEPARATE rd_config (different coin mint + vault), also frozen.
    let env_b = setup(&mut svm, &payer, 1_000_000);

    set_slot(&mut svm, env_a.emission_end + env_a.finalize_window + 1);
    freeze(&mut svm, &payer, &env_a).expect("freeze A");
    freeze(&mut svm, &payer, &env_b).expect("freeze B");

    // ATTACK: present A's stake against B's real config + funded vault. recipient ata owned by lp, B's mint.
    let stake_a = stake_pda(&env_a, &lp.pubkey(), &pf);
    let b_ata = create_token_account(&mut svm, &payer, &env_b.coin_mint, &lp.pubkey());
    let cranker = Keypair::new();
    svm.airdrop(&cranker.pubkey(), 1_000_000_000).unwrap();
    let cross = send(
        &mut svm,
        &payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: vec![
                AccountMeta::new(cranker.pubkey(), true),
                AccountMeta::new_readonly(env_b.rd_config, false),
                AccountMeta::new(stake_a, false),
                AccountMeta::new(env_b.vault, false),
                AccountMeta::new(b_ata, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data: vec![5u8],
        }],
        &[&cranker],
    );
    assert!(
        cross.is_err(),
        "A's stake cannot claim B's COIN — stake.config != config bind"
    );
    assert_eq!(
        token_amount(&svm, &b_ata),
        0,
        "no cross-genesis COIN paid out"
    );
    assert_eq!(
        token_amount(&svm, &env_b.vault),
        1_000_000,
        "genesis B's vault is untouched"
    );

    // (control) A's stake claims A's OWN vault for its real points.
    let a_ata = create_token_account(&mut svm, &payer, &env_a.coin_mint, &lp.pubkey());
    claim(&mut svm, &payer, &env_a, &lp, &a_ata, None).expect("claim in A pays from A");
    assert!(
        token_amount(&svm, &a_ata) > 0,
        "the staker claims its own genesis's COIN"
    );
}

// register guards distinct from the foreign-owner/pool/market tests: an out-of-range cohort, CROSS-PROGRAM
// type confusion (a capital cohort pointed at a percolator account, or an LP/trader cohort at a subledger
// position — the owner-PROGRAM check blocks reading the wrong struct at the bound offsets), and a
// double-register (the per-owner stake PDA already exists). Real .so.
#[test]
fn register_rejects_a_non_portfolio_percolator_witness() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);

    let owner = Keypair::new();
    let witness = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &witness,
        &env.stub_perc,
        &env.market,
        &owner.pubkey(),
        10_000,
        0,
    );
    let mut account = svm.get_account(&witness).unwrap();
    account.data[..8].copy_from_slice(&0x5045_5243_5631_3600u64.to_le_bytes());
    account.data[8..10].copy_from_slice(&16u16.to_le_bytes());
    account.data[10] = 1; // Percolator market, not portfolio.
    svm.set_account(witness, account).unwrap();

    assert!(
        register(
            &mut svm,
            &payer,
            &env,
            &owner,
            &owner.pubkey(),
            &witness,
            COHORT_LP,
        )
        .is_err(),
        "a Percolator-owned non-portfolio account must not mint portfolio-flow points"
    );
}

#[test]
fn register_rejects_a_non_position_subledger_witness() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);

    let owner = Keypair::new();
    let witness = Pubkey::new_unique();
    set_position(
        &mut svm,
        &witness,
        &env.stub_sub,
        &env.ins_pool,
        &owner.pubkey(),
        500,
        false,
    );
    let mut account = svm.get_account(&witness).unwrap();
    account.data[..8].copy_from_slice(b"SUBPOOL1");
    svm.set_account(witness, account).unwrap();

    assert!(
        register(
            &mut svm,
            &payer,
            &env,
            &owner,
            &owner.pubkey(),
            &witness,
            COHORT_INSURANCE,
        )
        .is_err(),
        "a Subledger-owned non-position account must not mint capital points"
    );
}

#[test]
fn portfolio_witness_kind_is_rechecked_at_crystallize_and_claim() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);

    let owner = Keypair::new();
    let portfolio = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &portfolio,
        &env.stub_perc,
        &env.market,
        &owner.pubkey(),
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &owner,
        &owner.pubkey(),
        &portfolio,
        COHORT_LP,
    )
    .expect("register a real portfolio");
    set_slot(&mut svm, 1_000);
    set_portfolio(
        &mut svm,
        &portfolio,
        &env.stub_perc,
        &env.market,
        &owner.pubkey(),
        10_000,
        0,
    );
    let mut wrong_kind = svm.get_account(&portfolio).unwrap();
    wrong_kind.data[10] = 1;
    svm.set_account(portfolio, wrong_kind).unwrap();
    assert!(
        crystallize(&mut svm, &payer, &env, &owner, &portfolio).is_err(),
        "a same-key non-portfolio account cannot enter the frozen denominator"
    );

    set_portfolio(
        &mut svm,
        &portfolio,
        &env.stub_perc,
        &env.market,
        &owner.pubkey(),
        10_000,
        0,
    );
    crystallize(&mut svm, &payer, &env, &owner, &portfolio)
        .expect("the restored portfolio crystallizes");
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");
    let recipient = create_token_account(&mut svm, &payer, &env.coin_mint, &owner.pubkey());
    let mut wrong_kind = svm.get_account(&portfolio).unwrap();
    wrong_kind.data[10] = 1;
    svm.set_account(portfolio, wrong_kind).unwrap();
    assert!(
        claim(&mut svm, &payer, &env, &owner, &recipient, None).is_err(),
        "a same-key non-portfolio account cannot satisfy the live claim cap"
    );
    assert_eq!(token_amount(&svm, &recipient), 0);

    set_portfolio(
        &mut svm,
        &portfolio,
        &env.stub_perc,
        &env.market,
        &owner.pubkey(),
        10_000,
        0,
    );
    claim(&mut svm, &payer, &env, &owner, &recipient, None)
        .expect("the restored portfolio claims its frozen share");
    assert!(token_amount(&svm, &recipient) > 0);
}

#[test]
fn capital_witness_kind_is_rechecked_at_crystallize_and_claim() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);

    let owner = Keypair::new();
    let position = Pubkey::new_unique();
    set_position(
        &mut svm,
        &position,
        &env.stub_sub,
        &env.ins_pool,
        &owner.pubkey(),
        500,
        false,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &owner,
        &owner.pubkey(),
        &position,
        COHORT_INSURANCE,
    )
    .expect("register a real position");
    set_slot(&mut svm, 1_000);
    let mut wrong_kind = svm.get_account(&position).unwrap();
    wrong_kind.data[..8].copy_from_slice(b"SUBPOOL1");
    svm.set_account(position, wrong_kind).unwrap();
    assert!(
        crystallize(&mut svm, &payer, &env, &owner, &position).is_err(),
        "a same-key non-position account cannot enter the frozen denominator"
    );

    set_position(
        &mut svm,
        &position,
        &env.stub_sub,
        &env.ins_pool,
        &owner.pubkey(),
        500,
        false,
    );
    crystallize(&mut svm, &payer, &env, &owner, &position)
        .expect("the restored position crystallizes");
    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze");
    let recipient = create_token_account(&mut svm, &payer, &env.coin_mint, &owner.pubkey());
    let mut wrong_kind = svm.get_account(&position).unwrap();
    wrong_kind.data[..8].copy_from_slice(b"SUBPOOL1");
    svm.set_account(position, wrong_kind).unwrap();
    assert!(
        claim(&mut svm, &payer, &env, &owner, &recipient, Some(&position)).is_err(),
        "a same-key non-position account cannot satisfy the live claim cap"
    );
    assert_eq!(token_amount(&svm, &recipient), 0);

    set_position(
        &mut svm,
        &position,
        &env.stub_sub,
        &env.ins_pool,
        &owner.pubkey(),
        500,
        false,
    );
    claim(&mut svm, &payer, &env, &owner, &recipient, Some(&position))
        .expect("the restored position claims its frozen share");
    assert!(token_amount(&svm, &recipient) > 0);
}

#[test]
fn register_rejects_out_of_range_cohort_cross_program_and_double_register() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);
    let alice = Keypair::new();

    // (1) cohort 5 > COHORT_FUNDING_PAYER(4) -> rejected (the linked account isn't even read).
    let any = Pubkey::new_unique();
    assert!(
        register(&mut svm, &payer, &env, &alice, &alice.pubkey(), &any, 5).is_err(),
        "out-of-range cohort must reject"
    );

    // (2) cross-program type confusion: insurance cohort pointed at a PERCOLATOR-owned account is rejected
    // (owner program != subledger_program), and symmetrically an LP cohort at a SUBLEDGER position.
    let perc_acct = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &perc_acct,
        &env.stub_perc,
        &env.market,
        &alice.pubkey(),
        5_000,
        0,
    );
    assert!(
        register(
            &mut svm,
            &payer,
            &env,
            &alice,
            &alice.pubkey(),
            &perc_acct,
            COHORT_INSURANCE
        )
        .is_err(),
        "insurance cohort must reject a percolator-owned account (wrong program)"
    );
    let sub_acct = Pubkey::new_unique();
    set_position(
        &mut svm,
        &sub_acct,
        &env.stub_sub,
        &env.ins_pool,
        &alice.pubkey(),
        500,
        false,
    );
    assert!(
        register(
            &mut svm,
            &payer,
            &env,
            &alice,
            &alice.pubkey(),
            &sub_acct,
            COHORT_LP
        )
        .is_err(),
        "lp cohort must reject a subledger-owned position (wrong program)"
    );

    // (3) double-register for the same owner (stake PDA now initialized) -> rejected.
    let pf = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &alice.pubkey(),
        0,
        0,
    );
    register(
        &mut svm,
        &payer,
        &env,
        &alice,
        &alice.pubkey(),
        &pf,
        COHORT_LP,
    )
    .expect("first register ok");
    assert!(
        register(
            &mut svm,
            &payer,
            &env,
            &alice,
            &alice.pubkey(),
            &pf,
            COHORT_LP
        )
        .is_err(),
        "double-register (stake PDA already initialized) must reject"
    );
}

// CROSS-COHORT DOUBLE-DIP (sweep tick D — wash/free-farm): a single percolator portfolio that BOTH
// provided liquidity (`received` > 0, the LP-cohort counter) AND took a directional loss
// (`crystallized - spent` > 0, the trader-cohort counter) represents activity the LP and trader cohorts
// each reward from a SEPARATE supply slice (10%/40%). If the SAME owner could register that one portfolio
// under BOTH cohorts, they'd farm two cohort shares for one portfolio's economics — a free extra slice with
// no extra capital at risk. The defense is structural: LP and trader map to the SAME residual reward-family
// byte in `[b"rd_stake", config, owner, linked_account, reward_family]`, so one linked portfolio can only have
// one residual stake; the second register lands on the already-initialized PDA and is rejected by the
// data_len()!=0 guard. Pin that an owner who legitimately registers their own dual-activity portfolio as
// TRADER cannot then also register it as LP (the economically-distinct cross-cohort case the same-cohort
// double-register above does not exercise). Real .so.
#[test]
fn register_same_owner_cannot_double_dip_lp_and_trader_cohorts_for_one_portfolio() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);

    let alice = Keypair::new();
    // Alice's own portfolio in the allow-listed genesis market has BOTH legs populated: it received LP
    // residual (9_000) AND crystallized a directional loss (6_000) — both cohort counters are positive.
    let pf = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &alice.pubkey(),
        9_000,
        6_000,
    );

    // She is the rightful owner, so the first registration (TRADER cohort) succeeds.
    register(
        &mut svm,
        &payer,
        &env,
        &alice,
        &alice.pubkey(),
        &pf,
        COHORT_TRADER,
    )
    .expect("owner registers her dual-activity portfolio once (trader cohort)");

    // The double-dip: register the SAME portfolio under the LP cohort to also claim the LP supply slice.
    // LP and trader share the residual-family seed, so this targets the SAME, now-occupied rd_stake PDA ->
    // rejected. One linked portfolio gets one residual supply slice; the LP `received` leg cannot be farmed
    // on top of the trader leg.
    assert!(register(&mut svm, &payer, &env, &alice, &alice.pubkey(), &pf, COHORT_LP).is_err(),
        "same owner cannot register one portfolio under BOTH the trader and LP cohorts (cross-cohort double-dip)");
    // And the reverse order is symmetric: a fresh owner registering LP first cannot then add a trader stake.
    let bob = Keypair::new();
    let pf2 = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &pf2,
        &env.stub_perc,
        &env.market,
        &bob.pubkey(),
        9_000,
        6_000,
    );
    register(&mut svm, &payer, &env, &bob, &bob.pubkey(), &pf2, COHORT_LP)
        .expect("bob registers LP first");
    assert!(
        register(
            &mut svm,
            &payer,
            &env,
            &bob,
            &bob.pubkey(),
            &pf2,
            COHORT_TRADER
        )
        .is_err(),
        "and LP-first cannot be topped up with a trader stake on the same single PDA either"
    );
}

// REWARD-LOSS / LIVENESS PROBE: LP/trader residual flow and paid funding are
// independent reward families. A normal portfolio can legitimately have both a
// residual-loss counter and paid-funding counters, so choosing one family must not
// make the other configured supply slice unreachable. LP and trader still share a
// family and remain mutually exclusive for the same linked portfolio.
#[test]
fn one_portfolio_can_register_residual_and_funding_reward_families() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env =
        setup_custom_split_with_fee_and_extras(&mut svm, &payer, 1_000_000, 0, 0, 0, 8_000, 0, &[]); // trader remainder = 20%, funding-payer = 80%
    set_slot(&mut svm, 100);

    let owner = Keypair::new();
    let portfolio = Pubkey::new_unique();
    set_portfolio_funding(
        &mut svm,
        &portfolio,
        &env.stub_perc,
        &env.market,
        &owner.pubkey(),
        0,
        0,
        0,
        0,
    );

    register(
        &mut svm,
        &payer,
        &env,
        &owner,
        &owner.pubkey(),
        &portfolio,
        COHORT_TRADER,
    )
    .expect("portfolio registers its residual reward family");
    register(
        &mut svm,
        &payer,
        &env,
        &owner,
        &owner.pubkey(),
        &portfolio,
        COHORT_FUNDING_PAYER,
    )
    .expect("the same portfolio also registers its independent funding reward family");

    set_slot(&mut svm, 1_500);
    let mut live = svm.get_account(&portfolio).unwrap();
    live.data[196..212].copy_from_slice(&10_000u128.to_le_bytes()); // crystallized loss
    live.data[244..260].copy_from_slice(&40_000u128.to_le_bytes()); // long funding paid
    svm.set_account(portfolio, live).unwrap();
    crystallize_cohort(
        &mut svm,
        &payer,
        &env,
        &owner,
        &owner.pubkey(),
        &portfolio,
        COHORT_TRADER,
    )
    .expect("residual family crystallizes");
    crystallize_cohort(
        &mut svm,
        &payer,
        &env,
        &owner,
        &owner.pubkey(),
        &portfolio,
        COHORT_FUNDING_PAYER,
    )
    .expect("funding family crystallizes");

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze both reward families");
    let ata = create_token_account(&mut svm, &payer, &env.coin_mint, &owner.pubkey());
    claim_cohort(
        &mut svm,
        &payer,
        &env,
        &owner,
        &ata,
        &portfolio,
        COHORT_TRADER,
    )
    .expect("claim the 20% residual slice");
    claim_cohort(
        &mut svm,
        &payer,
        &env,
        &owner,
        &ata,
        &portfolio,
        COHORT_FUNDING_PAYER,
    )
    .expect("claim the 80% funding slice");
    assert_eq!(
        token_amount(&svm, &ata),
        1_000_000,
        "one portfolio receives both independently configured reward families"
    );
}

// REWARD-LOSS / LIVENESS PROBE: the market allow-list permits one owner to have
// legitimate portfolios in multiple markets. Stake identity must include the linked
// portfolio as well as the reward family, or the second funding portfolio collides
// with the first and its paid-funding points become unclaimable.
#[test]
fn one_owner_can_claim_one_funding_family_across_two_linked_portfolios() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let extra_market = Pubkey::new_unique();
    let env = setup_funding_payer_only_with_fee_and_extras(
        &mut svm,
        &payer,
        1_000_000,
        0,
        &[extra_market],
    );
    set_slot(&mut svm, 100);

    let owner = Keypair::new();
    let primary = Pubkey::new_unique();
    let extra = Pubkey::new_unique();
    for (portfolio, market) in [(primary, env.market), (extra, extra_market)] {
        set_portfolio_funding(
            &mut svm,
            &portfolio,
            &env.stub_perc,
            &market,
            &owner.pubkey(),
            0,
            0,
            0,
            0,
        );
        register(
            &mut svm,
            &payer,
            &env,
            &owner,
            &owner.pubkey(),
            &portfolio,
            COHORT_FUNDING_PAYER,
        )
        .expect("each allow-listed linked portfolio gets its own funding stake");
    }

    set_slot(&mut svm, 1_500);
    set_portfolio_funding(
        &mut svm,
        &primary,
        &env.stub_perc,
        &env.market,
        &owner.pubkey(),
        10_000,
        0,
        0,
        0,
    );
    set_portfolio_funding(
        &mut svm,
        &extra,
        &env.stub_perc,
        &extra_market,
        &owner.pubkey(),
        0,
        0,
        10_000,
        0,
    );
    for portfolio in [primary, extra] {
        crystallize_cohort(
            &mut svm,
            &payer,
            &env,
            &owner,
            &owner.pubkey(),
            &portfolio,
            COHORT_FUNDING_PAYER,
        )
        .expect("both funding stakes crystallize independently");
    }

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).expect("freeze both linked contributions");
    let ata = create_token_account(&mut svm, &payer, &env.coin_mint, &owner.pubkey());
    for portfolio in [primary, extra] {
        claim_cohort(
            &mut svm,
            &payer,
            &env,
            &owner,
            &ata,
            &portfolio,
            COHORT_FUNDING_PAYER,
        )
        .expect("both linked stakes claim independently");
    }
    assert_eq!(
        token_amount(&svm, &ata),
        1_000_000,
        "equal paid-funding points across two portfolios split and exhaust the cohort"
    );
}

// Self-service finalize lifecycle guards: freeze is rejected before emission_end+finalize_window (else a
// permissionless caller could freeze early and forfeit slow backers' un-crystallized points); after freeze,
// register and crystallize are closed (else the frozen denominator could be diluted/altered); and a
// double-freeze is rejected (snapshot + bound vault are immutable). Real .so.
#[test]
fn self_service_lifecycle_guards_freeze_window_and_post_freeze_closure() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let env = setup(&mut svm, &payer, 1_000_000);
    set_slot(&mut svm, 100);

    // an LP backer registers + crystallizes during the accrual phase.
    let lp = Keypair::new();
    let pf = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        0,
        0,
    );
    register(&mut svm, &payer, &env, &lp, &lp.pubkey(), &pf, COHORT_LP).expect("register");
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        5_000,
        0,
    );
    crystallize(&mut svm, &payer, &env, &lp, &pf).expect("crystallize");

    // (1) freeze at the LAST in-window slot (emission_end + finalize_window - 1) is rejected. This
    // preserves the full reduce-only trader cleanup window before denominators become immutable.
    set_slot(&mut svm, env.emission_end + env.finalize_window - 1);
    assert!(
        freeze(&mut svm, &payer, &env).is_err(),
        "freeze at window_end - 1 must reject — the finalize window is still open"
    );

    // EXACTLY emission_end + finalize_window is the FIRST slot freeze is permitted (inclusive cutoff), one-shot.
    set_slot(&mut svm, env.emission_end + env.finalize_window);
    freeze(&mut svm, &payer, &env)
        .expect("freeze succeeds at exactly emission_end + finalize_window (first valid slot)");

    // (2) register is closed after freeze (would dilute the frozen denominator).
    let late = Keypair::new();
    let late_pf = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &late_pf,
        &env.stub_perc,
        &env.market,
        &late.pubkey(),
        9_000,
        0,
    );
    assert!(
        register(
            &mut svm,
            &payer,
            &env,
            &late,
            &late.pubkey(),
            &late_pf,
            COHORT_LP
        )
        .is_err(),
        "register after freeze must reject"
    );

    // (3) crystallize is closed after freeze (would alter the frozen denominator).
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        99_000,
        0,
    );
    assert!(
        crystallize(&mut svm, &payer, &env, &lp, &pf).is_err(),
        "crystallize after freeze must reject"
    );

    // (4) double-freeze is rejected (snapshot + bound vault immutable).
    assert!(
        freeze(&mut svm, &payer, &env).is_err(),
        "double-freeze must reject"
    );
}

// ATTACK PROBE (cohort over-allocation -> FCFS-strand LOF at init). Each cohort's COIN budget is
// `total_supply * bps / 10000` and the rd vault holds exactly `total_supply` (enforced at freeze,
// lib.rs:907). If the four cohort shares could sum to MORE than 100%, then Sum(cohort_supply) > vault: the
// cohorts that claim first drain the vault and the last cohort's honest claimants get InsufficientFunds —
// an order-dependent strand (the residual-side analogue of the distribution under-funded-seal LOF). The
// sole defense is the init guard `insurance_bps + backing_bps + lp_bps > 10000 -> reject` (lib.rs:579),
// with trader taking the saturating remainder so the four always sum to <= 10000. The conservation test
// below proves the in-bounds case; this pins the REJECTION of an over-allocating wire. Real rd .so.
#[test]
fn init_rejects_cohort_bps_summing_above_one_hundred_percent_no_overallocation() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64;
    let mint_auth = Keypair::new();
    let coin_mint = create_mint(&mut svm, &payer, &mint_auth.pubkey());
    let rd_config = Pubkey::find_program_address(&[b"rd_config", coin_mint.as_ref()], &rd_id()).0;
    let dist_config = dist_config_pda(&coin_mint, &rd_config);
    let vault = create_token_account(&mut svm, &payer, &coin_mint, &rd_config);
    mint_to(&mut svm, &payer, &coin_mint, &mint_auth, &vault, supply);
    let (stub_sub, stub_perc, ins_pool, back_pool, market) = (
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    );
    // ATTACK WIRE: insurance 50% + backing 50% + lp 50% = 150% (trader would saturate to 0). Sum > 100%.
    let mut d = vec![0u8];
    d.extend_from_slice(&supply.to_le_bytes());
    d.extend_from_slice(&2_000u64.to_le_bytes());
    d.extend_from_slice(&5_000u16.to_le_bytes()); // insurance 50%
    d.extend_from_slice(&5_000u16.to_le_bytes()); // backing 50%
    d.extend_from_slice(&5_000u16.to_le_bytes()); // lp 50% -> sum 150% > 10000
    d.extend_from_slice(&500u64.to_le_bytes());
    d.extend_from_slice(ins_pool.as_ref());
    d.extend_from_slice(back_pool.as_ref());
    d.extend_from_slice(market.as_ref());
    d.extend_from_slice(&[0u8]);
    let r = send(
        &mut svm,
        &payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: rd_init_accounts(
                payer.pubkey(),
                coin_mint,
                dist_config,
                stub_perc,
                stub_sub,
                rd_config,
                mint_auth.pubkey(),
            ),
            data: d,
        }],
        &[&mint_auth],
    );
    assert!(
        r.is_err(),
        "init with cohort bps summing to 150% must be rejected (no over-allocation)"
    );

    // CONTROL: the same wire with shares summing to EXACTLY 100% (50/30/20/0) initializes fine — the guard
    // rejects only the OVER-allocation, never a valid full split.
    let mut d = vec![0u8];
    d.extend_from_slice(&supply.to_le_bytes());
    d.extend_from_slice(&2_000u64.to_le_bytes());
    d.extend_from_slice(&5_000u16.to_le_bytes()); // insurance 50%
    d.extend_from_slice(&3_000u16.to_le_bytes()); // backing 30%
    d.extend_from_slice(&2_000u16.to_le_bytes()); // lp 20% -> sum 100%, trader 0
    d.extend_from_slice(&500u64.to_le_bytes());
    d.extend_from_slice(ins_pool.as_ref());
    d.extend_from_slice(back_pool.as_ref());
    d.extend_from_slice(market.as_ref());
    d.extend_from_slice(&[0u8]);
    let r = send(
        &mut svm,
        &payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: rd_init_accounts(
                payer.pubkey(),
                coin_mint,
                dist_config,
                stub_perc,
                stub_sub,
                rd_config,
                mint_auth.pubkey(),
            ),
            data: d,
        }],
        &[&mint_auth],
    );
    assert!(
        r.is_ok(),
        "a valid 100% split (50/30/20/0) must initialize — the guard rejects only over-allocation"
    );
}

// CONSERVATION property pin: across ALL FOUR cohorts with many stakes and deliberately NON-even point
// splits (so floor rounding leaves dust), the sum of claims must never exceed any cohort's supply nor the
// total supply, and the vault must be drained by EXACTLY the claimed total — never over-drawn. Real .so.
#[test]
fn cross_cohort_claims_never_exceed_cohort_or_total_supply() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 1_000_000_000_000).unwrap();
    let supply = 1_000_000u64; // ins 10% =100k, back 10% =100k, lp 40% =400k, trader 40% =400k
    let env = setup(&mut svm, &payer, supply);
    set_slot(&mut svm, 100);

    // Deliberately non-dividing denominators -> floor dust (Σ < cohort_supply) for several cohorts.
    let ins: Vec<(Keypair, Pubkey, u128)> = vec![
        (Keypair::new(), Pubkey::new_unique(), 1),
        (Keypair::new(), Pubkey::new_unique(), 1),
        (Keypair::new(), Pubkey::new_unique(), 1),
    ];
    let back: Vec<(Keypair, Pubkey, u128)> = vec![
        (Keypair::new(), Pubkey::new_unique(), 200),
        (Keypair::new(), Pubkey::new_unique(), 800),
    ];
    let lp: Vec<(Keypair, Pubkey, u128)> = vec![
        (Keypair::new(), Pubkey::new_unique(), 1_000),
        (Keypair::new(), Pubkey::new_unique(), 3_000),
        (Keypair::new(), Pubkey::new_unique(), 7),
    ];
    let trd: Vec<(Keypair, Pubkey, u128)> = vec![
        (Keypair::new(), Pubkey::new_unique(), 333),
        (Keypair::new(), Pubkey::new_unique(), 333),
        (Keypair::new(), Pubkey::new_unique(), 334),
    ];

    for (o, pos, shares) in &ins {
        set_position(
            &mut svm,
            pos,
            &env.stub_sub,
            &env.ins_pool,
            &o.pubkey(),
            *shares,
            false,
        );
        register(
            &mut svm,
            &payer,
            &env,
            o,
            &o.pubkey(),
            pos,
            COHORT_INSURANCE,
        )
        .unwrap();
        crystallize(&mut svm, &payer, &env, o, pos).unwrap();
    }
    for (o, pos, shares) in &back {
        set_position(
            &mut svm,
            pos,
            &env.stub_sub,
            &env.back_pool,
            &o.pubkey(),
            *shares,
            false,
        );
        register(&mut svm, &payer, &env, o, &o.pubkey(), pos, COHORT_BACKING).unwrap();
        crystallize(&mut svm, &payer, &env, o, pos).unwrap();
    }
    for (o, pf, recv) in &lp {
        set_portfolio(
            &mut svm,
            pf,
            &env.stub_perc,
            &env.market,
            &o.pubkey(),
            *recv,
            0,
        );
        register(&mut svm, &payer, &env, o, &o.pubkey(), pf, COHORT_LP).unwrap();
        crystallize(&mut svm, &payer, &env, o, pf).unwrap();
    }
    for (o, pf, cryst) in &trd {
        set_portfolio(
            &mut svm,
            pf,
            &env.stub_perc,
            &env.market,
            &o.pubkey(),
            0,
            *cryst,
        );
        register(&mut svm, &payer, &env, o, &o.pubkey(), pf, COHORT_TRADER).unwrap();
        crystallize(&mut svm, &payer, &env, o, pf).unwrap();
    }

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    freeze(&mut svm, &payer, &env).unwrap();

    let mut claim_cohort =
        |svm: &mut LiteSVM, members: &[(Keypair, Pubkey, u128)], share_value: bool| -> u64 {
            let mut sum = 0u64;
            for (o, linked, _) in members {
                let ata = create_token_account(svm, &payer, &env.coin_mint, &o.pubkey());
                claim(
                    svm,
                    &payer,
                    &env,
                    o,
                    &ata,
                    if share_value { Some(linked) } else { None },
                )
                .expect("claim");
                sum += token_amount(svm, &ata);
            }
            sum
        };
    let ins_sum = claim_cohort(&mut svm, &ins, true);
    let back_sum = claim_cohort(&mut svm, &back, true);
    let lp_sum = claim_cohort(&mut svm, &lp, false);
    let trd_sum = claim_cohort(&mut svm, &trd, false);

    let cs = |bps: u128| (supply as u128 * bps / 10_000) as u64;
    assert!(ins_sum <= cs(1_000), "insurance Σ <= cohort supply");
    assert!(back_sum <= cs(1_000), "backing Σ <= cohort supply");
    assert!(lp_sum <= cs(4_000), "lp Σ <= cohort supply");
    assert!(trd_sum <= cs(4_000), "trader Σ <= cohort supply");
    let total = ins_sum + back_sum + lp_sum + trd_sum;
    assert!(
        total <= supply,
        "total claims never exceed the fixed supply"
    );
    assert_eq!(
        token_amount(&svm, &env.vault),
        supply - total,
        "vault drained by EXACTLY the claimed total — never over"
    );
    // the non-even insurance split (3 equal shares, denom 3) must leave floor dust, proving Σ < cohort_supply.
    assert!(
        ins_sum < cs(1_000),
        "floor rounding leaves dust: Σ strictly under cohort supply"
    );
}

// CROSS-COHORT 100-CASE LIFECYCLE SWEEP: mixes tiny supplies, odd bps splits, zero/100% portfolio-flow
// fees, zero-bps cohorts, stale live caps, idle zero-point stakes, receiver-only funding canaries, foreign
// capital claim attempts, permissionless portfolio-flow claims, and varied claim order. This is the
// broad "weird branch" conservation probe: each case must pay only the expected capped/fee-adjusted amount,
// never let idle/receiver-only stakes farm, never let a foreign cranker force a capital claim, and never
// overdraw the vault regardless of claim order.
#[test]
fn cross_cohort_100_case_lifecycle_sweep_no_overdraw_or_free_farm() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 2_000_000_000_000).unwrap();

    let splits: [(u16, u16, u16, u16); 10] = [
        (0, 0, 0, 0),
        (10_000, 0, 0, 0),
        (0, 10_000, 0, 0),
        (0, 0, 10_000, 0),
        (0, 0, 0, 10_000),
        (1, 1, 1, 1),
        (333, 777, 1_111, 2_222),
        (5_000, 0, 0, 5_000),
        (2_500, 2_500, 2_500, 2_500),
        (9_999, 0, 0, 1),
    ];
    let fees: [u16; 10] = [0, 1, 7, 33, 333, 2_500, 5_000, 7_777, 9_999, 10_000];
    let supplies: [u64; 8] = [1, 2, 3, 7, 97, 10_003, 250_001, 1_000_003];
    let cohort_supply =
        |supply: u64, bps: u16| -> u64 { ((supply as u128) * (bps as u128) / 10_000) as u64 };
    let ceil_fee = |amount: u64, fee_bps: u16| -> u64 {
        (((amount as u128) * (fee_bps as u128) + 9_999) / 10_000) as u64
    };
    let flow_payout = |amount: u64, fee_bps: u16| -> u64 { amount - ceil_fee(amount, fee_bps) };

    for case_idx in 0..100usize {
        let (insurance_bps, backing_bps, lp_bps, funding_bps) = splits[case_idx % splits.len()];
        let trader_bps = 10_000u16
            .saturating_sub(insurance_bps)
            .saturating_sub(backing_bps)
            .saturating_sub(lp_bps)
            .saturating_sub(funding_bps);
        let fee_bps = fees[(case_idx / splits.len()) % fees.len()];
        let supply = supplies[(case_idx * 3) % supplies.len()] + (case_idx as u64 % 3);
        let extras: Vec<Pubkey> = if case_idx % 4 == 0 {
            vec![Pubkey::new_unique(), Pubkey::new_unique()]
        } else {
            Vec::new()
        };
        let env = setup_custom_split_with_fee_and_extras(
            &mut svm,
            &payer,
            supply,
            insurance_bps,
            backing_bps,
            lp_bps,
            funding_bps,
            fee_bps,
            &extras,
        );
        let market = if extras.is_empty() {
            env.market
        } else {
            extras[case_idx % extras.len()]
        };

        let attacker = Keypair::new();
        svm.airdrop(&attacker.pubkey(), 1_000_000_000).unwrap();
        let ins = Keypair::new();
        let ins_idle = Keypair::new();
        let back = Keypair::new();
        let back_idle = Keypair::new();
        let lp = Keypair::new();
        let lp_idle = Keypair::new();
        let trader = Keypair::new();
        let trader_idle = Keypair::new();
        let funder = Keypair::new();
        let fund_receiver = Keypair::new();

        let ins_pos = Pubkey::new_unique();
        let ins_idle_pos = Pubkey::new_unique();
        let back_pos = Pubkey::new_unique();
        let back_idle_pos = Pubkey::new_unique();
        let lp_pf = Pubkey::new_unique();
        let lp_idle_pf = Pubkey::new_unique();
        let trader_pf = Pubkey::new_unique();
        let trader_idle_pf = Pubkey::new_unique();
        let funding_pf = Pubkey::new_unique();
        let funding_receiver_pf = Pubkey::new_unique();

        let start_slot = 100 + case_idx as u64;
        set_slot(&mut svm, start_slot);

        let ins_shares = ((case_idx as u128 * 13) % 5_000) + 1;
        let back_shares = ((case_idx as u128 * 17) % 7_000) + 1;
        set_position(
            &mut svm,
            &ins_pos,
            &env.stub_sub,
            &env.ins_pool,
            &ins.pubkey(),
            ins_shares,
            false,
        );
        set_position(
            &mut svm,
            &ins_idle_pos,
            &env.stub_sub,
            &env.ins_pool,
            &ins_idle.pubkey(),
            ins_shares + 11,
            false,
        );
        set_position(
            &mut svm,
            &back_pos,
            &env.stub_sub,
            &env.back_pool,
            &back.pubkey(),
            back_shares,
            false,
        );
        set_position(
            &mut svm,
            &back_idle_pos,
            &env.stub_sub,
            &env.back_pool,
            &back_idle.pubkey(),
            back_shares + 13,
            false,
        );
        register(
            &mut svm,
            &payer,
            &env,
            &ins,
            &ins.pubkey(),
            &ins_pos,
            COHORT_INSURANCE,
        )
        .expect("register active insurance");
        register(
            &mut svm,
            &payer,
            &env,
            &ins_idle,
            &ins_idle.pubkey(),
            &ins_idle_pos,
            COHORT_INSURANCE,
        )
        .expect("register idle insurance");
        register(
            &mut svm,
            &payer,
            &env,
            &back,
            &back.pubkey(),
            &back_pos,
            COHORT_BACKING,
        )
        .expect("register active backing");
        register(
            &mut svm,
            &payer,
            &env,
            &back_idle,
            &back_idle.pubkey(),
            &back_idle_pos,
            COHORT_BACKING,
        )
        .expect("register idle backing");

        let lp_snap = ((case_idx as u128 * 19) % 1_000) + 1;
        let lp_delta = ((case_idx as u128 * 23) % 20_000) + 1;
        let trader_delta = ((case_idx as u128 * 29) % 15_000) + 1;
        let funding_long_snap = ((case_idx as u128 * 31) % 3_000) + 1;
        let funding_short_snap = ((case_idx as u128 * 37) % 3_000) + 1;
        let funding_delta = ((case_idx as u128 * 41) % 25_000) + 1;
        let funding_long_delta = funding_delta * (((case_idx % 5) + 1) as u128) / 6;
        let funding_short_delta = funding_delta - funding_long_delta;

        set_portfolio(
            &mut svm,
            &lp_pf,
            &env.stub_perc,
            &market,
            &lp.pubkey(),
            lp_snap,
            0,
        );
        set_portfolio(
            &mut svm,
            &lp_idle_pf,
            &env.stub_perc,
            &market,
            &lp_idle.pubkey(),
            lp_snap + 77,
            0,
        );
        set_portfolio_full(
            &mut svm,
            &trader_pf,
            &env.stub_perc,
            &market,
            &trader.pubkey(),
            0,
            0,
            0,
        );
        set_portfolio_full(
            &mut svm,
            &trader_idle_pf,
            &env.stub_perc,
            &market,
            &trader_idle.pubkey(),
            0,
            trader_delta + 55,
            0,
        );
        set_portfolio_funding(
            &mut svm,
            &funding_pf,
            &env.stub_perc,
            &market,
            &funder.pubkey(),
            funding_long_snap,
            0,
            funding_short_snap,
            0,
        );
        set_portfolio_funding(
            &mut svm,
            &funding_receiver_pf,
            &env.stub_perc,
            &market,
            &fund_receiver.pubkey(),
            0,
            funding_delta + 90_000,
            0,
            funding_delta + 80_000,
        );

        register(&mut svm, &payer, &env, &lp, &lp.pubkey(), &lp_pf, COHORT_LP)
            .expect("register active lp");
        register(
            &mut svm,
            &payer,
            &env,
            &lp_idle,
            &lp_idle.pubkey(),
            &lp_idle_pf,
            COHORT_LP,
        )
        .expect("register idle lp");
        register(
            &mut svm,
            &payer,
            &env,
            &trader,
            &trader.pubkey(),
            &trader_pf,
            COHORT_TRADER,
        )
        .expect("register active trader");
        register(
            &mut svm,
            &payer,
            &env,
            &trader_idle,
            &trader_idle.pubkey(),
            &trader_idle_pf,
            COHORT_TRADER,
        )
        .expect("register idle trader");
        register(
            &mut svm,
            &payer,
            &env,
            &funder,
            &funder.pubkey(),
            &funding_pf,
            COHORT_FUNDING_PAYER,
        )
        .expect("register active funding payer");
        register(
            &mut svm,
            &payer,
            &env,
            &fund_receiver,
            &fund_receiver.pubkey(),
            &funding_receiver_pf,
            COHORT_FUNDING_PAYER,
        )
        .expect("register receiver-only funding canary");

        let tenure = 64 + ((case_idx as u64 * 17) % 512);
        set_slot(&mut svm, start_slot + tenure);
        set_portfolio(
            &mut svm,
            &lp_pf,
            &env.stub_perc,
            &market,
            &lp.pubkey(),
            lp_snap + lp_delta,
            0,
        );
        set_portfolio_full(
            &mut svm,
            &trader_pf,
            &env.stub_perc,
            &market,
            &trader.pubkey(),
            0,
            trader_delta,
            0,
        );
        set_portfolio_funding(
            &mut svm,
            &funding_pf,
            &env.stub_perc,
            &market,
            &funder.pubkey(),
            funding_long_snap + funding_long_delta,
            0,
            funding_short_snap + funding_short_delta,
            0,
        );
        set_portfolio_funding(
            &mut svm,
            &funding_receiver_pf,
            &env.stub_perc,
            &market,
            &fund_receiver.pubkey(),
            0,
            funding_delta + 120_000,
            0,
            funding_delta + 130_000,
        );

        crystallize(&mut svm, &payer, &env, &ins, &ins_pos).expect("crystallize active insurance");
        crystallize(&mut svm, &payer, &env, &back, &back_pos).expect("crystallize active backing");
        crystallize_as(&mut svm, &payer, &env, &attacker, &lp.pubkey(), &lp_pf)
            .expect("permissionless lp crystallize");
        crystallize_as(
            &mut svm,
            &payer,
            &env,
            &attacker,
            &trader.pubkey(),
            &trader_pf,
        )
        .expect("permissionless trader crystallize");
        crystallize_as(
            &mut svm,
            &payer,
            &env,
            &attacker,
            &funder.pubkey(),
            &funding_pf,
        )
        .expect("permissionless funding-payer crystallize");
        crystallize(&mut svm, &payer, &env, &fund_receiver, &funding_receiver_pf)
            .expect("receiver-only funding crystallize");

        set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
        freeze(&mut svm, &payer, &env).expect("freeze");

        let ins_live = if case_idx % 5 == 0 {
            0
        } else if case_idx % 5 == 1 {
            ins_shares / 2
        } else {
            ins_shares
        };
        let back_live = if case_idx % 7 == 0 {
            0
        } else if case_idx % 7 == 1 {
            back_shares / 3
        } else {
            back_shares
        };
        set_position(
            &mut svm,
            &ins_pos,
            &env.stub_sub,
            &env.ins_pool,
            &ins.pubkey(),
            ins_live,
            case_idx % 5 == 0,
        );
        set_position(
            &mut svm,
            &back_pos,
            &env.stub_sub,
            &env.back_pool,
            &back.pubkey(),
            back_live,
            case_idx % 7 == 0,
        );
        let lp_live_delta = if case_idx % 6 == 0 {
            lp_delta / 2
        } else {
            lp_delta
        };
        set_portfolio(
            &mut svm,
            &lp_pf,
            &env.stub_perc,
            &market,
            &lp.pubkey(),
            lp_snap + lp_live_delta,
            0,
        );
        let trader_spent_after = if case_idx % 4 == 0 {
            trader_delta
        } else if case_idx % 4 == 1 {
            trader_delta / 2
        } else {
            0
        };
        set_portfolio_full(
            &mut svm,
            &trader_pf,
            &env.stub_perc,
            &market,
            &trader.pubkey(),
            0,
            trader_delta,
            trader_spent_after,
        );

        let ins_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &ins.pubkey());
        let ins_idle_ata =
            create_token_account(&mut svm, &payer, &env.coin_mint, &ins_idle.pubkey());
        let back_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &back.pubkey());
        let back_idle_ata =
            create_token_account(&mut svm, &payer, &env.coin_mint, &back_idle.pubkey());
        let lp_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &lp.pubkey());
        let lp_idle_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &lp_idle.pubkey());
        let trader_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &trader.pubkey());
        let trader_idle_ata =
            create_token_account(&mut svm, &payer, &env.coin_mint, &trader_idle.pubkey());
        let funding_ata = create_token_account(&mut svm, &payer, &env.coin_mint, &funder.pubkey());
        let funding_receiver_ata =
            create_token_account(&mut svm, &payer, &env.coin_mint, &fund_receiver.pubkey());

        assert!(
            claim_as(
                &mut svm,
                &payer,
                &env,
                &attacker,
                &ins.pubkey(),
                &ins_ata,
                Some(&ins_pos),
            )
            .is_err(),
            "case {case_idx}: foreign cranker cannot force an insurance claim"
        );
        assert_eq!(
            token_amount(&svm, &ins_ata),
            0,
            "case {case_idx}: rejected forced capital claim pays nothing"
        );

        if case_idx % 2 == 0 {
            claim(&mut svm, &payer, &env, &ins, &ins_ata, Some(&ins_pos))
                .expect("active insurance claim");
            claim(
                &mut svm,
                &payer,
                &env,
                &ins_idle,
                &ins_idle_ata,
                Some(&ins_idle_pos),
            )
            .expect("idle insurance claim");
            claim(&mut svm, &payer, &env, &back, &back_ata, Some(&back_pos))
                .expect("active backing claim");
            claim(
                &mut svm,
                &payer,
                &env,
                &back_idle,
                &back_idle_ata,
                Some(&back_idle_pos),
            )
            .expect("idle backing claim");
        }

        claim_as(
            &mut svm,
            &payer,
            &env,
            &attacker,
            &lp.pubkey(),
            &lp_ata,
            None,
        )
        .expect("permissionless lp claim to bound recipient");
        claim(&mut svm, &payer, &env, &lp_idle, &lp_idle_ata, None).expect("idle lp claim");
        claim_as(
            &mut svm,
            &payer,
            &env,
            &attacker,
            &trader.pubkey(),
            &trader_ata,
            None,
        )
        .expect("permissionless trader claim to bound recipient");
        claim(&mut svm, &payer, &env, &trader_idle, &trader_idle_ata, None)
            .expect("idle trader claim");
        claim_as(
            &mut svm,
            &payer,
            &env,
            &attacker,
            &funder.pubkey(),
            &funding_ata,
            None,
        )
        .expect("permissionless funding claim to bound recipient");
        claim_without_linked(
            &mut svm,
            &payer,
            &env,
            &fund_receiver,
            &funding_receiver_ata,
        )
        .expect("receiver-only funding claim without linked portfolio");

        if case_idx % 2 == 1 {
            claim(&mut svm, &payer, &env, &back, &back_ata, Some(&back_pos))
                .expect("active backing claim");
            claim(
                &mut svm,
                &payer,
                &env,
                &back_idle,
                &back_idle_ata,
                Some(&back_idle_pos),
            )
            .expect("idle backing claim");
            claim(&mut svm, &payer, &env, &ins, &ins_ata, Some(&ins_pos))
                .expect("active insurance claim");
            claim(
                &mut svm,
                &payer,
                &env,
                &ins_idle,
                &ins_idle_ata,
                Some(&ins_idle_pos),
            )
            .expect("idle insurance claim");
        }

        if case_idx % 17 == 0 {
            assert!(
                claim(&mut svm, &payer, &env, &lp, &lp_ata, None).is_err(),
                "case {case_idx}: active lp cannot double claim"
            );
            assert!(
                claim_without_linked(&mut svm, &payer, &env, &funder, &funding_ata).is_err(),
                "case {case_idx}: active funding payer cannot double claim"
            );
        }

        let ins_expected = ((cohort_supply(env.supply, insurance_bps) as u128)
            * core::cmp::min(ins_shares, ins_live)
            / ins_shares) as u64;
        let back_expected = ((cohort_supply(env.supply, backing_bps) as u128)
            * core::cmp::min(back_shares, back_live)
            / back_shares) as u64;
        let lp_amount =
            ((cohort_supply(env.supply, lp_bps) as u128) * lp_live_delta / lp_delta) as u64;
        let trader_live_delta = trader_delta.saturating_sub(trader_spent_after);
        let trader_amount = ((cohort_supply(env.supply, trader_bps) as u128) * trader_live_delta
            / trader_delta) as u64;
        let funding_amount = cohort_supply(env.supply, funding_bps);
        let lp_expected = flow_payout(lp_amount, fee_bps);
        let trader_expected = flow_payout(trader_amount, fee_bps);
        let funding_expected = flow_payout(funding_amount, fee_bps);

        assert_eq!(
            token_amount(&svm, &ins_ata),
            ins_expected,
            "case {case_idx}: insurance claim follows live share cap"
        );
        assert_eq!(
            token_amount(&svm, &back_ata),
            back_expected,
            "case {case_idx}: backing claim follows live share cap"
        );
        assert_eq!(
            token_amount(&svm, &lp_ata),
            lp_expected,
            "case {case_idx}: lp claim follows live cap and fee"
        );
        assert_eq!(
            token_amount(&svm, &trader_ata),
            trader_expected,
            "case {case_idx}: trader claim follows spent live cap and fee"
        );
        assert_eq!(
            token_amount(&svm, &funding_ata),
            funding_expected,
            "case {case_idx}: funding-payer claim follows funding bps and fee"
        );

        assert_eq!(
            token_amount(&svm, &ins_idle_ata),
            0,
            "case {case_idx}: idle insurance stake cannot dilute or farm"
        );
        assert_eq!(
            token_amount(&svm, &back_idle_ata),
            0,
            "case {case_idx}: idle backing stake cannot dilute or farm"
        );
        assert_eq!(
            token_amount(&svm, &lp_idle_ata),
            0,
            "case {case_idx}: idle lp stake cannot dilute or farm"
        );
        assert_eq!(
            token_amount(&svm, &trader_idle_ata),
            0,
            "case {case_idx}: idle trader stake cannot dilute or farm"
        );
        assert_eq!(
            token_amount(&svm, &funding_receiver_ata),
            0,
            "case {case_idx}: receiver-only funding stake cannot farm"
        );

        let total_paid = token_amount(&svm, &ins_ata)
            + token_amount(&svm, &ins_idle_ata)
            + token_amount(&svm, &back_ata)
            + token_amount(&svm, &back_idle_ata)
            + token_amount(&svm, &lp_ata)
            + token_amount(&svm, &lp_idle_ata)
            + token_amount(&svm, &trader_ata)
            + token_amount(&svm, &trader_idle_ata)
            + token_amount(&svm, &funding_ata)
            + token_amount(&svm, &funding_receiver_ata);
        assert!(
            total_paid <= env.supply,
            "case {case_idx}: all claims stay within fixed supply"
        );
        assert_eq!(
            token_amount(&svm, &env.vault),
            env.supply - total_paid,
            "case {case_idx}: vault only decreases by successful payouts"
        );
    }
}

// --- freeze GX/EZ guards (previously only the happy path was exercised; the src comment even cited a
// `set_authority_clears_delegate_no_vault_rug` test that never existed). These pin the negatives. ---
fn create_mint_with_freeze(
    svm: &mut LiteSVM,
    payer: &Keypair,
    mint_auth: &Pubkey,
    freeze_auth: Option<&Pubkey>,
) -> Pubkey {
    let mint = Keypair::new();
    let rent = svm.minimum_balance_for_rent_exemption(spl_token::state::Mint::LEN);
    let ixs = [
        system_instruction::create_account(
            &payer.pubkey(),
            &mint.pubkey(),
            rent,
            spl_token::state::Mint::LEN as u64,
            &spl_token::ID,
        ),
        spl_token::instruction::initialize_mint(
            &spl_token::ID,
            &mint.pubkey(),
            mint_auth,
            freeze_auth,
            6,
        )
        .unwrap(),
    ];
    let tx = Transaction::new_signed_with_payer(
        &ixs,
        Some(&payer.pubkey()),
        &[payer, &mint],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).unwrap();
    mint.pubkey()
}
// Init an rd_config for a prepared coin_mint (the vault is bound later at freeze). emission_end=2000, window=500.
fn rd_init(
    svm: &mut LiteSVM,
    payer: &Keypair,
    supply: u64,
    coin_mint: &Pubkey,
    mint_authority: &Keypair,
) -> Pubkey {
    let rd_config = Pubkey::find_program_address(&[b"rd_config", coin_mint.as_ref()], &rd_id()).0;
    let dist_config = dist_config_pda(&coin_mint, &rd_config);
    let stub_perc = Pubkey::new_unique();
    let stub_sub = Pubkey::new_unique();
    let d = rd_init_data(
        supply,
        2_000,
        1_000,
        1_000,
        4_000,
        500,
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        &[],
        Some(0),
    );
    send(
        svm,
        payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: rd_init_accounts(
                payer.pubkey(),
                *coin_mint,
                dist_config,
                stub_perc,
                stub_sub,
                rd_config,
                mint_authority.pubkey(),
            ),
            data: d,
        }],
        &[mint_authority],
    )
    .expect("rd init");
    rd_config
}
fn freeze_ix(
    svm: &mut LiteSVM,
    payer: &Keypair,
    rd_config: Pubkey,
    coin_mint: Pubkey,
    vault: Pubkey,
) -> Result<(), String> {
    send(
        svm,
        payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new(rd_config, false),
                AccountMeta::new_readonly(coin_mint, false),
                AccountMeta::new(vault, false),
            ],
            data: vec![4u8],
        }],
        &[],
    )
}

// DoS PROBE (non-SPL token-shaped vault at the permissionless freeze, sweep tick D): freeze BINDS config.vault
// from a caller-supplied account, validating its token FIELDS via Account::unpack — but unpack does NOT verify
// the owning program (distribution::init_config guards exactly this at lib.rs:342 with a warning). freeze is
// permissionless + one-shot. A griefer can craft a NON-SPL account with token-shaped bytes (owner field =
// rd_config, mint = coin_mint, amount = supply) and front-run the freeze with it: it passes every field check,
// binds config.vault to a non-SPL account, and stamps freeze_slot (so the real vault can never be bound). Then
// EVERY claim's spl_token transfer from config.vault fails (source not SPL-owned) -> the entire residual
// distribution is permanently bricked. freeze must reject a vault not owned by the SPL Token program.
#[test]
fn freeze_rejects_a_non_spl_owned_token_shaped_vault_no_front_run_brick() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let supply = 1_000_000u64;
    let env = setup(&mut svm, &payer, supply);
    set_slot(&mut svm, 100);

    // A real backer crystallizes, so there is genuinely a claim the brick would deny.
    let lp = Keypair::new();
    let pf = Pubkey::new_unique();
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        0,
        0,
    );
    register(&mut svm, &payer, &env, &lp, &lp.pubkey(), &pf, COHORT_LP).expect("reg");
    set_portfolio(
        &mut svm,
        &pf,
        &env.stub_perc,
        &env.market,
        &lp.pubkey(),
        9_000,
        0,
    );
    set_slot(&mut svm, 1_000);
    crystallize(&mut svm, &payer, &env, &lp, &pf).expect("cry");

    // Craft a SYSTEM-owned account whose data round-trips as an initialized token account: owner field =
    // rd_config, mint = coin_mint, amount = supply — passes every FIELD check, fails only on the owning program.
    let fake = spl_token::state::Account {
        mint: env.coin_mint,
        owner: env.rd_config,
        amount: supply,
        delegate: COption::None,
        state: spl_token::state::AccountState::Initialized,
        is_native: COption::None,
        delegated_amount: 0,
        close_authority: COption::None,
    };
    let mut fake_data = vec![0u8; spl_token::state::Account::LEN];
    spl_token::state::Account::pack(fake, &mut fake_data).unwrap();
    let fake_vault = Pubkey::new_unique();
    svm.set_account(
        fake_vault,
        Account {
            lamports: 10_000_000,
            data: fake_data,
            owner: solana_sdk::system_program::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();

    set_slot(&mut svm, env.emission_end + env.finalize_window + 1);
    // ATTACK: front-run the one-shot freeze with the fake vault.
    assert!(
        freeze_ix(&mut svm, &payer, env.rd_config, env.coin_mint, fake_vault).is_err(),
        "freeze must reject a token-shaped vault not owned by the SPL Token program (else a front-run binds a fake vault and bricks all claims)"
    );
    // The real vault still freezes + pays out (the rejected attempt did not consume the one-shot freeze).
    freeze_ix(&mut svm, &payer, env.rd_config, env.coin_mint, env.vault)
        .expect("the real SPL vault freezes");
    let ata = create_token_account(&mut svm, &payer, &env.coin_mint, &lp.pubkey());
    claim(&mut svm, &payer, &env, &lp, &ata, None).expect("claim pays from the real vault");
    assert_eq!(
        token_amount(&svm, &ata),
        400_000,
        "the LP backer claims its full cohort from the real vault"
    );
}

// finding GX/EZ: freeze BINDS the fixed-supply COIN vault, so it must reject any mint that could still be
// inflated (live mint authority) or freeze claimers (live freeze authority), and any vault that isn't the
// rd_config-owned full-supply account. Each case uses its own mint so the global mint.supply check isolates
// the guard under test.
#[test]
fn freeze_enforces_fixed_supply_and_vault_integrity() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 1_000_000_000_000).unwrap();
    let supply = 1_000_000u64;
    let past = 2_000u64 + 500 + 1;

    // (GX) a mint that still has a MINT authority is rejected (supply could be inflated under claimers);
    // after revoking it, the fixed-supply mint + rd-owned full vault is accepted.
    let ma = Keypair::new();
    let mint = create_mint(&mut svm, &payer, &ma.pubkey());
    let rd_config = rd_init(&mut svm, &payer, supply, &mint, &ma);
    let vault = create_token_account(&mut svm, &payer, &mint, &rd_config);
    mint_to(&mut svm, &payer, &mint, &ma, &vault, supply);
    set_slot(&mut svm, past);
    assert!(
        freeze_ix(&mut svm, &payer, rd_config, mint, vault).is_err(),
        "live mint authority must be rejected (GX inflation)"
    );
    revoke_mint(&mut svm, &payer, &mint, &ma);
    assert!(
        freeze_ix(&mut svm, &payer, rd_config, mint, vault).is_ok(),
        "fixed-supply mint + rd-owned full vault accepted"
    );

    // (GX) a mint that still has a FREEZE authority is rejected (claimers' COIN could be frozen = censorship);
    // after clearing it, accepted.
    let ma2 = Keypair::new();
    let fa = Keypair::new();
    let mint2 = create_mint_with_freeze(&mut svm, &payer, &ma2.pubkey(), Some(&fa.pubkey()));
    let rd2 = rd_init(&mut svm, &payer, supply, &mint2, &ma2);
    let vault2 = create_token_account(&mut svm, &payer, &mint2, &rd2);
    mint_to(&mut svm, &payer, &mint2, &ma2, &vault2, supply);
    revoke_mint(&mut svm, &payer, &mint2, &ma2); // clear mint authority; freeze authority remains
    set_slot(&mut svm, past);
    assert!(
        freeze_ix(&mut svm, &payer, rd2, mint2, vault2).is_err(),
        "live freeze authority must be rejected (GX freeze-claimers)"
    );
    let clear = spl_token::instruction::set_authority(
        &spl_token::ID,
        &mint2,
        None,
        AuthorityType::FreezeAccount,
        &fa.pubkey(),
        &[],
    )
    .unwrap();
    send(&mut svm, &payer, &[clear], &[&fa]).expect("clear freeze authority");
    assert!(
        freeze_ix(&mut svm, &payer, rd2, mint2, vault2).is_ok(),
        "after clearing freeze authority, accepted"
    );

    // A vault frozen BEFORE freeze authority is revoked remains frozen forever. Authority revocation
    // alone is therefore insufficient: accepting this one-shot vault would brick every later claim.
    let ma_frozen = Keypair::new();
    let fa_frozen = Keypair::new();
    let frozen_mint = create_mint_with_freeze(
        &mut svm,
        &payer,
        &ma_frozen.pubkey(),
        Some(&fa_frozen.pubkey()),
    );
    let frozen_rd = rd_init(&mut svm, &payer, supply, &frozen_mint, &ma_frozen);
    let frozen_vault = create_token_account(&mut svm, &payer, &frozen_mint, &frozen_rd);
    mint_to(
        &mut svm,
        &payer,
        &frozen_mint,
        &ma_frozen,
        &frozen_vault,
        supply,
    );
    let freeze_vault = spl_token::instruction::freeze_account(
        &spl_token::ID,
        &frozen_vault,
        &frozen_mint,
        &fa_frozen.pubkey(),
        &[],
    )
    .unwrap();
    send(&mut svm, &payer, &[freeze_vault], &[&fa_frozen]).expect("freeze reward vault");
    revoke_mint(&mut svm, &payer, &frozen_mint, &ma_frozen);
    let revoke_freeze = spl_token::instruction::set_authority(
        &spl_token::ID,
        &frozen_mint,
        None,
        AuthorityType::FreezeAccount,
        &fa_frozen.pubkey(),
        &[],
    )
    .unwrap();
    send(&mut svm, &payer, &[revoke_freeze], &[&fa_frozen])
        .expect("revoke freeze authority after freezing vault");
    set_slot(&mut svm, past);
    assert!(
        freeze_ix(&mut svm, &payer, frozen_rd, frozen_mint, frozen_vault,).is_err(),
        "an already-frozen vault must not consume the one-shot reward freeze"
    );
    assert_eq!(
        &svm.get_account(&frozen_rd).unwrap().data[318..326],
        &0u64.to_le_bytes(),
        "rejected frozen vault leaves the reward config unfrozen"
    );

    // (EZ) a vault NOT owned by rd_config is rejected even when fully funded.
    let ma3 = Keypair::new();
    let mint3 = create_mint(&mut svm, &payer, &ma3.pubkey());
    let rd3 = rd_init(&mut svm, &payer, supply, &mint3, &ma3);
    let attacker = Pubkey::new_unique();
    let decoy = create_token_account(&mut svm, &payer, &mint3, &attacker);
    mint_to(&mut svm, &payer, &mint3, &ma3, &decoy, supply);
    revoke_mint(&mut svm, &payer, &mint3, &ma3);
    set_slot(&mut svm, past);
    assert!(
        freeze_ix(&mut svm, &payer, rd3, mint3, decoy).is_err(),
        "non-rd-owned vault must be rejected (EZ)"
    );

    // (EZ) an rd-owned but UNDER-funded vault is rejected (mint.supply == total, but the bound vault holds < it).
    let ma4 = Keypair::new();
    let mint4 = create_mint(&mut svm, &payer, &ma4.pubkey());
    let rd4 = rd_init(&mut svm, &payer, supply, &mint4, &ma4);
    let under = create_token_account(&mut svm, &payer, &mint4, &rd4);
    let sink = create_token_account(&mut svm, &payer, &mint4, &Pubkey::new_unique());
    mint_to(&mut svm, &payer, &mint4, &ma4, &under, supply - 1);
    mint_to(&mut svm, &payer, &mint4, &ma4, &sink, 1); // total minted == supply, but `under` holds supply-1
    revoke_mint(&mut svm, &payer, &mint4, &ma4);
    set_slot(&mut svm, past);
    assert!(
        freeze_ix(&mut svm, &payer, rd4, mint4, under).is_err(),
        "under-funded rd-owned vault must be rejected (EZ)"
    );
}

// finding IL+: the LP/trader cohorts are scoped to an ALLOW-LIST of trusted-Pyth markets (the primary
// market_group plus up to MAX_EXTRA_MARKETS extras the orchestrator vetted at init), not a single market.
// A portfolio on ANY allow-listed market counts; one on a non-listed (e.g. attacker-oracle'd) market is
// rejected — the registrant cannot bring their own market. Real rd .so.
#[test]
fn lp_cohort_accepts_any_allowlisted_market_and_rejects_others() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(rd_id(), rd_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let mint_auth = Keypair::new();
    let coin_mint = create_mint(&mut svm, &payer, &mint_auth.pubkey());
    let rd_config = Pubkey::find_program_address(&[b"rd_config", coin_mint.as_ref()], &rd_id()).0;
    let dist_config = dist_config_pda(&coin_mint, &rd_config);
    let stub_perc = Pubkey::new_unique();
    let stub_sub = Pubkey::new_unique();
    let (m0, m1, m2, foreign) = (
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    );

    // init: only the LP cohort active (lp=10000 -> trader=0), market allow-list = {m0 (primary), m1, m2}.
    let mut d = vec![0u8];
    d.extend_from_slice(&1_000_000u64.to_le_bytes()); // supply
    d.extend_from_slice(&2_000u64.to_le_bytes()); // emission_end
    d.extend_from_slice(&0u16.to_le_bytes()); // insurance
    d.extend_from_slice(&0u16.to_le_bytes()); // backing
    d.extend_from_slice(&10_000u16.to_le_bytes()); // lp (trader = 0)
    d.extend_from_slice(&500u64.to_le_bytes()); // finalize_window
    d.extend_from_slice(Pubkey::default().as_ref()); // ins_pool (ins=0)
    d.extend_from_slice(Pubkey::default().as_ref()); // back_pool (back=0)
    d.extend_from_slice(m0.as_ref()); // market_group (primary)
    d.extend_from_slice(&[2u8]); // extra market count
    d.extend_from_slice(m1.as_ref());
    d.extend_from_slice(m2.as_ref());
    send(
        &mut svm,
        &payer,
        &[Instruction {
            program_id: rd_id(),
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new_readonly(coin_mint, false),
                AccountMeta::new_readonly(dist_id(), false),
                AccountMeta::new_readonly(dist_config, false),
                AccountMeta::new_readonly(stub_perc, false),
                AccountMeta::new_readonly(stub_sub, false),
                AccountMeta::new(rd_config, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
                AccountMeta::new_readonly(mint_auth.pubkey(), true),
            ],
            data: d,
        }],
        &[&mint_auth],
    )
    .expect("init with a 3-market allow-list");
    set_slot(&mut svm, 100);

    let reg = |svm: &mut LiteSVM, owner: &Keypair, pf: &Pubkey| -> Result<(), String> {
        let stake = Pubkey::find_program_address(
            &[
                b"rd_stake",
                rd_config.as_ref(),
                owner.pubkey().as_ref(),
                pf.as_ref(),
                &[COHORT_LP],
            ],
            &rd_id(),
        )
        .0;
        let account = svm.get_account(pf).unwrap();
        let market = Pubkey::new_from_array(account.data[16..48].try_into().unwrap());
        let archive = Pubkey::find_program_address(
            &[
                b"rd_portfolio_archive",
                stub_perc.as_ref(),
                market.as_ref(),
                owner.pubkey().as_ref(),
                pf.as_ref(),
            ],
            &rd_id(),
        )
        .0;
        ensure_mock_market(svm, &stub_perc, &market);
        send(
            svm,
            &payer,
            &[Instruction {
                program_id: rd_id(),
                accounts: vec![
                    AccountMeta::new(payer.pubkey(), true),
                    AccountMeta::new_readonly(rd_config, false),
                    AccountMeta::new_readonly(owner.pubkey(), true),
                    AccountMeta::new_readonly(owner.pubkey(), false),
                    AccountMeta::new_readonly(*pf, false),
                    AccountMeta::new(stake, false),
                    AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
                    AccountMeta::new_readonly(archive, false),
                    AccountMeta::new_readonly(market, false),
                    AccountMeta::new_readonly(retired_market_pda(&stub_perc, &market), false),
                ],
                data: vec![1u8, COHORT_LP],
            }],
            &[owner],
        )
    };
    // primary market -> accepted
    let a = Keypair::new();
    let a_pf = Pubkey::new_unique();
    set_portfolio(&mut svm, &a_pf, &stub_perc, &m0, &a.pubkey(), 0, 0);
    reg(&mut svm, &a, &a_pf).expect("primary allow-listed market accepted");
    // an extra allow-listed market -> accepted
    let b = Keypair::new();
    let b_pf = Pubkey::new_unique();
    set_portfolio(&mut svm, &b_pf, &stub_perc, &m2, &b.pubkey(), 0, 0);
    reg(&mut svm, &b, &b_pf).expect("extra allow-listed market accepted");
    // a NON-listed market -> rejected
    let c = Keypair::new();
    let c_pf = Pubkey::new_unique();
    set_portfolio(&mut svm, &c_pf, &stub_perc, &foreign, &c.pubkey(), 0, 0);
    assert!(
        reg(&mut svm, &c, &c_pf).is_err(),
        "a market NOT on the allow-list must be rejected"
    );
}
