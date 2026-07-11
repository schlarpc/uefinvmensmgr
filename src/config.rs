//! Runtime configuration, two sources with the later winning:
//! `uefinvmensmgr.cfg` next to the binary, then the command line (UEFI load
//! options or shell arguments).
//!
//! There is deliberately **no built-in action**: `op=` and `nsid=` must be
//! configured explicitly or the tool refuses to run. Behavioral settings
//! (countdown, reset) have neutral defaults.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use crate::options;

/// Namespace Attachment SEL values (NVMe Base Spec figure 295).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Operation {
    Attach,
    Detach,
}

impl Operation {
    pub fn sel(self) -> u32 {
        match self {
            Operation::Attach => 0,
            Operation::Detach => 1,
        }
    }
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operation::Attach => write!(f, "attach"),
            Operation::Detach => write!(f, "detach"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResetMode {
    Warm,
    Cold,
    Shutdown,
    /// Return to the firmware (boot menu / next boot entry) instead of resetting.
    None,
}

impl fmt::Display for ResetMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResetMode::Warm => write!(f, "warm"),
            ResetMode::Cold => write!(f, "cold"),
            ResetMode::Shutdown => write!(f, "shutdown"),
            ResetMode::None => write!(f, "none"),
        }
    }
}

/// Outcome of configuration gathering: either a config to act on, or the
/// user asked for help.
pub enum Gathered {
    Run(Config),
    Help,
}

/// A fully validated configuration, ready to act on.
pub struct Config {
    pub operation: Operation,
    pub nsid: u32,
    /// Match the NVMe controller by Identify serial number; `None` means
    /// "require exactly one NVMe controller in the system".
    pub serial: Option<String>,
    /// Controller IDs for the attachment's controller list; `None` means
    /// "use the target controller's own CNTLID" (like nvme-cli without -c).
    pub controller_ids: Option<Vec<u16>>,
    /// Seconds to show the plan and allow ESC before acting.
    pub countdown_secs: u32,
    pub reset: ResetMode,
}

impl Config {
    /// Full config: config file next to the binary (if present), then
    /// command-line overrides (if present). Fails unless `op` and `nsid`
    /// were configured by one of them.
    pub fn gather() -> Result<Gathered, String> {
        let mut settings = Settings::new();
        match options::read_config_file() {
            Some((path, content)) => {
                uefi::println!("config file {path}: loaded");
                settings
                    .apply_config_text(&content)
                    .map_err(|e| format!("in {path}: {e}"))?;
            }
            None => uefi::println!("no config file found next to the binary"),
        }
        match options::command_line() {
            Some(cmdline) => {
                if cmdline
                    .split_whitespace()
                    .any(|token| matches!(token, "help" | "-h" | "--help" | "-?" | "/?"))
                {
                    return Ok(Gathered::Help);
                }
                uefi::println!("command line: {cmdline}");
                settings.apply_command_line(&cmdline)?;
            }
            None => uefi::println!("no command line"),
        }
        settings.finalize().map(Gathered::Run)
    }

    /// The effective settings from the config file, for usage output.
    pub fn configured_summary() -> String {
        let mut settings = Settings::new();
        match options::read_config_file() {
            Some((path, content)) => match settings.apply_config_text(&content) {
                Ok(()) => format!("{settings} (from {path})"),
                Err(message) => format!("<invalid config file {path}: {message}>"),
            },
            None => String::from("<no config file found>"),
        }
    }
}

/// Settings accumulator; `operation`/`nsid` stay unset until configured.
struct Settings {
    operation: Option<Operation>,
    nsid: Option<u32>,
    serial: Option<String>,
    controller_ids: Option<Vec<u16>>,
    countdown_secs: u32,
    reset: ResetMode,
}

impl Settings {
    fn new() -> Settings {
        Settings {
            operation: None,
            nsid: None,
            serial: None,
            controller_ids: None,
            countdown_secs: 5,
            reset: ResetMode::Warm,
        }
    }

    fn finalize(self) -> Result<Config, String> {
        let (Some(operation), Some(nsid)) = (self.operation, self.nsid) else {
            return Err(String::from(
                "nothing to do: op= and nsid= must be set via the .cfg file or command line",
            ));
        };
        Ok(Config {
            operation,
            nsid,
            serial: self.serial,
            controller_ids: self.controller_ids,
            countdown_secs: self.countdown_secs,
            reset: self.reset,
        })
    }

    /// Parse config file text: `key=value` tokens, `#` starts a comment.
    fn apply_config_text(&mut self, text: &str) -> Result<(), String> {
        for line in text.lines() {
            let line = line.split_once('#').map_or(line, |(before, _)| before);
            self.apply_command_line(line)?;
        }
        Ok(())
    }

    /// Parse a `key=value key=value ...` command line.
    fn apply_command_line(&mut self, cmdline: &str) -> Result<(), String> {
        for token in cmdline.split_whitespace() {
            let (key, value) = token
                .split_once('=')
                .ok_or_else(|| format!("malformed argument '{token}' (expected key=value)"))?;
            self.set(key, value)?;
        }
        Ok(())
    }

    fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "op" => {
                self.operation = match value {
                    "attach" => Some(Operation::Attach),
                    "detach" => Some(Operation::Detach),
                    _ => return Err(format!("op must be attach|detach, got '{value}'")),
                }
            }
            "nsid" => {
                let nsid = value
                    .parse::<u32>()
                    .map_err(|_| format!("bad nsid '{value}'"))?;
                if nsid == 0 || nsid == u32::MAX {
                    return Err(format!("nsid {nsid} is not a valid target"));
                }
                self.nsid = Some(nsid);
            }
            "serial" => {
                self.serial = if value.is_empty() {
                    None
                } else {
                    Some(value.to_string())
                };
            }
            "ctrl" => {
                let ids = value
                    .split(',')
                    .map(|v| v.parse::<u16>().map_err(|_| format!("bad ctrl id '{v}'")))
                    .collect::<Result<Vec<u16>, String>>()?;
                self.controller_ids = if ids.is_empty() { None } else { Some(ids) };
            }
            "countdown" => {
                self.countdown_secs = value
                    .parse::<u32>()
                    .map_err(|_| format!("bad countdown '{value}'"))?;
            }
            "reset" => {
                self.reset = match value {
                    "warm" => ResetMode::Warm,
                    "cold" => ResetMode::Cold,
                    "shutdown" => ResetMode::Shutdown,
                    "none" => ResetMode::None,
                    _ => {
                        return Err(format!(
                            "reset must be warm|cold|shutdown|none, got '{value}'"
                        ))
                    }
                }
            }
            _ => return Err(format!("unknown argument '{key}'")),
        }
        Ok(())
    }
}

impl fmt::Display for Settings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "op={} nsid={} serial={} ctrl={} countdown={} reset={}",
            self.operation
                .map(|op| op.to_string())
                .unwrap_or_else(|| String::from("<unset>")),
            self.nsid
                .map(|nsid| nsid.to_string())
                .unwrap_or_else(|| String::from("<unset>")),
            self.serial.as_deref().unwrap_or("<sole controller>"),
            self.controller_ids
                .as_ref()
                .map(|ids| format!("{ids:?}"))
                .unwrap_or_else(|| String::from("<own cntlid>")),
            self.countdown_secs,
            self.reset,
        )
    }
}
