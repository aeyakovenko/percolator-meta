# spec.md — MetaDAO Futarchy → Percolator Market Factory + Deterministic Rewards

look at ../percolator-prog for the program source and as the depencency

this is a pure solana rust program only.  it should use the same litesvm setup for testing as ../percolator-prog

## Design constraints (MUST)

1. No admin keys, no multisigs, no off-chain publishers.
1. Everything “governance-like” is futarchy-gated by a MetaDAO proposal marked `executed=true`.
1. User funds are never at risk from futarchy itself: no futarchy-triggerable instruction may transfer, freeze, confiscate, or redirect user balances.
1. The DAO may claim COIN rewards like any user, but cannot claim other users’ balances.

-----

## 1. Programs

|Program                                      |Role                                                                             |
|---------------------------------------------|---------------------------------------------------------------------------------|
|`meta_dao` (existing)                        |Proposal lifecycle, futarchy voting, `executed` bit, contributor escrow, receipts|
|`percolator` (existing + two additions in §2)|Market creation, insurance vault, LP fee accounting                              |
|`rewards` (new, ~200 lines, non-upgradeable) |COIN mint-authority PDA, owner-reward claims, LP-reward claims                   |
|SPL Token Program                            |COIN mint and token accounts                                                     |

There is no separate rewards-oracle, rewards-admin, or multisig program. The `rewards` program holds only a COIN mint-authority PDA derived from the market slab key; it has no privileged signer of its own.

-----

## 2. Required additions to Percolator

The existing Percolator instruction set is used as-is except for the two additions below. Neither touches any existing instruction, account layout outside `_reserved`, or security invariant.

### 2.1 Store `market_start_slot` at InitMarket time

`SlabHeader._reserved[8..16]` is currently zero-initialized and unused at market creation. At the end of the `InitMarket` handler, write the current slot into that field:

```rust
state::write_market_start_slot(data, clock.slot);   // new write in InitMarket
state::read_market_start_slot(data) -> u64;          // new public reader
```

This single u64 is the only anchor the `rewards` program needs to compute elapsed epochs. It is written once and never mutated. The `rewards` program reads it via the slab account data; it does not accept a caller-supplied start slot.

### 2.2 New `QueryLpFees` instruction (tag = 24)

```
Instruction::QueryLpFees { lp_idx: u16 }
```

Checks: slab initialized, `lp_idx` is a valid LP account (`check_idx`).

Effect: Writes `fees_earned_total: u128` (16 bytes, little-endian) to Solana return-data via `set_return_data`. No state is mutated. No signer is required.

The `rewards` program CPIs this instruction to obtain the monotonically non-decreasing cumulative fee total for an LP position. Exposing it as a versioned instruction — rather than requiring callers to parse internal slab offsets — makes the interface stable against future `RiskEngine` layout changes.

-----

## 3. Assets

**SOL** — contributed by participants into a MetaDAO escrow, then seeded into the Percolator insurance vault at market creation.

**COIN** — SPL token minted exclusively by the `rewards` program.

COIN mint requirements:

- `mint_authority = PDA(rewards, [b"coin_mint_authority", coin_mint_key])`
- `freeze_authority = None`
- Decimals are fixed at creation and committed in the proposal hash.
- A single COIN mint is shared across all markets managed by the same DAO. The `CoinConfig` PDA (§10) gates which authority can register new markets.

-----

## 4. Epoch

```
EPOCH_SLOTS: u64          // constant compiled into the `rewards` program
epoch(slot) = slot / EPOCH_SLOTS
```

-----

## 5. MetaDAO proposal payload

A “Create Percolator Market” proposal commits to `market_config_hash = sha256(payload)` where `payload` is the canonical serialization of:

- Full Percolator `MarketConfig` (passed verbatim to `InitMarket`)
- `seed_sol_target: u64`
- `N: u64` — COIN emitted per epoch to all insurance contributors collectively
- `K: u128` — COIN per fee-atom in fixed-point (`FP = 2^64`); see §8.2
- `EPOCH_SLOTS` — must match the `rewards` program constant
- COIN mint address and decimals
- Rounding rule: integer truncation with sub-coin remainder carried forward (§8)

-----

## 6. Escrow safety (MetaDAO)

Each proposal has a SOL escrow PDA:

```
seed_escrow = PDA(meta_dao, [b"escrow", proposal_key])
```

Exactly two outcomes are permitted:

- **Fail or cancel** — each contributor withdraws their own lamports via a signed receipt instruction. No other account can receive funds.
- **Execute** — the entire escrow balance is transferred to the Percolator `insurance_vault` in the same atomic transaction as `InitMarket` (§7). No partial transfers, no intermediaries.

-----

## 7. Market creation (one atomic transaction, permissionless caller)

The caller assembles the following instructions in a single transaction after `proposal.executed == true`:

```
[0]  meta_dao::execute_and_create_market(proposal, market_config, N, K, coin_mint, coin_decimals)
       — verifies proposal.executed == true
       — verifies sha256(market_config, N, K, coin_mint, coin_decimals, EPOCH_SLOTS)
                 == proposal.market_config_hash
       — verifies seed_escrow.lamports == proposal.seed_sol_raised

[1]  percolator::InitMarket(
           admin           = seed_escrow_pda,    // temporary; burned in [3]
           collateral_mint = ...,
           ... market_config fields ...
     )
     // §2.1: percolator writes market_start_slot = clock.slot into slab

[2]  percolator::TopUpInsurance(amount = proposal.seed_sol_raised)
     // lamports: seed_escrow_pda → percolator insurance_vault

[3]  percolator::UpdateAdmin(new_admin = Pubkey::default())
     // signer: seed_escrow_pda (CPI with PDA seeds)
     // admin is now [0u8;32]; percolator::require_admin permanently rejects all-zeros
     // all admin instructions on this market are disabled forever

[4]  rewards::init_market_rewards(
           market_slab, N, K, coin_mint,
           total_contributed_lamports = proposal.seed_sol_raised
     )
     // signer: CoinConfig.authority (the DAO)
     // creates MarketRewardsCfg PDA (init guard — fails if already exists)
     // reads and stores market_start_slot from slab (never trusts caller-supplied value)
     // receipt_program is copied from CoinConfig
```

After instruction [3], Percolator’s `require_admin` check (`admin != [0u8;32]`) permanently rejects every admin instruction on this slab. No future governance call, upgrade, or multisig can invoke `UpdateConfig`, `SetRiskThreshold`, `ResolveMarket`, `WithdrawInsurance`, or any other admin-gated instruction on this market.

-----

## 8. Rewards math

### 8.1 Fixed-point scale

```
FP = 2^64
```

### 8.2 Owner rewards — receipt-proportional, no accumulator

Because insurance contributions are one-time and fixed at market creation, the standard LP-share accumulator is unnecessary. Each contributor’s entitlement is fully determined by immutable quantities:

```
epochs_elapsed  = epoch(current_slot) - epoch(market_start_slot)
total_entitled  = N × epochs_elapsed × receipt.contributed_lamports
                  / total_contributed_lamports          // integer division, truncates
claimable       = total_entitled - claim_state.coin_claimed
```

All inputs are on-chain with no Percolator changes beyond §2.1:

- `N` and `total_contributed_lamports` — stored in `MarketRewardsCfg`
- `market_start_slot` — read from slab via `read_market_start_slot`
- `receipt.contributed_lamports` — stored in MetaDAO receipt account
- `claim_state.coin_claimed` — stored in `OwnerClaimState` PDA

Rounding: integer truncation means a contributor’s share accumulates one COIN at a time. The fractional remainder is never lost; it becomes claimable in a subsequent epoch once `total_entitled` crosses the next integer.

**Instruction:** `rewards::claim_owner_rewards(market_slab, proposal, receipt, claim_state, coin_ata)`

- Signer must be `receipt.contributor`.
- Mints `claimable` COIN to `coin_ata`.
- Sets `claim_state.coin_claimed += claimable`.
- No Percolator CPI required.

### 8.3 LP rewards — fee-multiple via QueryLpFees

Per market, immutable:

- `K = lp_coin_per_fee_fp: u128` (FP), hard-capped by `MAX_LP_COIN_PER_FEE_FP`.

Claim math:

```
fees_earned_total  = QueryLpFees(market_slab, lp_idx)   // §2.2 CPI, read-only

entitled_fp        = (u256) fees_earned_total × K
claimable_fp       = entitled_fp - lp_claim_state.reward_claimed_fp
claimable_coins    = claimable_fp / FP
lp_claim_state.reward_claimed_fp += (u256) claimable_coins × FP
// sub-coin remainder stays claimable in future calls; nothing is dropped
```

**Instruction:** `rewards::claim_lp_rewards(market_slab, lp_idx, lp_claim_state, coin_ata)`

- Signer must be the LP position’s registered owner (read from slab data).
- CPIs `percolator::QueryLpFees` to read `fees_earned_total`.
- Mints `claimable_coins` COIN to `coin_ata`.
- `lp_claim_state.reward_claimed_fp` is monotonically non-decreasing; claim can never exceed entitlement.

-----

## 9. Contributor seed share redemption

The seed SOL lives in the Percolator `insurance_vault` and is subject to the risk engine’s normal loss-socialization rules. Contributors hold a proportional implicit claim on vault residual:

```
contributor_i share = receipt_i.contributed_lamports / total_contributed_lamports
```

Because the admin is burned in §7 step [3], `ResolveMarket` and `WithdrawInsurance` are permanently inaccessible on this market. If limited insurance withdrawal is desired, a `SetInsuranceWithdrawPolicy` authority must be configured **before** step [3] in the creation transaction. This is an optional extension; the core COIN reward flow does not require it.

-----

## 10. `rewards` program accounts

### CoinConfig

PDA seeds: `[b"coin_cfg", coin_mint_key]`

Created once per COIN token via `init_coin_config`. The authority (typically the MetaDAO program) is the only key that can register new markets for this COIN.

|Field             |Type  |Description                                     |
|------------------|------|-------------------------------------------------|
|`authority`       |Pubkey|Who can call `init_market_rewards` for this COIN |
|`receipt_program` |Pubkey|MetaDAO program that owns receipt accounts       |

### MarketRewardsCfg

PDA seeds: `[b"mrc", market_slab_key]`

|Field                       |Type  |Description                                   |
|----------------------------|------|----------------------------------------------|
|`market_slab`               |Pubkey|Percolator slab account                       |
|`coin_mint`                 |Pubkey|COIN mint address                             |
|`receipt_program`           |Pubkey|Copied from CoinConfig at init time           |
|`N`                         |u64   |Owner emission per epoch (COIN)               |
|`K`                         |u128  |LP COIN per fee-atom (FP)                     |
|`market_start_slot`         |u64   |Read from slab at init; immutable             |
|`total_contributed_lamports`|u64   |From proposal; immutable                      |

### OwnerClaimState

PDA seeds: `[b"ocs", market_slab_key, receipt_key]`

|Field         |Type|Description                               |
|--------------|----|------------------------------------------|
|`coin_claimed`|u64 |Cumulative COIN minted to this contributor|

### LpClaimState

PDA seeds: `[b"lcs", market_slab_key, lp_idx_le_bytes]`

|Field              |Type|Description                                       |
|-------------------|----|--------------------------------------------------|
|`reward_claimed_fp`|u256|Cumulative fixed-point entitlement already claimed|

-----

## 11. Forbidden capabilities (MUST NOT exist)

No instruction in MetaDAO, Percolator, or `rewards` may:

- Transfer tokens from arbitrary user accounts.
- Withdraw SOL from the Percolator `insurance_vault` except via Percolator’s existing, non-governance risk-engine rules.
- Freeze user token accounts.
- Set or change the COIN mint freeze authority (must remain `None`).
- Modify any reward parameter (`N`, `K`, `coin_mint`, `EPOCH_SLOTS`) after `init_market_rewards` is called.
- Invoke any Percolator admin instruction on a market whose admin has been burned.

-----

## 12. Explicit assumptions

- `SlabHeader._reserved[8..16]` is currently zero-initialized and not written by any Percolator instruction at market-creation time. For markets created through this flow, `write_market_start_slot` takes ownership of those bytes at `InitMarket`. Existing markets have `0` there and are incompatible with this spec.
- `fees_earned_total` inside the `RiskEngine` is monotonically non-decreasing. The `rewards` program assumes fees are never credited negatively.
- LP wash-trading to inflate `fees_earned_total` is an economic risk, not a custody risk. It is bounded by `MAX_LP_COIN_PER_FEE_FP` and Percolator’s `trading_fee_bps`.
- The `rewards` program is deployed non-upgradeable. The COIN mint `freeze_authority` is `None` at creation and cannot be set afterward.
- `QueryLpFees` (§2.2) performs no state mutation and cannot be used to drain funds or bypass engine authorization.

-----

## 13. Audit checklist

- [ ] `InitMarket` sets `admin = seed_escrow_pda`; `UpdateAdmin(zeros)` is called in the same transaction before any other instruction can exercise that admin authority.
- [ ] `seed_escrow` lamports flow only to `insurance_vault` (on execute) or back to contributors (on cancel/fail). No other disbursement path exists.
- [ ] `rewards::init_market_rewards` creates `MarketRewardsCfg` with an init guard; a second call on the same slab fails.
- [ ] `market_start_slot` is read from the slab by the `rewards` program; it is not accepted as an instruction argument.
- [ ] Owner reward claim is bounded: for all contributors combined, total claimable ≤ `N × epochs_elapsed`. No single contributor can claim more than their `contributed_lamports / total_contributed_lamports` fraction.
- [ ] LP reward claim satisfies `reward_claimed_fp ≤ fees_earned_total × K` at all times; sub-coin remainder accumulates and is never dropped.
- [ ] `QueryLpFees` mutates no state; `check_idx` is the only guard required.
- [ ] COIN mint `freeze_authority = None`; `rewards` program is non-upgradeable at deploy.
- [ ] After admin burn: `UpdateConfig`, `SetRiskThreshold`, `SetOracleAuthority`, `ResolveMarket`, `WithdrawInsurance`, `SetMaintenanceFee`, `AdminForceCloseAccount`, and `CloseSlab` all fail on this market.
