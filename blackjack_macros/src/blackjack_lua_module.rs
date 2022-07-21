use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote, ToTokens};
use syn::{
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
    Attribute, Expr, ItemFn, Path, PathArguments, PathSegment, ReturnType, Token, Type, TypePath,
};

use crate::utils::{ExprUtils, SynParseBufferExt};

#[derive(Default, Debug)]
struct LuaFnAttrs {
    under: Option<String>,
}

impl Parse for LuaFnAttrs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let properties = input.comma_separated_fn(|input| {
            let lhs: Ident = input.parse()?;
            let _eq_sign = input.expect_token::<Token![=]>();
            let rhs: Expr = input.parse()?;
            Ok((lhs, rhs))
        })?;

        let mut lua_attr = LuaFnAttrs::default();

        for (key, val) in properties.iter() {
            if key == "under" {
                lua_attr.under =
                    Some(val.assume_string_literal("Value for 'under' must be a string")?);
            }
        }

        Ok(lua_attr)
    }
}

#[derive(Debug)]
struct LuaFnDef {
    register_fn_ident: Ident,
    register_fn_item: TokenStream,
}

fn unwrap_result(typ: &Type) -> Option<&Type> {
    if let Type::Path(typepath) = typ {
        if let Some(seg) = typepath.path.segments.first() {
            if seg.ident == "Result" {
                if let PathArguments::AngleBracketed(bracketed) = &seg.arguments {
                    if let Some(syn::GenericArgument::Type(t)) = bracketed.args.iter().next() {
                        return Some(t);
                    }
                }
            }
        }
    }
    None
}

fn analyze_lua_fn(item_fn: &ItemFn, attrs: &LuaFnAttrs) -> syn::Result<LuaFnDef> {
    if item_fn.sig.generics.params.iter().count() > 0 {
        return Err(syn::Error::new(
            item_fn.sig.ident.span(),
            "Functions exported to lua can't have generic parameters.",
        ));
    } else if item_fn.sig.asyncness.is_some() {
        return Err(syn::Error::new(
            item_fn.sig.ident.span(),
            "Functions exported to lua can't be marked async.",
        ));
    }

    enum ArgKind {
        Owned,
        Ref,
        RefMut,
    }

    struct WrapperArg {
        kind: ArgKind,
        typ: Type,
        name: Ident,
    }

    let mut wrapper_fn_args = vec![];

    for arg in item_fn.sig.inputs.iter() {
        match arg {
            syn::FnArg::Receiver(_) => {
                return Err(syn::Error::new(
                    item_fn.sig.ident.span(),
                    "Can't use self here.",
                ));
            }
            syn::FnArg::Typed(t) => {
                let arg_name = match &*t.pat {
                    syn::Pat::Ident(id) => id.clone(),
                    _ => todo!(),
                };
                match &*t.ty {
                    Type::Reference(inner) => {
                        wrapper_fn_args.push(WrapperArg {
                            kind: if inner.mutability.is_some() {
                                ArgKind::RefMut
                            } else {
                                ArgKind::Ref
                            },
                            typ: *inner.elem.clone(),
                            name: arg_name.ident,
                        });
                    }
                    t => {
                        wrapper_fn_args.push(WrapperArg {
                            kind: ArgKind::Owned,
                            typ: t.clone(),
                            name: arg_name.ident,
                        });
                    }
                }
            }
        }
    }

    let register_fn_ident = format_ident!("__blackjack_export_{}_to_lua", &item_fn.sig.ident);
    let original_fn_name = item_fn.sig.ident.to_string();
    let original_fn_ident = &item_fn.sig.ident;

    let signature = {
        let types = wrapper_fn_args.iter().map(|arg| match &arg.kind {
            ArgKind::Owned => arg.typ.to_token_stream(),
            ArgKind::Ref | ArgKind::RefMut => quote! { mlua::AnyUserData },
        });
        let names = wrapper_fn_args.iter().map(|arg| &arg.name);

        quote! { (#(#names),*) : (#(#types),*) }
    };

    let borrows = wrapper_fn_args.iter().filter_map(|arg| {
        let name = &arg.name;
        let typ = &arg.typ;
        match arg.kind {
            ArgKind::Owned => None,
            ArgKind::Ref => Some(quote! {
                let #name = #name.borrow::<#typ>()?;
            }),
            ArgKind::RefMut => Some(quote! {
                let mut #name = #name.borrow_mut::<#typ>()?;
            }),
        }
    });

    let invoke_args = wrapper_fn_args
        .iter()
        .map(|WrapperArg { kind, name, .. }| match kind {
            ArgKind::Owned => quote! { #name },
            ArgKind::Ref => quote! { &#name},
            ArgKind::RefMut => quote! { &mut #name },
        });

    let (ret_typ, ret_is_result) = match &item_fn.sig.output {
        ReturnType::Default => (quote! { () }, false),
        ReturnType::Type(_, t) => match unwrap_result(t) {
            Some(inner) => (quote! { #inner }, true),
            None => (quote! { #t }, false),
        },
    };

    let call_fn_and_map_result = if ret_is_result {
        quote! {
            match #original_fn_ident(#(#invoke_args),*) {
                Ok(val) => { mlua::Result::Ok(val) },
                Err(err) => {
                    mlua::Result::Err(mlua::Error::RuntimeError(format!("{:?}", err)))
                }
            }
        }
    } else {
        quote! {
            mlua::Result::Ok(#original_fn_ident(#(#invoke_args),*))
        }
    };

    Ok(LuaFnDef {
        register_fn_item: quote! {
            pub fn #register_fn_ident(lua: &mlua::Lua) {
                fn __inner(lua: &mlua::Lua, #signature) -> mlua::Result<#ret_typ> {
                    #(#borrows)*
                    #call_fn_and_map_result
                }

                // TODO: This unwrap is not correct. If the table is not there it should be created.
                let table = lua.globals().get::<_, mlua::Table>("Ops").unwrap();
                table.set(
                    #original_fn_name,
                    lua.create_function(__inner).unwrap()
                ).unwrap()

            }
        },
        register_fn_ident,
    })
}

fn collect_lua_attr(attrs: &mut Vec<Attribute>) -> Option<LuaFnAttrs> {
    let mut lua_attrs = vec![];
    let mut to_remove = vec![];
    for (i, attr) in attrs.iter().enumerate() {
        if let Some(ident) = attr.path.get_ident() {
            if ident == "lua" {
                let lua_attr: LuaFnAttrs = attr.parse_args().unwrap();
                lua_attrs.push(lua_attr);
                to_remove.push(i);
            }
        }
    }

    for i in to_remove.into_iter() {
        attrs.remove(i);
    }

    if lua_attrs.len() > 1 {
        panic!("Only one #[lua(...)] annotation is supported per function.")
    }
    lua_attrs.into_iter().next()
}

pub(crate) fn blackjack_lua_module2(
    mut module: syn::ItemMod,
) -> Result<TokenStream, Box<dyn std::error::Error>> {
    // Any new items that will be appended at the end of the module are stored here.
    let mut new_items = vec![];

    if let Some((_, items)) = module.content.as_mut() {
        for item in items.iter_mut() {
            match item {
                syn::Item::Fn(item_fn) => {
                    let lua_attr = collect_lua_attr(&mut item_fn.attrs);
                    if let Some(lua_attr) = lua_attr {
                        new_items.push(analyze_lua_fn(item_fn, &lua_attr)?);
                    }
                }
                syn::Item::Impl(_) => todo!(),
                _ => { /* Ignore */ }
            }
        }
    } else {
        panic!("This macro only supports inline modules")
    }

    let global_register_fn_calls = new_items.iter().map(|LuaFnDef { register_fn_ident, .. }| {
        quote! { #register_fn_ident(lua); }
    });


    let original_items = module.content.as_ref().unwrap().1.iter();
    let new_items = new_items.iter().map(|n| &n.register_fn_item);
    let mod_name = module.ident;
    let visibility = module.vis;

    Ok(quote! {
        // TODO: This adds `pub` to mod that may not
        #visibility mod #mod_name {
            #(#original_items)*
            #(#new_items)*

            pub fn __blackjack_register_lua_fns(lua: &mlua::Lua) {
                #(#global_register_fn_calls)*
            }
        }
    })
}

#[cfg(test)]
mod test {

    use super::*;
    use crate::utils::write_and_fmt;

    #[test]
    fn test() {
        let input = quote! {
            pub mod lua_fns {
                use super::*;

                #[lua(under = "Ops")]
                pub fn test_exported_fn(
                    mesh: &mut HalfEdgeMesh,
                ) -> Result<i32> {
                    let mut conn = mesh.write_connectivity();
                    let f = conn.iter_faces().next().unwrap().0;
                    conn.remove_face(f);
                    Ok(42)
                }
            }
        };
        let module = syn::parse2(input).unwrap();
        write_and_fmt("/tmp/test.rs", blackjack_lua_module2(module).unwrap()).unwrap();
    }
}
