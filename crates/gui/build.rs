//! Makes the Windows build of `scriptrunner-gui.exe` ask for elevation (plan D15).
//!
//! The daemon's pipe only admits SYSTEM and Administrators, so a GUI started without elevation could
//! only show "access denied". Embedding `requireAdministrator` in the manifest makes Windows show the
//! UAC prompt when the program is opened instead.
//!
//! Only the application is affected (`rustc-link-arg-bin`): the library, the tests and the graphics
//! spike keep running unelevated.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");

    let windows = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows");
    if !windows {
        return;
    }
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|env| env == "msvc");
    if msvc {
        println!("cargo::rustc-link-arg-bin=scriptrunner-gui=/MANIFEST:EMBED");
        println!(
            "cargo::rustc-link-arg-bin=scriptrunner-gui=/MANIFESTUAC:level='requireAdministrator' uiAccess='false'"
        );
    } else {
        // The linker for the GNU target has no equivalent switch. It is only used to type-check
        // from Linux; the shipped build is MSVC.
        println!("cargo::warning=scriptrunner-gui is not marked requireAdministrator on this Windows target");
    }
}
