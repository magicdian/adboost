use std::fmt::Display;

use crate::RebootType;

/// PTY-allocation mode for a shell-v2 service — the mutually-exclusive final
/// argument in the AOSP `shell[,v2][,TERM=…][,pty|raw]:cmd` grammar.
///
/// `pty` and `raw` are alternatives, never both: passing both (the bogus
/// `shell,v2,pty,raw:` form) is what this type exists to make unrepresentable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ShellPtyMode {
    /// `raw`: no PTY; stdin/stdout are raw pipes. The default for a
    /// non-interactive `adb shell CMD`.
    #[default]
    Raw,
    /// `pty`: allocate a pseudo-terminal. Closing the PTY master (host-side
    /// session close / stdin EOF) makes the kernel deliver `SIGHUP` to the
    /// device-side foreground process group — the basis for "local cancel →
    /// remote process gets a signal and exits cleanly".
    Pty,
}

impl ShellPtyMode {
    /// The on-wire service-string argument for this mode (`"raw"` / `"pty"`).
    #[must_use]
    pub const fn as_arg(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Pty => "pty",
        }
    }
}

/// A typed shell-v2 service request: `shell,v2[,TERM=…][,raw|pty]:command`.
///
/// Replaces the former stringly-typed `ShellCommand(String, Vec<String>)` +
/// hardcoded `,raw:` suffix. Built with [`ShellV2Service::new`] (standard
/// defaults: no TERM, raw mode) and customized via the opt-in
/// [`with_term`](Self::with_term) / [`with_pty`](Self::with_pty) setters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellV2Service {
    /// The command to run. Empty = an interactive v2 shell (no command).
    pub command: String,
    /// Optional `TERM=…` argument (terminal type), set for interactive/PTY use.
    pub term: Option<String>,
    /// Whether to allocate a PTY (`pty`) or run raw (`raw`, the default).
    pub mode: ShellPtyMode,
}

impl ShellV2Service {
    /// A shell-v2 request for `command` with standard defaults (no TERM, raw).
    #[must_use]
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            term: None,
            mode: ShellPtyMode::Raw,
        }
    }

    /// Set the `TERM=…` argument (opt-in).
    #[must_use]
    pub fn with_term(mut self, term: impl Into<String>) -> Self {
        self.term = Some(term.into());
        self
    }

    /// Allocate a PTY for this session (opt-in; default is raw).
    #[must_use]
    pub fn with_pty(mut self) -> Self {
        self.mode = ShellPtyMode::Pty;
        self
    }
}

/// ADB commands that relates to an actual device.
pub enum ADBLocalCommand {
    /// Shell v1 (`shell:cmd`): no inner protocol framing, no exit code. An empty
    /// command is the legacy bare `shell:` form.
    ShellCommand(String),
    /// Shell v2 (`shell,v2[,TERM=…][,raw|pty]:cmd`): inner stdin/stdout/stderr/
    /// exit framing, optional PTY allocation. See [`ShellV2Service`].
    ShellV2(ShellV2Service),
    Shell,
    Exec(String),
    Sync,
    Reboot(RebootType),
    Reverse(String, String),
    ReverseRemove(String),
    ReverseRemoveAll,
    Reconnect,
    Remount,
    DisableVerity,
    EnableVerity,
    Uninstall(String, Option<String>),
    Install(u64, Option<String>),
    TcpIp(u16),
    Usb,
    Root,
    Unroot,
    /// Open a TCP connection to a port on the device (formats to "tcp:<port>")
    TcpConnect(u16),
    /// A verbatim local-service string, formatted as-is (no transformation).
    ///
    /// Used by the server frontend to transparently bridge a client's exact
    /// service string (e.g. `sync:`, `shell,v2,raw:ls`) onto the device: the
    /// server is a byte pipe for these sub-protocols, so it must forward the
    /// service string the client sent without re-encoding it.
    Raw(String),

    #[cfg(feature = "framebuffer")]
    FrameBuffer,
}

impl Display for ADBLocalCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sync => write!(f, "sync:"),
            Self::ShellCommand(command) => {
                // Shell v1: simple format for older ADB versions (no framing).
                write!(f, "shell:{command}")
            }
            Self::ShellV2(svc) => {
                // Shell v2 grammar: shell,v2[,TERM=…][,raw|pty]:cmd. The mode
                // (raw/pty) is the mutually-exclusive final argument — never both.
                write!(f, "shell,v2")?;
                if let Some(term) = &svc.term {
                    write!(f, ",TERM={term}")?;
                }
                write!(f, ",{}:{}", svc.mode.as_arg(), svc.command)
            }
            Self::Shell => match std::env::var("TERM") {
                Ok(term) => write!(f, "shell,TERM={term},raw:"),
                Err(_) => write!(f, "shell,raw:"),
            },
            Self::Exec(command) => write!(f, "exec:{command}"),
            Self::Reboot(reboot_type) => {
                write!(f, "reboot:{reboot_type}")
            }
            Self::Uninstall(package, user) => {
                write!(f, "exec:cmd package 'uninstall'")?;
                if let Some(user) = user {
                    write!(f, " --user {user}")?;
                }
                write!(f, " {package}")
            }
            Self::Install(size, user) => {
                write!(f, "exec:cmd package 'install'")?;
                if let Some(user) = user {
                    write!(f, " --user {user}")?;
                }
                write!(f, " -S {size}")
            }
            Self::Reverse(remote, local) => {
                write!(f, "reverse:forward:{remote};{local}")
            }
            Self::ReverseRemove(remote) => write!(f, "reverse:killforward:{remote}"),
            Self::ReverseRemoveAll => write!(f, "reverse:killforward-all"),
            Self::Reconnect => write!(f, "reconnect"),
            Self::Remount => write!(f, "remount:"),
            Self::DisableVerity => write!(f, "disable-verity:"),
            Self::EnableVerity => write!(f, "enable-verity:"),
            Self::TcpIp(port) => {
                write!(f, "tcpip:{port}")
            }
            Self::Usb => write!(f, "usb:"),
            Self::Root => write!(f, "root:"),
            Self::Unroot => write!(f, "unroot:"),
            Self::TcpConnect(port) => write!(f, "tcp:{port}"),
            Self::Raw(service) => write!(f, "{service}"),

            #[cfg(feature = "framebuffer")]
            Self::FrameBuffer => write!(f, "framebuffer:"),
        }
    }
}

#[test]
fn test_reverse_remove_command() {
    let command = ADBLocalCommand::ReverseRemove("tcp:7100".to_string());

    assert_eq!(command.to_string(), "reverse:killforward:tcp:7100");
}

// Regression guard: unlike `forward`, `reverse:forward:` is a genuine
// device-transport-scoped service (issued AFTER a `host:transport:` switch), so
// it must stay a plain `reverse:` local command and must NOT gain a
// `host-serial:` prefix. Locks that the forward fix did not "symmetrically"
// touch reverse.
#[test]
fn reverse_add_stays_device_scoped_no_host_prefix() {
    let command = ADBLocalCommand::Reverse("tcp:7100".to_string(), "tcp:8100".to_string());

    assert_eq!(command.to_string(), "reverse:forward:tcp:7100;tcp:8100");
}

#[test]
fn test_tcpip_command_encoding() {
    // `adb tcpip <port>` opens the `tcpip:<port>` device service.
    assert_eq!(ADBLocalCommand::TcpIp(5555).to_string(), "tcpip:5555");
    assert_eq!(ADBLocalCommand::TcpIp(0).to_string(), "tcpip:0");
}

#[test]
fn test_usb_command_encoding() {
    // `adb usb` opens the `usb:` device service (no argument).
    assert_eq!(ADBLocalCommand::Usb.to_string(), "usb:");
}

#[test]
fn test_unroot_command_encoding() {
    // `adb unroot` opens the `unroot:` device service (no argument), mirroring `root:`.
    assert_eq!(ADBLocalCommand::Unroot.to_string(), "unroot:");
}

#[test]
fn test_raw_command_is_verbatim() {
    // Raw formats the service string with no transformation — this is what lets
    // the server bridge a client's exact `sync:` / `shell,v2,raw:` verbatim.
    assert_eq!(
        ADBLocalCommand::Raw("sync:".to_string()).to_string(),
        "sync:"
    );
    assert_eq!(
        ADBLocalCommand::Raw("shell,v2,TERM=xterm,raw:ls".to_string()).to_string(),
        "shell,v2,TERM=xterm,raw:ls"
    );
}

#[test]
fn shell_v1_renders_bare_shell_service() {
    // v1 has no inner framing and no args: `shell:<cmd>`.
    assert_eq!(
        ADBLocalCommand::ShellCommand("echo hi".to_string()).to_string(),
        "shell:echo hi"
    );
}

#[test]
fn shell_v2_default_is_raw_no_term() {
    // Standard default: v2, raw mode, no TERM.
    assert_eq!(
        ADBLocalCommand::ShellV2(ShellV2Service::new("ls")).to_string(),
        "shell,v2,raw:ls",
        "default v2 service must render as shell,v2,raw:<cmd>"
    );
}

#[test]
fn shell_v2_with_term_inserts_term_before_mode() {
    assert_eq!(
        ADBLocalCommand::ShellV2(ShellV2Service::new("top").with_term("xterm-256color"))
            .to_string(),
        "shell,v2,TERM=xterm-256color,raw:top",
        "TERM must appear between v2 and the raw/pty mode segment"
    );
}

#[test]
fn shell_v2_pty_renders_pty_not_pty_raw() {
    // The whole point of the typed mode: PTY renders `…,pty:` — NOT the bogus
    // `…,pty,raw:` the old stringly-typed path would have produced if "pty" were
    // pushed into the args vec ahead of the hardcoded ",raw:" suffix.
    let svc = ShellV2Service::new("tcpdump -w -").with_pty();
    assert_eq!(
        ADBLocalCommand::ShellV2(svc).to_string(),
        "shell,v2,pty:tcpdump -w -",
        "pty mode must render shell,v2,pty:<cmd> (pty and raw are mutually exclusive)"
    );
}

#[test]
fn shell_v2_pty_with_term() {
    let svc = ShellV2Service::new("").with_term("xterm").with_pty();
    assert_eq!(
        ADBLocalCommand::ShellV2(svc).to_string(),
        "shell,v2,TERM=xterm,pty:",
        "an interactive PTY shell with TERM and no command renders trailing colon"
    );
}
