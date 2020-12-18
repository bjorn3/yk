fn main() {
    let target = std::env::var("TARGET").expect("TARGET");
    let target_dir = std::env::var("OUT_DIR").expect("OUT_DIR") + "/target";

    let result = std::process::Command::new("cargo")
        .arg("+nightly")
        .arg("build")
        .arg("--release")
        .arg("--target")
        .arg(&target)
        .arg("-p")
        .arg("ykrt_internal")
        .env("RUSTFLAGS", format!("-Clink-arg=-Wl,-rpath,/home/bjorn3/.rustup/toolchains/nightly-x86_64-unknown-linux-gnu/lib"))
        //.env_remove("RUSTFLAGS")
        .env("CARGO_TARGET_DIR", &target_dir)
        .status()
        .unwrap();
    assert!(result.success());

    println!("cargo:rerun-if-changed=..");
    println!("cargo:rustc-link-search={}/{}/release", target_dir, target);
    println!("cargo:rustc-link-lib=dylib=ykrt_internal");
}
