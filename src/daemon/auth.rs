use std::io::Write;

use crate::{
    error::{AppError, Result},
    http,
};

/// Prompt the user for a new password (twice to confirm) and return an Argon2id PHC hash.
pub(super) fn prompt_and_hash_password() -> Result<String> {
    loop {
        let pass1 = rpassword::prompt_password("Set daemon password: ").map_err(AppError::Io)?;
        if pass1.is_empty() {
            eprintln!("error: password must not be empty");
            continue;
        }
        let pass2 = rpassword::prompt_password("Confirm password: ").map_err(AppError::Io)?;
        if pass1 != pass2 {
            eprintln!("error: passwords do not match, please try again");
            continue;
        }
        return http::auth::hash_password(&pass1)
            .map_err(|e| AppError::Protocol(format!("password hashing failed: {e}")));
    }
}

/// Print a risk warning for `--no-auth` mode and require explicit confirmation.
pub(super) fn confirm_no_auth_risk() -> Result<()> {
    eprintln!();
    eprintln!("🚨🚨🚨  WARNING: --no-auth disables HTTP authentication  ⚠️⚠️⚠️");
    eprintln!();
    eprintln!("Anyone who can reach the HTTP port will have FULL CONTROL over");
    eprintln!("all sessions, including the ability to send arbitrary input.");
    eprintln!();
    eprintln!("Only proceed if you are CERTAIN the port is not publicly");
    eprintln!("accessible (e.g. behind a secure gateway or firewall).");
    eprintln!();
    eprint!("Type 'yes' to confirm and continue: ");
    std::io::stderr().flush().ok();

    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .map_err(AppError::Io)?;

    if input.trim() != "yes" {
        return Err(AppError::Protocol(
            "aborted: user did not confirm --no-auth risk".into(),
        ));
    }
    eprintln!();
    Ok(())
}
