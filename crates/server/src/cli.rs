//! The server's commands other than serving.
//!
//! `sharecraft-server add-user --email <email> --name <name> [--admin]`: creates an account
//! from the command line. The password is read without echo from the terminal, or as one line
//! from standard input when it is piped. It is never shown or logged.
//!
//! `sharecraft-server rebuild`: replaces the revisions recorded in the database for every
//! document in Git with what Git holds.

use std::io::{BufRead, IsTerminal};
use std::sync::Arc;

use records::{Db, NewUser};
use revisions::RevisionStore;

use crate::checkpoint::{Checkpointer, library_path};
use crate::config::Config;
use crate::projection::{Report, rebuild as rebuild_revisions};

/// How to call `rebuild`.
pub const REBUILD_USAGE: &str = "usage: sharecraft-server rebuild";

/// How to call `add-user`.
pub const ADD_USER_USAGE: &str =
    "usage: sharecraft-server add-user --email <email> --name <name> [--admin]";

/// The arguments of `add-user`.
#[derive(Debug, PartialEq, Eq)]
pub struct AddUser {
    pub email: String,
    pub name: String,
    pub admin: bool,
}

/// Reads the arguments that follow `add-user`.
///
/// # Errors
/// A message naming the problem when an option is unknown, repeated, missing or has no value.
pub fn parse_add_user(args: &[String]) -> Result<AddUser, String> {
    let mut email = None;
    let mut name = None;
    let mut admin = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let slot = match arg.as_str() {
            "--email" => &mut email,
            "--name" => &mut name,
            "--admin" if !admin => {
                admin = true;
                continue;
            }
            other => return Err(format!("unexpected argument {other:?}\n{ADD_USER_USAGE}")),
        };
        if slot.is_some() {
            return Err(format!("{arg} is given twice\n{ADD_USER_USAGE}"));
        }
        let value = args
            .next()
            .ok_or_else(|| format!("{arg} needs a value\n{ADD_USER_USAGE}"))?;
        *slot = Some(value.clone());
    }
    Ok(AddUser {
        email: email.ok_or_else(|| format!("--email is required\n{ADD_USER_USAGE}"))?,
        name: name.ok_or_else(|| format!("--name is required\n{ADD_USER_USAGE}"))?,
        admin,
    })
}

/// Runs `add-user` with the arguments that follow it: reads the password, connects to
/// `DATABASE_URL`, brings the schema up to date and creates the account.
///
/// # Errors
/// A message for the user when the arguments, the password, the database or the new account
/// are unusable. It never contains the password or the database URL.
pub async fn add_user(args: &[String]) -> Result<String, String> {
    let wanted = parse_add_user(args)?;
    let url = crate::config::database_url_from_env().map_err(|e| e.to_string())?;
    let password = read_password().map_err(|e| format!("cannot read the password: {e}"))?;
    let db = Db::connect(url.expose())
        .await
        .map_err(|e| format!("cannot connect to the database: {e}"))?;
    db.migrate()
        .await
        .map_err(|e| format!("cannot migrate the database: {e}"))?;
    let user = db
        .create_user(
            None,
            NewUser {
                email: &wanted.email,
                name: &wanted.name,
                password: &password,
                is_admin: wanted.admin,
            },
        )
        .await
        .map_err(|e| format!("cannot create the account: {e}"))?;
    let kind = if user.is_admin {
        "administrator"
    } else {
        "account"
    };
    Ok(format!("created {kind} {} ({})", user.email, user.id.0))
}

/// Runs `rebuild` with the arguments that follow it (there are none): connects to
/// `DATABASE_URL`, brings the schema up to date, opens the library repository under the data
/// directory and replaces the revision list of every document in it. Returns the report, whose
/// `failed` lists the documents that could not be rebuilt.
///
/// # Errors
/// A message for the user when the arguments, the settings, the database or the repository
/// are unusable. It never contains the database URL.
pub async fn rebuild(args: &[String]) -> Result<Report, String> {
    if let Some(arg) = args.first() {
        return Err(format!("unexpected argument {arg:?}\n{REBUILD_USAGE}"));
    }
    let config = Config::from_env().map_err(|e| e.to_string())?;
    let db = Db::connect(config.database_url.expose())
        .await
        .map_err(|e| format!("cannot connect to the database: {e}"))?;
    db.migrate()
        .await
        .map_err(|e| format!("cannot migrate the database: {e}"))?;
    let path = library_path(&config.data_dir);
    let opened = tokio::task::spawn_blocking(move || {
        RevisionStore::open(&path).map_err(|e| format!("cannot open {}: {e}", path.display()))
    })
    .await
    .map_err(|e| format!("cannot open the library: {e}"))??;
    let store: Arc<dyn Checkpointer> = Arc::new(opened);
    rebuild_revisions(store, &db)
        .await
        .map_err(|e| format!("cannot list the documents in Git: {e}"))
}

/// The password, typed without echo at a terminal, or the first line of piped input.
fn read_password() -> std::io::Result<String> {
    if std::io::stdin().is_terminal() {
        return rpassword::prompt_password("Password: ");
    }
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let without_newline = line.strip_suffix('\n').unwrap_or(&line);
    Ok(without_newline
        .strip_suffix('\r')
        .unwrap_or(without_newline)
        .to_owned())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn add_user_arguments_are_parsed() {
        assert_eq!(
            parse_add_user(&args(&["--email", "ann@x.test", "--name", "Ann Lee"])),
            Ok(AddUser {
                email: "ann@x.test".into(),
                name: "Ann Lee".into(),
                admin: false,
            })
        );
        assert_eq!(
            parse_add_user(&args(&[
                "--admin",
                "--name",
                "Ada",
                "--email",
                "ada@x.test"
            ])),
            Ok(AddUser {
                email: "ada@x.test".into(),
                name: "Ada".into(),
                admin: true,
            })
        );
        for bad in [
            &["--name", "Ann"][..],
            &["--email", "ann@x.test"],
            &["--email"],
            &[
                "--email", "a@x.test", "--email", "b@x.test", "--name", "Ann",
            ],
            &["--email", "a@x.test", "--name", "Ann", "--admin", "--admin"],
            &[
                "--email",
                "a@x.test",
                "--name",
                "Ann",
                "--password",
                "secret",
            ],
        ] {
            assert!(parse_add_user(&args(bad)).is_err(), "{bad:?}");
        }
    }
}
