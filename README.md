# uefinvmensmgr

A UEFI application that attaches or detaches an NVMe namespace — the moral
equivalent of `nvme attach-ns` / `nvme detach-ns` — then resets back into the
normal boot flow.

## Why

NVMe namespace attachment state is sticky across power cycles. If something
that normally manages attachment from the OS (`nvme attach-ns` /
`nvme detach-ns`) doesn't get the chance to run — say, a hard shutdown — a
detached namespace stays invisible, along with anything you keep on it. This
tool fixes that from the firmware boot menu (or any UEFI launch path),
without booting an OS at all.

## How it works

- Finds NVMe controllers via `EFI_NVM_EXPRESS_PASS_THRU_PROTOCOL`, which lives
  on the controller handle and therefore works while the namespace is detached.
- Identifies the right controller by Identify Controller serial number (or
  requires the system to have exactly one).
- Sends Namespace Attachment (admin opcode `15h`, SEL 0=attach / 1=detach)
  with a controller list defaulting to the controller's own CNTLID, exactly
  like `nvme attach-ns` without `-c`.
- Judges success by re-reading the active namespace ID list (Identify CNS
  `02h`), so "already attached" counts as success — the tool is idempotent.
- Warm-resets. Attachment state persists across all resets (NVMe Base Spec
  §5.22), so the re-enumerated namespace is visible from then on.

## Configuration

There is deliberately **no default action**: unless `op=` and `nsid=` are
provided, the tool prints usage and exits without touching anything.
Settings come from two places, with the command line winning:

1. **Config file** — a `.cfg` next to the binary with the same base name
   (`\EFI\BOOT\BOOTX64.EFI` → `\EFI\BOOT\BOOTX64.cfg`). Same `key=value`
   syntax, one or more per line, `#` starts a comment. The file is read from
   whatever volume the binary was loaded from, so it works for firmware
   "boot from file" and travels along on a USB stick. Renamed copies of the
   binary get their own configs (`attach.efi`+`attach.cfg`,
   `detach.efi`+`detach.cfg`).
2. **Command line** — UEFI shell arguments or a boot entry's optional data.

Keys (run with `help` to see this plus what the config file resolved to):

```
op=attach|detach       namespace attachment operation      (required)
nsid=<n>               target namespace id                 (required)
serial=<text>          match controller by Identify serial (default: require sole controller)
ctrl=<id>[,<id>...]    attachment controller list          (default: controller's own CNTLID)
countdown=<seconds>    ESC-to-cancel delay                 (default: 5)
reset=warm|cold|shutdown|none                              (default: warm)
```

Example config file for a recovery USB stick, at `\EFI\BOOT\BOOTX64.cfg`:

```
# re-attach the hidden namespace, then reset into the normal boot flow
op=attach nsid=1
serial=S6XNNF0R123456   # only touch this drive
```

For the UEFI shell it's `fs0:\uefinvmensmgr.efi op=attach nsid=1`, and for a
boot entry:

```sh
# note: pass the whole command line as ONE quoted -u argument; efibootmgr
# NUL-joins multiple words, which this tool (deliberately) rejects
efibootmgr -c -d /dev/nvme0n1 -p 1 -L 'Attach NVMe namespace' \
  -l '\EFI\nvmensmgr\uefinvmensmgr.efi' -u 'op=attach nsid=1'
# efibootmgr -c prepends to BootOrder — remove it or you build a reboot loop:
efibootmgr -o <previous boot order>
```

Launch it one-shot with `efibootmgr -n XXXX` (BootNext) or by picking the
entry in the firmware boot menu. The tool shows its plan and counts down;
ESC cancels without touching anything.

**Secure Boot**: a Boot#### entry launches the binary directly, so shim/MOK
does not apply. Sign the binary with a db-enrolled key (e.g. sbctl) or run
with Secure Boot disabled. Note that the `.cfg` file (like the command line)
is not covered by the signature.

## Building

```sh
nix build                 # → result/bin/uefinvmensmgr.efi (x86_64)
nix build .#esp           # → ESP-shaped dir with EFI/BOOT/BOOTX64.EFI
nix run .#qemu-test       # end-to-end test under OVMF (see below)
nix flake check           # build + clippy + fmt + qemu integration test
```

Or `cargo build` inside the dev shell (`direnv allow` / `nix develop`);
`.cargo/config.toml` defaults the target to `x86_64-unknown-uefi`.

## Testing

`nix run .#qemu-test` boots the binary under OVMF in QEMU against an emulated
NVMe subsystem, twice: once with namespace 1 starting **detached** and a
config file requesting attach, once starting attached with a config file
requesting detach. Both assert the state change via the active-NSID list.
No real hardware is touched.

To test the failure paths interactively, the dev shell has `qemu`; run the
script's command line by hand and add e.g. `detached=off` (already-attached
path) or drop the NVMe device entirely (no-controller path).
