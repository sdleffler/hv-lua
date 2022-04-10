use std::{any::TypeId, mem::MaybeUninit};
use std::{
    cell::{Ref, RefCell, RefMut},
    marker::PhantomData,
};
use std::{fmt, sync::Arc};
use std::{
    hash::{Hash, Hasher},
    sync::RwLock,
};
use std::{string::String as StdString, sync::Mutex};

#[cfg(not(feature = "send"))]
use std::rc::Rc;

#[cfg(feature = "async")]
use std::future::Future;

#[cfg(feature = "serialize")]
use {
    serde::ser::{self, Serialize, Serializer},
    std::result::Result as StdResult,
};

use hv_alchemy::{AlchemicalAny, AlchemicalPtr, Alchemy, IntoProxy, Type, TypeTable};
use hv_guarded_borrow::{NonBlockingGuardedBorrow, NonBlockingGuardedMutBorrowMut};

use crate::types::{Callback, LuaRef, MaybeSend};
use crate::util::{check_stack, get_userdata, take_userdata, StackGuard};
use crate::value::{FromLua, FromLuaMulti, ToLua, ToLuaMulti};
use crate::{
    error::{Error, Result},
    types::DestructedUserdataMT,
};
use crate::{ffi, RegistryKey};
use crate::{function::Function, types::MaybeSync};
use crate::{hv::alchemy::MetaType, lua::Lua};
use crate::{
    table::{Table, TablePairsIter},
    Value,
};

#[cfg(feature = "lua54")]
use std::os::raw::c_int;

#[cfg(feature = "async")]
use crate::types::AsyncCallback;

mod collections;

#[cfg(feature = "lua54")]
pub(crate) const USER_VALUE_MAXSLOT: usize = 8;

/// Kinds of metamethods that can be overridden.
///
/// Currently, this mechanism does not allow overriding the `__gc` metamethod, since there is
/// generally no need to do so: [`UserData`] implementors can instead just implement `Drop`.
///
/// [`UserData`]: crate::UserData
#[derive(Debug, Clone)]
pub enum MetaMethod {
    /// The `+` operator.
    Add,
    /// The `-` operator.
    Sub,
    /// The `*` operator.
    Mul,
    /// The `/` operator.
    Div,
    /// The `%` operator.
    Mod,
    /// The `^` operator.
    Pow,
    /// The unary minus (`-`) operator.
    Unm,
    /// The floor division (//) operator.
    /// Requires `feature = "lua54/lua53"`
    #[cfg(any(feature = "lua54", feature = "lua53", doc))]
    IDiv,
    /// The bitwise AND (&) operator.
    /// Requires `feature = "lua54/lua53"`
    #[cfg(any(feature = "lua54", feature = "lua53", doc))]
    BAnd,
    /// The bitwise OR (|) operator.
    /// Requires `feature = "lua54/lua53"`
    #[cfg(any(feature = "lua54", feature = "lua53", doc))]
    BOr,
    /// The bitwise XOR (binary ~) operator.
    /// Requires `feature = "lua54/lua53"`
    #[cfg(any(feature = "lua54", feature = "lua53", doc))]
    BXor,
    /// The bitwise NOT (unary ~) operator.
    /// Requires `feature = "lua54/lua53"`
    #[cfg(any(feature = "lua54", feature = "lua53", doc))]
    BNot,
    /// The bitwise left shift (<<) operator.
    #[cfg(any(feature = "lua54", feature = "lua53", doc))]
    Shl,
    /// The bitwise right shift (>>) operator.
    #[cfg(any(feature = "lua54", feature = "lua53", doc))]
    Shr,
    /// The string concatenation operator `..`.
    Concat,
    /// The length operator `#`.
    Len,
    /// The `==` operator.
    Eq,
    /// The `<` operator.
    Lt,
    /// The `<=` operator.
    Le,
    /// Index access `obj[key]`.
    Index,
    /// Index write access `obj[key] = value`.
    NewIndex,
    /// The call "operator" `obj(arg1, args2, ...)`.
    Call,
    /// The `__tostring` metamethod.
    ///
    /// This is not an operator, but will be called by methods such as `tostring` and `print`.
    ToString,
    /// The `__pairs` metamethod.
    ///
    /// This is not an operator, but it will be called by the built-in `pairs` function.
    ///
    /// Requires `feature = "lua54/lua53/lua52"`
    #[cfg(any(
        feature = "lua54",
        feature = "lua53",
        feature = "lua52",
        feature = "luajit52",
        doc
    ))]
    Pairs,
    /// The `__ipairs` metamethod.
    ///
    /// This is not an operator, but it will be called by the built-in [`ipairs`] function.
    ///
    /// Requires `feature = "lua52"`
    ///
    /// [`ipairs`]: https://www.lua.org/manual/5.2/manual.html#pdf-ipairs
    #[cfg(any(feature = "lua52", feature = "luajit52", doc))]
    IPairs,
    /// The `__close` metamethod.
    ///
    /// Executed when a variable, that marked as to-be-closed, goes out of scope.
    ///
    /// More information about to-be-closed variabled can be found in the Lua 5.4
    /// [documentation][lua_doc].
    ///
    /// Requires `feature = "lua54"`
    ///
    /// [lua_doc]: https://www.lua.org/manual/5.4/manual.html#3.3.8
    #[cfg(any(feature = "lua54", doc))]
    Close,
    /// A custom metamethod.
    ///
    /// Must not be in the protected list: `__gc`, `__metatable`, `__mlua*`.
    Custom(StdString),
}

impl PartialEq for MetaMethod {
    fn eq(&self, other: &Self) -> bool {
        self.name() == other.name()
    }
}

impl Eq for MetaMethod {}

impl Hash for MetaMethod {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name().hash(state);
    }
}

impl fmt::Display for MetaMethod {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "{}", self.name())
    }
}

impl MetaMethod {
    /// Returns Lua metamethod name, usually prefixed by two underscores.
    pub fn name(&self) -> &str {
        match self {
            MetaMethod::Add => "__add",
            MetaMethod::Sub => "__sub",
            MetaMethod::Mul => "__mul",
            MetaMethod::Div => "__div",
            MetaMethod::Mod => "__mod",
            MetaMethod::Pow => "__pow",
            MetaMethod::Unm => "__unm",

            #[cfg(any(feature = "lua54", feature = "lua53"))]
            MetaMethod::IDiv => "__idiv",
            #[cfg(any(feature = "lua54", feature = "lua53"))]
            MetaMethod::BAnd => "__band",
            #[cfg(any(feature = "lua54", feature = "lua53"))]
            MetaMethod::BOr => "__bor",
            #[cfg(any(feature = "lua54", feature = "lua53"))]
            MetaMethod::BXor => "__bxor",
            #[cfg(any(feature = "lua54", feature = "lua53"))]
            MetaMethod::BNot => "__bnot",
            #[cfg(any(feature = "lua54", feature = "lua53"))]
            MetaMethod::Shl => "__shl",
            #[cfg(any(feature = "lua54", feature = "lua53"))]
            MetaMethod::Shr => "__shr",

            MetaMethod::Concat => "__concat",
            MetaMethod::Len => "__len",
            MetaMethod::Eq => "__eq",
            MetaMethod::Lt => "__lt",
            MetaMethod::Le => "__le",
            MetaMethod::Index => "__index",
            MetaMethod::NewIndex => "__newindex",
            MetaMethod::Call => "__call",
            MetaMethod::ToString => "__tostring",

            #[cfg(any(
                feature = "lua54",
                feature = "lua53",
                feature = "lua52",
                feature = "luajit52"
            ))]
            MetaMethod::Pairs => "__pairs",
            #[cfg(any(feature = "lua52", feature = "luajit52"))]
            MetaMethod::IPairs => "__ipairs",

            #[cfg(feature = "lua54")]
            MetaMethod::Close => "__close",

            MetaMethod::Custom(ref name) => name,
        }
    }

    pub(crate) fn validate(self) -> Result<Self> {
        match self {
            MetaMethod::Custom(name) if name == "__gc" => Err(Error::MetaMethodRestricted(name)),
            MetaMethod::Custom(name) if name == "__metatable" => {
                Err(Error::MetaMethodRestricted(name))
            }
            MetaMethod::Custom(name) if name.starts_with("__mlua") => {
                Err(Error::MetaMethodRestricted(name))
            }
            _ => Ok(self),
        }
    }
}

impl From<StdString> for MetaMethod {
    fn from(name: StdString) -> Self {
        match name.as_str() {
            "__add" => MetaMethod::Add,
            "__sub" => MetaMethod::Sub,
            "__mul" => MetaMethod::Mul,
            "__div" => MetaMethod::Div,
            "__mod" => MetaMethod::Mod,
            "__pow" => MetaMethod::Pow,
            "__unm" => MetaMethod::Unm,

            #[cfg(any(feature = "lua54", feature = "lua53"))]
            "__idiv" => MetaMethod::IDiv,
            #[cfg(any(feature = "lua54", feature = "lua53"))]
            "__band" => MetaMethod::BAnd,
            #[cfg(any(feature = "lua54", feature = "lua53"))]
            "__bor" => MetaMethod::BOr,
            #[cfg(any(feature = "lua54", feature = "lua53"))]
            "__bxor" => MetaMethod::BXor,
            #[cfg(any(feature = "lua54", feature = "lua53"))]
            "__bnot" => MetaMethod::BNot,
            #[cfg(any(feature = "lua54", feature = "lua53"))]
            "__shl" => MetaMethod::Shl,
            #[cfg(any(feature = "lua54", feature = "lua53"))]
            "__shr" => MetaMethod::Shr,

            "__concat" => MetaMethod::Concat,
            "__len" => MetaMethod::Len,
            "__eq" => MetaMethod::Eq,
            "__lt" => MetaMethod::Lt,
            "__le" => MetaMethod::Le,
            "__index" => MetaMethod::Index,
            "__newindex" => MetaMethod::NewIndex,
            "__call" => MetaMethod::Call,
            "__tostring" => MetaMethod::ToString,

            #[cfg(any(
                feature = "lua54",
                feature = "lua53",
                feature = "lua52",
                feature = "luajit52"
            ))]
            "__pairs" => MetaMethod::Pairs,
            #[cfg(any(feature = "lua52", feature = "luajit52"))]
            "__ipairs" => MetaMethod::IPairs,

            #[cfg(feature = "lua54")]
            "__close" => MetaMethod::Close,

            _ => MetaMethod::Custom(name),
        }
    }
}

impl From<&str> for MetaMethod {
    fn from(name: &str) -> Self {
        MetaMethod::from(name.to_owned())
    }
}

/// Method registry for [`UserData`] implementors.
///
/// [`UserData`]: crate::UserData
pub trait UserDataMethods<'lua, T: UserData> {
    /// Add a regular method which accepts a `&T` as the first parameter.
    ///
    /// Regular methods are implemented by overriding the `__index` metamethod and returning the
    /// accessed method. This allows them to be used with the expected `userdata:method()` syntax.
    ///
    /// If `add_meta_method` is used to set the `__index` metamethod, the `__index` metamethod will
    /// be used as a fall-back if no regular method is found.
    fn add_method<S, A, R, M>(&mut self, name: &S, method: M)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + Fn(&'lua Lua, &T, A) -> Result<R>;

    /// Add a regular method which accepts a `&mut T` as the first parameter.
    ///
    /// Refer to [`add_method`] for more information about the implementation.
    ///
    /// [`add_method`]: #method.add_method
    fn add_method_mut<S, A, R, M>(&mut self, name: &S, method: M)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + FnMut(&'lua Lua, &mut T, A) -> Result<R>;

    /// Add an async method which accepts a `T` as the first parameter and returns Future.
    /// The passed `T` is cloned from the original value.
    ///
    /// Refer to [`add_method`] for more information about the implementation.
    ///
    /// Requires `feature = "async"`
    ///
    /// [`add_method`]: #method.add_method
    #[cfg(feature = "async")]
    #[cfg_attr(docsrs, doc(cfg(feature = "async")))]
    fn add_async_method<S, A, R, M, MR>(&mut self, name: &S, method: M)
    where
        T: Clone,
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + Fn(&'lua Lua, T, A) -> MR,
        MR: 'lua + Future<Output = Result<R>>;

    /// Add a regular method as a function which accepts generic arguments, the first argument will
    /// be a [`AnyUserData`] of type `T` if the method is called with Lua method syntax:
    /// `my_userdata:my_method(arg1, arg2)`, or it is passed in as the first argument:
    /// `my_userdata.my_method(my_userdata, arg1, arg2)`.
    ///
    /// Prefer to use [`add_method`] or [`add_method_mut`] as they are easier to use.
    ///
    /// [`AnyUserData`]: crate::AnyUserData
    /// [`add_method`]: #method.add_method
    /// [`add_method_mut`]: #method.add_method_mut
    fn add_function<S, A, R, F>(&mut self, name: &S, function: F)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + Fn(&'lua Lua, A) -> Result<R>;

    /// Add a regular method as a mutable function which accepts generic arguments.
    ///
    /// This is a version of [`add_function`] that accepts a FnMut argument.
    ///
    /// [`add_function`]: #method.add_function
    fn add_function_mut<S, A, R, F>(&mut self, name: &S, function: F)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + FnMut(&'lua Lua, A) -> Result<R>;

    /// Add a regular method as an async function which accepts generic arguments
    /// and returns Future.
    ///
    /// This is an async version of [`add_function`].
    ///
    /// Requires `feature = "async"`
    ///
    /// [`add_function`]: #method.add_function
    #[cfg(feature = "async")]
    #[cfg_attr(docsrs, doc(cfg(feature = "async")))]
    fn add_async_function<S, A, R, F, FR>(&mut self, name: &S, function: F)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + Fn(&'lua Lua, A) -> FR,
        FR: 'lua + Future<Output = Result<R>>;

    /// Add a metamethod which accepts a `&T` as the first parameter.
    ///
    /// # Note
    ///
    /// This can cause an error with certain binary metamethods that can trigger if only the right
    /// side has a metatable. To prevent this, use [`add_meta_function`].
    ///
    /// [`add_meta_function`]: #method.add_meta_function
    fn add_meta_method<S, A, R, M>(&mut self, meta: S, method: M)
    where
        S: Into<MetaMethod>,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + Fn(&'lua Lua, &T, A) -> Result<R>;

    /// Add a metamethod as a function which accepts a `&mut T` as the first parameter.
    ///
    /// # Note
    ///
    /// This can cause an error with certain binary metamethods that can trigger if only the right
    /// side has a metatable. To prevent this, use [`add_meta_function`].
    ///
    /// [`add_meta_function`]: #method.add_meta_function
    fn add_meta_method_mut<S, A, R, M>(&mut self, meta: S, method: M)
    where
        S: Into<MetaMethod>,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + FnMut(&'lua Lua, &mut T, A) -> Result<R>;

    /// Add an async metamethod which accepts a `T` as the first parameter and returns Future.
    /// The passed `T` is cloned from the original value.
    ///
    /// This is an async version of [`add_meta_method`].
    ///
    /// Requires `feature = "async"`
    ///
    /// [`add_meta_method`]: #method.add_meta_method
    #[cfg(all(feature = "async", not(feature = "lua51")))]
    #[cfg_attr(docsrs, doc(cfg(feature = "async")))]
    fn add_async_meta_method<S, A, R, M, MR>(&mut self, name: S, method: M)
    where
        T: Clone,
        S: Into<MetaMethod>,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + Fn(&'lua Lua, T, A) -> MR,
        MR: 'lua + Future<Output = Result<R>>;

    /// Add a metamethod which accepts generic arguments.
    ///
    /// Metamethods for binary operators can be triggered if either the left or right argument to
    /// the binary operator has a metatable, so the first argument here is not necessarily a
    /// userdata of type `T`.
    fn add_meta_function<S, A, R, F>(&mut self, meta: S, function: F)
    where
        S: Into<MetaMethod>,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + Fn(&'lua Lua, A) -> Result<R>;

    /// Add a metamethod as a mutable function which accepts generic arguments.
    ///
    /// This is a version of [`add_meta_function`] that accepts a FnMut argument.
    ///
    /// [`add_meta_function`]: #method.add_meta_function
    fn add_meta_function_mut<S, A, R, F>(&mut self, meta: S, function: F)
    where
        S: Into<MetaMethod>,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + FnMut(&'lua Lua, A) -> Result<R>;

    /// Add a metamethod which accepts generic arguments and returns Future.
    ///
    /// This is an async version of [`add_meta_function`].
    ///
    /// Requires `feature = "async"`
    ///
    /// [`add_meta_function`]: #method.add_meta_function
    #[cfg(all(feature = "async", not(feature = "lua51")))]
    #[cfg_attr(docsrs, doc(cfg(feature = "async")))]
    fn add_async_meta_function<S, A, R, F, FR>(&mut self, name: S, function: F)
    where
        S: Into<MetaMethod>,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + Fn(&'lua Lua, A) -> FR,
        FR: 'lua + Future<Output = Result<R>>;

    //
    // Below are internal methods used in generated code
    //

    #[doc(hidden)]
    fn add_callback(&mut self, _name: Vec<u8>, _callback: Callback<'lua, 'static>) {}

    #[doc(hidden)]
    #[cfg(feature = "async")]
    fn add_async_callback(&mut self, _name: Vec<u8>, _callback: AsyncCallback<'lua, 'static>) {}

    #[doc(hidden)]
    fn add_meta_callback(&mut self, _meta: MetaMethod, _callback: Callback<'lua, 'static>) {}

    #[doc(hidden)]
    #[cfg(feature = "async")]
    fn add_async_meta_callback(
        &mut self,
        _meta: MetaMethod,
        _callback: AsyncCallback<'lua, 'static>,
    ) {
    }
}

/// Field registry for [`UserData`] implementors.
///
/// [`UserData`]: crate::UserData
pub trait UserDataFields<'lua, T: UserData> {
    /// Add a regular field getter as a method which accepts a `&T` as the parameter.
    ///
    /// Regular field getters are implemented by overriding the `__index` metamethod and returning the
    /// accessed field. This allows them to be used with the expected `userdata.field` syntax.
    ///
    /// If `add_meta_method` is used to set the `__index` metamethod, the `__index` metamethod will
    /// be used as a fall-back if no regular field or method are found.
    fn add_field_method_get<S, R, M>(&mut self, name: &S, method: M)
    where
        S: AsRef<[u8]> + ?Sized,
        R: ToLua<'lua>,
        M: 'static + MaybeSend + Fn(&'lua Lua, &T) -> Result<R>;

    /// Add a regular field setter as a method which accepts a `&mut T` as the first parameter.
    ///
    /// Regular field setters are implemented by overriding the `__newindex` metamethod and setting the
    /// accessed field. This allows them to be used with the expected `userdata.field = value` syntax.
    ///
    /// If `add_meta_method` is used to set the `__newindex` metamethod, the `__newindex` metamethod will
    /// be used as a fall-back if no regular field is found.
    fn add_field_method_set<S, A, M>(&mut self, name: &S, method: M)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLua<'lua>,
        M: 'static + MaybeSend + FnMut(&'lua Lua, &mut T, A) -> Result<()>;

    /// Add a regular field getter as a function which accepts a generic [`AnyUserData`] of type `T`
    /// argument.
    ///
    /// Prefer to use [`add_field_method_get`] as it is easier to use.
    ///
    /// [`AnyUserData`]: crate::AnyUserData
    /// [`add_field_method_get`]: #method.add_field_method_get
    fn add_field_function_get<S, R, F>(&mut self, name: &S, function: F)
    where
        S: AsRef<[u8]> + ?Sized,
        R: ToLua<'lua>,
        F: 'static + MaybeSend + Fn(&'lua Lua, AnyUserData<'lua>) -> Result<R>;

    /// Add a regular field setter as a function which accepts a generic [`AnyUserData`] of type `T`
    /// first argument.
    ///
    /// Prefer to use [`add_field_method_set`] as it is easier to use.
    ///
    /// [`AnyUserData`]: crate::AnyUserData
    /// [`add_field_method_set`]: #method.add_field_method_set
    fn add_field_function_set<S, A, F>(&mut self, name: &S, function: F)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLua<'lua>,
        F: 'static + MaybeSend + FnMut(&'lua Lua, AnyUserData<'lua>, A) -> Result<()>;

    /// Add a metamethod value computed from `f`.
    ///
    /// This will initialize the metamethod value from `f` on `UserData` creation.
    ///
    /// # Note
    ///
    /// `mlua` will trigger an error on an attempt to define a protected metamethod,
    /// like `__gc` or `__metatable`.
    fn add_meta_field_with<S, R, F>(&mut self, meta: S, f: F)
    where
        S: Into<MetaMethod>,
        F: 'static + MaybeSend + Fn(&'lua Lua) -> Result<R>,
        R: ToLua<'lua>;

    //
    // Below are internal methods used in generated code
    //

    #[doc(hidden)]
    fn add_field_getter(&mut self, _name: Vec<u8>, _callback: Callback<'lua, 'static>) {}

    #[doc(hidden)]
    fn add_field_setter(&mut self, _name: Vec<u8>, _callback: Callback<'lua, 'static>) {}
}

/// Trait for custom userdata types.
///
/// By implementing this trait, a struct becomes eligible for use inside Lua code.
/// Implementation of [`ToLua`] is automatically provided, [`FromLua`] is implemented
/// only for `T: UserData + Clone`.
///
///
/// # Examples
///
/// ```
/// # use hv_lua::{Lua, Result, UserData};
/// # fn main() -> Result<()> {
/// # let lua = Lua::new();
/// struct MyUserData(i32);
///
/// impl UserData for MyUserData {}
///
/// // `MyUserData` now implements `ToLua`:
/// lua.globals().set("myobject", MyUserData(123))?;
///
/// lua.load("assert(type(myobject) == 'userdata')").exec()?;
/// # Ok(())
/// # }
/// ```
///
/// Custom fields, methods and operators can be provided by implementing `add_fields` or `add_methods`
/// (refer to [`UserDataFields`] and [`UserDataMethods`] for more information):
///
/// ```
/// # use hv_lua::{Lua, MetaMethod, Result, UserData, UserDataFields, UserDataMethods};
/// # fn main() -> Result<()> {
/// # let lua = Lua::new();
/// struct MyUserData(i32);
///
/// impl UserData for MyUserData {
///     fn add_fields<'lua, F: UserDataFields<'lua, Self>>(fields: &mut F) {
///         fields.add_field_method_get("val", |_, this| Ok(this.0));
///     }
///
///     fn add_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
///         methods.add_method_mut("add", |_, this, value: i32| {
///             this.0 += value;
///             Ok(())
///         });
///
///         methods.add_meta_method(MetaMethod::Add, |_, this, value: i32| {
///             Ok(this.0 + value)
///         });
///     }
/// }
///
/// lua.globals().set("myobject", MyUserData(123))?;
///
/// lua.load(r#"
///     assert(myobject.val == 123)
///     myobject:add(7)
///     assert(myobject.val == 130)
///     assert(myobject + 10 == 140)
/// "#).exec()?;
/// # Ok(())
/// # }
/// ```
///
/// [`ToLua`]: crate::ToLua
/// [`FromLua`]: crate::FromLua
/// [`UserDataFields`]: crate::UserDataFields
/// [`UserDataMethods`]: crate::UserDataMethods
pub trait UserData: Sized {
    /// Adds custom fields specific to this userdata.
    fn add_fields<'lua, F: UserDataFields<'lua, Self>>(_fields: &mut F) {}

    /// Adds custom methods and operators specific to this userdata.
    fn add_methods<'lua, M: UserDataMethods<'lua, Self>>(_methods: &mut M) {}

    /// Hook to perform static initialization when the metatable for this userdata is created.
    /// Called before `add_fields` and `add_methods`.
    ///
    /// This is useful, for example, for ensuring that the [`TypeTable`] for this type has some
    /// necessary traits registered with it.
    fn on_metatable_init(_table: Type<Self>) {}

    /// Adds custom fields to the *type* object for this userdata.
    fn add_type_fields<'lua, F: UserDataFields<'lua, Type<Self>>>(_fields: &mut F)
    where
        Self: 'static + MaybeSend,
    {
    }

    /// Adds custom methods and operators to the *type* object for this userdata.
    fn add_type_methods<'lua, M: UserDataMethods<'lua, Type<Self>>>(_methods: &mut M)
    where
        Self: 'static + MaybeSend,
    {
    }

    /// Hook to perform static initialization when the metatable for the [`TypedTypeTable<Self>`]
    /// userdata is created. Called before `add_type_fields` and `add_type_methods`.
    fn on_type_metatable_init(_table: Type<Type<Self>>)
    where
        Self: 'static + MaybeSend,
    {
    }
    /// Allows you to add extra conversion ways from Lua values to this type.
    fn from_lua_fallback(lua_value: Value, _: &Lua) -> Result<Self> {
        match lua_value {
            Value::UserData(_) => Err(Error::UserDataTypeMismatch),
            x => Err(Error::FromLuaConversionError {
                from: x.type_name(),
                to: "userdata",
                message: None,
            }),
        }
    }
}

// Wraps UserData in a way to always implement `serde::Serialize` trait.
pub(crate) struct UserDataCell(RefCell<AlchemicalPtr>);

impl UserDataCell {
    #[inline]
    pub(crate) fn new<T: 'static>(data: T) -> Self {
        UserDataCell(RefCell::new(AlchemicalPtr::new(Box::into_raw(Box::new(
            data,
        )))))
    }

    #[inline]
    pub(crate) fn new_nonstatic<T>(data: T) -> Self {
        UserDataCell(RefCell::new(unsafe {
            AlchemicalPtr::from_raw_parts(
                Box::into_raw(Box::new(data)).cast(),
                TypeTable::of::<()>(),
            )
        }))
    }

    // Immutably borrows the wrapped value.
    #[inline]
    pub(crate) unsafe fn try_borrow<T>(&self) -> Result<Ref<T>> {
        self.0
            .try_borrow()
            .map(|r| Ref::map(r, |r| &*r.as_ptr().cast()))
            .map_err(|_| Error::UserDataBorrowError)
    }

    // Mutably borrows the wrapped value.
    #[inline]
    pub(crate) unsafe fn try_borrow_mut<T>(&self) -> Result<RefMut<T>> {
        self.0
            .try_borrow_mut()
            .map(|r| RefMut::map(r, |r| &mut *r.as_ptr().cast()))
            .map_err(|_| Error::UserDataBorrowMutError)
    }

    #[inline]
    pub(crate) unsafe fn try_dyn_borrow<U: ?Sized + Alchemy>(&self) -> Result<Ref<U>> {
        let res = self.0.try_borrow();
        let r = res.map_err(|_| Error::UserDataBorrowError)?;
        Ref::filter_map(r, |r| r.downcast_dyn_ref::<U>()).map_err(|_| Error::UserDataDynMismatch)
    }

    #[inline]
    pub(crate) unsafe fn try_dyn_borrow_mut<U: ?Sized + Alchemy>(&self) -> Result<RefMut<U>> {
        let res = self.0.try_borrow_mut();
        let r = res.map_err(|_| Error::UserDataBorrowMutError)?;
        RefMut::filter_map(r, |r| r.downcast_dyn_mut::<U>()).map_err(|_| Error::UserDataDynMismatch)
    }

    // Consumes this `UserDataCell`, returning the wrapped value.
    #[inline]
    pub(crate) unsafe fn into_inner<T>(self) -> T {
        let ptr = self.0.into_inner().as_ptr().cast::<T>();
        *Box::from_raw(ptr)
    }

    #[inline]
    pub(crate) unsafe fn into_boxed(self) -> Box<dyn AlchemicalAny> {
        let ptr = self.0.into_inner().as_alchemical_any_mut();
        Box::from_raw(ptr)
    }
}

// pub(crate) enum UserDataWrapped<T> {
//     Default(Box<T>),
//     #[cfg(feature = "serialize")]
//     Serializable(Box<dyn erased_serde::Serialize>),
// }

// impl<T> UserDataWrapped<T> {
//     #[inline]
//     fn new(data: T) -> Self {
//         UserDataWrapped::Default(Box::new(data))
//     }

//     #[cfg(feature = "serialize")]
//     #[inline]
//     fn new_ser(data: T) -> Self
//     where
//         T: 'static + Serialize,
//     {
//         UserDataWrapped::Serializable(Box::new(data))
//     }

//     #[inline]
//     fn into_inner(self) -> T {
//         match self {
//             Self::Default(data) => *data,
//             #[cfg(feature = "serialize")]
//             Self::Serializable(data) => unsafe { *Box::from_raw(Box::into_raw(data) as *mut T) },
//         }
//     }
// }

// impl<T> Deref for UserDataWrapped<T> {
//     type Target = T;

//     #[inline]
//     fn deref(&self) -> &Self::Target {
//         match self {
//             Self::Default(data) => data,
//             #[cfg(feature = "serialize")]
//             Self::Serializable(data) => unsafe {
//                 &*(data.as_ref() as *const _ as *const Self::Target)
//             },
//         }
//     }
// }

// impl<T> DerefMut for UserDataWrapped<T> {
//     #[inline]
//     fn deref_mut(&mut self) -> &mut Self::Target {
//         match self {
//             Self::Default(data) => data,
//             #[cfg(feature = "serialize")]
//             Self::Serializable(data) => unsafe {
//                 &mut *(data.as_mut() as *mut _ as *mut Self::Target)
//             },
//         }
//     }
// }

#[cfg(feature = "serialize")]
struct UserDataSerializeError;

#[cfg(feature = "serialize")]
impl Serialize for UserDataSerializeError {
    fn serialize<S>(&self, _serializer: S) -> StdResult<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Err(ser::Error::custom("cannot serialize <userdata>"))
    }
}

/// Handle to an internal Lua userdata for any type that implements [`UserData`].
///
/// Similar to `std::any::Any`, this provides an interface for dynamic type checking via the [`is`]
/// and [`borrow`] methods.
///
/// Internally, instances are stored in a `RefCell`, to best match the mutable semantics of the Lua
/// language.
///
/// # Note
///
/// This API should only be used when necessary. Implementing [`UserData`] already allows defining
/// methods which check the type and acquire a borrow behind the scenes.
///
/// [`UserData`]: crate::UserData
/// [`is`]: crate::AnyUserData::is
/// [`borrow`]: crate::AnyUserData::borrow
#[derive(Clone, Debug)]
pub struct AnyUserData<'lua>(pub(crate) LuaRef<'lua>);

impl<'lua> AnyUserData<'lua> {
    /// Checks whether the type of this userdata is `T`.
    pub fn is<T: 'static + UserData>(&self) -> bool {
        match self.inspect::<T, _, _>(|_| Ok(())) {
            Ok(()) => true,
            Err(Error::UserDataTypeMismatch) => false,
            Err(_) => unreachable!(),
        }
    }

    /// Get the [`TypeTable`] of the value inside this userdata, if it has one.
    pub fn type_table(&self) -> Option<&'static TypeTable> {
        let lua = self.0.lua;
        unsafe {
            let _sg = StackGuard::new(lua.state);
            check_stack(lua.state, 2).expect("unreachable");
            lua.push_userdata_ref(&self.0).expect("unreachable")
        }
    }

    pub fn meta_type_table(&self) -> Result<&'static TypeTable> {
        self.dyn_borrow::<dyn MetaType>()
            .map(|t| t.type_table_of_subject())
    }

    /// Borrow this userdata immutably if it is of type `T`.
    ///
    /// # Errors
    ///
    /// Returns a `UserDataBorrowError` if the userdata is already mutably borrowed. Returns a
    /// `UserDataTypeMismatch` if the userdata is not of type `T`.
    #[inline]
    pub fn borrow<T: 'static + UserData>(&self) -> Result<Ref<T>> {
        self.inspect::<T, _, _>(|cell| unsafe { cell.try_borrow::<T>() })
    }

    /// Borrow this userdata mutably if it is of type `T`.
    ///
    /// # Errors
    ///
    /// Returns a `UserDataBorrowMutError` if the userdata cannot be mutably borrowed.
    /// Returns a `UserDataTypeMismatch` if the userdata is not of type `T`.
    #[inline]
    pub fn borrow_mut<T: 'static + UserData>(&self) -> Result<RefMut<T>> {
        self.inspect::<T, _, _>(|cell| unsafe { cell.try_borrow_mut::<T>() })
    }

    #[inline]
    pub fn dyn_borrow<U: ?Sized + Alchemy>(&self) -> Result<Ref<U>> {
        self.inspect_raw(|cell| unsafe { cell.try_dyn_borrow::<U>() })
    }

    #[inline]
    pub fn dyn_borrow_mut<U: ?Sized + Alchemy>(&self) -> Result<RefMut<U>> {
        self.inspect_raw(|cell| unsafe { cell.try_dyn_borrow_mut::<U>() })
    }

    /// Takes out the value of `UserData` and sets the special "destructed" metatable that prevents
    /// any further operations with this userdata.
    ///
    /// All associated user values will be also cleared.
    pub fn take<T: 'static + UserData>(&self) -> Result<T> {
        let lua = self.0.lua;
        unsafe {
            let _sg = StackGuard::new(lua.state);
            check_stack(lua.state, 3)?;

            let type_id = lua.push_userdata_ref(&self.0)?.map(|tinfo| tinfo.id);
            match type_id {
                Some(type_id) if type_id == TypeId::of::<T>() => {
                    // Try to borrow userdata exclusively
                    let _ = (*get_userdata::<UserDataCell>(lua.state, -1)).try_borrow_mut::<T>()?;

                    // Clear uservalue
                    #[cfg(any(feature = "lua54", feature = "lua53", feature = "lua52"))]
                    ffi::lua_pushnil(lua.state);
                    #[cfg(any(feature = "lua51", feature = "luajit"))]
                    protect_lua!(lua.state, 0, 1, fn(state) ffi::lua_newtable(state))?;
                    ffi::lua_setuservalue(lua.state, -2);

                    Ok(take_userdata::<UserDataCell>(lua.state).into_inner::<T>())
                }
                _ => Err(Error::UserDataTypeMismatch),
            }
        }
    }

    /// Clones the data inside the `UserData`, or takes out the value of `UserData` and sets the
    /// special "destructed" metatable that prevents any further operations with this userdata. It
    /// will first try cloning, and if that fails, it will take and destroy the userdata.
    pub fn clone_or_take<T: 'static + UserData>(&self) -> Result<T> {
        let lua = self.0.lua;
        unsafe {
            let _sg = StackGuard::new(lua.state);
            check_stack(lua.state, 2)?;

            let tinfo = lua.push_userdata_ref(&self.0)?;
            match tinfo {
                Some(tinfo) if tinfo.id == TypeId::of::<T>() => {
                    // Try to borrow userdata exclusively. At the same time, try cloning it.
                    let userdata_cell = get_userdata::<UserDataCell>(lua.state, -1);
                    let dyn_borrowed =
                        (*userdata_cell).try_dyn_borrow_mut::<dyn AlchemicalAny>()?;
                    let maybe_cloned = (*dyn_borrowed).try_clone();

                    if let Some(cloned) = maybe_cloned {
                        Ok(*cloned.downcast::<T>().unwrap())
                    } else {
                        // Clear uservalue
                        #[cfg(any(feature = "lua54", feature = "lua53", feature = "lua52"))]
                        ffi::lua_pushnil(lua.state);
                        #[cfg(any(feature = "lua51", feature = "luajit"))]
                        protect_lua!(lua.state, 0, 1, fn(state) ffi::lua_newtable(state))?;
                        ffi::lua_setuservalue(lua.state, -2);

                        Ok(take_userdata::<UserDataCell>(lua.state).into_inner::<T>())
                    }
                }
                Some(_) => Err(Error::UserDataTypeMismatch),
                _ => Err(Error::UserDataDestructed),
            }
        }
    }

    /// Takes out the value of `UserData` and sets the special "destructed" metatable that prevents
    /// any further operations with this userdata.
    pub fn dyn_take<U: ?Sized + Alchemy>(&self) -> Result<Box<U>> {
        let lua = self.0.lua;
        unsafe {
            let _sg = StackGuard::new(lua.state);
            check_stack(lua.state, 2)?;

            let type_table = lua.push_userdata_ref(&self.0)?;
            match type_table {
                Some(type_table) if type_table.id != TypeId::of::<DestructedUserdataMT>() => {
                    // Try to borrow userdata exclusively
                    let _ =
                        (*get_userdata::<UserDataCell>(lua.state, -1)).try_dyn_borrow_mut::<U>()?;

                    // Clear associated user values
                    #[cfg(feature = "lua54")]
                    for i in 1..=USER_VALUE_MAXSLOT {
                        ffi::lua_pushnil(lua.state);
                        ffi::lua_setiuservalue(lua.state, -2, i as c_int);
                    }
                    #[cfg(any(feature = "lua53", feature = "lua52"))]
                    {
                        ffi::lua_pushnil(lua.state);
                        ffi::lua_setuservalue(lua.state, -2);
                    }
                    #[cfg(any(feature = "lua51", feature = "luajit"))]
                    protect_lua!(lua.state, 1, 1, fn(state) {
                        ffi::lua_newtable(state);
                        ffi::lua_setuservalue(state, -2);
                    })?;

                    Ok(take_userdata::<UserDataCell>(lua.state)
                        .into_boxed()
                        .dyncast::<U>()
                        .unwrap())
                }
                Some(_) => Err(Error::UserDataDestructed),
                _ => Err(Error::UserDataTypeMismatch),
            }
        }
    }

    /// Clones the data inside the `UserData`, or takes out the value of `UserData` and sets the
    /// special "destructed" metatable that prevents any further operations with this userdata. It
    /// will first try cloning, and if that fails, it will take and destroy the userdata.
    pub fn dyn_clone_or_take<U: ?Sized + Alchemy>(&self) -> Result<Box<U>> {
        let lua = self.0.lua;
        unsafe {
            let _sg = StackGuard::new(lua.state);
            check_stack(lua.state, 2)?;

            let type_table = lua.push_userdata_ref(&self.0)?;
            match type_table {
                Some(type_table)
                    if type_table.id != TypeId::of::<DestructedUserdataMT>()
                        && type_table.is::<U>() =>
                {
                    // Try to borrow userdata exclusively. At the same time, try cloning it.
                    let maybe_cloned = (*get_userdata::<UserDataCell>(lua.state, -1))
                        .try_dyn_borrow_mut::<dyn AlchemicalAny>()?
                        .try_clone();

                    if let Some(cloned) = maybe_cloned {
                        Ok(cloned.dyncast::<U>().unwrap())
                    } else {
                        // Clear uservalue
                        #[cfg(any(feature = "lua54", feature = "lua53", feature = "lua52"))]
                        ffi::lua_pushnil(lua.state);
                        #[cfg(any(feature = "lua51", feature = "luajit"))]
                        protect_lua!(lua.state, 0, 1, fn(state) ffi::lua_newtable(state))?;
                        ffi::lua_setuservalue(lua.state, -2);

                        Ok(take_userdata::<UserDataCell>(lua.state)
                            .into_boxed()
                            .dyncast::<U>()
                            .unwrap())
                    }
                }
                Some(_) => Err(Error::UserDataDestructed),
                _ => Err(Error::UserDataTypeMismatch),
            }
        }
    }

    pub fn convert_into<T: 'static>(&self) -> Result<T> {
        let lua = self.0.lua;
        unsafe {
            let _sg = StackGuard::new(lua.state);
            check_stack(lua.state, 2)?;

            let type_table = lua.push_userdata_ref(&self.0)?;
            match type_table {
                Some(type_table) if type_table.id != TypeId::of::<DestructedUserdataMT>() => {
                    let into_proxy_t = type_table
                        .get::<dyn IntoProxy<T>>()
                        .ok_or(Error::UserDataDynMismatch)?;

                    // Try to borrow userdata exclusively. At the same time, try cloning it.
                    let this_ptr = (*get_userdata::<UserDataCell>(lua.state, -1))
                        .0
                        .try_borrow_mut()
                        .map_err(|_| Error::UserDataBorrowMutError)?
                        .as_ptr();

                    let mut converted = MaybeUninit::<T>::uninit();
                    into_proxy_t
                        .to_dyn_object_ptr::<dyn IntoProxy<T>>(this_ptr)
                        .convert_into_ptr(converted.as_mut_ptr());

                    if type_table.is_copy() {
                        Ok(converted.assume_init())
                    } else {
                        // Clear uservalue
                        #[cfg(any(feature = "lua54", feature = "lua53", feature = "lua52"))]
                        ffi::lua_pushnil(lua.state);
                        #[cfg(any(feature = "lua51", feature = "luajit"))]
                        protect_lua!(lua.state, 0, 1, fn(state) ffi::lua_newtable(state))?;
                        ffi::lua_setuservalue(lua.state, -2);

                        let ptr =
                            Box::into_raw(take_userdata::<UserDataCell>(lua.state).into_boxed());
                        std::alloc::dealloc(ptr as *mut u8, type_table.layout);

                        Ok(converted.assume_init())
                    }
                }
                Some(_) => Err(Error::UserDataDestructed),
                _ => Err(Error::UserDataTypeMismatch),
            }
        }
    }

    /// Sets an associated value to this `AnyUserData`.
    ///
    /// The value may be any Lua value whatsoever, and can be retrieved with [`get_user_value`].
    ///
    /// This is the same as calling [`set_nth_user_value`] with `n` set to 1.
    ///
    /// [`get_user_value`]: #method.get_user_value
    /// [`set_nth_user_value`]: #method.set_nth_user_value
    pub fn set_user_value<V: ToLua<'lua>>(&self, v: V) -> Result<()> {
        self.set_nth_user_value(1, v)
    }

    /// Sets an associated `n`th value to this `AnyUserData`.
    ///
    /// The value may be any Lua value whatsoever, and can be retrieved with [`get_nth_user_value`].
    /// `n` starts from 1 and can be up to 65535.
    ///
    /// This is supported for all Lua versions.
    /// In Lua 5.4 first 7 elements are stored in a most efficient way.
    /// For other Lua versions this functionality is provided using a wrapping table.
    ///
    /// [`get_nth_user_value`]: #method.get_nth_user_value
    pub fn set_nth_user_value<V: ToLua<'lua>>(&self, n: usize, v: V) -> Result<()> {
        if n < 1 || n > u16::MAX as usize {
            return Err(Error::RuntimeError(
                "user value index out of bounds".to_string(),
            ));
        }

        let lua = self.0.lua;
        unsafe {
            let _sg = StackGuard::new(lua.state);
            check_stack(lua.state, 5)?;

            lua.push_userdata_ref(&self.0)?;
            lua.push_value(v.to_lua(lua)?)?;

            #[cfg(feature = "lua54")]
            if n < USER_VALUE_MAXSLOT {
                ffi::lua_setiuservalue(lua.state, -2, n as c_int);
                return Ok(());
            }

            // Multiple (extra) user values are emulated by storing them in a table

            let getuservalue_t = |state, idx| {
                #[cfg(feature = "lua54")]
                return ffi::lua_getiuservalue(state, idx, USER_VALUE_MAXSLOT as c_int);
                #[cfg(not(feature = "lua54"))]
                return ffi::lua_getuservalue(state, idx);
            };
            let getn = |n: usize| {
                #[cfg(feature = "lua54")]
                return n - USER_VALUE_MAXSLOT + 1;
                #[cfg(not(feature = "lua54"))]
                return n;
            };

            protect_lua!(lua.state, 2, 0, |state| {
                if getuservalue_t(lua.state, -2) != ffi::LUA_TTABLE {
                    // Create a new table to use as uservalue
                    ffi::lua_pop(lua.state, 1);
                    ffi::lua_newtable(state);
                    ffi::lua_pushvalue(state, -1);

                    #[cfg(feature = "lua54")]
                    ffi::lua_setiuservalue(lua.state, -4, USER_VALUE_MAXSLOT as c_int);
                    #[cfg(not(feature = "lua54"))]
                    ffi::lua_setuservalue(lua.state, -4);
                }
                ffi::lua_pushvalue(state, -2);
                ffi::lua_rawseti(state, -2, getn(n) as ffi::lua_Integer);
            })?;

            Ok(())
        }
    }

    /// Returns an associated value set by [`set_user_value`].
    ///
    /// This is the same as calling [`get_nth_user_value`] with `n` set to 1.
    ///
    /// [`set_user_value`]: #method.set_user_value
    /// [`get_nth_user_value`]: #method.get_nth_user_value
    pub fn get_user_value<V: FromLua<'lua>>(&self) -> Result<V> {
        self.get_nth_user_value(1)
    }

    /// Returns an associated `n`th value set by [`set_nth_user_value`].
    ///
    /// `n` starts from 1 and can be up to 65535.
    ///
    /// This is supported for all Lua versions.
    /// In Lua 5.4 first 7 elements are stored in a most efficient way.
    /// For other Lua versions this functionality is provided using a wrapping table.
    ///
    /// [`set_nth_user_value`]: #method.set_nth_user_value
    pub fn get_nth_user_value<V: FromLua<'lua>>(&self, n: usize) -> Result<V> {
        if n < 1 || n > u16::MAX as usize {
            return Err(Error::RuntimeError(
                "user value index out of bounds".to_string(),
            ));
        }

        let lua = self.0.lua;
        unsafe {
            let _sg = StackGuard::new(lua.state);
            check_stack(lua.state, 4)?;

            lua.push_userdata_ref(&self.0)?;

            #[cfg(feature = "lua54")]
            if n < USER_VALUE_MAXSLOT {
                ffi::lua_getiuservalue(lua.state, -1, n as c_int);
                return V::from_lua(lua.pop_value(), lua);
            }

            // Multiple (extra) user values are emulated by storing them in a table

            let getuservalue_t = |state, idx| {
                #[cfg(feature = "lua54")]
                return ffi::lua_getiuservalue(state, idx, USER_VALUE_MAXSLOT as c_int);
                #[cfg(not(feature = "lua54"))]
                return ffi::lua_getuservalue(state, idx);
            };
            let getn = |n: usize| {
                #[cfg(feature = "lua54")]
                return n - USER_VALUE_MAXSLOT + 1;
                #[cfg(not(feature = "lua54"))]
                return n;
            };

            protect_lua!(lua.state, 1, 1, |state| {
                if getuservalue_t(lua.state, -1) != ffi::LUA_TTABLE {
                    ffi::lua_pushnil(lua.state);
                    return;
                }
                ffi::lua_rawgeti(state, -1, getn(n) as ffi::lua_Integer);
            })?;

            V::from_lua(lua.pop_value(), lua)
        }
    }

    /// Returns a metatable of this `UserData`.
    ///
    /// Returned [`UserDataMetatable`] object wraps the original metatable and
    /// provides safe access to its methods.
    ///
    /// For `T: UserData + 'static` returned metatable is shared among all instances of type `T`.
    ///
    /// [`UserDataMetatable`]: crate::UserDataMetatable
    pub fn get_metatable(&self) -> Result<UserDataMetatable<'lua>> {
        self.get_raw_metatable().map(UserDataMetatable)
    }

    fn get_raw_metatable(&self) -> Result<Table<'lua>> {
        unsafe {
            let lua = self.0.lua;
            let _sg = StackGuard::new(lua.state);
            check_stack(lua.state, 3)?;

            lua.push_userdata_ref(&self.0)?;
            ffi::lua_getmetatable(lua.state, -1); // Checked that non-empty on the previous call
            Ok(Table(lua.pop_ref()))
        }
    }

    pub(crate) fn equals<T: AsRef<Self>>(&self, other: T) -> Result<bool> {
        let other = other.as_ref();
        // Uses lua_rawequal() under the hood
        if self == other {
            return Ok(true);
        }

        let mt = self.get_raw_metatable()?;
        if mt != other.get_raw_metatable()? {
            return Ok(false);
        }

        if mt.contains_key("__eq")? {
            return mt
                .get::<_, Function>("__eq")?
                .call((self.clone(), other.clone()));
        }

        Ok(false)
    }

    fn inspect<'a, T, R, F>(&'a self, func: F) -> Result<R>
    where
        T: 'static + UserData,
        F: FnOnce(&'a UserDataCell) -> Result<R>,
    {
        let lua = self.0.lua;
        unsafe {
            let _sg = StackGuard::new(lua.state);
            check_stack(lua.state, 2)?;

            let type_id = lua.push_userdata_ref(&self.0)?.map(|t| t.id);
            match type_id {
                Some(type_id) if type_id == TypeId::of::<T>() => {
                    func(&*get_userdata::<UserDataCell>(lua.state, -1))
                }
                _ => Err(Error::UserDataTypeMismatch),
            }
        }
    }

    fn inspect_raw<'a, R, F>(&'a self, func: F) -> Result<R>
    where
        F: FnOnce(&'a UserDataCell) -> Result<R>,
    {
        let lua = self.0.lua;
        unsafe {
            let _sg = StackGuard::new(lua.state);
            check_stack(lua.state, 2)?;

            let type_id = lua.push_userdata_ref(&self.0)?.map(|t| t.id);
            match type_id {
                Some(type_id) if type_id == TypeId::of::<DestructedUserdataMT>() => {
                    Err(Error::UserDataDestructed)
                }
                Some(_) => func(&*get_userdata::<UserDataCell>(lua.state, -1)),
                _ => Err(Error::UserDataTypeMismatch),
            }
        }
    }
}

impl<'lua> PartialEq for AnyUserData<'lua> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<'lua> AsRef<AnyUserData<'lua>> for AnyUserData<'lua> {
    #[inline]
    fn as_ref(&self) -> &Self {
        self
    }
}

/// Handle to a `UserData` metatable.
#[derive(Clone, Debug)]
pub struct UserDataMetatable<'lua>(pub(crate) Table<'lua>);

impl<'lua> UserDataMetatable<'lua> {
    /// Gets the value associated to `key` from the metatable.
    ///
    /// If no value is associated to `key`, returns the `Nil` value.
    /// Access to restricted metamethods such as `__gc` or `__metatable` will cause an error.
    pub fn get<K: Into<MetaMethod>, V: FromLua<'lua>>(&self, key: K) -> Result<V> {
        self.0.raw_get(key.into().validate()?.name())
    }

    /// Sets a key-value pair in the metatable.
    ///
    /// If the value is `Nil`, this will effectively remove the `key`.
    /// Access to restricted metamethods such as `__gc` or `__metatable` will cause an error.
    /// Setting `__index` or `__newindex` metamethods is also restricted because their values are cached
    /// for `mlua` internal usage.
    pub fn set<K: Into<MetaMethod>, V: ToLua<'lua>>(&self, key: K, value: V) -> Result<()> {
        let key = key.into().validate()?;
        // `__index` and `__newindex` cannot be changed in runtime, because values are cached
        if key == MetaMethod::Index || key == MetaMethod::NewIndex {
            return Err(Error::MetaMethodRestricted(key.to_string()));
        }
        self.0.raw_set(key.name(), value)
    }

    /// Checks whether the metatable contains a non-nil value for `key`.
    pub fn contains<K: Into<MetaMethod>>(&self, key: K) -> Result<bool> {
        self.0.contains_key(key.into().validate()?.name())
    }

    /// Consumes this metatable and returns an iterator over the pairs of the metatable.
    ///
    /// The pairs are wrapped in a [`Result`], since they are lazily converted to `V` type.
    ///
    /// [`Result`]: crate::Result
    pub fn pairs<V: FromLua<'lua>>(self) -> UserDataMetatablePairs<'lua, V> {
        UserDataMetatablePairs(self.0.pairs())
    }
}

/// An iterator over the pairs of a [`UserData`] metatable.
///
/// It skips restricted metamethods, such as `__gc` or `__metatable`.
///
/// This struct is created by the [`UserDataMetatable::pairs`] method.
///
/// [`UserData`]: crate::UserData
/// [`UserDataMetatable::pairs`]: crate::UserDataMetatable::method.pairs
pub struct UserDataMetatablePairs<'lua, V>(TablePairsIter<'lua, StdString, V>);

impl<'lua, V> Iterator for UserDataMetatablePairs<'lua, V>
where
    V: FromLua<'lua>,
{
    type Item = Result<(MetaMethod, V)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.0.next()? {
                Ok((key, value)) => {
                    // Skip restricted metamethods
                    if let Ok(metamethod) = MetaMethod::from(key).validate() {
                        break Some(Ok((metamethod, value)));
                    }
                }
                Err(e) => break Some(Err(e)),
            }
        }
    }
}

#[cfg(feature = "serialize")]
impl<'lua> Serialize for AnyUserData<'lua> {
    fn serialize<S>(&self, serializer: S) -> StdResult<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let lua = self.0.lua;
        let res = unsafe {
            let _sg = StackGuard::new(lua.state);
            check_stack(lua.state, 3).map_err(ser::Error::custom)?;

            lua.push_userdata_ref(&self.0).map_err(ser::Error::custom)?;
            let ud = &*get_userdata::<UserDataCell>(lua.state, -1);
            ud.try_dyn_borrow::<dyn erased_serde::Serialize>()
        };
        match res {
            Ok(data) => data.serialize(serializer),
            Err(Error::UserDataDynMismatch) => UserDataSerializeError.serialize(serializer),
            Err(other) => Err(ser::Error::custom(other)),
        }
    }
}

impl UserData for RegistryKey {
    fn on_metatable_init(table: Type<Self>) {
        table.add_send().add_sync();
    }

    fn add_fields<'lua, F: UserDataFields<'lua, Self>>(fields: &mut F) {
        fields.add_field_method_get("value", |lua, key| lua.registry_value::<Value>(key));
    }

    fn on_type_metatable_init(table: Type<Type<Self>>) {
        #[cfg(feature = "hv-ecs")]
        table.add::<dyn crate::hv::ecs::ComponentType>();
    }

    #[allow(clippy::unit_arg)]
    fn add_type_methods<'lua, M: UserDataMethods<'lua, Type<Self>>>(methods: &mut M)
    where
        Self: 'static,
    {
        methods.add_function("new", |lua, value: Value| lua.create_registry_value(value));
        methods.add_function("expire", |lua, ()| Ok(lua.expire_registry_values()));
    }
}

/// Marker type for [`UserDataMethodsProxy`] and [`UserDataFieldsProxy`] indicating that the proxied
/// methods should go ahead normally on an attempt to access mutably.
///
/// If you're working with a type which is a reference to a [`UserData`]-implementing type and it
/// cannot deal with mutable access (but you still want to add a proxy) use [`Immutable`] and the
/// [`UserDataMethodsProxy::new_immutable`] and [`UserDataFieldsProxy::new_immutable`] constructors.
pub enum Mutable {}

/// Marker type for [`UserDataMethodsProxy`] and [`UserDataFieldsProxy`] indicating  that the
/// proxied methods should fail with an [`Error::UserDataBorrowMutError`] rather than succeeding.
pub enum Immutable {}

/// A proxy for [`UserDataMethods`] which automatically forwards methods on a type `T` which allows
/// guarded borrowing access to a wrapped type `U`. For example, this is used to implement
/// [`UserData`] for types like [`Arc<RwLock<U>>`]. For the [`UserDataFields`] equivalent, see
/// [`UserDataFieldsProxy`].
pub struct UserDataMethodsProxy<'a, 'lua, T, U, M, Marker>
where
    T: UserData + NonBlockingGuardedBorrow<U>,
    U: UserData,
    M: UserDataMethods<'lua, T>,
{
    inner: &'a mut M,
    _phantom: PhantomData<fn(&'lua (), T, U, Marker)>,
}

impl<'a, 'lua, T, U, D> UserDataMethods<'lua, U>
    for UserDataMethodsProxy<'a, 'lua, T, U, D, Mutable>
where
    T: UserData + NonBlockingGuardedBorrow<U> + NonBlockingGuardedMutBorrowMut<U>,
    U: UserData,
    D: UserDataMethods<'lua, T>,
{
    fn add_method<S, A, R, M>(&mut self, name: &S, method: M)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + Fn(&'lua Lua, &U, A) -> Result<R>,
    {
        self.inner.add_method(name, move |lua, this, args| {
            let guard = this
                .try_nonblocking_guarded_borrow()
                .map_err(|_| Error::UserDataProxyBorrowError)?;
            method(lua, &*guard, args)
        });
    }

    fn add_method_mut<S, A, R, M>(&mut self, name: &S, mut method: M)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + FnMut(&'lua Lua, &mut U, A) -> Result<R>,
    {
        self.inner.add_method_mut(name, move |lua, this, args| {
            let mut guard = this
                .try_nonblocking_guarded_mut_borrow_mut()
                .map_err(|_| Error::UserDataProxyBorrowMutError)?;
            method(lua, &mut *guard, args)
        });
    }

    fn add_function<S, A, R, F>(&mut self, name: &S, function: F)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + Fn(&'lua Lua, A) -> Result<R>,
    {
        self.inner.add_function(name, function);
    }

    fn add_function_mut<S, A, R, F>(&mut self, name: &S, function: F)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + FnMut(&'lua Lua, A) -> Result<R>,
    {
        self.inner.add_function_mut(name, function);
    }

    fn add_meta_method<S, A, R, M>(&mut self, meta: S, method: M)
    where
        S: Into<MetaMethod>,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + Fn(&'lua Lua, &U, A) -> Result<R>,
    {
        self.inner.add_meta_method(meta, move |lua, this, args| {
            let guard = this
                .try_nonblocking_guarded_borrow()
                .map_err(|_| Error::UserDataProxyBorrowError)?;
            method(lua, &*guard, args)
        });
    }

    fn add_meta_method_mut<S, A, R, M>(&mut self, meta: S, mut method: M)
    where
        S: Into<MetaMethod>,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + FnMut(&'lua Lua, &mut U, A) -> Result<R>,
    {
        self.inner
            .add_meta_method_mut(meta, move |lua, this, args| {
                let mut guard = this
                    .try_nonblocking_guarded_mut_borrow_mut()
                    .map_err(|_| Error::UserDataProxyBorrowMutError)?;
                method(lua, &mut *guard, args)
            });
    }

    fn add_meta_function<S, A, R, F>(&mut self, meta: S, function: F)
    where
        S: Into<MetaMethod>,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + Fn(&'lua Lua, A) -> Result<R>,
    {
        self.inner.add_meta_function(meta, function);
    }

    fn add_meta_function_mut<S, A, R, F>(&mut self, meta: S, function: F)
    where
        S: Into<MetaMethod>,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + FnMut(&'lua Lua, A) -> Result<R>,
    {
        self.inner.add_meta_function_mut(meta, function);
    }

    fn add_callback(&mut self, name: Vec<u8>, callback: Callback<'lua, 'static>) {
        self.inner.add_callback(name, callback);
    }

    fn add_meta_callback(&mut self, meta: MetaMethod, callback: Callback<'lua, 'static>) {
        self.inner.add_meta_callback(meta, callback);
    }

    #[cfg(feature = "async")]
    fn add_async_method<S, A, R, M, MR>(&mut self, name: &S, method: M)
    where
        U: Clone,
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + Fn(&'lua Lua, U, A) -> MR,
        MR: 'lua + Future<Output = Result<R>>,
    {
        self.inner.add_async_method(name, move |lua, this, args| {
            let guard = this
                .try_guarded_borrow()
                .map_err(|_| Error::UserDataProxyBorrowError)?;
            method(lua, (*guard).clone(), args)
        });
    }

    #[cfg(feature = "async")]
    fn add_async_function<S, A, R, F, FR>(&mut self, name: &S, function: F)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + Fn(&'lua Lua, A) -> FR,
        FR: 'lua + Future<Output = Result<R>>,
    {
        self.inner.add_async_function(name, function);
    }

    #[cfg(feature = "async")]
    fn add_async_callback(&mut self, name: Vec<u8>, callback: AsyncCallback<'lua, 'static>) {
        self.inner.add_async_callback(name, callback);
    }
}

impl<'a, 'lua, T, U, D> UserDataMethods<'lua, U>
    for UserDataMethodsProxy<'a, 'lua, T, U, D, Immutable>
where
    T: UserData + NonBlockingGuardedBorrow<U>,
    U: UserData,
    D: UserDataMethods<'lua, T>,
{
    fn add_method<S, A, R, M>(&mut self, name: &S, method: M)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + Fn(&'lua Lua, &U, A) -> Result<R>,
    {
        self.inner.add_method(name, move |lua, this, args| {
            let guard = this
                .try_nonblocking_guarded_borrow()
                .map_err(|_| Error::UserDataProxyBorrowError)?;
            method(lua, &*guard, args)
        });
    }

    fn add_method_mut<S, A, R, M>(&mut self, name: &S, _method: M)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + FnMut(&'lua Lua, &mut U, A) -> Result<R>,
    {
        self.inner.add_method_mut(name, |_, _, ()| {
            Err::<(), _>(Error::UserDataProxyBorrowMutError)
        });
    }

    fn add_function<S, A, R, F>(&mut self, name: &S, function: F)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + Fn(&'lua Lua, A) -> Result<R>,
    {
        self.inner.add_function(name, function);
    }

    fn add_function_mut<S, A, R, F>(&mut self, name: &S, function: F)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + FnMut(&'lua Lua, A) -> Result<R>,
    {
        self.inner.add_function_mut(name, function);
    }

    fn add_meta_method<S, A, R, M>(&mut self, meta: S, method: M)
    where
        S: Into<MetaMethod>,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + Fn(&'lua Lua, &U, A) -> Result<R>,
    {
        self.inner.add_meta_method(meta, move |lua, this, args| {
            let guard = this
                .try_nonblocking_guarded_borrow()
                .map_err(|_| Error::UserDataProxyBorrowError)?;
            method(lua, &*guard, args)
        });
    }

    fn add_meta_method_mut<S, A, R, M>(&mut self, meta: S, _method: M)
    where
        S: Into<MetaMethod>,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + FnMut(&'lua Lua, &mut U, A) -> Result<R>,
    {
        self.inner.add_meta_method_mut(meta, |_, _, ()| {
            Err::<(), _>(Error::UserDataProxyBorrowMutError)
        });
    }

    fn add_meta_function<S, A, R, F>(&mut self, meta: S, function: F)
    where
        S: Into<MetaMethod>,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + Fn(&'lua Lua, A) -> Result<R>,
    {
        self.inner.add_meta_function(meta, function);
    }

    fn add_meta_function_mut<S, A, R, F>(&mut self, meta: S, function: F)
    where
        S: Into<MetaMethod>,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + FnMut(&'lua Lua, A) -> Result<R>,
    {
        self.inner.add_meta_function_mut(meta, function);
    }

    fn add_callback(&mut self, name: Vec<u8>, callback: Callback<'lua, 'static>) {
        self.inner.add_callback(name, callback);
    }

    fn add_meta_callback(&mut self, meta: MetaMethod, callback: Callback<'lua, 'static>) {
        self.inner.add_meta_callback(meta, callback);
    }

    #[cfg(feature = "async")]
    fn add_async_method<S, A, R, M, MR>(&mut self, name: &S, method: M)
    where
        U: Clone,
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + MaybeSend + Fn(&'lua Lua, U, A) -> MR,
        MR: 'lua + Future<Output = Result<R>>,
    {
        self.inner.add_async_method(name, move |lua, this, args| {
            let guard = this
                .try_guarded_borrow()
                .map_err(|_| Error::UserDataProxyBorrowError)?;
            method(lua, (*guard).clone(), args)
        });
    }

    #[cfg(feature = "async")]
    fn add_async_function<S, A, R, F, FR>(&mut self, name: &S, function: F)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + MaybeSend + Fn(&'lua Lua, A) -> FR,
        FR: 'lua + Future<Output = Result<R>>,
    {
        self.inner.add_async_function(name, function);
    }

    #[cfg(feature = "async")]
    fn add_async_callback(&mut self, name: Vec<u8>, callback: AsyncCallback<'lua, 'static>) {
        self.inner.add_async_callback(name, callback);
    }
}

impl<'a, 'lua, T, U, M> UserDataMethodsProxy<'a, 'lua, T, U, M, Mutable>
where
    T: UserData + NonBlockingGuardedBorrow<U> + NonBlockingGuardedMutBorrowMut<U>,
    U: UserData,
    M: UserDataMethods<'lua, T>,
{
    pub fn new(methods: &'a mut M) -> Self {
        Self {
            inner: methods,
            _phantom: PhantomData,
        }
    }
}

impl<'a, 'lua, T, U, M> UserDataMethodsProxy<'a, 'lua, T, U, M, Immutable>
where
    T: UserData + NonBlockingGuardedBorrow<U>,
    U: UserData,
    M: UserDataMethods<'lua, T>,
{
    pub fn new_immutable(methods: &'a mut M) -> Self {
        Self {
            inner: methods,
            _phantom: PhantomData,
        }
    }
}

/// A proxy for [`UserDataFields`] which automatically forwards field accessors for some type `T`
/// which allows guarded borrowing access to a wrapped type `U`. For example, this is used to
/// implement [`UserData`] for types like [`Arc<RwLock<U>>`]. For the [`UserDataMethods`]
/// equivalent, see [`UserDataMethodsProxy`].
pub struct UserDataFieldsProxy<'a, 'lua, T, U, M, Marker>
where
    T: UserData + NonBlockingGuardedBorrow<U>,
    U: UserData,
    M: UserDataFields<'lua, T>,
{
    inner: &'a mut M,
    _phantom: PhantomData<fn(&'lua (), T, U, Marker)>,
}

impl<'a, 'lua, T, U, D> UserDataFields<'lua, U> for UserDataFieldsProxy<'a, 'lua, T, U, D, Mutable>
where
    T: UserData + NonBlockingGuardedBorrow<U> + NonBlockingGuardedMutBorrowMut<U>,
    U: UserData,
    D: UserDataFields<'lua, T>,
{
    fn add_field_method_get<S, R, M>(&mut self, name: &S, method: M)
    where
        S: AsRef<[u8]> + ?Sized,
        R: ToLua<'lua>,
        M: 'static + MaybeSend + Fn(&'lua Lua, &U) -> Result<R>,
    {
        self.inner.add_field_method_get(name, move |lua, u| {
            let guard = u
                .try_nonblocking_guarded_borrow()
                .map_err(|_| Error::UserDataProxyBorrowError)?;
            method(lua, &*guard)
        });
    }

    fn add_field_method_set<S, A, M>(&mut self, name: &S, mut method: M)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLua<'lua>,
        M: 'static + MaybeSend + FnMut(&'lua Lua, &mut U, A) -> Result<()>,
    {
        self.inner.add_field_method_set(name, move |lua, u, arg| {
            let mut guard = u
                .try_nonblocking_guarded_mut_borrow_mut()
                .map_err(|_| Error::UserDataProxyBorrowMutError)?;
            method(lua, &mut *guard, arg)
        });
    }

    fn add_field_function_get<S, R, F>(&mut self, name: &S, function: F)
    where
        S: AsRef<[u8]> + ?Sized,
        R: ToLua<'lua>,
        F: 'static + MaybeSend + Fn(&'lua Lua, AnyUserData<'lua>) -> Result<R>,
    {
        self.inner.add_field_function_get(name, function);
    }

    fn add_field_function_set<S, A, F>(&mut self, name: &S, function: F)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLua<'lua>,
        F: 'static + MaybeSend + FnMut(&'lua Lua, AnyUserData<'lua>, A) -> Result<()>,
    {
        self.inner.add_field_function_set(name, function);
    }

    fn add_meta_field_with<S, R, F>(&mut self, meta: S, f: F)
    where
        S: Into<MetaMethod>,
        F: 'static + MaybeSend + Fn(&'lua Lua) -> Result<R>,
        R: ToLua<'lua>,
    {
        self.inner.add_meta_field_with(meta, f);
    }

    fn add_field_getter(&mut self, name: Vec<u8>, callback: Callback<'lua, 'static>) {
        self.inner.add_field_getter(name, callback);
    }

    fn add_field_setter(&mut self, name: Vec<u8>, callback: Callback<'lua, 'static>) {
        self.inner.add_field_setter(name, callback);
    }
}

impl<'a, 'lua, T, U, D> UserDataFields<'lua, U>
    for UserDataFieldsProxy<'a, 'lua, T, U, D, Immutable>
where
    T: UserData + NonBlockingGuardedBorrow<U>,
    U: UserData,
    D: UserDataFields<'lua, T>,
{
    fn add_field_method_get<S, R, M>(&mut self, name: &S, method: M)
    where
        S: AsRef<[u8]> + ?Sized,
        R: ToLua<'lua>,
        M: 'static + MaybeSend + Fn(&'lua Lua, &U) -> Result<R>,
    {
        self.inner.add_field_method_get(name, move |lua, u| {
            let guard = u
                .try_nonblocking_guarded_borrow()
                .map_err(|_| Error::UserDataProxyBorrowError)?;
            method(lua, &*guard)
        });
    }

    fn add_field_method_set<S, A, M>(&mut self, name: &S, _method: M)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLua<'lua>,
        M: 'static + MaybeSend + FnMut(&'lua Lua, &mut U, A) -> Result<()>,
    {
        self.inner.add_field_method_set(name, |_, _, _: A| {
            Err::<(), _>(Error::UserDataProxyBorrowMutError)
        });
    }

    fn add_field_function_get<S, R, F>(&mut self, name: &S, function: F)
    where
        S: AsRef<[u8]> + ?Sized,
        R: ToLua<'lua>,
        F: 'static + MaybeSend + Fn(&'lua Lua, AnyUserData<'lua>) -> Result<R>,
    {
        self.inner.add_field_function_get(name, function);
    }

    fn add_field_function_set<S, A, F>(&mut self, name: &S, function: F)
    where
        S: AsRef<[u8]> + ?Sized,
        A: FromLua<'lua>,
        F: 'static + MaybeSend + FnMut(&'lua Lua, AnyUserData<'lua>, A) -> Result<()>,
    {
        self.inner.add_field_function_set(name, function);
    }

    fn add_meta_field_with<S, R, F>(&mut self, meta: S, f: F)
    where
        S: Into<MetaMethod>,
        F: 'static + MaybeSend + Fn(&'lua Lua) -> Result<R>,
        R: ToLua<'lua>,
    {
        self.inner.add_meta_field_with(meta, f);
    }

    fn add_field_getter(&mut self, name: Vec<u8>, callback: Callback<'lua, 'static>) {
        self.inner.add_field_getter(name, callback);
    }

    fn add_field_setter(&mut self, name: Vec<u8>, callback: Callback<'lua, 'static>) {
        self.inner.add_field_setter(name, callback);
    }
}

impl<'a, 'lua, T, U, M> UserDataFieldsProxy<'a, 'lua, T, U, M, Mutable>
where
    T: UserData + NonBlockingGuardedBorrow<U> + NonBlockingGuardedMutBorrowMut<U>,
    U: UserData,
    M: UserDataFields<'lua, T>,
{
    pub fn new(fields: &'a mut M) -> Self {
        Self {
            inner: fields,
            _phantom: PhantomData,
        }
    }
}

impl<'a, 'lua, T, U, M> UserDataFieldsProxy<'a, 'lua, T, U, M, Immutable>
where
    T: UserData + NonBlockingGuardedBorrow<U>,
    U: UserData,
    M: UserDataFields<'lua, T>,
{
    pub fn new_immutable(fields: &'a mut M) -> Self {
        Self {
            inner: fields,
            _phantom: PhantomData,
        }
    }
}

#[cfg(not(feature = "send"))]
impl<T: 'static + UserData> UserData for Rc<RefCell<T>> {
    fn on_metatable_init(table: Type<Self>) {
        table.add_clone();
    }

    fn add_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        T::add_methods(&mut UserDataMethodsProxy::new(methods));
    }

    fn add_fields<'lua, F: UserDataFields<'lua, Self>>(fields: &mut F) {
        T::add_fields(&mut UserDataFieldsProxy::new(fields))
    }
}

impl<T: 'static + UserData + MaybeSend> UserData for Arc<Mutex<T>> {
    fn on_metatable_init(table: Type<Self>) {
        table.add_clone();

        #[cfg(feature = "send")]
        table.add_send().add_sync();
    }

    fn add_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        T::add_methods(&mut UserDataMethodsProxy::new(methods));
    }

    fn add_fields<'lua, F: UserDataFields<'lua, Self>>(fields: &mut F) {
        T::add_fields(&mut UserDataFieldsProxy::new(fields))
    }
}

impl<T: 'static + UserData + MaybeSend + MaybeSync> UserData for Arc<RwLock<T>> {
    fn on_metatable_init(table: Type<Self>) {
        table.add_clone();

        #[cfg(feature = "send")]
        table.add_send().add_sync();
    }

    fn add_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        T::add_methods(&mut UserDataMethodsProxy::new(methods));
    }

    fn add_fields<'lua, F: UserDataFields<'lua, Self>>(fields: &mut F) {
        T::add_fields(&mut UserDataFieldsProxy::new(fields))
    }
}

pub trait TryCloneToUserDataExt {
    fn try_clone_to_user_data<'lua>(&self, lua: &'lua Lua) -> Result<AnyUserData<'lua>>;
}

impl<T: 'static + UserData> TryCloneToUserDataExt for T {
    fn try_clone_to_user_data<'lua>(&self, lua: &'lua Lua) -> Result<AnyUserData<'lua>> {
        let tt = hv_alchemy::of::<T>();
        let clone_fn = tt.get_clone().ok_or(Error::UserDataDynMismatch)?;

        if tt.is::<dyn Send>() {
            unsafe { lua.make_userdata::<T>(UserDataCell::new(clone_fn(self))) }
        } else {
            Err(Error::UserDataDynMismatch)
        }
    }
}
