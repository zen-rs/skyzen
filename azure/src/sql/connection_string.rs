//! Reading an Azure SQL connection string, and the two policy decisions tiberius leaves open.
//!
//! [`tiberius::Config::from_ado_string`] parses the ADO.NET form the Azure portal hands out, but
//! tiberius is a general SQL Server driver and its defaults are the general ones. Two of them are
//! wrong for Azure SQL specifically, and both fail *quietly* if left alone — which is why reading
//! the string is a module rather than a line at the call site.
//!
//! **Encryption.** tiberius defaults a missing `Encrypt` to [`EncryptionLevel::Off`]. Azure SQL
//! refuses an unencrypted login, so the connection would fail during the handshake with an error
//! that says nothing about the missing keyword. A string that does not mention `Encrypt` is
//! therefore given [`EncryptionLevel::Required`] here. One that says `Encrypt=false` — or the
//! `DANGER_PLAINTEXT` escape hatch — is honoured, because a local SQL Server or a container is a
//! real target for this backend too; it is logged, since it is never right against Azure.
//!
//! **`Authentication=`.** The portal's Microsoft Entra ID samples carry
//! `Authentication="Active Directory Default"` and its relatives. tiberius has no Entra support and
//! its parser does not look at the keyword at all, so such a string falls through to SQL
//! authentication with an empty username and password and fails as `Login failed for user ''`.
//! That is refused here instead, naming the value and what is supported.
//!
//! The string is read twice — once by [`connection_string::AdoNetString`] for these two decisions,
//! once by tiberius for everything else. That is deliberate: it is the same parser both times, so
//! the two readings cannot disagree about where a `;` inside a quoted password ends a value.

use connection_string::AdoNetString;
use deadpool_tiberius::{tiberius::EncryptionLevel, Manager};
use skyzen_services::DbError;

/// The keyword that decides whether the connection is encrypted.
const ENCRYPT_KEY: &str = "encrypt";

/// The keyword that selects an authentication *mechanism*, as opposed to credentials.
const AUTHENTICATION_KEY: &str = "authentication";

/// The one `Authentication=` value that means what tiberius actually does.
///
/// ADO.NET spells it `SqlPassword`; the comparison is case- and space-insensitive, so
/// `Sql Password` and `SQLPASSWORD` are the same value.
const SQL_PASSWORD: &str = "sqlpassword";

/// Build the pool manager for `connection_string`, applying both policies above.
///
/// The manager is a description of how to connect, not a connection: nothing is dialled here, so a
/// string with the wrong password is accepted and fails on the first query instead.
///
/// # Errors
///
/// [`DbError::Backend`] when the string is not a well-formed ADO.NET connection string, when
/// tiberius cannot read it, or when it asks for an authentication mechanism tiberius cannot
/// perform.
pub fn manager(connection_string: &str) -> Result<Manager, DbError> {
    let keywords = keywords(connection_string)?;
    check_authentication(&keywords)?;

    let manager = Manager::from_ado_string(connection_string).map_err(|error| {
        DbError::backend_with(
            format!("the Azure SQL connection string could not be read: {error}"),
            error,
        )
    })?;

    Ok(match keywords.get(ENCRYPT_KEY) {
        None => manager.encryption(EncryptionLevel::Required),
        Some(value) => {
            tracing::debug!(
                encrypt = %value,
                "the Azure SQL connection string sets `Encrypt` itself; Azure SQL accepts only \
                 encrypted connections, so anything but a truthy value will fail to log in",
            );
            manager
        }
    })
}

/// Read the keyword/value pairs of an ADO.NET connection string, with lowercased keywords.
fn keywords(connection_string: &str) -> Result<AdoNetString, DbError> {
    connection_string.parse().map_err(|error| {
        DbError::backend(format!(
            "the Azure SQL connection string is not a valid ADO.NET connection string: {error}"
        ))
    })
}

/// Refuse an `Authentication=` value tiberius cannot honour, rather than ignoring it.
fn check_authentication(keywords: &AdoNetString) -> Result<(), DbError> {
    let Some(requested) = keywords.get(AUTHENTICATION_KEY) else {
        return Ok(());
    };

    let normalized: String = requested
        .chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    if normalized == SQL_PASSWORD {
        return Ok(());
    }

    Err(DbError::backend(format!(
        "the Azure SQL connection string asks for `Authentication={requested}`, which this backend \
         cannot perform: its driver (tiberius) supports SQL Server authentication only, so a \
         Microsoft Entra ID mechanism would be silently ignored and the login would go out as an \
         empty user. Use a SQL login — `User ID=…;Password=…`, with either no `Authentication` \
         keyword or `Authentication=SqlPassword`."
    )))
}

#[cfg(test)]
mod tests {
    use super::{check_authentication, keywords, manager, AdoNetString};
    use deadpool_tiberius::tiberius::Config;

    /// The shape of the Azure portal's "ADO.NET (SQL authentication)" sample.
    const PORTAL_SAMPLE: &str = "Server=tcp:skyzen.database.windows.net,1433;\
         Initial Catalog=skyzen;Persist Security Info=False;User ID=skyzen_app;\
         Password=s3cr3t;MultipleActiveResultSets=False;Encrypt=True;\
         TrustServerCertificate=False;Connection Timeout=30;";

    fn parsed(connection_string: &str) -> AdoNetString {
        keywords(connection_string).expect("a valid ADO.NET string")
    }

    /// The error a rejected connection string reports.
    ///
    /// `Result::expect_err` is unavailable here: it needs the success type to be `Debug`, and
    /// `Manager` — a pool builder holding a boxed closure — is not.
    fn rejection(connection_string: &str, why: &str) -> String {
        match manager(connection_string) {
            Ok(_) => panic!("{why}"),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn the_portal_sql_authentication_sample_is_accepted() {
        manager(PORTAL_SAMPLE).expect("the portal's own sample should be accepted");
        // What tiberius reads out of it, through the same parse the manager uses.
        let config = Config::from_ado_string(PORTAL_SAMPLE).expect("tiberius reads it");
        assert_eq!(config.get_addr(), "skyzen.database.windows.net:1433");
    }

    #[test]
    fn a_string_without_a_port_gets_the_default_one() {
        let config = Config::from_ado_string("Server=tcp:skyzen.database.windows.net;Encrypt=true")
            .expect("a port is optional");
        assert_eq!(config.get_addr(), "skyzen.database.windows.net:1433");
        manager("Server=tcp:skyzen.database.windows.net;Encrypt=true").expect("accepted");
    }

    #[test]
    fn a_password_holding_a_semicolon_is_one_value_and_not_two_keywords() {
        // The reason the string is parsed rather than scanned: `split(';')` would cut this in half,
        // and the encryption check would then be reading part of a password as a keyword.
        let keywords = parsed("Server=tcp:host;User ID=app;Password={p;w=d};Encrypt=true");
        assert_eq!(keywords.get("password").map(String::as_str), Some("p;w=d"));
        assert!(keywords.contains_key("encrypt"));
    }

    #[test]
    fn sql_authentication_is_accepted_however_it_is_spelled() {
        for value in [
            "SqlPassword",
            "sqlpassword",
            "SQL Password",
            " Sql Password ",
        ] {
            check_authentication(&parsed(&format!("Server=tcp:host;Authentication={value}")))
                .unwrap_or_else(|error| panic!("`{value}` should be accepted: {error}"));
        }
    }

    #[test]
    fn an_entra_id_mechanism_is_refused_by_name_rather_than_ignored() {
        // Ignoring these is the failure this check exists to prevent: the login would go out as
        // SQL authentication with an empty user, and the server would say so in those words.
        for value in [
            "Active Directory Default",
            "ActiveDirectoryPassword",
            "Active Directory Managed Identity",
            "Active Directory Interactive",
            "ActiveDirectoryServicePrincipal",
        ] {
            let message = rejection(
                &format!(
                    "Server=tcp:skyzen.database.windows.net;Database=skyzen;Authentication={value}"
                ),
                "an unsupported mechanism should be refused",
            );
            assert!(message.contains(value), "{message}");
            assert!(message.contains("SqlPassword"), "{message}");
        }
    }

    #[test]
    fn a_malformed_string_is_a_loud_error() {
        let message = rejection("Server", "`Server` alone is not a keyword/value pair");
        assert!(message.contains("ADO.NET connection string"), "{message}");
    }

    #[test]
    fn the_encryption_policy_reads_the_keyword_the_string_actually_carries() {
        // `Config` exposes no getter for its encryption level, so what is asserted here is the
        // input to the decision — whether the string names `Encrypt` — together with both branches
        // producing a usable manager.
        assert!(!parsed("Server=tcp:host;Database=skyzen").contains_key("encrypt"));
        manager("Server=tcp:host;Database=skyzen")
            .expect("a string without `Encrypt` is accepted, and is given `Required`");

        let explicit = parsed("Server=tcp:host;Database=skyzen;Encrypt=false");
        assert_eq!(explicit.get("encrypt").map(String::as_str), Some("false"));
        manager("Server=tcp:host;Database=skyzen;Encrypt=false")
            .expect("an explicit `Encrypt` is honoured, for the on-premises case");
    }
}
