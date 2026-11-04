---
title: Privacy policy
description: What Bothy's apps and tools do with your data.
---

*Last updated: 26 October 2026.*

This policy covers the software published by the Bothy project (bothy-sync): the `bothy`
command-line tool, the Bothy Notes demo app and the libraries. These are referred to below as
"the software". It also covers sign-ins with the project's own Google OAuth client.

## The short version

The Bothy project runs **no servers** and **collects nothing**. Your data goes from your
devices to **your own Google Drive** and back. We never receive it, see it or store it.

## What the software accesses

When you sign in with Google, the software asks for permission to use these Google Drive
scopes:

- `drive.appdata`: a hidden folder in your Drive that only this app can see;
- `drive.file`: only the files and folders the software itself creates (for example
  `Bothy/robot`).

The software cannot see your other Drive files. It uses this access only to store and read the
files of your Bothy databases: your documents, encrypted if you chose a password, and small
files that let your own devices find each other.

## Where things are stored

- **On your devices:** your databases, and your Google sign-in (a refresh token), in the
  operating system's keyring or in a file only your user account can read.
- **In your Google Drive:** your databases' files. They count against your Drive storage, and
  you can delete them in Drive at any time.
- **Nowhere else.** There is no analytics, no telemetry, no crash reporting and no
  advertising.

## Network connections

The software connects only to:

- Google's sign-in and Drive APIs;
- when direct channels are on, your own other devices, plus a public STUN server
  (`stun.l.google.com` by default) that tells a device its public network address.

## Google API Services User Data Policy

The software's use and transfer of information received from Google APIs adheres to the
[Google API Services User Data Policy](https://developers.google.com/terms/api-services-user-data-policy),
including the Limited Use requirements. Data from Google APIs is not transferred to anyone,
not used for advertising, and not read by people.

## Removing access

Signing out in the software (`bothy auth logout`, or *Sign out* in the app) forgets the sign-in
on that device. To end the access on all your devices, remove the app at
[myaccount.google.com/permissions](https://myaccount.google.com/permissions) or run
`bothy auth logout --revoke`. To remove the data, delete the app's data in Google Drive (Settings → Manage apps) and the local database
files.

## Contact

Questions: open an issue at
[github.com/bothy-sync/bothy](https://github.com/bothy-sync/bothy/issues).
