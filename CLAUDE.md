# uefinvmensmgr - Agent Documentation

Technical guidance for AI agents working on this Rust + Nix codebase. Derived
from the schlarpc/rust-flake template, adapted for a `no_std` UEFI target.

## Quick Reference

| Task | Command |
|------|---------|
| Build (debug) | `cargo build` (target defaults to `x86_64-unknown-uefi`) |
| Build (release) | `cargo build --release` |
| Lint | `cargo clippy` (NOT `--all-targets`: test harnesses can't build for UEFI) |
| Format | `cargo fmt` |
| Nix build | `nix build` → `result/bin/uefinvmensmgr.efi` |
| End-to-end test | `nix run .#qemu-test` |
| Nix check all | `nix flake check` |

There is no `cargo test` / nextest / coverage: the artifact is a PE/COFF
`.efi` binary that cannot execute on the build host. The QEMU/OVMF
integration test (`checks.integration`, `apps.qemu-test`) is the test suite —
it boots the binary twice against an emulated NVMe subsystem (attach a
detached namespace, then detach an attached one, both configured via a
`BOOTX64.cfg`) and asserts state changes via the serial log (`RESULT: PASS`).

## What this program does

Re-attaches (or detaches) an NVMe namespace: sends NVMe Namespace Attachment
(admin opcode `15h`) through `EFI_NVM_EXPRESS_PASS_THRU_PROTOCOL`, then
warm-resets. See README.md for deployment instructions.

## Architecture

| File | Role |
|------|------|
| `src/main.rs` | Entry, plan display, ESC countdown, help/usage, reset; `#![no_std]`/`#![no_main]` |
| `src/config.rs` | `key=value` settings accumulator + validation; `op`/`nsid` are required |
| `src/options.rs` | Input discovery: `.cfg` file next to the binary, ShellParameters (UEFI shell), LoadedImage LoadOptions (boot entry) |
| `src/nvme.rs` | Controller discovery, Identify parsing, Namespace Attachment |

Key invariants:

- **No default action** (explicit user decision): `op=` and `nsid=` must come
  from the config file or command line; a bare launch prints usage and must
  not touch anything. Config precedence: `.cfg` file < command line.
- The config file is `<image directory>\<image basename>.cfg` on the volume
  the image was loaded from — that's what makes firmware "boot from file"
  (and USB sticks) work, and lets renamed copies carry distinct configs.
- **Outcome is judged by state, not command status**: firmware collapses NVMe
  completion statuses to `EFI_DEVICE_ERROR`, and benign errors ("already
  attached") must count as success. Always re-check the active NSID list
  (Identify CNS `02h`) after Namespace Attachment.
- LoadOptions from `efibootmgr -u` are UCS-2 with **no trailing NUL**, so
  `options.rs` decodes raw UTF-16LE bytes; do not switch to
  `load_options_as_cstr16()`.
- A detached namespace has no BlockIO handle; everything goes through the
  pass-thru protocol on the *controller* handle. Getting a namespace handle
  for a detached NSID relies on EDK2's `GetNextNamespace` iterating `1..=NN`
  regardless of attach state; if some firmware doesn't, the fallback plan is
  a raw `uefi-raw` pass-thru call (not yet implemented).

## uefi crate notes (v0.38)

- NVMe support (`uefi::proto::nvme`) needs the `alloc` feature; landed in 0.35.
- `boot::stall` takes `core::time::Duration`.
- `NvmeRequestBuilder::with_transfer_buffer` allocates; `use_transfer_buffer`
  takes a caller-filled `AlignedBuffer` (needed for host→controller payloads
  like the controller list). `AlignedBuffer` only exposes raw pointers —
  the one `unsafe` block in the codebase fills it.

## Build System

Same shape as rust-flake (crane + rust-overlay + rust-toolchain.toml as
toolchain source of truth), with UEFI adaptations:

- `CARGO_BUILD_TARGET = x86_64-unknown-uefi` in crane's commonArgs (and
  `.cargo/config.toml` for the dev shell); `doCheck = false` everywhere.
- `rust-toolchain.toml` lists the UEFI targets (Tier 2: prebuilt core/alloc,
  stable channel, no build-std).
- Nothing is baked into the binary; deployment pairs it with a `.cfg` file.
  `packages.esp` wraps the bare binary at `EFI/BOOT/BOOTX64.EFI`.
- clippy check runs without `--all-targets` and with `-- --deny warnings`.

Flakes only see git-tracked files: `git add` new files before `nix build`.

## Code Style

- rustfmt via `rustfmt.toml` (max width 100, 4 spaces). Run `cargo fmt`.
- Clippy config in `Cargo.toml` `[lints]`; `unwrap_used` warns (and CI denies
  warnings) — return `Result<_, String>` messages instead of unwrapping.
- `unsafe_code` warns: keep unsafe confined to the `AlignedBuffer` fill with
  `#[allow(unsafe_code)]` and a justification comment.
