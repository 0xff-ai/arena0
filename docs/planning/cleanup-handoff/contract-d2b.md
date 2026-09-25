# D2b One bounded field codec; derive Borsh where fields carry their bounds

## Contract

Round-1 §3.12. The shapes below are decided. Implement them exactly. Add no
type, function, trait impl, re-export or module that is not listed. Private
local variables are fine.

**Escalate instead of improvising.** If a shape cannot be achieved as written
(it does not compile, it changes an encoding, it breaks a test's observable
behavior other than an error-message text, or it needs something not listed),
stop that item. Put in your report the item, the exact obstacle
(file:line and the error) and the options you see. Items
that do not depend on the blocked one may be finished first.

**Encodings stay byte-identical.** Every derived codec below produces exactly
the bytes the hand-written one produced: same field order, same `u32` length
prefixes, `Option` as a `0/1` byte (the same bytes as the old `bool` flag), enum
variants declared in their old tag order. So no version constant changes
(`ABI_VERSION`, trace/receipt versions, store schema, negotiation encoding
versions). The existing round-trip and golden-byte tests must pass unchanged.
If one fails, the encoding changed: escalate.

`arena0-wire` is out of scope. Its frames bound composite sizes (abort
occurrence, fetch response) and it does not depend on `arena0-program`.

### 1. One bounded module: `crates/arena0-program/src/bounded.rs`

New file, declared `pub mod bounded;` in `crates/arena0-program/src/lib.rs`. No
root re-exports of its functions. Contents, exactly:

```rust
//! Bounded Borsh field codecs, used through
//! `#[borsh(serialize_with = "...", deserialize_with = "...")]` on fields whose
//! encoding carries a length bound. Readers reject an over-limit length prefix
//! before they allocate. Each encoding is the stock Borsh encoding of the
//! unbounded type.

use std::io;

use borsh::{BorshDeserialize, BorshSerialize};

pub fn write_bytes<const MAX: usize>(bytes: &[u8], writer: &mut impl io::Write) -> io::Result<()>;
pub fn read_bytes<const MAX: usize>(reader: &mut impl io::Read) -> io::Result<Vec<u8>>;
pub fn write_string<const MAX: usize>(value: &str, writer: &mut impl io::Write) -> io::Result<()>;
pub fn read_string<const MAX: usize>(reader: &mut impl io::Read) -> io::Result<String>;
pub fn write_option_string<const MAX: usize>(value: &Option<String>, writer: &mut impl io::Write) -> io::Result<()>;
pub fn read_option_string<const MAX: usize>(reader: &mut impl io::Read) -> io::Result<Option<String>>;
pub fn write_vec<const MAX: usize, T: BorshSerialize>(items: &[T], writer: &mut impl io::Write) -> io::Result<()>;
pub fn read_vec<const MAX: usize, T: BorshDeserialize>(reader: &mut impl io::Read) -> io::Result<Vec<T>>;
```

- `MAX` is a byte bound for bytes and strings, and an element-count bound for
  `write_vec`/`read_vec`.
- Writers: if the length is over `MAX`, return `io::ErrorKind::InvalidInput`
  with the message `format!("length {len} exceeds bound {MAX}")`. Otherwise write
  the `u32` length (`u32::try_from`, with the same `InvalidInput` on overflow)
  and then the bytes, or each element for `write_vec`.
- Readers: read the `u32` length. If it is over `MAX`, return
  `io::ErrorKind::InvalidData` with the same message *before* allocating. Then
  `vec![0; len]` plus `read_exact`, or, for `read_vec`,
  `Vec::with_capacity(len)` plus `T::deserialize_reader` per element. Strings
  map a UTF-8 error to `InvalidData`.
- The option pair writes the `0u8`/`1u8` Borsh `Option` tag and then
  `write_string::<MAX>`. The reader rejects any tag other than 0 or 1 with
  `InvalidData`.
- Each function gets a one-line doc comment. If clippy flags `ref_option` on
  `write_option_string`, add
  `#[allow(clippy::ref_option, reason = "borsh serialize_with passes the field by reference")]`
  to that function only.
- Attribute paths are string `ExprPath`s with a turbofish:
  `#[borsh(serialize_with = "bounded::write_bytes::<MAX_X>", deserialize_with = "bounded::read_bytes::<MAX_X>")]`.
  Import the module (`use crate::bounded;` inside `arena0-program`, `use arena0_program::bounded;` elsewhere)
  and the constant so the path stays short. If a multi-segment constant path
  is rejected as a generic argument, wrap it in braces (`::<{ path::MAX_X }>`).

**Delete the duplicates** and move their callers to this module:

- `crates/arena0-program/src/abi.rs`: `write_bounded_vec`, `read_bounded_vec`,
  `field_error`, `EnvelopeWriter`, `EnvelopeReader` and their `impl`s. The
  complete-envelope bound is already enforced on the raw buffer, by the
  sandbox (`engine/runtime.rs` `encode_envelope` and the result decode) and by
  the guest glue (`sdk-macros/src/program/guest_abi.rs`). If
  `AbiEnvelopeError::EnvelopeTooLarge` loses its last constructor, delete the
  variant. `ensure_field` and `AbiEnvelopeError::FieldTooLarge` stay: the
  `try_new` constructors use them.
- `crates/arena0-program/src/state.rs`: `read_borsh_bounded`. The macro's
  `BorshDeserialize` body becomes `bounded::read_bytes::<{ Self::MAX_LEN }>(reader).map(Self)`
  if `Self::MAX_LEN` is a `const` usable there. If it is not, escalate.
- `crates/arena0-protocol/src/bounded.rs`: delete the file and its `mod`
  line. Every `crate::bounded::…` user in `arena0-protocol` uses
  `arena0_program::bounded::…`.
- `TimerPayload::serialize_bounded` and `TimerPayload::deserialize_bounded`
  (`crates/arena0-protocol/src/timer.rs`): delete them. `TimerPayload` keeps its
  derive and gains field attributes: `type_name` gets
  `write_string`/`read_string::<MAX_TERMINAL_REASON_BYTES>`, and `data` gets
  `write_bytes`/`read_bytes::<MAX_TIMER_PAYLOAD_BYTES>`. Its callers (`Effect`,
  `Event`) then use the plain derived codec.

### 2. Pattern P1: derive with bounded field attributes

Replace the hand-written `BorshSerialize` and `BorshDeserialize` impls with
`#[derive(BorshSerialize, BorshDeserialize)]`. Put the listed attribute on each
bounded field, pairing `write_*` with `read_*` of the same kind and bound.
Unlisted fields use plain derive.

`arena0-program/src/abi.rs`:

| Type | Bounded fields |
|---|---|
| `CallStatus` | none (variants `Accepted`, `Rejected` in this order) |
| `InitInput` | `params`: bytes, `MAX_CALL_PAYLOAD_BYTES` |
| `InitializedState` | none |
| `DispatchInput` | `session`: bytes, `MAX_SESSION_CONTEXT_BYTES`; `event`: bytes, `MAX_CALL_PAYLOAD_BYTES` |
| `CalloutRequest` | `context`: bytes, `MAX_CALLOUT_CONTEXT_BYTES` |
| `QueryInput` | `session`: `MAX_SESSION_CONTEXT_BYTES`; `query`: `MAX_CALL_PAYLOAD_BYTES` |
| `QueryOutput` | `json`: `MAX_CALL_PAYLOAD_BYTES` |
| `ViewInput` | `session`: `MAX_SESSION_CONTEXT_BYTES`; `viewport`: `MAX_CALL_PAYLOAD_BYTES` |
| `ViewOutput` | `json`: `MAX_CALL_PAYLOAD_BYTES` |
| `OutcomeInput` | `session`: `MAX_SESSION_CONTEXT_BYTES` |
| `WriterInput` | none |
| `OutcomeOutput` | `borsh`, `json`: `MAX_CALL_PAYLOAD_BYTES` |

`CallStatus` keeps `tag`, `from_tag` and its serde impls (serde is out of scope).

`arena0-protocol`:

| Type | Bounded fields and variant order |
|---|---|
| `Effect` | `SessionEnd{outcome: bytes MAX_TERMINAL_OUTCOME_BYTES}`, `SessionAbort{reason: string MAX_TERMINAL_REASON_BYTES}`, `Broadcast{data: bytes MAX_EFFECT_PAYLOAD_BYTES}`, `SetTimer{delay_ms, timer}`, `Fail{reason: string MAX_TERMINAL_REASON_BYTES}` |
| `Event<M>` | `SessionStarted{ensemble}`, `MessageReceived{from, msg}`, `InputReceived{callout_index, data: bytes MAX_EFFECT_PAYLOAD_BYTES}`, `TimerFired{timer}` |
| `OpenCallout` | `context`: bytes, `arena0_program::MAX_CALLOUT_CONTEXT_BYTES` |
| `StopCause` | `Authenticated(AbortOccurrence)`, `Shared{kind, commitment, reason: string MAX_TERMINAL_REASON_BYTES}` |
| `AbortKind` | none (`Abort`, `Fail`); keep `tag`, `from_tag` and its serde impls |
| `TicketAction` | `Active{execution_bls, key_binding, issued_at_unix_ms, valid_for_ms}`, `Withdrawn` |
| `NegotiationFact` | declare the variants in tag order `Offer`, `Ticket`, `ActivationSignature`, `Counteroffer`, `ActivationAnnouncement` (this swaps the last two declarations) |
| `StepEvent` | `SessionStarted{ensemble}`, `Message{from, data: bytes MAX_EFFECT_PAYLOAD_BYTES}` |
| `StepTerminal` | `End{outcome: bytes MAX_TERMINAL_OUTCOME_BYTES}`, `Abort{reason: string MAX_TERMINAL_REASON_BYTES}`, `Fail{reason: string MAX_TERMINAL_REASON_BYTES}` |

Delete the tag constants (`EFFECT_*`, `EVENT_*`, `NEGOTIATION_FACT_*`) once they
lose their last user. Their only users today are the codecs. If a
derive-generated variant order differs from the old tags anywhere, escalate.
Do not add `#[borsh(use_discriminant)]`.

### 3. Pattern P2: derived serialize, validated deserialize through a private raw struct

For a type whose decode validates an invariant:

```rust
#[derive(..., BorshSerialize)]          // plus the field attributes
pub struct T { ... }

#[derive(BorshDeserialize)]
struct TRaw { /* the same fields, same order, same types, same attributes */ }

impl BorshDeserialize for T {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let raw = TRaw::deserialize_reader(reader)?;
        /* the existing validating constructor or check, applied to raw */
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
    }
}
```

No `TryFrom` impl and no new constructor: the body calls the validator that
exists today. The derived serializer drops the old serialize-side bound checks
where the bounded attribute now does them. It also drops the invariant checks,
because decode enforces them. The types:

| Type | Raw fields (bounded) | Validation applied to raw |
|---|---|---|
| `abi::DispatchOutput` | `status`, `reason` (option string, `MAX_REJECTION_REASON_BYTES`), `callout` | the two existing checks (accepted⇒no reason, rejected⇒no callout), same messages; build `Self` |
| `execution::outcome::TerminalOutcome` | `borsh`, `json` (bytes, `MAX_TERMINAL_OUTCOME_BYTES`) | `Self::new(raw.borsh, raw.json)` |
| `execution::abort::AbortOccurrence` | as today; `reason` string `MAX_TERMINAL_REASON_BYTES` | build `Self`, then `validate_shape()` |
| `execution::signing::GuestSignData` | as today; `payload` bytes `MAX_EFFECT_PAYLOAD_BYTES` | build `Self`, then `validate()` |
| `negotiation::Offer` | `data`, `tickets` | `Self::new(raw.data, raw.tickets)` |
| `trace::entry::TraceEntry` | as today | build `Self`, then `validate_version(entry.trace_version)` |

`TerminalOutcome` already derives its serializer. If it does not, give it the
derive with the attributes above. If any of these types has private fields that
a same-module raw struct cannot mirror, escalate.

### 4. Stays hand-written; bodies use `bounded`

These keep their impls, because they carry an in-band version byte, split one
variant across two tags, or wrap a validated newtype. Replace only the
length-prefix code inside them with `bounded::…` calls:

- `ExecutionAdmission` (version byte, and `Join` split across tags 1 and 3);
- `ReceiptBody` (version byte; use `read_vec::<MAX_RECEIPT_TRACE_ENTRIES, TraceEntry>`
  and `write_vec`) and `ReceiptArtifact`;
- `PreparedActivation`, `Activation`, `ActivationAnnouncement` (encoding
  version bytes);
- `exec_frame::ExecFrame` (converts through `WireExecFrame`; unchanged);
- `Ensemble<Committed>`: the serializer is
  `bounded::write_vec::<{ crate::MAX_PARTICIPANTS }, _>(&self.peers, writer)`
  and the deserializer is
  `Self::from_peers(bounded::read_vec::<{ crate::MAX_PARTICIPANTS }, PeerId>(reader)?)`
  with the existing error mapping;
- `JsonBytes`: `bounded::write_bytes::<MAX_CALL_PAYLOAD_BYTES>(&self.0, writer)`
  and `Self::try_new(bounded::read_bytes::<MAX_CALL_PAYLOAD_BYTES>(reader)?)`
  with the existing mapping;
- `BorshSchemaDocument`, `JsonSchemaDocument` (`schema.rs`): unchanged;
- the `bounded_state_bytes!` macro: section 1.

### 5. Tests

- `bounded.rs` `#[cfg(test)] mod tests`, one test per reader kind (bytes,
  string, option string, vec). Each encodes a length prefix of `MAX + 1`
  followed by *no* payload. The read must fail with `ErrorKind::InvalidData`,
  not `UnexpectedEof`, which proves the bound is checked before the payload is
  read or allocated. Add one writer test: an over-bound write returns
  `ErrorKind::InvalidInput`.
- One round trip per P1/P2 type family that has no round-trip test today
  (ABI inputs/outputs, `Effect`/`Event`, `StepEvent`/`StepTerminal`/`TraceEntry`,
  negotiation facts, `StopCause`/`AbortOccurrence`). Add it in the owning
  module's existing test module, asserting `decode(encode(x)) == x`.
- Delete tests that only exercised `EnvelopeWriter`/`EnvelopeReader` or a
  removed serialize-side check. List them in the report.
- A test that asserts a field-named error message (for example `"session context"`)
  may change its expected text to the `bounded` message. List each one.

## Acceptance

Run `cargo fmt --all`, `just build-programs` (the SDK and ABI changed),
`just check`, `cargo test -p arena0-program -p arena0-protocol -p arena0-sandbox -p arena0-store`,
and `just test`. All must pass. Your report must include:

- one line per section above;
- the hand-written `impl BorshSerialize|BorshDeserialize` count per crate
  (`arena0-program`, `arena0-protocol`) before and after;
- deleted and changed tests;
- any deviation (there should be none);
- the gate tails.
