# Payment verification budget, node consensus and the retry-later contract

The issuer answers every activation inside a fixed window instead of waiting for the slowest RPC
node. This document records the exact contract between the issuer, the RPC providers it asks and the
client that pastes a transaction hash.

Source of truth: `crates/licensing-core/src/budget.rs`, `crates/licensing-core/src/consensus.rs`,
`crates/licensing-core/src/rpc.rs`, `license-issuer/src/main.rs` and `license-issuer/src/secret.rs`.

## 1. One window for the whole chain

| Constant | Value | Meaning |
| --- | --- | --- |
| `VERIFICATION_BUDGET_MS` | 1 500 | Hard maximum for one payment verification, across **all** nodes. |
| `MAX_NODE_ATTEMPT_MS` | 700 | Cap for a single node attempt, however much time is left. |
| `MIN_NODE_SLICE_MS` | 150 | Smallest slice handed to a node; below it the node is skipped instead of contacted. |
| `RETRY_AFTER_MS` | 1 000 | Delay advertised to a client whose request reached no decision. |
| `QUOTE_BUDGET_MS` | 4 000 | Window for the pricing path (`/v1/quotes`), which reads four Chainlink rounds. |

Three things enforce the window:

1. **Share of the remainder.** Before each attempt the verifier computes
   `min(remaining / nodes_left, MAX_NODE_ATTEMPT_MS)`. A slow primary node cannot starve the
   fallbacks: they keep a proportional share of what is left.
2. **Per-request deadlines inside an attempt.** One attempt may issue several RPC calls (an Ethereum
   read is chain id + transaction + receipt + finalized block + block). Every request gets
   `deadline - now`, so an attempt cannot stretch past its slice.
3. **A watchdog in the handler.** `license-issuer` wraps the verifier in
   `tokio::time::timeout(budget.total(), …)`. Even if a socket ignored its timeout, the HTTP answer
   leaves after 1 500 ms.

There is no per-attempt-only timeout left: the client-visible bound is the global one.

## 2. Consensus across nodes

Payment facts are agreed, not trusted to one provider. The default configuration asks three
independent providers per chain and needs `CONSENSUS_NODES = 2` agreeing answers:

| Chain | Primary | Mirrors |
| --- | --- | --- |
| Solana | `CHAMPIONVIBE_SOLANA_RPC` (`https://api.mainnet-beta.solana.com`) | `solana-rpc.publicnode.com`, `solana-mainnet.public.blastapi.io` |
| Bitcoin | `CHAMPIONVIBE_BITCOIN_API` (`https://blockstream.info/api`) | `mempool.space/api`, `mempool.emzy.de/api` |
| Ethereum | `CHAMPIONVIBE_ETHEREUM_RPC` (`https://cloudflare-eth.com`) | `ethereum-rpc.publicnode.com`, `eth-mainnet.public.blastapi.io` |

`CHAMPIONVIBE_SOLANA_RPC_NODES`, `CHAMPIONVIBE_BITCOIN_API_NODES` and
`CHAMPIONVIBE_ETHEREUM_RPC_NODES` replace a whole list with a comma-separated value, for example
`CHAMPIONVIBE_ETHEREUM_RPC_NODES=https://my-archive-node.example,https://ethereum-rpc.publicnode.com`.
Self-hosted nodes are therefore first-class; the threshold is always
`min(CONSENSUS_NODES, configured nodes)`, so a deliberately single-node deployment still works.

Two answers agree when

* both report a settled transfer with the same received amount and confirm times within
  `CONFIRMED_AT_TOLERANCE_SECONDS = 2` seconds, or
* both report the same permanent rejection code.

A permanent rejection needs the same agreement: one lying or misbehaving node cannot reject a paying
customer, and one compromised node cannot mint a settlement either.

## 3. Outcomes

| Situation | Result | HTTP |
| --- | --- | --- |
| Consensus on a settled transfer that satisfies the quote | license signed | `200` |
| Consensus on a rejection (`below-minimum`, `wrong-recipient`, `failed-on-chain`, `no-transfer`) | terminal error | `422` |
| Quote window/age violation (`outside-quote-window`, `quote-expired`) | terminal error | `422` |
| Malformed hash or amount (`invalid-transaction-hash`, `invalid-quote`) | terminal error, no network access | `422` |
| Nodes answered, no consensus inside the window | **retry-later** | `503` + `Retry-After: 1` |
| No node produced any fact | **retry-later** (`nodes-unavailable`) | `503` + `Retry-After: 1` |
| Watchdog expired | **retry-later** (`budget-expired`) | `503` + `Retry-After: 1` |

The retry-later body is ordered and deterministic:

```json
{
  "error": "payment verification reached no consensus inside the budget; retry the same request",
  "code": "no-consensus",
  "retryAfterMs": 1000,
  "budgetMs": 1500,
  "elapsedMs": 1491,
  "orderedNodes": [
    { "order": 0, "label": "node-1", "state": "timed-out", "elapsedMs": 500 },
    { "order": 1, "label": "node-2", "state": "pending", "elapsedMs": 240 },
    { "order": 2, "label": "node-3", "state": "skipped", "elapsedMs": 0 }
  ]
}
```

Node labels are opaque (`node-1`…): provider URLs, transport errors and stack details stay in the
issuer log (`verification retry-later: node-1 timed-out …`). A replay of the same activation
transaction returns the identical license instead of a second charge, so retrying is safe.

The client also honours the signal: `src-tauri/src/commands.rs` inspects `Retry-After` on a `503`,
waits at most 2 s, and retries once before surfacing an error to the UI.

## 4. Sealing the signing seed

`ISSUER_SECRET_KEY` normally holds an **encrypted envelope**; a plaintext seed is refused unless the
operator opts in for a loopback development host.

```sh
# 1. seal the seed: the passphrase comes from the environment, the plaintext seed from stdin,
#    so the seed never reaches the process list, the shell history or a config file
export ISSUER_SECRET_KEY_PASSPHRASE='<long random passphrase>'
printf '%s\n' '<32-byte-hex-seed>' | cargo run -p championvibe-issuer -- --seal-secret-key

# 2. run the issuer with the envelope and the passphrase from separate secret stores
ISSUER_SECRET_KEY='enc:v1:pbkdf2-hmac-sha256:600000:<salt>:<nonce>:<ciphertext>' \
ISSUER_SECRET_KEY_PASSPHRASE='<passphrase>' \
  cargo run -p championvibe-issuer
```

Envelope format and parameters:

```
enc:v1:pbkdf2-hmac-sha256:<iterations>:<salt-hex>:<nonce-hex>:<ciphertext-and-tag-hex>
```

* Key derivation: PBKDF2-HMAC-SHA256, 600 000 iterations when sealing; accepted range
  100 000 – 4 000 000, canonical spelling enforced.
* Encryption: ChaCha20-Poly1305 with a fresh 16-byte salt and 12-byte nonce per seal.
* Additional authenticated data: the envelope header, so changing algorithm, iteration count or salt
  makes the envelope fail to open instead of silently weakening it.
* Variables: `ISSUER_SECRET_KEY` (envelope), `ISSUER_SECRET_KEY_FILE` (same value in a file that
  must be mode `0600` on Unix), `ISSUER_SECRET_KEY_PASSPHRASE`, `ISSUER_SECRET_KEY_PASSPHRASE_FILE`,
  and `CHAMPIONVIBE_ALLOW_PLAINTEXT_SECRET=1` for a development host.
* The legacy `CHAMPIONVIBE_SIGNING_KEY_HEX` still works, prints a deprecation notice and needs the
  same plaintext opt-in.
* The seed buffer is wiped after the signer is built; the plaintext seed only exists while sealing.

## 5. What is verified, and what is not

* `cargo test --workspace --locked` covers the budget arithmetic, the consensus tracker, the retry
  signal ordering, envelope sealing/opening/tampering and the rejection codes.
* `crates/licensing-core/tests/verification_budget.rs` runs a three-node chain with real timers and
  asserts that the chain stops inside its window and reports skipped nodes instead of extending it.
* **Not** covered by CI: a live mainnet transfer on each chain. Consensus, confirmation depth and
  finalization rules are exercised against recorded payload shapes, not against a funded wallet.
  Soak testing against a staging treasury before a release remains a manual step.
