//! Install the fake Codex proxy and wait for it before testing notices.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

pub(super) fn install(home: &Path, bin: &Path) {
    let proxy = bin.join("codex");
    fs::write(&proxy, include_bytes!("codex_notice.py")).unwrap();
    fs::set_permissions(proxy, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        home.join("sitecustomize.py"),
        include_bytes!("codex_notice_ready.py"),
    )
    .unwrap();
}

pub(super) fn wait_for_proxy<'a>(command: &'a mut Command, home: &Path) -> &'a mut Command {
    command
        .env("PYTHONPATH", home)
        .env("MALAK_TEST_CODEX_PROXY_READY", "180")
}
