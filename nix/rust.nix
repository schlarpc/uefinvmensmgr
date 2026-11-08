# breakbulk configuration: the Cargo workspace at the repository root, built
# only for the UEFI targets (the crate is #![no_std]/#![no_main] and cannot
# build for the host).
{ pkgs, ... }:
let
  # rust-toolchain.toml stays the single source of truth; its `targets` list
  # brings the prebuilt core/alloc for the UEFI triples.
  tc = pkgs.rust-bin.fromRustupToolchainFile ../rust-toolchain.toml;
  # rustc links UEFI images with lld-link, which writes a PDB for every
  # link whatever -C debuginfo says (as on msvc); without this breakbulk
  # warns "discarding unlisted rustc output *.pdb".
  uefiTraits = {
    debugSideFile = "pdb";
    linkerWritesDebugFile = true;
  };
in
{
  workspaces.main = {
    path = ".";
    toolchain = {
      rustc = tc;
      cargo = tc;
      # Defaults to pkgs.clippy, which is built against nixpkgs' rustc.
      clippy = tc;
    };
    targets = {
      # No test harness can run on firmware; the QEMU check is the test.
      x86_64-unknown-uefi = {
        tests = false;
        traits = uefiTraits;
      };
      aarch64-unknown-uefi = {
        tests = false;
        traits = uefiTraits;
      };
    };
    # Listing any variant replaces the default `{ default = { }; }`.
    variants.default = { };
    variants.clippy = {
      mode = "clippy";
      # No --all-targets: test harnesses can't build for UEFI.
      allTargets = false;
      clippyArgs = [ "--deny" "warnings" ];
      targets = [ "x86_64-unknown-uefi" ];
    };
  };
}
