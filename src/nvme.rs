//! NVMe controller discovery and admin commands via the UEFI
//! `EFI_NVM_EXPRESS_PASS_THRU_PROTOCOL`.
//!
//! The pass-thru protocol lives on the *controller* handle, so it works even
//! when the namespace we care about is currently detached. Attachment state
//! is committed at command completion and persists across all resets (NVMe
//! Base Spec §5.22), so a plain warm reset afterwards is enough for the
//! firmware and OS to re-enumerate.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use uefi::boot::{self, ScopedProtocol};
use uefi::proto::nvme::pass_thru::NvmePassThru;
use uefi::proto::nvme::{NvmeQueueType, NvmeRequestBuilder};

use crate::config::Operation;

const OPCODE_IDENTIFY: u8 = 0x06;
const OPCODE_NS_ATTACHMENT: u8 = 0x15;
const CNS_IDENTIFY_CONTROLLER: u32 = 0x01;
const CNS_ACTIVE_NSID_LIST: u32 = 0x02;
const IDENTIFY_LEN: usize = 4096;
const CONTROLLER_LIST_LEN: usize = 4096;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Fields we need from the Identify Controller data structure.
pub struct ControllerIdentity {
    pub model: String,
    pub serial: String,
    /// This controller's ID within its subsystem (bytes 79:78).
    pub cntlid: u16,
    /// Maximum valid NSID in the subsystem (bytes 519:516).
    pub nn: u32,
    /// OACS bit 3: Namespace Management/Attachment supported (bytes 257:256).
    pub supports_ns_mgmt: bool,
}

pub struct Controller {
    pass_thru: ScopedProtocol<NvmePassThru>,
    pub identity: ControllerIdentity,
}

/// Enumerate all NVMe controllers, then pick one: by Identify serial number
/// if `serial` is set, otherwise require the system to have exactly one.
pub fn select_controller(serial: Option<&str>) -> Result<Controller, String> {
    let handles = boot::find_handles::<NvmePassThru>()
        .map_err(|e| format!("no NVMe pass-thru handles found ({e:?})"))?;

    let mut controllers = Vec::new();
    for handle in handles {
        let mut pass_thru = match boot::open_protocol_exclusive::<NvmePassThru>(handle) {
            Ok(pass_thru) => pass_thru,
            Err(e) => {
                uefi::println!("warning: skipping NVMe controller (open failed: {e:?})");
                continue;
            }
        };
        match identify_controller(&mut pass_thru) {
            Ok(identity) => {
                uefi::println!(
                    "found NVMe controller: model '{}' serial '{}' cntlid {}",
                    identity.model,
                    identity.serial,
                    identity.cntlid
                );
                controllers.push(Controller {
                    pass_thru,
                    identity,
                });
            }
            Err(e) => uefi::println!("warning: skipping NVMe controller (identify failed: {e})"),
        }
    }

    match serial {
        Some(serial) => {
            let count = controllers.len();
            controllers
                .into_iter()
                .find(|c| c.identity.serial == serial)
                .ok_or_else(|| format!("no NVMe controller with serial '{serial}' (saw {count})"))
        }
        None => {
            if controllers.len() > 1 {
                return Err(format!(
                    "{} NVMe controllers found; configure serial=... to disambiguate",
                    controllers.len()
                ));
            }
            controllers
                .pop()
                .ok_or_else(|| String::from("no usable NVMe controllers found"))
        }
    }
}

impl Controller {
    /// Is `nsid` currently in the controller's active namespace ID list
    /// (Identify CNS 02h)?
    pub fn namespace_is_active(&mut self, nsid: u32) -> Result<bool, String> {
        let request = NvmeRequestBuilder::new(
            self.pass_thru.io_align(),
            OPCODE_IDENTIFY,
            NvmeQueueType::ADMIN,
        )
        .with_timeout(COMMAND_TIMEOUT)
        .with_cdw10(CNS_ACTIVE_NSID_LIST)
        .with_transfer_buffer(IDENTIFY_LEN)
        .map_err(|e| format!("buffer allocation failed: {e:?}"))?
        .build();

        let response = self
            .pass_thru
            .controller()
            .execute_command(request)
            .map_err(|e| format!("Identify (active NSID list) failed: {e:?}"))?;
        let data = response
            .transfer_buffer()
            .ok_or("Identify returned no data")?;
        Ok(data
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .take_while(|&id| id != 0)
            .any(|id| id == nsid))
    }

    /// Namespace Attachment (opcode 15h): attach or detach `nsid` for the
    /// given controller IDs. Equivalent to `nvme attach-ns`/`detach-ns`.
    ///
    /// The completion status is not recoverable through the pass-thru wrapper
    /// on failure (firmware maps NVMe error statuses to EFI_DEVICE_ERROR), so
    /// callers should re-check `namespace_is_active` to judge the outcome —
    /// e.g. "already attached" arrives as an error but is a success for us.
    pub fn namespace_attachment(
        &mut self,
        nsid: u32,
        operation: Operation,
        controller_ids: &[u16],
    ) -> Result<(), String> {
        // Controller List (NVMe Base Spec §4.4): u16 entry count, then
        // ascending u16 controller IDs, zero-filled to 4 KiB.
        if controller_ids.is_empty() || controller_ids.len() > 2047 {
            return Err(format!(
                "controller list must have 1..=2047 entries, got {}",
                controller_ids.len()
            ));
        }
        let mut sorted_ids: Vec<u16> = controller_ids.to_vec();
        sorted_ids.sort_unstable();
        let mut list = [0u8; CONTROLLER_LIST_LEN];
        let entry_count =
            u16::try_from(sorted_ids.len()).map_err(|_| "controller list too long")?;
        list[0..2].copy_from_slice(&entry_count.to_le_bytes());
        for (i, id) in sorted_ids.iter().enumerate() {
            list[2 + i * 2..4 + i * 2].copy_from_slice(&id.to_le_bytes());
        }

        let mut buffer = self
            .pass_thru
            .alloc_io_buffer(CONTROLLER_LIST_LEN)
            .map_err(|e| format!("buffer allocation failed: {e:?}"))?;
        fill_buffer(&mut buffer, &list);

        // The command must carry the target NSID, so we need a namespace
        // handle for it. EDK2's GetNextNamespace iterates 1..=NN regardless
        // of attachment state, so a detached namespace is still enumerable
        // on EDK2-derived firmware (i.e. approximately all of them).
        let mut namespace = self
            .pass_thru
            .iter_namespaces()
            .find(|ns| ns.namespace_id() == nsid)
            .ok_or_else(|| {
                format!(
                    "firmware did not enumerate nsid {nsid} (max valid nsid is {}); \
                     a raw pass-thru fallback would be needed on this firmware",
                    self.identity.nn
                )
            })?;

        let request = NvmeRequestBuilder::new(
            self.pass_thru.io_align(),
            OPCODE_NS_ATTACHMENT,
            NvmeQueueType::ADMIN,
        )
        .with_timeout(COMMAND_TIMEOUT)
        .with_cdw10(operation.sel())
        .use_transfer_buffer(&mut buffer)
        .map_err(|e| format!("buffer alignment error: {e:?}"))?
        .build();

        namespace
            .execute_command(request)
            .map(|_| ())
            .map_err(|e| format!("Namespace Attachment command failed: {e:?}"))
    }
}

fn identify_controller(
    pass_thru: &mut ScopedProtocol<NvmePassThru>,
) -> Result<ControllerIdentity, String> {
    let request =
        NvmeRequestBuilder::new(pass_thru.io_align(), OPCODE_IDENTIFY, NvmeQueueType::ADMIN)
            .with_timeout(COMMAND_TIMEOUT)
            .with_cdw10(CNS_IDENTIFY_CONTROLLER)
            .with_transfer_buffer(IDENTIFY_LEN)
            .map_err(|e| format!("buffer allocation failed: {e:?}"))?
            .build();

    let response = pass_thru
        .controller()
        .execute_command(request)
        .map_err(|e| format!("Identify Controller failed: {e:?}"))?;
    let data = response
        .transfer_buffer()
        .ok_or("Identify Controller returned no data")?;
    if data.len() < IDENTIFY_LEN {
        return Err(format!("short Identify Controller data: {}", data.len()));
    }

    let oacs = u16::from_le_bytes([data[256], data[257]]);
    Ok(ControllerIdentity {
        serial: ascii_field(&data[4..24]),
        model: ascii_field(&data[24..64]),
        cntlid: u16::from_le_bytes([data[78], data[79]]),
        nn: u32::from_le_bytes([data[516], data[517], data[518], data[519]]),
        supports_ns_mgmt: oacs & (1 << 3) != 0,
    })
}

/// Identify string fields are space-padded ASCII.
fn ascii_field(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '?'
            }
        })
        .collect::<String>()
        .trim()
        .into()
}

/// Copy `data` into the start of an IO-aligned buffer.
#[allow(unsafe_code)] // AlignedBuffer exposes raw pointers, not slices
fn fill_buffer(buffer: &mut uefi::mem::AlignedBuffer, data: &[u8]) {
    assert!(data.len() <= buffer.size());
    unsafe {
        core::ptr::copy_nonoverlapping(data.as_ptr(), buffer.ptr_mut(), data.len());
    }
}
