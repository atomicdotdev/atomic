//! Optional Node-API exports. All native code stays in libatomic; JavaScript
//! consumers load the compiled addon, not a second Rust wrapper crate.

use std::sync::LazyLock;

use napi::bindgen_prelude::Buffer;
use napi_derive::napi;

use crate::LocalService;

static SERVICE: LazyLock<LocalService> = LazyLock::new(LocalService::new);

#[napi]
pub async fn call(method: String, request: Buffer) -> napi::Result<Buffer> {
    SERVICE
        .call(&method, request.as_ref())
        .await
        .map(Buffer::from)
        .map_err(|error| {
            napi::Error::new(
                napi::Status::GenericFailure,
                format!("{}: {}", error.code(), error.message()),
            )
        })
}

#[napi]
pub fn descriptor() -> Buffer {
    Buffer::from(crate::FILE_DESCRIPTOR_SET.to_vec())
}
