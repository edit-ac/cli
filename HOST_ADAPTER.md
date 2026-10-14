# Native synchronization host adapter

The implemented `sync` command operates on an existing native hosted cache.
First acquisition and source editing use the `hosted::open` and
`HostedSession::prepare_source` library APIs. Hosted editor command/package
qualification remains separate. Local file editing and offline `status` need
neither a host executable nor credentials.

```
editac status ./note.md
editac status https://app.example.test/ac/DOCUMENT_UUID \
  --service-config ./service.json --cache-dir /absolute/private/cache-parent
editac sync https://app.example.test/ac/DOCUMENT_UUID \
  --service-config ./service.json --cache-dir /absolute/private/cache-parent \
  --adapter /absolute/configured/host-executable --attempts 3
```

`service.json` contains `host`, `endpoint` and the complete initialized `identity`
(`profile`, `schema`, `documentId`, `epoch`, `protocolVersion`, `updateEncoding`,
`offsets`). The identity must match the cache and qualified v4/v5 profile exactly.
The endpoint is explicit HTTPS configuration; this client implements no production
HTTP backend. Config files are bounded to 8 KiB. No credential belongs in that
file. A missing cache is an error and never creates an empty hosted replacement.

The configured executable receives one JSON request on stdin and emits exactly
`{"ok": VALUE}` or `{"error": ERROR_CODE}` on stdout. This is private local host
IPC, not a server wire format. Each request contains an `attachment` correlation
ID. The host must bind connect negotiation, baseline messages and application
receipts to the same authenticated attachment, isolate concurrent attachments,
and release its resources on disconnect. The ID is not a bearer credential.
The host supplies the existing qualified server adapter and its saved credential
store; this interface is not automatic discovery, a background runner or a
production service implementation.

| operation | additional request fields | successful VALUE |
| --- | --- | --- |
| credential | endpoint | saved token string or null |
| connect | endpoint, identity, credential | identity, receiptProtocol (qualified name or null) |
| exchange | frames (arrays of bytes) | received frames (arrays of bytes) |
| upload | entry (immutable native journal identity/bytes/checksum/updateId) | receipt (qualified envelope or null) |
| disconnect | none | null |

Error codes are `unavailable`, `authorization`, `revoked`, `incompatible` and
`invalid-protocol`. The executable and arguments are never interpolated into a
shell. The child environment is cleared; stderr is discarded, and requests and
responses live in private temporary files removed after the child is reaped.
Each operation has a three-second process deadline and 96 MiB IPC byte bound.
A host executable must perform bounded operations and must not leave descendants,
servers or background connections after disconnect. Service authentication and
receipt authenticity are host responsibilities.

Baseline messages are the pinned y-protocols 1.0.7 step1/step2/update frames over
Yrs/Yjs v1 bytes, with a 16 MiB frame/exchange bound, 32 frames per exchange and
32 handshake rounds. The complete schema/epoch identity is negotiated before
frames. Awareness does not enter this port. Application receipts use
`editac-durable-receipt-v1`: state `durable`, complete identity, writerId, sequence,
checksum and updateId. They must be emitted after the service durable commit.
Wrong/missing receipts, socket delivery, matching state vectors and successful
process exit cannot prune pending work. Receipts remove pending membership while
recovery journal bytes remain. Native received records are not local uploads.

Sync retries availability failures only (1–5 attempts); default backoff is 50 ms,
doubles, and is capped at 2 seconds. One explicit run uploads at most 32 entries
and 16 MiB. Later entries remain pending for another explicit invocation. Offline
cache/draft/Markdown export remains available after revocation or incompatibility.
The initial replica must be native-durable before credential access or transport
attachment. Received changes cross native persistence before another send. Dependent frames may
arrive out of order: only atomic missing-dependency rejections enter a bounded
32-frame/16 MiB receive window, retried as dependencies arrive. The attachment
stops after 32 exchanges if that window cannot drain; local pending work remains.
A delivery receipt must match the exact entry uploaded on that call.

Commands emit JSON status. A completed invocation can report `retry-exhausted`,
`revoked` or another failure state; the JSON connection state and pending count
are authoritative. Invalid arguments/configuration/recovery return exit 2.
`locallySaved` describes committed CRDT persistence, independently of drafts.
`remoteDurability` becomes `remotely-committed` only when exact receipts cover all
pending local updates; otherwise it is `unknown`. `remoteCommittedUpdates` counts
retained exact application receipts. A stale unresolved source draft is flagged
as a conflict requiring source revalidation/resolution. For an ordinary local
file, status observes source availability and reports durability unknown rather
than inferring a prior flush acknowledgment from file existence.

Qualification uses prepared authenticated transports and fake host executables.
No live credentials, hosted writes, provider inference or publication are implied.
The native durability envelope is the existing tested Unix filesystem boundary,
not a power-loss or arbitrary filesystem guarantee.
