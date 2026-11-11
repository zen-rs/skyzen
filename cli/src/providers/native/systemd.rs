//! The systemd unit a native deployment installs, and the environment file it reads.
//!
//! The unit starts the binary as a transient, unprivileged user (`DynamicUser=yes`) inside the
//! usual sandbox, with only the capabilities [`Sandbox`] derives from `[native.server]`, and
//! applies `[native.linux]` as root before every start, so a reboot restores the host's settings.
//! The application's variables — `[[secret]]` values and native wiring, as
//! [`crate::environment`] resolves them — are delivered in an owner-only `EnvironmentFile`.

use crate::environment::{self, ResolvedVariables};
use anyhow::{Context, Result};
use askama::Template;
use secrecy::SecretString;
use skyzen_manifest::{ListenSetting, NativeServerSection};
use std::{
    collections::BTreeSet,
    fmt::{self, Write as _},
    path::{Path, PathBuf},
};

/// Where systemd reads units an administrator installs (`systemd.unit(5)`).
pub const UNIT_DIRECTORY: &str = "/etc/systemd/system";

/// Where a deployment keeps each service's environment file.
pub const ENVIRONMENT_DIRECTORY: &str = "/etc/skyzen";

/// Where systemd puts a unit's `RuntimeDirectory=` (`systemd.exec(5)`).
const RUNTIME_ROOT: &str = "/run";

/// The paths of one deployment.
#[derive(Debug)]
pub struct Paths<'a> {
    /// The project root, which the application runs in.
    pub root: &'a Path,
    /// The release binary.
    pub binary: &'a Path,
    /// The `skyzen` executable that re-applies the host settings.
    pub skyzen: &'a Path,
    /// The deployed manifest, absolute.
    pub manifest: &'a Path,
}

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
    /// The environment file the variables are delivered in.
    environment_file: UnitPath,
    /// The serving settings, as the runtime's own variables.
    environment: Vec<UnitAssignment>,
    /// The privileges the service runs with.
    sandbox: Sandbox,
}

impl Unit {
    /// The unit for the binary at `paths`, serving as `sandbox` decides.
    ///
    /// # Errors
    ///
    /// Fails when a path holds a line break, which no unit file line can carry.
    pub fn new(name: &str, paths: &Paths<'_>, sandbox: Sandbox) -> Result<Self> {
        for path in [paths.root, paths.binary, paths.skyzen, paths.manifest] {
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
            working_directory: UnitPath(paths.root.to_path_buf()),
            host_apply: vec![
                path(paths.skyzen),
                arg("--manifest"),
                path(paths.manifest),
                arg("--provider"),
                arg("native"),
                arg("deploy"),
                arg("--host-only"),
            ],
            exec_start: vec![path(paths.binary)],
            environment_file: UnitPath(environment_file(name)),
            environment: sandbox.environment.clone(),
            sandbox,
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

    /// The environment file the unit reads.
    pub fn environment_file(&self) -> &Path {
        &self.environment_file.0
    }
}

/// Where the environment file of service `name` goes.
fn environment_file(name: &str) -> PathBuf {
    Path::new(ENVIRONMENT_DIRECTORY).join(format!("{name}.env"))
}

/// A capability the service is granted, ambient and in its bounding set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    /// Binding a port below `net.ipv4.ip_unprivileged_port_start`.
    NetBindService,
    /// `SO_PREFER_BUSY_POLL`, and raising `SO_BUSY_POLL_BUDGET` above the socket's (initially 0)
    /// budget (`sk_setsockopt`, `net/core/sock.c`).
    NetAdmin,
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NetBindService => "CAP_NET_BIND_SERVICE",
            Self::NetAdmin => "CAP_NET_ADMIN",
        })
    }
}

/// The privileges and writable places a native service runs with, derived from what the
/// manifest configures.
///
/// The service always runs as a `DynamicUser=` in a read-only system (`ProtectSystem=strict`,
/// `ProtectHome=read-only`, `PrivateTmp=`, `NoNewPrivileges=`), with `LimitMEMLOCK=infinity` so
/// zero-copy sends are not cut short by `RLIMIT_MEMLOCK`; what varies is here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    /// The capabilities granted; empty drops every one.
    pub capabilities: BTreeSet<Capability>,
    /// Whether the service gets `/var/lib/<name>` (`StateDirectory=`): a native SQLite database
    /// needs somewhere writable.
    pub state_directory: bool,
    /// Whether the service gets `/run/<name>` (`RuntimeDirectory=`), where it listens on a unix
    /// socket.
    pub runtime_directory: bool,
    /// The serving settings, as the runtime's own variables.
    environment: Vec<UnitAssignment>,
}

impl Sandbox {
    /// The sandbox of service `name` serving as `server` says, on a host whose unprivileged ports
    /// start at `unprivileged_port_start` (`net.ipv4.ip_unprivileged_port_start`); `state` is
    /// whether the application keeps a native SQLite database.
    ///
    /// # Errors
    ///
    /// `[native.server]` names no listen address, a unix socket outside the service's runtime
    /// directory, or refines busy polling that is off.
    pub fn of(
        name: &str,
        server: Option<&NativeServerSection>,
        unprivileged_port_start: u16,
        state: bool,
    ) -> Result<Self> {
        let default = NativeServerSection::default();
        let server = server.unwrap_or(&default);
        let mut capabilities = BTreeSet::new();
        let mut runtime_directory = false;
        let address = match &server.listen {
            None => anyhow::bail!(
                "[native.server] listen is not set; a deployed service would listen on an \
                 ephemeral localhost port nothing can find. Set it to an address, \
                 `listen = \"0.0.0.0:8080\"`, or a socket, `listen = {{ unix = \"/run/{name}/{name}.sock\" }}`"
            ),
            Some(ListenSetting::Tcp(address)) => {
                if address.port() != 0 && address.port() < unprivileged_port_start {
                    capabilities.insert(Capability::NetBindService);
                }
                address.to_string()
            }
            Some(ListenSetting::Unix { unix }) => {
                let directory = Path::new(RUNTIME_ROOT).join(name);
                if unix.parent() != Some(directory.as_path()) {
                    anyhow::bail!(
                        "[native.server] listen: the socket {} must be directly in {}, the one \
                         directory the service can write under /run",
                        unix.display(),
                        directory.display()
                    );
                }
                runtime_directory = true;
                format!(
                    "{}{}",
                    skyzen_core::UNIX_ADDRESS_PREFIX,
                    unix.to_string_lossy()
                )
            }
        };
        let mut environment = vec![UnitAssignment::new(skyzen_core::ADDRESS_ENV, address)];
        match server.busy_poll {
            Some(timeout) => {
                environment.push(UnitAssignment::new(
                    skyzen_core::BUSY_POLL_ENV,
                    timeout.get().to_string(),
                ));
                if server.busy_poll_prefer {
                    capabilities.insert(Capability::NetAdmin);
                    environment.push(UnitAssignment::new(
                        skyzen_core::BUSY_POLL_PREFER_ENV,
                        true.to_string(),
                    ));
                }
                if let Some(budget) = server.busy_poll_budget {
                    capabilities.insert(Capability::NetAdmin);
                    environment.push(UnitAssignment::new(
                        skyzen_core::BUSY_POLL_BUDGET_ENV,
                        budget.to_string(),
                    ));
                }
            }
            None if server.busy_poll_prefer || server.busy_poll_budget.is_some() => {
                anyhow::bail!(
                    "[native.server] busy_poll_prefer and busy_poll_budget refine busy polling, \
                     which is off: set busy_poll"
                )
            }
            None => {}
        }
        Ok(Self {
            capabilities,
            state_directory: state,
            runtime_directory,
            environment,
        })
    }

    /// The capabilities as `AmbientCapabilities=` and `CapabilityBoundingSet=` list them.
    fn capability_list(&self) -> String {
        self.capabilities
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The environment file holding the application's variables.
///
/// # Errors
///
/// Fails when the template does not render.
pub fn environment_file_contents(variables: &ResolvedVariables) -> Result<SecretString> {
    let entries = variables
        .iter()
        .map(|(name, value)| EnvironmentEntry {
            name: name.to_string(),
            value: EnvFileValue(environment::expose(value)),
        })
        .collect();
    EnvironmentFileTemplate { entries }
        .render()
        .map(SecretString::from)
        .context("failed to render the service's environment file")
}

#[derive(Template)]
#[template(path = "systemd.env.tmpl", escape = "none")]
struct EnvironmentFileTemplate<'a> {
    entries: Vec<EnvironmentEntry<'a>>,
}

struct EnvironmentEntry<'a> {
    name: String,
    value: EnvFileValue<'a>,
}

/// A value in an `EnvironmentFile=`, double-quoted as systemd's parser reads it back exactly
/// (`parse_env_file_internal`, `src/basic/env-file.c`): inside double quotes a backslash escapes
/// `"`, `\`, `` ` `` and `$`, and every other character, a line break included, stands for itself.
struct EnvFileValue<'a>(&'a str);

impl fmt::Display for EnvFileValue<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_char('"')?;
        for character in self.0.chars() {
            if matches!(character, '"' | '\\' | '`' | '$') {
                f.write_char('\\')?;
            }
            f.write_char(character)?;
        }
        f.write_char('"')
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

/// One `NAME=value` of an `Environment=` setting, quoted as `systemd.exec(5)` reads it: double
/// quotes with C escapes and `%` specifiers doubled; `$` means nothing there.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UnitAssignment {
    name: &'static str,
    value: String,
}

impl UnitAssignment {
    const fn new(name: &'static str, value: String) -> Self {
        Self { name, value }
    }
}

impl fmt::Display for UnitAssignment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_char('"')?;
        write_escaped(f, self.name, false)?;
        f.write_char('=')?;
        write_escaped(f, &self.value, false)?;
        f.write_char('"')
    }
}

/// One argument of an `Exec*=` command line, quoted as `systemd.service(5)` reads it: double
/// quotes with C escapes, `%` specifiers and `$` variable references doubled.
#[derive(Debug)]
struct UnitArg(String);

impl fmt::Display for UnitArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_char('"')?;
        write_escaped(f, &self.0, true)?;
        f.write_char('"')
    }
}

/// Write `value` with systemd's C escapes inside double quotes, `%` doubled, and `$` doubled when
/// the setting expands variables.
fn write_escaped(f: &mut fmt::Formatter<'_>, value: &str, dollar: bool) -> fmt::Result {
    for character in value.chars() {
        match character {
            '"' => f.write_str("\\\"")?,
            '\\' => f.write_str("\\\\")?,
            '%' => f.write_str("%%")?,
            '$' if dollar => f.write_str("$$")?,
            other if other.is_control() => write!(f, "\\x{:02x}", u32::from(other))?,
            other => f.write_char(other)?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Capability, EnvFileValue, Paths, Sandbox, Unit};
    use skyzen_manifest::{ListenSetting, NativeServerSection};
    use std::{num::NonZeroU16, path::Path};

    /// The unprivileged port floor every kernel defaults to (`inet_init_net`,
    /// `net/ipv4/af_inet.c`).
    const PORT_FLOOR: u16 = 1024;

    fn server(listen: &str) -> NativeServerSection {
        NativeServerSection {
            listen: Some(ListenSetting::Tcp(listen.parse().expect("an address"))),
            ..NativeServerSection::default()
        }
    }

    fn paths() -> Paths<'static> {
        Paths {
            root: Path::new("/srv/api"),
            binary: Path::new("/srv/api/target/release/api"),
            skyzen: Path::new("/usr/local/bin/skyzen"),
            manifest: Path::new("/srv/api/Skyzen.toml"),
        }
    }

    #[test]
    fn the_unit_runs_the_binary_unprivileged_after_applying_the_host_settings() {
        let sandbox =
            Sandbox::of("api", Some(&server("0.0.0.0:8080")), PORT_FLOOR, false).expect("sandbox");
        let unit = Unit::new("api", &paths(), sandbox).expect("a unit");
        let contents = unit.contents().expect("renders");

        assert_eq!(unit.path(), Path::new("/etc/systemd/system/api.service"));
        assert_eq!(unit.environment_file(), Path::new("/etc/skyzen/api.env"));
        for line in [
            "WorkingDirectory=/srv/api\n",
            "ExecStartPre=+\"/usr/local/bin/skyzen\" \"--manifest\" \"/srv/api/Skyzen.toml\" \
             \"--provider\" \"native\" \"deploy\" \"--host-only\"\n",
            "ExecStart=\"/srv/api/target/release/api\"\n",
            "DynamicUser=yes\n",
            "EnvironmentFile=/etc/skyzen/api.env\n",
            "Environment=\"SKYZEN_ADDRESS=0.0.0.0:8080\"\n",
            "CapabilityBoundingSet=\n",
            "LimitMEMLOCK=infinity\n",
            "NoNewPrivileges=yes\n",
            "ProtectSystem=strict\n",
            "ProtectHome=read-only\n",
            "PrivateTmp=yes\n",
        ] {
            assert!(contents.contains(line), "{line:?} missing from\n{contents}");
        }
        for absent in ["AmbientCapabilities", "StateDirectory", "RuntimeDirectory"] {
            assert!(!contents.contains(absent), "{absent} in\n{contents}");
        }
    }

    #[test]
    fn capabilities_follow_what_the_server_is_configured_to_do() {
        let low = Sandbox::of("api", Some(&server("[::]:443")), PORT_FLOOR, false).expect("ok");
        assert_eq!(
            low.capabilities.into_iter().collect::<Vec<_>>(),
            [Capability::NetBindService]
        );
        let lowered_floor =
            Sandbox::of("api", Some(&server("[::]:443")), 80, false).expect("sandbox");
        assert!(
            lowered_floor.capabilities.is_empty(),
            "443 is unprivileged there"
        );

        let mut polling = server("0.0.0.0:8080");
        polling.busy_poll = Some(50.try_into().expect("a timeout"));
        let plain = Sandbox::of("api", Some(&polling), PORT_FLOOR, false).expect("sandbox");
        assert!(
            plain.capabilities.is_empty(),
            "SO_BUSY_POLL needs no capability"
        );

        polling.busy_poll_budget = NonZeroU16::new(64);
        let budget = Sandbox::of("api", Some(&polling), PORT_FLOOR, true).expect("sandbox");
        assert_eq!(
            budget.capabilities.iter().copied().collect::<Vec<_>>(),
            [Capability::NetAdmin]
        );
        let contents = Unit::new("api", &paths(), budget)
            .expect("a unit")
            .contents()
            .expect("renders");
        for line in [
            "AmbientCapabilities=CAP_NET_ADMIN\n",
            "CapabilityBoundingSet=CAP_NET_ADMIN\n",
            "Environment=\"SKYZEN_BUSY_POLL=50\"\n",
            "Environment=\"SKYZEN_BUSY_POLL_BUDGET=64\"\n",
            "StateDirectory=api\n",
        ] {
            assert!(contents.contains(line), "{line:?} missing from\n{contents}");
        }

        let mut off = server("0.0.0.0:8080");
        off.busy_poll_prefer = true;
        assert!(Sandbox::of("api", Some(&off), PORT_FLOOR, false).is_err());
    }

    #[test]
    fn a_unix_socket_lives_in_the_runtime_directory() {
        let unix = |path: &str| NativeServerSection {
            listen: Some(ListenSetting::Unix { unix: path.into() }),
            ..NativeServerSection::default()
        };
        let sandbox =
            Sandbox::of("api", Some(&unix("/run/api/api.sock")), PORT_FLOOR, false).expect("ok");
        assert!(sandbox.runtime_directory);
        let contents = Unit::new("api", &paths(), sandbox)
            .expect("a unit")
            .contents()
            .expect("renders");
        assert!(contents.contains("Environment=\"SKYZEN_ADDRESS=unix:/run/api/api.sock\"\n"));
        assert!(contents.contains("RuntimeDirectory=api\n"), "{contents}");

        assert!(Sandbox::of("api", Some(&unix("/tmp/api.sock")), PORT_FLOOR, false).is_err());
        assert!(
            Sandbox::of("api", None, PORT_FLOOR, false).is_err(),
            "no listen address"
        );
    }

    #[test]
    fn paths_are_quoted_as_systemd_reads_them() {
        let sandbox =
            Sandbox::of("api", Some(&server("0.0.0.0:8080")), PORT_FLOOR, false).expect("ok");
        let unit = Unit::new(
            "api",
            &Paths {
                root: Path::new("/srv/50% \"off\""),
                binary: Path::new("/srv/a b/$HOME\\api"),
                skyzen: Path::new("/usr/bin/skyzen"),
                manifest: Path::new("/srv/Skyzen.toml"),
            },
            sandbox.clone(),
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
            &Paths {
                root: Path::new("/srv/a\nb"),
                ..paths()
            },
            sandbox,
        )
        .is_err());
    }

    #[test]
    fn an_environment_file_value_reads_back_exactly() {
        let quoted = |value: &str| EnvFileValue(value).to_string();
        assert_eq!(quoted("sk_live_123"), "\"sk_live_123\"");
        assert_eq!(quoted("a\"b\\c`d$e"), "\"a\\\"b\\\\c\\`d\\$e\"");
        assert_eq!(quoted("two\nlines 'x'"), "\"two\nlines 'x'\"");
    }
}
