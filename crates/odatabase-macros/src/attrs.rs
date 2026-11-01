//! Parsing of `#[odata(...)]` attributes.

use proc_macro2::TokenStream;
use quote::quote;
use syn::meta::ParseNestedMeta;
use syn::{Attribute, LitBool, LitInt, LitStr, Result, Type, parenthesized};

pub type Pairs = Vec<(String, String)>;

fn string(meta: &ParseNestedMeta) -> Result<String> {
    Ok(meta.value()?.parse::<LitStr>()?.value())
}

fn int(meta: &ParseNestedMeta) -> Result<u32> {
    meta.value()?.parse::<LitInt>()?.base10_parse()
}

/// Reads `name("a", "b")`.
fn strings(meta: &ParseNestedMeta) -> Result<Vec<String>> {
    let content;
    parenthesized!(content in meta.input);
    let list = content.parse_terminated(|p| p.parse::<LitStr>(), syn::Token![,])?;
    Ok(list.iter().map(LitStr::value).collect())
}

/// Reads `on(source = "target", ...)`. The left side may be a bare identifier.
fn pairs(meta: &ParseNestedMeta) -> Result<Pairs> {
    let mut out = Vec::new();
    meta.parse_nested_meta(|m| {
        let key = m
            .path
            .get_ident()
            .map(ToString::to_string)
            .unwrap_or_default();
        out.push((key, string(&m)?));
        Ok(())
    })?;
    Ok(out)
}

pub fn pairs_tokens(pairs: &Pairs) -> TokenStream {
    let items = pairs
        .iter()
        .map(|(s, t)| quote!(::odatabase::edm::ColumnPair::new(#s, #t)));
    quote!(vec![#(#items),*])
}

#[derive(Default)]
pub struct AnnotationAttr {
    pub term: String,
    pub value: Option<TokenStream>,
}

fn annotation(meta: &ParseNestedMeta) -> Result<TokenStream> {
    let mut a = AnnotationAttr::default();
    meta.parse_nested_meta(|m| {
        let v = quote!(::odatabase::edm::AnnotationValue);
        if m.path.is_ident("term") {
            a.term = string(&m)?;
        } else if m.path.is_ident("string") {
            let s = string(&m)?;
            a.value = Some(quote!(#v::String(#s.to_owned())));
        } else if m.path.is_ident("bool") {
            let b = m.value()?.parse::<LitBool>()?.value;
            a.value = Some(quote!(#v::Bool(#b)));
        } else if m.path.is_ident("int") {
            let i: i64 = m.value()?.parse::<LitInt>()?.base10_parse()?;
            a.value = Some(quote!(#v::Int(#i)));
        } else if m.path.is_ident("enum_member") {
            let s = string(&m)?;
            a.value = Some(quote!(#v::EnumMember(#s.to_owned())));
        } else if m.path.is_ident("path") {
            let s = string(&m)?;
            a.value = Some(quote!(#v::Path(#s.to_owned())));
        } else {
            return Err(m.error("expected term, string, bool, int, enum_member or path"));
        }
        Ok(())
    })?;
    let Some(value) = a.value else {
        return Err(meta.error("annotation needs a value"));
    };
    let term = a.term;
    Ok(quote!(::odatabase::edm::Annotation::new(#term, #value)))
}

/// Attributes of a struct.
#[derive(Default)]
pub struct TypeAttrs {
    pub name: Option<String>,
    pub table: Option<String>,
    pub set: Option<String>,
    pub no_set: bool,
    pub hidden_set: bool,
    pub singleton: Option<String>,
    pub is_abstract: bool,
    pub base: Option<Type>,
    pub discriminator: Option<(String, String)>,
    pub joined: Option<Pairs>,
    pub media: Option<(String, Option<String>, Option<String>)>,
    pub visible_for: Vec<String>,
    pub annotations: Vec<TokenStream>,
    pub query_extension: bool,
}

impl TypeAttrs {
    pub fn parse(attrs: &[Attribute]) -> Result<Self> {
        let mut t = Self::default();
        for attr in attrs.iter().filter(|a| a.path().is_ident("odata")) {
            attr.parse_nested_meta(|m| {
                if m.path.is_ident("name") {
                    t.name = Some(string(&m)?);
                } else if m.path.is_ident("table") {
                    t.table = Some(string(&m)?);
                } else if m.path.is_ident("set") {
                    t.set = Some(string(&m)?);
                } else if m.path.is_ident("no_set") {
                    t.no_set = true;
                } else if m.path.is_ident("hidden_set") {
                    t.hidden_set = true;
                } else if m.path.is_ident("singleton") {
                    t.singleton = Some(string(&m)?);
                } else if m.path.is_ident("abstract_type") {
                    t.is_abstract = true;
                } else if m.path.is_ident("base") {
                    t.base = Some(m.value()?.parse()?);
                } else if m.path.is_ident("discriminator") {
                    let (mut column, mut value) = (String::new(), String::new());
                    m.parse_nested_meta(|d| {
                        if d.path.is_ident("column") {
                            column = string(&d)?;
                        } else if d.path.is_ident("value") {
                            value = string(&d)?;
                        } else {
                            return Err(d.error("expected column or value"));
                        }
                        Ok(())
                    })?;
                    t.discriminator = Some((column, value));
                } else if m.path.is_ident("joined") {
                    t.joined = Some(pairs(&m)?);
                } else if m.path.is_ident("has_stream") {
                    let (mut column, mut ty, mut ty_col) = (String::new(), None, None);
                    m.parse_nested_meta(|d| {
                        if d.path.is_ident("column") {
                            column = string(&d)?;
                        } else if d.path.is_ident("content_type") {
                            ty = Some(string(&d)?);
                        } else if d.path.is_ident("content_type_column") {
                            ty_col = Some(string(&d)?);
                        } else {
                            return Err(
                                d.error("expected column, content_type or content_type_column")
                            );
                        }
                        Ok(())
                    })?;
                    t.media = Some((column, ty, ty_col));
                } else if m.path.is_ident("visible_for") {
                    t.visible_for = strings(&m)?;
                } else if m.path.is_ident("annotation") {
                    t.annotations.push(annotation(&m)?);
                } else if m.path.is_ident("query_extension") {
                    t.query_extension = true;
                } else {
                    return Err(m.error("unknown odata attribute on type"));
                }
                Ok(())
            })?;
        }
        Ok(t)
    }
}

pub enum NavKind {
    HasOne,
    HasMany,
    BelongsTo,
}

pub struct NavAttr {
    pub kind: NavKind,
    pub target: Option<Type>,
    pub on: Pairs,
    pub partner: Option<String>,
    pub required: bool,
    pub contains_target: bool,
    pub join_table: Option<(String, Pairs, Pairs)>,
}

fn nav(meta: &ParseNestedMeta, kind: NavKind) -> Result<NavAttr> {
    let mut n = NavAttr {
        kind,
        target: None,
        on: Vec::new(),
        partner: None,
        required: false,
        contains_target: false,
        join_table: None,
    };
    meta.parse_nested_meta(|m| {
        if m.path.is_ident("target") {
            n.target = Some(m.value()?.parse()?);
        } else if m.path.is_ident("on") {
            n.on = pairs(&m)?;
        } else if m.path.is_ident("partner") {
            n.partner = Some(string(&m)?);
        } else if m.path.is_ident("required") {
            n.required = true;
        } else if m.path.is_ident("contains_target") {
            n.contains_target = true;
        } else if m.path.is_ident("join_table") {
            let (mut table, mut source, mut target) = (String::new(), Vec::new(), Vec::new());
            m.parse_nested_meta(|j| {
                if j.path.is_ident("table") {
                    table = string(&j)?;
                } else if j.path.is_ident("source") {
                    source = pairs(&j)?;
                } else if j.path.is_ident("target") {
                    target = pairs(&j)?;
                } else {
                    return Err(j.error("expected table, source or target"));
                }
                Ok(())
            })?;
            n.join_table = Some((table, source, target));
        } else {
            return Err(m.error("unknown navigation option"));
        }
        Ok(())
    })?;
    if n.target.is_none() {
        return Err(meta.error("navigation needs target = Type"));
    }
    Ok(n)
}

pub struct CollectionAttr {
    pub table: String,
    pub owner: Pairs,
    pub value_column: Option<String>,
    pub order_column: Option<String>,
}

pub struct DescriptionAttr {
    pub table: String,
    pub join: Pairs,
    pub language_column: String,
    pub text_column: String,
}

/// Attributes of a field.
#[derive(Default)]
pub struct FieldAttrs {
    pub key: bool,
    pub etag: bool,
    pub ignore: bool,
    pub embedded: bool,
    pub computed: bool,
    pub searchable: bool,
    pub name: Option<String>,
    pub column: Option<String>,
    pub prefix: Option<String>,
    pub max_length: Option<u32>,
    pub precision: Option<u32>,
    pub scale: Option<u32>,
    pub srid: Option<u32>,
    pub transient: Option<Vec<String>>,
    pub collection: Option<CollectionAttr>,
    pub description: Option<DescriptionAttr>,
    pub protections: Vec<(String, bool)>,
    pub visible_for: Vec<String>,
    pub annotations: Vec<TokenStream>,
    pub nav: Option<NavAttr>,
}

impl FieldAttrs {
    pub fn parse(attrs: &[Attribute]) -> Result<Self> {
        let mut f = Self::default();
        for attr in attrs.iter().filter(|a| a.path().is_ident("odata")) {
            attr.parse_nested_meta(|m| f.parse_one(&m))?;
        }
        Ok(f)
    }

    fn parse_one(&mut self, m: &ParseNestedMeta) -> Result<()> {
        let f = self;
        if m.path.is_ident("key") {
            f.key = true;
        } else if m.path.is_ident("etag") {
            f.etag = true;
        } else if m.path.is_ident("ignore") {
            f.ignore = true;
        } else if m.path.is_ident("embedded") {
            f.embedded = true;
            if m.input.peek(syn::token::Paren) {
                m.parse_nested_meta(|e| {
                    if e.path.is_ident("prefix") {
                        f.prefix = Some(string(&e)?);
                        Ok(())
                    } else {
                        Err(e.error("expected prefix"))
                    }
                })?;
            }
        } else if m.path.is_ident("computed") {
            f.computed = true;
        } else if m.path.is_ident("searchable") {
            f.searchable = true;
        } else if m.path.is_ident("name") {
            f.name = Some(string(m)?);
        } else if m.path.is_ident("column") {
            f.column = Some(string(m)?);
        } else if m.path.is_ident("max_length") {
            f.max_length = Some(int(m)?);
        } else if m.path.is_ident("precision") {
            f.precision = Some(int(m)?);
        } else if m.path.is_ident("scale") {
            f.scale = Some(int(m)?);
        } else if m.path.is_ident("srid") {
            f.srid = Some(int(m)?);
        } else if m.path.is_ident("transient") {
            let mut requires = Vec::new();
            if m.input.peek(syn::token::Paren) {
                m.parse_nested_meta(|t| {
                    if t.path.is_ident("requires") {
                        requires = strings(&t)?;
                        Ok(())
                    } else {
                        Err(t.error("expected requires"))
                    }
                })?;
            }
            f.transient = Some(requires);
        } else if m.path.is_ident("collection") {
            let mut c = CollectionAttr {
                table: String::new(),
                owner: Vec::new(),
                value_column: None,
                order_column: None,
            };
            m.parse_nested_meta(|e| {
                if e.path.is_ident("table") {
                    c.table = string(&e)?;
                } else if e.path.is_ident("owner") {
                    c.owner = pairs(&e)?;
                } else if e.path.is_ident("value_column") {
                    c.value_column = Some(string(&e)?);
                } else if e.path.is_ident("order_column") {
                    c.order_column = Some(string(&e)?);
                } else {
                    return Err(e.error("expected table, owner, value_column or order_column"));
                }
                Ok(())
            })?;
            f.collection = Some(c);
        } else if m.path.is_ident("description") {
            let mut d = DescriptionAttr {
                table: String::new(),
                join: Vec::new(),
                language_column: "language".to_owned(),
                text_column: "text".to_owned(),
            };
            m.parse_nested_meta(|e| {
                if e.path.is_ident("table") {
                    d.table = string(&e)?;
                } else if e.path.is_ident("on") {
                    d.join = pairs(&e)?;
                } else if e.path.is_ident("language_column") {
                    d.language_column = string(&e)?;
                } else if e.path.is_ident("text_column") {
                    d.text_column = string(&e)?;
                } else {
                    return Err(e.error("expected table, on, language_column or text_column"));
                }
                Ok(())
            })?;
            f.description = Some(d);
        } else if m.path.is_ident("protected") {
            let (mut claim, mut wildcards) = (String::new(), false);
            m.parse_nested_meta(|e| {
                if e.path.is_ident("claim") {
                    claim = string(&e)?;
                } else if e.path.is_ident("wildcards") {
                    wildcards = true;
                } else {
                    return Err(e.error("expected claim or wildcards"));
                }
                Ok(())
            })?;
            f.protections.push((claim, wildcards));
        } else if m.path.is_ident("visible_for") {
            f.visible_for = strings(m)?;
        } else if m.path.is_ident("annotation") {
            f.annotations.push(annotation(m)?);
        } else if m.path.is_ident("has_many") {
            f.nav = Some(nav(m, NavKind::HasMany)?);
        } else if m.path.is_ident("has_one") {
            f.nav = Some(nav(m, NavKind::HasOne)?);
        } else if m.path.is_ident("belongs_to") {
            f.nav = Some(nav(m, NavKind::BelongsTo)?);
        } else {
            return Err(m.error("unknown odata attribute on field"));
        }
        Ok(())
    }
}
