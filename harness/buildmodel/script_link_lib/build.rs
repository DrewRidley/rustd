use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=native.c");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let object = out.join("native.o");
    let cc = env::var("BM_CC").unwrap_or_else(|_| "cc".to_owned());
    assert!(Command::new(&cc).args(["-c", "-fPIC", "native.c", "-o"]).arg(&object).status().unwrap().success());
    let ar = env::var("BM_AR").unwrap_or_else(|_| "ar".to_owned());
    assert!(Command::new(&ar).arg("crs").arg(out.join("libnative.a")).arg(&object).status().unwrap().success());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=native");
}
