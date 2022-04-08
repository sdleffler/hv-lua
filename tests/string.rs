use std::borrow::Cow;

use hv::lua::{Lua, Result, String};

#[test]
fn test_string_compare() {
    fn with_str<F: FnOnce(String)>(s: &str, f: F) {
        f(Lua::new().create_string(s).unwrap());
    }

    // Tests that all comparisons we want to have are usable
    with_str("teststring", |t| assert_eq!(t, "teststring")); // &str
    with_str("teststring", |t| assert_eq!(t, b"teststring")); // &[u8]
    with_str("teststring", |t| assert_eq!(t, b"teststring".to_vec())); // Vec<u8>
    with_str("teststring", |t| assert_eq!(t, "teststring".to_string())); // String
    with_str("teststring", |t| assert_eq!(t, t)); // mlua::String
    with_str("teststring", |t| {
        assert_eq!(t, Cow::from(b"teststring".as_ref()))
    }); // Cow (borrowed)
    with_str("bla", |t| assert_eq!(t, Cow::from(b"bla".to_vec()))); // Cow (owned)
}

#[test]
fn test_string_views() -> Result<()> {
    let lua = Lua::new();

    lua.load(
        r#"
        ok = "null bytes are valid utf-8, wh\0 knew?"
        err = "but \255 isn't :("
        empty = ""
    "#,
    )
    .exec()?;

    let globals = lua.globals();
    let ok: String = globals.get("ok")?;
    let err: String = globals.get("err")?;
    let empty: String = globals.get("empty")?;

    assert_eq!(ok.to_str()?, "null bytes are valid utf-8, wh\0 knew?");
    assert_eq!(
        ok.to_string_lossy(),
        "null bytes are valid utf-8, wh\0 knew?"
    );
    assert_eq!(
        ok.as_bytes(),
        &b"null bytes are valid utf-8, wh\0 knew?"[..]
    );

    assert!(err.to_str().is_err());
    assert_eq!(err.as_bytes(), &b"but \xff isn't :("[..]);

    assert_eq!(empty.to_str()?, "");
    assert_eq!(empty.as_bytes_with_nul(), &[0]);
    assert!(empty.as_bytes().is_empty());

    Ok(())
}

#[test]
fn test_raw_string() -> Result<()> {
    let lua = Lua::new();

    let rs = lua.create_string(&[0, 1, 2, 3, 0, 1, 2, 3])?;
    assert_eq!(rs.as_bytes(), &[0, 1, 2, 3, 0, 1, 2, 3]);

    Ok(())
}
