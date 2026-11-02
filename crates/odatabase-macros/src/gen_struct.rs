use crate::attrs::{FieldAttrs, NavKind, TypeAttrs, pairs_tokens};
use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Error, Field, Fields, Result, Type};

fn named_fields(input: &DeriveInput) -> Result<Vec<&Field>> {
    match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(f) => Ok(f.named.iter().collect()),
            _ => Err(Error::new_spanned(input, "expected named fields")),
        },
        _ => Err(Error::new_spanned(input, "expected a struct")),
    }
}

fn is_vec(ty: &Type) -> bool {
    matches!(ty, Type::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == "Vec"))
}

/// Emits a `Property` construction, leaving it in `p`.
fn property(field: &Field, f: &FieldAttrs) -> Result<TokenStream> {
    let ident = field.ident.as_ref().expect("named field");
    let rust = ident.to_string();
    let ty = &field.ty;
    if is_vec(ty) && f.collection.is_none() {
        return Err(Error::new_spanned(
            ty,
            "a Vec field needs #[odata(collection(...))]",
        ));
    }
    let name = match &f.name {
        Some(n) => quote!(#n.to_owned()),
        None => quote!(::odatabase::edm::edm_case(#rust)),
    };
    let column = if f.embedded {
        f.prefix.clone().unwrap_or_default()
    } else {
        f.column.clone().unwrap_or_else(|| rust.clone())
    };
    let opt = |v: &Option<u32>| match v {
        Some(v) => quote!(Some(#v)),
        None => quote!(None),
    };
    let (max_length, precision, scale, srid) = (
        opt(&f.max_length),
        opt(&f.precision),
        opt(&f.scale),
        opt(&f.srid),
    );
    let collection = match &f.collection {
        Some(c) => {
            let (table, owner) = (&c.table, pairs_tokens(&c.owner));
            let some = |v: &Option<String>| match v {
                Some(s) => quote!(Some(#s.to_owned())),
                None => quote!(None),
            };
            let (value, order) = (some(&c.value_column), some(&c.order_column));
            quote!(Some(::odatabase::edm::CollectionTable {
                table: #table.to_owned(),
                owner: #owner,
                value_column: #value,
                order_column: #order,
            }))
        }
        None => quote!(None),
    };
    let transient = match &f.transient {
        Some(req) => quote!(Some(::odatabase::edm::Transient {
            requires: vec![#(#req.to_owned()),*],
        })),
        None => quote!(None),
    };
    let description = match &f.description {
        Some(d) => {
            let (table, join) = (&d.table, pairs_tokens(&d.join));
            let (lang, text) = (&d.language_column, &d.text_column);
            quote!(Some(::odatabase::edm::DescriptionMapping {
                table: #table.to_owned(),
                join: #join,
                language_column: #lang.to_owned(),
                text_column: #text.to_owned(),
            }))
        }
        None => quote!(None),
    };
    let protections = f.protections.iter().map(|(claim, wildcards)| {
        quote!(::odatabase::edm::Protection { claim: #claim.to_owned(), allow_wildcards: #wildcards })
    });
    let visible = &f.visible_for;
    let annotations = &f.annotations;
    let (searchable, computed) = (f.searchable, f.computed);
    // An embedded value whose columns are all null reads as null, whatever the Rust type says.
    let embedded = f.embedded;
    Ok(quote! {
        let ft = <#ty as ::odatabase::edm::EdmField>::field_type(b);
        let mut p = ::odatabase::edm::Property::new(#name, #column, ft.kind);
        p.nullable = ft.nullable || #embedded;
        p.facets = ::odatabase::edm::Facets {
            max_length: #max_length,
            precision: #precision,
            scale: #scale,
            srid: #srid,
        };
        p.collection = #collection;
        p.searchable = #searchable;
        p.computed = #computed;
        p.transient = #transient;
        p.description = #description;
        p.protections = vec![#(#protections),*];
        p.visible_for = vec![#(#visible.to_owned()),*];
        p.annotations = vec![#(#annotations),*];
    })
}

pub fn complex(input: &DeriveInput) -> Result<TokenStream> {
    let ident = &input.ident;
    let attrs = TypeAttrs::parse(&input.attrs)?;
    let stem = ident.to_string();
    let type_name = attrs.name.clone().unwrap_or_else(|| stem.clone());
    let annotations = &attrs.annotations;
    let mut props = Vec::new();
    for field in named_fields(input)? {
        let f = FieldAttrs::parse(&field.attrs)?;
        if f.ignore {
            continue;
        }
        if f.key || f.etag || f.nav.is_some() {
            return Err(Error::new_spanned(field, "not allowed on a complex type"));
        }
        let build = property(field, &f)?;
        props.push(quote!({ #build t.properties.push(p); }));
    }
    Ok(quote! {
        impl ::odatabase::edm::EdmField for #ident {
            fn field_type(b: &mut ::odatabase::edm::ModelBuilder) -> ::odatabase::edm::FieldType {
                if b.first_visit(#stem) {
                    let mut t = ::odatabase::edm::ComplexType {
                        name: #type_name.to_owned(),
                        annotations: vec![#(#annotations),*],
                        ..Default::default()
                    };
                    #(#props)*
                    b.add_complex_type(t);
                }
                ::odatabase::edm::FieldType {
                    kind: ::odatabase::edm::TypeKind::Complex(#type_name.to_owned()),
                    nullable: false,
                    collection: false,
                }
            }
        }
    })
}

pub fn entity(input: &DeriveInput) -> Result<TokenStream> {
    let ident = &input.ident;
    let attrs = TypeAttrs::parse(&input.attrs)?;
    let stem = ident.to_string();
    let type_name = attrs.name.clone().unwrap_or_else(|| stem.clone());
    let mut body = Vec::new();
    let mut keys = Vec::new();
    let mut etag = quote!(None);
    let mut targets = Vec::new();
    for field in named_fields(input)? {
        let f = FieldAttrs::parse(&field.attrs)?;
        if f.ignore {
            continue;
        }
        let ident = field.ident.as_ref().expect("named field");
        let rust = ident.to_string();
        let name = match &f.name {
            Some(n) => quote!(#n.to_owned()),
            None => quote!(::odatabase::edm::edm_case(#rust)),
        };
        if let Some(nav) = &f.nav {
            let target = nav.target.as_ref().expect("checked by the parser");
            let collection = matches!(nav.kind, NavKind::HasMany);
            let nullable = !collection && !nav.required;
            let join = pairs_tokens(&nav.on);
            let partner = match &nav.partner {
                Some(s) => quote!(Some(#s.to_owned())),
                None => quote!(None),
            };
            let join_table = match &nav.join_table {
                Some((table, source, tgt)) => {
                    let (source, tgt) = (pairs_tokens(source), pairs_tokens(tgt));
                    quote!(Some(::odatabase::edm::JoinTable {
                        table: #table.to_owned(),
                        source: #source,
                        target: #tgt,
                    }))
                }
                None => quote!(None),
            };
            let contains = nav.contains_target;
            let visible = &f.visible_for;
            let annotations = &f.annotations;
            body.push(quote! {
                t.navigation.push(::odatabase::edm::NavigationProperty {
                    name: #name,
                    target: <#target as ::odatabase::edm::EdmEntity>::edm_name().to_owned(),
                    collection: #collection,
                    nullable: #nullable,
                    join: #join,
                    join_table: #join_table,
                    partner: #partner,
                    contains_target: #contains,
                    visible_for: vec![#(#visible.to_owned()),*],
                    annotations: vec![#(#annotations),*],
                });
            });
            targets.push(quote!(b.register_entity::<#target>();));
            continue;
        }
        if f.key {
            keys.push(name.clone());
        }
        if f.etag {
            etag = quote!(Some(#name));
        }
        let build = property(field, &f)?;
        body.push(quote!({ #build t.properties.push(p); }));
    }

    let is_abstract = attrs.is_abstract;
    let has_base = attrs.base.is_some();
    let table = match &attrs.table {
        Some(t) => quote!(#t.to_owned()),
        None if has_base && attrs.joined.is_none() => quote!(String::new()),
        None => quote!(::odatabase::edm::snake_case(#type_name)),
    };
    let base = match &attrs.base {
        Some(b) => {
            targets.push(quote!(b.register_entity::<#b>();));
            quote!(Some(<#b as ::odatabase::edm::EdmEntity>::edm_name().to_owned()))
        }
        None => quote!(None),
    };
    let inheritance = match (&attrs.joined, &attrs.discriminator) {
        (Some(j), d) => {
            let join = pairs_tokens(j);
            let disc = match d {
                Some((c, v)) => quote!(Some((#c.to_owned(), #v.to_owned()))),
                None => quote!(None),
            };
            quote!(Some(::odatabase::edm::Inheritance::Joined { join: #join, discriminator: #disc }))
        }
        (None, Some((c, v))) => quote!(Some(::odatabase::edm::Inheritance::SingleTable {
            discriminator: #c.to_owned(),
            value: #v.to_owned(),
        })),
        (None, None) => quote!(None),
    };
    let media = match &attrs.media {
        Some((column, ty, ty_col)) => {
            let ty = ty
                .clone()
                .unwrap_or_else(|| "application/octet-stream".to_owned());
            let ty_col = match ty_col {
                Some(s) => quote!(Some(#s.to_owned())),
                None => quote!(None),
            };
            quote!(Some(::odatabase::edm::MediaStream {
                content_column: #column.to_owned(),
                content_type: #ty.to_owned(),
                content_type_column: #ty_col,
            }))
        }
        None => quote!(None),
    };
    let visible = &attrs.visible_for;
    let track_changes = attrs.track_changes;
    let annotations = &attrs.annotations;
    let query_extension = attrs.query_extension;
    let contained = attrs.no_set || attrs.singleton.is_some() || is_abstract || has_base;
    let set = if contained && attrs.set.is_none() {
        quote!()
    } else {
        let name = match &attrs.set {
            Some(s) => quote!(#s.to_owned()),
            None => quote!(::odatabase::edm::plural(#type_name)),
        };
        let include = !attrs.hidden_set;
        quote! {
            b.add_entity_set(::odatabase::edm::EntitySet {
                name: #name,
                entity_type: #type_name.to_owned(),
                include_in_service_document: #include,
                annotations: Vec::new(),
            });
        }
    };
    let singleton = match &attrs.singleton {
        Some(s) => quote! {
            b.add_singleton(::odatabase::edm::Singleton {
                name: #s.to_owned(),
                entity_type: #type_name.to_owned(),
                annotations: Vec::new(),
            });
        },
        None => quote!(),
    };
    Ok(quote! {
        impl ::odatabase::edm::EdmEntity for #ident {
            fn edm_name() -> &'static str {
                #type_name
            }

            fn register(b: &mut ::odatabase::edm::ModelBuilder) {
                let mut t = ::odatabase::edm::EntityType {
                    name: #type_name.to_owned(),
                    table: #table,
                    key: vec![#(#keys),*],
                    base_type: #base,
                    is_abstract: #is_abstract,
                    inheritance: #inheritance,
                    etag: #etag,
                    media: #media,
                    visible_for: vec![#(#visible.to_owned()),*],
                    annotations: vec![#(#annotations),*],
                    query_extension: #query_extension,
                    track_changes: #track_changes,
                    ..Default::default()
                };
                #(#body)*
                b.add_entity_type(t);
                #set
                #singleton
                #(#targets)*
            }
        }
    })
}
