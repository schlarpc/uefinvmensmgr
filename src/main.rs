//! uefinvmensmgr: attach or detach an NVMe namespace from UEFI, then reset.
//!
//! Purpose: recover access to a detached namespace when whatever normally
//! manages attachment from an OS didn't get the chance to run (e.g. hard
//! shutdown) — NVMe attachment state is sticky across power cycles. Launch
//! this from the firmware boot menu (or a Boot#### entry / the UEFI shell),
//! let it re-attach the namespace, and it resets back into the normal boot
//! flow.

#![no_main]
#![no_std]

extern crate alloc;

mod config;
mod nvme;
mod options;

use alloc::format;
use alloc::string::String;
use core::time::Duration;

use uefi::prelude::*;
use uefi::proto::console::text::{Key, ScanCode};
use uefi::runtime::ResetType;
use uefi::{boot, println, runtime, system};

use config::{Config, Gathered, Operation, ResetMode};

#[entry]
fn main() -> Status {
    if uefi::helpers::init().is_err() {
        return Status::ABORTED;
    }

    println!();
    println!("=== uefinvmensmgr v{} ===", env!("CARGO_PKG_VERSION"));

    let config = match Config::gather() {
        Ok(Gathered::Run(config)) => config,
        Ok(Gathered::Help) => {
            print_usage();
            if !options::launched_from_shell() {
                wait_for_key(30);
            }
            return Status::SUCCESS;
        }
        Err(message) => {
            print_usage();
            return fail(&message);
        }
    };

    println!(
        "plan: {} nsid {} on {}",
        config.operation,
        config.nsid,
        config
            .serial
            .as_deref()
            .map(|s| format!("controller with serial '{s}'"))
            .unwrap_or_else(|| String::from("the sole NVMe controller")),
    );

    if !countdown(config.countdown_secs) {
        println!("cancelled by user; no changes made");
        boot::stall(Duration::from_secs(2));
        return Status::ABORTED;
    }

    match run(&config) {
        Ok(summary) => {
            println!("RESULT: PASS ({summary})");
            finish(config.reset)
        }
        Err(message) => {
            println!("RESULT: FAIL");
            fail(&message)
        }
    }
}

fn run(config: &Config) -> Result<String, String> {
    let mut controller = nvme::select_controller(config.serial.as_deref())?;
    let identity = &controller.identity;

    if !identity.supports_ns_mgmt {
        return Err(format!(
            "controller '{}' does not support namespace management (OACS bit 3 clear)",
            identity.model
        ));
    }
    if config.nsid > identity.nn {
        return Err(format!(
            "nsid {} exceeds this subsystem's maximum valid nsid ({})",
            config.nsid, identity.nn
        ));
    }

    let want_active = config.operation == Operation::Attach;
    if controller.namespace_is_active(config.nsid)? == want_active {
        return Ok(format!(
            "nsid {} was already {}ed; nothing to do",
            config.nsid, config.operation
        ));
    }

    let controller_ids = config
        .controller_ids
        .clone()
        .unwrap_or_else(|| alloc::vec![controller.identity.cntlid]);
    println!(
        "sending namespace {} for nsid {} to controller ids {:?}",
        config.operation, config.nsid, controller_ids
    );
    let command_result =
        controller.namespace_attachment(config.nsid, config.operation, &controller_ids);

    // Judge the outcome by observed state, not the command status: firmware
    // collapses NVMe error statuses (including benign ones like "namespace
    // already attached") into a bare EFI_DEVICE_ERROR.
    if controller.namespace_is_active(config.nsid)? == want_active {
        Ok(format!(
            "nsid {} is now {}ed",
            config.nsid, config.operation
        ))
    } else {
        Err(match command_result {
            Ok(()) => format!(
                "command succeeded but nsid {} state did not change",
                config.nsid
            ),
            Err(message) => message,
        })
    }
}

/// Count down, watching for ESC. Returns false if the user cancelled.
fn countdown(seconds: u32) -> bool {
    if seconds == 0 {
        return true;
    }
    println!("press ESC to cancel");
    for remaining in (1..=seconds).rev() {
        println!("  {remaining}...");
        for _ in 0..10 {
            boot::stall(Duration::from_millis(100));
            if matches!(read_key(), Some(Key::Special(ScanCode::ESCAPE))) {
                return false;
            }
        }
    }
    true
}

fn read_key() -> Option<Key> {
    system::with_stdin(|stdin| stdin.read_key().ok().flatten())
}

fn finish(mode: ResetMode) -> Status {
    let reset_type = match mode {
        ResetMode::Warm => ResetType::WARM,
        ResetMode::Cold => ResetType::COLD,
        ResetMode::Shutdown => ResetType::SHUTDOWN,
        ResetMode::None => {
            println!("reset=none; returning to firmware");
            boot::stall(Duration::from_secs(3));
            return Status::SUCCESS;
        }
    };
    println!("resetting in 3 seconds...");
    boot::stall(Duration::from_secs(3));
    runtime::reset(reset_type, Status::SUCCESS, None);
}

fn fail(message: &str) -> Status {
    println!("error: {message}");
    println!("no destructive changes were made");
    // From the shell there's a prompt to return to; anywhere else the screen
    // vanishes on exit, so hold it until a keypress.
    if !options::launched_from_shell() {
        println!("press any key (or wait 60s) to exit");
        wait_for_key(60);
    }
    Status::ABORTED
}

/// Wait for any keypress, up to `seconds`.
fn wait_for_key(seconds: u32) {
    for _ in 0..seconds * 10 {
        boot::stall(Duration::from_millis(100));
        if read_key().is_some() {
            break;
        }
    }
}

fn print_usage() {
    println!("usage: uefinvmensmgr.efi [key=value ...] | help");
    println!("  op=attach|detach       namespace attachment operation");
    println!("  nsid=<n>               target namespace id");
    println!("  serial=<text>          match controller by Identify serial number");
    println!("                         (unset: require exactly one NVMe controller)");
    println!("  ctrl=<id>[,<id>...]    controller ids for the attachment list");
    println!("                         (unset: the controller's own CNTLID)");
    println!("  countdown=<seconds>    ESC-to-cancel delay before acting");
    println!("  reset=warm|cold|shutdown|none   what to do after success");
    println!("op= and nsid= are required; there is no default action.");
    println!("a .cfg file named after the binary (e.g. BOOTX64.cfg next to");
    println!("BOOTX64.EFI) sets defaults with the same key=value syntax and");
    println!("'#' comments; command-line arguments win over it.");
    println!("configured: {}", Config::configured_summary());
}
