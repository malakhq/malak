//! Enable the optional real cross-account integration tests.

fn main() {
    println!("cargo:rerun-if-env-changed=MALAK_TEST_SECOND_ACCOUNT");
    println!("cargo:rerun-if-env-changed=MALAK_TEST_THIRD_ACCOUNT");
    println!("cargo:rustc-check-cfg=cfg(malak_second_account)");
    println!("cargo:rustc-check-cfg=cfg(malak_third_account)");
    if configured("MALAK_TEST_SECOND_ACCOUNT") {
        println!("cargo:rustc-cfg=malak_second_account");
        if configured("MALAK_TEST_THIRD_ACCOUNT") {
            println!("cargo:rustc-cfg=malak_third_account");
        }
    }
}

fn configured(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}
