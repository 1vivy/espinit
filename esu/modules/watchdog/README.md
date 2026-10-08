# `watchdog`: lab-only boot watchdog

**Status (2026-10-08)** — Lab-only. Not part of the default payload: add
`watchdog` to `modules_order` (and this module to the ESP) only in payloads that
are driven without physical access to the phone. It ships no `critical` marker,
no `recovery-ok` and no scripts, so it is optional and normal-boot only.

The module is one init service plus `module.prop`:

```
esu/modules/watchdog/
  module.prop
  initrc/watchdog.rc
```

`initrc/watchdog.rc` starts `esud watchdog 180` once, at `on init`, as root in
the `esu` domain (`oneshot`, `disabled`; the core RC's `on init` work and the
`esud early` staging precede it because module RC is concatenated after the
bootstrap RC). The kernelesp exec hook escapes init children executing exactly
`/debug_ramdisk/esu/bin/esud` into the esu domain, the same way the core RC's
`exec u:r:esu:s0 ... esud early` does.

## What the deadline does

`esud watchdog <seconds>` polls the `sys.boot_completed` property once per
second and exits 0 as soon as it is `1`. If Android has not reported completion
within the deadline, the watchdog performs exactly one bounded attempt of each
step, logging every step to kmsg with the `esu watchdog:` prefix:

1. **ESP receipt.** The last 64 KiB of the kernel log are read non-blocking from
   `/dev/kmsg` (every buffered record, keeping the tail) and written to
   `esu/receipts/watchdog.txt` on the ESP at `/debug_ramdisk/esp`. The ESP mount
   is remounted read-write for that single window and read-only again afterwards
   (`nosuid,nodev,noexec,relatime` with only `RDONLY` toggled, the same set the
   PID-1 failure receipt uses).
2. **BCB request.** `bootonce-bootloader` and the status cause
   `esu:watchdog:boot_completed` are written to the misc partition's command and
   status prefix only; every byte at or above offset 64 is left untouched.
   `/dev/block/by-name/misc` is used when it exists, otherwise the partition is
   resolved from the kernel's sysfs `PARTNAME` and opened through a node esu
   creates, which also works before init publishes the by-name links.
3. **Restart.** `sync()` and `reboot(Restart)` are issued through the syscall
   directly, because init may already be blocked in `mount_all` and cannot
   process a `sys.powerctl` request. The restart is a plain restart, never
   `RESTART2 bootloader`, so Surfacer and GBL stay in the boot path; the BCB
   command is what routes the next restart into the one-shot fastboot path.
   Under AOSP's `androidboot.init_fatal_panic=true` (the lab crash profile) the
   watchdog writes sysrq `c` first, as esuinit's fatal path does, so a 900e
   minidump keeps the printk ring (`md_KLOGBUF.BIN`; the console dump
   `md_KCONSOLE.BIN` drops these info-level notes).

Every step is best effort: a missing ESP mount, a missing misc partition or a
failed write is logged and never delays the restart. Failure of the restart
itself is retried twice more, then reported as a nonzero exit.

Device status (2026-10-08): the first enforcing hangs left the BCB request and the
reset but no receipt. The printk ring of 900e dump `20261008T105049Z-900e-2` showed
why: both remounts failed with EINVAL because the request lacked `MS_REMOUNT` (a
null-source fresh mount), and the read-only/read-write order was inverted. With
both fixed, a 6 s test deadline on an enforcing boot (payload `ba39ed2f79a4`, dump
`20261008T111534Z-900e-2`) wrote 65536 bytes to `esu/receipts/watchdog.txt`,
restored the read-only mount, recorded the cause in misc and panicked; the next
Surfacer consumed the command, and the ESP read back the receipt
(`20261008T111832Z-phone-efvs-esp-read`).

## Scope

The watchdog is a lab instrument, not part of the product boot contract. It
cannot repair a stuck boot and it does not classify the failure: it exists so a
phone that never finishes booting still leaves the ESP receipt, a recorded cause
and an observable reset instead of hanging with nobody able to press buttons.
Do not add it to a payload that must boot unattended on stock Android.
