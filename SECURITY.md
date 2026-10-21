# Security policy

## Supported versions

The latest v1 release is supported. There are no older maintained branches.

## Reporting a vulnerability

Please do not open a public issue for a security problem. Use GitHub's private
vulnerability reporting: open the repository's **Security** tab and choose
**Report a vulnerability**. If that is unavailable, contact the maintainer
through the repository's issue tracker without including exploit details, and a
private channel will be arranged.

Please include:

- the client version (Diagnostics shows it), the operating system, and the
  Immich server version;
- the impact and the steps to reproduce;
- logs or a debug bundle if they help. Review the bundle first: it contains your
  folder paths, account email, server URL, and logs, but not your password, API
  key, or session token.

This is a volunteer project without a bug bounty. Reports are acknowledged as
time allows, and fixes are released without advance notice when practical.

## Scope

Especially interesting: credential storage and handling, log and debug-bundle
redaction, the upload path, the single-instance handoff, and the Windows
installer. Vulnerabilities in Immich itself belong in the
[Immich project](https://github.com/immich-app/immich/security).
