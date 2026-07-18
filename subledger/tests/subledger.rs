//! End-to-end litesvm tests for the subledger program: real SPL token vault,
//! PDA-signed withdrawals, and both exit policies (principal / with-surplus),
//! including the impaired-pool pro-rata path.

use litesvm::LiteSVM;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    program_option::COption,
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction,
    transaction::Transaction,
};

const OWN_VAULT_DEPOSIT_WINDOW_SLOTS: u64 = u64::MAX;
const OWN_VAULT_DEPOSIT_START_SLOT: u64 = 0;
const OWN_VAULT_BOOTSTRAP_DELAY_SLOTS: u64 = 0;
fn program_id() -> Pubkey {
    subledger_program::id()
}

fn so_path() -> String {
    // workspace target/deploy/subledger_program.so
    format!(
        "{}/../target/deploy/subledger_program.so",
        env!("CARGO_MANIFEST_DIR")
    )
}

struct Env {
    svm: LiteSVM,
    payer: Keypair,
    mint: Pubkey,
    mint_authority: Keypair,
}

impl Env {
    fn new() -> Self {
        let mut svm = LiteSVM::new();
        svm.add_program_from_file(program_id(), so_path()).unwrap();
        let payer = Keypair::new();
        svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
        let mint_authority = Keypair::new();
        let mint = create_mint(&mut svm, &payer, &mint_authority.pubkey());
        Env {
            svm,
            payer,
            mint,
            mint_authority,
        }
    }

    /// Signs with the env payer (fee payer) plus any `extra` signers.
    fn send(&mut self, ixs: &[Instruction], extra: &[&Keypair]) -> Result<(), String> {
        self.svm.expire_blockhash();
        let bh = self.svm.latest_blockhash();
        let payer = clone_kp(&self.payer);
        let mut signers: Vec<&Keypair> = Vec::with_capacity(1 + extra.len());
        signers.push(&payer);
        signers.extend_from_slice(extra);
        let payer_pubkey = self.payer.pubkey();
        let tx = Transaction::new_signed_with_payer(ixs, Some(&payer_pubkey), &signers, bh);
        self.svm.send_transaction(tx).map(|_| ()).map_err(|e| format!("{:?}", e))
    }

    fn token_amount(&self, account: &Pubkey) -> u64 {
        let acc = self.svm.get_account(account).unwrap();
        spl_token::state::Account::unpack(&acc.data).unwrap().amount
    }
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
    let ix = spl_token::instruction::mint_to(&spl_token::ID, mint, dest, &authority.pubkey(), &[], amount).unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[payer, authority],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).unwrap();
}

fn pool_pda(mint: &Pubkey, asset_id: u64, policy: u8) -> Pubkey {
    pool_pda_for_domain(mint, asset_id, policy, 0)
}

fn pool_pda_for_domain(mint: &Pubkey, asset_id: u64, policy: u8, domain: u8) -> Pubkey {
    // Own-vault pools commit to the default market binding (no percolator market).
    let no_market = Pubkey::default();
    let domain = [domain];
    let policy = [policy];
    let window = OWN_VAULT_DEPOSIT_WINDOW_SLOTS.to_le_bytes();
    let start = OWN_VAULT_DEPOSIT_START_SLOT.to_le_bytes();
    let delay = OWN_VAULT_BOOTSTRAP_DELAY_SLOTS.to_le_bytes();
    Pubkey::find_program_address(
        &[
            b"subledger_pool",
            mint.as_ref(),
            &asset_id.to_le_bytes(),
            no_market.as_ref(),
            no_market.as_ref(),
            no_market.as_ref(),
            &policy,
            &domain,
            &window,
            &start,
            &delay,
        ],
        &program_id(),
    )
    .0
}

fn position_pda(pool: &Pubkey, owner: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"subledger_position", pool.as_ref(), owner.as_ref()],
        &program_id(),
    )
    .0
}

fn legacy_master_pool_pda(mint: &Pubkey, asset_id: u64) -> (Pubkey, u8) {
    let no_market = Pubkey::default();
    Pubkey::find_program_address(
        &[
            b"subledger_pool",
            mint.as_ref(),
            &asset_id.to_le_bytes(),
            no_market.as_ref(),
            no_market.as_ref(),
        ],
        &program_id(),
    )
}

#[derive(Clone, Copy, Debug)]
enum HistoricalPoolSeeds {
    Base,
    Market,
    Coin,
    PolicyDomain,
    Window,
    Start,
    Bootstrap,
}

fn historical_pool_pda(
    mint: &Pubkey,
    asset_id: u64,
    policy: u8,
    version: HistoricalPoolSeeds,
) -> (Pubkey, u8) {
    let asset_id = asset_id.to_le_bytes();
    let no_market = Pubkey::default();
    let policy = [policy];
    let domain = [0u8];
    let window = OWN_VAULT_DEPOSIT_WINDOW_SLOTS.to_le_bytes();
    let start = OWN_VAULT_DEPOSIT_START_SLOT.to_le_bytes();
    let delay = OWN_VAULT_BOOTSTRAP_DELAY_SLOTS.to_le_bytes();
    let mut seeds: Vec<&[u8]> = vec![b"subledger_pool", mint.as_ref(), &asset_id];
    match version {
        HistoricalPoolSeeds::Base => {}
        HistoricalPoolSeeds::Market => {
            seeds.extend_from_slice(&[no_market.as_ref(), no_market.as_ref()]);
        }
        HistoricalPoolSeeds::Coin => {
            seeds.extend_from_slice(&[no_market.as_ref(), no_market.as_ref(), no_market.as_ref()]);
        }
        HistoricalPoolSeeds::PolicyDomain => {
            seeds.extend_from_slice(&[
                no_market.as_ref(),
                no_market.as_ref(),
                no_market.as_ref(),
                &policy,
                &domain,
            ]);
        }
        HistoricalPoolSeeds::Window => {
            seeds.extend_from_slice(&[
                no_market.as_ref(),
                no_market.as_ref(),
                no_market.as_ref(),
                &policy,
                &domain,
                &window,
            ]);
        }
        HistoricalPoolSeeds::Start => {
            seeds.extend_from_slice(&[
                no_market.as_ref(),
                no_market.as_ref(),
                no_market.as_ref(),
                &policy,
                &domain,
                &window,
                &start,
            ]);
        }
        HistoricalPoolSeeds::Bootstrap => {
            seeds.extend_from_slice(&[
                no_market.as_ref(),
                no_market.as_ref(),
                no_market.as_ref(),
                &policy,
                &domain,
                &window,
                &start,
                &delay,
            ]);
        }
    }
    Pubkey::find_program_address(&seeds, &program_id())
}

fn historical_pool_data(
    size: usize,
    mint: &Pubkey,
    asset_id: u64,
    vault: &Pubkey,
    outstanding: u64,
    policy: u8,
    bump: u8,
) -> Vec<u8> {
    let mut data = vec![0u8; size];
    data[..8].copy_from_slice(b"SUBPOOL1");
    data[8..40].copy_from_slice(mint.as_ref());
    data[40..48].copy_from_slice(&asset_id.to_le_bytes());
    data[48..80].copy_from_slice(vault.as_ref());
    data[80..88].copy_from_slice(&outstanding.to_le_bytes());
    data[88] = policy;
    data[89] = bump;
    data[90] = 0;
    if size == 216 {
        data[208..216].copy_from_slice(&u64::MAX.to_le_bytes());
    } else if size >= 256 {
        data[240..248].copy_from_slice(&u64::MAX.to_le_bytes());
        data[248..256].copy_from_slice(&OWN_VAULT_DEPOSIT_WINDOW_SLOTS.to_le_bytes());
    }
    data
}

fn historical_position_data(size: usize, pool: &Pubkey, owner: &Pubkey, principal: u64) -> Vec<u8> {
    let mut data = vec![0u8; size];
    data[..8].copy_from_slice(b"SUBPOS01");
    data[8..40].copy_from_slice(pool.as_ref());
    data[40..72].copy_from_slice(owner.as_ref());
    data[72..80].copy_from_slice(&principal.to_le_bytes());
    if size >= 104 {
        data[89..97].copy_from_slice(&1u64.to_le_bytes());
    }
    data
}

fn init_pool_ix(env: &Env, pool: &Pubkey, vault: &Pubkey, asset_id: u64, policy: u8) -> Instruction {
    init_pool_ix_for_domain(env, pool, vault, asset_id, policy, 0)
}

fn init_pool_ix_for_domain(
    env: &Env,
    pool: &Pubkey,
    vault: &Pubkey,
    asset_id: u64,
    policy: u8,
    domain: u8,
) -> Instruction {
    let mut data = vec![0u8]; // IX_INIT_POOL
    data.extend_from_slice(&asset_id.to_le_bytes());
    data.push(policy);
    data.push(domain);
    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(env.mint, false),
            AccountMeta::new(*pool, false),
            AccountMeta::new_readonly(*vault, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    }
}

fn deposit_ix(env: &Env, pool: &Pubkey, owner: &Pubkey, owner_ata: &Pubkey, vault: &Pubkey, amount: u64) -> Instruction {
    let mut data = vec![1u8]; // IX_DEPOSIT
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new(*owner, true),
            AccountMeta::new(*pool, false),
            AccountMeta::new(position_pda(pool, owner), false),
            AccountMeta::new(*owner_ata, false),
            AccountMeta::new(*vault, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    }
}

fn withdraw_ix(
    env: &Env,
    pool: &Pubkey,
    owner: &Pubkey,
    owner_ata: &Pubkey,
    vault: &Pubkey,
) -> Instruction {
    let position = env.svm.get_account(&position_pda(pool, owner)).unwrap();
    let mut data = vec![2u8]; // IX_WITHDRAW
    data.extend_from_slice(&position.data[72..80]); // principal
    if position.data.len() >= 97 {
        data.extend_from_slice(&position.data[89..97]); // last deposit slot
    } else {
        data.extend_from_slice(&0u64.to_le_bytes());
    }
    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new(*owner, true),
            AccountMeta::new(*pool, false),
            AccountMeta::new(position_pda(pool, owner), false),
            AccountMeta::new(*owner_ata, false),
            AccountMeta::new(*vault, false),
            AccountMeta::new_readonly(spl_token::ID, false),
        ],
        data,
    }
}

/// Funds a depositor: airdrop SOL, create their ATA, mint `amount` to it.
fn new_depositor(env: &mut Env, amount: u64) -> (Keypair, Pubkey) {
    let kp = Keypair::new();
    env.svm.airdrop(&kp.pubkey(), 10_000_000_000).unwrap();
    let payer = clone_kp(&env.payer);
    let auth = clone_kp(&env.mint_authority);
    let mint = env.mint;
    let ata = create_token_account(&mut env.svm, &payer, &mint, &kp.pubkey());
    if amount > 0 {
        mint_to(&mut env.svm, &payer, &mint, &auth, &ata, amount);
    }
    (kp, ata)
}

fn clone_kp(kp: &Keypair) -> Keypair {
    Keypair::from_bytes(&kp.to_bytes()).unwrap()
}

// STALE BACKING EXIT DOS: neither the legacy amountless wire nor an exact stale
// snapshot may cross a later top-up. Position PDAs are one-per-owner and a retired
// position cannot be re-created or re-entered, so this would permanently deny that
// backing depositor's future reward participation even while deposits stay open.
#[test]
fn presigned_own_vault_exit_cannot_retire_a_later_backing_top_up() {
    let mut env = Env::new();
    let asset_id = 88;
    let pool = pool_pda_for_domain(&env.mint, asset_id, 1, 1);
    let vault =
        create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool);
    env.send(
        &[init_pool_ix_for_domain(
            &env, &pool, &vault, asset_id, 1, 1,
        )],
        &[],
    )
    .expect("initialize the segregated backing pool");

    let (alice, alice_ata) = new_depositor(&mut env, 6);
    env.send(
        &[deposit_ix(
            &env,
            &pool,
            &alice.pubkey(),
            &alice_ata,
            &vault,
            1,
        )],
        &[&alice],
    )
    .expect("alice deposits the unit covered by the first exit signature");

    env.svm.expire_blockhash();
    let held_blockhash = env.svm.latest_blockhash();
    let payer = clone_kp(&env.payer);
    let exact_exit = withdraw_ix(&env, &pool, &alice.pubkey(), &alice_ata, &vault);
    let mut legacy_exit = exact_exit.clone();
    legacy_exit.data = vec![2u8];
    let withheld_legacy_exit = Transaction::new_signed_with_payer(
        &[legacy_exit],
        Some(&payer.pubkey()),
        &[&payer, &alice],
        held_blockhash,
    );
    let withheld_exact_exit = Transaction::new_signed_with_payer(
        &[exact_exit],
        Some(&payer.pubkey()),
        &[&payer, &alice],
        held_blockhash,
    );
    let top_up = Transaction::new_signed_with_payer(
        &[deposit_ix(
            &env,
            &pool,
            &alice.pubkey(),
            &alice_ata,
            &vault,
            4,
        )],
        Some(&payer.pubkey()),
        &[&payer, &alice],
        held_blockhash,
    );
    env.svm
        .send_transaction(top_up)
        .expect("alice's later four-unit backing top-up lands");
    let position = position_pda(&pool, &alice.pubkey());
    let position_after_top_up = env.svm.get_account(&position).unwrap();
    assert_eq!(
        u64::from_le_bytes(position_after_top_up.data[72..80].try_into().unwrap()),
        5,
    );
    assert_eq!(position_after_top_up.data[88], 0);
    assert_eq!(env.token_amount(&alice_ata), 1);
    assert_eq!(env.token_amount(&vault), 5);

    let stale_result = env.svm.send_transaction(withheld_legacy_exit);
    if stale_result.is_ok() {
        let retired = env.svm.get_account(&position).unwrap();
        assert_eq!(
            u64::from_le_bytes(retired.data[72..80].try_into().unwrap()),
            5,
            "the vulnerable exit reads all later principal",
        );
        assert_eq!(retired.data[88], 1, "the canonical position is retired");
        assert_eq!(env.token_amount(&alice_ata), 6);
        assert!(
            env.send(
                &[deposit_ix(
                    &env,
                    &pool,
                    &alice.pubkey(),
                    &alice_ata,
                    &vault,
                    1,
                )],
                &[&alice],
            )
            .is_err(),
            "a retired backing position has no public re-entry path",
        );
        panic!("a pre-top-up exit authorization retired the later backing position");
    }
    assert!(
        env.svm.send_transaction(withheld_exact_exit).is_err(),
        "the snapshot-bound wire rejects the stale one-unit backing authorization",
    );

    assert_eq!(env.svm.get_account(&position).unwrap(), position_after_top_up);
    assert_eq!(env.token_amount(&alice_ata), 1);
    assert_eq!(env.token_amount(&vault), 5);
    env.send(
        &[deposit_ix(
            &env,
            &pool,
            &alice.pubkey(),
            &alice_ata,
            &vault,
            1,
        )],
        &[&alice],
    )
    .expect("a rejected stale exit leaves the backing position usable");
    assert_eq!(env.token_amount(&alice_ata), 0);
    assert_eq!(env.token_amount(&vault), 6);
}

// UPGRADE LOF PROBE: origin/master created 208-byte pools and derived their PDA
// without the later COIN/policy/schedule seeds. Growing Pool to 272 bytes must not
// make an existing owner-bound vault impossible to sign for. This constructs the
// exact master bytes and exercises a real SPL withdrawal against the current SBF.
#[test]
fn legacy_master_pool_owner_can_withdraw_after_layout_and_seed_upgrade() {
    let mut env = Env::new();
    let asset_id = 77;
    let amount = 123_456u64;
    let (pool, bump) = legacy_master_pool_pda(&env.mint, asset_id);
    let vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool);
    let (owner, owner_ata) = new_depositor(&mut env, 0);
    mint_to(
        &mut env.svm,
        &clone_kp(&env.payer),
        &env.mint,
        &clone_kp(&env.mint_authority),
        &vault,
        amount,
    );

    let mut pool_data = vec![0u8; 208];
    pool_data[..8].copy_from_slice(b"SUBPOOL1");
    pool_data[8..40].copy_from_slice(env.mint.as_ref());
    pool_data[40..48].copy_from_slice(&asset_id.to_le_bytes());
    pool_data[48..80].copy_from_slice(vault.as_ref());
    pool_data[80..88].copy_from_slice(&amount.to_le_bytes());
    pool_data[88] = 0; // POLICY_PRINCIPAL
    pool_data[89] = bump;
    pool_data[90] = 0;
    env.svm
        .set_account(
            pool,
            solana_sdk::account::Account {
                lamports: 10_000_000,
                data: pool_data,
                owner: program_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let position = position_pda(&pool, &owner.pubkey());
    let mut position_data = vec![0u8; 120];
    position_data[..8].copy_from_slice(b"SUBPOS01");
    position_data[8..40].copy_from_slice(pool.as_ref());
    position_data[40..72].copy_from_slice(owner.pubkey().as_ref());
    position_data[72..80].copy_from_slice(&amount.to_le_bytes());
    position_data[89..97].copy_from_slice(&1u64.to_le_bytes());
    env.svm
        .set_account(
            position,
            solana_sdk::account::Account {
                lamports: 10_000_000,
                data: position_data,
                owner: program_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    env.send(
        &[withdraw_ix(&env, &pool, &owner.pubkey(), &owner_ata, &vault)],
        &[&owner],
    )
    .expect("legacy owner withdrawal remains live after upgrade");
    assert_eq!(env.token_amount(&owner_ata), amount);
    assert_eq!(env.token_amount(&vault), 0);
    assert_eq!(env.svm.get_account(&pool).unwrap().data.len(), 208);
}

// Every historical pool layout and both same-size seed transitions must still
// produce a real PDA signature after upgrade. Each case executes a distinct SPL
// transfer through the current SBF program; this is a compatibility matrix, not
// repeated execution of one configuration.
#[test]
fn every_historical_pool_seed_schema_preserves_owner_withdrawal() {
    let cases = [
        ("base-96", 96, 96, HistoricalPoolSeeds::Base),
        ("base-160", 160, 104, HistoricalPoolSeeds::Base),
        ("base-192", 192, 104, HistoricalPoolSeeds::Base),
        ("market-192", 192, 104, HistoricalPoolSeeds::Market),
        ("market-208", 208, 120, HistoricalPoolSeeds::Market),
        ("market-216", 216, 120, HistoricalPoolSeeds::Market),
        ("coin-240", 240, 120, HistoricalPoolSeeds::Coin),
        (
            "policy-domain-240",
            240,
            120,
            HistoricalPoolSeeds::PolicyDomain,
        ),
        ("window-256", 256, 120, HistoricalPoolSeeds::Window),
        ("start-264", 264, 120, HistoricalPoolSeeds::Start),
        ("bootstrap-272", 272, 120, HistoricalPoolSeeds::Bootstrap),
    ];

    for (index, (name, pool_size, position_size, seed_version)) in cases.into_iter().enumerate() {
        let mut env = Env::new();
        let asset_id = 1_000 + index as u64;
        let amount = 10_000 + index as u64;
        let (pool, bump) = historical_pool_pda(&env.mint, asset_id, 0, seed_version);
        let vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool);
        mint_to(
            &mut env.svm,
            &clone_kp(&env.payer),
            &env.mint,
            &clone_kp(&env.mint_authority),
            &vault,
            amount,
        );
        let (owner, owner_ata) = new_depositor(&mut env, 0);
        env.svm
            .set_account(
                pool,
                solana_sdk::account::Account {
                    lamports: 10_000_000,
                    data: historical_pool_data(
                        pool_size, &env.mint, asset_id, &vault, amount, 0, bump,
                    ),
                    owner: program_id(),
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
        env.svm
            .set_account(
                position_pda(&pool, &owner.pubkey()),
                solana_sdk::account::Account {
                    lamports: 10_000_000,
                    data: historical_position_data(position_size, &pool, &owner.pubkey(), amount),
                    owner: program_id(),
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();

        env.send(
            &[withdraw_ix(&env, &pool, &owner.pubkey(), &owner_ata, &vault)],
            &[&owner],
        )
        .unwrap_or_else(|error| panic!("{name} owner withdrawal failed: {error}"));
        assert_eq!(env.token_amount(&owner_ata), amount, "{name}");
        assert_eq!(env.token_amount(&vault), 0, "{name}");
        assert_eq!(env.svm.get_account(&pool).unwrap().data.len(), pool_size);
    }
}

#[test]
fn pre_share_surplus_pool_keeps_original_exit_math_and_rejects_new_deposits() {
    let mut env = Env::new();
    let asset_id = 9_001;
    let principal = 100u64;
    let balance = 150u64;
    let (pool, bump) = historical_pool_pda(&env.mint, asset_id, 1, HistoricalPoolSeeds::Base);
    let vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool);
    mint_to(
        &mut env.svm,
        &clone_kp(&env.payer),
        &env.mint,
        &clone_kp(&env.mint_authority),
        &vault,
        balance,
    );
    let (owner, owner_ata) = new_depositor(&mut env, 0);
    env.svm
        .set_account(
            pool,
            solana_sdk::account::Account {
                lamports: 10_000_000,
                data: historical_pool_data(160, &env.mint, asset_id, &vault, principal, 1, bump),
                owner: program_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    env.svm
        .set_account(
            position_pda(&pool, &owner.pubkey()),
            solana_sdk::account::Account {
                lamports: 10_000_000,
                data: historical_position_data(104, &pool, &owner.pubkey(), principal),
                owner: program_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let (late, late_ata) = new_depositor(&mut env, 1);
    assert!(
        env.send(
            &[deposit_ix(
                &env,
                &pool,
                &late.pubkey(),
                &late_ata,
                &vault,
                1,
            )],
            &[&late],
        )
        .is_err(),
        "a pre-share pool cannot persist new share attribution"
    );
    assert_eq!(env.token_amount(&late_ata), 1);
    assert_eq!(env.token_amount(&vault), balance);

    env.send(
        &[withdraw_ix(&env, &pool, &owner.pubkey(), &owner_ata, &vault)],
        &[&owner],
    )
    .expect("the historical with-surplus owner receives the original pro-rata payout");
    assert_eq!(env.token_amount(&owner_ata), balance);
    assert_eq!(env.token_amount(&vault), 0);
}

// DoS PROBE (non-SPL token-shaped vault at init_pool, sweep tick D): init_pool PERSISTS pool.vault
// (lib.rs:493) after validating its token FIELDS via Account::unpack — but, like the rd freeze just fixed, it
// did NOT check vault.owner == spl_token::ID. unpack verifies bytes, not the owning program. init_pool is
// permissionless (PDA = mint+asset_id), so a front-runner can craft a NON-SPL account with token-shaped bytes
// (mint = mint, owner field = pool PDA) and squat the canonical pool PDA, binding a fake vault — the pool can
// never be re-inited (AlreadyInitialized) and every deposit's token_balance(vault) then rejects the fake, so the
// pool is permanently bricked. init_pool must reject a vault not owned by the SPL Token program.
#[test]
fn init_pool_rejects_a_non_spl_owned_token_shaped_vault_no_front_run_brick() {
    let mut env = Env::new();
    let asset_id = 13;
    let pool = pool_pda(&env.mint, asset_id, 1);

    // SYSTEM-owned account with token-shaped data: mint = env.mint, owner field = the pool PDA. Passes the
    // field checks, fails only on the owning program.
    let fake = spl_token::state::Account {
        mint: env.mint, owner: pool, amount: 0, delegate: COption::None,
        state: spl_token::state::AccountState::Initialized, is_native: COption::None,
        delegated_amount: 0, close_authority: COption::None,
    };
    let mut fake_data = vec![0u8; spl_token::state::Account::LEN];
    spl_token::state::Account::pack(fake, &mut fake_data).unwrap();
    let fake_vault = Pubkey::new_unique();
    env.svm.set_account(fake_vault, solana_sdk::account::Account { lamports: 10_000_000, data: fake_data, owner: solana_sdk::system_program::ID, executable: false, rent_epoch: 0 }).unwrap();

    assert!(
        env.send(&[init_pool_ix(&env, &pool, &fake_vault, asset_id, 1)], &[]).is_err(),
        "init_pool must reject a token-shaped vault not owned by the SPL Token program (else a front-run binds a fake vault and bricks the pool)"
    );

    // The real SPL vault inits fine (the rejected attempt did not squat the pool PDA).
    let real_vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool);
    env.send(&[init_pool_ix(&env, &pool, &real_vault, asset_id, 1)], &[]).expect("real SPL vault accepted");
}

// DoS PROBE: SPL Token preserves a non-native account's close authority when its owner changes.
// A front-runner can therefore prepare an empty vault owned by the future pool PDA while retaining
// a close authority, initialize the deterministic pool against it, and then close the vault. The
// pool account remains initialized and every later deposit is permanently bound to a missing vault.
#[test]
fn init_pool_rejects_a_vault_with_an_external_close_authority() {
    let mut env = Env::new();
    let asset_id = 18;
    let pool = pool_pda(&env.mint, asset_id, 1);
    let attacker = Keypair::new();
    let prepared_vault = create_token_account(
        &mut env.svm,
        &clone_kp(&env.payer),
        &env.mint,
        &attacker.pubkey(),
    );

    let set_close = spl_token::instruction::set_authority(
        &spl_token::ID,
        &prepared_vault,
        Some(&attacker.pubkey()),
        spl_token::instruction::AuthorityType::CloseAccount,
        &attacker.pubkey(),
        &[],
    )
    .unwrap();
    env.send(&[set_close], &[&attacker]).expect("set close authority");

    let set_owner = spl_token::instruction::set_authority(
        &spl_token::ID,
        &prepared_vault,
        Some(&pool),
        spl_token::instruction::AuthorityType::AccountOwner,
        &attacker.pubkey(),
        &[],
    )
    .unwrap();
    env.send(&[set_owner], &[&attacker]).expect("transfer token ownership to pool PDA");

    let prepared = spl_token::state::Account::unpack(
        &env.svm.get_account(&prepared_vault).unwrap().data,
    )
    .unwrap();
    assert_eq!(prepared.owner, pool);
    assert_eq!(prepared.close_authority, COption::Some(attacker.pubkey()));

    assert!(
        env.send(
            &[init_pool_ix(&env, &pool, &prepared_vault, asset_id, 1)],
            &[],
        )
        .is_err(),
        "init_pool must reject a vault an external authority can close"
    );

    let honest_vault =
        create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool);
    env.send(
        &[init_pool_ix(&env, &pool, &honest_vault, asset_id, 1)],
        &[],
    )
    .expect("rejected attack must leave the canonical pool available");
}

// SOURCE-OF-TRUTH OFFSET CANARY (sweep): genesis-vote + residual-distributor read the subledger Position
// (principal=vote weight, start_slot=reward tenure, shares=rd share-value points) and Pool (outstanding=quorum
// denominator) by HARDCODED byte offsets, cross-pinned in their offsets.rs to the subledger's EXPORTED consts
// (POS_*_OFF / POOL_OUTSTANDING_PRINCIPAL_OFF). But those exported consts are SEPARATE declarations — the actual
// Position/Pool serialize uses inline offsets. So a serialize reorder that didn't also update the const would
// pass every cross-pin yet silently break the real cross-program read (vote-weight/quorum miscompute -> capture/
// LOF). This is the missing link: pin the EXPORTED consts against the REAL serialized layout by reading a live
// deposit-created Position/Pool at those consts and asserting the known values.
#[test]
fn exported_position_and_pool_offset_consts_match_the_real_serialized_layout() {
    use subledger_program as sub;
    let mut env = Env::new();
    env.svm.set_sysvar(&solana_sdk::clock::Clock { slot: 100, unix_timestamp: 100, ..Default::default() });
    let asset_id = 17;
    let pool = pool_pda(&env.mint, asset_id, 1);
    let vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool);
    env.send(&[init_pool_ix(&env, &pool, &vault, asset_id, 1)], &[]).expect("init WITH_SURPLUS pool"); // policy 1 -> shares
    let (alice, alice_ata) = new_depositor(&mut env, 12_345);
    env.send(&[deposit_ix(&env, &pool, &alice.pubkey(), &alice_ata, &vault, 12_345)], &[&alice]).unwrap();

    let pos = position_pda(&pool, &alice.pubkey());
    let d = env.svm.get_account(&pos).unwrap().data;
    let rdk = |o: usize| Pubkey::new_from_array(d[o..o + 32].try_into().unwrap());
    assert_eq!(rdk(sub::POS_POOL_OFF), pool, "POS_POOL_OFF");
    assert_eq!(rdk(sub::POS_OWNER_OFF), alice.pubkey(), "POS_OWNER_OFF (vote-power owner)");
    assert_eq!(u64::from_le_bytes(d[sub::POS_PRINCIPAL_OFF..sub::POS_PRINCIPAL_OFF + 8].try_into().unwrap()), 12_345, "POS_PRINCIPAL_OFF (vote weight)");
    assert_eq!(u64::from_le_bytes(d[sub::POS_START_SLOT_OFF..sub::POS_START_SLOT_OFF + 8].try_into().unwrap()), 100, "POS_START_SLOT_OFF (tenure)");
    assert_eq!(d[sub::POS_WITHDRAWN_OFF], 0, "POS_WITHDRAWN_OFF (flag — not withdrawn)");
    assert_eq!(u128::from_le_bytes(d[sub::POS_SHARES_OFF..sub::POS_SHARES_OFF + 16].try_into().unwrap()), 12_345u128 * 1_000_000, "POS_SHARES_OFF (rd share-value points source)");

    let pd = env.svm.get_account(&pool).unwrap().data;
    assert_eq!(u64::from_le_bytes(pd[sub::POOL_OUTSTANDING_PRINCIPAL_OFF..sub::POOL_OUTSTANDING_PRINCIPAL_OFF + 8].try_into().unwrap()), 12_345, "POOL_OUTSTANDING_PRINCIPAL_OFF (quorum denominator)");
}

#[test]
fn principal_policy_healthy_pays_principal_and_keeps_surplus() {
    let mut env = Env::new();
    let asset_id = 1;
    let pool = pool_pda(&env.mint, asset_id, 0);
    let vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool);

    env.send(&[init_pool_ix(&env, &pool, &vault, asset_id, 0)], &[])
        .expect("init pool");

    let (alice, alice_ata) = new_depositor(&mut env, 60);
    let (bob, bob_ata) = new_depositor(&mut env, 40);
    env.send(&[deposit_ix(&env, &pool, &alice.pubkey(), &alice_ata, &vault, 60)], &[&alice]).unwrap();
    env.send(&[deposit_ix(&env, &pool, &bob.pubkey(), &bob_ata, &vault, 40)], &[&bob]).unwrap();
    assert_eq!(env.token_amount(&vault), 100, "principal deposited");

    // Simulate local fees/yield: 50 extra tokens land in the vault.
    let auth = clone_kp(&env.mint_authority);
    mint_to(&mut env.svm, &clone_kp(&env.payer), &env.mint, &auth, &vault, 50);
    assert_eq!(env.token_amount(&vault), 150);

    // Healthy (balance 150 >= outstanding 100): principal policy returns principal only.
    env.send(&[withdraw_ix(&env, &pool, &alice.pubkey(), &alice_ata, &vault)], &[&alice]).unwrap();
    assert_eq!(env.token_amount(&alice_ata), 60, "alice gets principal, not surplus");

    env.send(&[withdraw_ix(&env, &pool, &bob.pubkey(), &bob_ata, &vault)], &[&bob]).unwrap();
    assert_eq!(env.token_amount(&bob_ata), 40, "bob gets principal");

    // The 50 surplus stays in the pool (no further claimant under principal policy).
    assert_eq!(env.token_amount(&vault), 50, "surplus retained in pool");

    // Double-withdraw is rejected.
    assert!(env.send(&[withdraw_ix(&env, &pool, &alice.pubkey(), &alice_ata, &vault)], &[&alice]).is_err());
}

#[test]
fn with_surplus_policy_returns_yield_pro_rata() {
    let mut env = Env::new();
    let asset_id = 2;
    let pool = pool_pda(&env.mint, asset_id, 1);
    let vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool);

    env.send(&[init_pool_ix(&env, &pool, &vault, asset_id, 1)], &[])
        .expect("init pool");

    let (alice, alice_ata) = new_depositor(&mut env, 60);
    let (bob, bob_ata) = new_depositor(&mut env, 40);
    env.send(&[deposit_ix(&env, &pool, &alice.pubkey(), &alice_ata, &vault, 60)], &[&alice]).unwrap();
    env.send(&[deposit_ix(&env, &pool, &bob.pubkey(), &bob_ata, &vault, 40)], &[&bob]).unwrap();

    let auth = clone_kp(&env.mint_authority);
    mint_to(&mut env.svm, &clone_kp(&env.payer), &env.mint, &auth, &vault, 50); // balance 150
    assert_eq!(env.token_amount(&vault), 150);

    // With-surplus, share-based: ~pro-rata against the live balance (both deposited before the
    // surplus, so shares ∝ principal). alice ~150*60/100 = 90, minus 1 unit of virtual-offset dust.
    env.send(&[withdraw_ix(&env, &pool, &alice.pubkey(), &alice_ata, &vault)], &[&alice]).unwrap();
    assert_eq!(env.token_amount(&alice_ata), 89, "alice gets principal + surplus share (1 dust to the inflation offset)");
    // Bob receives his own floored claim; Alice's remainder cannot accrue to him.
    env.send(&[withdraw_ix(&env, &pool, &bob.pubkey(), &bob_ata, &vault)], &[&bob]).unwrap();
    assert_eq!(env.token_amount(&bob_ata), 59, "bob gets his floored surplus share without prior-exit dust");
    assert_eq!(env.token_amount(&vault), 2, "both floor remainders stay in the protocol pool");
}

#[test]
fn with_surplus_rounding_reserve_does_not_block_the_next_minimum_deposit() {
    let mut env = Env::new();
    let asset_id = 3;
    let pool = pool_pda(&env.mint, asset_id, 1);
    let vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool);
    env.send(&[init_pool_ix(&env, &pool, &vault, asset_id, 1)], &[])
        .expect("init with-surplus pool");

    let (alice, alice_ata) = new_depositor(&mut env, 60);
    let (bob, bob_ata) = new_depositor(&mut env, 40);
    env.send(&[deposit_ix(&env, &pool, &alice.pubkey(), &alice_ata, &vault, 60)], &[&alice])
        .expect("alice deposit");
    env.send(&[deposit_ix(&env, &pool, &bob.pubkey(), &bob_ata, &vault, 40)], &[&bob])
        .expect("bob deposit");
    let auth = clone_kp(&env.mint_authority);
    mint_to(&mut env.svm, &clone_kp(&env.payer), &env.mint, &auth, &vault, 50);
    env.send(&[withdraw_ix(&env, &pool, &alice.pubkey(), &alice_ata, &vault)], &[&alice])
        .expect("alice exits");
    env.send(&[withdraw_ix(&env, &pool, &bob.pubkey(), &bob_ata, &vault)], &[&bob])
        .expect("bob exits");
    assert_eq!(env.token_amount(&vault), 2, "two floor atoms remain as protocol reserve");

    let (carol, carol_ata) = new_depositor(&mut env, 1);
    env.send(&[deposit_ix(&env, &pool, &carol.pubkey(), &carol_ata, &vault, 1)], &[&carol])
        .expect("the empty epoch's reserve cannot block a fresh minimum deposit");
    env.send(&[withdraw_ix(&env, &pool, &carol.pubkey(), &carol_ata, &vault)], &[&carol])
        .expect("fresh minimum position remains withdrawable");
    assert_eq!(env.token_amount(&carol_ata), 1);
    assert_eq!(env.token_amount(&vault), 2, "protocol reserve remains segregated");
}

// TENURE-FAIRNESS (finding HT): the branch claims (lib.rs) POLICY_WITH_SURPLUS is SHARE-based so a
// LATE depositor cannot claim surplus that accrued before it joined. The INSURANCE path honours that
// (shares), but the OWN-VAULT path used pro-rata-by-principal, so a late depositor captured a pro-rata
// slice of the PRE-EXISTING surplus — an LOF for the early depositor. Shares must be applied to
// own-vault WITH_SURPLUS too: a deposit priced by the live balance can only redeem surplus accrued
// during its own tenure.
#[test]
fn with_surplus_late_depositor_cannot_capture_pre_existing_surplus() {
    let mut env = Env::new();
    let asset_id = 7;
    let pool = pool_pda(&env.mint, asset_id, 1);
    let vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool);
    env.send(&[init_pool_ix(&env, &pool, &vault, asset_id, 1)], &[]).expect("init pool"); // WITH_SURPLUS

    // Alice is the sole depositor while a 100 surplus accrues (balance 100 -> 200).
    let (alice, alice_ata) = new_depositor(&mut env, 100);
    env.send(&[deposit_ix(&env, &pool, &alice.pubkey(), &alice_ata, &vault, 100)], &[&alice]).unwrap();
    let auth = clone_kp(&env.mint_authority);
    mint_to(&mut env.svm, &clone_kp(&env.payer), &env.mint, &auth, &vault, 100);
    assert_eq!(env.token_amount(&vault), 200);

    // BOB joins LATE (after the surplus already exists).
    let (bob, bob_ata) = new_depositor(&mut env, 100);
    env.send(&[deposit_ix(&env, &pool, &bob.pubkey(), &bob_ata, &vault, 100)], &[&bob]).unwrap();

    // Alice must keep her FULL pre-bob surplus: 100 principal + 100 surplus = 200. (Pro-rata would
    // give her only 300*100/200 = 150, letting the late bob capture 50 of her surplus.)
    env.send(&[withdraw_ix(&env, &pool, &alice.pubkey(), &alice_ata, &vault)], &[&alice]).unwrap();
    assert_eq!(env.token_amount(&alice_ata), 199, "alice keeps her full pre-bob surplus (1 dust to the inflation offset); the late bob cannot capture it");
    // Bob cannot absorb Alice's floor remainder by exiting last.
    env.send(&[withdraw_ix(&env, &pool, &bob.pubkey(), &bob_ata, &vault)], &[&bob]).unwrap();
    assert_eq!(env.token_amount(&bob_ata), 99, "the late depositor redeems its own floored claim without prior-exit dust");
    assert_eq!(env.token_amount(&vault), 2, "both floor remainders stay in the protocol pool");
}

// FIRST-DEPOSITOR INFLATION ATTACK (finding HU): an own-vault pool's vault is a plain SPL token
// account ANYONE can donate into. A 1-atom first depositor donates to inflate the share price and
// skim a later depositor's rounding. The VIRTUAL_SHARES offset must bound this so the attacker can
// never extract more than it put in (deposit + donation).
#[test]
fn first_depositor_inflation_attack_cannot_skim_a_later_depositor() {
    let mut env = Env::new();
    let asset_id = 9;
    let pool = pool_pda(&env.mint, asset_id, 1);
    let vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool);
    env.send(&[init_pool_ix(&env, &pool, &vault, asset_id, 1)], &[]).expect("init pool"); // WITH_SURPLUS

    // Attacker is the FIRST depositor with 1 atom, then DONATES 3_000_000 directly into the vault.
    let (attacker, attacker_ata) = new_depositor(&mut env, 1);
    env.send(&[deposit_ix(&env, &pool, &attacker.pubkey(), &attacker_ata, &vault, 1)], &[&attacker]).unwrap();
    let donation = 3_000_000u64;
    set_token_amount(&mut env.svm, &vault, 1 + donation); // inflate the share price
    // Victim deposits.
    let victim_deposit = 4_000_000u64;
    let (victim, victim_ata) = new_depositor(&mut env, victim_deposit);
    env.send(&[deposit_ix(&env, &pool, &victim.pubkey(), &victim_ata, &vault, victim_deposit)], &[&victim]).unwrap();

    // Attacker withdraws — must NOT profit: out <= (1 deposited + donation). Without the offset the
    // attacker would skim the victim's rounding (out > in).
    env.send(&[withdraw_ix(&env, &pool, &attacker.pubkey(), &attacker_ata, &vault)], &[&attacker]).unwrap();
    let attacker_out = env.token_amount(&attacker_ata);
    assert!(attacker_out <= 1 + donation, "inflation attacker cannot extract more than deposit+donation: {attacker_out}");
    // Victim recovers ~its principal (not materially skimmed).
    env.send(&[withdraw_ix(&env, &pool, &victim.pubkey(), &victim_ata, &vault)], &[&victim]).unwrap();
    let victim_out = env.token_amount(&victim_ata);
    assert!(victim_out >= victim_deposit - 10, "victim recovers ~its principal, not skimmed: {victim_out}");
}

// PUBLIC LOF: nonzero shares are not sufficient if their immediate redemption value is zero. A first
// depositor can donate directly to an own-vault pool, making a later one-atom deposit mint positive dust
// shares that the program accepts but retires for a zero payout even though no market loss occurred.
#[test]
fn a_deposit_with_zero_immediate_share_value_is_rejected_without_moving_principal() {
    let mut env = Env::new();
    let asset_id = 10;
    let pool = pool_pda(&env.mint, asset_id, 1);
    let vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool);
    env.send(&[init_pool_ix(&env, &pool, &vault, asset_id, 1)], &[])
        .expect("init with-surplus pool");

    let (attacker, attacker_ata) = new_depositor(&mut env, 5);
    env.send(
        &[deposit_ix(
            &env,
            &pool,
            &attacker.pubkey(),
            &attacker_ata,
            &vault,
            1,
        )],
        &[&attacker],
    )
    .expect("attacker seeds one atom");
    let donate = spl_token::instruction::transfer(
        &spl_token::ID,
        &attacker_ata,
        &vault,
        &attacker.pubkey(),
        &[],
        4,
    )
    .unwrap();
    env.send(&[donate], &[&attacker])
        .expect("attacker donates four atoms through SPL Token");
    assert_eq!(env.token_amount(&vault), 5);

    let (victim, victim_ata) = new_depositor(&mut env, 1);
    let deposit = env.send(
        &[deposit_ix(
            &env,
            &pool,
            &victim.pubkey(),
            &victim_ata,
            &vault,
            1,
        )],
        &[&victim],
    );
    assert!(
        deposit.is_err(),
        "a share purchase with zero immediate value must reject before transfer"
    );

    assert_eq!(
        env.token_amount(&victim_ata),
        1,
        "rejected deposit leaves the victim's principal untouched"
    );
    assert_eq!(
        env.token_amount(&vault),
        5,
        "rejected deposit does not donate principal into the pool"
    );
    assert!(
        env.svm
            .get_account(&position_pda(&pool, &victim.pubkey()))
            .is_none(),
        "rejected first deposit creates no dead position"
    );
}

// PUBLIC LOF: a donation can make a positive share mint materially under-value the new deposit.
// The attacker cannot recover the donation, but can grief a victim into losing half of a deposit
// to virtual-share rounding unless the program rejects before moving the victim's principal.
#[test]
fn a_deposit_with_material_immediate_rounding_loss_is_rejected_before_transfer() {
    let mut env = Env::new();
    let asset_id = 12;
    let pool = pool_pda(&env.mint, asset_id, 1);
    let vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool);
    env.send(&[init_pool_ix(&env, &pool, &vault, asset_id, 1)], &[])
        .expect("init with-surplus pool");

    let donation = 100_000_000;
    let (attacker, attacker_ata) = new_depositor(&mut env, donation + 1);
    env.send(
        &[deposit_ix(
            &env,
            &pool,
            &attacker.pubkey(),
            &attacker_ata,
            &vault,
            1,
        )],
        &[&attacker],
    )
    .expect("attacker seeds one atom");
    let donate = spl_token::instruction::transfer(
        &spl_token::ID,
        &attacker_ata,
        &vault,
        &attacker.pubkey(),
        &[],
        donation,
    )
    .unwrap();
    env.send(&[donate], &[&attacker])
        .expect("attacker donates through SPL Token");
    assert_eq!(env.token_amount(&vault), donation + 1);

    // This buys one share whose immediate redemption is only 50 atoms. Positive shares alone do
    // not keep the accepted deposit within the documented one-atom rounding bound.
    let (victim, victim_ata) = new_depositor(&mut env, 100);
    let deposit = env.send(
        &[deposit_ix(
            &env,
            &pool,
            &victim.pubkey(),
            &victim_ata,
            &vault,
            100,
        )],
        &[&victim],
    );
    assert!(
        deposit.is_err(),
        "a deposit with material immediate rounding loss must reject"
    );
    assert_eq!(env.token_amount(&victim_ata), 100);
    assert_eq!(env.token_amount(&vault), donation + 1);
    assert!(
        env.svm
            .get_account(&position_pda(&pool, &victim.pubkey()))
            .is_none(),
        "rejected deposit creates no dead position"
    );
}

// LOF PROBE (finding HB zero-share guard, sweep tick B): the test above pins the SKIM bound (value not
// stolen); the distinct, UNTESTED safety property is the zero-share REJECT. With a balance >> total_shares
// (an attacker first-deposits 1 atom then donates to inflate the price), a small victim deposit rounds to 0
// shares. WITHOUT the guard the deposit would be ACCEPTED, mint 0 shares, and the victim's principal would be
// transferred into the vault to be redeemed by the existing shareholders — a clean total LOSS of that deposit.
// The guard (lib.rs:596 own-vault / :980 slab) rejects BEFORE the token transfer, so funds never move. The
// lock-out is also a recoverable, self-defeating grief: a deposit above the threshold mints fair shares.
#[test]
fn a_deposit_that_rounds_to_zero_shares_is_rejected_before_any_transfer_no_silent_loss() {
    let mut env = Env::new();
    let asset_id = 11;
    let pool = pool_pda(&env.mint, asset_id, 1);
    let vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool);
    env.send(&[init_pool_ix(&env, &pool, &vault, asset_id, 1)], &[]).expect("init pool"); // WITH_SURPLUS

    // Attacker first-deposits 1 atom (-> 1_000_000 shares = VIRTUAL_SHARES), then donates 1e9 into the vault.
    let (attacker, attacker_ata) = new_depositor(&mut env, 1);
    env.send(&[deposit_ix(&env, &pool, &attacker.pubkey(), &attacker_ata, &vault, 1)], &[&attacker]).unwrap();
    set_token_amount(&mut env.svm, &vault, 1_000_000_000); // balance >> total_shares: price inflated ~1e9/1e6

    // Victim tries a 100-atom deposit: shares = 100*(1e6+1e6)/(1e9+1) = 0 (floor) -> MUST be rejected.
    let (victim, victim_ata) = new_depositor(&mut env, 10_000_100);
    let small = env.send(&[deposit_ix(&env, &pool, &victim.pubkey(), &victim_ata, &vault, 100)], &[&victim]);
    assert!(small.is_err(), "a deposit that would mint 0 shares must be rejected (no silent principal donation)");
    // CRITICAL LOF pin: the reject is atomic — the victim's 100 atoms never left its ATA.
    assert_eq!(env.token_amount(&victim_ata), 10_000_100, "rejected deposit transfers NOTHING — no principal lost to a 0-share mint");

    // Recoverable + fair: an amount aligned to the live share price mints 2,000 shares and has an
    // immediate value of 1,000,000, exactly one atom below the 1,000,001-atom deposit.
    env.send(
        &[deposit_ix(
            &env,
            &pool,
            &victim.pubkey(),
            &victim_ata,
            &vault,
            1_000_001,
        )],
        &[&victim],
    )
    .expect("bounded-rounding deposit mints shares");
    assert_eq!(env.token_amount(&victim_ata), 9_000_099);
    env.send(&[withdraw_ix(&env, &pool, &victim.pubkey(), &victim_ata, &vault)], &[&victim]).unwrap();
    assert_eq!(
        env.token_amount(&victim_ata),
        10_000_099,
        "accepted deposit loses only the documented one atom of entry-rounding dust"
    );
}

fn set_token_amount(svm: &mut LiteSVM, account: &Pubkey, amount: u64) {
    let mut acc = svm.get_account(account).unwrap();
    let mut state = spl_token::state::Account::unpack(&acc.data).unwrap();
    state.amount = amount;
    spl_token::state::Account::pack(state, &mut acc.data).unwrap();
    svm.set_account(*account, acc).unwrap();
}

#[test]
fn impaired_pool_is_pro_rata_and_order_independent() {
    let mut env = Env::new();
    let asset_id = 3;
    let pool = pool_pda(&env.mint, asset_id, 0);
    let vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool);

    env.send(&[init_pool_ix(&env, &pool, &vault, asset_id, 0)], &[])
        .expect("init pool");

    let (alice, alice_ata) = new_depositor(&mut env, 60);
    let (bob, bob_ata) = new_depositor(&mut env, 40);
    env.send(&[deposit_ix(&env, &pool, &alice.pubkey(), &alice_ata, &vault, 60)], &[&alice]).unwrap();
    env.send(&[deposit_ix(&env, &pool, &bob.pubkey(), &bob_ata, &vault, 40)], &[&bob]).unwrap();
    assert_eq!(env.token_amount(&vault), 100);

    // Impair the pool: a 50% market loss leaves only 50 in the vault against 100
    // outstanding principal.
    set_token_amount(&mut env.svm, &vault, 50);

    // Alice withdraws first: pro-rata 50 * 60 / 100 = 30 (a 50% haircut).
    env.send(&[withdraw_ix(&env, &pool, &alice.pubkey(), &alice_ata, &vault)], &[&alice]).unwrap();
    assert_eq!(env.token_amount(&alice_ata), 30, "alice takes her pro-rata 50% haircut");

    // Bob withdraws second: full principal retired from outstanding keeps the ratio,
    // so bob gets the same 50% — 20 of 40 — order-independent, no bank run.
    env.send(&[withdraw_ix(&env, &pool, &bob.pubkey(), &bob_ata, &vault)], &[&bob]).unwrap();
    assert_eq!(env.token_amount(&bob_ata), 20, "bob takes the same 50% haircut, not a worse one");
    assert_eq!(env.token_amount(&vault), 0, "impaired balance fully and fairly distributed");
}

#[test]
fn non_owner_cannot_withdraw_another_position() {
    let mut env = Env::new();
    let asset_id = 4;
    let pool = pool_pda(&env.mint, asset_id, 0);
    let vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool);
    env.send(&[init_pool_ix(&env, &pool, &vault, asset_id, 0)], &[]).unwrap();

    let (alice, alice_ata) = new_depositor(&mut env, 60);
    env.send(&[deposit_ix(&env, &pool, &alice.pubkey(), &alice_ata, &vault, 60)], &[&alice]).unwrap();

    // An attacker signs and points the withdraw at alice's position PDA, paying to
    // their own ATA. The position PDA is keyed by alice's pubkey, so the attacker's
    // derived position differs and the owner check rejects it.
    let (attacker, attacker_ata) = new_depositor(&mut env, 0);
    let mut ix = withdraw_ix(&env, &pool, &alice.pubkey(), &attacker_ata, &vault);
    ix.accounts[0] = AccountMeta::new(attacker.pubkey(), true); // attacker signs
    assert!(
        env.send(&[ix], &[&attacker]).is_err(),
        "only the position owner can withdraw it"
    );
    assert_eq!(env.token_amount(&attacker_ata), 0);
}

// OWNER-SIGNED PAYOUT REDIRECT: checking the position signer is not enough when
// the pool PDA signs the SPL transfer. A malicious transaction builder can retain
// the real owner's signature but substitute an attacker-owned destination unless
// the program binds the payout token account back to that owner.
#[test]
fn owner_signed_withdraw_cannot_redirect_to_a_foreign_token_account() {
    let mut env = Env::new();
    let asset_id = 4_004;
    let amount = 60u64;
    let pool = pool_pda(&env.mint, asset_id, 0);
    let vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool);
    env.send(&[init_pool_ix(&env, &pool, &vault, asset_id, 0)], &[])
        .unwrap();

    let (victim, victim_ata) = new_depositor(&mut env, amount);
    env.send(
        &[deposit_ix(
            &env,
            &pool,
            &victim.pubkey(),
            &victim_ata,
            &vault,
            amount,
        )],
        &[&victim],
    )
    .unwrap();
    let (_attacker, attacker_ata) = new_depositor(&mut env, 0);
    let position = position_pda(&pool, &victim.pubkey());
    let pool_before = env.svm.get_account(&pool).unwrap();
    let position_before = env.svm.get_account(&position).unwrap();
    let vault_before = env.svm.get_account(&vault).unwrap();

    assert!(
        env.send(
            &[withdraw_ix(
                &env,
                &pool,
                &victim.pubkey(),
                &attacker_ata,
                &vault,
            )],
            &[&victim],
        )
        .is_err(),
        "a valid owner signature must not authorize payout to an attacker-owned account"
    );
    assert_eq!(env.svm.get_account(&pool).unwrap(), pool_before);
    assert_eq!(env.svm.get_account(&position).unwrap(), position_before);
    assert_eq!(env.svm.get_account(&vault).unwrap(), vault_before);
    assert_eq!(env.token_amount(&victim_ata), 0);
    assert_eq!(env.token_amount(&attacker_ata), 0);

    env.send(
        &[withdraw_ix(
            &env,
            &pool,
            &victim.pubkey(),
            &victim_ata,
            &vault,
        )],
        &[&victim],
    )
    .expect("owner still exits to their own token account after the rejected redirect");
    assert_eq!(env.token_amount(&victim_ata), amount);
    assert_eq!(env.token_amount(&vault), 0);
}

// Anti-theft boundary: init_pool must reject a vault that is NOT owned by the pool
// PDA. If it accepted an attacker-owned vault, the attacker could stand up a pool,
// lure a victim's deposit (tag 1 transfers owner -> pool.vault), and then drain the
// funds directly via SPL as the vault owner — while the program's withdraw (which
// signs as the pool PDA) could never move them. The vault must be pool-PDA-owned so
// only this program can move funds out.
#[test]
fn init_pool_rejects_a_vault_not_owned_by_the_pool() {
    let mut env = Env::new();
    let asset_id = 0u64;
    let pool = pool_pda(&env.mint, asset_id, 0);

    // A vault owned by an ATTACKER rather than the pool PDA.
    let attacker = Pubkey::new_unique();
    let rogue_vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &attacker);
    assert!(
        env.send(&[init_pool_ix(&env, &pool, &rogue_vault, asset_id, 0)], &[]).is_err(),
        "init_pool must reject a vault not owned by the pool PDA"
    );

    // The canonical (pool-PDA-owned) vault is accepted.
    let good_vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool);
    env.send(&[init_pool_ix(&env, &pool, &good_vault, asset_id, 0)], &[])
        .expect("a pool-PDA-owned vault is accepted");
}

// CROSS-POOL DRAIN (pool-isolation half of the owner/pool guard): withdraw checks BOTH position.owner ==
// owner AND position.pool == pool_account (lib.rs process_withdraw). non_owner_cannot_withdraw_another_
// position pins the owner half; this pins the POOL half. Without it, an attacker who holds a real position
// in pool-A (their own deposit) could pass that position alongside pool-B's pool + vault and withdraw
// against pool-B — using pool-A principal to drain a DIFFERENT pool's vault (another depositor's funds).
#[test]
fn cannot_drain_a_foreign_pool_with_a_position_from_another_pool() {
    let mut env = Env::new();
    // Two independent own-vault pools (same mint, different asset_ids), each with its own vault.
    let pool_a = pool_pda(&env.mint, 1, 0);
    let vault_a = create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool_a);
    env.send(&[init_pool_ix(&env, &pool_a, &vault_a, 1, 0)], &[]).expect("init pool A");
    let pool_b = pool_pda(&env.mint, 2, 0);
    let vault_b = create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool_b);
    env.send(&[init_pool_ix(&env, &pool_b, &vault_b, 2, 0)], &[]).expect("init pool B");

    // Attacker holds a 1M position in pool-A; a victim funds pool-B with 1M.
    let (attacker, attacker_ata) = new_depositor(&mut env, 1_000_000);
    env.send(&[deposit_ix(&env, &pool_a, &attacker.pubkey(), &attacker_ata, &vault_a, 1_000_000)], &[&attacker]).unwrap();
    let (victim, victim_ata) = new_depositor(&mut env, 1_000_000);
    env.send(&[deposit_ix(&env, &pool_b, &victim.pubkey(), &victim_ata, &vault_b, 1_000_000)], &[&victim]).unwrap();
    assert_eq!(env.token_amount(&vault_b), 1_000_000, "victim's pool-B vault funded");

    // ATTACK: withdraw against pool-B + vault-B, but pass the attacker's pool-A POSITION (principal 1M).
    let mut attack = withdraw_ix(&env, &pool_a, &attacker.pubkey(), &attacker_ata, &vault_a);
    attack.accounts[1] = AccountMeta::new(pool_b, false);
    attack.accounts[4] = AccountMeta::new(vault_b, false);
    assert!(env.send(&[attack], &[&attacker]).is_err(),
        "withdraw must reject a position bound to a DIFFERENT pool (cross-pool drain)");
    assert_eq!(env.token_amount(&vault_b), 1_000_000, "pool-B vault untouched — victim's funds safe");
    assert_eq!(env.token_amount(&attacker_ata), 0, "attacker gained nothing from the cross-pool attempt");

    // The attacker's own pool-A position is intact: they can still exit pool-A for exactly their principal.
    env.send(&[withdraw_ix(&env, &pool_a, &attacker.pubkey(), &attacker_ata, &vault_a)], &[&attacker]).expect("attacker exits their OWN pool A");
    assert_eq!(env.token_amount(&attacker_ata), 1_000_000, "attacker recovers only their own pool-A principal, never pool-B's");
}

// LIVENESS/LOF BOUNDARY: share redemption is mathematically
// `shares * (balance + 1) / (total_shares + VIRTUAL_SHARES)`. Each input and the
// final quotient fit their serialized types here, but the direct u128 product does
// not. A valid deposit must not become permanently unwithdrawable because only an
// intermediate representation overflows.
#[test]
fn large_with_surplus_deposit_can_withdraw_without_intermediate_mul_overflow() {
    let mut env = Env::new();
    let asset_id = 21;
    let pool = pool_pda(&env.mint, asset_id, 1);
    let vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool);
    env.send(&[init_pool_ix(&env, &pool, &vault, asset_id, 1)], &[])
        .expect("init with-surplus backing pool");

    // First-deposit shares are amount * 1_000_000 and fit u128. Multiplying those
    // shares by the live balance exceeds u128, although the exact redemption is
    // simply the original u64 amount.
    let amount = 20_000_000_000_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    env.send(
        &[deposit_ix(&env, &pool, &alice.pubkey(), &alice_ata, &vault, amount)],
        &[&alice],
    )
    .expect("large public backing deposit");
    assert_eq!(env.token_amount(&alice_ata), 0);
    assert_eq!(env.token_amount(&vault), amount);

    env.send(
        &[withdraw_ix(&env, &pool, &alice.pubkey(), &alice_ata, &vault)],
        &[&alice],
    )
    .expect("representable redemption must not overflow an intermediate product");

    assert_eq!(env.token_amount(&alice_ata), amount);
    assert_eq!(env.token_amount(&vault), 0);
}

#[test]
fn large_with_surplus_second_deposit_can_mint_representable_shares() {
    let mut env = Env::new();
    let asset_id = 22;
    let pool = pool_pda(&env.mint, asset_id, 1);
    let vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &env.mint, &pool);
    env.send(&[init_pool_ix(&env, &pool, &vault, asset_id, 1)], &[])
        .expect("init with-surplus backing pool");

    let amount = 20_000_000_000_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let (bob, bob_ata) = new_depositor(&mut env, amount);
    env.send(
        &[deposit_ix(&env, &pool, &alice.pubkey(), &alice_ata, &vault, amount)],
        &[&alice],
    )
    .expect("first large deposit");
    env.send(
        &[deposit_ix(&env, &pool, &bob.pubkey(), &bob_ata, &vault, amount)],
        &[&bob],
    )
    .expect("second deposit's representable share quotient must not overflow its product");

    env.send(
        &[withdraw_ix(&env, &pool, &alice.pubkey(), &alice_ata, &vault)],
        &[&alice],
    )
    .expect("first depositor exits");
    env.send(
        &[withdraw_ix(&env, &pool, &bob.pubkey(), &bob_ata, &vault)],
        &[&bob],
    )
    .expect("second depositor exits");
    assert_eq!(env.token_amount(&alice_ata), amount);
    assert_eq!(env.token_amount(&bob_ata), amount);
    assert_eq!(env.token_amount(&vault), 0);
}
