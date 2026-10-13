//! Single instance: one running client per user.
//!
//! The first launch binds a namespaced local socket (a named pipe on Windows,
//! a Unix-domain socket elsewhere). A later launch connects, asks the running
//! instance to show its window, and exits.

use async_channel::Sender;
use interprocess::local_socket::prelude::*;
use interprocess::local_socket::{GenericNamespaced, ListenerOptions};
use std::fmt::Write as _;
use std::io::{self, Read, Write as _};

/// The per-user socket name that marks the running instance.
///
/// Namespaced sockets share one namespace between every user on the machine,
/// so the name carries the user identity.
#[must_use]
pub fn socket_name() -> String {
    socket_name_for(&current_user())
}

/// Builds the socket name for one user identity.
///
/// A socket name cannot carry every character an account name can (Windows
/// rejects `\`, for one), so invalid bytes become `%xx` escapes. The escape is
/// injective, so two distinct users never share a name.
fn socket_name_for(user: &str) -> String {
    let mut name = String::from("immich-sync-");
    for byte in user.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            name.push(char::from(byte));
        } else {
            write!(name, "%{byte:02x}").expect("writing to a `String` cannot fail");
        }
    }
    name.push_str(".sock");
    name
}

/// The OS user identity that scopes the socket name.
///
/// The desktop session sets these variables. Ticket #3 brings the
/// platform-directory abstraction that replaces this environment lookup.
fn current_user() -> String {
    ["USERNAME", "USER", "LOGNAME"]
        .into_iter()
        .find_map(|key| std::env::var(key).ok())
        .filter(|user| !user.is_empty())
        // shortcut: with no user variable every user shares one name; #3
        // replaces this lookup with the platform directories.
        .unwrap_or_else(|| "unknown".to_owned())
}

/// A message a second launch sends to the running instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Bring the main window to the front, opening it if needed.
    Show,
}

impl Request {
    /// Wire form of [`Request::Show`].
    pub const SHOW: &'static [u8] = b"show\n";

    /// Parses a wire message. Unknown payloads return `None`.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        (bytes == Self::SHOW).then_some(Self::Show)
    }
}

/// Claims the single instance, or hands off to the running one.
///
/// Returns `Some(listener)` for the primary and `None` after asking the
/// running instance to focus. Connect first and bind on failure: binding
/// first would make a second launch look like the primary until the bind
/// fails.
///
/// # Errors
///
/// Returns an error when the socket can be neither reached nor bound.
pub fn claim(name: &str) -> io::Result<Option<LocalSocketListener>> {
    let name = name.to_ns_name::<GenericNamespaced>()?;
    if let Ok(mut stream) = LocalSocketStream::connect(name.clone()) {
        // The connect already proved a primary exists; a dropped handshake
        // must not turn this process into a duplicate primary.
        let _ = stream.write_all(Request::SHOW);
        return Ok(None);
    }

    let listener = ListenerOptions::new()
        .name(name)
        .try_overwrite(true)
        .create_sync()?;
    Ok(Some(listener))
}

/// Blocks for the next recognised request, skipping unknown messages.
///
/// # Errors
///
/// Returns an error when the listener itself fails.
pub fn next_request(listener: &LocalSocketListener) -> io::Result<Request> {
    loop {
        let Some(result) = listener.incoming().next() else {
            return Err(io::Error::other("single-instance listener stopped"));
        };
        let mut connection = result?;
        if let Some(request) = read_request(&mut connection)? {
            return Ok(request);
        }
    }
}

/// Reads one request from `reader`.
///
/// A read can return a partial message, so accumulate until a newline or EOF,
/// bounded by the buffer. An unrecognised or oversized message reads as `None`.
///
/// # Errors
///
/// Returns an error when the underlying reader fails.
fn read_request(reader: &mut impl Read) -> io::Result<Option<Request>> {
    let mut buffer = [0u8; 64];
    let mut filled = 0;
    while filled < buffer.len() {
        let read = reader.read(&mut buffer[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
        if buffer[..filled].contains(&b'\n') {
            break;
        }
    }
    Ok(Request::parse(&buffer[..filled]))
}

/// Forwards every request from `listener` to `requests`. Runs on its own thread.
pub fn forward(listener: LocalSocketListener, requests: Sender<Request>) {
    loop {
        match next_request(&listener) {
            Ok(request) => {
                if requests.send_blocking(request).is_err() {
                    break;
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, "the single-instance listener stopped");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_names_are_scoped_to_the_user() {
        assert_ne!(socket_name_for("alice"), socket_name_for("bob"));
    }

    #[test]
    fn socket_name_escapes_characters_a_socket_name_cannot_carry() {
        assert_eq!(
            socket_name_for(r"corp\alice"),
            "immich-sync-corp%5calice.sock"
        );
        assert_eq!(socket_name_for("alice"), "immich-sync-alice.sock");
    }

    #[test]
    fn assembles_a_request_split_across_reads() {
        // `chain` returns the first slice, then the second: one message
        // arriving in two reads.
        let mut reader = b"sh".as_slice().chain(b"ow\n".as_slice());

        assert_eq!(
            read_request(&mut reader).expect("read the handoff"),
            Some(Request::Show)
        );
    }

    #[test]
    fn parses_the_show_request() {
        assert_eq!(Request::parse(b"show\n"), Some(Request::Show));
    }

    #[test]
    fn ignores_an_unknown_request() {
        assert_eq!(Request::parse(b"pause\n"), None);
    }

    #[test]
    fn second_launch_hands_off_to_the_primary() {
        let name = format!("immich-sync-test-{}.sock", std::process::id());

        let listener = claim(&name)
            .expect("bind the single-instance socket")
            .expect("the first claim is the primary");
        let second = claim(&name).expect("connect to the primary");
        assert!(second.is_none(), "the second claim is a secondary");

        assert_eq!(
            next_request(&listener).expect("read the handoff"),
            Request::Show
        );
    }
}
