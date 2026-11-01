use crate::attrs::TypeAttrs;
use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Error, Fields, Result};

pub fn derive(input: &DeriveInput) -> Result<TokenStream> {
    let Data::Enum(data) = &input.data else {
        return Err(Error::new_spanned(input, "ODataEnum needs an enum"));
    };
    let ident = &input.ident;
    let mut stored_as_string = false;
    let mut is_flags = false;
    let mut type_name = ident.to_string();
    for attr in input.attrs.iter().filter(|a| a.path().is_ident("odata")) {
        attr.parse_nested_meta(|m| {
            if m.path.is_ident("as_string") {
                stored_as_string = true;
            } else if m.path.is_ident("flags") {
                is_flags = true;
            } else if m.path.is_ident("name") {
                type_name = m.value()?.parse::<syn::LitStr>()?.value();
            } else {
                return Err(m.error("expected as_string, flags or name"));
            }
            Ok(())
        })?;
    }
    let mut members = Vec::new();
    for variant in &data.variants {
        if !matches!(variant.fields, Fields::Unit) {
            return Err(Error::new_spanned(
                variant,
                "ODataEnum variants cannot hold data",
            ));
        }
        let v = &variant.ident;
        let name = TypeAttrs::parse(&variant.attrs)?
            .name
            .unwrap_or_else(|| v.to_string());
        members.push(quote!((#name.to_owned(), #ident::#v as i64)));
    }
    let stem = ident.to_string();
    Ok(quote! {
        impl ::odatabase::edm::EdmField for #ident {
            fn field_type(b: &mut ::odatabase::edm::ModelBuilder) -> ::odatabase::edm::FieldType {
                if b.first_visit(#stem) {
                    b.add_enum_type(::odatabase::edm::EnumType {
                        name: #type_name.to_owned(),
                        is_flags: #is_flags,
                        stored_as_string: #stored_as_string,
                        members: vec![#(#members),*],
                    });
                }
                ::odatabase::edm::FieldType {
                    kind: ::odatabase::edm::TypeKind::Enum(#type_name.to_owned()),
                    nullable: false,
                    collection: false,
                }
            }
        }
    })
}
