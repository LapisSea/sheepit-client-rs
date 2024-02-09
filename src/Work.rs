use std::future::Future;
use lazy_static::lazy_static;
use tokio::task::JoinHandle;

lazy_static! {
     pub static ref TOKIO_RT: tokio::runtime::Runtime = tokio::runtime::Runtime::new().unwrap();
}

pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
	where
		F: Future + Send + 'static,
		F::Output: Send + 'static,
{
	TOKIO_RT.spawn(future)
}

pub fn block<F: Future>(future: F) -> F::Output
	where
		F: Future + Send + 'static,
		F::Output: Send + 'static,
{
	TOKIO_RT.block_on(future)
}