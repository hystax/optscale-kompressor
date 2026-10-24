//! Windows only: embed a version resource, so Task Manager, Explorer and the
//! file's Properties show "OptScale Kompressor" and its version instead of the
//! bare file name. Every other target builds as if this file did not exist.
//!
//! The resource compiler is the target toolchain's own, found by
//! `embed-resource`: `rc.exe` for MSVC, `x86_64-w64-mingw32-windres` for the
//! cross build in `make win`. Without one the build goes on with a warning:
//! the resource is a label, not a reason to fail.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let license = manifest.join("../../LICENSE");
    println!("cargo:rerun-if-changed={}", license.display());
    let copyright = copyright_line(&license);

    let version = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    // FILEVERSION takes four numbers; a pre-release suffix lives only in the
    // string fields.
    let mut numbers = version
        .split(['-', '+'])
        .next()
        .unwrap_or_default()
        .split('.')
        .map(|part| part.parse::<u16>().unwrap_or(0));
    let (major, minor, patch) = (
        numbers.next().unwrap_or(0),
        numbers.next().unwrap_or(0),
        numbers.next().unwrap_or(0),
    );

    let rc = format!(
        r#"#include <winver.h>

1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEOS VOS_NT_WINDOWS32
FILETYPE VFT_APP
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904B0"
        BEGIN
            VALUE "CompanyName", "Hystax"
            VALUE "FileDescription", "OptScale Kompressor"
            VALUE "FileVersion", "{version}"
            VALUE "InternalName", "kompressor"
            VALUE "LegalCopyright", "{copyright}"
            VALUE "OriginalFilename", "kompressor.exe"
            VALUE "ProductName", "OptScale Kompressor"
            VALUE "ProductVersion", "{version}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#
    );

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let path = out.join("kompressor.rc");
    std::fs::write(&path, rc).expect("write the resource script");
    // Not `manifest_optional()`: it treats a compiler that is not there
    // (`NotAttempted`) as success, and the exe would lose its label silently.
    use embed_resource::CompilationResult;
    let result = embed_resource::compile(&path, embed_resource::NONE);
    if !matches!(
        result,
        CompilationResult::Ok | CompilationResult::NotWindows
    ) {
        println!("cargo:warning=building without the Windows version resource: {result}");
    }
}

/// The LICENSE's copyright line, so the year is updated in one place. A
/// packaged crate has no LICENSE two levels up; the holder alone then.
fn copyright_line(license: &Path) -> String {
    std::fs::read_to_string(license)
        .ok()
        .and_then(|text| {
            text.lines()
                .map(str::trim)
                .find(|line| line.starts_with("Copyright"))
                .map(|line| format!("{line}, MIT License"))
        })
        .unwrap_or_else(|| "Hystax, MIT License".to_string())
        .replace('"', "'")
}
