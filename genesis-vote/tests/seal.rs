//! Cross-program integration: the genesis-vote trigger seals a distribution
//! proposal by CPI (the genesis-vote config PDA is the distribution program's
//! seal authority). The Percolator-backed deposit/vote path is exercised in the
//! chain integration; here we inject a winning tally directly and prove the seal.

use litesvm::LiteSVM;
use solana_sdk::{
    clock::Clock,
    instruction::{AccountMeta, Instruction},
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction,
    transaction::Transaction,
};

fn gv_id() -> Pubkey {
    genesis_vote_program::id()
}
fn dist_id() -> Pubkey {
    distribution_program::id()
}
fn so(name: &str) -> String {
    format!("{}/../target/deploy/{}.so", env!("CARGO_MANIFEST_DIR"), name)
}
fn clone_kp(kp: &Keypair) -> Keypair {
    Keypair::from_bytes(&kp.to_bytes()).unwrap()
}
const DISTRIBUTION_CLAIM_WINDOW_SLOTS: u64 = 1_000_000;
fn dist_config_pda(coin_mint: &Pubkey, authority: &Pubkey) -> Pubkey {
    let claim_window = DISTRIBUTION_CLAIM_WINDOW_SLOTS.to_le_bytes();
    Pubkey::find_program_address(
        &[
            b"dist_config",
            coin_mint.as_ref(),
            authority.as_ref(),
            &claim_window,
        ],
        &dist_id(),
    )
    .0
}

const TEST_BOOTSTRAP_DELAY_SLOTS: u64 = 1;
const TEST_BOOTSTRAP_START_SLOT: u64 = 0;

fn gv_init_data(delay_slots: u64) -> Vec<u8> {
    gv_init_schedule_data(delay_slots, TEST_BOOTSTRAP_START_SLOT)
}

fn gv_init_schedule_data(delay_slots: u64, start_slot: u64) -> Vec<u8> {
    let mut data = vec![0u8];
    data.extend_from_slice(&delay_slots.to_le_bytes());
    data.extend_from_slice(&start_slot.to_le_bytes());
    data
}

fn gv_config_for_schedule(
    coin_mint: &Pubkey,
    sub_pool: &Pubkey,
    delay_slots: u64,
    start_slot: u64,
) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"gv_config",
            coin_mint.as_ref(),
            sub_pool.as_ref(),
            &delay_slots.to_le_bytes(),
            &start_slot.to_le_bytes(),
        ],
        &gv_id(),
    )
    .0
}

struct Env {
    svm: LiteSVM,
    payer: Keypair,
    coin_mint: Pubkey,
    mint_auth: Keypair,
    gv_config: Pubkey,
    dist_config: Pubkey,
    vault: Pubkey,
    sub_pid: Pubkey,
    sub_pool: Pubkey,
    total_supply: u64,
}

impl Env {
    fn new() -> Self {
        Self::with_supply(100)
    }

    fn with_supply(total_supply: u64) -> Self {
        let mut svm = LiteSVM::new();
        svm.add_program_from_file(gv_id(), so("genesis_vote_program")).unwrap();
        svm.add_program_from_file(dist_id(), so("distribution_program")).unwrap();
        let payer = Keypair::new();
        svm.airdrop(&payer.pubkey(), 1_000_000_000_000).unwrap();
        let mint_auth = Keypair::new();
        let coin_mint = create_mint(&mut svm, &payer, &mint_auth.pubkey());

        // Stand-in subledger program id + insurance pool; the genesis-vote config
        // pins these and the trigger re-reads the pool's outstanding live. The gv
        // config PDA now commits to the pool (finding R), so derive it after.
        let sub_pid = Pubkey::new_from_array([7u8; 32]);
        let sub_pool = Pubkey::new_from_array([8u8; 32]);
        let gv_config = gv_config_for_schedule(
            &coin_mint,
            &sub_pool,
            TEST_BOOTSTRAP_DELAY_SLOTS,
            TEST_BOOTSTRAP_START_SLOT,
        );
        let dist_config = dist_config_pda(&coin_mint, &gv_config);
        let vault = create_token_account(&mut svm, &payer, &coin_mint, &dist_config);
        mint_to(&mut svm, &payer, &coin_mint, &mint_auth, &vault, total_supply);

        let mut env = Env {
            svm,
            payer,
            coin_mint,
            mint_auth,
            gv_config,
            dist_config,
            vault,
            sub_pid,
            sub_pool,
            total_supply,
        };
        env.set_pool_outstanding(0);
        env.init_distribution();
        env.init_gv().expect("gv init");
        env
    }

    /// Everything except the genesis-vote InitConfig (so a test can poison a wired
    /// dependency and assert init refuses to bind to it).
    fn new_unwired() -> Self {
        let mut svm = LiteSVM::new();
        svm.add_program_from_file(gv_id(), so("genesis_vote_program")).unwrap();
        svm.add_program_from_file(dist_id(), so("distribution_program")).unwrap();
        let payer = Keypair::new();
        svm.airdrop(&payer.pubkey(), 1_000_000_000_000).unwrap();
        let mint_auth = Keypair::new();
        let coin_mint = create_mint(&mut svm, &payer, &mint_auth.pubkey());
        let sub_pid = Pubkey::new_from_array([7u8; 32]);
        let sub_pool = Pubkey::new_from_array([8u8; 32]);
        let gv_config = gv_config_for_schedule(
            &coin_mint,
            &sub_pool,
            TEST_BOOTSTRAP_DELAY_SLOTS,
            TEST_BOOTSTRAP_START_SLOT,
        );
        let dist_config = dist_config_pda(&coin_mint, &gv_config);
        let vault = create_token_account(&mut svm, &payer, &coin_mint, &dist_config);
        mint_to(&mut svm, &payer, &coin_mint, &mint_auth, &vault, 100);
        let mut env = Env { svm, payer, coin_mint, mint_auth, gv_config, dist_config, vault, sub_pid, sub_pool, total_supply: 100 };
        env.set_pool_outstanding(0);
        env.init_distribution();
        env
    }

    fn new_unwired_with_schedule(delay_slots: u64, start_slot: u64) -> Self {
        let mut svm = LiteSVM::new();
        svm.add_program_from_file(gv_id(), so("genesis_vote_program")).unwrap();
        svm.add_program_from_file(dist_id(), so("distribution_program")).unwrap();
        let payer = Keypair::new();
        svm.airdrop(&payer.pubkey(), 1_000_000_000_000).unwrap();
        let mint_auth = Keypair::new();
        let coin_mint = create_mint(&mut svm, &payer, &mint_auth.pubkey());
        let sub_pid = Pubkey::new_from_array([7u8; 32]);
        let sub_pool = Pubkey::new_from_array([8u8; 32]);
        let gv_config = gv_config_for_schedule(&coin_mint, &sub_pool, delay_slots, start_slot);
        let dist_config = dist_config_pda(&coin_mint, &gv_config);
        let vault = create_token_account(&mut svm, &payer, &coin_mint, &dist_config);
        mint_to(&mut svm, &payer, &coin_mint, &mint_auth, &vault, 100);
        let mut env = Env {
            svm,
            payer,
            coin_mint,
            mint_auth,
            gv_config,
            dist_config,
            vault,
            sub_pid,
            sub_pool,
            total_supply: 100,
        };
        env.set_pool_outstanding(0);
        env.init_distribution();
        env
    }

    /// Write a fake subledger insurance pool account (owned by the stand-in
    /// subledger program) with the given `outstanding_principal` at offset 80..88
    /// and the SUBPOOL1 discriminator. This is what the trigger reads live.
    fn set_pool_outstanding(&mut self, outstanding: u64) {
        // 192-byte SUBPOOL1: mint at [8..40], outstanding at [80..88], and the
        // vote_authority at [160..192] = this gv config PDA (init_config now binds
        // the pool to the config, so the fixture must satisfy that).
        let mut data = vec![0u8; 192];
        data[..8].copy_from_slice(b"SUBPOOL1");
        data[8..40].copy_from_slice(self.coin_mint.as_ref());
        data[80..88].copy_from_slice(&outstanding.to_le_bytes());
        data[160..192].copy_from_slice(self.gv_config.as_ref());
        let acc = solana_sdk::account::Account {
            lamports: 1_000_000,
            data,
            owner: self.sub_pid,
            executable: false,
            rent_epoch: 0,
        };
        self.svm.set_account(self.sub_pool, acc).unwrap();
    }

    fn send(&mut self, ixs: &[Instruction], extra: &[&Keypair]) -> Result<(), String> {
        self.svm.expire_blockhash();
        let bh = self.svm.latest_blockhash();
        let payer = clone_kp(&self.payer);
        let mut signers: Vec<&Keypair> = vec![&payer];
        signers.extend_from_slice(extra);
        let pk = self.payer.pubkey();
        let tx = Transaction::new_signed_with_payer(ixs, Some(&pk), &signers, bh);
        self.svm.send_transaction(tx).map(|_| ()).map_err(|e| format!("{:?}", e))
    }

    fn set_slot(&mut self, slot: u64) {
        let mut clock = self.svm.get_sysvar::<Clock>();
        clock.slot = slot;
        self.svm.set_sysvar::<Clock>(&clock);
    }

    // distribution InitConfig with authority = the genesis-vote config PDA.
    fn init_distribution(&mut self) {
        // Fixed-supply COIN (Safety §4): revoke the mint authority before init.
        let revoke = spl_token::instruction::set_authority(
            &spl_token::ID,
            &self.coin_mint,
            None,
            spl_token::instruction::AuthorityType::MintTokens,
            &self.mint_auth.pubkey(),
            &[],
        )
        .unwrap();
        let auth = clone_kp(&self.mint_auth);
        self.send(&[revoke], &[&auth]).expect("revoke coin mint authority");

        let mut data = vec![0u8];
        data.extend_from_slice(&DISTRIBUTION_CLAIM_WINDOW_SLOTS.to_le_bytes()); // claim window
        data.extend_from_slice(&self.total_supply.to_le_bytes()); // total supply
        let ix = Instruction {
            program_id: dist_id(),
            accounts: vec![
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new(self.dist_config, false),
                AccountMeta::new_readonly(self.vault, false),
                AccountMeta::new_readonly(self.gv_config, false), // authority = gv config PDA
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data,
        };
        self.send(&[ix], &[]).expect("dist init");
    }

    fn dist_proposal(&self, id: u64) -> Pubkey {
        Pubkey::find_program_address(&[b"dist_proposal", self.dist_config.as_ref(), &id.to_le_bytes()], &dist_id()).0
    }

    fn create_dist_proposal(&mut self, id: u64, entries: &[(Pubkey, u64)]) -> Pubkey {
        let proposal = self.dist_proposal(id);
        let mut data = vec![1u8];
        data.extend_from_slice(&id.to_le_bytes());
        let capacity = core::cmp::max(1, entries.len() as u32);
        data.extend_from_slice(&capacity.to_le_bytes());
        let create = Instruction {
            program_id: dist_id(),
            accounts: vec![
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new_readonly(self.dist_config, false),
                AccountMeta::new(proposal, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data,
        };
        self.send(&[create], &[]).expect("create proposal");
        let mut ad = vec![2u8];
        ad.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for (pk, amt) in entries {
            ad.extend_from_slice(pk.as_ref());
            ad.extend_from_slice(&amt.to_le_bytes());
        }
        let append = Instruction {
            program_id: dist_id(),
            accounts: vec![
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new_readonly(self.dist_config, false),
                AccountMeta::new(proposal, false),
            ],
            data: ad,
        };
        self.send(&[append], &[]).expect("append");
        proposal
    }

    fn init_gv(&mut self) -> Result<(), String> {
        let result = self.init_gv_with_delay(TEST_BOOTSTRAP_DELAY_SLOTS);
        if result.is_ok() {
            self.set_slot(TEST_BOOTSTRAP_DELAY_SLOTS);
        }
        result
    }

    fn init_gv_with_delay(&mut self, delay_slots: u64) -> Result<(), String> {
        let dummy = Pubkey::new_unique();
        let ix = Instruction {
            program_id: gv_id(),
            accounts: vec![
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new(self.gv_config, false),
                AccountMeta::new_readonly(dist_id(), false),
                AccountMeta::new_readonly(self.dist_config, false),
                AccountMeta::new_readonly(self.sub_pid, false),  // subledger_program
                AccountMeta::new_readonly(self.sub_pool, false), // subledger_pool
                AccountMeta::new_readonly(dummy, false),         // _reserved
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: gv_init_data(delay_slots),
        };
        self.send(&[ix], &[])
    }

    fn init_gv_with_schedule(&mut self, delay_slots: u64, start_slot: u64) -> Result<(), String> {
        let dummy = Pubkey::new_unique();
        let ix = Instruction {
            program_id: gv_id(),
            accounts: vec![
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new(self.gv_config, false),
                AccountMeta::new_readonly(dist_id(), false),
                AccountMeta::new_readonly(self.dist_config, false),
                AccountMeta::new_readonly(self.sub_pid, false),
                AccountMeta::new_readonly(self.sub_pool, false),
                AccountMeta::new_readonly(dummy, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: gv_init_schedule_data(delay_slots, start_slot),
        };
        self.send(&[ix], &[])
    }

    /// init_config but binding an arbitrary `dist` account as the distribution_config
    /// (instead of `self.dist_config`). Used to prove the gv config refuses to seal a
    /// distribution that is not authority-bound to this very config PDA.
    fn init_gv_with_dist(&mut self, dist: Pubkey) -> Result<(), String> {
        let dummy = Pubkey::new_unique();
        let ix = Instruction {
            program_id: gv_id(),
            accounts: vec![
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new(self.gv_config, false),
                AccountMeta::new_readonly(dist_id(), false),
                AccountMeta::new_readonly(dist, false),
                AccountMeta::new_readonly(self.sub_pid, false),
                AccountMeta::new_readonly(self.sub_pool, false),
                AccountMeta::new_readonly(dummy, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: gv_init_data(TEST_BOOTSTRAP_DELAY_SLOTS),
        };
        self.send(&[ix], &[])
    }

    /// init_config but passing an arbitrary `prog` as the distribution_program AND an
    /// arbitrary `dist` as the distribution_config — to prove gv refuses to bind a genesis
    /// to a NON-CANONICAL distribution program (anti front-run squat, the gv dual of HK).
    fn init_gv_with_prog_and_dist(&mut self, prog: Pubkey, dist: Pubkey) -> Result<(), String> {
        let dummy = Pubkey::new_unique();
        let ix = Instruction {
            program_id: gv_id(),
            accounts: vec![
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new(self.gv_config, false),
                AccountMeta::new_readonly(prog, false),
                AccountMeta::new_readonly(dist, false),
                AccountMeta::new_readonly(self.sub_pid, false),
                AccountMeta::new_readonly(self.sub_pool, false),
                AccountMeta::new_readonly(dummy, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: gv_init_data(TEST_BOOTSTRAP_DELAY_SLOTS),
        };
        self.send(&[ix], &[])
    }

    /// Plant a valid-looking distribution config OWNED BY an arbitrary program `prog`
    /// (vs plant_foreign_dist, which always owns by the real distribution program).
    fn plant_dist_owned_by(&mut self, at: Pubkey, prog: Pubkey, coin: Pubkey, authority: Pubkey) {
        let mut data = vec![0u8; 168];
        data[..8].copy_from_slice(b"DISTCFG1");
        data[8..40].copy_from_slice(coin.as_ref());
        data[40..72].copy_from_slice(self.vault.as_ref());
        data[72..104].copy_from_slice(authority.as_ref());
        self.svm
            .set_account(
                at,
                solana_sdk::account::Account {
                    lamports: 2_000_000,
                    data,
                    owner: prog,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
    }

    /// Plant a fully-valid-looking distribution config (right owner/disc/coin) at an
    /// arbitrary address, with `authority` set to whatever we pass — so we can craft one
    /// whose seal authority is NOT this gv config PDA.
    fn plant_foreign_dist(&mut self, at: Pubkey, coin: Pubkey, authority: Pubkey) {
        let mut data = vec![0u8; 168]; // CONFIG_SIZE
        data[..8].copy_from_slice(b"DISTCFG1");
        data[8..40].copy_from_slice(coin.as_ref());
        data[40..72].copy_from_slice(self.vault.as_ref());
        data[72..104].copy_from_slice(authority.as_ref());
        self.svm
            .set_account(
                at,
                solana_sdk::account::Account {
                    lamports: 2_000_000,
                    data,
                    owner: dist_id(),
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
    }

    /// Overwrite the fake pool's vote_authority (bytes 160..192) with an arbitrary
    /// key, leaving everything else valid.
    fn poison_pool_vote_authority(&mut self, bad: &Pubkey) {
        let mut acc = self.svm.get_account(&self.sub_pool).unwrap();
        acc.data[160..192].copy_from_slice(bad.as_ref());
        self.svm.set_account(self.sub_pool, acc).unwrap();
    }

    fn gv_proposal_pda(&self, dist_proposal: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[b"gv_proposal", self.gv_config.as_ref(), dist_proposal.as_ref()], &gv_id()).0
    }

    fn register(&mut self, dist_proposal: &Pubkey) -> Pubkey {
        let gv_proposal = self.gv_proposal_pda(dist_proposal);
        let ix = Instruction {
            program_id: gv_id(),
            accounts: vec![
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new_readonly(self.gv_config, false),
                AccountMeta::new(gv_proposal, false),
                AccountMeta::new_readonly(*dist_proposal, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: vec![2u8],
        };
        self.send(&[ix], &[]).expect("register");
        gv_proposal
    }

    /// register signed by an arbitrary keypair (not the dist-proposal's creator) — to
    /// prove the register creator-binding refuses a foreign registrant.
    fn register_as(&mut self, dist_proposal: &Pubkey, signer: &Keypair) -> Result<Pubkey, String> {
        let gv_proposal = self.gv_proposal_pda(dist_proposal);
        let ix = Instruction {
            program_id: gv_id(),
            accounts: vec![
                AccountMeta::new(signer.pubkey(), true),
                AccountMeta::new_readonly(self.gv_config, false),
                AccountMeta::new(gv_proposal, false),
                AccountMeta::new_readonly(*dist_proposal, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: vec![2u8],
        };
        let s = clone_kp(signer);
        self.send(&[ix], &[&s]).map(|_| gv_proposal)
    }

    /// Inject a winning tally directly (the Percolator-backed deposit/vote path is
    /// tested in the chain integration). Sets gv config global tallies and the
    /// gv proposal-vote support.
    fn inject_tally(&mut self, gv_proposal: &Pubkey, voted_principal: u64, cast_weight: u64, outstanding: u64, support_weight: u64, support_principal: u64) {
        let mut cfg = self.svm.get_account(&self.gv_config).unwrap();
        cfg.data[200..208].copy_from_slice(&voted_principal.to_le_bytes());
        cfg.data[208..224].copy_from_slice(&(cast_weight as u128).to_le_bytes()); // GG: total_cast_weight now u128
        cfg.data[224..232].copy_from_slice(&outstanding.to_le_bytes());
        self.svm.set_account(self.gv_config, cfg).unwrap();

        let mut pv = self.svm.get_account(gv_proposal).unwrap();
        pv.data[72..88].copy_from_slice(&(support_weight as u128).to_le_bytes()); // GG: support_weight now u128
        pv.data[88..96].copy_from_slice(&support_principal.to_le_bytes());
        self.svm.set_account(*gv_proposal, pv).unwrap();
    }

    fn trigger(&mut self, gv_proposal: &Pubkey, dist_proposal: &Pubkey) -> Result<(), String> {
        let ix = Instruction {
            program_id: gv_id(),
            accounts: vec![
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new(self.gv_config, false),
                AccountMeta::new(*gv_proposal, false),
                AccountMeta::new_readonly(dist_id(), false),
                AccountMeta::new(self.dist_config, false),
                AccountMeta::new(*dist_proposal, false),
                AccountMeta::new_readonly(self.sub_pool, false), // live quorum denominator
            ],
            data: vec![4u8],
        };
        self.send(&[ix], &[])
    }

    fn dist_sealed_proposal(&self) -> Pubkey {
        let cfg = self.svm.get_account(&self.dist_config).unwrap();
        Pubkey::new_from_array(cfg.data[120..152].try_into().unwrap())
    }
}

fn create_mint(svm: &mut LiteSVM, payer: &Keypair, authority: &Pubkey) -> Pubkey {
    let mint = Keypair::new();
    let rent = svm.minimum_balance_for_rent_exemption(spl_token::state::Mint::LEN);
    let ixs = [
        system_instruction::create_account(&payer.pubkey(), &mint.pubkey(), rent, spl_token::state::Mint::LEN as u64, &spl_token::ID),
        spl_token::instruction::initialize_mint(&spl_token::ID, &mint.pubkey(), authority, None, 6).unwrap(),
    ];
    let tx = Transaction::new_signed_with_payer(&ixs, Some(&payer.pubkey()), &[payer, &mint], svm.latest_blockhash());
    svm.send_transaction(tx).unwrap();
    mint.pubkey()
}
fn create_token_account(svm: &mut LiteSVM, payer: &Keypair, mint: &Pubkey, owner: &Pubkey) -> Pubkey {
    let acc = Keypair::new();
    let rent = svm.minimum_balance_for_rent_exemption(spl_token::state::Account::LEN);
    let ixs = [
        system_instruction::create_account(&payer.pubkey(), &acc.pubkey(), rent, spl_token::state::Account::LEN as u64, &spl_token::ID),
        spl_token::instruction::initialize_account(&spl_token::ID, &acc.pubkey(), mint, owner).unwrap(),
    ];
    let tx = Transaction::new_signed_with_payer(&ixs, Some(&payer.pubkey()), &[payer, &acc], svm.latest_blockhash());
    svm.send_transaction(tx).unwrap();
    acc.pubkey()
}
fn mint_to(svm: &mut LiteSVM, payer: &Keypair, mint: &Pubkey, authority: &Keypair, dest: &Pubkey, amount: u64) {
    let ix = spl_token::instruction::mint_to(&spl_token::ID, mint, dest, &authority.pubkey(), &[], amount).unwrap();
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[payer, authority], svm.latest_blockhash());
    svm.send_transaction(tx).unwrap();
}

#[test]
fn trigger_seals_the_distribution_cross_program() {
    let mut env = Env::new();
    let alice = Pubkey::new_unique();
    let bob = Pubkey::new_unique();
    let dist_proposal = env.create_dist_proposal(1, &[(alice, 60), (bob, 40)]);
    let gv_proposal = env.register(&dist_proposal);
    env.set_pool_outstanding(10); // live quorum denominator

    // Below quorum/majority: trigger is rejected.
    env.inject_tally(&gv_proposal, 4, 8, 10, 3, 4); // voted 4 of 10 -> 4*2=8 !> 10
    assert!(env.trigger(&gv_proposal, &dist_proposal).is_err(), "no quorum");

    // Quorum + majority: total_voted 10 of 10 (20>10), support_weight 8 of 8 (16>8).
    env.inject_tally(&gv_proposal, 10, 8, 10, 8, 10);
    assert_eq!(env.dist_sealed_proposal(), Pubkey::default(), "not sealed yet");
    env.trigger(&gv_proposal, &dist_proposal).expect("trigger seals");

    // The distribution program now has this proposal sealed as the winner.
    assert_eq!(env.dist_sealed_proposal(), dist_proposal, "distribution sealed cross-program");

    // Re-trigger is rejected (gv proposal already executed; distribution already sealed).
    assert!(env.trigger(&gv_proposal, &dist_proposal).is_err(), "no double seal");
}

// MAX-SHAPE HANDOFF PROBE: a distribution that reaches the advertised 10,000-entry
// shape must remain registerable and sealable through the permissionless genesis CPI.
#[test]
fn trigger_seals_a_full_max_capacity_distribution() {
    const ENTRIES: u32 = 10_000;
    const CHUNK: usize = 24;
    let mut env = Env::with_supply(ENTRIES as u64);
    let proposal = env.dist_proposal(99);

    let mut create_data = vec![1u8];
    create_data.extend_from_slice(&99u64.to_le_bytes());
    create_data.extend_from_slice(&ENTRIES.to_le_bytes());
    let create = Instruction {
        program_id: dist_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(env.dist_config, false),
            AccountMeta::new(proposal, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: create_data,
    };
    env.send(&[create], &[]).expect("create maximum proposal");

    let recipient = Pubkey::new_unique();
    let mut appended = 0u32;
    while appended < ENTRIES {
        let count = core::cmp::min(CHUNK, (ENTRIES - appended) as usize);
        let mut append_data = vec![2u8];
        append_data.extend_from_slice(&(count as u32).to_le_bytes());
        for _ in 0..count {
            append_data.extend_from_slice(recipient.as_ref());
            append_data.extend_from_slice(&1u64.to_le_bytes());
        }
        let append = Instruction {
            program_id: dist_id(),
            accounts: vec![
                AccountMeta::new(env.payer.pubkey(), true),
                AccountMeta::new_readonly(env.dist_config, false),
                AccountMeta::new(proposal, false),
            ],
            data: append_data,
        };
        env.send(&[append], &[]).expect("append maximum proposal chunk");
        appended += count as u32;
    }
    assert_eq!(env.svm.get_account(&proposal).unwrap().data.len(), 400_104);

    let gv_proposal = env.register(&proposal);
    env.set_pool_outstanding(1);
    env.inject_tally(&gv_proposal, 1, 1, 1, 1, 1);
    env.trigger(&gv_proposal, &proposal).expect("genesis trigger seals maximum proposal");
    assert_eq!(env.dist_sealed_proposal(), proposal);
}

#[test]
fn trigger_rejects_before_configured_bootstrap_delay_elapsed() {
    let mut env = Env::new_unwired_with_schedule(100, 0);
    env.init_gv_with_delay(100).expect("gv init with configured bootstrap delay");

    let alice = Pubkey::new_unique();
    let dist_proposal = env.create_dist_proposal(1, &[(alice, 100)]);
    let gv_proposal = env.register(&dist_proposal);
    env.set_pool_outstanding(10);
    env.inject_tally(&gv_proposal, 10, 8, 10, 8, 10);

    env.set_slot(99);
    assert!(
        env.trigger(&gv_proposal, &dist_proposal).is_err(),
        "quorum and majority must not seal before the bootstrap delay"
    );
    assert_eq!(env.dist_sealed_proposal(), Pubkey::default(), "not sealed before bootstrap end");

    env.set_slot(100);
    env.trigger(&gv_proposal, &dist_proposal).expect("trigger after bootstrap delay");
    assert_eq!(env.dist_sealed_proposal(), dist_proposal, "sealed once bootstrap delay elapsed");
}

#[test]
fn terminal_attestation_requires_elapsed_schedule_and_canonical_subledger_pool() {
    let mut env = Env::new();
    let attest = |config: Pubkey, pool: Pubkey| Instruction {
        program_id: gv_id(),
        accounts: vec![
            AccountMeta::new_readonly(config, false),
            AccountMeta::new_readonly(pool, false),
        ],
        data: vec![6u8],
    };

    env.set_slot(TEST_BOOTSTRAP_START_SLOT + TEST_BOOTSTRAP_DELAY_SLOTS - 1);
    assert!(
        env.send(&[attest(env.gv_config, env.sub_pool)], &[])
            .is_err(),
        "terminal custody cannot bypass the committed bootstrap deadline"
    );

    let foreign_pool = Pubkey::new_unique();
    let mut foreign_data = vec![0u8; 192];
    foreign_data[..8].copy_from_slice(b"SUBPOOL1");
    env.svm
        .set_account(
            foreign_pool,
            solana_sdk::account::Account {
                lamports: 1_000_000,
                data: foreign_data,
                owner: env.sub_pid,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    env.set_slot(TEST_BOOTSTRAP_START_SLOT + TEST_BOOTSTRAP_DELAY_SLOTS);
    assert!(
        env.send(&[attest(env.gv_config, foreign_pool)], &[])
            .is_err(),
        "a same-program foreign pool cannot satisfy terminal finality"
    );
    env.send(&[attest(env.gv_config, env.sub_pool)], &[])
        .expect("the canonical pool attests at the exact bootstrap end");
}

#[test]
fn init_config_schedule_cannot_be_first_writer_shortened_or_started_early() {
    let intended_delay = 100u64;
    let intended_start = 1_000u64;
    let mut env = Env::new_unwired_with_schedule(intended_delay, intended_start);

    assert!(
        env.init_gv_with_schedule(1, intended_start).is_err(),
        "a short-delay first writer must land outside the intended schedule-bound config"
    );
    assert!(
        env.init_gv_with_schedule(intended_delay, 0).is_err(),
        "an early-start first writer must land outside the intended schedule-bound config"
    );
    assert!(
        env.svm.get_account(&env.gv_config).is_none(),
        "the intended config PDA remains uninitialized after wrong-schedule attempts"
    );
    env.init_gv_with_schedule(intended_delay, intended_start)
        .expect("the intended schedule initializes the intended config PDA");

    let recipient = Pubkey::new_unique();
    let dist_proposal = env.create_dist_proposal(1, &[(recipient, 100)]);
    let gv_proposal = env.register(&dist_proposal);
    env.set_pool_outstanding(10);
    env.inject_tally(&gv_proposal, 10, 8, 10, 8, 10);

    env.set_slot(intended_start + intended_delay - 1);
    assert!(
        env.trigger(&gv_proposal, &dist_proposal).is_err(),
        "trigger must wait until the explicit bootstrap start plus delay"
    );
    env.set_slot(intended_start + intended_delay);
    env.trigger(&gv_proposal, &dist_proposal)
        .expect("trigger succeeds at the explicit bootstrap end");
    assert_eq!(env.dist_sealed_proposal(), dist_proposal, "the scheduled genesis seals");
}

// SUBSTITUTED-POOL QUORUM COLLAPSE (no-capital / minority capture via account substitution). The trigger
// re-reads the LIVE pool outstanding as the quorum denominator (the deliberate fix vs a stale cache). That
// design is only safe if the pool ACCOUNT is bound: if the trigger trusted whatever pool the cranker
// passes, an attacker with a tiny minority of voted principal could pass a FOREIGN, zero-outstanding pool
// so `total_voted_principal*2 > 0` trivially clears quorum -> a minority seals the entire COIN
// distribution. lib.rs:761 binds the pool to (owner == subledger_program AND key == config.subledger_pool).
// Every existing trigger test passes the canonical pool; the substitution reject side was untested.
#[test]
fn trigger_rejects_a_substituted_pool_that_would_collapse_the_quorum() {
    let mut env = Env::new();
    let alice = Pubkey::new_unique();
    let bob = Pubkey::new_unique();
    let dist_proposal = env.create_dist_proposal(1, &[(alice, 60), (bob, 40)]);
    let gv_proposal = env.register(&dist_proposal);

    // Real pool: large outstanding (100). A minority (5) voted but holds a clear weighted majority, so
    // ONLY the principal quorum stands between the attacker and a seal.
    env.set_pool_outstanding(100);
    env.inject_tally(&gv_proposal, 5, 8, 100, 8, 5); // voted 5 of 100 -> 5*2=10 !> 100 (no quorum); majority 8 of 8
    assert!(env.trigger(&gv_proposal, &dist_proposal).is_err(), "minority cannot seal against the real pool");
    assert_eq!(env.dist_sealed_proposal(), Pubkey::default(), "not sealed");

    // ATTACK: substitute a foreign, ZERO-outstanding pool to collapse the quorum denominator. Were the
    // passed pool trusted, quorum would read 0 and 5*2=10 > 0 -> seal. The canonical-pool binding rejects
    // any pool whose key != config.subledger_pool (here a fresh key, though correctly subledger-owned).
    let foreign_pool = Pubkey::new_unique();
    let mut data = vec![0u8; 192];
    data[..8].copy_from_slice(b"SUBPOOL1");
    data[8..40].copy_from_slice(env.coin_mint.as_ref());
    data[80..88].copy_from_slice(&0u64.to_le_bytes()); // outstanding = 0 (the bypass value)
    data[160..192].copy_from_slice(env.gv_config.as_ref());
    env.svm
        .set_account(
            foreign_pool,
            solana_sdk::account::Account { lamports: 1_000_000, data, owner: env.sub_pid, executable: false, rent_epoch: 0 },
        )
        .unwrap();

    let ix = Instruction {
        program_id: gv_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new(env.gv_config, false),
            AccountMeta::new(gv_proposal, false),
            AccountMeta::new_readonly(dist_id(), false),
            AccountMeta::new(env.dist_config, false),
            AccountMeta::new(dist_proposal, false),
            AccountMeta::new_readonly(foreign_pool, false), // substituted zero-outstanding pool
        ],
        data: vec![4u8],
    };
    assert!(env.send(&[ix], &[]).is_err(), "trigger must reject a non-canonical pool (quorum cannot be collapsed by substitution)");
    assert_eq!(env.dist_sealed_proposal(), Pubkey::default(), "still not sealed after the substitution attempt");
}

// STRICT MAJORITY/QUORUM (a tie is NOT enough): trigger requires total_voted_principal*2 > live_outstanding
// AND support_weight*2 > total_cast_weight (lib.rs:743,748 — strict `* 2 <= ... -> reject`). The existing
// happy/sad tests use clearly-below (4/10) and clearly-above (10/10); the EXACT-50% boundary was untested.
// A `>` -> `>=` regression would let a MINORITY that holds exactly half the principal, or a proposal with
// exactly half the cast weight, capture the entire COIN distribution — winner-take-all on a tie.
#[test]
fn trigger_requires_a_strict_majority_and_quorum_not_a_tie() {
    let mut env = Env::new();
    let alice = Pubkey::new_unique();
    let dist_proposal = env.create_dist_proposal(1, &[(alice, 100)]);
    let gv_proposal = env.register(&dist_proposal);
    env.set_pool_outstanding(10); // live quorum denominator

    // EXACTLY 50% principal quorum: voted 5 of 10 -> 5*2 == 10, NOT > 10. A tie is not a quorum.
    // (Majority is satisfied to isolate the quorum boundary.)
    env.inject_tally(&gv_proposal, 5, 8, 10, 8, 5);
    assert!(env.trigger(&gv_proposal, &dist_proposal).is_err(), "exactly-half principal is NOT a quorum");
    assert_eq!(env.dist_sealed_proposal(), Pubkey::default(), "not sealed on a tie quorum");

    // EXACTLY 50% weighted majority: quorum satisfied (10 of 10), support 4 of cast 8 -> 4*2 == 8, NOT > 8.
    env.inject_tally(&gv_proposal, 10, 8, 10, 4, 10);
    assert!(env.trigger(&gv_proposal, &dist_proposal).is_err(), "exactly-half cast weight is NOT a majority");
    assert_eq!(env.dist_sealed_proposal(), Pubkey::default(), "not sealed on a tie majority");

    // One unit past BOTH ties: voted 6 of 10 (12 > 10) and support 5 of cast 8 (10 > 8) -> seals.
    env.inject_tally(&gv_proposal, 6, 8, 10, 5, 6);
    env.trigger(&gv_proposal, &dist_proposal).expect("a STRICT majority + quorum seals");
    assert_eq!(env.dist_sealed_proposal(), dist_proposal, "strict majority + quorum seals the winner");
}

// REINIT DOS: init_config is permissionless. If an already-initialized gv config could be
// re-initialized, the second init would RESET the global tallies (total_voted_principal,
// total_cast_weight, outstanding) to 0 while every voter's ballot PDA + subledger vote-lock
// persists — desyncing the genesis: it could never reach quorum again (permanent DOS), and an
// in-flight winning vote would be silently wiped. The `data_len() != 0 -> AccountAlreadyInitialized`
// gate blocks the second init. (Parallel of the subledger `insurance_pool_cannot_be_reinitialized_
// after_funding`, finding AJ, for the genesis governance config.)
#[test]
fn gv_config_cannot_be_reinitialized_to_wipe_a_vote() {
    let mut env = Env::new(); // gv config already initialized + wired
    let alice = Pubkey::new_unique();
    let dist_proposal = env.create_dist_proposal(1, &[(alice, 100)]);
    let gv_proposal = env.register(&dist_proposal);
    env.set_pool_outstanding(10);
    env.inject_tally(&gv_proposal, 10, 8, 10, 8, 10); // a quorum+majority vote is in progress

    // ATTACK: re-init the live config to zero its tallies.
    assert!(env.init_gv().is_err(), "an initialized gv config cannot be re-initialized");

    // The vote is intact: the genesis triggers + seals exactly as if the re-init never happened.
    env.trigger(&gv_proposal, &dist_proposal).expect("vote survived the rejected re-init");
    assert_eq!(env.dist_sealed_proposal(), dist_proposal, "winner sealed — re-init could not reset the tally");
}

// Griefing-DOS boundary: register is permissionless EXCEPT it binds to the distribution
// proposal's creator (lib.rs:471). The gv_proposal is a UNIQUE PDA f(config, dist_proposal),
// and register freezes a (entry_count, total_amount) SNAPSHOT that `trigger` later requires to
// match exactly. So if a non-creator could register a victim's PARTIALLY-built proposal early,
// it would (a) seize the only gv_proposal PDA (the creator can't re-register — AccountAlready-
// Initialized) and (b) freeze a stale snapshot; the creator's remaining appends would then make
// the live proposal mismatch the snapshot forever, so trigger could NEVER seal it — the victim's
// distribution is permanently unwinnable. The creator-binding blocks this.
#[test]
fn register_rejects_a_non_creator_front_runner() {
    let mut env = Env::new();
    // The creator (env.payer) builds its proposal.
    let alice = Pubkey::new_unique();
    let dist_proposal = env.create_dist_proposal(1, &[(alice, 100)]);

    // An attacker (different signer) tries to register the victim's proposal to seize the PDA
    // and freeze the snapshot. Refused: the dist proposal's creator is env.payer, not them.
    let attacker = Keypair::new();
    env.svm.airdrop(&attacker.pubkey(), 1_000_000_000).unwrap();
    assert!(
        env.register_as(&dist_proposal, &attacker).is_err(),
        "a non-creator must not be able to register someone else's distribution proposal"
    );

    // The genuine creator registers successfully — the gv_proposal PDA was never seized.
    let gv_proposal = env.register(&dist_proposal);
    // And it is fully usable: a quorum+majority tally seals it (PDA + snapshot are the creator's).
    env.set_pool_outstanding(10);
    env.inject_tally(&gv_proposal, 10, 8, 10, 8, 10);
    env.trigger(&gv_proposal, &dist_proposal).expect("creator's proposal still seals");
    assert_eq!(env.dist_sealed_proposal(), dist_proposal, "creator's distribution sealed");
}

// EMPTY-PROPOSAL REGISTRATION (incomplete distribution): register requires entry_count > 0 (lib.rs:476) so
// only a FULLY-built proposal becomes votable. An empty proposal (entry_count == 0) would freeze a (0, 0)
// snapshot: if it won, the distribution names no recipients and the entire funded vault burns unclaimed; or,
// registering empty then appending would make the live proposal mismatch the (0,0) snapshot forever, so
// trigger could never seal it (a permanently-unwinnable, vote-soaking proposal). The guard blocks it.
#[test]
fn register_rejects_an_empty_proposal() {
    let mut env = Env::new();
    // A proposal created but never appended to: entry_count == 0.
    let empty = env.create_dist_proposal(1, &[]);
    let creator = clone_kp(&env.payer);
    assert!(
        env.register_as(&empty, &creator).is_err(),
        "an empty proposal (entry_count == 0) must not be registerable for voting"
    );
    let gv_empty = env.gv_proposal_pda(&empty);
    assert!(env.svm.get_account(&gv_empty).is_none(), "no gv proposal-vote account created for the empty proposal");

    // A non-empty proposal by the same creator registers fine — the gate is emptiness, not the creator.
    let full = env.create_dist_proposal(2, &[(Pubkey::new_unique(), 100)]);
    env.register(&full);
}

// Defense in depth: current registrations require a full declared shape, so no
// public append can change the proposal afterward. The trigger snapshot still
// rejects a corrupted or historical proposal whose header changes after registration.
#[test]
fn trigger_refuses_a_distribution_inflated_after_registration() {
    let mut env = Env::new();
    let alice = Pubkey::new_unique();
    // Honest distribution voters approve: 60 to alice, the remaining 40 of the 100 supply is burned.
    let dist_proposal = env.create_dist_proposal(1, &[(alice, 60)]);
    let gv_proposal = env.register(&dist_proposal); // snapshot frozen at (entry_count=1, total=60)
    env.set_pool_outstanding(10);
    env.inject_tally(&gv_proposal, 10, 8, 10, 8, 10); // quorum + majority on the HONEST proposal

    let mut changed = env.svm.get_account(&dist_proposal).unwrap();
    changed.data[84..88].copy_from_slice(&2u32.to_le_bytes());
    changed.data[88..96].copy_from_slice(&100u64.to_le_bytes());
    env.svm.set_account(dist_proposal, changed).unwrap();

    // The trigger must now REFUSE: the live (entry_count=2, total=100) no longer matches the frozen
    // snapshot (1, 60). The voters' approved distribution can never be silently inflated.
    assert!(
        env.trigger(&gv_proposal, &dist_proposal).is_err(),
        "trigger must refuse a distribution that changed after registration"
    );
    assert_eq!(env.dist_sealed_proposal(), Pubkey::default(), "nothing sealed — the rug was blocked, not paid out");
}

// OFFSET CANARY: registration reads capacity and trigger reads entry_count plus
// total_amount from the real distribution proposal layout.
#[test]
fn gv_distribution_snapshot_offsets_match_the_real_distribution_proposal_layout() {
    let mut env = Env::new();
    let entries = [
        (Pubkey::new_unique(), 10u64),
        (Pubkey::new_unique(), 20u64),
        (Pubkey::new_unique(), 30u64),
    ];
    let proposal = env.create_dist_proposal(7, &entries); // 3 entries, total 60, via the real distribution .so
    let data = env.svm.get_account(&proposal).unwrap().data;
    let capacity = u32::from_le_bytes(
        data[genesis_vote_program::DIST_PROPOSAL_CAPACITY_OFF
            ..genesis_vote_program::DIST_PROPOSAL_ENTRY_COUNT_OFF]
            .try_into()
            .unwrap(),
    );
    let entry_count = u32::from_le_bytes(
        data[genesis_vote_program::DIST_PROPOSAL_ENTRY_COUNT_OFF
            ..genesis_vote_program::DIST_PROPOSAL_TOTAL_AMOUNT_OFF]
            .try_into()
            .unwrap(),
    );
    let total_amount = u64::from_le_bytes(
        data[genesis_vote_program::DIST_PROPOSAL_TOTAL_AMOUNT_OFF
            ..genesis_vote_program::DIST_PROPOSAL_TOTAL_AMOUNT_OFF + 8]
            .try_into()
            .unwrap(),
    );
    assert_eq!(capacity, 3);
    assert_eq!(entry_count, 3);
    assert_eq!(total_amount, 60);
}

// COIN-SUPPLY REDIRECT (trigger substitutes a sibling distribution proposal): trigger is permissionless
// and CPIs SealWinner with whatever `distribution_proposal` account the caller passes. The ONLY thing
// binding the seal to the proposal voters actually backed is `*distribution_proposal.key !=
// pv.distribution_proposal` (lib.rs trigger). Attack: register the legit proposal P (bound to winner G),
// drive G to quorum+majority, then trigger(G, Q) where Q is the ATTACKER's sibling — created under the
// SAME distribution config, with the SAME (entry_count, total_amount) = P's registered snapshot, but the
// attacker as the sole recipient. Mutation-sharpness: the snapshot match is deliberate — Q's (1, 100)
// equals G's snapshot, so the bait-and-switch snapshot guard would NOT catch it; only the key-binding
// check stops Q. If that check regressed, trigger would seal Q (Q.config == dist_config, total <= supply)
// and the attacker would mint the WHOLE genesis COIN supply to themselves. Must be rejected, and the
// honest trigger(G, P) must still seal P afterwards (proving only the redirect was blocked, not the win).
#[test]
fn trigger_cannot_redirect_to_a_sibling_distribution_proposal() {
    let mut env = Env::new();
    let legit = Pubkey::new_unique();
    let attacker = Pubkey::new_unique();
    // P: the proposal voters back. Q: the attacker's sibling — same shape (1 entry, total 100), but pays
    // the attacker. Both bound to env.dist_config (create_dist_proposal uses self.dist_config).
    let p = env.create_dist_proposal(1, &[(legit, 100)]);
    let q = env.create_dist_proposal(2, &[(attacker, 100)]);
    let g = env.register(&p); // pv.distribution_proposal = P, snapshot = (1, 100)

    env.set_pool_outstanding(10);
    env.inject_tally(&g, 10, 8, 10, 8, 10); // quorum (10*2 > 10) + strict majority (support 8*2 > cast 8)

    // ATTACK: trigger the real winner G but hand it the attacker's sibling Q.
    assert!(
        env.trigger(&g, &q).is_err(),
        "trigger must refuse a distribution proposal other than the one bound to the winning gv proposal"
    );
    assert_eq!(env.dist_sealed_proposal(), Pubkey::default(), "nothing sealed — the supply was not redirected");

    // The honest trigger (G, P) still works: only the redirect was blocked, the win stands.
    env.trigger(&g, &p).expect("the genuine winner still seals its own bound proposal");
    assert_eq!(env.dist_sealed_proposal(), p, "the proposal voters actually backed is the one sealed");
}

// CROSS-CONFIG BINDING at register (votable-but-unsealable genesis-stall DOS): register binds the
// distribution proposal to THIS genesis's distribution config (lib.rs:459, header.config[8..40] ==
// config.distribution_config). Without it, a proposal created under a DIFFERENT (e.g. attacker-owned)
// distribution config could be registered + voted on; if it then won, trigger would CPI
// SealWinner(our_dist_config, foreign_proposal) which the distribution program rejects (header.config
// mismatch) — and since the win is winner-take-all, no other proposal could seal either, stalling the
// genesis forever. The creator-binding test (register_rejects_a_non_creator_front_runner) pins the
// creator half; this pins the config half. We plant a proposal that is valid in EVERY other respect
// (right disc, creator == payer, non-empty) so the ONLY rejecting check is the config binding.
#[test]
fn register_rejects_a_proposal_from_a_foreign_distribution_config() {
    let mut env = Env::new();
    let foreign_config = Pubkey::new_unique(); // NOT env.dist_config
    let bad_proposal = Pubkey::new_unique();

    // A distribution-program-owned proposal header: disc DISTPRP1, config = FOREIGN, creator = payer
    // (so the creator-binding passes), capacity 1, entry_count 1, total 100 — fully registerable but
    // for the config.
    let mut data = vec![0u8; 257];
    data[..8].copy_from_slice(b"DISTPRP1");
    data[8..40].copy_from_slice(foreign_config.as_ref());
    data[48..80].copy_from_slice(env.payer.pubkey().as_ref()); // creator
    data[80..84].copy_from_slice(&1u32.to_le_bytes()); // capacity
    data[84..88].copy_from_slice(&1u32.to_le_bytes()); // entry_count (non-empty)
    data[88..96].copy_from_slice(&100u64.to_le_bytes()); // total_amount
    env.svm
        .set_account(
            bad_proposal,
            solana_sdk::account::Account {
                lamports: 2_000_000,
                data,
                owner: dist_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    // The genuine creator (payer) tries to register it — refused purely on the config binding, so it
    // never becomes votable and can never stall finalize.
    let creator = clone_kp(&env.payer);
    assert!(
        env.register_as(&bad_proposal, &creator).is_err(),
        "a proposal under a foreign distribution config must not be registerable for voting"
    );
    let gv_proposal = env.gv_proposal_pda(&bad_proposal);
    assert!(env.svm.get_account(&gv_proposal).is_none(), "no gv proposal-vote account was created");
}

// LAMPORT PRE-FUND INIT-DOS (finding AI), genesis-vote config: the gv config PDA is
// deterministic (f(coin_mint, subledger_pool), both public), and init_config is permissionless.
// System `create_account` aborts with AccountAlreadyInUse on ANY pre-existing lamports, so an
// attacker could transfer 1 lamport to the gv config PDA (no signature needed) BEFORE the genesis
// orchestrator inits it — permanently bricking the genesis GOVERNANCE config (no config -> no
// voting/trigger -> the whole genesis stalls), and the dust can never be swept from a system-owned
// PDA. gv's create_pda is robust (top-up the rent shortfall, then allocate + assign via
// invoke_signed, which only need data-empty + system-owned), so it tolerates the pre-funding.
// (The subledger pool + twap book inits have the same guard + their own tests; this pins the gv
// config init, the central governance account.)
#[test]
fn lamport_prefund_cannot_brick_gv_config_init() {
    let mut env = Env::new_unwired(); // dist config + pool wired; gv config NOT yet inited
    // Attacker dust on the deterministic gv config PDA.
    env.svm
        .set_account(
            env.gv_config,
            solana_sdk::account::Account {
                lamports: 1,
                data: vec![],
                owner: solana_sdk::system_program::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    // Init must STILL succeed (robust create handles the pre-funded PDA).
    env.init_gv().expect("robust create tolerates the dusted gv config PDA");
    // The config is genuinely initialized + usable: it is now owned by the gv program with data,
    // and a real proposal registers + seals against it.
    let cfg = env.svm.get_account(&env.gv_config).unwrap();
    assert_eq!(cfg.owner, gv_id(), "gv config now owned by the program");
    assert!(!cfg.data.is_empty(), "gv config initialized despite the dust");
    let dist_proposal = env.create_dist_proposal(1, &[(Pubkey::new_unique(), 100)]);
    let gv_proposal = env.register(&dist_proposal);
    env.set_pool_outstanding(10);
    env.inject_tally(&gv_proposal, 10, 8, 10, 8, 10);
    env.trigger(&gv_proposal, &dist_proposal).expect("genesis proceeds normally after a dusted init");
}

/// Regression: the quorum denominator is the LIVE subledger pool outstanding, not
/// the cached config value (synced only on votes). A minority that voted early
/// while the pool was small cannot capture the distribution after honest deposits
/// grow the pool without a re-vote.
#[test]
fn trigger_uses_live_pool_outstanding_not_stale_cache() {
    let mut env = Env::new();
    let alice = Pubkey::new_unique();
    let dist_proposal = env.create_dist_proposal(1, &[(alice, 100)]);
    let gv_proposal = env.register(&dist_proposal);

    // The attacker voted early with 6 when the pool was tiny: the CACHED config
    // outstanding is a stale 6 (6*2=12 > 6 would "pass" against the cache).
    env.inject_tally(&gv_proposal, 6, 8, 6, 8, 6);

    // ...but honest depositors have since grown the LIVE pool to 1006 without a
    // re-vote. The trigger reads the live pool, so 6*2=12 is NOT > 1006 -> rejected.
    env.set_pool_outstanding(1006);
    assert!(
        env.trigger(&gv_proposal, &dist_proposal).is_err(),
        "stale-cache minority capture must be blocked by the live-pool quorum read"
    );
    assert_eq!(env.dist_sealed_proposal(), Pubkey::default(), "not sealed");

    // Once a real quorum forms against the live pool (e.g. the pool shrinks back to
    // 10 via exits, or enough principal votes), the trigger proceeds.
    env.set_pool_outstanding(10);
    env.trigger(&gv_proposal, &dist_proposal).expect("trigger seals at real quorum");
    assert_eq!(env.dist_sealed_proposal(), dist_proposal);
}

// Winner-take-all is irreversible across COMPETING proposals. The single-proposal
// re-trigger is blocked by `pv.executed`; this pins the DISTINCT, defense-in-depth
// boundary: two proposals share ONE distribution config, and once proposal A seals,
// proposal B must not be able to seal a DIFFERENT distribution — even if B's gv tally
// is made to look winning (e.g. a post-execution weight-shift: voters may retract from
// the executed A, dropping total_cast_weight, then pile weight onto B). The true gate
// is the distribution `seal_winner`'s is_sealed() check: B's trigger passes every gv
// check, sets pv_B.executed, then the seal CPI fails because the config is already
// sealed — reverting B's trigger whole. So there is exactly one sealed distribution.
#[test]
fn a_second_proposal_cannot_reseal_after_a_winner_is_sealed() {
    let mut env = Env::new();
    let alice = Pubkey::new_unique();
    let bob = Pubkey::new_unique();
    // Two distinct distribution proposals under the SAME dist config.
    let prop_a = env.create_dist_proposal(1, &[(alice, 100)]);
    let prop_b = env.create_dist_proposal(2, &[(bob, 100)]);
    let gv_a = env.register(&prop_a);
    let gv_b = env.register(&prop_b);
    env.set_pool_outstanding(10);

    // A reaches quorum + weighted majority and seals.
    env.inject_tally(&gv_a, 10, 8, 10, 8, 10);
    env.trigger(&gv_a, &prop_a).expect("A triggers + seals");
    assert_eq!(env.dist_sealed_proposal(), prop_a, "A is the sealed winner");

    // Now make B ALSO look winning at the gv layer (simulating a post-seal weight
    // shift onto B). B passes every genesis-vote check, but the distribution is
    // already sealed, so the seal_winner CPI rejects and B's trigger reverts.
    env.inject_tally(&gv_b, 10, 8, 10, 8, 10);
    assert!(
        env.trigger(&gv_b, &prop_b).is_err(),
        "a second proposal must not be able to reseal a different distribution"
    );
    // The sealed winner is unchanged: exactly one distribution, A's.
    assert_eq!(env.dist_sealed_proposal(), prop_a, "still A — winner-take-all is irreversible");
}

// Setup-integrity: InitConfig must refuse to wire the genesis to a subledger pool
// whose vote_authority is NOT this config's PDA. Otherwise an honest orchestrator
// could bind to a poisoned/foreign pool (cf. finding G): votes' SetVoteLock CPI
// would fail (vote_authority mismatch), bricking the whole genesis. Failing fast at
// init makes the misconfiguration impossible to miss.
#[test]
fn init_config_rejects_pool_not_bound_to_this_config() {
    let mut env = Env::new_unwired();

    // Pool's vote_authority is an attacker key, not this gv config PDA.
    let attacker = Pubkey::new_unique();
    env.poison_pool_vote_authority(&attacker);
    assert!(env.init_gv().is_err(), "must refuse a pool not bound to this config");

    // Repair the binding -> init now succeeds.
    let gv = env.gv_config;
    env.poison_pool_vote_authority(&gv);
    env.init_gv().expect("a correctly-bound pool is accepted");
}

// Finding H regression (distribution side, the parallel of the pool-binding negative
// above): init_config must refuse to wire the genesis to a distribution config whose seal
// `authority` is NOT this gv config PDA, or that distributes a DIFFERENT coin. Otherwise an
// attacker front-running the permissionless init_config could bind the genesis to a
// distribution it does NOT control the seal of — making the trigger's seal CPI fail
// (authority mismatch) and bricking finalize (DOS), or pointing the genesis at the wrong
// COIN. The honest distribution's own seed binds its authority and claim window (finding P/AA:
// dist_config = f(coin, authority, claim_window)), so the ONLY distribution that satisfies
// `authority == gv PDA` under the configured deadline is the real one whose funded vault holds the
// COIN — which an attacker cannot forge.
#[test]
fn init_config_rejects_a_distribution_not_authority_bound_to_this_config() {
    // (a) right coin, but seal authority is an attacker key (not this gv config PDA).
    let mut env = Env::new_unwired();
    let foreign = Pubkey::new_unique();
    let attacker = Pubkey::new_unique();
    env.plant_foreign_dist(foreign, env.coin_mint, attacker);
    assert!(
        env.init_gv_with_dist(foreign).is_err(),
        "must refuse a distribution whose seal authority is not this gv config"
    );

    // (b) seal authority correctly = this gv config PDA, but a DIFFERENT coin_mint.
    let gv = env.gv_config;
    let wrong_coin = Pubkey::new_unique();
    env.plant_foreign_dist(foreign, wrong_coin, gv);
    assert!(
        env.init_gv_with_dist(foreign).is_err(),
        "must refuse a distribution for a different coin even if authority-bound"
    );

    // The real, authority+coin-bound distribution is accepted — the boundary is exact,
    // not a blanket reject.
    let real = env.dist_config;
    env.init_gv_with_dist(real).expect("the authority+coin-bound distribution is accepted");
}

// INIT SQUAT via a FAKE distribution PROGRAM (finalize DOS, finding IC; the gv dual of residual's HK):
// init_config bound the distribution_config by `owner == <the PASSED distribution_program>` + coin +
// authority==this-config, but did NOT pin the distribution_program to the canonical program ID. The
// gv_config PDA is predictable (f(coin_mint, pool)), so an attacker could deploy a trivial fake
// "distribution program", craft a config OWNED BY it that satisfies every byte check (disc, coin, and
// authority == the deterministic gv PDA), and front-run init_config to SQUAT the canonical gv_config.
// The squat redirects nothing — the trigger's stored fake program cannot touch the REAL COIN vault —
// but it permanently bricks sealing the real distribution through this gv_config: a recoverable-only-
// by-re-mint finalize DOS. Mirrors residual's HK (rd_init_rejects_a_fake_distribution_program).
#[test]
fn init_rejects_a_fake_distribution_program() {
    let mut env = Env::new_unwired();
    // A config owned by a FAKE program, with otherwise-perfect bytes (coin + authority == gv PDA).
    let fake_prog = Pubkey::new_unique();
    let fake_cfg = Pubkey::new_unique();
    let gv = env.gv_config;
    env.plant_dist_owned_by(fake_cfg, fake_prog, env.coin_mint, gv);
    assert!(
        env.init_gv_with_prog_and_dist(fake_prog, fake_cfg).is_err(),
        "gv init must reject a non-canonical distribution program (anti front-run squat)"
    );
    // Boundary check: the SAME bytes under the REAL distribution program are accepted — the reject is
    // the program-pin, not some unrelated failure. (The real dist_config is authority+coin+window bound.)
    let real = env.dist_config;
    env.init_gv_with_dist(real).expect("the canonical distribution program + bound config is accepted");
}

// Finding R regression: the gv config PDA now commits to its subledger_pool. init_config
// is permissionless, and the distribution config it binds is a UNIQUE PDA f(COIN, gv PDA,
// claim_window) whose seal authority is pinned to one gv PDA. So a genesis can be wired to exactly
// ONE pool — the one the real distribution's authority commits to. An attacker cannot front-run
// init_config to bind the genesis to a DIFFERENT (their own) valid pool: doing so makes
// `expected` = f(COIN, attacker_pool), which no longer matches the distribution's pinned
// authority, so the binding is refused. (Pre-fix the gv PDA was f(COIN) regardless of the
// pool, so a front-run could bind the real distribution to an attacker pool and misroute
// every deposit.)
#[test]
fn gv_config_cannot_be_bound_to_a_substituted_pool() {
    let mut env = Env::new(); // real gv config bound to env.sub_pool; dist authority = that gv PDA

    // The gv config PDA now commits to the pool: it is NOT the old market-only address.
    // (This assertion would fail before the finding-R fix, where gv config = f(COIN).)
    let coin_only = Pubkey::find_program_address(&[b"gv_config", env.coin_mint.as_ref()], &gv_id()).0;
    assert_ne!(env.gv_config, coin_only, "gv config PDA commits to the subledger_pool (finding R)");
    let unscheduled = Pubkey::find_program_address(
        &[b"gv_config", env.coin_mint.as_ref(), env.sub_pool.as_ref()],
        &gv_id(),
    )
    .0;
    assert_ne!(env.gv_config, unscheduled, "gv config PDA commits to the bootstrap schedule");

    // An attacker's OWN valid insurance pool at a different address, with vote_authority
    // set to the gv PDA *that* pool would imply — so the pool's own binding check passes.
    let attacker_pool = Pubkey::new_from_array([9u8; 32]);
    let attacker_gv = gv_config_for_schedule(
        &env.coin_mint,
        &attacker_pool,
        TEST_BOOTSTRAP_DELAY_SLOTS,
        TEST_BOOTSTRAP_START_SLOT,
    );
    let mut data = vec![0u8; 192];
    data[..8].copy_from_slice(b"SUBPOOL1");
    data[8..40].copy_from_slice(env.coin_mint.as_ref());
    data[160..192].copy_from_slice(attacker_gv.as_ref());
    env.svm
        .set_account(
            attacker_pool,
            solana_sdk::account::Account {
                lamports: 1_000_000,
                data,
                owner: env.sub_pid,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    assert_ne!(attacker_gv, env.gv_config, "the pool is part of the gv config PDA");

    // Attacker tries to init a gv config bound to THEIR pool, reusing the real (unique)
    // distribution config. expected = f(COIN, attacker_pool) = attacker_gv, but the
    // distribution's seal authority is the REAL gv PDA -> the distribution binding fails.
    let dummy = Pubkey::new_unique();
    let ix = Instruction {
        program_id: gv_id(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(env.coin_mint, false),
            AccountMeta::new(attacker_gv, false),
            AccountMeta::new_readonly(dist_id(), false),
            AccountMeta::new_readonly(env.dist_config, false),
            AccountMeta::new_readonly(env.sub_pid, false),
            AccountMeta::new_readonly(attacker_pool, false),
            AccountMeta::new_readonly(dummy, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: gv_init_data(TEST_BOOTSTRAP_DELAY_SLOTS),
    };
    assert!(
        env.send(&[ix], &[]).is_err(),
        "the genesis cannot be bound to a substituted pool the distribution does not commit to"
    );
}
