# Vendored generic-bootctl provenance

`payloads/boot-hal/vendor/generic-bootctl` is a pinned copy of the shared
boot-control crates from <https://github.com/1vivy/generic-bootctl>:

| field | value |
| --- | --- |
| repository | `github.com/1vivy/generic-bootctl` |
| commit | `a1cede7b6215e203abd2d26fd37f4a90acb3792d` |
| subject | `Implement raw HIDL boot-control transport and manifest-driven registration` |
| vendored paths | `crates/core`, `crates/aidl` |
| license | Apache-2.0 (upstream project license) |

The copy was produced with
`git -C generic-bootctl archive a1cede7 crates/core crates/aidl | tar -x -C payloads/boot-hal/vendor/generic-bootctl`
and `diff -r` against that archive is empty: every vendored byte, including the
upstream tests, is identical to the pinned commit.

| file | sha256 |
| --- | --- |
| crates/core/Cargo.toml | 4e2e019c7bace6fc304893336b4b694ca4b8fff173eea61d66d160786522b172 |
| crates/core/src/lib.rs | efc41dcea4009e664b754973a977242f21c8f7b36644ff997c9931c6fecdaf00 |
| crates/core/tests/semantics.rs | 5f3cc8c24f7eb1a4bd7b126635641fac29ab535f771434bb297fa12ddb83c5ef |
| crates/aidl/Cargo.toml | e05bf4a289962085d1b314a456bc514c1f51c403658ae915413d63132cec9717 |
| crates/aidl/src/lib.rs | 2d811169b347bec002e5abf115540e87da2592969e6d28147c0a2ebdf1aa4cf2 |
| crates/aidl/src/android.rs | 3650f58452a76a1a8b6acd2073451a66fbb755f0b6b18eb65b1efe01c12cb039 |

From this worktree, check canonical identity with:

```sh
git -C ../generic-bootctl archive a1cede7 crates/core crates/aidl | tar -t
diff -r "$(mktemp -d)/crates" payloads/boot-hal/vendor/generic-bootctl/crates
```

## Deviations from the upstream manifests

None inside the vendored files. The three upstream workspace fields the crates
inherit (`version`, `edition`, `rust-version`, `license`, `[lints]`) are supplied
by `payloads/boot-hal/Cargo.toml` through `[workspace.package]` and
`[workspace.lints]`, so `crates/*/Cargo.toml` stays byte-identical instead of
being rewritten for this workspace. The vendored crates add no third-party
dependency: the whole workspace still builds with `--locked --offline` and the
produced executable has the same shared-library dependencies as before.

## What stays in `payloads/boot-hal`

The esu product code is not vendored: `src/backend.rs` (efivarfs records,
`BootedRom` identity, misc VAB mirror, retry policy), `src/wire.rs` (GBS1/GBM1
byte layouts), `src/main.rs` (Android entry and the always-writable service gate)
and `tests/efivarfs.rs`. Upstream owns slot health policy, the frozen AIDL V1
dispatch and the Binder transport.

The core additions this consumer required were requested upstream and landed
before the pin (`Extend core backend lifecycle and slot health policy for
consumers`): `Backend::slot_count`, the default `Backend::prepare` hook,
`HealthOnSuccess::{ResetToOne, PreserveNonZero}` and index-before-storage slot
validation. The esu backend uses all four; no fork of the shared logic exists
here.
