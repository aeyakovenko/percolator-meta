# Percolator Rewards Program

Solana program that adds staking vault rewards and LP fee rewards to [Percolator](https://github.com/aeyakovenko/percolator-prog) markets, governed by MetaDAO futarchy.

## Overview

- **Staking vault**: Users deposit collateral into a per-market vault to earn COIN rewards over time (Synthetix-style accumulator)
- **LP rewards**: Liquidity providers earn COIN proportional to fees earned, via CPI to Percolator's `QueryLpFees`
- **COIN token**: Minted exclusively by this program via a PDA mint authority. Shared across all markets managed by the same DAO
- **No admin keys**: All governance actions are futarchy-gated. Market admin is burned at creation. The program is deployed non-upgradeable

## Design Constraints

1. No admin keys, no multisigs, no off-chain publishers
2. Everything governance-like is futarchy-gated by a MetaDAO proposal marked `executed=true`
3. User funds are never at risk from futarchy: no futarchy-triggerable instruction may transfer, freeze, confiscate, or redirect user balances
4. The DAO may stake and claim COIN rewards like any user, but cannot claim other users' staked collateral

## Instructions

| Tag | Instruction | Description |
|-----|-------------|-------------|
| 0 | `init_market_rewards` | Create per-market reward config + staking vault (requires CoinConfig authority) |
| 1 | `stake` | Deposit collateral to vault, earn COIN over time |
| 2 | `unstake` | Withdraw collateral + claim pending COIN (lockup enforced) |
| 3 | `init_coin_config` | One-time setup of COIN mint authority config |
| 4 | `claim_stake_rewards` | Harvest pending COIN without unstaking (no lockup check) |
| 5 | `claim_lp_rewards` | Claim COIN for LP fee earnings via CPI |

## Accounts

| Account | PDA Seeds | Description |
|---------|-----------|-------------|
| CoinConfig | `[b"coin_cfg", coin_mint]` | Authority that can register new markets for a COIN |
| MarketRewardsCfg | `[b"mrc", market_slab]` | Per-market reward parameters and accumulator state |
| StakePosition | `[b"sp", market_slab, user]` | Per-user staking position |
| LpClaimState | `[b"lcs", market_slab, lp_idx]` | Per-LP cumulative claim tracking |
| Stake Vault | `[b"stake_vault", market_slab]` | SPL token account holding staked collateral |
| Mint Authority | `[b"coin_mint_authority", coin_mint]` | PDA that signs COIN mints |

## Dependencies

- [percolator-prog](https://github.com/aeyakovenko/percolator-prog) — Percolator Solana program (provides `state::read_market_start_slot`, `QueryLpFees`)
- [percolator](https://github.com/aeyakovenko/percolator) — Core library (account layout, `RiskEngine` constants)

## Building

```bash
cargo build-sbf --manifest-path program/Cargo.toml
```

## Testing

Requires the percolator-prog BPF binary to be built first:

```bash
cd ../percolator-prog && cargo build-sbf
cd ../percolator-meta
RUST_MIN_STACK=8388608 cargo test --test integration
```

The `RUST_MIN_STACK=8MB` is required due to Percolator's >1MB `RiskEngine` stack size.

## Spec

See [spec.md](spec.md) for the full design specification, reward math, market creation flow, and audit checklist.
