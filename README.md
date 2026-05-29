# Percolator Meta

A Solana program that bootstraps a Percolator market and distributes the initial COIN supply through a **Sybil-resistant governance vote**.

Depositing capital is purely a Sybil check, not an investment. Participants put principal **at risk** in the bootstrap market for one reason — to earn the right to vote on the COIN distribution. There is **no financial reward**: no yield and no profit share. The capital *is* custodied by the program — it is transferred into program-owned vaults and deployed into the market, and can only be withdrawn back out through the program — but the rules are fixed and non-discretionary: no party can divert depositor principal (governance may draw only genuine *surplus* above it), and depositors recover their principal (pro-rata against recoverable funds if the market lost money). The capital at risk *is* the cost of voting, which is what makes votes expensive to Sybil.

The winning COIN distribution becomes the **MetaDAO**: the new COIN is its token, and it inherits the program's keys (via the Squads handover) along with any surplus.

## ⚠️ Status & Disclaimer

Experimental, **educational-use-only** software, provided **AS IS** with no warranties or conditions of any kind (see [LICENSE](LICENSE)). This is research/educational code, not financial advice and not a guarantee of correctness or fitness for any purpose. Genesis participants put real capital **at risk** in a live market and can lose it — the deposit is a Sybil-resistance bond, not an investment. Use at your own risk.

## Genesis Plan

1. MetaDAO initializes `CoinConfig` with a configurable bootstrap delay in slots. A zero delay is live immediately; the intended launch setting can be a six-month slot delay.
2. Futarchy creates `GenesisConfig` with a fixed COIN reward supply.
3. Anyone may join at any time during the deposit phase — there is no fixed window — by depositing base units into `genesis_vault`; additional deposits are allowed. Each deposit (re)sets the position's `start_slot` to the current slot (last-write-time), so topping up late resets the clock. Deposits close when futarchy deploys the pool at kickstart.
4. **Depositing is a Sybil check, not an investment.** The capital is put at risk in the bootstrap market for one purpose: to earn voting power over the COIN distribution. Depositors receive no yield and no profit share — committed capital-at-risk is simply the cost of a vote.
5. Futarchy kickstarts the Percolator market with the pooled capital as a 50/50 split (`floor(total / 2)` to insurance, the remainder to backing), and sets a capital-protected withdraw policy (`max_bps=10000, deposits_only=1, cooldown=0`) so principal — never market profits — is recoverable.
6. **Withdraw any time; holding is what counts.** A capital provider may withdraw at any point — there is no lock-up. Withdrawal **never pulls from the market itself**: it pays a **pro-rata share of the shared `genesis_vault`** (so if the market lost money everyone takes the same haircut, regardless of withdrawal order — no first-come-first-served bank run) and **forfeits the vote**. Bringing deployed capital back into that shared vault is the **futarchy-gated** `recover_genesis_market` step — keeping recovery governed keeps deposited principal at risk in the market until governance winds it down. Decoupling recovery (which fills the shared vault, helping everyone equally) from withdrawal (which takes a pro-rata slice) is what removes the bank run. (Open design tension: because recovery is governed, a depositor exiting *before* governance has recovered market capital realizes the un-recovered remainder as loss — the "exit any time at no loss" pillar trades off against keeping the Sybil bond at risk; this is a governance choice, not resolved here.) A non-voter exits freely whenever they like; a depositor backing a proposal must first **retract** their ballot (which backs their weight and principal out of that proposal's tally) so a vote can never outlive the capital that backed it. As people leave, the quorum denominator (outstanding principal) shrinks with them — quorum is recomputed against whoever is still committed.
7. **One vote, one proposal.** Anyone may register a candidate distribution; each depositor backs exactly one with their stake (re-voting moves it). Vote weight is time-weighted: `floor(log2(hold_time)) × principal`, resolved at vote time. Earlier/longer holders weigh more, and there is no weight without time committed at risk.
8. **Permissionless winner-take-all trigger.** The vote is the authority, so executing it needs no governance signer. Anyone may trigger a proposal once quorum is valid (`total_voted_principal × 2 > outstanding`) **and** it holds a strict majority of the cast log-weight (`support_weight × 2 > total_cast_weight`). The first valid trigger mints **100% of the fixed COIN supply** to that proposal's destination and ends the decision — winner-take-all. Triggering requires a kickstarted market, so COIN can never be issued on a genesis whose capital was never deployed.
9. Finalization requires a kicked market and `minted_supply == reward_supply` (i.e. a proposal has been triggered); it remains a thin governance step that sequences the principal recovery before withdrawals switch to the haircut path.
10. The triggered distribution's COIN **is** the MetaDAO token. Control of the market and its keys transfers to the MetaDAO through the Squads handover.

Any surplus (value in `genesis_vault` above outstanding principal) belongs to the MetaDAO, drawn through the keys it inherits — the program itself pays out only principal.

**Operational invariant:** futarchy must `kickstart_genesis_market` before (or together with) `activate_live`. Minting and finalization both require a kicked market, so going live without kickstarting can never issue COIN — and because there is no withdrawal lock, depositors can still exit (non-voters directly, voters after retracting), so capital is never stranded. The kickstart-before-live ordering itself remains a governance responsibility.

## Post-Genesis Lifecycle

After `activate_live`, anyone may create additional Percolator markets through `init_percolator_market`. The caller funds the market account, but the COIN-specific `percolator_market_admin` PDA becomes Percolator admin.

Futarchy controls the market lifecycle through explicit meta-program instructions:

- Percolator market init, asset lifecycle, oracle setup, fee policy, resolve, and close-slab cleanup.
- Builder-code approvals by `(coin_mint, builder_program, code_hash)` plus a terms hash.

Raw Percolator `UpdateAuthority` and funding/withdrawal tags are not exposed through the generic admin proxy. Custody-bearing authority changes must use explicit setup paths.

Cranks and permissionless Percolator maintenance remain external.

## Capital Accounting

There is **no yield-bearing deposit product** anywhere in this program. All deposited capital is a Sybil bond: at risk in the bootstrap market, recoverable on withdrawal (pro-rata under loss, since depositors bear the market risk), and earning only voting power — never a financial return. The program custodies the capital (it is transferred into program-owned vaults and deployed into the market, and is only withdrawable through the program), but no party can divert depositor principal: the program pays principal back, and only genuine surplus (value above outstanding principal) is the MetaDAO's, drawn through the keys it inherits.

After genesis, ongoing insurance/backing capital for the market is the MetaDAO's responsibility — funded from its treasury and surplus through the keys it holds — not from external depositors earning yield.

## Tested Surface

The LiteSVM suite runs against the real percolator, governance, rewards, and Squads v4 binaries. A single end-to-end test (`test_full_genesis_to_dao_lifecycle_end_to_end`) chains the entire lifecycle with no shortcuts: deposit → real market init → Squads 1/1+48h creation → real 50/50 kickstart → go live → a depositor exits (futarchy recovers market principal into the shared vault, then the depositor takes a pro-rata withdraw) → propose/vote/mint 100% of supply → finalize → hand the Squads config-authority to the winning DAO → remaining depositors withdraw. Individual phases are also covered in isolation:

- Configurable bootstrap delay, open-ended deposit phase, and live activation.
- Genesis deposit, vote, 100% supply mint, finalize, withdrawal, surplus, recovery, and 50/50 kickstart.
- Withdraw at any time: full refund from the vault before kickstart; after kickstart, a pro-rata share of the shared vault (capital is recovered from the market by the futarchy-gated `recover_genesis_market`). The pro-rata exit is **order-independent and loop-proof** in both the live decision window and post-finalization (`test_prefinalization_exit_is_pro_rata_and_order_independent`, `test_underfunded_genesis_haircut_is_order_independent_and_loop_proof`) — no first-come-first-served bank run. Non-voters exit freely, voters must retract their ballots first, and the quorum denominator recomputes as people leave — so a vote counts only if held through the final slot.
- Time-weighted votes: `floor(log2(hold_time)) × principal` rewards earlier/longer holders, last-write-time `start_slot` reset on re-deposit, one vote per voter to one proposal, and permissionless winner-take-all triggering (a proposal needs a strict majority of cast weight plus a principal quorum; exactly-half fails both).
- Permissionless market creation plus futarchy-controlled Percolator lifecycle/admin operations.
- Builder approvals and executable-program validation.
- Genesis→DAO Squads handover: program-created 1/1 + 48h multisig and config-authority rotation, driven through governance against the real mainnet Squads binary, plus a standalone harness proving timelock enforcement and upgrade-key rotation (`program/tests/squads_handover.rs`).
- Disabled legacy staking/reward-pool instruction tags.

Current full-suite smoke target:

```bash
cargo build-sbf --manifest-path governance/Cargo.toml
cargo build-sbf --manifest-path program/Cargo.toml
RUST_MIN_STACK=8388608 cargo test --manifest-path program/Cargo.toml --test integration
```

The integration test also requires a built Percolator BPF binary at `../percolator-prog/target/deploy/percolator_prog.so`.

## Instructions

| Tag | Instruction | Purpose |
|-----|-------------|---------|
| 3 | `init_coin_config` | One-time COIN governance/mint setup |
| 8 | `mint_reward` | Governance-gated discretionary COIN mint |
| 10 | `transfer_mint_authority` | Transfer or burn COIN mint authority |
| 11 | `activate_live` | Move from bootstrap to live after delay |
| 19 | `init_percolator_market` | Permissionless Percolator `InitMarket` via PDA admin |
| 20 | `percolator_admin` | Futarchy-gated Percolator lifecycle/admin CPI |
| 21 | `init_genesis_bootstrap` | Create genesis config and base-token vault |
| 22 | `genesis_deposit` | Sybil-bond deposit; (re)sets `start_slot` (last-write-time) |
| 23 | `genesis_withdraw` | Withdraw a pro-rata share of the shared vault at any time (no market pull; full-claim settle, order-independent); forfeits the vote |
| 24 | `trigger_genesis_distribution` | **Permissionless**: mint the full supply to a quorum-valid majority winner |
| 25 | `finalize_genesis` | Complete genesis after kickstart and full mint |
| 26 | `draw_genesis_surplus` | DAO draws surplus above outstanding principal |
| 27 | `kickstart_genesis_market` | Deploy genesis principal 50/50 to the market |
| 28 | `recover_genesis_market` | Futarchy-gated: recover bootstrap market principal into the shared `genesis_vault` |
| 29 | `init_genesis_distribution` | Register a candidate full-supply distribution |
| 30 | `vote_genesis_distribution` | Back one proposal or retract (action: back / retract) |
| 31 | `approve_builder` | Governed builder-code and terms registry |
| 32 | `init_genesis_squads` | Futarchy-gated CPI creating the per-coin Squads 1/1 multisig (48h timelock) |
| 33 | `handover_genesis_squads` | Rotate the multisig `config_authority` to the winning DAO after finalization |

Tags `0`–`2`, `4`–`7`, `9`, and the former risk-vault slots `12`–`18` are intentionally disabled: there is no yield-bearing deposit product, so the insurance/backing risk-vault accounting (`init_risk_vault`, `risk_deposit`, `risk_withdraw`, `sync_risk_vault`, `risk_claim_rewards`, …) is removed.

## Squads Handover

The genesis market's governance is held by a program-created [Squads v4](https://squads.so) multisig: a controlled 1/1 multisig with a 48-hour timelock whose `config_authority` is this program's `percolator_market_admin` PDA from genesis (`init_genesis_squads`). The multisig address is deterministic per coin (create-key seed `[b"genesis_squads", coin_mint]`).

Control transfers to the winning genesis DAO by rotating that `config_authority` (`handover_genesis_squads`, permitted only once genesis is finalized). Percolator's own `UpdateAuthority` is never touched, so no incoming-authority consent is required and depositor custody is never re-pointed. Both instructions are reached through the governance adapter (tags `17`/`18`).

## Key PDAs

| Account | Seeds |
|---------|-------|
| `CoinConfig` | `[b"coin_cfg", coin_mint]` |
| `CoinMintAuthority` | `[b"coin_mint_authority", coin_mint]` |
| `percolator_market_admin` | `[b"percolator_market_admin", coin_mint]` |
| `GenesisConfig` | `[b"genesis_cfg", coin_mint]` |
| `GenesisVault` | `[b"genesis_vault", coin_mint]` |
| `GenesisPosition` | `[b"genesis_position", genesis_cfg, user]` |
| `GenesisDistribution` | `[b"genesis_distribution", genesis_cfg, proposal_id]` |
| `BuilderApproval` | `[b"builder_approval", coin_mint, builder_program, code_hash]` |
| Squads create-key | `[b"genesis_squads", coin_mint]` (this program) |
| Squads multisig | `[b"multisig", b"multisig", create_key]` (Squads v4) |

## License

Licensed under the [Apache License 2.0](LICENSE). Provided "as is", educational use only — see the disclaimer above.
