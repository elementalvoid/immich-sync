# Credential strategy: store the password, keep the token in memory

Status: accepted
Date: 2026-10-12

The user logs in with an email address and a password. The client stores the password in the OS credential store (Windows Credential Manager) and keeps the Immich session token in memory only. At startup the client logs in again. After a 401 it logs in again with the stored password. The config window also accepts an API key for servers with password login disabled. We chose the password over the token because the Immich session token has no refresh path, and the server can revoke it or clean it up at any time. A stored token then fails with no recovery. The cost is long-term password storage in the OS store. The password never touches the config file or the logs.

## Considered Options

- **Session token only.** Rejected: a revoked session stops sync until the user types the password again.
- **Password and token.** Rejected: two stored secrets with no benefit.
- **API key only.** Rejected: the brief asks for password login, and the key needs manual creation.

## Consequences

- The client needs a re-login path after a 401.
- The client needs a clear message when the server disables password login.
- The client validates the credential at startup.
- Credentials stay out of logs, crash reports, and the config file.
