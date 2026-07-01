# Research: Native AOSP `adb forward` / `adb reverse` CLI argument order

- **Query**: For `adb forward <a> <b>` and `adb reverse <a> <b>`, which arg is LOCAL (host) and which is REMOTE (device)? Give exact help text + commandline.cpp wire mapping. Decide arg order for adboost_cli subcommands.
- **Scope**: external (AOSP source) + internal (adboost mapping cross-check)
- **Date**: 2026-07-01

## TL;DR (the two canonical CLI orders)

| CLI command | Canonical arg order | Wire string emitted |
|---|---|---|
| `adb forward` | `adb forward LOCAL REMOTE` (**local-first**) | `host:forward:<LOCAL>;<REMOTE>` |
| `adb reverse` | `adb reverse REMOTE LOCAL` (**remote-first**) | `reverse:forward:<REMOTE>;<LOCAL>` |

**The key asymmetry**: `forward` is **local-first**, `reverse` is **remote-first**. They are MIRRORS of each other. In BOTH cases the wire string is `…forward:<argv[0]>;<argv[1]>` — the first CLI arg always lands in the first wire field. Only the *meaning* of the first arg flips (LOCAL for forward, REMOTE for reverse), because reverse tunnels in the opposite direction.

## Findings

### External References — AOSP `packages/modules/adb/client/commandline.cpp` (main branch)

Fetched from
`https://android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/client/commandline.cpp`

#### 1. Help text (exact lines)

```
 forward [--no-rebind] LOCAL REMOTE
     forward socket connection using:
       tcp:<port> (<local> may be "tcp:0" to pick any open port)
       localabstract:<unix domain socket name>
       ...
       jdwp:<process pid> (remote only)
       vsock:<CID>:<port> (remote only)
 forward --remove LOCAL   remove specific forward socket connection
 ...
 reverse [--no-rebind] REMOTE LOCAL
     reverse socket connection using:
       tcp:<port> (<remote> may be "tcp:0" to pick any open port)
       localabstract:<unix domain socket name>
       ...
 reverse --remove REMOTE  remove specific reverse socket connection
 reverse --remove-all     remove all reverse socket connections from device
```

- `forward` help literally reads `forward [--no-rebind] LOCAL REMOTE` → **arg1 = LOCAL, arg2 = REMOTE**.
- `reverse` help literally reads `reverse [--no-rebind] REMOTE LOCAL` → **arg1 = REMOTE, arg2 = LOCAL**.
- `forward --remove` takes `LOCAL`; `reverse --remove` takes `REMOTE` (the removal key is the first-arg / bind side of each).

#### 2. Command dispatch & wire mapping (single shared handler)

`forward` and `reverse` are handled by the **same** code block; the only branch is the `host_prefix`:

```cpp
} else if (!strcmp(argv[0], "forward") || !strcmp(argv[0], "reverse")) {
    bool reverse = !strcmp(argv[0], "reverse");
    --argc; ++argv;                       // drop the "forward"/"reverse" word
    ...
    std::string host_prefix;
    if (reverse) {
        host_prefix = "reverse:";         // reverse -> reverse:...
    } else {
        host_prefix = "host:";            // forward -> host:...
    }

    std::string cmd, error_message;
    if (strcmp(argv[0], "--list") == 0) {
        return adb_query_command(host_prefix + "list-forward");
    } else if (strcmp(argv[0], "--remove-all") == 0) {
        cmd = "killforward-all";
    } else if (strcmp(argv[0], "--remove") == 0) {
        // forward --remove <local>   (reverse --remove <remote>)
        cmd = std::string("killforward:") + argv[1];
    } else if (strcmp(argv[0], "--no-rebind") == 0) {
        // forward --no-rebind <local> <remote>
        cmd = std::string("forward:norebind:") + argv[1] + ";" + argv[2];
    } else {
        // forward <local> <remote>
        if (argc != 2) error_exit("forward takes two arguments");
        ...
        cmd = std::string("forward:") + argv[0] + ";" + argv[1];
    }
    ...
    unique_fd fd(adb_connect(nullptr, host_prefix + cmd, ...));
```

So the fully-assembled wire strings are:

- **forward**: `host_prefix="host:"`, `cmd="forward:"+argv[0]+";"+argv[1]`
  → **`host:forward:<argv0=LOCAL>;<argv1=REMOTE>`**
- **reverse**: `host_prefix="reverse:"`, `cmd="forward:"+argv[0]+";"+argv[1]`
  → **`reverse:forward:<argv0=REMOTE>;<argv1=LOCAL>`**

Note the mechanical consequence: the shared handler ALWAYS writes `forward:<argv[0]>;<argv[1]>`. The wire protocol's `<first>;<second>` fields are "bind side ; dial side". For forward the bind side is on the host (LOCAL) and dial side on the device (REMOTE); for reverse the bind side is on the device (REMOTE) and dial side on the host (LOCAL). The CLI help simply relabels the two positional args to match, which is why the human-facing order flips.

### Cross-check: how adboost already maps this (internal)

adboost's protocol layer already encodes the correct AOSP wire order:

| File | Line | Fact |
|---|---|---|
| `adboost/src/models/adb_host_command.rs` | 83-87 | `Forward { selector, local, remote } => "{prefix}forward:{local};{remote}"` — local-first, matches native `host:forward:LOCAL;REMOTE`. Test at L133-139 (`forward_renders_local_then_remote`) locks `forward:tcp:1111;tcp:2222` (local;remote). |
| `adboost/src/models/adb_local_command.rs` | 154-155 | `Reverse(remote, local) => "reverse:forward:{remote};{local}"` — remote-first, matches native `reverse:forward:REMOTE;LOCAL`. Test at L191-194 locks `reverse:forward:tcp:7100;tcp:8100` (remote;local). |

Library method signatures (the API adboost_cli calls):

| File | Line | Signature |
|---|---|---|
| `adboost/src/proxy/device_commands/forward.rs` | 17 | `pub async fn forward(&mut self, remote: String, local: String)` — **NOTE: param order is `(remote, local)`**, and internally builds `Forward { local, remote }`. |
| `adboost/src/proxy/device_commands/reverse.rs` | 9 | `pub async fn reverse(&mut self, remote: String, local: String)` — param order `(remote, local)`, builds `Reverse(remote, local)`. |

So BOTH library methods take arguments in `(remote, local)` order.

### Cross-check: current adboost_cli handler (the reported bug site)

| File | Line | Current call |
|---|---|---|
| `adboost_cli/src/handlers/local_commands.rs` | 35 | `ForwardCommand::Add { local, remote } => device.forward(local, remote)` |
| `adboost_cli/src/handlers/local_commands.rs` | 40 | `ReverseCommand::Add { remote, local } => device.reverse(remote, local)` |

CLI arg model (`adboost_cli/src/models/local.rs`):
- L37 `ForwardCommand::Add { local: String, remote: String }`
- L47 `ReverseCommand::Add { remote: String, local: String }`

**Observation (fact, not a fix):** `device.forward`'s signature is `forward(remote, local)` (forward.rs:17), but the CLI calls `device.forward(local, remote)` — the two positional arguments are passed in the opposite order to what the method's parameter names declare. `device.reverse`'s signature is `reverse(remote, local)` and the CLI calls `device.reverse(remote, local)`, which matches by name. (Whether the forward call is actually wrong depends on the intended CLI positional order — see canonical orders above. This file only documents the native reference and the current wiring; the decision belongs to the implementer.)

## Caveats / Not Found

- Help text and mapping are from the AOSP `main` branch of `packages/modules/adb`; historical releases have used the same LOCAL/REMOTE vs REMOTE/LOCAL ordering for years, but exact line numbers vary by revision.
- The `--no-rebind`, `--remove`, `--remove-all`, `--list` sub-flags follow the same first-arg semantics (forward's key is LOCAL, reverse's key is REMOTE); documented above but not the focus of the task.
- This research does not assert the fix; it establishes the native canonical orders (`forward LOCAL REMOTE`, `reverse REMOTE LOCAL`) and the exact wire strings so the implementer can align adboost_cli.
