use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    path::Path,
};

use heck::{ToKebabCase, ToPascalCase, ToSnakeCase};
use syn::{Fields, GenericArgument, Item, PathArguments, Type, Visibility};

use crate::wit::utils::{RustFilesFinder, is_valid_helper_enum, is_valid_helper_struct};

use super::{MappingMode, bedrock_type_info, convert_value};

pub(super) struct BedrockValues<'a> {
    types: HashMap<String, (String, Item)>,
    projections: RefCell<BTreeMap<String, String>>,
    legacy: Option<&'a HashMap<&'a str, Vec<&'a str>>>,
}

impl<'a> BedrockValues<'a> {
    pub(super) fn load(legacy: Option<&'a HashMap<&'a str, Vec<&'a str>>>) -> Self {
        let root = Path::new("../../crates/pumpkin-protocol/src/bedrock");
        let paths = [
            root.join("server"),
            root.join("client"),
            root.join("network_item.rs"),
        ];
        let mut types = HashMap::new();
        for path in paths.iter().flat_map(|path| RustFilesFinder::new(path)) {
            let relative = path.strip_prefix(root).unwrap();
            let relative = if relative.file_name().is_some_and(|name| name == "mod.rs") {
                relative.parent().unwrap().to_owned()
            } else {
                relative.with_extension("")
            };
            let module = format!(
                "pumpkin_protocol::bedrock::{}",
                relative.to_string_lossy().replace('/', "::")
            );
            let file = syn::parse_file(&fs::read_to_string(&path).unwrap()).unwrap();
            for item in file.items {
                let name = match &item {
                    Item::Enum(enumeration) if is_valid_helper_enum(enumeration) => {
                        &enumeration.ident
                    }
                    Item::Struct(record) if is_valid_helper_struct(record) => &record.ident,
                    Item::Struct(record)
                        if matches!(record.vis, Visibility::Public(_))
                            && matches!(record.fields, Fields::Unit)
                            && record.generics.params.is_empty() =>
                    {
                        &record.ident
                    }
                    _ => continue,
                };
                types.insert(name.to_string(), (module.clone(), item));
            }
        }
        Self {
            types,
            projections: RefCell::default(),
            legacy,
        }
    }

    pub(super) fn contains_member(&self, ty: &str, member: &str) -> bool {
        let Some(members) = self
            .legacy
            .and_then(|legacy| legacy.get(ty.to_kebab_case().as_str()))
        else {
            return true;
        };
        let name = member.to_kebab_case();
        members.iter().skip(1).any(|line| {
            line.trim_start_matches('%').split([':', '(', ',']).next() == Some(name.as_str())
        })
    }

    pub(super) fn project(&self, ty: &Type, source: &str) -> Option<String> {
        self.project_value(ty, source, &mut HashSet::new())
    }

    pub(super) fn append_used_projections(&self, output: &mut String) {
        let projections = self.projections.borrow();
        let mut used = BTreeSet::new();
        let mut fragments = vec![output.as_str()];
        while let Some(fragment) = fragments.pop() {
            for (name, definition) in projections.iter() {
                if !used.contains(name) && fragment.contains(&format!("{name}(")) {
                    used.insert(name);
                    fragments.push(definition);
                }
            }
        }
        for name in used {
            output.push_str(&projections[name]);
        }
    }

    fn project_value(
        &self,
        ty: &Type,
        source: &str,
        visiting: &mut HashSet<String>,
    ) -> Option<String> {
        let (leaf, is_ref, is_slice) = bedrock_type_info(ty);
        let (prep, value, supported) = convert_value(
            &leaf,
            is_ref,
            is_slice,
            source,
            None,
            MappingMode::Deserialize,
            "",
        );
        if supported && prep.is_empty() {
            return Some(value);
        }
        if let Type::Array(array) = ty {
            let value = self.project_value(&array.elem, "element", visiting)?;
            return Some(format!(
                "{source}.into_iter().map(|element| Some({value})).collect::<Option<Vec<_>>>()?"
            ));
        }
        let Type::Path(path) = ty else {
            return None;
        };
        let segment = path.path.segments.last()?;
        let name = segment.ident.to_string();

        if name == "Bitset" {
            return Some(format!(
                "vec![{source}.bits as i64, ({source}.bits >> 64) as i64]"
            ));
        }

        if name == "SyncedActorDataList" {
            return self.project_value(
                &syn::parse_quote!(HashMap<u32, MetadataValue>),
                &format!("{source}.0"),
                visiting,
            );
        }

        if name == "HashMap"
            && let PathArguments::AngleBracketed(params) = &segment.arguments
            && let (Some(GenericArgument::Type(key)), Some(GenericArgument::Type(item))) =
                (params.args.first(), params.args.iter().nth(1))
        {
            let key = self.project_value(key, "key", visiting)?;
            let item = self.project_value(item, "item", visiting)?;
            return Some(format!(
                "{{ let mut entries = {source}.into_iter().collect::<Vec<_>>();\nentries.sort_unstable_by(|a, b| a.0.cmp(&b.0));\nentries.into_iter().map(|(key, item)| Some(({key}, {item}))).collect::<Option<Vec<_>>>()? }}"
            ));
        }

        if let PathArguments::AngleBracketed(params) = &segment.arguments
            && let Some(GenericArgument::Type(inner)) = params.args.first()
        {
            return match name.as_str() {
                "Option" => {
                    let value = self.project_value(inner, "element", visiting)?;
                    if value.contains('?') {
                        Some(format!(
                            "{source}.map_or(Some(None), |element| Some(Some({value})))?"
                        ))
                    } else {
                        Some(format!("{source}.map(|element| {value})"))
                    }
                }
                "Vec" => {
                    let value = self.project_value(inner, "element", visiting)?;
                    Some(format!(
                        "{source}.into_iter().map(|element| Some({value})).collect::<Option<Vec<_>>>()?"
                    ))
                }
                "Box" => {
                    if let Type::Slice(slice) = inner {
                        let value = self.project_value(&slice.elem, "element", visiting)?;
                        Some(format!(
                            "{source}.into_vec().into_iter().map(|element| Some({value})).collect::<Option<Vec<_>>>()?"
                        ))
                    } else {
                        let value = self.project_value(inner, "unboxed", visiting)?;
                        Some(format!("{{ let unboxed = *{source}; {value} }}"))
                    }
                }
                "NonZero" => self.project_value(inner, &format!("{source}.get()"), visiting),
                "EnumAsStr" => {
                    self.project_value(inner, &format!("{source}.into_inner()"), visiting)
                }
                _ => None,
            };
        }

        if matches!(name.as_str(), "Nbt" | "NbtCompound") {
            // WIT's NBT tree describes the tags below the document name.
            let compound = if name == "Nbt" {
                format!("{source}.root_tag")
            } else {
                source.to_string()
            };
            return Some(format!(
                "crate::common::to_wit_nbt_tree(pumpkin_nbt::tag::NbtTag::Compound({compound}))"
            ));
        }

        self.project_named_value(&name, &name, source, visiting)
    }

    fn project_named_value(
        &self,
        rust_name: &str,
        wit_name: &str,
        source: &str,
        visiting: &mut HashSet<String>,
    ) -> Option<String> {
        let (rust_module, ty) = self.types.get(rust_name)?;
        let function = format!("project_bedrock_{}", wit_name.to_snake_case());
        let call = format!("{function}({source})?");
        if self.projections.borrow().contains_key(&function) {
            return Some(call);
        }
        if !visiting.insert(rust_name.to_string()) {
            return None;
        }
        let wit_name = wit_identifier(wit_name);
        let (generics, wit_type, result) = match ty {
            Item::Struct(record) => {
                let wit_type = if matches!(record.fields, Fields::Unit) {
                    "crate::pumpkin::plugin::common::InexpressibleHostType".to_string()
                } else {
                    format!("crate::pumpkin::plugin::bedrock_packets::{wit_name}")
                };
                (
                    &record.generics,
                    wit_type,
                    self.project_record(record, "value", &wit_name, visiting),
                )
            }
            Item::Enum(enumeration) => (
                &enumeration.generics,
                format!("crate::pumpkin::plugin::bedrock_packets::{wit_name}"),
                self.project_enum(enumeration, rust_module, "value", &wit_name, visiting),
            ),
            _ => unreachable!(),
        };
        visiting.remove(rust_name);
        let result = result?;
        let lifetimes = match generics.lifetimes().count() {
            0 => "",
            1 => "<'_>",
            _ => return None,
        };
        // These functions share one code path for runtime values, including owned collections.
        let definition = format!(
            "#[allow(clippy::missing_const_for_fn)]\nfn {function}(value: {rust_module}::{rust_name}{lifetimes}) -> Option<{wit_type}> {{\n    Some({result})\n}}\n\n"
        );
        self.projections.borrow_mut().insert(function, definition);
        Some(call)
    }

    fn project_record(
        &self,
        record: &syn::ItemStruct,
        source: &str,
        wit_name: &str,
        visiting: &mut HashSet<String>,
    ) -> Option<String> {
        if matches!(record.fields, Fields::Unit) {
            // Existing WIT schemas use this sentinel for a unit struct wrapped by a variant.
            return Some(format!(
                "{{ let _ = {source}; crate::pumpkin::plugin::common::InexpressibleHostType::Setinel }}"
            ));
        }
        let Fields::Named(fields) = &record.fields else {
            return None;
        };
        let mut fields_out = String::new();
        for field in &fields.named {
            let ident = field.ident.as_ref()?;
            if !self.contains_member(wit_name, &ident.to_string()) {
                continue;
            }
            if !matches!(field.vis, Visibility::Public(_)) {
                return None;
            }
            let source = format!("record_value.{ident}");
            let value = if self.legacy.is_some()
                && record.ident == "PlayerInventoryAction"
                && ident == "transaction"
            {
                self.project_named_value(
                    "UseItemTransactionData",
                    "PlayerUseItemTransactionData",
                    &source,
                    visiting,
                )?
            } else {
                self.project_value(&field.ty, &source, visiting)?
            };
            fields_out.push_str(&format!("{}: {value},\n", wit_field(ident)));
        }
        Some(format!(
            "{{\nlet record_value = {source};\ncrate::pumpkin::plugin::bedrock_packets::{wit_name} {{\n{fields_out}}}\n}}"
        ))
    }

    fn project_enum(
        &self,
        enumeration: &syn::ItemEnum,
        rust_module: &str,
        source: &str,
        wit_name: &str,
        visiting: &mut HashSet<String>,
    ) -> Option<String> {
        let rust_name = &enumeration.ident;
        let wit_path = "crate::pumpkin::plugin::bedrock_packets";
        let mut arms = String::new();
        for variant in &enumeration.variants {
            let rust_case = &variant.ident;
            let case = wit_identifier(&rust_case.to_string());
            let from = format!("{rust_module}::{rust_name}::{rust_case}");
            let to = format!("{wit_path}::{wit_name}::{case}");
            if self.legacy.is_some()
                && rust_name == "TransactionData"
                && matches!(case.as_str(), "Normal" | "Mismatch")
            {
                arms.push_str(&format!("{from}(_) => {to}(String::new()),\n"));
                continue;
            }
            if !self.contains_member(&rust_name.to_string(), &rust_case.to_string()) {
                let fields = match variant.fields {
                    Fields::Unit => "",
                    Fields::Named(_) => " { .. }",
                    Fields::Unnamed(_) => "(..)",
                };
                arms.push_str(&format!("{from}{fields} => return None,\n"));
                continue;
            }
            match &variant.fields {
                Fields::Unit => arms.push_str(&format!("{from} => {to},\n")),
                Fields::Named(fields) => {
                    let mut bindings = String::new();
                    let mut values = String::new();
                    for field in &fields.named {
                        let ident = field.ident.as_ref()?;
                        let value = self.project_value(&field.ty, &ident.to_string(), visiting)?;
                        bindings.push_str(&format!("{ident},"));
                        values.push_str(&format!("{}: {value},\n", wit_field(ident)));
                    }
                    arms.push_str(&format!("{from} {{ {bindings} }} => {to}({wit_path}::{wit_name}{case} {{\n{values}}}),\n"));
                }
                Fields::Unnamed(fields) => {
                    let mut bindings = Vec::new();
                    let mut values = Vec::new();
                    for (index, field) in fields.unnamed.iter().enumerate() {
                        let binding = format!("element_{index}");
                        values.push(self.project_value(&field.ty, &binding, visiting)?);
                        bindings.push(binding);
                    }
                    let value = if values.len() == 1 {
                        values.pop()?
                    } else {
                        format!("({})", values.join(","))
                    };
                    arms.push_str(&format!(
                        "{from}({}) => {to}({value}),\n",
                        bindings.join(",")
                    ));
                }
            }
        }
        Some(format!("match {source} {{\n{arms}}}"))
    }
}

fn wit_identifier(name: &str) -> String {
    name.to_kebab_case().to_pascal_case()
}

fn wit_field(ident: &syn::Ident) -> String {
    ident.to_string().to_snake_case()
}
