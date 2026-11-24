//! The scripts of the bundled crypt-v01 pack, compiled in.

use crate::load::ScriptSources;

macro_rules! script {
    ($path:literal) => {
        (
            $path,
            include_str!(concat!("../../../worlds/crypt-v01/scripts/", $path)),
        )
    };
}

/// Every script of crypt-v01, by its path under `scripts/`.
pub const CRYPT_V01_FILES: [(&str, &str); 6] = [
    script!("lib.rhai"),
    script!("rooms/ossuary.rhai"),
    script!("rooms/alley.rhai"),
    script!("items/iron_ore.rhai"),
    script!("items/bone_charm.rhai"),
    script!("minds/hale.rhai"),
];

/// The scripts of the bundled pack.
#[must_use]
pub fn crypt_v01_sources() -> ScriptSources {
    ScriptSources {
        manifest: Some(include_str!("../../../worlds/crypt-v01/scripts.toml").to_owned()),
        files: CRYPT_V01_FILES
            .iter()
            .map(|(p, t)| ((*p).to_owned(), (*t).to_owned()))
            .collect(),
        problems: Vec::new(),
        warnings: Vec::new(),
    }
}
