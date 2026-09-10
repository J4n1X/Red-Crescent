//! `-rdynamic` exports the statically linked Lua C API so `dlopen`ed modules
//! can resolve `lua_gettop` and friends; without it they fail to load. It also
//! defeats dead-code elimination (~70% larger binary), hence the opt-in
//! feature. Without it, `main` rejects `c_module_dirs` at startup.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let enabled = std::env::var("CARGO_FEATURE_C_MODULES").is_ok();
    if enabled && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-arg=-rdynamic");
    }
}
