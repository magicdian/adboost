# Persistence & External State

> **There is no database in this project.** `xp_adb_client` is an ADB protocol
> client library — it talks to Android devices over USB / TCP, not to any
> datastore. This file replaces the generic "database guidelines" template
> with the persistence/external-state conventions that *do* apply here.

---

## No database, no ORM, no migrations

The project has no SQL/NoSQL store, no ORM, and no migrations. Do not introduce
one to solve a problem that file-based or in-memory state can handle. If a
genuine persistence need arises, raise it as a design discussion first.

---

## The persistent state that does exist

### ADB RSA key (on-disk, read-only by convention)

ADB device authentication uses an RSA keypair read from the filesystem:

- `ADBRsaKey`, `read_adb_private_key` — `message_devices/models/adb_rsa_key.rs`.
- Default key path resolved via `utils::get_default_adb_key_path`.
- If no key is found, a **random** key is generated and a `warn!` is logged
  (`message_devices/usb/persistent.rs:61`) — see `logging-guidelines.md`.
- Never log or persist private key bytes (see `logging-guidelines.md` →
  "What NOT to Log").

### In-memory session multiplexing (USB)

The closest thing to a "connection pool" is the persistent USB session layer:

- `PersistentUsbConnection`, `MultiplexedSession`, `SessionChannels` —
  `message_devices/usb/persistent.rs`.
- One CNXN+AUTH'd USB connection is multiplexed across logical sessions, with a
  background reader thread routing messages.
- Shared state is guarded by `Mutex`. **Lock-handling caveat:** the current code
  uses `lock().unwrap()` (9 sites in `persistent.rs`), which is **known tech
  debt** — new lock sites must propagate `RustADBError::PoisonError` via `?`
  instead (see `error-handling.md` → "Common Mistakes").

### USB transport concurrency (nusb endpoint-split model)

The USB transport (`message_devices/usb/usb_transport.rs`) runs on **`nusb`**,
whose `Endpoint<EpType, Dir>` is `&mut self`-exclusive and **not `Clone`**. The
IN and OUT endpoints are two independent objects. To preserve the reader-thread
(IN) + writer (OUT) concurrency model — and because `ADBMessageTransport` is
bounded `: ADBTransport + Clone + Send + 'static` so `USBTransport` **must stay
`Clone`** (the non-persistent `ADBMessageDevice<USBTransport>` path clones the
transport to share it across its own reader/writer) — the IN and OUT endpoints
live behind **two separate `Arc<Mutex<…>>` locks**, not one shared lock.

> **Why two locks, not one:** the reader's blocking 1s read holds only the IN
> lock; the writer holds only the OUT lock. No code path acquires both, so the
> reader can never stall the writer and there is no lock-ordering deadlock. A
> single shared `Arc<Mutex<USBTransport>>` would let a long blocking read starve
> writes — do not collapse the two locks back into one.

---

## State conventions

- Prefer **passing state explicitly** through the device/transport types over
  global mutable statics. The only statics are `LazyLock<Regex>` for parsing
  (compile-time-constant patterns).
- Transport/device structs own their connection state; commands operate on
  `&mut self` or `&self` of the owning device.

---

## Common Mistakes

- Reaching for a database/ORM where the protocol model needs none.
- Adding a global mutable static instead of threading state through the device
  type.
- Copying the `lock().unwrap()` pattern from `persistent.rs` — propagate
  `PoisonError` instead.
