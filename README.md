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
