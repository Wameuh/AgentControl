# Security policy

## Supported versions

Only the latest release receives security fixes.

## Reporting a vulnerability

Please **don't open a public issue**. Use GitHub's private reporting:
Security tab → "Report a vulnerability". You'll get an answer within a week.

## Scope

The daemon, the desktop/Android app, the pairing script (`pair.sh`) and the
release pipeline. See the "Security model" section of the README for what
AgentControl does and doesn't protect against.

## Release signing

Releases ship `SHA256SUMS` signed with minisign (Ed25519) — see
`SHA256SUMS.minisig` next to each release asset. Public key:
[`packaging/agentcontrol.pub`](packaging/agentcontrol.pub). The desktop app,
the daemon self-update and the remote install all verify the signature and
the checksums before installing.

### Key rotation

When the signing key rotates, the transition release stays signed by the
previous key while carrying the new `packaging/agentcontrol.pub` — clients
verify releases with the key they were built with, so this release is the
one that installs the new key for the next rotation. Out of band, you can
always verify `SHA256SUMS.minisig` against the `packaging/agentcontrol.pub`
committed on that release's tag.
