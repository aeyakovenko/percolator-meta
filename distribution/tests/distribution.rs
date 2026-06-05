//! End-to-end litesvm tests: fixed COIN vault, proposal list, authority-gated
//! seal, permissionless per-recipient claim (pull, indexed), and burn-unclaimed
//! after the claim window.

use litesvm::LiteSVM;
use solana_sdk::{
    account::Account,
    clock::Clock,
    instruction::{AccountMeta, Instruction},
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction,
    transaction::Transaction,
};

fn pid() -> Pubkey {
    distribution_program::id()
}
fn so_path() -> String {
    format!("{}/../target/deploy/distribution_program.so", env!("CARGO_MANIFEST_DIR"))
}
fn clone_kp(kp: &Keypair) -> Keypair {
    Keypair::from_bytes(&kp.to_bytes()).unwrap()
}

struct Env {
    svm: LiteSVM,
    payer: Keypair,
    coin_mint: Pubkey,
    mint_authority: Keypair,
    config: Pubkey,
    vault: Pubkey,
    authority: Keypair,
}

impl Env {
    /// Sets up a COIN mint, a config PDA + vault holding `supply` COIN, and runs
    /// InitConfig with the given claim window.
    fn new(supply: u64, claim_window: u64) -> Self {
        let mut svm = LiteSVM::new();
        svm.add_program_from_file(pid(), so_path()).unwrap();
        let payer = Keypair::new();
        svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
        let mint_authority = Keypair::new();
        let coin_mint = create_mint(&mut svm, &payer, &mint_authority.pubkey());

        // The config PDA binds the authority (anti front-run, finding AA), so derive it after.
        let authority = Keypair::new();
        let config = Pubkey::find_program_address(&[b"dist_config", coin_mint.as_ref(), authority.pubkey().as_ref()], &pid()).0;
        let vault = create_token_account(&mut svm, &payer, &coin_mint, &config);
        mint_to(&mut svm, &payer, &coin_mint, &mint_authority, &vault, supply);
        // Fixed supply: revoke the mint authority before init (the canonical
        // genesis-setup flow; distribution requires a non-mintable COIN).
        revoke_mint_authority(&mut svm, &payer, &coin_mint, &mint_authority);

        let mut env = Env { svm, payer, coin_mint, mint_authority, config, vault, authority };

        let mut data = vec![0u8]; // IX_INIT_CONFIG
        data.extend_from_slice(&claim_window.to_le_bytes());
        data.extend_from_slice(&supply.to_le_bytes());
        let auth = env.authority.pubkey();
        let ix = Instruction {
            program_id: pid(),
            accounts: vec![
                AccountMeta::new(env.payer.pubkey(), true),
                AccountMeta::new_readonly(env.coin_mint, false),
                AccountMeta::new(env.config, false),
                AccountMeta::new_readonly(env.vault, false),
                AccountMeta::new_readonly(auth, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data,
        };
        env.send(&[ix], &[]).expect("init config");
        env
    }

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
        spl_token::state::Account::unpack(&self.svm.get_account(account).unwrap().data).unwrap().amount
    }

    fn set_slot(&mut self, slot: u64) {
        self.svm.set_sysvar(&Clock { slot, ..Default::default() });
    }

    fn proposal_pda(&self, id: u64) -> Pubkey {
        Pubkey::find_program_address(
            &[b"dist_proposal", self.config.as_ref(), &id.to_le_bytes()],
            &pid(),
        )
        .0
    }

    fn create_proposal(&mut self, id: u64, capacity: u32) -> Pubkey {
        let proposal = self.proposal_pda(id);
        let mut data = vec![1u8]; // IX_CREATE_PROPOSAL
        data.extend_from_slice(&id.to_le_bytes());
        data.extend_from_slice(&capacity.to_le_bytes());
        let ix = Instruction {
            program_id: pid(),
            accounts: vec![
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new_readonly(self.config, false),
                AccountMeta::new(proposal, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data,
        };
        self.send(&[ix], &[]).expect("create proposal");
        proposal
    }

    fn append(&mut self, proposal: &Pubkey, entries: &[(Pubkey, u64)]) -> Result<(), String> {
        let mut data = vec![2u8]; // IX_APPEND_ENTRIES
        data.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for (pk, amt) in entries {
            data.extend_from_slice(pk.as_ref());
            data.extend_from_slice(&amt.to_le_bytes());
        }
        let ix = Instruction {
            program_id: pid(),
            accounts: vec![
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new_readonly(self.config, false),
                AccountMeta::new(*proposal, false),
            ],
            data,
        };
        self.send(&[ix], &[])
    }

    /// append_entries signed by an arbitrary keypair (not self.payer) — to prove the
    /// proposal's creator-binding refuses a foreign appender.
    fn append_as(&mut self, proposal: &Pubkey, signer: &Keypair, entries: &[(Pubkey, u64)]) -> Result<(), String> {
        let mut data = vec![2u8]; // IX_APPEND_ENTRIES
        data.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for (pk, amt) in entries {
            data.extend_from_slice(pk.as_ref());
            data.extend_from_slice(&amt.to_le_bytes());
        }
        let ix = Instruction {
            program_id: pid(),
            accounts: vec![
                AccountMeta::new(signer.pubkey(), true), // creator slot = the attacker
                AccountMeta::new_readonly(self.config, false),
                AccountMeta::new(*proposal, false),
            ],
            data,
        };
        let s = clone_kp(signer);
        self.send(&[ix], &[&s])
    }

    fn seal(&mut self, proposal: &Pubkey, signer: &Keypair) -> Result<(), String> {
        let ix = Instruction {
            program_id: pid(),
            accounts: vec![
                AccountMeta::new_readonly(signer.pubkey(), true),
                AccountMeta::new(self.config, false),
                AccountMeta::new(*proposal, false),
            ],
            data: vec![3u8], // IX_SEAL_WINNER
        };
        let s = clone_kp(signer);
        self.send(&[ix], &[&s])
    }

    fn claim(&mut self, proposal: &Pubkey, recipient: &Keypair, ata: &Pubkey, index: u32) -> Result<(), String> {
        let mut data = vec![4u8]; // IX_CLAIM
        data.extend_from_slice(&index.to_le_bytes());
        let ix = Instruction {
            program_id: pid(),
            accounts: vec![
                AccountMeta::new_readonly(recipient.pubkey(), true),
                AccountMeta::new_readonly(self.config, false),
                AccountMeta::new(*proposal, false),
                AccountMeta::new(self.vault, false),
                AccountMeta::new(*ata, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data,
        };
        let r = clone_kp(recipient);
        self.send(&[ix], &[&r])
    }

    fn burn_unclaimed(&mut self) -> Result<(), String> {
        let ix = Instruction {
            program_id: pid(),
            accounts: vec![
                AccountMeta::new_readonly(self.payer.pubkey(), true),
                AccountMeta::new_readonly(self.config, false),
                AccountMeta::new(self.vault, false),
                AccountMeta::new(self.coin_mint, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data: vec![5u8], // IX_BURN_UNCLAIMED
        };
        self.send(&[ix], &[])
    }

    fn new_recipient(&mut self) -> (Keypair, Pubkey) {
        let kp = Keypair::new();
        self.svm.airdrop(&kp.pubkey(), 10_000_000_000).unwrap();
        let payer = clone_kp(&self.payer);
        let mint = self.coin_mint;
        let ata = create_token_account(&mut self.svm, &payer, &mint, &kp.pubkey());
        (kp, ata)
    }
}

fn revoke_mint_authority(svm: &mut LiteSVM, payer: &Keypair, mint: &Pubkey, authority: &Keypair) {
    let ix = spl_token::instruction::set_authority(
        &spl_token::ID,
        mint,
        None,
        spl_token::instruction::AuthorityType::MintTokens,
        &authority.pubkey(),
        &[],
    )
    .unwrap();
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[payer, authority], svm.latest_blockhash());
    svm.send_transaction(tx).unwrap();
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

fn token_account_bytes(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Vec<u8> {
    let mut data = vec![0u8; spl_token::state::Account::LEN];
    data[0..32].copy_from_slice(mint.as_ref());
    data[32..64].copy_from_slice(owner.as_ref());
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    data[108] = 1; // AccountState::Initialized
    data
}

#[test]
fn seal_then_recipients_claim_their_entries() {
    let mut env = Env::new(100, 1_000_000);
    let proposal = env.create_proposal(1, 4);
    let (alice, alice_ata) = env.new_recipient();
    let (bob, bob_ata) = env.new_recipient();
    env.append(&proposal, &[(alice.pubkey(), 60), (bob.pubkey(), 40)]).expect("append");

    // Claims are blocked until the winner is sealed.
    assert!(env.claim(&proposal, &alice, &alice_ata, 0).is_err(), "no claim before seal");

    // Only the configured authority (the vote/trigger) can seal.
    let imposter = Keypair::new();
    env.svm.airdrop(&imposter.pubkey(), 1_000_000_000).unwrap();
    assert!(env.seal(&proposal, &imposter).is_err(), "non-authority cannot seal");
    let auth = clone_kp(&env.authority);
    env.seal(&proposal, &auth).expect("seal by authority");

    // Each recipient pulls their own entry by index.
    env.claim(&proposal, &alice, &alice_ata, 0).expect("alice claim");
    // RECIPIENT BINDING (anti-theft), asserted while bob's entry is STILL UNCLAIMED so ONLY the
    // pk==recipient check (lib.rs claim) can reject — not the amount==0 double-claim guard: alice
    // (a different, valid recipient) cannot pull bob's index-1 entry to her own ATA. Without the
    // binding she would steal bob's 40. (The later index-1 assertion fires after bob claims, so it
    // only exercises amount==0 — this is the sharp recipient-binding pin.)
    assert!(env.claim(&proposal, &alice, &alice_ata, 1).is_err(), "alice cannot claim bob's UNCLAIMED entry");
    assert_eq!(env.token_amount(&alice_ata), 60, "alice's balance unchanged by the rejected theft");
    env.claim(&proposal, &bob, &bob_ata, 1).expect("bob claim");
    assert_eq!(env.token_amount(&alice_ata), 60);
    assert_eq!(env.token_amount(&bob_ata), 40);
    assert_eq!(env.token_amount(&env.vault.clone()), 0, "vault fully distributed");

    // Double-claim and wrong-recipient claim are rejected.
    assert!(env.claim(&proposal, &alice, &alice_ata, 0).is_err(), "no double claim");
    let attacker_ata = {
        let (_, ata) = env.new_recipient();
        ata
    };
    assert!(env.claim(&proposal, &alice, &attacker_ata, 1).is_err(), "cannot claim bob's entry");
}

// ANTI-REPLAY (entry-zeroing) sharpness: claim zeroes the entry's amount after paying, so a re-claim
// reads amount==0 and is refused. The double-claim assertion in seal_then_recipients_claim above fires
// AFTER the vault is fully drained (alice + bob both claimed), so a removed entry-zeroing would be masked
// by transfer-insufficiency (the vault is empty) — mutation-blind. Here the re-claimer has a SMALL entry
// (10) and re-claims BEFORE the co-recipient claims, so the vault is STILL FUNDED (holds bob's 90). Without
// the entry-zeroing, alice's re-claim would pay her 10 AGAIN out of bob's funds — a cross-user double-spend
// LOF. The zeroed entry must reject the replay; the vault stays whole. (Found mutation-blind.)
#[test]
fn double_claim_cannot_drain_other_recipients_while_the_vault_is_funded() {
    let mut env = Env::new(100, 1_000_000);
    let proposal = env.create_proposal(1, 4);
    let (alice, alice_ata) = env.new_recipient();
    let (bob, bob_ata) = env.new_recipient();
    env.append(&proposal, &[(alice.pubkey(), 10), (bob.pubkey(), 90)]).expect("append");
    let auth = clone_kp(&env.authority);
    env.seal(&proposal, &auth).expect("seal");

    env.claim(&proposal, &alice, &alice_ata, 0).expect("alice claims her 10");
    assert_eq!(env.token_amount(&alice_ata), 10, "alice paid once");
    assert_eq!(env.token_amount(&env.vault.clone()), 90, "vault still holds bob's 90");

    // ATTACK: alice re-claims index 0 while the vault is funded -> the zeroed entry must reject it.
    assert!(env.claim(&proposal, &alice, &alice_ata, 0).is_err(), "a re-claim of a zeroed entry must be rejected");
    assert_eq!(env.token_amount(&alice_ata), 10, "alice did NOT double-claim");
    assert_eq!(env.token_amount(&env.vault.clone()), 90, "vault untouched — bob's 90 not drained by the replay");
    // bob still claims his full 90.
    env.claim(&proposal, &bob, &bob_ata, 1).expect("bob claims his 90");
    assert_eq!(env.token_amount(&bob_ata), 90, "bob recovers his full entry");
}

// REINIT A SEALED CONFIG (vault-redirect, finding AJ for distribution): re-initializing a LIVE, sealed
// config would reset config.sealed_proposal + seal_slot — un-sealing it so an attacker could re-seal to
// THEIR proposal and redirect the entire COIN vault, or re-open the claim window. End-to-end safety test
// completing the reinit coverage (subledger-pool / gv-config reinit are pinned; distribution's was not).
// DOUBLY-DEFENDED (mutation-confirmed): the explicit `data_len != 0` reject (lib.rs:285) is the clean error,
// but even with it removed the reinit STILL fails because create_pda_robust's System allocate can only run
// on a system-owned, data-empty account — and a live config is distribution-owned. So the data_len check is
// defense-in-depth over that runtime backstop; this test pins the end-to-end invariant, not a single line.
#[test]
fn a_sealed_config_cannot_be_reinitialized_to_redirect_the_vault() {
    let mut env = Env::new(100, 1_000_000);
    let proposal = env.create_proposal(1, 4);
    let (alice, alice_ata) = env.new_recipient();
    env.append(&proposal, &[(alice.pubkey(), 100)]).expect("append");
    let auth = clone_kp(&env.authority);
    env.seal(&proposal, &auth).expect("seal the winner");

    // ATTACK: re-init the live, sealed config to wipe its seal.
    let mut data = vec![0u8]; // IX_INIT_CONFIG
    data.extend_from_slice(&1_000_000u64.to_le_bytes()); // claim window
    data.extend_from_slice(&100u64.to_le_bytes());       // total supply
    let reinit = Instruction {
        program_id: pid(),
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(env.coin_mint, false),
            AccountMeta::new(env.config, false),
            AccountMeta::new_readonly(env.vault, false),
            AccountMeta::new_readonly(env.authority.pubkey(), false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    };
    assert!(env.send(&[reinit], &[]).is_err(), "a live, sealed config cannot be re-initialized");

    // The seal is intact: alice still claims her full 100 from the ORIGINAL winner — no redirect.
    env.claim(&proposal, &alice, &alice_ata, 0).expect("the original sealed distribution still pays");
    assert_eq!(env.token_amount(&alice_ata), 100, "alice got her full allocation — the reinit did not redirect the vault");
}

// MISSING-SIGNER (seal -> claim the entire COIN supply): seal_winner gates on BOTH the authority's
// SIGNATURE and its key (== config.authority, the gv config PDA in genesis). The imposter case in the
// happy-path test pins the KEY half (a wrong signer is rejected). THIS pins the is_signer half: if seal
// accepted a KEY match WITHOUT a signature, an attacker could merely NAME the real authority as a
// read-only account and seal an attacker-chosen proposal with no authorization — then claim the whole
// funded vault. The authority is a PDA in genesis (only the trigger CPI makes it sign), so is_signer is
// the line between "the vote authorized this seal" and "someone merely named the vote".
#[test]
fn seal_rejects_naming_the_authority_without_its_signature() {
    let mut env = Env::new(100, 1_000_000);
    let proposal = env.create_proposal(1, 4);
    let (alice, _alice_ata) = env.new_recipient();
    env.append(&proposal, &[(alice.pubkey(), 100)]).expect("seed the proposal");

    // ATTACK: name the REAL authority as a read-only NON-signer account; nobody signs as it (only the
    // tx fee-payer, who is not the authority, signs the transaction).
    let ix = Instruction {
        program_id: pid(),
        accounts: vec![
            AccountMeta::new_readonly(env.authority.pubkey(), false), // real authority, NOT signing
            AccountMeta::new(env.config, false),
            AccountMeta::new(proposal, false),
        ],
        data: vec![3u8], // IX_SEAL_WINNER
    };
    assert!(env.send(&[ix], &[]).is_err(), "naming the authority without its signature must be rejected");
    // Nothing sealed: the config's sealed_proposal stays default.
    let cfg = env.svm.get_account(&env.config).unwrap();
    assert_eq!(&cfg.data[120..152], &[0u8; 32], "no proposal was sealed");

    // Sanity: the genuine authority, signing, still seals — the guard is the signature, not a freeze.
    let auth = clone_kp(&env.authority);
    env.seal(&proposal, &auth).expect("the real authority seals with its signature");
}

// LOF boundary: competing proposals share ONE config and ONE funded vault (the genesis
// votes among several candidate COIN distributions, only one wins + is sealed). A losing
// proposal's recipients must NEVER be able to pull from that vault. claim binds
// `config.sealed_proposal == proposal.key` (lib.rs:518). Build the worst case: a self-dealing
// LOSING proposal that allocates the ENTIRE supply to the attacker, then prove its claim is
// refused after a different proposal wins — and the winner's vault is fully intact.
#[test]
fn a_losing_proposal_cannot_claim_the_winners_vault() {
    let mut env = Env::new(100, 1_000_000);

    // Winner: proposal 1, all 100 to alice.
    let winner = env.create_proposal(1, 4);
    let (alice, alice_ata) = env.new_recipient();
    env.append(&winner, &[(alice.pubkey(), 100)]).expect("seed winner");

    // Loser: proposal 2 under the same config/vault, a self-dealing full-supply grab.
    let loser = env.create_proposal(2, 4);
    let (mallory, mallory_ata) = env.new_recipient();
    env.append(&loser, &[(mallory.pubkey(), 100)]).expect("seed loser");

    // The vote/trigger seals the winner.
    let auth = clone_kp(&env.authority);
    env.seal(&winner, &auth).expect("seal winner");

    // Mallory's losing proposal cannot pay out of the shared vault.
    assert!(
        env.claim(&loser, &mallory, &mallory_ata, 0).is_err(),
        "a losing proposal must not claim the shared vault"
    );
    // Re-sealing the loser to redirect the vault is also refused (config already sealed).
    assert!(env.seal(&loser, &auth).is_err(), "cannot reseal to the loser");
    assert_eq!(env.token_amount(&mallory_ata), 0, "attacker got nothing");

    // The legitimate winner still claims the full, untouched vault.
    env.claim(&winner, &alice, &alice_ata, 0).expect("winner claim");
    assert_eq!(env.token_amount(&alice_ata), 100);
    assert_eq!(env.token_amount(&env.vault.clone()), 0, "vault went only to the winner");
}

#[test]
fn unclaimed_is_burned_after_window() {
    let mut env = Env::new(100, 50); // window = 50 slots
    env.set_slot(10);
    let proposal = env.create_proposal(1, 4);
    let (alice, alice_ata) = env.new_recipient();
    let (bob, _bob_ata) = env.new_recipient();
    env.append(&proposal, &[(alice.pubkey(), 60), (bob.pubkey(), 40)]).expect("append");

    let auth = clone_kp(&env.authority);
    env.seal(&proposal, &auth).expect("seal"); // seal_slot = 10, window ends at 60

    // Alice claims; bob never does.
    env.claim(&proposal, &alice, &alice_ata, 0).expect("alice claim");
    assert_eq!(env.token_amount(&alice_ata), 60);

    // Past the window: claims rejected, unclaimed (bob's 40) is permissionlessly burned.
    env.set_slot(60);
    assert!(env.claim(&proposal, &bob, &alice_ata, 1).is_err(), "window closed");

    let mint_before = spl_token::state::Mint::unpack(&env.svm.get_account(&env.coin_mint).unwrap().data).unwrap().supply;
    env.burn_unclaimed().expect("burn unclaimed");
    assert_eq!(env.token_amount(&env.vault.clone()), 0, "vault emptied");
    let mint_after = spl_token::state::Mint::unpack(&env.svm.get_account(&env.coin_mint).unwrap().data).unwrap().supply;
    assert_eq!(mint_before - mint_after, 40, "unclaimed 40 burned from supply");
}

// Anti-griefing: burn_unclaimed must be rejected while the claim window is still
// open. Otherwise anyone could permissionlessly burn the pool mid-window and
// destroy claimants' COIN before they get a chance to claim it (a DOS/LOF on every
// recipient who hasn't claimed yet).
#[test]
fn burn_unclaimed_is_rejected_during_the_claim_window() {
    let mut env = Env::new(100, 50); // window = 50 slots
    env.set_slot(10);
    let proposal = env.create_proposal(1, 4);
    let (alice, alice_ata) = env.new_recipient();
    let (bob, bob_ata) = env.new_recipient();
    env.append(&proposal, &[(alice.pubkey(), 60), (bob.pubkey(), 40)]).expect("append");
    let auth = clone_kp(&env.authority);
    env.seal(&proposal, &auth).expect("seal"); // seal_slot = 10, window ends at 60

    // A griefer tries to burn the unclaimed pool mid-window — must be rejected.
    env.set_slot(30);
    assert!(env.burn_unclaimed().is_err(), "cannot burn while the claim window is open");
    // Even one slot before close (window_end - 1) is still too early.
    env.set_slot(59);
    assert!(env.burn_unclaimed().is_err(), "still within the window at window_end - 1");

    // The funds were never touched: both recipients can still claim in full.
    env.claim(&proposal, &alice, &alice_ata, 0).expect("alice claims");
    env.claim(&proposal, &bob, &bob_ata, 1).expect("bob claims");
    assert_eq!(env.token_amount(&alice_ata), 60);
    assert_eq!(env.token_amount(&bob_ata), 40);
    assert_eq!(env.token_amount(&env.vault.clone()), 0, "fully claimed");

    // At/after window_end the burn is permitted (here a no-op: nothing unclaimed).
    env.set_slot(60);
    env.burn_unclaimed().expect("burn allowed once the window has closed");
}

// PREMATURE BURN BEFORE SEAL (permissionless vault-torch DOS): burn_unclaimed is permissionless and refuses
// to run until config.is_sealed() (lib.rs). Before the genesis vote seals a winner the vault is FUNDED but
// undistributed, so a premature burn would destroy the ENTIRE supply and NO recipient could ever be paid.
// The is_sealed() check is the SOLE guard here: before any seal config.seal_slot == 0, so
// window_end = seal_slot + claim_window == claim_window — meaning once the genesis runs past claim_window
// slots the window-gate (clock < window_end) no longer blocks a burn, and only is_sealed() stands between an
// attacker and the funded vault. The during-window test pins the window gate; this pins the seal gate.
#[test]
fn burn_unclaimed_before_the_genesis_seals_cannot_torch_the_vault() {
    let mut env = Env::new(100, 50); // supply 100, claim window 50; vault funded, NOT yet sealed
    assert_eq!(env.token_amount(&env.vault.clone()), 100, "vault funded with the full supply, undistributed");

    // Warp PAST claim_window so the window-gate would NOT block a burn — isolating the is_sealed() guard.
    env.set_slot(60);

    // ATTACK: a permissionless cranker burns the unclaimed vault BEFORE a winner is sealed.
    assert!(env.burn_unclaimed().is_err(), "burn_unclaimed must be refused until a winner is sealed");
    assert_eq!(env.token_amount(&env.vault.clone()), 100, "vault intact — the undistributed supply was not torched");
}

#[test]
fn append_cannot_exceed_total_supply() {
    let mut env = Env::new(100, 1_000_000);
    let proposal = env.create_proposal(1, 4);
    let (alice, _) = env.new_recipient();
    let (bob, _) = env.new_recipient();
    // 60 + 50 = 110 > total_supply 100 -> rejected.
    assert!(
        env.append(&proposal, &[(alice.pubkey(), 60), (bob.pubkey(), 50)]).is_err(),
        "cannot allocate more than the fixed supply"
    );
}

// MALFORMED ENTRIES (zero amount / zero-address recipient): append rejects amount == 0 || pk ==
// Pubkey::default() (lib.rs:append_entries). A zero-amount entry is permanently unclaimable and just
// soaks a slot; a default-pubkey (zero address) entry allocates a chunk of the fixed supply to a key NOBODY
// can ever sign for, so that COIN is locked out of every real recipient and lost. The guard keeps the sealed
// distribution list well-formed; a bad entry rejects the WHOLE chunk atomically (no partial corruption).
#[test]
fn append_rejects_a_zero_amount_or_default_pubkey_entry() {
    let mut env = Env::new(100, 1_000_000);
    let proposal = env.create_proposal(1, 4);
    let (alice, _) = env.new_recipient();

    assert!(env.append(&proposal, &[(alice.pubkey(), 0)]).is_err(), "a zero-amount entry must be rejected");
    assert!(env.append(&proposal, &[(Pubkey::default(), 50)]).is_err(), "a default-pubkey (zero address) entry must be rejected");
    // A multi-entry chunk with one bad entry rejects the WHOLE append — atomic, no partial write.
    assert!(env.append(&proposal, &[(alice.pubkey(), 50), (Pubkey::default(), 50)]).is_err(), "one malformed entry rejects the whole chunk");

    // The proposal was never partially written: a clean append still works and is the FIRST entry.
    env.append(&proposal, &[(alice.pubkey(), 60)]).expect("a well-formed entry is accepted");
    let pd = env.svm.get_account(&proposal).unwrap();
    assert_eq!(u32::from_le_bytes(pd.data[84..88].try_into().unwrap()), 1, "exactly one entry — the rejected appends wrote nothing");
    assert_eq!(u64::from_le_bytes(pd.data[88..96].try_into().unwrap()), 60, "total_amount is just the one clean entry");
}

// LOF boundary: append_entries binds to the proposal's recorded creator (lib.rs:417,
// `header.creator != *creator.key`). A proposal is a candidate COIN distribution list; if
// its winner is sealed, its entries become directly claimable. So a hostile actor who could
// append to *someone else's* in-flight proposal would inject a self-dealing entry into the
// unallocated headroom (below total_supply, so the cap check at :442 wouldn't catch it) and
// claim that COIN the moment the honest proposal won the vote. Confirm the creator-binding
// blocks a foreign appender, while the real creator can still extend its own proposal.
#[test]
fn append_entries_rejects_a_foreign_creator() {
    let mut env = Env::new(100, 1_000_000);
    // Honest creator = env.payer; proposal allocates 40 of 100, leaving 60 headroom.
    let proposal = env.create_proposal(1, 8);
    let (honest, _) = env.new_recipient();
    env.append(&proposal, &[(honest.pubkey(), 40)]).expect("creator seeds its own proposal");

    // Attacker signs, takes the creator slot, tries to inject a self-entry into the 60 headroom.
    let attacker = Keypair::new();
    env.svm.airdrop(&attacker.pubkey(), 1_000_000_000).unwrap();
    assert!(
        env.append_as(&proposal, &attacker, &[(attacker.pubkey(), 60)]).is_err(),
        "a non-creator must not be able to append entries to someone else's proposal"
    );

    // The genuine creator can still append into the same headroom — the binding is to the
    // creator, not a freeze.
    env.append(&proposal, &[(honest.pubkey(), 60)]).expect("the real creator extends its proposal");
}

// LOF boundary (post-seal headroom grab): append_entries refuses once the proposal is sealed
// (lib.rs `header.sealed || config.is_sealed()`). The danger it blocks: a winning proposal that
// allocated only PART of the supply (say 60 of 100) leaves 40 of unallocated headroom. That 40 is
// meant to be burned as unclaimed after the window — deflation accruing to ALL coin holders. Without
// the seal-freeze, the (real) creator could wait until the vote sealed THEIR proposal, then append a
// self-dealing entry into the 40 headroom (below total_supply, so the cap check never fires) and
// claim it — converting protocol-wide deflation into a private grab AFTER voters could no longer
// react. Confirm the freeze blocks it and the remainder is burnable, not grabbable.
#[test]
fn append_to_a_sealed_winner_cannot_grab_the_unallocated_headroom() {
    let mut env = Env::new(100, 50); // supply 100, claim window 50
    env.set_slot(10);
    let proposal = env.create_proposal(1, 8);
    let (alice, alice_ata) = env.new_recipient();
    env.append(&proposal, &[(alice.pubkey(), 60)]).expect("creator allocates 60 of 100"); // 40 headroom

    let auth = clone_kp(&env.authority);
    env.seal(&proposal, &auth).expect("the vote seals this winner"); // seal_slot 10, window ends 60

    // ATTACK: the creator appends a self-dealing 40 into the still-open headroom of the SEALED winner.
    // total_amount would be 60+40 = 100 == total_supply, so the supply cap would NOT catch it — only
    // the seal-freeze stands in the way.
    let grabber = Keypair::new();
    let grab_ata = create_token_account(&mut env.svm, &env.payer, &env.coin_mint, &grabber.pubkey());
    assert!(
        env.append(&proposal, &[(grabber.pubkey(), 40)]).is_err(),
        "a sealed winner must not accept new entries — the unallocated headroom is not grabbable"
    );
    // The injected entry never landed: there is no index-1 entry to claim.
    assert!(env.claim(&proposal, &grabber, &grab_ata, 1).is_err(), "no entry was appended post-seal");

    // The honest allocation still pays exactly 60; the 40 remainder stays in the vault.
    env.claim(&proposal, &alice, &alice_ata, 0).expect("alice claims her 60");
    assert_eq!(env.token_amount(&alice_ata), 60);
    assert_eq!(env.token_amount(&env.vault.clone()), 40, "the unallocated 40 is still in the vault, untouched");

    // After the window, that 40 is BURNED (deflation to all holders) — never reachable by the grabber.
    env.set_slot(60);
    let supply_before = spl_token::state::Mint::unpack(&env.svm.get_account(&env.coin_mint).unwrap().data).unwrap().supply;
    env.burn_unclaimed().expect("burn the unallocated remainder");
    let supply_after = spl_token::state::Mint::unpack(&env.svm.get_account(&env.coin_mint).unwrap().data).unwrap().supply;
    assert_eq!(supply_before - supply_after, 40, "the 40 headroom was burned, not captured");
    assert_eq!(env.token_amount(&grab_ata), 0, "the grabber got nothing");
}

// Solvency invariant: InitConfig must reject a vault that holds less than the
// promised total_supply. Otherwise a config could promise 100 COIN while the vault
// holds only 60 — the seal (total_amount <= total_supply) would pass, then early
// claimants drain the 60 and honest late claimants are stranded (claim-race LOF).
// This pins that the promised supply is backed by real tokens at init time.
#[test]
fn init_config_rejects_an_underfunded_vault() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(pid(), so_path()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let mint_authority = Keypair::new();
    let coin_mint = create_mint(&mut svm, &payer, &mint_authority.pubkey());

    let authority = Keypair::new();
    let config = Pubkey::find_program_address(&[b"dist_config", coin_mint.as_ref(), authority.pubkey().as_ref()], &pid()).0;
    let vault = create_token_account(&mut svm, &payer, &coin_mint, &config);
    // Fund the vault with only 60, but promise a total_supply of 100.
    mint_to(&mut svm, &payer, &coin_mint, &mint_authority, &vault, 60);
    revoke_mint_authority(&mut svm, &payer, &coin_mint, &mint_authority);

    let build_init = |total_supply: u64| {
        let mut data = vec![0u8]; // IX_INIT_CONFIG
        data.extend_from_slice(&1_000_000u64.to_le_bytes()); // claim window
        data.extend_from_slice(&total_supply.to_le_bytes());
        Instruction {
            program_id: pid(),
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new_readonly(coin_mint, false),
                AccountMeta::new(config, false),
                AccountMeta::new_readonly(vault, false),
                AccountMeta::new_readonly(authority.pubkey(), false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data,
        }
    };
    let send = |svm: &mut LiteSVM, ix: Instruction| -> Result<(), String> {
        let bh = svm.latest_blockhash();
        let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], bh);
        svm.send_transaction(tx).map(|_| ()).map_err(|e| format!("{:?}", e))
    };

    // Underfunded (vault 60 < promised 100): rejected.
    assert!(send(&mut svm, build_init(100)).is_err(), "underfunded vault must be rejected");
    // Promising exactly what the vault holds (60) succeeds — supply is tied to real tokens.
    send(&mut svm, build_init(60)).expect("fully-backed supply is accepted");
}

// SOLVENCY (anti-mask of init_config_rejects_an_underfunded_vault): that test mints only 60, so its
// build_init(100) is actually rejected by the SUPPLY-EQUALITY check (mint.supply 60 != total_supply 100,
// lib.rs:304) — the solvency check (:318) is masked and never the deciding guard. This pins :318 directly:
// mint the FULL 100 supply (mint.supply == total_supply == 100) but seed only 60 into the vault (40 minted
// to a decoy held outside). Now :304 PASSES, so ONLY the solvency check `vault.amount < total_supply` stands
// between an underfunded vault and a claim-race LOF (early claimants drain 60, late ones stranded). Without
// :318 this init would succeed.
#[test]
fn init_config_rejects_a_vault_underfunded_below_a_fully_minted_supply() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(pid(), so_path()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let mint_authority = Keypair::new();
    let coin_mint = create_mint(&mut svm, &payer, &mint_authority.pubkey());

    let authority = Keypair::new();
    let config = Pubkey::find_program_address(&[b"dist_config", coin_mint.as_ref(), authority.pubkey().as_ref()], &pid()).0;
    let vault = create_token_account(&mut svm, &payer, &coin_mint, &config);
    // The FULL supply is 100, but the vault holds only 60 — the other 40 are minted to a decoy account
    // (e.g. an attacker's), so the mint's supply == 100 == total_supply (passing the supply-equality check)
    // while the vault is underfunded.
    let decoy = create_token_account(&mut svm, &payer, &coin_mint, &payer.pubkey());
    mint_to(&mut svm, &payer, &coin_mint, &mint_authority, &vault, 60);
    mint_to(&mut svm, &payer, &coin_mint, &mint_authority, &decoy, 40);
    revoke_mint_authority(&mut svm, &payer, &coin_mint, &mint_authority);

    let mut data = vec![0u8]; // IX_INIT_CONFIG
    data.extend_from_slice(&1_000_000u64.to_le_bytes()); // claim window
    data.extend_from_slice(&100u64.to_le_bytes()); // total_supply == mint.supply, but vault holds only 60
    let ix = Instruction {
        program_id: pid(),
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(coin_mint, false),
            AccountMeta::new(config, false),
            AccountMeta::new_readonly(vault, false),
            AccountMeta::new_readonly(authority.pubkey(), false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    };
    let bh = svm.latest_blockhash();
    let r = svm.send_transaction(Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], bh));
    assert!(r.is_err(), "a vault holding 60 of a fully-minted 100 supply must be rejected (solvency, :318)");
    // No config PDA was created — the underfunded bind was refused.
    assert!(svm.get_account(&config).map_or(true, |a| a.data.is_empty()), "no config bound to the underfunded vault");
}

// TYPE-COSPLAY INIT SQUAT: init_config must reject a vault account that has SPL Token
// account-shaped bytes but is not actually owned by the SPL Token program. Otherwise a
// front-runner could initialize the real distribution config with a fake vault whose
// bytes claim (mint = COIN, owner = config PDA, amount = total_supply). Later claims
// and burns would fail at the SPL Token CPI, and the real config PDA would already be
// occupied: permanent distribution DOS.
#[test]
fn init_config_rejects_a_token_shaped_vault_not_owned_by_spl_token() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(pid(), so_path()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let mint_authority = Keypair::new();
    let coin_mint = create_mint(&mut svm, &payer, &mint_authority.pubkey());
    let authority = Pubkey::new_unique();
    let config = Pubkey::find_program_address(&[b"dist_config", coin_mint.as_ref(), authority.as_ref()], &pid()).0;

    let decoy = create_token_account(&mut svm, &payer, &coin_mint, &payer.pubkey());
    mint_to(&mut svm, &payer, &coin_mint, &mint_authority, &decoy, 100);
    revoke_mint_authority(&mut svm, &payer, &coin_mint, &mint_authority);

    let fake_vault = Pubkey::new_unique();
    svm.set_account(
        fake_vault,
        Account {
            lamports: svm.minimum_balance_for_rent_exemption(spl_token::state::Account::LEN),
            data: token_account_bytes(&coin_mint, &config, 100),
            owner: solana_sdk::system_program::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();

    let mut data = vec![0u8]; // IX_INIT_CONFIG
    data.extend_from_slice(&1_000_000u64.to_le_bytes());
    data.extend_from_slice(&100u64.to_le_bytes());
    let ix = Instruction {
        program_id: pid(),
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(coin_mint, false),
            AccountMeta::new(config, false),
            AccountMeta::new_readonly(fake_vault, false),
            AccountMeta::new_readonly(authority, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    };
    let bh = svm.latest_blockhash();
    let r = svm.send_transaction(Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], bh));
    assert!(r.is_err(), "a token-shaped vault not owned by SPL Token must be rejected");
    assert!(svm.get_account(&config).map_or(true, |a| a.data.is_empty()), "fake vault must not squat the config");
}

// ZERO CLAIM WINDOW (recipient-LOF DOS): init_config rejects claim_window_slots == 0 (lib.rs:276). A zero
// window is catastrophic — window_end = seal_slot + 0 = seal_slot, so claim (clock < window_end) is refused
// the instant the winner seals, and burn_unclaimed (clock >= window_end) immediately torches the WHOLE
// vault: every recipient loses their COIN without a chance to claim. The guard blocks the config at
// creation. (The sibling `total_supply == 0` clause is doubly-defended by the anti-hoarding
// mint.supply == total_supply check — already pinned by init_config_rejects_a_mintable_coin — so only the
// claim-window clause is single-guard here.)
#[test]
fn init_config_rejects_a_zero_claim_window() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(pid(), so_path()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let mint_authority = Keypair::new();
    let coin_mint = create_mint(&mut svm, &payer, &mint_authority.pubkey());
    let authority = Keypair::new();
    let config = Pubkey::find_program_address(&[b"dist_config", coin_mint.as_ref(), authority.pubkey().as_ref()], &pid()).0;
    let vault = create_token_account(&mut svm, &payer, &coin_mint, &config);
    mint_to(&mut svm, &payer, &coin_mint, &mint_authority, &vault, 100); // fully back supply 100
    revoke_mint_authority(&mut svm, &payer, &coin_mint, &mint_authority);

    let build = |window: u64, supply: u64| {
        let mut data = vec![0u8]; // IX_INIT_CONFIG
        data.extend_from_slice(&window.to_le_bytes());
        data.extend_from_slice(&supply.to_le_bytes());
        Instruction {
            program_id: pid(),
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new_readonly(coin_mint, false),
                AccountMeta::new(config, false),
                AccountMeta::new_readonly(vault, false),
                AccountMeta::new_readonly(authority.pubkey(), false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data,
        }
    };
    let send = |svm: &mut LiteSVM, ix: Instruction| -> Result<(), String> {
        svm.expire_blockhash();
        let bh = svm.latest_blockhash();
        svm.send_transaction(Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], bh)).map(|_| ()).map_err(|e| format!("{:?}", e))
    };

    // A ZERO claim window would make every claim impossible -> all recipients lose their COIN. Rejected.
    assert!(send(&mut svm, build(0, 100)).is_err(), "a zero claim window must be rejected (no one could ever claim)");
    // A valid window + fully-funded supply is accepted (the config PDA was never touched by the reject).
    send(&mut svm, build(50, 100)).expect("a valid window + fully-funded supply is accepted");
}

// Fixed-supply invariant (README Safety §4): a COIN whose mint authority is NOT
// revoked must be refused — otherwise the authority holder could mint unlimited COIN
// outside the fixed distribution pool and dilute every recipient (no "mint to drain").
#[test]
fn init_config_rejects_a_mintable_coin() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(pid(), so_path()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let mint_authority = Keypair::new();
    let coin_mint = create_mint(&mut svm, &payer, &mint_authority.pubkey());
    let auth = Keypair::new().pubkey();
    let config = Pubkey::find_program_address(&[b"dist_config", coin_mint.as_ref(), auth.as_ref()], &pid()).0;
    let vault = create_token_account(&mut svm, &payer, &coin_mint, &config);
    mint_to(&mut svm, &payer, &coin_mint, &mint_authority, &vault, 100);

    let build = || {
        let mut data = vec![0u8]; // IX_INIT_CONFIG
        data.extend_from_slice(&1_000_000u64.to_le_bytes());
        data.extend_from_slice(&100u64.to_le_bytes());
        Instruction {
            program_id: pid(),
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new_readonly(coin_mint, false),
                AccountMeta::new(config, false),
                AccountMeta::new_readonly(vault, false),
                AccountMeta::new_readonly(auth, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data,
        }
    };
    let send = |svm: &mut LiteSVM, ix: Instruction| -> Result<(), String> {
        svm.expire_blockhash(); // distinct blockhash so the retried ix isn't a dup
        let bh = svm.latest_blockhash();
        let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], bh);
        svm.send_transaction(tx).map(|_| ()).map_err(|e| format!("{:?}", e))
    };

    // Mint authority still live: rejected.
    assert!(send(&mut svm, build()).is_err(), "must reject a still-mintable COIN");

    // Pre-mint extra COIN to the attacker (supply 150 > distributed pool 100), THEN
    // revoke. Must still be rejected: undistributed COIN outside the pool would
    // dominate governance.
    let attacker_ata = create_token_account(&mut svm, &payer, &coin_mint, &Pubkey::new_unique());
    mint_to(&mut svm, &payer, &coin_mint, &mint_authority, &attacker_ata, 50);
    revoke_mint_authority(&mut svm, &payer, &coin_mint, &mint_authority);
    assert!(send(&mut svm, build()).is_err(), "must reject a COIN with supply > the distributed pool");
}

// init_config also rejects a COIN with a live FREEZE authority (a separate clause from the mint
// authority). A freezable COIN is a DOS/LOF: the freeze authority could freeze the distribution VAULT
// (the config PDA can no longer transfer out -> EVERY claim reverts -> the whole genesis payout is
// bricked) or freeze an individual recipient's account to block their claim. The mintable-coin test
// above only exercises the mint-authority clause (the test mints carry no freeze authority); this pins
// the freeze-authority clause specifically: mint authority revoked + supply == total_supply + vault
// funded, so the ONLY thing left to reject on is the live freeze authority.
#[test]
fn init_config_rejects_a_freezable_coin() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(pid(), so_path()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let mint_authority = Keypair::new();
    let freeze_authority = Keypair::new();

    // A COIN with BOTH a mint authority (revoked below) and a live FREEZE authority.
    let mint = Keypair::new();
    let rent = svm.minimum_balance_for_rent_exemption(spl_token::state::Mint::LEN);
    let ixs = [
        system_instruction::create_account(&payer.pubkey(), &mint.pubkey(), rent, spl_token::state::Mint::LEN as u64, &spl_token::ID),
        spl_token::instruction::initialize_mint(&spl_token::ID, &mint.pubkey(), &mint_authority.pubkey(), Some(&freeze_authority.pubkey()), 6).unwrap(),
    ];
    let tx = Transaction::new_signed_with_payer(&ixs, Some(&payer.pubkey()), &[&payer, &mint], svm.latest_blockhash());
    svm.send_transaction(tx).unwrap();
    let coin_mint = mint.pubkey();

    let auth = Keypair::new().pubkey();
    let config = Pubkey::find_program_address(&[b"dist_config", coin_mint.as_ref(), auth.as_ref()], &pid()).0;
    let vault = create_token_account(&mut svm, &payer, &coin_mint, &config);
    mint_to(&mut svm, &payer, &coin_mint, &mint_authority, &vault, 100); // entire supply -> vault
    // Revoke ONLY the mint authority; leave the freeze authority live (the dangerous case).
    revoke_mint_authority(&mut svm, &payer, &coin_mint, &mint_authority);

    let mut data = vec![0u8];
    data.extend_from_slice(&1_000_000u64.to_le_bytes());
    data.extend_from_slice(&100u64.to_le_bytes());
    let ix = Instruction {
        program_id: pid(),
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(coin_mint, false),
            AccountMeta::new(config, false),
            AccountMeta::new_readonly(vault, false),
            AccountMeta::new_readonly(auth, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    };
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], svm.latest_blockhash());
    assert!(
        svm.send_transaction(tx).is_err(),
        "a COIN with a live freeze authority must be rejected — it could freeze the vault (brick all claims)"
    );
}

// Once the entire COIN supply is the distribution pool (and mint authority revoked),
// the config is accepted — proving every COIN that exists is in this vault.
#[test]
fn init_config_accepts_a_fully_in_vault_fixed_supply_coin() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(pid(), so_path()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let mint_authority = Keypair::new();
    let coin_mint = create_mint(&mut svm, &payer, &mint_authority.pubkey());
    let authority = Pubkey::new_unique();
    let config = Pubkey::find_program_address(&[b"dist_config", coin_mint.as_ref(), authority.as_ref()], &pid()).0;
    let vault = create_token_account(&mut svm, &payer, &coin_mint, &config);
    mint_to(&mut svm, &payer, &coin_mint, &mint_authority, &vault, 100); // entire supply -> vault
    revoke_mint_authority(&mut svm, &payer, &coin_mint, &mint_authority);
    let mut data = vec![0u8];
    data.extend_from_slice(&1_000_000u64.to_le_bytes());
    data.extend_from_slice(&100u64.to_le_bytes());
    let ix = Instruction {
        program_id: pid(),
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(coin_mint, false),
            AccountMeta::new(config, false),
            AccountMeta::new_readonly(vault, false),
            AccountMeta::new_readonly(authority, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    };
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], svm.latest_blockhash());
    svm.send_transaction(tx).expect("entire-supply-in-vault COIN accepted");
}

// LAMPORT PRE-FUND INIT-DOS (finding AI): the dist config PDA is deterministic
// (f(coin_mint, authority), both public) and init_config is permissionless. System
// `create_account` aborts with AccountAlreadyInUse on ANY pre-existing lamports, so an attacker
// could transfer 1 lamport to the config PDA (no signature needed) BEFORE the genesis orchestrator
// inits it — and the dust can never be swept from a system-owned PDA. If init used plain
// create_account this would PERMANENTLY brick the distribution config that custodies the ENTIRE
// COIN supply (no config -> the funded vault can never be sealed/claimed -> genesis payout frozen).
// init_config's create_pda_robust (top-up + allocate + assign via invoke_signed; re-init gated on
// data_len, not lamports) tolerates the dust. (Sibling of the subledger-pool / twap-book / gv-config
// prefund tests; this pins the distribution config init, which holds the COIN supply.)
#[test]
fn lamport_prefund_cannot_brick_config_init() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(pid(), so_path()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let mint_authority = Keypair::new();
    let coin_mint = create_mint(&mut svm, &payer, &mint_authority.pubkey());
    let authority = Pubkey::new_unique();
    let config = Pubkey::find_program_address(&[b"dist_config", coin_mint.as_ref(), authority.as_ref()], &pid()).0;
    let vault = create_token_account(&mut svm, &payer, &coin_mint, &config);
    mint_to(&mut svm, &payer, &coin_mint, &mint_authority, &vault, 100);
    revoke_mint_authority(&mut svm, &payer, &coin_mint, &mint_authority);

    // ATTACK: dust the deterministic config PDA with 1 lamport before the orchestrator inits.
    svm.set_account(
        config,
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
    let mut data = vec![0u8];
    data.extend_from_slice(&1_000_000u64.to_le_bytes()); // claim window
    data.extend_from_slice(&100u64.to_le_bytes()); // total supply
    let ix = Instruction {
        program_id: pid(),
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(coin_mint, false),
            AccountMeta::new(config, false),
            AccountMeta::new_readonly(vault, false),
            AccountMeta::new_readonly(authority, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    };
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], svm.latest_blockhash());
    svm.send_transaction(tx).expect("robust create tolerates the dusted config PDA");

    // The config is genuinely initialized + usable: program-owned with data, and a proposal can be
    // created under it (the init wrote valid state, not a half-allocated husk).
    let acc = svm.get_account(&config).unwrap();
    assert_eq!(acc.owner, pid(), "config now program-owned");
    assert!(!acc.data.is_empty(), "config initialized despite the dust");
    let proposal = Pubkey::find_program_address(&[b"dist_proposal", config.as_ref(), &1u64.to_le_bytes()], &pid()).0;
    let mut pdata = vec![1u8];
    pdata.extend_from_slice(&1u64.to_le_bytes());
    pdata.extend_from_slice(&4u32.to_le_bytes());
    let cix = Instruction {
        program_id: pid(),
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(config, false),
            AccountMeta::new(proposal, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: pdata,
    };
    let tx = Transaction::new_signed_with_payer(&[cix], Some(&payer.pubkey()), &[&payer], svm.latest_blockhash());
    svm.send_transaction(tx).expect("a proposal registers under the dusted-then-inited config");
}

// ADVERSARIAL (init front-run theft, finding AA): the config PDA binds the AUTHORITY, so an
// attacker cannot init the per-mint config with authority=themselves over the deployer's
// already-funded vault (owned by the LEGIT config PDA). Their authority derives a DIFFERENT PDA
// that does not own the vault, so init is rejected and the funded supply is untouchable.
#[test]
fn init_config_authority_bound_blocks_funded_vault_hijack() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(pid(), so_path()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let mint_authority = Keypair::new();
    let coin_mint = create_mint(&mut svm, &payer, &mint_authority.pubkey());

    // Deployer's legit config + funded vault (vault owned by the LEGIT, authority-bound config PDA).
    let legit_authority = Pubkey::new_unique();
    let legit_config = Pubkey::find_program_address(&[b"dist_config", coin_mint.as_ref(), legit_authority.as_ref()], &pid()).0;
    let vault = create_token_account(&mut svm, &payer, &coin_mint, &legit_config);
    mint_to(&mut svm, &payer, &coin_mint, &mint_authority, &vault, 100);
    revoke_mint_authority(&mut svm, &payer, &coin_mint, &mint_authority);

    let init = |config: Pubkey, authority: Pubkey| {
        let mut data = vec![0u8];
        data.extend_from_slice(&1_000_000u64.to_le_bytes());
        data.extend_from_slice(&100u64.to_le_bytes());
        Instruction { program_id: pid(), accounts: vec![
            AccountMeta::new(payer.pubkey(), true), AccountMeta::new_readonly(coin_mint, false), AccountMeta::new(config, false),
            AccountMeta::new_readonly(vault, false), AccountMeta::new_readonly(authority, false), AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ], data }
    };

    // ATTACK: front-run init with authority=attacker pointed at the legit funded vault.
    let attacker = Keypair::new();
    let attacker_config = Pubkey::find_program_address(&[b"dist_config", coin_mint.as_ref(), attacker.pubkey().as_ref()], &pid()).0;
    assert_ne!(attacker_config, legit_config);
    let bh = svm.latest_blockhash();
    assert!(svm.send_transaction(Transaction::new_signed_with_payer(&[init(attacker_config, attacker.pubkey())], Some(&payer.pubkey()), &[&payer], bh)).is_err(),
        "attacker's authority cannot init over the legit funded vault (different PDA, not the vault owner)");
    let v = svm.get_account(&vault).unwrap();
    assert_eq!(u64::from_le_bytes(v.data[64..72].try_into().unwrap()), 100, "funded supply untouched");

    // The legit deployer (authority over its OWN PDA's vault) inits fine.
    let bh = svm.latest_blockhash();
    svm.send_transaction(Transaction::new_signed_with_payer(&[init(legit_config, legit_authority)], Some(&payer.pubkey()), &[&payer], bh)).expect("legit init succeeds");
}
