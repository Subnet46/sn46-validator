use serde::Serialize;

/// Compact JSON with keys in byte order regardless of serde_json feature unification. JSON is UTF-8,
/// so this is a `String`; callers that sign or compare bytes take `as_bytes`.
pub fn canonical_json<T: Serialize + ?Sized>(value: &T) -> String {
    let mut value = serde_json::to_value(value).expect("value serializes");
    value.sort_all_objects();
    serde_json::to_string(&value).expect("value serializes")
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn canonical_json_matches_expected_bytes() {
        // (value, expected bytes as hex)
        let cases = [
            (
                json!({"z": "é漢😀", "a": "\u{2028}\u{2029}"}),
                "7b2261223a22e280a8e280a9222c227a223a22c3a9e6bca2f09f9880227d",
            ),
            (
                json!({"c": "\u{0}\u{1}\u{1f}\u{7f}\"\\/\u{8}\u{c}\n\r\t"}),
                "7b2263223a225c75303030305c75303030315c75303031667f5c225c5c2f5c625c665c6e5c725c74227d",
            ),
            (
                json!({"b": 1, "a": 2, "B": 3, "aa": 4, "a0": 5, "_": 6, "é": 7, "z": 8}),
                "7b2242223a332c225f223a362c2261223a322c226130223a352c226161223a342c2262223a312c227a223a382c22c3a9223a377d",
            ),
            (
                json!({"x": [[], {}, [1, [2, [3]]], {"k": {"kk": null}}], "t": true, "f": false, "n": null}),
                "7b2266223a66616c73652c226e223a6e756c6c2c2274223a747275652c2278223a5b5b5d2c7b7d2c5b312c5b322c5b335d5d5d2c7b226b223a7b226b6b223a6e756c6c7d7d5d7d",
            ),
            (
                json!({"max": 9007199254740991i64, "min": -9007199254740991i64, "zero": 0}),
                "7b226d6178223a393030373139393235343734303939312c226d696e223a2d393030373139393235343734303939312c227a65726f223a307d",
            ),
            (json!({}), "7b7d"),
            (
                json!([1, "a", null, {"b": []}]),
                "5b312c2261222c6e756c6c2c7b2262223a5b5d7d5d",
            ),
            (json!("plain"), "22706c61696e22"),
        ];
        for (value, expected) in cases {
            let expected = hex::decode(expected).unwrap();
            assert_eq!(canonical_json(&value).as_bytes(), expected);
            assert_eq!(serde_json::from_slice::<Value>(&expected).unwrap(), value);
        }
    }

    #[test]
    fn integers_beyond_u64_serialize_as_plain_digits() {
        // Scoring components are u128; arbitrary_precision keeps them as plain digits.
        let value = serde_json::to_value(2049638230412172401666u128).unwrap();
        assert_eq!(canonical_json(&value), "2049638230412172401666");
        assert_eq!(canonical_json(&json!(u64::MAX)), "18446744073709551615");
    }
}
