//! Native plugins: libraries loaded once at startup that register Lua
//! functions through the C ABI in `plugins/rc_plugin.h`. See `plugins/ABI.md`.
//!
//! Loading only records what a plugin registers; `install` builds the Lua
//! side into each state from that record. A plugin never sees a Lua state,
//! which is what lets pooling stay on.

mod host;

use std::collections::BTreeMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::path::{Path, PathBuf};

use mlua::{Lua, Table};
use red_crescent_plugin::ffi::{
    RC_PLUGIN_ABI, RcFunction, RcHandleType, RcPluginContext, RcRegistryCOM, RcStatus,
};
use serde::Deserialize;

pub(crate) use host::end_request;

/// One entry of `plugins` in rc_config.lua.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PluginSetting {
    /// Without an extension; see [`candidates`].
    pub path: PathBuf,
    #[serde(default)]
    pub options: Option<serde_json::Value>,
}

/// Everything the loaded plugins registered. Leaked at load, since plugins
/// are never unloaded.
#[derive(Default)]
pub struct Plugins {
    plugins: Vec<Plugin>,
    /// Indexed by type id, across all plugins.
    handle_types: Vec<HandleType>,
}

struct Plugin {
    name: String,
    functions: Vec<Function>,
}

pub(crate) struct Function {
    /// `plugin.function`, for errors.
    qualified: String,
    name: String,
    func: RcFunction,
    userdata: *mut c_void,
    flags: u32,
}

struct HandleType {
    name: &'static str,
    methods: Vec<Function>,
    destroy: Option<unsafe extern "C" fn(*mut c_void)>,
}

// SAFETY: the pointers are the plugin's, and the ABI makes the plugin
// responsible for its functions being callable from any thread.
unsafe impl Send for Plugins {}
unsafe impl Sync for Plugins {}

impl Plugins {
    /// No plugins: what a server without a `plugins` setting uses.
    pub fn none() -> &'static Plugins {
        static NONE: std::sync::OnceLock<Plugins> = std::sync::OnceLock::new();
        NONE.get_or_init(Plugins::default)
    }

    /// The Lua names, in load order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.plugins.iter().map(|p| p.name.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

/// Names a plugin cannot take: every global a request environment defines.
fn reserved(name: &str) -> bool {
    const LUA_KEYWORDS: &[&str] = &[
        "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if",
        "in", "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
    ];
    const ENV_NAMES: &[&str] = &[
        "request",
        "response",
        "package",
        "require",
        "include",
        "flush",
        "load",
        "loadstring",
        "_G",
        "_VERSION",
        "_ENV",
    ];
    LUA_KEYWORDS.contains(&name)
        || ENV_NAMES.contains(&name)
        || crate::api::env_names().any(|n| n == name)
}

fn valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The files `path` may name: as written, then the platform's library name.
fn candidates(path: &Path) -> Vec<PathBuf> {
    let mut out = vec![path.to_path_buf()];
    if let (Some(dir), Some(stem)) = (path.parent(), path.file_name().and_then(|s| s.to_str())) {
        if cfg!(windows) {
            out.push(dir.join(format!("{stem}.dll")));
        } else if cfg!(target_os = "macos") {
            out.push(dir.join(format!("lib{stem}.dylib")));
        } else {
            out.push(dir.join(format!("lib{stem}.so")));
            out.push(dir.join(format!("{stem}.so")));
        }
    }
    out
}

/// Loads every configured plugin, or says which one failed and why.
pub fn load(
    settings: &BTreeMap<String, PluginSetting>,
    serve_dir: &Path,
    data_dir: &Path,
) -> Result<&'static Plugins, String> {
    if settings.is_empty() {
        return Ok(Plugins::none());
    }
    let mut plugins = Plugins::default();
    for (name, setting) in settings {
        load_one(&mut plugins, name, setting, serve_dir, data_dir)
            .map_err(|e| format!("plugin '{name}': {e}"))?;
    }
    Ok(Box::leak(Box::new(plugins)))
}

fn load_one(
    plugins: &mut Plugins,
    name: &str,
    setting: &PluginSetting,
    serve_dir: &Path,
    data_dir: &Path,
) -> Result<(), String> {
    if !valid_identifier(name) {
        return Err("the name must be a Lua identifier".into());
    }
    if reserved(name) {
        return Err("the name clashes with a built-in".into());
    }

    let path = candidates(&setting.path)
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| format!("no library at {}", setting.path.display()))?
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if path.starts_with(serve_dir) {
        return Err(format!(
            "{} is inside the serve directory, where it would be downloadable",
            path.display()
        ));
    }

    // SAFETY: loading native code runs its initializers; the operator vouched
    // for this library by naming it.
    let library = unsafe { libloading::Library::new(&path) }
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let library: &'static libloading::Library = Box::leak(Box::new(library));

    // SAFETY: the signatures are the ABI's; a library exporting these names
    // with others is not a plugin, and the ABI check below is the first call.
    let (abi, install) = unsafe {
        let abi = library
            .get::<unsafe extern "C" fn() -> u32>(b"rc_plugin_abi\0")
            .map_err(|_| format!("{} exports no rc_plugin_abi", path.display()))?;
        let install = library
            .get::<unsafe extern "C" fn(*const RcPluginContext, *mut RcRegistryCOM) -> RcStatus>(
                b"rc_plugin_install\0",
            )
            .map_err(|_| format!("{} exports no rc_plugin_install", path.display()))?;
        (*abi, *install)
    };
    let wanted = unsafe { abi() };
    if wanted != RC_PLUGIN_ABI {
        return Err(format!(
            "built for plugin ABI {wanted}, this server speaks {RC_PLUGIN_ABI}"
        ));
    }

    // Kept for the life of the process, so a plugin may hold on to them.
    let leak = |s: String| -> *const c_char {
        CString::new(s)
            .map(|c| c.into_raw() as *const c_char)
            .unwrap_or(c"".as_ptr())
    };
    let options = match &setting.options {
        Some(value) => serde_json::to_string(value).map_err(|e| e.to_string())?,
        None => "{}".to_string(),
    };
    let ctx = RcPluginContext {
        size: size_of::<RcPluginContext>() as u32,
        abi: RC_PLUGIN_ABI,
        server_version_cstr: leak(env!("CARGO_PKG_VERSION").to_string()),
        name_cstr: leak(name.to_string()),
        serve_dir_cstr: leak(serve_dir.display().to_string()),
        data_dir_cstr: leak(data_dir.display().to_string()),
        options_json_cstr: leak(options),
        host: &host::HOST,
    };

    let mut registry = Registry {
        com: RcRegistryCOM {
            size: size_of::<RcRegistryCOM>() as u32,
            function: register_function,
            handle_type: register_handle_type,
        },
        plugin: name.to_string(),
        functions: Vec::new(),
        handle_types: Vec::new(),
        first_type_id: plugins.handle_types.len() as u32,
        error: None,
    };
    let status = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        install(&ctx, &mut registry.com)
    }))
    .map_err(|_| "rc_plugin_install panicked".to_string())?;
    if let Some(e) = registry.error {
        return Err(e);
    }
    if status != RcStatus::RC_OK {
        return Err(format!("rc_plugin_install returned {}", status.0));
    }

    log::info!(
        "plugin '{name}' loaded from {}: {} function(s), {} handle type(s)",
        path.display(),
        registry.functions.len(),
        registry.handle_types.len()
    );
    plugins.handle_types.extend(registry.handle_types);
    plugins.plugins.push(Plugin {
        name: name.to_string(),
        functions: registry.functions,
    });
    Ok(())
}

/// The registry a plugin's install writes into. `com` first, so the pointer
/// the plugin holds is also a pointer to this.
#[repr(C)]
struct Registry {
    com: RcRegistryCOM,
    plugin: String,
    functions: Vec<Function>,
    handle_types: Vec<HandleType>,
    first_type_id: u32,
    /// The first misuse; install fails with it.
    error: Option<String>,
}

/// # Safety
/// `ptr` must be null or a NUL-terminated string.
unsafe fn name_from(ptr: *const c_char) -> Result<String, String> {
    if ptr.is_null() {
        return Err("a null name".into());
    }
    let name = unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| "a name that is not UTF-8".to_string())?;
    if !valid_identifier(name) {
        return Err(format!("'{name}' is not a Lua identifier"));
    }
    Ok(name.to_string())
}

fn function(
    plugin: &str,
    name: String,
    func: RcFunction,
    userdata: *mut c_void,
    flags: u32,
) -> Function {
    Function {
        qualified: format!("{plugin}.{name}"),
        name,
        func,
        userdata,
        flags,
    }
}

unsafe extern "C" fn register_function(
    this: *mut RcRegistryCOM,
    name: *const c_char,
    func: RcFunction,
    userdata: *mut c_void,
    flags: u32,
) -> RcStatus {
    // SAFETY: `this` is the `com` field of the Registry `load_one` handed out.
    let registry = unsafe { &mut *(this as *mut Registry) };
    let result = unsafe { name_from(name) }.and_then(|name| {
        if registry.functions.iter().any(|f| f.name == name) {
            return Err(format!("'{name}' is registered twice"));
        }
        Ok(name)
    });
    match result {
        Ok(name) => {
            let f = function(&registry.plugin, name, func, userdata, flags);
            registry.functions.push(f);
            RcStatus::RC_OK
        }
        Err(e) => {
            registry.error.get_or_insert(e);
            RcStatus::RC_ERROR
        }
    }
}

unsafe extern "C" fn register_handle_type(
    this: *mut RcRegistryCOM,
    spec: *const RcHandleType,
    type_id: *mut u32,
) -> RcStatus {
    let registry = unsafe { &mut *(this as *mut Registry) };
    let result = (|| {
        if spec.is_null() || type_id.is_null() {
            return Err("handle_type: null argument".to_string());
        }
        let spec = unsafe { &*spec };
        let name = unsafe { name_from(spec.name_cstr) }?;
        let mut methods = Vec::with_capacity(spec.nmethods);
        for i in 0..spec.nmethods {
            let m = unsafe { &*spec.methods.add(i) };
            let method = unsafe { name_from(m.name_cstr) }?;
            let owner = format!("{}:{name}", registry.plugin);
            methods.push(function(&owner, method, m.fn_, spec.userdata, m.flags));
        }
        let id = registry.first_type_id + registry.handle_types.len() as u32;
        registry.handle_types.push(HandleType {
            name: Box::leak(format!("{}.{name}", registry.plugin).into_boxed_str()),
            methods,
            destroy: spec.destroy,
        });
        unsafe { *type_id = id };
        Ok(())
    })();
    match result {
        Ok(()) => RcStatus::RC_OK,
        Err(e) => {
            registry.error.get_or_insert(e);
            RcStatus::RC_ERROR
        }
    }
}

/// Builds every plugin's table into `lua`: a global per plugin, also
/// reachable with `require`, and the method tables handles look up.
pub(crate) fn install(lua: &Lua, plugins: &'static Plugins) -> mlua::Result<()> {
    if plugins.is_empty() {
        return Ok(());
    }
    for plugin in &plugins.plugins {
        let table = lua.create_table()?;
        for f in &plugin.functions {
            table.set(f.name.as_str(), host::wrap(lua, plugins, f)?)?;
        }
        lua.globals().set(plugin.name.as_str(), table.clone())?;
        lua.register_module(&plugin.name, table)?;
    }
    host::install(lua, plugins)
}

/// The per-type method tables, in type-id order.
fn method_tables(lua: &Lua, plugins: &'static Plugins) -> mlua::Result<Table> {
    let all = lua.create_table()?;
    for (id, ty) in plugins.handle_types.iter().enumerate() {
        let methods = lua.create_table()?;
        for m in &ty.methods {
            methods.set(m.name.as_str(), host::wrap(lua, plugins, m)?)?;
        }
        all.raw_set(id + 1, methods)?;
    }
    Ok(all)
}
