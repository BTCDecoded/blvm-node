# BIP21 URI Scheme Registration for Installers

This guide explains how to use the BIP21 URI scheme registration utilities in installers and packaging workflows.

## Overview

The `bip21` module provides utilities to register the `bitcoin:` URI scheme with the operating system, allowing Bitcoin URIs to be handled by your application.

## Usage in Installers

### Example: Generate All Registration Files

```rust
use reference_node::bip21::UriSchemeRegistration;

// Configure registration
let registration = UriSchemeRegistration::new(
    "/usr/local/bin/bllvm",  // or "C:\\Program Files\\Bitcoin Commons\\bllvm.exe" on Windows
    "Bitcoin Commons BLLVM"
)
.with_description("Bitcoin Node")
.with_icon("/usr/share/icons/bitcoin-commons.png");

// Generate all platform-specific files
let files = registration.generate_installer_files();

// Use in installer:
// - files["windows.reg"] -> import during Windows installation
// - files["macos-info-plist.xml"] -> add to Info.plist CFBundleURLTypes array
// - files["linux.desktop"] -> install to ~/.local/share/applications/ or /usr/share/applications/
// - files["linux-mime.xml"] -> install to /usr/share/mime/packages/ (update-mime-database)
```

## Platform-Specific Instructions

### Windows

**During Installation:**
1. Generate `.reg` file using `registration::generate_windows_registry_file()`
2. Import the registry file during installation:
   ```powershell
   regedit /s bitcoin-uri.reg
   ```
   Or use Windows Installer (MSI) custom actions to write registry entries directly.

**Registry Location:**
- `HKEY_CLASSES_ROOT\bitcoin`

### macOS

**During Installation:**
1. Generate Info.plist entry using `registration::generate_macos_info_plist_entry()`
2. Add the XML fragment to your app's `Info.plist` under `CFBundleURLTypes` array:
   ```xml
   <key>CFBundleURLTypes</key>
   <array>
       <!-- Generated entry here -->
   </array>
   ```
3. For App Store apps, this is already in your bundle; for standalone installers, ensure Info.plist is updated.

**Location:**
- `Contents/Info.plist` in your `.app` bundle

### Linux

**During Installation (User-specific):**
```bash
# Generate desktop entry
registration::write_linux_desktop_entry(&config, 
    &home_dir.join(".local/share/applications/bitcoin-commons-bllvm-bitcoin.desktop"))?;

# Update desktop database
xdg-desktop-menu forceupdate
```

**During Installation (System-wide):**
```bash
# Requires root
# Install desktop entry
registration::write_linux_desktop_entry(&config,
    Path::new("/usr/share/applications/bitcoin-commons-bllvm-bitcoin.desktop"))?;

# Install MIME type
write_file("/usr/share/mime/packages/bitcoin.xml", 
    registration::generate_linux_mime_type())?;

# Update databases
update-desktop-database /usr/share/applications
update-mime-database /usr/share/mime
```

**File Locations:**
- Desktop entry: `~/.local/share/applications/` (user) or `/usr/share/applications/` (system)
- MIME type: `/usr/share/mime/packages/bitcoin.xml` (system-wide only)

## Installer Integration Examples

### Example 1: RPM Package (.spec file)

```spec
%install
# Generate desktop entry
# (Run during build with registration utilities)
install -m 644 bitcoin-commons-bllvm-bitcoin.desktop %{buildroot}%{_datadir}/applications/
install -m 644 bitcoin.xml %{buildroot}%{_datadir}/mime/packages/

%post
# Update desktop and MIME databases
update-desktop-database %{_datadir}/applications || :
update-mime-database %{_datadir}/mime || :
```

### Example 2: DEB Package

Add to `debian/bitcoin-commons-bllvm.install`:
```
usr/share/applications/bitcoin-commons-bllvm-bitcoin.desktop
usr/share/mime/packages/bitcoin.xml
```

Add to `debian/bitcoin-commons-bllvm.postinst`:
```bash
#!/bin/bash
set -e

update-desktop-database /usr/share/applications || true
update-mime-database /usr/share/mime || true
```

### Example 3: macOS .pkg Installer

Include the generated Info.plist entry in your app bundle's `Info.plist` before creating the package.

### Example 4: Windows MSI Installer

Add custom action to import the `.reg` file or write registry entries directly using Windows Installer XML (WiX) or similar.

## Testing URI Scheme Registration

After installation, test that the URI scheme is registered:

### Windows
```cmd
start bitcoin:1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa?amount=0.01
```

### macOS/Linux
```bash
xdg-open "bitcoin:1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa?amount=0.01"
```

This should launch your application with the URI as an argument.

## Handling URI Arguments in Your Application

Your application should accept the URI as a command-line argument and parse it:

```rust
use reference_node::bip21::BitcoinUri;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    
    if args.len() > 1 && args[1].starts_with("bitcoin:") {
        match BitcoinUri::parse(&args[1]) {
            Ok(uri) => {
                println!("Address: {}", uri.address);
                if let Some(amount) = uri.amount {
                    println!("Amount: {} BTC", amount);
                }
                // Process payment...
            }
            Err(e) => {
                eprintln!("Invalid Bitcoin URI: {}", e);
            }
        }
    }
}
```

## Notes

- **Permissions**: Linux system-wide registration requires root/sudo
- **User vs System**: Prefer user-specific registration when possible (no root required)
- **Desktop Environment**: Linux registration works with XDG-compliant desktop environments (GNOME, KDE, XFCE, etc.)
- **Security**: Validate URIs before processing payments (verify addresses, check amounts)

