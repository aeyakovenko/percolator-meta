# spec.md — MetaDAO Futarchy → Percolator Market Factory + Staking Vault Rewards

look at ../percolator-prog for the program source and as the depencency

this is a pure solana rust program only.  it should use the same litesvm setup for testing as ../percolator-prog

## Design constraints (MUST)

1. No admin keys, no multisigs, no off-chain publishers.
1. Everything "governance-like" is expected to be triggered from a MetaDAO proposal marked `executed=true`.
1. User funds are never at risk from futarchy itself: no futarchy-triggerable instruction may transfer, freeze, confiscate, or redirect user balances.
1. The DAO may stake and claim COIN rewards like any user, but cannot claim other users' staked collateral.
1. Current implementation assumption: the DAO-controlled client bootstraps the governed authority path for this rewards instance at creation time. `rewards` does not independently prove MetaDAO execution beyond that configured path.

-----

## 1. Programs

|Program                                      |Role                                                                             |
|---------------------------------------------|---------------------------------------------------------------------------------|
|`meta_dao` (existing)                        |Proposal lifecycle, futarchy voting, `executed` bit                              |
|`governance_adapter` (current implementation)|Owns the governance authority PDA and CPIs into `rewards`; bootstrap/signing shim |
|`percolator` (existing + one addition in §2) |Market creation, insurance vault                                                |
|`rewards` (new, non-upgradeable)             |COIN mint-authority PDA, staking vault, stake/unstake, governance-gated minting  |
|SPL Token Program                            |COIN mint, collateral token accounts, staking vault                              |

Current implementation note: `rewards` accepts a governance PDA owned by `governance_adapter`, and that adapter is expected to be initialized only from the intended MetaDAO-controlled creation flow. The adapter is a signing shim, not a policy engine. The trust boundary is therefore established during the init ceremony rather than re-proven inside `rewards`.

-----

## 2. Required additions to Percolator

The existing Percolator instruction set is used as-is except for the addition below. It does not touch any existing instruction, account layout outside `_reserved`, or security invariant.

### 2.1 Store `market_start_slot` at InitMarket time

`SlabHeader._reserved[8..16]` is currently zero-initialized and unused at market creation. At the end of the `InitMarket` handler, write the current slot into that field:

```rust
state::write_market_start_slot(data, clock.slot);   // new write in InitMarket
state::read_market_start_slot(data) -> u64;          // new public reader
```

This single u64 is the only anchor the `rewards` program needs to compute elapsed time. It is written once and never mutated. The `rewards` program reads it via the slab account data; it does not accept a caller-supplied start slot.

-----

## 3. Assets

**Collateral** — SPL token deposited by stakers into a per-market staking vault. Each market has its own collateral mint and isolated vault.

**COIN** — SPL token minted exclusively by the `rewards` program. Shared across all markets managed by the same DAO.

COIN mint requirements:

- `mint_authority = PDA(rewards, [b"coin_mint_authority", coin_mint_key])`
- `freeze_authority = None`
- Decimals are fixed at creation and committed in the proposal hash.
- A single COIN mint is shared across all markets. The `CoinConfig` PDA (§10) gates which authority can register new markets.

-----

## 4. Epoch

```
epoch_slots: u64          // per-market, set at init_market_rewards; immutable
```

`epoch_slots` defines the minimum lockup period for stakers and the rate denominator for reward calculation. It is stored in `MarketRewardsCfg` and can differ per market (futarchy votes on it).

-----

## 5. MetaDAO proposal payload

A "Create Percolator Market" proposal commits to `market_config_hash = sha256(payload)` where `payload` is the canonical serialization of:

- Full Percolator `MarketConfig` (passed verbatim to `InitMarket`)
- `N: u64` — COIN emitted per epoch to stakers collectively
- `epoch_slots: u64` — minimum lockup / reward period
- COIN mint address and decimals
- Collateral mint address
- Rounding rule: integer truncation with sub-coin remainder carried forward (§8)

-----

## 6. Staking vault

Each market has a per-market staking vault:

```
stake_vault = PDA(rewards, [b"stake_vault", market_slab_key])
```

The vault is an SPL token account whose authority is the MRC PDA. Users deposit collateral to earn COIN rewards. Each market is isolated: separate collateral, separate vault, separate reward rate.

-----

## 7. Market creation (one atomic transaction, permissionless caller)

Bootstrap prerequisite: before the first governed call for a `(rewards_program, coin_mint)` pair, the DAO-controlled client initializes the governance authority path for that pair. The current repo assumes this binding is established during instance creation and then reused for all governed CPIs.

The caller assembles the following instructions in a single transaction after `proposal.executed == true`:

```
[0]  meta_dao::execute_and_create_market(proposal, market_config, N, epoch_slots, coin_mint)
       — verifies proposal.executed == true
       — verifies sha256(market_config, N, epoch_slots, coin_mint, collateral_mint)
                 == proposal.market_config_hash

[1]  percolator::InitMarket(
           admin           = seed_escrow_pda,    // temporary; burned in [3]
           collateral_mint = ...,
           ... market_config fields ...
     )
     // §2.1: percolator writes market_start_slot = clock.slot into slab

[2]  percolator::TopUpInsurance(amount = proposal.seed_sol_raised)
     // lamports: seed_escrow_pda → percolator insurance_vault

[3]  percolator::UpdateAdmin(new_admin = Pubkey::default())
     // admin is now [0u8;32]; percolator::require_admin permanently rejects all-zeros
     // all admin instructions on this market are disabled forever

[4]  rewards::init_market_rewards(
           market_slab, N, epoch_slots, coin_mint, collateral_mint
     )
     // signer: CoinConfig.authority (the preconfigured governance authority path)
     // creates MarketRewardsCfg PDA (init guard — fails if already exists)
     // creates stake_vault SPL token account PDA
     // reads and stores market_start_slot from slab (never trusts caller-supplied value)
```

After instruction [3], Percolator's `require_admin` check (`admin != [0u8;32]`) permanently rejects every admin instruction on this slab.

-----

## 8. Rewards math

### 8.1 Fixed-point scale

```
FP = 2^64
```

### 8.2 Staker rewards — Synthetix-style accumulator

Per market, `MarketRewardsCfg` maintains:

- `reward_per_token_stored: u128` — global accumulator (FP-scaled)
- `last_update_slot: u64`
- `total_staked: u64`

On every stake/unstake/claim, the accumulator is updated:

```
elapsed = current_slot - last_update_slot
if total_staked > 0 and elapsed > 0:
    delta = N * elapsed * FP / (epoch_slots * total_staked)   // u256 intermediate
    reward_per_token_stored += delta
last_update_slot = current_slot
```

Per user, `StakePosition` maintains:

- `amount: u64`
- `deposit_slot: u64`
- `reward_per_token_paid: u128`
- `pending_rewards: u64`

On settle (before any stake/unstake):

```
delta = reward_per_token_stored - reward_per_token_paid
earned = amount * delta / FP
pending_rewards += earned
reward_per_token_paid = reward_per_token_stored
```

**Instruction:** `rewards::stake(amount)` — deposits collateral to vault, creates/updates position, resets lockup.

**Instruction:** `rewards::unstake(amount)` — requires `current_slot >= deposit_slot + epoch_slots`, transfers collateral back, mints pending COIN, closes position on full unstake.

**Instruction:** `rewards::claim_stake_rewards()` — mints pending COIN without unstaking. No lockup check required.

### 8.3 Governance-gated minting (`mint_reward`)

The DAO can vote to mint COIN to any destination (e.g., rewarding best-performing LPs identified off-chain).

**Instruction:** `rewards::mint_reward(amount)`

- Signer must be `CoinConfig.authority` (the preconfigured governance authority path for this COIN).
- Mints `amount` COIN to any provided SPL token destination account.
- Amount must be non-zero.

This replaces on-chain LP fee tracking. LP performance identification is an off-chain process; the DAO votes to reward whichever LPs perform best.

-----

## 9. Staking lockup and withdrawal

Stakers must hold collateral for at least `epoch_slots` after their last deposit before unstaking. Each new stake resets the lockup timer (`deposit_slot = current_slot`).

Claiming COIN rewards (`claim_stake_rewards`) does NOT require lockup to elapse — stakers can harvest COIN at any time.

On full unstake (amount == staked balance), the StakePosition PDA is closed and rent is returned to the user.

-----

## 10. `rewards` program accounts

### CoinConfig

PDA seeds: `[b"coin_cfg", coin_mint_key]`

Created once per COIN token via `init_coin_config`. The authority is the preconfigured governance PDA path established by the DAO-controlled init ceremony for this COIN. It is the only key that can register new markets for this COIN and call `mint_reward`.

|Field             |Type  |Description                                     |
|------------------|------|-------------------------------------------------|
|`authority`       |Pubkey|Who can call `init_market_rewards` and `mint_reward` for this COIN |

### MarketRewardsCfg

PDA seeds: `[b"mrc", market_slab_key]`

|Field                       |Type  |Description                                          |
|----------------------------|------|-----------------------------------------------------|
|`market_slab`               |Pubkey|Percolator slab account                              |
|`coin_mint`                 |Pubkey|COIN mint address                                    |
|`collateral_mint`           |Pubkey|Collateral token for this market's staking vault     |
|`n_per_epoch`               |u64   |COIN emitted per epoch to stakers                    |
|`epoch_slots`               |u64   |Minimum lockup / reward period (slots)               |
|`market_start_slot`         |u64   |Read from slab at init; immutable                    |
|`reward_per_token_stored`   |u128  |Synthetix-style accumulator (FP-scaled)              |
|`last_update_slot`          |u64   |Last slot accumulator was updated                    |
|`total_staked`              |u64   |Total collateral currently staked                    |

### StakePosition

PDA seeds: `[b"sp", market_slab_key, user_pubkey]`

|Field                    |Type |Description                                       |
|-------------------------|-----|--------------------------------------------------|
|`amount`                 |u64  |Collateral currently staked                       |
|`deposit_slot`           |u64  |Slot of last deposit (lockup reference)           |
|`reward_per_token_paid`  |u128 |Accumulator snapshot at last settle               |
|`pending_rewards`        |u64  |Unsettled COIN rewards                            |

-----

## 11. Forbidden capabilities (MUST NOT exist)

No instruction in MetaDAO, Percolator, or `rewards` may:

- Transfer tokens from arbitrary user accounts.
- Withdraw collateral from the staking vault except to the user who staked it (via `unstake`).
- Withdraw SOL from the Percolator `insurance_vault` except via Percolator's existing, non-governance risk-engine rules.
- Freeze user token accounts.
- Set or change the COIN mint freeze authority (must remain `None`).
- Modify any reward parameter (`N`, `epoch_slots`, `coin_mint`) after `init_market_rewards` is called.
- Invoke any Percolator admin instruction on a market whose admin has been burned.

-----

## 12. Explicit assumptions

- `SlabHeader._reserved[8..16]` is currently zero-initialized and not written by any Percolator instruction at market-creation time. For markets created through this flow, `write_market_start_slot` takes ownership of those bytes at `InitMarket`. Existing markets have `0` there and are incompatible with this spec.
- The `rewards` program is deployed non-upgradeable. The COIN mint `freeze_authority` is `None` at creation and cannot be set afterward.
- Integer truncation in the Synthetix accumulator may cause up to 1 COIN per claim to be deferred. The sub-coin remainder is never lost; it becomes claimable as the accumulator advances.

-----

## 13. Audit checklist

- [ ] `InitMarket` sets `admin = seed_escrow_pda`; `UpdateAdmin(zeros)` is called in the same transaction before any other instruction can exercise that admin authority.
- [ ] `rewards::init_market_rewards` creates `MarketRewardsCfg` with an init guard; a second call on the same slab fails.
- [ ] `market_start_slot` is read from the slab by the `rewards` program; it is not accepted as an instruction argument.
- [ ] Staker reward accumulator update is serialized to MRC before any CPI, preventing double-accumulation.
- [ ] Staker collateral can only be withdrawn by the depositor, after lockup elapses, to their own token account.
- [ ] For all stakers combined: total claimable per slot ≤ `N / epoch_slots`. No single staker can claim more than their `amount / total_staked` fraction.
- [ ] `mint_reward` requires `CoinConfig.authority` as signer; unauthorized callers are rejected.
- [ ] COIN mint `freeze_authority = None`; `rewards` program is non-upgradeable at deploy.
- [ ] After admin burn: `UpdateConfig`, `SetRiskThreshold`, `SetOracleAuthority`, `ResolveMarket`, `WithdrawInsurance`, `SetMaintenanceFee`, `AdminForceCloseAccount`, and `CloseSlab` all fail on this market.
- [ ] `CoinConfig.authority` is the only key that can register new markets for a given COIN; unauthorized callers are rejected.
- [ ] Stake vault PDA authority is the MRC PDA; only `unstake` can transfer collateral out.
- [ ] Deployment/init docs specify how the DAO-controlled client bootstraps the governance authority path for this rewards instance.
