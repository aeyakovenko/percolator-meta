//! Real-percolator litesvm end-to-end test for the non-custodial insurance
//! deposit / vote / exit flow.
//!
//! Proves, against the REAL percolator binary
//! (`../percolator-prog/target/deploy/percolator_prog.so`, loaded into litesvm):
//!
//! 1. A user deposits into market-0 INSURANCE through the `subledger` program (the
//!    subledger pool PDA is asset-0's insurance authority + operator). Funds land in
//!    the Percolator insurance vault and a subledger position records
//!    `owner, principal, start_slot`.
//! 2. `genesis-vote` reads that subledger position (principal + start_slot) and the
//!    pool's `outstanding_principal` to weight a vote.
//! 3. The user does a principal-only, owner-authorized exit through the subledger
//!    and gets their principal back. Non-owner exits and over-principal exits fail.
//!
//! Market-0 setup note: the percolator `UpdateAssetAuthority` handler requires the
//! *incoming* authority to co-sign when it is non-zero. The subledger pool is a
//! PDA, which cannot co-sign a top-level instruction, so we cannot rotate authority
//! to it with a plain `UpdateAssetAuthority`. Instead — exactly like the existing
//! genesis integration's manual market — we build the Live market-0 slab with the
//! real percolator state helper `init_market_account_zero_copy`, setting
//! `marketauth = pool_pda` (which percolator copies into asset-0's
//! insurance_authority + insurance_operator + asset_admin) and the deposits-only
//! insurance-withdraw policy (max_bps=10000, deposits_only=1, cooldown=0). The real
//! percolator binary then validates every TopUp/Withdraw CPI against that stored
//! state. This is the on-chain equivalent of the production flow, where the market
//! is born under the controlling PDA via a PDA-signed `InitMarket` CPI.

use litesvm::LiteSVM;
use solana_program_runtime::compute_budget::ComputeBudget;
use solana_sdk::{
    account::Account,
    clock::Clock,
    compute_budget::ComputeBudgetInstruction,
    instruction::{AccountMeta, Instruction},
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction,
    transaction::Transaction,
};

const ATA_PROGRAM_ID: Pubkey =
    solana_sdk::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

fn sub_id() -> Pubkey {
    subledger_program::id()
}
fn gv_id() -> Pubkey {
    genesis_vote_program::id()
}
fn dist_id() -> Pubkey {
    distribution_program::id()
}
fn perc_id() -> Pubkey {
    percolator_prog::id()
}

fn so(name: &str) -> String {
    format!("{}/../target/deploy/{}.so", env!("CARGO_MANIFEST_DIR"), name)
}
fn perc_so() -> String {
    format!(
        "{}/../../percolator-prog/target/deploy/percolator_prog.so",
        env!("CARGO_MANIFEST_DIR")
    )
}
fn clone_kp(kp: &Keypair) -> Keypair {
    Keypair::from_bytes(&kp.to_bytes()).unwrap()
}

const ASSET_ID: u64 = 0;
const POLICY_PRINCIPAL: u8 = 0;

struct Env {
    svm: LiteSVM,
    payer: Keypair,
    /// The at-risk COLLATERAL mint (mintable here to fund depositors). The subledger
    /// insurance pool and the percolator market-0 collateral use this.
    mint: Pubkey,
    /// The distributed COIN mint — a DIFFERENT, fixed-supply token (mint authority
    /// revoked at distribution init). genesis-vote + distribution are keyed by this.
    coin_mint: Pubkey,
    mint_auth: Keypair,
    slab: Pubkey,
    vault_authority: Pubkey,
    perc_vault: Pubkey,
    pool: Pubkey,
}

impl Env {
    fn new() -> Self {
        let mut svm = LiteSVM::new().with_compute_budget(ComputeBudget {
            compute_unit_limit: 1_400_000,
            heap_size: 256 * 1024,
            ..ComputeBudget::default()
        });
        svm.add_program_from_file(sub_id(), so("subledger_program")).unwrap();
        svm.add_program_from_file(gv_id(), so("genesis_vote_program")).unwrap();
        svm.add_program_from_file(dist_id(), so("distribution_program")).unwrap();
        svm.add_program_from_file(perc_id(), perc_so()).unwrap();

        let payer = Keypair::new();
        svm.airdrop(&payer.pubkey(), 1_000_000_000_000).unwrap();
        let mint_auth = Keypair::new();
        let mint = create_mint(&mut svm, &payer, &mint_auth.pubkey());
        // The distributed COIN is a separate fixed-supply token (authority revoked in
        // setup_vote once the distribution vault is funded).
        let coin_mint = create_mint(&mut svm, &payer, &mint_auth.pubkey());

        // The market slab is chosen first; the pool PDA commits to it (finding Q).
        let slab = Pubkey::new_unique();

        // The subledger insurance pool PDA: asset-0 insurance authority + operator,
        // bound to (mint, asset_id, market_slab, percolator_program).
        let pool = Pubkey::find_program_address(
            &[
                b"subledger_pool",
                mint.as_ref(),
                &ASSET_ID.to_le_bytes(),
                slab.as_ref(),
                perc_id().as_ref(),
            ],
            &sub_id(),
        )
        .0;

        // Build the real Live market-0 slab with marketauth = pool PDA and the
        // deposits-only principal-recovery insurance policy.
        let init_slot = 100u64;
        let slab_data = make_live_market(&slab, &mint, &pool, init_slot);
        svm.set_account(
            slab,
            Account {
                lamports: 1_000_000_000,
                data: slab_data,
                owner: perc_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

        let vault_authority =
            Pubkey::find_program_address(&[b"vault", slab.as_ref()], &perc_id()).0;
        // The canonical insurance vault: ATA of vault_authority for `mint`.
        let perc_vault = Pubkey::find_program_address(
            &[vault_authority.as_ref(), spl_token::ID.as_ref(), mint.as_ref()],
            &ATA_PROGRAM_ID,
        )
        .0;
        svm.set_account(
            perc_vault,
            Account {
                lamports: 1_000_000,
                data: token_account_data(&mint, &vault_authority, 0),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

        svm.set_sysvar(&Clock {
            slot: init_slot,
            unix_timestamp: 100,
            ..Clock::default()
        });

        Env {
            svm,
            payer,
            mint,
            coin_mint,
            mint_auth,
            slab,
            vault_authority,
            perc_vault,
            pool,
        }
    }

    fn send(&mut self, ixs: &[Instruction], extra: &[&Keypair]) -> Result<(), String> {
        self.svm.expire_blockhash();
        let bh = self.svm.latest_blockhash();
        let payer = clone_kp(&self.payer);
        let mut signers: Vec<&Keypair> = vec![&payer];
        signers.extend_from_slice(extra);
        let pk = self.payer.pubkey();
        let mut all = vec![ComputeBudgetInstruction::set_compute_unit_limit(1_400_000)];
        all.extend_from_slice(ixs);
        let tx = Transaction::new_signed_with_payer(&all, Some(&pk), &signers, bh);
        self.svm.send_transaction(tx).map(|_| ()).map_err(|e| format!("{:?}", e))
    }

    fn token_amount(&self, account: &Pubkey) -> u64 {
        let acc = self.svm.get_account(account).unwrap();
        spl_token::state::Account::unpack(&acc.data).unwrap().amount
    }

    fn warp_slot(&mut self, slot: u64) {
        self.svm.set_sysvar(&Clock {
            slot,
            unix_timestamp: slot as i64,
            ..Clock::default()
        });
    }

    fn position_pda(&self, owner: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(
            &[b"subledger_position", self.pool.as_ref(), owner.as_ref()],
            &sub_id(),
        )
        .0
    }

    // ---- subledger ----

    fn init_insurance_pool(&mut self) {
        let mut data = vec![3u8]; // IX_INIT_INSURANCE_POOL
        data.extend_from_slice(&ASSET_ID.to_le_bytes());
        data.push(POLICY_PRINCIPAL);
        let ix = Instruction {
            program_id: sub_id(),
            accounts: vec![
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new_readonly(self.mint, false),
                AccountMeta::new(self.pool, false),
                AccountMeta::new_readonly(self.perc_vault, false),
                AccountMeta::new_readonly(self.slab, false),
                AccountMeta::new_readonly(perc_id(), false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
                // vote_authority = the genesis-vote config PDA (keyed by the COIN).
                AccountMeta::new_readonly(gv_config_pda(&self.coin_mint, &self.pool), false),
            ],
            data,
        };
        self.send(&[ix], &[]).expect("init insurance pool");
    }

    fn insurance_deposit(
        &mut self,
        owner: &Keypair,
        owner_ata: &Pubkey,
        holding: &Pubkey,
        amount: u64,
    ) -> Result<(), String> {
        let mut data = vec![4u8]; // IX_INSURANCE_DEPOSIT
        data.extend_from_slice(&amount.to_le_bytes());
        let ix = Instruction {
            program_id: sub_id(),
            accounts: vec![
                AccountMeta::new(owner.pubkey(), true),
                AccountMeta::new(self.pool, false),
                AccountMeta::new(self.position_pda(&owner.pubkey()), false),
                AccountMeta::new(*owner_ata, false),
                AccountMeta::new(*holding, false),
                AccountMeta::new(self.slab, false),
                AccountMeta::new(self.perc_vault, false),
                AccountMeta::new_readonly(perc_id(), false),
                AccountMeta::new_readonly(spl_token::ID, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data,
        };
        self.send(&[ix], &[owner])
    }

    fn insurance_withdraw(
        &mut self,
        owner: &Keypair,
        owner_ata: &Pubkey,
        holding: &Pubkey,
        signer: &Keypair,
        amount: u64,
    ) -> Result<(), String> {
        let mut data = vec![5u8]; // IX_INSURANCE_WITHDRAW
        data.extend_from_slice(&amount.to_le_bytes());
        let ix = Instruction {
            program_id: sub_id(),
            accounts: vec![
                AccountMeta::new(signer.pubkey(), true),
                AccountMeta::new(self.pool, false),
                AccountMeta::new(self.position_pda(&owner.pubkey()), false),
                AccountMeta::new(*owner_ata, false),
                AccountMeta::new(*holding, false),
                AccountMeta::new(self.slab, false),
                AccountMeta::new(self.perc_vault, false),
                AccountMeta::new_readonly(self.vault_authority, false),
                AccountMeta::new_readonly(perc_id(), false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data,
        };
        self.send(&[ix], &[signer])
    }

    fn read_position(&self, owner: &Pubkey) -> (u64, u64, bool) {
        let acc = self.svm.get_account(&self.position_pda(owner)).unwrap();
        let principal = u64::from_le_bytes(acc.data[72..80].try_into().unwrap());
        let start_slot = u64::from_le_bytes(acc.data[89..97].try_into().unwrap());
        let withdrawn = acc.data[88] == 1;
        (principal, start_slot, withdrawn)
    }

    fn pool_outstanding(&self) -> u64 {
        let acc = self.svm.get_account(&self.pool).unwrap();
        u64::from_le_bytes(acc.data[80..88].try_into().unwrap())
    }
}

// "THOSE WHO STAY DECIDE" (intended design; reviewed re: external issue #20, kept by design).
// The genesis quorum is measured against the LIVE subledger outstanding, deliberately, so that exits during
// voting recompute it: a non-voter who leaves FORFEITS their share of the decision. alice holds 2% of the
// committed pool and votes; bob (98%, a non-voter) exits during voting. Before bob leaves, alice lacks quorum
// (2*2 !> 100); after bob forfeits by exiting, alice — now the majority of the remaining at-risk capital —
// decides. This is governance, NOT theft: bob gets his full principal back (only the COIN governance follows
// participation). #20 proposed anchoring quorum to the committed pool instead; that was reviewed and declined
// because it trades this capture-resistance for low-turnout STALLS (a passive majority could freeze the
// genesis forever). The complementary deposit-during-voting griefing (the inflate-quorum DOS) and the
// deposit-deadline that would bound BOTH are tracked in SECURITY_LOG as off-harness orchestration work.
#[test]
fn those_who_stay_decide_after_a_nonvoting_majority_forfeits_by_exiting() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);

    let (alice, alice_ata) = new_depositor(&mut env, 20_000); // 2%
    let (bob, bob_ata) = new_depositor(&mut env, 980_000); // 98%, never votes
    let pool = env.pool;
    let a_hold = create_holding(&mut env, &pool);
    let b_hold = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &a_hold, 20_000).expect("alice deposit");
    env.insurance_deposit(&bob, &bob_ata, &b_hold, 980_000).expect("bob deposit");
    assert_eq!(env.pool_outstanding(), 1_000_000, "full committed pool");

    let alice_dest = Pubkey::new_unique();
    let (dist_proposal, gv_proposal) = create_and_register_proposal(&mut env, &ve, 1, &alice_dest);
    env.warp_slot(1124); // alice's position has time-weight
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("alice backs her proposal");

    // Before the exit: a 2% voter lacks quorum against the full committed pool.
    assert!(
        gv_trigger(&mut env, &ve, &gv_proposal, &dist_proposal).is_err(),
        "2% cannot trigger against 100% committed"
    );

    // The 98% leaves VOLUNTARILY: insurance_withdraw is OWNER-SIGNED (bob signs his own exit). No one can
    // force a depositor out — `principal_only_owner_exit_returns_funds_and_guards` pins that a non-owner
    // cannot withdraw. So the capture below can only happen if the majority CHOOSES to forfeit.
    env.insurance_withdraw(&bob, &bob_ata, &b_hold, &bob, 980_000).expect("bob voluntarily exits (owner-signed)");
    assert_eq!(env.pool_outstanding(), 20_000, "outstanding recomputed to the at-risk capital that stayed");
    assert_eq!(env.token_amount(&bob_ata), 980_000, "the exiting majority keeps its FULL principal — no theft");

    // Now alice — the majority of the capital that STAYED at risk — decides. Intended, not a vuln.
    let dc = env.svm.get_account(&ve.dist_config).unwrap();
    assert!(dc.data[120..152] == [0u8; 32], "not sealed before the trigger");
    gv_trigger(&mut env, &ve, &gv_proposal, &dist_proposal).expect("those who stay decide: alice seals");
    let dc = env.svm.get_account(&ve.dist_config).unwrap();
    let sealed_to = Pubkey::new_from_array(dc.data[120..152].try_into().unwrap());
    assert_eq!(sealed_to, dist_proposal, "alice's proposal sealed — governance follows the capital that stayed");
}

// insurance_deposit routes funds user -> holding -> percolator insurance vault (TopUpInsurance, pool-signed).
// The transit `holding` must be a pool-PDA-owned token account for the pool mint. A holding the depositor
// controls would let the user->holding leg land funds in an attacker account before the (failing) TopUp; the
// deposit now validates it up front (matching insurance_withdraw), so a non-pool holding is refused outright.
#[test]
fn insurance_deposit_rejects_a_non_pool_holding() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let (alice, alice_ata) = new_depositor(&mut env, 1_000_000);

    // A token account of the correct mint but owned by an ATTACKER, not the pool PDA.
    let attacker = Pubkey::new_unique();
    let rogue_holding = Pubkey::new_unique();
    env.svm
        .set_account(
            rogue_holding,
            solana_sdk::account::Account {
                lamports: 1_000_000_000,
                data: token_account_data(&env.mint, &attacker, 0),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    assert!(
        env.insurance_deposit(&alice, &alice_ata, &rogue_holding, 1_000_000).is_err(),
        "deposit must reject a holding not owned by the pool PDA"
    );
    assert_eq!(env.pool_outstanding(), 0, "no credit from the rejected deposit");
    assert_eq!(env.token_amount(&alice_ata), 1_000_000, "alice's capital untouched");
}

// A pool-PDA-owned holding token account (created per depositor).
fn create_holding(env: &mut Env, owner_pool: &Pubkey) -> Pubkey {
    let acc = Keypair::new();
    let rent = env
        .svm
        .minimum_balance_for_rent_exemption(spl_token::state::Account::LEN);
    let mint = env.mint;
    let ixs = [
        system_instruction::create_account(
            &env.payer.pubkey(),
            &acc.pubkey(),
            rent,
            spl_token::state::Account::LEN as u64,
            &spl_token::ID,
        ),
        spl_token::instruction::initialize_account(&spl_token::ID, &acc.pubkey(), &mint, owner_pool)
            .unwrap(),
    ];
    let payer = clone_kp(&env.payer);
    let tx = Transaction::new_signed_with_payer(
        &ixs,
        Some(&payer.pubkey()),
        &[&payer, &acc],
        env.svm.latest_blockhash(),
    );
    env.svm.send_transaction(tx).unwrap();
    acc.pubkey()
}

/// Slab base offset of the percolator `MarketGroupV16` header
/// (HEADER_LEN 16 + WRAPPER_CONFIG_LEN 432).
const MARKET_GROUP_OFF: usize = 448;

/// Drive the live asset-0 insurance down to `new_insurance` *consistently*, exactly as a real
/// venue loss would: insurance, vault, the per-domain budgets and the remaining-budget total
/// all drop together so percolator's `validate_shape` invariants still hold. Every offset is
/// pinned against the REAL percolator struct (`offset_of!`) or canaried by value, so a layout
/// change in the percolator binary fails loudly here instead of silently mis-reading the fund.
fn impair_market(env: &mut Env, new_insurance: u128) {
    use percolator::MarketGroupV16HeaderAccount as H;
    let off_vault = MARKET_GROUP_OFF + core::mem::offset_of!(H, vault);
    let off_ins = MARKET_GROUP_OFF + core::mem::offset_of!(H, insurance);
    let off_rem = MARKET_GROUP_OFF + core::mem::offset_of!(H, insurance_domain_budget_remaining_total);
    // The exact constant the subledger ships must equal offset_of(insurance) — the whole point
    // of the pro-rata feature is reading the insurance fund, NOT the (larger) vault total.
    assert_eq!(off_ins, MARKET_GROUP_OFF + 301, "insurance offset drifted from real percolator struct");
    assert_ne!(off_ins, off_vault, "insurance must not alias vault");
    // Pin the SUBLEDGER's shipped src constant against the real struct too. The functional haircut tests
    // below set vault == insurance (a consistent loss), so they CANNOT distinguish a src offset that
    // accidentally reads vault@733 instead of insurance@749 — only this assertion catches a regression of
    // PERC_INSURANCE_OFFSET itself (the canary above only pins offset_of!, not what the program ships).
    assert_eq!(subledger_program::PERC_INSURANCE_OFFSET, off_ins,
        "subledger PERC_INSURANCE_OFFSET drifted from real percolator insurance field (would read vault as insurance)");

    // The asset-0 domain budgets live in the first asset slot (Market<T>), which the real
    // percolator binary packs immediately after the header. Locate the [long, short] u128 pair
    // by value (both == half the funded insurance after the 50/50 credit split) and canary that
    // they reconcile to the header's remaining-budget total.
    let mut acct = env.svm.get_account(&env.slab).unwrap();
    let rd = |d: &[u8], o: usize| u128::from_le_bytes(d[o..o + 16].try_into().unwrap());
    let rem = rd(&acct.data, off_rem);
    let mut off_long = None;
    let slot0 = MARKET_GROUP_OFF + core::mem::size_of::<H>();
    for o in slot0..acct.data.len().saturating_sub(48) {
        if rd(&acct.data, o) == rem / 2
            && rd(&acct.data, o + 16) == rem - rem / 2
            && rd(&acct.data, o + 32) == 0  // spent_long
            && rd(&acct.data, o + 48) == 0  // spent_short
        {
            off_long = Some(o);
            break;
        }
    }
    let off_long = off_long.expect("locate asset-0 domain budget pair in slab");
    let off_short = off_long + 16;
    assert_eq!(
        rd(&acct.data, off_long) + rd(&acct.data, off_short),
        rem,
        "domain budgets must sum to remaining-budget total (layout canary)"
    );

    let long = new_insurance / 2;
    let short = new_insurance - long;
    acct.data[off_vault..off_vault + 16].copy_from_slice(&new_insurance.to_le_bytes());
    acct.data[off_ins..off_ins + 16].copy_from_slice(&new_insurance.to_le_bytes());
    acct.data[off_rem..off_rem + 16].copy_from_slice(&new_insurance.to_le_bytes());
    acct.data[off_long..off_long + 16].copy_from_slice(&long.to_le_bytes());
    acct.data[off_short..off_short + 16].copy_from_slice(&short.to_le_bytes());
    env.svm.set_account(env.slab, acct).unwrap();
}

fn make_live_market(slab: &Pubkey, mint: &Pubkey, marketauth: &Pubkey, init_slot: u64) -> Vec<u8> {
    let initial_price = 1_000_000u64;
    let mut wrapper = percolator_prog::state::WrapperConfigV16::default();
    wrapper.marketauth = marketauth.to_bytes();
    wrapper.collateral_mint = mint.to_bytes();
    wrapper.last_good_oracle_slot = init_slot;
    // Principal-only insurance withdraw: deposits_only caps to deposited principal,
    // never market profits; max_bps=10000 + cooldown=0 = full principal, no rate limit.
    wrapper.insurance_withdraw_max_bps = 10_000;
    wrapper.insurance_withdraw_deposits_only = 1;
    wrapper.insurance_withdraw_cooldown_slots = 0;
    wrapper.permissionless_resolve_stale_slots = 2_000;
    wrapper.force_close_delay_slots = 100;
    wrapper.oracle_mode = percolator_prog::constants::ORACLE_MODE_MANUAL;
    wrapper.mark_ewma_e6 = initial_price;
    wrapper.mark_ewma_last_slot = init_slot;
    wrapper.mark_ewma_halflife_slots =
        percolator_prog::constants::DEFAULT_MARK_EWMA_HALFLIFE_SLOTS;
    wrapper.oracle_target_price_e6 = initial_price;

    let mut data = vec![0u8; percolator_prog::constants::MARKET_ACCOUNT_LEN];
    let mut cfg = percolator_prog::risk::V16Config::public_user_fund(1, 0, 10);
    cfg.min_nonzero_mm_req = 1;
    cfg.min_nonzero_im_req = 2;
    cfg.maintenance_margin_bps = 10_000;
    cfg.initial_margin_bps = 10_000;
    cfg.max_trading_fee_bps = 10_000;
    cfg.max_accrual_dt_slots = 1;
    cfg.min_funding_lifetime_slots = 1;
    cfg.max_price_move_bps_per_slot = 10_000;
    cfg.max_account_b_settlement_chunks = 1;
    cfg.max_bankrupt_close_chunks = 1;
    cfg.max_bankrupt_close_lifetime_slots = 1;
    cfg.public_b_chunk_atoms = 1;
    percolator_prog::state::init_market_account_zero_copy(
        &mut data,
        &wrapper,
        cfg,
        slab.to_bytes(),
        initial_price,
        init_slot,
    )
    .expect("manual percolator market init");
    data
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

fn token_account_data(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Vec<u8> {
    let mut data = vec![0u8; spl_token::state::Account::LEN];
    let acc = spl_token::state::Account {
        mint: *mint,
        owner: *owner,
        amount,
        state: spl_token::state::AccountState::Initialized,
        ..Default::default()
    };
    spl_token::state::Account::pack(acc, &mut data).unwrap();
    data
}

fn create_token_account(svm: &mut LiteSVM, payer: &Keypair, mint: &Pubkey, owner: &Pubkey) -> Pubkey {
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

fn mint_to(svm: &mut LiteSVM, payer: &Keypair, mint: &Pubkey, authority: &Keypair, dest: &Pubkey, amount: u64) {
    let ix =
        spl_token::instruction::mint_to(&spl_token::ID, mint, dest, &authority.pubkey(), &[], amount)
            .unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[payer, authority],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).unwrap();
}

/// Funds a depositor: airdrop SOL, create their ATA, mint `amount` to it.
fn new_depositor(env: &mut Env, amount: u64) -> (Keypair, Pubkey) {
    let kp = Keypair::new();
    env.svm.airdrop(&kp.pubkey(), 10_000_000_000).unwrap();
    let payer = clone_kp(&env.payer);
    let auth = clone_kp(&env.mint_auth);
    let mint = env.mint;
    let ata = create_token_account(&mut env.svm, &payer, &mint, &kp.pubkey());
    if amount > 0 {
        mint_to(&mut env.svm, &payer, &mint, &auth, &ata, amount);
    }
    (kp, ata)
}

// ---------------------------------------------------------------------------
// genesis-vote + distribution setup (for the vote-read step)
// ---------------------------------------------------------------------------

struct VoteEnv {
    gv_config: Pubkey,
    dist_config: Pubkey,
    coin_vault: Pubkey,
}

fn gv_config_pda(mint: &Pubkey, subledger_pool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"gv_config", mint.as_ref(), subledger_pool.as_ref()], &gv_id()).0
}
fn dist_config_pda(mint: &Pubkey, authority: &Pubkey) -> Pubkey {
    // finding AA: the distribution config PDA binds its seal AUTHORITY (the gv config) into the
    // seed, so an attacker can't squat a funded config under a different authority.
    Pubkey::find_program_address(&[b"dist_config", mint.as_ref(), authority.as_ref()], &dist_id()).0
}

fn revoke_mint_authority(env: &mut Env, mint: &Pubkey) {
    let ix = spl_token::instruction::set_authority(
        &spl_token::ID,
        mint,
        None,
        spl_token::instruction::AuthorityType::MintTokens,
        &env.mint_auth.pubkey(),
        &[],
    )
    .unwrap();
    let auth = clone_kp(&env.mint_auth);
    env.send(&[ix], &[&auth]).expect("revoke mint authority");
}

fn setup_vote(env: &mut Env) -> VoteEnv {
    // gv + distribution are keyed by the COIN (a fixed-supply mint, distinct from
    // the collateral `env.mint` the subledger pool holds).
    let coin_mint = env.coin_mint;
    let gv_config = gv_config_pda(&coin_mint, &env.pool);
    let dist_config = dist_config_pda(&coin_mint, &gv_config);

    // distribution InitConfig with seal authority = the gv config PDA. Fund the COIN
    // vault, then REVOKE the COIN mint authority (the distribution requires a
    // fixed-supply COIN, README Safety §4).
    let dist_vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &coin_mint, &dist_config);
    mint_to(&mut env.svm, &clone_kp(&env.payer), &coin_mint, &clone_kp(&env.mint_auth), &dist_vault, 100);
    revoke_mint_authority(env, &coin_mint);
    let mut data = vec![0u8];
    data.extend_from_slice(&1_000_000u64.to_le_bytes()); // claim window
    data.extend_from_slice(&100u64.to_le_bytes()); // total supply
    let ix = Instruction {
        program_id: dist_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(coin_mint, false),
            AccountMeta::new(dist_config, false),
            AccountMeta::new_readonly(dist_vault, false),
            AccountMeta::new_readonly(gv_config, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    };
    env.send(&[ix], &[]).expect("dist init");

    // genesis-vote InitConfig: stores the subledger program + pool to read at vote.
    let ix = Instruction {
        program_id: gv_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(coin_mint, false),
            AccountMeta::new(gv_config, false),
            AccountMeta::new_readonly(dist_id(), false),
            AccountMeta::new_readonly(dist_config, false),
            AccountMeta::new_readonly(sub_id(), false),   // subledger_program
            AccountMeta::new_readonly(env.pool, false),   // subledger_pool
            AccountMeta::new_readonly(Pubkey::default(), false), // reserved
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: vec![0u8],
    };
    env.send(&[ix], &[]).expect("gv init");

    VoteEnv { gv_config, dist_config, coin_vault: dist_vault }
}

fn create_and_register_proposal(env: &mut Env, ve: &VoteEnv, id: u64, dest: &Pubkey) -> (Pubkey, Pubkey) {
    let dist_proposal =
        Pubkey::find_program_address(&[b"dist_proposal", ve.dist_config.as_ref(), &id.to_le_bytes()], &dist_id()).0;
    // create
    let mut data = vec![1u8];
    data.extend_from_slice(&id.to_le_bytes());
    data.extend_from_slice(&4u32.to_le_bytes());
    let create = Instruction {
        program_id: dist_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(ve.dist_config, false),
            AccountMeta::new(dist_proposal, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    };
    env.send(&[create], &[]).expect("create proposal");
    // append one entry (full supply to `dest`).
    let mut ad = vec![2u8];
    ad.extend_from_slice(&1u32.to_le_bytes());
    ad.extend_from_slice(dest.as_ref());
    ad.extend_from_slice(&100u64.to_le_bytes());
    let append = Instruction {
        program_id: dist_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(ve.dist_config, false),
            AccountMeta::new(dist_proposal, false),
        ],
        data: ad,
    };
    env.send(&[append], &[]).expect("append");

    // genesis-vote register_proposal
    let gv_proposal =
        Pubkey::find_program_address(&[b"gv_proposal", ve.gv_config.as_ref(), dist_proposal.as_ref()], &gv_id()).0;
    let reg = Instruction {
        program_id: gv_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(ve.gv_config, false),
            AccountMeta::new(gv_proposal, false),
            AccountMeta::new_readonly(dist_proposal, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: vec![2u8],
    };
    env.send(&[reg], &[]).expect("register");
    (dist_proposal, gv_proposal)
}

fn gv_vote(
    env: &mut Env,
    ve: &VoteEnv,
    voter: &Keypair,
    gv_proposal: &Pubkey,
    action: u8,
) -> Result<(), String> {
    let gv_ballot =
        Pubkey::find_program_address(&[b"gv_ballot", ve.gv_config.as_ref(), voter.pubkey().as_ref()], &gv_id()).0;
    let ix = Instruction {
        program_id: gv_id(),
        accounts: vec![
            AccountMeta::new(voter.pubkey(), true),
            AccountMeta::new(ve.gv_config, false),
            AccountMeta::new(gv_ballot, false),
            AccountMeta::new(*gv_proposal, false),
            AccountMeta::new(env.position_pda(&voter.pubkey()), false),
            AccountMeta::new_readonly(env.pool, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(sub_id(), false),
        ],
        data: vec![3u8, action],
    };
    env.send(&[ix], &[voter])
}

// Permissionless winner-take-all trigger: seals the distribution to the winning
// proposal. One voter holding 100% trivially clears quorum + majority.
fn gv_trigger(env: &mut Env, ve: &VoteEnv, gv_proposal: &Pubkey, dist_proposal: &Pubkey) -> Result<(), String> {
    let ix = Instruction {
        program_id: gv_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new(ve.gv_config, false),
            AccountMeta::new(*gv_proposal, false),
            AccountMeta::new_readonly(dist_id(), false),
            AccountMeta::new(ve.dist_config, false),
            AccountMeta::new(*dist_proposal, false),
            AccountMeta::new_readonly(env.pool, false), // live quorum denominator
        ],
        data: vec![4u8],
    };
    env.send(&[ix], &[])
}

fn gv_proposal_support(env: &Env, gv_proposal: &Pubkey) -> (u64, u64) {
    let acc = env.svm.get_account(gv_proposal).unwrap();
    // GG: support_weight is now u128 @72..88; test values fit u64, so read u128 and narrow.
    let support_weight = u128::from_le_bytes(acc.data[72..88].try_into().unwrap()) as u64;
    let support_principal = u64::from_le_bytes(acc.data[88..96].try_into().unwrap());
    (support_weight, support_principal)
}

// ===========================================================================
// Tests
// ===========================================================================

#[test]
fn deposit_into_real_percolator_insurance_records_position() {
    let mut env = Env::new();
    env.init_insurance_pool();

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);

    let before = env.token_amount(&env.perc_vault.clone());
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("insurance deposit");
    let after = env.token_amount(&env.perc_vault.clone());

    // Funds landed in the REAL Percolator insurance vault.
    assert_eq!(after - before, amount, "percolator insurance balance rose by deposit");
    assert_eq!(env.token_amount(&alice_ata), 0, "user ATA drained");

    // Position records principal + a nonzero start_slot; outstanding tracked.
    let (principal, start_slot, withdrawn) = env.read_position(&alice.pubkey());
    assert_eq!(principal, amount);
    assert_eq!(start_slot, 100, "start_slot = clock at deposit");
    assert!(!withdrawn);
    assert_eq!(env.pool_outstanding(), amount);
}

// Venue haircut behaviour of the insurance exit, against real percolator (finding L FIXED).
//
// SURPLUS: correctly EXCLUDED. percolator caps each WithdrawInsuranceLimited to
// `insurance*max_bps/1e4` then `min(deposit_remaining)`; with deposits_only=1 the cap
// is the deposited principal, so market profit/surplus is never withdrawable here.
//
// HAIRCUT: now PRO-RATA, not first-come. insurance_withdraw reads the LIVE asset-0 insurance
// straight from the slab; under an impairment (a venue loss that draws insurance below total
// outstanding principal) every exit receives insurance*amount/outstanding instead of the full
// principal. So a loss is shared proportionally and the exit is ORDER-INDEPENDENT — both an
// early and a late depositor take the SAME haircut; the first exit can no longer drain the
// pool and strand the rest.
#[test]
fn impaired_insurance_exit_is_pro_rata() {
    let mut env = Env::new();
    env.init_insurance_pool();

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let (bob, bob_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let a_hold = create_holding(&mut env, &pool);
    let b_hold = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &a_hold, amount).expect("alice deposit");
    env.insurance_deposit(&bob, &bob_ata, &b_hold, amount).expect("bob deposit");
    assert_eq!(env.token_amount(&env.perc_vault.clone()), 2 * amount, "insurance funded by both");
    assert_eq!(env.pool_outstanding(), 2 * amount, "outstanding = both deposits");

    // Simulate a 50% venue loss: the market drew half the insurance to cover trader losses.
    // A real loss debits the insurance fund, the vault, the per-domain budgets and the
    // remaining-budget total together, so we mirror that exactly against the real slab layout
    // (otherwise percolator's `validate_shape` invariant insurance >= domain-budget-remaining
    // rejects the next withdraw with EngineLockActive). After this the authoritative `insurance`
    // figure is 1M < outstanding 2M -> impaired.
    impair_market(&mut env, amount as u128);
    // Vault token balance drops to the same 1M (the other 1M was paid out covering the loss).
    env.svm
        .set_account(
            env.perc_vault,
            Account {
                lamports: 1_000_000,
                data: token_account_data(&env.mint, &env.vault_authority, amount),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    // Alice (early) exits her full principal but receives only her pro-rata share:
    // insurance(1,000,000) * amount(1,000,000) / outstanding(2,000,000) = 500,000 (a 50% haircut).
    env.insurance_withdraw(&alice, &alice_ata, &a_hold, &alice, amount).expect("alice exits");
    assert_eq!(env.token_amount(&alice_ata), 500_000, "early depositor takes the 50% haircut, not the full principal");
    assert_eq!(env.token_amount(&env.perc_vault.clone()), 500_000, "half the impaired insurance remains for bob");
    assert_eq!(env.pool_outstanding(), amount, "alice's full principal left the outstanding accounting");

    // Bob (late) gets the SAME 50% haircut — the pool was NOT drained by the first exit.
    // insurance now 500,000, outstanding now 1,000,000 -> 500,000 * 1,000,000 / 1,000,000 = 500,000.
    env.insurance_withdraw(&bob, &bob_ata, &b_hold, &bob, amount).expect("bob exits with the same haircut");
    assert_eq!(env.token_amount(&bob_ata), 500_000, "late depositor takes the SAME 50% haircut — order-independent");
    assert_eq!(env.token_amount(&env.perc_vault.clone()), 0, "impaired insurance fully and fairly distributed");
}

// CO-DEPOSITOR DRAIN (the per-position withdraw bound): insurance_withdraw caps `amount` by BOTH
// `position.principal` AND `pool.outstanding_principal` (lib.rs:1054). Because outstanding is the SUM of
// every position's principal, the per-position bound is always the tighter one and is the LOF-critical
// guard: drop it and a depositor in a multi-party pool can withdraw up to the WHOLE pool (amount up to
// outstanding), draining co-depositors AND underflowing their own `position.principal` to ~u64::MAX —
// a self-perpetuating infinite-withdrawal position. The pro-rata test above only ever withdraws each
// depositor's exact principal, so it never exercises this bound (the mutation that removes
// `amount > position.principal` passes the whole suite). This pins it: in a HEALTHY 2-party pool Alice
// tries to pull the whole 2M (> her 1M, but == outstanding so the pool-only bound would allow it); the
// withdraw MUST be rejected and Bob's funds MUST be untouched.
#[test]
fn a_depositor_cannot_withdraw_more_than_their_own_principal_and_drain_a_co_depositor() {
    let mut env = Env::new();
    env.init_insurance_pool();

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let (bob, bob_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let a_hold = create_holding(&mut env, &pool);
    let b_hold = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &a_hold, amount).expect("alice deposit");
    env.insurance_deposit(&bob, &bob_ata, &b_hold, amount).expect("bob deposit");
    assert_eq!(env.pool_outstanding(), 2 * amount, "outstanding = both deposits");

    // Pool is HEALTHY (insurance 2M >= outstanding 2M): payout would return the full requested `amount`
    // 1:1, so absent the per-position bound Alice's 2M request clears and steals Bob's 1M.
    let r = env.insurance_withdraw(&alice, &alice_ata, &a_hold, &alice, 2 * amount);
    assert!(r.is_err(), "withdrawing MORE than one's own principal must be rejected (co-depositor drain)");

    // Bob's capital is fully intact and the accounting is unchanged.
    assert_eq!(env.token_amount(&alice_ata), 0, "the over-withdraw paid Alice nothing");
    assert_eq!(env.token_amount(&bob_ata), 0, "bob still holds no payout — his principal is safe in the pool");
    assert_eq!(env.token_amount(&env.perc_vault.clone()), 2 * amount, "the insurance vault was NOT drained");
    assert_eq!(env.pool_outstanding(), 2 * amount, "outstanding unchanged — no principal left the accounting");
    let (a_principal, _, a_withdrawn) = env.read_position(&alice.pubkey());
    assert_eq!(a_principal, amount, "Alice's recorded principal is intact (no underflow)");
    assert!(!a_withdrawn, "Alice's position is not retired");

    // And the honest path still works: Alice may withdraw EXACTLY her own 1M.
    env.insurance_withdraw(&alice, &alice_ata, &a_hold, &alice, amount).expect("exact-principal exit succeeds");
    assert_eq!(env.token_amount(&alice_ata), amount, "Alice gets exactly her own principal");
    assert_eq!(env.pool_outstanding(), amount, "only Alice's principal left; Bob's 1M remains");
}

// FULLY-IMPAIRED EXIT (zero-payout retire must not DOS): under a TOTAL loss insurance is wiped to 0, so a
// depositor's owed = floor(0 * amount / outstanding) = 0. percolator rejects a zero-amount
// WithdrawInsuranceLimited, so insurance_withdraw guards the CPI behind `if owed > 0` (lib.rs:1081) and
// still retires the position. Without that guard a wiped depositor could NEVER retire — their lost
// principal would stay in pool.outstanding forever, permanently inflating the genesis QUORUM DENOMINATOR
// (which the trigger reads live) and bricking finalize. This is a different boundary than the pro-rata
// haircut (owed > 0).
#[test]
fn a_fully_impaired_exit_still_retires_the_position_without_a_zero_amount_cpi() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("deposit");
    assert_eq!(env.pool_outstanding(), amount, "principal outstanding");

    // TOTAL loss: the market wiped the entire insurance fund. Mirror it across the slab AND the vault.
    impair_market(&mut env, 0);
    env.svm.set_account(env.perc_vault, Account {
        lamports: 1_000_000, data: token_account_data(&env.mint, &env.vault_authority, 0),
        owner: spl_token::ID, executable: false, rent_epoch: 0,
    }).unwrap();

    // Alice exits her now-worthless position: owed == 0, but it MUST still retire (no zero-amount CPI, no DOS).
    env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, amount).expect("fully-impaired exit still retires the position");
    assert_eq!(env.token_amount(&alice_ata), 0, "nothing to pay — the principal was lost to the market");
    let (principal, _, withdrawn) = env.read_position(&alice.pubkey());
    assert_eq!(principal, 0, "position drained");
    assert!(withdrawn, "position retired");
    assert_eq!(env.pool_outstanding(), 0, "the wiped principal is removed from outstanding — quorum denominator stays accurate");
}

// TERMINAL-STATE REINIT (re-deposit into a RETIRED position): once a position is fully withdrawn
// (principal 0, withdrawn=true) insurance_deposit must REFUSE it (lib.rs:897, the `|| p.withdrawn` clause
// of its position-load guard; the own-vault process_deposit has a separate guard at :517). The
// position PDA is f(pool, owner), so a retired owner keeps the SAME terminal PDA. Without the guard a
// re-deposit would record principal>0 while `withdrawn` stays true (deposit never clears it) -> the
// funds can NEVER be withdrawn again (insurance_withdraw rejects withdrawn positions, :607) = stuck,
// AND pool.outstanding is inflated by that stuck principal, permanently dragging the genesis QUORUM
// DENOMINATOR (trigger reads outstanding live) for EVERY voter. Self-initiated, but the quorum drag is
// systemic — so the terminal guard matters. This pins it; previously only the *vote* side of a
// withdrawn position was tested (cannot_vote_with_a_withdrawn_position), not re-deposit.
#[test]
fn cannot_redeposit_into_a_retired_position() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("deposit");
    assert_eq!(env.pool_outstanding(), amount, "principal outstanding");

    // Healthy full exit: alice withdraws everything; the position retires.
    env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, amount).expect("full exit");
    let (principal, _, withdrawn) = env.read_position(&alice.pubkey());
    assert_eq!(principal, 0, "position drained");
    assert!(withdrawn, "position retired");
    assert_eq!(env.pool_outstanding(), 0, "outstanding cleared");
    assert_eq!(env.token_amount(&alice_ata), amount, "principal returned to alice");

    // ATTACK: re-deposit into the retired position. Must be REFUSED (terminal), before any transfer.
    assert!(
        env.insurance_deposit(&alice, &alice_ata, &holding, amount).is_err(),
        "re-deposit into a fully-withdrawn (retired) position must be refused"
    );
    // No stuck principal, no quorum drag, no funds moved: everything is exactly as it was after exit.
    assert_eq!(env.pool_outstanding(), 0, "outstanding NOT inflated by the rejected re-deposit");
    let (principal2, _, withdrawn2) = env.read_position(&alice.pubkey());
    assert_eq!(principal2, 0, "position still drained");
    assert!(withdrawn2, "position still retired");
    assert_eq!(env.token_amount(&alice_ata), amount, "alice's funds untouched (not sunk into a stuck position)");
}

// ROUNDING-GAME under impairment (split-withdraw, LOF on co-depositors): the haircut payout is
// mul_div_floor(insurance, amount, outstanding) and insurance_withdraw allows PARTIAL exits. A
// sophisticated exiter could try to beat their pro-rata share — or drain a co-depositor — by
// splitting their exit into many small partial withdraws, hoping the per-chunk rounding accumulates
// in their favour. Because each chunk FLOORS, splitting can only ever round DOWN: the splitter can
// never exceed their single-shot share, and the rounding dust is left in the insurance fund for
// whoever stays — never extracted. With an odd insurance (1,000,001) the dust is a real atom, so a
// round-UP regression would let the splitter cross 500_000 and the vault would be over-drawn (the
// co-depositor drained or the percolator CPI failing). Pins finding-L's conservation under the
// realistic split attack — the existing test only does single lump-sum exits.
#[test]
fn cannot_over_withdraw_to_drain_a_codepositor() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let (bob, bob_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let a_hold = create_holding(&mut env, &pool);
    let b_hold = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &a_hold, amount).expect("alice deposit");
    env.insurance_deposit(&bob, &bob_ata, &b_hold, amount).expect("bob deposit");
    assert_eq!(env.pool_outstanding(), 2 * amount, "outstanding = both deposits");
    assert_eq!(env.token_amount(&env.perc_vault.clone()), 2 * amount, "insurance fully funded (healthy)");

    // ATTACK: alice withdraws 2M (== outstanding, but > her OWN 1M principal) -> would drain bob. The
    // `amount > position.principal` cap (lib.rs:1054) is the ONLY thing that can reject it here: the sibling
    // `amount > outstanding` cap PASSES (2M is not > 2M), so a sole-depositor test would mask this guard.
    // Pins the per-position over-withdraw cap with a co-depositor present (sharp where the sole-depositor
    // test `cannot_withdraw_more_than_your_own_recorded_principal` is blind).
    assert!(
        env.insurance_withdraw(&alice, &alice_ata, &a_hold, &alice, 2 * amount).is_err(),
        "a depositor cannot withdraw more than their OWN recorded principal (would drain a co-depositor)"
    );
    assert_eq!(env.token_amount(&alice_ata), 0, "alice gained nothing from the over-withdraw");
    assert_eq!(env.token_amount(&env.perc_vault.clone()), 2 * amount, "insurance untouched by the rejected over-withdraw");
    // bob can still exit with his full principal — it was never drained.
    env.insurance_withdraw(&bob, &bob_ata, &b_hold, &bob, amount).expect("bob exits his own principal");
    assert_eq!(env.token_amount(&bob_ata), amount, "bob recovers his full 1M — not drained");
}

#[test]
fn splitting_an_impaired_exit_cannot_beat_the_pro_rata_or_drain_a_codepositor() {
    let mut env = Env::new();
    env.init_insurance_pool();

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let (bob, bob_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let a_hold = create_holding(&mut env, &pool);
    let b_hold = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &a_hold, amount).expect("alice deposit");
    env.insurance_deposit(&bob, &bob_ata, &b_hold, amount).expect("bob deposit");
    assert_eq!(env.pool_outstanding(), 2 * amount, "outstanding = both deposits");

    // Impair to an ODD 1,000,001 against outstanding 2,000,000 (just over a 50% loss). Mirror the
    // loss across the slab AND the vault token balance exactly as the lump-sum test does.
    let impaired = 1_000_001u128;
    impair_market(&mut env, impaired);
    env.svm
        .set_account(
            env.perc_vault,
            Account {
                lamports: 1_000_000,
                data: token_account_data(&env.mint, &env.vault_authority, impaired as u64),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    // ATTACK: alice splits her 1,000,000 exit into three uneven partial withdraws instead of one,
    // trying to make the per-chunk floor round in her favour. Each chunk floors, so her running
    // total can only fall short of — never exceed — her single-shot pro-rata share.
    for chunk in [400_000u64, 300_000, 300_000] {
        env.insurance_withdraw(&alice, &alice_ata, &a_hold, &alice, chunk).expect("alice partial exit");
    }
    let alice_total = env.token_amount(&alice_ata);
    let (a_principal, _, a_withdrawn) = env.read_position(&alice.pubkey());
    assert_eq!(a_principal, 0, "alice's full principal left the outstanding accounting across the splits");
    assert!(a_withdrawn, "alice's position is retired");
    assert!(
        alice_total <= 500_000,
        "a splitter can never exceed her floored 50% pro-rata share (got {alice_total})"
    );

    // Bob, who never split, exits last and is NOT drained — he collects at least as much as the
    // splitter, and the rounding atom the floor withheld from alice accrues to him.
    env.insurance_withdraw(&bob, &bob_ata, &b_hold, &bob, amount).expect("bob exits whole");
    let bob_total = env.token_amount(&bob_ata);
    assert!(
        bob_total >= alice_total,
        "the co-depositor who stayed is not drained by the splitter (bob {bob_total} >= alice {alice_total})"
    );

    // Conservation: the two exits together distribute EXACTLY the impaired insurance — never more
    // (no over-extraction) and the vault ends empty (no stranded principal).
    assert_eq!(alice_total + bob_total, impaired as u64, "exactly the impaired insurance was paid out — no more");
    assert_eq!(env.token_amount(&env.perc_vault.clone()), 0, "vault fully and fairly distributed");
}

// OVER-WITHDRAW DRAIN (principal-only cap, per-depositor): insurance_withdraw caps the amount to
// `amount <= position.principal && amount <= pool.outstanding` (lib.rs:1054). This pins the END-TO-END
// invariant that a depositor can never pull more than their own recorded principal. DOUBLY-DEFENDED:
// mutation-removing the `amount <= position.principal` half does NOT let the drain through right after
// deposits, because percolator's own insurance >= domain-budget-remaining (EngineLock) invariant rejects
// any withdraw that would drop insurance below the funded budgets. The subledger cap is the load-bearing
// per-caller guard only once the market has SPENT budgets so insurance > budget-remaining (not constructed
// here); in the budget-tracking state percolator backstops it. Either way the depositor is capped to their
// own principal — this test verifies that against the real binaries (no underflow, co-depositor safe).
#[test]
fn cannot_withdraw_more_than_your_own_recorded_principal() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let (bob, bob_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let a_hold = create_holding(&mut env, &pool);
    let b_hold = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &a_hold, amount).expect("alice deposit");
    env.insurance_deposit(&bob, &bob_ata, &b_hold, amount).expect("bob deposit");
    assert_eq!(env.token_amount(&env.perc_vault.clone()), 2 * amount, "both principals in insurance");
    assert_eq!(env.pool_outstanding(), 2 * amount);

    // ATTACK: alice withdraws 2M (the whole pool) — more than her own 1M — which would drain bob.
    assert!(
        env.insurance_withdraw(&alice, &alice_ata, &a_hold, &alice, 2 * amount).is_err(),
        "withdraw must reject amount > the caller's own recorded principal"
    );
    assert_eq!(env.token_amount(&env.perc_vault.clone()), 2 * amount, "insurance untouched — bob's principal safe");
    let (a_principal, _, a_withdrawn) = env.read_position(&alice.pubkey());
    assert_eq!(a_principal, amount, "alice's recorded principal intact (no underflow)");
    assert!(!a_withdrawn, "alice's position not retired by the failed over-withdraw");

    // Alice can still exit exactly her OWN 1M; bob's 1M stays outstanding.
    env.insurance_withdraw(&alice, &alice_ata, &a_hold, &alice, amount).expect("alice exits her own principal");
    assert_eq!(env.token_amount(&alice_ata), amount, "alice got exactly her own 1M, never bob's");
    assert_eq!(env.pool_outstanding(), amount, "bob's 1M still outstanding");
}

// LAMPORT PRE-FUND INIT-DOS (finding AI): every init handler creates its PDA with the System
// `create_account`, which FAILS with AccountAlreadyInUse if the destination already holds ANY
// lamports — and the handlers additionally guard `lamports() != 0 -> AlreadyInitialized`. An attacker
// can transfer 1 lamport to the deterministic pool PDA (a transfer needs NO destination signature)
// BEFORE the genesis init, permanently bricking init_insurance_pool — and with it the whole genesis,
// since the lamports can never be swept (no one can sign for a system-owned PDA, and the legit init
// keeps rejecting). The robust create (top-up the rent shortfall, then allocate + assign via
// invoke_signed) tolerates the pre-funding because allocate/assign only require data-empty +
// system-owned, not zero lamports. This test dusts the PDA and asserts init STILL succeeds.
#[test]
fn lamport_prefund_cannot_brick_insurance_pool_init() {
    let mut env = Env::new();
    env.svm.set_account(env.pool, Account {
        lamports: 1, // attacker dust
        data: vec![],
        owner: solana_sdk::system_program::ID,
        executable: false,
        rent_epoch: 0,
    }).unwrap();
    env.init_insurance_pool(); // must still succeed (robust create handles the pre-funded PDA)
    let acc = env.svm.get_account(&env.pool).unwrap();
    assert_eq!(acc.owner, sub_id(), "pool created + owned by subledger despite the dust");
    assert!(acc.data.len() >= 88, "pool data initialized");
}

// RE-INIT PROTECTION (regression guard for finding AI): the finding-AI fix relaxed the init guard
// from `lamports() != 0 || data_len() != 0` to `data_len() != 0` so a dusted-but-empty PDA can still
// be created. This must NOT weaken re-init protection — an already-initialized pool has data, so a
// second init_insurance_pool on the same PDA must still be rejected. Otherwise an attacker could
// re-init a LIVE pool and reset pool.outstanding_principal (the genesis quorum denominator) to 0, or
// re-point its vault/policy — a state-reset governance/LOF attack. Previously untested stack-wide.
#[test]
fn insurance_pool_cannot_be_reinitialized_after_funding() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let pool = env.pool;
    let (alice, alice_ata) = new_depositor(&mut env, 1_000_000);
    let hold = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &hold, 1_000_000).expect("deposit");
    assert_eq!(env.pool_outstanding(), 1_000_000, "pool has live outstanding");

    // ATTACK: re-init the SAME pool PDA (would zero outstanding / re-point bindings if it succeeded).
    let mut data = vec![3u8]; // IX_INIT_INSURANCE_POOL
    data.extend_from_slice(&ASSET_ID.to_le_bytes());
    data.push(POLICY_PRINCIPAL);
    let reinit = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(env.mint, false),
            AccountMeta::new(pool, false),
            AccountMeta::new_readonly(env.perc_vault, false),
            AccountMeta::new_readonly(env.slab, false),
            AccountMeta::new_readonly(perc_id(), false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(gv_config_pda(&env.coin_mint, &pool), false),
        ],
        data,
    };
    assert!(env.send(&[reinit], &[]).is_err(), "re-init of a live pool must be rejected (data_len guard)");
    assert_eq!(env.pool_outstanding(), 1_000_000, "outstanding (quorum denominator) untouched by the blocked re-init");
}

// DEPOSIT MARKET-BINDING (Sybil-resistance core): vote weight must be backed by capital genuinely at
// risk in the GENESIS market. insurance_deposit credits position.principal (which becomes vote weight)
// and CPIs TopUpInsurance to move the capital into the pool's bound market. If a depositor could pass
// a FOREIGN market_slab (one they control) while depositing to the genesis pool, they'd get a credited
// position while routing capital somewhere they can reclaim it — free governance power, defeating the
// whole Sybil check. deposit pins market_slab == pool.market_slab (+ vault + program). Distinct code
// path from the withdraw foreign-slab pin (finding AF): a regression dropping the deposit pin would
// NOT be caught by AF. Previously untested.
#[test]
fn deposit_with_foreign_market_slab_credits_no_position() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let pool = env.pool;
    let (attacker, atk_ata) = new_depositor(&mut env, 1_000_000);
    let hold = create_holding(&mut env, &pool);

    // A DIFFERENT live market the attacker would rather route capital to (clone at a fresh address).
    let foreign_slab = Pubkey::new_unique();
    let fs = env.svm.get_account(&env.slab).unwrap();
    env.svm.set_account(foreign_slab, fs).unwrap();

    // ATTACK: deposit to the genesis pool but point market_slab at the foreign market.
    let mut data = vec![4u8];
    data.extend_from_slice(&1_000_000u64.to_le_bytes());
    let attack = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(attacker.pubkey(), true),
            AccountMeta::new(pool, false),
            AccountMeta::new(env.position_pda(&attacker.pubkey()), false),
            AccountMeta::new(atk_ata, false),
            AccountMeta::new(hold, false),
            AccountMeta::new(foreign_slab, false), // <-- substituted market
            AccountMeta::new(env.perc_vault, false),
            AccountMeta::new_readonly(perc_id(), false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    };
    assert!(env.send(&[attack], &[&attacker]).is_err(), "deposit with a foreign market_slab must be rejected");
    let pos = env.svm.get_account(&env.position_pda(&attacker.pubkey()));
    assert!(pos.is_none() || pos.unwrap().data.is_empty(), "no credited position from the blocked deposit");
    assert_eq!(env.pool_outstanding(), 0, "no free vote weight credited");
    assert_eq!(env.token_amount(&atk_ata), 1_000_000, "attacker's capital untouched");
}

// CROSS-MARKET HAIRCUT-BASIS SUBSTITUTION (LOF): the pro-rata exit reads the live insurance basis
// from the passed market_slab (findings L + T). If a depositor in an IMPAIRED pool could pass a
// DIFFERENT, HEALTHY market's slab, payout() would read that market's full insurance and treat the
// exit as un-impaired — paying FULL principal while the pull still drains the real (impaired) market,
// stealing the loss-share owed to the remaining depositors. Defense: withdraw pins
// market_slab == pool.market_slab (subledger/src/lib.rs) BEFORE it reads insurance or signs the
// WithdrawInsuranceLimited pull. Symmetric to the twap's
// e2e_execute_rejects_foreign_market_vault_authority; previously untested on the subledger side.
#[test]
fn foreign_market_slab_cannot_inflate_the_haircut() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let a_hold = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &a_hold, amount).expect("alice deposit");

    // A DIFFERENT, HEALTHY market (2M insurance) — the bait the attacker wants payout() to read.
    let foreign_slab = Pubkey::new_unique();
    let mut fs = env.svm.get_account(&env.slab).unwrap();
    let off_ins = MARKET_GROUP_OFF + 301;
    fs.data[off_ins..off_ins + 16].copy_from_slice(&2_000_000u128.to_le_bytes());
    env.svm.set_account(foreign_slab, fs).unwrap();

    // Impair the REAL market to 50%: an honest exit owes only 500k (insurance 500k / outstanding 1M).
    impair_market(&mut env, 500_000u128);
    env.svm.set_account(env.perc_vault, Account {
        lamports: 1_000_000,
        data: token_account_data(&env.mint, &env.vault_authority, 500_000),
        owner: spl_token::ID, executable: false, rent_epoch: 0,
    }).unwrap();

    // ATTACK: withdraw with market_slab pointing at the HEALTHY foreign market to read its 2M basis.
    let mut d = vec![5u8]; d.extend_from_slice(&amount.to_le_bytes());
    let attack = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(alice.pubkey(), true),
            AccountMeta::new(pool, false),
            AccountMeta::new(env.position_pda(&alice.pubkey()), false),
            AccountMeta::new(alice_ata, false),
            AccountMeta::new(a_hold, false),
            AccountMeta::new(foreign_slab, false), // <-- substituted slab
            AccountMeta::new(env.perc_vault, false),
            AccountMeta::new_readonly(env.vault_authority, false),
            AccountMeta::new_readonly(perc_id(), false),
            AccountMeta::new_readonly(spl_token::ID, false),
        ],
        data: d,
    };
    assert!(env.send(&[attack], &[&alice]).is_err(), "a foreign market_slab must be rejected (key != pool.market_slab)");
    let (_, _, withdrawn) = env.read_position(&alice.pubkey());
    assert!(!withdrawn, "the position is untouched after the blocked attack");
    assert_eq!(env.token_amount(&alice_ata), 0, "no funds extracted via the foreign slab");

    // The honest exit (real slab) pays only the 50% haircut — the foreign slab bought no advantage.
    env.insurance_withdraw(&alice, &alice_ata, &a_hold, &alice, amount).expect("honest exit");
    assert_eq!(env.token_amount(&alice_ata), 500_000, "honest pro-rata haircut is 500k, never the 1M a healthy basis would pay");
}

// Full genesis lifecycle with ALL real programs (percolator + subledger +
// genesis-vote + distribution): a depositor puts collateral at risk in percolator
// insurance, votes, the permissionless trigger seals the winning distribution by CPI,
// and the winning recipient CLAIMS the fixed-supply COIN. Pins that the whole chain
// produces a claimable distribution end-to-end (a broken link here bricks the genesis).
#[test]
fn full_lifecycle_deposit_vote_seal_then_recipient_claims_coin() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);

    // The depositor (voter) and a separate COIN recipient named by the proposal.
    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("collateral deposit");

    let recipient = Keypair::new();
    let recipient_coin_ata =
        create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.coin_mint, &recipient.pubkey());

    // Proposal allocates the full COIN supply (100) to the recipient.
    let (dist_proposal, gv_proposal) = create_and_register_proposal(&mut env, &ve, 1, &recipient.pubkey());

    // Vote it to quorum + majority, then permissionlessly trigger the seal.
    env.warp_slot(1124);
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("vote");
    gv_trigger(&mut env, &ve, &gv_proposal, &dist_proposal).expect("trigger seals the distribution");

    // The recipient claims their COIN from the sealed distribution.
    assert_eq!(env.token_amount(&recipient_coin_ata), 0, "nothing before claim");
    let mut data = vec![4u8]; // IX_CLAIM
    data.extend_from_slice(&0u32.to_le_bytes()); // index 0
    let claim = Instruction {
        program_id: dist_id(),
        accounts: vec![
            AccountMeta::new_readonly(recipient.pubkey(), true),
            AccountMeta::new_readonly(ve.dist_config, false),
            AccountMeta::new(dist_proposal, false),
            AccountMeta::new(ve.coin_vault, false),
            AccountMeta::new(recipient_coin_ata, false),
            AccountMeta::new_readonly(spl_token::ID, false),
        ],
        data,
    };
    env.send(&[claim], &[&recipient]).expect("recipient claims the COIN");
    assert_eq!(env.token_amount(&recipient_coin_ata), 100, "winner received the full COIN pool");

    // Re-claiming the same entry is refused (entry zeroed).
    let mut data = vec![4u8];
    data.extend_from_slice(&0u32.to_le_bytes());
    let reclaim = Instruction {
        program_id: dist_id(),
        accounts: vec![
            AccountMeta::new_readonly(recipient.pubkey(), true),
            AccountMeta::new_readonly(ve.dist_config, false),
            AccountMeta::new(dist_proposal, false),
            AccountMeta::new(ve.coin_vault, false),
            AccountMeta::new(recipient_coin_ata, false),
            AccountMeta::new_readonly(spl_token::ID, false),
        ],
        data,
    };
    assert!(env.send(&[reclaim], &[&recipient]).is_err(), "cannot double-claim");
}

// Anti bait-and-switch: a creator must not be able to change the distribution after
// voters have backed it. Build a PARTIAL proposal (room to append), register + vote
// it, then append a self-allocation — the trigger must REFUSE to seal the changed
// proposal (its entry_count/total_amount snapshot no longer matches).
#[test]
fn proposal_changed_after_registration_cannot_be_sealed() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);
    let dist_config = ve.dist_config;

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("deposit");

    let id = 1u64;
    let dist_proposal =
        Pubkey::find_program_address(&[b"dist_proposal", dist_config.as_ref(), &id.to_le_bytes()], &dist_id()).0;
    let mut cd = vec![1u8];
    cd.extend_from_slice(&id.to_le_bytes());
    cd.extend_from_slice(&4u32.to_le_bytes());
    env.send(&[Instruction {
        program_id: dist_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(dist_config, false),
            AccountMeta::new(dist_proposal, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: cd,
    }], &[]).expect("create proposal");

    let append = |env: &mut Env, dest: &Pubkey, amt: u64| -> Result<(), String> {
        let mut ad = vec![2u8];
        ad.extend_from_slice(&1u32.to_le_bytes());
        ad.extend_from_slice(dest.as_ref());
        ad.extend_from_slice(&amt.to_le_bytes());
        env.send(&[Instruction {
            program_id: dist_id(),
            accounts: vec![
                AccountMeta::new(env.payer.pubkey(), true),
                AccountMeta::new_readonly(dist_config, false),
                AccountMeta::new(dist_proposal, false),
            ],
            data: ad,
        }], &[])
    };
    // A fair partial allocation (40 of 100): leaves room to append later.
    let fair = Pubkey::new_unique();
    append(&mut env, &fair, 40).expect("append fair entry");

    // Register the gv proposal — snapshots (entry_count=1, total_amount=40).
    let gv_proposal =
        Pubkey::find_program_address(&[b"gv_proposal", ve.gv_config.as_ref(), dist_proposal.as_ref()], &gv_id()).0;
    env.send(&[Instruction {
        program_id: gv_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(ve.gv_config, false),
            AccountMeta::new(gv_proposal, false),
            AccountMeta::new_readonly(dist_proposal, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: vec![2u8],
    }], &[]).expect("register");

    // Voters back it to quorum + majority.
    env.warp_slot(1124);
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("vote");

    // ATTACK: the creator appends a self-allocation AFTER voters committed.
    let attacker = Pubkey::new_unique();
    append(&mut env, &attacker, 60).expect("creator can still append (no dist-level lock)");

    // The trigger must refuse to seal the changed proposal.
    assert!(
        gv_trigger(&mut env, &ve, &gv_proposal, &dist_proposal).is_err(),
        "trigger must reject a proposal changed after registration"
    );
}

#[test]
fn genesis_vote_reads_subledger_position_and_weights() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("deposit");
    // deposit at slot 100.

    let dest = Pubkey::new_unique();
    let (_dist_proposal, gv_proposal) = create_and_register_proposal(&mut env, &ve, 1, &dest);

    // Advance the clock so hold = 1124 - 100 = 1024 -> floor(log2(1024)) = 10.
    env.warp_slot(1124);
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("vote backs proposal");

    let (support_weight, support_principal) = gv_proposal_support(&env, &gv_proposal);
    assert_eq!(support_principal, amount);
    // weight = floor(log2(hold)) * principal = 10 * 1_000_000.
    assert_eq!(support_weight, 10 * amount, "weight = floor(log2(hold)) * principal");
}

// REGRESSION for finding GG (FIXED): weight-tally overflow -> vote-freeze DOS. vote_weight =
// floor(log2(hold)) * principal; the weighted sum Σ(mᵢ·Pᵢ) can legitimately exceed u64::MAX (up to ~30·Σ
// principal on a large-supply collateral). With the OLD u64 total_cast_weight, once a saturating whale pushed
// it to u64::MAX, EVERY later vote's checked_add overflowed and was REJECTED — a sub-majority whale could
// freeze the tally and block all honest voters (genesis bricked); naive saturation would instead let a
// minority pass the support*2 > total_cast majority. FIX: total_cast_weight / support_weight / voted_weight +
// vote_weight are now u128. This pins it WITHOUT impossible deposits: inject total_cast_weight = u64::MAX (the
// old overflow boundary) and confirm a real vote still lands and the tally grows PAST u64::MAX.
#[test]
fn a_high_cast_weight_tally_does_not_overflow_and_block_honest_votes() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);

    let amount = 1_000_000u64;
    let (alice, a_ata) = new_depositor(&mut env, amount);
    env.insurance_deposit(&alice, &a_ata, &holding, amount).expect("deposit");
    let dest = Pubkey::new_unique();
    let (_dp, gv_prop) = create_and_register_proposal(&mut env, &ve, 1, &dest);
    env.warp_slot(1124); // hold ~1024 -> m = 10

    // Inject total_cast_weight = u64::MAX — exactly where the OLD u64 tally would overflow on the next vote's
    // checked_add and reject it (the freeze point). With u128 it is far from the ceiling.
    let mut cfg = env.svm.get_account(&ve.gv_config).unwrap();
    cfg.data[208..224].copy_from_slice(&(u64::MAX as u128).to_le_bytes());
    env.svm.set_account(ve.gv_config, cfg).unwrap();

    // Alice's vote adds ~10*amount weight ON TOP of u64::MAX — must NOT be blocked by overflow.
    gv_vote(&mut env, &ve, &alice, &gv_prop, 1)
        .expect("an honest vote must not be blocked by a near-u64::MAX cast-weight tally (GG fix)");

    // The tally grew PAST u64::MAX (u128) instead of overflowing/rejecting.
    let cast = u128::from_le_bytes(
        env.svm.get_account(&ve.gv_config).unwrap().data[208..224].try_into().unwrap());
    assert!(cast > u64::MAX as u128, "total_cast_weight exceeded u64::MAX without overflow (= {})", cast);
}

// FORGED-POSITION SUPPLY THEFT (the single highest-stakes vector): `vote` reads (principal, start_slot)
// from the subledger position to compute weight = floor(log2(hold))*principal. If an attacker could feed a
// FORGED position with a u64::MAX principal, they'd mint themselves an astronomical weight + principal,
// single-handedly clear quorum + majority, and TRIGGER winner-take-all to seize 100% of the COIN supply.
// Two layered guards block this and BOTH were previously uncovered (every other vote test uses a real,
// correctly-owned position, so neither guard was ever exercised against a forgery):
//   (a) owner check  — `sub_position.owner == config.subledger_program` (lib.rs:559): the position must be
//       owned by the real subledger program; an attacker-fabricated account (any other owner) is refused.
//   (b) PDA key bind — `sub_position.key == PDA(["subledger_position", pool, voter], subledger)` (:569):
//       even a subledger-owned account is refused unless it sits at the one canonical address for (pool,voter).
// This test forges BOTH ways against a real vote and asserts the weight is NOT inflated and the bid to seize
// the supply via trigger FAILS. Mutation-sharp: case (a) fails if :559 is dropped; case (b) if :569 is dropped.
#[test]
fn a_forged_subledger_position_cannot_fabricate_vote_weight_to_steal_the_supply() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);

    // Alice is a real depositor with a TINY principal (1 atom) — far below any quorum on her own.
    let (alice, alice_ata) = new_depositor(&mut env, 1);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, 1).expect("deposit");
    let dest = Pubkey::new_unique();
    let (dist_proposal, gv_proposal) = create_and_register_proposal(&mut env, &ve, 1, &dest);
    env.warp_slot(1124);

    let pos_pda = env.position_pda(&alice.pubkey());
    let real = env.svm.get_account(&pos_pda).unwrap();

    // ---- ATTACK (a): canonical PDA address, but flip the OWNER to a non-subledger program and forge a
    //      u64::MAX principal. Only the owner check (:559) stands between this and a fabricated weight.
    {
        let mut forged = real.clone();
        forged.owner = Pubkey::new_unique(); // NOT the subledger program
        forged.data[72..80].copy_from_slice(&u64::MAX.to_le_bytes()); // colossal principal
        env.svm.set_account(pos_pda, forged).unwrap();
        assert!(
            gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).is_err(),
            "a non-subledger-owned position must be refused (owner check, :559)"
        );
        let (w, p) = gv_proposal_support(&env, &gv_proposal);
        assert_eq!((w, p), (0, 0), "no weight may be credited from a forged-owner position");
    }

    // ---- ATTACK (b): a genuinely subledger-OWNED account carrying the same forged u64::MAX principal, but
    //      sitting at a WRONG address (not the (pool,voter) PDA). Only the key binding (:569) refuses it.
    {
        let mut forged = real.clone();
        forged.data[72..80].copy_from_slice(&u64::MAX.to_le_bytes());
        let wrong_key = Pubkey::new_unique();
        env.svm.set_account(wrong_key, forged).unwrap(); // owner stays = subledger program
        let gv_ballot = Pubkey::find_program_address(
            &[b"gv_ballot", ve.gv_config.as_ref(), alice.pubkey().as_ref()], &gv_id()).0;
        let ix = Instruction {
            program_id: gv_id(),
            accounts: vec![
                AccountMeta::new(alice.pubkey(), true),
                AccountMeta::new(ve.gv_config, false),
                AccountMeta::new(gv_ballot, false),
                AccountMeta::new(gv_proposal, false),
                AccountMeta::new(wrong_key, false), // <- substituted position at a non-canonical key
                AccountMeta::new_readonly(env.pool, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
                AccountMeta::new_readonly(sub_id(), false),
            ],
            data: vec![3u8, 1u8],
        };
        assert!(env.send(&[ix], &[&alice]).is_err(),
            "a position at a non-canonical address must be refused (PDA key bind, :569)");
        let (w, p) = gv_proposal_support(&env, &gv_proposal);
        assert_eq!((w, p), (0, 0), "no weight may be credited from a wrong-key position");
    }

    // The seizure attempt is dead: no quorum was ever fabricated, so trigger cannot mint the supply.
    assert!(gv_trigger(&mut env, &ve, &gv_proposal, &dist_proposal).is_err(),
        "with no fabricated weight, the winner-take-all trigger must fail");
}

// SUBSTITUTED-POOL FAKE QUORUM (sibling of the forged-position vector): `trigger` measures quorum as
// `total_voted_principal*2 > live_outstanding`, reading `outstanding` LIVE from the subledger pool. If an
// attacker could feed a pool reporting a tiny outstanding, a MINORITY voter would clear quorum and seize
// 100% of the COIN supply. The pool is bound by owner AND key to config.subledger_pool (lib.rs:738); the KEY
// bind is the sole guard, because the owner check alone is insufficient — anyone can permissionlessly init
// their own EMPTY subledger pool (also subledger-owned). Previously uncovered: every trigger test passed the
// canonical pool, so mutating the key bind left seal (14) + integration (40) green. This pins it.
#[test]
fn trigger_with_a_substituted_low_outstanding_pool_cannot_fake_quorum_to_steal_the_supply() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);

    // Alice holds a tiny principal and votes; Bob holds a large principal and does NOT vote. So the voted
    // principal (alice) is a strict minority of the live outstanding (alice+bob): quorum is NOT met.
    let (alice, alice_ata) = new_depositor(&mut env, 1);
    env.insurance_deposit(&alice, &alice_ata, &holding, 1).expect("alice deposit");
    let (bob, bob_ata) = new_depositor(&mut env, 1_000);
    env.insurance_deposit(&bob, &bob_ata, &holding, 1_000).expect("bob deposit");

    let dest = Pubkey::new_unique();
    let (dist_proposal, gv_proposal) = create_and_register_proposal(&mut env, &ve, 1, &dest);
    env.warp_slot(1124);
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("alice votes (minority)");

    // SANITY: against the REAL pool, quorum legitimately fails (2*1 <= 1001) — the trigger is blocked.
    assert!(gv_trigger(&mut env, &ve, &gv_proposal, &dist_proposal).is_err(),
        "a minority cannot trigger against the real pool (no quorum)");

    // ATTACK: forge a subledger-OWNED pool byte-identical to the real one but with outstanding_principal = 0,
    // at a NON-canonical key. If trigger read THIS pool, quorum = 2*1 > 0 -> PASS and the minority seizes the
    // whole supply. The key binding to config.subledger_pool (:738) is the sole guard.
    let mut fake = env.svm.get_account(&pool).unwrap();        // real pool: subledger-owned, valid disc
    fake.data[80..88].copy_from_slice(&0u64.to_le_bytes());    // zero the outstanding denominator
    let fake_pool = Pubkey::new_unique();
    env.svm.set_account(fake_pool, fake).unwrap();             // owner stays = subledger program

    let ix = Instruction {
        program_id: gv_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new(ve.gv_config, false),
            AccountMeta::new(gv_proposal, false),
            AccountMeta::new_readonly(dist_id(), false),
            AccountMeta::new(ve.dist_config, false),
            AccountMeta::new(dist_proposal, false),
            AccountMeta::new_readonly(fake_pool, false),        // <- substituted empty pool
        ],
        data: vec![4u8],
    };
    assert!(env.send(&[ix], &[]).is_err(),
        "a substituted low-outstanding pool must be refused (pool key bind, :738)");

    // The proposal was never sealed (executed @ offset 88 stays 0) — the supply seizure failed.
    let pv = env.svm.get_account(&gv_proposal).unwrap();
    assert_eq!(pv.data[96], 0, "proposal must not be marked executed by the foiled attack"); // GG: executed @96
}

// RE-VOTE WEIGHT INFLATION (no sibling backstop — pure gv accounting): `vote` backs out the ballot's prior
// contribution from BOTH the proposal's support_weight/principal AND the global total_cast/total_voted
// BEFORE re-adding the fresh weight (lib.rs:618-622). Without that backout, a voter could call vote N times
// on the SAME proposal and have their weight (and principal) counted N times — inflating the proposal's
// majority share and the quorum numerator with one position's capital. Unlike the over-withdraw cap there is
// NO percolator/other backstop here: the tallies are gv-owned state, so this backout is the SOLE guard.
#[test]
fn re_voting_the_same_proposal_does_not_double_count_weight() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);
    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("deposit");
    let dest = Pubkey::new_unique();
    let (_dist, gv_proposal) = create_and_register_proposal(&mut env, &ve, 1, &dest);
    env.warp_slot(1124); // hold age 1024 -> floor(log2)=10 -> weight 10*principal

    let read_cast = |env: &Env| -> u64 {
        u128::from_le_bytes(env.svm.get_account(&ve.gv_config).unwrap().data[208..224].try_into().unwrap()) as u64
    };

    // First vote: weight counted once.
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("first vote");
    assert_eq!(gv_proposal_support(&env, &gv_proposal), (10 * amount, amount), "one vote, weight once");
    assert_eq!(read_cast(&env), 10 * amount, "global cast = one vote");

    // ATTACK: vote AGAIN on the SAME proposal. The backout removes the prior contribution before re-adding,
    // so the tallies stay at exactly one vote — they must NOT double.
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("re-vote accepted (idempotent)");
    assert_eq!(gv_proposal_support(&env, &gv_proposal), (10 * amount, amount),
        "re-vote must NOT double the proposal's weight/principal");
    assert_eq!(read_cast(&env), 10 * amount, "re-vote must NOT double the global cast weight");

    // Even a third time stays put.
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("third vote");
    assert_eq!(gv_proposal_support(&env, &gv_proposal).0, 10 * amount, "weight still counted exactly once");
    assert_eq!(read_cast(&env), 10 * amount, "global cast still exactly one vote");
}

// ONE-VOTE-ONE-PROPOSAL (cross-proposal phantom inflation): a voter with a live ballot on proposal A must
// RETRACT before backing proposal B (lib.rs:612). It is subtler than the same-proposal double-count: the
// re-vote backout subtracts ballot.voted_weight from the PASSED proposal, so backing a DIFFERENT proposal B
// would subtract A's weight from B (corrupting B, or underflowing if B is empty) while leaving A's tally
// untouched — a PHANTOM weight stranded on A that no live ballot backs, inflating A's majority share. The
// guard is line 612. Single-guard, no backstop (pure gv tallies). Pre-fund B with bob's equal vote so the
// mutation path (remove 612) does NOT underflow — the corruption is the sharp signal.
#[test]
fn cannot_back_a_second_proposal_without_retracting_the_first() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);
    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let (bob, bob_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let a_hold = create_holding(&mut env, &pool);
    let b_hold = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &a_hold, amount).expect("alice deposit");
    env.insurance_deposit(&bob, &bob_ata, &b_hold, amount).expect("bob deposit");
    let (_da, gv_a) = create_and_register_proposal(&mut env, &ve, 1, &Pubkey::new_unique());
    let (_db, gv_b) = create_and_register_proposal(&mut env, &ve, 2, &Pubkey::new_unique());
    env.warp_slot(1124); // both weights = 10*principal
    let read_cast = |env: &Env| -> u64 {
        u128::from_le_bytes(env.svm.get_account(&ve.gv_config).unwrap().data[208..224].try_into().unwrap()) as u64
    };

    // bob backs B (so B already has support == alice's weight; without the guard the backout from B would
    // not underflow). alice backs A.
    gv_vote(&mut env, &ve, &bob, &gv_b, 1).expect("bob backs B");
    gv_vote(&mut env, &ve, &alice, &gv_a, 1).expect("alice backs A");
    assert_eq!(gv_proposal_support(&env, &gv_a), (10 * amount, amount), "A has alice's vote");
    assert_eq!(gv_proposal_support(&env, &gv_b), (10 * amount, amount), "B has bob's vote");

    // ATTACK: alice (live on A) backs B WITHOUT retracting A. Must be refused — else her weight would be
    // double-represented across A and B.
    assert!(
        gv_vote(&mut env, &ve, &alice, &gv_b, 1).is_err(),
        "a voter with a live ballot on A must retract before backing B"
    );
    assert_eq!(gv_proposal_support(&env, &gv_a), (10 * amount, amount), "A's tally intact — no phantom left behind");
    assert_eq!(gv_proposal_support(&env, &gv_b), (10 * amount, amount), "B's tally unchanged by the rejected cross-vote");
    assert_eq!(read_cast(&env), 20 * amount, "global cast = exactly the two real votes");

    // The LEGIT switch works: alice retracts A, then backs B. A -> 0, B -> alice + bob.
    gv_vote(&mut env, &ve, &alice, &gv_a, 2).expect("alice retracts A");
    gv_vote(&mut env, &ve, &alice, &gv_b, 1).expect("alice now backs B");
    assert_eq!(gv_proposal_support(&env, &gv_a), (0, 0), "A fully released after retract");
    assert_eq!(gv_proposal_support(&env, &gv_b), (20 * amount, 2 * amount), "B now holds both votes");
    assert_eq!(read_cast(&env), 20 * amount, "global cast still exactly two votes — never inflated");
}

// DEPOSIT != VOTE (top-up while a ballot is LIVE must not inflate the tally nor unlock the pledge):
// insurance_deposit checks p.withdrawn but NOT vote_locked, and it never touches the gv tallies (those are
// gv-owned state). So a voter may add capital while voted, but doing so must (a) NOT silently raise their
// counted weight/principal — that would inject vote power bypassing vote's weight/age/backout path — and
// (b) NOT unlock the at-risk capital. The only way to count fresh capital is a re-vote, which resets the age
// clock (top_up_resets... pins that). This interaction (deposit x live ballot) was untested.
#[test]
fn topping_up_a_voted_position_does_not_inflate_or_unlock_the_vote() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);
    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, 2 * amount); // funds for the first deposit + a top-up
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("first deposit");
    let (_d, gv_proposal) = create_and_register_proposal(&mut env, &ve, 1, &Pubkey::new_unique());
    env.warp_slot(1124); // weight 10*principal
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("vote");
    assert_eq!(gv_proposal_support(&env, &gv_proposal), (10 * amount, amount), "vote counted once");
    assert!(env.svm.get_account(&env.position_pda(&alice.pubkey())).unwrap().data[97] == 1, "position vote-locked");

    // TOP UP while the ballot is live (deposit ignores the lock) — allowed (no DOS on adding capital).
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("top-up while voted is allowed");

    // (a) The tally is UNCHANGED — the extra capital is NOT counted until a re-vote.
    assert_eq!(gv_proposal_support(&env, &gv_proposal), (10 * amount, amount),
        "top-up must NOT inflate the live vote — deposit is not a vote");
    // (b) The position grew but stays LOCKED: the pledged capital can't be exited without retracting.
    let (principal, _start, withdrawn) = env.read_position(&alice.pubkey());
    assert_eq!(principal, 2 * amount, "top-up landed (principal grew)");
    assert!(!withdrawn);
    assert!(env.svm.get_account(&env.position_pda(&alice.pubkey())).unwrap().data[97] == 1,
        "top-up did NOT unlock the live vote — capital still pledged");
    assert!(env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, amount).is_err(),
        "the topped-up, still-voted position cannot exit until the vote is retracted");
}

// TARGETED DISENFRANCHISEMENT (lamport-prefund DOS on a voter's ballot, finding AI on the vote path):
// the ballot PDA is f(gv_config, voter) — fully deterministic from a public voter key — and `vote`
// lazily creates it on the first back. If that creation used the System `create_account` (which aborts
// with AccountAlreadyInUse on ANY pre-existing lamports), an attacker could transfer 1 lamport (no
// signature needed) to a target voter's ballot PDA and PERMANENTLY block that specific voter from ever
// casting a ballot — silencing a large holder to swing the genesis. gv's create_pda is robust (top up
// the rent shortfall, then allocate + assign via invoke_signed, which only need data-empty +
// system-owned), so the dusted ballot still gets created and the vote lands. The existing prefund test
// covers the gv CONFIG account; this pins the per-voter BALLOT path.
#[test]
fn dusting_a_voters_ballot_pda_cannot_block_their_vote() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("deposit");

    let dest = Pubkey::new_unique();
    let (_dist_proposal, gv_proposal) = create_and_register_proposal(&mut env, &ve, 1, &dest);

    // ATTACK: dust alice's deterministic ballot PDA with 1 lamport before she ever votes.
    let ballot = Pubkey::find_program_address(
        &[b"gv_ballot", ve.gv_config.as_ref(), alice.pubkey().as_ref()],
        &gv_id(),
    ).0;
    env.svm.set_account(ballot, Account {
        lamports: 1, // attacker dust, system-owned + empty
        data: vec![],
        owner: solana_sdk::system_program::ID,
        executable: false,
        rent_epoch: 0,
    }).unwrap();

    // The vote STILL lands — the robust create absorbs the dust instead of aborting.
    env.warp_slot(1124);
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("vote lands despite the dusted ballot PDA");

    let ballot_acc = env.svm.get_account(&ballot).unwrap();
    assert_eq!(ballot_acc.owner, gv_id(), "ballot created + owned by genesis-vote despite the dust");
    let (support_weight, support_principal) = gv_proposal_support(&env, &gv_proposal);
    assert_eq!(support_principal, amount, "alice's principal counts");
    assert_eq!(support_weight, 10 * amount, "alice's weight counts — she was not silenced");
}

// FINDING-AI for the POSITION PDA (exclusion DOS, higher-stakes than the ballot): the subledger position
// PDA ["subledger_position", pool, owner] is deterministic, so an attacker can DUST a victim's position with
// lamports BEFORE they ever deposit. A naive create_account fails on a prefunded account -> the victim could
// NEVER open a position -> totally excluded from the genesis (no capital at risk, so no vote, no weight, no
// claim). The ballot-dust case is pinned (dusting_a_voters_ballot_pda_cannot_block_their_vote) but the
// position — the FIRST gate, before voting even matters — was not. This pins create_pda_robust on the deposit
// path: a dusted position PDA is absorbed (top up the deficit + allocate/assign), and the deposit still lands.
#[test]
fn dusting_a_depositors_position_pda_cannot_block_their_deposit() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);

    // ATTACK: dust alice's deterministic position PDA with 1 lamport BEFORE she deposits.
    let position = env.position_pda(&alice.pubkey());
    env.svm.set_account(position, Account {
        lamports: 1, // attacker dust, system-owned + empty
        data: vec![],
        owner: solana_sdk::system_program::ID,
        executable: false,
        rent_epoch: 0,
    }).unwrap();

    // The deposit STILL lands — the robust create absorbs the dust instead of aborting.
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("deposit lands despite the dusted position PDA");

    let pos = env.svm.get_account(&position).unwrap();
    assert_eq!(pos.owner, sub_id(), "position created + owned by subledger despite the dust");
    let principal = u64::from_le_bytes(pos.data[72..80].try_into().unwrap());
    assert_eq!(principal, amount, "alice's full principal is recorded — she was not excluded");
}

// Finding B (vote-outlives-capital): a live genesis ballot must keep its principal
// at risk. Before the fix, a voter could vote (recording a principal/weight snapshot)
// then insurance-withdraw their capital, leaving a free, capital-less ballot that
// still counted toward quorum/majority — worse after the live-outstanding fix, since
// withdrawing shrinks the denominator while the snapshot numerator stays. Now the
// genesis-vote CPIs the subledger to lock the position while the ballot is live;
// withdraw is refused until the voter retracts (which clears the lock).
#[test]
fn vote_locked_principal_cannot_exit_until_retracted() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("deposit");

    let dest = Pubkey::new_unique();
    let (_dist_proposal, gv_proposal) = create_and_register_proposal(&mut env, &ve, 1, &dest);

    let vote_locked = |env: &Env| -> bool {
        env.svm.get_account(&env.position_pda(&alice.pubkey())).unwrap().data[97] == 1
    };

    // Before voting: not locked, and a withdraw would be allowed.
    assert!(!vote_locked(&env), "fresh position is not vote-locked");

    // Vote → the genesis-vote CPI locks the position.
    env.warp_slot(1124);
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("vote backs proposal");
    assert!(vote_locked(&env), "voting locks the principal");

    // The attack: try to withdraw the capital while the ballot is still live.
    let err = env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, amount);
    assert!(err.is_err(), "vote-locked principal cannot be withdrawn");
    // Funds stayed in insurance; the position is intact.
    assert_eq!(env.token_amount(&env.perc_vault.clone()), amount, "capital still at risk");
    let (principal, _s, withdrawn) = env.read_position(&alice.pubkey());
    assert_eq!(principal, amount);
    assert!(!withdrawn);

    // Retract → the CPI clears the lock; the ballot's principal/weight is removed.
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 2).expect("retract");
    assert!(!vote_locked(&env), "retract clears the lock");
    let (support_weight, support_principal) = gv_proposal_support(&env, &gv_proposal);
    assert_eq!(support_weight, 0, "retract removes the ballot's weight");
    assert_eq!(support_principal, 0, "retract removes the ballot's principal");

    // Now the exit succeeds: capital can only leave once it no longer backs a vote.
    env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, amount).expect("exit after retract");
    assert_eq!(env.token_amount(&alice_ata), amount, "principal returned post-retract");
    assert_eq!(env.token_amount(&env.perc_vault.clone()), 0, "insurance drained");
}

// CROSS-TAG FORFEITURE BYPASS: a voter whose insurance principal is vote-locked is refused the
// insurance exit (tag 5, which checks vote_locked). The escape an attacker would reach for is the OTHER
// withdraw — the own-vault `process_withdraw` (tag 2), which has NO vote_locked check at all (locking is
// an insurance-only concept). If tag 2 could operate on an insurance pool it would skip the lock AND try
// to pay from the percolator insurance vault — letting a live ballot outlive its capital (finding B).
// process_withdraw's is_insurance type guard (lib.rs:670) rejects an insurance pool outright, BEFORE any
// vault/seed work, so the vote-lock cannot be sidestepped by routing the exit through a different tag.
// The existing vote_locked test only drives tag 5; this pins tag 2 as the forfeiture-bypass defense.
// Because every check preceding line 670 passes here (owner signs, token_program ok, pool+position are
// subledger-owned, data empty), an is_err is PRECISELY the is_insurance guard firing — not incidental.
#[test]
fn vote_locked_insurance_position_cannot_be_drained_via_own_vault_withdraw() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("deposit");

    let dest = Pubkey::new_unique();
    let (_dist_proposal, gv_proposal) = create_and_register_proposal(&mut env, &ve, 1, &dest);

    // Vote → the position is now vote-locked; the insurance exit (tag 5) is refused.
    env.warp_slot(1124);
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("vote backs proposal");
    assert_eq!(env.svm.get_account(&env.position_pda(&alice.pubkey())).unwrap().data[97], 1, "vote-locked");
    assert!(env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, amount).is_err(), "tag-5 exit blocked by the lock");

    // ATTACK: route the exit through tag 2 (own-vault withdraw) to dodge the vote_locked check.
    let tag2 = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new_readonly(alice.pubkey(), true),
            AccountMeta::new(pool, false),
            AccountMeta::new(env.position_pda(&alice.pubkey()), false),
            AccountMeta::new(alice_ata, false),
            AccountMeta::new(env.perc_vault, false),
            AccountMeta::new_readonly(spl_token::ID, false),
        ],
        data: vec![2u8],
    };
    assert!(env.send(&[tag2], &[&alice]).is_err(), "tag-2 own-vault withdraw must reject an insurance pool (is_insurance guard)");

    // Capital stayed at risk; the position is intact and still lockable/retractable as before.
    assert_eq!(env.token_amount(&env.perc_vault), amount, "insurance vault untouched");
    let (principal, _s, withdrawn) = env.read_position(&alice.pubkey());
    assert_eq!(principal, amount, "principal intact");
    assert!(!withdrawn, "not marked withdrawn");
}

// STALE-WEIGHT VOTING (vote-outlives-capital, the partial-withdraw facet of finding B). vote_weight is
// floor(log2(age)) * principal, read from the LIVE subledger Position each call (genesis-vote never caches
// a ballot's weight from an earlier vote, and never trusts the position's `withdrawn` flag — it relies on
// principal being eager-decremented by process_insurance_withdraw, lib.rs:1267). So a voter who reduces
// their at-risk capital must immediately get reduced voting power on a re-vote — they cannot keep voting
// with stale-high weight after pulling capital out. The full-withdraw case (principal -> 0 -> weight 0 ->
// rejected) has cannot_vote_with_a_withdrawn_position, but that drives the REAL tag-23 withdraw (red post-
// percolator-rebuild); the partial-withdraw RE-VOTE was untested. Here the post-withdraw position state
// (principal cut 10x, still active) is set directly so the REAL genesis-vote binary is exercised without
// the broken withdraw CPI. Slot is held fixed, so the ONLY change between the two votes is the principal.
#[test]
fn a_revote_after_reducing_capital_uses_the_live_principal_not_stale_weight() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("deposit");
    let (_dist, gv_proposal) = create_and_register_proposal(&mut env, &ve, 1, &Pubkey::new_unique());

    // Vote with the full 1_000_000 at risk.
    env.warp_slot(1124); // age 1024 -> floor_log2 = 10
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("vote with full capital");
    let (w_full, p_full) = gv_proposal_support(&env, &gv_proposal);
    assert_eq!(p_full, 1_000_000, "support principal = full deposit");
    assert_eq!(w_full, 10_000_000, "weight = 1_000_000 * floor_log2(1024)=10");

    // Retract to clear the ballot + lock (the only legitimate way to free the principal for a withdraw).
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 2).expect("retract");

    // SIMULATE a partial insurance withdraw: process_insurance_withdraw eager-decrements
    // position.principal (lib.rs:1267); a 9/10 withdraw leaves principal = 100_000, withdrawn = false.
    // (Mocked directly because the real tag-23 withdraw CPI is fail-closed post-rebuild.)
    let pos_key = env.position_pda(&alice.pubkey());
    let mut pos = env.svm.get_account(&pos_key).unwrap();
    pos.data[72..80].copy_from_slice(&100_000u64.to_le_bytes());
    env.svm.set_account(pos_key, pos).unwrap();
    let (live_principal, _s, withdrawn) = env.read_position(&alice.pubkey());
    assert_eq!(live_principal, 100_000, "live principal reduced 10x");
    assert!(!withdrawn, "still an active (partially-funded) position");

    // RE-VOTE (same slot, so age is unchanged) -> weight must reflect the LIVE 100_000, not the stale 1M.
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("re-vote with reduced capital");
    let (w_live, p_live) = gv_proposal_support(&env, &gv_proposal);
    assert_eq!(p_live, 100_000, "re-vote records the LIVE principal, not the stale 1_000_000");
    assert_eq!(w_live, 1_000_000, "weight = 100_000 * 10 (live), 10x less than the stale-capital weight");
}

// CROSS-PROPOSAL TALLY INTEGRITY (quorum/majority manipulation across competing proposals): one voter,
// two competing proposals A and B sharing one gv_config. The real vote handler enforces one-vote-one-
// proposal (lib.rs:634 — a live ballot must be on THIS proposal) and migrates the GLOBAL tallies on a
// switch (lib.rs:641-648 — back out the prior contribution from the proposal AND from
// total_cast_weight/total_voted_principal before applying the new one). If either side leaked, a voter
// could (a) leave phantom support on A after moving to B, or (b) double-count their weight into the
// global majority/quorum denominators — manipulating which proposal triggers. All prior seal.rs trigger
// tests INJECT tallies; this drives the REAL deposit->vote->switch path against the real subledger +
// genesis-vote binaries, which is where the migration arithmetic actually lives.
#[test]
fn switching_a_vote_between_competing_proposals_migrates_the_global_tallies() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);
    let gv_config = ve.gv_config;
    let read_tallies = |env: &Env| -> (u64, u128) {
        let cfg = env.svm.get_account(&gv_config).unwrap();
        (
            u64::from_le_bytes(cfg.data[200..208].try_into().unwrap()),   // total_voted_principal
            u128::from_le_bytes(cfg.data[208..224].try_into().unwrap()),  // total_cast_weight (GG: u128)
        )
    };

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("deposit");

    let (a_dist, a_gv) = create_and_register_proposal(&mut env, &ve, 1, &Pubkey::new_unique());
    let (_b_dist, b_gv) = create_and_register_proposal(&mut env, &ve, 2, &Pubkey::new_unique());

    // Give the position tenure so its weight is non-zero, then back A.
    env.warp_slot(1124);
    gv_vote(&mut env, &ve, &alice, &a_gv, 1).expect("alice backs A");
    let (a_w, a_p) = gv_proposal_support(&env, &a_gv);
    assert!(a_w > 0 && a_p == amount, "A holds alice's full weight+principal");
    assert_eq!(read_tallies(&env), (a_p, a_w as u128), "globals == A's contribution");

    // (1) One-vote-one-proposal: backing B while the A ballot is live is REJECTED (no switch without
    // retract). The failed tx must not mutate any tally.
    assert!(gv_vote(&mut env, &ve, &alice, &b_gv, 1).is_err(), "cannot back a second proposal while live");
    assert_eq!(gv_proposal_support(&env, &b_gv), (0, 0), "B gained nothing from the rejected back");
    assert_eq!(gv_proposal_support(&env, &a_gv), (a_w, a_p), "A's tally intact after the rejected back");
    assert_eq!(read_tallies(&env), (a_p, a_w as u128), "globals unchanged by the rejected back");

    // (2) Retract A -> A's support AND the globals zero out (no phantom support left behind).
    gv_vote(&mut env, &ve, &alice, &a_gv, 2).expect("retract A");
    assert_eq!(gv_proposal_support(&env, &a_gv), (0, 0), "A drained on retract");
    assert_eq!(read_tallies(&env), (0, 0), "globals zeroed on retract");

    // (3) Back B (no slot change, so weight is identical) -> B holds the weight, A stays zero, and the
    // globals equal exactly ONE contribution (not doubled).
    gv_vote(&mut env, &ve, &alice, &b_gv, 1).expect("alice backs B");
    assert_eq!(gv_proposal_support(&env, &b_gv), (a_w, a_p), "B now holds the full weight+principal");
    assert_eq!(gv_proposal_support(&env, &a_gv), (0, 0), "A still zero — no phantom support");
    assert_eq!(read_tallies(&env), (a_p, a_w as u128), "globals == ONE contribution, not double-counted");
    let _ = a_dist;
}

// FLASH-DEPOSIT QUORUM PUMP (Sybil timing): vote weight = floor(log2(hold_age)) * principal, and a
// position with age < 2 has ZERO weight. The vote handler rejects a weight-0 vote OUTRIGHT. That
// rejection is load-bearing: a vote ADDS the position's PRINCIPAL to total_voted_principal (the quorum
// numerator) right after the weight check. If a weight-0 vote were accepted, an attacker could deposit a
// large sum and vote in the SAME slot — pumping the principal quorum (total_voted_principal*2 > outstanding)
// toward a premature trigger — while contributing no time-weight at all. So the "too recent" reject is what
// forces capital to actually sit at risk before it can count toward quorum.
#[test]
fn a_too_recent_position_cannot_vote_or_pump_the_quorum() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("deposit");
    let dest = Pubkey::new_unique();
    let (_dp, gv_proposal) = create_and_register_proposal(&mut env, &ve, 1, &dest);

    // ATTACK: vote in the SAME slot as the deposit (age 0 -> weight 0).
    assert!(
        gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).is_err(),
        "a too-recent (age<2) position cannot vote"
    );
    let (w, p) = gv_proposal_support(&env, &gv_proposal);
    assert_eq!((w, p), (0, 0), "the rejected fresh vote credited NO weight and NO principal (no quorum pump)");

    // After holding long enough, the SAME position votes normally; weight grows with log2(age).
    env.warp_slot(1124); // age 1024 -> floor(log2)=10
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("an aged position votes");
    let (w2, p2) = gv_proposal_support(&env, &gv_proposal);
    assert_eq!(p2, amount, "the aged vote finally credits the principal");
    assert_eq!(w2, 10 * amount, "weight = floor(log2(1024)) * principal");
}

// Cross-config binding (finalize-DOS): a vote may only be registered against a
// distribution proposal that belongs to THIS genesis's distribution config. A
// proposal owned by the distribution program but under a DIFFERENT config, if it
// won, could never be sealed (trigger CPIs SealWinner with config.distribution_config,
// which the distribution rejects on header.config mismatch) — bricking finalize
// forever. register_proposal must refuse to bind such a proposal up front.
#[test]
fn register_rejects_foreign_distribution_proposal() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env); // genesis distribution config is under env.mint

    // Build a FOREIGN, fully-legitimate distribution config under a different mint.
    let foreign_mint = create_mint(&mut env.svm, &clone_kp(&env.payer), &env.mint_auth.pubkey());
    let foreign_authority = Pubkey::new_unique();
    let foreign_config = dist_config_pda(&foreign_mint, &foreign_authority);
    let foreign_vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &foreign_mint, &foreign_config);
    mint_to(&mut env.svm, &clone_kp(&env.payer), &foreign_mint, &clone_kp(&env.mint_auth), &foreign_vault, 100);
    revoke_mint_authority(&mut env, &foreign_mint); // fixed-supply COIN (Safety §4)
    let mut data = vec![0u8]; // IX_INIT_CONFIG
    data.extend_from_slice(&1_000_000u64.to_le_bytes());
    data.extend_from_slice(&100u64.to_le_bytes());
    let init = Instruction {
        program_id: dist_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(foreign_mint, false),
            AccountMeta::new(foreign_config, false),
            AccountMeta::new_readonly(foreign_vault, false),
            AccountMeta::new_readonly(foreign_authority, false), // bound into the config seed (finding AA)
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    };
    env.send(&[init], &[]).expect("foreign dist config init");

    // A proposal + entry under the FOREIGN config.
    let id = 7u64;
    let foreign_proposal =
        Pubkey::find_program_address(&[b"dist_proposal", foreign_config.as_ref(), &id.to_le_bytes()], &dist_id()).0;
    let mut cd = vec![1u8];
    cd.extend_from_slice(&id.to_le_bytes());
    cd.extend_from_slice(&4u32.to_le_bytes());
    env.send(&[Instruction {
        program_id: dist_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(foreign_config, false),
            AccountMeta::new(foreign_proposal, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: cd,
    }], &[]).expect("create foreign proposal");

    // Now try to register a genesis vote against that foreign proposal.
    let gv_proposal =
        Pubkey::find_program_address(&[b"gv_proposal", ve.gv_config.as_ref(), foreign_proposal.as_ref()], &gv_id()).0;
    let reg = Instruction {
        program_id: gv_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(ve.gv_config, false),
            AccountMeta::new(gv_proposal, false),
            AccountMeta::new_readonly(foreign_proposal, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: vec![2u8],
    };
    let res = env.send(&[reg], &[]);
    assert!(res.is_err(), "must not register a vote against a foreign-config proposal");
    // The gv_proposal account was never created.
    assert!(env.svm.get_account(&gv_proposal).map_or(true, |a| a.data.is_empty()));

    // Sanity: a proposal under the genesis's OWN config still registers fine.
    let dest = Pubkey::new_unique();
    let (_dp, gv_ok) = create_and_register_proposal(&mut env, &ve, 1, &dest);
    assert!(env.svm.get_account(&gv_ok).is_some_and(|a| !a.data.is_empty()), "own-config proposal registers");
}

// The vote-lock must not become a permanent freeze. After the winner is sealed
// (pv.executed), a WINNING voter's position is still locked — they must be able to
// RETRACT post-seal to release the lock and exit their principal. (The seal is
// immutable; only NEW backing is forbidden post-seal.) Without this, the very
// voters who carried the winning proposal would have their capital frozen forever.
#[test]
fn winning_voter_can_retract_and_exit_after_finalize() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("deposit");

    let dest = Pubkey::new_unique();
    let (dist_proposal, gv_proposal) = create_and_register_proposal(&mut env, &ve, 1, &dest);

    env.warp_slot(1124);
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("vote");

    // Finalize: the single voter holds 100%, so quorum + majority both hold.
    gv_trigger(&mut env, &ve, &gv_proposal, &dist_proposal).expect("trigger seals the winner");

    // Still locked immediately post-seal: capital can't sneak out without retracting.
    let err = env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, amount);
    assert!(err.is_err(), "still vote-locked post-seal until retracted");

    // The freeze fix: a winning voter can retract AFTER finalize (only new backing
    // is forbidden once sealed), which clears the subledger lock.
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 2).expect("retract must be allowed post-seal");

    // ...and then recover their principal. No permanent freeze.
    env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, amount).expect("exit after finalize+retract");
    assert_eq!(env.token_amount(&alice_ata), amount, "principal recovered after finalize");
}

// Griefing-freeze: init_insurance_pool is permissionless and records vote_authority
// as-is, so an attacker could front-run pool creation with a hostile vote_authority.
// That must NOT let them freeze depositors: set_vote_lock requires the position
// OWNER to sign, so a position can only be (un)locked when its owner is acting on
// their own vote. Here a hostile authority tries to lock a victim and fails; the
// victim's funds stay withdrawable.
#[test]
fn hostile_vote_authority_cannot_freeze_a_depositor() {
    let mut env = Env::new();
    let attacker = Keypair::new();
    env.svm.airdrop(&attacker.pubkey(), 1_000_000_000).unwrap();

    // Pool created with the ATTACKER as vote_authority (the front-run scenario).
    let mut data = vec![3u8]; // IX_INIT_INSURANCE_POOL
    data.extend_from_slice(&ASSET_ID.to_le_bytes());
    data.push(POLICY_PRINCIPAL);
    let init = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(env.mint, false),
            AccountMeta::new(env.pool, false),
            AccountMeta::new_readonly(env.perc_vault, false),
            AccountMeta::new_readonly(env.slab, false),
            AccountMeta::new_readonly(perc_id(), false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(attacker.pubkey(), false), // hostile vote_authority
        ],
        data,
    };
    env.send(&[init], &[]).expect("init pool with hostile authority");

    let amount = 1_000_000u64;
    let (victim, victim_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&victim, &victim_ata, &holding, amount).expect("deposit");

    // Attacker signs as the vote_authority and tries to lock the victim's position
    // WITHOUT the victim's signature (victim passed as a non-signer account).
    let attack = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new_readonly(attacker.pubkey(), true),
            AccountMeta::new_readonly(env.pool, false),
            AccountMeta::new(env.position_pda(&victim.pubkey()), false),
            AccountMeta::new_readonly(victim.pubkey(), false), // owner NOT signing
        ],
        data: vec![6u8, 1u8], // IX_SET_VOTE_LOCK, locked=1
    };
    let res = env.send(&[attack], &[&attacker]);
    assert!(res.is_err(), "cannot lock a position the owner did not sign for");

    // The victim is not frozen — their principal is still withdrawable.
    env.insurance_withdraw(&victim, &victim_ata, &holding, &victim, amount).expect("victim can still exit");
    assert_eq!(env.token_amount(&victim_ata), amount, "depositor funds were never frozen");
}

// SYBIL HOLE (vote outlives capital): set_vote_lock requires BOTH the owner AND the vote_authority
// (the gv config PDA) to sign. The freeze test above pins the owner-sig half. THIS pins the
// vote_authority-sig half — the one that stops an owner from SELF-UNLOCKING. The lock is only ever
// cleared by the gv vote-RETRACT CPI (which makes the config PDA sign and also removes the ballot's
// weight/principal). If an owner could clear the lock directly — by naming the gv config as a
// read-only (unsigned) account — they would withdraw their principal while their ballot stays live:
// a vote backed by capital that is no longer at risk (the core Sybil break the whole bootstrap rests
// on). The vote_authority.is_signer check rejects it.
#[test]
fn owner_cannot_self_unlock_a_live_vote_to_exit_capital() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("deposit");
    let dest = Pubkey::new_unique();
    let (_dp, gv_proposal) = create_and_register_proposal(&mut env, &ve, 1, &dest);

    let vote_locked = |env: &Env| -> bool {
        env.svm.get_account(&env.position_pda(&alice.pubkey())).unwrap().data[97] == 1
    };

    env.warp_slot(1124);
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("vote backs proposal");
    assert!(vote_locked(&env), "voting locks the principal");

    // ATTACK: alice calls set_vote_lock(0) on her OWN position, naming the gv config as the
    // vote_authority but WITHOUT its signature (only alice signs as the owner).
    let attack = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new_readonly(ve.gv_config, false), // gv config NAMED but NOT signing
            AccountMeta::new_readonly(env.pool, false),
            AccountMeta::new(env.position_pda(&alice.pubkey()), false),
            AccountMeta::new_readonly(alice.pubkey(), true), // owner signs
        ],
        data: vec![6u8, 0u8], // IX_SET_VOTE_LOCK, locked = 0 (unlock)
    };
    assert!(
        env.send(&[attack], &[&alice]).is_err(),
        "owner cannot self-unlock without the gv authority's signature"
    );
    assert!(vote_locked(&env), "position stays locked — self-unlock refused");

    // ATTACK 2 (anti-mask of attack 1): the first attack is refused by the is_signer check (the gv config
    // is named but does not sign). To force the vote_authority *key binding* to be the sole decider, alice
    // names HERSELF as the vote_authority AND signs — so is_signer PASSES and only
    // `pool.vote_authority != *vote_authority.key` (lib.rs:1186) stands between her and a self-unlock. If it
    // were dropped, she would unlock her own live-voted position and exit while the ballot still counts
    // (finding B: ballot outlives capital). Must be refused.
    let attack2 = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new_readonly(alice.pubkey(), true), // alice names HERSELF as vote_authority AND signs
            AccountMeta::new_readonly(env.pool, false),
            AccountMeta::new(env.position_pda(&alice.pubkey()), false),
            AccountMeta::new_readonly(alice.pubkey(), true), // owner = same alice, signs
        ],
        data: vec![6u8, 0u8], // IX_SET_VOTE_LOCK, locked = 0 (unlock)
    };
    assert!(
        env.send(&[attack2], &[&alice]).is_err(),
        "owner cannot self-unlock by naming themselves as the vote_authority (key binding, :1186)"
    );
    assert!(vote_locked(&env), "position stays locked — self-named-authority unlock refused");

    // The capital still cannot leave while the ballot is live.
    assert!(
        env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, amount).is_err(),
        "vote-locked principal still cannot exit"
    );
    assert_eq!(env.token_amount(&env.perc_vault.clone()), amount, "capital still at risk");
}

#[test]
fn principal_only_owner_exit_returns_funds_and_guards() {
    let mut env = Env::new();
    env.init_insurance_pool();

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("deposit");
    assert_eq!(env.token_amount(&env.perc_vault.clone()), amount);

    // A non-owner cannot withdraw the owner's position.
    let (mallory, _mallory_ata) = new_depositor(&mut env, 0);
    let err = env.insurance_withdraw(&alice, &alice_ata, &holding, &mallory, 1);
    assert!(err.is_err(), "non-owner cannot withdraw");

    // Cannot withdraw more than the recorded principal.
    let err = env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, amount + 1);
    assert!(err.is_err(), "cannot exceed recorded principal");

    // Partial principal-only exit.
    env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, 400_000).expect("partial exit");
    assert_eq!(env.token_amount(&alice_ata), 400_000, "user got partial principal back");
    assert_eq!(env.token_amount(&env.perc_vault.clone()), 600_000, "insurance decreased");
    let (principal, _start, withdrawn) = env.read_position(&alice.pubkey());
    assert_eq!(principal, 600_000);
    assert!(!withdrawn);
    assert_eq!(env.pool_outstanding(), 600_000);

    // Exit the remainder.
    env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, 600_000).expect("full exit");
    assert_eq!(env.token_amount(&alice_ata), amount, "user got all principal back");
    assert_eq!(env.token_amount(&env.perc_vault.clone()), 0, "insurance drained");
    let (principal, _start, withdrawn) = env.read_position(&alice.pubkey());
    assert_eq!(principal, 0);
    assert!(withdrawn, "position retired at zero principal");
    assert_eq!(env.pool_outstanding(), 0);

    // A retired position cannot be withdrawn again.
    let err = env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, 1);
    assert!(err.is_err(), "retired position cannot withdraw");
}

// NON-OWNER INSURANCE PRINCIPAL THEFT (genesis-critical, owner half of insurance_withdraw's guard):
// insurance_withdraw re-derives the POOL PDA but NOT the position PDA, so `position.owner == owner`
// (lib.rs:1039) is the SOLE guard that only the depositor can pull their at-risk principal. The own-vault
// path has non_owner_cannot_withdraw_another_position; the genesis insurance path (where the real money
// lives) had no equivalent. Without this check an attacker who SIGNS could pass the VICTIM's position and
// route the payout to their own ATA, stealing the victim's insurance principal.
#[test]
fn a_non_owner_cannot_withdraw_a_victims_insurance_principal() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let amount = 1_000_000u64;
    let (victim, victim_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&victim, &victim_ata, &holding, amount).expect("victim deposit");
    assert_eq!(env.token_amount(&env.perc_vault.clone()), amount, "victim's principal is in insurance");

    // Attacker signs (account-0 owner = attacker) but targets the VICTIM's position, routing the payout
    // to the attacker's own ATA. (insurance_withdraw's owner param drives the position PDA; signer is the
    // submitting key — here they differ, which is exactly the theft attempt.)
    let (attacker, attacker_ata) = new_depositor(&mut env, 0);
    assert!(
        env.insurance_withdraw(&victim, &attacker_ata, &holding, &attacker, amount).is_err(),
        "a non-owner must NOT be able to withdraw the victim's insurance principal"
    );
    assert_eq!(env.token_amount(&env.perc_vault.clone()), amount, "victim's insurance untouched");
    assert_eq!(env.token_amount(&attacker_ata), 0, "attacker gained nothing");
    let (principal, _, withdrawn) = env.read_position(&victim.pubkey());
    assert_eq!(principal, amount, "victim's position principal intact");
    assert!(!withdrawn, "victim's position not retired by the failed theft");

    // The genuine owner can still exit normally.
    env.insurance_withdraw(&victim, &victim_ata, &holding, &victim, amount).expect("victim exits their own position");
    assert_eq!(env.token_amount(&victim_ata), amount, "owner recovers their full principal");
}

// Type-confusion boundary: the own-vault deposit path (tag 1) must REJECT an
// insurance pool. An insurance pool's `vault` is the percolator insurance vault,
// owned by the percolator vault_authority — not this pool PDA. Without the guard,
// an own-vault deposit would SPL-transfer the user's funds straight into that
// vault with NO TopUpInsurance CPI (percolator never counts them) and record an
// own-vault position; the matching own-vault withdraw could never sign those
// funds back out (the pool PDA is not the vault's token authority) → the user's
// principal is stranded. This pins that the misuse is refused up front.
#[test]
fn own_vault_deposit_is_rejected_on_an_insurance_pool() {
    let mut env = Env::new();
    env.init_insurance_pool();

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;

    // Own-vault deposit (IX_DEPOSIT = 1) aimed at the insurance pool, with the
    // insurance vault passed as the own-vault `vault`. The guard must fire before
    // any token movement.
    let mut data = vec![1u8];
    data.extend_from_slice(&amount.to_le_bytes());
    let ix = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(alice.pubkey(), true),
            AccountMeta::new(pool, false),
            AccountMeta::new(env.position_pda(&alice.pubkey()), false),
            AccountMeta::new(alice_ata, false),
            AccountMeta::new(env.perc_vault, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    };
    let res = env.send(&[ix], &[&alice]);
    assert!(res.is_err(), "own-vault deposit must be refused on an insurance pool");

    // And the user's funds never moved into the insurance vault.
    assert_eq!(env.token_amount(&alice_ata), amount, "depositor funds untouched");
    assert_eq!(env.token_amount(&env.perc_vault.clone()), 0, "insurance vault untouched");
}

// Canonical-vault pin (issue #24, active path): init_insurance_pool must reject a
// vault that is owned by the correct vault_authority and holds the correct mint
// but is NOT the canonical ATA. Percolator (F-VAULT-FRAG) would reject such a
// vault on every deposit/withdraw CPI, so binding a pool to it leaves the pool
// permanently inert. Pinning the canonical address at init fails fast instead.
#[test]
fn init_insurance_pool_rejects_non_canonical_vault() {
    let mut env = Env::new();

    // A second token account owned by the very same vault_authority, correct mint,
    // but at a fresh (non-canonical) address.
    let rogue_vault = Pubkey::new_unique();
    env.svm
        .set_account(
            rogue_vault,
            solana_sdk::account::Account {
                lamports: 1_000_000_000,
                data: token_account_data(&env.mint, &env.vault_authority, 0),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    assert_ne!(rogue_vault, env.perc_vault, "precondition: not the canonical ATA");

    let mut data = vec![3u8]; // IX_INIT_INSURANCE_POOL
    data.extend_from_slice(&ASSET_ID.to_le_bytes());
    data.push(POLICY_PRINCIPAL);
    let ix = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(env.mint, false),
            AccountMeta::new(env.pool, false),
            AccountMeta::new_readonly(rogue_vault, false),
            AccountMeta::new_readonly(env.slab, false),
            AccountMeta::new_readonly(perc_id(), false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(gv_config_pda(&env.coin_mint, &env.pool), false),
        ],
        data,
    };
    let res = env.send(&[ix], &[]);
    assert!(res.is_err(), "init must reject a non-canonical vault");

    // The pool account was never created, so the canonical path still works.
    assert!(env.svm.get_account(&env.pool).map_or(true, |a| a.data.is_empty()));
    env.init_insurance_pool();
}

// Verify the percolator UpdateAssetAuthority encoding the TWAP handoff bridge
// (twap-program IX_ACCEPT_OPERATOR) relies on — tag 65, asset_index 0,
// kind=INSURANCE_OPERATOR(2), accounts [current(signer), new(signer), market(w)] —
// against the REAL percolator binary, so the handoff can't silently fail.
#[test]
fn percolator_update_asset_authority_operator_encoding_is_accepted() {
    let mut svm = LiteSVM::new().with_compute_budget(ComputeBudget {
        compute_unit_limit: 1_400_000,
        heap_size: 256 * 1024,
        ..ComputeBudget::default()
    });
    svm.add_program_from_file(perc_id(), perc_so()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 1_000_000_000_000).unwrap();
    let mint_auth = Keypair::new();
    let mint = create_mint(&mut svm, &payer, &mint_auth.pubkey());

    let admin = Keypair::new(); // marketauth -> asset-0 asset_admin
    let slab = Pubkey::new_unique();
    let init_slot = 100u64;
    let slab_data = make_live_market(&slab, &mint, &admin.pubkey(), init_slot);
    svm.set_account(
        slab,
        Account { lamports: 1_000_000_000, data: slab_data, owner: perc_id(), executable: false, rent_epoch: 0 },
    )
    .unwrap();
    svm.set_sysvar(&Clock { slot: init_slot, unix_timestamp: 100, ..Clock::default() });

    let new_op = Keypair::new();
    let mut data = vec![65u8]; // IX_UPDATE_ASSET_AUTHORITY
    data.extend_from_slice(&0u16.to_le_bytes()); // asset_index 0
    data.push(2u8); // ASSET_AUTH_INSURANCE_OPERATOR
    data.extend_from_slice(new_op.pubkey().as_ref());
    let ix = Instruction {
        program_id: perc_id(),
        accounts: vec![
            AccountMeta::new_readonly(admin.pubkey(), true),
            AccountMeta::new_readonly(new_op.pubkey(), true),
            AccountMeta::new(slab, false),
        ],
        data,
    };
    let bh = svm.latest_blockhash();
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer, &admin, &new_op], bh);
    svm.send_transaction(tx).expect("real percolator accepts the operator rotation encoding");

    // ADVERSARIAL: a random key (not the asset_admin, not the current operator)
    // cannot hijack the insurance operator. The whole handoff's safety rests on
    // percolator gating authority rotations — if it didn't, anyone could seize the
    // operator and drain insurance. Pin that percolator rejects it.
    let attacker = Keypair::new();
    let attacker_target = Keypair::new();
    let mut bad = vec![65u8];
    bad.extend_from_slice(&0u16.to_le_bytes());
    bad.push(2u8); // INSURANCE_OPERATOR
    bad.extend_from_slice(attacker_target.pubkey().as_ref());
    let bad_ix = Instruction {
        program_id: perc_id(),
        accounts: vec![
            AccountMeta::new_readonly(attacker.pubkey(), true), // NOT the asset_admin/operator
            AccountMeta::new_readonly(attacker_target.pubkey(), true),
            AccountMeta::new(slab, false),
        ],
        data: bad,
    };
    svm.expire_blockhash();
    let bh = svm.latest_blockhash();
    let tx = Transaction::new_signed_with_payer(&[bad_ix], Some(&payer.pubkey()), &[&payer, &attacker, &attacker_target], bh);
    assert!(
        svm.send_transaction(tx).is_err(),
        "a non-authority must not be able to hijack the insurance operator"
    );
}


// Front-run griefing DOS (finding M2): register_proposal is otherwise permissionless,
// so an attacker could register a creator's partially-built proposal, freezing the
// (entry_count,total_amount) snapshot; the creator's next append would then make the
// live proposal mismatch the snapshot and trigger would reject it forever. Fixed by
// requiring the registrant to be the proposal's creator. Here a non-creator is
// rejected and the creator succeeds.
#[test]
fn only_the_proposal_creator_can_register_it() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);
    let dist_config = ve.dist_config;

    let creator = Keypair::new();
    env.svm.airdrop(&creator.pubkey(), 10_000_000_000).unwrap();
    let id = 1u64;
    let dist_proposal =
        Pubkey::find_program_address(&[b"dist_proposal", dist_config.as_ref(), &id.to_le_bytes()], &dist_id()).0;
    let mut cd = vec![1u8];
    cd.extend_from_slice(&id.to_le_bytes());
    cd.extend_from_slice(&4u32.to_le_bytes());
    env.send(&[Instruction {
        program_id: dist_id(),
        accounts: vec![
            AccountMeta::new(creator.pubkey(), true),
            AccountMeta::new_readonly(dist_config, false),
            AccountMeta::new(dist_proposal, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: cd,
    }], &[&creator]).expect("creator creates proposal");
    let dest = Pubkey::new_unique();
    let mut ad = vec![2u8];
    ad.extend_from_slice(&1u32.to_le_bytes());
    ad.extend_from_slice(dest.as_ref());
    ad.extend_from_slice(&100u64.to_le_bytes());
    env.send(&[Instruction {
        program_id: dist_id(),
        accounts: vec![
            AccountMeta::new(creator.pubkey(), true),
            AccountMeta::new_readonly(dist_config, false),
            AccountMeta::new(dist_proposal, false),
        ],
        data: ad,
    }], &[&creator]).expect("creator appends");

    let gv_proposal =
        Pubkey::find_program_address(&[b"gv_proposal", ve.gv_config.as_ref(), dist_proposal.as_ref()], &gv_id()).0;
    let register = |payer: Pubkey| Instruction {
        program_id: gv_id(),
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new_readonly(ve.gv_config, false),
            AccountMeta::new(gv_proposal, false),
            AccountMeta::new_readonly(dist_proposal, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: vec![2u8],
    };

    // ATTACKER (env.payer, not the creator) cannot front-register.
    assert!(
        env.send(&[register(env.payer.pubkey())], &[]).is_err(),
        "a non-creator must not be able to register the proposal"
    );
    // The creator can register their own.
    env.send(&[register(creator.pubkey())], &[&creator]).expect("creator registers");
    assert!(env.svm.get_account(&gv_proposal).is_some_and(|a| !a.data.is_empty()), "gv_proposal created by the creator");
}

// Finding Q regression: init_insurance_pool is permissionless, so before the pool PDA
// committed to its market binding an attacker could grab the genesis pool PDA
// (= f(COIN_mint, asset 0)) FIRST, bound to a percolator market THEY control, with
// vote_authority set to the predictable real gv config PDA — passing the gv binding
// check and routing every depositor's principal into the attacker's market (LOF). Now
// the pool PDA commits to (mint, asset_id, market_slab, percolator_program), so an
// attacker's pool lands at a DIFFERENT address and the genesis pool PDA — bound to the
// real market — stays free and untouched.
#[test]
fn init_insurance_pool_cannot_be_squatted_to_misdirect_the_genesis_pool() {
    let mut env = Env::new();

    // The attacker stands up their OWN percolator market with a canonical insurance
    // vault for the same COIN mint (marketauth is irrelevant to pool init).
    let attacker_slab = Pubkey::new_unique();
    let attacker_marketauth = Pubkey::new_unique();
    let slab_data = make_live_market(&attacker_slab, &env.mint, &attacker_marketauth, 100);
    env.svm.set_account(attacker_slab, Account {
        lamports: 1_000_000_000, data: slab_data, owner: perc_id(), executable: false, rent_epoch: 0,
    }).unwrap();
    let attacker_vault_authority =
        Pubkey::find_program_address(&[b"vault", attacker_slab.as_ref()], &perc_id()).0;
    let attacker_vault = Pubkey::find_program_address(
        &[attacker_vault_authority.as_ref(), spl_token::ID.as_ref(), env.mint.as_ref()],
        &ATA_PROGRAM_ID,
    ).0;
    env.svm.set_account(attacker_vault, Account {
        lamports: 1_000_000, data: token_account_data(&env.mint, &attacker_vault_authority, 0),
        owner: spl_token::ID, executable: false, rent_epoch: 0,
    }).unwrap();

    // The attacker's pool PDA is bound to THEIR market — a different address from the
    // genesis pool (env.pool), which is bound to the real market (env.slab).
    let attacker_pool = Pubkey::find_program_address(
        &[b"subledger_pool", env.mint.as_ref(), &ASSET_ID.to_le_bytes(), attacker_slab.as_ref(), perc_id().as_ref()],
        &sub_id(),
    ).0;
    assert_ne!(attacker_pool, env.pool, "the market binding is part of the pool PDA");

    // The attacker CAN init their own pool (init is permissionless) — but only at THEIR
    // PDA, bound to THEIR market. It does NOT touch the genesis pool PDA.
    let mut data = vec![3u8]; // IX_INIT_INSURANCE_POOL
    data.extend_from_slice(&ASSET_ID.to_le_bytes());
    data.push(POLICY_PRINCIPAL);
    let squat = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(env.mint, false),
            AccountMeta::new(attacker_pool, false),
            AccountMeta::new_readonly(attacker_vault, false),
            AccountMeta::new_readonly(attacker_slab, false),
            AccountMeta::new_readonly(perc_id(), false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(gv_config_pda(&env.coin_mint, &env.pool), false),
        ],
        data,
    };
    env.send(&[squat], &[]).expect("attacker may init their own pool, but at their own PDA");
    assert!(env.svm.get_account(&env.pool).map_or(true, |a| a.data.is_empty()), "genesis pool PDA untouched");

    // THE GENESIS POOL STILL INITS: the squat did not block it, and it binds the REAL
    // market — depositor principal can only ever route into the real market.
    env.init_insurance_pool();
    let pool_acc = env.svm.get_account(&env.pool).unwrap();
    let bound_market = Pubkey::new_from_array(pool_acc.data[96..128].try_into().unwrap());
    assert_eq!(bound_market, env.slab, "genesis pool binds the REAL market, not the attacker's");
}

// FRONT-RUN BRICK via an out-of-range policy (permanent withdraw DOS): init_insurance_pool is
// permissionless and the genesis pool PDA is deterministic, so an attacker can race the orchestrator to
// it. The market/vault bindings are part of the PDA seeds (squat test above), but `policy` is a free
// instruction byte. If init did not reject policy > POLICY_WITH_SURPLUS, an attacker could initialize the
// REAL genesis pool PDA with a garbage policy: payout()'s `_ => Err` (and Pool::deserialize's policy
// guard) would then make EVERY insurance_deposit/withdraw revert, and the legit init is refused
// (AccountAlreadyInitialized) — the canonical pool is bricked and depositor exits are frozen forever.
// lib.rs:732 rejects the bad policy up front; this pins it (the PDA stays free for the real init).
#[test]
fn front_running_the_genesis_pool_with_a_bad_policy_is_rejected() {
    let mut env = Env::new();

    // ATTACK: init the REAL genesis pool PDA (real mint/vault/slab bindings — so only the policy is
    // wrong) with an out-of-range policy = POLICY_WITH_SURPLUS + 1.
    let mut data = vec![3u8]; // IX_INIT_INSURANCE_POOL
    data.extend_from_slice(&ASSET_ID.to_le_bytes());
    data.push(2u8); // out of range: only 0 (principal) and 1 (with-surplus) are real policies
    let bad = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(env.mint, false),
            AccountMeta::new(env.pool, false),
            AccountMeta::new_readonly(env.perc_vault, false),
            AccountMeta::new_readonly(env.slab, false),
            AccountMeta::new_readonly(perc_id(), false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(gv_config_pda(&env.coin_mint, &env.pool), false),
        ],
        data,
    };
    assert!(env.send(&[bad], &[]).is_err(), "init must reject an out-of-range insurance policy");
    assert!(env.svm.get_account(&env.pool).map_or(true, |a| a.data.is_empty()), "genesis pool PDA untouched — not bricked");

    // The genesis pool then inits normally and is fully usable: a deposit + full exit round-trips.
    env.init_insurance_pool();
    let pool = env.pool;
    let (alice, alice_ata) = new_depositor(&mut env, 1_000_000);
    let hold = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &hold, 1_000_000).expect("deposit into the real pool");
    env.insurance_withdraw(&alice, &alice_ata, &hold, &alice, 1_000_000).expect("exit is not bricked");
    assert_eq!(env.token_amount(&alice_ata), 1_000_000, "principal fully recovered — the pool works");
}

// CROSS-INSTRUCTION PDA SQUAT (account-confusion/seed-collision): both init_pool (own-vault, tag 0)
// and init_insurance_pool (tag 3) derive their pool PDA from pool_seeds(mint, asset_id, market_slab,
// percolator_program). The genesis insurance pool lives at (mint, 0, REAL_market, REAL_program). If
// init_pool let the caller supply the market/program seed parts, an attacker could derive that exact
// address with a BACKING-domain own-vault pool, seize the PDA (legit init then fails
// AccountAlreadyInitialized), and brick the genesis (genesis-vote needs is_insurance() == true).
// init_pool defends by HARDCODING the market/program seed components to Pubkey::default() (lib.rs:394),
// so own-vault pools are confined to the (mint, asset_id, default, default) namespace — provably
// disjoint from any real-market insurance pool. This pins that isolation: init_pool cannot be pointed
// at the genesis insurance PDA. (The init_insurance_pool foreign-market + bad-policy squats are pinned
// separately; this closes the wrong-instruction angle.)
#[test]
fn own_vault_init_pool_cannot_squat_the_genesis_insurance_pda() {
    let mut env = Env::new();

    // The own-vault namespace for the same (mint, asset_id) is a DIFFERENT address than the genesis
    // insurance pool — the market/program seed parts differ (default vs the real market).
    let own_vault_pda = Pubkey::find_program_address(
        &[b"subledger_pool", env.mint.as_ref(), &ASSET_ID.to_le_bytes(), Pubkey::default().as_ref(), Pubkey::default().as_ref()],
        &sub_id(),
    ).0;
    assert_ne!(own_vault_pda, env.pool, "own-vault and insurance pool PDAs are structurally disjoint");

    // ATTACK: call init_pool (own-vault) pointing pool_account at the genesis insurance PDA, asset_id 0,
    // domain = BACKING. init_pool re-derives the expected PDA with the DEFAULT market/program and finds
    // it != env.pool -> InvalidSeeds, before it ever touches the vault.
    let mut data = vec![0u8]; // IX_INIT_POOL
    data.extend_from_slice(&ASSET_ID.to_le_bytes());
    data.push(POLICY_PRINCIPAL);
    data.push(1u8); // DOMAIN_BACKING
    let squat = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(env.mint, false),
            AccountMeta::new(env.pool, false), // the genesis insurance PDA
            AccountMeta::new_readonly(Pubkey::new_unique(), false), // vault (never reached)
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    };
    assert!(env.send(&[squat], &[]).is_err(), "init_pool must not be redirectable onto the insurance PDA");
    assert!(env.svm.get_account(&env.pool).map_or(true, |a| a.data.is_empty()), "genesis insurance PDA untouched");

    // CONTROL: the genuine insurance init still proceeds at that PDA — INSURANCE domain (byte 90 == 0),
    // bound to the REAL market, not a squatted BACKING pool.
    env.init_insurance_pool();
    let acc = env.svm.get_account(&env.pool).unwrap();
    assert_eq!(acc.data[90], 0, "genesis pool domain = INSURANCE (not the attacker's BACKING)");
    assert_eq!(Pubkey::new_from_array(acc.data[96..128].try_into().unwrap()), env.slab, "bound to the real market");
}

// PHANTOM-CAPITAL VOTE (Sybil-resistance core): vote weight must reflect capital GENUINELY at risk.
// Probe: deposit P, back a proposal, retract, WITHDRAW the capital, then back AGAIN — trying to vote
// with principal already pulled out. genesis-vote `read_sub_position` reads `principal` and does NOT
// check a withdrawn flag, so IF withdraw left `principal` intact (only flipping a flag) the re-vote
// would award full weight for capital no longer at risk, while the quorum denominator (live
// outstanding) had dropped — a free, denominator-shrinking Sybil vote. BLOCKED:
// `process_insurance_withdraw` DECREMENTS `position.principal -= amount`, so a full exit zeroes the
// live principal; the re-vote computes weight 0 and is rejected. (A partial exit leaves only the
// remaining at-risk principal as weight — also correct.)
#[test]
fn cannot_vote_with_a_withdrawn_position() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("deposit");
    let dest = Pubkey::new_unique();
    let (_dist_proposal, gv_proposal) = create_and_register_proposal(&mut env, &ve, 1, &dest);

    env.warp_slot(1124);
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("first vote (real capital at risk)");
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 2).expect("retract to unlock");
    env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, amount).expect("withdraw — capital returned");
    assert_eq!(env.token_amount(&alice_ata), amount, "alice got her capital back");
    assert_eq!(env.pool_outstanding(), 0, "outstanding no longer counts the withdrawn principal");

    // The withdrawal zeroed the LIVE principal, so there is no phantom capital to vote with.
    let (live_principal, _start, withdrawn) = env.read_position(&alice.pubkey());
    assert_eq!(live_principal, 0, "full withdraw zeroes the position's live principal");
    assert!(withdrawn, "position marked withdrawn");

    // ATTACK: vote AGAIN with the now-empty position — rejected (weight 0).
    assert!(gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).is_err(),
        "voting with a fully-withdrawn (zero-principal) position must be rejected");
}

// OWN-VAULT WITHDRAW vs INSURANCE pool (instruction isolation; closes finding AR's 2nd path): the
// own-vault withdraw (IX 2, process_withdraw) sets `withdrawn=true` and pays out WITHOUT decrementing
// principal. If it could run against the genesis INSURANCE pool, a voter could "exit" via it, leave
// principal intact, and re-vote with phantom capital (finding AR). Guarded three independent ways:
// (a) `if pool.is_insurance() -> reject` up front, (b) the percolator insurance vault is owned by the
// market vault_authority not the pool, so the pool can't sign its transfer, (c) the position is
// mutated only AFTER the payout transfer, so any failure reverts it. Pinned: IX 2 on the genesis
// insurance position is rejected and the position is left fully intact (no phantom withdrawn state).
#[test]
fn own_vault_withdraw_is_rejected_on_an_insurance_pool() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("deposit");

    let attack = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(alice.pubkey(), true),
            AccountMeta::new(pool, false),
            AccountMeta::new(env.position_pda(&alice.pubkey()), false),
            AccountMeta::new(alice_ata, false),
            AccountMeta::new(env.perc_vault, false),
            AccountMeta::new_readonly(spl_token::ID, false),
        ],
        data: vec![2u8], // IX_WITHDRAW (own-vault)
    };
    assert!(env.send(&[attack], &[&alice]).is_err(), "own-vault withdraw must be rejected on an insurance pool");
    let (principal, _start, withdrawn) = env.read_position(&alice.pubkey());
    assert_eq!(principal, amount, "position principal intact after the rejected own-vault withdraw");
    assert!(!withdrawn, "position not retired (no phantom withdrawn state)");
    assert_eq!(env.pool_outstanding(), amount, "pool outstanding intact");
}

// TOP-UP RESETS HOLD-TIME (Sybil-resistance: no early-squat-then-top-up). Vote weight is
// floor(log2(now - start_slot)) * principal. If a top-up did NOT reset start_slot, a whale could
// deposit 1 atom at genesis start, let the age compound, then top up a huge principal right before
// voting and have ALL of it earn the early-join age = inflated weight. insurance_deposit resets
// `position.start_slot = clock` on EVERY deposit, so a late top-up's age clock starts now. The
// existing coverage only checks start_slot after the FIRST deposit; this pins the top-up reset.
#[test]
fn top_up_resets_the_position_start_slot() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let (alice, alice_ata) = new_depositor(&mut env, 2_000_000);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);

    env.insurance_deposit(&alice, &alice_ata, &holding, 1).expect("early small deposit");
    let (_p0, start0, _w0) = env.read_position(&alice.pubkey());

    // Age compounds, then a HUGE top-up much later.
    env.warp_slot(1_000);
    env.insurance_deposit(&alice, &alice_ata, &holding, 1_999_999).expect("late huge top-up");
    let (principal, start1, _w1) = env.read_position(&alice.pubkey());

    assert_eq!(principal, 2_000_000, "principal accumulated across deposits");
    assert_eq!(start1, 1_000, "top-up RESET start_slot to now — the huge late capital earns no early-join age");
    assert!(start1 > start0, "start_slot moved forward (no inherited early-join hold time)");
}

/// Patch ONLY the asset-0 `source_insurance_credit_reserved_total_atoms` lien into the slab,
/// leaving `insurance` / `vault` / the per-domain budgets at their healthy funded values.
/// This faithfully mirrors a live trader insurance-credit reservation: percolator's
/// `update_source_credit_aggregate_totals` (percolator/src/v16.rs:5534) bumps this field WITHOUT
/// touching `header.insurance`, and percolator caps every insurance withdraw at
/// `insurance - reserved` (`market_insurance_withdraw_capacity_view`, v16_program.rs:5129-5135).
/// The subledger's `read_asset0_insurance` (subledger/src/lib.rs:370-376) reads ONLY raw
/// `header.insurance` and never subtracts this lien.
fn set_credit_lien(env: &mut Env, reserved: u128) {
    use percolator::MarketGroupV16HeaderAccount as H;
    let off_res =
        MARKET_GROUP_OFF + core::mem::offset_of!(H, source_insurance_credit_reserved_total_atoms);
    let off_ins = MARKET_GROUP_OFF + core::mem::offset_of!(H, insurance);
    assert_eq!(off_ins, MARKET_GROUP_OFF + 301, "insurance offset drifted from real percolator struct");
    let mut acct = env.svm.get_account(&env.slab).unwrap();
    let insurance = u128::from_le_bytes(acct.data[off_ins..off_ins + 16].try_into().unwrap());
    assert!(reserved <= insurance, "lien cannot exceed insurance (percolator reservation cap v16.rs:6249)");
    acct.data[off_res..off_res + 16].copy_from_slice(&reserved.to_le_bytes());
    env.svm.set_account(env.slab, acct).unwrap();
    // insurance itself is UNCHANGED — that is the whole point: the fund looks full to a raw read.
    let acct2 = env.svm.get_account(&env.slab).unwrap();
    assert_eq!(
        u128::from_le_bytes(acct2.data[off_ins..off_ins + 16].try_into().unwrap()),
        insurance,
        "set_credit_lien must not touch header.insurance"
    );
}

// INSURANCE-CREDIT LIEN — the impairment haircut must price against `insurance - reserved` (regression).
//
// This pins the second over-count channel beyond finding T's vault-vs-insurance offset fix. The subledger
// prices every exit off `read_asset0_insurance` = the RAW `header.insurance` scalar (subledger/src/lib.rs),
// and `payout(POLICY_PRINCIPAL, balance, outstanding, amount)` returns the FULL principal 1:1 whenever
// `balance >= outstanding`. But percolator does NOT make all of `header.insurance` withdrawable: the real
// cap is `insurance - source_insurance_credit_reserved_total_atoms`, a per-trader insurance-credit lien a
// live trader raises WITHOUT moving `header.insurance` (percolator/src/v16.rs:5520), bounded only by
// `<= insurance` (v16.rs:6249) — so it can encumber the very principal the subledger depositors funded.
//
// Without the fix, while a reservation is live the subledger reads the impaired fund as HEALTHY and pays
// the EARLY exiter full principal 1:1, draining `insurance - reserved`; the LATE exiter's tag-57 withdraw
// then reverts (`EngineLockActive`, v16_program.rs:8269) and their principal is stranded — voiding the
// order-independence `impaired_insurance_exit_is_pro_rata` (and finding ER) guarantees. The fix prices the
// haircut against `insurance - reserved` (subledger read at the WITHDRAW site only — NOT the deposit
// share-mint, where subtracting a transient lien would misprice shares). This test pins it: a credit lien
// must produce the SAME pro-rata share for the first and last exiter. (It FAILS pre-fix: the late exiter
// reverts; the CONTROL run with no lien is unaffected, isolating the lien as the cause.)
/// Read `(insurance, reserved, outstanding)` straight from the live slab + pool, the same scalars the
/// subledger reads, so the expected pro-rata is computed from the FORMULA rather than a hand-figure.
/// Doubles as a layout canary: it pins BOTH shipped subledger offsets against the real percolator struct.
fn read_fund_state(env: &Env) -> (u128, u128, u128) {
    use percolator::MarketGroupV16HeaderAccount as H;
    let off_ins = MARKET_GROUP_OFF + core::mem::offset_of!(H, insurance);
    let off_res =
        MARKET_GROUP_OFF + core::mem::offset_of!(H, source_insurance_credit_reserved_total_atoms);
    assert_eq!(off_ins, subledger_program::PERC_INSURANCE_OFFSET, "insurance offset drifted");
    assert_eq!(off_res, subledger_program::PERC_INSURANCE_RESERVED_OFFSET, "reserved offset drifted");
    let acct = env.svm.get_account(&env.slab).unwrap();
    let rd = |o: usize| u128::from_le_bytes(acct.data[o..o + 16].try_into().unwrap());
    (rd(off_ins), rd(off_res), env.pool_outstanding() as u128)
}

#[test]
fn insurance_credit_lien_exit_stays_pro_rata_and_order_independent() {
    // Run a two-depositor (1M each, 2M outstanding) exit under a given asset-0 credit lien. The market
    // is otherwise HEALTHY: insurance == vault == 2M. Returns
    // (alice_payout, bob_withdraw_ok, bob_payout, expected_each) where expected_each is the pro-rata
    // share derived FROM THE SLAB via amount * (insurance - reserved) / outstanding — so the assertions
    // test the fix's formula, not a hand-computed number.
    fn run(reserved_lien: u128) -> (u64, bool, u64, u128) {
        let mut env = Env::new();
        env.init_insurance_pool();
        let amount = 1_000_000u64;
        let (alice, alice_ata) = new_depositor(&mut env, amount);
        let (bob, bob_ata) = new_depositor(&mut env, amount);
        let pool = env.pool;
        let a_hold = create_holding(&mut env, &pool);
        let b_hold = create_holding(&mut env, &pool);
        env.insurance_deposit(&alice, &alice_ata, &a_hold, amount).expect("alice deposit");
        env.insurance_deposit(&bob, &bob_ata, &b_hold, amount).expect("bob deposit");
        assert_eq!(env.pool_outstanding(), 2 * amount, "outstanding = both deposits");
        assert_eq!(env.token_amount(&env.perc_vault.clone()), 2 * amount, "insurance funded by both");

        // A live trader reserves `reserved_lien` of asset-0 insurance credit. header.insurance is
        // UNCHANGED — the fund reads HEALTHY to a raw read — but only `insurance - reserved` is withdrawable.
        if reserved_lien > 0 {
            set_credit_lien(&mut env, reserved_lien);
        }

        // Expected per-depositor pro-rata, from the FORMULA the fix must implement, read live pre-exit.
        let (insurance, reserved, outstanding) = read_fund_state(&env);
        let expected_each = (amount as u128) * insurance.saturating_sub(reserved) / outstanding;

        env.insurance_withdraw(&alice, &alice_ata, &a_hold, &alice, amount).expect("alice (early) exits");
        let alice_payout = env.token_amount(&alice_ata);
        let bob_res = env.insurance_withdraw(&bob, &bob_ata, &b_hold, &bob, amount);
        (alice_payout, bob_res.is_ok(), env.token_amount(&bob_ata), expected_each)
    }

    // CONTROL — no lien. Healthy fund: withdrawable == insurance, each owed the full principal 1:1.
    let (a0, bob_ok0, b0, exp0) = run(0);
    assert_eq!(exp0, 1_000_000, "control: withdrawable == insurance => full principal each");
    assert_eq!(a0 as u128, exp0, "control: early exiter gets the full-principal pro-rata");
    assert!(bob_ok0, "control: late exiter also succeeds");
    assert_eq!(b0 as u128, exp0, "control: late exiter gets the same");

    // TREATMENT — a 1M trader credit lien. Withdrawable = insurance(2M) - reserved(1M) = 1M across 2M of
    // principal => a 50% haircut. With the fix the haircut is priced against `insurance - reserved`, so
    // BOTH depositors take the SAME pro-rata share and neither is stranded, regardless of exit order.
    let (a1, bob_ok1, b1, exp1) = run(1_000_000);
    assert_eq!(exp1, 500_000, "treatment: 1M withdrawable / 2M principal => 50% haircut each");
    assert!(bob_ok1, "fix: late exiter is NOT stranded — the lien-aware haircut leaves budget for them");
    assert_eq!(a1 as u128, exp1, "fix: early exiter takes the pro-rata haircut, not full 1:1");
    assert_eq!(b1 as u128, exp1, "fix: late exiter takes the SAME pro-rata haircut");
    assert_eq!(a1, b1, "fix: order-independent — first and last exiter receive exactly the same");
}

