# macOS Dev bundle

A signed, provisioned bundle is the only way the brain gets its data protection keychain groups
on macOS: an unsigned or ad-hoc Cargo build can't hold them, so it stays refused. `cargo xtask
mac-bundle` builds that bundle for the Dev edition. It's for development on a Mac you've
provisioned; release signing, Developer ID and notarization are separate and not done yet.

```text
cargo xtask mac-bundle --identity <sha1> --shell-profile <file> --brain-profile <file> --out <new-dir>
```

```mermaid
flowchart LR
  P[two Mac App Development profiles] --> C{checked}
  C --> B[cargo build: Dev shell and brain]
  B --> S[stage the bundle]
  S --> SB[sign the brain helper] --> SS[sign the shell app]
  SS --> V[verify signatures and entitlements]
  V --> L[launch the brain once: exit 69]
```

- **Inputs.** `--identity` is the 40-digit SHA-1 of an Apple Development certificate whose
  private key is on this Mac; no name matching, so codesign can't pick a different one. Each
  profile is read once, and those bytes are both checked and embedded. It must be a current macOS
  profile from one team, authorize that certificate, name exactly the role's App ID from
  `constants::AppleIds::DEV`, and allow the role's own group and the shared instance group. The
  expiry is checked again after the build, just before signing. `--out` must not exist yet.
  These checks read the profile's decoded contents without verifying Apple's signature on it, so
  they catch a wrong or stale profile early but aren't the authority: the system is, when it
  launches each app.
- **Layout.** `Kumochi Dev.app/Contents/MacOS/kumochi`, with the brain nested at
  `Contents/Helpers/kumochid.app` as a background-only app (`LSBackgroundOnly`). Each app embeds
  its own profile at `Contents/embedded.provisionprofile`.
- **Signing.** The brain helper is signed first, then the shell, each with hardened runtime, no
  timestamp, and exactly three entitlements: its App ID, its team, and two keychain groups (its
  App ID and the instance group). The signed entitlements are read back and must match.
- **Launch check.** Every product entry on macOS still exits 69, so the signed brain is run once
  with no arguments and must exit 69 within 10 s; it's killed and reaped if it doesn't. The system
  kills a binary whose signature or entitlements its profile doesn't authorize before `main`, so a
  69 shows the brain's were accepted. The shell isn't launched, so its signature is only checked
  statically.
- **Nothing identifying is printed or committed.** The team, identity and profile values stay in
  the output folder. Messages name only the option or the role, never a path or an argument's
  value. The stderr of codesign and the other tools, and Cargo's build output, which can name them
  or local paths, are withheld; a failed build says which `cargo build` shows why.
- **The first signed use** of the key may ask for the login keychain password. Only the Mac's
  owner answers that. Each codesign call gets 2 minutes, and every other tool 30 s, in its own
  process group; at the deadline the whole group is stopped and the task fails, so a prompt
  nobody answers, or a helper that keeps a pipe open, doesn't hang it.
