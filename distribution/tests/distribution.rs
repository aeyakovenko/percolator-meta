//! End-to-end litesvm tests: fixed COIN vault, proposal list, authority-gated
//! seal, permissionless per-recipient claim (pull, indexed), and burn-unclaimed
//! after the claim window.

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

fn pid() -> Pubkey {
    distribution_program::id()
}
fn so_path() -> String {
    format!("{}/../target/deploy/distribution_program.so", env!("CARGO_MANIFEST_DIR"))
}
fn clone_kp(kp: &Keypair) -> Keypair {
    Keypair::from_bytes(&kp.to_bytes()).unwrap()
}
fn dist_config_pda(coin_mint: &Pubkey, authority: &Pubkey, claim_window: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"dist_config",
            coin_mint.as_ref(),
            authority.as_ref(),
            &claim_window.to_le_bytes(),
        ],
        &pid(),
    )
    .0
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

        // The config PDA binds the authority and claim window, so derive it after.
        let authority = Keypair::new();
        let config = dist_config_pda(&coin_mint, &authority.pubkey(), claim_window);
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

fn translate_sealed_distribution_to_legacy(
    env: &mut Env,
    current_proposal: &Pubkey,
    authority_bound: bool,
) -> Pubkey {
    let current_config = env.config;
    let mut config_account = env.svm.get_account(&current_config).unwrap();
    let mut proposal_account = env.svm.get_account(current_proposal).unwrap();
    let proposal_id = u64::from_le_bytes(proposal_account.data[40..48].try_into().unwrap());
    let (legacy_config, legacy_bump) = if authority_bound {
        Pubkey::find_program_address(
            &[
                b"dist_config",
                env.coin_mint.as_ref(),
                env.authority.pubkey().as_ref(),
            ],
            &pid(),
        )
    } else {
        Pubkey::find_program_address(&[b"dist_config", env.coin_mint.as_ref()], &pid())
    };
    let legacy_proposal = Pubkey::find_program_address(
        &[
            b"dist_proposal",
            legacy_config.as_ref(),
            &proposal_id.to_le_bytes(),
        ],
        &pid(),
    )
    .0;

    config_account.data[120..152].copy_from_slice(legacy_proposal.as_ref());
    config_account.data[160] = legacy_bump;
    env.svm.set_account(legacy_config, config_account).unwrap();

    proposal_account.data[8..40].copy_from_slice(legacy_config.as_ref());
    env.svm.set_account(legacy_proposal, proposal_account).unwrap();

    // Historical init made this config PDA the SPL vault authority.
    let mut vault_account = env.svm.get_account(&env.vault).unwrap();
    let mut vault_state = spl_token::state::Account::unpack(&vault_account.data).unwrap();
    vault_state.owner = legacy_config;
    spl_token::state::Account::pack(vault_state, &mut vault_account.data).unwrap();
    env.svm.set_account(env.vault, vault_account).unwrap();

    env.config = legacy_config;
    legacy_proposal
}

// UPGRADE-INDUCED VAULT LOCK: the safe authority-bound config stayed 168 bytes when
// claim_window joined its PDA seed. The current decoder accepts that generation,
// but signing only the latest seeds strands recipient claims and the terminal burn.
#[test]
fn authority_bound_historical_distribution_preserves_claim_and_terminal_burn() {
    let mut env = Env::new(100, 10);
    let (recipient, recipient_ata) = env.new_recipient();
    let proposal = env.create_proposal(7, 1);
    env.append(&proposal, &[(recipient.pubkey(), 40)]).expect("append");
    let authority = clone_kp(&env.authority);
    env.seal(&proposal, &authority).expect("seal");
    let legacy_proposal = translate_sealed_distribution_to_legacy(&mut env, &proposal, true);

    env.claim(&legacy_proposal, &recipient, &recipient_ata, 0)
        .expect("authority-bound historical recipient claim remains live");
    assert_eq!(env.token_amount(&recipient_ata), 40, "recipient paid");
    assert_eq!(env.token_amount(&env.vault), 60, "only the claim left the vault");

    let seal_slot = u64::from_le_bytes(
        env.svm.get_account(&env.config).unwrap().data[152..160]
            .try_into()
            .unwrap(),
    );
    env.set_slot(seal_slot + 10);
    env.burn_unclaimed().expect("authority-bound historical terminal burn remains live");
    assert_eq!(env.token_amount(&env.vault), 0, "no reward coins stranded");
    let mint_supply = spl_token::state::Mint::unpack(
        &env.svm.get_account(&env.coin_mint).unwrap().data,
    )
    .unwrap()
    .supply;
    assert_eq!(mint_supply, 40, "only the claimed reward remains in supply");
}

// The mint-only schema was retired by 418ca15 because a first writer could bind a
// pre-funded vault to an attacker authority. Compatibility must never make that
// quarantined config a signer again, even if its bytes otherwise decode cleanly.
#[test]
fn theft_vulnerable_mint_only_distribution_config_stays_inert() {
    let mut env = Env::new(100, 10);
    let (recipient, recipient_ata) = env.new_recipient();
    let proposal = env.create_proposal(7, 1);
    env.append(&proposal, &[(recipient.pubkey(), 100)]).expect("append");
    let authority = clone_kp(&env.authority);
    env.seal(&proposal, &authority).expect("seal");
    let legacy_proposal = translate_sealed_distribution_to_legacy(&mut env, &proposal, false);

    assert!(
        env.claim(&legacy_proposal, &recipient, &recipient_ata, 0).is_err(),
        "known-unsafe mint-only config must not regain a claim signer"
    );
    assert_eq!(env.token_amount(&recipient_ata), 0, "no attacker-directed claim moved coins");
    assert_eq!(env.token_amount(&env.vault), 100, "quarantined vault unchanged");

    let seal_slot = u64::from_le_bytes(
        env.svm.get_account(&env.config).unwrap().data[152..160]
            .try_into()
            .unwrap(),
    );
    env.set_slot(seal_slot + 10);
    assert!(env.burn_unclaimed().is_err(), "mint-only config must not regain any signer path");
    assert_eq!(env.token_amount(&env.vault), 100, "quarantine remains atomic after burn attempt");
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

// WINDOW-OVERFLOW BRICK (permanent fund-freeze from one config value): claim + burn both gate on
// `window_end = seal_slot + claim_window_slots`. init_config bounds claim_window_slots != 0 but had NO
// upper bound, so a near-u64::MAX window makes seal_slot + window overflow once seal_slot > 0. Under the
// old checked_add().ok_or() that returned ArithmeticOverflow -> EVERY claim AND every burn reverts ->
// the whole vault is frozen forever (no claim, no burn). The fix saturates window_end to u64::MAX, so an
// over-long window just keeps claims open indefinitely (burn never fires) — graceful, never a brick.
// This test seals at a NON-ZERO slot (so seal_slot + u64::MAX would overflow) and asserts the claim still
// pays; it FAILS on the pre-fix checked_add (claim overflow-reverts). Normal windows are unaffected.
#[test]
fn an_absurd_claim_window_saturates_and_never_bricks_claims() {
    let mut env = Env::new(100, u64::MAX); // the pathological deployer/config value
    let proposal = env.create_proposal(1, 1);
    let (alice, alice_ata) = env.new_recipient();
    env.append(&proposal, &[(alice.pubkey(), 100)]).expect("append");

    // Seal at a NON-ZERO slot so seal_slot + u64::MAX overflows u64 (the brick trigger).
    env.set_slot(1_000);
    let auth = clone_kp(&env.authority);
    env.seal(&proposal, &auth).expect("seal at slot 1000");

    // Far past any normal window: the claim must STILL succeed (saturated window_end = u64::MAX), not
    // overflow-revert. Pre-fix this reverted with ArithmeticOverflow -> a permanent freeze.
    env.set_slot(5_000_000);
    env.claim(&proposal, &alice, &alice_ata, 0).expect("claim must not overflow-brick on an absurd window");
    assert_eq!(env.token_amount(&alice_ata), 100, "recipient paid in full — vault not frozen");

    // Graceful tail: with an effectively infinite window, burn stays CLOSED (window never ends) rather
    // than bricking — the unclaimed remainder is always claimable, never stuck.
    assert!(env.burn_unclaimed().is_err(), "burn stays closed under a saturated (infinite) window — no premature torch");
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

// ATTACK PROBE (cross-recipient / outsider claim — the pull-model recipient binding): claim is a PULL —
// the entry at `index` may be redeemed ONLY by the pubkey recorded there, and that pubkey must SIGN
// (lib.rs:544 is_signer + 577 pk == recipient.key). Without the 577 bind, any signer could pass a victim's
// index and drain their COIN into their own account. Distinct from double-claim (re-claiming a ZEROED entry)
// and from wrong-proposal (a_losing_proposal_cannot_claim). Here the entries are LIVE and the vault funded;
// the only thing stopping the theft is the recipient bind. Proven end-to-end against the real distribution .so.
#[test]
fn claim_index_is_bound_to_its_named_recipient_no_cross_or_outsider_claim() {
    let mut env = Env::new(100, 1_000_000);
    let proposal = env.create_proposal(1, 4);
    let (alice, alice_ata) = env.new_recipient();
    let (bob, bob_ata) = env.new_recipient();
    env.append(&proposal, &[(alice.pubkey(), 10), (bob.pubkey(), 90)]).expect("append");
    let auth = clone_kp(&env.authority);
    env.seal(&proposal, &auth).expect("seal");

    // (1) Cross-recipient: alice signs but names BOB's index 1 (his 90) into her own ata -> the recorded
    //     pubkey (bob) != signer (alice) -> rejected. Alice cannot harvest bob's larger entry.
    assert!(env.claim(&proposal, &alice, &alice_ata, 1).is_err(), "alice cannot claim bob's index — recipient bind");
    // (2) Outsider: mallory is in NO entry; she signs and names alice's index 0 into her own ata -> rejected.
    let (mallory, mallory_ata) = env.new_recipient();
    assert!(env.claim(&proposal, &mallory, &mallory_ata, 0).is_err(), "an outsider cannot claim any entry");
    // The vault is byte-for-byte untouched by the rejected attempts; both real recipients still claim in full.
    assert_eq!(env.token_amount(&env.vault.clone()), 100, "no COIN moved on the rejected cross/outsider claims");
    assert_eq!(env.token_amount(&mallory_ata), 0, "the outsider received nothing");
    env.claim(&proposal, &alice, &alice_ata, 0).expect("alice claims her own 10");
    env.claim(&proposal, &bob, &bob_ata, 1).expect("bob claims his own 90");
    assert_eq!(env.token_amount(&alice_ata), 10, "alice got exactly her entry");
    assert_eq!(env.token_amount(&bob_ata), 90, "bob got exactly his entry");
}

// LOF PROBE (third-party claim-theft, sweep tick C): the cross/outsider test above has the attacker SIGN as
// themselves and get rejected on the pubkey bind (lib.rs:577). The DISTINCT, load-bearing guard is
// `recipient.is_signer` (lib.rs:544): a cranker passes the VICTIM as a NON-signer recipient account (so the
// entry pubkey == recipient.key bind PASSES) but routes the payout to the attacker's OWN ata. Without the
// signer check this is a clean claim-theft LOF — anyone could drain every named recipient into their own
// wallet. Pins that the claim is a genuine pull authorized by the recipient's signature, not a permissionless
// push a third party can redirect.
#[test]
fn claim_requires_the_named_recipients_signature_no_third_party_redirect_theft() {
    let mut env = Env::new(100, 1_000_000);
    let proposal = env.create_proposal(1, 4);
    let (victim, victim_ata) = env.new_recipient();
    let (_attacker, attacker_ata) = env.new_recipient();
    env.append(&proposal, &[(victim.pubkey(), 100)]).expect("append");
    let auth = clone_kp(&env.authority);
    env.seal(&proposal, &auth).expect("seal");

    let vault = env.vault;
    let config = env.config;
    // Crank a claim for the victim's index 0 with the victim as a NON-signer, paying the attacker's ata.
    let mut data = vec![4u8]; // IX_CLAIM
    data.extend_from_slice(&0u32.to_le_bytes());
    let ix = Instruction { program_id: pid(), accounts: vec![
        AccountMeta::new_readonly(victim.pubkey(), false), // VICTIM, but NOT a signer
        AccountMeta::new_readonly(config, false),
        AccountMeta::new(proposal, false),
        AccountMeta::new(vault, false),
        AccountMeta::new(attacker_ata, false),             // payout redirected to the attacker's own ata
        AccountMeta::new_readonly(spl_token::ID, false),
    ], data };
    assert!(env.send(&[ix], &[]).is_err(), "a claim without the named recipient's signature must be rejected (no third-party theft)");
    assert_eq!(env.token_amount(&attacker_ata), 0, "attacker harvested nothing");
    assert_eq!(env.token_amount(&vault), 100, "vault byte-for-byte untouched by the unsigned claim");

    // The legitimate recipient still claims their own entry in full (the guard didn't trap the funds).
    env.claim(&proposal, &victim, &victim_ata, 0).expect("victim claims their own entry");
    assert_eq!(env.token_amount(&victim_ata), 100, "victim recovers their full entry");
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

// RE-CREATE A LIVE PROPOSAL (creator/entry reset, sibling of a_sealed_config_cannot_be_reinitialized). The
// proposal PDA is [config, proposal_id] — NOT keyed by creator — so anyone may ATTEMPT to create a given id.
// create_proposal rejects a non-empty proposal account (data_len != 0 -> AccountAlreadyInitialized). Without
// it, a second party could re-create an id the real creator already built (and voters may have backed via gv),
// RESETTING header.creator + wiping entry_count -> seizing append rights and erasing the approved list (the gv
// snapshot would later reject the seal, but the list is already corrupted). DOUBLY-DEFENDED like the config:
// even absent the data_len check, create_pda_robust's System allocate only runs on a system-owned, data-empty
// account, and a live proposal is distribution-owned -> the re-create cannot allocate.
#[test]
fn create_proposal_cannot_recreate_a_live_proposal_to_reset_it() {
    let mut env = Env::new(100, 1_000_000);
    let proposal = env.create_proposal(1, 4);
    let (alice, alice_ata) = env.new_recipient();
    env.append(&proposal, &[(alice.pubkey(), 100)]).expect("the creator builds the approved list");

    // ATTACK: a DIFFERENT party re-creates the SAME proposal id to reset its creator + wipe its entries.
    let attacker = Keypair::new();
    env.svm.airdrop(&attacker.pubkey(), 1_000_000_000).unwrap();
    let mut data = vec![1u8]; // IX_CREATE_PROPOSAL
    data.extend_from_slice(&1u64.to_le_bytes()); // the SAME id
    data.extend_from_slice(&8u32.to_le_bytes()); // a different capacity
    let recreate = Instruction { program_id: pid(), accounts: vec![
        AccountMeta::new(attacker.pubkey(), true), AccountMeta::new_readonly(env.config, false),
        AccountMeta::new(proposal, false), AccountMeta::new_readonly(solana_sdk::system_program::ID, false)], data };
    assert!(env.send(&[recreate], &[&attacker]).is_err(), "a live proposal cannot be re-created/reset by anyone");

    // The original proposal is intact — creator + entries unchanged.
    let hdr = env.svm.get_account(&proposal).unwrap().data;
    assert_eq!(u32::from_le_bytes(hdr[84..88].try_into().unwrap()), 1, "entry_count unchanged — the list was not wiped");
    assert_eq!(&hdr[48..80], env.payer.pubkey().as_ref(), "creator unchanged — append rights not seized");

    // The genuine creator still seals, and alice claims her preserved 100 — the approved list survived.
    let auth = clone_kp(&env.authority);
    env.seal(&proposal, &auth).expect("the original, intact proposal still seals");
    env.claim(&proposal, &alice, &alice_ata, 0).expect("alice claims her preserved entry");
    assert_eq!(env.token_amount(&alice_ata), 100, "the approved entry survived the re-create attempt");
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

// DEAD-GENESIS (sealing an EMPTY proposal). seal_winner rejects header.entry_count == 0 (lib.rs). Sealing an
// empty distribution would finalize the genesis to a list NOBODY can claim — the entire fixed COIN supply
// becomes burnable and no recipient is ever paid (a total-loss griefing of the genesis outcome). genesis-vote
// separately blocks REGISTERING an empty proposal for voting (seal.rs), but distribution is a PLUGGABLE seam:
// its own entry_count==0 guard is the decider-AGNOSTIC backstop that also protects the residual-distributor
// seal path (and any future decider). That distribution-level guard was untested. A rejected empty seal must
// also leave the config UNsealed, so a real winner can still seal afterward (the attempt can't brick genesis).
#[test]
fn seal_rejects_an_empty_proposal_no_dead_genesis() {
    let mut env = Env::new(100, 1_000_000);
    let empty = env.create_proposal(1, 4); // created but never appended -> entry_count == 0
    let auth = clone_kp(&env.authority);

    // The authority (correct key + signature) tries to seal the empty proposal -> rejected by entry_count==0.
    assert!(env.seal(&empty, &auth).is_err(), "an empty proposal (entry_count==0) cannot be sealed");
    let cfg = env.svm.get_account(&env.config).unwrap();
    assert_eq!(&cfg.data[120..152], &[0u8; 32], "the rejected empty seal left the config UNsealed — genesis not bricked");

    // A real, non-empty proposal still seals afterward and pays its recipient — the empty attempt was inert.
    let real = env.create_proposal(2, 4);
    let (alice, alice_ata) = env.new_recipient();
    env.append(&real, &[(alice.pubkey(), 100)]).expect("append a real entry");
    env.seal(&real, &auth).expect("a non-empty proposal seals normally");
    env.claim(&real, &alice, &alice_ata, 0).expect("the recipient claims");
    assert_eq!(env.token_amount(&alice_ata), 100, "genesis finalized to a LIVE distribution, not a dead one");
}

// CROSS-CONFIG SEAL (decider-agnostic self-protection). distribution is a pluggable seam — it must NOT
// assume the calling decider passes one of its OWN proposals. seal_winner binds `header.config ==
// config_account.key` (lib.rs:511) so config B's authority can never seal config A's proposal (which would
// make B's vault pay A's recipients — a mis-distribution / LOF). A buggy or hostile decider that hands
// seal_winner a foreign proposal is rejected at the program level. This isolates that bind: config B's
// real authority signs (auth check passes), the proposal is program-owned and fits B's supply (so absent
// the bind it WOULD seal), and the only deviation is its header.config -> rejected; B stays unsealed.
#[test]
fn seal_rejects_a_proposal_from_a_foreign_config() {
    // Config A with a real proposal (header.config = A).
    let mut env = Env::new(1_000, 1_000_000);
    let alice = Pubkey::new_unique();
    let proposal_a = env.create_proposal(1, 4);
    env.append(&proposal_a, &[(alice, 1_000)]).expect("append to A");

    // Stand up an independent config B (own coin_mint + authority + funded vault).
    let mint_auth_b = Keypair::new();
    let coin_b = create_mint(&mut env.svm, &env.payer, &mint_auth_b.pubkey());
    let authority_b = Keypair::new();
    let config_b = dist_config_pda(&coin_b, &authority_b.pubkey(), 1_000_000);
    let vault_b = create_token_account(&mut env.svm, &env.payer, &coin_b, &config_b);
    mint_to(&mut env.svm, &env.payer, &coin_b, &mint_auth_b, &vault_b, 1_000);
    revoke_mint_authority(&mut env.svm, &env.payer, &coin_b, &mint_auth_b);
    let mut d = vec![0u8]; d.extend_from_slice(&1_000_000u64.to_le_bytes()); d.extend_from_slice(&1_000u64.to_le_bytes());
    env.send(&[Instruction { program_id: pid(), accounts: vec![
        AccountMeta::new(env.payer.pubkey(), true), AccountMeta::new_readonly(coin_b, false),
        AccountMeta::new(config_b, false), AccountMeta::new_readonly(vault_b, false),
        AccountMeta::new_readonly(authority_b.pubkey(), false), AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
    ], data: d }], &[]).expect("init config B");

    // ATTACK: config B's authority seals config A's proposal. header.config (A) != config B -> rejected.
    let bad = Instruction { program_id: pid(), accounts: vec![
        AccountMeta::new_readonly(authority_b.pubkey(), true),
        AccountMeta::new(config_b, false),
        AccountMeta::new(proposal_a, false),
    ], data: vec![3u8] };
    assert!(env.send(&[bad], &[&authority_b]).is_err(), "config B must not seal a proposal belonging to config A");
    let cfg_b = env.svm.get_account(&config_b).unwrap();
    assert_eq!(&cfg_b.data[120..152], &[0u8; 32], "config B stayed unsealed — no cross-config seal");
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

// WINDOW CUTOFF EXACTNESS (off-by-one guard, sweep tick C): claim rejects when clock.slot >= window_end
// (lib.rs:565) and burn rejects when clock.slot < window_end (:637) — disjoint, no gap, no overlap. The burn
// test pins claim-REJECTED at exactly window_end; this pins the complement — a claim SUCCEEDS on the LAST
// in-window slot (window_end - 1) — proving the cutoff is exactly window_end (recipients get the FULL window, not
// window-1, and there is no dead slot where neither claim nor burn is allowed).
#[test]
fn claim_succeeds_through_the_last_window_slot_and_fails_exactly_at_window_end() {
    let mut env = Env::new(100, 50); // window = 50 slots
    env.set_slot(10);
    let proposal = env.create_proposal(1, 4);
    let (alice, alice_ata) = env.new_recipient();
    let (bob, bob_ata) = env.new_recipient();
    env.append(&proposal, &[(alice.pubkey(), 60), (bob.pubkey(), 40)]).expect("append");
    let auth = clone_kp(&env.authority);
    env.seal(&proposal, &auth).expect("seal"); // seal_slot = 10, window_end = 60

    // LAST valid claim slot: window_end - 1 = 59. alice claims successfully (the window is inclusive of 59).
    env.set_slot(59);
    env.claim(&proposal, &alice, &alice_ata, 0).expect("claim must succeed on the final in-window slot (window_end - 1)");
    assert_eq!(env.token_amount(&alice_ata), 60, "alice claimed on the last window slot");

    // EXACTLY window_end = 60: the window is closed (claim uses >= window_end). bob's claim is rejected.
    env.set_slot(60);
    assert!(env.claim(&proposal, &bob, &bob_ata, 1).is_err(), "claim rejected exactly at window_end (>=, not >)");
    assert_eq!(env.token_amount(&bob_ata), 0, "bob got nothing — the window closed inclusively at window_end");
}

// CONSERVATION (burn destroys unclaimed allocations AND unallocated headroom): the vault is funded with the
// FULL fixed supply, but a winning proposal may allocate LESS than that (total_amount <= total_supply). After
// claims, the vault still holds (a) any unclaimed entries PLUS (b) the unallocated headroom (supply -
// total_amount). burn_unclaimed burns the WHOLE remaining vault (lib.rs:641), so BOTH must be destroyed — else
// the headroom would sit locked in the vault forever or leak. The existing burn test allocates the full supply
// (headroom = 0), so the headroom-burn half of conservation was untested. Pins: claimed + burned == supply, and
// the COIN mint supply drops by exactly (unclaimed + headroom) — real deflation to the COIN holders.
#[test]
fn burn_unclaimed_also_burns_unallocated_headroom_full_conservation() {
    let mut env = Env::new(100, 50); // fixed supply = 100, vault funded with all 100
    env.set_slot(10);
    let proposal = env.create_proposal(1, 4);
    let (alice, alice_ata) = env.new_recipient();
    let (bob, bob_ata) = env.new_recipient();
    // Allocate only 70 of the 100 supply: alice 50, bob 20 -> 30 of unallocated HEADROOM left in the vault.
    env.append(&proposal, &[(alice.pubkey(), 50), (bob.pubkey(), 20)]).expect("append 70 of 100");
    let auth = clone_kp(&env.authority);
    env.seal(&proposal, &auth).expect("seal"); // seal_slot = 10, window ends at 60

    // Alice claims her 50; bob never claims his 20. Vault now holds bob's 20 + the 30 headroom = 50.
    env.claim(&proposal, &alice, &alice_ata, 0).expect("alice claim");
    assert_eq!(env.token_amount(&alice_ata), 50, "alice got exactly her allocation");
    assert_eq!(env.token_amount(&env.vault.clone()), 50, "vault = bob's unclaimed 20 + 30 unallocated headroom");

    // Past the window the burn destroys the WHOLE remaining vault: bob's unclaimed 20 AND the 30 headroom.
    env.set_slot(60);
    let mint_before = spl_token::state::Mint::unpack(&env.svm.get_account(&env.coin_mint).unwrap().data).unwrap().supply;
    env.burn_unclaimed().expect("burn unclaimed + headroom");
    let mint_after = spl_token::state::Mint::unpack(&env.svm.get_account(&env.coin_mint).unwrap().data).unwrap().supply;
    assert_eq!(env.token_amount(&env.vault.clone()), 0, "vault fully emptied — no headroom stranded");
    assert_eq!(mint_before - mint_after, 50, "burned exactly bob's unclaimed 20 + the 30 unallocated headroom");
    // Full conservation: claimed (alice 50) + burned (50) == the fixed supply (100). Nothing leaked or stranded.
    assert_eq!(env.token_amount(&alice_ata) + (mint_before - mint_after), 100, "claimed + burned == total supply");
    // Bob's window has closed — his unclaimed entry is gone (burned), not recoverable.
    assert!(env.claim(&proposal, &bob, &bob_ata, 1).is_err(), "bob cannot claim after the window / burn");
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

// ATTACK PROBE (split-allocation supply-cap bypass across appends): append accumulates total_amount into the
// PERSISTED proposal header and re-checks `total_amount <= total_supply` on every entry (lib.rs:append_entries,
// checked_add). The single-append test (append_cannot_exceed_total_supply) only proves one oversized chunk is
// rejected; it does NOT prove the cap is CUMULATIVE across calls. If the running total reset per call, a creator
// could split an over-allocation into many small appends — each individually under-supply — and the sealed list
// would promise more COIN than the vault holds, so the LAST claimers get nothing (a vault over-draw / claim
// insolvency). This pins the cross-call accumulation, the exactly-equal boundary, and that a rejected overflow
// append leaves the prior committed entries intact + claimable. Real distribution .so.
#[test]
fn append_supply_cap_is_cumulative_across_calls_and_a_rejected_overflow_preserves_prior_entries() {
    let mut env = Env::new(100, 1_000_000); // fixed supply = 100
    let proposal = env.create_proposal(1, 8);
    let (alice, alice_ata) = env.new_recipient();
    let (bob, bob_ata) = env.new_recipient();
    let (carol, carol_ata) = env.new_recipient();

    // Append A commits alice=60 (persisted total = 60).
    env.append(&proposal, &[(alice.pubkey(), 60)]).expect("append A: 60 <= 100");
    // Append B (50) is rejected by the CUMULATIVE cap: persisted 60 + 50 = 110 > 100. A per-call reset would
    // have wrongly accepted it.
    assert!(env.append(&proposal, &[(bob.pubkey(), 50)]).is_err(), "split-allocation past supply is rejected cumulatively");
    // Append C (40) fills EXACTLY to the supply: 60 + 40 = 100 (cap is `> supply`, so == supply is allowed).
    env.append(&proposal, &[(bob.pubkey(), 40)]).expect("append C: 60 + 40 == 100 is the boundary, allowed");
    // Append D (even 1) now overflows: 100 + 1 > 100 -> rejected. The vault can never be promised more than it holds.
    assert!(env.append(&proposal, &[(carol.pubkey(), 1)]).is_err(), "once total == supply, any further entry rejects");

    // The rejected appends left NO phantom entries: sealing and claiming distributes exactly 100, no more.
    let auth = clone_kp(&env.authority);
    env.seal(&proposal, &auth).expect("seal");
    env.claim(&proposal, &alice, &alice_ata, 0).expect("alice claims her committed 60");
    env.claim(&proposal, &bob, &bob_ata, 1).expect("bob claims his committed 40");
    assert_eq!(env.token_amount(&alice_ata), 60, "alice's prior entry survived the rejected appends");
    assert_eq!(env.token_amount(&bob_ata), 40, "bob's entry is the exactly-to-supply boundary entry");
    assert_eq!(env.token_amount(&carol_ata), 0, "carol's overflow entry was never committed");
    assert_eq!(env.token_amount(&env.vault.clone()), 0, "exactly the supply distributed — no over-draw, no stranded headroom");
}

// CORRUPTION/DoS PROBE (entry-count capacity cap + atomic partial-batch, sweep tick C): the proposal account is
// sized for exactly `capacity` entries (create:414); appending a (capacity+1)th entry would write past the
// account. append guards it per-entry (lib.rs:467 entry_count >= capacity -> reject) BEFORE the write, and since
// header.serialize runs only AFTER the loop, a batch that overflows capacity mid-loop must commit NEITHER entry
// (atomic revert). The supply-cap test above uses capacity 8 (never binds); this pins the capacity cap itself
// and the partial-batch atomicity — distinct from the economic supply cap.
#[test]
fn append_entry_count_capacity_cap_and_an_overflowing_batch_reverts_atomically() {
    let mut env = Env::new(1_000, 1_000_000); // supply 1000 — generous, so only CAPACITY binds
    let proposal = env.create_proposal(1, 2); // capacity = 2 entries
    let (alice, alice_ata) = env.new_recipient();
    let (bob, _bob_ata) = env.new_recipient();
    let (carol, _carol_ata) = env.new_recipient();
    let (dave, dave_ata) = env.new_recipient();
    let (eve, _eve_ata) = env.new_recipient();

    // One entry committed (entry_count 1 of capacity 2).
    env.append(&proposal, &[(alice.pubkey(), 10)]).expect("append alice: 1 of 2");
    // A BATCH of two would take entry_count 1 -> 3 > capacity 2. bob fits mid-loop (1->2), carol hits the cap
    // (:467) -> Err -> the whole tx reverts, so bob's mid-loop write is rolled back too.
    assert!(env.append(&proposal, &[(bob.pubkey(), 20), (carol.pubkey(), 30)]).is_err(),
        "a batch that overflows capacity is rejected");
    // PROOF of atomicity: the slot bob would have taken is still FREE — a single dave append succeeds into it.
    // If bob had been partially committed (entry_count==2), this would be rejected as full.
    env.append(&proposal, &[(dave.pubkey(), 40)]).expect("the overflow batch committed NOTHING — slot 1 is still free for dave");
    // Now full (entry_count 2 == capacity 2): any further entry is rejected by the capacity cap.
    assert!(env.append(&proposal, &[(eve.pubkey(), 50)]).is_err(), "appending past capacity is rejected");

    // Seal + claim: exactly alice(0)=10 and dave(1)=40 exist; bob/carol/eve were never committed.
    let auth = clone_kp(&env.authority);
    env.seal(&proposal, &auth).expect("seal");
    env.claim(&proposal, &alice, &alice_ata, 0).expect("alice claims index 0");
    env.claim(&proposal, &dave, &dave_ata, 1).expect("dave claims index 1 — the slot the overflow batch did NOT take");
    assert_eq!(env.token_amount(&alice_ata), 10, "alice committed");
    assert_eq!(env.token_amount(&dave_ata), 40, "dave got the second slot cleanly");
}

// MAX-SHAPE PROGRESS PROBE: the public API advertises 10,000-entry distributions. The
// maximum-sized proposal must be allocatable, fillable through transaction-sized append chunks,
// sealable, and able to serve its final index under the normal compute budget.
#[test]
fn max_capacity_proposal_can_fill_seal_and_claim_the_last_entry() {
    const ENTRIES: u32 = 10_000;
    const CHUNK: usize = 24;
    let mut env = Env::new(ENTRIES as u64, 1_000_000);
    let proposal = env.create_proposal(99, ENTRIES);
    assert_eq!(env.svm.get_account(&proposal).unwrap().data.len(), 104,
        "creation allocates only the header");

    let (recipient, recipient_ata) = env.new_recipient();
    let entry = (recipient.pubkey(), 1u64);
    let mut appended = 0u32;
    while appended < ENTRIES {
        let count = core::cmp::min(CHUNK, (ENTRIES - appended) as usize);
        env.append(&proposal, &vec![entry; count]).expect("append max-capacity chunk");
        appended += count as u32;
    }
    assert_eq!(env.svm.get_account(&proposal).unwrap().data.len(), 104 + ENTRIES as usize * 40,
        "appends grow to the declared final capacity");

    let authority = clone_kp(&env.authority);
    env.seal(&proposal, &authority).expect("seal max proposal");
    env.claim(&proposal, &recipient, &recipient_ata, ENTRIES - 1)
        .expect("claim final max-capacity index");
    assert_eq!(env.token_amount(&recipient_ata), 1);
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
    let config = dist_config_pda(&coin_mint, &authority.pubkey(), 1_000_000);
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
    let config = dist_config_pda(&coin_mint, &authority.pubkey(), 1_000_000);
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

// TYPE-COSPLAY INIT SQUAT (permissionless-init DOS): init_config unpacks the caller-supplied
// `vault` as an SPL Token account via Pack::unpack, which does NOT verify the account's owning
// program. A front-runner can therefore hand init_config a NON-SPL-owned account whose bytes are
// shaped like an initialized token account (mint == COIN, owner == config PDA, amount ==
// total_supply) so it clears every structural check (mint/owner/amount). Because the config PDA is
// canonical per (mint, authority) and cannot be re-initialized, this squats the one real
// distribution config with a vault that no SPL Token CPI (claim/burn) can ever drive: permanent
// distribution DOS. init_config must reject vault.owner != spl_token::ID before unpacking. (The
// coin_mint owner check is symmetric, guarding the same type-cosplay on the mint input.)
#[test]
fn init_config_rejects_a_non_spl_owned_token_shaped_vault() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(pid(), so_path()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let mint_authority = Keypair::new();
    let coin_mint = create_mint(&mut svm, &payer, &mint_authority.pubkey());

    let authority = Keypair::new();
    let config = dist_config_pda(&coin_mint, &authority.pubkey(), 1_000_000);

    // Mint the full supply to a real holder and revoke authority, so the COIN mint itself satisfies
    // the fixed-supply invariants (mint.supply == total_supply, no mint/freeze authority). This
    // isolates the vault-owner check as the sole reason the init must fail.
    let real_holder = create_token_account(&mut svm, &payer, &coin_mint, &payer.pubkey());
    mint_to(&mut svm, &payer, &coin_mint, &mint_authority, &real_holder, 100);
    revoke_mint_authority(&mut svm, &payer, &coin_mint, &mint_authority);

    // Craft a SYSTEM-owned account whose data round-trips through SPL Token's own packer as an
    // initialized token account claiming the full supply, owned by the config PDA — i.e. it passes
    // everything init_config checks EXCEPT the owning program.
    let fake_state = spl_token::state::Account {
        mint: coin_mint,
        owner: config,
        amount: 100,
        delegate: COption::None,
        state: spl_token::state::AccountState::Initialized,
        is_native: COption::None,
        delegated_amount: 0,
        close_authority: COption::None,
    };
    let mut fake_data = vec![0u8; spl_token::state::Account::LEN];
    spl_token::state::Account::pack(fake_state, &mut fake_data).unwrap();
    let fake_vault = Pubkey::new_unique();
    svm.set_account(
        fake_vault,
        Account {
            lamports: svm.minimum_balance_for_rent_exemption(fake_data.len()),
            data: fake_data,
            owner: solana_sdk::system_program::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();

    let mut data = vec![0u8]; // IX_INIT_CONFIG
    data.extend_from_slice(&1_000_000u64.to_le_bytes()); // claim window
    data.extend_from_slice(&100u64.to_le_bytes()); // total_supply == mint.supply == fake vault amount
    let ix = Instruction {
        program_id: pid(),
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(coin_mint, false),
            AccountMeta::new(config, false),
            AccountMeta::new_readonly(fake_vault, false),
            AccountMeta::new_readonly(authority.pubkey(), false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data,
    };
    let bh = svm.latest_blockhash();
    let r = svm.send_transaction(Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], bh));
    assert!(r.is_err(), "a token-shaped vault not owned by SPL Token must be rejected");
    assert!(svm.get_account(&config).map_or(true, |a| a.data.is_empty()), "the fake vault must not squat the config PDA");
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
    let config = dist_config_pda(&coin_mint, &authority.pubkey(), 0);
    let vault = create_token_account(&mut svm, &payer, &coin_mint, &config);
    mint_to(&mut svm, &payer, &coin_mint, &mint_authority, &vault, 100); // fully back supply 100
    revoke_mint_authority(&mut svm, &payer, &coin_mint, &mint_authority);

    let build = || {
        let mut data = vec![0u8]; // IX_INIT_CONFIG
        data.extend_from_slice(&0u64.to_le_bytes());
        data.extend_from_slice(&100u64.to_le_bytes());
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

    // Every other input is valid for the zero-window PDA: without the explicit guard,
    // this transaction would initialize successfully and make every claim impossible.
    assert!(send(&mut svm, build()).is_err(), "a zero claim window must be rejected (no one could ever claim)");
    assert!(svm.get_account(&config).map_or(true, |a| a.data.is_empty()), "the rejected zero-window config must remain uninitialized");
}

// CLAIM-WINDOW FIRST-WRITER BIND: init_config is permissionless. If the config PDA is only
// `(coin_mint, authority)`, the first initializer can lock the funded vault into a hostile
// claim_window_slots value before the intended setup lands. Bind the window into the config PDA so
// a wrong-window first write hits a different address, while the intended address can still run the
// full seal -> claim path with PDA signer seeds that include the same window.
#[test]
fn init_config_binds_claim_window_into_the_config_pda_and_signer_seeds() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(pid(), so_path()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let mint_authority = Keypair::new();
    let coin_mint = create_mint(&mut svm, &payer, &mint_authority.pubkey());
    let authority = Keypair::new();
    let intended_window = 1_000_000u64;
    let hostile_window = 1u64;
    let config = dist_config_pda(&coin_mint, &authority.pubkey(), intended_window);
    let vault = create_token_account(&mut svm, &payer, &coin_mint, &config);
    mint_to(&mut svm, &payer, &coin_mint, &mint_authority, &vault, 100);
    revoke_mint_authority(&mut svm, &payer, &coin_mint, &mint_authority);

    let build_init = |claim_window: u64| {
        let mut data = vec![0u8];
        data.extend_from_slice(&claim_window.to_le_bytes());
        data.extend_from_slice(&100u64.to_le_bytes());
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
    let send_init = |svm: &mut LiteSVM, ix: Instruction| -> Result<(), String> {
        svm.expire_blockhash();
        let bh = svm.latest_blockhash();
        svm.send_transaction(Transaction::new_signed_with_payer(
            &[ix],
            Some(&payer.pubkey()),
            &[&payer],
            bh,
        ))
        .map(|_| ())
        .map_err(|e| format!("{:?}", e))
    };

    assert!(
        send_init(&mut svm, build_init(hostile_window)).is_err(),
        "a hostile claim window must not initialize the intended config PDA"
    );
    assert!(
        svm.get_account(&config).map_or(true, |a| a.data.is_empty()),
        "the failed wrong-window attempt left the intended config uninitialized"
    );
    send_init(&mut svm, build_init(intended_window)).expect("intended window initializes");

    let mut env = Env {
        svm,
        payer,
        coin_mint,
        mint_authority,
        config,
        vault,
        authority,
    };
    let proposal = env.create_proposal(1, 1);
    let (recipient, recipient_ata) = env.new_recipient();
    env.append(&proposal, &[(recipient.pubkey(), 100)])
        .expect("append recipient");
    let auth = clone_kp(&env.authority);
    env.seal(&proposal, &auth).expect("seal");
    env.claim(&proposal, &recipient, &recipient_ata, 0)
        .expect("claim signed by the window-bound config PDA");
    assert_eq!(env.token_amount(&recipient_ata), 100);
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
    let config = dist_config_pda(&coin_mint, &auth, 1_000_000);
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
    let config = dist_config_pda(&coin_mint, &auth, 1_000_000);
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
    let config = dist_config_pda(&coin_mint, &authority, 1_000_000);
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
    let config = dist_config_pda(&coin_mint, &authority, 1_000_000);
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
    let legit_config = dist_config_pda(&coin_mint, &legit_authority, 1_000_000);
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
    let attacker_config = dist_config_pda(&coin_mint, &attacker.pubkey(), 1_000_000);
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

// DoS/BRICK PROBE (wrong-mint vault, sweep tick C): init_config binds the vault on BOTH owner AND mint
// (lib.rs:346 vault_state.mint != coin_mint || vault_state.owner != expected_config). The hijack test above
// covers the OWNER half; the MINT half was untested. A vault that is SPL-owned, owned by the CORRECT config PDA,
// and funded to total_supply — but of a DIFFERENT mint — would (without the guard) pass init, yet every claim's
// SPL transfer (vault -> coin_mint recipient ata) fails on mint mismatch -> the whole genesis is bricked and the
// real COIN supply stranded forever. Pin that init catches it up front (not at the first failed claim).
#[test]
fn init_config_rejects_a_vault_of_the_wrong_mint_no_bricked_undistributable_genesis() {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(pid(), so_path()).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    let mint_authority = Keypair::new();
    let coin_mint = create_mint(&mut svm, &payer, &mint_authority.pubkey());
    let authority = Pubkey::new_unique();
    let config = dist_config_pda(&coin_mint, &authority, 1_000_000);

    // coin_mint's ENTIRE 100 supply is minted (to a decoy) so the supply==total_supply check passes and we
    // REACH the vault.mint guard; then revoke the authority (fixed-supply invariant).
    let decoy = create_token_account(&mut svm, &payer, &coin_mint, &Pubkey::new_unique());
    mint_to(&mut svm, &payer, &coin_mint, &mint_authority, &decoy, 100);
    revoke_mint_authority(&mut svm, &payer, &coin_mint, &mint_authority);

    // A WRONG-mint vault: a DIFFERENT SPL mint, owned by the CORRECT config PDA, funded with 100 — so ONLY the
    // mint differs (the owner is right), isolating the vault.mint guard.
    let wrong_auth = Keypair::new();
    let wrong_mint = create_mint(&mut svm, &payer, &wrong_auth.pubkey());
    let wrong_vault = create_token_account(&mut svm, &payer, &wrong_mint, &config);
    mint_to(&mut svm, &payer, &wrong_mint, &wrong_auth, &wrong_vault, 100);

    let mut data = vec![0u8];
    data.extend_from_slice(&1_000_000u64.to_le_bytes()); // claim_window_slots
    data.extend_from_slice(&100u64.to_le_bytes());       // total_supply
    let ix = Instruction { program_id: pid(), accounts: vec![
        AccountMeta::new(payer.pubkey(), true), AccountMeta::new_readonly(coin_mint, false), AccountMeta::new(config, false),
        AccountMeta::new_readonly(wrong_vault, false), AccountMeta::new_readonly(authority, false), AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
    ], data };
    let bh = svm.latest_blockhash();
    assert!(
        svm.send_transaction(Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], bh)).is_err(),
        "a vault of the wrong mint must be rejected at init (else every claim bricks on SPL mint mismatch — undistributable genesis)"
    );
}
