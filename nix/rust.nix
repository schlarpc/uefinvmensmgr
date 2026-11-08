# breakbulk configuration: the Cargo workspace at the repository root, built
# only for the UEFI targets (the crate is #![no_std]/#![no_main] and cannot
# build for the host).
{ pkgs, ... }:
let
  # rust-toolchain.toml stays the single source of truth; its `targets` list
  # brings the prebuilt core/alloc for the UEFI triples.
  tc = pkgs.rust-bin.fromRustupToolchainFile ../rust-toolchain.toml;
in
{
  workspaces.main = {
    path = ".";
    # clippy defaults to this toolchain too.
    toolchain = {
      rustc = tc;
      cargo = tc;
    };
    targets = {
      # No test harness can run on firmware; the QEMU check is the test.
      x86_64-unknown-uefi.tests = false;
      aarch64-unknown-uefi.tests = false;
    };
    variants.clippy = {
      mode = "clippy";
      # No --all-targets: test harnesses can't build for UEFI.
      allTargets = false;
      clippyArgs = [ "--deny" "warnings" ];
      targets = [ "x86_64-unknown-uefi" ];
    };
  };
}
