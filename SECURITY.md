# Reporting Security Issues

## esu baseline scope

This repository is a **source baseline** for esu, a fork of
[KernelSU](https://github.com/tiann/KernelSU). It is not a released product or
a supported installation path, and its README describes planned behavior that
is not implemented here yet. Do not treat a successful source build, module
load, or README example as evidence that a device boots safely or that any
partition, generation, or receipt guarantee holds.

esu inherits KernelSU's early-boot privilege level. Relevant findings
include, without limitation: bypass of a documented generation or module
self-check; forged or substituted PID-1 binary, module, or configuration;
failure to persist or honor the managed-boot hard-failure receipt; and
`gpt.ko` projection that exposes physical partition names or implements
writes outside its in-memory contract. Note that a raw-LU or raw block device
accessed with sufficient privilege is explicitly outside `gpt.ko`'s
enforcement boundary — that is a documented limit, not a vulnerability.

The planned early failure receipt lives on the ESP at
`/esu/receipts/failure.json`, not on unprojected `/metadata`. The ESP is
normally mounted read-only; failure replacement alone opens a bounded
read-write remount window for a same-directory temporary file, file `fsync`,
atomic rename, and directory `fsync`, followed by ESP sync and read-only
remount. Receipt-storage or remount failure must not permit Android handoff.
`/metadata/esu` is reserved for daemon runtime receipts/logs after
successful handoff, once metadata is available.

Planned `gpt.ko` schema v1 accepts whole block-device backends only. A
preallocated ESP regular file must be attached through a standard loop device
before APPLY and supplied as that block backend, with the attachment retained
for the projection's lifetime. No regular-file, extent, or FIEMAP ABI is added
to `gpt.ko`; this does not enlarge its documented enforcement boundary.

Retention of esu's inherited kernel-module lifecycle is intentional
boot-substrate reuse, not KernelSU Manager or root-product compatibility.
The core remains GPL-3.0. Future `thin.ko` is planned as a separate
GPL-2.0-only module aggregated with, not linked into or relicensed as, the
GPL-3.0 core. Per-subtree license and provenance notices are required; the
top-level license does not relicense that separate module. This is a planned
component boundary, not a claim that `thin.ko` is implemented.

Report esu security issues privately through this fork's GitHub Security
Advisory [Report a Vulnerability](https://github.com/1vivy/kernelesp/security/advisories/new)
form. This is the reporting route for esu-specific defects, including
identity collisions with a real KernelSU installation, the esu contract,
and esu state paths. Do not post security reports or working exploits in
public issues; keep disclosure coordinated until a fix or agreed disclosure.

Reports should state the exact commit, device/firmware, generation values,
configuration, and reproducible steps, plus whether a real KernelSU
installation was present.

## Defects demonstrably present upstream

If a defect is demonstrably present in upstream KernelSU, also report it
through [KernelSU's security policy](https://github.com/tiann/KernelSU/security/policy).
This additional upstream route applies only to defects present upstream, not
to esu-specific reports.
