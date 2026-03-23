# Percolator Rewards Program

Solana program that adds staking vault rewards to [Percolator](https://github.com/aeyakovenko/percolator-prog) markets, governed by MetaDAO futarchy.

## Overview

- **Staking vault**: Users deposit collateral into a per-market vault to earn COIN rewards over time (Synthetix-style accumulator)
- **Governance-gated minting**: The DAO can vote to mint COIN to any destination (e.g., rewarding best-performing LPs identified off-chain)
- **COIN token**: Minted exclusively by this program via a PDA mint authority. Shared across all markets managed by the same DAO
- **No admin keys**: All governance actions are futarchy-gated. Market admin is burned at creation. The program is deployed non-upgradeable
- **Trusted bootstrap**: The governing authority path for an instance is established by the DAO-controlled client at init time and then reused for all governed calls

## Design Constraints

1. No admin keys, no multisigs, no off-chain publishers
2. Everything governance-like is expected to come from a MetaDAO proposal marked `executed=true`
3. User funds are never at risk from futarchy: no futarchy-triggerable instruction may transfer, freeze, confiscate, or redirect user balances
4. The DAO may stake and claim COIN rewards like any user, but cannot claim other users' staked collateral
5. The current implementation treats the MetaDAO binding as a deployment/init assumption: the DAO-controlled client must bootstrap the governing PDA path for the intended instance at creation time

## Trusted Init Ceremony

The current code does not independently prove MetaDAO proposal execution inside `rewards`. Instead, the trust boundary is established during instance creation:

1. The DAO-controlled client creates the governed authority path for the intended `(rewards_program, coin_mint)` pair.
2. `init_coin_config` is called through that same governed path.
3. Percolator market creation burns admin before `init_market_rewards`.
4. All future governed calls (`init_market_rewards`, `mint_reward`) continue to flow through the same preconfigured authority path.

If that bootstrap is not performed by the intended DAO-controlled flow, the repo's governance assumptions do not hold.

## Instructions

| Tag | Instruction | Description |
|-----|-------------|-------------|
| 0 | `init_market_rewards` | Create per-market reward config + staking vault (requires the preconfigured CoinConfig authority path) |
| 1 | `stake` | Deposit collateral to vault, earn COIN over time |
| 2 | `unstake` | Withdraw collateral + claim pending COIN (lockup enforced) |
| 3 | `init_coin_config` | One-time setup of COIN mint authority config via the governed init path |
| 4 | `claim_stake_rewards` | Harvest pending COIN without unstaking (no lockup check) |
| 5 | `mint_reward` | Governance-gated: mint COIN to any destination (requires the preconfigured CoinConfig authority path) |

## Accounts

| Account | PDA Seeds | Description |
|---------|-----------|-------------|
| CoinConfig | `[b"coin_cfg", coin_mint]` | Stores the preconfigured governance authority path for this COIN |
| MarketRewardsCfg | `[b"mrc", market_slab]` | Per-market reward parameters and accumulator state |
| StakePosition | `[b"sp", market_slab, user]` | Per-user staking position |
| Stake Vault | `[b"stake_vault", market_slab]` | SPL token account holding staked collateral |
| Mint Authority | `[b"coin_mint_authority", coin_mint]` | PDA that signs COIN mints |

## Dependencies

- [percolator-prog](https://github.com/aeyakovenko/percolator-prog) — Percolator Solana program (provides `state::read_market_start_slot`)

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

## Spec

See [spec.md](spec.md) for the full design specification, reward math, market creation flow, and audit checklist.
