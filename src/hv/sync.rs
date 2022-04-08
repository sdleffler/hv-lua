use std::sync::Arc;

use hv_alchemy::Type;
use hv_cell::{ArcCell, ArcRef, ArcRefMut, AtomicRefCell};
use hv_elastic::{Elastic, StretchedMut, StretchedRef};

use crate::{
    types::{MaybeSend, MaybeSync},
    userdata::{UserDataFieldsProxy, UserDataMethodsProxy},
    UserData, UserDataFields, UserDataMethods,
};

impl<T: 'static + UserData + MaybeSend + MaybeSync> UserData for Arc<AtomicRefCell<T>> {
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

impl<T: 'static + UserData + MaybeSend + MaybeSync> UserData for ArcCell<T> {
    fn on_metatable_init(table: Type<Self>) {
        table.add_clone();

        #[cfg(feature = "send")]
        table.add_send().add_sync();
    }

    fn add_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("borrow", |_lua, this, ()| Ok(this.borrow()));
        methods.add_method("borrow_mut", |_lua, this, ()| Ok(this.borrow_mut()));
        methods.add_method("try_borrow", |_lua, this, ()| Ok(this.try_borrow().ok()));
        methods.add_method("try_borrow_mut", |_lua, this, ()| {
            Ok(this.try_borrow_mut().ok())
        });
    }
}

impl<T, C> UserData for ArcRef<T, C>
where
    T: 'static + UserData + MaybeSend + MaybeSync,
    C: 'static + ?Sized + MaybeSend + MaybeSync,
{
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

impl<T, C> UserData for ArcRefMut<T, C>
where
    T: 'static + UserData + MaybeSend + MaybeSync,
    C: 'static + ?Sized + MaybeSend + MaybeSync,
{
    #[allow(unused_variables)]
    fn on_metatable_init(table: Type<Self>) {
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

impl<T: 'static + UserData + MaybeSend + MaybeSync> UserData for Elastic<StretchedMut<T>> {
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

impl<T: 'static + UserData + MaybeSend + MaybeSync> UserData for Elastic<StretchedRef<T>> {
    fn on_metatable_init(table: Type<Self>) {
        table.add_clone();

        #[cfg(feature = "send")]
        table.add_send().add_sync();
    }

    fn add_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        T::add_methods(&mut UserDataMethodsProxy::new_immutable(methods));
    }

    fn add_fields<'lua, F: UserDataFields<'lua, Self>>(fields: &mut F) {
        T::add_fields(&mut UserDataFieldsProxy::new_immutable(fields))
    }
}
