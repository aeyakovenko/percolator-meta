# Percolator Insurance Deposit Program

Solana program that incentivizes insurance capital deposits for [Percolator](https://github.com/aeyakovenko/percolator-prog) markets, governed by MetaDAO futarchy.

## How It Works

1. **DAO creates a market** — MetaDAO governance initializes a Percolator market and sets up an insurance deposit pool with COIN reward parameters (yield rate, epoch length)
2. **Users deposit collateral** — Anyone can deposit collateral into the per-market vault and earn COIN (DAO token) as yield, proportional to their deposit size and duration
3. **Users withdraw anytime** — No lockup. Depositors get their full deposit back plus earned COIN rewards at any time
4. **Insurance profits flow to vault** — Insurance gains (liquidation fees, etc.) are sent to the vault by the DAO or external systems, increasing vault balance above total depositor capital
5. **DAO draws profits** — Governance can draw only the **profit** portion (`vault_balance - total_staked`) from the vault. Depositor capital is always protected
6. **After all depositors exit** — When `total_staked == 0`, the DAO can draw any remaining profit from the vault

## Capital Protection

**Depositor capital is never at risk from governance.** The `draw_insurance` instruction enforces:

```
drawable = vault_balance - total_staked   (profit only)
```

The DAO cannot draw below `total_staked`. Depositors always get their full deposit back.

COIN rewards are minted (not drawn from the vault), so they are also never at risk.

### How Rewards Work

Depositors earn COIN proportional to their share of the pool and time deposited (Synthetix-style accumulator):

```
reward_rate = n_per_epoch / epoch_slots   (COIN per slot for the entire pool)
your_rate = reward_rate * your_deposit / total_staked
```

| Scenario | Collateral returned | COIN rewards |
|----------|-------------------|--------------|
| Deposit and withdraw same slot | 100% | 0 COIN |
| Deposit 1 epoch, no profit draw | 100% | ~N COIN |
| Deposit 1 epoch, DAO drew profits | 100% (capital protected) | ~N COIN |
| Withdraw before others | 100% | pro-rata to time |
| Stay longer than others | 100% | more COIN (larger share after others leave) |

### Per-Market Isolation

Each market has independent:
- Deposit vault (separate SPL token account)
- Reward rate (`n_per_epoch`, `epoch_slots`)
- Total staked tracking
- Depositor positions (per-user PDAs)

**Isolation guarantees (cross-market):**
- DAO drawing profit from Market A does not touch Market B's vault
- Profit budget is per-market: profit in Market A does not let the DAO draw Market B's depositor capital
- A loss in Market A (defense-in-depth scenario) does not haircut Market B depositors
- The same user staking in two markets has two independent positions; withdrawing from one does not affect the other
- Operations on Market A's MRC do not mutate Market B's MRC state
- Cross-market account substitution (passing Market A's MRC with Market B's vault) is rejected by PDA verification

### Reward Conservation

Total COIN emitted equals `n_per_epoch × elapsed_epochs` (within at most 1 token per active depositor lost to fixed-point truncation). The accumulator math cannot create or destroy rewards beyond that bound.

### Withdrawal Guarantee

**The DAO cannot block withdrawals.** The `unstake` instruction is fully permissionless:
- No governance key is checked during withdrawal
- No governance-modifiable state gates the transfer
- Every account in the path is either user-controlled or program-derived (PDA)
- `claim_stake_rewards` also always succeeds — COIN is minted, not drawn from the vault

### Proportional Withdrawal (Defense-in-Depth)

As defense-in-depth, withdrawals use proportional math: `actual = min(amount, amount * vault_balance / total_staked)`. Under normal operation `vault_balance >= total_staked`, so this equals the full deposit. If the vault were ever underfunded (which `draw_insurance` prevents), all depositors would take the same proportional share.

### Attack Vector Analysis

| Vector | Protection |
|--------|-----------|
| DAO draws depositor capital | `draw_insurance` enforces `amount <= vault_balance - total_staked` — only profits drawable |
| Attacker draws from vault | Requires governance PDA signature — only DAO votes can trigger draws |
| Attacker withdraws another user's deposit | StakePosition PDA derived from `[market_slab, user_pubkey]` — cryptographically bound |
| Attacker uses fake MRC account | MRC verified via PDA derivation — fake accounts don't match expected key |
| Attacker inflates shared COIN | `init_market_rewards` requires governance authority — only DAO can register markets |
| Flash deposit to steal rewards | Same-slot deposit+withdraw earns 0 COIN |
| Withdraw 1 repeatedly for rounding profit | Integer division truncates down — repeated small withdrawals total exact deposit |
| 1-token dilution attack | 1 token in a 1M pool dilutes rewards by < 0.0001% — negligible |
| Direct vault transfer manipulation | Extra tokens become drawable profit — depositors unaffected |
| Freeze user's COIN tokens | COIN mint has no freeze authority — verified at init |
| Claim then unstake double-counting | `pending_rewards` zeroed on claim — no double payout |
| Cross-market vault substitution | MRC/vault/slab keys all PDA-derived from `market_slab` — substituting another market's vault fails the PDA check |
| Drain Market B using Market A's profit | Profit is computed per-market (`vault_B - total_staked_B`); Market A's surplus is irrelevant |
| Loss in one market spreads to another | Each market has its own SPL token account vault and MRC; no shared capital between markets |
| Steal another user's position via SP PDA | StakePosition PDA is derived from `[b"sp", market_slab, user]` — bound to both market and user |

## Tested Invariants

Each invariant is enforced by at least one integration test. Run `RUST_MIN_STACK=8388608 cargo test --manifest-path program/Cargo.toml --test integration` to verify all 87.

### Capital protection
- DAO cannot draw depositor capital — `test_draw_depositor_capital_rejected`
- DAO can draw exactly the available profit — `test_draw_only_profits`
- Depositors get full deposit back after DAO drew profits — `test_depositors_always_get_full_deposit_back`, `test_depositor_capital_protected_after_profit_draw`
- DAO can drain remaining vault only after all depositors exit — `test_draw_all_remaining_after_depositors_withdraw`
- Non-governance signers cannot call `draw_insurance` — `test_draw_insurance_non_governance_rejected`
- Zero-amount draws rejected — `test_draw_zero_amount_rejected`

### Withdrawal guarantee
- `unstake` succeeds even when the vault is fully empty — `test_withdrawal_always_succeeds_after_full_drain`
- `claim_stake_rewards` succeeds independent of vault state — `test_claim_rewards_always_works`
- No governance action can prevent withdrawal — `test_withdrawal_always_works_no_governance_block`

### Market isolation
- Drawing from Market A leaves Market B's vault exactly unchanged — `test_isolation_draw_from_market_a_does_not_touch_market_b`
- Profit in Market A does not let DAO drain Market B — `test_isolation_dao_cannot_draw_from_market_b_via_market_a_profit`
- Cross-market account substitution rejected — `test_isolation_cross_market_attack_wrong_mrc_with_other_vault`, `test_isolation_unstake_wrong_market_vault_rejected`
- Same user has independent positions in different markets — `test_isolation_alice_two_market_positions_independent`
- Loss/drain in Market A does not haircut Market B — `test_isolation_market_a_drained_does_not_haircut_market_b`
- Profit is computed per-market — `test_isolation_per_market_profit_calculation`
- MRC state changes are per-market — `test_isolation_market_a_loss_does_not_change_market_b_total_staked`
- DAO can drain remaining in one market while others continue — `test_isolation_dao_can_only_drain_after_local_market_depositors_exit`
- Different markets can have different yield rates — `test_two_markets_share_one_coin`

### Reward math
- Conservation: total emitted ≈ `n_per_epoch × elapsed_epochs` — `test_two_users_equal_stake`, `test_two_users_different_amounts`, `test_staker_joins_later`
- Proportional split by stake size — `test_two_users_different_amounts`
- Same-slot stake+withdraw earns 0 COIN — `test_immediate_withdraw_returns_deposit_zero_rewards`, `test_adversarial_flash_deposit_no_extra_rewards`
- Late depositors share rewards from join time only — `test_staker_joins_later`
- N=0 emits no rewards — `test_n_zero_no_rewards_emitted`
- Claim then unstake does not double-count — `test_adversarial_claim_then_unstake_no_double_rewards`, `test_claim_then_unstake_no_double_rewards`

### Adversarial attacks
- Cannot steal another user's position via wrong SP PDA — `test_adversarial_steal_via_wrong_sp_pda`, `test_unstake_wrong_user_sp_rejected`, `test_no_instruction_to_redirect_user_funds`
- Direct vault transfer cannot drain depositors — `test_adversarial_direct_vault_transfer_no_steal`
- 1-token dilution attack is negligible — `test_adversarial_1_token_dilution_negligible`
- Repeated 1-token withdrawals don't extract more than fair share — `test_adversarial_withdraw_1_repeatedly_no_rounding_exploit`
- Same-slot triple-op (stake+claim+unstake) earns 0 — `test_adversarial_same_slot_triple_op`
- Fake MRC accounts rejected — `test_adversarial_fake_mrc_rejected`
- Wrong stake_vault PDA rejected — `test_unstake_wrong_stake_vault_fails`
- Cross-market COIN inflation rejected — `test_unauthorized_market_cannot_inflate_shared_coin`

### Defense-in-depth (proportional withdrawal)
- Equal positions take equal haircut — `test_proportional_withdrawal_defense_in_depth`
- Unequal positions take equal haircut rate — `test_proportional_withdrawal_unequal_positions_defense_in_depth`
- Partial withdrawal math correct — `test_proportional_partial_withdrawal_defense_in_depth`
- Full drain returns 0 collateral but does not revert — `test_proportional_full_drain_defense_in_depth`

### Init guards
- Slab admin must be burned before reward init — `test_init_market_rewards_live_admin_fails`
- Cannot init twice — `test_init_market_rewards_double_init_fails`, `test_init_coin_config_double_init_fails`
- COIN mint must have no freeze authority — `test_init_coin_config_freeze_authority_fails`
- COIN mint authority must be the program PDA — `test_init_coin_config_wrong_mint_authority_fails`
- COIN mint must be SPL Token-owned — `test_init_coin_config_non_spl_mint_rejected`
- Direct EOA authority rejected — `test_init_coin_config_direct_eoa_authority_rejected`
- Wrong governance authority rejected — `test_init_market_rewards_wrong_authority_fails`

## Instructions

| Tag | Instruction | Description |
|-----|-------------|-------------|
| 0 | `init_market_rewards` | Create per-market reward config + deposit vault (governance-gated) |
| 1 | `stake` | Deposit collateral to vault, begin earning COIN |
| 2 | `unstake` | Withdraw full deposit + claim pending COIN (no lockup) |
| 3 | `init_coin_config` | One-time COIN mint authority setup (governance-gated) |
| 4 | `claim_stake_rewards` | Harvest pending COIN without withdrawing collateral |
| 5 | `draw_insurance` | Governance-gated: withdraw profits from vault (only excess above total_staked) |

## Accounts

| Account | PDA Seeds | Description |
|---------|-----------|-------------|
| CoinConfig | `[b"coin_cfg", coin_mint]` | Governance authority path for this COIN |
| MarketRewardsCfg | `[b"mrc", market_slab]` | Per-market reward parameters and accumulator state |
| StakePosition | `[b"sp", market_slab, user]` | Per-user deposit position (accounting units) |
| Deposit Vault | `[b"stake_vault", market_slab]` | SPL token account holding deposited collateral + profits |
| Mint Authority | `[b"coin_mint_authority", coin_mint]` | PDA that signs COIN mints |

## Building

```bash
cargo build-sbf --manifest-path program/Cargo.toml
```

## Testing

Requires the percolator-prog BPF binary to be built first:

```bash
cd ../percolator-prog && cargo build-sbf
cd ../percolator-meta
cargo build-sbf --manifest-path governance/Cargo.toml
cargo build-sbf --manifest-path program/Cargo.toml
RUST_MIN_STACK=8388608 cargo test --manifest-path program/Cargo.toml --test integration
```

The `RUST_MIN_STACK=8MB` is required due to Percolator's >1MB `RiskEngine` stack size.
