//! Adaptive defaults driven by the SystemInformation.txt export the project ships.
//!
//! Windows `msinfo32` produces a tab-separated `Item\tValue` export under
//! section headers like `[System Summary]`. OpenNOW treats that file as the
//! source of truth for hardware constraints on first launch: when present
//! (next to the binary, in the data directory, or pointed at by
//! `OPENNOW_SYSTEM_INFORMATION`), it is parsed and the streaming defaults are
//! downgraded to a profile the host can actually sustain. User-set values
//! stored in settings.json are never overwritten.
//!
//! shortcut: only the fields that change runtime defaults are parsed. The full
//! export has hundreds of rows; we read RAM, CPU, BIOS mode, Secure Boot,
//! and the device-encryption failure string (which surfaces TPM usability).

use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

const LOW_MEMORY_GB_THRESHOLD: f64 = 6.0;
const LOW_MEMORY_MIB_FALLBACK: u64 = 6 * 1024;

/// Subset of a msinfo32 export that influences OpenNOW runtime defaults.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SystemInformation {
    pub os_name: Option<String>,
    pub os_version: Option<String>,
    pub system_name: Option<String>,
    pub system_manufacturer: Option<String>,
    pub system_model: Option<String>,
    pub processor: Option<String>,
    pub processor_ghz: Option<f64>,
    pub physical_cores: Option<u32>,
    pub logical_processors: Option<u32>,
    pub installed_ram_gb: Option<f64>,
    pub available_physical_memory_mib: Option<u64>,
    pub bios_mode: Option<String>,
    pub secure_boot_state: Option<String>,
    pub tpm_usable: Option<bool>,
    pub virt_enabled_in_firmware: Option<bool>,
    pub source: Option<PathBuf>,
}

/// Coarse capability bucket derived from SystemInformation. Streams of code
/// consume booleans, not raw msinfo32 strings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CapabilityProfile {
    pub low_memory: bool,
    pub legacy_cpu: bool,
    pub legacy_bios: bool,
    pub no_secure_boot: bool,
    pub no_tpm: bool,
}

impl CapabilityProfile {
    /// True when *any* constraint is set. Used to decide whether to surface
    /// the adaptive note in diagnostics.
    pub fn constrained(&self) -> bool {
        self.low_memory
            || self.legacy_cpu
            || self.legacy_bios
            || self.no_secure_boot
            || self.no_tpm
    }
}

impl SystemInformation {
    /// Parse a msinfo32 export. Tolerant of trailing tabs, blank lines, and
    /// section headers — every row we do not recognise is skipped.
    pub fn parse(text: &str) -> Self {
        let mut info = Self::default();
        for line in text.lines() {
            let trimmed = line.trim_end_matches(['\t', '\r', ' ', '\u{00a0}']);
            if trimmed.is_empty() || trimmed.starts_with('[') {
                continue;
            }
            let (key, value) = match trimmed.split_once('\t') {
                Some(pair) => pair,
                None => continue,
            };
            let key = key.trim();
            let value = value.trim();
            match key {
                "OS Name" => info.os_name = Some(value.to_owned()),
                "Version" => info.os_version = Some(value.to_owned()),
                "System Name" => info.system_name = Some(value.to_owned()),
                "System Manufacturer" => info.system_manufacturer = Some(value.to_owned()),
                "System Model" => info.system_model = Some(value.to_owned()),
                "Processor" => apply_processor(value, &mut info),
                "Installed Physical Memory (RAM)" => {
                    info.installed_ram_gb = parse_gigabytes(value);
                }
                "Available Physical Memory" => {
                    info.available_physical_memory_mib = parse_mebibytes(value);
                }
                "BIOS Mode" => info.bios_mode = Some(value.to_owned()),
                "Secure Boot State" => info.secure_boot_state = Some(value.to_owned()),
                "Automatic Device Encryption Support" => {
                    info.tpm_usable = Some(!value.to_ascii_lowercase().contains("tpm is not usable"));
                }
                "Hyper-V - Virtualization Enabled in Firmware" => {
                    info.virt_enabled_in_firmware = Some(value.eq_ignore_ascii_case("yes"));
                }
                _ => {}
            }
        }
        info
    }

    /// Read the export from one of the well-known locations, parsing only when
    /// present. Missing file is not an error — callers fall back to defaults.
    pub fn load(data_dir: &Path) -> Self {
        if let Some(path) = locate(data_dir) {
            if let Ok(text) = fs::read_to_string(&path) {
                let mut info = Self::parse(&text);
                info.source = Some(path);
                return info;
            }
        }
        Self::default()
    }

    /// Derive the capability profile from the parsed system information.
    pub fn capability_profile(&self) -> CapabilityProfile {
        let installed_gb = self.installed_ram_gb.unwrap_or(0.0);
        let low_memory = installed_gb > 0.0 && installed_gb <= LOW_MEMORY_GB_THRESHOLD;
        // No reliable RAM export but Available Memory says the box is starved.
        let low_memory = low_memory
            || self
                .available_physical_memory_mib
                .is_some_and(|mib| mib <= LOW_MEMORY_MIB_FALLBACK);

        let legacy_cpu = self
            .processor
            .as_deref()
            .is_some_and(looks_like_legacy_cpu);

        let legacy_bios = self
            .bios_mode
            .as_deref()
            .is_some_and(|mode| mode.eq_ignore_ascii_case("legacy"));

        let no_secure_boot = self
            .secure_boot_state
            .as_deref()
            .is_some_and(|state| matches!(state.to_ascii_lowercase().as_str(), "unsupported" | "off"));

        let no_tpm = self.tpm_usable.is_some_and(|usable| !usable);

        CapabilityProfile {
            low_memory,
            legacy_cpu,
            legacy_bios,
            no_secure_boot,
            no_tpm,
        }
    }
}

fn apply_processor(value: &str, info: &mut SystemInformation) {
    info.processor = Some(value.to_owned());
    if let Some((_, rest)) = value.split_once("@ ") {
        // "2.53GHz" or "2.53 GHz"
        let rest = rest.trim_start();
        let digits_end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if let Ok(ghz) = rest[..digits_end].parse::<f64>() {
            info.processor_ghz = Some(ghz);
        }
    }
    if let Some(start) = value.find("Core(s)") {
        let prefix = &value[..start];
        if let Some(digit_run) = prefix
            .rsplit(|c: char| c == ',' || c == ' ')
            .find(|part| part.chars().any(|c| c.is_ascii_digit()))
        {
            if let Ok(cores) = digit_run.trim().parse::<u32>() {
                info.physical_cores = Some(cores);
            }
        }
    }
    if let Some(start) = value.find("Logical Processor(s)") {
        let prefix = &value[..start];
        if let Some(digit_run) = prefix
            .rsplit(|c: char| c == ',' || c == ' ')
            .find(|part| part.chars().any(|c| c.is_ascii_digit()))
        {
            if let Ok(threads) = digit_run.trim().parse::<u32>() {
                info.logical_processors = Some(threads);
            }
        }
    }
}

fn parse_gigabytes(value: &str) -> Option<f64> {
    let lower = value.to_ascii_lowercase();
    let number_end = lower
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(lower.len());
    let amount = lower[..number_end].parse::<f64>().ok()?;
    if lower[number_end..].trim().starts_with("gb") {
        Some(amount)
    } else {
        None
    }
}

fn parse_mebibytes(value: &str) -> Option<u64> {
    let lower = value.to_ascii_lowercase();
    let number_end = lower
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(lower.len());
    let amount = lower[..number_end].parse::<f64>().ok()?;
    if lower[number_end..].trim().starts_with("mb") {
        Some(amount.round() as u64)
    } else {
        None
    }
}

/// Detect Intel Core parts too old to drive HEVC/AV1 hardware decode or AVX2.
/// Sandy Bridge (2011) introduced AVX; Haswell (2013) introduced AVX2 and the
/// integrated HEVC decoder OpenNOW relies on. We treat anything pre-Sandy
/// Bridge as legacy.
fn looks_like_legacy_cpu(processor: &str) -> bool {
    let lower = processor.to_ascii_lowercase();
    if lower.contains("pentium") || lower.contains("celeron") {
        return true;
    }
    // First-generation Core i3/i5/i7 mobile parts (Arrandale/Clarksfield, 2010)
    // are written like "i3 CPU M 380" — no dash-number generation suffix.
    if lower.contains("intel(r) core(tm)") && lower.contains("cpu") && lower.contains(" m ") {
        return true;
    }
    // Sandy Bridge and newer spell the SKU with a dash: "i3-2100", "i7-1165G7".
    // A bare "i3"/"i5"/"i7" without a dash-number suffix is pre-Sandy Bridge.
    if lower.contains("intel(r) core(tm)") {
        let generation = lower.split("intel(r) core(tm)").nth(1).unwrap_or("");
        if !generation.contains('-') {
            return true;
        }
    }
    false
}

/// Resolve the SystemInformation export path: env var, then data dir, then the
/// executable's parent (portable Windows zip layout).
fn locate(data_dir: &Path) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("OPENNOW_SYSTEM_INFORMATION") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let candidate = data_dir.join("SystemInformation.txt");
    if candidate.is_file() {
        return Some(candidate);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let candidate = parent.join("SystemInformation.txt");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Override defaults the user has not yet persisted. `persisted_keys` is the
/// set of keys that came from settings.json — anything missing is still at the
/// stock default and safe to nudge toward the host's capability profile.
///
/// We never touch persisted values, so a user who tuned their bitrate keeps it.
pub fn apply_adaptive_defaults(
    values: &mut Map<String, Value>,
    persisted_keys: &HashSet<String>,
    info: &SystemInformation,
) {
    let profile = info.capability_profile();
    if !profile.constrained() {
        return;
    }
    let mut override_if_unused = |key: &str, value: Value| {
        if !persisted_keys.contains(key) {
            values.insert(key.to_owned(), value);
        }
    };
    if profile.low_memory {
        override_if_unused("resolution", json!("1280x720"));
        override_if_unused("fps", json!(30));
        override_if_unused("maxBitrateMbps", json!(20));
        override_if_unused("replayBufferEnabled", json!(false));
        override_if_unused("replayBufferMemoryMiB", json!(64));
    }
    if profile.legacy_cpu {
        override_if_unused("codec", json!("h264"));
        override_if_unused("fallbackCodec", json!("h264"));
        override_if_unused("decoderPreference", json!("software"));
        override_if_unused("nativeVideoBackend", json!("software"));
        override_if_unused("enableHdr", json!(false));
        override_if_unused("colorQuality", json!("8bit_420"));
        override_if_unused("frameGeneration", json!("off"));
        override_if_unused("upscaling", json!("off"));
    }
    // legacy_bios / no_secure_boot / no_tpm do not change defaults: the
    // existing update-signing path falls back to checksum verification, and
    // telemetry defaults already require consent. We surface these in the
    // diagnostics export so support can see why a signed update behaves like a
    // checksum-only download.
}

/// Render the parsed SystemInformation as JSON for the diagnostics export.
/// Identifies the source path so the export is self-explaining.
pub fn to_json(info: &SystemInformation) -> Value {
    let profile = info.capability_profile();
    json!({
        "osName": info.os_name,
        "osVersion": info.os_version,
        "systemName": info.system_name,
        "systemManufacturer": info.system_manufacturer,
        "systemModel": info.system_model,
        "processor": info.processor,
        "processorGhz": info.processor_ghz,
        "physicalCores": info.physical_cores,
        "logicalProcessors": info.logical_processors,
        "installedRamGb": info.installed_ram_gb,
        "availablePhysicalMemoryMib": info.available_physical_memory_mib,
        "biosMode": info.bios_mode,
        "secureBootState": info.secure_boot_state,
        "tpmUsable": info.tpm_usable,
        "virtualizationEnabledInFirmware": info.virt_enabled_in_firmware,
        "source": info.source.as_ref().map(|path| path.to_string_lossy().to_string()),
        "capabilityProfile": {
            "lowMemory": profile.low_memory,
            "legacyCpu": profile.legacy_cpu,
            "legacyBios": profile.legacy_bios,
            "noSecureBoot": profile.no_secure_boot,
            "noTpm": profile.no_tpm,
            "constrained": profile.constrained()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const SAMPLE: &str = "\n[System Summary]\n\nItem\tValue\t\nOS Name\tMicrosoft Windows 11 Enterprise\t\nVersion\t10.0.26200 Build 26200\t\nSystem Name\tWIN-HGRDCPHO2DP\t\nSystem Manufacturer\tLENOVO\t\nSystem Model\t2539A12\t\nSystem Type\tx64-based PC\t\nProcessor\tIntel(R) Core(TM) i3 CPU       M 380  @ 2.53GHz, 2533 Mhz, 2 Core(s), 4 Logical Processor(s)\t\nBIOS Version/Date\tLENOVO 6IET80WW (1.40 ), 12/1/2011\t\nBIOS Mode\tLegacy\t\nSecure Boot State\tUnsupported\t\nPCR7 Configuration\tBinding Not Possible\t\nInstalled Physical Memory (RAM)\t4.00 GB\t\nTotal Physical Memory\t3.86 GB\t\nAvailable Physical Memory\t732 MB\t\nKernel DMA Protection\tOff\t\nVirtualization-based security\tNot enabled\t\nAutomatic Device Encryption Support\tReasons for failed automatic device encryption: TPM is not usable, PCR7 binding is not supported, Hardware Security Test Interface failed and device is not Modern Standby, Un-allowed DMA capable bus/device(s) detected, TPM is not usable\t\nHyper-V - Virtualization Enabled in Firmware\tYes\t\n";

    #[test]
    fn parses_system_summary_fields() {
        let info = SystemInformation::parse(SAMPLE);
        assert_eq!(info.os_name.as_deref(), Some("Microsoft Windows 11 Enterprise"));
        assert_eq!(info.system_name.as_deref(), Some("WIN-HGRDCPHO2DP"));
        assert_eq!(info.system_manufacturer.as_deref(), Some("LENOVO"));
        assert_eq!(info.system_model.as_deref(), Some("2539A12"));
        assert_eq!(
            info.processor.as_deref(),
            Some("Intel(R) Core(TM) i3 CPU       M 380  @ 2.53GHz, 2533 Mhz, 2 Core(s), 4 Logical Processor(s)")
        );
        assert_eq!(info.processor_ghz, Some(2.53));
        assert_eq!(info.physical_cores, Some(2));
        assert_eq!(info.logical_processors, Some(4));
        assert_eq!(info.installed_ram_gb, Some(4.0));
        assert_eq!(info.available_physical_memory_mib, Some(732));
        assert_eq!(info.bios_mode.as_deref(), Some("Legacy"));
        assert_eq!(info.secure_boot_state.as_deref(), Some("Unsupported"));
        assert_eq!(info.tpm_usable, Some(false));
        assert_eq!(info.virt_enabled_in_firmware, Some(true));
    }

    #[test]
    fn detects_legacy_arrandale_cpu() {
        assert!(looks_like_legacy_cpu(
            "Intel(R) Core(TM) i3 CPU       M 380  @ 2.53GHz, 2533 Mhz, 2 Core(s), 4 Logical Processor(s)"
        ));
        assert!(!looks_like_legacy_cpu(
            "Intel(R) Core(TM) i7-9700K CPU @ 3.60GHz, 3600 Mhz, 8 Core(s), 8 Logical Processor(s)"
        ));
        assert!(!looks_like_legacy_cpu(
            "Intel(R) Core(TM) i5-8250U CPU @ 1.60GHz, 1800 Mhz, 4 Core(s), 8 Logical Processor(s)"
        ));
        assert!(looks_like_legacy_cpu("Pentium(R) CPU P6200 @ 2.13GHz"));
        assert!(!looks_like_legacy_cpu("AMD Ryzen 5 3600 6-Core Processor"));
    }

    #[test]
    fn profile_matches_shipped_systeminformation() {
        let info = SystemInformation::parse(SAMPLE);
        let profile = info.capability_profile();
        assert!(profile.low_memory);
        assert!(profile.legacy_cpu);
        assert!(profile.legacy_bios);
        assert!(profile.no_secure_boot);
        assert!(profile.no_tpm);
        assert!(profile.constrained());
    }

    #[test]
    fn modern_system_is_unconstrained() {
        let text = "\n[System Summary]\n\nProcessor\tIntel(R) Core(TM) i7-12700K CPU @ 3.60GHz, 3600 Mhz, 12 Core(s), 20 Logical Processor(s)\t\nInstalled Physical Memory (RAM)\t32.00 GB\t\nBIOS Mode\tUEFI\t\nSecure Boot State\tOn\t\nAutomatic Device Encryption Support\tDevice encryption is enabled.\t\n";
        let info = SystemInformation::parse(text);
        let profile = info.capability_profile();
        assert!(!profile.low_memory);
        assert!(!profile.legacy_cpu);
        assert!(!profile.legacy_bios);
        assert!(!profile.no_secure_boot);
        assert!(!profile.no_tpm);
        assert!(!profile.constrained());
    }

    #[test]
    fn adaptive_defaults_skip_persisted_keys() {
        let info = SystemInformation::parse(SAMPLE);
        let mut values = Map::new();
        // Stock defaults a real load would have seeded.
        values.insert("resolution".to_owned(), json!("1920x1080"));
        values.insert("fps".to_owned(), json!(60));
        values.insert("maxBitrateMbps".to_owned(), json!(75));
        values.insert("codec".to_owned(), json!("auto"));
        values.insert("replayBufferMemoryMiB".to_owned(), json!(256));
        let mut persisted = HashSet::new();
        persisted.insert("maxBitrateMbps".to_owned());

        apply_adaptive_defaults(&mut values, &persisted, &info);

        assert_eq!(values["resolution"], json!("1280x720"));
        assert_eq!(values["fps"], json!(30));
        // Persisted user value survives.
        assert_eq!(values["maxBitrateMbps"], json!(75));
        assert_eq!(values["replayBufferMemoryMiB"], json!(64));
        assert_eq!(values["codec"], json!("h264"));
    }

    #[test]
    fn adaptive_defaults_skip_unconstrained_hosts() {
        let text = "\nProcessor\tIntel(R) Core(TM) i7-12700K CPU @ 3.60GHz, 3600 Mhz, 12 Core(s), 20 Logical Processor(s)\t\nInstalled Physical Memory (RAM)\t32.00 GB\t\nBIOS Mode\tUEFI\t\nSecure Boot State\tOn\t\n";
        let info = SystemInformation::parse(text);
        let mut values = Map::new();
        values.insert("resolution".to_owned(), json!("1920x1080"));
        values.insert("fps".to_owned(), json!(60));
        let persisted = HashSet::new();
        apply_adaptive_defaults(&mut values, &persisted, &info);
        assert_eq!(values["resolution"], json!("1920x1080"));
        assert_eq!(values["fps"], json!(60));
    }

    #[test]
    fn load_reads_dropped_export_from_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SystemInformation.txt");
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(SAMPLE.as_bytes()).unwrap();
        let info = SystemInformation::load(dir.path());
        assert_eq!(info.installed_ram_gb, Some(4.0));
        assert_eq!(info.source.as_ref(), Some(&path));
    }

    #[test]
    fn load_without_file_returns_empty_info() {
        let dir = tempfile::tempdir().unwrap();
        let info = SystemInformation::load(dir.path());
        assert_eq!(info, SystemInformation::default());
    }

    #[test]
    fn to_json_round_trips_the_profile() {
        let info = SystemInformation::parse(SAMPLE);
        let value = to_json(&info);
        assert_eq!(value["osName"], "Microsoft Windows 11 Enterprise");
        assert_eq!(value["installedRamGb"], 4.0);
        assert_eq!(value["biosMode"], "Legacy");
        assert_eq!(value["capabilityProfile"]["legacyCpu"], true);
        assert_eq!(value["capabilityProfile"]["constrained"], true);
    }
}
