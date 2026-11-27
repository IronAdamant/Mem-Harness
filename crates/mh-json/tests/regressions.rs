use mh_json::{parse, Value};

#[test]
fn edges() {
    let mut v = parse(r#"{"a":1,"b":2,"a":3,"a":{"x":1}}"#).unwrap();
    assert_eq!(v.to_json(), r#"{"a":{"x":1},"b":2}"#);
    v.remove("a");
    assert!(v.get("a").is_none());
    v.set("a", Value::int(5));
    assert_eq!(v.to_json(), r#"{"b":2,"a":5}"#);
    assert!(parse(&("[".repeat(256) + &"]".repeat(256))).is_ok());
    assert!(parse(&("[".repeat(258) + &"]".repeat(258))).is_err());
    assert_eq!(parse("-0").unwrap(), Value::Num("-0".into()));
    for bad in ["1e", "1e+", "-01", ".5", r#""\u00""#] {
        assert!(parse(bad).is_err(), "{bad}");
    }
    // Lone surrogates become U+FFFD instead of failing the whole text.
    assert_eq!(parse(r#""\ud83dA""#).unwrap(), Value::str("\u{fffd}A"));
    assert_eq!(parse(r#""\udc00""#).unwrap(), Value::str("\u{fffd}"));
    let k = parse(r#"{"\u0000":"\u001f"}"#).unwrap();
    assert_eq!(parse(&k.to_json()).unwrap(), k);
    let big = format!("[{}]", vec!["{\"k\":1}"; 200_000].join(","));
    let t = std::time::Instant::now();
    assert!(parse(&big).is_ok());
    assert!(t.elapsed().as_secs() < 5);
    let keys: Vec<String> = (0..100_000).map(|i| format!("\"k{i}\":{i}")).collect();
    let t = std::time::Instant::now();
    assert!(parse(&format!("{{{}}}", keys.join(","))).is_ok());
    assert!(t.elapsed().as_secs() < 5);
}

/// API hazard only: a hand-built object with a repeated key has `get`
/// reading the last entry while `set` writes the first.
#[test]
fn obj_with_repeated_key_get_and_set_agree() {
    let mut o = Value::obj(vec![("a", Value::int(1)), ("a", Value::int(2))]);
    o.set("a", Value::int(9));
    assert_eq!(o.get("a"), Some(&Value::int(9)));
}

/// `peek` sees the id and whether a `"method"` key is present, wherever
/// it sits, past values that do not parse.
#[test]
fn peek_tells_requests_from_replies() {
    let req = r#"{"jsonrpc":"2.0","id":2,"method":"roots/list","params":{"x":"\q"}}"#;
    let p = mh_json::peek(req);
    assert_eq!(p.id, Some(mh_json::Value::Num("2".into())));
    assert!(p.method);
    let late = r#"{"params":{"x":"\q"},"id":"a","method":"m"}"#;
    assert!(mh_json::peek(late).method);
    let reply = r#"{"jsonrpc":"2.0","id":2,"result":{"x":"\q"}}"#;
    let p = mh_json::peek(reply);
    assert_eq!(p.id, Some(mh_json::Value::Num("2".into())));
    assert!(!p.method);
    // A "method" inside a value is not the message's own.
    assert!(!mh_json::peek(r#"{"id":1,"result":{"method":"x","y":"\q"}}"#).method);
    assert_eq!(mh_json::peek("not json"), mh_json::Peek::default());
}
