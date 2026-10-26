use crate::{Result, storage_error};

// The public storage traits are synchronous. postgres and the S3 adapter own
// runtimes; avoid entering either runtime from inside an async worker.
pub(crate) fn run<T: Send>(work: impl FnOnce() -> Result<T> + Send) -> Result<T> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(work)
        }
        Ok(_) => std::thread::scope(|scope| {
            scope
                .spawn(work)
                .join()
                .map_err(|_| storage_error("storage worker panicked"))?
        }),
        Err(_) => work(),
    }
}
