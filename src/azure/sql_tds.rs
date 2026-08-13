//! Live T-SQL against the Azure SQL **database engine** — azpect's only
//! non-REST data plane. Everything here is loud and opt-out by design:
//!
//! - The UI marks every feature backed by this module with a ⚠ and shows the
//!   exact statement being run; `sql_live_queries = false` in config.toml
//!   disables the module wholesale (checked by the callers *and* enforced
//!   here, defense-in-depth).
//! - Only the fixed, read-only `SELECT`s below are ever sent — no user input
//!   is interpolated into SQL.
//!
//! Two queries serve the SQL views:
//!
//! - [`fetch_db_users`] — `sys.database_principals`: the database's actual
//!   user list, merged into the audit roll-up so principals with *zero* audit
//!   activity become visible (the audit trail alone has survivorship bias).
//! - [`fetch_sessions`] — `sys.dm_exec_sessions` (+ connections): who is
//!   connected *right now*, since when, and how long idle — the half of the
//!   "can I delete this user" question history can't answer.
//!
//! ## Failure modes worth friendly errors
//!
//! Unlike the REST planes, TDS commonly fails for environmental reasons:
//! a firewall / private endpoint that doesn't admit this machine (connect
//! timeout), or a signed-in identity that isn't a user in the database
//! ("Login failed"). [`friendly_tds_error`] rewrites both into actionable
//! text. Seeing *all* sessions additionally needs `VIEW DATABASE STATE`-ish
//! permission; a plain user sees only its own session.

use anyhow::{anyhow, Context};
use chrono::{DateTime, NaiveDateTime, Utc};
use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use crate::azure::auth::{AzureAuth, SCOPE_SQL};

/// Upper bound on connect + login + query. TDS to an unreachable server
/// (firewall DROP) would otherwise hang for the OS's TCP timeout.
pub const TDS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// The database-users statement, verbatim (shown in the UI next to the KQL).
/// `principal_id > 4` skips the fixed system principals (`dbo`, `guest`,
/// `INFORMATION_SCHEMA`, `sys`); the type filter keeps users / external
/// (Entra) users and groups and drops roles.
pub const DB_USERS_SQL: &str = "SELECT name, type_desc, authentication_type_desc, create_date \
FROM sys.database_principals \
WHERE type IN ('S','U','E','X','G') AND principal_id > 4 \
ORDER BY name";

/// The open-sessions statement, verbatim (shown in the UI). User processes
/// only; the connections join carries the client IP.
pub const SESSIONS_SQL: &str = "SELECT s.session_id, s.login_name, s.status, s.login_time, \
s.last_request_end_time, s.host_name, s.program_name, c.client_net_address, \
DB_NAME(s.database_id) AS database_name \
FROM sys.dm_exec_sessions s \
LEFT JOIN sys.dm_exec_connections c ON c.session_id = s.session_id \
WHERE s.is_user_process = 1 \
ORDER BY s.login_name, s.login_time";

/// One row of `sys.database_principals` — a user that *exists*, whether or
/// not the audit log has ever seen it.
#[derive(Clone, Debug)]
pub struct DbUser {
    pub name: String,
    /// `type_desc`: `SQL_USER`, `EXTERNAL_USER`, `EXTERNAL_GROUP`, …
    pub kind: String,
    /// `authentication_type_desc`: `DATABASE`, `EXTERNAL`, `NONE`, …
    pub auth: String,
    pub created: Option<DateTime<Utc>>,
}

impl DbUser {
    /// Short display tag for the roll-up's APPS column slot.
    pub fn kind_tag(&self) -> &'static str {
        match self.kind.as_str() {
            "SQL_USER" => "sql user",
            "EXTERNAL_USER" => "entra user",
            "EXTERNAL_GROUP" => "entra group",
            "WINDOWS_USER" => "windows user",
            "WINDOWS_GROUP" => "windows group",
            _ => "user",
        }
    }
}

/// One row of `sys.dm_exec_sessions` (user processes only).
#[derive(Clone, Debug)]
pub struct DbSession {
    pub id: i16,
    pub login: String,
    /// `running` / `sleeping` / `dormant` / `preconnect`.
    pub status: String,
    pub login_time: Option<DateTime<Utc>>,
    /// End of the last completed request — "idle since".
    pub idle_since: Option<DateTime<Utc>>,
    pub host: String,
    pub program: String,
    pub ip: String,
    pub database: String,
}

/// Enumerate the database's users. Requires `sql_live_queries` (double-checked
/// here) and a working TDS path to `{server}.database.windows.net`.
pub async fn fetch_db_users(
    auth: &AzureAuth,
    server: &str,
    database: &str,
    live_queries_enabled: bool,
) -> anyhow::Result<Vec<DbUser>> {
    let mut client = connect(auth, server, database, live_queries_enabled).await?;
    let rows = run_query(&mut client, DB_USERS_SQL).await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            Some(DbUser {
                name: str_col(row, "name")?,
                kind: str_col(row, "type_desc").unwrap_or_default(),
                auth: str_col(row, "authentication_type_desc").unwrap_or_default(),
                created: datetime_col(row, "create_date"),
            })
        })
        .collect())
}

/// List the open sessions visible to the signed-in identity. `database:
/// None` connects to `master` — on Azure SQL that's where a server-admin
/// identity sees sessions across the logical server; a database scope shows
/// that database's sessions.
pub async fn fetch_sessions(
    auth: &AzureAuth,
    server: &str,
    database: Option<&str>,
    live_queries_enabled: bool,
) -> anyhow::Result<Vec<DbSession>> {
    let db = database.unwrap_or("master");
    let mut client = connect(auth, server, db, live_queries_enabled).await?;
    let rows = run_query(&mut client, SESSIONS_SQL).await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            Some(DbSession {
                id: row.get::<i16, _>("session_id")?,
                login: str_col(row, "login_name")?,
                status: str_col(row, "status")
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                login_time: datetime_col(row, "login_time"),
                idle_since: datetime_col(row, "last_request_end_time"),
                host: str_col(row, "host_name").unwrap_or_default(),
                program: str_col(row, "program_name").unwrap_or_default(),
                ip: str_col(row, "client_net_address").unwrap_or_default(),
                database: str_col(row, "database_name").unwrap_or_default(),
            })
        })
        .collect())
}

type TdsClient = Client<Compat<TcpStream>>;

/// Open a TDS connection to `{server}.database.windows.net/{database}` with
/// the signed-in identity's Entra token. Handles Azure SQL's login-time
/// routing redirect (one hop). Bounded by [`TDS_TIMEOUT`].
async fn connect(
    auth: &AzureAuth,
    server: &str,
    database: &str,
    live_queries_enabled: bool,
) -> anyhow::Result<TdsClient> {
    // Defense-in-depth: the callers gate on the config flag before spawning,
    // but no code path may open a TDS socket when the user disabled it.
    if !live_queries_enabled {
        return Err(anyhow!(
            "live T-SQL queries are disabled (sql_live_queries = false in config.toml)"
        ));
    }
    let token = auth.token(SCOPE_SQL).await?;
    let host = format!("{server}.database.windows.net");

    let mut config = Config::new();
    config.host(&host);
    config.port(1433);
    config.database(database);
    config.authentication(AuthMethod::aad_token(&token));
    config.encryption(EncryptionLevel::Required);

    tokio::time::timeout(TDS_TIMEOUT, connect_with_redirect(config))
        .await
        .map_err(|_| {
            anyhow!(
                "timed out after {}s connecting to {host}:1433 — the server's firewall \
                 or private endpoint likely doesn't admit this machine's IP \
                 (REST features are unaffected; add a firewall rule to use live T-SQL)",
                TDS_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| friendly_tds_error(e, &host, database))
}

/// TCP + TDS login, following Azure SQL's routing `ENVCHANGE` once — the
/// gateway commonly redirects the session to the node hosting the database.
async fn connect_with_redirect(config: Config) -> Result<TdsClient, tiberius::error::Error> {
    let tcp = TcpStream::connect(config.get_addr()).await?;
    tcp.set_nodelay(true)?;
    match Client::connect(config.clone(), tcp.compat_write()).await {
        Ok(client) => Ok(client),
        Err(tiberius::error::Error::Routing { host, port }) => {
            let mut redirected = config;
            redirected.host(&host);
            redirected.port(port);
            let tcp = TcpStream::connect(redirected.get_addr()).await?;
            tcp.set_nodelay(true)?;
            Client::connect(redirected, tcp.compat_write()).await
        }
        Err(e) => Err(e),
    }
}

async fn run_query(client: &mut TdsClient, sql: &str) -> anyhow::Result<Vec<tiberius::Row>> {
    let result = tokio::time::timeout(TDS_TIMEOUT, client.simple_query(sql))
        .await
        .map_err(|_| {
            anyhow!(
                "timed out after {}s running the query",
                TDS_TIMEOUT.as_secs()
            )
        })?
        .context("running T-SQL query")?;
    tokio::time::timeout(TDS_TIMEOUT, result.into_first_result())
        .await
        .map_err(|_| {
            anyhow!(
                "timed out after {}s reading query rows",
                TDS_TIMEOUT.as_secs()
            )
        })?
        .context("reading T-SQL rows")
}

/// Rewrite the common environmental TDS failures into text that names the fix.
fn friendly_tds_error(e: tiberius::error::Error, host: &str, database: &str) -> anyhow::Error {
    let raw = e.to_string();
    let lower = raw.to_lowercase();
    if lower.contains("login failed") || lower.contains("login error") {
        anyhow!(
            "{raw}\n\nyour signed-in identity isn't a user in '{database}' on {host} \
             (or lacks CONNECT). Live T-SQL needs a contained user or admin mapping — \
             ARM Reader is not enough."
        )
    } else if lower.contains("connection refused")
        || lower.contains("connection reset")
        || lower.contains("denied")
        || lower.contains("firewall")
    {
        anyhow!(
            "{raw}\n\n{host}:1433 rejected the connection — likely the server firewall / \
             private endpoint doesn't admit this machine's IP."
        )
    } else {
        anyhow!("{raw}")
    }
}

fn str_col(row: &tiberius::Row, name: &str) -> Option<String> {
    row.get::<&str, _>(name).map(str::to_owned)
}

/// `datetime` columns come back as `NaiveDateTime`; Azure SQL system time is
/// UTC, so stamping Utc directly is correct there.
fn datetime_col(row: &tiberius::Row, name: &str) -> Option<DateTime<Utc>> {
    row.get::<NaiveDateTime, _>(name)
        .map(|dt| DateTime::from_naive_utc_and_offset(dt, Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_are_fixed_and_read_only() {
        // The UI shows these verbatim and the module's contract is "only
        // these two, ever" — keep them SELECT-shaped and free of any
        // formatting placeholders someone could be tempted to interpolate.
        for sql in [DB_USERS_SQL, SESSIONS_SQL] {
            assert!(sql.starts_with("SELECT "), "read-only: {sql}");
            assert!(!sql.contains('{'), "formatting placeholder in {sql}");
            // Word-level: `sys.dm_exec_sessions` legitimately contains "exec".
            for verb in ["INSERT", "UPDATE", "DELETE", "EXEC", "DROP", "ALTER"] {
                assert!(
                    !sql.to_uppercase().split_whitespace().any(|w| w == verb),
                    "{verb} in {sql}"
                );
            }
        }
        assert!(DB_USERS_SQL.contains("sys.database_principals"));
        assert!(SESSIONS_SQL.contains("sys.dm_exec_sessions"));
    }

    #[tokio::test]
    async fn disabled_flag_fails_closed_before_any_network() {
        // Demo auth would also refuse the token, but the flag must win first —
        // the error names the config knob, not the credential.
        let err = fetch_sessions(&AzureAuth::demo(), "srv", None, false)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("sql_live_queries"));
        let err = fetch_db_users(&AzureAuth::demo(), "srv", "orders", false)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("sql_live_queries"));
    }

    #[test]
    fn friendly_errors_name_the_fix() {
        let login = friendly_tds_error(
            tiberius::error::Error::Protocol("Login failed for user ''.".into()),
            "srv.database.windows.net",
            "orders",
        );
        assert!(format!("{login:#}").contains("isn't a user in 'orders'"));
        let refused = friendly_tds_error(
            tiberius::error::Error::Protocol("Connection refused (os error 111)".into()),
            "srv.database.windows.net",
            "master",
        );
        assert!(format!("{refused:#}").contains("firewall"));
    }

    #[test]
    fn db_user_kind_tags() {
        let mut u = DbUser {
            name: "x".into(),
            kind: "EXTERNAL_USER".into(),
            auth: "EXTERNAL".into(),
            created: None,
        };
        assert_eq!(u.kind_tag(), "entra user");
        u.kind = "SQL_USER".into();
        assert_eq!(u.kind_tag(), "sql user");
        u.kind = "SOMETHING_NEW".into();
        assert_eq!(u.kind_tag(), "user");
    }
}
