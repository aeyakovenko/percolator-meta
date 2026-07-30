//! Real-percolator litesvm end-to-end test for the non-custodial insurance
//! deposit / vote / exit flow.
//!
//! Proves, against the REAL percolator binary
//! (the Cargo-pinned `target/deploy/percolator_prog.so`, loaded into LiteSVM):
//!
//! 1. A user deposits into market-0 INSURANCE through the `subledger` program (the
//!    subledger pool PDA is asset-0's insurance authority + operator). Funds land in
//!    the Percolator insurance vault and a subledger position records
//!    `owner, principal, start_slot`.
//! 2. `genesis-vote` reads that subledger position principal and the
//!    pool's `outstanding_principal` to weight a vote.
//! 3. The user does a principal-only, owner-authorized exit through the subledger
//!    and gets their principal back. Non-owner exits and over-principal exits fail.
//!
//! Market-0 setup starts under a temporary signer and executes Subledger's real
//! pool-cosigned `accept_operator` path. That CPI rotates the insurance roles and
//! asset admin to the pool and seals its market generation before any deposit. The
//! real Percolator binary then validates every TopUp/Withdraw CPI against that state.

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
fn gv_init_data(delay_slots: u64, start_slot: u64) -> Vec<u8> {
    let mut data = vec![0u8];
    data.extend_from_slice(&delay_slots.to_le_bytes());
    data.extend_from_slice(&start_slot.to_le_bytes());
    data
}

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
    let pinned = format!("{}/../target/deploy/percolator_prog.so", env!("CARGO_MANIFEST_DIR"));
    assert!(std::path::Path::new(&pinned).exists(), "missing Cargo-pinned Percolator SBF at {pinned}");
    pinned
}
fn clone_kp(kp: &Keypair) -> Keypair {
    Keypair::from_bytes(&kp.to_bytes()).unwrap()
}

const ASSET_ID: u64 = 0;
const POLICY_PRINCIPAL: u8 = 0;
const POLICY_WITH_SURPLUS: u8 = 1;
const DOMAIN_INSURANCE: u8 = 0;
const DEFAULT_GENESIS_DEPOSIT_WINDOW_SLOTS: u64 = 1_512_000;
const DEFAULT_GENESIS_DEPOSIT_START_SLOT: u64 = 0;
const DEFAULT_GENESIS_BOOTSTRAP_DELAY_SLOTS: u64 = 38_880_000;
const OWN_VAULT_DEPOSIT_WINDOW_SLOTS: u64 = u64::MAX;
const OWN_VAULT_DEPOSIT_START_SLOT: u64 = 0;
const OWN_VAULT_BOOTSTRAP_DELAY_SLOTS: u64 = 0;

fn insurance_pool_pda_with_schedule(
    mint: &Pubkey,
    coin_mint: &Pubkey,
    slab: &Pubkey,
    policy: u8,
    deposit_window_slots: u64,
    deposit_start_slot: u64,
    bootstrap_delay_slots: u64,
) -> Pubkey {
    let policy_seed = [policy];
    let domain_seed = [DOMAIN_INSURANCE];
    Pubkey::find_program_address(
        &[
            b"subledger_pool",
            mint.as_ref(),
            &ASSET_ID.to_le_bytes(),
            slab.as_ref(),
            perc_id().as_ref(),
            coin_mint.as_ref(),
            &policy_seed,
            &domain_seed,
            &deposit_window_slots.to_le_bytes(),
            &deposit_start_slot.to_le_bytes(),
            &bootstrap_delay_slots.to_le_bytes(),
        ],
        &sub_id(),
    )
    .0
}

fn cross_backing_pool_pda_with_schedule(
    mint: &Pubkey,
    coin_mint: &Pubkey,
    slab: &Pubkey,
    policy: u8,
    deposit_window_slots: u64,
    deposit_start_slot: u64,
    bootstrap_delay_slots: u64,
) -> Pubkey {
    let policy_seed = [policy];
    let domain_seed = [DOMAIN_INSURANCE];
    Pubkey::find_program_address(
        &[
            b"subledger_pool",
            mint.as_ref(),
            &ASSET_ID.to_le_bytes(),
            slab.as_ref(),
            perc_id().as_ref(),
            coin_mint.as_ref(),
            &policy_seed,
            &domain_seed,
            &deposit_window_slots.to_le_bytes(),
            &deposit_start_slot.to_le_bytes(),
            &bootstrap_delay_slots.to_le_bytes(),
            b"cross-backing",
        ],
        &sub_id(),
    )
    .0
}

fn legacy_master_insurance_pool_pda(
    mint: &Pubkey,
    slab: &Pubkey,
    asset_id: u64,
) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[
            b"subledger_pool",
            mint.as_ref(),
            &asset_id.to_le_bytes(),
            slab.as_ref(),
            perc_id().as_ref(),
        ],
        &sub_id(),
    )
}

fn cross_backing_ledger_pda(pool: &Pubkey, domain: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"subledger_backing_ledger",
            pool.as_ref(),
            &domain.to_le_bytes(),
        ],
        &sub_id(),
    )
    .0
}

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
    market_admin: Keypair,
    slab: Pubkey,
    vault_authority: Pubkey,
    perc_vault: Pubkey,
    pool: Pubkey,
    deposit_window_slots: u64,
    bootstrap_start_slot: u64,
    bootstrap_delay_slots: u64,
}

impl Env {
    fn new() -> Self {
        Self::new_for_policy(POLICY_PRINCIPAL)
    }

    fn new_cross_backing() -> Self {
        Self::new_for_policy_with_bootstrap_schedule_and_cross_backing(
            POLICY_PRINCIPAL,
            DEFAULT_GENESIS_DEPOSIT_WINDOW_SLOTS,
            DEFAULT_GENESIS_DEPOSIT_START_SLOT,
            DEFAULT_GENESIS_BOOTSTRAP_DELAY_SLOTS,
            true,
        )
    }

    fn new_for_policy(pool_policy: u8) -> Self {
        Self::new_for_policy_with_bootstrap_schedule(
            pool_policy,
            DEFAULT_GENESIS_DEPOSIT_WINDOW_SLOTS,
            DEFAULT_GENESIS_DEPOSIT_START_SLOT,
            DEFAULT_GENESIS_BOOTSTRAP_DELAY_SLOTS,
        )
    }

    fn new_for_policy_with_window(pool_policy: u8, deposit_window_slots: u64) -> Self {
        Self::new_for_policy_with_schedule(pool_policy, deposit_window_slots, 100)
    }

    fn new_for_policy_with_schedule(
        pool_policy: u8,
        deposit_window_slots: u64,
        deposit_start_slot: u64,
    ) -> Self {
        Self::new_for_policy_with_bootstrap_schedule(
            pool_policy,
            deposit_window_slots,
            deposit_start_slot,
            DEFAULT_GENESIS_BOOTSTRAP_DELAY_SLOTS,
        )
    }

    fn new_for_policy_with_bootstrap_schedule(
        pool_policy: u8,
        deposit_window_slots: u64,
        bootstrap_start_slot: u64,
        bootstrap_delay_slots: u64,
    ) -> Self {
        Self::new_for_policy_with_bootstrap_schedule_and_cross_backing(
            pool_policy,
            deposit_window_slots,
            bootstrap_start_slot,
            bootstrap_delay_slots,
            false,
        )
    }

    fn new_for_policy_with_bootstrap_schedule_and_cross_backing(
        pool_policy: u8,
        deposit_window_slots: u64,
        bootstrap_start_slot: u64,
        bootstrap_delay_slots: u64,
        cross_backing: bool,
    ) -> Self {
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
        let market_admin = Keypair::new();
        svm.airdrop(&market_admin.pubkey(), 1_000_000_000)
            .unwrap();

        // The market slab is chosen first; the pool PDA commits to it (finding Q).
        let slab = Pubkey::new_unique();

        // The subledger insurance pool PDA: asset-0 insurance authority + operator,
        // bound to (mint, asset_id, market_slab, percolator_program, coin_mint,
        // policy, domain, deposit_window_slots, bootstrap_start_slot,
        // bootstrap_delay_slots).
        let pool = if cross_backing {
            cross_backing_pool_pda_with_schedule(
                &mint,
                &coin_mint,
                &slab,
                pool_policy,
                deposit_window_slots,
                bootstrap_start_slot,
                bootstrap_delay_slots,
            )
        } else {
            insurance_pool_pda_with_schedule(
                &mint,
                &coin_mint,
                &slab,
                pool_policy,
                deposit_window_slots,
                bootstrap_start_slot,
                bootstrap_delay_slots,
            )
        };

        // Build the real Live market-0 slab under a temporary signer, then run
        // the same pool-cosigned custody grant used by production.
        let init_slot = 100u64;
        let slab_data = make_live_market(&slab, &mint, &market_admin.pubkey(), init_slot);
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
            market_admin,
            slab,
            vault_authority,
            perc_vault,
            pool,
            deposit_window_slots,
            bootstrap_start_slot,
            bootstrap_delay_slots,
        }
    }

    fn gv_config_pda(&self) -> Pubkey {
        gv_config_pda_for_schedule(
            &self.coin_mint,
            &self.pool,
            self.bootstrap_delay_slots,
            self.bootstrap_start_slot,
        )
    }

    fn bootstrap_end_slot(&self) -> u64 {
        self.bootstrap_start_slot
            .checked_add(self.bootstrap_delay_slots)
            .expect("test bootstrap schedule must fit")
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

    fn append_deposit_position_snapshot(&self, data: &mut Vec<u8>, owner: &Pubkey) {
        let position = self.svm.get_account(&self.position_pda(owner));
        if let Some(position) = position.filter(|account| {
            account.owner == sub_id() && account.data.len() >= 97
        }) {
            data.extend_from_slice(&position.data[72..80]);
            data.extend_from_slice(&position.data[89..97]);
            data.extend_from_slice(&position.data[80..88]);
        } else {
            data.extend_from_slice(&[0u8; 24]);
        }
    }

    // ---- subledger ----

    fn init_insurance_pool(&mut self) {
        self.init_insurance_pool_policy(POLICY_PRINCIPAL);
    }

    fn init_insurance_pool_policy(&mut self, policy: u8) {
        self.init_insurance_pool_policy_with_window(policy, None);
    }

    fn init_insurance_pool_policy_with_window(&mut self, policy: u8, window_slots: Option<u64>) {
        let start_slot = window_slots.map(|_| 100);
        self.init_insurance_pool_policy_with_schedule(policy, window_slots, start_slot);
    }

    fn init_insurance_pool_policy_with_schedule(
        &mut self,
        policy: u8,
        window_slots: Option<u64>,
        start_slot: Option<u64>,
    ) {
        let mut data = vec![3u8]; // IX_INIT_INSURANCE_POOL
        data.extend_from_slice(&ASSET_ID.to_le_bytes());
        data.push(policy);
        if let Some(window_slots) = window_slots {
            assert_eq!(window_slots, self.deposit_window_slots);
            assert_eq!(start_slot, Some(self.bootstrap_start_slot));
            data.extend_from_slice(&window_slots.to_le_bytes());
            data.extend_from_slice(
                &start_slot
                    .expect("custom deposit window requires an explicit start slot")
                    .to_le_bytes(),
            );
            data.extend_from_slice(&self.bootstrap_delay_slots.to_le_bytes());
        }
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
                AccountMeta::new_readonly(self.gv_config_pda(), false),
                AccountMeta::new_readonly(self.coin_mint, false),
            ],
            data,
        };
        self.send(&[ix], &[]).expect("init insurance pool");
        self.grant_insurance_pool();
    }

    fn init_cross_backing_genesis_pool(&mut self) {
        let mut data = vec![14u8]; // IX_INIT_CROSS_BACKING_GENESIS_POOL
        data.extend_from_slice(&ASSET_ID.to_le_bytes());
        data.push(POLICY_PRINCIPAL);
        data.extend_from_slice(&self.deposit_window_slots.to_le_bytes());
        data.extend_from_slice(&self.bootstrap_start_slot.to_le_bytes());
        data.extend_from_slice(&self.bootstrap_delay_slots.to_le_bytes());
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
                AccountMeta::new_readonly(self.gv_config_pda(), false),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new(cross_backing_ledger_pda(&self.pool, 0), false),
                AccountMeta::new(cross_backing_ledger_pda(&self.pool, 1), false),
            ],
            data,
        };
        self.send(&[ix], &[])
            .expect("init cross-backing genesis pool");
        self.grant_insurance_pool();
    }

    fn grant_insurance_pool(&mut self) {
        let market_admin = clone_kp(&self.market_admin);
        let ix = Instruction {
            program_id: sub_id(),
            accounts: vec![
                AccountMeta::new_readonly(market_admin.pubkey(), true),
                AccountMeta::new(self.pool, false),
                AccountMeta::new(self.slab, false),
                AccountMeta::new_readonly(perc_id(), false),
            ],
            data: vec![7u8], // IX_ACCEPT_OPERATOR
        };
        self.send(&[ix], &[&market_admin])
            .expect("grant market custody to insurance pool");
        let grant_slot = self.svm.get_sysvar::<Clock>().slot;
        let pool_data = self.svm.get_account(&self.pool).unwrap().data;
        assert_eq!(
            pool_data[272] & 2,
            2,
            "the production first-grant path seals the pool generation",
        );
        let grant_slot_offset = match pool_data.len() {
            329 => 321,
            size if size >= 361 => 353,
            size => panic!("pool layout {size} has no custody grant slot"),
        };
        assert_eq!(
            u64::from_le_bytes(
                pool_data[grant_slot_offset..grant_slot_offset + 8]
                    .try_into()
                    .unwrap(),
            ),
            grant_slot + 1,
            "the pool records its first custody grant slot plus one",
        );
        self.warp_slot(grant_slot + 1);
    }

    fn cross_backing_deposit(
        &mut self,
        owner: &Keypair,
        owner_ata: &Pubkey,
        holding: &Pubkey,
        amount: u64,
    ) -> Result<(), String> {
        let mut data = vec![4u8]; // IX_INSURANCE_DEPOSIT
        data.extend_from_slice(&amount.to_le_bytes());
        self.append_deposit_position_snapshot(&mut data, &owner.pubkey());
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
                AccountMeta::new(cross_backing_ledger_pda(&self.pool, 0), false),
                AccountMeta::new(cross_backing_ledger_pda(&self.pool, 1), false),
                AccountMeta::new_readonly(perc_id(), false),
                AccountMeta::new_readonly(spl_token::ID, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data,
        };
        self.send(&[ix], &[owner])
    }

    fn cross_backing_withdraw(
        &mut self,
        owner: &Keypair,
        owner_ata: &Pubkey,
        holding: &Pubkey,
        amount: u64,
    ) -> Result<(), String> {
        let (expected_principal, expected_start_slot, _) = self.read_position(&owner.pubkey());
        let expected_action_nonce = self.position_action_nonce(&owner.pubkey());
        let mut data = vec![5u8]; // IX_INSURANCE_WITHDRAW
        data.extend_from_slice(&amount.to_le_bytes());
        data.extend_from_slice(&expected_principal.to_le_bytes());
        data.extend_from_slice(&expected_start_slot.to_le_bytes());
        data.extend_from_slice(&expected_action_nonce.to_le_bytes());
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
                AccountMeta::new_readonly(self.vault_authority, false),
                AccountMeta::new(cross_backing_ledger_pda(&self.pool, 0), false),
                AccountMeta::new(cross_backing_ledger_pda(&self.pool, 1), false),
                AccountMeta::new_readonly(perc_id(), false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data,
        };
        self.send(&[ix], &[owner])
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
        self.append_deposit_position_snapshot(&mut data, &owner.pubkey());
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
        let (expected_principal, expected_start_slot, _) = self.read_position(&owner.pubkey());
        let expected_action_nonce = self.position_action_nonce(&owner.pubkey());
        let mut data = vec![5u8]; // IX_INSURANCE_WITHDRAW
        data.extend_from_slice(&amount.to_le_bytes());
        data.extend_from_slice(&expected_principal.to_le_bytes());
        data.extend_from_slice(&expected_start_slot.to_le_bytes());
        data.extend_from_slice(&expected_action_nonce.to_le_bytes());
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

    fn position_shares(&self, owner: &Pubkey) -> u128 {
        let acc = self.svm.get_account(&self.position_pda(owner)).unwrap();
        u128::from_le_bytes(acc.data[104..120].try_into().unwrap())
    }

    fn position_action_nonce(&self, owner: &Pubkey) -> u64 {
        let acc = self.svm.get_account(&self.position_pda(owner)).unwrap();
        u64::from_le_bytes(acc.data[80..88].try_into().unwrap())
    }

    fn position_share_generation(&self, owner: &Pubkey) -> u64 {
        let acc = self.svm.get_account(&self.position_pda(owner)).unwrap();
        let mut generation = [0u8; 8];
        generation[..5].copy_from_slice(&acc.data[99..104]);
        u64::from_le_bytes(generation)
    }

    fn pool_outstanding(&self) -> u64 {
        let acc = self.svm.get_account(&self.pool).unwrap();
        u64::from_le_bytes(acc.data[80..88].try_into().unwrap())
    }
    fn pool_total_shares(&self) -> u128 {
        let acc = self.svm.get_account(&self.pool).unwrap();
        u128::from_le_bytes(acc.data[192..208].try_into().unwrap())
    }

    fn pool_share_generation(&self) -> u64 {
        let acc = self.svm.get_account(&self.pool).unwrap();
        let mut generation = [0u8; 8];
        generation[..5].copy_from_slice(&acc.data[91..96]);
        u64::from_le_bytes(generation)
    }
}

fn translate_funded_pool_to_legacy(
    env: &mut Env,
    owner: &Pubkey,
    legacy_asset_id: u64,
) -> Pubkey {
    let current_pool = env.pool;
    let current_pool_account = env.svm.get_account(&current_pool).unwrap();
    let current_position = env.position_pda(owner);
    let current_position_account = env.svm.get_account(&current_position).unwrap();
    let (legacy_pool, legacy_bump) =
        legacy_master_insurance_pool_pda(&env.mint, &env.slab, legacy_asset_id);

    let mut legacy_pool_data = current_pool_account.data[..208].to_vec();
    legacy_pool_data[40..48].copy_from_slice(&legacy_asset_id.to_le_bytes());
    legacy_pool_data[89] = legacy_bump;
    env.svm
        .set_account(
            legacy_pool,
            Account {
                lamports: current_pool_account.lamports,
                data: legacy_pool_data,
                owner: sub_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let legacy_position = Pubkey::find_program_address(
        &[
            b"subledger_position",
            legacy_pool.as_ref(),
            owner.as_ref(),
        ],
        &sub_id(),
    )
    .0;
    let mut legacy_position_data = current_position_account.data;
    legacy_position_data[8..40].copy_from_slice(legacy_pool.as_ref());
    env.svm
        .set_account(
            legacy_position,
            Account {
                lamports: current_position_account.lamports,
                data: legacy_position_data,
                owner: sub_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    // Historical market init copied this pool PDA into all asset-0 authority
    // fields, regardless of the subledger's unvalidated metadata asset_id.
    let mut slab_account = env.svm.get_account(&env.slab).unwrap();
    let mut profile = percolator_prog::state::read_asset_oracle_profile(&slab_account.data, 0)
        .expect("read asset-0 profile");
    let legacy_authority = legacy_pool.to_bytes();
    profile.insurance_authority = legacy_authority;
    profile.insurance_operator = legacy_authority;
    profile.backing_bucket_authority = legacy_authority;
    profile.oracle_authority = legacy_authority;
    profile.asset_admin = legacy_authority;
    percolator_prog::state::write_asset_oracle_profile(&mut slab_account.data, 0, &profile)
        .expect("write historical asset-0 authority");
    env.svm.set_account(env.slab, slab_account).unwrap();
    env.pool = legacy_pool;
    legacy_pool
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
    let mut env = Env::new_for_policy_with_bootstrap_schedule(POLICY_PRINCIPAL, 1_200, 0, 1_200);
    env.init_insurance_pool_policy_with_schedule(POLICY_PRINCIPAL, Some(1_200), Some(0));
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
    env.warp_slot(1124);
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

// POST-DEADLINE VOTE RACE: trigger becomes permissionless at bootstrap_end_slot,
// so new backing must be closed at that exact boundary. Otherwise a depositor can
// wait for the six-month result, refresh old weight or retract and switch proposals
// before a trigger lands. Retraction must remain live so the vote lock never traps
// principal after the deadline.
#[test]
fn bootstrap_deadline_closes_new_backing_but_keeps_retract_and_exit_live() {
    let mut env = Env::new_for_policy_with_bootstrap_schedule(POLICY_PRINCIPAL, 100, 100, 1_000);
    env.init_insurance_pool_policy_with_schedule(POLICY_PRINCIPAL, Some(100), Some(100));
    let ve = setup_vote(&mut env);
    let (_, proposal_a) =
        create_and_register_proposal(&mut env, &ve, 1, &Pubkey::new_unique());
    let (_, proposal_b) =
        create_and_register_proposal(&mut env, &ve, 2, &Pubkey::new_unique());

    let (alice, alice_ata) = new_depositor(&mut env, 1);
    let (bob, bob_ata) = new_depositor(&mut env, 1);
    let pool = env.pool;
    let alice_holding = create_holding(&mut env, &pool);
    let bob_holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &alice_holding, 1)
        .expect("alice deposits during the window");
    env.insurance_deposit(&bob, &bob_ata, &bob_holding, 1)
        .expect("bob deposits during the window");

    env.warp_slot(env.bootstrap_end_slot() - 1);
    gv_vote(&mut env, &ve, &alice, &proposal_a, 1).expect("alice backs before the deadline");
    env.warp_slot(env.bootstrap_end_slot());

    gv_vote(&mut env, &ve, &alice, &proposal_a, 2)
        .expect("post-deadline retract remains the owner escape hatch");
    assert!(
        gv_vote(&mut env, &ve, &bob, &proposal_b, 1).is_err(),
        "a previously idle depositor cannot add support after voting closes"
    );
    assert!(
        gv_vote(&mut env, &ve, &alice, &proposal_b, 1).is_err(),
        "a depositor cannot retract and switch proposals after voting closes"
    );
    assert_eq!(
        env.svm
            .get_account(&env.position_pda(&alice.pubkey()))
            .unwrap()
            .data[97],
        0,
        "the rejected re-back cannot restore alice's vote lock"
    );

    env.insurance_withdraw(&alice, &alice_ata, &alice_holding, &alice, 1)
        .expect("alice exits after retracting");
    env.insurance_withdraw(&bob, &bob_ata, &bob_holding, &bob, 1)
        .expect("the rejected late voter exits without a lock");
    assert_eq!(env.token_amount(&alice_ata), 1);
    assert_eq!(env.token_amount(&bob_ata), 1);
}

// VOTE-TIME WEIGHT THEFT (free winner-take-all capture): vote weight is live principal read from
// the voter's SUBLEDGER POSITION. If the vote bound the position to the voter loosely, a tiny-stake attacker
// could present a WHALE's position and cast the whale's principal onto the attacker's own proposal — seizing
// the 100%-of-supply mint with someone else's capital, for ~free. The defense is the canonical-PDA bind
// (lib.rs:588-593): expected_sub_pos = PDA(["subledger_position", config.subledger_pool, VOTER]), so the only
// position a signer can vote with is THEIR OWN. This pins it end-to-end against the real subledger + gv: mallory
// (tiny) presenting alice's (whale) position is rejected (InvalidSeeds), and mallory's own position still votes.
#[test]
fn gv_vote_cannot_borrow_another_voters_position_to_steal_weight() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);

    let (whale, whale_ata) = new_depositor(&mut env, 980_000); // the capital whose weight the attacker covets
    let (mallory, mallory_ata) = new_depositor(&mut env, 1); // a 1-atom attacker stake
    let pool = env.pool;
    let w_hold = create_holding(&mut env, &pool);
    let m_hold = create_holding(&mut env, &pool);
    env.insurance_deposit(&whale, &whale_ata, &w_hold, 980_000).expect("whale deposit");
    env.insurance_deposit(&mallory, &mallory_ata, &m_hold, 1).expect("mallory deposit");

    let mallory_dest = Pubkey::new_unique();
    let (_dist_proposal, gv_proposal) = create_and_register_proposal(&mut env, &ve, 1, &mallory_dest);
    env.warp_slot(1124);

    // ATTACK: mallory signs, but substitutes the WHALE's position account (index 4) to cast the whale's weight.
    let mallory_ballot =
        Pubkey::find_program_address(&[b"gv_ballot", ve.gv_config.as_ref(), mallory.pubkey().as_ref()], &gv_id()).0;
    let mut steal = gv_vote_ix(&env, &ve, &mallory.pubkey(), &gv_proposal, 1);
    assert_eq!(steal.accounts[2].pubkey, mallory_ballot);
    steal.accounts[4] = AccountMeta::new(env.position_pda(&whale.pubkey()), false);
    assert!(
        env.send(&[steal], &[&mallory]).is_err(),
        "voting with another depositor's position must be rejected (the PDA bind ties the position to the signer)"
    );

    // CONTROL: mallory voting with their OWN (1-atom) position is accepted — the bind blocks theft, not voting.
    gv_vote(&mut env, &ve, &mallory, &gv_proposal, 1).expect("mallory may vote with her own position");
    // And the proposal carries only mallory's 1-atom principal — the whale's 980k weight was NOT captured.
    let pv = env.svm.get_account(&gv_proposal).unwrap();
    let support_principal = u64::from_le_bytes(pv.data[88..96].try_into().unwrap());
    assert_eq!(support_principal, 1, "only mallory's own 1 atom backs the proposal — no borrowed whale weight");
}

// insurance_deposit routes funds user -> holding -> Percolator's domain budgets (pool-signed).
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

// WITHDRAW TRANSIT (redeemed-principal routed through an attacker account): the exit moves percolator insurance ->
// holding -> depositor. The holding must be the pool-PDA-owned transit; a non-pool holding would land the redeemed
// principal in an attacker's account mid-flight. The fail-fast guard is `holding.owner == pool PDA` (matching the
// deposit's). The deposit's non-pool-holding rejection is pinned (insurance_deposit_rejects_a_non_pool_holding); the
// withdraw's — the one that carries principal OUT — was not. This pins it (rejected fail-fast, position intact),
// then the honest exit via the pool holding succeeds.
#[test]
fn insurance_withdraw_rejects_a_non_pool_holding() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let (alice, alice_ata) = new_depositor(&mut env, 1_000_000);
    let pool = env.pool;
    let legit_holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &legit_holding, 1_000_000).expect("alice deposit");
    assert_eq!(env.pool_outstanding(), 1_000_000, "deposited");

    // A token account of the correct mint owned by an ATTACKER, not the pool PDA — used as the withdraw transit.
    let attacker = Pubkey::new_unique();
    let rogue_holding = Pubkey::new_unique();
    env.svm.set_account(rogue_holding, solana_sdk::account::Account {
        lamports: 1_000_000_000,
        data: token_account_data(&env.mint, &attacker, 0),
        owner: spl_token::ID, executable: false, rent_epoch: 0,
    }).unwrap();

    // ATTACK: exit through the attacker-owned holding. Must be rejected (holding.owner == pool fail-fast, before
    // the percolator CPI — which would also reject a non-operator-owned destination as the backstop).
    assert!(
        env.insurance_withdraw(&alice, &alice_ata, &rogue_holding, &alice, 1_000_000).is_err(),
        "withdraw must reject a holding not owned by the pool PDA — no routing the redeemed principal through an attacker account"
    );
    assert_eq!(env.pool_outstanding(), 1_000_000, "position intact — the rejected withdraw retired nothing");
    assert_eq!(env.token_amount(&alice_ata), 0, "alice's ata unchanged — the rogue withdraw paid nothing");

    // The honest exit (pool-owned holding) recovers her principal.
    env.insurance_withdraw(&alice, &alice_ata, &legit_holding, &alice, 1_000_000).expect("honest withdraw via the pool holding");
    assert_eq!(env.token_amount(&alice_ata), 1_000_000, "alice recovers her principal via the canonical pool holding");
    assert_eq!(env.pool_outstanding(), 0, "position retired by the honest exit");
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

fn create_canonical_pool_holding(env: &mut Env) -> Pubkey {
    let holding = Pubkey::find_program_address(
        &[env.pool.as_ref(), spl_token::ID.as_ref(), env.mint.as_ref()],
        &ATA_PROGRAM_ID,
    )
    .0;
    env.svm
        .set_account(
            holding,
            Account {
                lamports: 1_000_000,
                data: token_account_data(&env.mint, &env.pool, 0),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    holding
}

/// Slab base offset of the percolator `MarketGroupV16` header
/// from the pinned wrapper API. Never duplicate this number in the canary.
const MARKET_GROUP_OFF: usize = percolator_prog::constants::MARKET_GROUP_OFF;

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
    assert_eq!(subledger_program::PERC_MARKET_GROUP_OFFSET, percolator_prog::constants::MARKET_GROUP_OFF,
        "Percolator wrapper growth shifted the market-group base");
    // Pin the SUBLEDGER's shipped src constant against the real struct too. The functional haircut tests
    // below set vault == insurance (a consistent loss), so they CANNOT distinguish a src offset that
    // accidentally reads vault@749 instead of insurance@765 — only this assertion catches a regression of
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

fn activate_external_asset(env: &mut Env, authority: &Pubkey) {
    let mut acct = env.svm.get_account(&env.slab).unwrap();
    acct.data.resize(
        percolator_prog::state::market_account_len_for_capacity(2).unwrap(),
        0,
    );
    let mut profile = percolator_prog::state::activate_dynamic_asset_slot(
        &mut acct.data,
        1,
        101,
        1_000_000,
        authority.to_bytes(),
        authority.to_bytes(),
        authority.to_bytes(),
        authority.to_bytes(),
    )
    .expect("append asset 1 through the pinned engine transition");
    profile.asset_admin = authority.to_bytes();
    percolator_prog::state::write_asset_oracle_profile(&mut acct.data, 1, &profile)
        .expect("write asset-1 authority profile");
    env.svm.set_account(env.slab, acct).unwrap();
}

fn asset_insurance_remaining(env: &Env, asset_index: usize) -> u128 {
    let data = env.svm.get_account(&env.slab).unwrap().data;
    let (_, group) = percolator_prog::state::read_market(&data).unwrap();
    let long = asset_index * 2;
    let short = long + 1;
    group.insurance_domain_budget[long]
        .checked_sub(group.insurance_domain_spent[long])
        .unwrap()
        .checked_add(
            group.insurance_domain_budget[short]
                .checked_sub(group.insurance_domain_spent[short])
                .unwrap(),
        )
        .unwrap()
        .min(group.insurance)
}

// Model a valid 1M loss isolated to asset 0 while asset 1's 2M insurance remains intact.
// This mirrors the exact engine counters after both asset-0 domains spend 500k and the paid
// loss leaves the shared vault; all aggregate invariants continue to reconcile.
fn impair_asset0_with_external_insurance(env: &mut Env) {
    use percolator::{EngineAssetSlotV16Account as E, MarketGroupV16HeaderAccount as H};
    let mut acct = env.svm.get_account(&env.slab).unwrap();
    let header = MARKET_GROUP_OFF;
    let engine0 = header
        + core::mem::size_of::<H>()
        + percolator_prog::constants::ASSET_ORACLE_WRAPPER_LEN;
    let write_u128 = |data: &mut [u8], offset: usize, value: u128| {
        data[offset..offset + 16].copy_from_slice(&value.to_le_bytes());
    };

    write_u128(
        &mut acct.data,
        header + core::mem::offset_of!(H, vault),
        3_000_000,
    );
    write_u128(
        &mut acct.data,
        header + core::mem::offset_of!(H, insurance),
        3_000_000,
    );
    write_u128(
        &mut acct.data,
        header + core::mem::offset_of!(H, insurance_domain_budget_remaining_total),
        3_000_000,
    );
    write_u128(
        &mut acct.data,
        engine0 + core::mem::offset_of!(E, insurance_domain_spent_long),
        500_000,
    );
    write_u128(
        &mut acct.data,
        engine0 + core::mem::offset_of!(E, insurance_domain_spent_short),
        500_000,
    );
    env.svm.set_account(env.slab, acct).unwrap();
    env.svm
        .set_account(
            env.perc_vault,
            Account {
                lamports: 1_000_000,
                data: token_account_data(&env.mint, &env.vault_authority, 3_000_000),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
}

fn make_live_market(slab: &Pubkey, mint: &Pubkey, marketauth: &Pubkey, init_slot: u64) -> Vec<u8> {
    make_live_market_with_public_chunk(slab, mint, marketauth, init_slot, 1)
}

fn make_live_market_with_public_chunk(
    slab: &Pubkey,
    mint: &Pubkey,
    marketauth: &Pubkey,
    init_slot: u64,
    public_b_chunk_atoms: u128,
) -> Vec<u8> {
    make_live_market_with_public_chunk_and_margin(
        slab,
        mint,
        marketauth,
        init_slot,
        public_b_chunk_atoms,
        10_000,
    )
}

fn make_live_market_with_public_chunk_and_margin(
    slab: &Pubkey,
    mint: &Pubkey,
    marketauth: &Pubkey,
    init_slot: u64,
    public_b_chunk_atoms: u128,
    margin_bps: u64,
) -> Vec<u8> {
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
    let h_max = if margin_bps == 10_000 { 10 } else { 6_480_000 };
    let mut cfg = percolator_prog::risk::V16Config::public_user_fund(1, 0, h_max);
    cfg.min_nonzero_mm_req = if margin_bps == 10_000 { 1 } else { 599 };
    cfg.min_nonzero_im_req = if margin_bps == 10_000 { 2 } else { 600 };
    cfg.maintenance_margin_bps = margin_bps;
    cfg.initial_margin_bps = margin_bps;
    cfg.max_trading_fee_bps = 10_000;
    cfg.max_accrual_dt_slots = 1;
    cfg.min_funding_lifetime_slots = 1;
    cfg.max_price_move_bps_per_slot = if margin_bps == 10_000 {
        10_000
    } else {
        margin_bps.saturating_sub(100).max(1)
    };
    cfg.max_account_b_settlement_chunks = 1;
    cfg.max_bankrupt_close_chunks = 1;
    cfg.max_bankrupt_close_lifetime_slots = 1;
    cfg.public_b_chunk_atoms = public_b_chunk_atoms;
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

fn install_public_loss_fixture(env: &mut Env, oracle_authority: &Pubkey) {
    let mut data = make_live_market_with_public_chunk(
        &env.slab,
        &env.mint,
        &env.market_admin.pubkey(),
        100,
        percolator::MAX_VAULT_TVL,
    );
    let (mut wrapper, _) = percolator_prog::state::read_market(&data).unwrap();
    wrapper.marketauth = oracle_authority.to_bytes();
    percolator_prog::state::write_wrapper_config(&mut data, &wrapper).unwrap();
    let mut profile = percolator_prog::state::read_asset_oracle_profile(&data, 0).unwrap();
    profile.oracle_authority = oracle_authority.to_bytes();
    percolator_prog::state::write_asset_oracle_profile(&mut data, 0, &profile).unwrap();

    let mut account = env.svm.get_account(&env.slab).unwrap();
    account.data = data;
    env.svm.set_account(env.slab, account).unwrap();
}

fn install_public_loss_fixture_with_margin(
    env: &mut Env,
    oracle_authority: &Pubkey,
    margin_bps: u64,
) {
    let mut data = make_live_market_with_public_chunk_and_margin(
        &env.slab,
        &env.mint,
        &env.market_admin.pubkey(),
        100,
        percolator::MAX_VAULT_TVL,
        margin_bps,
    );
    let (mut wrapper, _) = percolator_prog::state::read_market(&data).unwrap();
    wrapper.marketauth = oracle_authority.to_bytes();
    percolator_prog::state::write_wrapper_config(&mut data, &wrapper).unwrap();
    let mut profile = percolator_prog::state::read_asset_oracle_profile(&data, 0).unwrap();
    profile.oracle_authority = oracle_authority.to_bytes();
    percolator_prog::state::write_asset_oracle_profile(&mut data, 0, &profile).unwrap();

    let mut account = env.svm.get_account(&env.slab).unwrap();
    account.data = data;
    env.svm.set_account(env.slab, account).unwrap();
}

fn replace_with_permissionless_funding_loss_market(
    env: &mut Env,
    oracle: &Keypair,
    margin_bps: u64,
) {
    use percolator_prog::ix::Instruction as PIx;

    let market = Keypair::new();
    let market_len = percolator_prog::state::market_account_len_for_capacity(1).unwrap();
    let market_rent = env.svm.minimum_balance_for_rent_exemption(market_len);
    env.send(
        &[system_instruction::create_account(
            &env.payer.pubkey(),
            &market.pubkey(),
            market_rent,
            market_len as u64,
            &perc_id(),
        )],
        &[&market],
    )
    .expect("public caller allocates a fresh Percolator market");

    let market_admin = clone_kp(&env.market_admin);
    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(market_admin.pubkey(), true),
                AccountMeta::new(market.pubkey(), false),
                AccountMeta::new_readonly(env.mint, false),
            ],
            data: PIx::InitMarket {
                max_portfolio_assets: 1,
                h_min: 0,
                h_max: 10,
                initial_price: 1_000_000,
                min_nonzero_mm_req: 1,
                min_nonzero_im_req: 2,
                maintenance_margin_bps: margin_bps,
                initial_margin_bps: margin_bps,
                max_trading_fee_bps: 10_000,
                trade_fee_base_bps: 0,
                liquidation_fee_bps: 0,
                liquidation_fee_cap: 0,
                min_liquidation_abs: 0,
                max_price_move_bps_per_slot: 9_000,
                max_accrual_dt_slots: 1,
                max_abs_funding_e9_per_slot: 10_000,
                min_funding_lifetime_slots: 1,
                max_account_b_settlement_chunks: 1,
                max_bankrupt_close_chunks: 1,
                max_bankrupt_close_lifetime_slots: 1,
                public_b_chunk_atoms: percolator::MAX_VAULT_TVL,
                maintenance_fee_per_slot: 0,
            }
            .encode(),
        }],
        &[&market_admin],
    )
    .expect("permissionless creator initializes the live market");

    env.slab = market.pubkey();
    env.vault_authority =
        Pubkey::find_program_address(&[b"vault", env.slab.as_ref()], &perc_id()).0;
    env.perc_vault = Pubkey::find_program_address(
        &[
            env.vault_authority.as_ref(),
            spl_token::ID.as_ref(),
            env.mint.as_ref(),
        ],
        &ATA_PROGRAM_ID,
    )
    .0;
    env.svm
        .set_account(
            env.perc_vault,
            Account {
                lamports: 1_000_000,
                data: token_account_data(&env.mint, &env.vault_authority, 0),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    env.pool = cross_backing_pool_pda_with_schedule(
        &env.mint,
        &env.coin_mint,
        &env.slab,
        POLICY_PRINCIPAL,
        env.deposit_window_slots,
        env.bootstrap_start_slot,
        env.bootstrap_delay_slots,
    );

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(market_admin.pubkey(), true),
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::UpdateAssetAuthority {
                asset_index: 0,
                kind: 4,
                new_pubkey: oracle.pubkey().to_bytes(),
            }
            .encode(),
        }],
        &[&market_admin, oracle],
    )
    .expect("creator assigns the independent authenticated-mark authority");
}

fn create_percolator_portfolio(env: &mut Env, owner: &Keypair, capital: u64) -> Pubkey {
    let portfolio = Pubkey::new_unique();
    env.svm
        .set_account(
            portfolio,
            Account {
                lamports: 1_000_000_000,
                data: vec![
                    0u8;
                    percolator_prog::state::portfolio_account_len_for_market_slots(1)
                        .unwrap()
                ],
                owner: perc_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new(owner.pubkey(), true),
                AccountMeta::new(env.slab, false),
                AccountMeta::new(portfolio, false),
            ],
            data: percolator_prog::ix::Instruction::InitPortfolio.encode(),
        }],
        &[owner],
    )
    .expect("initialize public Percolator portfolio");

    let payer = clone_kp(&env.payer);
    let mint_auth = clone_kp(&env.mint_auth);
    let source = create_token_account(&mut env.svm, &payer, &env.mint, &owner.pubkey());
    mint_to(&mut env.svm, &payer, &env.mint, &mint_auth, &source, capital);
    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new(owner.pubkey(), true),
                AccountMeta::new(env.slab, false),
                AccountMeta::new(portfolio, false),
                AccountMeta::new(source, false),
                AccountMeta::new(env.perc_vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data: percolator_prog::ix::Instruction::Deposit { amount: capital as u128 }.encode(),
        }],
        &[owner],
    )
    .expect("fund public Percolator portfolio");
    portfolio
}

fn close_resolved_portfolios(env: &mut Env, portfolios: &[(&Keypair, Pubkey)]) {
    use percolator_prog::ix::Instruction as PIx;

    let payer = clone_kp(&env.payer);
    let payouts: Vec<_> = portfolios
        .iter()
        .map(|(owner, portfolio)| {
            let destination =
                create_token_account(&mut env.svm, &payer, &env.mint, &owner.pubkey());
            (*owner, *portfolio, destination)
        })
        .collect();

    // A winner's haircut rate is terminal only after every winner has converted its
    // unreceipted bound into a receipt. Round-robin the public lifecycle so one early
    // winner cannot block all later winners from making that market-wide progress.
    for _ in 0..512 {
        let mut open = 0usize;
        for (owner, portfolio, destination) in &payouts {
            if env
                .svm
                .get_account(portfolio)
                .map_or(true, |account| account.lamports == 0)
            {
                continue;
            }
            open += 1;
            let _ = env.send(
                &[Instruction {
                    program_id: perc_id(),
                    accounts: vec![
                        AccountMeta::new_readonly(owner.pubkey(), true),
                        AccountMeta::new(env.slab, false),
                        AccountMeta::new(*portfolio, false),
                        AccountMeta::new(*destination, false),
                        AccountMeta::new(env.perc_vault, false),
                        AccountMeta::new_readonly(env.vault_authority, false),
                        AccountMeta::new_readonly(spl_token::ID, false),
                    ],
                    data: PIx::CloseResolved {
                        fee_rate_per_slot: 0,
                    }
                    .encode(),
                }],
                &[owner],
            );
            let _ = env.send(
                &[Instruction {
                    program_id: perc_id(),
                    accounts: vec![
                        AccountMeta::new_readonly(owner.pubkey(), true),
                        AccountMeta::new(env.slab, false),
                        AccountMeta::new(*portfolio, false),
                        AccountMeta::new(*destination, false),
                        AccountMeta::new(env.perc_vault, false),
                        AccountMeta::new_readonly(env.vault_authority, false),
                        AccountMeta::new_readonly(spl_token::ID, false),
                    ],
                    data: PIx::ClaimResolvedPayoutTopup.encode(),
                }],
                &[owner],
            );
            let _ = env.send(
                &[Instruction {
                    program_id: perc_id(),
                    accounts: vec![
                        AccountMeta::new_readonly(owner.pubkey(), true),
                        AccountMeta::new(env.slab, false),
                        AccountMeta::new(*portfolio, false),
                    ],
                    data: PIx::ClosePortfolio.encode(),
                }],
                &[owner],
            );
        }
        if open == 0 {
            return;
        }
    }

    let open: Vec<_> = payouts
        .iter()
        .filter_map(|(owner, portfolio, _)| {
            env.svm
                .get_account(portfolio)
                .filter(|account| account.lamports != 0)
                .map(|_| (owner.pubkey(), *portfolio))
        })
        .collect();
    panic!("resolved public portfolios remain withdrawal gates: {open:?}");
}

fn public_percolator_crank(
    env: &mut Env,
    portfolio: Pubkey,
    slot: u64,
    observe_asset: bool,
) -> Result<(), String> {
    use percolator_prog::ix::Instruction as PIx;

    let observations = if observe_asset {
        vec![percolator_prog::ix::CrankObservationHint {
            asset_index: 0,
            oracle_accounts: 0,
        }]
    } else {
        vec![]
    };
    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new(env.payer.pubkey(), true),
                AccountMeta::new(env.slab, false),
                AccountMeta::new(portfolio, false),
            ],
            data: PIx::PermissionlessCrank {
                now_slot: slot,
                observations,
            }
            .encode(),
        }],
        &[],
    )
}

fn effective_price(env: &Env) -> u64 {
    percolator_prog::state::read_market(&env.svm.get_account(&env.slab).unwrap().data)
        .unwrap()
        .1
        .assets[0]
        .effective_price
}

fn advance_public_mark(
    env: &mut Env,
    oracle: &Keypair,
    observer_portfolio: Pubkey,
    slot: &mut u64,
    target: u64,
    max_steps: usize,
) {
    use percolator_prog::ix::Instruction as PIx;

    *slot = slot.checked_add(1).unwrap();
    env.warp_slot(*slot);
    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::PushAuthMark {
                asset_index: 0,
                now_slot: *slot,
                mark_e6: target,
            }
            .encode(),
        }],
        &[oracle],
    )
    .expect("publish authenticated target mark");

    for _ in 0..max_steps {
        env.warp_slot(*slot);
        public_percolator_crank(env, observer_portfolio, *slot, true)
            .expect("an empty public portfolio advances the bounded mark");
        if effective_price(env) == target {
            return;
        }
        *slot = slot.checked_add(1).unwrap();
    }
    panic!(
        "bounded public mark did not reach {target}; stopped at {}",
        effective_price(env),
    );
}

fn open_public_pair(
    env: &mut Env,
    position_q: i128,
    price: u64,
    capital: u64,
) -> (Keypair, Pubkey, Keypair, Pubkey) {
    open_public_pair_with_fee(env, position_q, price, capital, 0)
}

fn open_public_pair_with_fee(
    env: &mut Env,
    position_q: i128,
    price: u64,
    capital: u64,
    fee_bps: u64,
) -> (Keypair, Pubkey, Keypair, Pubkey) {
    use percolator_prog::ix::Instruction as PIx;

    let long = Keypair::new();
    let short = Keypair::new();
    for owner in [&long, &short] {
        env.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
    }
    let long_portfolio = create_percolator_portfolio(env, &long, capital);
    let short_portfolio = create_percolator_portfolio(env, &short, capital);
    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new(long.pubkey(), true),
                AccountMeta::new(short.pubkey(), true),
                AccountMeta::new(env.slab, false),
                AccountMeta::new(long_portfolio, false),
                AccountMeta::new(short_portfolio, false),
            ],
            data: PIx::TradeNoCpi {
                asset_index: 0,
                size_q: position_q,
                exec_price: price,
                fee_bps,
            }
            .encode(),
        }],
        &[&long, &short],
    )
    .expect("open balanced public position");
    (long, long_portfolio, short, short_portfolio)
}

fn open_public_pair_with_capitals(
    env: &mut Env,
    position_q: i128,
    price: u64,
    long_capital: u64,
    short_capital: u64,
) -> (Keypair, Pubkey, Keypair, Pubkey) {
    use percolator_prog::ix::Instruction as PIx;

    let long = Keypair::new();
    let short = Keypair::new();
    for owner in [&long, &short] {
        env.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
    }
    let long_portfolio = create_percolator_portfolio(env, &long, long_capital);
    let short_portfolio = create_percolator_portfolio(env, &short, short_capital);
    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new(long.pubkey(), true),
                AccountMeta::new(short.pubkey(), true),
                AccountMeta::new(env.slab, false),
                AccountMeta::new(long_portfolio, false),
                AccountMeta::new(short_portfolio, false),
            ],
            data: PIx::TradeNoCpi {
                asset_index: 0,
                size_q: position_q,
                exec_price: price,
                fee_bps: 0,
            }
            .encode(),
        }],
        &[&long, &short],
    )
    .expect("open asymmetric-capital public position");
    (long, long_portfolio, short, short_portfolio)
}

fn liquidate_stale_public_loser(env: &mut Env, portfolio: Pubkey, slot: u64) {
    public_percolator_crank(env, portfolio, slot, true)
        .expect("settle the stale losing portfolio at the reached mark");
    for _ in 0..4 {
        public_percolator_crank(env, portfolio, slot, false)
            .expect("permissionless liquidation makes bounded progress");
    }
    let state = percolator_prog::state::read_portfolio(
        &env.svm.get_account(&portfolio).unwrap().data,
    )
    .unwrap();
    assert!(percolator::active_bitmap_is_empty(
        state.active_bitmap.map(percolator::V16PodU64::get),
    ));
}

fn clear_stale_public_winner(
    env: &mut Env,
    owner: &Keypair,
    portfolio: Pubkey,
    slot: u64,
) {
    use percolator_prog::ix::Instruction as PIx;

    public_percolator_crank(env, portfolio, slot, false)
        .expect("refresh the reset winner");
    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new(owner.pubkey(), true),
                AccountMeta::new(env.slab, false),
                AccountMeta::new(portfolio, false),
            ],
            data: PIx::ForfeitRecoveryLeg {
                asset_index: 0,
                b_delta_budget: percolator::MAX_VAULT_TVL,
            }
            .encode(),
        }],
        &[owner],
    )
    .expect("owner clears the stale offsetting leg");
    for side in [0, 1] {
        env.send(
            &[Instruction {
                program_id: perc_id(),
                accounts: vec![AccountMeta::new(env.slab, false)],
                data: PIx::FinalizeResetSide {
                    asset_index: 0,
                    side,
                }
                .encode(),
            }],
            &[],
        )
        .expect("permissionless side reset completes");
    }
    let state = percolator_prog::state::read_portfolio(
        &env.svm.get_account(&portfolio).unwrap().data,
    )
    .unwrap();
    assert!(percolator::active_bitmap_is_empty(
        state.active_bitmap.map(percolator::V16PodU64::get),
    ));
}

fn clear_stale_public_winner_with_backing(
    env: &mut Env,
    owner: &Keypair,
    portfolio: Pubkey,
    slot: u64,
) {
    use percolator_prog::ix::Instruction as PIx;

    public_percolator_crank(env, portfolio, slot, false)
        .expect("refresh the source-backed reset winner");
    for _ in 0..256 {
        let state = percolator_prog::state::read_portfolio(
            &env.svm.get_account(&portfolio).unwrap().data,
        )
        .unwrap();
        if percolator::active_bitmap_is_empty(
            state.active_bitmap.map(percolator::V16PodU64::get),
        ) {
            break;
        }
        env.send(
            &[Instruction {
                program_id: perc_id(),
                accounts: vec![
                    AccountMeta::new(owner.pubkey(), true),
                    AccountMeta::new(env.slab, false),
                    AccountMeta::new(portfolio, false),
                ],
                data: PIx::ForfeitRecoveryLeg {
                    asset_index: 0,
                    b_delta_budget: percolator::MAX_VAULT_TVL,
                }
                .encode(),
            }],
            &[owner],
        )
        .expect("owner advances the bounded source-backed stale leg");
    }
    let state = percolator_prog::state::read_portfolio(
        &env.svm.get_account(&portfolio).unwrap().data,
    )
    .unwrap();
    assert!(percolator::active_bitmap_is_empty(
        state.active_bitmap.map(percolator::V16PodU64::get),
    ));
    for side in [0, 1] {
        env.send(
            &[Instruction {
                program_id: perc_id(),
                accounts: vec![AccountMeta::new(env.slab, false)],
                data: PIx::FinalizeResetSide {
                    asset_index: 0,
                    side,
                }
                .encode(),
            }],
            &[],
        )
        .expect("permissionless source-backed side reset completes");
    }
}

fn run_complete_public_insurance_loss(
    env: &mut Env,
    oracle: &Keypair,
    observer_portfolio: Pubkey,
    slot: &mut u64,
    low_entry: u64,
    low_capital: u64,
    high_entry: u64,
    high_capital: u64,
    domain_tranche: u64,
    expected_remaining: u128,
) -> Vec<(Keypair, Pubkey)> {
    run_complete_public_insurance_loss_with_fee(
        env,
        oracle,
        observer_portfolio,
        slot,
        low_entry,
        low_capital,
        high_entry,
        high_capital,
        domain_tranche,
        expected_remaining,
        0,
    )
    .0
}

fn run_complete_public_insurance_loss_with_fee(
    env: &mut Env,
    oracle: &Keypair,
    observer_portfolio: Pubkey,
    slot: &mut u64,
    low_entry: u64,
    low_capital: u64,
    high_entry: u64,
    high_capital: u64,
    domain_tranche: u64,
    expected_remaining_without_fee: u128,
    opening_fee_bps: u64,
) -> (Vec<(Keypair, Pubkey)>, u128) {
    let position_q = 1_000_000_000_000i128;
    let pnl_atoms_per_price = position_q.unsigned_abs() / percolator::POS_SCALE;
    let insurance_before = asset_insurance_remaining(env, 0);
    assert_eq!(effective_price(env), low_entry);
    assert_eq!(
        (domain_tranche as u128 + low_capital as u128) % pnl_atoms_per_price,
        0,
    );
    let opening_fee_budget = pnl_atoms_per_price
        .checked_mul(low_entry as u128)
        .and_then(|notional| notional.checked_mul(opening_fee_bps as u128))
        .map(|numerator| numerator.div_ceil(10_000))
        .and_then(|fee| u64::try_from(fee).ok())
        .unwrap();
    let fee_adjusted_capital = low_capital.checked_add(opening_fee_budget).unwrap();

    let (low_long, low_long_portfolio, low_short, low_short_portfolio) =
        open_public_pair_with_fee(
            env,
            position_q,
            low_entry,
            fee_adjusted_capital,
            opening_fee_bps,
        );
    let opening_fee = asset_insurance_remaining(env, 0)
        .checked_sub(insurance_before)
        .expect("opening trade fees cannot reduce insurance");
    let high_target = low_entry
        .checked_add(
            u64::try_from(
                (domain_tranche as u128 + low_capital as u128) / pnl_atoms_per_price,
            )
            .unwrap(),
        )
        .unwrap();
    advance_public_mark(env, oracle, observer_portfolio, slot, high_target, 300);
    liquidate_stale_public_loser(env, low_short_portfolio, *slot);
    assert_eq!(
        asset_insurance_remaining(env, 0),
        insurance_before + opening_fee - domain_tranche as u128,
    );
    clear_stale_public_winner(env, &low_long, low_long_portfolio, *slot);

    advance_public_mark(env, oracle, observer_portfolio, slot, high_entry, 100);
    let (high_long, high_long_portfolio, high_short, high_short_portfolio) =
        open_public_pair(env, position_q, high_entry, high_capital);
    let terminal_low = low_entry;
    let long_loss = pnl_atoms_per_price
        .checked_mul((high_entry - terminal_low) as u128)
        .unwrap();
    let second_insurance_loss = long_loss.checked_sub(high_capital as u128).unwrap();
    assert_eq!(
        second_insurance_loss,
        insurance_before - domain_tranche as u128 - expected_remaining_without_fee,
    );
    advance_public_mark(env, oracle, observer_portfolio, slot, terminal_low, 400);
    liquidate_stale_public_loser(env, high_long_portfolio, *slot);
    assert_eq!(
        asset_insurance_remaining(env, 0),
        expected_remaining_without_fee + opening_fee,
    );
    clear_stale_public_winner(env, &high_short, high_short_portfolio, *slot);

    (
        vec![
            (low_long, low_long_portfolio),
            (low_short, low_short_portfolio),
            (high_long, high_long_portfolio),
            (high_short, high_short_portfolio),
        ],
        opening_fee,
    )
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

fn gv_config_pda_for_schedule(
    mint: &Pubkey,
    subledger_pool: &Pubkey,
    bootstrap_delay_slots: u64,
    bootstrap_start_slot: u64,
) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"gv_config",
            mint.as_ref(),
            subledger_pool.as_ref(),
            &bootstrap_delay_slots.to_le_bytes(),
            &bootstrap_start_slot.to_le_bytes(),
        ],
        &gv_id(),
    )
    .0
}
const DISTRIBUTION_CLAIM_WINDOW_SLOTS: u64 = 1_000_000;
fn dist_config_pda(mint: &Pubkey, authority: &Pubkey) -> Pubkey {
    // finding AA: the distribution config PDA binds its seal AUTHORITY (the gv config) into the
    // seed, and its claim window, so an attacker can't squat a funded config under a different
    // authority or deadline.
    let claim_window = DISTRIBUTION_CLAIM_WINDOW_SLOTS.to_le_bytes();
    Pubkey::find_program_address(
        &[b"dist_config", mint.as_ref(), authority.as_ref(), &claim_window],
        &dist_id(),
    )
    .0
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
    let gv_config = env.gv_config_pda();
    let dist_config = dist_config_pda(&coin_mint, &gv_config);

    // distribution InitConfig with seal authority = the gv config PDA. Fund the COIN
    // vault, then REVOKE the COIN mint authority (the distribution requires a
    // fixed-supply COIN, README Safety §4).
    let dist_vault = create_token_account(&mut env.svm, &clone_kp(&env.payer), &coin_mint, &dist_config);
    mint_to(&mut env.svm, &clone_kp(&env.payer), &coin_mint, &clone_kp(&env.mint_auth), &dist_vault, 100);
    revoke_mint_authority(env, &coin_mint);
    let mut data = vec![0u8];
    data.extend_from_slice(&DISTRIBUTION_CLAIM_WINDOW_SLOTS.to_le_bytes()); // claim window
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
        data: gv_init_data(env.bootstrap_delay_slots, env.bootstrap_start_slot),
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
    data.extend_from_slice(&1u32.to_le_bytes());
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
            AccountMeta::new_readonly(ve.dist_config, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: vec![2u8],
    };
    env.send(&[reg], &[]).expect("register");
    (dist_proposal, gv_proposal)
}

fn legacy_gv_vote_ix(
    env: &Env,
    ve: &VoteEnv,
    voter: &Pubkey,
    gv_proposal: &Pubkey,
    action: u8,
) -> Instruction {
    let gv_ballot =
        Pubkey::find_program_address(&[b"gv_ballot", ve.gv_config.as_ref(), voter.as_ref()], &gv_id()).0;
    Instruction {
        program_id: gv_id(),
        accounts: vec![
            AccountMeta::new(*voter, true),
            AccountMeta::new(ve.gv_config, false),
            AccountMeta::new(gv_ballot, false),
            AccountMeta::new(*gv_proposal, false),
            AccountMeta::new(env.position_pda(voter), false),
            AccountMeta::new_readonly(env.pool, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(sub_id(), false),
        ],
        data: vec![3u8, action],
    }
}

fn gv_vote_ix(
    env: &Env,
    ve: &VoteEnv,
    voter: &Pubkey,
    gv_proposal: &Pubkey,
    action: u8,
) -> Instruction {
    let mut ix = legacy_gv_vote_ix(env, ve, voter, gv_proposal, action);
    if env.svm.get_account(&ve.gv_config).unwrap().data.len() == 264 {
        let ballot = Pubkey::find_program_address(
            &[b"gv_ballot", ve.gv_config.as_ref(), voter.as_ref()],
            &gv_id(),
        )
        .0;
        let vote_nonce = env
            .svm
            .get_account(&ballot)
            .filter(|account| account.data.len() >= 120)
            .map(|account| u64::from_le_bytes(account.data[96..104].try_into().unwrap()))
            .unwrap_or(0);
        ix.data.extend_from_slice(&vote_nonce.to_le_bytes());
        if let Some(position) = env
            .svm
            .get_account(&env.position_pda(voter))
            .filter(|account| account.data.len() >= 97)
        {
            ix.data.extend_from_slice(&position.data[72..80]);
            ix.data.extend_from_slice(&position.data[89..97]);
            ix.data.extend_from_slice(&position.data[80..88]);
        } else {
            ix.data.extend_from_slice(&[0u8; 24]);
        }
    }
    ix
}

fn gv_vote(
    env: &mut Env,
    ve: &VoteEnv,
    voter: &Keypair,
    gv_proposal: &Pubkey,
    action: u8,
) -> Result<(), String> {
    let ix = gv_vote_ix(env, ve, &voter.pubkey(), gv_proposal, action);
    env.send(&[ix], &[voter])
}

// Permissionless winner-take-all trigger: seals the distribution to the winning
// proposal. One voter holding 100% trivially clears quorum + majority.
fn gv_trigger(
    env: &mut Env,
    ve: &VoteEnv,
    gv_proposal: &Pubkey,
    dist_proposal: &Pubkey,
) -> Result<(), String> {
    // Genesis gates on slots. Keep the fixture's oracle wall-clock fresh because
    // production cranks update it continuously during the six-month bootstrap.
    let mut clock = env.svm.get_sysvar::<Clock>();
    clock.slot = env.bootstrap_end_slot();
    env.svm.set_sysvar(&clock);
    gv_trigger_now(env, ve, gv_proposal, dist_proposal)
}

fn gv_trigger_now(
    env: &mut Env,
    ve: &VoteEnv,
    gv_proposal: &Pubkey,
    dist_proposal: &Pubkey,
) -> Result<(), String> {
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

// gv Config.total_voted_principal (the quorum numerator) @ 200..208.
fn gv_total_voted_principal(env: &Env, ve: &VoteEnv) -> u64 {
    let acc = env.svm.get_account(&ve.gv_config).unwrap();
    u64::from_le_bytes(acc.data[200..208].try_into().unwrap())
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
    assert_eq!(start_slot, 101, "start_slot = first slot after custody grant");
    assert!(!withdrawn);
    assert_eq!(env.pool_outstanding(), amount);
}

#[test]
fn legacy_genesis_pool_cannot_squat_cross_backing_genesis_address() {
    let mut env = Env::new();
    env.init_insurance_pool();

    let cross_pool = cross_backing_pool_pda_with_schedule(
        &env.mint,
        &env.coin_mint,
        &env.slab,
        POLICY_PRINCIPAL,
        env.deposit_window_slots,
        env.bootstrap_start_slot,
        env.bootstrap_delay_slots,
    );
    assert_ne!(env.pool, cross_pool);
    let cross_vote_authority = gv_config_pda_for_schedule(
        &env.coin_mint,
        &cross_pool,
        env.bootstrap_delay_slots,
        env.bootstrap_start_slot,
    );
    let mut data = vec![14u8]; // IX_INIT_CROSS_BACKING_GENESIS_POOL
    data.extend_from_slice(&ASSET_ID.to_le_bytes());
    data.push(POLICY_PRINCIPAL);
    data.extend_from_slice(&env.deposit_window_slots.to_le_bytes());
    data.extend_from_slice(&env.bootstrap_start_slot.to_le_bytes());
    data.extend_from_slice(&env.bootstrap_delay_slots.to_le_bytes());
    let ix = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(env.mint, false),
            AccountMeta::new(cross_pool, false),
            AccountMeta::new_readonly(env.perc_vault, false),
            AccountMeta::new_readonly(env.slab, false),
            AccountMeta::new_readonly(perc_id(), false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(cross_vote_authority, false),
            AccountMeta::new_readonly(env.coin_mint, false),
            AccountMeta::new(cross_backing_ledger_pda(&cross_pool, 0), false),
            AccountMeta::new(cross_backing_ledger_pda(&cross_pool, 1), false),
        ],
        data,
    };
    env.send(&[ix], &[])
        .expect("cross-backed genesis init remains available after legacy init");

    assert_eq!(env.svm.get_account(&env.pool).unwrap().data.len(), 361);
    assert_eq!(env.svm.get_account(&cross_pool).unwrap().data.len(), 361);
}

#[test]
fn genesis_cross_backing_splits_globally_and_returns_only_owner_principal() {
    let mut env = Env::new_cross_backing();
    env.init_cross_backing_genesis_pool();

    let (alice, alice_ata) = new_depositor(&mut env, 1);
    let (bob, bob_ata) = new_depositor(&mut env, 1);
    let pool_holding = create_canonical_pool_holding(&mut env);

    env.cross_backing_deposit(&alice, &alice_ata, &pool_holding, 1)
        .expect("alice deposits one base unit");
    env.cross_backing_deposit(&bob, &bob_ata, &pool_holding, 1)
        .expect("bob deposits one base unit");

    let market = env.svm.get_account(&env.slab).unwrap();
    let insurance = percolator_accounting::read_asset_insurance_remaining(&market.data, 0)
        .expect("asset-0 insurance");
    let backing = percolator_accounting::read_asset_backing_balances(&market.data, 0)
        .expect("asset-0 backing");
    assert_eq!(insurance, 1, "half of aggregate genesis capital is insurance");
    assert_eq!(
        backing
            .iter()
            .map(|domain| domain.principal_atoms)
            .sum::<u128>(),
        1,
        "half of aggregate genesis capital is market-risk backing"
    );
    assert_eq!(env.pool_outstanding(), 2, "each base unit remains one vote");
    assert_eq!(env.token_amount(&env.perc_vault), 2, "all capital stays in Percolator custody");

    let mut ledger_principal = 0u128;
    for domain in 0..2u16 {
        let ledger_account = env
            .svm
            .get_account(&cross_backing_ledger_pda(&env.pool, domain))
            .expect("canonical backing ledger");
        assert_eq!(ledger_account.owner, perc_id());
        if percolator_prog::state::is_initialized(&ledger_account.data) {
            let ledger = percolator_prog::state::read_backing_domain_ledger(&ledger_account.data)
                .expect("initialized backing ledger");
            assert_eq!(ledger.market_group, env.slab.to_bytes());
            assert_eq!(ledger.authority, env.pool.to_bytes());
            assert_eq!(ledger.domain, domain);
            ledger_principal += ledger.total_principal_atoms;
        } else {
            assert!(ledger_account.data.iter().all(|byte| *byte == 0));
        }
    }
    assert_eq!(ledger_principal, 1);

    env.cross_backing_withdraw(&alice, &alice_ata, &pool_holding, 1)
        .expect("alice recovers only her base unit");
    env.cross_backing_withdraw(&bob, &bob_ata, &pool_holding, 1)
        .expect("bob recovers only his base unit");
    assert_eq!(env.token_amount(&alice_ata), 1);
    assert_eq!(env.token_amount(&bob_ata), 1);
    assert_eq!(env.pool_outstanding(), 0);
    let market = env.svm.get_account(&env.slab).unwrap();
    assert_eq!(
        percolator_accounting::read_asset_insurance_remaining(&market.data, 0).unwrap(),
        0
    );
    assert!(percolator_accounting::read_asset_backing_balances(&market.data, 0)
        .unwrap()
        .iter()
        .all(|domain| domain.principal_atoms == 0));
    assert_eq!(env.token_amount(&env.perc_vault), 0);
}

// PUBLIC DOS: a one-atom first deposit lands entirely in insurance and would
// otherwise leave both deterministic backing ledgers blank. A foreign market
// authority must not be able to bind either ledger through Percolator's public
// first-write path and block the funded owner's valid principal withdrawal.
#[test]
fn foreign_market_cannot_claim_a_funded_cross_backing_pools_blank_ledger() {
    use percolator_prog::ix::Instruction as PIx;

    let mut env = Env::new_cross_backing();
    env.init_cross_backing_genesis_pool();
    for domain in 0..2u16 {
        let ledger = env
            .svm
            .get_account(&cross_backing_ledger_pda(&env.pool, domain))
            .unwrap();
        assert_eq!(ledger.owner, sub_id(), "blank ledger remains quarantined");
        assert!(ledger.data.iter().all(|byte| *byte == 0));
    }

    let (victim, victim_ata) = new_depositor(&mut env, 1);
    let pool_holding = create_canonical_pool_holding(&mut env);
    env.cross_backing_deposit(&victim, &victim_ata, &pool_holding, 1)
        .expect("victim deposits one base unit before the attack");
    assert_eq!(env.pool_outstanding(), 1);
    assert_eq!(env.token_amount(&victim_ata), 0);
    assert_eq!(env.token_amount(&env.perc_vault), 1);
    for domain in 0..2u16 {
        let ledger = env
            .svm
            .get_account(&cross_backing_ledger_pda(&env.pool, domain))
            .unwrap();
        assert_eq!(ledger.owner, perc_id());
        let ledger = percolator_prog::state::read_backing_domain_ledger(&ledger.data)
            .expect("first valid deposit binds both ledgers");
        assert_eq!(ledger.market_group, env.slab.to_bytes());
        assert_eq!(ledger.authority, env.pool.to_bytes());
        assert_eq!(ledger.domain, domain);
    }

    let attacker = Keypair::new();
    let attacker_market = Keypair::new();
    let market_len = percolator_prog::state::market_account_len_for_capacity(1).unwrap();
    let market_rent = env.svm.minimum_balance_for_rent_exemption(market_len);
    env.send(
        &[system_instruction::create_account(
            &env.payer.pubkey(),
            &attacker_market.pubkey(),
            market_rent,
            market_len as u64,
            &perc_id(),
        )],
        &[&attacker_market],
    )
    .expect("attacker publicly allocates an unrelated market");
    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(attacker.pubkey(), true),
                AccountMeta::new(attacker_market.pubkey(), false),
                AccountMeta::new_readonly(env.mint, false),
            ],
            data: PIx::InitMarket {
                max_portfolio_assets: 1,
                h_min: 0,
                h_max: 10,
                initial_price: 1_000,
                min_nonzero_mm_req: 599,
                min_nonzero_im_req: 600,
                maintenance_margin_bps: 5_000,
                initial_margin_bps: 5_000,
                max_trading_fee_bps: 10_000,
                trade_fee_base_bps: 0,
                liquidation_fee_bps: 0,
                liquidation_fee_cap: 0,
                min_liquidation_abs: 0,
                max_price_move_bps_per_slot: 4_900,
                max_accrual_dt_slots: 1,
                max_abs_funding_e9_per_slot: 0,
                min_funding_lifetime_slots: 1,
                max_account_b_settlement_chunks: 1,
                max_bankrupt_close_chunks: 1,
                max_bankrupt_close_lifetime_slots: 1,
                public_b_chunk_atoms: percolator::MAX_VAULT_TVL,
                maintenance_fee_per_slot: 0,
            }
            .encode(),
        }],
        &[&attacker],
    )
    .expect("attacker publicly initializes the unrelated market");

    let payer = clone_kp(&env.payer);
    let mint_auth = clone_kp(&env.mint_auth);
    let attacker_source = create_token_account(
        &mut env.svm,
        &payer,
        &env.mint,
        &attacker.pubkey(),
    );
    mint_to(
        &mut env.svm,
        &payer,
        &env.mint,
        &mint_auth,
        &attacker_source,
        1,
    );
    let attacker_vault_authority = Pubkey::find_program_address(
        &[b"vault", attacker_market.pubkey().as_ref()],
        &perc_id(),
    )
    .0;
    let attacker_vault = Pubkey::find_program_address(
        &[
            attacker_vault_authority.as_ref(),
            spl_token::ID.as_ref(),
            env.mint.as_ref(),
        ],
        &ATA_PROGRAM_ID,
    )
    .0;
    env.svm
        .set_account(
            attacker_vault,
            Account {
                lamports: 1_000_000,
                data: token_account_data(&env.mint, &attacker_vault_authority, 0),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let victim_ledger = cross_backing_ledger_pda(&env.pool, 0);
    let attack = env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(attacker.pubkey(), true),
                AccountMeta::new(attacker_market.pubkey(), false),
                AccountMeta::new(attacker_source, false),
                AccountMeta::new(attacker_vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
                AccountMeta::new(victim_ledger, false),
            ],
            data: PIx::TopUpBackingBucket {
                domain: 0,
                amount: 1,
                expiry_slot: 10_000,
            }
            .encode(),
        }],
        &[&attacker],
    );
    if attack.is_ok() {
        let ledger = env.svm.get_account(&victim_ledger).unwrap();
        let ledger = percolator_prog::state::read_backing_domain_ledger(&ledger.data)
            .expect("successful attack binds the victim ledger");
        assert_eq!(ledger.market_group, attacker_market.pubkey().to_bytes());
        assert_eq!(ledger.authority, attacker.pubkey().to_bytes());
    }

    let withdrawal = env.cross_backing_withdraw(&victim, &victim_ata, &pool_holding, 1);
    assert!(
        attack.is_err() || withdrawal.is_ok(),
        "a public foreign-market bind must not strand an existing depositor: {withdrawal:?}"
    );
    assert!(
        attack.is_err(),
        "the deterministic ledger must reject an unrelated market's first write"
    );
    withdrawal.expect("victim recovers the deposited base unit");
    assert_eq!(env.pool_outstanding(), 0);
    assert_eq!(env.token_amount(&victim_ata), 1);
    assert_eq!(env.token_amount(&env.perc_vault), 0);
}

// PUBLIC DOS/LOF: fair share rounding can leave a whole protocol atom after the
// last owner claim is retired. If that atom remains in cross backing, the legacy
// whole-backing cleanup is intentionally unavailable and the market cannot become
// empty. The final exit must isolate it in the canonical protocol escrow without
// increasing either owner's payout.
#[test]
fn cross_backing_rounding_reserve_cannot_remain_ownerless_in_percolator() {
    let mut env = Env::new_cross_backing();
    env.init_cross_backing_genesis_pool();

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool_holding = create_canonical_pool_holding(&mut env);
    env.cross_backing_deposit(&alice, &alice_ata, &pool_holding, amount)
        .expect("Alice deposits before the insurance loss");

    // Consume the insurance half while preserving the real backing half. The
    // existing helper updates every insurance-domain counter; restore only the
    // aggregate vault value that belongs to still-live backing.
    impair_market(&mut env, 0);
    let backing_after_loss = percolator_accounting::read_asset_backing_balances(
        &env.svm.get_account(&env.slab).unwrap().data,
        0,
    )
    .unwrap()
    .into_iter()
    .map(|balance| balance.principal_atoms)
    .sum::<u128>();
    assert_eq!(backing_after_loss, u128::from(amount / 2));
    let mut market_after_loss = env.svm.get_account(&env.slab).unwrap();
    {
        let (_, group) =
            percolator_prog::state::market_view_mut(&mut market_after_loss.data).unwrap();
        group.header.vault = percolator::V16PodU128::new(backing_after_loss);
        group
            .validate_shape()
            .expect("insurance loss plus preserved backing remains engine-valid");
    }
    env.svm.set_account(env.slab, market_after_loss).unwrap();
    env.svm
        .set_account(
            env.perc_vault,
            Account {
                lamports: 1_000_000,
                data: token_account_data(
                    &env.mint,
                    &env.vault_authority,
                    backing_after_loss as u64,
                ),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let (bob, bob_ata) = new_depositor(&mut env, amount);
    env.cross_backing_deposit(&bob, &bob_ata, &pool_holding, amount)
        .expect("Bob recapitalizes after Alice's loss");
    env.cross_backing_withdraw(&alice, &alice_ata, &pool_holding, amount)
        .expect("Alice realizes only her tenure loss");
    env.cross_backing_withdraw(&bob, &bob_ata, &pool_holding, amount)
        .expect("Bob recovers his later capital without inheriting Alice's loss");

    assert_eq!(env.token_amount(&alice_ata), amount / 2);
    assert_eq!(env.token_amount(&bob_ata), amount - 1);
    assert_eq!(env.pool_outstanding(), 0);
    assert_eq!(
        env.token_amount(&pool_holding),
        1,
        "the final whole-atom reserve is isolated as protocol surplus",
    );
    assert_eq!(env.token_amount(&env.perc_vault), 0);
    assert!(
        percolator_accounting::read_asset_backing_balances(
            &env.svm.get_account(&env.slab).unwrap().data,
            0,
        )
        .unwrap()
        .into_iter()
        .all(|balance| !balance.has_any_state()),
        "no ownerless backing state can block terminal market cleanup",
    );
}

// A partial exiter can choose the nominal tranche boundaries that feed the
// insurance/backing allocation. Per-call floors must not let that choice shift
// an impaired co-depositor's claim or leave live backing after the final sweep.
#[test]
fn cross_backing_split_exit_cannot_shift_an_impaired_codepositors_protection() {
    let run = |split_exit: bool| {
        let mut env = Env::new_cross_backing();
        env.init_cross_backing_genesis_pool();

        let amount = 1_000_000u64;
        let (alice, alice_ata) = new_depositor(&mut env, amount);
        let (bob, bob_ata) = new_depositor(&mut env, amount);
        let pool_holding = create_canonical_pool_holding(&mut env);
        env.cross_backing_deposit(&alice, &alice_ata, &pool_holding, amount)
            .expect("Alice deposits into both protection classes");
        env.cross_backing_deposit(&bob, &bob_ata, &pool_holding, amount)
            .expect("Bob deposits into both protection classes");

        impair_market(&mut env, 1);
        let backing = percolator_accounting::read_asset_backing_balances(
            &env.svm.get_account(&env.slab).unwrap().data,
            0,
        )
        .unwrap()
        .into_iter()
        .map(|balance| balance.principal_atoms)
        .sum::<u128>();
        assert_eq!(backing, u128::from(amount));
        let protected = backing + 1;
        let mut impaired_market = env.svm.get_account(&env.slab).unwrap();
        {
            let (_, group) =
                percolator_prog::state::market_view_mut(&mut impaired_market.data).unwrap();
            group.header.vault = percolator::V16PodU128::new(protected);
            group
                .validate_shape()
                .expect("one-class impairment plus live backing remains valid");
        }
        env.svm.set_account(env.slab, impaired_market).unwrap();
        env.svm
            .set_account(
                env.perc_vault,
                Account {
                    lamports: 1_000_000,
                    data: token_account_data(
                        &env.mint,
                        &env.vault_authority,
                        protected as u64,
                    ),
                    owner: spl_token::ID,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();

        if split_exit {
            for chunk in [400_000u64, 300_000, 300_000] {
                env.cross_backing_withdraw(
                    &alice,
                    &alice_ata,
                    &pool_holding,
                    chunk,
                )
                .expect("Alice's bounded partial exit remains live");
            }
        } else {
            env.cross_backing_withdraw(
                &alice,
                &alice_ata,
                &pool_holding,
                amount,
            )
            .expect("Alice's control exit remains live");
        }
        env.cross_backing_withdraw(&bob, &bob_ata, &pool_holding, amount)
            .expect("Bob exits after Alice");

        (
            env.token_amount(&alice_ata),
            env.token_amount(&bob_ata),
            env.token_amount(&env.perc_vault),
            env.token_amount(&pool_holding),
            env.pool_outstanding(),
        )
    };

    let control = run(false);
    let split = run(true);
    assert_eq!(control, (500_000, 500_000, 0, 1, 0));
    assert_eq!(
        split, control,
        "uneven partial exits cannot change either payout or reserve custody",
    );
}

// PUBLIC LOF: every small partial exit independently rounds position shares,
// the aggregate insurance/backing tranche, and each long/short domain debit. A
// splitter must never collect more than the position's pre-exit whole-claim value
// or reduce an independent depositor's claim after a public Percolator loss.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PublicSplitExitOutcome {
    attacker_payout: u128,
    victim_payout: u128,
    attacker_whole_claim: u128,
    victim_whole_claim: u128,
    protected: u128,
}

fn run_public_cross_backing_repeated_split_exit(
    attacker_principals: &[u64],
    victim_principal: u64,
    target_mark: u64,
    partial_exit_chunks: Option<&[u64]>,
) -> PublicSplitExitOutcome {
    use percolator_prog::ix::Instruction as PIx;

    let mut env = Env::new_cross_backing();
    let oracle = Keypair::new();
    env.svm.airdrop(&oracle.pubkey(), 1_000_000_000).unwrap();
    install_public_loss_fixture(&mut env, &oracle.pubkey());
    env.init_cross_backing_genesis_pool();

    let mut attackers = Vec::with_capacity(attacker_principals.len());
    for principal in attacker_principals {
        let (owner, owner_ata) = new_depositor(&mut env, *principal);
        attackers.push((owner, owner_ata, *principal));
    }
    let (victim, victim_ata) = new_depositor(&mut env, victim_principal);
    let pool_holding = create_canonical_pool_holding(&mut env);
    for (owner, owner_ata, principal) in &attackers {
        env.cross_backing_deposit(owner, owner_ata, &pool_holding, *principal)
            .expect("attacker funds aggregate protection");
    }
    env.cross_backing_deposit(
        &victim,
        &victim_ata,
        &pool_holding,
        victim_principal,
    )
    .expect("victim funds aggregate protection");

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ConfigureAuthMark {
                asset_index: 0,
                now_slot: 100,
                initial_mark_e6: 100,
            }
            .encode(),
        }],
        &[&oracle],
    )
    .expect("configure the authenticated mark");

    let long = Keypair::new();
    let short = Keypair::new();
    for owner in [&long, &short] {
        env.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
    }
    let long_portfolio = create_percolator_portfolio(&mut env, &long, 1_000_000);
    let short_portfolio = create_percolator_portfolio(&mut env, &short, 200);
    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new(long.pubkey(), true),
                AccountMeta::new(short.pubkey(), true),
                AccountMeta::new(env.slab, false),
                AccountMeta::new(long_portfolio, false),
                AccountMeta::new(short_portfolio, false),
            ],
            data: PIx::TradeNoCpi {
                asset_index: 0,
                size_q: percolator::POS_SCALE as i128,
                exec_price: 100,
                fee_bps: 0,
            }
            .encode(),
        }],
        &[&long, &short],
    )
    .expect("open the public loss pair");

    let mut slot = 100;
    advance_public_mark(
        &mut env,
        &oracle,
        long_portfolio,
        &mut slot,
        target_mark,
        300,
    );
    liquidate_stale_public_loser(&mut env, short_portfolio, slot);

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ResolveMarket.encode(),
        }],
        &[&oracle],
    )
    .expect("resolve the publicly impaired market");
    close_resolved_portfolios(
        &mut env,
        &[(&long, long_portfolio), (&short, short_portfolio)],
    );

    let market = env.svm.get_account(&env.slab).unwrap();
    let backing_balances =
        percolator_accounting::read_asset_backing_balances(&market.data, 0).unwrap();
    let backing_sources =
        percolator_accounting::read_asset_backing_source_credits(&market.data, 0).unwrap();
    let ledger_principals = [0u16, 1u16].map(|domain| {
        let ledger = env
            .svm
            .get_account(&cross_backing_ledger_pda(&env.pool, domain))
            .unwrap();
        percolator_prog::state::read_backing_domain_ledger(&ledger.data)
            .unwrap()
            .total_principal_atoms
    });
    let protected_backing = backing_balances
        .into_iter()
        .zip(backing_sources)
        .zip(ledger_principals)
        .map(|((balance, source), principal)| {
            balance
                .provider_protected_principal_atoms(principal, source)
                .unwrap()
        })
        .sum::<u128>();
    let protected = asset_insurance_remaining(&env, 0) + protected_backing;
    let attacker_principal = attacker_principals.iter().sum::<u64>();
    let deposited = u128::from(attacker_principal) + u128::from(victim_principal);
    assert!(
        protected < deposited,
        "the public trade must impair the owner pool: insurance={}, protected_backing={protected_backing}, balances={backing_balances:?}, sources={backing_sources:?}, ledgers={ledger_principals:?}",
        asset_insurance_remaining(&env, 0),
    );

    let attacker_shares = attackers
        .iter()
        .map(|(owner, _, _)| env.position_shares(&owner.pubkey()))
        .sum::<u128>();
    let victim_shares = env.position_shares(&victim.pubkey());
    let total_shares = env.pool_total_shares();
    let whole_claim = attacker_shares * (protected + 1) / (total_shares + 1_000_000);
    let victim_whole_claim = victim_shares * (protected + 1) / (total_shares + 1_000_000);
    if let Some(exit_chunks) = partial_exit_chunks {
        assert_eq!(attackers.len(), 1);
        assert_eq!(exit_chunks.iter().sum::<u64>(), attacker_principal);
        let (attacker, attacker_ata, _) = &attackers[0];
        for amount in exit_chunks {
            env.cross_backing_withdraw(
                attacker,
                attacker_ata,
                &pool_holding,
                *amount,
            )
            .expect("the public split exit remains live");
        }
    } else {
        for (attacker, attacker_ata, principal) in &attackers {
            env.cross_backing_withdraw(
                attacker,
                attacker_ata,
                &pool_holding,
                *principal,
            )
            .expect("each public attacker identity exits once");
        }
    }
    let attacker_payout = attackers
        .iter()
        .map(|(_, owner_ata, _)| u128::from(env.token_amount(owner_ata)))
        .sum::<u128>();
    assert!(
        attacker_payout <= whole_claim,
        "split exits collected {attacker_payout}, above the pre-attack whole claim {whole_claim}",
    );

    env.cross_backing_withdraw(
        &victim,
        &victim_ata,
        &pool_holding,
        victim_principal,
    )
    .expect("the co-depositor retains a bounded exit");
    let victim_payout = u128::from(env.token_amount(&victim_ata));
    assert_eq!(env.pool_outstanding(), 0);
    PublicSplitExitOutcome {
        attacker_payout,
        victim_payout,
        attacker_whole_claim: whole_claim,
        victim_whole_claim,
        protected,
    }
}

#[test]
fn public_cross_backing_repeated_split_exit_cannot_drain_a_codepositor() {
    let control =
        run_public_cross_backing_repeated_split_exit(&[12], 2, 303, Some(&[12]));
    let split = run_public_cross_backing_repeated_split_exit(
        &[12],
        2,
        303,
        Some(&[3, 3, 3, 3]),
    );
    assert_eq!(
        control,
        PublicSplitExitOutcome {
            attacker_payout: 7,
            victim_payout: 1,
            attacker_whole_claim: 7,
            victim_whole_claim: 1,
            protected: 8,
        },
    );
    assert!(
        split.attacker_payout <= control.attacker_payout,
        "splitting must not increase the attacker's payout",
    );
    assert!(
        split.victim_payout >= control.victim_payout,
        "four public three-atom exits reduce the independent victim from {} to {}",
        control.victim_payout,
        split.victim_payout,
    );
}

#[test]
fn public_zero_payout_partial_exits_cannot_grief_a_codepositor() {
    let control =
        run_public_cross_backing_repeated_split_exit(&[14], 14, 301, Some(&[14]));
    let dust =
        run_public_cross_backing_repeated_split_exit(&[14], 14, 301, Some(&[1; 14]));
    assert_eq!(
        (control.attacker_payout, control.victim_payout, control.protected),
        (13, 13, 26),
    );
    assert_eq!(dust.attacker_payout, 0);
    assert!(
        dust.victim_payout >= dust.victim_whole_claim,
        "fourteen public zero-payout exits reduced the victim from {} to {}",
        dust.victim_whole_claim,
        dust.victim_payout,
    );
}

#[test]
fn public_zero_payout_identity_splitting_cannot_grief_a_codepositor() {
    let split_identities =
        run_public_cross_backing_repeated_split_exit(&[1; 14], 14, 301, None);
    assert_eq!(split_identities.attacker_payout, 0);
    assert!(
        split_identities.victim_payout >= split_identities.victim_whole_claim,
        "fourteen public one-unit identities reduced the victim from {} to {}",
        split_identities.victim_whole_claim,
        split_identities.victim_payout,
    );
}

// PUBLIC LOF PROBE: protocol funding surplus can refill backing after an owner
// loss without restoring the loss-only share rate. A zero-value public exit must
// not let a later zero-value exit checkpoint that recovery so its subsequent
// consumption charges the same backing atom to an independent owner twice.
// The same loss must remain fixed when the recovery arrives before any owner
// action has observed the transient backing low.
#[test]
fn recovered_protocol_backing_cannot_be_charged_as_a_second_owner_loss() {
    use percolator_prog::ix::Instruction as PIx;

    #[derive(Clone, Copy, Debug)]
    struct Outcome {
        victim_payout: u64,
        first_sync_payout: u64,
        recovery_sync_payout: u64,
        protocol_value_after_exit: u128,
        backing_after_first_loss: u128,
        backing_after_recovery: u128,
        backing_after_second_loss: u128,
    }

    let run =
        |sync_first_loss_before_recovery: bool, sync_recovery_before_second_loss: bool| {
        let mut env = Env::new_cross_backing();
        let oracle = Keypair::new();
        let observer = Keypair::new();
        for owner in [&oracle, &observer] {
            env.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
        }
        replace_with_permissionless_funding_loss_market(&mut env, &oracle, 10_000);
        env.init_cross_backing_genesis_pool();

        let victim_principal = 31u64;
        let (victim, victim_ata) = new_depositor(&mut env, victim_principal);
        let (first_sync, first_sync_ata) = new_depositor(&mut env, 1);
        let (recovery_sync, recovery_sync_ata) = new_depositor(&mut env, 1);
        let pool_holding = create_canonical_pool_holding(&mut env);
        for (owner, destination, amount) in [
            (&victim, &victim_ata, victim_principal),
            (&first_sync, &first_sync_ata, 1),
            (&recovery_sync, &recovery_sync_ata, 1),
        ] {
            env.cross_backing_deposit(owner, destination, &pool_holding, amount)
                .expect("owner deposits into the public cross-backed pool");
        }

        env.send(
            &[Instruction {
                program_id: perc_id(),
                accounts: vec![
                    AccountMeta::new_readonly(oracle.pubkey(), true),
                    AccountMeta::new(env.slab, false),
                ],
                data: PIx::ConfigureAuthMark {
                    asset_index: 0,
                    now_slot: 100,
                    initial_mark_e6: 100,
                }
                .encode(),
            }],
            &[&oracle],
        )
        .expect("configure the authenticated funding mark");
        let observer_portfolio = create_percolator_portfolio(&mut env, &observer, 0);
        let mut slot = 100u64;

        let (first_long, first_long_portfolio, first_short, first_short_portfolio) =
            open_public_pair_with_capitals(
                &mut env,
                percolator::POS_SCALE as i128,
                100,
                1_000_000,
                200,
            );
        advance_public_mark(
            &mut env,
            &oracle,
            observer_portfolio,
            &mut slot,
            303,
            300,
        );
        liquidate_stale_public_loser(&mut env, first_short_portfolio, slot);
        clear_stale_public_winner_with_backing(
            &mut env,
            &first_long,
            first_long_portfolio,
            slot,
        );

        let backing_protection = |env: &Env| {
            let market = env.svm.get_account(&env.slab).unwrap();
            let balances =
                percolator_accounting::read_asset_backing_balances(&market.data, 0).unwrap();
            let sources =
                percolator_accounting::read_asset_backing_source_credits(&market.data, 0)
                    .unwrap();
            let principals = [0u16, 1u16].map(|domain| {
                let ledger = env
                    .svm
                    .get_account(&cross_backing_ledger_pda(&env.pool, domain))
                    .unwrap();
                percolator_prog::state::read_backing_domain_ledger(&ledger.data)
                    .unwrap()
                    .total_principal_atoms
            });
            let provider_backing = balances
                .into_iter()
                .zip(sources)
                .zip(principals)
                .map(|((balance, source), principal)| {
                    balance
                        .provider_protected_principal_atoms(principal, source)
                        .unwrap()
                })
                .sum::<u128>();
            let pool = env.svm.get_account(&env.pool).unwrap();
            let pending = u64::from_le_bytes(pool.data[273..281].try_into().unwrap())
                .checked_add(u64::from_le_bytes(
                    pool.data[281..289].try_into().unwrap(),
                ))
                .unwrap();
            provider_backing + u128::from(pending)
        };
        let backing_after_first_loss = backing_protection(&env);
        assert!(backing_after_first_loss < 15);
        if sync_first_loss_before_recovery {
            env.cross_backing_withdraw(&first_sync, &first_sync_ata, &pool_holding, 1)
                .expect("zero-value exit records the first real owner loss");
            assert_eq!(env.token_amount(&first_sync_ata), 0);
        }
        if !sync_recovery_before_second_loss {
            assert!(sync_first_loss_before_recovery);
            env.cross_backing_withdraw(
                &recovery_sync,
                &recovery_sync_ata,
                &pool_holding,
                1,
            )
            .expect("control checkpoints only the first-loss backing low");
            assert_eq!(env.token_amount(&recovery_sync_ata), 0);
        }

        let funding_size_q = (percolator::POS_SCALE / 10) as i128;
        let (funding_long, funding_long_portfolio, funding_short, funding_short_portfolio) =
            open_public_pair(&mut env, funding_size_q, 303, 1_000_000);
        advance_public_mark(
            &mut env,
            &oracle,
            observer_portfolio,
            &mut slot,
            1_000_000,
            300,
        );
        for portfolio in [funding_long_portfolio, funding_short_portfolio] {
            for _ in 0..64 {
                public_percolator_crank(&mut env, portfolio, slot, true)
                    .expect("public funding settlement makes bounded progress");
            }
        }
        env.send(
            &[Instruction {
                program_id: perc_id(),
                accounts: vec![
                    AccountMeta::new(funding_long.pubkey(), true),
                    AccountMeta::new(funding_short.pubkey(), true),
                    AccountMeta::new(env.slab, false),
                    AccountMeta::new(funding_long_portfolio, false),
                    AccountMeta::new(funding_short_portfolio, false),
                ],
                data: PIx::TradeNoCpi {
                    asset_index: 0,
                    size_q: -funding_size_q,
                    exec_price: effective_price(&env),
                    fee_bps: 0,
                }
                .encode(),
            }],
            &[&funding_long, &funding_short],
        )
        .expect("the fully collateralized funding pair flattens");
        for portfolio in [funding_long_portfolio, funding_short_portfolio] {
            for _ in 0..8 {
                public_percolator_crank(&mut env, portfolio, slot, false)
                    .expect("flat funding portfolio settles");
            }
        }
        let reverse_entry = effective_price(&env);
        let (
            reverse_funding_long,
            reverse_funding_long_portfolio,
            reverse_funding_short,
            reverse_funding_short_portfolio,
        ) = open_public_pair(&mut env, funding_size_q, reverse_entry, 1_000_000);
        advance_public_mark(
            &mut env,
            &oracle,
            observer_portfolio,
            &mut slot,
            303,
            300,
        );
        for portfolio in [
            reverse_funding_long_portfolio,
            reverse_funding_short_portfolio,
        ] {
            for _ in 0..64 {
                public_percolator_crank(&mut env, portfolio, slot, true)
                    .expect("reverse funding settlement makes bounded progress");
            }
        }
        env.send(
            &[Instruction {
                program_id: perc_id(),
                accounts: vec![
                    AccountMeta::new(reverse_funding_long.pubkey(), true),
                    AccountMeta::new(reverse_funding_short.pubkey(), true),
                    AccountMeta::new(env.slab, false),
                    AccountMeta::new(reverse_funding_long_portfolio, false),
                    AccountMeta::new(reverse_funding_short_portfolio, false),
                ],
                data: PIx::TradeNoCpi {
                    asset_index: 0,
                    size_q: -funding_size_q,
                    exec_price: effective_price(&env),
                    fee_bps: 0,
                }
                .encode(),
            }],
            &[&reverse_funding_long, &reverse_funding_short],
        )
        .expect("the reverse fully collateralized funding pair flattens");
        for portfolio in [
            reverse_funding_long_portfolio,
            reverse_funding_short_portfolio,
        ] {
            for _ in 0..8 {
                public_percolator_crank(&mut env, portfolio, slot, false)
                    .expect("flat reverse-funding portfolio settles");
            }
        }
        let backing_after_recovery = backing_protection(&env);
        assert!(
            backing_after_recovery > backing_after_first_loss,
            "organic funding must refill at least one previously lost backing atom: first={backing_after_first_loss}, recovery={backing_after_recovery}",
        );

        if sync_recovery_before_second_loss {
            if !sync_first_loss_before_recovery {
                env.cross_backing_withdraw(
                    &first_sync,
                    &first_sync_ata,
                    &pool_holding,
                    1,
                )
                .expect("first owner action records the loss after protocol recovery");
            }
            env.cross_backing_withdraw(
                &recovery_sync,
                &recovery_sync_ata,
                &pool_holding,
                1,
            )
            .expect("attacker checkpoints the protocol-funded recovery");
            assert_eq!(env.token_amount(&recovery_sync_ata), 0);
        }

        let second_entry = effective_price(&env);
        let second_target = second_entry.checked_add(1_000_010).unwrap();
        let (second_long, second_long_portfolio, second_short, second_short_portfolio) =
            open_public_pair_with_capitals(
                &mut env,
                (percolator::POS_SCALE / 10) as i128,
                second_entry,
                1_000_000,
                100_000,
            );
        advance_public_mark(
            &mut env,
            &oracle,
            observer_portfolio,
            &mut slot,
            second_target,
            300,
        );
        liquidate_stale_public_loser(&mut env, second_short_portfolio, slot);
        clear_stale_public_winner_with_backing(
            &mut env,
            &second_long,
            second_long_portfolio,
            slot,
        );
        let backing_after_second_loss = backing_protection(&env);
        assert!(
            backing_after_second_loss >= backing_after_first_loss,
            "the second loss must consume only intervening protocol-funded recovery: first={backing_after_first_loss}, second={backing_after_second_loss}",
        );

        let market_admin = clone_kp(&env.market_admin);
        env.send(
            &[Instruction {
                program_id: perc_id(),
                accounts: vec![
                    AccountMeta::new_readonly(market_admin.pubkey(), true),
                    AccountMeta::new(env.slab, false),
                ],
                data: PIx::ResolveMarket.encode(),
            }],
            &[&market_admin],
        )
        .expect("resolve after both bounded loss epochs");
        close_resolved_portfolios(
            &mut env,
            &[
                (&observer, observer_portfolio),
                (&first_long, first_long_portfolio),
                (&first_short, first_short_portfolio),
                (&funding_long, funding_long_portfolio),
                (&funding_short, funding_short_portfolio),
                (&reverse_funding_long, reverse_funding_long_portfolio),
                (&reverse_funding_short, reverse_funding_short_portfolio),
                (&second_long, second_long_portfolio),
                (&second_short, second_short_portfolio),
            ],
        );

        env.cross_backing_withdraw(
            &victim,
            &victim_ata,
            &pool_holding,
            victim_principal,
        )
        .expect("independent victim retains a bounded terminal exit");

        Outcome {
            victim_payout: env.token_amount(&victim_ata),
            first_sync_payout: env.token_amount(&first_sync_ata),
            recovery_sync_payout: env.token_amount(&recovery_sync_ata),
            protocol_value_after_exit: u128::from(env.token_amount(&env.perc_vault))
                + u128::from(env.token_amount(&pool_holding)),
            backing_after_first_loss,
            backing_after_recovery,
            backing_after_second_loss,
        }
    };

    let control = run(true, false);
    let checkpointed_recovery = run(true, true);
    assert_eq!(
        checkpointed_recovery.backing_after_first_loss,
        control.backing_after_first_loss,
    );
    assert_eq!(
        checkpointed_recovery.backing_after_recovery,
        control.backing_after_recovery,
    );
    assert_eq!(
        checkpointed_recovery.backing_after_second_loss,
        control.backing_after_second_loss,
    );
    assert_eq!(
        (
            checkpointed_recovery.victim_payout,
            checkpointed_recovery.protocol_value_after_exit,
        ),
        (control.victim_payout, control.protocol_value_after_exit),
        "a zero-value checkpoint cannot charge protocol recovery to the victim twice",
    );
    assert_eq!(checkpointed_recovery.first_sync_payout, 0);
    assert_eq!(checkpointed_recovery.recovery_sync_payout, 0);

    let delayed_first_observation = run(false, true);
    assert_eq!(
        delayed_first_observation.backing_after_first_loss,
        control.backing_after_first_loss,
    );
    assert_eq!(
        delayed_first_observation.backing_after_recovery,
        control.backing_after_recovery,
    );
    assert_eq!(
        delayed_first_observation.backing_after_second_loss,
        control.backing_after_second_loss,
    );
    assert_eq!(
        u128::from(delayed_first_observation.victim_payout)
            + delayed_first_observation.protocol_value_after_exit,
        u128::from(control.victim_payout) + control.protocol_value_after_exit,
        "the delayed path must conserve the same owner and protocol value",
    );
    assert_eq!(
        (
            delayed_first_observation.first_sync_payout,
            delayed_first_observation.recovery_sync_payout,
            delayed_first_observation.victim_payout,
            delayed_first_observation.protocol_value_after_exit,
        ),
        (
            control.first_sync_payout,
            control.recovery_sync_payout,
            control.victim_payout,
            control.protocol_value_after_exit,
        ),
        "a delayed checkpoint cannot move protocol recovery into an owner payout",
    );
}

// PUBLIC LOF PROBE: principal-only insurance owners bear venue losses but do not own trade fees.
// Generate the same bankruptcy with and without opening fees; fees may increase protocol reserve,
// but they must not restore an already-incurred owner claim.
#[test]
fn public_trade_fees_cannot_erase_an_ordinary_principal_pool_loss() {
    use percolator_prog::ix::Instruction as PIx;

    #[derive(Clone, Copy, Debug)]
    struct Outcome {
        owner_payout: u64,
        insurance_spent: u128,
        protected_before_exit: u128,
        protocol_reserve: u128,
    }

    let run = |opening_fee_bps: u64| {
        let mut env = Env::new();
        let oracle = Keypair::new();
        env.svm.airdrop(&oracle.pubkey(), 1_000_000_000).unwrap();
        install_public_loss_fixture(&mut env, &oracle.pubkey());
        env.init_insurance_pool();

        let principal = 29u64;
        let (owner, owner_ata) = new_depositor(&mut env, principal);
        let pool = env.pool;
        let holding = create_holding(&mut env, &pool);
        env.insurance_deposit(&owner, &owner_ata, &holding, principal)
            .expect("the independent owner funds principal-only insurance");

        env.send(
            &[Instruction {
                program_id: perc_id(),
                accounts: vec![
                    AccountMeta::new_readonly(oracle.pubkey(), true),
                    AccountMeta::new(env.slab, false),
                ],
                data: PIx::ConfigureAuthMark {
                    asset_index: 0,
                    now_slot: 100,
                    initial_mark_e6: 100,
                }
                .encode(),
            }],
            &[&oracle],
        )
        .expect("configure the authenticated public-loss mark");

        let long = Keypair::new();
        let short = Keypair::new();
        for trader in [&long, &short] {
            env.svm.airdrop(&trader.pubkey(), 1_000_000_000).unwrap();
        }
        let opening_fee = 100u64
            .checked_mul(opening_fee_bps)
            .and_then(|product| product.checked_add(9_999))
            .map(|product| product / 10_000)
            .unwrap();
        let long_portfolio = create_percolator_portfolio(&mut env, &long, 1_000_000);
        let short_portfolio =
            create_percolator_portfolio(&mut env, &short, 200 + opening_fee);
        env.send(
            &[Instruction {
                program_id: perc_id(),
                accounts: vec![
                    AccountMeta::new(long.pubkey(), true),
                    AccountMeta::new(short.pubkey(), true),
                    AccountMeta::new(env.slab, false),
                    AccountMeta::new(long_portfolio, false),
                    AccountMeta::new(short_portfolio, false),
                ],
                data: PIx::TradeNoCpi {
                    asset_index: 0,
                    size_q: percolator::POS_SCALE as i128,
                    exec_price: 100,
                    fee_bps: opening_fee_bps,
                }
                .encode(),
            }],
            &[&long, &short],
        )
        .expect("public users open the fee-controlled loss pair");

        let mut slot = 100u64;
        advance_public_mark(
            &mut env,
            &oracle,
            long_portfolio,
            &mut slot,
            301,
            300,
        );
        liquidate_stale_public_loser(&mut env, short_portfolio, slot);
        clear_stale_public_winner(&mut env, &long, long_portfolio, slot);

        env.send(
            &[Instruction {
                program_id: perc_id(),
                accounts: vec![
                    AccountMeta::new_readonly(oracle.pubkey(), true),
                    AccountMeta::new(env.slab, false),
                ],
                data: PIx::ResolveMarket.encode(),
            }],
            &[&oracle],
        )
        .expect("resolve after bounded public loss cleanup");
        close_resolved_portfolios(
            &mut env,
            &[(&long, long_portfolio), (&short, short_portfolio)],
        );

        let market = env.svm.get_account(&env.slab).unwrap();
        let insurance_spent = percolator_accounting::read_asset_insurance_spent(&market.data, 0)
            .unwrap()
            .into_iter()
            .sum::<u128>();
        let protected_before_exit = asset_insurance_remaining(&env, 0);
        env.insurance_withdraw(&owner, &owner_ata, &holding, &owner, principal)
            .expect("the owner realizes the loss-adjusted principal claim");
        Outcome {
            owner_payout: env.token_amount(&owner_ata),
            insurance_spent,
            protected_before_exit,
            protocol_reserve: asset_insurance_remaining(&env, 0),
        }
    };

    let control = run(0);
    let with_fees = run(100);
    assert!(control.insurance_spent > 0);
    assert_eq!(with_fees.insurance_spent, control.insurance_spent);
    assert!(with_fees.protected_before_exit > control.protected_before_exit);
    assert_eq!(
        with_fees.owner_payout, control.owner_payout,
        "protocol trade fees cannot erase the independent owner's venue loss",
    );
    assert!(with_fees.protocol_reserve > control.protocol_reserve);
}

// PUBLIC LOF: wallet retries may carry distinct transaction signatures for one
// intended deposit. The Subledger action itself must bind the position snapshot,
// or every retry can expose another tranche of the owner's tokens to market loss.
#[test]
fn presigned_insurance_deposit_retry_cannot_double_expose_principal() {
    use percolator_prog::ix::Instruction as PIx;

    let mut env = Env::new();
    let oracle = Keypair::new();
    let observer = Keypair::new();
    for actor in [&oracle, &observer] {
        env.svm.airdrop(&actor.pubkey(), 1_000_000_000).unwrap();
    }
    install_public_loss_fixture(&mut env, &oracle.pubkey());
    env.init_insurance_pool();

    let tranche = 30u64;
    let (owner, owner_ata) = new_depositor(&mut env, 2 * tranche);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    let mut deposit_data = vec![4u8]; // IX_INSURANCE_DEPOSIT
    deposit_data.extend_from_slice(&tranche.to_le_bytes());
    let deposit = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(owner.pubkey(), true),
            AccountMeta::new(env.pool, false),
            AccountMeta::new(env.position_pda(&owner.pubkey()), false),
            AccountMeta::new(owner_ata, false),
            AccountMeta::new(holding, false),
            AccountMeta::new(env.slab, false),
            AccountMeta::new(env.perc_vault, false),
            AccountMeta::new_readonly(perc_id(), false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: deposit_data,
    };

    env.svm.expire_blockhash();
    let held_blockhash = env.svm.latest_blockhash();
    let payer = clone_kp(&env.payer);
    let first = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            deposit.clone(),
        ],
        Some(&payer.pubkey()),
        &[&payer, &owner],
        held_blockhash,
    );
    let retry = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_399_999),
            deposit,
        ],
        Some(&payer.pubkey()),
        &[&payer, &owner],
        held_blockhash,
    );
    env.svm
        .send_transaction(first)
        .expect("the intended deposit executes");
    let retry_result = env.svm.send_transaction(retry);

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ConfigureAuthMark {
                asset_index: 0,
                now_slot: 100,
                initial_mark_e6: 100,
            }
            .encode(),
        }],
        &[&oracle],
    )
    .expect("configure the authenticated public-loss mark");

    let live_principal = env.read_position(&owner.pubkey()).0;
    let domain_tranche = live_principal / 2;
    let observer_portfolio = create_percolator_portfolio(&mut env, &observer, 0);
    let mut slot = 100u64;
    let (low_long, low_long_portfolio, low_short, low_short_portfolio) =
        open_public_pair_with_capitals(
            &mut env,
            percolator::POS_SCALE as i128,
            100,
            1_000_000,
            200,
        );
    advance_public_mark(
        &mut env,
        &oracle,
        observer_portfolio,
        &mut slot,
        300 + domain_tranche,
        100,
    );
    liquidate_stale_public_loser(&mut env, low_short_portfolio, slot);
    clear_stale_public_winner(&mut env, &low_long, low_long_portfolio, slot);
    assert_eq!(
        asset_insurance_remaining(&env, 0),
        u128::from(domain_tranche),
        "the first public bankruptcy consumes one insurance domain",
    );

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ResolveMarket.encode(),
        }],
        &[&oracle],
    )
    .expect("resolve after the public loss");
    close_resolved_portfolios(
        &mut env,
        &[
            (&low_long, low_long_portfolio),
            (&low_short, low_short_portfolio),
            (&observer, observer_portfolio),
        ],
    );

    let market = env.svm.get_account(&env.slab).unwrap();
    let insurance_spent = percolator_accounting::read_asset_insurance_spent(&market.data, 0)
        .unwrap()
        .into_iter()
        .sum::<u128>();
    assert_eq!(
        insurance_spent,
        u128::from(domain_tranche),
        "the public loss consumes half of every accepted deposit",
    );
    env.insurance_withdraw(
        &owner,
        &owner_ata,
        &holding,
        &owner,
        live_principal,
    )
    .expect("the owner retires the fully impaired position");

    assert_eq!(
        env.token_amount(&owner_ata),
        tranche + tranche / 2,
        "only the intended tranche can enter the market-loss waterfall",
    );
    assert_eq!(
        live_principal, tranche,
        "one intended deposit creates one loss-bearing claim",
    );
    assert!(
        retry_result.is_err(),
        "a distinct signed retry cannot expose a second tranche",
    );
}

#[test]
fn public_full_cross_backing_impairment_cannot_capture_fresh_recapitalization() {
    use percolator_prog::ix::Instruction as PIx;

    let mut env = Env::new_cross_backing();
    let oracle = Keypair::new();
    let observer = Keypair::new();
    for owner in [&oracle, &observer] {
        env.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
    }
    install_public_loss_fixture_with_margin(&mut env, &oracle.pubkey(), 1_000);
    env.init_cross_backing_genesis_pool();
    assert_eq!(env.svm.get_account(&env.pool).unwrap().data.len(), 361);

    let high_entry = 200u64;
    let side_protection = 2_000_000u64;
    let high_capital = 98_000_000u64;
    let impaired_principal = side_protection.checked_mul(2).unwrap();
    let (impaired_owner, impaired_ata) = new_depositor(&mut env, impaired_principal);
    let pool_holding = create_canonical_pool_holding(&mut env);
    env.cross_backing_deposit(
        &impaired_owner,
        &impaired_ata,
        &pool_holding,
        impaired_principal,
    )
    .expect("fund both cross-backing loss domains");

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ConfigureAuthMark {
                asset_index: 0,
                now_slot: 100,
                initial_mark_e6: 100,
            }
            .encode(),
        }],
        &[&oracle],
    )
    .expect("configure the authenticated mark");
    let observer_portfolio = create_percolator_portfolio(&mut env, &observer, 0);
    let position_q = 1_000_000_000_000i128;
    let pnl_atoms_per_price = position_q.unsigned_abs() / percolator::POS_SCALE;
    let low_capital = 10_000_000u64;
    assert_eq!(
        (u128::from(side_protection) + u128::from(low_capital)) % pnl_atoms_per_price,
        0,
    );

    let (low_long, low_long_portfolio, low_short, low_short_portfolio) =
        open_public_pair(&mut env, position_q, 100, low_capital);
    let high_target = 100u64
        .checked_add(
            u64::try_from(
                (u128::from(side_protection) + u128::from(low_capital))
                    / pnl_atoms_per_price,
            )
            .unwrap(),
        )
        .unwrap();
    let mut slot = 100;
    advance_public_mark(
        &mut env,
        &oracle,
        observer_portfolio,
        &mut slot,
        high_target,
        300,
    );
    liquidate_stale_public_loser(&mut env, low_short_portfolio, slot);
    clear_stale_public_winner_with_backing(&mut env, &low_long, low_long_portfolio, slot);

    advance_public_mark(
        &mut env,
        &oracle,
        observer_portfolio,
        &mut slot,
        high_entry,
        100,
    );
    let (high_long, high_long_portfolio, high_short, high_short_portfolio) =
        open_public_pair(&mut env, position_q, high_entry, high_capital);
    advance_public_mark(
        &mut env,
        &oracle,
        observer_portfolio,
        &mut slot,
        100,
        400,
    );
    liquidate_stale_public_loser(&mut env, high_long_portfolio, slot);
    clear_stale_public_winner_with_backing(&mut env, &high_short, high_short_portfolio, slot);

    let protected = {
        let market = env.svm.get_account(&env.slab).unwrap();
        let balances =
            percolator_accounting::read_asset_backing_balances(&market.data, 0).unwrap();
        let sources =
            percolator_accounting::read_asset_backing_source_credits(&market.data, 0).unwrap();
        let ledgers = [0u16, 1u16].map(|domain| {
            let ledger = env
                .svm
                .get_account(&cross_backing_ledger_pda(&env.pool, domain))
                .unwrap();
            percolator_prog::state::read_backing_domain_ledger(&ledger.data)
                .unwrap()
                .total_principal_atoms
        });
        asset_insurance_remaining(&env, 0)
            + balances
                .into_iter()
                .zip(sources)
                .zip(ledgers)
                .map(|((balance, source), principal)| {
                    balance
                        .provider_protected_principal_atoms(principal, source)
                        .unwrap()
                })
                .sum::<u128>()
    };
    assert_eq!(protected, 0, "both owner-protection domains are fully impaired");

    let recapitalization = 1_000_000u64;
    let payer = clone_kp(&env.payer);
    let mint_authority = clone_kp(&env.mint_auth);
    let mint = env.mint;
    mint_to(
        &mut env.svm,
        &payer,
        &mint,
        &mint_authority,
        &impaired_ata,
        recapitalization,
    );
    env.cross_backing_deposit(
        &impaired_owner,
        &impaired_ata,
        &pool_holding,
        recapitalization,
    )
    .expect("the impaired owner adds fresh capital to its stale position PDA");
    let (fresh_owner, fresh_ata) = new_depositor(&mut env, recapitalization);
    env.cross_backing_deposit(
        &fresh_owner,
        &fresh_ata,
        &pool_holding,
        recapitalization,
    )
    .expect("fresh owner recapitalizes the fully impaired pool");
    let recapitalized_pool = env.svm.get_account(&env.pool).unwrap().data;
    assert_eq!(
        u128::from_le_bytes(recapitalized_pool[289..305].try_into().unwrap()),
        1,
    );
    assert_eq!(
        u128::from_le_bytes(recapitalized_pool[305..321].try_into().unwrap()),
        1_000_000,
    );
    assert_eq!(
        env.position_share_generation(&impaired_owner.pubkey()),
        env.pool_share_generation(),
    );
    assert_eq!(
        env.position_share_generation(&fresh_owner.pubkey()),
        env.pool_share_generation(),
    );

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ResolveMarket.encode(),
        }],
        &[&oracle],
    )
    .expect("resolve after both public loss sides are cleared");
    close_resolved_portfolios(
        &mut env,
        &[
            (&observer, observer_portfolio),
            (&low_long, low_long_portfolio),
            (&low_short, low_short_portfolio),
            (&high_long, high_long_portfolio),
            (&high_short, high_short_portfolio),
        ],
    );

    env.cross_backing_withdraw(
        &impaired_owner,
        &impaired_ata,
        &pool_holding,
        impaired_principal,
    )
    .expect("the mixed position retires stale nominal principal pro rata");
    assert_eq!(
        env.token_amount(&impaired_ata),
        800_000,
        "withdrawing stale nominal principal cannot claim all mixed fresh shares",
    );
    env.cross_backing_withdraw(
        &fresh_owner,
        &fresh_ata,
        &pool_holding,
        recapitalization,
    )
    .expect("the independent current generation retains every recapitalization atom");
    assert_eq!(env.token_amount(&fresh_ata), recapitalization);
    env.cross_backing_withdraw(
        &impaired_owner,
        &impaired_ata,
        &pool_holding,
        recapitalization,
    )
    .expect("the mixed position collects only its remaining fresh share value");
    assert_eq!(env.token_amount(&impaired_ata), recapitalization);
    assert_eq!(env.pool_outstanding(), 0);
}

#[test]
fn bootstrap_unlock_cannot_expire_genesis_backing_before_provider_exit() {
    use percolator_prog::ix::Instruction as PIx;

    let mut env = Env::new_cross_backing();
    let oracle = Keypair::new();
    let observer = Keypair::new();
    for owner in [&oracle, &observer] {
        env.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
    }
    install_public_loss_fixture(&mut env, &oracle.pubkey());
    env.init_cross_backing_genesis_pool();

    let principal = 400_000_000u64;
    let (depositor, depositor_ata) = new_depositor(&mut env, principal);
    let pool_holding = create_canonical_pool_holding(&mut env);
    env.cross_backing_deposit(&depositor, &depositor_ata, &pool_holding, principal)
        .expect("fund Genesis insurance and backing");
    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ConfigureAuthMark {
                asset_index: 0,
                // The deployed program authenticates this against Clock, so even
                // the oracle cannot force the non-expiring sentinel to mature.
                now_slot: u64::MAX,
                initial_mark_e6: 100,
            }
            .encode(),
        }],
        &[&oracle],
    )
    .expect("configure the independent authenticated mark");

    let observer_portfolio = create_percolator_portfolio(&mut env, &observer, 0);
    let (long, long_portfolio, short, short_portfolio) =
        open_public_pair(&mut env, 1_000_000_000_000, 100, 100_000_000);
    let mut slot = 100;
    advance_public_mark(&mut env, &oracle, observer_portfolio, &mut slot, 101, 4);
    public_percolator_crank(&mut env, long_portfolio, slot, false)
        .expect("settle the solvent winner and create its source lien");
    public_percolator_crank(&mut env, short_portfolio, slot, false)
        .expect("settle the solvent loser");
    let winner = percolator_prog::state::read_portfolio(
        &env.svm.get_account(&long_portfolio).unwrap().data,
    )
    .unwrap();
    assert!(winner.pnl.get() > 0);
    assert!(
        winner
            .source_domains
            .into_iter()
            .any(|source| source.source_claim_bound_num.get() != 0),
        "the live winner has a genuine claim against Genesis backing",
    );

    assert!(
        env.cross_backing_withdraw(
            &depositor,
            &depositor_ata,
            &pool_holding,
            principal,
        )
        .is_err(),
        "live market exposure prevents the provider from escaping before unlock",
    );
    assert_eq!(env.pool_outstanding(), principal);

    env.warp_slot(env.bootstrap_end_slot());
    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ResolveMarket.encode(),
        }],
        &[&oracle],
    )
    .expect("resolve at the configured Genesis unlock boundary");
    close_resolved_portfolios(
        &mut env,
        &[
            (&observer, observer_portfolio),
            (&long, long_portfolio),
            (&short, short_portfolio),
        ],
    );
    env.cross_backing_withdraw(
        &depositor,
        &depositor_ata,
        &pool_holding,
        principal,
    )
    .expect("the provider exits after market risk clears");

    assert_eq!(env.token_amount(&depositor_ata), principal);
    assert_eq!(env.token_amount(&env.perc_vault), 0);
    assert_eq!(env.token_amount(&pool_holding), 0);
    assert_eq!(env.pool_outstanding(), 0);
}

// PUBLIC LOF: a bankrupt trade temporarily materializes counterparty backing
// that is not owned by the genesis provider ledgers. A fresh depositor entering
// after every leg is cleared must not price against value that deterministic
// resolved-portfolio cleanup will remove, or the old owner captures fresh funds.
#[test]
fn transient_trader_backing_cannot_recapitalize_an_old_generation() {
    use percolator_prog::ix::Instruction as PIx;

    let mut env = Env::new_cross_backing();
    let oracle = Keypair::new();
    let observer = Keypair::new();
    for owner in [&oracle, &observer] {
        env.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
    }
    install_public_loss_fixture_with_margin(&mut env, &oracle.pubkey(), 1_000);
    env.init_cross_backing_genesis_pool();

    let principal = 4_000u64;
    let trader_capital = 1_000_000u64;
    let (old_owner, old_ata) = new_depositor(&mut env, principal);
    let pool_holding = create_canonical_pool_holding(&mut env);
    env.cross_backing_deposit(&old_owner, &old_ata, &pool_holding, principal)
        .expect("fund the loss-bearing genesis position");

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ConfigureAuthMark {
                asset_index: 0,
                now_slot: 100,
                initial_mark_e6: 100,
            }
            .encode(),
        }],
        &[&oracle],
    )
    .expect("configure authenticated mark");
    let observer_portfolio = create_percolator_portfolio(&mut env, &observer, 0);
    let (long, long_portfolio, short, short_portfolio) = open_public_pair(
        &mut env,
        percolator::POS_SCALE as i128,
        100,
        trader_capital,
    );
    let mut slot = 100;
    advance_public_mark(
        &mut env,
        &oracle,
        observer_portfolio,
        &mut slot,
        1_002_100,
        300,
    );
    liquidate_stale_public_loser(&mut env, short_portfolio, slot);
    clear_stale_public_winner_with_backing(&mut env, &long, long_portfolio, slot);

    let transient_balances = percolator_accounting::read_asset_backing_balances(
        &env.svm.get_account(&env.slab).unwrap().data,
        0,
    )
    .unwrap();
    let transient_sources = percolator_accounting::read_asset_backing_source_credits(
        &env.svm.get_account(&env.slab).unwrap().data,
        0,
    )
    .unwrap();
    let transient_backing = transient_balances
        .into_iter()
        .map(|balance| balance.protected_principal_atoms().unwrap())
        .sum::<u128>();
    assert!(
        transient_backing > u128::from(principal),
        "the public loss must materialize non-provider trader backing",
    );
    let ledger_principals = [0u16, 1u16].map(|domain| {
        let account = env
            .svm
            .get_account(&cross_backing_ledger_pda(&env.pool, domain))
            .unwrap();
        percolator_prog::state::read_backing_domain_ledger(&account.data)
            .unwrap()
            .total_principal_atoms
    });
    let provider_backing = transient_balances
        .into_iter()
        .zip(ledger_principals)
        .zip(transient_sources)
        .map(|((balance, principal), source)| {
            balance
                .provider_protected_principal_atoms(principal, source)
                .unwrap()
        })
        .sum::<u128>();
    assert_eq!(
        asset_insurance_remaining(&env, 0) + provider_backing,
        u128::from(principal / 2),
        "provider-ledger pricing observes the complete pre-deposit impairment: balances={transient_balances:?}, principals={ledger_principals:?}",
    );

    let (fresh_owner, fresh_ata) = new_depositor(&mut env, principal);
    env.cross_backing_deposit(&fresh_owner, &fresh_ata, &pool_holding, principal)
        .expect("fresh owner deposits after every public leg is cleared");

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ResolveMarket.encode(),
        }],
        &[&oracle],
    )
    .expect("resolve without exposing the fresh deposit to another trade");
    close_resolved_portfolios(
        &mut env,
        &[
            (&observer, observer_portfolio),
            (&long, long_portfolio),
            (&short, short_portfolio),
        ],
    );

    let protected_after_cleanup = asset_insurance_remaining(&env, 0)
        .checked_add(
            percolator_accounting::read_asset_backing_balances(
                &env.svm.get_account(&env.slab).unwrap().data,
                0,
            )
            .unwrap()
            .into_iter()
            .map(|balance| balance.protected_principal_atoms().unwrap())
            .sum::<u128>(),
        )
        .unwrap();
    assert_eq!(protected_after_cleanup, 3 * u128::from(principal) / 2);
    let cleanup_balances = percolator_accounting::read_asset_backing_balances(
        &env.svm.get_account(&env.slab).unwrap().data,
        0,
    )
    .unwrap();
    let cleanup_ledger_principals = [0u16, 1u16].map(|domain| {
        let account = env
            .svm
            .get_account(&cross_backing_ledger_pda(&env.pool, domain))
            .unwrap();
        percolator_prog::state::read_backing_domain_ledger(&account.data)
            .unwrap()
            .total_principal_atoms
    });
    let cleanup_sources = percolator_accounting::read_asset_backing_source_credits(
        &env.svm.get_account(&env.slab).unwrap().data,
        0,
    )
    .unwrap();
    assert!(cleanup_sources
        .iter()
        .all(|source| source.exact_positive_claim_num == 0));
    let cleanup_provider_backing = cleanup_balances
        .into_iter()
        .zip(cleanup_ledger_principals)
        .zip(cleanup_sources)
        .map(|((balance, principal), source)| {
            balance
                .provider_protected_principal_atoms(principal, source)
                .unwrap()
        })
        .sum::<u128>();
    assert_eq!(
        asset_insurance_remaining(&env, 0) + cleanup_provider_backing,
        protected_after_cleanup,
    );

    let old_before = env.token_amount(&old_ata);
    let fresh_before = env.token_amount(&fresh_ata);
    env.cross_backing_withdraw(&old_owner, &old_ata, &pool_holding, principal)
        .expect("old owner realizes only the pre-deposit market loss");
    env.cross_backing_withdraw(&fresh_owner, &fresh_ata, &pool_holding, principal)
        .expect("fresh owner exits after taking no additional market risk");
    assert_eq!(
        env.token_amount(&old_ata) - old_before,
        principal / 2,
        "the old generation realizes its pre-deposit market loss",
    );
    assert!(
        env.token_amount(&fresh_ata) - fresh_before >= principal - 1,
        "the old generation cannot capture more than the documented rounding atom",
    );
}

// PUBLIC DOS: one Genesis atom funds only the short backing bucket. A trader
// loss can then materialize source backing in the empty long bucket with
// Percolator's short fallback expiry. The pre-bound, zero-principal long ledger
// must not inherit that expiry or block later Genesis backing.
#[test]
fn transient_source_backing_cannot_block_a_later_genesis_deposit() {
    use percolator_prog::ix::Instruction as PIx;

    let mut env = Env::new_cross_backing();
    let oracle = Keypair::new();
    let observer = Keypair::new();
    for owner in [&oracle, &observer] {
        env.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
    }
    install_public_loss_fixture_with_margin(&mut env, &oracle.pubkey(), 1_000);
    env.init_cross_backing_genesis_pool();
    let pool_holding = create_canonical_pool_holding(&mut env);

    let (first, first_ata) = new_depositor(&mut env, 1);
    env.cross_backing_deposit(&first, &first_ata, &pool_holding, 1)
        .expect("the first voter initializes one aggregate backing atom");
    let first_backing = percolator_accounting::read_asset_backing_balances(
        &env.svm.get_account(&env.slab).unwrap().data,
        0,
    )
    .unwrap();
    assert_eq!(first_backing.map(|balance| balance.principal_atoms), [0, 1]);

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ConfigureAuthMark {
                asset_index: 0,
                now_slot: 100,
                initial_mark_e6: 100,
            }
            .encode(),
        }],
        &[&oracle],
    )
    .expect("configure the independent authenticated mark");
    let observer_portfolio = create_percolator_portfolio(&mut env, &observer, 0);
    let (long, long_portfolio, short, short_portfolio) =
        open_public_pair(&mut env, 100_000_000_000, 100, 1_000_000);
    let mut slot = 100;
    advance_public_mark(&mut env, &oracle, observer_portfolio, &mut slot, 89, 10);
    liquidate_stale_public_loser(&mut env, long_portfolio, slot);
    clear_stale_public_winner_with_backing(&mut env, &short, short_portfolio, slot);

    let market = env.svm.get_account(&env.slab).unwrap();
    let transient_backing =
        percolator_accounting::read_asset_backing_balances(&market.data, 0).unwrap();
    let transient_sources =
        percolator_accounting::read_asset_backing_source_credits(&market.data, 0).unwrap();
    assert!(transient_backing[0].protected_principal_atoms().unwrap() > 0);
    assert!(transient_sources[0].exact_positive_claim_num > 0);
    let long_ledger = env
        .svm
        .get_account(&cross_backing_ledger_pda(&env.pool, 0))
        .unwrap();
    let long_ledger = percolator_prog::state::read_backing_domain_ledger(&long_ledger.data)
        .expect("both canonical ledgers are bound by the first valid deposit");
    assert_eq!(long_ledger.market_group, env.slab.to_bytes());
    assert_eq!(long_ledger.authority, env.pool.to_bytes());
    assert_eq!(long_ledger.domain, 0);
    assert_eq!(
        long_ledger.total_principal_atoms, 0,
        "the long bucket contains only trader source backing",
    );

    let (second, second_ata) = new_depositor(&mut env, 1);
    env.cross_backing_deposit(&second, &second_ata, &pool_holding, 1)
        .expect("the next aggregate atom funds insurance");
    let (third, third_ata) = new_depositor(&mut env, 1);
    env.cross_backing_deposit(&third, &third_ata, &pool_holding, 1)
        .expect("trader source expiry cannot veto later Genesis backing");

    assert_eq!(env.pool_outstanding(), 3);
    assert_eq!(env.token_amount(&first_ata), 0);
    assert_eq!(env.token_amount(&second_ata), 0);
    assert_eq!(env.token_amount(&third_ata), 0);

    let pool_data = env.svm.get_account(&env.pool).unwrap().data;
    assert_eq!(pool_data.len(), 361, "current cross-backed pool layout");
    assert_eq!(
        [
            u64::from_le_bytes(pool_data[273..281].try_into().unwrap()),
            u64::from_le_bytes(pool_data[281..289].try_into().unwrap()),
        ],
        [1, 0],
        "the incompatible long-domain atom remains explicit owner principal",
    );
    assert_eq!(env.token_amount(&pool_holding), 1);

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ResolveMarket.encode(),
        }],
        &[&oracle],
    )
    .expect("resolve after the deposit window attack");
    close_resolved_portfolios(
        &mut env,
        &[
            (&observer, observer_portfolio),
            (&long, long_portfolio),
            (&short, short_portfolio),
        ],
    );

    for (owner, destination) in [
        (&first, &first_ata),
        (&second, &second_ata),
        (&third, &third_ata),
    ] {
        env.cross_backing_withdraw(owner, destination, &pool_holding, 1)
            .expect("each one-vote depositor retains an owner-bound exit");
        assert_eq!(env.token_amount(destination), 1);
    }
    let final_pool_data = env.svm.get_account(&env.pool).unwrap().data;
    assert_eq!(
        [
            u64::from_le_bytes(final_pool_data[273..281].try_into().unwrap()),
            u64::from_le_bytes(final_pool_data[281..289].try_into().unwrap()),
        ],
        [0, 0],
        "all staged owner principal is retired with the final claim",
    );
    assert_eq!(env.pool_outstanding(), 0);
}

// PUBLIC DOS PROBE: a trader can choose either side of a loss sequence. If both
// transient source-backed buckets carry an incompatible expiry before Genesis,
// every backing atom must remain staged without blocking deposits or owner exits.
#[test]
fn bilateral_transient_sources_cannot_block_genesis_backing_or_refunds() {
    use percolator_prog::ix::Instruction as PIx;

    let mut env = Env::new_cross_backing();
    let oracle = Keypair::new();
    let observer = Keypair::new();
    for owner in [&oracle, &observer] {
        env.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
    }
    install_public_loss_fixture_with_margin(&mut env, &oracle.pubkey(), 1_000);
    env.init_cross_backing_genesis_pool();
    let pool_holding = create_canonical_pool_holding(&mut env);

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ConfigureAuthMark {
                asset_index: 0,
                now_slot: 100,
                initial_mark_e6: 100,
            }
            .encode(),
        }],
        &[&oracle],
    )
    .expect("configure the independent authenticated mark");
    let observer_portfolio = create_percolator_portfolio(&mut env, &observer, 0);

    let position_q = 100_000_000_000i128;
    let (first_long, first_long_portfolio, first_short, first_short_portfolio) =
        open_public_pair(&mut env, position_q, 100, 1_000_000);
    let mut slot = 100;
    advance_public_mark(&mut env, &oracle, observer_portfolio, &mut slot, 89, 10);
    liquidate_stale_public_loser(&mut env, first_long_portfolio, slot);
    clear_stale_public_winner_with_backing(
        &mut env,
        &first_short,
        first_short_portfolio,
        slot,
    );

    let (second_long, second_long_portfolio, second_short, second_short_portfolio) =
        open_public_pair(&mut env, position_q, 89, 1_000_000);
    advance_public_mark(&mut env, &oracle, observer_portfolio, &mut slot, 100, 10);
    liquidate_stale_public_loser(&mut env, second_short_portfolio, slot);
    clear_stale_public_winner_with_backing(
        &mut env,
        &second_long,
        second_long_portfolio,
        slot,
    );

    let market = env.svm.get_account(&env.slab).unwrap();
    let sources =
        percolator_accounting::read_asset_backing_source_credits(&market.data, 0).unwrap();
    assert!(sources[0].exact_positive_claim_num > 0);
    assert!(sources[1].exact_positive_claim_num > 0);
    for domain in 0..2u16 {
        let ledger = env
            .svm
            .get_account(&cross_backing_ledger_pda(&env.pool, domain))
            .unwrap();
        assert!(
            !percolator_prog::state::is_initialized(&ledger.data),
            "both buckets contain trader source backing only",
        );
    }

    let mut depositors = Vec::new();
    for _ in 0..4 {
        let (owner, destination) = new_depositor(&mut env, 1);
        env.cross_backing_deposit(&owner, &destination, &pool_holding, 1)
            .expect("bilateral source expiry cannot veto a Genesis base unit");
        depositors.push((owner, destination));
    }
    let pool_data = env.svm.get_account(&env.pool).unwrap().data;
    assert_eq!(
        [
            u64::from_le_bytes(pool_data[273..281].try_into().unwrap()),
            u64::from_le_bytes(pool_data[281..289].try_into().unwrap()),
        ],
        [1, 1],
        "both incompatible backing atoms remain segregated owner principal",
    );
    assert_eq!(env.pool_outstanding(), 4);
    assert_eq!(env.token_amount(&pool_holding), 2);

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ResolveMarket.encode(),
        }],
        &[&oracle],
    )
    .expect("resolve after both public source buckets are materialized");
    close_resolved_portfolios(
        &mut env,
        &[
            (&observer, observer_portfolio),
            (&first_long, first_long_portfolio),
            (&first_short, first_short_portfolio),
            (&second_long, second_long_portfolio),
            (&second_short, second_short_portfolio),
        ],
    );

    for (owner, destination) in depositors {
        env.cross_backing_withdraw(&owner, &destination, &pool_holding, 1)
            .expect("each bilateral-source Genesis unit remains owner-recoverable");
        assert_eq!(env.token_amount(&destination), 1);
    }
    assert_eq!(env.pool_outstanding(), 0);
    assert_eq!(env.token_amount(&pool_holding), 0);
}

// REGRESSION PROBE: a trader-source expiry can keep part of Genesis backing
// staged in the pool ATA. A later public loss must still fix one pool-wide share
// rate: dust-splitting one impaired position cannot consume staged principal,
// reduce an independent owner's claim, or strand that owner's bounded exit.
#[test]
fn staged_backing_survives_an_indexed_haircut_and_dust_split_exit() {
    use percolator_prog::ix::Instruction as PIx;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Outcome {
        splitter_payout: u64,
        victim_payout: u64,
        victim_whole_claim: u128,
        protected_before_exits: u128,
        final_protocol_value: u128,
    }

    let run = |split_exit: bool| {
        let mut env = Env::new_cross_backing();
        let oracle = Keypair::new();
        let observer = Keypair::new();
        for owner in [&oracle, &observer] {
            env.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
        }
        install_public_loss_fixture_with_margin(&mut env, &oracle.pubkey(), 1_000);
        env.init_cross_backing_genesis_pool();
        let pool_holding = create_canonical_pool_holding(&mut env);

        // Seed only the short provider bucket, then publicly materialize a long
        // trader-source bucket whose finite expiry rejects Genesis' u64::MAX top-up.
        let (seed_owner, seed_ata) = new_depositor(&mut env, 1);
        env.cross_backing_deposit(&seed_owner, &seed_ata, &pool_holding, 1)
            .expect("seed the short provider bucket");
        env.send(
            &[Instruction {
                program_id: perc_id(),
                accounts: vec![
                    AccountMeta::new_readonly(oracle.pubkey(), true),
                    AccountMeta::new(env.slab, false),
                ],
                data: PIx::ConfigureAuthMark {
                    asset_index: 0,
                    now_slot: 100,
                    initial_mark_e6: 100,
                }
                .encode(),
            }],
            &[&oracle],
        )
        .expect("configure the independent authenticated mark");
        let observer_portfolio = create_percolator_portfolio(&mut env, &observer, 0);
        let (source_long, source_long_portfolio, source_short, source_short_portfolio) =
            open_public_pair(&mut env, 100_000_000_000, 100, 1_000_000);
        let mut slot = 100;
        advance_public_mark(
            &mut env,
            &oracle,
            observer_portfolio,
            &mut slot,
            89,
            10,
        );
        liquidate_stale_public_loser(&mut env, source_long_portfolio, slot);
        clear_stale_public_winner_with_backing(
            &mut env,
            &source_short,
            source_short_portfolio,
            slot,
        );

        let (splitter, splitter_ata) = new_depositor(&mut env, 14);
        let (victim, victim_ata) = new_depositor(&mut env, 14);
        env.cross_backing_deposit(&splitter, &splitter_ata, &pool_holding, 14)
            .expect("splitter deposits after the public expiry mismatch");
        env.cross_backing_deposit(&victim, &victim_ata, &pool_holding, 14)
            .expect("victim deposits after the public expiry mismatch");

        let staged_before_loss = {
            let pool = env.svm.get_account(&env.pool).unwrap();
            [
                u64::from_le_bytes(pool.data[273..281].try_into().unwrap()),
                u64::from_le_bytes(pool.data[281..289].try_into().unwrap()),
            ]
        };
        assert_eq!(staged_before_loss, [7, 0]);
        assert_eq!(env.token_amount(&pool_holding), 7);
        assert_eq!(env.pool_outstanding(), 29);

        // The second public pair crosses its own margin. This is the real market
        // loss that must lower the indexed owner-claim rate.
        let (loss_long, loss_long_portfolio, loss_short, loss_short_portfolio) =
            open_public_pair(
                &mut env,
                percolator::POS_SCALE as i128,
                89,
                600,
            );
        advance_public_mark(
            &mut env,
            &oracle,
            observer_portfolio,
            &mut slot,
            691,
            300,
        );
        liquidate_stale_public_loser(&mut env, loss_short_portfolio, slot);
        clear_stale_public_winner_with_backing(
            &mut env,
            &loss_long,
            loss_long_portfolio,
            slot,
        );

        env.send(
            &[Instruction {
                program_id: perc_id(),
                accounts: vec![
                    AccountMeta::new_readonly(oracle.pubkey(), true),
                    AccountMeta::new(env.slab, false),
                ],
                data: PIx::ResolveMarket.encode(),
            }],
            &[&oracle],
        )
        .expect("resolve after the indexed public loss");
        close_resolved_portfolios(
            &mut env,
            &[
                (&observer, observer_portfolio),
                (&source_long, source_long_portfolio),
                (&source_short, source_short_portfolio),
                (&loss_long, loss_long_portfolio),
                (&loss_short, loss_short_portfolio),
            ],
        );

        let market = env.svm.get_account(&env.slab).unwrap();
        let backing_balances =
            percolator_accounting::read_asset_backing_balances(&market.data, 0).unwrap();
        let backing_sources =
            percolator_accounting::read_asset_backing_source_credits(&market.data, 0).unwrap();
        let ledger_principals = [0u16, 1u16].map(|domain| {
            let ledger = env
                .svm
                .get_account(&cross_backing_ledger_pda(&env.pool, domain))
                .unwrap();
            percolator_prog::state::read_backing_domain_ledger(&ledger.data)
                .unwrap()
                .total_principal_atoms
        });
        let provider_backing = backing_balances
            .into_iter()
            .zip(backing_sources)
            .zip(ledger_principals)
            .map(|((balance, source), principal)| {
                balance
                    .provider_protected_principal_atoms(principal, source)
                    .unwrap()
            })
            .sum::<u128>();
        let pool_before_exits = env.svm.get_account(&env.pool).unwrap();
        let staged_before_exits = [
            u64::from_le_bytes(pool_before_exits.data[273..281].try_into().unwrap()),
            u64::from_le_bytes(pool_before_exits.data[281..289].try_into().unwrap()),
        ];
        assert_eq!(staged_before_exits, staged_before_loss);
        let protected_before_exits = asset_insurance_remaining(&env, 0)
            + provider_backing
            + staged_before_exits.into_iter().map(u128::from).sum::<u128>();
        assert!(
            protected_before_exits < 29,
            "the second public pair must impair owner protection: insurance={}, provider_backing={provider_backing}, staged={staged_before_exits:?}",
            asset_insurance_remaining(&env, 0),
        );

        let victim_shares = env.position_shares(&victim.pubkey());
        let total_shares = env.pool_total_shares();
        let victim_whole_claim = victim_shares * (protected_before_exits + 1)
            / (total_shares + 1_000_000);

        env.cross_backing_withdraw(&seed_owner, &seed_ata, &pool_holding, 1)
            .expect("the zero-value seed claim fixes the exact loss rate");
        assert_eq!(env.token_amount(&seed_ata), 0);

        if split_exit {
            for _ in 0..14 {
                env.cross_backing_withdraw(&splitter, &splitter_ata, &pool_holding, 1)
                    .expect("each dust split remains a bounded public exit");
            }
        } else {
            env.cross_backing_withdraw(&splitter, &splitter_ata, &pool_holding, 14)
                .expect("the lump-sum control exit remains live");
        }
        env.cross_backing_withdraw(&victim, &victim_ata, &pool_holding, 14)
            .expect("the independent owner retains a bounded exit");

        let final_pool = env.svm.get_account(&env.pool).unwrap();
        assert_eq!(
            [
                u64::from_le_bytes(final_pool.data[273..281].try_into().unwrap()),
                u64::from_le_bytes(final_pool.data[281..289].try_into().unwrap()),
            ],
            [0, 0],
        );
        assert_eq!(env.pool_outstanding(), 0);

        let splitter_payout = env.token_amount(&splitter_ata);
        let victim_payout = env.token_amount(&victim_ata);
        let final_protocol_value = u128::from(env.token_amount(&env.perc_vault))
            + u128::from(env.token_amount(&pool_holding));
        let owner_payouts = u128::from(splitter_payout) + u128::from(victim_payout);
        assert!(
            owner_payouts <= protected_before_exits,
            "owners cannot claim non-provider trader-source value",
        );
        assert!(
            final_protocol_value >= protected_before_exits - owner_payouts,
            "every unpaid protected atom remains in protocol custody",
        );

        Outcome {
            splitter_payout,
            victim_payout,
            victim_whole_claim,
            protected_before_exits,
            final_protocol_value,
        }
    };

    let control = run(false);
    let split = run(true);
    assert_eq!(control.protected_before_exits, 25);
    assert_eq!(control.victim_whole_claim, 12);
    assert_eq!(control.splitter_payout, 12);
    assert_eq!(control.victim_payout, 12);
    assert_eq!(split.splitter_payout, 0);
    assert_eq!(control.protected_before_exits, split.protected_before_exits);
    assert!(split.splitter_payout <= control.splitter_payout);
    assert_eq!(
        split.victim_payout, control.victim_payout,
        "dust exits cannot transfer staged backing or indexed value away from the victim",
    );
    assert!(
        u128::from(split.victim_payout) >= split.victim_whole_claim,
        "the victim receives {}/{} indexed atoms",
        split.victim_payout,
        split.victim_whole_claim,
    );
    assert!(
        split.final_protocol_value >= control.final_protocol_value,
        "splitter forfeitures remain protocol value instead of leaving custody",
    );
}

// UPGRADE LOF PROBE: the original deployed pool was 208 bytes and used only the
// market-binding PDA seeds. A program upgrade must preserve its existing owners'
// exit path, but must not reopen that pre-window genesis pool to new deposits.
// Build a funded fixture through the real Percolator binary, translate only the
// subledger account/PDA to its historical wire format, then exercise both sides.
#[test]
fn legacy_master_insurance_pool_rejects_new_deposits_but_owner_can_withdraw() {
    let mut env = Env::new();
    env.init_insurance_pool();

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let current_pool = env.pool;
    let current_holding = create_holding(&mut env, &current_pool);
    env.insurance_deposit(&alice, &alice_ata, &current_holding, amount)
        .expect("fund real Percolator insurance before translating the fixture");

    let legacy_pool = translate_funded_pool_to_legacy(&mut env, &alice.pubkey(), ASSET_ID);

    let legacy_holding = create_holding(&mut env, &legacy_pool);
    let (bob, bob_ata) = new_depositor(&mut env, 1);
    let bob_holding = create_holding(&mut env, &legacy_pool);
    assert!(
        env.insurance_deposit(&bob, &bob_ata, &bob_holding, 1)
            .is_err(),
        "an upgrade must not reopen an unbounded legacy genesis deposit period"
    );
    assert_eq!(env.token_amount(&bob_ata), 1);
    assert_eq!(env.token_amount(&bob_holding), 0);
    assert!(env
        .svm
        .get_account(&env.position_pda(&bob.pubkey()))
        .is_none());
    assert_eq!(env.token_amount(&env.perc_vault), amount);

    env.insurance_withdraw(&alice, &alice_ata, &legacy_holding, &alice, amount)
        .expect("legacy owner withdrawal remains live through real Percolator");
    assert_eq!(env.token_amount(&alice_ata), amount);
    assert_eq!(env.token_amount(&env.perc_vault), 0);
    assert_eq!(env.pool_outstanding(), 0);
    assert_eq!(env.svm.get_account(&legacy_pool).unwrap().data.len(), 208);
}

// PR #115 COMPATIBILITY PROBE: pre-share insurance pools have live owner
// principal but no `total_shares` field. The positive read-only attestation used
// by TWAP's resolved recovery must recognize that legitimate historical claim.
#[test]
fn pre_share_insurance_pool_attests_live_principal_and_preserves_owner_exit() {
    let mut env = Env::new();
    env.init_insurance_pool();

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let current_pool = env.pool;
    let current_holding = create_holding(&mut env, &current_pool);
    env.insurance_deposit(&alice, &alice_ata, &current_holding, amount)
        .expect("fund real Percolator insurance before translating the fixture");

    let legacy_pool = translate_funded_pool_to_legacy(&mut env, &alice.pubkey(), ASSET_ID);
    let mut pool_account = env.svm.get_account(&legacy_pool).unwrap();
    pool_account.data.truncate(192);
    env.svm.set_account(legacy_pool, pool_account).unwrap();
    let legacy_position = env.position_pda(&alice.pubkey());
    let mut position_account = env.svm.get_account(&legacy_position).unwrap();
    position_account.data.truncate(104);
    env.svm.set_account(legacy_position, position_account).unwrap();

    env.send(
        &[Instruction {
            program_id: sub_id(),
            accounts: vec![AccountMeta::new_readonly(legacy_pool, false)],
            data: vec![11u8], // IX_ASSERT_PRINCIPAL
        }],
        &[],
    )
    .expect("historical outstanding principal is a live owner claim");

    let legacy_holding = create_holding(&mut env, &legacy_pool);
    let historical_snapshot = env.read_position(&alice.pubkey());
    let historical_action_nonce = env.position_action_nonce(&alice.pubkey());
    let mut full_exit_data = vec![13u8]; // IX_INSURANCE_WITHDRAW_FULL
    full_exit_data.extend_from_slice(&historical_snapshot.0.to_le_bytes());
    full_exit_data.extend_from_slice(&historical_snapshot.1.to_le_bytes());
    full_exit_data.extend_from_slice(&historical_action_nonce.to_le_bytes());
    let exit_accounts = vec![
        AccountMeta::new(alice.pubkey(), true),
        AccountMeta::new(legacy_pool, false),
        AccountMeta::new(legacy_position, false),
        AccountMeta::new(alice_ata, false),
        AccountMeta::new(legacy_holding, false),
        AccountMeta::new(env.slab, false),
        AccountMeta::new(env.perc_vault, false),
        AccountMeta::new_readonly(env.vault_authority, false),
        AccountMeta::new_readonly(perc_id(), false),
        AccountMeta::new_readonly(spl_token::ID, false),
    ];
    assert!(
        env.send(
            &[Instruction {
                program_id: sub_id(),
                accounts: exit_accounts.clone(),
                data: vec![13u8],
            }],
            &[&alice],
        )
        .is_err(),
        "predecessor positions reject the replayable amountless wire",
    );
    env.send(
        &[Instruction {
            program_id: sub_id(),
            accounts: exit_accounts,
            data: full_exit_data,
        }],
        &[&alice],
    )
    .expect("the snapshot-bound wire recovers predecessor Percolator principal");
    assert_eq!(env.token_amount(&alice_ata), amount);
    assert_eq!(env.token_amount(&env.perc_vault), 0);
    assert_eq!(env.pool_outstanding(), 0);
}

// LEGACY UPGRADE LOF PROBE: before commit 4e72483, permissionless insurance
// init accepted any u64 asset_id even though TopUpInsurance always credited
// asset 0. The owner must therefore exit from asset 0 as well; using the stale
// metadata ID in WithdrawInsuranceAsset strands every deposit in that pool.
#[test]
fn legacy_nonzero_asset_id_pool_withdraws_from_the_asset_zero_it_funded() {
    let mut env = Env::new();
    env.init_insurance_pool();

    let amount = 1_000_000u64;
    let legacy_asset_id = 7u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let current_pool = env.pool;
    let current_holding = create_holding(&mut env, &current_pool);
    env.insurance_deposit(&alice, &alice_ata, &current_holding, amount)
        .expect("historical deposit funds real Percolator asset-0 insurance");

    let legacy_pool =
        translate_funded_pool_to_legacy(&mut env, &alice.pubkey(), legacy_asset_id);
    assert_eq!(
        u64::from_le_bytes(
            env.svm.get_account(&legacy_pool).unwrap().data[40..48]
                .try_into()
                .unwrap()
        ),
        legacy_asset_id,
        "fixture carries the metadata accepted by the historical public init"
    );
    let legacy_holding = create_holding(&mut env, &legacy_pool);

    env.insurance_withdraw(&alice, &alice_ata, &legacy_holding, &alice, amount)
        .expect("the legacy owner exits from asset 0, matching the deposit domain");
    assert_eq!(env.token_amount(&alice_ata), amount);
    assert_eq!(env.token_amount(&env.perc_vault), 0);
    assert_eq!(env.pool_outstanding(), 0);
}

// REWARD-TENURE PROBE: start_slot is last-write-time, so every deposit stamps it to `now`.
// A one-atom early deposit followed by a large late top-up must not give the late capital the
// early atom's insurance/backing reward tenure.
#[test]
fn a_top_up_deposit_resets_start_slot_late_capital_cannot_borrow_reward_tenure() {
    let mut env = Env::new();
    env.init_insurance_pool();

    let (alice, alice_ata) = new_depositor(&mut env, 2_000_000);
    let pool = env.pool;
    let h1 = create_holding(&mut env, &pool);
    let h2 = create_holding(&mut env, &pool);

    // (1) Dust deposit in the first valid post-grant slot -> banks start_slot = 101.
    env.insurance_deposit(&alice, &alice_ata, &h1, 1).expect("dust deposit");
    let (p1, s1, _) = env.read_position(&alice.pubkey());
    assert_eq!(p1, 1, "1 atom of principal");
    assert_eq!(s1, 101, "dust deposit dates the position to slot 101");

    // (2) Warp ahead (within the market's oracle-freshness window), then TOP UP the real capital. The reset
    // moves start_slot to the top-up slot.
    env.warp_slot(1124);
    env.insurance_deposit(&alice, &alice_ata, &h2, 1_999_999).expect("top-up");
    let (p2, s2, _) = env.read_position(&alice.pubkey());
    assert_eq!(p2, 2_000_000, "principal accumulates across both deposits");
    assert_eq!(s2, 1124, "start_slot RESET to the top-up slot — the 1_999_999 cannot claim slot-101 tenure");
    assert!(s2 > s1, "the tenure clock moved forward on the top-up; the dust-banked early slot is gone");
    // The whole 2M is now dated to slot 1124 for capital-reward accounting.
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

// PUBLIC LOF: a principal-policy deposit made after an impairment must not recapitalize
// earlier positions at par. Alice bore the loss before Bob arrived, so Bob's new principal
// must buy in at the live share price. Otherwise the pool-wide principal ratio lets Alice
// take part of Bob's deposit while both public withdrawals still appear pro rata.
#[test]
fn principal_pool_late_deposit_does_not_socialize_an_earlier_loss() {
    for bob_exits_first in [false, true] {
        let mut env = Env::new();
        env.init_insurance_pool();

        let amount = 1_000_000u64;
        let (alice, alice_ata) = new_depositor(&mut env, amount);
        let pool = env.pool;
        let alice_holding = create_holding(&mut env, &pool);
        env.insurance_deposit(&alice, &alice_ata, &alice_holding, amount)
            .expect("alice deposits before the loss");

        let impaired = amount / 2;
        impair_market(&mut env, impaired as u128);
        env.svm
            .set_account(
                env.perc_vault,
                Account {
                    lamports: 1_000_000,
                    data: token_account_data(&env.mint, &env.vault_authority, impaired),
                    owner: spl_token::ID,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();

        let (bob, bob_ata) = new_depositor(&mut env, amount);
        let bob_holding = create_holding(&mut env, &pool);
        env.insurance_deposit(&bob, &bob_ata, &bob_holding, amount)
            .expect("bob deposits after the loss while the genesis window remains open");
        assert_eq!(
            env.token_amount(&env.perc_vault),
            impaired + amount,
            "Bob contributes one full unit of fresh principal"
        );

        if bob_exits_first {
            env.insurance_withdraw(&bob, &bob_ata, &bob_holding, &bob, amount)
                .expect("Bob exits first without inheriting Alice's historical loss");
            env.insurance_withdraw(&alice, &alice_ata, &alice_holding, &alice, amount)
                .expect("Alice realizes only the loss from her own tenure");
        } else {
            env.insurance_withdraw(&alice, &alice_ata, &alice_holding, &alice, amount)
                .expect("Alice realizes only the loss from her own tenure");
            env.insurance_withdraw(&bob, &bob_ata, &bob_holding, &bob, amount)
                .expect("Bob exits second without inheriting Alice's historical loss");
        }
        let alice_received = env.token_amount(&alice_ata);
        let bob_received = env.token_amount(&bob_ata);
        assert!(
            (impaired..=impaired + 1).contains(&alice_received),
            "Alice cannot recover her historical loss from Bob's later deposit"
        );
        assert!(
            (amount - 1..=amount).contains(&bob_received),
            "Bob's post-loss capital is protected in either exit order"
        );
        let reserve = env.token_amount(&env.perc_vault);
        assert_eq!(reserve, 1, "the share-floor remainder stays protocol insurance");
        assert_eq!(
            alice_received + bob_received + reserve,
            impaired + amount,
            "user payouts plus protocol reserve conserve the funded insurance"
        );
        assert_eq!(env.pool_outstanding(), 0);
    }
}

// PUBLIC LOF: Percolator's asset-wide insurance withdrawal consumes the long domain first.
// Without a Subledger-side compensating redeposit, a temporary depositor can round-trip
// principal and move another depositor's live protection into the short domain at negligible
// cost. The remaining capital must keep the same long/short risk allocation.
#[test]
fn depositor_round_trip_cannot_reassign_another_owners_insurance_domain() {
    use percolator_prog::ix::Instruction as PIx;

    let mut env = Env::new();
    let oracle = Keypair::new();
    env.svm.airdrop(&oracle.pubkey(), 1_000_000_000).unwrap();
    install_public_loss_fixture(&mut env, &oracle.pubkey());
    env.init_insurance_pool();

    let domains = |env: &Env| {
        let (_, group) = percolator_prog::state::read_market(
            &env.svm.get_account(&env.slab).unwrap().data,
        )
        .unwrap();
        [
            group.insurance_domain_budget[0] - group.insurance_domain_spent[0],
            group.insurance_domain_budget[1] - group.insurance_domain_spent[1],
        ]
    };

    let victim_principal = 4u64;
    let (victim, victim_ata) = new_depositor(&mut env, victim_principal);
    let pool = env.pool;
    let victim_holding = create_holding(&mut env, &pool);
    env.insurance_deposit(
        &victim,
        &victim_ata,
        &victim_holding,
        victim_principal,
    )
    .expect("victim funds balanced live insurance");
    assert_eq!(domains(&env), [2, 2]);

    let attacker_principal = victim_principal;
    let (attacker, attacker_ata) = new_depositor(&mut env, attacker_principal);
    let attacker_holding = create_holding(&mut env, &pool);
    env.insurance_deposit(
        &attacker,
        &attacker_ata,
        &attacker_holding,
        attacker_principal,
    )
    .expect("attacker temporarily doubles the fund");
    env.insurance_withdraw(
        &attacker,
        &attacker_ata,
        &attacker_holding,
        &attacker,
        attacker_principal,
    )
    .expect("attacker exits while the market is live and healthy");

    assert!(
        env.token_amount(&attacker_ata) >= attacker_principal - 1,
        "the domain-moving round trip costs at most the existing share-floor atom",
    );
    assert_eq!(env.read_position(&victim.pubkey()).0, victim_principal);
    assert_eq!(env.pool_outstanding(), victim_principal);
    assert_eq!(
        domains(&env),
        [2, 2],
        "one depositor's exit cannot reassign another owner's live insurance protection",
    );

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ConfigureAuthMark {
                asset_index: 0,
                now_slot: 100,
                initial_mark_e6: 100,
            }
            .encode(),
        }],
        &[&oracle],
    )
    .expect("configure authenticated mark");

    let long = Keypair::new();
    let short = Keypair::new();
    for owner in [&long, &short] {
        env.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
    }
    let long_portfolio = create_percolator_portfolio(&mut env, &long, 1_000_000);
    let short_portfolio = create_percolator_portfolio(&mut env, &short, 200);
    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new(long.pubkey(), true),
                AccountMeta::new(short.pubkey(), true),
                AccountMeta::new(env.slab, false),
                AccountMeta::new(long_portfolio, false),
                AccountMeta::new(short_portfolio, false),
            ],
            data: PIx::TradeNoCpi {
                asset_index: 0,
                size_q: (2 * percolator::POS_SCALE) as i128,
                exec_price: 100,
                fee_bps: 0,
            }
            .encode(),
        }],
        &[&long, &short],
    )
    .expect("open a public long/short pair after the attempted domain move");
    env.warp_slot(101);
    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::PushAuthMark {
                asset_index: 0,
                now_slot: 101,
                mark_e6: 1_000,
            }
            .encode(),
        }],
        &[&oracle],
    )
    .expect("move the mark against the undercapitalized short");
    public_percolator_crank(&mut env, long_portfolio, 101, true)
        .expect("crank the winning side");
    env.warp_slot(102);
    public_percolator_crank(&mut env, long_portfolio, 102, true)
        .expect("refresh the winning side");
    env.warp_slot(103);
    for _ in 0..2 {
        public_percolator_crank(&mut env, short_portfolio, 103, true)
            .expect("permissionless liquidation consumes the preserved domain");
    }
    assert_eq!(
        asset_insurance_remaining(&env, 0),
        2,
        "the victim's preserved two-atom domain absorbs the public loss",
    );
}

// PUBLIC LOF: spent domain budget is historical, not live protection. After a
// one-sided loss, fresh insurance must rebuild the configured 50/50 live allocation
// instead of balancing gross budgets around atoms that the market already consumed.
#[test]
fn post_loss_recapitalization_rebalances_live_insurance_domains() {
    use percolator_prog::ix::Instruction as PIx;

    let mut env = Env::new();
    let oracle = Keypair::new();
    let observer = Keypair::new();
    for owner in [&oracle, &observer] {
        env.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
    }
    install_public_loss_fixture_with_margin(&mut env, &oracle.pubkey(), 1_000);
    env.init_insurance_pool();

    let domains = |env: &Env| {
        let (_, group) = percolator_prog::state::read_market(
            &env.svm.get_account(&env.slab).unwrap().data,
        )
        .unwrap();
        [
            group.insurance_domain_budget[0] - group.insurance_domain_spent[0],
            group.insurance_domain_budget[1] - group.insurance_domain_spent[1],
        ]
    };

    let domain_tranche = 999_999u64;
    let principal = domain_tranche.checked_mul(2).unwrap();
    let (old_owner, old_owner_ata) = new_depositor(&mut env, principal);
    let pool = env.pool;
    let old_holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&old_owner, &old_owner_ata, &old_holding, principal)
        .expect("fund the initial balanced insurance generation");
    assert_eq!(domains(&env), [domain_tranche as u128; 2]);

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ConfigureAuthMark {
                asset_index: 0,
                now_slot: 100,
                initial_mark_e6: 100,
            }
            .encode(),
        }],
        &[&oracle],
    )
    .expect("configure authenticated mark");

    let observer_portfolio = create_percolator_portfolio(&mut env, &observer, 0);
    let low_entry = 100u64;
    let low_capital = 10_000_001u64;
    let position_q = 1_000_000_000_000i128;
    let pnl_atoms_per_price = position_q.unsigned_abs() / percolator::POS_SCALE;
    let high_target = low_entry
        .checked_add(
            u64::try_from(
                (domain_tranche as u128 + low_capital as u128) / pnl_atoms_per_price,
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(
        (domain_tranche as u128 + low_capital as u128) % pnl_atoms_per_price,
        0,
    );
    let (long, long_portfolio, _, short_portfolio) =
        open_public_pair(&mut env, position_q, low_entry, low_capital);
    let mut slot = 100;
    advance_public_mark(
        &mut env,
        &oracle,
        observer_portfolio,
        &mut slot,
        high_target,
        300,
    );
    liquidate_stale_public_loser(&mut env, short_portfolio, slot);
    let impaired_domains = domains(&env);
    assert_eq!(
        impaired_domains[0] + impaired_domains[1],
        domain_tranche as u128,
    );
    assert!(impaired_domains.contains(&0));
    clear_stale_public_winner(&mut env, &long, long_portfolio, slot);

    let (fresh_owner, fresh_owner_ata) = new_depositor(&mut env, principal);
    let fresh_holding = create_holding(&mut env, &pool);
    env.insurance_deposit(
        &fresh_owner,
        &fresh_owner_ata,
        &fresh_holding,
        principal,
    )
    .expect("fresh capital recapitalizes the live market");
    let recapitalized_domains = domains(&env);

    let second_entry = 115u64;
    advance_public_mark(
        &mut env,
        &oracle,
        observer_portfolio,
        &mut slot,
        second_entry,
        20,
    );
    let second_loss = principal as u128;
    let second_capital = pnl_atoms_per_price
        .checked_mul((second_entry - low_entry) as u128)
        .unwrap()
        .checked_sub(second_loss)
        .and_then(|value| u64::try_from(value).ok())
        .unwrap();
    let (_, second_long_portfolio, second_short, second_short_portfolio) =
        open_public_pair(&mut env, position_q, second_entry, second_capital);
    advance_public_mark(
        &mut env,
        &oracle,
        observer_portfolio,
        &mut slot,
        low_entry,
        300,
    );
    liquidate_stale_public_loser(&mut env, second_long_portfolio, slot);
    let insurance_after_second_loss = asset_insurance_remaining(&env, 0);
    clear_stale_public_winner(
        &mut env,
        &second_short,
        second_short_portfolio,
        slot,
    );

    assert!(
        insurance_after_second_loss
            >= (recapitalized_domains[0] + recapitalized_domains[1]) / 2,
        "one public side loss consumed more than its configured half: before={recapitalized_domains:?} after={insurance_after_second_loss}",
    );
    assert!(
        recapitalized_domains[0].abs_diff(recapitalized_domains[1]) <= 1,
        "fresh insurance must not inherit a historical domain loss: {recapitalized_domains:?}",
    );
}

#[test]
fn one_atom_deposits_balance_globally_and_a_round_trip_cannot_move_the_remainder() {
    let mut env = Env::new();
    env.init_insurance_pool();

    let domains = |env: &Env| {
        let (_, group) = percolator_prog::state::read_market(
            &env.svm.get_account(&env.slab).unwrap().data,
        )
        .unwrap();
        [
            group.insurance_domain_budget[0] - group.insurance_domain_spent[0],
            group.insurance_domain_budget[1] - group.insurance_domain_spent[1],
        ]
    };
    let pool = env.pool;

    let (victim, victim_ata) = new_depositor(&mut env, 1);
    let victim_holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&victim, &victim_ata, &victim_holding, 1)
        .expect("first one-atom deposit");
    assert_eq!(domains(&env), [0, 1]);

    let (attacker, attacker_ata) = new_depositor(&mut env, 1);
    let attacker_holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&attacker, &attacker_ata, &attacker_holding, 1)
        .expect("second one-atom deposit");
    assert_eq!(
        domains(&env),
        [1, 1],
        "rounding is global to the pool, not repeated in one domain per caller",
    );

    env.insurance_withdraw(&attacker, &attacker_ata, &attacker_holding, &attacker, 1)
        .expect("second depositor exits");
    assert_eq!(domains(&env), [0, 1]);
    assert_eq!(env.token_amount(&attacker_ata), 1);
    assert_eq!(env.read_position(&victim.pubkey()).0, 1);
}

// PUBLIC LOF: an attacker can split a fully impaired generation across dust positions, then
// alternate each zero-value exit with a one-atom recapitalization. If zero exits keep changing
// the domain-routing parity, every new atom lands on the same side instead of the 50/50 split.
#[test]
fn zero_value_generation_exits_cannot_steer_recapitalization_into_one_domain() {
    use percolator_prog::ix::Instruction as PIx;

    let mut env = Env::new();
    let oracle = Keypair::new();
    let observer = Keypair::new();
    for owner in [&oracle, &observer] {
        env.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
    }
    install_public_loss_fixture_with_margin(&mut env, &oracle.pubkey(), 1_000);
    env.init_insurance_pool();

    let domains = |env: &Env| {
        let (_, group) = percolator_prog::state::read_market(
            &env.svm.get_account(&env.slab).unwrap().data,
        )
        .unwrap();
        [
            group.insurance_domain_budget[0] - group.insurance_domain_spent[0],
            group.insurance_domain_budget[1] - group.insurance_domain_spent[1],
        ]
    };

    let high_entry = 1_000_000_110u64;
    let high_capital = 100_000u64
        .checked_mul(high_entry)
        .unwrap();
    let second_domain_loss = 900_000u64
        .checked_mul(high_entry)
        .unwrap()
        .checked_sub(100_000_000)
        .unwrap();
    let first_domain_loss = second_domain_loss.checked_sub(1).unwrap();
    let old_principal = first_domain_loss
        .checked_add(second_domain_loss)
        .unwrap();
    const STALE_DUST_POSITIONS: u64 = 4;
    let whale_principal = old_principal
        .checked_sub(STALE_DUST_POSITIONS)
        .unwrap();
    let (old_whale, old_whale_ata) = new_depositor(&mut env, whale_principal);
    let pool = env.pool;
    let old_whale_holding = create_holding(&mut env, &pool);
    env.insurance_deposit(
        &old_whale,
        &old_whale_ata,
        &old_whale_holding,
        whale_principal,
    )
    .expect("fund the main loss-bearing position");
    let mut stale_dust = Vec::new();
    for _ in 0..STALE_DUST_POSITIONS {
        let (owner, ata) = new_depositor(&mut env, 1);
        let holding = create_holding(&mut env, &pool);
        env.insurance_deposit(&owner, &ata, &holding, 1)
            .expect("split the impaired generation across dust positions");
        stale_dust.push((owner, ata, holding));
    }
    assert_eq!(
        domains(&env),
        [first_domain_loss as u128, second_domain_loss as u128],
    );

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ConfigureAuthMark {
                asset_index: 0,
                now_slot: 100,
                initial_mark_e6: 100,
            }
            .encode(),
        }],
        &[&oracle],
    )
    .expect("configure the authenticated mark");
    let observer_portfolio = create_percolator_portfolio(&mut env, &observer, 0);

    let mut slot = 100;
    run_complete_public_insurance_loss(
        &mut env,
        &oracle,
        observer_portfolio,
        &mut slot,
        100,
        10_000_001,
        high_entry,
        high_capital,
        first_domain_loss,
        0,
    );
    assert_eq!(domains(&env), [0, 0]);

    let mut fresh_positions = Vec::new();
    for (stale_owner, stale_ata, stale_holding) in stale_dust {
        let (fresh_owner, fresh_ata) = new_depositor(&mut env, 1);
        let fresh_holding = create_holding(&mut env, &pool);
        env.insurance_deposit(&fresh_owner, &fresh_ata, &fresh_holding, 1)
            .expect("recapitalize after the complete public loss");
        env.insurance_withdraw(&stale_owner, &stale_ata, &stale_holding, &stale_owner, 1)
            .expect("retire one fully impaired dust position");
        assert_eq!(env.token_amount(&stale_ata), 0);
        fresh_positions.push((fresh_owner, fresh_ata, fresh_holding));
    }
    env.insurance_withdraw(
        &old_whale,
        &old_whale_ata,
        &old_whale_holding,
        &old_whale,
        whale_principal,
    )
    .expect("retire the final fully impaired position");
    assert_eq!(env.token_amount(&old_whale_ata), 0);
    assert_eq!(env.pool_outstanding(), STALE_DUST_POSITIONS);
    assert_eq!(
        domains(&env),
        [2, 2],
        "fresh one-atom deposits must alternate domains despite interleaved stale exits",
    );

    for (owner, ata, _) in fresh_positions {
        assert_eq!(env.read_position(&owner.pubkey()).0, 1);
        assert_eq!(env.token_amount(&ata), 0);
    }
}

// CROSS-ASSET EXIT DOS: the market header's `insurance` is global, while tag-57 debits
// only asset 0. If asset 0 is impaired and an external asset keeps the global total above
// outstanding principal, a global quote asks Percolator for more than asset 0 owns and the
// owner's entire exit reverts. The subledger must price the haircut from asset 0's domains.
#[test]
fn external_asset_insurance_cannot_hide_asset0_impairment_or_block_owner_exit() {
    let mut env = Env::new();
    env.init_insurance_pool();

    let principal = 2_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, principal);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, principal)
        .expect("asset-0 principal deposit");

    let external_amount = 2_000_000u64;
    let (provider, provider_source) = new_depositor(&mut env, external_amount);
    activate_external_asset(&mut env, &provider.pubkey());
    let topup = Instruction {
        program_id: perc_id(),
        accounts: vec![
            AccountMeta::new_readonly(provider.pubkey(), true),
            AccountMeta::new(env.slab, false),
            AccountMeta::new(provider_source, false),
            AccountMeta::new(env.perc_vault, false),
            AccountMeta::new_readonly(spl_token::ID, false),
        ],
        data: percolator_prog::ix::Instruction::TopUpInsuranceDomain {
            domain: 2,
            amount: external_amount as u128,
        }
        .encode(),
    };
    env.send(&[topup], &[&provider])
        .expect("external provider funds asset 1");
    assert_eq!(asset_insurance_remaining(&env, 0), principal as u128);
    assert_eq!(asset_insurance_remaining(&env, 1), external_amount as u128);

    impair_asset0_with_external_insurance(&mut env);
    assert_eq!(asset_insurance_remaining(&env, 0), 1_000_000);
    assert_eq!(asset_insurance_remaining(&env, 1), external_amount as u128);
    let (_, group) = percolator_prog::state::read_market(
        &env.svm.get_account(&env.slab).unwrap().data,
    )
    .unwrap();
    assert_eq!(group.insurance, 3_000_000, "global insurance still exceeds Alice's principal");

    env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, principal)
        .expect("asset-local pro-rata exit must not be blocked by foreign insurance");
    assert_eq!(
        env.token_amount(&alice_ata),
        1_000_000,
        "Alice receives all remaining asset-0 insurance, a 50% pro-rata haircut"
    );
    assert_eq!(env.pool_outstanding(), 0, "the impaired position retires");
    assert_eq!(
        asset_insurance_remaining(&env, 1),
        external_amount as u128,
        "the external provider's segregated asset-1 insurance is untouched"
    );
}

// POLICY DISTINCTION (surplus exclusion under POLICY_PRINCIPAL, sweep tick B): the impaired test above pins the
// DOWNSIDE (pro-rata haircut). This pins the UPSIDE under POLICY_PRINCIPAL specifically: when the insurance grows
// ABOVE outstanding (the market earns a surplus — e.g. the 3 bps fees that accrue to asset-0 insurance, verified
// in sim/), a POLICY_PRINCIPAL depositor recovers ONLY their principal — never a slice of the surplus, which
// stays in insurance to back the market and fund the buy/burn. (POLICY_WITH_SURPLUS is the configurable
// alternative that DOES distribute the surplus pro-rata — see policy_with_surplus_distributes_surplus_pro_rata.)
// percolator's WithdrawInsuranceLimited caps each POLICY_PRINCIPAL exit to deposited principal; dropping that
// here would let a depositor pull insurance*amount/outstanding (> principal) = an LOF draining the buy/burn fuel.
#[test]
fn surplus_above_outstanding_is_excluded_a_depositor_recovers_principal_only_not_yield() {
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
    assert_eq!(env.pool_outstanding(), 2 * amount, "outstanding = both deposits (2M)");

    // The market EARNS a 1M surplus: insurance grows 2M -> 3M (1M ABOVE the 2M outstanding). Mirror it
    // consistently on the slab (insurance/vault/budgets) and on the SPL vault token account.
    impair_market(&mut env, 3_000_000);
    env.svm
        .set_account(
            env.perc_vault,
            Account {
                lamports: 1_000_000,
                data: token_account_data(&env.mint, &env.vault_authority, 3_000_000),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    // Alice withdraws her principal. With insurance(3M) >= outstanding(2M) the deposits_only cap pays her
    // EXACTLY her 1M principal — NOT 1M * 3M/2M = 1.5M. The surplus is excluded.
    env.insurance_withdraw(&alice, &alice_ata, &a_hold, &alice, amount).expect("alice exits");
    assert_eq!(env.token_amount(&alice_ata), 1_000_000, "depositor recovers principal ONLY — never a slice of the market surplus");
    assert_eq!(env.token_amount(&env.perc_vault.clone()), 2_000_000, "the 1M surplus + bob's 1M principal stay in insurance (buy/burn fuel untouched)");
    assert_eq!(env.pool_outstanding(), amount, "alice's full principal left the outstanding accounting");

    // Bob likewise recovers his principal only — the surplus is still not distributable to him.
    env.insurance_withdraw(&bob, &bob_ata, &b_hold, &bob, amount).expect("bob exits");
    assert_eq!(env.token_amount(&bob_ata), 1_000_000, "second depositor also recovers principal only");
    assert_eq!(env.token_amount(&env.perc_vault.clone()), 1_000_000, "the 1M surplus REMAINS in insurance after all principals are returned");
    assert_eq!(env.pool_outstanding(), 0, "all principal retired; the surviving 1M is pure surplus, not a depositor's");
}

// ISSUE #40 REGRESSION (principal-policy partial exit must reduce RD live-cap shares): insurance deposits
// mint Position.shares for residual-distributor share-value cohorts even when the pool's payout policy is
// POLICY_PRINCIPAL. A principal-policy partial withdraw keeps the principal-only payout rule, but it must still
// burn the same fraction of shares as the principal fraction being retired. Otherwise a depositor can reduce
// capital at risk before claiming RD COIN while preserving a stale-high live share cap.
#[test]
fn principal_policy_partial_insurance_withdraw_burns_proportional_shares_issue_40() {
    let mut env = Env::new();
    env.init_insurance_pool(); // POLICY_PRINCIPAL

    let amount = 100_000u64;
    let withdraw_amount = 99_999u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("deposit");

    let shares_before = env.position_shares(&alice.pubkey());
    let pool_shares_before = env.pool_total_shares();
    assert!(shares_before > 0, "insurance deposits record shares for RD share-value accounting");
    assert_eq!(pool_shares_before, shares_before, "single depositor owns the whole share supply");

    let expected_burn = shares_before * withdraw_amount as u128 / amount as u128;
    let expected_after = shares_before - expected_burn;
    env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, withdraw_amount)
        .expect("principal-policy partial withdraw");

    let (principal_after, _start_slot, withdrawn) = env.read_position(&alice.pubkey());
    assert_eq!(principal_after, amount - withdraw_amount, "principal falls by the withdrawn amount");
    assert!(!withdrawn, "one atom of principal remains, so the position is still live");
    assert_eq!(
        env.position_shares(&alice.pubkey()),
        expected_after,
        "principal-policy partial withdraw burns the retired principal fraction of shares"
    );
    assert_eq!(
        env.pool_total_shares(),
        expected_after,
        "pool total shares falls with the position shares"
    );
    assert_eq!(env.token_amount(&alice_ata), withdraw_amount, "principal-policy payout remains principal-only");
}

// POLICY_WITH_SURPLUS pays out PRO-RATA SURPLUS (sweep tick B — the configurable policy distinction, INTENDED):
// the two insurance-withdraw policies are a DEPLOYMENT CHOICE with an explicit tradeoff:
//   * POLICY_PRINCIPAL  — a depositor recovers PRINCIPAL ONLY (haircut under loss, NO upside). Surplus stays in
//     insurance as the MetaDAO's buy/burn fuel. Pinned by surplus_above_outstanding_is_excluded above.
//   * POLICY_WITH_SURPLUS — a depositor redeems SHARES at the live balance, so a surplus IS distributed
//     pro-rata (principal + a tenure-fair slice of the yield). This is NOT a loss-of-funds: it is the chosen
//     policy. Its tradeoff is governance pressure — because the surplus is winnable, an attacker has an
//     incentive to game the COIN distribution to capture it (an accepted, deliberate design cost).
// owed = shares*insurance/total_shares (lib.rs:1211-1221); the deposits_only market flag is only a POOL-level
// cap (total out <= total in), so under POLICY_WITH_SURPLUS the surplus is correctly withdrawable. This pins
// the surplus-distributing half of the configurable behavior, which was previously untested (the prior test
// only covered POLICY_PRINCIPAL). It also confirms the exit does NOT revert when a surplus exists (no DoS).
#[test]
fn policy_with_surplus_distributes_surplus_pro_rata_the_configurable_alternative_to_principal_only() {
    let mut env = Env::new_for_policy(POLICY_WITH_SURPLUS);
    env.init_insurance_pool_policy(POLICY_WITH_SURPLUS); // shares; surplus is distributed pro-rata

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let (bob, bob_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let a_hold = create_holding(&mut env, &pool);
    let b_hold = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &a_hold, amount).expect("alice deposit");
    env.insurance_deposit(&bob, &bob_ata, &b_hold, amount).expect("bob deposit");
    assert_eq!(env.pool_outstanding(), 2 * amount, "outstanding = both deposits (2M)");

    // The market earns a 1M surplus: insurance 2M -> 3M (1M ABOVE the 2M outstanding), mirrored on the SPL vault.
    impair_market(&mut env, 3_000_000);
    env.svm.set_account(env.perc_vault, Account {
        lamports: 1_000_000, data: token_account_data(&env.mint, &env.vault_authority, 3_000_000),
        owner: spl_token::ID, executable: false, rent_epoch: 0,
    }).unwrap();

    // Alice exits her full 1M principal. Under POLICY_WITH_SURPLUS she redeems her shares at the live 3M
    // balance and collects ~1.5M = her 1M principal + her pro-rata HALF of the 1M surplus (1 atom to the
    // virtual-share inflation offset). The exit succeeds (no DoS) and the surplus IS distributed — intended.
    env.insurance_withdraw(&alice, &alice_ata, &a_hold, &alice, amount)
        .expect("POLICY_WITH_SURPLUS exit succeeds when a surplus exists");
    assert_eq!(env.token_amount(&alice_ata), 1_499_999,
        "POLICY_WITH_SURPLUS pays principal + a pro-rata slice of the surplus (~1.5M), unlike POLICY_PRINCIPAL");
    assert_eq!(env.pool_outstanding(), amount, "alice's full principal left the outstanding accounting");

    // Bob exits too and collects his own floored surplus slice. Neither exit may
    // absorb the other's floor remainder; both atoms stay protocol insurance.
    env.insurance_withdraw(&bob, &bob_ata, &b_hold, &bob, amount).expect("bob exits");
    assert_eq!(env.token_amount(&bob_ata), 1_499_999, "bob collects his own pro-rata surplus slice without prior-exit dust");
    assert_eq!(env.pool_outstanding(), 0, "all principal retired");
    assert_eq!(env.token_amount(&env.perc_vault.clone()), 2, "both whole-atom floor remainders remain protocol insurance");
}

// FREE-FARM PROBE (late depositor cannot capture pre-existing surplus, sweep tick B): POLICY_WITH_SURPLUS makes
// the surplus WINNABLE (last tick), so the obvious free-farm is to NOT do the work: wait until a surplus has
// accrued, then deposit and immediately exit to skim a pro-rata slice of surplus you never earned. The defense
// is that insurance_deposit prices the minted shares against the LIVE insurance balance BEFORE the top-up
// (lib.rs:985-986, mint_shares(amount, total_shares, insurance_before)) — so a late depositor buys in at the
// inflated share price and receives only enough shares to redeem their OWN principal, never the early backers'
// surplus. (The HB guard at lib.rs:993 additionally rejects a deposit that would round to ZERO shares.) This
// pins the early-vs-late fairness that makes the winnable-surplus tradeoff safe: surplus accrues to whoever bore
// the risk while it built, not to a last-second joiner. Previously untested on the insurance path.
#[test]
fn policy_with_surplus_late_depositor_cannot_capture_pre_existing_surplus() {
    let mut env = Env::new_for_policy(POLICY_WITH_SURPLUS);
    env.init_insurance_pool_policy(POLICY_WITH_SURPLUS);

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let (bob, bob_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let a_hold = create_holding(&mut env, &pool);
    let b_hold = create_holding(&mut env, &pool);

    // Alice deposits EARLY and bears the risk while the surplus builds.
    env.insurance_deposit(&alice, &alice_ata, &a_hold, amount).expect("alice (early) deposit");
    assert_eq!(env.pool_outstanding(), amount, "outstanding = alice's 1M");

    // The market earns a 1M surplus BEFORE bob arrives: insurance 1M -> 2M, mirrored on the SPL vault.
    impair_market(&mut env, 2_000_000);
    env.svm.set_account(env.perc_vault, Account {
        lamports: 1_000_000, data: token_account_data(&env.mint, &env.vault_authority, 2_000_000),
        owner: spl_token::ID, executable: false, rent_epoch: 0,
    }).unwrap();

    // Bob deposits LATE (1M into a pool whose share price has already doubled). insurance_deposit prices his
    // shares against the live 2M, so he gets ~half the shares per atom that alice did — he can only redeem his
    // own principal back, NOT a slice of alice's surplus.
    env.insurance_deposit(&bob, &bob_ata, &b_hold, amount).expect("bob (late) deposit");
    assert_eq!(env.pool_outstanding(), 2 * amount, "outstanding = both principals (2M)");

    // Bob immediately exits his full 1M principal. He recovers his principal ONLY (1 atom SHORT, to the
    // virtual-share inflation offset — rounding favors the protocol, never the late skimmer): no surplus.
    env.insurance_withdraw(&bob, &bob_ata, &b_hold, &bob, amount).expect("bob exits");
    assert_eq!(env.token_amount(&bob_ata), 999_999, "the late depositor recovers PRINCIPAL ONLY (1 atom to the offset) — he captured none of the pre-existing surplus");

    // Alice (who bore the risk while the surplus built) exits and collects her
    // own floored claim without absorbing Bob's prior remainder.
    env.insurance_withdraw(&alice, &alice_ata, &a_hold, &alice, amount).expect("alice exits");
    assert_eq!(env.token_amount(&alice_ata), 1_999_999, "the early depositor collects principal + the floored pre-existing surplus");
    assert_eq!(env.pool_outstanding(), 0, "all principal retired; no surplus leaked to the late joiner");
    assert_eq!(env.token_amount(&env.perc_vault.clone()), 2, "both users' floor remainders stay protocol insurance");
}

// POLICY_WITH_SURPLUS under a DOWNSIDE impairment — share-redemption haircut is ORDER-INDEPENDENT + CONSERVES
// (sweep tick B). impaired_insurance_exit_is_pro_rata (above) pins the order-independent haircut for
// POLICY_PRINCIPAL (the `owed = insurance*amount/outstanding` payout path). POLICY_WITH_SURPLUS takes the OTHER
// path — redeem_shares(shares, balance, total_shares) with the VIRTUAL_SHARES + (balance+1) ERC4626 offsets —
// where the FIRST exiter's redemption shifts both `balance` and `total_shares` for the SECOND. So
// order-independence is NOT obvious here: a rounding bias could over-pay the early exiter and STRAND the late
// one (an order-dependent LOF), or the two redemptions could collectively OVER-DRAW the impaired insurance.
// This pins that the share path also gives both depositors the SAME ~50% haircut and never pays out more than
// the impaired balance. Real percolator slab + subledger .so.
#[test]
fn policy_with_surplus_impaired_exit_is_order_independent_and_conserves() {
    let mut env = Env::new_for_policy(POLICY_WITH_SURPLUS);
    env.init_insurance_pool_policy(POLICY_WITH_SURPLUS); // share redemption

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let (bob, bob_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let a_hold = create_holding(&mut env, &pool);
    let b_hold = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &a_hold, amount).expect("alice deposit");
    env.insurance_deposit(&bob, &bob_ata, &b_hold, amount).expect("bob deposit");
    assert_eq!(env.pool_outstanding(), 2 * amount, "outstanding = both deposits (2M)");

    // 50% venue loss: insurance 2M -> 1M (impaired: balance 1M < outstanding 2M), mirrored on the SPL vault.
    impair_market(&mut env, amount as u128);
    env.svm.set_account(env.perc_vault, Account {
        lamports: 1_000_000, data: token_account_data(&env.mint, &env.vault_authority, amount),
        owner: spl_token::ID, executable: false, rent_epoch: 0,
    }).unwrap();

    // Alice (early) redeems her shares at the live impaired balance: ~50% haircut.
    env.insurance_withdraw(&alice, &alice_ata, &a_hold, &alice, amount).expect("alice exits");
    let alice_got = env.token_amount(&alice_ata);
    // Bob (late) redeems at the NOW-lower balance/total_shares — must get the SAME haircut, never stranded.
    env.insurance_withdraw(&bob, &bob_ata, &b_hold, &bob, amount).expect("bob exits with the same haircut");
    let bob_got = env.token_amount(&bob_ata);

    // ORDER-INDEPENDENCE: both take the ~50% haircut (within 1 atom of rounding); the first exit did NOT
    // drain the share pool and strand bob, and did NOT over-pay alice at bob's expense.
    assert!((499_999..=500_000).contains(&alice_got), "early exiter takes the ~50% share haircut, got {alice_got}");
    assert!((499_999..=500_000).contains(&bob_got), "late exiter takes the SAME ~50% share haircut (not stranded), got {bob_got}");
    assert!(alice_got.abs_diff(bob_got) <= 1, "the two exits differ by at most 1 atom of rounding — order-independent");
    // CONSERVATION: the two share redemptions together never pay out more than the impaired insurance (1M);
    // rounding favors the protocol (a tiny virtual-offset dust may remain), never over-draws.
    assert!(alice_got as u128 + bob_got as u128 <= amount as u128, "total paid <= impaired insurance — no over-draw");
    assert_eq!(env.pool_outstanding(), 0, "all principal retired");
}

// A complete loss makes every prior share valueless. Repeated recapitalization therefore advances
// the share generation instead of carrying an exponentially growing denominator forward. The same
// owner's principal history remains intact for governance/reward accounting, but only the final
// surviving tranche can claim live insurance.
#[test]
fn current_share_generation_can_exit_after_repeated_total_impairment() {
    let mut env = Env::new_for_policy(POLICY_WITH_SURPLUS);
    env.init_insurance_pool_policy(POLICY_WITH_SURPLUS);

    let tranche = 1_000_000_000u64;
    let total_deposit = tranche * 3;
    let (owner, owner_ata) = new_depositor(&mut env, total_deposit);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);

    for cycle in 0..3 {
        env.insurance_deposit(&owner, &owner_ata, &holding, tranche)
            .expect("recapitalize insurance");
        if cycle < 2 {
            // A real venue loss can consume the insurance while the position's principal/share
            // attribution remains. Mirror that loss in the slab and canonical SPL vault.
            impair_market(&mut env, 0);
            env.svm
                .set_account(
                    env.perc_vault,
                    Account {
                        lamports: 1_000_000,
                        data: token_account_data(&env.mint, &env.vault_authority, 0),
                        owner: spl_token::ID,
                        executable: false,
                        rent_epoch: 0,
                    },
                )
                .unwrap();
        }
    }

    assert_eq!(env.pool_share_generation(), 2);
    assert_eq!(env.position_share_generation(&owner.pubkey()), 2);
    assert_eq!(env.position_shares(&owner.pubkey()), tranche as u128 * 1_000_000);
    assert_eq!(env.read_position(&owner.pubkey()).0, total_deposit);

    env.insurance_withdraw(&owner, &owner_ata, &holding, &owner, total_deposit)
        .expect("the current generation remains withdrawable");
    assert_eq!(
        env.token_amount(&owner_ata),
        tranche,
        "old generations cannot claim the surviving recapitalization",
    );
    assert_eq!(env.pool_outstanding(), 0, "the owner's full principal attribution retires");
    assert_eq!(env.pool_total_shares(), 0, "the full exit burns every real share");
}

#[test]
fn impaired_share_generation_cannot_claim_a_later_deposit() {
    let mut env = Env::new();
    env.init_insurance_pool();

    let amount = 1_000_000u64;
    let (old_owner, old_ata) = new_depositor(&mut env, amount);
    let (new_owner, new_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let old_holding = create_holding(&mut env, &pool);
    let new_holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&old_owner, &old_ata, &old_holding, amount)
        .expect("fund loss-bearing generation");

    impair_market(&mut env, 0);
    env.svm
        .set_account(
            env.perc_vault,
            Account {
                lamports: 1_000_000,
                data: token_account_data(&env.mint, &env.vault_authority, 0),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    env.insurance_deposit(&new_owner, &new_ata, &new_holding, amount)
        .expect("recapitalize the fully impaired pool");

    assert_eq!(env.pool_share_generation(), 1);
    assert_eq!(env.position_share_generation(&old_owner.pubkey()), 0);
    assert_eq!(env.position_share_generation(&new_owner.pubkey()), 1);
    env.insurance_withdraw(&old_owner, &old_ata, &old_holding, &old_owner, amount)
        .expect("the impaired position retires at zero without touching current capital");
    assert_eq!(env.token_amount(&old_ata), 0);
    assert_eq!(asset_insurance_remaining(&env, 0), amount as u128);

    env.insurance_withdraw(&new_owner, &new_ata, &new_holding, &new_owner, amount)
        .expect("the current depositor retains the recapitalization");
    assert_eq!(env.token_amount(&new_ata), amount);
    assert_eq!(env.pool_outstanding(), 0);
    assert_eq!(env.pool_total_shares(), 0);
}

#[test]
fn public_repeated_total_losses_cannot_exhaust_the_insurance_share_namespace() {
    use percolator_prog::ix::Instruction as PIx;

    let mut env = Env::new();
    let oracle = Keypair::new();
    let observer = Keypair::new();
    for owner in [&oracle, &observer] {
        env.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
    }
    install_public_loss_fixture_with_margin(&mut env, &oracle.pubkey(), 1_000);
    env.init_insurance_pool();

    let high_entry = 1_000_000_110u64;
    let high_capital = 100_000u64.checked_mul(high_entry).unwrap();
    let domain_tranche = 900_000u64
        .checked_mul(high_entry)
        .unwrap()
        .checked_sub(100_000_000)
        .unwrap();
    let tranche = domain_tranche.checked_mul(2).unwrap();
    let (attacker, attacker_ata) = new_depositor(&mut env, tranche * 2 + 200);
    let pool = env.pool;
    let attacker_holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&attacker, &attacker_ata, &attacker_holding, tranche)
        .expect("fund the first loss-bearing tranche");

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ConfigureAuthMark {
                asset_index: 0,
                now_slot: 100,
                initial_mark_e6: 100,
            }
            .encode(),
        }],
        &[&oracle],
    )
    .expect("configure authenticated mark");
    let observer_portfolio = create_percolator_portfolio(&mut env, &observer, 0);

    let mut slot = 100;
    run_complete_public_insurance_loss(
        &mut env,
        &oracle,
        observer_portfolio,
        &mut slot,
        100,
        10_000_000,
        high_entry,
        high_capital,
        domain_tranche,
        0,
    );
    assert_eq!(asset_insurance_remaining(&env, 0), 0);

    env.insurance_deposit(&attacker, &attacker_ata, &attacker_holding, tranche)
        .expect("recapitalize after the first complete public loss");
    run_complete_public_insurance_loss(
        &mut env,
        &oracle,
        observer_portfolio,
        &mut slot,
        100,
        10_000_000,
        high_entry,
        high_capital,
        domain_tranche,
        0,
    );
    assert_eq!(asset_insurance_remaining(&env, 0), 0);

    let mut successful_dust_recapitalizations = 0usize;
    for _ in 0..200 {
        if env
            .insurance_deposit(&attacker, &attacker_ata, &attacker_holding, 1)
            .is_err()
        {
            break;
        }
        successful_dust_recapitalizations += 1;
    }
    let (victim, victim_ata) = new_depositor(&mut env, 1);
    let victim_holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&victim, &victim_ata, &victim_holding, 1)
        .expect("an open deposit window must retain a bounded path for later capital");
    assert_eq!(
        successful_dust_recapitalizations, 200,
        "fully impaired recapitalization must not retain a finite-share admission cliff",
    );
    assert_eq!(env.pool_share_generation(), 2);
    assert_eq!(env.position_share_generation(&attacker.pubkey()), 2);
    assert_eq!(env.position_share_generation(&victim.pubkey()), 2);

    assert_eq!(
        env.position_shares(&attacker.pubkey()),
        successful_dust_recapitalizations as u128 * 1_000_000,
        "old-generation shares must not survive into the recapitalized fund",
    );
    assert_eq!(env.position_shares(&victim.pubkey()), 1_000_000);
    assert_eq!(
        env.pool_total_shares(),
        (successful_dust_recapitalizations as u128 + 1) * 1_000_000,
    );
    assert_eq!(asset_insurance_remaining(&env, 0), 201);
}

// PUBLIC LOF PROBE: an incumbent partially exits, its remaining tranche is impaired, a second
// owner recapitalizes, and both then bear another public loss. Trade fees in the second epoch are
// protocol reserve; they must not change either owner's loss-adjusted claim or the exit order.
#[test]
fn staggered_partial_exit_and_second_loss_cannot_shift_fees_between_owners() {
    use percolator_prog::ix::Instruction as PIx;

    #[derive(Clone, Copy, Debug)]
    struct Outcome {
        incumbent_partial: u64,
        incumbent_total: u64,
        late_total: u64,
        insurance_spent: u128,
        opening_fee: u128,
        protocol_reserve: u128,
    }

    let run = |opening_fee_bps: u64| {

    let mut env = Env::new();
    let oracle = Keypair::new();
    let observer = Keypair::new();
    for owner in [&oracle, &observer] {
        env.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
    }
    install_public_loss_fixture_with_margin(&mut env, &oracle.pubkey(), 1_000);
    env.init_insurance_pool();

    let low_entry = 100u64;
    let high_entry = 1_000_100u64;
    let low_capital = 10_000_000u64;
    let first_loss = 50_000_000u64;
    let long_loss = (high_entry - low_entry).checked_mul(1_000_000).unwrap();
    let late_principal = first_loss.checked_mul(2).unwrap();
    let first_principal = first_loss.checked_mul(4).unwrap();

    let (incumbent, incumbent_ata) = new_depositor(&mut env, first_principal);
    let pool = env.pool;
    let incumbent_holding = create_holding(&mut env, &pool);
    env.insurance_deposit(
        &incumbent,
        &incumbent_ata,
        &incumbent_holding,
        first_principal,
    )
    .expect("incumbent funds both domains before the first loss");
    let virtual_shares = 1_000_000u128;
    let incumbent_shares = (first_principal as u128)
        .checked_mul(virtual_shares)
        .unwrap();
    let partial_principal = first_principal / 2;
    let shares_to_retire = incumbent_shares
        .checked_mul(partial_principal as u128)
        .unwrap()
        / first_principal as u128;
    let expected_partial = shares_to_retire
        .checked_mul(asset_insurance_remaining(&env, 0) + 1)
        .unwrap()
        / env.pool_total_shares().checked_add(virtual_shares).unwrap();
    env.insurance_withdraw(
        &incumbent,
        &incumbent_ata,
        &incumbent_holding,
        &incumbent,
        partial_principal,
    )
    .expect("incumbent partially exits before public exposure");
    let incumbent_partial = env.token_amount(&incumbent_ata);
    assert_eq!(incumbent_partial as u128, expected_partial);
    assert_eq!(env.read_position(&incumbent.pubkey()).0, partial_principal);

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ConfigureAuthMark {
                asset_index: 0,
                now_slot: 100,
                initial_mark_e6: low_entry,
            }
            .encode(),
        }],
        &[&oracle],
    )
    .expect("configure authenticated mark");
    let observer_portfolio = create_percolator_portfolio(&mut env, &observer, 0);
    let mut slot = 100;

    let expected_after_first = asset_insurance_remaining(&env, 0)
        .checked_sub(u128::from(first_loss))
        .unwrap();
    let mut loss_portfolios = run_complete_public_insurance_loss(
        &mut env,
        &oracle,
        observer_portfolio,
        &mut slot,
        low_entry,
        low_capital,
        high_entry,
        long_loss,
        first_loss,
        expected_after_first,
    );
    assert_eq!(asset_insurance_remaining(&env, 0), expected_after_first);

    let (late, late_ata) = new_depositor(&mut env, late_principal);
    let late_holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&late, &late_ata, &late_holding, late_principal)
        .expect("late owner enters at the first loss-adjusted share price");

    let protected_before_second = asset_insurance_remaining(&env, 0);
    let second_domain_loss = first_loss / 5;
    let expected_without_fee = protected_before_second
        .checked_sub(u128::from(second_domain_loss) * 2)
        .unwrap();
    let (second_portfolios, opening_fee) = run_complete_public_insurance_loss_with_fee(
        &mut env,
        &oracle,
        observer_portfolio,
        &mut slot,
        low_entry,
        low_capital,
        high_entry,
        long_loss.checked_sub(second_domain_loss).unwrap(),
        second_domain_loss,
        expected_without_fee,
        opening_fee_bps,
    );
    loss_portfolios.extend(second_portfolios);
    assert_eq!(
        asset_insurance_remaining(&env, 0),
        expected_without_fee + opening_fee,
    );

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ResolveMarket.encode(),
        }],
        &[&oracle],
    )
    .expect("resolve after both public loss epochs");
    loss_portfolios.push((observer, observer_portfolio));
    let terminal: Vec<_> = loss_portfolios
        .iter()
        .map(|(owner, portfolio)| (owner, *portfolio))
        .collect();
    close_resolved_portfolios(&mut env, &terminal);

    let market = env.svm.get_account(&env.slab).unwrap();
    let insurance_spent = percolator_accounting::read_asset_insurance_spent(&market.data, 0)
        .unwrap()
        .into_iter()
        .sum::<u128>();
    env.insurance_withdraw(&late, &late_ata, &late_holding, &late, late_principal)
        .expect("late owner realizes only the losses from its own tenure");
    env.insurance_withdraw(
        &incumbent,
        &incumbent_ata,
        &incumbent_holding,
        &incumbent,
        partial_principal,
    )
    .expect("incumbent retires its remaining impaired principal");

    let incumbent_total = env.token_amount(&incumbent_ata);
    let late_total = env.token_amount(&late_ata);
    let protocol_reserve = asset_insurance_remaining(&env, 0);
    assert_eq!(
        u128::from(incumbent_total) + u128::from(late_total) + protocol_reserve,
        u128::from(first_principal + late_principal) - insurance_spent + opening_fee,
        "owner payouts and protocol reserve conserve deposits, public losses, and fees",
    );
    assert!(incumbent_total < first_principal);
    assert!(late_total < late_principal);

    Outcome {
        incumbent_partial,
        incumbent_total,
        late_total,
        insurance_spent,
        opening_fee,
        protocol_reserve,
    }
    };

    let control = run(0);
    let with_fees = run(100);
    assert!(with_fees.opening_fee > 0);
    assert_eq!(control.insurance_spent, with_fees.insurance_spent);
    assert_eq!(control.incumbent_partial, with_fees.incumbent_partial);
    assert_eq!(
        control.incumbent_total, with_fees.incumbent_total,
        "second-epoch fees cannot restore the incumbent's impaired claim",
    );
    assert_eq!(
        control.late_total, with_fees.late_total,
        "second-epoch fees cannot increase the later owner's principal-only claim",
    );
    assert_eq!(
        with_fees.protocol_reserve,
        control.protocol_reserve + with_fees.opening_fee,
        "every fee atom remains protocol reserve after both owners exit",
    );
}

// PUBLIC LOF: the first partial exit from an obsolete share generation pays zero and lazily
// normalizes the position. That normalization must not make a second exit look like a historical
// shareless position, which would let the impaired owner claim a later depositor's recapitalization.
// The loss below is produced by the pinned Percolator binary through authenticated marks, public
// trades, and permissionless liquidation/reset cranks.
#[test]
fn stale_generation_partial_exit_cannot_replay_as_a_historical_position() {
    use percolator_prog::ix::Instruction as PIx;

    let mut env = Env::new();
    let oracle = Keypair::new();
    let observer = Keypair::new();
    for owner in [&oracle, &observer] {
        env.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
    }
    install_public_loss_fixture_with_margin(&mut env, &oracle.pubkey(), 1_000);
    env.init_insurance_pool();

    let high_entry = 1_000_000_110u64;
    let high_capital = 100_000u64.checked_mul(high_entry).unwrap();
    let domain_tranche = 900_000u64
        .checked_mul(high_entry)
        .unwrap()
        .checked_sub(100_000_000)
        .unwrap();
    let impaired_principal = domain_tranche.checked_mul(2).unwrap();
    let (impaired_owner, impaired_ata) = new_depositor(&mut env, impaired_principal);
    let pool = env.pool;
    let impaired_holding = create_holding(&mut env, &pool);
    env.insurance_deposit(
        &impaired_owner,
        &impaired_ata,
        &impaired_holding,
        impaired_principal,
    )
    .expect("fund the loss-bearing generation");

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ConfigureAuthMark {
                asset_index: 0,
                now_slot: 100,
                initial_mark_e6: 100,
            }
            .encode(),
        }],
        &[&oracle],
    )
    .expect("configure authenticated mark");
    let observer_portfolio = create_percolator_portfolio(&mut env, &observer, 0);
    let mut slot = 100;
    let mut loss_portfolios = run_complete_public_insurance_loss(
        &mut env,
        &oracle,
        observer_portfolio,
        &mut slot,
        100,
        10_000_000,
        high_entry,
        high_capital,
        domain_tranche,
        0,
    );
    assert_eq!(asset_insurance_remaining(&env, 0), 0);

    let recapitalization = 1_000_000u64;
    let (new_owner, new_ata) = new_depositor(&mut env, recapitalization);
    let new_holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&new_owner, &new_ata, &new_holding, recapitalization)
        .expect("a new owner recapitalizes the fully impaired pool");
    assert_eq!(asset_insurance_remaining(&env, 0), recapitalization as u128);

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ResolveMarket.encode(),
        }],
        &[&oracle],
    )
    .expect("the authenticated authority resolves the completed public loss epoch");
    loss_portfolios.push((observer, observer_portfolio));
    let loss_portfolio_refs: Vec<_> = loss_portfolios
        .iter()
        .map(|(owner, portfolio)| (owner, *portfolio))
        .collect();
    close_resolved_portfolios(&mut env, &loss_portfolio_refs);
    assert_ne!(
        env.position_share_generation(&impaired_owner.pubkey()),
        env.pool_share_generation(),
    );

    env.insurance_withdraw(
        &impaired_owner,
        &impaired_ata,
        &impaired_holding,
        &impaired_owner,
        1,
    )
    .expect("the first stale partial exit retires one lost principal atom at zero payout");
    assert_eq!(env.token_amount(&impaired_ata), 0);
    assert_eq!(env.position_shares(&impaired_owner.pubkey()), 0);
    assert_eq!(
        env.position_share_generation(&impaired_owner.pubkey()),
        env.pool_share_generation(),
    );

    env.insurance_withdraw(
        &impaired_owner,
        &impaired_ata,
        &impaired_holding,
        &impaired_owner,
        impaired_principal - 1,
    )
    .expect("the remainder of the impaired position retires at zero payout");
    assert_eq!(
        env.token_amount(&impaired_ata),
        0,
        "normalization cannot turn an obsolete zero-share position into a pro-rata legacy claim",
    );
    assert_eq!(
        asset_insurance_remaining(&env, 0),
        recapitalization as u128,
        "the new owner's recapitalization remains segregated",
    );

    env.insurance_withdraw(
        &new_owner,
        &new_ata,
        &new_holding,
        &new_owner,
        recapitalization,
    )
    .expect("the current-generation owner recovers the recapitalization");
    assert_eq!(env.token_amount(&new_ata), recapitalization);
}

// PUBLIC DEPOSIT-LIVENESS PROBE: a one-atom residual prevents the exact-loss generation reset.
// Repeated real market loss/recapitalization cycles must not amplify the finite share exchange rate
// until even the minimum deposit overflows. Every loss below is produced by the pinned Percolator
// binary through authenticated marks, public trades, and permissionless liquidation/reset cranks.
#[test]
fn repeated_near_total_public_losses_preserve_one_atom_deposit_liveness() {
    use percolator_prog::ix::Instruction as PIx;

    let mut env = Env::new();
    let oracle = Keypair::new();
    let observer = Keypair::new();
    for owner in [&oracle, &observer] {
        env.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
    }
    install_public_loss_fixture_with_margin(&mut env, &oracle.pubkey(), 1_000);
    env.init_insurance_pool();

    let high_entry = 1_000_000_110u64;
    let high_capital = 100_000u64.checked_mul(high_entry).unwrap();
    let domain_tranche = 900_000u64
        .checked_mul(high_entry)
        .unwrap()
        .checked_sub(100_000_000)
        .unwrap();
    let tranche = domain_tranche.checked_mul(2).unwrap();
    let (attacker, attacker_ata) = new_depositor(&mut env, tranche * 2 + 105);
    let pool = env.pool;
    let attacker_holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&attacker, &attacker_ata, &attacker_holding, tranche)
        .expect("fund the first near-total loss cycle");

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ConfigureAuthMark {
                asset_index: 0,
                now_slot: 100,
                initial_mark_e6: 100,
            }
            .encode(),
        }],
        &[&oracle],
    )
    .expect("configure authenticated mark");
    let observer_portfolio = create_percolator_portfolio(&mut env, &observer, 0);

    let mut slot = 100;
    run_complete_public_insurance_loss(
        &mut env,
        &oracle,
        observer_portfolio,
        &mut slot,
        100,
        10_000_000,
        high_entry,
        high_capital + 1,
        domain_tranche,
        1,
    );
    env.insurance_deposit(&attacker, &attacker_ata, &attacker_holding, tranche)
        .expect("recapitalize the one-atom residual");
    run_complete_public_insurance_loss(
        &mut env,
        &oracle,
        observer_portfolio,
        &mut slot,
        100,
        10_000_000,
        high_entry,
        high_capital,
        domain_tranche,
        1,
    );

    for (deposit, domain) in [(100u64, 50u64), (4u64, 2u64)] {
        env.insurance_deposit(&attacker, &attacker_ata, &attacker_holding, deposit)
            .expect("recapitalize the one-atom residual at the amplified share price");
        run_complete_public_insurance_loss(
            &mut env,
            &oracle,
            observer_portfolio,
            &mut slot,
            100,
            11_000_000 - domain,
            1_000,
            900_000_000 - domain,
            domain,
            1,
        );
    }

    assert_eq!(asset_insurance_remaining(&env, 0), 1);
    let (victim, victim_ata) = new_depositor(&mut env, 1);
    let victim_holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&victim, &victim_ata, &victim_holding, 1)
        .expect("a live one-atom pool must retain its minimum public deposit path");
    assert_eq!(asset_insurance_remaining(&env, 0), 2);

    let scaled_generation = env.pool_share_generation();
    let reset_generation_mask = (1u64 << 20) - 1;
    assert_eq!(scaled_generation & reset_generation_mask, 0);
    assert!(
        scaled_generation >> 20 > 0,
        "the public replay must actually enter the lazy share-rescale path",
    );
    assert_ne!(
        env.position_share_generation(&attacker.pubkey()),
        scaled_generation,
        "an untouched depositor remains lazy until its next public operation",
    );
    let stale_attacker_shares = env.position_shares(&attacker.pubkey());
    assert_eq!(env.token_amount(&attacker_ata), 1);
    env.insurance_deposit(&attacker, &attacker_ata, &attacker_holding, 1)
        .expect("an existing depositor can normalize lazily and top up");
    assert_eq!(
        env.position_share_generation(&attacker.pubkey()),
        scaled_generation,
    );
    assert!(env.position_shares(&attacker.pubkey()) < stale_attacker_shares);
    assert_eq!(asset_insurance_remaining(&env, 0), 3);
}

// SHARE-INFLATION FIRST-DEPOSITOR THEFT (finding HB, surface B). The classic ERC4626 attack: a dust first
// depositor DONATES into the fund to inflate the live share PRICE (balance >> total_shares) so a later
// depositor's shares round toward ZERO; that principal then lands in the fund for 0 shares and the attacker's
// pre-existing shares redeem it (theft). The subledger defends with TWO layers: VIRTUAL_SHARES=1e6 bounds the
// price skew, AND insurance_deposit REJECTS any deposit that would mint 0 shares (lib.rs:994, finding HB).
//
// EMPIRICAL NOTE (this tick): the REAL percolator caps the insurance fund — TopUpInsurance reverts (custom
// error 0xe) long before insurance can be inflated to the ~2e12 needed to round a 1-USDC victim to 0 shares.
// So the dangerous regime for a MEANINGFUL victim is blocked at the percolator layer, ahead of HB. HB is the
// subledger backstop for the SUB-CAP regime, which this pins with realistic small values: a dust attacker
// (2 atoms -> 2e6 shares) + a modest 3e6 surplus rounds a 1-atom victim to exactly 0 shares. The victim's
// TopUpInsurance CPI SUCCEEDS at this realistic insurance (mutation-SHARP: neutering HB lets the 0-share
// deposit through — verified), so HB is the SOLE rejecter, not a slab-shape CPI error. Previously untested.
#[test]
fn share_inflation_first_depositor_donation_cannot_strand_a_later_depositor_finding_hb() {
    let mut env = Env::new_for_policy(POLICY_WITH_SURPLUS);
    env.init_insurance_pool_policy(POLICY_WITH_SURPLUS); // share-priced

    // Attacker is the FIRST (dust) depositor: 2 atoms -> 2*VIRTUAL_SHARES = 2e6 shares, insurance = 2.
    let (attacker, attacker_ata) = new_depositor(&mut env, 2);
    let pool = env.pool;
    let att_hold = create_holding(&mut env, &pool);
    env.insurance_deposit(&attacker, &attacker_ata, &att_hold, 2).expect("attacker dust deposit");
    let shares_before = env.pool_total_shares();
    assert_eq!(shares_before, 2_000_000, "first dust deposit mints amount*VIRTUAL_SHARES");

    // ATTACK: donate to inflate the live share price. A modest 3e6 surplus is well within percolator's cap, so
    // the CPI path stays valid. impair_market raises the slab insurance to 3e6; mirror the REAL SPL vault to
    // the same balance so the victim's TopUpInsurance CPI SUCCEEDS — HB is then the only thing that can reject.
    let donation = 3_000_000u128;
    impair_market(&mut env, donation);
    env.svm.set_account(env.perc_vault, Account {
        lamports: 1_000_000, data: token_account_data(&env.mint, &env.vault_authority, donation as u64),
        owner: spl_token::ID, executable: false, rent_epoch: 0,
    }).unwrap();

    // VICTIM deposits 1 atom. shares = V*(total_shares+VS)/(insurance+1) = 1*(2e6+1e6)/(3e6+1) = 0.
    // finding HB: a zero-share deposit is REJECTED — the victim never hands principal to the attacker's shares.
    let (victim, victim_ata) = new_depositor(&mut env, 1);
    let vic_hold = create_holding(&mut env, &pool);
    let r = env.insurance_deposit(&victim, &victim_ata, &vic_hold, 1);
    assert!(r.is_err(), "a zero-share (inflation-rounded) deposit must be REJECTED (finding HB), got {r:?}");
    // The victim KEEPS their principal — the rejected deposit moved nothing.
    assert_eq!(env.token_amount(&victim_ata), 1, "victim retains their principal; not stranded for 0 shares");
    assert_eq!(env.pool_total_shares(), shares_before, "no shares minted for the victim; the attacker cannot redeem the victim's principal");
}

// CO-DEPOSITOR DRAIN (no-drain property, DEFENSE-IN-DEPTH): insurance_withdraw caps `amount` by BOTH
// `position.principal` AND `pool.outstanding_principal`. Because outstanding is the SUM of every position's
// principal, the per-position clause is the tighter one: without it a depositor in a multi-party pool could
// request up to the WHOLE pool (amount up to outstanding) and drain co-depositors. This test pins the no-drain
// property end-to-end: in a HEALTHY 2-party pool Alice tries to pull the whole 2M (> her 1M, but == outstanding
// so the pool-only clause would allow it); the withdraw MUST be rejected and Bob's funds MUST be untouched.
// HONEST NOTE (mutation-blind for the explicit clause): the over-withdraw is blocked by TWO layers, so dropping
// the per-position clause alone leaves this test (and the suite) GREEN — the `overflow-checks = true` backstop
// makes `position.principal -= amount` panic-REVERT on the same `amount > principal` condition (it does NOT wrap
// to u64::MAX), atomically reverting the already-issued CPI/transfer. So the clause cannot be sharply isolated by
// a value test (the underflow always reverts first); it is the clean early-reject, overflow-checks is the
// load-bearing backstop. The test still correctly verifies the SECURITY property (no co-depositor drain).
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
// never exceed their single-shot share, and the rounding dust remains unowned protocol insurance
// rather than becoming an exit-order reward. With an odd insurance (1,000,001) the dust is a real atom, so a
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

// PUBLIC LOF: whole-atom share redemption must not let the last exiter absorb every earlier
// depositor's fractional claim. The venue loss below is produced entirely through the pinned
// Percolator public interface; only the initial authority wiring is fixture state.
#[test]
fn impaired_exit_order_cannot_transfer_rounding_value_to_the_last_depositor() {
    use percolator_prog::ix::Instruction as PIx;

    let mut env = Env::new();
    let oracle = Keypair::new();
    env.svm.airdrop(&oracle.pubkey(), 1_000_000_000).unwrap();
    install_public_loss_fixture(&mut env, &oracle.pubkey());
    env.init_insurance_pool();

    let (attacker, attacker_ata) = new_depositor(&mut env, 3);
    let (victim, victim_ata) = new_depositor(&mut env, 1);
    let pool = env.pool;
    let attacker_holding = create_holding(&mut env, &pool);
    let victim_holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&attacker, &attacker_ata, &attacker_holding, 3)
        .expect("attacker deposits three insurance atoms");
    env.insurance_deposit(&victim, &victim_ata, &victim_holding, 1)
        .expect("victim deposits one insurance atom");
    assert_eq!(asset_insurance_remaining(&env, 0), 4);

    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ConfigureAuthMark {
                asset_index: 0,
                now_slot: 100,
                initial_mark_e6: 100,
            }
            .encode(),
        }],
        &[&oracle],
    )
    .expect("configure the legitimate authenticated mark");

    let long = Keypair::new();
    let short = Keypair::new();
    for owner in [&long, &short] {
        env.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
    }
    let long_portfolio = create_percolator_portfolio(&mut env, &long, 1_000_000);
    let short_portfolio = create_percolator_portfolio(&mut env, &short, 200);
    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new(long.pubkey(), true),
                AccountMeta::new(short.pubkey(), true),
                AccountMeta::new(env.slab, false),
                AccountMeta::new(long_portfolio, false),
                AccountMeta::new(short_portfolio, false),
            ],
            data: PIx::TradeNoCpi {
                asset_index: 0,
                size_q: (2 * percolator::POS_SCALE) as i128,
                exec_price: 100,
                fee_bps: 0,
            }
            .encode(),
        }],
        &[&long, &short],
    )
    .expect("open the public long/short pair");

    env.warp_slot(101);
    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::PushAuthMark {
                asset_index: 0,
                now_slot: 101,
                mark_e6: 1_000,
            }
            .encode(),
        }],
        &[&oracle],
    )
    .expect("move the public mark against the undercapitalized short");

    let crank = |env: &mut Env, portfolio: Pubkey, now_slot: u64| {
        env.send(
            &[Instruction {
                program_id: perc_id(),
                accounts: vec![
                    AccountMeta::new(env.payer.pubkey(), true),
                    AccountMeta::new(env.slab, false),
                    AccountMeta::new(portfolio, false),
                ],
                data: PIx::PermissionlessCrank {
                    now_slot,
                    observations: vec![percolator_prog::ix::CrankObservationHint {
                        asset_index: 0,
                        oracle_accounts: 0,
                    }],
                }
                .encode(),
            }],
            &[],
        )
    };
    crank(&mut env, long_portfolio, 101).expect("crank the winning side");
    env.warp_slot(102);
    crank(&mut env, long_portfolio, 102).expect("refresh the winning side");
    env.warp_slot(103);
    for _ in 0..2 {
        crank(&mut env, short_portfolio, 103)
            .expect("permissionless liquidation consumes only the funded domain");
    }
    assert_eq!(
        asset_insurance_remaining(&env, 0),
        2,
        "the public liquidation spends the two atoms in its balanced funded domain",
    );

    // Resolve and remove both public portfolios so the real Percolator withdrawal gate is clear.
    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ResolveMarket.encode(),
        }],
        &[&oracle],
    )
    .expect("resolve the publicly impaired market");
    close_resolved_portfolios(
        &mut env,
        &[(&long, long_portfolio), (&short, short_portfolio)],
    );

    // The victim exits first. Their fractional claim rounds down, but the aggregate rounding atom
    // must become protocol reserve rather than increasing the attacker's later exchange rate.
    env.insurance_withdraw(&victim, &victim_ata, &victim_holding, &victim, 1)
        .expect("victim retires the impaired one-atom position");
    env.insurance_withdraw(&attacker, &attacker_ata, &attacker_holding, &attacker, 3)
        .expect("attacker retires last");
    assert_eq!(env.token_amount(&victim_ata), 0);
    assert_eq!(
        env.token_amount(&attacker_ata),
        1,
        "last-exit ordering cannot transfer the victim's rounded claim to the attacker",
    );
    assert_eq!(
        asset_insurance_remaining(&env, 0),
        1,
        "aggregate whole-atom rounding remains protocol insurance",
    );
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

    // Bob, who never split, exits last and is NOT drained. Alice's floor remainder
    // remains protocol insurance rather than becoming an exit-order reward for Bob.
    env.insurance_withdraw(&bob, &bob_ata, &b_hold, &bob, amount).expect("bob exits whole");
    let bob_total = env.token_amount(&bob_ata);
    assert!(
        bob_total >= alice_total,
        "the co-depositor who stayed is not drained by the splitter (bob {bob_total} >= alice {alice_total})"
    );

    let reserve = env.token_amount(&env.perc_vault);
    assert_eq!(reserve, 1, "the aggregate floor remainder stays protocol insurance");
    assert_eq!(alice_total + bob_total + reserve, impaired as u64, "payouts plus reserve conserve the impaired insurance");
}

// The share-redemption policy has a different partial-exit path from the principal policy above:
// every chunk retires proportional position shares, while the pool burns only enough shares to keep
// its post-exit exchange rate from increasing. Exercise that branch repeatedly so a splitter cannot
// turn per-chunk floor remainders into value taken from the depositor who remains in the pool.
#[test]
fn with_surplus_split_exit_cannot_capture_a_codepositors_rounding() {
    let mut env = Env::new_for_policy(POLICY_WITH_SURPLUS);
    env.init_insurance_pool_policy(POLICY_WITH_SURPLUS);

    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let (bob, bob_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let alice_holding = create_holding(&mut env, &pool);
    let bob_holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &alice_holding, amount)
        .expect("alice deposits");
    env.insurance_deposit(&bob, &bob_ata, &bob_holding, amount)
        .expect("bob deposits");

    let impaired = 1_000_001u64;
    impair_market(&mut env, impaired as u128);
    env.svm
        .set_account(
            env.perc_vault,
            Account {
                lamports: 1_000_000,
                data: token_account_data(&env.mint, &env.vault_authority, impaired),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    for chunk in [400_000u64, 300_000, 300_000] {
        env.insurance_withdraw(&alice, &alice_ata, &alice_holding, &alice, chunk)
            .expect("split share exit remains live");
    }
    let alice_total = env.token_amount(&alice_ata);
    assert!(
        alice_total <= 500_000,
        "split redemption cannot exceed Alice's floored half, got {alice_total}"
    );

    env.insurance_withdraw(&bob, &bob_ata, &bob_holding, &bob, amount)
        .expect("the remaining depositor exits");
    let bob_total = env.token_amount(&bob_ata);
    let reserve = env.token_amount(&env.perc_vault);
    assert!(
        bob_total >= alice_total,
        "Alice's split exit cannot drain Bob: alice={alice_total}, bob={bob_total}"
    );
    assert_eq!(reserve, 1, "the whole-atom rounding reserve remains unowned");
    assert_eq!(
        alice_total + bob_total + reserve,
        impaired,
        "all impaired insurance remains with depositors or protocol reserve"
    );
    assert_eq!(env.pool_outstanding(), 0, "both principal claims retire");
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
            AccountMeta::new_readonly(env.gv_config_pda(), false),
            AccountMeta::new_readonly(env.coin_mint, false),
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
    let (expected_principal, expected_start_slot, _) = env.read_position(&alice.pubkey());
    let mut d = vec![5u8];
    d.extend_from_slice(&amount.to_le_bytes());
    d.extend_from_slice(&expected_principal.to_le_bytes());
    d.extend_from_slice(&expected_start_slot.to_le_bytes());
    d.extend_from_slice(&env.position_action_nonce(&alice.pubkey()).to_le_bytes());
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

// Registration accepts only a proposal whose declared entry shape is complete.
// This removes creator action before voters can lock capital: a partial proposal
// cannot become votable, while a registered full proposal has no room to append.
#[test]
fn partial_proposal_cannot_be_registered_or_mutated_after_registration() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);
    let dist_config = ve.dist_config;

    let id = 1u64;
    let dist_proposal =
        Pubkey::find_program_address(&[b"dist_proposal", dist_config.as_ref(), &id.to_le_bytes()], &dist_id()).0;
    let mut cd = vec![1u8];
    cd.extend_from_slice(&id.to_le_bytes());
    cd.extend_from_slice(&2u32.to_le_bytes());
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

    let gv_proposal =
        Pubkey::find_program_address(&[b"gv_proposal", ve.gv_config.as_ref(), dist_proposal.as_ref()], &gv_id()).0;
    let register = Instruction {
        program_id: gv_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(ve.gv_config, false),
            AccountMeta::new(gv_proposal, false),
            AccountMeta::new_readonly(dist_proposal, false),
            AccountMeta::new_readonly(dist_config, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: vec![2u8],
    };
    assert!(
        env.send(&[register.clone()], &[]).is_err(),
        "a partial declared shape cannot become votable"
    );
    assert!(
        env.svm
            .get_account(&gv_proposal)
            .is_none_or(|account| account.data.is_empty()),
        "failed registration is atomic"
    );

    let second = Pubkey::new_unique();
    append(&mut env, &second, 60).expect("creator completes the declared shape");
    env.send(&[register], &[])
        .expect("the complete proposal can be registered");
    let proposal_after_registration = env.svm.get_account(&dist_proposal).unwrap();
    let vote_after_registration = env.svm.get_account(&gv_proposal).unwrap();
    assert!(
        append(&mut env, &Pubkey::new_unique(), 1).is_err(),
        "a registered full-capacity proposal has no mutable entry slot"
    );
    assert_eq!(
        env.svm.get_account(&dist_proposal).unwrap(),
        proposal_after_registration
    );
    assert_eq!(
        env.svm.get_account(&gv_proposal).unwrap(),
        vote_after_registration
    );
}

#[test]
fn genesis_vote_reads_subledger_principal_one_for_one() {
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
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("vote backs proposal");

    let (support_weight, support_principal) = gv_proposal_support(&env, &gv_proposal);
    assert_eq!(support_principal, amount);
    assert_eq!(support_weight, amount, "one principal base unit contributes one vote");
}

// LEGACY-UPGRADE REGRESSION for finding GG: historical age-weighted configurations can carry a
// total_cast_weight at the old u64 boundary. The retained u128 layout must still accept a current
// principal-only vote so an upgraded in-progress configuration cannot freeze honest voters.
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
    env.warp_slot(1124);

    // Inject total_cast_weight = u64::MAX — exactly where the OLD u64 tally would overflow on the next vote's
    // checked_add and reject it (the freeze point). With u128 it is far from the ceiling.
    let mut cfg = env.svm.get_account(&ve.gv_config).unwrap();
    cfg.data[208..224].copy_from_slice(&(u64::MAX as u128).to_le_bytes());
    env.svm.set_account(ve.gv_config, cfg).unwrap();

    // Alice's vote adds `amount` on top of u64::MAX and must not overflow.
    gv_vote(&mut env, &ve, &alice, &gv_prop, 1)
        .expect("an honest vote must not be blocked by a near-u64::MAX cast-weight tally (GG fix)");

    // The tally grew PAST u64::MAX (u128) instead of overflowing/rejecting.
    let cast = u128::from_le_bytes(
        env.svm.get_account(&ve.gv_config).unwrap().data[208..224].try_into().unwrap());
    assert!(cast > u64::MAX as u128, "total_cast_weight exceeded u64::MAX without overflow (= {})", cast);
}

// FORGED-POSITION SUPPLY THEFT: `vote` reads principal from the subledger position. If an attacker could feed a
// FORGED position with a u64::MAX principal, they would mint themselves the same weight and principal,
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
        let mut ix = gv_vote_ix(&env, &ve, &alice.pubkey(), &gv_proposal, 1);
        assert_eq!(ix.accounts[2].pubkey, gv_ballot);
        ix.accounts[4] = AccountMeta::new(wrong_key, false);
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
    env.warp_slot(1124);

    let read_cast = |env: &Env| -> u64 {
        u128::from_le_bytes(env.svm.get_account(&ve.gv_config).unwrap().data[208..224].try_into().unwrap()) as u64
    };

    // First vote: weight counted once.
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("first vote");
    assert_eq!(gv_proposal_support(&env, &gv_proposal), (amount, amount), "one vote, weight once");
    assert_eq!(read_cast(&env), amount, "global cast = one vote");

    // ATTACK: vote AGAIN on the SAME proposal. The backout removes the prior contribution before re-adding,
    // so the tallies stay at exactly one vote — they must NOT double.
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("re-vote accepted (idempotent)");
    assert_eq!(gv_proposal_support(&env, &gv_proposal), (amount, amount),
        "re-vote must NOT double the proposal's weight/principal");
    assert_eq!(read_cast(&env), amount, "re-vote must NOT double the global cast weight");

    // Even a third time stays put.
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("third vote");
    assert_eq!(gv_proposal_support(&env, &gv_proposal).0, amount, "weight still counted exactly once");
    assert_eq!(read_cast(&env), amount, "global cast still exactly one vote");
}

// ANTI-DOUBLE-VOTE via a SECOND (non-canonical) BALLOT (surface B): the per-voter ballot is the PDA
// ["gv_ballot", config, voter]; vote re-derives it and binds ballot_account.key == that PDA (lib.rs:607).
// re_voting_the_same_proposal (above) shows re-using the CANONICAL ballot subtract-old/add-news (no
// double-count) — but a voter could instead try to add weight a SECOND time by passing a DIFFERENT ballot
// account, banking two ballots' worth of cast weight (a free majority/quorum inflation toward supply theft).
// This pins that a non-canonical ballot is REJECTED so each voter has exactly one ballot. (The guard is
// defense-in-depth: even with the 607 key-bind removed, the create path's invoke_signed seeds derive the
// CANONICAL address — a fresh non-canonical account can't be created — and the re-vote path checks the stored
// ballot.owner == voter; so the property holds via those backstops. This was the untested boundary the 607
// mutation-blindness surfaced.)
#[test]
fn a_voter_cannot_double_vote_with_a_second_non_canonical_ballot() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("deposit");
    let dest = Pubkey::new_unique();
    let (_dp, gv_proposal) = create_and_register_proposal(&mut env, &ve, 1, &dest);
    env.warp_slot(1124);
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("alice's one legitimate vote");
    let support_after_one = gv_proposal_support(&env, &gv_proposal).0;
    assert!(support_after_one > 0, "alice's single vote is counted");

    // ATTACK: vote AGAIN but pass a DIFFERENT (non-canonical, fresh) ballot account to bank a second tally.
    let rogue_ballot = Pubkey::new_unique();
    let mut rogue_vote = gv_vote_ix(&env, &ve, &alice.pubkey(), &gv_proposal, 1);
    rogue_vote.accounts[2] = AccountMeta::new(rogue_ballot, false);
    assert!(env.send(&[rogue_vote], &[&alice]).is_err(),
        "a second vote with a non-canonical ballot must be rejected — one ballot per voter");
    // The proposal's weight is UNCHANGED: alice still has exactly one ballot's worth, no double-count.
    assert_eq!(gv_proposal_support(&env, &gv_proposal).0, support_after_one,
        "the rejected second-ballot vote did not inflate the proposal's weight");
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
    env.warp_slot(1124);
    let read_cast = |env: &Env| -> u64 {
        u128::from_le_bytes(env.svm.get_account(&ve.gv_config).unwrap().data[208..224].try_into().unwrap()) as u64
    };

    // bob backs B (so B already has support == alice's weight; without the guard the backout from B would
    // not underflow). alice backs A.
    gv_vote(&mut env, &ve, &bob, &gv_b, 1).expect("bob backs B");
    gv_vote(&mut env, &ve, &alice, &gv_a, 1).expect("alice backs A");
    assert_eq!(gv_proposal_support(&env, &gv_a), (amount, amount), "A has alice's vote");
    assert_eq!(gv_proposal_support(&env, &gv_b), (amount, amount), "B has bob's vote");

    // ATTACK: alice (live on A) backs B WITHOUT retracting A. Must be refused — else her weight would be
    // double-represented across A and B.
    assert!(
        gv_vote(&mut env, &ve, &alice, &gv_b, 1).is_err(),
        "a voter with a live ballot on A must retract before backing B"
    );
    assert_eq!(gv_proposal_support(&env, &gv_a), (amount, amount), "A's tally intact — no phantom left behind");
    assert_eq!(gv_proposal_support(&env, &gv_b), (amount, amount), "B's tally unchanged by the rejected cross-vote");
    assert_eq!(read_cast(&env), 2 * amount, "global cast = exactly the two real votes");

    // The LEGIT switch works: alice retracts A, then backs B. A -> 0, B -> alice + bob.
    gv_vote(&mut env, &ve, &alice, &gv_a, 2).expect("alice retracts A");
    gv_vote(&mut env, &ve, &alice, &gv_b, 1).expect("alice now backs B");
    assert_eq!(gv_proposal_support(&env, &gv_a), (0, 0), "A fully released after retract");
    assert_eq!(gv_proposal_support(&env, &gv_b), (2 * amount, 2 * amount), "B now holds both votes");
    assert_eq!(read_cast(&env), 2 * amount, "global cast still exactly two votes — never inflated");
}

// DOUBLE-RETRACT (tally corruption / underflow): after a voter retracts, the ballot is no longer live, so a SECOND
// retract must be a no-op rejection ("nothing to retract") — it must NOT decrement the proposal/global tallies a
// second time. Without the has_live_ballot gate (lib.rs:641) + the explicit nothing-to-retract guard (646), a
// re-retract would checked_sub the already-released contribution again: a clean underflow-revert at best, a
// corrupted quorum denominator at worst. This pins that the second retract errors and EVERY tally is left exactly
// where the first retract put it, and that the ballot is still usable (the voter can back again).
#[test]
fn double_retract_is_rejected_and_does_not_double_release_the_tally() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);
    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let a_hold = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &a_hold, amount).expect("alice deposit");
    let (_da, gv_a) = create_and_register_proposal(&mut env, &ve, 1, &Pubkey::new_unique());
    env.warp_slot(1124);
    let read_cast = |env: &Env| -> u64 {
        u128::from_le_bytes(env.svm.get_account(&ve.gv_config).unwrap().data[208..224].try_into().unwrap()) as u64
    };

    gv_vote(&mut env, &ve, &alice, &gv_a, 1).expect("alice backs A");
    assert_eq!(gv_proposal_support(&env, &gv_a), (amount, amount), "A has alice's vote");
    assert_eq!(read_cast(&env), amount, "global cast = alice's one vote");

    // First retract: fully releases the tally.
    gv_vote(&mut env, &ve, &alice, &gv_a, 2).expect("alice retracts A");
    assert_eq!(gv_proposal_support(&env, &gv_a), (0, 0), "A released after the first retract");
    assert_eq!(read_cast(&env), 0, "global cast back to zero");

    // ATTACK: a SECOND retract must be rejected (nothing to retract) and must NOT touch any tally.
    assert!(gv_vote(&mut env, &ve, &alice, &gv_a, 2).is_err(), "a double-retract must be rejected — no live ballot");
    assert_eq!(gv_proposal_support(&env, &gv_a), (0, 0), "A's tally untouched by the rejected double-retract (no double-release / underflow)");
    assert_eq!(read_cast(&env), 0, "global cast still zero — no second decrement");

    // The ballot is intact: alice can back again, and the tally returns to exactly one vote.
    gv_vote(&mut env, &ve, &alice, &gv_a, 1).expect("alice re-backs A after the rejected double-retract");
    assert_eq!(gv_proposal_support(&env, &gv_a), (amount, amount), "re-back restores exactly one vote — ballot not corrupted");
    assert_eq!(read_cast(&env), amount, "global cast = one vote again");
}

// PRESIGNED RETRACT REPLAY: the ballot PDA is reused when a voter retracts and later backs the
// same proposal again. A relayer must not be able to hold the old signed retract until backing
// closes, then erase the replacement vote and leave the genesis distribution without a winner.
#[test]
fn presigned_retract_cannot_remove_a_replacement_vote_after_bootstrap_deadline() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);
    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("alice deposits");
    let (dist_proposal, gv_proposal) =
        create_and_register_proposal(&mut env, &ve, 1, &Pubkey::new_unique());

    let bootstrap_end = env.bootstrap_end_slot();
    env.warp_slot(bootstrap_end - 4);
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("alice backs the winner");
    let first_support = gv_proposal_support(&env, &gv_proposal);
    assert!(first_support.0 > 0);
    assert_eq!(first_support.1, amount);

    // Alice signs a retract through one relayer. Keep that exact transaction off-chain while she
    // lands a distinct sponsored retract and then re-backs the proposal before the deadline.
    env.svm.expire_blockhash();
    let held_blockhash = env.svm.latest_blockhash();
    let payer = clone_kp(&env.payer);
    let exact_retract = gv_vote_ix(&env, &ve, &alice.pubkey(), &gv_proposal, 2);
    let withheld_exact_retract = Transaction::new_signed_with_payer(
        &[ComputeBudgetInstruction::set_compute_unit_limit(1_400_000), exact_retract.clone()],
        Some(&payer.pubkey()),
        &[&payer, &alice],
        held_blockhash,
    );
    let withheld_legacy_retract = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            legacy_gv_vote_ix(&env, &ve, &alice.pubkey(), &gv_proposal, 2),
        ],
        Some(&payer.pubkey()),
        &[&payer, &alice],
        held_blockhash,
    );
    env.svm
        .send_transaction(Transaction::new_signed_with_payer(
            &[ComputeBudgetInstruction::set_compute_unit_limit(1_399_999), exact_retract],
            Some(&payer.pubkey()),
            &[&payer, &alice],
            held_blockhash,
        ))
        .expect("alice lands a distinct legitimate retract");
    assert_eq!(gv_proposal_support(&env, &gv_proposal), (0, 0));

    let reback = gv_vote_ix(&env, &ve, &alice.pubkey(), &gv_proposal, 1);
    env.svm
        .send_transaction(Transaction::new_signed_with_payer(
            &[ComputeBudgetInstruction::set_compute_unit_limit(1_399_998), reback],
            Some(&payer.pubkey()),
            &[&payer, &alice],
            held_blockhash,
        ))
        .expect("alice re-backs before voting closes");
    let replacement_support = gv_proposal_support(&env, &gv_proposal);
    assert!(replacement_support.0 > 0);
    assert_eq!(replacement_support.1, amount);

    // Once backing closes, replaying the old authorization must not retract the new vote. If it
    // does, no voter transaction can restore support and the fixed-supply distribution stays stuck.
    env.warp_slot(bootstrap_end);
    assert!(
        env.svm.send_transaction(withheld_exact_retract).is_err(),
        "an exact retract signature must be bound to one ballot incarnation"
    );
    assert!(
        env.svm.send_transaction(withheld_legacy_retract).is_err(),
        "a nonce-bearing ballot must reject a predecessor action-only retract"
    );
    assert_eq!(
        gv_proposal_support(&env, &gv_proposal),
        replacement_support,
        "the replacement vote remains counted after the stale replay"
    );
    assert!(
        gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).is_err(),
        "the deadline leaves no re-back recovery after a stale retract"
    );

    gv_trigger_now(&mut env, &ve, &gv_proposal, &dist_proposal)
        .expect("the intact replacement vote seals the distribution");
    assert_eq!(env.svm.get_account(&gv_proposal).unwrap().data[96], 1);
}

// PRE-BALLOT RETRACT REPLAY: a transaction signed while the canonical ballot does not exist
// must not become valid after the voter's first ballot is created. This covers both predecessor
// encodings and the current position-incarnation wire: the first vote's lock transition must make
// every pre-ballot authorization stale before backing closes.
#[test]
fn presigned_preballot_retracts_cannot_remove_a_later_first_vote() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);
    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount)
        .expect("alice deposits");
    let (dist_proposal, gv_proposal) =
        create_and_register_proposal(&mut env, &ve, 1, &Pubkey::new_unique());

    let bootstrap_end = env.bootstrap_end_slot();
    env.warp_slot(bootstrap_end - 4);
    env.svm.expire_blockhash();
    let held_blockhash = env.svm.latest_blockhash();
    let payer = clone_kp(&env.payer);

    // The canonical ballot is still absent when this predecessor retract is signed.
    let ballot = Pubkey::find_program_address(
        &[
            b"gv_ballot",
            ve.gv_config.as_ref(),
            alice.pubkey().as_ref(),
        ],
        &gv_id(),
    )
    .0;
    assert!(env.svm.get_account(&ballot).is_none());
    let mut old_exact_retract = legacy_gv_vote_ix(&env, &ve, &alice.pubkey(), &gv_proposal, 2);
    old_exact_retract.data.extend_from_slice(&0u64.to_le_bytes());
    let withheld_old_exact_retract = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            old_exact_retract,
        ],
        Some(&payer.pubkey()),
        &[&payer, &alice],
        held_blockhash,
    );
    let withheld_legacy_retract = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_399_999),
            legacy_gv_vote_ix(&env, &ve, &alice.pubkey(), &gv_proposal, 2),
        ],
        Some(&payer.pubkey()),
        &[&payer, &alice],
        held_blockhash,
    );
    let withheld_position_bound_retract = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_399_998),
            gv_vote_ix(&env, &ve, &alice.pubkey(), &gv_proposal, 2),
        ],
        Some(&payer.pubkey()),
        &[&payer, &alice],
        held_blockhash,
    );

    let first_back = gv_vote_ix(&env, &ve, &alice.pubkey(), &gv_proposal, 1);
    env.svm
        .send_transaction(Transaction::new_signed_with_payer(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(1_399_997),
                first_back,
            ],
            Some(&payer.pubkey()),
            &[&payer, &alice],
            held_blockhash,
        ))
        .expect("alice creates and backs her first ballot");
    let first_support = gv_proposal_support(&env, &gv_proposal);
    assert_eq!(first_support.1, amount);

    env.warp_slot(bootstrap_end);
    assert!(
        env.svm.send_transaction(withheld_old_exact_retract).is_err(),
        "the prior nonce-only wire must not act on a later first ballot"
    );
    assert!(
        env.svm.send_transaction(withheld_legacy_retract).is_err(),
        "the predecessor action-only wire must not act on a later first ballot"
    );
    assert!(
        env.svm
            .send_transaction(withheld_position_bound_retract)
            .is_err(),
        "the first vote's position-lock transition must stale a pre-ballot retract"
    );
    assert_eq!(
        gv_proposal_support(&env, &gv_proposal),
        first_support,
        "the first vote remains counted after the pre-ballot replay"
    );
    gv_trigger_now(&mut env, &ve, &gv_proposal, &dist_proposal)
        .expect("the intact first vote seals the distribution");
}

// POSITION-INCARNATION RETRACT REPLAY: topping up a live ballot intentionally leaves its counted
// contribution unchanged. A retract signed before that top-up must not remain valid against the
// later Subledger position, especially when deposits and backing share a final admissible slot.
#[test]
fn presigned_retract_cannot_cross_a_later_position_top_up() {
    let start = 100u64;
    let delay = 10u64;
    let mut env = Env::new_for_policy_with_bootstrap_schedule(
        POLICY_PRINCIPAL,
        delay,
        start,
        delay,
    );
    env.init_insurance_pool_policy_with_schedule(POLICY_PRINCIPAL, Some(delay), Some(start));
    let ve = setup_vote(&mut env);
    let (dist_proposal, gv_proposal) =
        create_and_register_proposal(&mut env, &ve, 1, &Pubkey::new_unique());

    let (alice, alice_ata) = new_depositor(&mut env, 6);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, 5)
        .expect("alice deposits five voting units");
    env.warp_slot(start + 1);
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("alice backs five units");
    let original_support = gv_proposal_support(&env, &gv_proposal);
    assert_eq!(original_support.1, 5);

    env.warp_slot(start + delay - 1);
    env.svm.expire_blockhash();
    let held_blockhash = env.svm.latest_blockhash();
    let payer = clone_kp(&env.payer);
    let ballot = Pubkey::find_program_address(
        &[
            b"gv_ballot",
            ve.gv_config.as_ref(),
            alice.pubkey().as_ref(),
        ],
        &gv_id(),
    )
    .0;
    let vote_nonce = env.svm.get_account(&ballot).unwrap().data[96..104].to_vec();
    let mut old_exact_retract = legacy_gv_vote_ix(&env, &ve, &alice.pubkey(), &gv_proposal, 2);
    old_exact_retract.data.extend_from_slice(&vote_nonce);
    let withheld_old_exact = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            old_exact_retract,
        ],
        Some(&payer.pubkey()),
        &[&payer, &alice],
        held_blockhash,
    );
    let withheld_position_bound = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_399_999),
            gv_vote_ix(&env, &ve, &alice.pubkey(), &gv_proposal, 2),
        ],
        Some(&payer.pubkey()),
        &[&payer, &alice],
        held_blockhash,
    );

    let mut top_up_data = vec![4u8];
    top_up_data.extend_from_slice(&1u64.to_le_bytes());
    env.append_deposit_position_snapshot(&mut top_up_data, &alice.pubkey());
    let top_up = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(alice.pubkey(), true),
            AccountMeta::new(env.pool, false),
            AccountMeta::new(env.position_pda(&alice.pubkey()), false),
            AccountMeta::new(alice_ata, false),
            AccountMeta::new(holding, false),
            AccountMeta::new(env.slab, false),
            AccountMeta::new(env.perc_vault, false),
            AccountMeta::new_readonly(perc_id(), false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: top_up_data,
    };
    env.svm
        .send_transaction(Transaction::new_signed_with_payer(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(1_399_998),
                top_up,
            ],
            Some(&payer.pubkey()),
            &[&payer, &alice],
            held_blockhash,
        ))
        .expect("alice tops up in the final deposit slot");
    assert_eq!(
        gv_proposal_support(&env, &gv_proposal),
        original_support,
        "a top-up does not silently change the live ballot"
    );

    env.warp_slot(start + delay);
    assert!(
        env.svm.send_transaction(withheld_old_exact).is_err(),
        "the prior ballot-only wire cannot retract a later position incarnation"
    );
    assert!(
        env.svm.send_transaction(withheld_position_bound).is_err(),
        "the current wire must reject the stale position snapshot"
    );
    assert_eq!(gv_proposal_support(&env, &gv_proposal), original_support);
    gv_trigger_now(&mut env, &ve, &gv_proposal, &dist_proposal)
        .expect("the intact five-of-six support still seals the distribution");
}

// PRESIGNED BACK REPLAY: after a voter lands and then retracts a vote, a relayer must not be able
// to restore that withdrawn support with an older signature in the final admissible backing slot.
// Otherwise the relayer can trigger a distribution the voter explicitly stopped supporting.
#[test]
fn presigned_back_cannot_restore_a_retracted_vote_at_the_bootstrap_deadline() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);
    let amount = 1_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("alice deposits");
    let (dist_proposal, gv_proposal) =
        create_and_register_proposal(&mut env, &ve, 1, &Pubkey::new_unique());

    let bootstrap_end = env.bootstrap_end_slot();
    env.warp_slot(bootstrap_end - 4);
    env.svm.expire_blockhash();
    let held_blockhash = env.svm.latest_blockhash();
    let payer = clone_kp(&env.payer);
    let exact_back = gv_vote_ix(&env, &ve, &alice.pubkey(), &gv_proposal, 1);
    let withheld_exact_back = Transaction::new_signed_with_payer(
        &[ComputeBudgetInstruction::set_compute_unit_limit(1_400_000), exact_back.clone()],
        Some(&payer.pubkey()),
        &[&payer, &alice],
        held_blockhash,
    );
    let withheld_legacy_back = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            legacy_gv_vote_ix(&env, &ve, &alice.pubkey(), &gv_proposal, 1),
        ],
        Some(&payer.pubkey()),
        &[&payer, &alice],
        held_blockhash,
    );
    env.svm
        .send_transaction(Transaction::new_signed_with_payer(
            &[ComputeBudgetInstruction::set_compute_unit_limit(1_399_999), exact_back],
            Some(&payer.pubkey()),
            &[&payer, &alice],
            held_blockhash,
        ))
        .expect("alice lands a distinct legitimate back");
    assert_eq!(gv_proposal_support(&env, &gv_proposal).1, amount);

    let exact_retract = gv_vote_ix(&env, &ve, &alice.pubkey(), &gv_proposal, 2);
    env.svm
        .send_transaction(Transaction::new_signed_with_payer(
            &[ComputeBudgetInstruction::set_compute_unit_limit(1_399_998), exact_retract],
            Some(&payer.pubkey()),
            &[&payer, &alice],
            held_blockhash,
        ))
        .expect("alice retracts the vote and releases its support");
    assert_eq!(gv_proposal_support(&env, &gv_proposal), (0, 0));

    env.warp_slot(bootstrap_end - 1);
    assert!(
        env.svm.send_transaction(withheld_exact_back).is_err(),
        "an exact back signature must be bound to the ballot incarnation it observed"
    );
    assert!(
        env.svm.send_transaction(withheld_legacy_back).is_err(),
        "a nonce-bearing ballot must reject a predecessor action-only back"
    );
    assert_eq!(
        gv_proposal_support(&env, &gv_proposal),
        (0, 0),
        "neither stale authorization restores withdrawn support"
    );

    let fresh_back = gv_vote_ix(&env, &ve, &alice.pubkey(), &gv_proposal, 1);
    env.svm
        .send_transaction(Transaction::new_signed_with_payer(
            &[ComputeBudgetInstruction::set_compute_unit_limit(1_399_997), fresh_back],
            Some(&payer.pubkey()),
            &[&payer, &alice],
            held_blockhash,
        ))
        .expect("a back signed for the current ballot incarnation remains live");
    assert_eq!(gv_proposal_support(&env, &gv_proposal).1, amount);

    env.warp_slot(bootstrap_end);
    gv_trigger_now(&mut env, &ve, &gv_proposal, &dist_proposal)
        .expect("only the freshly authorized support seals the proposal");
    assert_eq!(env.svm.get_account(&gv_proposal).unwrap().data[96], 1);
}

// PRINCIPAL-UNBOUND BACK REPLAY: a back authorization must commit to the principal
// it counts. Otherwise a relayer can hold a signed re-back, let the owner top up a
// live vote, then land the old authorization in the final deposit/backing slot and
// turn support the owner authorized for one atom into support for the larger balance.
#[test]
fn presigned_back_cannot_count_a_later_top_up_at_the_bootstrap_deadline() {
    let start = 100u64;
    let delay = 10u64;
    let mut env = Env::new_for_policy_with_bootstrap_schedule(
        POLICY_PRINCIPAL,
        delay,
        start,
        delay,
    );
    env.init_insurance_pool_policy_with_schedule(POLICY_PRINCIPAL, Some(delay), Some(start));
    let ve = setup_vote(&mut env);
    let (dist_proposal, gv_proposal) =
        create_and_register_proposal(&mut env, &ve, 1, &Pubkey::new_unique());

    let (alice, alice_ata) = new_depositor(&mut env, 5);
    let (bob, bob_ata) = new_depositor(&mut env, 2);
    let pool = env.pool;
    let alice_holding = create_holding(&mut env, &pool);
    let bob_holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &alice_holding, 1)
        .expect("alice deposits one authorized vote unit");
    env.insurance_deposit(&bob, &bob_ata, &bob_holding, 2)
        .expect("bob supplies the nonvoting quorum denominator");

    env.warp_slot(start + 1);
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("alice backs one unit");
    assert_eq!(gv_proposal_support(&env, &gv_proposal), (1, 1));

    env.warp_slot(start + delay - 1);
    env.svm.expire_blockhash();
    let held_blockhash = env.svm.latest_blockhash();
    let payer = clone_kp(&env.payer);
    let exact_back = gv_vote_ix(&env, &ve, &alice.pubkey(), &gv_proposal, 1);
    let withheld_exact_back = Transaction::new_signed_with_payer(
        &[ComputeBudgetInstruction::set_compute_unit_limit(1_400_000), exact_back],
        Some(&payer.pubkey()),
        &[&payer, &alice],
        held_blockhash,
    );
    let withheld_legacy_back = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_399_999),
            legacy_gv_vote_ix(&env, &ve, &alice.pubkey(), &gv_proposal, 1),
        ],
        Some(&payer.pubkey()),
        &[&payer, &alice],
        held_blockhash,
    );

    // Land a distinct top-up transaction with the same live blockhash. Deposits
    // are still open in this final slot, but depositing is deliberately not voting.
    let mut top_up_data = vec![4u8];
    top_up_data.extend_from_slice(&4u64.to_le_bytes());
    env.append_deposit_position_snapshot(&mut top_up_data, &alice.pubkey());
    let top_up = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(alice.pubkey(), true),
            AccountMeta::new(env.pool, false),
            AccountMeta::new(env.position_pda(&alice.pubkey()), false),
            AccountMeta::new(alice_ata, false),
            AccountMeta::new(alice_holding, false),
            AccountMeta::new(env.slab, false),
            AccountMeta::new(env.perc_vault, false),
            AccountMeta::new_readonly(perc_id(), false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: top_up_data,
    };
    env.svm
        .send_transaction(Transaction::new_signed_with_payer(
            &[ComputeBudgetInstruction::set_compute_unit_limit(1_399_998), top_up],
            Some(&payer.pubkey()),
            &[&payer, &alice],
            held_blockhash,
        ))
        .expect("the final-slot top-up lands");
    assert_eq!(env.read_position(&alice.pubkey()).0, 5);
    assert_eq!(
        gv_proposal_support(&env, &gv_proposal),
        (1, 1),
        "depositing fresh principal does not authorize fresh support",
    );

    assert!(
        env.svm.send_transaction(withheld_legacy_back).is_err(),
        "a principal-unbound predecessor back must not count the top-up",
    );
    assert!(
        env.svm.send_transaction(withheld_exact_back).is_err(),
        "a back signed for the old principal must not count the top-up",
    );
    assert_eq!(
        gv_proposal_support(&env, &gv_proposal),
        (1, 1),
        "both stale authorizations leave the original vote unchanged",
    );

    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1)
        .expect("a fresh final-slot re-back can authorize all five units");
    assert_eq!(gv_proposal_support(&env, &gv_proposal), (5, 5));

    env.warp_slot(start + delay);
    gv_trigger_now(&mut env, &ve, &gv_proposal, &dist_proposal)
        .expect("only the freshly authorized quorum seals the distribution");
    assert_eq!(env.svm.get_account(&gv_proposal).unwrap().data[96], 1);
}

// STALE FULL-EXIT REPLAY: neither the legacy amountless wire nor a current
// snapshot-bound owner signature may cross a later deposit incarnation. Otherwise a
// relayer can hold an exit authorization, wait for a final-slot top-up, and land it
// after deposits close. The owner receives the tokens but permanently loses the
// Genesis vote and reward opportunity because a retired position cannot be re-created.
#[test]
fn presigned_full_exit_cannot_retire_a_later_top_up_after_deposits_close() {
    let start = 100u64;
    let deposit_window = 10u64;
    let bootstrap_delay = 100u64;
    let mut env = Env::new_for_policy_with_bootstrap_schedule(
        POLICY_PRINCIPAL,
        deposit_window,
        start,
        bootstrap_delay,
    );
    env.init_insurance_pool_policy_with_schedule(
        POLICY_PRINCIPAL,
        Some(deposit_window),
        Some(start),
    );
    let ve = setup_vote(&mut env);
    let (_, gv_proposal) =
        create_and_register_proposal(&mut env, &ve, 1, &Pubkey::new_unique());

    let (alice, alice_ata) = new_depositor(&mut env, 5);
    let (bob, bob_ata) = new_depositor(&mut env, 5);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    let bob_holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, 1)
        .expect("alice deposits the principal covered by the first exit signature");
    env.insurance_deposit(&bob, &bob_ata, &bob_holding, 5)
        .expect("bob deposits the principal covered by the incarnation signature");
    let alice_snapshot = env.read_position(&alice.pubkey());
    let bob_snapshot = env.read_position(&bob.pubkey());
    let alice_action_nonce = env.position_action_nonce(&alice.pubkey());
    let bob_action_nonce = env.position_action_nonce(&bob.pubkey());
    assert_eq!(alice_snapshot, (1, start + 1, false));
    assert_eq!(bob_snapshot, (5, start + 1, false));

    env.warp_slot(start + deposit_window - 1);
    env.svm.expire_blockhash();
    let held_blockhash = env.svm.latest_blockhash();
    let payer = clone_kp(&env.payer);
    let alice_full_exit_accounts = vec![
        AccountMeta::new(alice.pubkey(), true),
        AccountMeta::new(env.pool, false),
        AccountMeta::new(env.position_pda(&alice.pubkey()), false),
        AccountMeta::new(alice_ata, false),
        AccountMeta::new(holding, false),
        AccountMeta::new(env.slab, false),
        AccountMeta::new(env.perc_vault, false),
        AccountMeta::new_readonly(env.vault_authority, false),
        AccountMeta::new_readonly(perc_id(), false),
        AccountMeta::new_readonly(spl_token::ID, false),
    ];
    let legacy_full_exit = Instruction {
        program_id: sub_id(),
        accounts: alice_full_exit_accounts.clone(),
        data: vec![13u8], // IX_INSURANCE_WITHDRAW_FULL, signed for one live unit
    };
    let withheld_legacy_full_exit = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            legacy_full_exit,
        ],
        Some(&payer.pubkey()),
        &[&payer, &alice],
        held_blockhash,
    );
    let mut alice_exact_exit_data = vec![13u8]; // IX_INSURANCE_WITHDRAW_FULL
    alice_exact_exit_data.extend_from_slice(&alice_snapshot.0.to_le_bytes());
    alice_exact_exit_data.extend_from_slice(&alice_snapshot.1.to_le_bytes());
    alice_exact_exit_data.extend_from_slice(&alice_action_nonce.to_le_bytes());
    let withheld_alice_exact_exit = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_399_998),
            Instruction {
                program_id: sub_id(),
                accounts: alice_full_exit_accounts,
                data: alice_exact_exit_data,
            },
        ],
        Some(&payer.pubkey()),
        &[&payer, &alice],
        held_blockhash,
    );

    let mut bob_exact_exit_data = vec![13u8]; // IX_INSURANCE_WITHDRAW_FULL
    bob_exact_exit_data.extend_from_slice(&bob_snapshot.0.to_le_bytes());
    bob_exact_exit_data.extend_from_slice(&bob_snapshot.1.to_le_bytes());
    bob_exact_exit_data.extend_from_slice(&bob_action_nonce.to_le_bytes());
    let withheld_bob_exact_exit = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_399_997),
            Instruction {
                program_id: sub_id(),
                accounts: vec![
                    AccountMeta::new(bob.pubkey(), true),
                    AccountMeta::new(env.pool, false),
                    AccountMeta::new(env.position_pda(&bob.pubkey()), false),
                    AccountMeta::new(bob_ata, false),
                    AccountMeta::new(bob_holding, false),
                    AccountMeta::new(env.slab, false),
                    AccountMeta::new(env.perc_vault, false),
                    AccountMeta::new_readonly(env.vault_authority, false),
                    AccountMeta::new_readonly(perc_id(), false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                ],
                data: bob_exact_exit_data,
            },
        ],
        Some(&payer.pubkey()),
        &[&payer, &bob],
        held_blockhash,
    );

    let mut top_up_data = vec![4u8]; // IX_INSURANCE_DEPOSIT
    top_up_data.extend_from_slice(&4u64.to_le_bytes());
    env.append_deposit_position_snapshot(&mut top_up_data, &alice.pubkey());
    let top_up = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(alice.pubkey(), true),
            AccountMeta::new(env.pool, false),
            AccountMeta::new(env.position_pda(&alice.pubkey()), false),
            AccountMeta::new(alice_ata, false),
            AccountMeta::new(holding, false),
            AccountMeta::new(env.slab, false),
            AccountMeta::new(env.perc_vault, false),
            AccountMeta::new_readonly(perc_id(), false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: top_up_data,
    };
    env.svm
        .send_transaction(Transaction::new_signed_with_payer(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(1_399_999),
                top_up,
            ],
            Some(&payer.pubkey()),
            &[&payer, &alice],
            held_blockhash,
        ))
        .expect("alice's final-slot top-up lands");

    let mut bob_withdraw_data = vec![5u8]; // IX_INSURANCE_WITHDRAW
    bob_withdraw_data.extend_from_slice(&1u64.to_le_bytes());
    bob_withdraw_data.extend_from_slice(&bob_snapshot.0.to_le_bytes());
    bob_withdraw_data.extend_from_slice(&bob_snapshot.1.to_le_bytes());
    bob_withdraw_data.extend_from_slice(&bob_action_nonce.to_le_bytes());
    let bob_withdraw = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(bob.pubkey(), true),
            AccountMeta::new(env.pool, false),
            AccountMeta::new(env.position_pda(&bob.pubkey()), false),
            AccountMeta::new(bob_ata, false),
            AccountMeta::new(bob_holding, false),
            AccountMeta::new(env.slab, false),
            AccountMeta::new(env.perc_vault, false),
            AccountMeta::new_readonly(env.vault_authority, false),
            AccountMeta::new_readonly(perc_id(), false),
            AccountMeta::new_readonly(spl_token::ID, false),
        ],
        data: bob_withdraw_data,
    };
    let mut bob_redeposit_data = vec![4u8]; // IX_INSURANCE_DEPOSIT
    bob_redeposit_data.extend_from_slice(&1u64.to_le_bytes());
    bob_redeposit_data.extend_from_slice(&(bob_snapshot.0 - 1).to_le_bytes());
    bob_redeposit_data.extend_from_slice(&bob_snapshot.1.to_le_bytes());
    bob_redeposit_data.extend_from_slice(&(bob_action_nonce + 1).to_le_bytes());
    let bob_redeposit = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(bob.pubkey(), true),
            AccountMeta::new(env.pool, false),
            AccountMeta::new(env.position_pda(&bob.pubkey()), false),
            AccountMeta::new(bob_ata, false),
            AccountMeta::new(bob_holding, false),
            AccountMeta::new(env.slab, false),
            AccountMeta::new(env.perc_vault, false),
            AccountMeta::new_readonly(perc_id(), false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: bob_redeposit_data,
    };
    env.svm
        .send_transaction(Transaction::new_signed_with_payer(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(1_399_996),
                bob_withdraw,
                bob_redeposit,
            ],
            Some(&payer.pubkey()),
            &[&payer, &bob],
            held_blockhash,
        ))
        .expect("bob replaces one unit in the final slot without changing principal");
    assert_eq!(env.read_position(&alice.pubkey()), (5, 109, false));
    assert_eq!(env.read_position(&bob.pubkey()), (5, 109, false));
    assert_eq!(env.token_amount(&alice_ata), 0);
    assert_eq!(env.token_amount(&bob_ata), 0);

    env.warp_slot(start + deposit_window);
    let stale_result = env.svm.send_transaction(withheld_legacy_full_exit);
    if stale_result.is_ok() {
        assert_eq!(
            env.read_position(&alice.pubkey()),
            (0, 109, true),
            "the vulnerable wire retires all five units",
        );
        assert_eq!(
            env.token_amount(&alice_ata),
            5,
            "the stale exit returns funds but forfeits Genesis participation",
        );
        assert!(
            gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).is_err(),
            "a retired position cannot cast the lost post-cutoff vote",
        );
        panic!(
            "an exit signed before the top-up retired the larger position after deposits closed"
        );
    }
    assert!(
        env.svm
            .send_transaction(withheld_alice_exact_exit)
            .is_err(),
        "the current wire rejects a stale principal and deposit-slot snapshot",
    );
    assert!(
        env.svm
            .send_transaction(withheld_bob_exact_exit)
            .is_err(),
        "the current wire rejects a stale deposit slot even when principal is unchanged",
    );
    assert_eq!(
        env.read_position(&alice.pubkey()),
        (5, 109, false),
        "the stale authorization leaves all five vote units live",
    );
    assert_eq!(
        env.read_position(&bob.pubkey()),
        (5, 109, false),
        "the stale incarnation authorization leaves bob's vote units live",
    );
    assert_eq!(env.token_amount(&alice_ata), 0);
    assert_eq!(env.token_amount(&bob_ata), 0);

    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1)
        .expect("the owner retains the post-cutoff Genesis vote opportunity");
    gv_vote(&mut env, &ve, &bob, &gv_proposal, 1)
        .expect("the same-principal redepositor retains the Genesis vote opportunity");
    assert_eq!(gv_proposal_support(&env, &gv_proposal), (10, 10));
}

// STALE PARTIAL-EXIT REPLAY: an amount-only withdrawal signed against ten units
// must not consume the five-unit remainder after a replacement withdrawal lands.
// Otherwise a relayer can turn one authorized partial exit into a terminal exit
// after deposits close, permanently deleting the owner's Genesis participation.
#[test]
fn presigned_partial_withdraw_cannot_retire_the_remainder_after_deposits_close() {
    let start = 100u64;
    let deposit_window = 10u64;
    let bootstrap_delay = 100u64;
    let mut env = Env::new_for_policy_with_bootstrap_schedule(
        POLICY_PRINCIPAL,
        deposit_window,
        start,
        bootstrap_delay,
    );
    env.init_insurance_pool_policy_with_schedule(
        POLICY_PRINCIPAL,
        Some(deposit_window),
        Some(start),
    );
    let ve = setup_vote(&mut env);
    let (_, gv_proposal) =
        create_and_register_proposal(&mut env, &ve, 1, &Pubkey::new_unique());

    let (alice, alice_ata) = new_depositor(&mut env, 10);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, 10)
        .expect("alice deposits ten vote units");
    let initial_snapshot = env.read_position(&alice.pubkey());
    let initial_action_nonce = env.position_action_nonce(&alice.pubkey());
    assert_eq!(initial_snapshot, (10, start + 1, false));

    env.warp_slot(start + deposit_window - 1);
    env.svm.expire_blockhash();
    let held_blockhash = env.svm.latest_blockhash();
    let payer = clone_kp(&env.payer);
    let accounts = vec![
        AccountMeta::new(alice.pubkey(), true),
        AccountMeta::new(env.pool, false),
        AccountMeta::new(env.position_pda(&alice.pubkey()), false),
        AccountMeta::new(alice_ata, false),
        AccountMeta::new(holding, false),
        AccountMeta::new(env.slab, false),
        AccountMeta::new(env.perc_vault, false),
        AccountMeta::new_readonly(env.vault_authority, false),
        AccountMeta::new_readonly(perc_id(), false),
        AccountMeta::new_readonly(spl_token::ID, false),
    ];
    let mut withdraw_data = vec![5u8]; // IX_INSURANCE_WITHDRAW
    withdraw_data.extend_from_slice(&5u64.to_le_bytes());
    withdraw_data.extend_from_slice(&initial_snapshot.0.to_le_bytes());
    withdraw_data.extend_from_slice(&initial_snapshot.1.to_le_bytes());
    withdraw_data.extend_from_slice(&initial_action_nonce.to_le_bytes());
    let withheld = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            Instruction {
                program_id: sub_id(),
                accounts: accounts.clone(),
                data: withdraw_data.clone(),
            },
        ],
        Some(&payer.pubkey()),
        &[&payer, &alice],
        held_blockhash,
    );
    let replacement = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_399_999),
            Instruction {
                program_id: sub_id(),
                accounts,
                data: withdraw_data,
            },
        ],
        Some(&payer.pubkey()),
        &[&payer, &alice],
        held_blockhash,
    );

    env.svm
        .send_transaction(replacement)
        .expect("the replacement five-unit withdrawal lands");
    assert_eq!(env.read_position(&alice.pubkey()), (5, start + 1, false));
    assert_eq!(env.token_amount(&alice_ata), 5);

    env.warp_slot(start + deposit_window);
    let stale_result = env.svm.send_transaction(withheld);
    if stale_result.is_ok() {
        assert_eq!(env.read_position(&alice.pubkey()), (0, start + 1, true));
        assert_eq!(env.token_amount(&alice_ata), 10);
        assert!(
            gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).is_err(),
            "the stale partial exit permanently removes the post-cutoff vote",
        );
        panic!("one partial-withdraw authorization retired the replacement remainder");
    }

    assert_eq!(env.read_position(&alice.pubkey()), (5, start + 1, false));
    assert_eq!(env.token_amount(&alice_ata), 5);
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1)
        .expect("rejecting the stale withdrawal preserves five vote units");
    assert_eq!(gv_proposal_support(&env, &gv_proposal), (5, 5));
}

#[test]
fn same_slot_round_trip_cannot_restore_a_stale_withdrawal_snapshot() {
    let start = 100u64;
    let mut env = Env::new_for_policy_with_bootstrap_schedule(
        POLICY_PRINCIPAL,
        10,
        start,
        100,
    );
    env.init_insurance_pool_policy_with_schedule(POLICY_PRINCIPAL, Some(10), Some(start));
    let (alice, alice_ata) = new_depositor(&mut env, 20);
    let pool = env.pool;
    let position = env.position_pda(&alice.pubkey());
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, 10)
        .expect("initial same-slot deposit");
    let initial_snapshot = env.read_position(&alice.pubkey());
    let initial_action_nonce = env.position_action_nonce(&alice.pubkey());
    assert_eq!(initial_snapshot, (10, start + 1, false));

    let withdraw_accounts = vec![
        AccountMeta::new(alice.pubkey(), true),
        AccountMeta::new(pool, false),
        AccountMeta::new(position, false),
        AccountMeta::new(alice_ata, false),
        AccountMeta::new(holding, false),
        AccountMeta::new(env.slab, false),
        AccountMeta::new(env.perc_vault, false),
        AccountMeta::new_readonly(env.vault_authority, false),
        AccountMeta::new_readonly(perc_id(), false),
        AccountMeta::new_readonly(spl_token::ID, false),
    ];
    let mut withdraw_data = vec![5u8]; // IX_INSURANCE_WITHDRAW
    withdraw_data.extend_from_slice(&5u64.to_le_bytes());
    withdraw_data.extend_from_slice(&initial_snapshot.0.to_le_bytes());
    withdraw_data.extend_from_slice(&initial_snapshot.1.to_le_bytes());
    withdraw_data.extend_from_slice(&initial_action_nonce.to_le_bytes());

    env.svm.expire_blockhash();
    let held_blockhash = env.svm.latest_blockhash();
    let payer = clone_kp(&env.payer);
    let withheld = Transaction::new_signed_with_payer(
        &[Instruction {
            program_id: sub_id(),
            accounts: withdraw_accounts.clone(),
            data: withdraw_data.clone(),
        }],
        Some(&payer.pubkey()),
        &[&payer, &alice],
        held_blockhash,
    );

    let mut redeposit_data = vec![4u8]; // IX_INSURANCE_DEPOSIT
    redeposit_data.extend_from_slice(&5u64.to_le_bytes());
    redeposit_data.extend_from_slice(&(initial_snapshot.0 - 5).to_le_bytes());
    redeposit_data.extend_from_slice(&initial_snapshot.1.to_le_bytes());
    redeposit_data.extend_from_slice(&(initial_action_nonce + 1).to_le_bytes());
    let redeposit = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(alice.pubkey(), true),
            AccountMeta::new(pool, false),
            AccountMeta::new(position, false),
            AccountMeta::new(alice_ata, false),
            AccountMeta::new(holding, false),
            AccountMeta::new(env.slab, false),
            AccountMeta::new(env.perc_vault, false),
            AccountMeta::new_readonly(perc_id(), false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: redeposit_data,
    };
    env.svm
        .send_transaction(Transaction::new_signed_with_payer(
            &[
                Instruction {
                    program_id: sub_id(),
                    accounts: withdraw_accounts,
                    data: withdraw_data,
                },
                redeposit,
            ],
            Some(&payer.pubkey()),
            &[&payer, &alice],
            held_blockhash,
        ))
        .expect("same-slot withdraw and redeposit restores the visible balance");
    assert_eq!(
        env.read_position(&alice.pubkey()),
        initial_snapshot,
        "principal and deposit slot alone repeat after the round trip",
    );
    assert_eq!(
        env.position_action_nonce(&alice.pubkey()),
        initial_action_nonce + 2,
    );
    assert!(
        env.svm.send_transaction(withheld).is_err(),
        "the action nonce invalidates the otherwise identical stale snapshot",
    );

    env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, 5)
        .expect("a fresh nonce-bound partial withdrawal remains live");
    assert_eq!(env.read_position(&alice.pubkey()), (5, start + 1, false));
    assert_eq!(env.token_amount(&alice_ata), 15);
}

// DEPOSIT != VOTE (top-up while a ballot is LIVE must not inflate the tally nor unlock the pledge):
// insurance_deposit checks p.withdrawn but NOT vote_locked, and it never touches the gv tallies (those are
// gv-owned state). So a voter may add capital while voted, but doing so must (a) NOT silently raise their
// counted weight/principal — that would inject vote power bypassing vote's exact backout path — and
// (b) NOT unlock the at-risk capital. The only way to count fresh capital is a re-vote.
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
    env.warp_slot(1124);
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("vote");
    assert_eq!(gv_proposal_support(&env, &gv_proposal), (amount, amount), "vote counted once");
    assert!(env.svm.get_account(&env.position_pda(&alice.pubkey())).unwrap().data[97] == 1, "position vote-locked");

    // TOP UP while the ballot is live (deposit ignores the lock) — allowed (no DOS on adding capital).
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("top-up while voted is allowed");

    // (a) The tally is UNCHANGED — the extra capital is NOT counted until a re-vote.
    assert_eq!(gv_proposal_support(&env, &gv_proposal), (amount, amount),
        "top-up must NOT inflate the live vote — deposit is not a vote");
    // (b) The position grew but stays LOCKED: the pledged capital can't be exited without retracting.
    let (principal, _start, withdrawn) = env.read_position(&alice.pubkey());
    assert_eq!(principal, 2 * amount, "top-up landed (principal grew)");
    assert!(!withdrawn);
    assert!(env.svm.get_account(&env.position_pda(&alice.pubkey())).unwrap().data[97] == 1,
        "top-up did NOT unlock the live vote — capital still pledged");
    assert!(env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, amount).is_err(),
        "the topped-up, still-voted position cannot exit until the vote is retracted");

    // Retraction must back out the ORIGINAL ballot snapshot, not the now-larger
    // position. It must also release the current position in full; otherwise a
    // legal top-up while voted would permanently lock the added principal.
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 2).expect("retract after top-up");
    assert_eq!(gv_proposal_support(&env, &gv_proposal), (0, 0),
        "retract backs out the stored ballot contribution exactly");
    assert!(env.svm.get_account(&env.position_pda(&alice.pubkey())).unwrap().data[97] == 0,
        "retract releases the topped-up position");
    env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, 2 * amount)
        .expect("recover the entire topped-up principal after retracting");
    assert_eq!(env.token_amount(&alice_ata), 2 * amount,
        "a stale ballot snapshot cannot strand the top-up or original deposit");
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
    assert_eq!(support_weight, amount, "alice's weight counts — she was not silenced");
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

    // SUBTLER VARIANT (weight-per-capital inflation): a PARTIAL withdraw is ALSO blocked. The lock guards on the
    // flag (lib.rs:1176), BEFORE the amount check (:1181), so a voter cannot shrink their capital-at-risk (here
    // to half) while the ballot keeps counting their FULL recorded principal/weight. If the lock were
    // amount-based, this would pass the full-withdraw assertion above yet let a 0.9M-withdrawn voter keep a 1M
    // ballot — 10x weight-per-capital, cheapening quorum/majority manipulation. Pin that partial is refused too.
    assert!(env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, amount / 2).is_err(),
        "a PARTIAL vote-locked withdraw is ALSO rejected — capital-at-risk cannot drop below the voted principal");
    let (principal_after, _s2, withdrawn_after) = env.read_position(&alice.pubkey());
    assert_eq!(principal_after, amount, "position principal unchanged by the rejected partial withdraw");
    assert!(!withdrawn_after, "no partial withdrawal was recorded");
    assert_eq!(env.token_amount(&env.perc_vault.clone()), amount, "full capital still at risk behind the ballot");

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

fn exercise_legacy_genesis_retract(config_size: usize, pool_bound: bool, label: &str) {
    let mut env = Env::new();
    env.init_insurance_pool();

    let amount = 1_000_000u64;
    let voted_weight = 7_000_000u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount).expect("deposit");

    let (legacy_config, legacy_config_bump) = if pool_bound {
        Pubkey::find_program_address(
            &[b"gv_config", env.coin_mint.as_ref(), pool.as_ref()],
            &gv_id(),
        )
    } else {
        Pubkey::find_program_address(&[b"gv_config", env.coin_mint.as_ref()], &gv_id())
    };
    let distribution_config = Pubkey::new_unique();
    let distribution_proposal = Pubkey::new_unique();
    let (legacy_proposal, _) = Pubkey::find_program_address(
        &[
            b"gv_proposal",
            legacy_config.as_ref(),
            distribution_proposal.as_ref(),
        ],
        &gv_id(),
    );
    let (legacy_ballot, _) = Pubkey::find_program_address(
        &[b"gv_ballot", legacy_config.as_ref(), alice.pubkey().as_ref()],
        &gv_id(),
    );

    let mut config_data = vec![0u8; config_size];
    config_data[..8].copy_from_slice(b"GVCONFG1");
    config_data[8..40].copy_from_slice(env.coin_mint.as_ref());
    config_data[40..72].copy_from_slice(dist_id().as_ref());
    config_data[72..104].copy_from_slice(distribution_config.as_ref());
    config_data[104..136].copy_from_slice(sub_id().as_ref());
    config_data[136..168].copy_from_slice(pool.as_ref());
    config_data[200..208].copy_from_slice(&amount.to_le_bytes());
    if config_size == 232 {
        config_data[208..216].copy_from_slice(&voted_weight.to_le_bytes());
        config_data[216..224].copy_from_slice(&amount.to_le_bytes());
        config_data[224] = legacy_config_bump;
    } else {
        config_data[208..224].copy_from_slice(&(voted_weight as u128).to_le_bytes());
        config_data[224..232].copy_from_slice(&amount.to_le_bytes());
        config_data[232] = legacy_config_bump;
        if config_size == 248 {
            config_data[233..241].copy_from_slice(&38_880_000u64.to_le_bytes());
        }
    }
    env.svm
        .set_account(
            legacy_config,
            Account {
                lamports: 1_000_000_000,
                data: config_data,
                owner: gv_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let ballot_size = if config_size == 232 { 112 } else { 120 };
    let contribution_end = if config_size == 232 { 88 } else { 96 };
    let mut ballot_data = vec![0u8; ballot_size];
    ballot_data[..8].copy_from_slice(b"GVBALOT1");
    ballot_data[8..40].copy_from_slice(alice.pubkey().as_ref());
    ballot_data[40..72].copy_from_slice(legacy_proposal.as_ref());
    if config_size == 232 {
        ballot_data[72..80].copy_from_slice(&voted_weight.to_le_bytes());
        ballot_data[80..88].copy_from_slice(&amount.to_le_bytes());
    } else {
        ballot_data[72..88].copy_from_slice(&(voted_weight as u128).to_le_bytes());
        ballot_data[88..96].copy_from_slice(&amount.to_le_bytes());
    }
    env.svm
        .set_account(
            legacy_ballot,
            Account {
                lamports: 1_000_000_000,
                data: ballot_data,
                owner: gv_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let proposal_size = if config_size == 232 { 104 } else { 112 };
    let mut proposal_data = vec![0u8; proposal_size];
    proposal_data[..8].copy_from_slice(b"GVPROPV1");
    proposal_data[8..40].copy_from_slice(legacy_config.as_ref());
    proposal_data[40..72].copy_from_slice(distribution_proposal.as_ref());
    if config_size == 232 {
        proposal_data[72..80].copy_from_slice(&voted_weight.to_le_bytes());
        proposal_data[80..88].copy_from_slice(&amount.to_le_bytes());
    } else {
        proposal_data[72..88].copy_from_slice(&(voted_weight as u128).to_le_bytes());
        proposal_data[88..96].copy_from_slice(&amount.to_le_bytes());
    }
    env.svm
        .set_account(
            legacy_proposal,
            Account {
                lamports: 1_000_000_000,
                data: proposal_data,
                owner: gv_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    // Preserve the exact cross-program state an old successful vote created.
    let mut pool_account = env.svm.get_account(&pool).unwrap();
    pool_account.data[160..192].copy_from_slice(legacy_config.as_ref());
    env.svm.set_account(pool, pool_account).unwrap();
    let position = env.position_pda(&alice.pubkey());
    let mut position_account = env.svm.get_account(&position).unwrap();
    position_account.data[97] = 1;
    env.svm.set_account(position, position_account).unwrap();

    let config_before = env.svm.get_account(&legacy_config).unwrap().data;
    let ballot_before = env.svm.get_account(&legacy_ballot).unwrap().data;
    let proposal_before = env.svm.get_account(&legacy_proposal).unwrap().data;
    let vote_ix = |voter: Pubkey, action: u8| Instruction {
        program_id: gv_id(),
        accounts: vec![
            AccountMeta::new(voter, true),
            AccountMeta::new(legacy_config, false),
            AccountMeta::new(legacy_ballot, false),
            AccountMeta::new(legacy_proposal, false),
            AccountMeta::new(position, false),
            AccountMeta::new_readonly(pool, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(sub_id(), false),
        ],
        data: vec![3u8, action],
    };

    // Compatibility never revives legacy governance: backing still rejects and
    // cannot mutate the old ballot or release its capital lock.
    assert!(env.send(&[vote_ix(alice.pubkey(), 1)], &[&alice]).is_err(), "{label}: backing stays disabled");
    assert_eq!(env.svm.get_account(&legacy_ballot).unwrap().data, ballot_before, "{label}: rejected backing is atomic");
    assert_eq!(env.svm.get_account(&position).unwrap().data[97], 1, "{label}: rejected backing keeps the lock");

    // A different signer cannot use the recovery path to alter Alice's ballot.
    let attacker = Keypair::new();
    env.svm.airdrop(&attacker.pubkey(), 1_000_000).unwrap();
    assert!(env.send(&[vote_ix(attacker.pubkey(), 2)], &[&attacker]).is_err(), "{label}: non-owner retract rejected");
    assert_eq!(env.svm.get_account(&legacy_ballot).unwrap().data, ballot_before, "{label}: non-owner changed no ballot state");
    assert_eq!(env.svm.get_account(&position).unwrap().data[97], 1, "{label}: non-owner cannot release the lock");

    env.send(&[vote_ix(alice.pubkey(), 2)], &[&alice])
        .unwrap_or_else(|e| panic!("{label}: legacy owner can always retract: {e}"));

    let ballot_after = env.svm.get_account(&legacy_ballot).unwrap().data;
    assert_eq!(&ballot_after[40..72], Pubkey::default().as_ref(), "live ballot cleared");
    assert!(ballot_after[40..contribution_end].iter().all(|byte| *byte == 0), "{label}: legacy contribution cleared");
    assert_eq!(
        env.svm.get_account(&position).unwrap().data[97],
        0,
        "subledger vote-lock released"
    );
    assert_eq!(
        env.svm.get_account(&legacy_config).unwrap().data,
        config_before,
        "inert legacy governance tallies are not revived or rewritten"
    );
    assert_eq!(
        env.svm.get_account(&legacy_proposal).unwrap().data,
        proposal_before,
        "inert legacy proposal tallies are not revived or rewritten"
    );
    assert!(env.send(&[vote_ix(alice.pubkey(), 2)], &[&alice]).is_err(), "{label}: cleared ballot cannot replay");
    assert_eq!(env.svm.get_account(&legacy_ballot).unwrap().data, ballot_after, "{label}: replay changed no state");
    assert_eq!(env.svm.get_account(&position).unwrap().data[97], 0, "{label}: replay cannot relock principal");

    env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, amount)
        .unwrap_or_else(|e| panic!("{label}: owner recovers the full deposit after legacy retract: {e}"));
    assert_eq!(env.token_amount(&alice_ata), amount, "{label}: principal returned");
    assert_eq!(env.token_amount(&env.perc_vault), 0, "{label}: insurance principal exited");
}

// UPGRADE-INDUCED PRINCIPAL LOCK: the vote-lock spans four exact historical
// config/PDA generations. Current decoders formerly rejected every one before the
// SetVoteLock(0) CPI, permanently preventing a signed owner from withdrawing.
#[test]
fn every_legacy_genesis_ballot_generation_can_retract_and_recover_real_percolator_principal() {
    for (config_size, pool_bound, label) in [
        (232, false, "232-byte coin-only"),
        (232, true, "232-byte pool-bound"),
        (240, true, "240-byte widened"),
        (248, true, "248-byte bootstrap-end"),
    ] {
        exercise_legacy_genesis_retract(config_size, pool_bound, label);
    }
}

// CROSS-TAG FORFEITURE BYPASS: a voter whose insurance principal is vote-locked is refused the
// insurance exit (tag 5, which checks vote_locked). The escape an attacker would reach for is the OTHER
// withdraw — the own-vault `process_withdraw` (tag 2), which has NO vote_locked check at all (locking is
// an insurance-only concept). If tag 2 could operate on an insurance pool it would skip the lock AND try
// to pay from the percolator insurance vault — letting a live ballot outlive its capital (finding B).
// process_withdraw's is_insurance type guard (lib.rs:670) rejects an insurance pool outright, BEFORE any
// vault/seed work, so the vote-lock cannot be sidestepped by routing the exit through a different tag.
// The existing vote_locked test only drives tag 5; this pins tag 2 as the forfeiture-bypass defense.
// Because every check preceding the type guard passes here (owner signs, token program and exact
// snapshot are valid, pool+position are subledger-owned), the rejection pins the is_insurance guard.
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
    let (snapshot_principal, snapshot_start, _) = env.read_position(&alice.pubkey());
    let mut tag2_data = vec![2u8];
    tag2_data.extend_from_slice(&snapshot_principal.to_le_bytes());
    tag2_data.extend_from_slice(&snapshot_start.to_le_bytes());
    tag2_data.extend_from_slice(&env.position_action_nonce(&alice.pubkey()).to_le_bytes());
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
        data: tag2_data,
    };
    assert!(env.send(&[tag2], &[&alice]).is_err(), "tag-2 own-vault withdraw must reject an insurance pool (is_insurance guard)");

    // Capital stayed at risk; the position is intact and still lockable/retractable as before.
    assert_eq!(env.token_amount(&env.perc_vault), amount, "insurance vault untouched");
    let (principal, _s, withdrawn) = env.read_position(&alice.pubkey());
    assert_eq!(principal, amount, "principal intact");
    assert!(!withdrawn, "not marked withdrawn");
}

// STALE-WEIGHT VOTING (vote-outlives-capital, the partial-withdraw facet of finding B). Vote weight is
// principal read from the LIVE subledger Position each call (genesis-vote never trusts
// the position's `withdrawn` flag — it relies on
// principal being eager-decremented by process_insurance_withdraw, lib.rs:1267). So a voter who reduces
// their at-risk capital must immediately get reduced voting power on a re-vote — they cannot keep voting
// with stale-high weight after pulling capital out. The full-withdraw case (principal -> 0 -> weight 0 ->
// rejected) has cannot_vote_with_a_withdrawn_position, but that drives the REAL tag-23 withdraw (red post-
// percolator-rebuild); the partial-withdraw RE-VOTE was untested. Here the post-withdraw position state
// (principal cut 10x, still active) is set directly so the REAL genesis-vote binary is exercised without
// the broken withdraw CPI. The only change between the two votes is principal.
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
    env.warp_slot(1124);
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("vote with full capital");
    let (w_full, p_full) = gv_proposal_support(&env, &gv_proposal);
    assert_eq!(p_full, 1_000_000, "support principal = full deposit");
    assert_eq!(w_full, 1_000_000, "weight equals the full live principal");

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

    // Re-vote: weight must reflect the live 100_000, not the stale 1M.
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("re-vote with reduced capital");
    let (w_live, p_live) = gv_proposal_support(&env, &gv_proposal);
    assert_eq!(p_live, 100_000, "re-vote records the LIVE principal, not the stale 1_000_000");
    assert_eq!(w_live, 100_000, "weight equals the reduced live principal");
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

    // Back A with the live principal.
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

    // (3) Back B -> B holds the weight, A stays zero, and the
    // globals equal exactly ONE contribution (not doubled).
    gv_vote(&mut env, &ve, &alice, &b_gv, 1).expect("alice backs B");
    assert_eq!(gv_proposal_support(&env, &b_gv), (a_w, a_p), "B now holds the full weight+principal");
    assert_eq!(gv_proposal_support(&env, &a_gv), (0, 0), "A still zero — no phantom support");
    assert_eq!(read_tallies(&env), (a_p, a_w as u128), "globals == ONE contribution, not double-counted");
    let _ = a_dist;
}

// GENESIS REQUIREMENT: a same-slot vote contributes exactly the live principal to both support and
// quorum, and atomically vote-locks that capital. The short deposit window controls admission;
// the lock prevents the counted principal from leaving while the ballot remains live.
#[test]
fn a_same_slot_vote_counts_exact_principal_and_locks_it() {
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

    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("same-slot principal votes");
    assert_eq!(
        gv_proposal_support(&env, &gv_proposal),
        (amount, amount),
        "support weight and support principal both equal live principal"
    );
    let config = env.svm.get_account(&ve.gv_config).unwrap();
    assert_eq!(
        u64::from_le_bytes(config.data[200..208].try_into().unwrap()),
        amount,
        "same-slot principal counts exactly once toward quorum"
    );
    assert_eq!(
        env.svm
            .get_account(&env.position_pda(&alice.pubkey()))
            .unwrap()
            .data[97],
        1,
        "the same transaction locks the counted principal"
    );
}

// FIRST-POST-GRANT-SLOT LIVENESS: a grant at slot 0 records a nonzero sentinel and deposits
// open at slot 1. Genesis must accept that earliest valid deposit without an extra delay.
#[test]
fn a_first_post_grant_slot_deposit_can_vote_immediately() {
    let mut env = Env::new();
    env.warp_slot(0);
    env.init_insurance_pool();
    let ve = setup_vote(&mut env);

    let amount = 1u64;
    let (alice, alice_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &holding, amount)
        .expect("first post-grant-slot deposit through real Percolator");
    assert_eq!(
        env.read_position(&alice.pubkey()),
        (amount, 1, false),
        "the public deposit path records the earliest valid post-grant timestamp"
    );

    let dest = Pubkey::new_unique();
    let (_dist_proposal, gv_proposal) =
        create_and_register_proposal(&mut env, &ve, 1, &dest);
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1)
        .expect("the first post-grant-slot position has immediate principal weight");
    assert_eq!(
        gv_proposal_support(&env, &gv_proposal),
        (amount, amount),
        "one deposited base unit gives one vote in the first post-grant slot"
    );
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
    data.extend_from_slice(&DISTRIBUTION_CLAIM_WINDOW_SLOTS.to_le_bytes());
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
    cd.extend_from_slice(&1u32.to_le_bytes());
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
    let mut env = Env::new_for_policy_with_bootstrap_schedule(POLICY_PRINCIPAL, 1_200, 0, 1_200);
    env.init_insurance_pool_policy_with_schedule(POLICY_PRINCIPAL, Some(1_200), Some(0));
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

// VETO-EXIT in ONE ATOMIC TRANSACTION (surface B): a depositor vetoes the genesis by RETRACTING their vote
// and WITHDRAWING their principal in a SINGLE tx [retract_ix, withdraw_ix]. The retract (gv vote action 2)
// CPIs the subledger SetVoteLock(0) to clear position.vote_locked; the withdraw (subledger tag 5) then checks
// position.vote_locked == false. For the one-tx veto to work, the lock-clear written by instruction 1 MUST be
// visible to instruction 2 within the same transaction (intra-tx account-state propagation), AND the gv tally
// must drop so quorum recomputes as the voter leaves. This pins that the veto is atomic — there is no two-step
// window where the voter is exposed (locked-but-not-yet-exited or exited-but-still-voting), and the lock is
// never a trap. Real gv + subledger + percolator .so.
#[test]
fn veto_exit_retract_and_withdraw_in_one_atomic_tx() {
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
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1).expect("alice votes (locks her position)");
    let voted_principal_before = gv_total_voted_principal(&env, &ve);
    assert_eq!(voted_principal_before, amount, "alice's principal is counted toward quorum while she votes");

    // CONTROL: a bare withdraw (no retract) is rejected — the position is vote-locked.
    assert!(env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, amount).is_err(),
        "a vote-locked position cannot exit without retracting first");

    // THE VETO-EXIT: retract + withdraw in ONE transaction.
    let retract_ix = gv_vote_ix(&env, &ve, &alice.pubkey(), &gv_proposal, 2);
    let (expected_principal, expected_start_slot, _) = env.read_position(&alice.pubkey());
    let expected_action_nonce = env.position_action_nonce(&alice.pubkey()).wrapping_add(1);
    let mut wdata = vec![5u8];
    wdata.extend_from_slice(&amount.to_le_bytes());
    wdata.extend_from_slice(&expected_principal.to_le_bytes());
    wdata.extend_from_slice(&expected_start_slot.to_le_bytes());
    wdata.extend_from_slice(&expected_action_nonce.to_le_bytes());
    let withdraw_ix = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(alice.pubkey(), true),
            AccountMeta::new(env.pool, false),
            AccountMeta::new(env.position_pda(&alice.pubkey()), false),
            AccountMeta::new(alice_ata, false),
            AccountMeta::new(holding, false),
            AccountMeta::new(env.slab, false),
            AccountMeta::new(env.perc_vault, false),
            AccountMeta::new_readonly(env.vault_authority, false),
            AccountMeta::new_readonly(perc_id(), false),
            AccountMeta::new_readonly(spl_token::ID, false),
        ],
        data: wdata,
    };
    env.send(&[retract_ix, withdraw_ix], &[&alice]).expect("atomic retract+withdraw veto-exit succeeds");

    // The lock-clear in ix 1 was visible to ix 2 -> principal recovered, AND alice's vote is gone (quorum
    // recomputes as she leaves): both halves of the veto landed atomically.
    assert_eq!(env.token_amount(&alice_ata), amount, "alice recovered her full principal in the same tx as the retract");
    assert_eq!(gv_total_voted_principal(&env, &ve), 0, "alice's vote was retracted — her principal no longer counts toward quorum");
    assert_eq!(env.pool_outstanding(), 0, "alice's principal left the pool outstanding accounting");
}

// SAME-MARKET INIT SQUAT: init_insurance_pool is permissionless and the canonical
// genesis pool PDA is first-writer-wins. A hostile vote_authority on the real pool
// would permanently consume that PDA; genesis-vote then refuses to bind because the
// pool does not point back at the canonical gv_config. The program must reject that
// init before creating the account.
#[test]
fn hostile_vote_authority_cannot_squat_the_genesis_pool() {
    let mut env = Env::new();
    let attacker = Keypair::new();

    // ATTACK: initialize the REAL genesis pool PDA for the REAL market, but bind
    // vote_authority to an attacker key instead of the canonical genesis-vote config.
    // If this succeeds, the pool PDA is consumed forever and genesis-vote InitConfig
    // rejects it later because pool.vote_authority != gv_config.
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
            AccountMeta::new_readonly(attacker.pubkey(), false),
            AccountMeta::new_readonly(env.coin_mint, false),
        ],
        data,
    };
    assert!(
        env.send(&[init], &[]).is_err(),
        "same-market genesis pool init must reject a noncanonical vote authority"
    );
    assert!(
        env.svm.get_account(&env.pool).map_or(true, |a| a.data.is_empty()),
        "the rejected hostile init must leave the canonical pool PDA free"
    );

    // ATTACK 2: use a fake COIN mint and its matching gv_config while still pointing
    // at the real pool PDA. This must also reject; otherwise a fake-coin squat could
    // pass the vote-authority check and consume the real pool address.
    let fake_coin = Pubkey::new_unique();
    let fake_vote_authority = gv_config_pda_for_schedule(
        &fake_coin,
        &env.pool,
        env.bootstrap_delay_slots,
        env.bootstrap_start_slot,
    );
    let mut data = vec![3u8]; // IX_INIT_INSURANCE_POOL
    data.extend_from_slice(&ASSET_ID.to_le_bytes());
    data.push(POLICY_PRINCIPAL);
    let fake_coin_init = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(env.mint, false),
            AccountMeta::new(env.pool, false),
            AccountMeta::new_readonly(env.perc_vault, false),
            AccountMeta::new_readonly(env.slab, false),
            AccountMeta::new_readonly(perc_id(), false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(fake_vote_authority, false),
            AccountMeta::new_readonly(fake_coin, false),
        ],
        data,
    };
    assert!(
        env.send(&[fake_coin_init], &[]).is_err(),
        "fake-coin matching gv_config must not squat the real coin's pool PDA"
    );
    assert!(
        env.svm.get_account(&env.pool).map_or(true, |a| a.data.is_empty()),
        "the rejected fake-coin init must leave the canonical pool PDA free"
    );

    env.init_insurance_pool();
    let ve = setup_vote(&mut env);
    assert_eq!(
        ve.gv_config,
        env.gv_config_pda(),
        "the legitimate genesis-vote config still initializes against the canonical pool"
    );
}

// SYBIL HOLE (vote outlives capital): set_vote_lock requires BOTH the owner AND the vote_authority
// (the gv config PDA) to sign. This pins the vote_authority-sig half — the one that stops an owner
// from SELF-UNLOCKING. The lock is only ever
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

// OWNER-SIGNED PAYOUT REDIRECT: insurance_withdraw correctly binds the signer to
// the position, but the pool PDA signs both downstream transfers. The destination
// must also belong to that owner or a malicious transaction builder can preserve
// the victim's signature while replacing only the payout account.
#[test]
fn owner_signed_insurance_withdraw_cannot_redirect_or_self_alias_the_payout() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let amount = 1_000_000u64;
    let (victim, victim_ata) = new_depositor(&mut env, amount);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);
    env.insurance_deposit(&victim, &victim_ata, &holding, amount)
        .expect("victim deposit");
    let (_attacker, attacker_ata) = new_depositor(&mut env, 0);
    let position = env.position_pda(&victim.pubkey());

    let pool_before = env.svm.get_account(&env.pool).unwrap();
    let position_before = env.svm.get_account(&position).unwrap();
    let slab_before = env.svm.get_account(&env.slab).unwrap();
    let vault_before = env.svm.get_account(&env.perc_vault).unwrap();
    let holding_before = env.svm.get_account(&holding).unwrap();
    assert!(
        env.insurance_withdraw(&victim, &attacker_ata, &holding, &victim, amount)
            .is_err(),
        "a valid owner signature must not authorize insurance payout to an attacker"
    );
    assert_eq!(env.svm.get_account(&env.pool).unwrap(), pool_before);
    assert_eq!(env.svm.get_account(&position).unwrap(), position_before);
    assert_eq!(env.svm.get_account(&env.slab).unwrap(), slab_before);
    assert_eq!(env.svm.get_account(&env.perc_vault).unwrap(), vault_before);
    assert_eq!(env.svm.get_account(&holding).unwrap(), holding_before);
    assert_eq!(env.token_amount(&attacker_ata), 0);

    // SPL Token accepts a fully-authorized self-transfer as a no-op. Naming the
    // pool holding as both source and payout must not consume the position while
    // leaving its withdrawn principal stranded in that shared account.
    assert!(
        env.insurance_withdraw(&victim, &holding, &holding, &victim, amount)
            .is_err(),
        "holding-to-itself payout must reject before principal accounting changes"
    );
    assert_eq!(env.svm.get_account(&env.pool).unwrap(), pool_before);
    assert_eq!(env.svm.get_account(&position).unwrap(), position_before);
    assert_eq!(env.svm.get_account(&env.slab).unwrap(), slab_before);
    assert_eq!(env.svm.get_account(&env.perc_vault).unwrap(), vault_before);
    assert_eq!(env.svm.get_account(&holding).unwrap(), holding_before);

    env.insurance_withdraw(&victim, &victim_ata, &holding, &victim, amount)
        .expect("victim still exits to their own token account");
    assert_eq!(env.token_amount(&victim_ata), amount);
    assert_eq!(env.token_amount(&attacker_ata), 0);
    assert_eq!(env.token_amount(&holding), 0);
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
            AccountMeta::new_readonly(env.gv_config_pda(), false),
            AccountMeta::new_readonly(env.coin_mint, false),
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
    cd.extend_from_slice(&1u32.to_le_bytes());
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
            AccountMeta::new_readonly(dist_config, false),
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
// the pool PDA commits to (mint, asset_id, market_slab, percolator_program, coin_mint, policy, domain),
// so an attacker's pool lands at a DIFFERENT address and the genesis pool PDA — bound
// to the real market and COIN — stays free and untouched.
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
    let attacker_policy = [POLICY_PRINCIPAL];
    let attacker_domain = [DOMAIN_INSURANCE];
    let attacker_window = DEFAULT_GENESIS_DEPOSIT_WINDOW_SLOTS.to_le_bytes();
    let attacker_start = DEFAULT_GENESIS_DEPOSIT_START_SLOT.to_le_bytes();
    let attacker_delay = DEFAULT_GENESIS_BOOTSTRAP_DELAY_SLOTS.to_le_bytes();
    let attacker_pool = Pubkey::find_program_address(
        &[
            b"subledger_pool",
            env.mint.as_ref(),
            &ASSET_ID.to_le_bytes(),
            attacker_slab.as_ref(),
            perc_id().as_ref(),
            env.coin_mint.as_ref(),
            &attacker_policy,
            &attacker_domain,
            &attacker_window,
            &attacker_start,
            &attacker_delay,
        ],
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
            AccountMeta::new_readonly(
                gv_config_pda_for_schedule(
                    &env.coin_mint,
                    &attacker_pool,
                    env.bootstrap_delay_slots,
                    env.bootstrap_start_slot,
                ),
                false,
            ),
            AccountMeta::new_readonly(env.coin_mint, false),
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
            AccountMeta::new_readonly(env.gv_config_pda(), false),
            AccountMeta::new_readonly(env.coin_mint, false),
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

// The genesis insurance pool path is asset-0 only: deposits CPI into Percolator's
// TopUpInsurance instruction, while exits use the asset-indexed withdraw. Accepting
// a nonzero asset_id would create a pool whose deposits top up asset 0 but whose
// exits try to withdraw a different asset, stranding that pool's depositors.
#[test]
fn init_insurance_pool_rejects_nonzero_asset_id() {
    let mut env = Env::new();
    let asset_id = 1u64;
    let pool = Pubkey::find_program_address(
        &[
            b"subledger_pool",
            env.mint.as_ref(),
            &asset_id.to_le_bytes(),
            env.slab.as_ref(),
            perc_id().as_ref(),
            env.coin_mint.as_ref(),
            &[POLICY_PRINCIPAL],
            &[DOMAIN_INSURANCE],
            &DEFAULT_GENESIS_DEPOSIT_WINDOW_SLOTS.to_le_bytes(),
            &DEFAULT_GENESIS_DEPOSIT_START_SLOT.to_le_bytes(),
            &DEFAULT_GENESIS_BOOTSTRAP_DELAY_SLOTS.to_le_bytes(),
        ],
        &sub_id(),
    )
    .0;

    let mut data = vec![3u8]; // IX_INIT_INSURANCE_POOL
    data.extend_from_slice(&asset_id.to_le_bytes());
    data.push(POLICY_PRINCIPAL);
    let ix = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(env.mint, false),
            AccountMeta::new(pool, false),
            AccountMeta::new_readonly(env.perc_vault, false),
            AccountMeta::new_readonly(env.slab, false),
            AccountMeta::new_readonly(perc_id(), false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(
                gv_config_pda_for_schedule(
                    &env.coin_mint,
                    &pool,
                    env.bootstrap_delay_slots,
                    env.bootstrap_start_slot,
                ),
                false,
            ),
            AccountMeta::new_readonly(env.coin_mint, false),
        ],
        data,
    };

    assert!(
        env.send(&[ix], &[]).is_err(),
        "asset-0 genesis insurance init must reject nonzero asset_id"
    );
    assert!(
        env.svm.get_account(&pool).map_or(true, |a| a.data.is_empty()),
        "nonzero-asset pool PDA remains uninitialized"
    );

    env.init_insurance_pool();
    let pool = env.pool;
    let (alice, alice_ata) = new_depositor(&mut env, 1_000_000);
    let hold = create_holding(&mut env, &pool);
    env.insurance_deposit(&alice, &alice_ata, &hold, 1_000_000).expect("asset-0 pool remains usable");
    env.insurance_withdraw(&alice, &alice_ata, &hold, &alice, 1_000_000).expect("asset-0 exit remains usable");
}

// FIRST-WRITER POLICY SQUAT (soft-veto bypass): the intended genesis pool is share-based
// (POLICY_WITH_SURPLUS). If the policy byte is not part of the pool PDA namespace, a front-runner can
// initialize the exact canonical pool with POLICY_PRINCIPAL, consuming it before the orchestrator creates
// the share-based pool. Deposits/votes still work, but exits no longer redeem surplus, so the residual
// distributor's soft-veto design is silently disabled. The wrong in-range policy must land at a different
// PDA, leaving the intended share-based pool free.
#[test]
fn wrong_in_range_policy_cannot_squat_the_share_based_genesis_pool() {
    let mut env = Env::new_for_policy(POLICY_WITH_SURPLUS);

    let mut data = vec![3u8]; // IX_INIT_INSURANCE_POOL
    data.extend_from_slice(&ASSET_ID.to_le_bytes());
    data.push(POLICY_PRINCIPAL);
    let squat = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(env.mint, false),
            AccountMeta::new(env.pool, false),
            AccountMeta::new_readonly(env.perc_vault, false),
            AccountMeta::new_readonly(env.slab, false),
            AccountMeta::new_readonly(perc_id(), false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(env.gv_config_pda(), false),
            AccountMeta::new_readonly(env.coin_mint, false),
        ],
        data,
    };
    assert!(
        env.send(&[squat], &[]).is_err(),
        "a principal-policy init must not consume the intended share-based genesis pool PDA"
    );
    assert!(
        env.svm.get_account(&env.pool).map_or(true, |a| a.data.is_empty()),
        "intended share-based genesis pool PDA remains free"
    );

    env.init_insurance_pool_policy(POLICY_WITH_SURPLUS);
    let pool_acc = env.svm.get_account(&env.pool).unwrap();
    assert_eq!(pool_acc.data[88], POLICY_WITH_SURPLUS, "real genesis pool is POLICY_WITH_SURPLUS");
}

// The genesis deposit window must close on-chain. Without this, late capital can
// inflate the live outstanding quorum denominator right before trigger without
// carrying comparable voting tenure, turning the bootstrap into a late-deposit DOS.
#[test]
fn genesis_insurance_deposit_window_rejects_late_capital_but_not_exits() {
    let mut env = Env::new_for_policy_with_window(POLICY_PRINCIPAL, 3);
    env.init_insurance_pool_policy_with_window(POLICY_PRINCIPAL, Some(3));
    let pool = env.pool;
    let (alice, alice_ata) = new_depositor(&mut env, 10);
    let alice_hold = create_holding(&mut env, &pool);

    env.warp_slot(101);
    env.insurance_deposit(&alice, &alice_ata, &alice_hold, 10)
        .expect("deposit before the short genesis window closes");
    assert_eq!(env.pool_outstanding(), 10);

    let (bob, bob_ata) = new_depositor(&mut env, 10);
    let bob_hold = create_holding(&mut env, &pool);
    env.warp_slot(104);
    assert!(
        env.insurance_deposit(&bob, &bob_ata, &bob_hold, 10).is_err(),
        "late capital must not be able to inflate the genesis quorum denominator"
    );
    assert_eq!(
        env.token_amount(&bob_ata),
        10,
        "rejected late deposit moved no funds"
    );
    assert_eq!(
        env.pool_outstanding(),
        10,
        "late rejected deposit did not change outstanding principal"
    );

    env.insurance_withdraw(&alice, &alice_ata, &alice_hold, &alice, 10)
        .expect("existing depositors can exit after deposits close");
    assert_eq!(env.token_amount(&alice_ata), 10);
}

// FIRST-WRITER SCHEDULE SQUAT: a custom window length alone is not enough. If
// the absolute start is implicit, a permissionless first writer can initialize
// the otherwise correct pool before launch, starting the deposit clock early and
// closing it before normal participants arrive. The configured start is now
// explicit, PDA-bound, and enforced by insurance_deposit.
#[test]
fn configured_deposit_start_cannot_be_opened_early_by_permissionless_init() {
    let start_slot = 110u64;
    let window = 3u64;
    let mut env = Env::new_for_policy_with_schedule(POLICY_PRINCIPAL, window, start_slot);

    env.init_insurance_pool_policy_with_schedule(POLICY_PRINCIPAL, Some(window), Some(start_slot));
    let pool = env.pool;

    let (early, early_ata) = new_depositor(&mut env, 10);
    let early_hold = create_holding(&mut env, &pool);
    env.warp_slot(start_slot - 1);
    assert!(
        env.insurance_deposit(&early, &early_ata, &early_hold, 10)
            .is_err(),
        "deposits before the configured start must not open early"
    );
    assert_eq!(
        env.token_amount(&early_ata),
        10,
        "rejected pre-start deposit moved no funds"
    );

    let (alice, alice_ata) = new_depositor(&mut env, 10);
    let alice_hold = create_holding(&mut env, &pool);
    env.warp_slot(start_slot);
    env.insurance_deposit(&alice, &alice_ata, &alice_hold, 10)
        .expect("deposit succeeds once the configured window opens");

    let (late, late_ata) = new_depositor(&mut env, 10);
    let late_hold = create_holding(&mut env, &pool);
    env.warp_slot(start_slot + window);
    assert!(
        env.insurance_deposit(&late, &late_ata, &late_hold, 10)
            .is_err(),
        "deposits at/after the configured deadline must close"
    );
    assert_eq!(env.token_amount(&late_ata), 10);
}

#[test]
fn bootstrap_schedule_is_one_pda_bound_contract_across_pool_and_vote() {
    let window = 3u64;
    let start = 110u64;
    let delay = 10u64;
    let mut env =
        Env::new_for_policy_with_bootstrap_schedule(POLICY_PRINCIPAL, window, start, delay);

    // A permissionless first writer using a different bootstrap delay derives a
    // different pool and cannot consume the intended schedule's PDA.
    let hostile_delay = delay + 1;
    let mut hostile_data = vec![3u8];
    hostile_data.extend_from_slice(&ASSET_ID.to_le_bytes());
    hostile_data.push(POLICY_PRINCIPAL);
    hostile_data.extend_from_slice(&window.to_le_bytes());
    hostile_data.extend_from_slice(&start.to_le_bytes());
    hostile_data.extend_from_slice(&hostile_delay.to_le_bytes());
    let hostile_vote_authority =
        gv_config_pda_for_schedule(&env.coin_mint, &env.pool, hostile_delay, start);
    let hostile_init = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(env.mint, false),
            AccountMeta::new(env.pool, false),
            AccountMeta::new_readonly(env.perc_vault, false),
            AccountMeta::new_readonly(env.slab, false),
            AccountMeta::new_readonly(perc_id(), false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(hostile_vote_authority, false),
            AccountMeta::new_readonly(env.coin_mint, false),
        ],
        data: hostile_data,
    };
    assert!(env.send(&[hostile_init], &[]).is_err());
    assert!(env
        .svm
        .get_account(&env.pool)
        .map_or(true, |a| a.data.is_empty()));

    env.init_insurance_pool_policy_with_schedule(POLICY_PRINCIPAL, Some(window), Some(start));
    let pool_data = env.svm.get_account(&env.pool).unwrap().data;
    assert_eq!(
        u64::from_le_bytes(pool_data[248..256].try_into().unwrap()),
        window
    );
    assert_eq!(
        u64::from_le_bytes(pool_data[256..264].try_into().unwrap()),
        start
    );
    assert_eq!(
        u64::from_le_bytes(pool_data[264..272].try_into().unwrap()),
        delay
    );
    assert_eq!(
        env.gv_config_pda(),
        gv_config_pda_for_schedule(&env.coin_mint, &env.pool, delay, start)
    );
}

#[test]
fn deposit_window_cannot_outlive_the_bootstrap() {
    let window = 11u64;
    let start = 110u64;
    let delay = 10u64;
    let mut env =
        Env::new_for_policy_with_bootstrap_schedule(POLICY_PRINCIPAL, window, start, delay);
    let mut data = vec![3u8];
    data.extend_from_slice(&ASSET_ID.to_le_bytes());
    data.push(POLICY_PRINCIPAL);
    data.extend_from_slice(&window.to_le_bytes());
    data.extend_from_slice(&start.to_le_bytes());
    data.extend_from_slice(&delay.to_le_bytes());
    let ix = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(env.mint, false),
            AccountMeta::new(env.pool, false),
            AccountMeta::new_readonly(env.perc_vault, false),
            AccountMeta::new_readonly(env.slab, false),
            AccountMeta::new_readonly(perc_id(), false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(env.gv_config_pda(), false),
            AccountMeta::new_readonly(env.coin_mint, false),
        ],
        data,
    };
    assert!(
        env.send(&[ix], &[]).is_err(),
        "genesis cannot become triggerable while insurance deposits remain open"
    );
    assert!(env
        .svm
        .get_account(&env.pool)
        .map_or(true, |a| a.data.is_empty()));
}

#[test]
fn exact_schedule_boundaries_complete_real_percolator_genesis_and_return_principal() {
    let window = 3u64;
    let start = 110u64;
    let delay = 10u64;
    let end = start + delay;
    let mut env =
        Env::new_for_policy_with_bootstrap_schedule(POLICY_PRINCIPAL, window, start, delay);
    env.init_insurance_pool_policy_with_schedule(POLICY_PRINCIPAL, Some(window), Some(start));
    let ve = setup_vote(&mut env);
    let pool = env.pool;
    let (alice, alice_ata) = new_depositor(&mut env, 1);
    let alice_hold = create_holding(&mut env, &pool);

    env.warp_slot(start - 1);
    assert!(env
        .insurance_deposit(&alice, &alice_ata, &alice_hold, 1)
        .is_err());
    env.warp_slot(start);
    env.insurance_deposit(&alice, &alice_ata, &alice_hold, 1)
        .expect("exact bootstrap start accepts the one-base-unit deposit");

    let recipient = Pubkey::new_unique();
    let (dist_proposal, gv_proposal) = create_and_register_proposal(&mut env, &ve, 1, &recipient);
    env.warp_slot(start + 2);
    gv_vote(&mut env, &ve, &alice, &gv_proposal, 1)
        .expect("one base unit gives one principal vote");

    let (late, late_ata) = new_depositor(&mut env, 1);
    let late_hold = create_holding(&mut env, &pool);
    env.warp_slot(start + window);
    assert!(env
        .insurance_deposit(&late, &late_ata, &late_hold, 1)
        .is_err());
    assert_eq!(
        env.token_amount(&late_ata),
        1,
        "deadline rejection moves no funds"
    );

    env.warp_slot(end - 1);
    assert!(
        gv_trigger_now(&mut env, &ve, &gv_proposal, &dist_proposal).is_err(),
        "genesis cannot seal one slot before bootstrap end"
    );
    env.warp_slot(end);
    gv_trigger_now(&mut env, &ve, &gv_proposal, &dist_proposal)
        .expect("genesis seals at the exact configured end");

    gv_vote(&mut env, &ve, &alice, &gv_proposal, 2).expect("winner retract releases vote lock");
    env.insurance_withdraw(&alice, &alice_ata, &alice_hold, &alice, 1)
        .expect("initial risk capital remains owner-withdrawable");
    assert_eq!(env.token_amount(&alice_ata), 1);
    assert_eq!(env.pool_outstanding(), 0);
}

// FIRST-WRITER DEPOSIT-WINDOW SQUAT: the window is part of the depositor contract.
// A hostile short window can make normal depositors miss genesis; a hostile long
// window re-opens the late-capital quorum grief the on-chain window is supposed
// to close. Permissionless init must therefore not let a wrong-window call
// consume the intended genesis pool PDA.
#[test]
fn wrong_deposit_window_cannot_squat_the_genesis_pool() {
    let intended_window = 3u64;
    let hostile_window = 1u64;
    let mut env = Env::new_for_policy_with_window(POLICY_PRINCIPAL, intended_window);

    let mut data = vec![3u8]; // IX_INIT_INSURANCE_POOL
    data.extend_from_slice(&ASSET_ID.to_le_bytes());
    data.push(POLICY_PRINCIPAL);
    data.extend_from_slice(&hostile_window.to_le_bytes());
    data.extend_from_slice(&100u64.to_le_bytes());
    data.extend_from_slice(&env.bootstrap_delay_slots.to_le_bytes());
    let squat = Instruction {
        program_id: sub_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(env.mint, false),
            AccountMeta::new(env.pool, false),
            AccountMeta::new_readonly(env.perc_vault, false),
            AccountMeta::new_readonly(env.slab, false),
            AccountMeta::new_readonly(perc_id(), false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(env.gv_config_pda(), false),
            AccountMeta::new_readonly(env.coin_mint, false),
        ],
        data,
    };
    assert!(
        env.send(&[squat], &[]).is_err(),
        "wrong-window init must not consume the intended genesis pool PDA"
    );
    assert!(
        env.svm
            .get_account(&env.pool)
            .map_or(true, |a| a.data.is_empty()),
        "genesis pool remains free for the intended window"
    );

    env.init_insurance_pool_policy_with_window(POLICY_PRINCIPAL, Some(intended_window));
    let pool = env.pool;
    let (alice, alice_ata) = new_depositor(&mut env, 10);
    let alice_hold = create_holding(&mut env, &pool);
    env.warp_slot(101);
    env.insurance_deposit(&alice, &alice_ata, &alice_hold, 10)
        .expect("intended-window pool accepts an in-window deposit");
}
// CROSS-INSTRUCTION PDA SQUAT (account-confusion/seed-collision): both init_pool (own-vault, tag 0)
// and init_insurance_pool (tag 3) derive their pool PDA from pool_seeds(mint, asset_id, market_slab,
// percolator_program, coin_mint, policy, domain). The genesis insurance pool lives at
// (mint, 0, REAL_market, REAL_program, REAL_coin, policy, INSURANCE). If
// init_pool let the caller supply the market/program seed parts, an attacker could derive that exact
// address with a BACKING-domain own-vault pool, seize the PDA (legit init then fails
// AccountAlreadyInitialized), and brick the genesis (genesis-vote needs is_insurance() == true).
// init_pool defends by HARDCODING the market/program seed components to Pubkey::default() (lib.rs:394),
// so own-vault pools are confined to the (mint, asset_id, default, default, default) namespace — provably
// disjoint from any real-market insurance pool. This pins that isolation: init_pool cannot be pointed
// at the genesis insurance PDA. (The init_insurance_pool foreign-market + bad-policy squats are pinned
// separately; this closes the wrong-instruction angle.)
#[test]
fn own_vault_init_pool_cannot_squat_the_genesis_insurance_pda() {
    let mut env = Env::new();

    // The own-vault namespace for the same (mint, asset_id) is a DIFFERENT address than the genesis
    // insurance pool — the market/program seed parts differ (default vs the real market).
    let own_policy = [POLICY_PRINCIPAL];
    let own_domain = [1u8]; // DOMAIN_BACKING
    let own_window = OWN_VAULT_DEPOSIT_WINDOW_SLOTS.to_le_bytes();
    let own_start = OWN_VAULT_DEPOSIT_START_SLOT.to_le_bytes();
    let own_delay = OWN_VAULT_BOOTSTRAP_DELAY_SLOTS.to_le_bytes();
    let own_vault_pda = Pubkey::find_program_address(
        &[
            b"subledger_pool",
            env.mint.as_ref(),
            &ASSET_ID.to_le_bytes(),
            Pubkey::default().as_ref(),
            Pubkey::default().as_ref(),
            Pubkey::default().as_ref(),
            &own_policy,
            &own_domain,
            &own_window,
            &own_start,
            &own_delay,
        ],
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

    let (snapshot_principal, snapshot_start, _) = env.read_position(&alice.pubkey());
    let mut attack_data = vec![2u8]; // IX_WITHDRAW (own-vault)
    attack_data.extend_from_slice(&snapshot_principal.to_le_bytes());
    attack_data.extend_from_slice(&snapshot_start.to_le_bytes());
    attack_data.extend_from_slice(&env.position_action_nonce(&alice.pubkey()).to_le_bytes());
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
        data: attack_data,
    };
    assert!(env.send(&[attack], &[&alice]).is_err(), "own-vault withdraw must be rejected on an insurance pool");
    let (principal, _start, withdrawn) = env.read_position(&alice.pubkey());
    assert_eq!(principal, amount, "position principal intact after the rejected own-vault withdraw");
    assert!(!withdrawn, "position not retired (no phantom withdrawn state)");
    assert_eq!(env.pool_outstanding(), amount, "pool outstanding intact");
}

// TOP-UP RESETS REWARD TENURE. If a top-up did not reset start_slot, a one-atom early
// position could give much later capital the early atom's insurance/backing reward tenure.
// Genesis voting itself uses principal only.
#[test]
fn top_up_resets_the_position_start_slot() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let (alice, alice_ata) = new_depositor(&mut env, 2_000_000);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);

    env.insurance_deposit(&alice, &alice_ata, &holding, 1).expect("early small deposit");
    let (_p0, start0, _w0) = env.read_position(&alice.pubkey());

    // Reward tenure accrues, then a large top-up lands much later.
    env.warp_slot(1_000);
    env.insurance_deposit(&alice, &alice_ata, &holding, 1_999_999).expect("late huge top-up");
    let (principal, start1, _w1) = env.read_position(&alice.pubkey());

    assert_eq!(principal, 2_000_000, "principal accumulated across deposits");
    assert_eq!(start1, 1_000, "top-up resets start_slot to the late capital's own reward start");
    assert!(start1 > start0, "late capital does not inherit earlier reward tenure");
}

// LIVENESS BOUNDARY: large withdrawal amounts must advance the position nonce
// once per state change, not consume nonce space proportional to value. The same
// bounded recycling sequence that saturated historical payout telemetry must
// leave the nonce far from its representational ceiling and preserve final exit.
#[test]
fn large_cumulative_withdrawals_cannot_exhaust_the_position_action_nonce() {
    let mut env = Env::new();
    env.init_insurance_pool();
    let live_limit = u64::try_from(percolator::MAX_VAULT_TVL).unwrap();
    let chunk = live_limit - 1;
    let full_cycles = u64::MAX / chunk;
    let remainder = u64::MAX % chunk;
    assert!(full_cycles < 2_000, "probe remains bounded");
    let (alice, alice_ata) = new_depositor(&mut env, live_limit);
    let pool = env.pool;
    let holding = create_holding(&mut env, &pool);

    env.insurance_deposit(&alice, &alice_ata, &holding, live_limit)
        .expect("maximum live-shape insurance deposit");
    env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, chunk)
        .expect("first valid partial exit");

    for _ in 1..full_cycles {
        env.insurance_deposit(&alice, &alice_ata, &holding, chunk)
            .expect("bounded recycled top-up");
        env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, chunk)
            .expect("bounded recycled partial exit");
    }
    if remainder > 0 {
        env.insurance_deposit(&alice, &alice_ata, &holding, remainder)
            .expect("remainder top-up");
        env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, remainder)
            .expect("counter reaches u64::MAX");
    }

    let expected_nonce_before_final = full_cycles
        .checked_mul(2)
        .and_then(|nonce| nonce.checked_add(u64::from(remainder > 0) * 2))
        .unwrap();
    let position = env.svm.get_account(&env.position_pda(&alice.pubkey())).unwrap();
    assert_eq!(
        u64::from_le_bytes(position.data[80..88].try_into().unwrap()),
        expected_nonce_before_final,
        "withdrawal value cannot accelerate nonce exhaustion",
    );
    assert_eq!(env.read_position(&alice.pubkey()).0, 1);
    assert_eq!(env.pool_outstanding(), 1);
    assert_eq!(env.token_amount(&env.perc_vault), 1);

    env.insurance_withdraw(&alice, &alice_ata, &holding, &alice, 1)
        .expect("counter saturation must not block the final principal exit");

    let (principal, _, withdrawn) = env.read_position(&alice.pubkey());
    assert_eq!(principal, 0);
    assert!(withdrawn);
    let position = env.svm.get_account(&env.position_pda(&alice.pubkey())).unwrap();
    assert_eq!(
        u64::from_le_bytes(position.data[80..88].try_into().unwrap()),
        expected_nonce_before_final + 1,
        "the final exit advances the nonce exactly once"
    );
    assert_eq!(env.pool_outstanding(), 0);
    assert_eq!(env.token_amount(&env.perc_vault), 0);
    assert_eq!(env.token_amount(&alice_ata), live_limit);
}
