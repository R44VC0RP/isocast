fn main() {
    // The interface itself (ui/*.slint) is compiled by isocast-ui-interface,
    // a library of its own, so the one huge generated module builds once
    // per profile instead of once for the app and again for its tests.

    // On macOS, an Info.plist linked into the binary names the app
    // ("Isocast" in the menu bar's app menu and the Dock) when it runs
    // outside a bundle, as `cargo run` does.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let plist =
            std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("Info.plist");
        println!("cargo:rerun-if-changed={}", plist.display());
        println!(
            "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }

    // On Windows (MSVC), the app icon as a resource in the executable: the
    // Start menu, Explorer, the taskbar and Alt-Tab show it, and windows
    // load it by ordinal (src/chrome_windows.rs).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        let ico = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("windows/isocast.ico");
        println!("cargo:rerun-if-changed={}", ico.display());
        let res = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("isocast.res");
        std::fs::write(
            &res,
            icon_resources(&std::fs::read(&ico).expect("windows/isocast.ico")),
        )
        .expect("OUT_DIR is writable");
        // link.exe takes a compiled resource file as an input of its own.
        println!("cargo:rustc-link-arg-bins={}", res.display());
    }
}

/// The resource ordinal of the app icon (`RT_GROUP_ICON`).
const ICON_ORDINAL: u16 = 1;

/// A compiled resource file (.res) holding `ico`'s images as `RT_ICON`s
/// and their directory as the `RT_GROUP_ICON` `ICON_ORDINAL`: what rc.exe
/// makes of `1 ICON "isocast.ico"`, without needing the Windows SDK's
/// tools on PATH.
fn icon_resources(ico: &[u8]) -> Vec<u8> {
    let u16_at = |at: usize| u16::from_le_bytes([ico[at], ico[at + 1]]);
    let u32_at = |at: usize| u32::from_le_bytes(ico[at..at + 4].try_into().unwrap());
    assert!(
        ico.len() >= 6 && u16_at(0) == 0 && u16_at(2) == 1,
        "isocast.ico is an icon"
    );
    let count = u16_at(4) as usize;
    // One resource: a 32-byte header (ordinal type and name), the data,
    // padded to four bytes.
    fn resource(out: &mut Vec<u8>, kind: u16, name: u16, data: &[u8]) {
        let flags: u16 = if kind == 0 { 0 } else { 0x1030 }; // moveable, pure, discardable
        let language: u16 = if kind == 0 { 0 } else { 0x0409 };
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(32u32.to_le_bytes());
        for value in [0xffff_u16, kind, 0xffff, name] {
            out.extend(value.to_le_bytes());
        }
        out.extend(0u32.to_le_bytes()); // data version
        out.extend(flags.to_le_bytes());
        out.extend(language.to_le_bytes());
        out.extend([0u8; 8]); // version, characteristics
        out.extend(data);
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
    }
    let mut out = Vec::new();
    // A .res file starts with an empty resource.
    resource(&mut out, 0, 0, &[]);
    let mut group = Vec::new();
    group.extend(0u16.to_le_bytes());
    group.extend(1u16.to_le_bytes());
    group.extend((count as u16).to_le_bytes());
    for i in 0..count {
        let entry = 6 + 16 * i;
        let (size, offset) = (u32_at(entry + 8) as usize, u32_at(entry + 12) as usize);
        let id = i as u16 + 1;
        resource(&mut out, 3, id, &ico[offset..offset + size]); // RT_ICON
        // The directory entry: width .. bit count as in the .ico, the
        // image's size, then its RT_ICON id instead of a file offset.
        group.extend(&ico[entry..entry + 8]);
        group.extend((size as u32).to_le_bytes());
        group.extend(id.to_le_bytes());
    }
    resource(&mut out, 14, ICON_ORDINAL, &group); // RT_GROUP_ICON
    out
}
