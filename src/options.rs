//! Configuration input discovery: a config file next to the binary, and the
//! two ways this binary gets launched with arguments — the UEFI shell
//! (`ShellParameters`, real argv) and a boot manager `Boot####` entry
//! (`LoadedImage` load options, raw OptionalData).
//!
//! Firmware "boot from file" provides no command line; the config file (read
//! from the same volume and directory the image was loaded from) still works
//! there. Without either source the tool refuses to act.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use uefi::proto::device_path::media::FilePath;
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::shell_params::ShellParameters;
use uefi::{boot, CString16};

/// Read the config file next to the binary, if present: same directory,
/// same base name, `.cfg` extension (e.g. `\EFI\BOOT\BOOTX64.EFI` →
/// `\EFI\BOOT\BOOTX64.cfg`). Returns `(display path, content)`.
pub fn read_config_file() -> Option<(String, String)> {
    let (directory, stem) = image_location()?;
    let path = format!("{directory}\\{stem}.cfg");
    let path16 = CString16::try_from(path.as_str()).ok()?;
    let filesystem = boot::get_image_file_system(boot::image_handle()).ok()?;
    let content = uefi::fs::FileSystem::new(filesystem)
        .read(uefi::fs::Path::new(&path16))
        .ok()?;
    // Tolerate a UTF-8 BOM from well-meaning text editors.
    let text = String::from_utf8_lossy(&content)
        .trim_start_matches('\u{feff}')
        .to_string();
    Some((path, text))
}

/// Directory and file stem of our own image on its filesystem, e.g.
/// `("\EFI\BOOT", "BOOTX64")`. The directory is empty for the volume root.
fn image_location() -> Option<(String, String)> {
    let loaded_image = boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle()).ok()?;
    let device_path = loaded_image.file_path()?;
    let mut segments: Vec<String> = Vec::new();
    for node in device_path.node_iter() {
        if let Ok(file_path) = <&FilePath>::try_from(node) {
            let segment = file_path.path_name().to_cstring16().ok()?;
            segments.push(segment.to_string());
        }
    }
    let full = segments.join("\\");
    let full = full.trim_start_matches('\\').trim_end_matches('\\');
    let (directory, file_name) = match full.rsplit_once('\\') {
        Some((directory, file_name)) => (format!("\\{directory}"), file_name),
        None => (String::new(), full),
    };
    let stem = file_name
        .rsplit_once('.')
        .map_or(file_name, |(stem, _)| stem);
    if stem.is_empty() {
        return None;
    }
    Some((directory, stem.to_string()))
}

/// Whether we were launched by the UEFI shell (used to decide if help/error
/// output needs a keypress pause, or if there's a prompt to return to).
pub fn launched_from_shell() -> bool {
    boot::open_protocol_exclusive::<ShellParameters>(boot::image_handle()).is_ok()
}

pub fn command_line() -> Option<String> {
    if launched_from_shell() {
        // The shell is authoritative: do NOT fall back to LoadOptions, which
        // the EDK2 shell also fills with the full command line *including*
        // the program name (argv[0]) — that would misparse a bare launch.
        from_shell_parameters()
    } else {
        from_load_options()
    }
}

/// Launched from the UEFI shell: argv[0] is our own path, the rest are args.
fn from_shell_parameters() -> Option<String> {
    let params = boot::open_protocol_exclusive::<ShellParameters>(boot::image_handle()).ok()?;
    let args: Vec<String> = params.args().skip(1).map(ToString::to_string).collect();
    if args.is_empty() {
        None
    } else {
        Some(args.join(" "))
    }
}

/// Launched as a boot option: LoadOptions is the Boot#### entry's
/// OptionalData, verbatim. `efibootmgr -u '...'` stores UCS-2 with *no*
/// trailing NUL (and NUL-joins multiple trailing words), so we decode the
/// raw bytes as UTF-16LE ourselves instead of using `load_options_as_cstr16`.
fn from_load_options() -> Option<String> {
    let loaded_image = boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle()).ok()?;
    let bytes = loaded_image.load_options_as_bytes()?;
    let units = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|&unit| unit != 0);
    let decoded: String = char::decode_utf16(units)
        .map(|result| result.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();
    let trimmed = decoded.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
