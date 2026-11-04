# Changelog

What changed in each release, for the people running it. Versions follow
[semantic versioning](https://semver.org); the collar protocol has its own versions, in
[opencollar](https://github.com/open-pasture/opencollar/tree/main/protocol).

Add a line under **Unreleased** in the pull request that makes a change people will notice.
`scripts/set-version.py` doesn't touch this file: rename Unreleased to the new version when you
cut a release (see `docs/RELEASING.md`).

## Unreleased

- Hands and up can claim collars into any herd on the farm; viewers get a plain refusal.
  Same-farm herd moves preserve the collar id and key, with a notice and undo window
  (`UNDO_WINDOW_HOURS`, default 24 hours pending a final owner decision). Wrong claim codes
  never reveal that a serial is already claimed or which herd it is in; a farm-wide wrong-code
  rate only raises a soft abuse alarm (it does not lock out other people).
- Cross-farm collar handover: owners can release a collar (revokes its key here), take a
  signed one-time transfer token to another farm, and export that collar's own history only
  (audit without user ids). Hands claim the token on the receiving farm, keep the same collar
  id, and can bring the export history with them.

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
