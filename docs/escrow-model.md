# Escrow Evidence Anchoring Model

This document specifies the cryptographic evidence anchoring model for the Stellar GreenPay Escrow Contract.

---

## 🎯 Background & Motivation

In traditional decentralized escrow models, funds can often be released unilaterally by an admin or client without anchoring verifiable proof that the agreed project milestones or environmental deliverables were actually completed. This creates a single point of failure and centralizes trust in contract administrators.

To guarantee verifiable accountability on the Stellar blockchain, the GreenPay Escrow Contract implements **Evidence Anchoring** for `release_funds()`:
Each call to this operation requires the caller to provide a cryptographic SHA-256 digest of the project completion report or audit evidence (`project_report_hash`). Other payout paths, such as milestone releases, do not currently accept or emit this evidence hash.

---

## 🔐 Evidence Anchoring Mechanism

```
   Project Deliverable / Report
                 │
                 ▼
         SHA-256 Hashing
                 │
                 ▼
     project_report_hash (BytesN<32>)
                 │
                 ▼
    EscrowContract::release_funds()
                 │
                 ├─► Transfers escrowed tokens to Freelancer / Recipient
                 │
                 └─► Emits On-Chain Event:
                     FundsReleased {
                         project_id,
                         report_hash,
                         amount
                     }
```

### 1. Cryptographic Report Hash (`project_report_hash`)
- **Type**: `BytesN<32>` (32-byte array representing SHA-256 hash).
- **Format**: Pre-image is the canonical byte stream of the verified project completion report (e.g., photo evidence, satellite validation, or audit certification stored on IPFS or distributed storage).
- **Immutability**: Once recorded on-chain in the `FundsReleased` event, anyone can independently verify that the off-chain report matches the anchored hash.

### 2. Event Emission (`FundsReleased`)
When `release_funds` is successfully executed, the contract emits a topic-indexed event:

```rust
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct FundsReleased {
    pub project_id: String,
    pub report_hash: BytesN<32>,
    pub amount: i128,
}
```

- **Topic**: `(symbol_short!("funds_rel"), job_id)`
- **Payload**: `FundsReleased` struct containing `project_id`, `report_hash`, and released `amount`.

---

## ⚙️ Contract Method Specification

```rust
pub fn release_funds(
    env: Env,
    caller: Address,
    job_id: String,
    project_report_hash: BytesN<32>,
) -> i128
```

### Parameters
- `caller`: Address of the admin or client executing the release. Must authenticate via `caller.require_auth()`.
- `job_id`: Unique identifier of the escrow job.
- `project_report_hash`: 32-byte SHA-256 hash of the milestone completion or final report.

### Invariants Enforced
1. **Authorization**: Caller must be either the designated contract admin or the job's client.
2. **Dispute Guard**: If `job.disputed == true`, release is blocked until disputes are formally resolved.
3. **Milestone Accounting**: Releases all unreleased milestones, marks milestones as released, and transitions job status to `JobStatus::Completed`.
4. **Deterministic Allocation**: Milestone payouts are differences between cumulative percentage totals, so rounding is stable regardless of release order and all milestone allocations sum to the original escrow amount. Arithmetic avoids multiplying the full escrow amount by a percentage.
5. **Atomic Transfer and Recovery**: A failed token transfer reverts the entire Soroban invocation, including milestone/status writes, so the same operation can be retried after the token issue is resolved. Dispute settlement marks every remaining milestone settled in the same atomic invocation as its transfer/refund.
6. **Idempotency Guard**: Rejects release when every milestone has already been settled. A remaining milestone with a zero-value allocation can still be marked settled without making a zero-value token call.
7. **Audit Trail**: Publishes the `FundsReleased` event anchoring the hash.

### Failure Recovery & Retry Semantics

The escrow contract is intentionally robust against transient token or ledger failures:

- `release_milestone()` and `claim_milestone()` transfer funds only after all validation checks. If the token transfer fails, the entire Soroban invocation reverts, leaving the milestone state unchanged and making the operation safe to retry.
- `resolve_dispute()` settles every unpaid milestone and performs the token transfer in the same atomic transaction, so the contract never transitions to a partially settled dispute state.
- `release_funds()` rejects an already-completed job by checking whether any unreleased milestones remain; this prevents duplicate settlement while allowing zero-value remainder milestones to be closed without sending a zero-value transfer.
- Because the contract stores the milestone structure and status updates in the same ledger transaction as the transfer, external observers never see a half-applied milestone release.

---

## 🔍 Verification by Donors & Auditors

Donors and external auditors can verify any fund release on Stellar Horizon or Soroban RPC:
1. Fetch the transaction events matching topic `funds_rel`.
2. Extract `report_hash` and `amount`.
3. Download the evidence report from IPFS or public storage.
4. Compute `SHA-256(report_data)` and confirm it matches `report_hash` byte-for-byte.
