use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=Info.plist.template");

    let template = fs::read_to_string("Info.plist.template").expect("read Info.plist.template");
    let version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION is set by cargo");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR is set by cargo");

    let plist = PathBuf::from(out_dir).join("Info.plist");
    fs::write(&plist, template.replace("__VERSION__", &version)).expect("write Info.plist");

    println!(
        "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        plist.display()
    );
}
