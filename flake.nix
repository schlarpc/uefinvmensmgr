{
  description = "UEFI tool that attaches/detaches an NVMe namespace, then resets";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    systems.url = "github:nix-systems/default";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    breakbulk = {
      url = "github:schlarpc/breakbulk";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    # nix-direnv for the development shell
    nix-direnv = {
      url = "github:nix-community/nix-direnv";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, systems, rust-overlay, breakbulk, nix-direnv, ... }:
    let
      eachSystem = nixpkgs.lib.genAttrs (import systems);

      # The firmware target we cross-compile to; host tests never run because
      # the artifact is a PE/COFF .efi binary, not a host executable.
      uefiTargets = {
        x86_64 = "x86_64-unknown-uefi";
        aarch64 = "aarch64-unknown-uefi";
      };

      # Helper to get pkgs for a system with rust-overlay applied
      pkgsFor = system: import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };

      # Rust toolchain - pinned via rust-toolchain.toml (single source of truth).
      # Its `targets` list pulls in the prebuilt core/alloc for the UEFI targets.
      rustToolchainFor = system:
        let pkgs = pkgsFor system;
        in pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

      # One derivation per Cargo unit; configuration lives in nix/rust.nix.
      repoFor = system: breakbulk.lib.mkRepository {
        pkgs = pkgsFor system;
        # Only what Cargo reads: the crate sits at the repository root, so
        # its source view would otherwise include README, flake.nix, ...
        root = nixpkgs.lib.fileset.toSource {
          root = ./.;
          fileset = nixpkgs.lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./.cargo
            ./src
          ];
        };
        modules = [ ./nix/rust.nix ];
      };

      # Build the .efi binary. Configuration is NOT baked in; deployment
      # pairs the binary with a <basename>.cfg file (see README).
      mkPackageFor = system: { uefiTarget ? uefiTargets.x86_64 }:
        (repoFor system).workspaces.main.target.${uefiTarget}.packages.uefinvmensmgr;

      # Directory shaped like an ESP: the binary at the removable-media
      # fallback path, so firmware "boot from file" / QEMU vvfat just works.
      mkEspFor = system: package:
        let pkgs = pkgsFor system;
        in
        pkgs.runCommand "uefinvmensmgr-esp" { } ''
          mkdir -p $out/EFI/BOOT
          cp ${package}/bin/uefinvmensmgr.efi $out/EFI/BOOT/BOOTX64.EFI
        '';

      # End-to-end test: boot OVMF in QEMU against an emulated NVMe subsystem.
      # Boot 1: namespace starts *detached*, a BOOTX64.cfg requests attach.
      # Boot 2: namespace starts attached, the cfg requests detach.
      # -no-reboot makes the app's final warm reset exit QEMU.
      mkQemuTestFor = system:
        let
          pkgs = pkgsFor system;
          esp = mkEspFor system (mkPackageFor system { });
        in
        pkgs.writeShellApplication {
          name = "uefinvmensmgr-qemu-test";
          runtimeInputs = [ pkgs.qemu ];
          text = ''
            workdir=$(mktemp -d)
            trap 'rm -rf "$workdir"' EXIT
            truncate -s 64M "$workdir/ns1.img"

            # boot <espdir> <detached> <marker>...
            boot() {
              espdir=$1; detached=$2; shift 2
              install -m 644 ${pkgs.OVMF.fd}/FV/OVMF_VARS.fd "$workdir/vars.fd"
              # accel=tcg deliberately: under KVM, OVMF's NVMe driver init
              # races QEMU's nvme device and hangs pre-BDS roughly half the
              # time (observed with QEMU 11.0.1 + OVMF 202605). TCG is
              # deterministic and still boots in well under a minute.
              timeout --foreground 300 qemu-system-x86_64 \
                -machine q35,accel=tcg -m 512M \
                -display none -serial stdio -no-reboot -net none \
                -drive if=pflash,format=raw,readonly=on,file=${pkgs.OVMF.fd}/FV/OVMF_CODE.fd \
                -drive if=pflash,format=raw,file="$workdir/vars.fd" \
                -device nvme-subsys,id=subsys0,nqn=testsubsys \
                -device nvme,serial=QEMUNVME0001,id=nvme0,subsys=subsys0 \
                -drive file="$workdir/ns1.img",if=none,format=raw,id=nvm1 \
                -device nvme-ns,drive=nvm1,bus=nvme0,nsid=1,detached="$detached" \
                -drive format=raw,file=fat:rw:"$espdir" \
                | tee "$workdir/serial.log" || true
              echo
              for marker in "$@"; do
                if ! grep -aq "$marker" "$workdir/serial.log"; then
                  echo "qemu integration test: FAIL (missing '$marker')"
                  exit 1
                fi
              done
            }

            mkdir -p "$workdir/esp-attach/EFI/BOOT" "$workdir/esp-detach/EFI/BOOT"
            cp ${esp}/EFI/BOOT/BOOTX64.EFI "$workdir/esp-attach/EFI/BOOT/"
            cp ${esp}/EFI/BOOT/BOOTX64.EFI "$workdir/esp-detach/EFI/BOOT/"
            printf 'op=attach nsid=1\ncountdown=2\n' > "$workdir/esp-attach/EFI/BOOT/BOOTX64.cfg"
            printf 'op=detach nsid=1\ncountdown=2\n' > "$workdir/esp-detach/EFI/BOOT/BOOTX64.cfg"

            echo "=== boot 1: detached namespace, cfg requests attach ==="
            boot "$workdir/esp-attach" on \
              "BOOTX64.cfg: loaded" \
              "RESULT: PASS (nsid 1 is now attached)"

            echo "=== boot 2: attached namespace, cfg requests detach ==="
            boot "$workdir/esp-detach" off \
              "BOOTX64.cfg: loaded" \
              "RESULT: PASS (nsid 1 is now detached)"

            echo "qemu integration test: PASS"
          '';
        };
    in
    {
      # The main package output
      packages = eachSystem (system:
        let pkgs = pkgsFor system;
        in
        rec {
          default = mkPackageFor system { };
          uefinvmensmgr = default;
          uefinvmensmgr-aarch64 = mkPackageFor system { uefiTarget = uefiTargets.aarch64; };
          esp = mkEspFor system default;
        } // nixpkgs.lib.optionalAttrs (pkgsFor system).stdenv.isLinux {
          qemu-test = mkQemuTestFor system;
        });

      apps = eachSystem (system:
        nixpkgs.lib.optionalAttrs (pkgsFor system).stdenv.isLinux {
          qemu-test = {
            type = "app";
            program = "${mkQemuTestFor system}/bin/uefinvmensmgr-qemu-test";
          };
        });

      # Checks run by `nix flake check`
      checks = eachSystem (system:
        let
          pkgs = pkgsFor system;
          rustToolchain = rustToolchainFor system;
        in
        # Builds of each product, clippy, noSourceReferences.
        (repoFor system).checks // {
          # Check formatting
          fmt = pkgs.runCommand "uefinvmensmgr-fmt"
            { nativeBuildInputs = [ rustToolchain ]; }
            ''
              cd ${./.}
              cargo fmt --check
              touch $out
            '';
        } // nixpkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          # Boot the binary under OVMF against an emulated NVMe subsystem
          integration = pkgs.runCommand "uefinvmensmgr-integration"
            { nativeBuildInputs = [ (mkQemuTestFor system) ]; }
            ''
              uefinvmensmgr-qemu-test
              touch $out
            '';
        });

      # Development shell
      devShells = eachSystem (system:
        let
          pkgs = pkgsFor system;
          rustToolchain = rustToolchainFor system;
        in
        {
          default = pkgs.mkShell {
            nativeBuildInputs = [
              # Rust toolchain (includes rust-analyzer, rustfmt, clippy)
              rustToolchain

              # Watch mode for rapid development
              pkgs.bacon

              # Dependency management
              pkgs.cargo-edit

              # Security auditing
              pkgs.cargo-audit

              # Macro expansion (debugging)
              pkgs.cargo-expand

              # nix-direnv for this flake's shell
              nix-direnv.packages.${system}.default
            ] ++ nixpkgs.lib.optionals pkgs.stdenv.isLinux [
              # Manual QEMU/OVMF testing
              pkgs.qemu
            ];

            RUST_BACKTRACE = "1";
          };
        });

      # Expose nix-direnv for .envrc to use
      lib = {
        inherit nix-direnv;
      };
    };
}
