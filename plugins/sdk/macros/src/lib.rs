//! `#[plugin]`: turns a module of plain Rust functions into a Red Crescent
//! plugin. Documented on the re-export in `red-crescent-plugin`.

use std::ffi::CString;

use proc_macro::TokenStream;
use proc_macro2::{Literal, Span, TokenStream as Tokens};
use quote::{format_ident, quote, quote_spanned};
use syn::spanned::Spanned;
use syn::{
    Attribute, FnArg, GenericArgument, Ident, ImplItem, Item, ItemFn, ItemImpl, ItemMod, LitStr,
    PathArguments, ReturnType, Signature, Type, parse_macro_input,
};

/// Turns an inline module into a plugin: generates `rc_plugin_abi`,
/// `rc_plugin_install` and a C entry point for every function and method.
///
/// Inside the module:
///
/// - `#[function]` on a `fn` makes it `plugin.name(...)`. Options:
///   `blocking` (it may wait on disk or network), `name = "lua_name"`, and
///   `raw` for a hand-written `unsafe extern "C" fn(*mut RcCall, *mut c_void)
///   -> RcStatus`.
/// - `#[install]` on at most one `fn(&Context) -> Result<State>` (or
///   `-> State`) runs once at startup. A function or method that takes
///   `&State` gets it; `State` must be `Send + Sync`.
/// - `#[constant]` on a `const` puts its value in the plugin's table: a
///   string, `bool`, `i64`, `f64`, or `Value::Null` (`sqlite.NULL`).
/// - `#[handle]` on an `impl Type` block makes `Type` a Lua object. Every
///   `&self` method in the block becomes a Lua method (`obj:name(...)`);
///   `#[method(blocking, name = "...")]` adjusts one. Put helpers in a plain
///   `impl` block. Returning a `Type` from a function hands it to Lua; Lua
///   drops it once collected. Methods take `&self` because a callback into
///   Lua can call another method on the same object: mutate through `Cell`
///   or `RefCell`.
///
/// Parameters are matched by type: `&Call` is the running call, `&State` the
/// install's state, and everything else a Lua argument, in order, converted
/// with `FromLua`. A last `Variadic<T>` takes the rest. The return type is
/// anything `IntoLuaMulti` (a value, a tuple, `()`), or a `Result` of it.
#[proc_macro_attribute]
pub fn plugin(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        let attr = Tokens::from(attr);
        return syn::Error::new(attr.span(), "#[plugin] takes no arguments")
            .to_compile_error()
            .into();
    }
    let mut module = parse_macro_input!(item as ItemMod);
    match expand(&mut module) {
        Ok(()) => quote!(#module).into(),
        Err(e) => e.to_compile_error().into(),
    }
}

#[derive(Default)]
struct Options {
    name: Option<String>,
    blocking: bool,
    raw: bool,
}

struct Export {
    lua_name: String,
    entry: Ident,
    flags: Tokens,
}

struct HandleType {
    ty: Type,
    lua_name: String,
    statics: Ident,
    methods: Vec<Export>,
}

struct Install {
    ident: Ident,
    takes_context: bool,
    returns_result: bool,
    state: Option<Type>,
}

fn expand(module: &mut ItemMod) -> syn::Result<()> {
    let span = module.span();
    let Some((_, items)) = module.content.as_mut() else {
        return Err(syn::Error::new(
            span,
            "#[plugin] needs an inline module: `mod name { ... }`",
        ));
    };

    let mut install = None;
    for item in items.iter_mut() {
        if let Item::Fn(f) = item
            && let Some(attr) = take_attr(&mut f.attrs, "install")
        {
            if install.is_some() {
                return Err(syn::Error::new(
                    attr.span(),
                    "only one #[install] per plugin",
                ));
            }
            install = Some(parse_install(f)?);
        }
    }
    let state = install.as_ref().and_then(|i| i.state.clone());

    let mut generated = Vec::new();
    let mut functions = Vec::new();
    let mut constants = Vec::new();
    let mut handles = Vec::new();
    for item in items.iter_mut() {
        match item {
            Item::Fn(f) => {
                if let Some(attr) = take_attr(&mut f.attrs, "function") {
                    let opts = parse_options(&attr, true)?;
                    let lua_name = lua_name(&opts, &f.sig.ident);
                    let flags = flags(&opts);
                    if opts.raw {
                        functions.push(Export {
                            lua_name,
                            entry: f.sig.ident.clone(),
                            flags,
                        });
                    } else {
                        let entry = format_ident!("__rc_fn_{}", f.sig.ident);
                        generated.push(entry_point(&entry, &f.sig, None, state.as_ref())?);
                        functions.push(Export {
                            lua_name,
                            entry,
                            flags,
                        });
                    }
                }
            }
            Item::Const(c) => {
                if let Some(attr) = take_attr(&mut c.attrs, "constant") {
                    let opts = parse_options(&attr, false)?;
                    if opts.blocking {
                        return Err(syn::Error::new(attr.span(), "a constant cannot block"));
                    }
                    constants.push((lua_name(&opts, &c.ident), c.ident.clone()));
                }
            }
            Item::Impl(imp) => {
                if let Some(attr) = take_attr(&mut imp.attrs, "handle") {
                    let handle = handle_type(imp, &attr, state.as_ref(), &mut generated)?;
                    handles.push(handle);
                }
            }
            _ => {}
        }
    }

    for h in &handles {
        generated.push(handle_impls(h));
    }
    generated.push(exports(install.as_ref(), &functions, &constants, &handles));
    for tokens in generated {
        items.push(Item::Verbatim(tokens));
    }
    Ok(())
}

fn take_attr(attrs: &mut Vec<Attribute>, name: &str) -> Option<Attribute> {
    let i = attrs.iter().position(|a| a.path().is_ident(name))?;
    Some(attrs.remove(i))
}

fn parse_options(attr: &Attribute, allow_raw: bool) -> syn::Result<Options> {
    let mut opts = Options::default();
    if matches!(attr.meta, syn::Meta::Path(_)) {
        return Ok(opts);
    }
    attr.parse_nested_meta(|meta| {
        if meta.path.is_ident("blocking") {
            opts.blocking = true;
        } else if meta.path.is_ident("name") {
            opts.name = Some(meta.value()?.parse::<LitStr>()?.value());
        } else if allow_raw && meta.path.is_ident("raw") {
            opts.raw = true;
        } else {
            return Err(meta.error(if allow_raw {
                "expected `blocking`, `raw` or `name = \"...\"`"
            } else {
                "expected `blocking` or `name = \"...\"`"
            }));
        }
        Ok(())
    })?;
    Ok(opts)
}

fn lua_name(opts: &Options, ident: &Ident) -> String {
    opts.name.clone().unwrap_or_else(|| {
        let s = ident.to_string();
        s.strip_prefix("r#").map(str::to_owned).unwrap_or(s)
    })
}

fn flags(opts: &Options) -> Tokens {
    if opts.blocking {
        quote!(::red_crescent_plugin::__private::RC_FN_BLOCKING)
    } else {
        quote!(0)
    }
}

fn c_str(s: &str, span: Span) -> syn::Result<Literal> {
    let c = CString::new(s).map_err(|_| syn::Error::new(span, "a name cannot contain NUL"))?;
    let mut lit = Literal::c_string(&c);
    lit.set_span(span);
    Ok(lit)
}

/// The last segment of a plain path type, e.g. `Sqlite` in `crate::Sqlite`.
fn last_segment(ty: &Type) -> Option<&syn::PathSegment> {
    match ty {
        Type::Path(p) if p.qself.is_none() => p.path.segments.last(),
        _ => None,
    }
}

fn parse_install(f: &ItemFn) -> syn::Result<Install> {
    let sig = &f.sig;
    if sig.inputs.len() > 1 {
        return Err(syn::Error::new(
            sig.inputs.span(),
            "#[install] takes `&Context` or nothing",
        ));
    }
    let (returns_result, state) = match &sig.output {
        ReturnType::Default => (false, None),
        ReturnType::Type(_, ty) => match last_segment(ty) {
            Some(seg) if seg.ident == "Result" => {
                let inner = match &seg.arguments {
                    PathArguments::AngleBracketed(args) => {
                        args.args.first().and_then(|a| match a {
                            GenericArgument::Type(t) => Some(t.clone()),
                            _ => None,
                        })
                    }
                    _ => None,
                };
                let inner =
                    inner.ok_or_else(|| syn::Error::new(ty.span(), "expected `Result<State>`"))?;
                (true, Some(inner))
            }
            _ => (false, Some((**ty).clone())),
        },
    };
    let state = state.filter(|t| !matches!(t, Type::Tuple(t) if t.elems.is_empty()));
    Ok(Install {
        ident: sig.ident.clone(),
        takes_context: !sig.inputs.is_empty(),
        returns_result,
        state,
    })
}

enum Param {
    Call { by_ref: bool },
    State,
    Lua,
}

fn classify(ty: &Type, state: Option<&Type>) -> Param {
    let (inner, by_ref) = match ty {
        Type::Reference(r) => (&*r.elem, true),
        other => (other, false),
    };
    let Some(seg) = last_segment(inner) else {
        return Param::Lua;
    };
    if seg.ident == "Call" {
        return Param::Call { by_ref };
    }
    if by_ref
        && let Some(state) = state.and_then(last_segment)
        && state.ident == seg.ident
    {
        return Param::State;
    }
    Param::Lua
}

/// The `extern "C"` function Lua calls for `sig`: converts the arguments,
/// calls it, pushes what it returns. `owner` is the handle type for a method.
fn entry_point(
    entry: &Ident,
    sig: &Signature,
    owner: Option<&Type>,
    state: Option<&Type>,
) -> syn::Result<Tokens> {
    if sig.asyncness.is_some() {
        return Err(syn::Error::new(
            sig.span(),
            "plugin functions cannot be async",
        ));
    }
    if let Some(p) = sig.generics.type_params().next() {
        return Err(syn::Error::new(
            p.span(),
            "plugin functions cannot be generic over types",
        ));
    }
    let ident = &sig.ident;
    let mut lets = Vec::new();
    let mut args = Vec::new();
    for (i, input) in sig.inputs.iter().enumerate() {
        match input {
            FnArg::Receiver(r) => {
                if owner.is_none() {
                    return Err(syn::Error::new(r.span(), "a #[function] takes no `self`"));
                }
                args.push(quote!(__this));
            }
            FnArg::Typed(pt) => {
                let value = match classify(&pt.ty, state) {
                    Param::Call { by_ref: true } => quote!(__call),
                    Param::Call { by_ref: false } => quote!(*__call),
                    Param::State => {
                        let state = state.expect("Param::State implies a state type");
                        lets.insert(
                            0,
                            quote!(let __state = unsafe {
                                ::red_crescent_plugin::__private::state::<#state>(__userdata)
                            };),
                        );
                        quote!(__state)
                    }
                    Param::Lua => {
                        let v = format_ident!("__arg{}", i);
                        lets.push(quote_spanned!(pt.ty.span()=> let #v = __args.next()?;));
                        quote!(#v)
                    }
                };
                args.push(value);
            }
        }
    }
    let (args_new, this, callee) = match owner {
        Some(ty) => (
            quote!(method),
            quote!(let __this = __args.this::<#ty>()?;),
            quote!(<#ty>::#ident),
        ),
        None => (quote!(function), quote!(), quote!(#ident)),
    };
    Ok(quote! {
        #[allow(non_snake_case)]
        unsafe extern "C" fn #entry(
            raw: *mut ::red_crescent_plugin::__private::RcCall,
            __userdata: ::red_crescent_plugin::__private::Userdata,
        ) -> ::red_crescent_plugin::__private::RcStatus {
            let _ = __userdata;
            let __body = |__call: &::red_crescent_plugin::Call<'_>| {
                #[allow(unused_mut)]
                let mut __args = ::red_crescent_plugin::__private::Args::#args_new(__call);
                #this
                #(#lets)*
                ::red_crescent_plugin::__private::ret(__call, #callee(#(#args),*))
            };
            unsafe { ::red_crescent_plugin::__private::run(raw, __body) }
        }
    })
}

fn handle_type(
    imp: &mut ItemImpl,
    attr: &Attribute,
    state: Option<&Type>,
    generated: &mut Vec<Tokens>,
) -> syn::Result<HandleType> {
    if let Some((_, path, _)) = &imp.trait_ {
        return Err(syn::Error::new(
            path.span(),
            "#[handle] goes on an inherent `impl Type`",
        ));
    }
    if !imp.generics.params.is_empty() {
        return Err(syn::Error::new(
            imp.generics.span(),
            "a #[handle] type cannot be generic",
        ));
    }
    let ty = (*imp.self_ty).clone();
    let type_ident = last_segment(&ty)
        .map(|s| s.ident.clone())
        .ok_or_else(|| syn::Error::new(ty.span(), "#[handle] needs a named type"))?;
    let opts = parse_options(attr, false)?;
    if opts.blocking {
        return Err(syn::Error::new(
            attr.span(),
            "`blocking` goes on #[method], not #[handle]",
        ));
    }
    let type_name = lua_name(&opts, &type_ident);

    let mut methods = Vec::new();
    for item in imp.items.iter_mut() {
        let ImplItem::Fn(f) = item else { continue };
        let attr = take_attr(&mut f.attrs, "method");
        let Some(receiver) = f.sig.receiver() else {
            if let Some(attr) = attr {
                return Err(syn::Error::new(attr.span(), "a #[method] takes `&self`"));
            }
            continue;
        };
        if receiver.reference.is_none() || receiver.mutability.is_some() {
            return Err(syn::Error::new(
                receiver.span(),
                "handle methods take `&self`: a callback into Lua can reach the same \
                 object again, so mutate through `Cell` or `RefCell`",
            ));
        }
        let opts = match &attr {
            Some(attr) => parse_options(attr, false)?,
            None => Options::default(),
        };
        let entry = format_ident!("__rc_method_{}_{}", type_ident, f.sig.ident);
        generated.push(entry_point(&entry, &f.sig, Some(&ty), state)?);
        methods.push(Export {
            lua_name: lua_name(&opts, &f.sig.ident),
            entry,
            flags: flags(&opts),
        });
    }
    Ok(HandleType {
        ty,
        lua_name: type_name,
        statics: format_ident!("__RC_METHODS_{}", type_ident),
        methods,
    })
}

fn handle_impls(h: &HandleType) -> Tokens {
    let HandleType {
        ty,
        lua_name,
        statics,
        methods,
    } = h;
    let name = c_str(lua_name, ty.span()).unwrap_or_else(|_| Literal::c_string(c"handle"));
    let count = methods.len();
    let entries = methods.iter().map(|m| {
        let name =
            c_str(&m.lua_name, Span::call_site()).unwrap_or_else(|_| Literal::c_string(c"_"));
        let entry = &m.entry;
        let flags = &m.flags;
        quote!(::red_crescent_plugin::__private::Method::new(#name, #entry, #flags))
    });
    quote! {
        unsafe impl ::red_crescent_plugin::Handle for #ty {
            const NAME: &'static ::core::ffi::CStr = #name;
            fn type_slot() -> &'static ::core::sync::atomic::AtomicU32 {
                static ID: ::core::sync::atomic::AtomicU32 =
                    ::core::sync::atomic::AtomicU32::new(u32::MAX);
                &ID
            }
        }

        impl ::red_crescent_plugin::IntoLua for #ty {
            fn into_lua<'a>(
                self,
                _: &::red_crescent_plugin::Call<'_>,
            ) -> ::red_crescent_plugin::Result<::red_crescent_plugin::__private::Lowered<'a>> {
                Ok(::red_crescent_plugin::__private::Lowered::handle(self))
            }
        }

        impl<'c> ::red_crescent_plugin::FromLua<'c> for &'c #ty {
            fn from_lua(
                value: ::red_crescent_plugin::Value<'c>,
            ) -> ::red_crescent_plugin::Result<Self> {
                ::red_crescent_plugin::__private::handle_from_lua(value)
            }
        }

        #[allow(non_upper_case_globals)]
        static #statics: [::red_crescent_plugin::__private::Method; #count] = [#(#entries),*];
    }
}

fn exports(
    install: Option<&Install>,
    functions: &[Export],
    constants: &[(String, Ident)],
    handles: &[HandleType],
) -> Tokens {
    let mut run_install = quote!(let __ud = ::core::ptr::null_mut(););
    if let Some(install) = install {
        let ident = &install.ident;
        let ctx = install.takes_context.then(|| quote!(__ctx));
        let question = install.returns_result.then(|| quote!(?));
        run_install = match &install.state {
            Some(state) => quote! {
                let __state: #state = #ident(#ctx) #question;
                let __ud = ::red_crescent_plugin::__private::leak_state(__state);
            },
            None => quote! {
                #ident(#ctx) #question;
                let __ud = ::core::ptr::null_mut();
            },
        };
    }
    let register_handles = handles.iter().map(|h| {
        let ty = &h.ty;
        let statics = &h.statics;
        quote!(unsafe { __reg.handle::<#ty>(&#statics, __ud) }?;)
    });
    let register_functions = functions.iter().map(|f| {
        let name = c_str(&f.lua_name, f.entry.span()).unwrap_or_else(|_| Literal::c_string(c"_"));
        let entry = &f.entry;
        let flags = &f.flags;
        quote!(unsafe { __reg.function(#name, #entry, #flags, __ud) }?;)
    });
    let register_constants = constants.iter().map(|(lua_name, ident)| {
        let name = c_str(lua_name, ident.span()).unwrap_or_else(|_| Literal::c_string(c"_"));
        quote!(__reg.constant(#name, #ident)?;)
    });
    quote! {
        #[unsafe(no_mangle)]
        pub extern "C" fn rc_plugin_abi() -> u32 {
            ::red_crescent_plugin::__private::RC_PLUGIN_ABI
        }

        /// # Safety
        /// Called by the server, once, with the pointers the ABI describes.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn rc_plugin_install(
            ctx: *const ::red_crescent_plugin::__private::RcPluginContext,
            reg: *mut ::red_crescent_plugin::__private::RcRegistryCOM,
        ) -> ::red_crescent_plugin::__private::RcStatus {
            unsafe {
                ::red_crescent_plugin::__private::install(ctx, reg, |__ctx, __reg| {
                    let _ = __ctx;
                    #run_install
                    #(#register_handles)*
                    #(#register_functions)*
                    #(#register_constants)*
                    Ok(())
                })
            }
        }
    }
}
