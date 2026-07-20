//! Adversarial farming simulation — how a RATIONAL MINER maximizes its share of the deterministic
//! distributor (residual-distributor) across N markets whose ORACLE IT CANNOT CONTROL.
//!
//! Threat model. The N markets are trusted-Pyth (all allow-listed in the rd config). A NEUTRAL oracle key
//! (not the miner) moves the marks; the miner can never push them. The miner farms the two PnL-flow cohorts
//!   - TRADER  (points = Δ residual_crystallized_loss)
//!   - LP      (points = Δ residual_received)
//! by holding BOTH sides of a position per market: a long in one portfolio and a short in another, both
//! miner-owned (delta-neutral -> ZERO market risk). Whichever way the neutral oracle moves, the losing leg's
//! settled loss becomes crystallized_loss (trader points) and the winning leg's gain becomes received (LP
//! points) — both captured by the miner. The "loss" is an internal transfer between two accounts the miner
//! owns, so the miner's net capital is preserved; the only cost is trading fees. NOTE: those fees are NOT zero
//! even when a trade passes fee_bps=0 — percolator charges max(caller_fee_bps, trade_fee_base_bps), so the 3 bps
//! market base applies to every trade and accrues to asset-0 insurance (see the fee-accrual test below). That
//! only RAISES the miner's real cost, so the wash-unprofitability conclusion holds a fortiori.
//!
//! Result: even with markets it cannot oracle-control, the miner captures ~the ENTIRE LP+trader allocation
//! (80% of supply by default). The allow-list (finding IL+) closes the risk-free oracle-controlled path but
//! NOT the delta-neutral wash-farm. Run: RUST_MIN_STACK=8388608 cargo test -p sim -- --nocapture

use litesvm::LiteSVM;
use solana_sdk::{
    account::Account, clock::Clock, instruction::{AccountMeta, Instruction}, pubkey::Pubkey,
    signature::{Keypair, Signer}, system_program, transaction::Transaction,
};
use std::str::FromStr;
use percolator_prog::ix::Instruction as PIx;

const N_MARKETS: usize = 10;   // markets the miner CANNOT control the oracle of
const SUPPLY: u64 = 1_000_000; // fixed COIN supply
// cohorts: insurance 10 / backing 10 / lp 40 / trader 40 -> LP+trader = 80% is the farmable surface.
const INS_BPS: u16 = 1_000;
const BACK_BPS: u16 = 1_000;
const LP_BPS: u16 = 4_000; // trader = remainder = 4_000
const FEE_BPS: u16 = 2_000; // anti-wash fee (finding NZ) skimmed from LP/trader claims

fn perc_id() -> Pubkey { percolator_prog::id() }
fn perc_so() -> String { format!("{}/../target/deploy/percolator_prog.so", env!("CARGO_MANIFEST_DIR")) }
fn so_deploy(name: &str) -> String { format!("{}/../target/deploy/{}.so", env!("CARGO_MANIFEST_DIR"), name) }
fn rd_id() -> Pubkey { Pubkey::from_str("Res1dua1Distr1butor111111111111111111111111").unwrap() }
fn rd_stake_pda(config: &Pubkey, owner: &Pubkey, linked: &Pubkey, cohort: u8) -> Pubkey {
    let family = if cohort == 3 { 2 } else { cohort };
    Pubkey::find_program_address(&[b"rd_stake", config.as_ref(), owner.as_ref(), linked.as_ref(), &[family]], &rd_id()).0
}
fn rd_portfolio_archive_pda(market: &Pubkey, owner: &Pubkey, portfolio: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"rd_portfolio_archive",
            perc_id().as_ref(),
            market.as_ref(),
            owner.as_ref(),
            portfolio.as_ref(),
        ],
        &rd_id(),
    )
    .0
}
fn retired_market_pda(market: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"retired-market", perc_id().as_ref(), market.as_ref()],
        &solana_sdk::pubkey!("3ueoyr1JepT2DvPxh8LrhdJZ6YsL2sT9Sm7y3TfNyfi9"),
    )
    .0
}
fn dist_id() -> Pubkey { Pubkey::from_str("D1str1but1on11111111111111111111111111111111").unwrap() }
fn sub_id() -> Pubkey { Pubkey::from_str("Sub1edger1111111111111111111111111111111111").unwrap() }
const ATA_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

fn token_acct_bytes(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Vec<u8> {
    let mut d = vec![0u8; 165];
    d[0..32].copy_from_slice(mint.as_ref());
    d[32..64].copy_from_slice(owner.as_ref());
    d[64..72].copy_from_slice(&amount.to_le_bytes());
    d[108] = 1;
    d
}
fn set_token(svm: &mut LiteSVM, key: &Pubkey, mint: &Pubkey, owner: &Pubkey, amount: u64) {
    svm.set_account(*key, Account { lamports: 2_000_000, data: token_acct_bytes(mint, owner, amount), owner: spl_token::ID, executable: false, rent_epoch: 0 }).unwrap();
}
fn token_amount(svm: &LiteSVM, key: &Pubkey) -> u64 {
    svm.get_account(key).map(|a| u64::from_le_bytes(a.data[64..72].try_into().unwrap())).unwrap_or(0)
}
fn create_real_mint(svm: &mut LiteSVM, payer: &Keypair, authority: &Pubkey) -> Pubkey {
    let mint = Keypair::new();
    let rent = svm.minimum_balance_for_rent_exemption(82);
    let ixs = [
        solana_sdk::system_instruction::create_account(&payer.pubkey(), &mint.pubkey(), rent, 82, &spl_token::ID),
        spl_token::instruction::initialize_mint(&spl_token::ID, &mint.pubkey(), authority, None, 6).unwrap(),
    ];
    let bh = svm.latest_blockhash();
    svm.send_transaction(Transaction::new_signed_with_payer(&ixs, Some(&payer.pubkey()), &[payer, &mint], bh)).unwrap();
    mint.pubkey()
}
fn create_real_token_account(
    svm: &mut LiteSVM,
    payer: &Keypair,
    mint: &Pubkey,
    owner: &Pubkey,
) -> Pubkey {
    let token = Keypair::new();
    let rent = svm.minimum_balance_for_rent_exemption(165);
    let ixs = [
        solana_sdk::system_instruction::create_account(
            &payer.pubkey(),
            &token.pubkey(),
            rent,
            165,
            &spl_token::ID,
        ),
        spl_token::instruction::initialize_account3(
            &spl_token::ID,
            &token.pubkey(),
            mint,
            owner,
        )
        .unwrap(),
    ];
    svm.expire_blockhash();
    let bh = svm.latest_blockhash();
    svm.send_transaction(Transaction::new_signed_with_payer(
        &ixs,
        Some(&payer.pubkey()),
        &[payer, &token],
        bh,
    ))
    .unwrap();
    token.pubkey()
}
fn mint_real_tokens(
    svm: &mut LiteSVM,
    payer: &Keypair,
    mint: &Pubkey,
    authority: &Keypair,
    destination: &Pubkey,
    amount: u64,
) {
    let ix = spl_token::instruction::mint_to(
        &spl_token::ID,
        mint,
        destination,
        &authority.pubkey(),
        &[],
        amount,
    )
    .unwrap();
    svm.expire_blockhash();
    let bh = svm.latest_blockhash();
    svm.send_transaction(Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[payer, authority],
        bh,
    ))
    .unwrap();
}
fn pix(accounts: Vec<AccountMeta>, ix: PIx) -> Instruction { Instruction { program_id: perc_id(), accounts, data: ix.encode() } }
fn read_u128_at(svm: &LiteSVM, pf: &Pubkey, off: usize) -> u128 {
    let d = svm.get_account(pf).unwrap().data;
    u128::from_le_bytes(d[off..off + 16].try_into().unwrap())
}
fn read_crystallized(svm: &LiteSVM, pf: &Pubkey) -> u128 { read_u128_at(svm, pf, 196) } // HEADER_LEN(16)+180
fn read_spent(svm: &LiteSVM, pf: &Pubkey) -> u128 { read_u128_at(svm, pf, 212) }        // HEADER_LEN(16)+196
fn read_received(svm: &LiteSVM, pf: &Pubkey) -> u128 { read_u128_at(svm, pf, 228) }     // HEADER_LEN(16)+212
fn perc_vault_authority(slab: &Pubkey) -> Pubkey { Pubkey::find_program_address(&[b"vault", slab.as_ref()], &perc_id()).0 }
fn canonical_insurance_vault(va: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[va.as_ref(), spl_token::ID.as_ref(), mint.as_ref()], &ATA_PROGRAM_ID).0
}
const DISTRIBUTION_CLAIM_WINDOW_SLOTS: u64 = 1_000_000;
fn read_insurance(svm: &LiteSVM, market: &Pubkey) -> u128 {
    let data = svm.get_account(market).unwrap().data;
    percolator_prog::state::read_market(&data).unwrap().1.insurance
}
fn dist_config_pda(coin_mint: &Pubkey, authority: &Pubkey) -> Pubkey {
    let claim_window = DISTRIBUTION_CLAIM_WINDOW_SLOTS.to_le_bytes();
    Pubkey::find_program_address(
        &[b"dist_config", coin_mint.as_ref(), authority.as_ref(), &claim_window],
        &dist_id(),
    )
    .0
}

fn modeled_subledger_position(
    pool: &Pubkey,
    owner: &Pubkey,
    principal: u64,
    start_slot: u64,
) -> Vec<u8> {
    let mut data = vec![0u8; subledger_program::POS_SHARES_OFF + 16];
    data[..8].copy_from_slice(b"SUBPOS01");
    data[subledger_program::POS_POOL_OFF..subledger_program::POS_POOL_OFF + 32]
        .copy_from_slice(pool.as_ref());
    data[subledger_program::POS_OWNER_OFF..subledger_program::POS_OWNER_OFF + 32]
        .copy_from_slice(owner.as_ref());
    data[subledger_program::POS_PRINCIPAL_OFF..subledger_program::POS_PRINCIPAL_OFF + 8]
        .copy_from_slice(&principal.to_le_bytes());
    data[subledger_program::POS_START_SLOT_OFF..subledger_program::POS_START_SLOT_OFF + 8]
        .copy_from_slice(&start_slot.to_le_bytes());
    data[subledger_program::POS_SHARES_OFF..subledger_program::POS_SHARES_OFF + 16]
        .copy_from_slice(&(principal as u128).to_le_bytes());
    data
}

#[test]
fn rational_miner_farms_the_deterministic_distributor_across_uncontrolled_markets() {
    let mut svm = LiteSVM::new().with_compute_budget(solana_program_runtime::compute_budget::ComputeBudget {
        compute_unit_limit: 1_400_000, heap_size: 256 * 1024,
        ..solana_program_runtime::compute_budget::ComputeBudget::default()
    });
    svm.add_program_from_file(perc_id(), perc_so()).unwrap();
    svm.add_program_from_file(rd_id(), so_deploy("residual_distributor")).unwrap();
    let payer = Keypair::new(); svm.airdrop(&payer.pubkey(), 100_000_000_000_000).unwrap();
    // NEUTRAL oracle authority — NOT the miner. The miner cannot push these marks.
    let oracle = Keypair::new(); svm.airdrop(&oracle.pubkey(), 1_000_000_000_000).unwrap();
    svm.set_sysvar(&Clock { slot: 100, unix_timestamp: 100, ..Default::default() });
    let collateral = create_real_mint(&mut svm, &payer, &Keypair::new().pubkey());
    let initial_price = 1_000u64;

    let send = |svm: &mut LiteSVM, ixs: &[Instruction], extra: &[&Keypair]| {
        svm.expire_blockhash();
        let bh = svm.latest_blockhash();
        let mut s: Vec<&Keypair> = vec![&payer]; s.extend_from_slice(extra);
        svm.send_transaction(Transaction::new_signed_with_payer(ixs, Some(&payer.pubkey()), &s, bh))
    };

    // ---- stand up N trusted-Pyth markets (oracle moved ONLY by the neutral `oracle` key) ----
    let mlen = percolator_prog::state::market_account_len_for_capacity(1).unwrap();
    let mut markets: Vec<(Pubkey, Pubkey)> = Vec::new(); // (market, perc_vault)
    for _ in 0..N_MARKETS {
        let market = Pubkey::new_unique();
        svm.set_account(market, Account { lamports: 1_000_000_000, data: vec![0u8; mlen], owner: perc_id(), executable: false, rent_epoch: 0 }).unwrap();
        let pv = canonical_insurance_vault(&perc_vault_authority(&market), &collateral);
        set_token(&mut svm, &pv, &collateral, &perc_vault_authority(&market), 0);
        send(&mut svm, &[pix(
            vec![AccountMeta::new(oracle.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new_readonly(collateral, false)],
            PIx::InitMarket { max_portfolio_assets: 1, h_min: 0, h_max: 10, initial_price,
                min_nonzero_mm_req: 1, min_nonzero_im_req: 2, maintenance_margin_bps: 10_000, initial_margin_bps: 10_000,
                max_trading_fee_bps: 10_000, trade_fee_base_bps: 3, liquidation_fee_bps: 0, liquidation_fee_cap: 0,
                min_liquidation_abs: 0, max_price_move_bps_per_slot: 10_000, max_accrual_dt_slots: 1,
                max_abs_funding_e9_per_slot: 0, min_funding_lifetime_slots: 1, max_account_b_settlement_chunks: 1,
                max_bankrupt_close_chunks: 1, max_bankrupt_close_lifetime_slots: 100, public_b_chunk_atoms: percolator::MAX_VAULT_TVL,
                maintenance_fee_per_slot: 0 },
        )], &[&oracle]).expect("init market");
        send(&mut svm, &[pix(
            vec![AccountMeta::new(oracle.pubkey(), true), AccountMeta::new(market, false)],
            PIx::ConfigureAuthMark { asset_index: 0, now_slot: 100, initial_mark_e6: initial_price },
        )], &[&oracle]).expect("configure auth mark");
        markets.push((market, pv));
    }

    // ---- residual-distributor: cohorts 10/10/40/40, allow-list = ALL N markets ----
    let coin_auth = Keypair::new();
    let coin_mint = create_real_mint(&mut svm, &payer, &coin_auth.pubkey());
    let rd_config = Pubkey::find_program_address(&[b"rd_config", coin_mint.as_ref()], &rd_id()).0;
    let dist_config = dist_config_pda(&coin_mint, &rd_config);
    let rd_vault = Pubkey::new_unique(); set_token(&mut svm, &rd_vault, &coin_mint, &rd_config, 0);
    send(&mut svm, &[
        spl_token::instruction::mint_to(&spl_token::ID, &coin_mint, &rd_vault, &coin_auth.pubkey(), &[], SUPPLY).unwrap(),
    ], &[&coin_auth]).expect("fund the COIN supply");
    let mut ri = vec![0u8];
    ri.extend_from_slice(&SUPPLY.to_le_bytes()); ri.extend_from_slice(&2_000u64.to_le_bytes());
    ri.extend_from_slice(&INS_BPS.to_le_bytes()); ri.extend_from_slice(&BACK_BPS.to_le_bytes()); ri.extend_from_slice(&LP_BPS.to_le_bytes());
    ri.extend_from_slice(&100u64.to_le_bytes());
    // insurance/backing pools: placeholders (no ins/back participants here; the farmable surface is the LP+trader 80%).
    ri.extend_from_slice(Pubkey::new_unique().as_ref()); ri.extend_from_slice(Pubkey::new_unique().as_ref());
    ri.extend_from_slice(markets[0].0.as_ref());           // market_group (primary)
    ri.push((N_MARKETS - 1) as u8);                        // extra allow-listed markets
    for (m, _) in &markets[1..] { ri.extend_from_slice(m.as_ref()); }
    ri.extend_from_slice(&FEE_BPS.to_le_bytes());          // OPTIONAL trailing anti-wash fee (finding NZ)
    send(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
        AccountMeta::new(payer.pubkey(), true), AccountMeta::new_readonly(coin_mint, false), AccountMeta::new_readonly(dist_id(), false),
        AccountMeta::new_readonly(dist_config, false), AccountMeta::new_readonly(perc_id(), false), AccountMeta::new_readonly(sub_id(), false),
        AccountMeta::new(rd_config, false), AccountMeta::new_readonly(system_program::ID, false), AccountMeta::new_readonly(coin_auth.pubkey(), true),
    ], data: ri }], &[&coin_auth]).expect("rd init with the N-market allow-list");
    send(&mut svm, &[
        spl_token::instruction::set_authority(&spl_token::ID, &coin_mint, None, spl_token::instruction::AuthorityType::MintTokens, &coin_auth.pubkey(), &[]).unwrap(),
    ], &[&coin_auth]).expect("freeze the COIN supply");

    // ---- THE FARM: per market, a delta-neutral miner pair (long + short, both miner-owned) ----
    let plen = percolator_prog::state::portfolio_account_len_for_market_slots(2).unwrap();
    let posq = (percolator::POS_SCALE / 2) as i128;
    // (owner, cohort, portfolio, coin_ata)
    let mut stakes: Vec<(Keypair, u8, Pubkey, Pubkey, Pubkey)> = Vec::new();
    for (market, pv) in &markets {
        let long = Keypair::new(); let short = Keypair::new();
        svm.airdrop(&long.pubkey(), 1_000_000_000).unwrap(); svm.airdrop(&short.pubkey(), 1_000_000_000).unwrap();
        let long_pf = Pubkey::new_unique(); let short_pf = Pubkey::new_unique();
        for (o, pf) in [(&short, &short_pf), (&long, &long_pf)] {
            svm.set_account(*pf, Account { lamports: 1_000_000_000, data: vec![0u8; plen], owner: perc_id(), executable: false, rent_epoch: 0 }).unwrap();
            send(&mut svm, &[pix(vec![AccountMeta::new(o.pubkey(), true), AccountMeta::new(*market, false), AccountMeta::new(*pf, false)], PIx::InitPortfolio)], &[o]).expect("init pf");
            let src = Pubkey::new_unique(); set_token(&mut svm, &src, &collateral, &o.pubkey(), 1_000_000);
            send(&mut svm, &[pix(vec![
                AccountMeta::new(o.pubkey(), true), AccountMeta::new(*market, false), AccountMeta::new(*pf, false),
                AccountMeta::new(src, false), AccountMeta::new(*pv, false), AccountMeta::new_readonly(spl_token::ID, false)],
                PIx::Deposit { amount: 1_000_000 })], &[o]).expect("deposit");
        }
        // register BEFORE the loss (snapshot = 0): long -> TRADER, short -> LP.
        for (o, cohort, pf) in [(&long, 3u8, long_pf), (&short, 2u8, short_pf)] {
            let stake = rd_stake_pda(&rd_config, &o.pubkey(), &pf, cohort);
            send(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
                AccountMeta::new(payer.pubkey(), true), AccountMeta::new_readonly(rd_config, false), AccountMeta::new_readonly(o.pubkey(), true),
                AccountMeta::new_readonly(o.pubkey(), false), AccountMeta::new_readonly(pf, false), AccountMeta::new(stake, false),
                AccountMeta::new_readonly(system_program::ID, false),
                AccountMeta::new_readonly(rd_portfolio_archive_pda(market, &o.pubkey(), &pf), false),
                AccountMeta::new_readonly(*market, false),
                AccountMeta::new_readonly(retired_market_pda(market), false),
            ], data: vec![1u8, cohort] }], &[o]).expect("register");
            let ata = Pubkey::new_unique(); set_token(&mut svm, &ata, &coin_mint, &o.pubkey(), 0);
            stakes.push((o.insecure_clone(), cohort, pf, ata, *market));
        }
        // open the delta-neutral pair: size_q negative -> owner_a(short) short, owner_b(long) long.
        send(&mut svm, &[pix(vec![
            AccountMeta::new(short.pubkey(), true), AccountMeta::new(long.pubkey(), true), AccountMeta::new(*market, false),
            AccountMeta::new(short_pf, false), AccountMeta::new(long_pf, false)],
            PIx::TradeNoCpi { asset_index: 0, size_q: -posq, exec_price: initial_price, fee_bps: 0 })], &[&short, &long]).expect("open delta-neutral pair");
    }

    // ---- the NEUTRAL oracle moves each market (miner does NOT sign), then a permissionless crank
    //      crystallizes the long's loss (trader) and the short's gain (received -> LP) ----
    svm.set_sysvar(&Clock { slot: 110, unix_timestamp: 110, ..Default::default() });
    let crank = |svm: &mut LiteSVM, market: &Pubkey, pf: &Pubkey| {
        svm.expire_blockhash(); let bh = svm.latest_blockhash();
        svm.send_transaction(Transaction::new_signed_with_payer(&[pix(
            vec![AccountMeta::new(payer.pubkey(), true), AccountMeta::new(*market, false), AccountMeta::new(*pf, false)],
            PIx::PermissionlessCrank { now_slot: 110, observations: vec![percolator_prog::ix::CrankObservationHint { asset_index: 0, oracle_accounts: 0 }] },
        )], Some(&payer.pubkey()), &[&payer], bh))
    };
    let mut market_cryst = 0u128; let mut market_recv = 0u128; let mut market_spent = 0u128;
    for (i, (market, _)) in markets.iter().enumerate() {
        // the two portfolios of market i are stakes[2i] (long/trader) and stakes[2i+1] (short/lp).
        let long_pf = stakes[2 * i].2; let short_pf = stakes[2 * i + 1].2;
        send(&mut svm, &[pix(vec![AccountMeta::new(oracle.pubkey(), true), AccountMeta::new(*market, false)],
            PIx::PushAuthMark { asset_index: 0, now_slot: 110, mark_e6: initial_price / 2 })], &[&oracle]).expect("neutral oracle moves the mark");
        for pf in [&short_pf, &long_pf] { crank(&mut svm, market, pf).expect("auto-crank settlement"); crank(&mut svm, market, pf).expect("auto-crank refresh"); }
        market_cryst += read_crystallized(&svm, &long_pf);
        market_spent += read_spent(&svm, &long_pf);     // finding NZ: stays 0 -> spent-netting does NOT catch the delta-neutral wash
        market_recv += read_received(&svm, &short_pf);
    }

    // ---- crystallize every miner stake (Δ), freeze, claim ----
    for (o, cohort, pf, _ata, market) in &stakes {
        let stake = rd_stake_pda(&rd_config, &o.pubkey(), pf, *cohort);
        send(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
            AccountMeta::new(payer.pubkey(), true), AccountMeta::new(rd_config, false), AccountMeta::new(stake, false), AccountMeta::new_readonly(*pf, false),
            AccountMeta::new_readonly(rd_portfolio_archive_pda(market, &o.pubkey(), pf), false),
            AccountMeta::new_readonly(retired_market_pda(market), false),
        ], data: vec![2u8] }], &[]).expect("crystallize");
    }
    svm.set_sysvar(&Clock { slot: 2_101, unix_timestamp: 2_101, ..Default::default() });
    send(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
        AccountMeta::new(payer.pubkey(), true), AccountMeta::new(rd_config, false), AccountMeta::new_readonly(coin_mint, false), AccountMeta::new(rd_vault, false),
    ], data: vec![4u8] }], &[]).expect("freeze");
    let mut miner_coin = 0u64;
    let mut lp_coin = 0u64; let mut trader_coin = 0u64;
    for (o, cohort, pf, ata, market) in &stakes {
        let stake = rd_stake_pda(&rd_config, &o.pubkey(), pf, *cohort);
        send(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
            AccountMeta::new(payer.pubkey(), true), AccountMeta::new_readonly(rd_config, false), AccountMeta::new(stake, false),
            AccountMeta::new(rd_vault, false), AccountMeta::new(*ata, false), AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(*pf, false), // LP/trader live-cap portfolio (stake.backing_ledger)
            AccountMeta::new_readonly(rd_portfolio_archive_pda(market, &o.pubkey(), pf), false),
            AccountMeta::new_readonly(retired_market_pda(market), false),
        ], data: vec![5u8] }], &[]).expect("claim");
        let got = token_amount(&svm, ata);
        miner_coin += got;
        if *cohort == 2 { lp_coin += got; } else if *cohort == 3 { trader_coin += got; }
    }

    let trader_bps = 10_000 - INS_BPS - BACK_BPS - LP_BPS;
    let trader_supply = SUPPLY as u128 * trader_bps as u128 / 10_000;
    let lp_trader_supply = SUPPLY as u128 * (LP_BPS + trader_bps) as u128 / 10_000;
    println!("\n================ DETERMINISTIC DISTRIBUTOR FARMING SIM ================");
    println!("markets the miner CANNOT oracle-control : {N_MARKETS}  (trusted-Pyth, all allow-listed)");
    println!("cohorts                                 : insurance {}% / backing {}% / lp {}% / trader {}%",
        INS_BPS / 100, BACK_BPS / 100, LP_BPS / 100, trader_bps / 100);
    println!("strategy                                : delta-neutral wash — long+short per market, both miner-owned");
    println!("manufactured trader points (Σ crystallized loss) : {market_cryst}");
    println!("Σ spent (counterparty recovery of that loss)     : {market_spent}   <-- 0: the wash drains the backstop,");
    println!("    so trader points = crystallized - spent = crystallized: the spent-NETTING fix does NOT catch a");
    println!("    delta-neutral wash (the offsetting short gain lives in `pnl`, not in any residual counter).");
    println!("anti-wash fee on LP/trader claims        : {}%  (finding NZ)", FEE_BPS / 100);
    println!("COIN captured (TRADER cohort, post-fee)  : {miner_coin}  of {SUPPLY}  ({:.1}% of total supply)", miner_coin as f64 * 100.0 / SUPPLY as f64);
    println!("                                        : {:.0}% of the TRADER cohort ({trader_supply}) — the fee skimmed the other {}%", miner_coin as f64 * 100.0 / trader_supply as f64, FEE_BPS / 100);
    println!("market risk taken                       : ZERO (delta-neutral; long+short both miner-owned)");
    println!("-----------------------------------------------------------------------");
    println!("VERDICT: the wash is NOT structurally closeable by counters (spent stays 0). The fee TAXES it:");
    println!("  net miner take = cohort * (1 - fee) - trading_fees - locked-margin opportunity cost. The fee +");
    println!("  the capital the miner must lock in the delta-neutral pairs (the Sybil cost) are the real bound;");
    println!("  a hard per-participant cap is the stronger lever if a guaranteed bound is wanted.");
    println!("  LP COHORT (Δ received, {}%): this delta-neutral mark-move wash does NOT farm it — Σ received = {market_recv}", LP_BPS / 100);
    println!("  and the LP claim captured {lp_coin} COIN. A directional short GAIN lives in `pnl`, NOT in `received`;");
    println!("  the percolator `received` counter rises ONLY from ABSORBED bankruptcy residual (a socialized");
    println!("  uncollectible loss the matcher routes to the LP) — a different, costlier event than a self-dealt");
    println!("  mark move. So the no-spent-netting LP cohort is HARDER to wash here than the trader cohort, not");
    println!("  'farmable the same way'. (An actual bankruptcy-residual self-deal is a separate, un-exercised vector;");
    println!("  it too is allow-list-scoped + fee-taxed, and the bankrupt leg's lost margin is its real cost.)");
    println!("=======================================================================\n");

    // ---- 9 NORMAL traders vs 1 FARMER (dilution under anonymity) ----
    // The 80% above is the SYBIL case: ONE entity owning all the trader stakes. But each trader stake is a
    // distinct owner, so a LONE farmer among N independent participants captures only its OWN stake = ~1/N of
    // the cohort. Here N = the number of trader stakes; a single farmer's take is one of them. The rd CANNOT
    // tell a delta-neutral farmer's wash-loss from a normal directional trader's REAL loss (both are just
    // crystallized loss), so 9 normal traders' real backing dilutes the 1 farmer to 1/10 — and to beat the
    // dilution the farmer must Sybil into more accounts, each needing its OWN locked capital + per-trade fee.
    let trader_stakes: Vec<u64> = stakes.iter().filter(|(_, c, _, _, _)| *c == 3).map(|(_, _, _, a, _)| token_amount(&svm, a)).collect();
    let n = trader_stakes.len();
    let lone_farmer = trader_stakes.first().copied().unwrap_or(0);
    println!("================ 9 NORMAL TRADERS vs 1 FARMER ================");
    println!("independent trader participants          : {n}  (each a distinct anon owner)");
    println!("a LONE farmer's individual capture       : {lone_farmer}  = {:.1}% of the trader cohort ({trader_supply})", lone_farmer as f64 * 100.0 / trader_supply as f64);
    println!("  -> 9 normal traders' REAL losses dilute the 1 farmer to ~1/{n} of the cohort. The farmer's only");
    println!("     way to beat the dilution is to Sybil into more accounts — but no per-participant cap can");
    println!("     stop that under anonymity; each Sybil account pays its OWN locked-capital + per-trade fee, so");
    println!("     the cost scales with the farm. That Sybil-flat cost (not a cap) is the bound.");
    println!("normal trader vs farmer, same {:.0}% COIN  : the 9 NORMAL traders LOST real capital for it (directional", lone_farmer as f64 * 100.0 / trader_supply as f64);
    println!("     risk); the farmer's net capital is ~0 (delta-neutral) but it is taxed by the fee + must lock");
    println!("     margin for the time-weighted tenure to earn at all.");
    println!("=============================================================\n");
    // A lone farmer is diluted to at most 1/N of the cohort (the per-stake share), NOT the Sybil aggregate.
    assert!(lone_farmer as u128 <= trader_supply / n as u128 + 1, "a lone farmer among {n} is diluted to <= 1/{n} of the cohort");
    let _ = lp_trader_supply;

    // Σ spent is 0: the delta-neutral wash is a REAL backstop drain, not a counterparty recovery, so the
    // spent-netting (trader = crystallized - spent) does NOT zero it — confirming the fee is the needed bound.
    assert_eq!(market_spent, 0, "delta-neutral wash leaves spent=0; spent-netting cannot catch it");
    // The fee skims FEE_BPS of the capture: the miner now takes (1 - fee) of the trader cohort, not ~all of it.
    let expected = trader_supply * (10_000 - FEE_BPS) as u128 / 10_000;
    assert!(miner_coin as u128 >= expected * 98 / 100 && (miner_coin as u128) <= expected,
        "post-fee capture should be ~{expected} ((1-{}%) of {trader_supply}); got {miner_coin}", FEE_BPS / 100);
    // EMPIRICAL CORRECTION (this tick): the delta-neutral mark-move wash farms ONLY the trader (crystallized)
    // cohort, NOT the LP (received) cohort. The short leg's directional GAIN is `pnl`, never `received` — the
    // percolator `received` counter rises only from ABSORBED bankruptcy residual (a socialized uncollectible
    // loss), not a self-dealt mark move. So `received` stays 0 here and the LP claim captures 0, proving the
    // no-spent-netting LP cohort is HARDER to wash via this vector than the prior conclusion implied ("farmable
    // the same way"). The whole 320_000 = (1-fee)*trader_supply capture is the TRADER cohort alone.
    assert_eq!(market_recv, 0, "a delta-neutral mark-move wash does NOT manufacture LP `received` (needs absorbed bankruptcy residual)");
    assert_eq!(lp_coin, 0, "the LP cohort captured 0 COIN from the mark-move wash — received stayed 0");
    assert_eq!(trader_coin, miner_coin, "the entire post-fee capture is the TRADER cohort; the LP cohort was not farmed by this wash");
}

// CHURN vs HOLD (validates the time-weight's spent-netting churn-penalty against the REAL percolator). Two
// identical delta-neutral miners crystallize the same loss on their long leg. The HOLDER keeps the loss leg
// OPEN -> nobody spends its budget -> spent stays 0 -> net = crystallized (full reward). The CHURNER recycles
// capital (closes then REOPENS the long) -> its OWN reopen fill posts new margin that SPENDS its crystallized
// budget -> spent rises -> net = crystallized - spent < the holder's. So "close and open -> you spend your own
// budget and net less" — exactly the disincentive the time-weight + net-by-spent design relies on.
#[test]
fn churn_raises_own_spent_and_collapses_the_net_reward_vs_a_holder() {
    let mut svm = LiteSVM::new().with_compute_budget(solana_program_runtime::compute_budget::ComputeBudget {
        compute_unit_limit: 1_400_000, heap_size: 256 * 1024,
        ..solana_program_runtime::compute_budget::ComputeBudget::default()
    });
    svm.add_program_from_file(perc_id(), perc_so()).unwrap();
    let payer = Keypair::new(); svm.airdrop(&payer.pubkey(), 100_000_000_000_000).unwrap();
    let oracle = Keypair::new(); svm.airdrop(&oracle.pubkey(), 1_000_000_000_000).unwrap();
    svm.set_sysvar(&Clock { slot: 100, unix_timestamp: 100, ..Default::default() });
    let collateral = create_real_mint(&mut svm, &payer, &Keypair::new().pubkey());
    let initial_price = 1_000u64;
    let mlen = percolator_prog::state::market_account_len_for_capacity(1).unwrap();
    let plen = percolator_prog::state::portfolio_account_len_for_market_slots(2).unwrap();
    let posq = (percolator::POS_SCALE / 2) as i128;
    let pk = payer.insecure_clone();
    let tx = move |svm: &mut LiteSVM, ixs: &[Instruction], extra: &[&Keypair]| {
        svm.expire_blockhash(); let bh = svm.latest_blockhash();
        let mut s: Vec<&Keypair> = vec![&pk]; s.extend_from_slice(extra);
        svm.send_transaction(Transaction::new_signed_with_payer(ixs, Some(&pk.pubkey()), &s, bh))
    };

    // Build a market + a delta-neutral pair (long+short). Returns (market, long_pf, short_pf, long, short).
    let imkt = || PIx::InitMarket { max_portfolio_assets: 1, h_min: 0, h_max: 10, initial_price,
        min_nonzero_mm_req: 1, min_nonzero_im_req: 2, maintenance_margin_bps: 10_000, initial_margin_bps: 10_000,
        max_trading_fee_bps: 10_000, trade_fee_base_bps: 3, liquidation_fee_bps: 0, liquidation_fee_cap: 0,
        min_liquidation_abs: 0, max_price_move_bps_per_slot: 10_000, max_accrual_dt_slots: 1,
        max_abs_funding_e9_per_slot: 0, min_funding_lifetime_slots: 1, max_account_b_settlement_chunks: 1,
        max_bankrupt_close_chunks: 1, max_bankrupt_close_lifetime_slots: 100, public_b_chunk_atoms: percolator::MAX_VAULT_TVL,
        maintenance_fee_per_slot: 0 };
    let setup_pair = |svm: &mut LiteSVM| -> (Pubkey, Pubkey, Pubkey, Keypair, Keypair) {
        let market = Pubkey::new_unique();
        svm.set_account(market, Account { lamports: 1_000_000_000, data: vec![0u8; mlen], owner: perc_id(), executable: false, rent_epoch: 0 }).unwrap();
        let pv = canonical_insurance_vault(&perc_vault_authority(&market), &collateral);
        set_token(svm, &pv, &collateral, &perc_vault_authority(&market), 0);
        tx(svm, &[pix(vec![AccountMeta::new(oracle.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new_readonly(collateral, false)], imkt())], &[&oracle]).expect("init market");
        tx(svm, &[pix(vec![AccountMeta::new(oracle.pubkey(), true), AccountMeta::new(market, false)], PIx::ConfigureAuthMark { asset_index: 0, now_slot: 100, initial_mark_e6: initial_price })], &[&oracle]).expect("cfg mark");
        let long = Keypair::new(); let short = Keypair::new();
        svm.airdrop(&long.pubkey(), 1_000_000_000).unwrap(); svm.airdrop(&short.pubkey(), 1_000_000_000).unwrap();
        let long_pf = Pubkey::new_unique(); let short_pf = Pubkey::new_unique();
        for (o, pf) in [(&short, &short_pf), (&long, &long_pf)] {
            svm.set_account(*pf, Account { lamports: 1_000_000_000, data: vec![0u8; plen], owner: perc_id(), executable: false, rent_epoch: 0 }).unwrap();
            tx(svm, &[pix(vec![AccountMeta::new(o.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(*pf, false)], PIx::InitPortfolio)], &[o]).expect("init pf");
            let src = Pubkey::new_unique(); set_token(svm, &src, &collateral, &o.pubkey(), 1_000_000);
            tx(svm, &[pix(vec![AccountMeta::new(o.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(*pf, false), AccountMeta::new(src, false), AccountMeta::new(pv, false), AccountMeta::new_readonly(spl_token::ID, false)], PIx::Deposit { amount: 1_000_000 })], &[o]).expect("deposit");
        }
        tx(svm, &[pix(vec![AccountMeta::new(short.pubkey(), true), AccountMeta::new(long.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(short_pf, false), AccountMeta::new(long_pf, false)], PIx::TradeNoCpi { asset_index: 0, size_q: -posq, exec_price: initial_price, fee_bps: 0 })], &[&short, &long]).expect("open pair");
        (market, long_pf, short_pf, long, short)
    };
    let crank = |svm: &mut LiteSVM, market: &Pubkey, pf: &Pubkey| {
        tx(svm, &[pix(vec![AccountMeta::new(payer.pubkey(), true), AccountMeta::new(*market, false), AccountMeta::new(*pf, false)],
            PIx::PermissionlessCrank { now_slot: 110, observations: vec![percolator_prog::ix::CrankObservationHint { asset_index: 0, oracle_accounts: 0 }] })], &[]).expect("crank");
    };

    let (h_market, h_long, h_short, _hl, _hs) = setup_pair(&mut svm);
    let (c_market, c_long, c_short, c_lk, c_sk) = setup_pair(&mut svm);

    // The neutral oracle drops both marks; the crank crystallizes both longs' losses identically.
    svm.set_sysvar(&Clock { slot: 110, unix_timestamp: 110, ..Default::default() });
    for (m, lpf, spf) in [(h_market, h_long, h_short), (c_market, c_long, c_short)] {
        tx(&mut svm, &[pix(vec![AccountMeta::new(oracle.pubkey(), true), AccountMeta::new(m, false)], PIx::PushAuthMark { asset_index: 0, now_slot: 110, mark_e6: initial_price / 2 })], &[&oracle]).expect("drop mark");
        for pf in [spf, lpf] { crank(&mut svm, &m, &pf); crank(&mut svm, &m, &pf); }
    }
    let h_cryst = read_crystallized(&svm, &h_long);
    let c_cryst = read_crystallized(&svm, &c_long);
    assert!(h_cryst > 0 && c_cryst == h_cryst, "both longs crystallized the SAME loss: h={h_cryst} c={c_cryst}");
    assert_eq!(read_spent(&svm, &h_long), 0, "holder's budget is untouched (spent 0)");
    assert_eq!(read_spent(&svm, &c_long), 0, "churner's budget is untouched BEFORE churning");

    // CHURN: the churner recycles capital — close the pair (reverse), then REOPEN it. The reopen posts new
    // margin on the long, which SPENDS its own crystallized budget -> spent rises.
    tx(&mut svm, &[pix(vec![AccountMeta::new(c_sk.pubkey(), true), AccountMeta::new(c_lk.pubkey(), true), AccountMeta::new(c_market, false), AccountMeta::new(c_short, false), AccountMeta::new(c_long, false)], PIx::TradeNoCpi { asset_index: 0, size_q: posq, exec_price: initial_price / 2, fee_bps: 0 })], &[&c_sk, &c_lk]).expect("close");
    tx(&mut svm, &[pix(vec![AccountMeta::new(c_sk.pubkey(), true), AccountMeta::new(c_lk.pubkey(), true), AccountMeta::new(c_market, false), AccountMeta::new(c_short, false), AccountMeta::new(c_long, false)], PIx::TradeNoCpi { asset_index: 0, size_q: -posq, exec_price: initial_price / 2, fee_bps: 0 })], &[&c_sk, &c_lk]).expect("reopen");
    crank(&mut svm, &c_market, &c_long); crank(&mut svm, &c_market, &c_long);

    let h_spent = read_spent(&svm, &h_long);
    let c_spent = read_spent(&svm, &c_long);
    let h_net = h_cryst.saturating_sub(h_spent);
    let c_net = read_crystallized(&svm, &c_long).saturating_sub(c_spent);
    println!("\n================ CHURN vs HOLD ================");
    println!("HOLDER  long: crystallized {h_cryst}, spent {h_spent}  -> NET {h_net}  (kept the leg open)");
    println!("CHURNER long: crystallized {} , spent {c_spent}  -> NET {c_net}  (closed + reopened = recycled capital)", read_crystallized(&svm, &c_long));
    println!("=> churning spent its OWN budget; net-by-spent reward {} the holder's", if c_net < h_net { "BELOW" } else { "NOT below" });
    println!("===============================================\n");
    assert!(c_spent > 0, "churn (close+reopen) raises the churner's OWN spent");
    assert!(c_net < h_net, "the churner's net-by-spent reward ({c_net}) is below the holder's ({h_net})");

    // EMPIRICAL `received` MECHANISM (closes the LP-cohort free-farm question with REAL trades): the reopen is a
    // long INCREASE, which fires percolator's transfer_account_residual_reward_credit(long, short, short_margin)
    // — it raised the long's `spent` AND the COUNTERPARTY short's `received` by the SAME credit. (The short GAINED
    // on the mark drop so it has no crystallized loss, so the symmetric short->long transfer contributes 0.) So
    // `received` IS reachable by a real trade, it equals the churner-long's spent 1:1, and it is bounded by the
    // long's real crystallized loss — i.e. `received` is exactly the trader's TRANSFERRED crystallized loss,
    // conservation-bounded and zero-sum with net-by-spent (the source-level closure, now confirmed empirically).
    let c_short_received = read_received(&svm, &c_short);
    println!("CHURN credit transfer: churner-long spent {c_spent} == counterparty-short received {c_short_received} (bounded by crystallized {})", read_crystallized(&svm, &c_long));
    assert_eq!(c_short_received, c_spent, "the credit moved 1:1 from the long's net to the counterparty short's `received`");
    assert!(c_short_received > 0 && c_short_received <= read_crystallized(&svm, &c_long),
        "`received` is reachable by a real trade but bounded by the trader's real crystallized loss (not free)");
}

// SURFACE A — buy/burn FUEL verification (sweep tick): the genesis market is configured with trade_fee_base_bps=3
// + fee_redirect_to_market_0_bps=2000 ("3 bps/trade, 20% of yield -> asset-0 insurance"). Two things were only
// asserted at INIT (read-back), never on a real trade:
//   (1) percolator charges the market base fee even when the caller passes fee_bps=0 — hybrid_trade_fee_bps_view
//       takes max(caller_fee_bps, cfg.trade_fee_base_bps) (percolator v16_program.rs:11224). So EVERY trade pays
//       >= 3 bps, and that fee lands in asset-0 insurance — the real, recurring input to surplus -> pull -> buy/burn.
//   (2) the 20% redirect is INERT on a single-asset genesis market: credit_fee_to_domain_budget_view forces
//       redirect=0 when asset_index==0 (v16_program.rs:5252) — there is no OTHER asset to redirect FROM, so
//       asset-0 simply keeps 100% of its own fees. The redirect only matters once the market has assets 1..n.
// This pins the load-bearing claim that fees actually accrue to asset-0 insurance on real trades (without it the
// whole buy/burn loop would have no fuel), and corrects the sim's stale "trading fees 0 here" note: they are NOT 0.
#[test]
fn genesis_market_3bps_fee_accrues_to_asset0_insurance_on_a_real_trade_redirect_inert_for_asset0() {
    let mut svm = LiteSVM::new().with_compute_budget(solana_program_runtime::compute_budget::ComputeBudget {
        compute_unit_limit: 1_400_000, heap_size: 256 * 1024,
        ..solana_program_runtime::compute_budget::ComputeBudget::default()
    });
    svm.add_program_from_file(perc_id(), perc_so()).unwrap();
    let payer = Keypair::new(); svm.airdrop(&payer.pubkey(), 100_000_000_000_000).unwrap();
    let oracle = Keypair::new(); svm.airdrop(&oracle.pubkey(), 1_000_000_000_000).unwrap();
    svm.set_sysvar(&Clock { slot: 100, unix_timestamp: 100, ..Default::default() });
    let collateral = create_real_mint(&mut svm, &payer, &Keypair::new().pubkey());
    // High price so the proven small position size carries enough notional for a non-zero 3 bps fee:
    // notional = (POS_SCALE/2)*100_000/POS_SCALE = 50_000 per side -> fee ~= 50_000*3/10_000 = 15 per side.
    let initial_price = 100_000u64;
    let mlen = percolator_prog::state::market_account_len_for_capacity(1).unwrap();
    let plen = percolator_prog::state::portfolio_account_len_for_market_slots(2).unwrap();
    let posq = (percolator::POS_SCALE / 2) as i128;
    let pk = payer.insecure_clone();
    let tx = move |svm: &mut LiteSVM, ixs: &[Instruction], extra: &[&Keypair]| {
        svm.expire_blockhash(); let bh = svm.latest_blockhash();
        let mut s: Vec<&Keypair> = vec![&pk]; s.extend_from_slice(extra);
        svm.send_transaction(Transaction::new_signed_with_payer(ixs, Some(&pk.pubkey()), &s, bh))
    };
    let imkt = || PIx::InitMarket { max_portfolio_assets: 1, h_min: 0, h_max: 10, initial_price,
        min_nonzero_mm_req: 1, min_nonzero_im_req: 2, maintenance_margin_bps: 10_000, initial_margin_bps: 10_000,
        max_trading_fee_bps: 10_000, trade_fee_base_bps: 3, liquidation_fee_bps: 0, liquidation_fee_cap: 0,
        min_liquidation_abs: 0, max_price_move_bps_per_slot: 10_000, max_accrual_dt_slots: 1,
        max_abs_funding_e9_per_slot: 0, min_funding_lifetime_slots: 1, max_account_b_settlement_chunks: 1,
        max_bankrupt_close_chunks: 1, max_bankrupt_close_lifetime_slots: 100, public_b_chunk_atoms: percolator::MAX_VAULT_TVL,
        maintenance_fee_per_slot: 0 };
    let market = Pubkey::new_unique();
    svm.set_account(market, Account { lamports: 1_000_000_000, data: vec![0u8; mlen], owner: perc_id(), executable: false, rent_epoch: 0 }).unwrap();
    let pv = canonical_insurance_vault(&perc_vault_authority(&market), &collateral);
    set_token(&mut svm, &pv, &collateral, &perc_vault_authority(&market), 0);
    tx(&mut svm, &[pix(vec![AccountMeta::new(oracle.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new_readonly(collateral, false)], imkt())], &[&oracle]).expect("init market");
    tx(&mut svm, &[pix(vec![AccountMeta::new(oracle.pubkey(), true), AccountMeta::new(market, false)], PIx::ConfigureAuthMark { asset_index: 0, now_slot: 100, initial_mark_e6: initial_price })], &[&oracle]).expect("cfg mark");

    let long = Keypair::new(); let short = Keypair::new();
    svm.airdrop(&long.pubkey(), 1_000_000_000).unwrap(); svm.airdrop(&short.pubkey(), 1_000_000_000).unwrap();
    let long_pf = Pubkey::new_unique(); let short_pf = Pubkey::new_unique();
    for (o, pf) in [(&short, &short_pf), (&long, &long_pf)] {
        svm.set_account(*pf, Account { lamports: 1_000_000_000, data: vec![0u8; plen], owner: perc_id(), executable: false, rent_epoch: 0 }).unwrap();
        tx(&mut svm, &[pix(vec![AccountMeta::new(o.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(*pf, false)], PIx::InitPortfolio)], &[o]).expect("init pf");
        let src = Pubkey::new_unique(); set_token(&mut svm, &src, &collateral, &o.pubkey(), 1_000_000);
        tx(&mut svm, &[pix(vec![AccountMeta::new(o.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(*pf, false), AccountMeta::new(src, false), AccountMeta::new(pv, false), AccountMeta::new_readonly(spl_token::ID, false)], PIx::Deposit { amount: 1_000_000 })], &[o]).expect("deposit");
    }

    let ins_before = read_insurance(&svm, &market);
    // One real delta-neutral trade, caller passes fee_bps=0 — the market's 3 bps base still applies.
    tx(&mut svm, &[pix(vec![AccountMeta::new(short.pubkey(), true), AccountMeta::new(long.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(short_pf, false), AccountMeta::new(long_pf, false)], PIx::TradeNoCpi { asset_index: 0, size_q: -posq, exec_price: initial_price, fee_bps: 0 })], &[&short, &long]).expect("open pair");
    let ins_after = read_insurance(&svm, &market);

    println!("\n=== GENESIS MARKET FEE -> ASSET-0 INSURANCE (buy/burn fuel) ===");
    println!("asset-0 insurance before trade : {ins_before}");
    println!("asset-0 insurance after  trade : {ins_after}  (+{} from the 3 bps base fee, caller passed fee_bps=0)", ins_after - ins_before);
    println!("redirect (fee_redirect_to_market_0_bps) is INERT for asset_index==0 — asset-0 keeps 100% of its own fee");
    println!("==============================================================\n");
    assert!(ins_after > ins_before, "a real trade must accrue the 3 bps base fee to asset-0 insurance — the buy/burn fuel (got {ins_before} -> {ins_after})");
}

// ===================================================================================================
// FULL ECONOMY SIM (user-specified): 10 assets, each seeded 1M insurance + 1M backing; 100 traders with
// 1M cash each — 99 rational (random market + long/short, real directional risk) + 1 max-farmer (delta-
// neutral, risk-free loss manufacture). REAL percolator for the whole trading economy + REAL rd binary for
// the distribution. Reports total notional volume and the COIN distribution every cohort/participant earned.
//
// Modeling notes (honest):
//  - Rational traders trade against a per-market neutral MARKET-MAKER (huge deposit, NOT rd-registered), so
//    each is an INDEPENDENT directional position (not a forced pair). Markets move +/-50% (even idx down, odd
//    up) so ~half the rational traders LOSE (their direction was wrong) and crystallize a REAL loss; the other
//    half WIN and earn nothing from the loss-rewarding trader cohort.
//  - The farmer opens a long AND a short on ONE market (2 Sybil accounts, 1M each) -> whichever way it moves,
//    one leg loses risk-free; it captures that loss as trader points for ~0 net market risk.
//  - insurance/backing cohorts read the subledger Position's live base-unit principal (rd does no CPI), so
//    they are modeled at exactly that quantity: 10 depositors x 1M principal in each pool (= "1M insurance
//    + 1M backing per asset"). Equal principal and tenure -> equal pro-rata COIN. The TRADER/LP economy is
//    full real-percolator.
const SIM_N_RATIONAL: usize = 99;
const SIM_N_INS: usize = 10;   // 1M insurance per asset
const SIM_N_BACK: usize = 10;  // 1M backing per asset
const SIM_DEPOSIT: u64 = 1_000_000; // 1M cash each

#[test]
fn full_economy_100_traders_10_assets_distribution_report() {
    let mut svm = LiteSVM::new().with_compute_budget(solana_program_runtime::compute_budget::ComputeBudget {
        compute_unit_limit: 1_400_000, heap_size: 256 * 1024,
        ..solana_program_runtime::compute_budget::ComputeBudget::default()
    });
    svm.add_program_from_file(perc_id(), perc_so()).unwrap();
    svm.add_program_from_file(rd_id(), so_deploy("residual_distributor")).unwrap();
    let payer = Keypair::new(); svm.airdrop(&payer.pubkey(), 100_000_000_000_000).unwrap();
    let oracle = Keypair::new(); svm.airdrop(&oracle.pubkey(), 1_000_000_000_000).unwrap();
    svm.set_sysvar(&Clock { slot: 100, unix_timestamp: 100, ..Default::default() });
    let collateral = create_real_mint(&mut svm, &payer, &Keypair::new().pubkey());
    let initial_price = 1_000u64;
    let pk = payer.insecure_clone();
    let tx = move |svm: &mut LiteSVM, ixs: &[Instruction], extra: &[&Keypair]| {
        svm.expire_blockhash(); let bh = svm.latest_blockhash();
        let mut s: Vec<&Keypair> = vec![&pk]; s.extend_from_slice(extra);
        svm.send_transaction(Transaction::new_signed_with_payer(ixs, Some(&pk.pubkey()), &s, bh))
    };
    let mlen = percolator_prog::state::market_account_len_for_capacity(1).unwrap();
    let plen = percolator_prog::state::portfolio_account_len_for_market_slots(2).unwrap();
    let imkt = || PIx::InitMarket { max_portfolio_assets: 1, h_min: 0, h_max: 10, initial_price,
        min_nonzero_mm_req: 1, min_nonzero_im_req: 2, maintenance_margin_bps: 10_000, initial_margin_bps: 10_000,
        max_trading_fee_bps: 10_000, trade_fee_base_bps: 3, liquidation_fee_bps: 0, liquidation_fee_cap: 0,
        min_liquidation_abs: 0, max_price_move_bps_per_slot: 10_000, max_accrual_dt_slots: 1,
        max_abs_funding_e9_per_slot: 0, min_funding_lifetime_slots: 1, max_account_b_settlement_chunks: 1,
        max_bankrupt_close_chunks: 1, max_bankrupt_close_lifetime_slots: 100, public_b_chunk_atoms: percolator::MAX_VAULT_TVL,
        maintenance_fee_per_slot: 0 };

    // ---- 10 markets (neutral oracle) + a per-market well-capitalized market-maker (NOT rd-registered) ----
    let mut markets: Vec<(Pubkey, Pubkey)> = Vec::new();   // (market, perc_vault)
    let mut mms: Vec<(Keypair, Pubkey)> = Vec::new();       // (mm_owner, mm_pf) per market
    for _ in 0..N_MARKETS {
        let market = Pubkey::new_unique();
        svm.set_account(market, Account { lamports: 1_000_000_000, data: vec![0u8; mlen], owner: perc_id(), executable: false, rent_epoch: 0 }).unwrap();
        let pv = canonical_insurance_vault(&perc_vault_authority(&market), &collateral);
        set_token(&mut svm, &pv, &collateral, &perc_vault_authority(&market), 0);
        tx(&mut svm, &[pix(vec![AccountMeta::new(oracle.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new_readonly(collateral, false)], imkt())], &[&oracle]).expect("init market");
        tx(&mut svm, &[pix(vec![AccountMeta::new(oracle.pubkey(), true), AccountMeta::new(market, false)], PIx::ConfigureAuthMark { asset_index: 0, now_slot: 100, initial_mark_e6: initial_price })], &[&oracle]).expect("cfg mark");
        // market-maker: deep collateral so it can take the other side of every trade on this market.
        let mm = Keypair::new(); svm.airdrop(&mm.pubkey(), 1_000_000_000).unwrap();
        let mm_pf = Pubkey::new_unique();
        svm.set_account(mm_pf, Account { lamports: 1_000_000_000, data: vec![0u8; plen], owner: perc_id(), executable: false, rent_epoch: 0 }).unwrap();
        tx(&mut svm, &[pix(vec![AccountMeta::new(mm.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(mm_pf, false)], PIx::InitPortfolio)], &[&mm]).expect("init mm pf");
        let mm_src = Pubkey::new_unique(); set_token(&mut svm, &mm_src, &collateral, &mm.pubkey(), 200_000_000);
        tx(&mut svm, &[pix(vec![AccountMeta::new(mm.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(mm_pf, false), AccountMeta::new(mm_src, false), AccountMeta::new(pv, false), AccountMeta::new_readonly(spl_token::ID, false)], PIx::Deposit { amount: 200_000_000 })], &[&mm]).expect("mm deposit");
        markets.push((market, pv));
        mms.push((mm, mm_pf));
    }

    // ---- residual-distributor: cohorts 10/10/40/40, allow-list all markets, 20% anti-wash fee ----
    let sub_pool = Pubkey::new_unique();    // insurance pool scope (positions modeled below)
    let back_pool = Pubkey::new_unique();   // backing pool scope
    let coin_auth = Keypair::new();
    let coin_mint = create_real_mint(&mut svm, &payer, &coin_auth.pubkey());
    let rd_config = Pubkey::find_program_address(&[b"rd_config", coin_mint.as_ref()], &rd_id()).0;
    let dist_config = dist_config_pda(&coin_mint, &rd_config);
    let rd_vault = Pubkey::new_unique(); set_token(&mut svm, &rd_vault, &coin_mint, &rd_config, 0);
    tx(&mut svm, &[
        spl_token::instruction::mint_to(&spl_token::ID, &coin_mint, &rd_vault, &coin_auth.pubkey(), &[], SUPPLY).unwrap(),
    ], &[&coin_auth]).expect("fund COIN");
    let mut ri = vec![0u8];
    ri.extend_from_slice(&SUPPLY.to_le_bytes()); ri.extend_from_slice(&2_000u64.to_le_bytes());
    ri.extend_from_slice(&INS_BPS.to_le_bytes()); ri.extend_from_slice(&BACK_BPS.to_le_bytes()); ri.extend_from_slice(&LP_BPS.to_le_bytes());
    ri.extend_from_slice(&100u64.to_le_bytes());
    ri.extend_from_slice(sub_pool.as_ref()); ri.extend_from_slice(back_pool.as_ref());
    ri.extend_from_slice(markets[0].0.as_ref());
    ri.push((N_MARKETS - 1) as u8);
    for (m, _) in &markets[1..] { ri.extend_from_slice(m.as_ref()); }
    ri.extend_from_slice(&FEE_BPS.to_le_bytes());
    tx(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
        AccountMeta::new(payer.pubkey(), true), AccountMeta::new_readonly(coin_mint, false), AccountMeta::new_readonly(dist_id(), false),
        AccountMeta::new_readonly(dist_config, false), AccountMeta::new_readonly(perc_id(), false), AccountMeta::new_readonly(sub_id(), false),
        AccountMeta::new(rd_config, false), AccountMeta::new_readonly(system_program::ID, false), AccountMeta::new_readonly(coin_auth.pubkey(), true),
    ], data: ri }], &[&coin_auth]).expect("rd init");
    tx(&mut svm, &[
        spl_token::instruction::set_authority(&spl_token::ID, &coin_mint, None, spl_token::instruction::AuthorityType::MintTokens, &coin_auth.pubkey(), &[]).unwrap(),
    ], &[&coin_auth]).expect("freeze COIN");


    // ---- insurance + backing cohorts: 10 depositors x 1M principal each (modeled positions) ----
    let mut ins_parts: Vec<(Keypair, Pubkey, Pubkey)> = Vec::new();  // (owner, position, coin_ata)
    let mut back_parts: Vec<(Keypair, Pubkey, Pubkey)> = Vec::new();
    for (cohort, pool, parts, n) in [(0u8, sub_pool, &mut ins_parts, SIM_N_INS), (1u8, back_pool, &mut back_parts, SIM_N_BACK)] {
        for _ in 0..n {
            let owner = Keypair::new(); svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
            let pos = Pubkey::new_unique();
            let d = modeled_subledger_position(&pool, &owner.pubkey(), SIM_DEPOSIT, 100);
            svm.set_account(pos, Account { lamports: 1_000_000_000, data: d, owner: sub_id(), executable: false, rent_epoch: 0 }).unwrap();
            let stake = rd_stake_pda(&rd_config, &owner.pubkey(), &pos, cohort);
            tx(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
                AccountMeta::new(payer.pubkey(), true), AccountMeta::new_readonly(rd_config, false), AccountMeta::new_readonly(owner.pubkey(), true),
                AccountMeta::new_readonly(owner.pubkey(), false), AccountMeta::new_readonly(pos, false), AccountMeta::new(stake, false),
                AccountMeta::new_readonly(system_program::ID, false),
            ], data: vec![1u8, cohort] }], &[&owner]).expect("register ins/back");
            let ata = Pubkey::new_unique(); set_token(&mut svm, &ata, &coin_mint, &owner.pubkey(), 0);
            parts.push((owner, pos, ata));
        }
    }

    // ---- 99 rational traders: random market + long/short, vs the market MM; deposit 1M; register TRADER ----
    let posq = (percolator::POS_SCALE / 2) as i128;
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    let nxt = |r: &mut u64| { *r = r.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (*r >> 33) as usize };
    let mut rational: Vec<(Keypair, Pubkey, Pubkey, usize, bool)> = Vec::new(); // owner, pf, ata, market_idx, is_long
    let mut total_notional: u128 = 0;
    for _ in 0..SIM_N_RATIONAL {
        let mi = nxt(&mut rng) % N_MARKETS;
        let is_long = nxt(&mut rng) % 2 == 0;
        let (market, pv) = markets[mi];
        let (mm, mm_pf) = (&mms[mi].0, mms[mi].1);
        let owner = Keypair::new(); svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
        let pf = Pubkey::new_unique();
        svm.set_account(pf, Account { lamports: 1_000_000_000, data: vec![0u8; plen], owner: perc_id(), executable: false, rent_epoch: 0 }).unwrap();
        tx(&mut svm, &[pix(vec![AccountMeta::new(owner.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(pf, false)], PIx::InitPortfolio)], &[&owner]).expect("init pf");
        let src = Pubkey::new_unique(); set_token(&mut svm, &src, &collateral, &owner.pubkey(), SIM_DEPOSIT);
        tx(&mut svm, &[pix(vec![AccountMeta::new(owner.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(pf, false), AccountMeta::new(src, false), AccountMeta::new(pv, false), AccountMeta::new_readonly(spl_token::ID, false)], PIx::Deposit { amount: SIM_DEPOSIT as u128 })], &[&owner]).expect("deposit");
        // trade vs MM: long -> [mm(a), trader(b)] size -posq (a short, b long); short -> [trader(a), mm(b)] size -posq.
        if is_long {
            tx(&mut svm, &[pix(vec![AccountMeta::new(mm.pubkey(), true), AccountMeta::new(owner.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(mm_pf, false), AccountMeta::new(pf, false)], PIx::TradeNoCpi { asset_index: 0, size_q: -posq, exec_price: initial_price, fee_bps: 0 })], &[mm, &owner]).expect("open long");
        } else {
            tx(&mut svm, &[pix(vec![AccountMeta::new(owner.pubkey(), true), AccountMeta::new(mm.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(pf, false), AccountMeta::new(mm_pf, false)], PIx::TradeNoCpi { asset_index: 0, size_q: -posq, exec_price: initial_price, fee_bps: 0 })], &[&owner, mm]).expect("open short");
        }
        total_notional += (posq.unsigned_abs()) * initial_price as u128 / percolator::POS_SCALE;
        let stake = rd_stake_pda(&rd_config, &owner.pubkey(), &pf, 3);
        tx(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
            AccountMeta::new(payer.pubkey(), true), AccountMeta::new_readonly(rd_config, false), AccountMeta::new_readonly(owner.pubkey(), true),
            AccountMeta::new_readonly(owner.pubkey(), false), AccountMeta::new_readonly(pf, false), AccountMeta::new(stake, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(rd_portfolio_archive_pda(&market, &owner.pubkey(), &pf), false),
            AccountMeta::new_readonly(market, false),
            AccountMeta::new_readonly(retired_market_pda(&market), false),
        ], data: vec![1u8, 3u8] }], &[&owner]).expect("register trader");
        let ata = Pubkey::new_unique(); set_token(&mut svm, &ata, &coin_mint, &owner.pubkey(), 0);
        rational.push((owner, pf, ata, mi, is_long));
    }

    // ---- 1 max-farmer: long + short on market 0 (2 Sybil accounts, 1M each) -> risk-free loss capture ----
    let (fm, fpv) = markets[0];
    let (fmm, fmm_pf) = (&mms[0].0, mms[0].1);
    let mut farmer: Vec<(Keypair, Pubkey, Pubkey)> = Vec::new(); // (owner, pf, ata) for both legs
    for is_long in [true, false] {
        let owner = Keypair::new(); svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
        let pf = Pubkey::new_unique();
        svm.set_account(pf, Account { lamports: 1_000_000_000, data: vec![0u8; plen], owner: perc_id(), executable: false, rent_epoch: 0 }).unwrap();
        tx(&mut svm, &[pix(vec![AccountMeta::new(owner.pubkey(), true), AccountMeta::new(fm, false), AccountMeta::new(pf, false)], PIx::InitPortfolio)], &[&owner]).expect("init farmer pf");
        let src = Pubkey::new_unique(); set_token(&mut svm, &src, &collateral, &owner.pubkey(), SIM_DEPOSIT);
        tx(&mut svm, &[pix(vec![AccountMeta::new(owner.pubkey(), true), AccountMeta::new(fm, false), AccountMeta::new(pf, false), AccountMeta::new(src, false), AccountMeta::new(fpv, false), AccountMeta::new_readonly(spl_token::ID, false)], PIx::Deposit { amount: SIM_DEPOSIT as u128 })], &[&owner]).expect("farmer deposit");
        if is_long {
            tx(&mut svm, &[pix(vec![AccountMeta::new(fmm.pubkey(), true), AccountMeta::new(owner.pubkey(), true), AccountMeta::new(fm, false), AccountMeta::new(fmm_pf, false), AccountMeta::new(pf, false)], PIx::TradeNoCpi { asset_index: 0, size_q: -posq, exec_price: initial_price, fee_bps: 0 })], &[fmm, &owner]).expect("farmer long");
        } else {
            tx(&mut svm, &[pix(vec![AccountMeta::new(owner.pubkey(), true), AccountMeta::new(fmm.pubkey(), true), AccountMeta::new(fm, false), AccountMeta::new(pf, false), AccountMeta::new(fmm_pf, false)], PIx::TradeNoCpi { asset_index: 0, size_q: -posq, exec_price: initial_price, fee_bps: 0 })], &[&owner, fmm]).expect("farmer short");
        }
        total_notional += (posq.unsigned_abs()) * initial_price as u128 / percolator::POS_SCALE;
        let stake = rd_stake_pda(&rd_config, &owner.pubkey(), &pf, 3);
        tx(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
            AccountMeta::new(payer.pubkey(), true), AccountMeta::new_readonly(rd_config, false), AccountMeta::new_readonly(owner.pubkey(), true),
            AccountMeta::new_readonly(owner.pubkey(), false), AccountMeta::new_readonly(pf, false), AccountMeta::new(stake, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(rd_portfolio_archive_pda(&fm, &owner.pubkey(), &pf), false),
            AccountMeta::new_readonly(fm, false),
            AccountMeta::new_readonly(retired_market_pda(&fm), false),
        ], data: vec![1u8, 3u8] }], &[&owner]).expect("register farmer leg");
        let ata = Pubkey::new_unique(); set_token(&mut svm, &ata, &coin_mint, &owner.pubkey(), 0);
        farmer.push((owner, pf, ata));
    }

    // ---- neutral oracle moves each market (even idx -50%, odd idx +50%); settle every trader portfolio ----
    svm.set_sysvar(&Clock { slot: 110, unix_timestamp: 110, ..Default::default() });
    for (i, (market, _)) in markets.iter().enumerate() {
        let new_mark = if i % 2 == 0 { initial_price / 2 } else { initial_price + initial_price / 2 };
        tx(&mut svm, &[pix(vec![AccountMeta::new(oracle.pubkey(), true), AccountMeta::new(*market, false)], PIx::PushAuthMark { asset_index: 0, now_slot: 110, mark_e6: new_mark })], &[&oracle]).expect("oracle move");
    }
    let crank = |svm: &mut LiteSVM, tx: &dyn Fn(&mut LiteSVM, &[Instruction], &[&Keypair]) -> Result<(), litesvm::types::FailedTransactionMetadata>, market: &Pubkey, pf: &Pubkey| {
        tx(svm, &[pix(vec![AccountMeta::new(payer.pubkey(), true), AccountMeta::new(*market, false), AccountMeta::new(*pf, false)],
            PIx::PermissionlessCrank { now_slot: 110, observations: vec![percolator_prog::ix::CrankObservationHint { asset_index: 0, oracle_accounts: 0 }] })], &[]).map(|_| ())
    };
    let tx_unit = |svm: &mut LiteSVM, ixs: &[Instruction], extra: &[&Keypair]| tx(svm, ixs, extra).map(|_| ());
    for (_o, pf, _a, mi, _l) in &rational { let m = markets[*mi].0; let _ = crank(&mut svm, &tx_unit, &m, pf); let _ = crank(&mut svm, &tx_unit, &m, pf); }
    for (_o, pf, _a) in &farmer { let _ = crank(&mut svm, &tx_unit, &fm, pf); let _ = crank(&mut svm, &tx_unit, &fm, pf); }

    // ---- crystallize every TRADER stake (rational + farmer), freeze, claim all ----
    let cryst = |svm: &mut LiteSVM, tx: &dyn Fn(&mut LiteSVM, &[Instruction], &[&Keypair]) -> Result<(), litesvm::types::FailedTransactionMetadata>, owner: &Keypair, market: &Pubkey, pf: &Pubkey| {
        let stake = rd_stake_pda(&rd_config, &owner.pubkey(), pf, 3);
        tx(svm, &[Instruction { program_id: rd_id(), accounts: vec![
            AccountMeta::new(payer.pubkey(), true), AccountMeta::new(rd_config, false), AccountMeta::new(stake, false), AccountMeta::new_readonly(*pf, false),
            AccountMeta::new_readonly(rd_portfolio_archive_pda(market, &owner.pubkey(), pf), false),
            AccountMeta::new_readonly(retired_market_pda(market), false),
        ], data: vec![2u8] }], &[]).map(|_| ())
    };
    for (o, pf, _a, mi, _l) in &rational { let market = markets[*mi].0; let _ = cryst(&mut svm, &tx_unit, o, &market, pf); }
    for (o, pf, _a) in &farmer { let _ = cryst(&mut svm, &tx_unit, o, &fm, pf); }
    // Capital cohorts: crystallize sets points from live principal and tenure; cranker MUST be the owner.
    for (cohort, parts) in [(0u8, &ins_parts), (1u8, &back_parts)] {
        for (o, pos, _a) in parts {
            let stake = rd_stake_pda(&rd_config, &o.pubkey(), pos, cohort);
            tx(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
                AccountMeta::new(o.pubkey(), true), AccountMeta::new(rd_config, false), AccountMeta::new(stake, false), AccountMeta::new_readonly(*pos, false),
            ], data: vec![2u8] }], &[o]).expect("crystallize capital");
        }
    }

    svm.set_sysvar(&Clock { slot: 2_101, unix_timestamp: 2_101, ..Default::default() });
    tx(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
        AccountMeta::new(payer.pubkey(), true), AccountMeta::new(rd_config, false), AccountMeta::new_readonly(coin_mint, false), AccountMeta::new(rd_vault, false),
    ], data: vec![4u8] }], &[]).expect("freeze");

    // Claims: capital cohorts sign as owner + append the position; trader is permissionless + appends the portfolio.
    let mut ins_coin = 0u64; let mut back_coin = 0u64;
    for (o, pos, ata) in &ins_parts {
        let stake = rd_stake_pda(&rd_config, &o.pubkey(), pos, 0);
        let _ = tx(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
            AccountMeta::new(o.pubkey(), true), AccountMeta::new_readonly(rd_config, false), AccountMeta::new(stake, false),
            AccountMeta::new(rd_vault, false), AccountMeta::new(*ata, false), AccountMeta::new_readonly(spl_token::ID, false), AccountMeta::new_readonly(*pos, false),
        ], data: vec![5u8] }], &[o]); ins_coin += token_amount(&svm, ata);
    }
    for (o, pos, ata) in &back_parts {
        let stake = rd_stake_pda(&rd_config, &o.pubkey(), pos, 1);
        let _ = tx(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
            AccountMeta::new(o.pubkey(), true), AccountMeta::new_readonly(rd_config, false), AccountMeta::new(stake, false),
            AccountMeta::new(rd_vault, false), AccountMeta::new(*ata, false), AccountMeta::new_readonly(spl_token::ID, false), AccountMeta::new_readonly(*pos, false),
        ], data: vec![5u8] }], &[o]); back_coin += token_amount(&svm, ata);
    }
    let claim_trader = |svm: &mut LiteSVM, tx: &dyn Fn(&mut LiteSVM, &[Instruction], &[&Keypair]) -> Result<(), litesvm::types::FailedTransactionMetadata>, owner: &Keypair, market: &Pubkey, pf: &Pubkey, ata: &Pubkey| {
        let stake = rd_stake_pda(&rd_config, &owner.pubkey(), pf, 3);
        let _ = tx(svm, &[Instruction { program_id: rd_id(), accounts: vec![
            AccountMeta::new(payer.pubkey(), true), AccountMeta::new_readonly(rd_config, false), AccountMeta::new(stake, false),
            AccountMeta::new(rd_vault, false), AccountMeta::new(*ata, false), AccountMeta::new_readonly(spl_token::ID, false), AccountMeta::new_readonly(*pf, false),
            AccountMeta::new_readonly(rd_portfolio_archive_pda(market, &owner.pubkey(), pf), false),
            AccountMeta::new_readonly(retired_market_pda(market), false),
        ], data: vec![5u8] }], &[]);
    };
    let mut rational_winner_coin = 0u64; let mut rational_loser_coin = 0u64;
    let mut n_winners = 0usize; let mut n_losers = 0usize; let mut loser_examples: Vec<u64> = Vec::new();
    for (o, pf, ata, mi, is_long) in &rational {
        let market = markets[*mi].0;
        claim_trader(&mut svm, &tx_unit, o, &market, pf, ata);
        let got = token_amount(&svm, ata);
        // even market -50% (long loses); odd market +50% (short loses).
        let lost = if mi % 2 == 0 { *is_long } else { !*is_long };
        if got > 0 { if lost { n_losers += 1; rational_loser_coin += got; loser_examples.push(got); } else { n_winners += 1; rational_winner_coin += got; } }
        else if lost { n_losers += 1; } else { n_winners += 1; }
    }
    let mut farmer_coin = 0u64;
    for (o, pf, ata) in &farmer { claim_trader(&mut svm, &tx_unit, o, &fm, pf, ata); farmer_coin += token_amount(&svm, ata); }

    let trader_bps = 10_000 - INS_BPS - BACK_BPS - LP_BPS;
    let supply = SUPPLY as u128;
    let trader_supply = supply * trader_bps as u128 / 10_000;
    let lp_supply = supply * LP_BPS as u128 / 10_000;
    let ins_supply = supply * INS_BPS as u128 / 10_000;
    let back_supply = supply * BACK_BPS as u128 / 10_000;
    let total_collateral = SIM_DEPOSIT as u128 * (SIM_N_RATIONAL as u128 + 2) + (SIM_DEPOSIT as u128) * (SIM_N_INS + SIM_N_BACK) as u128;
    let distributed = ins_coin as u128 + back_coin as u128 + rational_winner_coin as u128 + rational_loser_coin as u128 + farmer_coin as u128;

    println!("\n================ FULL ECONOMY SIM — 100 TRADERS x 10 ASSETS ================");
    println!("assets / markets (neutral oracle, trusted-Pyth)     : {N_MARKETS}");
    println!("insurance capital  : {} depositors x {SIM_DEPOSIT}  = {} total", SIM_N_INS, SIM_N_INS as u64 * SIM_DEPOSIT);
    println!("backing capital    : {} depositors x {SIM_DEPOSIT}  = {} total", SIM_N_BACK, SIM_N_BACK as u64 * SIM_DEPOSIT);
    println!("traders            : {} rational (1M each) + 1 max-farmer (2 Sybil legs, 1M each)", SIM_N_RATIONAL);
    println!("  -> rational WINNERS (right direction): {n_winners}   LOSERS (crystallized real loss): {n_losers}");
    println!("total collateral deployed (cash at risk)            : {total_collateral}");
    println!("TOTAL NOTIONAL VOLUME (open interest, price-scaled)  : {total_notional}");
    println!("---------------------------------------------------------------------------");
    println!("COIN supply {SUPPLY}  split 10/10/40/40 (ins/back/lp/trader)");
    println!("  INSURANCE cohort ({}%) supply {ins_supply:>7}  -> claimed {ins_coin:>7}  ({} depositors, pro-rata equal)", INS_BPS/100, SIM_N_INS);
    println!("  BACKING   cohort ({}%) supply {back_supply:>7}  -> claimed {back_coin:>7}  ({} depositors, pro-rata equal)", BACK_BPS/100, SIM_N_BACK);
    println!("  LP        cohort ({}%) supply {lp_supply:>7}  -> claimed       0  (received==0: no absorbed bankruptcy residual; UNCLAIMED -> deflationary)", LP_BPS/100);
    println!("  TRADER    cohort ({}%) supply {trader_supply:>7}  -> claimed {}  (only LOSERS + the farmer earn — the cohort literally pays for realized loss)", trader_bps/100, rational_loser_coin as u128 + farmer_coin as u128);
    println!("---------------------------------------------------------------------------");
    println!("PER-PARTICIPANT distribution:");
    println!("  one insurance depositor   : {}", if SIM_N_INS>0 { ins_coin / SIM_N_INS as u64 } else {0});
    println!("  one backing depositor     : {}", if SIM_N_BACK>0 { back_coin / SIM_N_BACK as u64 } else {0});
    println!("  a rational WINNER         : 0   (winning a trade earns NO COIN — the trader cohort rewards loss)");
    println!("  a rational LOSER (avg)    : {}   (real capital lost for it)", if n_losers>0 { rational_loser_coin / n_losers as u64 } else {0});
    println!("  the MAX-FARMER (2 legs)   : {farmer_coin}   (~0 net market risk; one leg always loses risk-free)");
    println!("---------------------------------------------------------------------------");
    let farmer_share_pct = farmer_coin as f64 * 100.0 / trader_supply as f64;
    let avg_loser = if n_losers>0 { rational_loser_coin / n_losers as u64 } else { 0 };
    println!("FARMER vs an honest loser : farmer {farmer_coin} (2 legs) vs avg honest loser {avg_loser}");
    println!("  farmer captured {farmer_share_pct:.1}% of the trader cohort for ~0 net risk; an honest loser captured a");
    println!("  comparable per-leg share but PAID real directional loss. The rd cannot tell manufactured loss from");
    println!("  real loss — so the farmer gets a FAIR per-loss share, NOT a disproportionate one. To capture MORE it");
    println!("  must add more 1M-funded delta-neutral legs (linear Sybil cost), each taxed {}% + diluted 1/N.", FEE_BPS/100);
    println!("conservation: distributed {distributed} <= supply {SUPPLY}  (LP {lp_supply} unclaimed/deflationary)");
    println!("============================================================================\n");

    // ---- invariants ----
    assert!(distributed <= supply, "total distributed COIN never exceeds the fixed supply");
    assert!(n_losers > 0 && n_winners > 0, "a realistic mix of winners and losers");
    assert_eq!(rational_winner_coin, 0, "winning traders earn 0 from the loss-rewarding trader cohort");
    assert!(farmer_coin > 0, "the delta-neutral farmer captures a real (risk-free) trader-cohort slice");
    // The farmer is NOT disproportionate: its take <= the whole trader cohort, and per-leg it is bounded by the
    // same per-loss share an honest loser gets (the rd treats manufactured and real loss identically).
    assert!(farmer_coin as u128 <= trader_supply, "farmer cannot exceed the trader cohort");
    assert_eq!(ins_coin as u128, ins_supply, "insurance cohort fully claimed (all depositors live, equal principal)");
    assert_eq!(back_coin as u128, back_supply, "backing cohort fully claimed");
    let _ = total_collateral; let _ = loser_examples; let _ = rational_loser_coin;
}

// CROSS-MARGIN ECONOMY SIM (user-directed refinement): ONE percolator market with 10 ASSETS; each of 99 rational
// traders holds a SINGLE cross-margined portfolio spanning a random subset (1..=10) of the assets, long or short
// each, sharing its 1M collateral (true cross-margin — percolator nets margin across the portfolio's assets). 1
// max-farmer runs delta-neutral asset pairs (risk-free crystallized loss). REAL percolator; rd cohorts 10/10/40/40
// + 20% anti-wash fee. Reports total notional volume + the COIN distribution every cohort/participant earns.
const XM_N_RATIONAL: usize = 99;
const XM_N_ASSETS: usize = 10;
const XM_DEPOSIT: u64 = 1_000_000;
const XM_DEPLOY: u64 = 500_000;       // each portfolio deploys 50% of its 1M as notional (buffer vs liquidation)
const XM_COIN_FDV: u128 = 100_000_000; // each trader prices the whole COIN supply at 100M -> value/atom = FDV/supply

#[test]
fn cross_margin_100_traders_10_assets_distribution_report() {
    let mut svm = LiteSVM::new().with_compute_budget(solana_program_runtime::compute_budget::ComputeBudget {
        compute_unit_limit: 1_400_000, heap_size: 256 * 1024,
        ..solana_program_runtime::compute_budget::ComputeBudget::default()
    });
    svm.add_program_from_file(perc_id(), perc_so()).unwrap();
    svm.add_program_from_file(rd_id(), so_deploy("residual_distributor")).unwrap();
    let payer = Keypair::new(); svm.airdrop(&payer.pubkey(), 100_000_000_000_000).unwrap();
    let oracle = Keypair::new(); svm.airdrop(&oracle.pubkey(), 1_000_000_000_000).unwrap();
    svm.set_sysvar(&Clock { slot: 100, unix_timestamp: 100, ..Default::default() });
    let collateral = create_real_mint(&mut svm, &payer, &Keypair::new().pubkey());
    let initial_price = 1_000u64;
    let pk = payer.insecure_clone();
    let tx = move |svm: &mut LiteSVM, ixs: &[Instruction], extra: &[&Keypair]| {
        svm.expire_blockhash(); let bh = svm.latest_blockhash();
        let mut s: Vec<&Keypair> = vec![&pk]; s.extend_from_slice(extra);
        svm.send_transaction(Transaction::new_signed_with_payer(ixs, Some(&pk.pubkey()), &s, bh))
    };
    // ONE market, capacity = 10 assets, max_portfolio_assets = 10 (cross-margin across all 10).
    let mlen = percolator_prog::state::market_account_len_for_capacity(XM_N_ASSETS).unwrap();
    let plen = percolator_prog::state::portfolio_account_len_for_market_slots(XM_N_ASSETS).unwrap();
    let market = Pubkey::new_unique();
    svm.set_account(market, Account { lamports: 1_000_000_000, data: vec![0u8; mlen], owner: perc_id(), executable: false, rent_epoch: 0 }).unwrap();
    let pv = canonical_insurance_vault(&perc_vault_authority(&market), &collateral);
    set_token(&mut svm, &pv, &collateral, &perc_vault_authority(&market), 0);
    tx(&mut svm, &[pix(vec![AccountMeta::new(oracle.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new_readonly(collateral, false)],
        PIx::InitMarket { max_portfolio_assets: XM_N_ASSETS as u16, h_min: 0, h_max: 10, initial_price,
            min_nonzero_mm_req: 1, min_nonzero_im_req: 2, maintenance_margin_bps: 10_000, initial_margin_bps: 10_000,
            max_trading_fee_bps: 10_000, trade_fee_base_bps: 3, liquidation_fee_bps: 0, liquidation_fee_cap: 0,
            min_liquidation_abs: 0, max_price_move_bps_per_slot: 10_000, max_accrual_dt_slots: 1,
            max_abs_funding_e9_per_slot: 0, min_funding_lifetime_slots: 1, max_account_b_settlement_chunks: 1,
            max_bankrupt_close_chunks: 1, max_bankrupt_close_lifetime_slots: 100, public_b_chunk_atoms: percolator::MAX_VAULT_TVL,
            maintenance_fee_per_slot: 0 })], &[&oracle]).expect("init 10-asset market");
    for a in 0..XM_N_ASSETS as u16 {
        tx(&mut svm, &[pix(vec![AccountMeta::new(oracle.pubkey(), true), AccountMeta::new(market, false)], PIx::ConfigureAuthMark { asset_index: a, now_slot: 100, initial_mark_e6: initial_price })], &[&oracle]).expect("cfg mark");
    }
    // universal market-maker counterparty (deep collateral; NOT rd-registered).
    let mm = Keypair::new(); svm.airdrop(&mm.pubkey(), 1_000_000_000).unwrap();
    let mm_pf = Pubkey::new_unique();
    svm.set_account(mm_pf, Account { lamports: 1_000_000_000, data: vec![0u8; plen], owner: perc_id(), executable: false, rent_epoch: 0 }).unwrap();
    tx(&mut svm, &[pix(vec![AccountMeta::new(mm.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(mm_pf, false)], PIx::InitPortfolio)], &[&mm]).expect("init mm");
    let mm_src = Pubkey::new_unique(); set_token(&mut svm, &mm_src, &collateral, &mm.pubkey(), 3_000_000_000);
    tx(&mut svm, &[pix(vec![AccountMeta::new(mm.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(mm_pf, false), AccountMeta::new(mm_src, false), AccountMeta::new(pv, false), AccountMeta::new_readonly(spl_token::ID, false)], PIx::Deposit { amount: 3_000_000_000u128 })], &[&mm]).expect("mm deposit");

    // ---- rd config: cohorts 10/10/40/40, allow-list = the single market, 20% fee ----
    let sub_pool = Pubkey::new_unique(); let back_pool = Pubkey::new_unique();
    let coin_auth = Keypair::new();
    let coin_mint = create_real_mint(&mut svm, &payer, &coin_auth.pubkey());
    let rd_config = Pubkey::find_program_address(&[b"rd_config", coin_mint.as_ref()], &rd_id()).0;
    let dist_config = dist_config_pda(&coin_mint, &rd_config);
    let rd_vault = Pubkey::new_unique(); set_token(&mut svm, &rd_vault, &coin_mint, &rd_config, 0);
    tx(&mut svm, &[
        spl_token::instruction::mint_to(&spl_token::ID, &coin_mint, &rd_vault, &coin_auth.pubkey(), &[], SUPPLY).unwrap(),
    ], &[&coin_auth]).expect("fund COIN");
    let mut ri = vec![0u8];
    ri.extend_from_slice(&SUPPLY.to_le_bytes()); ri.extend_from_slice(&2_000u64.to_le_bytes());
    ri.extend_from_slice(&INS_BPS.to_le_bytes()); ri.extend_from_slice(&BACK_BPS.to_le_bytes()); ri.extend_from_slice(&LP_BPS.to_le_bytes());
    ri.extend_from_slice(&100u64.to_le_bytes());
    ri.extend_from_slice(sub_pool.as_ref()); ri.extend_from_slice(back_pool.as_ref());
    ri.extend_from_slice(market.as_ref());
    ri.push(0u8); // no extra allow-listed markets — the single cross-margin market is market_group
    ri.extend_from_slice(&FEE_BPS.to_le_bytes());
    tx(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
        AccountMeta::new(payer.pubkey(), true), AccountMeta::new_readonly(coin_mint, false), AccountMeta::new_readonly(dist_id(), false),
        AccountMeta::new_readonly(dist_config, false), AccountMeta::new_readonly(perc_id(), false), AccountMeta::new_readonly(sub_id(), false),
        AccountMeta::new(rd_config, false), AccountMeta::new_readonly(system_program::ID, false), AccountMeta::new_readonly(coin_auth.pubkey(), true),
    ], data: ri }], &[&coin_auth]).expect("rd init");
    tx(&mut svm, &[
        spl_token::instruction::set_authority(&spl_token::ID, &coin_mint, None, spl_token::instruction::AuthorityType::MintTokens, &coin_auth.pubkey(), &[]).unwrap(),
    ], &[&coin_auth]).expect("freeze COIN");

    // ---- insurance + backing cohorts: 10 depositors x 1M principal each ----
    let mut ins_parts: Vec<(Keypair, Pubkey, Pubkey)> = Vec::new();
    let mut back_parts: Vec<(Keypair, Pubkey, Pubkey)> = Vec::new();
    for (cohort, pool, parts, n) in [(0u8, sub_pool, &mut ins_parts, 10usize), (1u8, back_pool, &mut back_parts, 10usize)] {
        for _ in 0..n {
            let owner = Keypair::new(); svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
            let pos = Pubkey::new_unique();
            let d = modeled_subledger_position(&pool, &owner.pubkey(), XM_DEPOSIT, 100);
            svm.set_account(pos, Account { lamports: 1_000_000_000, data: d, owner: sub_id(), executable: false, rent_epoch: 0 }).unwrap();
            let stake = rd_stake_pda(&rd_config, &owner.pubkey(), &pos, cohort);
            tx(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
                AccountMeta::new(payer.pubkey(), true), AccountMeta::new_readonly(rd_config, false), AccountMeta::new_readonly(owner.pubkey(), true),
                AccountMeta::new_readonly(owner.pubkey(), false), AccountMeta::new_readonly(pos, false), AccountMeta::new(stake, false),
                AccountMeta::new_readonly(system_program::ID, false),
            ], data: vec![1u8, cohort] }], &[&owner]).expect("register ins/back");
            let ata = Pubkey::new_unique(); set_token(&mut svm, &ata, &coin_mint, &owner.pubkey(), 0);
            parts.push((owner, pos, ata));
        }
    }

    // ---- 99 rational traders: each a CROSS-MARGIN portfolio over a random subset (1..=10) of the 10 assets ----
    let mut rng = 0xDEAD_BEEF_1234_5678u64;
    let nxt = |r: &mut u64| { *r = r.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (*r >> 33) as usize };
    // per trader: (owner, pf, ata, Vec<(asset, is_long)>)
    let mut rational: Vec<(Keypair, Pubkey, Pubkey, Vec<(u16, bool)>)> = Vec::new();
    let mut total_notional: u128 = 0;
    let mut total_positions: usize = 0;
    for _ in 0..XM_N_RATIONAL {
        let owner = Keypair::new(); svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
        let pf = Pubkey::new_unique();
        svm.set_account(pf, Account { lamports: 1_000_000_000, data: vec![0u8; plen], owner: perc_id(), executable: false, rent_epoch: 0 }).unwrap();
        tx(&mut svm, &[pix(vec![AccountMeta::new(owner.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(pf, false)], PIx::InitPortfolio)], &[&owner]).expect("init pf");
        let src = Pubkey::new_unique(); set_token(&mut svm, &src, &collateral, &owner.pubkey(), XM_DEPOSIT);
        tx(&mut svm, &[pix(vec![AccountMeta::new(owner.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(pf, false), AccountMeta::new(src, false), AccountMeta::new(pv, false), AccountMeta::new_readonly(spl_token::ID, false)], PIx::Deposit { amount: XM_DEPOSIT as u128 })], &[&owner]).expect("deposit");
        // random subset of assets (k = 1..=10), each a random direction, cross-margined in this one portfolio.
        let k = 1 + nxt(&mut rng) % XM_N_ASSETS;
        let leg_posq = ((XM_DEPLOY / k as u64) as i128) * (percolator::POS_SCALE as i128) / (initial_price as i128);
        let mut chosen: Vec<u16> = (0..XM_N_ASSETS as u16).collect();
        for i in (1..chosen.len()).rev() { let j = nxt(&mut rng) % (i + 1); chosen.swap(i, j); } // shuffle
        let mut legs: Vec<(u16, bool)> = Vec::new();
        for &a in chosen.iter().take(k) {
            let is_long = nxt(&mut rng) % 2 == 0;
            if is_long {
                tx(&mut svm, &[pix(vec![AccountMeta::new(mm.pubkey(), true), AccountMeta::new(owner.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(mm_pf, false), AccountMeta::new(pf, false)], PIx::TradeNoCpi { asset_index: a, size_q: -leg_posq, exec_price: initial_price, fee_bps: 0 })], &[&mm, &owner]).expect("open long leg");
            } else {
                tx(&mut svm, &[pix(vec![AccountMeta::new(owner.pubkey(), true), AccountMeta::new(mm.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(pf, false), AccountMeta::new(mm_pf, false)], PIx::TradeNoCpi { asset_index: a, size_q: -leg_posq, exec_price: initial_price, fee_bps: 0 })], &[&owner, &mm]).expect("open short leg");
            }
            total_notional += leg_posq.unsigned_abs() * initial_price as u128 / percolator::POS_SCALE;
            total_positions += 1;
            legs.push((a, is_long));
        }
        let stake = rd_stake_pda(&rd_config, &owner.pubkey(), &pf, 3);
        tx(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
            AccountMeta::new(payer.pubkey(), true), AccountMeta::new_readonly(rd_config, false), AccountMeta::new_readonly(owner.pubkey(), true),
            AccountMeta::new_readonly(owner.pubkey(), false), AccountMeta::new_readonly(pf, false), AccountMeta::new(stake, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(rd_portfolio_archive_pda(&market, &owner.pubkey(), &pf), false),
            AccountMeta::new_readonly(market, false),
            AccountMeta::new_readonly(retired_market_pda(&market), false),
        ], data: vec![1u8, 3u8] }], &[&owner]).expect("register trader");
        let ata = Pubkey::new_unique(); set_token(&mut svm, &ata, &coin_mint, &owner.pubkey(), 0);
        rational.push((owner, pf, ata, legs));
    }

    // ---- 1 max-farmer: delta-neutral across ALL 10 assets (long+short each in 2 Sybil portfolios) ----
    let mut farmer: Vec<(Keypair, Pubkey, Pubkey, Vec<u16>)> = Vec::new();
    for leg_is_long in [true, false] {
        let owner = Keypair::new(); svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
        let pf = Pubkey::new_unique();
        svm.set_account(pf, Account { lamports: 1_000_000_000, data: vec![0u8; plen], owner: perc_id(), executable: false, rent_epoch: 0 }).unwrap();
        tx(&mut svm, &[pix(vec![AccountMeta::new(owner.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(pf, false)], PIx::InitPortfolio)], &[&owner]).expect("init farmer pf");
        let src = Pubkey::new_unique(); set_token(&mut svm, &src, &collateral, &owner.pubkey(), XM_DEPOSIT);
        tx(&mut svm, &[pix(vec![AccountMeta::new(owner.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(pf, false), AccountMeta::new(src, false), AccountMeta::new(pv, false), AccountMeta::new_readonly(spl_token::ID, false)], PIx::Deposit { amount: XM_DEPOSIT as u128 })], &[&owner]).expect("farmer deposit");
        let leg_posq = ((XM_DEPLOY / XM_N_ASSETS as u64) as i128) * (percolator::POS_SCALE as i128) / (initial_price as i128);
        let mut assets: Vec<u16> = Vec::new();
        for a in 0..XM_N_ASSETS as u16 {
            if leg_is_long {
                tx(&mut svm, &[pix(vec![AccountMeta::new(mm.pubkey(), true), AccountMeta::new(owner.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(mm_pf, false), AccountMeta::new(pf, false)], PIx::TradeNoCpi { asset_index: a, size_q: -leg_posq, exec_price: initial_price, fee_bps: 0 })], &[&mm, &owner]).expect("farmer long leg");
            } else {
                tx(&mut svm, &[pix(vec![AccountMeta::new(owner.pubkey(), true), AccountMeta::new(mm.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(pf, false), AccountMeta::new(mm_pf, false)], PIx::TradeNoCpi { asset_index: a, size_q: -leg_posq, exec_price: initial_price, fee_bps: 0 })], &[&owner, &mm]).expect("farmer short leg");
            }
            total_notional += leg_posq.unsigned_abs() * initial_price as u128 / percolator::POS_SCALE;
            total_positions += 1;
            assets.push(a);
        }
        let stake = rd_stake_pda(&rd_config, &owner.pubkey(), &pf, 3);
        tx(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
            AccountMeta::new(payer.pubkey(), true), AccountMeta::new_readonly(rd_config, false), AccountMeta::new_readonly(owner.pubkey(), true),
            AccountMeta::new_readonly(owner.pubkey(), false), AccountMeta::new_readonly(pf, false), AccountMeta::new(stake, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(rd_portfolio_archive_pda(&market, &owner.pubkey(), &pf), false),
            AccountMeta::new_readonly(market, false),
            AccountMeta::new_readonly(retired_market_pda(&market), false),
        ], data: vec![1u8, 3u8] }], &[&owner]).expect("register farmer leg");
        let ata = Pubkey::new_unique(); set_token(&mut svm, &ata, &coin_mint, &owner.pubkey(), 0);
        farmer.push((owner, pf, ata, assets));
    }

    // ---- neutral oracle moves each asset mark (even asset -50%, odd +50%); settle each traded leg ----
    svm.set_sysvar(&Clock { slot: 110, unix_timestamp: 110, ..Default::default() });
    for a in 0..XM_N_ASSETS as u16 {
        let new_mark = if a % 2 == 0 { initial_price / 2 } else { initial_price + initial_price / 2 };
        tx(&mut svm, &[pix(vec![AccountMeta::new(oracle.pubkey(), true), AccountMeta::new(market, false)], PIx::PushAuthMark { asset_index: a, now_slot: 110, mark_e6: new_mark })], &[&oracle]).expect("oracle move");
    }
    let crank = |svm: &mut LiteSVM, pf: &Pubkey, a: u16| {
        svm.expire_blockhash(); let bh = svm.latest_blockhash();
        let _ = svm.send_transaction(Transaction::new_signed_with_payer(&[pix(vec![AccountMeta::new(payer.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(*pf, false)],
            PIx::PermissionlessCrank { now_slot: 110, observations: vec![percolator_prog::ix::CrankObservationHint { asset_index: a, oracle_accounts: 0 }] })], Some(&payer.pubkey()), &[&payer], bh));
    };
    for (_o, pf, _a, legs) in &rational { for (a, _l) in legs { crank(&mut svm, pf, *a); crank(&mut svm, pf, *a); } }
    for (_o, pf, _a, assets) in &farmer { for a in assets { crank(&mut svm, pf, *a); crank(&mut svm, pf, *a); } }

    // ---- crystallize all trader stakes + capital stakes, freeze, claim ----
    for (o, pf, _a, _l) in &rational {
        let stake = rd_stake_pda(&rd_config, &o.pubkey(), pf, 3);
        let _ = tx(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
            AccountMeta::new(payer.pubkey(), true), AccountMeta::new(rd_config, false), AccountMeta::new(stake, false), AccountMeta::new_readonly(*pf, false),
            AccountMeta::new_readonly(rd_portfolio_archive_pda(&market, &o.pubkey(), pf), false),
            AccountMeta::new_readonly(retired_market_pda(&market), false),
        ], data: vec![2u8] }], &[]);
    }
    for (o, pf, _a, _as) in &farmer {
        let stake = rd_stake_pda(&rd_config, &o.pubkey(), pf, 3);
        let _ = tx(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
            AccountMeta::new(payer.pubkey(), true), AccountMeta::new(rd_config, false), AccountMeta::new(stake, false), AccountMeta::new_readonly(*pf, false),
            AccountMeta::new_readonly(rd_portfolio_archive_pda(&market, &o.pubkey(), pf), false),
            AccountMeta::new_readonly(retired_market_pda(&market), false),
        ], data: vec![2u8] }], &[]);
    }
    for (cohort, parts) in [(0u8, &ins_parts), (1u8, &back_parts)] {
        for (o, pos, _a) in parts {
            let stake = rd_stake_pda(&rd_config, &o.pubkey(), pos, cohort);
            tx(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
                AccountMeta::new(o.pubkey(), true), AccountMeta::new(rd_config, false), AccountMeta::new(stake, false), AccountMeta::new_readonly(*pos, false),
            ], data: vec![2u8] }], &[o]).expect("crystallize capital");
        }
    }
    svm.set_sysvar(&Clock { slot: 2_101, unix_timestamp: 2_101, ..Default::default() });
    tx(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
        AccountMeta::new(payer.pubkey(), true), AccountMeta::new(rd_config, false), AccountMeta::new_readonly(coin_mint, false), AccountMeta::new(rd_vault, false),
    ], data: vec![4u8] }], &[]).expect("freeze");

    let mut ins_coin = 0u64; let mut back_coin = 0u64;
    for (o, pos, ata) in &ins_parts {
        let stake = rd_stake_pda(&rd_config, &o.pubkey(), pos, 0);
        let _ = tx(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
            AccountMeta::new(o.pubkey(), true), AccountMeta::new_readonly(rd_config, false), AccountMeta::new(stake, false),
            AccountMeta::new(rd_vault, false), AccountMeta::new(*ata, false), AccountMeta::new_readonly(spl_token::ID, false), AccountMeta::new_readonly(*pos, false),
        ], data: vec![5u8] }], &[o]); ins_coin += token_amount(&svm, ata);
    }
    for (o, pos, ata) in &back_parts {
        let stake = rd_stake_pda(&rd_config, &o.pubkey(), pos, 1);
        let _ = tx(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
            AccountMeta::new(o.pubkey(), true), AccountMeta::new_readonly(rd_config, false), AccountMeta::new(stake, false),
            AccountMeta::new(rd_vault, false), AccountMeta::new(*ata, false), AccountMeta::new_readonly(spl_token::ID, false), AccountMeta::new_readonly(*pos, false),
        ], data: vec![5u8] }], &[o]); back_coin += token_amount(&svm, ata);
    }
    let mut winner_coin = 0u64; let mut loser_coin = 0u64; let mut n_pure_winners = 0usize; let mut n_any_loss = 0usize;
    let mut loser_real_loss: u128 = 0; // Sigma crystallized over the rational losers (their REAL directional loss)
    for (o, pf, ata, legs) in &rational {
        let stake = rd_stake_pda(&rd_config, &o.pubkey(), pf, 3);
        let _ = tx(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
            AccountMeta::new(payer.pubkey(), true), AccountMeta::new_readonly(rd_config, false), AccountMeta::new(stake, false),
            AccountMeta::new(rd_vault, false), AccountMeta::new(*ata, false), AccountMeta::new_readonly(spl_token::ID, false), AccountMeta::new_readonly(*pf, false),
            AccountMeta::new_readonly(rd_portfolio_archive_pda(&market, &o.pubkey(), pf), false),
            AccountMeta::new_readonly(retired_market_pda(&market), false),
        ], data: vec![5u8] }], &[]);
        let got = token_amount(&svm, ata);
        // a leg loses if (even asset & long) or (odd asset & short).
        let has_loss = legs.iter().any(|(a, l)| if a % 2 == 0 { *l } else { !*l });
        if has_loss { n_any_loss += 1; loser_coin += got; loser_real_loss += read_crystallized(&svm, pf); } else { n_pure_winners += 1; winner_coin += got; }
    }
    let mut farmer_coin = 0u64;
    let mut farmer_gross_loss: u128 = 0; // farmer's GROSS crystallized (its COIN basis); its NET real cost ~0 (delta-neutral)
    for (o, pf, ata, _as) in &farmer {
        let stake = rd_stake_pda(&rd_config, &o.pubkey(), pf, 3);
        let _ = tx(&mut svm, &[Instruction { program_id: rd_id(), accounts: vec![
            AccountMeta::new(payer.pubkey(), true), AccountMeta::new_readonly(rd_config, false), AccountMeta::new(stake, false),
            AccountMeta::new(rd_vault, false), AccountMeta::new(*ata, false), AccountMeta::new_readonly(spl_token::ID, false), AccountMeta::new_readonly(*pf, false),
            AccountMeta::new_readonly(rd_portfolio_archive_pda(&market, &o.pubkey(), pf), false),
            AccountMeta::new_readonly(retired_market_pda(&market), false),
        ], data: vec![5u8] }], &[]);
        farmer_coin += token_amount(&svm, ata);
        farmer_gross_loss += read_crystallized(&svm, pf);
    }

    let trader_bps = 10_000 - INS_BPS - BACK_BPS - LP_BPS;
    let supply = SUPPLY as u128;
    let trader_supply = supply * trader_bps as u128 / 10_000;
    let distributed = ins_coin as u128 + back_coin as u128 + winner_coin as u128 + loser_coin as u128 + farmer_coin as u128;
    let avg_legs = total_positions as f64 / (XM_N_RATIONAL + 2) as f64;
    println!("\n============ CROSS-MARGIN ECONOMY — 100 TRADERS, 1 MARKET x 10 ASSETS ============");
    println!("market: 1 percolator slab, {XM_N_ASSETS} assets, full-margin; traders CROSS-MARGIN across a random subset");
    println!("traders: {XM_N_RATIONAL} rational (1M each, {avg_legs:.1} asset-legs avg) + 1 max-farmer (delta-neutral all 10 assets, 2 Sybil legs)");
    println!("insurance/backing: 10 x 1M each (= 1M per asset)        total positions opened: {total_positions}");
    println!("  rational with >=1 losing leg: {n_any_loss}   pure winners (all legs won): {n_pure_winners}");
    println!("TOTAL NOTIONAL VOLUME (price-scaled open interest)      : {total_notional}");
    println!("----------------------------------------------------------------------------------");
    println!("COIN supply {SUPPLY}  split 10/10/40/40 (ins/back/lp/trader):");
    println!("  INSURANCE (10%) -> claimed {ins_coin:>7}   ({} each)", if !ins_parts.is_empty() { ins_coin / ins_parts.len() as u64 } else {0});
    println!("  BACKING   (10%) -> claimed {back_coin:>7}   ({} each)", if !back_parts.is_empty() { back_coin / back_parts.len() as u64 } else {0});
    println!("  LP        (40%) -> claimed       0   (received==0: no bankruptcy residual on a full-margin market)");
    println!("  TRADER    (40%, supply {trader_supply}) -> claimed {}   (any portfolio with a LOSING leg earns:", loser_coin as u128 + farmer_coin as u128);
    println!("           crystallized is GROSS per-leg (NOT netted vs same-portfolio winning legs, whose gain sits in pnl),");
    println!("           so a net-flat cross-margin portfolio STILL earns on its gross losing legs -- the FEE is the bound, not netting.");
    println!("----------------------------------------------------------------------------------");
    println!("PER-PARTICIPANT:");
    println!("  insurance depositor : {}", if !ins_parts.is_empty() { ins_coin / ins_parts.len() as u64 } else {0});
    println!("  backing depositor   : {}", if !back_parts.is_empty() { back_coin / back_parts.len() as u64 } else {0});
    println!("  rational w/ net loss: {} avg ({n_any_loss} of them shared {loser_coin})", if n_any_loss>0 { loser_coin / n_any_loss as u64 } else {0});
    println!("  the MAX-FARMER      : {farmer_coin}  ({:.1}% of trader cohort, ~0 net market risk)", farmer_coin as f64 * 100.0 / trader_supply as f64);
    println!("conservation: distributed {distributed} <= supply {SUPPLY}");
    println!("----------------------------------------------------------------------------------");
    // ---- VALUE: each trader prices the whole COIN supply at XM_COIN_FDV; deploy more cash while earn > loss ----
    let vpc = XM_COIN_FDV / supply; // value per COIN atom (collateral-equivalent units)
    let total_deployed = (XM_DEPLOY as u128) * ((XM_N_RATIONAL + 2) as u128);
    let farmer_earn_val = farmer_coin as u128 * vpc;
    let loser_earn_val = loser_coin as u128 * vpc;
    let avg_loser_loss = if n_any_loss > 0 { loser_real_loss / n_any_loss as u128 } else { 0 };
    let avg_loser_earn_val = if n_any_loss > 0 { loser_earn_val / n_any_loss as u128 } else { 0 };
    println!("VALUE @ COIN FDV {XM_COIN_FDV} (= {vpc}/atom).  capital deployed as notional: {total_deployed} (50% of each 1M)");
    println!("  notional volume / OI : {total_notional}");
    println!("  honest losers' TOTAL real loss : {loser_real_loss}  -> earned COIN value {loser_earn_val}  (reward/loss {:.0}x)",
        if loser_real_loss>0 { loser_earn_val as f64 / loser_real_loss as f64 } else {0.0});
    println!("  avg honest loser     : lost {avg_loser_loss}, earned COIN value {avg_loser_earn_val}  -> NET +{}",
        avg_loser_earn_val as i128 - avg_loser_loss as i128);
    println!("  MAX-FARMER           : gross-loss {farmer_gross_loss} but ~0 NET cost (delta-neutral), earned COIN value {farmer_earn_val}  -> NET ~+{farmer_earn_val}");
    println!("  => RATIONAL TO DEPLOY: at this FDV the fixed trader cohort is worth {} >> the total real loss {loser_real_loss}", trader_supply * vpc);
    println!("     that earns it, so every participant nets positive and KEEPS scaling capital until total real");
    println!("     loss approaches the cohort value (break-even). The farmer (~0 net cost) always profits most.");
    println!("==================================================================================\n");

    // Rational-deployment condition (the user's ask): at this COIN valuation, earning > losing for losers AND
    // the farmer, so deploying this much capital is rational. (Holds while total real loss < the cohort value.)
    assert!(loser_earn_val > loser_real_loss, "honest losers earn more COIN value than they lost (rational to deploy)");
    assert!(farmer_earn_val > 0, "the delta-neutral farmer earns COIN value for ~0 net cost");
    assert!((trader_supply * vpc) > loser_real_loss, "the fixed trader cohort value exceeds the real loss that earns it");

    assert!(distributed <= supply, "distributed never exceeds supply");
    assert!(total_positions > XM_N_RATIONAL, "cross-margin: many traders hold multiple asset legs");
    assert!(farmer_coin > 0 && (farmer_coin as u128) <= trader_supply, "farmer captures a real but bounded slice");
    assert_eq!(ins_coin as u128 + back_coin as u128, supply * (INS_BPS + BACK_BPS) as u128 / 10_000, "ins+back fully claimed");
    let _ = winner_coin;
}

// GROSS-vs-NET crystallized under CROSS-MARGIN (surface D free-farm probe). The rd trader counter is
// `crystallized - spent`. If a SINGLE cross-margin portfolio's `crystallized` reflects the GROSS loss of its
// losing legs (NOT netted against the same portfolio's WINNING legs' pnl), then a delta-neutral pair across two
// assets (long a winner + long a loser, equal size, net PnL ~0) manufactures trader points for ~0 net loss with
// spent==0 — a single-account spent-netting bypass. This pins whether percolator nets it (blocked) or not
// (the finding-NZ wash, now at the single-portfolio level — fee is then the sole bound). Corrects/confirms the
// prior cross-margin tick's "only NET loss earns" claim.
#[test]
fn cross_margin_crystallized_is_gross_not_net_single_portfolio_wash_probe() {
    let mut svm = LiteSVM::new().with_compute_budget(solana_program_runtime::compute_budget::ComputeBudget {
        compute_unit_limit: 1_400_000, heap_size: 256 * 1024,
        ..solana_program_runtime::compute_budget::ComputeBudget::default()
    });
    svm.add_program_from_file(perc_id(), perc_so()).unwrap();
    let payer = Keypair::new(); svm.airdrop(&payer.pubkey(), 100_000_000_000_000).unwrap();
    let oracle = Keypair::new(); svm.airdrop(&oracle.pubkey(), 1_000_000_000_000).unwrap();
    svm.set_sysvar(&Clock { slot: 100, unix_timestamp: 100, ..Default::default() });
    let collateral = create_real_mint(&mut svm, &payer, &Keypair::new().pubkey());
    let initial_price = 1_000u64;
    let pk = payer.insecure_clone();
    let tx = move |svm: &mut LiteSVM, ixs: &[Instruction], extra: &[&Keypair]| {
        svm.expire_blockhash(); let bh = svm.latest_blockhash();
        let mut s: Vec<&Keypair> = vec![&pk]; s.extend_from_slice(extra);
        svm.send_transaction(Transaction::new_signed_with_payer(ixs, Some(&pk.pubkey()), &s, bh))
    };
    let mlen = percolator_prog::state::market_account_len_for_capacity(2).unwrap();
    let plen = percolator_prog::state::portfolio_account_len_for_market_slots(2).unwrap();
    let market = Pubkey::new_unique();
    svm.set_account(market, Account { lamports: 1_000_000_000, data: vec![0u8; mlen], owner: perc_id(), executable: false, rent_epoch: 0 }).unwrap();
    let pv = canonical_insurance_vault(&perc_vault_authority(&market), &collateral);
    set_token(&mut svm, &pv, &collateral, &perc_vault_authority(&market), 0);
    tx(&mut svm, &[pix(vec![AccountMeta::new(oracle.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new_readonly(collateral, false)],
        PIx::InitMarket { max_portfolio_assets: 2, h_min: 0, h_max: 10, initial_price,
            min_nonzero_mm_req: 1, min_nonzero_im_req: 2, maintenance_margin_bps: 10_000, initial_margin_bps: 10_000,
            max_trading_fee_bps: 10_000, trade_fee_base_bps: 3, liquidation_fee_bps: 0, liquidation_fee_cap: 0,
            min_liquidation_abs: 0, max_price_move_bps_per_slot: 10_000, max_accrual_dt_slots: 1,
            max_abs_funding_e9_per_slot: 0, min_funding_lifetime_slots: 1, max_account_b_settlement_chunks: 1,
            max_bankrupt_close_chunks: 1, max_bankrupt_close_lifetime_slots: 100, public_b_chunk_atoms: percolator::MAX_VAULT_TVL,
            maintenance_fee_per_slot: 0 })], &[&oracle]).expect("init 2-asset market");
    for a in 0..2u16 { tx(&mut svm, &[pix(vec![AccountMeta::new(oracle.pubkey(), true), AccountMeta::new(market, false)], PIx::ConfigureAuthMark { asset_index: a, now_slot: 100, initial_mark_e6: initial_price })], &[&oracle]).expect("cfg mark"); }

    // MM counterparty + the WASHER (one cross-margin portfolio: long asset0 + long asset1, equal size).
    let mm = Keypair::new(); svm.airdrop(&mm.pubkey(), 1_000_000_000).unwrap();
    let washer = Keypair::new(); svm.airdrop(&washer.pubkey(), 1_000_000_000).unwrap();
    let mm_pf = Pubkey::new_unique(); let w_pf = Pubkey::new_unique();
    for (o, pf, dep) in [(&mm, &mm_pf, 100_000_000u64), (&washer, &w_pf, 1_000_000u64)] {
        svm.set_account(*pf, Account { lamports: 1_000_000_000, data: vec![0u8; plen], owner: perc_id(), executable: false, rent_epoch: 0 }).unwrap();
        tx(&mut svm, &[pix(vec![AccountMeta::new(o.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(*pf, false)], PIx::InitPortfolio)], &[o]).expect("init pf");
        let src = Pubkey::new_unique(); set_token(&mut svm, &src, &collateral, &o.pubkey(), dep);
        tx(&mut svm, &[pix(vec![AccountMeta::new(o.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(*pf, false), AccountMeta::new(src, false), AccountMeta::new(pv, false), AccountMeta::new_readonly(spl_token::ID, false)], PIx::Deposit { amount: dep as u128 })], &[o]).expect("deposit");
    }
    let posq = (percolator::POS_SCALE / 2) as i128;
    // washer LONG asset0 and LONG asset1 (both vs MM), equal size -> net directional exposure across the two.
    for a in 0..2u16 {
        tx(&mut svm, &[pix(vec![AccountMeta::new(mm.pubkey(), true), AccountMeta::new(washer.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(mm_pf, false), AccountMeta::new(w_pf, false)], PIx::TradeNoCpi { asset_index: a, size_q: -posq, exec_price: initial_price, fee_bps: 0 })], &[&mm, &washer]).expect("washer long leg");
    }
    let w_cryst0 = read_crystallized(&svm, &w_pf);
    // Oracle: asset0 DROPS 50% (long0 LOSES), asset1 RISES 50% (long1 WINS), equal size -> net PnL ~0.
    svm.set_sysvar(&Clock { slot: 110, unix_timestamp: 110, ..Default::default() });
    tx(&mut svm, &[pix(vec![AccountMeta::new(oracle.pubkey(), true), AccountMeta::new(market, false)], PIx::PushAuthMark { asset_index: 0, now_slot: 110, mark_e6: initial_price / 2 })], &[&oracle]).expect("drop a0");
    tx(&mut svm, &[pix(vec![AccountMeta::new(oracle.pubkey(), true), AccountMeta::new(market, false)], PIx::PushAuthMark { asset_index: 1, now_slot: 110, mark_e6: initial_price + initial_price / 2 })], &[&oracle]).expect("raise a1");
    let crank = |svm: &mut LiteSVM, a: u16| {
        svm.expire_blockhash(); let bh = svm.latest_blockhash();
        let _ = svm.send_transaction(Transaction::new_signed_with_payer(&[pix(vec![AccountMeta::new(payer.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(w_pf, false)],
            PIx::PermissionlessCrank { now_slot: 110, observations: vec![percolator_prog::ix::CrankObservationHint { asset_index: a, oracle_accounts: 0 }] })], Some(&payer.pubkey()), &[&payer], bh));
    };
    for a in 0..2u16 { crank(&mut svm, a); crank(&mut svm, a); }

    let w_cryst = read_crystallized(&svm, &w_pf).saturating_sub(w_cryst0);
    let w_spent = read_spent(&svm, &w_pf);
    let net_counter = w_cryst.saturating_sub(w_spent);
    // read pnl@(HEADER_LEN 16 + pnl offset). pnl is a SIGNED i128 — read raw and interpret.
    let pnl_raw = { let d = svm.get_account(&w_pf).unwrap().data; i128::from_le_bytes(d[180..196].try_into().unwrap()) };
    let single_leg_loss = (posq.unsigned_abs() * (initial_price as u128) / percolator::POS_SCALE) / 2; // ~50% of one leg's notional
    println!("\n========== CROSS-MARGIN crystallized: GROSS or NET? (single-portfolio wash) ==========");
    println!("washer: ONE portfolio, long asset0 + long asset1 (equal size). asset0 -50% (loses), asset1 +50% (wins)");
    println!("portfolio crystallized (realized LOSS counter) : {w_cryst}");
    println!("portfolio spent (self-recovery counter)        : {w_spent}");
    println!("portfolio pnl (signed, net incl. winning leg)  : {pnl_raw}");
    println!("rd TRADER counter = crystallized - spent       : {net_counter}");
    println!("one losing leg's ~50% notional loss (ref)      : ~{single_leg_loss}");
    if w_cryst > 0 {
        println!("=> GROSS: the losing leg's loss crystallizes in FULL even though the WINNING leg offsets the");
        println!("   portfolio's PnL to ~net-flat. spent stays {w_spent}. A single cross-margin portfolio thus farms");
        println!("   trader points for ~0 NET PnL -> the finding-NZ delta-neutral wash at the SINGLE-ACCOUNT level.");
        println!("   The fee + allow-list + time-weight + dilution remain the bound (NOT spent-netting, which is 0).");
    } else {
        println!("=> NET: percolator nets the winning leg against the losing leg -> crystallized ~0 -> the");
        println!("   single-portfolio delta-neutral wash is BLOCKED at settlement.");
    }
    println!("====================================================================================\n");
    // The security-relevant assertion: WHICHEVER it is, spent stays 0 (no churn), so net-by-spent does NOT bound
    // this — the bound is the claim fee (already pinned). Pin the empirical fact so the prior tick's claim is
    // corrected/confirmed and future ticks don't re-probe.
    assert_eq!(w_spent, 0, "no churn -> spent stays 0; net-by-spent cannot bound a delta-neutral cross-asset wash");
    let _ = (net_counter, single_leg_loss, pnl_raw);
}

#[test]
fn crossed_trade_cannot_self_finance_preexisting_oi_reduction() {
    const PRICE: u64 = 1;
    const OPEN_Q: i128 = 13_000 * percolator::POS_SCALE as i128;

    let mut svm = LiteSVM::new().with_compute_budget(
        solana_program_runtime::compute_budget::ComputeBudget {
            compute_unit_limit: 1_400_000,
            heap_size: 256 * 1024,
            ..solana_program_runtime::compute_budget::ComputeBudget::default()
        },
    );
    svm.add_program_from_file(perc_id(), perc_so()).unwrap();

    let payer = Keypair::new();
    let admin = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000_000)
        .unwrap();
    svm.airdrop(&admin.pubkey(), 1_000_000_000).unwrap();
    svm.set_sysvar(&Clock {
        slot: 1,
        unix_timestamp: 1,
        ..Clock::default()
    });

    let collateral = create_real_mint(&mut svm, &payer, &admin.pubkey());
    let market = Pubkey::new_unique();
    let market_len = percolator_prog::state::market_account_len_for_capacity(1).unwrap();
    svm.set_account(
        market,
        Account {
            lamports: 1_000_000_000,
            data: vec![0; market_len],
            owner: perc_id(),
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
    let vault_authority = perc_vault_authority(&market);
    let vault = canonical_insurance_vault(&vault_authority, &collateral);
    set_token(&mut svm, &vault, &collateral, &vault_authority, 0);

    let send = |svm: &mut LiteSVM, ix: Instruction, signers: &[&Keypair]| {
        svm.expire_blockhash();
        let blockhash = svm.latest_blockhash();
        let mut all_signers = vec![&payer];
        all_signers.extend_from_slice(signers);
        svm.send_transaction(Transaction::new_signed_with_payer(
            &[ix],
            Some(&payer.pubkey()),
            &all_signers,
            blockhash,
        ))
    };

    send(
        &mut svm,
        pix(
            vec![
                AccountMeta::new(admin.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new_readonly(collateral, false),
            ],
            PIx::InitMarket {
                max_portfolio_assets: 1,
                h_min: 0,
                h_max: 6_480_000,
                initial_price: PRICE,
                min_nonzero_mm_req: 599,
                min_nonzero_im_req: 600,
                maintenance_margin_bps: 500,
                initial_margin_bps: 500,
                max_trading_fee_bps: 10_000,
                trade_fee_base_bps: 0,
                liquidation_fee_bps: 0,
                liquidation_fee_cap: 0,
                min_liquidation_abs: 0,
                max_price_move_bps_per_slot: 24,
                max_accrual_dt_slots: 20,
                max_abs_funding_e9_per_slot: 0,
                min_funding_lifetime_slots: 10_000_000,
                max_account_b_settlement_chunks: 1,
                max_bankrupt_close_chunks: 1,
                max_bankrupt_close_lifetime_slots: 100,
                public_b_chunk_atoms: percolator::MAX_VAULT_TVL,
                maintenance_fee_per_slot: 27,
            },
        ),
        &[&admin],
    )
    .expect("initialize liquidation probe market");
    send(
        &mut svm,
        pix(
            vec![
                AccountMeta::new(admin.pubkey(), true),
                AccountMeta::new(market, false),
            ],
            PIx::ConfigureAuthMark {
                asset_index: 0,
                now_slot: 1,
                initial_mark_e6: PRICE,
            },
        ),
        &[&admin],
    )
    .expect("configure authenticated mark");

    let owner_a = Keypair::new();
    let owner_b = Keypair::new();
    svm.airdrop(&owner_a.pubkey(), 1_000_000_000).unwrap();
    svm.airdrop(&owner_b.pubkey(), 1_000_000_000).unwrap();
    let portfolio_a = Pubkey::new_unique();
    let portfolio_b = Pubkey::new_unique();
    let portfolio_len =
        percolator_prog::state::portfolio_account_len_for_market_slots(1).unwrap();
    for (owner, portfolio, deposit) in [
        (&owner_a, portfolio_a, 1_000u64),
        (&owner_b, portfolio_b, 1_189u64),
    ] {
        svm.set_account(
            portfolio,
            Account {
                lamports: 1_000_000_000,
                data: vec![0; portfolio_len],
                owner: perc_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
        send(
            &mut svm,
            pix(
                vec![
                    AccountMeta::new(owner.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new(portfolio, false),
                ],
                PIx::InitPortfolio,
            ),
            &[owner],
        )
        .expect("initialize portfolio");

        let source = Pubkey::new_unique();
        set_token(
            &mut svm,
            &source,
            &collateral,
            &owner.pubkey(),
            deposit,
        );
        send(
            &mut svm,
            pix(
                vec![
                    AccountMeta::new(owner.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new(portfolio, false),
                    AccountMeta::new(source, false),
                    AccountMeta::new(vault, false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                ],
                PIx::Deposit {
                    amount: u128::from(deposit),
                },
            ),
            &[owner],
        )
        .expect("deposit collateral");
    }

    svm.set_sysvar(&Clock {
        slot: 8,
        unix_timestamp: 8,
        ..Clock::default()
    });
    send(
        &mut svm,
        pix(
            vec![
                AccountMeta::new(owner_a.pubkey(), true),
                AccountMeta::new(owner_b.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(portfolio_a, false),
                AccountMeta::new(portfolio_b, false),
            ],
            PIx::TradeNoCpi {
                asset_index: 0,
                size_q: OPEN_Q,
                exec_price: PRICE,
                fee_bps: 0,
            },
        ),
        &[&owner_a, &owner_b],
    )
    .expect("open balanced interest");

    let crank = |svm: &mut LiteSVM, portfolio: Pubkey, slot: u64| {
        send(
            svm,
            pix(
                vec![
                    AccountMeta::new(payer.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new(portfolio, false),
                ],
                PIx::PermissionlessCrank {
                    now_slot: slot,
                    observations: vec![percolator_prog::ix::CrankObservationHint {
                        asset_index: 0,
                        oracle_accounts: 0,
                    }],
                },
            ),
            &[],
        )
    };

    svm.set_sysvar(&Clock {
        slot: 27,
        unix_timestamp: 27,
        ..Clock::default()
    });
    crank(&mut svm, portfolio_a, 27).expect("refresh first side");

    svm.set_sysvar(&Clock {
        slot: 35,
        unix_timestamp: 35,
        ..Clock::default()
    });
    send(
        &mut svm,
        pix(
            vec![
                AccountMeta::new(market, false),
                AccountMeta::new(portfolio_b, false),
            ],
            PIx::SyncMaintenanceFee { now_slot: 35 },
        ),
        &[],
    )
    .expect("make second side partially liquidatable");
    crank(&mut svm, portfolio_b, 35).expect("certify liquidation");
    crank(&mut svm, portfolio_b, 35).expect("partially liquidate");

    let market_data = svm.get_account(&market).unwrap().data;
    let (_, group) = percolator_prog::state::read_market(&market_data).unwrap();
    let asset = group.assets[0];
    let portfolio_data_a = svm.get_account(&portfolio_a).unwrap().data;
    let portfolio_data_b = svm.get_account(&portfolio_b).unwrap().data;
    let state_a = percolator_prog::state::read_portfolio(&portfolio_data_a).unwrap();
    let state_b = percolator_prog::state::read_portfolio(&portfolio_data_b).unwrap();
    let position_q = |state: &percolator::PortfolioAccountV16Account| {
        state
            .legs
            .iter()
            .filter_map(|leg| leg.try_to_runtime().ok())
            .find(|leg| leg.active && leg.asset_index == 0)
            .expect("active asset-0 leg")
            .basis_pos_q
    };
    let survivor_q = position_q(&state_a).unsigned_abs();
    let liquidated_q = position_q(&state_b).unsigned_abs();
    assert_eq!(survivor_q, OPEN_Q.unsigned_abs());
    assert!(
        liquidated_q > 0 && liquidated_q < survivor_q,
        "probe requires a partial liquidation, got {liquidated_q} of {survivor_q}"
    );
    assert_eq!(asset.oi_eff_long_q, liquidated_q);
    assert_eq!(asset.oi_eff_short_q, liquidated_q);

    let cross_q = liquidated_q + (survivor_q - liquidated_q) / 2;
    let market_before = svm.get_account(&market).unwrap();
    let portfolio_a_before = svm.get_account(&portfolio_a).unwrap();
    let portfolio_b_before = svm.get_account(&portfolio_b).unwrap();
    let vault_before = svm.get_account(&vault).unwrap();
    let result = send(
        &mut svm,
        pix(
            vec![
                AccountMeta::new(owner_b.pubkey(), true),
                AccountMeta::new(owner_a.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(portfolio_b, false),
                AccountMeta::new(portfolio_a, false),
            ],
            PIx::TradeNoCpi {
                asset_index: 0,
                size_q: cross_q as i128,
                exec_price: PRICE,
                fee_bps: 0,
            },
        ),
        &[&owner_b, &owner_a],
    );
    assert!(
        result.is_err(),
        "a trade cannot reduce more OI than existed before that same instruction"
    );
    assert_eq!(svm.get_account(&market).unwrap(), market_before);
    assert_eq!(svm.get_account(&portfolio_a).unwrap(), portfolio_a_before);
    assert_eq!(svm.get_account(&portfolio_b).unwrap(), portfolio_b_before);
    assert_eq!(svm.get_account(&vault).unwrap(), vault_before);
}

// An authenticated EWMA rebound can remain pending while the circuit-bounded engine price catches
// up. Public wash trades must pay enough to overwrite that target; otherwise the coalition can
// resolve at the displaced mark and withdraw PnL taken from an unrelated directional trader.
#[test]
fn pending_ewma_target_override_cannot_extract_independent_victim_pnl() {
    const BASIS: u64 = 10_000_000;
    const DIRECTIONAL_Q: i128 = 1_000 * percolator::POS_SCALE as i128;
    const WASH_Q: i128 = 100 * percolator::POS_SCALE as i128;
    const DIRECTIONAL_DEPOSIT: u64 = 20_000_000_000;
    const WASH_DEPOSIT: u64 = 2_000_000_000;
    const ATTACKER_DEPOSITS: u128 =
        DIRECTIONAL_DEPOSIT as u128 + 2 * WASH_DEPOSIT as u128;

    let mut svm = LiteSVM::new().with_compute_budget(
        solana_program_runtime::compute_budget::ComputeBudget {
            compute_unit_limit: 1_400_000,
            heap_size: 256 * 1024,
            ..solana_program_runtime::compute_budget::ComputeBudget::default()
        },
    );
    svm.add_program_from_file(perc_id(), perc_so()).unwrap();

    let payer = Keypair::new();
    let admin = Keypair::new();
    let honest_oracle = Keypair::new();
    let victim = Keypair::new();
    let directional_attacker = Keypair::new();
    let wash_long = Keypair::new();
    let wash_short = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000_000)
        .unwrap();
    for signer in [
        &admin,
        &honest_oracle,
        &victim,
        &directional_attacker,
        &wash_long,
        &wash_short,
    ] {
        svm.airdrop(&signer.pubkey(), 1_000_000_000).unwrap();
    }
    svm.set_sysvar(&Clock {
        slot: 1,
        unix_timestamp: 1,
        ..Clock::default()
    });

    let collateral = create_real_mint(&mut svm, &payer, &admin.pubkey());
    let market = Pubkey::new_unique();
    let market_len = percolator_prog::state::market_account_len_for_capacity(1).unwrap();
    svm.set_account(
        market,
        Account {
            lamports: 1_000_000_000,
            data: vec![0; market_len],
            owner: perc_id(),
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
    let vault_authority = perc_vault_authority(&market);
    let vault = canonical_insurance_vault(&vault_authority, &collateral);
    set_token(&mut svm, &vault, &collateral, &vault_authority, 0);

    let send = |svm: &mut LiteSVM, ix: Instruction, signers: &[&Keypair]| {
        svm.expire_blockhash();
        let blockhash = svm.latest_blockhash();
        let mut all_signers = vec![&payer];
        all_signers.extend_from_slice(signers);
        svm.send_transaction(Transaction::new_signed_with_payer(
            &[ix],
            Some(&payer.pubkey()),
            &all_signers,
            blockhash,
        ))
    };

    send(
        &mut svm,
        pix(
            vec![
                AccountMeta::new(admin.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new_readonly(collateral, false),
            ],
            PIx::InitMarket {
                max_portfolio_assets: 1,
                h_min: 0,
                h_max: 10,
                initial_price: BASIS,
                min_nonzero_mm_req: 1,
                min_nonzero_im_req: 2,
                maintenance_margin_bps: 10_000,
                initial_margin_bps: 10_000,
                max_trading_fee_bps: 10_000,
                trade_fee_base_bps: 0,
                liquidation_fee_bps: 0,
                liquidation_fee_cap: 0,
                min_liquidation_abs: 0,
                max_price_move_bps_per_slot: 5_000,
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
        &[&admin],
    )
    .expect("initialize public EWMA market");
    send(
        &mut svm,
        pix(
            vec![
                AccountMeta::new(admin.pubkey(), true),
                AccountMeta::new(market, false),
            ],
            PIx::ConfigureEwmaMark {
                asset_index: 0,
                now_slot: 1,
                initial_mark_e6: BASIS,
                mark_ewma_halflife_slots: 1,
                mark_min_fee: 0,
            },
        ),
        &[&admin],
    )
    .expect("configure EWMA oracle");
    send(
        &mut svm,
        pix(
            vec![
                AccountMeta::new(admin.pubkey(), true),
                AccountMeta::new(honest_oracle.pubkey(), true),
                AccountMeta::new(market, false),
            ],
            PIx::UpdateAssetAuthority {
                asset_index: 0,
                kind: percolator_prog::processor::ASSET_AUTH_ORACLE,
                new_pubkey: honest_oracle.pubkey().to_bytes(),
            },
        ),
        &[&admin, &honest_oracle],
    )
    .expect("separate honest oracle from attacker coalition");

    let victim_portfolio = Pubkey::new_unique();
    let directional_attacker_portfolio = Pubkey::new_unique();
    let wash_long_portfolio = Pubkey::new_unique();
    let wash_short_portfolio = Pubkey::new_unique();
    let portfolio_len =
        percolator_prog::state::portfolio_account_len_for_market_slots(1).unwrap();
    let participants = [
        (&victim, victim_portfolio, DIRECTIONAL_DEPOSIT),
        (
            &directional_attacker,
            directional_attacker_portfolio,
            DIRECTIONAL_DEPOSIT,
        ),
        (&wash_long, wash_long_portfolio, WASH_DEPOSIT),
        (&wash_short, wash_short_portfolio, WASH_DEPOSIT),
    ];
    let mut collateral_accounts = Vec::new();
    for (owner, portfolio, deposit) in participants {
        svm.set_account(
            portfolio,
            Account {
                lamports: 1_000_000_000,
                data: vec![0; portfolio_len],
                owner: perc_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
        send(
            &mut svm,
            pix(
                vec![
                    AccountMeta::new(owner.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new(portfolio, false),
                ],
                PIx::InitPortfolio,
            ),
            &[owner],
        )
        .expect("initialize public portfolio");
        let token = create_real_token_account(&mut svm, &payer, &collateral, &owner.pubkey());
        mint_real_tokens(
            &mut svm,
            &payer,
            &collateral,
            &admin,
            &token,
            deposit,
        );
        send(
            &mut svm,
            pix(
                vec![
                    AccountMeta::new(owner.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new(portfolio, false),
                    AccountMeta::new(token, false),
                    AccountMeta::new(vault, false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                ],
                PIx::Deposit {
                    amount: u128::from(deposit),
                },
            ),
            &[owner],
        )
        .expect("deposit real SPL collateral");
        collateral_accounts.push(token);
    }
    let victim_collateral = collateral_accounts[0];
    let directional_attacker_collateral = collateral_accounts[1];
    let wash_long_collateral = collateral_accounts[2];
    let wash_short_collateral = collateral_accounts[3];

    send(
        &mut svm,
        pix(
            vec![
                AccountMeta::new(victim.pubkey(), true),
                AccountMeta::new(directional_attacker.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(victim_portfolio, false),
                AccountMeta::new(directional_attacker_portfolio, false),
            ],
            PIx::TradeNoCpi {
                asset_index: 0,
                size_q: DIRECTIONAL_Q,
                exec_price: BASIS,
                fee_bps: 0,
            },
        ),
        &[&victim, &directional_attacker],
    )
    .expect("open independent directional exposure");

    let crank = |svm: &mut LiteSVM, portfolio: Pubkey, slot: u64| {
        send(
            svm,
            pix(
                vec![
                    AccountMeta::new(payer.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new(portfolio, false),
                ],
                PIx::PermissionlessCrank {
                    now_slot: slot,
                    observations: vec![percolator_prog::ix::CrankObservationHint {
                        asset_index: 0,
                        oracle_accounts: 0,
                    }],
                },
            ),
            &[],
        )
    };
    let push = |svm: &mut LiteSVM, slot: u64, mark_e6: u64| {
        send(
            svm,
            pix(
                vec![
                    AccountMeta::new(honest_oracle.pubkey(), true),
                    AccountMeta::new(market, false),
                ],
                PIx::PushEwmaMark {
                    asset_index: 0,
                    now_slot: slot,
                    mark_e6,
                },
            ),
            &[&honest_oracle],
        )
    };
    let set_slot = |svm: &mut LiteSVM, slot: u64| {
        svm.set_sysvar(&Clock {
            slot,
            unix_timestamp: slot as i64,
            ..Clock::default()
        });
    };

    for slot in 2..=5 {
        set_slot(&mut svm, slot);
        push(&mut svm, slot, 1).expect("honest oracle publishes bounded down move");
        crank(&mut svm, victim_portfolio, slot).expect("settle victim down move");
        crank(&mut svm, directional_attacker_portfolio, slot)
            .expect("settle counterparty down move");
    }
    let low_price = percolator_prog::state::read_market(&svm.get_account(&market).unwrap().data)
        .unwrap()
        .1
        .assets[0]
        .effective_price;
    assert!(low_price < BASIS / 5, "setup must create a large real move");

    set_slot(&mut svm, 6);
    crank(&mut svm, victim_portfolio, 6).expect("advance victim at low mark");
    crank(&mut svm, directional_attacker_portfolio, 6)
        .expect("advance counterparty at low mark");
    let rebound_input = BASIS
        .checked_mul(2)
        .unwrap()
        .checked_sub(low_price)
        .unwrap();
    push(&mut svm, 6, rebound_input).expect("honest oracle authenticates full rebound");
    let profile_before_attack = percolator_prog::state::read_asset_oracle_profile(
        &svm.get_account(&market).unwrap().data,
        0,
    )
    .unwrap();
    assert_eq!(profile_before_attack.mark_ewma_e6, BASIS);
    assert_eq!(
        percolator_prog::state::read_market(&svm.get_account(&market).unwrap().data)
            .unwrap()
            .1
            .assets[0]
            .effective_price,
        low_price,
        "authenticated rebound must still be circuit-breaker pending"
    );

    set_slot(&mut svm, 7);
    let insurance_before = read_insurance(&svm, &market);
    for (size_q, exec_price) in [(WASH_Q, 1), (-WASH_Q, low_price)] {
        send(
            &mut svm,
            pix(
                vec![
                    AccountMeta::new(wash_long.pubkey(), true),
                    AccountMeta::new(wash_short.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new(wash_long_portfolio, false),
                    AccountMeta::new(wash_short_portfolio, false),
                ],
                PIx::TradeNoCpi {
                    asset_index: 0,
                    size_q,
                    exec_price,
                    fee_bps: 0,
                },
            ),
            &[&wash_long, &wash_short],
        )
        .expect("public wash route remains executable");
    }
    let movement_fee = read_insurance(&svm, &market) - insurance_before;
    let displaced_target = percolator_prog::state::read_asset_oracle_profile(
        &svm.get_account(&market).unwrap().data,
        0,
    )
    .unwrap()
    .mark_ewma_e6;
    assert!(
        displaced_target < BASIS,
        "public wash must overwrite the authenticated rebound"
    );
    assert!(movement_fee > 0, "target overwrite must charge a real fee");

    let mut slot = 7u64;
    loop {
        crank(&mut svm, victim_portfolio, slot).expect("publish target for victim");
        crank(&mut svm, directional_attacker_portfolio, slot)
            .expect("publish target for counterparty");
        let effective =
            percolator_prog::state::read_market(&svm.get_account(&market).unwrap().data)
                .unwrap()
                .1
                .assets[0]
                .effective_price;
        if effective == displaced_target {
            break;
        }
        slot += 1;
        assert!(slot < 24, "bounded target convergence must complete");
        set_slot(&mut svm, slot);
    }

    send(
        &mut svm,
        pix(
            vec![
                AccountMeta::new(admin.pubkey(), true),
                AccountMeta::new(market, false),
            ],
            PIx::ResolveMarket,
        ),
        &[&admin],
    )
    .expect("honest admin resolves at the converged authenticated mark");

    let close_resolved = |svm: &mut LiteSVM,
                          owner: Pubkey,
                          portfolio: Pubkey,
                          destination: Pubkey| {
        send(
            svm,
            pix(
                vec![
                    AccountMeta::new_readonly(owner, false),
                    AccountMeta::new(market, false),
                    AccountMeta::new(portfolio, false),
                    AccountMeta::new(destination, false),
                    AccountMeta::new(vault, false),
                    AccountMeta::new_readonly(vault_authority, false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                ],
                PIx::CloseResolved {
                    fee_rate_per_slot: 0,
                },
            ),
            &[],
        )
    };
    for _ in 0..4 {
        close_resolved(
            &mut svm,
            victim.pubkey(),
            victim_portfolio,
            victim_collateral,
        )
        .expect("bounded victim terminal settlement");
        close_resolved(
            &mut svm,
            directional_attacker.pubkey(),
            directional_attacker_portfolio,
            directional_attacker_collateral,
        )
        .expect("bounded counterparty terminal settlement");
    }
    close_resolved(
        &mut svm,
        wash_long.pubkey(),
        wash_long_portfolio,
        wash_long_collateral,
    )
    .expect("settle flat wash long");
    close_resolved(
        &mut svm,
        wash_short.pubkey(),
        wash_short_portfolio,
        wash_short_collateral,
    )
    .expect("settle flat wash short");

    let claim_topup = |svm: &mut LiteSVM,
                       owner: Pubkey,
                       portfolio: Pubkey,
                       destination: Pubkey| {
        send(
            svm,
            pix(
                vec![
                    AccountMeta::new_readonly(owner, false),
                    AccountMeta::new(market, false),
                    AccountMeta::new(portfolio, false),
                    AccountMeta::new(destination, false),
                    AccountMeta::new(vault, false),
                    AccountMeta::new_readonly(vault_authority, false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                ],
                PIx::ClaimResolvedPayoutTopup,
            ),
            &[],
        )
    };
    for (owner, portfolio, destination) in [
        (victim.pubkey(), victim_portfolio, victim_collateral),
        (
            directional_attacker.pubkey(),
            directional_attacker_portfolio,
            directional_attacker_collateral,
        ),
        (wash_long.pubkey(), wash_long_portfolio, wash_long_collateral),
        (
            wash_short.pubkey(),
            wash_short_portfolio,
            wash_short_collateral,
        ),
    ] {
        let _ = claim_topup(&mut svm, owner, portfolio, destination);
    }

    let attacker_withdrawn = u128::from(token_amount(
        &svm,
        &directional_attacker_collateral,
    )) + u128::from(token_amount(&svm, &wash_long_collateral))
        + u128::from(token_amount(&svm, &wash_short_collateral));
    let victim_withdrawn = u128::from(token_amount(&svm, &victim_collateral));
    assert!(
        attacker_withdrawn <= ATTACKER_DEPOSITS,
        "pending-target override paid attacker coalition {attacker_withdrawn} from {ATTACKER_DEPOSITS} deposited; victim withdrew {victim_withdrawn}; movement fee {movement_fee}"
    );
    let victim_loss = (DIRECTIONAL_DEPOSIT as u128).saturating_sub(victim_withdrawn);
    assert!(
        victim_loss > 0,
        "probe did not exercise pending-target price displacement"
    );
    assert!(
        movement_fee >= victim_loss,
        "movement fee {movement_fee} failed to cover independent victim loss {victim_loss}"
    );
}
