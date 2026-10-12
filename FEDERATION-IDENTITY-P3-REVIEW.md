# P3 node identity handshake — implementation review

This is a source review and reproducible regression contract, **not an independent
cryptographic audit**. The remaining release/platform gates live in
[FEDERATION-IDENTITY-FINISH-GATE.md](FEDERATION-IDENTITY-FINISH-GATE.md).

## Trust and state, from first principles

The primary authorizes an exact registered `(name, public identity)` pair.
The secondary trusts only the operator's independently supplied primary pin.
No network message supplies a replacement trust root. A challenge is freshness,
not identity authentication by itself. A misconfigured pin is not automatically
corrected, and a malicious identity deliberately configured as trusted remains
trusted; this protocol cannot repair a compromised bootstrap channel.

Only one Hello may introduce a secondary name/key. Join carries only its proof,
so it cannot substitute a second ECDH identity. Both peers build the same
transcript from their own state. Every field uses `u32_be(length) || bytes`:

1. `oly-node-join-v2`
2. federation version as two big-endian bytes (`2`)
3. `secondary-proof`
4. role label `secondary`
5. role label `primary`
6. claimed UTF-8 name (nonempty, <=256 bytes, no control characters)
7. raw secondary Ed25519 key (32 bytes)
8. raw primary Ed25519 key (32 bytes)
9. fresh secondary nonce (32 bytes)
10. fresh primary nonce (32 bytes)

The primary verifies strict Ed25519 against the exact name/key registered in
`node_identities`. Weak, non-canonical and non-prime-subgroup public points are
rejected; all-zero X25519 DH is rejected. Fresh primary nonces fence captured
Join proofs on new sockets, without a global ever-growing nonce database.

The sole state progression is:

```text
Hello -> Challenge -> Join -> sealed KeyConfirm -> sealed Ready -> sealed Joined
```

KeyConfirm and Ready explicitly carry the transcript hash. Their authentication
comes from AES-GCM, **not a separate HMAC**. The primary proves possession of
matching channel keys, then the secondary proves the same. Registration occurs
atomically after Ready; the final acknowledgement is sealed. No application RPC
is processed and no connector success is reported before encrypted Joined.
A generation token fences disconnect so stale cleanup cannot remove a new owner.

One 5-second deadline covers the exchange on both sides (secondary includes
DNS/TCP/WS connection). Primary final Joined send uses the same deadline. Terminal
plaintext error messages are diagnostic only, untrusted, and never authorize
state or trigger a plaintext decoding fallback. Failures are throttled separately
from optional HTTP browser authentication. Handshake binary JSON is uncompressed
and limited to 4096 bytes after WS assembly; the WS allocation cap is 64 MiB,
matching supported application payloads. Thus the pre-parser allocation bound is
64 MiB, **not** 4096 bytes. A dedicated small pre-auth WS allocation layer would
be a further resource-hardening project.

## Channel construction

Ed25519 seed -> raw expanded scalar bytes -> X25519 StaticSecret; X25519 clamps
internally. Public identity converts via the Edwards-to-Montgomery map.

```text
shared = X25519(local_private, peer_public)
T = SHA256(length_delimited_transcript)
HKDF-SHA256 salt = T, input key material = shared
info = "oly-channel-v2" || '|' || lex_low_identity || '|'
       || lex_high_identity || '|' || direction ("c2s" / "s2c")
```

Fixed identity lengths make the HKDF info unambiguous. Identities are sorted so
both local orientations derive the same keys; the signed transcript binds their
actual roles. c2s means secondary->primary, s2c primary->secondary.

```text
frame = 0x21 || seq:u64_be || ciphertext || GCM_tag:16
nonce = zero[4] || seq:u64_be
AAD = "oly-node-frame-v2" || T || direction || 0x21 || seq:u64_be
```

The single relay task owns non-Clone mutable channel state. Each direction has
its own key/counters. Handshake frames consume sequence numbers: KeyConfirm s2c
0, Ready c2s 0, Joined s2c 1. Application traffic continues those counters;
there is no reset at readiness. Receive requires the next exact sequence and
commits only after authentication **and parsing**. Failure poisons the state and
the socket closes, rather than skipping a bad frame. Counter exhaustion fails
before reuse. Only authenticated application keepalives refresh liveness.

Static-static DH **does not provide forward secrecy**. Fresh transcript nonces
separate sessions and prevent nonce/key reuse across normal reconnects; they do
not protect captured traffic after a long-term identity compromise.

## Fixed regression vector

`fixed_handshake_crypto_vector` freezes the following values (base64). These are
locally generated regression vectors, not independently certified reference
vectors. Primary seed `[7;32]`, secondary seed `[8;32]`, name `worker`, secondary
nonce `[1;32]`, primary nonce `[2;32]`, version 2, role `secondary-proof`:

```text
transcript SHA256: HmtdC0lXeYwNKHouLnfydZW8IaLOo3Trj6KGmdDE7DQ=
secondary signature: DT3jT3pYnGE0YLAbIrpjYIq7kpEqEjUhBLiDqRJDS4cbDcZtGu22pwaBUzFuxyjPomfDEe3obj/yCyfRcbxeCg==
c2s: mfzqQzh8/eny4kmnF08AM6UmqF39ygIVLzXArYynzgI=
s2c: 4InHLOGwjhF5h9bZILyvlzFt+Fdx3jPFzeAoCrVo/O4=
```

For primitive framing regression only, primary seq 0 sealing the bytes
`{"type":"key_confirm"}` (not a full production KeyConfirm) yields:

```text
IQAAAAAAAAAAJpkj8mmYdLZyejRx1iDTOjN9SGxhDtokVT0lE2FlJWBhNilDsAc=
```

## Observable tests

`tests/e2e_federation_security.rs` runs raw adversarial peers against real daemon
processes, and controlled fake primaries against the **real binary connector**.
It does not implement a parallel `join_for_test`. It reuses production crypto
primitives so the tests focus on wire transitions, registry effects and real
CLI feedback; unit tests independently freeze crypto mutation and vector checks.

Coverage includes captured Join replay; freshly re-signed unauthorized name;
unregistered key and invalid signature; legacy proof rejection; repeated Hello;
wrong version/weak key/oversized input; missing/incorrect/plaintext Ready;
sealed KeyConfirm/Joined sequence observation; failed duplicates preserving the
owner; replayed application event producing exactly one real SSE effect;
prior-session replay; wrong pin without false success and successful corrected
rejoin; forged plaintext Joined at all three connector boundaries; timeout and
stop cancellation; daemon restart preserving seeds/accept records/saved joins;
pending RPC failure on terminal channel errors; real password HTTP
login/session/cookie/logout with machine Bearer rejection. Registry unit tests cover concurrent insertion and stale
cleanup. Existing lifecycle and credited streaming-attach tests exercise the
new sole production path.
