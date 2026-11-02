fn main() {
    println!("cargo:rerun-if-changed=assets/app_res.o");
    println!("cargo:rerun-if-changed=assets/iconforge.ico");
    #[cfg(windows)]
    {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
        let res_obj = std::path::Path::new(&manifest_dir)
            .join("assets")
            .join("app_res.o");
        if res_obj.exists() {
            println!("cargo:rustc-link-arg-bins={}", res_obj.display());
        }
    }
}
