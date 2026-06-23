//! Headless `SSH_ASKPASS` helper mode.
//!
//! `ssh` invokes us as `sshelf "<prompt>"` (with `SSHELF_ASKPASS=1` in the environment). Because
//! a connect that auto-supplies a stored secret runs with `SSH_ASKPASS_REQUIRE=force`, *every*
//! interactive prompt is routed here (proven by the M0 spikes — see docs/ssh-command.md), and a
//! prompt we decline is NOT retried on the terminal — it simply fails. So we answer:
//!   - **password/passphrase** prompts → the stored secret for `SSHELF_HOST_ID`;
//!   - any **other** prompt → the one-time 2FA code in `SSHELF_2FA_CODE`, if one was queued for
//!     this connection (the user entered it just before connecting);
//!   - otherwise we decline (exit non-zero) — e.g. an unexpected prompt with no code queued.

use crate::paths::Paths;
use crate::secrets;

const HOST_ID_ENV: &str = "SSHELF_HOST_ID";
/// Env var carrying a one-time verification code the user entered for this connection.
pub(crate) const CODE_ENV: &str = "SSHELF_2FA_CODE";

/// What a given prompt should be answered with (decided without IO, so it's unit-testable).
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    /// The stored login password / key passphrase.
    Secret,
    /// The queued one-time 2FA code.
    Code,
    /// Nothing — decline.
    Decline,
}

/// Decide how to answer `prompt`. A password/passphrase prompt always takes the stored secret;
/// any other prompt takes the queued code when one is present (the user opted into 2FA for this
/// connection, so a non-secret prompt is the verification step). Host-key prompts never reach
/// here in practice — connect passes `StrictHostKeyChecking=accept-new`.
fn classify(prompt: &str, has_code: bool) -> Answer {
    if is_secret_prompt(prompt) {
        Answer::Secret
    } else if has_code {
        Answer::Code
    } else {
        Answer::Decline
    }
}

/// Run askpass mode for the given prompt; returns the process exit code.
pub fn run(prompt: &str) -> i32 {
    let code = std::env::var(CODE_ENV).ok().filter(|c| !c.is_empty());
    match classify(prompt, code.is_some()) {
        Answer::Secret => supply_secret(),
        Answer::Code => {
            let code = zeroize::Zeroizing::new(code.unwrap_or_default());
            // ssh reads one line and strips the trailing newline.
            println!("{}", code.as_str());
            0
        }
        Answer::Decline => 1,
    }
}

/// Look up and print the stored secret for `SSHELF_HOST_ID`; exit code per success.
fn supply_secret() -> i32 {
    let Ok(id) = std::env::var(HOST_ID_ENV) else {
        return 1;
    };
    if id.is_empty() {
        return 1;
    }
    let Ok(paths) = Paths::resolve() else {
        return 1;
    };
    match secrets::get_password(&paths.vault_file(), &id) {
        Ok(Some(pw)) => {
            let pw = zeroize::Zeroizing::new(pw);
            println!("{}", pw.as_str());
            0
        }
        _ => 1,
    }
}

/// True if the prompt is asking for the host's stored secret — a login **password** or a key
/// **passphrase**.
///
/// We match the *shape* of OpenSSH's standard prompts (not just the substring), so a
/// compromised server can't phish the stored secret with a keyboard-interactive prompt like
/// "Type your password to continue:". Recognized prompts:
///
/// - classic password auth `user@host's password:` and PAM `Password:` → end with "password:"
/// - key passphrase `Enter passphrase for key '<path>':` → contains "passphrase for"
///
/// Host-key confirmations, OTP/verification codes, and arbitrary server text are declined.
fn is_secret_prompt(prompt: &str) -> bool {
    let p = prompt.trim().to_lowercase();
    p.ends_with("password:") || p.contains("passphrase for")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_standard_password_and_passphrase_prompts() {
        assert!(is_secret_prompt("tester@host's password: "));
        assert!(is_secret_prompt("Password:"));
        assert!(is_secret_prompt(
            "Enter passphrase for key '/home/u/.ssh/id_ed25519': "
        ));
    }

    #[test]
    fn declines_host_key_otp_and_phishing_prompts() {
        assert!(!is_secret_prompt(
            "Are you sure you want to continue connecting (yes/no/[fingerprint])? "
        ));
        assert!(!is_secret_prompt("Verification code: "));
        // Server-controlled keyboard-interactive prompts that merely mention the word:
        assert!(!is_secret_prompt(
            "Please confirm your password for this operation:"
        ));
        assert!(!is_secret_prompt("Type your password to continue:"));
    }

    #[test]
    fn classify_routes_password_code_and_decline() {
        // A password/passphrase prompt always takes the stored secret, code queued or not.
        assert_eq!(classify("tester@host's password: ", true), Answer::Secret);
        assert_eq!(
            classify("Enter passphrase for key '/k': ", false),
            Answer::Secret
        );
        // The 2FA verification prompt takes the queued code…
        assert_eq!(classify("Verification code: ", true), Answer::Code);
        // …but with no code queued, a non-secret prompt is declined (the old behavior).
        assert_eq!(classify("Verification code: ", false), Answer::Decline);
        // A second/unknown prompt during a 2FA connect still gets the (one) queued code.
        assert_eq!(classify("One-time password (OATH): ", true), Answer::Code);
    }
}
