macro_rules! tuples {
    ($macro:ident) => {
        $macro!();
        $macro!(T0);
        $macro!(T0, T1);
        $macro!(T0, T1, T2);
        $macro!(T0, T1, T2, T3);
        $macro!(T0, T1, T2, T3, T4);
        $macro!(T0, T1, T2, T3, T4, T5);
        $macro!(T0, T1, T2, T3, T4, T5, T6);
        $macro!(T0, T1, T2, T3, T4, T5, T6, T7);
        $macro!(T0, T1, T2, T3, T4, T5, T6, T7, T8);
        $macro!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9);
        $macro!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10);
        $macro!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11);
        $macro!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12);
        $macro!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13);
        $macro!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14);
    };
}

macro_rules! impl_deref {
    ($ty:tt) => {
        impl<T: Send + Sync> std::ops::Deref for $ty<T> {
            type Target = T;
            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        impl<T: Send + Sync> std::ops::DerefMut for $ty<T> {
            fn deref_mut(&mut self) -> &mut Self::Target {
                &mut self.0
            }
        }
    };

    ($ty:tt,$target:ty) => {
        impl std::ops::Deref for $ty {
            type Target = $target;
            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        impl std::ops::DerefMut for $ty {
            fn deref_mut(&mut self) -> &mut Self::Target {
                &mut self.0
            }
        }
    };
}

/// Implement empty OpenAPI schema for a type without requiring `utoipa` in downstream crates.
#[macro_export]
macro_rules! ignore_openapi {
    ($ty:ty) => {
        impl $crate::PartialSchema for $ty {
            fn schema() -> $crate::openapi::SchemaRef {
                <$crate::openapi::IgnoreOpenApi<$ty> as $crate::PartialSchema>::schema()
            }
        }

        impl $crate::ToSchema for $ty {}
    };
}
