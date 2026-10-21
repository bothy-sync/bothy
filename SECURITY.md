# Security policy

## Reporting a vulnerability

Please report security issues **privately** through GitHub:
**Security → Report a vulnerability** on <https://github.com/bothy-sync/bothy>. Do not open a
public issue.

Include what you found, how to reproduce it, and what an attacker could do. We aim to reply
within 7 days and to agree on a disclosure date with you.

## Supported versions

Bothy is before 1.0; only the latest release gets fixes.

## What is in scope

- The `bothy` and `bothy-cli` crates, and the on-remote format they read and write.
- How the CLI handles OAuth tokens and client files.
- Parsing of remote files. A device must not crash, hang or apply invalid data when another
  device, or someone with access to the Drive account, writes malformed files.

## Known limits in 0.1

- **No encryption yet.** Remote files are readable by anyone with access to the Google account,
  and by Google. Client-side encryption is planned for 0.2. The local SQLite database is not
  encrypted either; it relies on the operating system's protection.
- **OAuth clients are your own.** Desktop client secrets are not confidential by Google's
  design. The CLI stores the refresh token in the OS keyring, or in a `0600` file where no
  keyring exists.
- **Trust between devices.** Every device that can sign in to the account can write to the
  database. Bothy validates the format and integrity of remote files, not the intent of their
  author.
