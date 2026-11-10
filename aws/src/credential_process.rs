//! The `credential_process` credentials source, on a runtime that needs no tokio context.
//!
//! aws-config's own provider runs the process through `tokio::process`, so it is compiled out
//! here; [`CredentialProcessProvider`] is its replacement. It reads the active profile's
//! `credential_process` from the shared AWS config files exactly as the AWS CLI does — profile
//! from `AWS_PROFILE` (else `default`), files from `AWS_CONFIG_FILE` and
//! `AWS_SHARED_CREDENTIALS_FILE` (else `~/.aws/config` and `~/.aws/credentials`) — runs the command
//! through the platform shell on `async-process`, and reads the credentials it prints.
//!
//! The command's standard output is the credentials, so it is never logged or put in an error;
//! its standard error is the diagnostic, and is.

use core::fmt;
use std::time::SystemTime;

use async_process::{Command, Stdio};
use aws_config::profile;
use aws_credential_types::{
    provider::{error::CredentialsError, future, ProvideCredentials},
    Credentials,
};
use aws_runtime::env_config::file::EnvConfigFiles;
use aws_smithy_types::date_time::{DateTime, Format};
use aws_types::os_shim_internal::{Env, Fs};
use serde_json::Value;

/// The name credentials from this provider carry.
const PROVIDER_NAME: &str = "CredentialProcess";

/// The profile property that holds the command.
const PROPERTY: &str = "credential_process";

/// Credentials from the active profile's `credential_process`.
///
/// The command must exit with status 0 and print this JSON on standard output:
///
/// ```json
/// {
///     "Version": 1,
///     "AccessKeyId": "access key id",
///     "SecretAccessKey": "secret access key",
///     "SessionToken": "session token",
///     "Expiration": "2030-05-26T12:34:56Z"
/// }
/// ```
///
/// `Version` must be 1. `SessionToken` is optional. `Expiration` is an optional RFC 3339 time and
/// becomes the credentials' expiry, which is what makes the SDK's credentials cache run the
/// command again; without it the credentials never expire.
///
/// A profile without `credential_process` is not an error: the provider reports that it loaded
/// nothing, and a credentials chain moves on to its next source. A profile with one that fails —
/// a non-zero exit, output that is not this JSON, an unknown `Version` — is an error naming the
/// profile and the cause, which carries the command's standard error and never its standard
/// output.
#[derive(Clone)]
pub struct CredentialProcessProvider {
    env: Env,
    fs: Fs,
    files: EnvConfigFiles,
}

impl CredentialProcessProvider {
    /// A provider reading the process environment and the real file system.
    #[must_use]
    pub fn new() -> Self {
        Self {
            env: Env::real(),
            fs: Fs::real(),
            files: EnvConfigFiles::default(),
        }
    }

    async fn credentials(&self) -> Result<Credentials, CredentialsError> {
        let profiles = profile::load(&self.fs, &self.env, &self.files, None)
            .await
            .map_err(CredentialsError::provider_error)?;
        let name = profiles.selected_profile();
        let Some(command) = profiles
            .get_profile(name)
            .and_then(|profile| profile.get(PROPERTY))
        else {
            return Err(CredentialsError::not_loaded(format!(
                "profile `{name}` has no `{PROPERTY}`"
            )));
        };
        // The command line may carry a secret argument, so only its profile is logged.
        tracing::debug!(
            profile = name,
            "loading credentials from `credential_process`"
        );

        let output = shell(command)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|source| CredentialsError::from(Error::spawn(name, &source)))?;
        if !output.status.success() {
            return Err(Error::exit(name, output.status, &output.stderr).into());
        }
        parse(name, &output.stdout).map_err(CredentialsError::provider_error)
    }
}

impl Default for CredentialProcessProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for CredentialProcessProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialProcessProvider")
            .finish_non_exhaustive()
    }
}

impl ProvideCredentials for CredentialProcessProvider {
    fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        future::ProvideCredentials::new(self.credentials())
    }
}

/// `command` run through the platform shell, as the AWS CLI does.
fn shell(command: &str) -> Command {
    #[cfg(windows)]
    {
        use async_process::windows::CommandExt as _;
        // `cmd.exe` does its own quoting: the line goes in verbatim, wrapped in the extra pair of
        // quotes it strips, which keeps a quoted program path with spaces intact.
        let mut shell = Command::new("cmd.exe");
        shell.arg("/C").raw_arg(format!("\"{command}\""));
        shell
    }
    #[cfg(not(windows))]
    {
        let mut shell = Command::new("sh");
        shell.arg("-c").arg(command);
        shell
    }
}

/// Why a profile's `credential_process` did not yield credentials. Never holds the command's
/// standard output.
#[derive(Debug)]
struct Error {
    profile: String,
    cause: String,
}

impl Error {
    fn new(profile: &str, cause: String) -> Self {
        Self {
            profile: profile.to_owned(),
            cause,
        }
    }

    fn spawn(profile: &str, source: &std::io::Error) -> Self {
        Self::new(profile, format!("the command could not be run: {source}"))
    }

    fn exit(profile: &str, status: std::process::ExitStatus, stderr: &[u8]) -> Self {
        Self::new(
            profile,
            format!(
                "the command exited with {status}; stderr: {}",
                String::from_utf8_lossy(stderr).trim()
            ),
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{PROPERTY}` of profile `{}` failed: {}",
            self.profile, self.cause
        )
    }
}

impl std::error::Error for Error {}

impl From<Error> for CredentialsError {
    fn from(error: Error) -> Self {
        Self::provider_error(error)
    }
}

/// The credentials in a command's standard output.
///
/// A parse failure reports where and what kind, never the offending text: it may be the secret.
fn parse(profile: &str, stdout: &[u8]) -> Result<Credentials, Error> {
    let fail = |cause: String| Error::new(profile, cause);
    let document: Value = serde_json::from_slice(stdout).map_err(|error| {
        fail(format!(
            "the output is not JSON ({:?} at line {} column {})",
            error.classify(),
            error.line(),
            error.column()
        ))
    })?;
    let Value::Object(fields) = document else {
        return Err(fail("the output is not a JSON object".to_owned()));
    };
    let string = |key: &str| match fields.get(key) {
        Some(Value::String(value)) => Ok(Some(value.clone())),
        None | Some(Value::Null) => Ok(None),
        Some(_) => Err(fail(format!("`{key}` is not a string"))),
    };
    let required = |key: &str| string(key)?.ok_or_else(|| fail(format!("`{key}` is missing")));

    match fields.get("Version") {
        Some(Value::Number(version)) if version.as_u64() == Some(1) => {}
        Some(Value::Number(version)) => {
            return Err(fail(format!("`Version` is {version}, only 1 is supported")))
        }
        Some(_) => return Err(fail("`Version` is not a number".to_owned())),
        None => return Err(fail("`Version` is missing".to_owned())),
    }
    let access_key_id = required("AccessKeyId")?;
    let secret_access_key = required("SecretAccessKey")?;
    let session_token = string("SessionToken")?;
    let expiry = string("Expiration")?
        .map(|text| {
            DateTime::from_str(&text, Format::DateTimeWithOffset)
                .map_err(|error| fail(format!("`Expiration` is not an RFC 3339 time: {error}")))
                .and_then(|time| {
                    SystemTime::try_from(time)
                        .map_err(|_| fail("`Expiration` is out of range".to_owned()))
                })
        })
        .transpose()?;

    Ok(Credentials::new(
        access_key_id,
        secret_access_key,
        session_token,
        expiry,
        PROVIDER_NAME,
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt as _,
        path::{Path, PathBuf},
        time::{Duration, UNIX_EPOCH},
    };

    use aws_credential_types::provider::ProvideCredentials as _;
    use serial_test::serial;
    use tempfile::TempDir;

    use crate::sdk::load_config;

    /// Sets process environment variables and restores them on drop.
    struct Environment(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl Environment {
        fn set(vars: &[(&'static str, Option<&Path>)]) -> Self {
            let saved = vars
                .iter()
                .map(|(name, _)| (*name, std::env::var_os(name)))
                .collect();
            for (name, value) in vars {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
            Self(saved)
        }
    }

    impl Drop for Environment {
        fn drop(&mut self) {
            for (name, value) in self.0.drain(..) {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    /// Config files in a temp directory, selected through the environment.
    struct Fixture {
        dir: TempDir,
        _environment: Environment,
    }

    impl Fixture {
        /// A `[profile test]` with `config` as its body and `credentials` as the credentials
        /// file's content.
        fn new(config: &str, credentials: &str) -> Self {
            let dir = TempDir::new().expect("a temp directory");
            let config_file = dir.path().join("config");
            let credentials_file = dir.path().join("credentials");
            fs::write(&config_file, format!("[profile test]\n{config}\n")).expect("write config");
            fs::write(&credentials_file, credentials).expect("write credentials");
            let environment = Environment::set(&[
                ("AWS_CONFIG_FILE", Some(&config_file)),
                ("AWS_SHARED_CREDENTIALS_FILE", Some(&credentials_file)),
                ("AWS_PROFILE", Some(Path::new("test"))),
                ("AWS_ACCESS_KEY_ID", None),
                ("AWS_SECRET_ACCESS_KEY", None),
                ("AWS_SESSION_TOKEN", None),
                ("AWS_EC2_METADATA_DISABLED", Some(Path::new("true"))),
            ]);
            Self {
                dir,
                _environment: environment,
            }
        }

        /// An executable shell script with `body`.
        fn script(&self, name: &str, body: &str) -> PathBuf {
            let path = self.dir.path().join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
            path
        }

        /// A fixture whose profile runs a script with `body`.
        fn with_script(body: &str) -> Self {
            let fixture = Self::new("", "");
            let script = fixture.script("credentials.sh", body);
            fs::write(
                fixture.dir.path().join("config"),
                format!(
                    "[profile test]\ncredential_process = {}\n",
                    script.display()
                ),
            )
            .expect("write config");
            fixture
        }
    }

    fn assert_no_tokio_runtime() {
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "the test must run with no tokio runtime"
        );
    }

    /// An error's message and each of its sources', as a user reading the full chain sees them.
    fn chain(error: &dyn std::error::Error) -> String {
        let mut message = error.to_string();
        let mut source = error.source();
        while let Some(cause) = source {
            message.push_str(": ");
            message.push_str(&cause.to_string());
            source = cause.source();
        }
        message
    }

    async fn credentials() -> aws_credential_types::provider::Result {
        load_config()
            .await
            .credentials_provider()
            .expect("the loaded config has a credentials provider")
            .provide_credentials()
            .await
    }

    #[serial]
    #[compio::test]
    async fn a_command_printing_full_credentials_provides_them_with_their_expiry() {
        assert_no_tokio_runtime();
        let _fixture = Fixture::with_script(
            r#"cat <<'JSON'
{"Version": 1, "AccessKeyId": "AKIDCJK", "SecretAccessKey": "秘密-秘密-비밀",
 "SessionToken": "token-トークン", "Expiration": "2099-01-01T00:00:00Z"}
JSON"#,
        );

        let credentials = credentials().await.expect("the command succeeds");

        assert_eq!(credentials.access_key_id(), "AKIDCJK");
        assert_eq!(credentials.secret_access_key(), "秘密-秘密-비밀");
        assert_eq!(credentials.session_token(), Some("token-トークン"));
        assert_eq!(
            credentials.expiry(),
            Some(UNIX_EPOCH + Duration::from_hours(1_130_808))
        );
    }

    #[serial]
    #[compio::test]
    async fn a_command_printing_only_the_required_fields_provides_non_expiring_credentials() {
        assert_no_tokio_runtime();
        let _fixture = Fixture::with_script(
            r#"echo '{"Version": 1, "AccessKeyId": "AKIDMIN", "SecretAccessKey": "minimal"}'"#,
        );

        let credentials = credentials().await.expect("the command succeeds");

        assert_eq!(credentials.access_key_id(), "AKIDMIN");
        assert_eq!(credentials.session_token(), None);
        assert_eq!(credentials.expiry(), None);
    }

    #[serial]
    #[compio::test]
    async fn a_failing_command_is_an_error_with_its_stderr_and_never_its_stdout() {
        assert_no_tokio_runtime();
        let _fixture = Fixture::with_script(
            "echo 'stdout-secret-value'\necho 'the vault is sealed' >&2\nexit 3",
        );

        let error = credentials().await.expect_err("the command fails");

        let message = chain(&error);
        assert!(message.contains("profile `test`"), "{message}");
        assert!(message.contains("the vault is sealed"), "{message}");
        assert!(!message.contains("stdout-secret-value"), "{message}");
    }

    #[serial]
    #[compio::test]
    async fn malformed_output_is_an_error_that_does_not_echo_it() {
        assert_no_tokio_runtime();
        let _fixture = Fixture::with_script("echo 'AKIDLEAK SECRETLEAK {not json'");

        let error = credentials().await.expect_err("the output is not JSON");

        let message = chain(&error);
        assert!(message.contains("profile `test`"), "{message}");
        assert!(message.contains("not JSON"), "{message}");
        assert!(!message.contains("SECRETLEAK"), "{message}");
    }

    #[serial]
    #[compio::test]
    async fn an_unknown_version_is_an_error() {
        assert_no_tokio_runtime();
        let _fixture = Fixture::with_script(
            r#"echo '{"Version": 2, "AccessKeyId": "AKID", "SecretAccessKey": "secret"}'"#,
        );

        let error = credentials().await.expect_err("version 2 is unknown");

        let message = chain(&error);
        assert!(message.contains("profile `test`"), "{message}");
        assert!(message.contains("`Version` is 2"), "{message}");
    }

    #[serial]
    #[compio::test]
    async fn a_profile_without_credential_process_falls_through_to_the_chain() {
        assert_no_tokio_runtime();
        let _fixture = Fixture::new(
            "region = us-east-1",
            "[test]\naws_access_key_id = AKIDSTATIC\naws_secret_access_key = static-secret\n",
        );

        let credentials = credentials().await.expect("the static keys are found");

        assert_eq!(credentials.access_key_id(), "AKIDSTATIC");
        assert_eq!(credentials.secret_access_key(), "static-secret");
    }
}
