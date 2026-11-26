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
    for bad in [
        "1e",
        "1e+",
        "-01",
        ".5",
        r#""\ud83dA""#,
        r#""\udc00""#,
        r#""\u00""#,
    ] {
        assert!(parse(bad).is_err(), "{bad}");
    }
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
