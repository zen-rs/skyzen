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

macro_rules! impl_base_responder {
    ($($ty:ty),*) => {
        $(
            impl Responder for $ty {
                fn respond_to(
                    self,
                    _request: &http_kit::Request,
                    response:&mut http_kit::Response,
                ) -> http_kit::Result<()> {
                    response.headers_mut().insert(http_kit::header::CONTENT_TYPE,http_kit::header::HeaderValue::from_static("application/octet-stream"));
                    *response.body_mut() = http_kit::Body::from(self);
                    Ok(())
                }
            }
        )*
    };
}

macro_rules! impl_base_utf8_responder {
    ($($ty:ty),*) => {
        $(
            impl Responder for $ty {
                fn respond_to(
                    self,
                    _request: &http_kit::Request,
                    response: &mut http_kit::Response,
                ) -> http_kit::Result<()> {
                    response.headers_mut().insert(http_kit::header::CONTENT_TYPE,http_kit::header::HeaderValue::from_static("text/plain; charset=utf-8"));
                    *response.body_mut() = http_kit::Body::from(self);
                    Ok(())
                }
            }
        )*
    };
}
