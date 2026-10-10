# Security policy

## Product boundary

Egysk is a privileged early-boot userspace and kernel-helper cutover, not
a boot-qualified release merely because its source builds. Enforcing Android
boot, HAL-domain EFI I/O and credential-consumer gates require separate evidence.
Report the exact product commit, Magisk pin/patchset, kernel KMI and configuration.

The helper is not an app-root provider: no credential grant, allowlist/profile
API, SU redirection or backing-file credential-provider ABI belongs to it. Its
retained policy and one-pass rc controls are root-only. Native upstream SU
machinery retains authorization; separation must not become an unauthenticated
request path. An installed root's files, socket, ordinary SU entrypoint and
lifecycle are not ours to appropriate.

Installed modules and supplied policies are trusted privileged code, not a
sandbox for hostile root modules. Relevant defects include unauthorized stage or
installer access, archive/path traversal, source/device substitution, namespace
collisions, premature service start, lost generation state, active-lower mutation,
and writes or relabelling outside owned metadata subtrees. Android keys and
password-slot records are not module upper/work directories.

The raw ESP remains RW and nosuid,nodev,noexec. Executables are copied to tmpfs;
FAT is neither executed nor treated as a per-file SELinux-label store. Restore
the same selected backing after Android's root transition, without live upper/work
overlap or competing filesystem ownership. Fatal managed-bootstrap failures must
not be treated as successful Android handoff. Receipts, when available, are
failure evidence rather than permission to continue.

Independent LKMs and platform tools have their own security boundaries. EFI I/O
uses its backing file's opener credentials, not a helper or caller-credential
fallback; UID 0 does not bypass SELinux file use or block-device policy. Partition
projection is not protection against an actor already able to access raw backing
devices. Symbol relocation cannot repair incompatible kernel layouts, CFI, CRCs
or module signatures.

## Disclosure

Report product-specific issues privately through the core Egysk repository's
[Report a Vulnerability](https://github.com/1vivy/egysk/security/advisories/new)
route. The modules and LKM repositories use this same reporting route. Do not
post working exploits, private keys or credential state in public issues.

Include reproducible steps, exact artifact hashes, observed policy mode, logs with
secrets removed, and whether another root installation was present. Report defects
demonstrably present upstream to the relevant upstream project's security
maintainers as well; do not attribute product-only changes to upstream.

KernelSU history and GPL-3.0 notices are retained. The top-level license does not
relicense independently distributed LKMs or other licensed subtrees.
