use heck::ToShoutySnakeCase;
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use serde::Deserialize;
use std::{collections::BTreeSet, fs};

#[derive(Deserialize)]
struct PacketIds {
    protocol: u32,
    packets: Vec<PacketId>,
}

#[derive(Deserialize)]
struct PacketId {
    id: i32,
    name: String,
}

pub(crate) fn build() -> TokenStream {
    let source = fs::read_to_string("../../assets/bedrock/packets.json")
        .expect("Failed to read Bedrock packet IDs");
    let mut data: PacketIds =
        serde_json::from_str(&source).expect("Failed to parse Bedrock packet IDs");
    data.packets.sort_by_key(|packet| packet.id);

    let mut ids = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut constants = Vec::new();
    let mut name_arms = Vec::new();
    for packet in data.packets {
        let id = packet.id;
        let name = packet.name;
        assert!((0..=0x3ff).contains(&id), "Invalid Bedrock packet ID: {id}");
        assert!(ids.insert(id), "Duplicate Bedrock packet ID: {id}");
        let ident = name.to_shouty_snake_case();
        assert!(
            names.insert(ident.clone()),
            "Duplicate Bedrock packet: {name}"
        );
        let ident = format_ident!("{ident}");
        constants.push(quote! { pub const #ident: i32 = #id; });
        name_arms.push(quote! { #id => Some(#name), });
    }
    let protocol = data.protocol;

    quote! {
        /// Protocol version of the Bedrock packet ID catalogue.
        pub const PROTOCOL_VERSION: u32 = #protocol;

        #(#constants)*

        /// Look up a known protocol ID, including retained legacy packets.
        /// A known ID does not imply a gameplay handler or a typed codec exists.
        #[must_use]
        pub const fn packet_name(id: i32) -> Option<&'static str> {
            match id {
                #(#name_arms)*
                _ => None,
            }
        }
    }
}
