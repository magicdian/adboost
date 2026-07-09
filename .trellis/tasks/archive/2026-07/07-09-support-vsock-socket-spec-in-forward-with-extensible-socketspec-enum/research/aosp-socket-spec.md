# Research: AOSP adb SocketSpec / socket_spec

- **Query**: What socket spec types does native adb support, how does vsock work on the wire, what's valid for local vs remote, and how is the C++ type structured?
- **Scope**: external (AOSP source on googlesource)
- **Date**: 2026-07-09

## Findings

### 1. Complete Socket Spec Types

Native adb supports these socket spec types (from `client/commandline.cpp` help text and `socket_spec.cpp`):

| Spec Prefix | Format | Description | Local? | Remote? | Listen-only? |
|---|---|---|---|---|---|
| `tcp:` | `tcp:<port>` or `tcp:[host:]<port>` | TCP socket | Yes | Yes | No |
| `localabstract:` | `localabstract:<name>` | Abstract Unix domain socket | Yes | Yes | No |
| `localreserved:` | `localreserved:<name>` | Reserved Unix domain socket | Yes (device-only) | Yes | No |
| `localfilesystem:` | `localfilesystem:<name>` | Filesystem Unix domain socket | Yes | Yes | No |
| `dev:` | `dev:<path>` | Character device (open O_RDWR) | No | Yes (remote only) | No |
| `dev-raw:` | `dev-raw:<path>` | Raw-mode character device | No | Yes (remote only) | No |
| `jdwp:` | `jdwp:<pid>` | JDWP debug connection | No | Yes (remote only) | No |
| `vsock:` | `vsock:<CID>:<port>` (connect) / `vsock:<port>` (listen) | VM socket (Linux-only) | No* | Yes (remote only) | No |
| `acceptfd:` | `acceptfd:<fd>` | Inherited socket FD | Yes (listen only) | No | Yes |

*Note: vsock CAN be used for listen (on the host side for adb server socket), but the `adb forward` help marks it "remote only" meaning the client CLI won't let you use it as the LOCAL endpoint of a forward. The server itself can listen on `vsock:<port>` (used for adb server socket spec, `-L vsock:<port>`).

### 2. vsock Wire Format

#### Connect format (used as forward REMOTE)

```
vsock:<CID>:<port>
```

Examples:
- `vsock:2:5555` — connect to CID 2 (host), port 5555
- `vsock:3:46668` — connect to CID 3, port 46668

The string is sent as-is in the `A_OPEN` payload to adbd. On the device side, `service_to_fd()` calls `is_socket_spec("vsock:2:5555")` which returns `true`, then `socket_spec_connect()` parses it:

```cpp
// socket_spec.cpp: socket_spec_connect() vsock branch
std::vector<std::string> fragments = android::base::Split(spec_str, ":");
// fragments = ["vsock", "<cid>", "<port>"]
// fragments.size() must be 2 or 3
unsigned int cid = ParseUint(fragments[1]);
unsigned int port_value = ParseUint(fragments[2]);  // or from *port param if fragments.size()==2
```

Then connects via:
```cpp
sockaddr_vm addr{};
addr.svm_family = AF_VSOCK;
addr.svm_port = port_value;
addr.svm_cid = cid;
connect(fd, &addr, sizeof(addr));
```

#### Listen format (used for adb server socket, NOT forward local)

```
vsock:<port>
```

This is `vsock:<port>` (only 2 colon-separated parts). Used with `socket_spec_listen()`:
```cpp
// Listen on any CID at the given port
addr.svm_port = port == 0 ? VMADDR_PORT_ANY : port;
addr.svm_cid = VMADDR_CID_ANY;
```

#### Host-side port resolution

`get_host_socket_spec_port()` accepts both `tcp:` and `vsock:` specs. For vsock, it expects the listen format (`vsock:<port>`, 2 fragments only).

### 3. Local vs Remote Validity

From the `adb forward` help and source analysis:

**LOCAL endpoint** (host-side listener, uses `socket_spec_listen()`):
- `tcp:<port>` — primary; port 0 means auto-assign
- `localabstract:<name>` — Linux only (abstract namespace)
- `localfilesystem:<name>` — not Windows
- `localreserved:<name>` — device only (not available when `ADB_HOST`)
- `acceptfd:<fd>` — listen only (inherited fd from socket activation)

**REMOTE endpoint** (device-side connect, sent via `A_OPEN` to adbd):
- `tcp:<port>` — connects via `socket_spec_connect()` -> loopback on device
- `localabstract:<name>` — connects via `socket_spec_connect()` -> abstract unix socket on device
- `localreserved:<name>` — connects via `socket_spec_connect()`
- `localfilesystem:<name>` — connects via `socket_spec_connect()`
- `vsock:<CID>:<port>` — connects via `socket_spec_connect()` -> AF_VSOCK (Linux only)
- `dev:<path>` — handled by `daemon_service_to_fd()` (NOT a socket spec)
- `dev-raw:<path>` — handled by `daemon_service_to_fd()` (NOT a socket spec)
- `jdwp:<pid>` — handled by `daemon_service_to_fd()` (NOT a socket spec)

Key insight: The remote endpoint dispatch in `service_to_fd()` (services.cpp:77) works in two tiers:
1. If `is_socket_spec(name)` is true -> `socket_spec_connect(&ret, name, ...)`
2. Otherwise -> `daemon_service_to_fd(name, transport)` (handles dev:, jdwp:, shell:, etc.)

For `reverse`, the spec types for LOCAL/REMOTE are reversed (help shows only tcp, localabstract, localreserved, localfilesystem for reverse).

### 4. C++ Type Structure

**There is no `SocketSpec` enum in AOSP adb.** The socket spec is always a raw string (`std::string_view`). Dispatch is done via prefix-matching at runtime:

```cpp
// socket_spec.h — the public API is all string-based:
bool is_socket_spec(std::string_view spec);
bool is_local_socket_spec(std::string_view spec);
bool socket_spec_connect(unique_fd* fd, std::string_view address, int* port,
                         std::string* serial, std::string* error);
int socket_spec_listen(std::string_view spec, std::string* error, int* resolved_tcp_port = nullptr);
bool parse_tcp_socket_spec(std::string_view spec, std::string* hostname, int* port,
                           std::string* serial, std::string* error);
int get_host_socket_spec_port(std::string_view spec, std::string* error);
```

The only structured type is for local socket namespaces:

```cpp
// socket_spec.cpp
struct LocalSocketType {
    int socket_namespace;  // ANDROID_SOCKET_NAMESPACE_*
    bool available;        // platform availability
};

static auto& kLocalSocketTypes = *new std::unordered_map<std::string, LocalSocketType>({
    { "local",          { ANDROID_SOCKET_NAMESPACE_FILESYSTEM, !ADB_WINDOWS } }, // host
    { "localreserved",  { ANDROID_SOCKET_NAMESPACE_RESERVED, !ADB_HOST } },
    { "localabstract",  { ANDROID_SOCKET_NAMESPACE_ABSTRACT, ADB_LINUX } },
    { "localfilesystem",{ ANDROID_SOCKET_NAMESPACE_FILESYSTEM, !ADB_WINDOWS } },
});
```

The `is_socket_spec()` recognizer:

```cpp
bool is_socket_spec(std::string_view spec) {
    for (const auto& it : kLocalSocketTypes) {
        std::string prefix = it.first + ":";
        if (spec.starts_with(prefix)) {
            return true;
        }
    }
    return spec.starts_with("tcp:") || spec.starts_with("acceptfd:") || spec.starts_with("vsock:");
}
```

### 5. Forward Wire Protocol (end-to-end)

For `adb forward tcp:8080 vsock:2:5555`:

1. CLI builds service string: `"forward:tcp:8080;vsock:2:5555"`
2. CLI sends to server: `host-serial:<serial>:forward:tcp:8080;vsock:2:5555` (via smartsocket)
3. Server's `handle_forward_request()` splits on `;` -> local=`"tcp:8080"`, remote=`"vsock:2:5555"`
4. Server calls `install_listener("tcp:8080", "vsock:2:5555", transport, ...)`
5. `socket_spec_listen("tcp:8080")` binds host port 8080
6. On each inbound TCP connection: `connect_to_remote(s, "vsock:2:5555")` sends `A_OPEN` with payload `"vsock:2:5555\0"` to adbd
7. adbd's `create_local_service_socket("vsock:2:5555")` -> `service_to_fd("vsock:2:5555")`
8. `is_socket_spec("vsock:2:5555")` returns true -> `socket_spec_connect()` -> AF_VSOCK connect to CID=2, port=5555

### 6. Platform Constraints

- `vsock:` — Linux only (`#if ADB_LINUX`). Returns error "vsock is only supported on linux" on other platforms.
- `localabstract:` — Linux only (abstract unix socket namespace).
- `localreserved:` — Not available on host (`!ADB_HOST`).
- `localfilesystem:` — Not available on Windows.
- `acceptfd:` — Not available on Windows; cannot be used with `socket_spec_connect` (only listen).

## Source Files

| File | URL |
|---|---|
| `socket_spec.cpp` | `android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/socket_spec.cpp` |
| `socket_spec.h` | `android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/socket_spec.h` |
| `services.cpp` | `android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/services.cpp` |
| `daemon/services.cpp` | `android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/daemon/services.cpp` |
| `adb.cpp` (handle_forward_request) | `android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/adb.cpp` |
| `adb_listeners.cpp` (install_listener) | `android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/adb_listeners.cpp` |
| `adb_utils.cpp` (forward_targets_are_valid) | `android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/adb_utils.cpp` |
| `client/commandline.cpp` (help text, forward CLI) | `android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/client/commandline.cpp` |
| `sockets.cpp` (connect_to_remote) | `android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/sockets.cpp` |

## Caveats / Notes

- **No enum exists in AOSP** — the C++ implementation is entirely string-prefix-dispatched. A Rust enum would be a *design improvement* over AOSP, not a port of an existing type.
- **`dev:` and `jdwp:` are NOT socket specs** — they are device services. They happen to be valid as forward *remote* endpoints because `service_to_fd` falls through to `daemon_service_to_fd` when `is_socket_spec` returns false. They cannot be used as forward *local* endpoints.
- **`vsock:` has two formats**: connect = `vsock:<cid>:<port>` (3 parts), listen = `vsock:<port>` (2 parts). The forward remote uses the 3-part connect format.
- **`forward_targets_are_valid()`** only validates tcp port ranges — it does NOT reject non-tcp specs. The actual validation of whether a spec is openable happens at runtime on the device.
- **`dev-raw:`** is a newer addition (feature-gated behind `kFeatureDevRaw`). The `forward_dest_is_featured()` function checks device capabilities before allowing it.
