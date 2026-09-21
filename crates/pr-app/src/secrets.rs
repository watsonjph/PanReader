//! Passwords, in the OS keychain and nowhere else.
//!
//! Invariant 13 draws two lines and this file is on the allowed side of both. Signing
//! in to a server the reader runs or has an account on -- Suwayomi, Komga, Kavita, a
//! WebDAV box -- is their own library and their own account, and has nothing to do with
//! the bot-detection question. What matters is where the password lives.
//!
//! It lives in the OS credential store. Never in the settings blob, never in the
//! database, never in a backup: a backup that carries the token to fetch backups is a
//! credential leak wearing a useful hat.
//!
//! Keyed by **origin** rather than by the URL someone typed. Basic auth applies to a
//! whole server, a catalog's feeds are spread across its paths, and storing a password
//! per path would ask for it again on the first link the reader followed.

use anyhow::Context as _;

/// What the credential store files these under.
const SERVICE: &str = "PanReader";

/// `https://host:port`, which is the scope a password actually has.
pub fn origin(url: &str) -> anyhow::Result<String> {
    let parsed = url::Url::parse(url).with_context(|| format!("{url} is not a URL"))?;
    let host = parsed
        .host_str()
        .with_context(|| format!("{url} has no host"))?;
    Ok(match parsed.port() {
        Some(port) => format!("{}://{host}:{port}", parsed.scheme()),
        None => format!("{}://{host}", parsed.scheme()),
    })
}

fn entry(origin: &str) -> anyhow::Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, origin)
        .with_context(|| format!("no credential store for {origin}"))
}

pub fn set(origin: &str, password: &str) -> anyhow::Result<()> {
    entry(origin)?
        .set_password(password)
        .with_context(|| format!("could not save the password for {origin}"))
}

/// The password for an origin, or nothing.
///
/// A missing entry is `None` rather than an error: not having a password is the normal
/// state for almost every server, and a keychain that is locked or absent should mean
/// "ask again", not "the app is broken".
pub fn get(origin: &str) -> Option<String> {
    match entry(origin) {
        Ok(entry) => entry.get_password().ok(),
        Err(e) => {
            tracing::debug!(origin, "no credential store: {e}");
            None
        }
    }
}

/// Forget one. Deleting something that was never there is a success, because the state
/// the caller asked for is the state they end up in.
pub fn forget(origin: &str) -> anyhow::Result<()> {
    if let Ok(entry) = entry(origin) {
        match entry.delete_credential() {
            Ok(()) => {}
            Err(keyring::Error::NoEntry) => {}
            Err(e) => return Err(e).context("could not remove the password"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A password is scoped to a server, not to the one link someone happened to paste.
    #[test]
    fn an_origin_is_the_server_and_not_the_path() {
        assert_eq!(
            origin("https://suwayomi.test/api/opds/v1.2/series?page=2").unwrap(),
            "https://suwayomi.test"
        );
        assert_eq!(
            origin("http://192.168.1.4:4567/api/opds/v1.2").unwrap(),
            "http://192.168.1.4:4567"
        );
        // A different port is a different server, and so is a different scheme.
        assert_ne!(
            origin("http://a.test:4567/x").unwrap(),
            origin("http://a.test:8080/x").unwrap()
        );
        assert_ne!(
            origin("http://a.test/x").unwrap(),
            origin("https://a.test/x").unwrap()
        );
        assert!(origin("not a url").is_err());
        assert!(origin("file:///etc/passwd").is_err(), "no host");
    }

    /// Round trip through the real credential store. Uses an origin nothing else could
    /// collide with, and removes it afterwards.
    #[test]
    fn a_password_round_trips_through_the_os_keychain() {
        let origin = "https://panreader-test.invalid:65000";
        let _ = forget(origin);
        assert_eq!(get(origin), None, "nothing to start with");

        if set(origin, "hunter2").is_err() {
            // A headless CI box has no credential store, and that is not a failure of
            // this code. The `get` path above already covers the absent case.
            return;
        }
        assert_eq!(get(origin).as_deref(), Some("hunter2"));

        // Overwriting is how changing a password works.
        set(origin, "correct horse").unwrap();
        assert_eq!(get(origin).as_deref(), Some("correct horse"));

        forget(origin).unwrap();
        assert_eq!(get(origin), None);
        forget(origin).unwrap(); // idempotent
    }
}
