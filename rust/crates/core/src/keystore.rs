//! Keystore integration — re-exports from pay-keystore.

pub use pay_keystore::*;

#[cfg(target_os = "macos")]
pub use pay_keystore::backends::apple_keychain::AppleKeychain;

#[cfg(target_os = "linux")]
pub use pay_keystore::backends::gnome_keyring::GnomeKeyring;

pub use pay_keystore::backends::onepassword::OnePassword;
