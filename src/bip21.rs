//! BIP21: URI Scheme Implementation
//!
//! Implements the Bitcoin URI scheme for payments:
//! bitcoin:<address>[?amount=<amount>][?label=<label>][?message=<message>]
//!
//! Specification: https://github.com/bitcoin/bips/blob/master/bip-0021.mediawiki
//!
//! ## OS-Level URI Scheme Registration
//!
//! For installers and workflows, this module provides utilities to register the `bitcoin:`
//! URI scheme with the operating system:
//!
//! - **Windows**: Registry entries via `.reg` file or direct registry manipulation
//! - **macOS**: Info.plist CFBundleURLTypes configuration
//! - **Linux**: Desktop entry files (.desktop) and MIME type registration
//!
//! These utilities are designed to be used by installers/packaging systems.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// BIP21 URI parsing error
#[derive(Debug, Clone)]
pub enum Bip21Error {
    InvalidScheme,
    InvalidFormat,
    MissingAddress,
    InvalidAmount,
    InvalidParameter,
}

impl std::fmt::Display for Bip21Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Bip21Error::InvalidScheme => write!(f, "Invalid URI scheme (must be 'bitcoin:')"),
            Bip21Error::InvalidFormat => write!(f, "Invalid URI format"),
            Bip21Error::MissingAddress => write!(f, "Missing Bitcoin address"),
            Bip21Error::InvalidAmount => write!(f, "Invalid amount"),
            Bip21Error::InvalidParameter => write!(f, "Invalid parameter"),
        }
    }
}

impl std::error::Error for Bip21Error {}

/// Parsed BIP21 Bitcoin URI
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BitcoinUri {
    /// Bitcoin address (required)
    pub address: String,
    /// Amount in BTC (optional)
    pub amount: Option<f64>,
    /// Label for payment request (optional)
    pub label: Option<String>,
    /// Message to display (optional)
    pub message: Option<String>,
    /// Additional parameters (optional, for extensibility)
    #[serde(flatten)]
    pub params: HashMap<String, String>,
}

impl BitcoinUri {
    /// Parse a BIP21 URI string
    ///
    /// Example: "bitcoin:1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa?amount=0.01&label=Test"
    pub fn parse(uri: &str) -> Result<Self, Bip21Error> {
        // Must start with "bitcoin:"
        if !uri.starts_with("bitcoin:") {
            return Err(Bip21Error::InvalidScheme);
        }

        // Remove "bitcoin:" prefix
        let uri_body = &uri[8..];

        // Split address and query parameters
        let (address_part, query_part) = if let Some(pos) = uri_body.find('?') {
            (&uri_body[..pos], Some(&uri_body[pos + 1..]))
        } else {
            (uri_body, None)
        };

        // Address is required and must be non-empty
        if address_part.is_empty() {
            return Err(Bip21Error::MissingAddress);
        }

        let address = address_part.to_string();

        // Parse query parameters
        let mut amount = None;
        let mut label = None;
        let mut message = None;
        let mut params = HashMap::new();

        if let Some(query) = query_part {
            for param in query.split('&') {
                if param.is_empty() {
                    continue;
                }

                let (key, value) = if let Some(eq_pos) = param.find('=') {
                    let k = &param[..eq_pos];
                    let v = &param[eq_pos + 1..];

                    // URL decode value
                    let decoded_value = url_decode(v)?;

                    (k, decoded_value)
                } else {
                    // Parameter without value
                    (param, String::new())
                };

                match key {
                    "amount" => {
                        // Parse amount as BTC
                        let parsed_amount: f64 =
                            value.parse().map_err(|_| Bip21Error::InvalidAmount)?;

                        // BIP21: Amount must be positive
                        if parsed_amount <= 0.0 {
                            return Err(Bip21Error::InvalidAmount);
                        }

                        amount = Some(parsed_amount);
                    }
                    "label" => {
                        label = Some(value);
                    }
                    "message" => {
                        message = Some(value);
                    }
                    _ => {
                        // Store additional parameters
                        params.insert(key.to_string(), value);
                    }
                }
            }
        }

        Ok(BitcoinUri {
            address,
            amount,
            label,
            message,
            params,
        })
    }

    /// Convert to BIP21 URI string
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        let mut uri = format!("bitcoin:{}", self.address);
        let mut params = Vec::new();

        if let Some(amt) = self.amount {
            params.push(format!("amount={}", amt));
        }

        if let Some(ref lbl) = self.label {
            params.push(format!("label={}", url_encode(lbl)));
        }

        if let Some(ref msg) = self.message {
            params.push(format!("message={}", url_encode(msg)));
        }

        // Add additional parameters
        for (key, value) in &self.params {
            params.push(format!("{}={}", key, url_encode(value)));
        }

        if !params.is_empty() {
            uri.push('?');
            uri.push_str(&params.join("&"));
        }

        uri
    }
}

// ============================================================================
// OS-Level URI Scheme Registration
// ============================================================================

/// URI scheme registration configuration
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UriSchemeRegistration {
    /// Application executable path
    pub executable_path: PathBuf,
    /// Application name (for display)
    pub app_name: String,
    /// Application description
    pub description: Option<String>,
    /// Icon path (optional)
    pub icon_path: Option<PathBuf>,
}

/// Platform-specific URI scheme registration utilities
pub mod registration {
    use super::UriSchemeRegistration;
    use std::io::Write;

    /// Generate Windows registry file for URI scheme registration
    ///
    /// Returns a `.reg` file content that can be imported to register the bitcoin: URI scheme
    pub fn generate_windows_registry_file(config: &UriSchemeRegistration) -> String {
        let exe_path = config
            .executable_path
            .to_str()
            .unwrap_or("")
            .replace('\\', "\\\\"); // Escape backslashes for registry

        format!(
            r#"Windows Registry Editor Version 5.00

[HKEY_CLASSES_ROOT\bitcoin]
@="URL:Bitcoin Payment Protocol"
"URL Protocol"=""

[HKEY_CLASSES_ROOT\bitcoin\DefaultIcon]
@="{icon_path}"

[HKEY_CLASSES_ROOT\bitcoin\shell]

[HKEY_CLASSES_ROOT\bitcoin\shell\open]

[HKEY_CLASSES_ROOT\bitcoin\shell\open\command]
@="\"{exe_path}\" \"%1\""
"#,
            exe_path = exe_path,
            icon_path = config
                .icon_path
                .as_ref()
                .and_then(|p| p.to_str())
                .unwrap_or(&exe_path)
                .replace('\\', "\\\\")
        )
    }

    /// Generate macOS Info.plist CFBundleURLTypes entry
    ///
    /// Returns the XML fragment to add to Info.plist's CFBundleURLTypes array
    pub fn generate_macos_info_plist_entry(config: &UriSchemeRegistration) -> String {
        format!(
            r#"    <dict>
        <key>CFBundleURLName</key>
        <string>{app_name}</string>
        <key>CFBundleURLSchemes</key>
        <array>
            <string>bitcoin</string>
        </array>
        <key>CFBundleTypeRole</key>
        <string>Viewer</string>
    </dict>"#,
            app_name = config.app_name
        )
    }

    /// Generate Linux desktop entry file for URI scheme registration
    ///
    /// Returns the content of a `.desktop` file that registers the bitcoin: URI scheme
    pub fn generate_linux_desktop_entry(config: &UriSchemeRegistration) -> String {
        let exec_path = config.executable_path.to_str().unwrap_or("");
        let icon_path = config
            .icon_path
            .as_ref()
            .and_then(|p| p.to_str())
            .unwrap_or("");

        format!(
            r#"[Desktop Entry]
Version=1.0
Type=Application
Name={app_name}
Comment={description}
Exec={exec_path} %u
Icon={icon_path}
MimeType=x-scheme-handler/bitcoin;
NoDisplay=true
"#,
            app_name = config.app_name,
            description = config
                .description
                .as_deref()
                .unwrap_or("Bitcoin Payment Handler"),
            exec_path = exec_path,
            icon_path = icon_path
        )
    }

    /// Generate Linux MIME type registration (for systems using shared-mime-info)
    ///
    /// Returns XML content for a MIME type definition file
    pub fn generate_linux_mime_type() -> String {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<mime-info xmlns="http://www.freedesktop.org/standards/shared-mime-info">
    <mime-type type="x-scheme-handler/bitcoin">
        <comment>Bitcoin payment URI</comment>
        <glob pattern="bitcoin:*"/>
    </mime-type>
</mime-info>
"#
        .to_string()
    }

    /// Write Windows registry file to disk
    pub fn write_windows_registry_file(
        config: &UriSchemeRegistration,
        output_path: &std::path::Path,
    ) -> std::io::Result<()> {
        let content = generate_windows_registry_file(config);
        let mut file = std::fs::File::create(output_path)?;
        file.write_all(content.as_bytes())?;
        Ok(())
    }

    /// Write Linux desktop entry to standard location
    ///
    /// On Linux, desktop entries should be placed in:
    /// - `~/.local/share/applications/` (user-specific)
    /// - `/usr/share/applications/` (system-wide, requires root)
    pub fn write_linux_desktop_entry(
        config: &UriSchemeRegistration,
        output_path: &std::path::Path,
    ) -> std::io::Result<()> {
        let content = generate_linux_desktop_entry(config);
        let mut file = std::fs::File::create(output_path)?;
        file.write_all(content.as_bytes())?;

        // Make executable (desktop entries should be executable)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = file.metadata()?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(output_path, perms)?;
        }

        Ok(())
    }
}

/// Utility functions for installer integration
impl UriSchemeRegistration {
    /// Create registration config from application info
    pub fn new(executable_path: impl Into<PathBuf>, app_name: impl Into<String>) -> Self {
        Self {
            executable_path: executable_path.into(),
            app_name: app_name.into(),
            description: None,
            icon_path: None,
        }
    }

    /// Set application description
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set icon path
    pub fn with_icon(mut self, icon_path: impl Into<PathBuf>) -> Self {
        self.icon_path = Some(icon_path.into());
        self
    }

    /// Generate all platform-specific registration files for an installer
    ///
    /// Returns a map of platform names to file contents
    pub fn generate_installer_files(&self) -> HashMap<String, String> {
        let mut files = HashMap::new();

        files.insert(
            "windows.reg".to_string(),
            registration::generate_windows_registry_file(self),
        );

        files.insert(
            "macos-info-plist.xml".to_string(),
            registration::generate_macos_info_plist_entry(self),
        );

        files.insert(
            "linux.desktop".to_string(),
            registration::generate_linux_desktop_entry(self),
        );

        files.insert(
            "linux-mime.xml".to_string(),
            registration::generate_linux_mime_type(),
        );

        files
    }
}

/// URL decode a string (basic implementation)
fn url_decode(encoded: &str) -> Result<String, Bip21Error> {
    let mut decoded = String::new();
    let mut chars = encoded.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '%' {
            // Hex encoded character
            let hex1 = chars.next().ok_or(Bip21Error::InvalidParameter)?;
            let hex2 = chars.next().ok_or(Bip21Error::InvalidParameter)?;

            let hex_str = format!("{}{}", hex1, hex2);
            let byte =
                u8::from_str_radix(&hex_str, 16).map_err(|_| Bip21Error::InvalidParameter)?;

            decoded.push(byte as char);
        } else if ch == '+' {
            // Plus sign is decoded as space
            decoded.push(' ');
        } else {
            decoded.push(ch);
        }
    }

    Ok(decoded)
}

/// URL encode a string (basic implementation)
fn url_encode(s: &str) -> String {
    let mut encoded = String::new();

    for ch in s.chars() {
        match ch {
            ' ' => encoded.push_str("%20"),
            '!' => encoded.push_str("%21"),
            '"' => encoded.push_str("%22"),
            '#' => encoded.push_str("%23"),
            '$' => encoded.push_str("%24"),
            '%' => encoded.push_str("%25"),
            '&' => encoded.push_str("%26"),
            '\'' => encoded.push_str("%27"),
            '(' => encoded.push_str("%28"),
            ')' => encoded.push_str("%29"),
            '*' => encoded.push_str("%2A"),
            '+' => encoded.push_str("%2B"),
            ',' => encoded.push_str("%2C"),
            '/' => encoded.push_str("%2F"),
            ':' => encoded.push_str("%3A"),
            ';' => encoded.push_str("%3B"),
            '=' => encoded.push_str("%3D"),
            '?' => encoded.push_str("%3F"),
            '@' => encoded.push_str("%40"),
            '[' => encoded.push_str("%5B"),
            '\\' => encoded.push_str("%5C"),
            ']' => encoded.push_str("%5D"),
            _ => {
                // Check if ASCII printable
                if ch.is_ascii() && !ch.is_control() {
                    encoded.push(ch);
                } else {
                    // Encode as UTF-8 bytes
                    for byte in ch.to_string().as_bytes() {
                        encoded.push_str(&format!("%{:02X}", byte));
                    }
                }
            }
        }
    }

    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_uri() {
        let uri = BitcoinUri::parse("bitcoin:1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa").unwrap();
        assert_eq!(uri.address, "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa");
        assert_eq!(uri.amount, None);
        assert_eq!(uri.label, None);
    }

    #[test]
    fn test_parse_uri_with_amount() {
        let uri =
            BitcoinUri::parse("bitcoin:1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa?amount=0.01").unwrap();
        assert_eq!(uri.address, "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa");
        assert_eq!(uri.amount, Some(0.01));
    }

    #[test]
    fn test_parse_uri_with_all_params() {
        let uri = BitcoinUri::parse("bitcoin:1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa?amount=0.01&label=Test%20Label&message=Test%20Message").unwrap();
        assert_eq!(uri.address, "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa");
        assert_eq!(uri.amount, Some(0.01));
        assert_eq!(uri.label, Some("Test Label".to_string()));
        assert_eq!(uri.message, Some("Test Message".to_string()));
    }

    #[test]
    fn test_parse_invalid_scheme() {
        let result = BitcoinUri::parse("http://example.com");
        assert!(matches!(result, Err(Bip21Error::InvalidScheme)));
    }

    #[test]
    fn test_parse_missing_address() {
        let result = BitcoinUri::parse("bitcoin:");
        assert!(matches!(result, Err(Bip21Error::MissingAddress)));
    }

    #[test]
    fn test_to_string() {
        let uri = BitcoinUri {
            address: "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa".to_string(),
            amount: Some(0.01),
            label: Some("Test".to_string()),
            message: None,
            params: HashMap::new(),
        };

        let uri_str = uri.to_string();
        assert!(uri_str.starts_with("bitcoin:1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa"));
        assert!(uri_str.contains("amount=0.01"));
        assert!(uri_str.contains("label=Test"));
    }
}

#[cfg(test)]
mod registration_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_windows_registry_generation() {
        let config = UriSchemeRegistration::new(
            PathBuf::from("C:\\Program Files\\Bitcoin Commons\\bllvm.exe"),
            "Bitcoin Commons BLLVM",
        )
        .with_description("Bitcoin Node");

        let reg_content = registration::generate_windows_registry_file(&config);
        assert!(reg_content.contains("bitcoin"));
        assert!(reg_content.contains("bllvm.exe"));
        assert!(reg_content.contains("URL:Bitcoin Payment Protocol"));
    }

    #[test]
    fn test_macos_info_plist_generation() {
        let config = UriSchemeRegistration::new(
            PathBuf::from("/usr/local/bin/bllvm"),
            "Bitcoin Commons BLLVM",
        );

        let plist_content = registration::generate_macos_info_plist_entry(&config);
        assert!(plist_content.contains("bitcoin"));
        assert!(plist_content.contains("Bitcoin Commons BLLVM"));
        assert!(plist_content.contains("CFBundleURLSchemes"));
    }

    #[test]
    fn test_linux_desktop_entry_generation() {
        let config =
            UriSchemeRegistration::new(PathBuf::from("/usr/bin/bllvm"), "Bitcoin Commons BLLVM")
                .with_description("Bitcoin Node");

        let desktop_content = registration::generate_linux_desktop_entry(&config);
        assert!(desktop_content.contains("bitcoin"));
        assert!(desktop_content.contains("bllvm"));
        assert!(desktop_content.contains("x-scheme-handler/bitcoin"));
        assert!(desktop_content.contains("Bitcoin Node"));
    }

    #[test]
    fn test_installer_files_generation() {
        let config =
            UriSchemeRegistration::new(PathBuf::from("/usr/bin/bllvm"), "Bitcoin Commons BLLVM");

        let files = config.generate_installer_files();
        assert_eq!(files.len(), 4);
        assert!(files.contains_key("windows.reg"));
        assert!(files.contains_key("macos-info-plist.xml"));
        assert!(files.contains_key("linux.desktop"));
        assert!(files.contains_key("linux-mime.xml"));
    }
}
