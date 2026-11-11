//! The systemd unit a native deployment installs: it starts the binary, and applies
//! `[native.linux]` before every start, so a reboot restores the host's settings.

use anyhow::{Context, Result};
use askama::Template;
use std::{
    fmt,
    path::{Path, PathBuf},
};

/// Where systemd reads units an administrator installs (`systemd.unit(5)`).
pub const UNIT_DIRECTORY: &str = "/etc/systemd/system";

/// The unit of one deployed binary.
#[derive(Debug, Template)]
#[template(path = "systemd.service.tmpl", escape = "none")]
pub struct Unit {
    /// The unit's name without `.service`: the binary's.
    name: String,
    /// The project root, which the application runs in.
    working_directory: UnitPath,
    /// `skyzen deploy --host-only` against the deployed manifest.
    host_apply: Vec<UnitArg>,
    /// The binary.
    exec_start: Vec<UnitArg>,
}

impl Unit {
    /// The unit for `binary` of the project at `root`, whose host settings `skyzen` re-applies
    /// from `manifest`.
    ///
    /// # Errors
    ///
    /// Fails when a path holds a line break, which no unit file line can carry.
    pub fn new(
        name: &str,
        root: &Path,
        binary: &Path,
        skyzen: &Path,
        manifest: &Path,
    ) -> Result<Self> {
        for path in [root, binary, skyzen, manifest] {
            if path.to_string_lossy().contains(['\n', '\r']) {
                anyhow::bail!(
                    "`{}` holds a line break, which a systemd unit cannot carry; move it",
                    path.display()
                );
            }
        }
        let arg = |value: &str| UnitArg(value.to_owned());
        let path = |path: &Path| UnitArg(path.to_string_lossy().into_owned());
        Ok(Self {
            name: name.to_owned(),
            working_directory: UnitPath(root.to_path_buf()),
            host_apply: vec![
                path(skyzen),
                arg("--manifest"),
                path(manifest),
                arg("--provider"),
                arg("native"),
                arg("deploy"),
                arg("--host-only"),
            ],
            exec_start: vec![path(binary)],
        })
    }

    /// Where the unit is installed.
    pub fn path(&self) -> PathBuf {
        Path::new(UNIT_DIRECTORY).join(format!("{}.service", self.name))
    }

    /// The unit's name, as `systemctl` takes it.
    pub fn unit_name(&self) -> String {
        format!("{}.service", self.name)
    }

    /// The unit file.
    ///
    /// # Errors
    ///
    /// Fails when the template does not render.
    pub fn contents(&self) -> Result<String> {
        self.render().context("failed to render the systemd unit")
    }
}

/// A path in a setting that takes one path: specifiers (`%`) are expanded there, quotes are not.
#[derive(Debug)]
struct UnitPath(PathBuf);

impl fmt::Display for UnitPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.to_string_lossy().replace('%', "%%"))
    }
}

/// One argument of an `Exec*=` command line, quoted as `systemd.service(5)` reads it: double
/// quotes with C escapes, `%` specifiers and `$` variable references doubled.
#[derive(Debug)]
struct UnitArg(String);

impl fmt::Display for UnitArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use fmt::Write as _;

        f.write_char('"')?;
        for character in self.0.chars() {
            match character {
                '"' => f.write_str("\\\"")?,
                '\\' => f.write_str("\\\\")?,
                '%' => f.write_str("%%")?,
                '$' => f.write_str("$$")?,
                other if other.is_control() => write!(f, "\\x{:02x}", u32::from(other))?,
                other => f.write_char(other)?,
            }
        }
        f.write_char('"')
    }
}

#[cfg(test)]
mod tests {
    use super::Unit;
    use std::path::Path;

    #[test]
    fn the_unit_starts_the_binary_after_applying_the_host_settings() {
        let unit = Unit::new(
            "api",
            Path::new("/srv/api"),
            Path::new("/srv/api/target/release/api"),
            Path::new("/usr/local/bin/skyzen"),
            Path::new("/srv/api/Skyzen.toml"),
        )
        .expect("a unit");
        let contents = unit.contents().expect("renders");

        assert_eq!(unit.path(), Path::new("/etc/systemd/system/api.service"));
        assert!(
            contents.contains("WorkingDirectory=/srv/api\n"),
            "{contents}"
        );
        assert!(
            contents.contains(
                "ExecStartPre=+\"/usr/local/bin/skyzen\" \"--manifest\" \"/srv/api/Skyzen.toml\" \
                 \"--provider\" \"native\" \"deploy\" \"--host-only\"\n"
            ),
            "{contents}"
        );
        assert!(
            contents.contains("ExecStart=\"/srv/api/target/release/api\"\n"),
            "{contents}"
        );
        assert!(
            contents.contains("WantedBy=multi-user.target"),
            "{contents}"
        );
    }

    #[test]
    fn paths_are_quoted_as_systemd_reads_them() {
        let unit = Unit::new(
            "api",
            Path::new("/srv/50% \"off\""),
            Path::new("/srv/a b/$HOME\\api"),
            Path::new("/usr/bin/skyzen"),
            Path::new("/srv/Skyzen.toml"),
        )
        .expect("a unit");
        let contents = unit.contents().expect("renders");

        assert!(
            contents.contains("WorkingDirectory=/srv/50%% \"off\"\n"),
            "{contents}"
        );
        assert!(
            contents.contains("ExecStart=\"/srv/a b/$$HOME\\\\api\"\n"),
            "{contents}"
        );
        assert!(Unit::new(
            "api",
            Path::new("/srv/a\nb"),
            Path::new("/srv/api"),
            Path::new("/usr/bin/skyzen"),
            Path::new("/srv/Skyzen.toml"),
        )
        .is_err());
    }
}
