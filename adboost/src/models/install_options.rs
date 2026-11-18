/// Options controlling an `adb install` (streamed `cmd package install`) request.
///
/// Every flag is opt-in: [`InstallOptions::default`] renders the historical
/// `install` behavior (no replacement, no permission grant), which keeps the
/// legacy [`ADBProxyDevice::install`](crate::proxy::ADBProxyDevice::install)
/// semantics unchanged.
///
/// The rendered order on the wire is stable — `-r -g -t -d --user <id>` — so
/// callers and tests can rely on it.
// Independent `adb install` switches are the whole point of this type; a bitfield
// would obscure the public contract and its documented wire order.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallOptions {
    /// `-r`: replace an already installed package (upgrade / overwrite install).
    pub replace: bool,
    /// `-g`: grant all runtime permissions declared by the package.
    pub grant: bool,
    /// `-t`: allow installing packages marked as test-only.
    pub test_only: bool,
    /// `-d`: allow a version downgrade.
    pub downgrade: bool,
    /// `--user <id>`: install for a specific Android user instead of all users.
    pub user: Option<String>,
}

impl InstallOptions {
    /// Append the option flags to a service string in the stable wire order.
    ///
    /// Shared by both [`ADBLocalCommand::Install`] compatibility rendering and
    /// [`ADBLocalCommand::InstallWithOptions`], so the two can never drift.
    ///
    /// [`ADBLocalCommand::Install`]: crate::models::ADBLocalCommand::Install
    /// [`ADBLocalCommand::InstallWithOptions`]: crate::models::ADBLocalCommand::InstallWithOptions
    pub(crate) fn write_flags(&self, out: &mut impl std::fmt::Write) -> std::fmt::Result {
        if self.replace {
            out.write_str(" -r")?;
        }
        if self.grant {
            out.write_str(" -g")?;
        }
        if self.test_only {
            out.write_str(" -t")?;
        }
        if self.downgrade {
            out.write_str(" -d")?;
        }
        if let Some(user) = &self.user {
            write!(out, " --user {user}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::InstallOptions;

    fn flags(options: &InstallOptions) -> String {
        let mut out = String::new();
        options
            .write_flags(&mut out)
            .expect("writing into a String cannot fail");
        out
    }

    #[test]
    fn default_renders_no_flags() {
        assert_eq!(
            flags(&InstallOptions::default()),
            "",
            "default options must not alter the historical install service string"
        );
    }

    #[test]
    fn flags_render_in_stable_order() {
        let options = InstallOptions {
            replace: true,
            grant: true,
            test_only: true,
            downgrade: true,
            user: Some("10".to_string()),
        };
        assert_eq!(
            flags(&options),
            " -r -g -t -d --user 10",
            "flag order is part of the wire contract"
        );
    }

    #[test]
    fn only_selected_flags_are_rendered() {
        let options = InstallOptions {
            replace: true,
            grant: true,
            ..InstallOptions::default()
        };
        assert_eq!(flags(&options), " -r -g");
    }

    #[test]
    fn user_alone_is_rendered() {
        let options = InstallOptions {
            user: Some("0".to_string()),
            ..InstallOptions::default()
        };
        assert_eq!(flags(&options), " --user 0");
    }
}
