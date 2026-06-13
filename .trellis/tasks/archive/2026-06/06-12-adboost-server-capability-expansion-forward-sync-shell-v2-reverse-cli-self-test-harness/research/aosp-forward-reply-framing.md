# Research: AOSP adb server forward/killforward/list-forward reply framing

- **Query**: Exact byte-level reply framing AOSP's adb *server* sends for host:forward / host:killforward / host:killforward-all / host:list-forward, to keep native adb + scrcpy clients in sync.
- **Scope**: external (AOSP source, fetched from android.googlesource.com `platform/packages/modules/adb`, branch `main`, 2026-06-12)
- **Date**: 2026-06-12

## Primitives (these define every byte)

From `adb_io.cpp`:

```cpp
// line 68
bool SendOkay(borrowed_fd fd) {
    return WriteFdExactly(fd, "OKAY", 4);          // EXACTLY the 4 ASCII bytes "OKAY", no length prefix
}

// line 72
bool SendFail(borrowed_fd fd, std::string_view reason) {
    return WriteFdExactly(fd, "FAIL", 4) && SendProtocolString(fd, reason);  // "FAIL" + framed reason
}

// line 37
bool SendProtocolString(borrowed_fd fd, std::string_view s) {
    unsigned int length = s.size();
    ...
    auto str = android::base::StringPrintf("%04x", length).append(s);  // 4 lowercase hex digits + body
    return WriteFdExactly(fd, str);
}
```

So the three wire tokens are:
- `SendOkay`  -> `OKAY` (4 bytes, NO trailing length, NO body)
- `SendProtocolString(s)` -> `%04x` of `s.size()` (4 ASCII hex chars, lowercase, zero-padded) immediately followed by the raw bytes of `s`
- `SendFail(reason)` -> `FAIL` (4 bytes) + `SendProtocolString(reason)`

The 4-hex length prefix is lowercase `%04x` and is the byte length of the body that follows. The reader (`ReadProtocolString`, adb_io.cpp:50) does `strtoul(buf, nullptr, 16)` on the 4 chars then reads exactly that many bytes.

## The two-OKAY ("smartsocket") convention

`handle_forward_request` (adb.cpp:1133) is compiled into BOTH the host server and the device daemon. The `#if ADB_HOST` blocks add an EXTRA `SendOkay` that ONLY the server (the side scrcpy/adb-client talks to on :5037) emits. The comments are explicit:

```cpp
// adb.cpp:1149  (killforward-all)
/* On the host: 1st OKAY is connect, 2nd OKAY is status */
SendOkay(reply_fd);   // under #if ADB_HOST  -> connect/smartsocket OKAY
#endif
SendOkay(reply_fd);   // always -> status OKAY
```

```cpp
// adb.cpp:1208  (forward / killforward success)
#if ADB_HOST
// On the host: 1st OKAY is connect, 2nd OKAY is status.
SendOkay(reply_fd);   // connect OKAY
#endif
SendOkay(reply_fd);   // status OKAY
```

### Why forward emits TWO okays but `host:version` emits ONE okay + body

This is a real asymmetry and the crux of staying in sync.

- For a **normal host query** like `host:version`, the dispatcher itself does the single smartsocket OKAY (the connect/accept ack) and then the handler does `SendOkay(reply_fd, body)` which is `SendOkay` + `SendProtocolString` (adb.cpp:1480 `SendOkay(reply_fd, StringPrintf("%04x", ADB_SERVER_VERSION))`). Net result for the client: ONE `OKAY` consumed as connect status, then a `%04x`-framed body.

  Wait — note `SendOkay(fd, s)` (the 2-arg overload, adb.cpp:1245) is itself `SendOkay(fd)` + `SendProtocolString(fd, s)`. So `host:version` on the wire is actually `OKAY` + `0004` + `0029` (length 4 + the version hex). The client's `_adb_connect` reads the leading `OKAY` as connect status (adb_status), then the caller `adb_query`/`adb_query_command` reads the framed body. So host:version = **1 OKAY + framed body**.

- For the **forward family**, the handler emits the connect OKAY (`#if ADB_HOST`) AND a second status OKAY, with NO body in the common case. So the client sees **OKAY OKAY** = `OKAYOKAY` (8 bytes), optionally followed by a framed resolved-port body.

The difference exists because forward/killforward are dispatched through `handle_forward_request` which historically owned its own connect-ack, whereas host queries are framed by the generic host-service dispatcher. Bottom line for our server implementation: **forward/killforward/killforward-all/list-forward must emit the connect OKAY themselves** (do not let a generic dispatcher also prepend one, or you double it to three).

## Client read path (proves the framing the server MUST match)

`client/commandline.cpp` forward handler (around line 1955):

```cpp
unique_fd fd(adb_connect(nullptr, host_prefix + cmd, &error_message, true));
if (fd < 0 || !adb_status(fd.get(), &error_message)) {   // reads the SECOND OKAY
    error_exit(...);
}
// Server or device may optionally return a resolved TCP port number.
std::string resolved_port;
if (ReadProtocolString(fd, &resolved_port, &error_message) && !resolved_port.empty()) {
    printf("%s\n", resolved_port.c_str());                // reads optional %04x + decimal port
}
ReadOrderlyShutdown(fd);
```

And `adb_connect`/`_adb_connect` (client/adb_client.cpp:158-195): because the service starts with `host`, `_adb_connect` itself calls `adb_status(fd, error)` (line 190) which consumes the FIRST `OKAY` (the connect ack). Then commandline.cpp calls `adb_status` a SECOND time (line 1956) to consume the status `OKAY`. So the client unambiguously expects **two OKAYs** for forward, then an optional framed body.

`adb_status` (client/adb_client.cpp:137): reads 4 bytes; `OKAY` -> ok; `FAIL` -> reads framed reason and reports error; anything else -> "protocol fault (status XX XX XX XX?!)". So a malformed first/second token desyncs immediately.

---

## Answers to the specific questions

### 1. Success `host:forward:<local>;<remote>` (non-zero local tcp port)

Server writes (adb.cpp:1207-1219):
```
OKAY      (connect OKAY, #if ADB_HOST)
OKAY      (status OKAY)
```
i.e. the 8 bytes `OKAYOKAY`. **No length-prefixed body** when the resolved port equals the requested non-zero port (see note below — actually the body is sent whenever `resolved_tcp_port != 0`; see Q2). For a plain `tcp:<nonzero>` forward, `install_listener` leaves `resolved_tcp_port == 0` (it only sets it on port-0 auto-assign), so the common case is exactly `OKAYOKAY` with nothing after. Source: adb.cpp lines 1208-1219.

### 2. Success `host:forward:tcp:0;<remote>` (local port 0 = OS auto-assign)

```
OKAY                 (connect OKAY)
OKAY                 (status OKAY)
%04x<decimal-port>   (SendProtocolString of StringPrintf("%d", resolved_tcp_port))
```
The framing is: second OKAY, THEN a 4-lowercase-hex length prefix + the ASCII **decimal** port string. Example for port 38271: `OKAY` `OKAY` `0005` `38271`. Source: adb.cpp:1215-1217:
```cpp
if (resolved_tcp_port != 0) {
    SendProtocolString(reply_fd, android::base::StringPrintf("%d", resolved_tcp_port));
}
```
So it is **"OKAY" + "OKAY" + 4-hex-len + decimal port** — NOT "OKAY + 4-hex-len + port". The resolved port is reported in DECIMAL ASCII (via `%d`), and its length is what the `%04x` prefix measures (number of digits, not 4).

### 3. `host:killforward:<local>` success and `host:killforward-all` success

- `killforward:<local>` success goes through the same `forward:`/`killforward:` branch. On `INSTALL_STATUS_OK` it sends connect OKAY + status OKAY, and since `resolved_tcp_port` stays 0 for kill, **no body**. Wire bytes: `OKAYOKAY`. Source: adb.cpp:1197-1219.
- `killforward-all` success (adb.cpp:1146-1153): connect OKAY + status OKAY, no body. Wire bytes: `OKAYOKAY`.

### 4. `host:list-forward`

Source (adb.cpp:1136-1143):
```cpp
if (!strcmp(service, "list-forward")) {
    std::string listeners = format_listeners();
#if ADB_HOST
    SendOkay(reply_fd);                       // connect OKAY
#endif
    SendProtocolString(reply_fd, listeners);  // %04x + body
    return true;
}
```
So list-forward is **ONE OKAY (connect) + `%04x` length + body**. NOTE: this is the ONE exception in the forward family — it sends a SINGLE OKAY then a framed body (like host:version), NOT two OKAYs. The status is implicit in the framed body.

Body format (one entry per listener) from `format_listeners()` (adb_listeners.cpp:129-144):
```
"<serial> <local-name> <remote-name>\n"
```
Built with `StringAppendF(&result, "%s %s %s\n", serial_or_"(reverse)", local_name, connect_to)`:
- Fields separated by a single space ` `, each line terminated by `\n`.
- Serial is the device serial, or the literal `(reverse)` for reverse rules (which have no serial).
- `local-name` is like `tcp:5555`, `remote-name` like `tcp:5555` / `localabstract:...`.
- Smart-socket listeners are skipped (`if (l->isSmartSocket()) continue;`).
- The `%04x` length prefix measures the total byte length of the whole concatenated multi-line string (each line already ends with `\n`; the result has a trailing `\n` and no extra terminator). Empty list -> body length 0 -> `OKAY0000`.

### 5. `norebind:` modifier — `host:forward:norebind:<local>;<remote>`

Parsing (adb.cpp:1171-1177): after stripping `forward:`, if the remainder starts with `norebind:`, it strips that and sets `flags |= INSTALL_LISTENER_NO_REBIND` (adb.cpp:1200-1203). Effect: `install_listener` will REFUSE to replace an existing rule for the same local port instead of rebinding it.

Failure reply when a rule already exists for that local port: `install_listener` returns `INSTALL_STATUS_CANNOT_REBIND`, which maps to the message `"cannot rebind existing socket"` (adb.cpp:1230-1232) and is sent via `SendFail` (adb.cpp:1237). On the wire:
```
FAIL                                  (4 bytes)
%04x"cannot rebind existing socket"   (length 0x1e = 30, then the 30-char reason)
```
Important: `SendFail` does NOT emit a connect OKAY first — the FAIL replaces both OKAYs. So an error reply is `FAIL` + framed reason ONLY (no preceding OKAY). The client's first `adb_status` (inside `_adb_connect`) reads `FAIL` and reports the reason; it never reaches the second `adb_status`. This means on error the server must send EXACTLY one FAIL token (not OKAY-then-FAIL).

### 6. First vs second OKAY (smartsocket convention)

- The FIRST OKAY is the smartsocket/"connect" acknowledgement — on the server it is emitted by the `#if ADB_HOST` `SendOkay` inside `handle_forward_request` for forward/killforward/killforward-all/list-forward. (For generic host queries it is emitted by the host-service dispatcher.)
- The SECOND OKAY is the operation "status" OKAY.
- Yes — the forward family (forward / killforward / killforward-all) genuinely emits **TWO OKAYs** (`OKAYOKAY`), optionally followed by a framed body (only forward with auto-assigned port adds the body).
- `host:version` (and other host data queries) emit **ONE OKAY + framed body** (`OKAY` + `%04x` + payload).
- `host:list-forward` is the odd one: **ONE OKAY + framed body** (despite being in the forward handler), because it returns data and uses `SendProtocolString` for the body rather than a second bare OKAY.

### 7. FAIL framing for forward errors

`SendFail` (adb_io.cpp:72): `"FAIL"` (4 raw bytes) + `SendProtocolString(reason)` = `%04x`(lowercase hex length of reason) + reason bytes. No OKAY precedes a FAIL in the forward path. Source: adb.cpp:1237 and adb_io.cpp:72-74.

Concrete forward error reasons (adb.cpp:1222-1236):
- bad forward parse: `"bad forward: <service>"` / `"bad killforward: <service>"` (sent earlier, adb.cpp:1184/1190)
- `INSTALL_STATUS_INTERNAL_ERROR` -> `"internal error"`
- `INSTALL_STATUS_CANNOT_BIND` -> `"cannot bind listener: <error>"`
- `INSTALL_STATUS_CANNOT_REBIND` -> `"cannot rebind existing socket"`
- `INSTALL_STATUS_LISTENER_NOT_FOUND` -> `"listener '<service>' not found"` (this is the killforward-of-nonexistent-rule case)

---

## Quick byte-sequence cheat sheet (server -> client, host/:5037 side)

| Request | Success bytes on wire | Notes |
|---|---|---|
| `host:forward:tcp:5555;tcp:5555` | `OKAY` `OKAY` | two bare OKAYs, no body (resolved_tcp_port==0) |
| `host:forward:tcp:0;tcp:5555` | `OKAY` `OKAY` `%04x` `<dec port>` | port in ASCII decimal, len-prefixed |
| `host:forward:norebind:tcp:5555;...` (already bound) | `FAIL` `001e` `cannot rebind existing socket` | single FAIL, no OKAY |
| `host:killforward:tcp:5555` | `OKAY` `OKAY` | |
| `host:killforward:tcp:5555` (no such rule) | `FAIL` `%04x` `listener 'tcp:5555' not found` | |
| `host:killforward-all` | `OKAY` `OKAY` | |
| `host:list-forward` | `OKAY` `%04x` `<serial local remote\n ...>` | SINGLE OKAY + framed body |
| `host:version` (reference) | `OKAY` `%04x` `<hex version>` | SINGLE OKAY + framed body |

All length prefixes are 4 lowercase hex ASCII chars (`%04x`). `OKAY`/`FAIL` are 4 raw ASCII bytes with NO length prefix.

## Source files / functions (AOSP `platform/packages/modules/adb`, branch main)

- `adb.cpp`:
  - `handle_forward_request(...)` lines 1127-1242 (the dispatcher with all `#if ADB_HOST` OKAYs)
  - 2-arg `SendOkay(fd, s)` overload line 1245
  - `host:version` reply line 1480
- `adb_io.cpp`: `SendProtocolString` line 37, `ReadProtocolString` line 50, `SendOkay` line 68, `SendFail` line 72
- `adb_listeners.cpp`: `format_listeners()` line 129, `remove_listener` 146, `remove_all_listeners` 158; `InstallStatus`/`INSTALL_LISTENER_NO_REBIND` flags consumed at adb.cpp:1200-1234
- `client/adb_client.cpp`: `adb_status` line 137 (reads OKAY/FAIL), `_adb_connect` line 158 (consumes first OKAY for `host*` services), `adb_kill_server` 198
- `client/commandline.cpp`: forward/reverse handler lines 1910-1967 (consumes SECOND OKAY via `adb_status` at 1956, then optional `ReadProtocolString` resolved port at 1962)

## Caveats / Not Found

- Fetched from branch `main` on 2026-06-12; the two-OKAY convention and framing are long-standing and stable across recent AOSP, but line numbers will drift between revisions. The `#if ADB_HOST` comments ("1st OKAY is connect, 2nd OKAY is status") are the authoritative anchor.
- `resolved_tcp_port` is only set non-zero by `install_listener` for OS-auto-assigned ports (local `tcp:0`). I inferred this from the handler (it is initialized to 0 and only the port-0 path resolves a real port); I did not paste `install_listener`'s body. If you need to confirm exactly when a non-zero resolved port is echoed for a `tcp:0` vs named local socket, read `install_listener` in `adb_listeners.cpp`.
- The `%04x` is lowercase hex (`StringPrintf("%04x", ...)`). scrcpy parses these case-insensitively via strtoul base 16, but emit lowercase to match adb exactly.
