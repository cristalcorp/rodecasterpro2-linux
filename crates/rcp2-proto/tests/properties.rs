//! Property tests: the decoder accepts everything the encoder writes, and
//! never panics on arbitrary bytes (they come from a USB device).

use proptest::prelude::*;
use rcp2_proto::{Node, Var, classify, decode_change, decode_tree, encode_tree};

fn var() -> impl Strategy<Value = Var> {
    prop_oneof![
        any::<i32>().prop_map(Var::Int),
        any::<bool>().prop_map(Var::Bool),
        // NaN != NaN would break equality, not the decoder: keep finite values.
        (-1e12f64..1e12).prop_map(Var::Double),
        "[^\0]{0,24}".prop_map(Var::String),
        any::<i64>().prop_map(Var::Int64),
        proptest::collection::vec(any::<u8>(), 0..32).prop_map(Var::Binary),
    ]
}

fn node() -> impl Strategy<Value = Node> {
    let leaf = (
        "[A-Za-z]{1,12}",
        proptest::collection::vec(("[a-zA-Z]{1,16}", var()), 0..6),
    )
        .prop_map(|(name, properties)| Node {
            name,
            properties,
            children: vec![],
        });
    leaf.prop_recursive(4, 64, 8, |inner| {
        (
            "[A-Z]{1,12}",
            proptest::collection::vec(("[a-zA-Z]{1,16}", var()), 0..6),
            proptest::collection::vec(inner, 0..8),
        )
            .prop_map(|(name, properties, children)| Node {
                name,
                properties,
                children,
            })
    })
}

proptest! {
    #[test]
    fn every_encoded_tree_decodes_to_itself(tree in node()) {
        prop_assert_eq!(decode_tree(&encode_tree(&tree)).unwrap(), tree);
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = decode_tree(&bytes);
        let _ = decode_change(&bytes);
        let _ = classify(&bytes);
    }

    #[test]
    fn corrupted_trees_never_panic(tree in node(), index in any::<prop::sample::Index>(), byte in any::<u8>()) {
        let mut bytes = encode_tree(&tree);
        if !bytes.is_empty() {
            let at = index.index(bytes.len());
            bytes[at] = byte;
        }
        let _ = decode_tree(&bytes);
    }
}
