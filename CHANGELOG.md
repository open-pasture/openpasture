# Changelog

What changed in each release, for the people running it. Versions follow
[semantic versioning](https://semver.org); the collar protocol has its own versions, in
[opencollar](https://github.com/open-pasture/opencollar/tree/main/protocol).

Add a line under **Unreleased** in the pull request that makes a change people will notice.
`scripts/set-version.py` doesn't touch this file: rename Unreleased to the new version when you
cut a release (see `docs/RELEASING.md`).

## Unreleased

## 0.2.2 (2026-10-01)

- Desktop: a workbench layout with a rail, sidebars and tabs. Things that need a person (a move
  to approve, a collar gone quiet) show as asks in place, instead of a separate alerts page.

## 0.2.1 (2026-09-30)

- Desktop: an update button at the foot of the sidebar when a new version is out.

## 0.2.0 (2026-09-30)

- Desktop: the app updates itself from GitHub releases. Updates are signed and checked before
  they install. 0.1.0 has no updater: download the DMG once more.
- A new sidebar layout and design system.
- Field-ready for one farm of 250 collars: people and roles, alerts and approvals by text with
  your own Twilio number, email, webhooks and Web Push, the morning brief, strip schedules
  staged on the collars, pre-send checks, animals and imports, reports and the welfare record,
  and a phone layout that installs over https.
- Self-hosting and texting guides.

## 0.1.0 (2026-09-26)

First release: the server with the web UI built in, the macOS app, server binaries for Linux
and macOS, and a Docker image. Collars report over HTTP and get signed boundaries back;
`collar-sim` speaks the same protocol.
