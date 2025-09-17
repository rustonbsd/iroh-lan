
use std::{boxed, future::Future, pin::Pin};

use anyhow::anyhow;
use futures::FutureExt;
use tokio::sync::{mpsc, oneshot};

pub type PreBoxActorFut<'a, T> = dyn Future<Output = T> + Send + 'a;
pub type ActorFut<'a, T> = Pin<boxed::Box<PreBoxActorFut<'a, T>>>;
pub type Action<A> = Box<dyn for<'a> FnOnce(&'a mut A) -> ActorFut<'a, ()> + Send + 'static>;

pub fn into_actor_fut_res<'a, Fut, T>(
    fut: Fut,
) -> ActorFut<'a, anyhow::Result<T>>
where
    Fut: Future<Output = anyhow::Result<T>> + Send + 'a,
    T: Send + 'a,
{
    Box::pin(fut)
}

pub fn into_actor_fut_ok<'a, Fut, T>(
    fut: Fut,
) -> ActorFut<'a, anyhow::Result<T>>
where
    Fut: Future<Output = T> + Send + 'a,
    T: Send + 'a,
{
    Box::pin(async move { Ok::<T, anyhow::Error>(fut.await) })
}

pub fn into_actor_fut_unit_ok<'a, Fut>(
    fut: Fut,
) -> ActorFut<'a, anyhow::Result<()>>
where
    Fut: Future<Output = ()> + Send + 'a,
{
    Box::pin(async move {
        fut.await;
        Ok::<(), anyhow::Error>(())
    })
}

#[macro_export]
macro_rules! act {
    // takes single expression that yields Result<T, anyhow::Error>
    ($actor:ident => $expr:expr) => {{
        move |$actor| $crate::actor::into_actor_fut_res(($expr))
    }};

    // takes a block that yields Result<T, anyhow::Error> and can use ?
    ($actor:ident => $body:block) => {{
        move |$actor| $crate::actor::into_actor_fut_res($body)
    }};
}

#[macro_export]
macro_rules! act_ok {
    // takes single expression that yields T
    ($actor:ident => $expr:expr) => {{
        move |$actor| $crate::actor::into_actor_fut_ok(($expr))
    }};

    // For call() with a block that returns T (no ?), we map to Ok(T)
    // takes a block that yields T (no = u)
    ($actor:ident => $body:block) => {{
        move |$actor| $crate::actor::into_actor_fut_ok($body)
    }};
}

pub trait Actor: Send + 'static {
    fn run(&mut self) -> impl Future<Output = anyhow::Result<()>> + Send;
}

#[derive(Debug)]
pub struct Handle<A> {
    tx: mpsc::Sender<Action<A>>,
}

impl<A> Clone for Handle<A> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

pub fn box_fut<'a, T, Fut>(fut: Fut) -> ActorFut<'a, T>
where
    Fut: Future<Output = T> + Send + 'a,
{
    Box::pin(fut)
}

impl<A> Handle<A>
where
    A: Send + 'static,
{
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<Action<A>>) {
        let (tx, rx) = mpsc::channel(capacity);
        (Self { tx }, rx)
    }

    pub async fn call<R, F>(&self, f: F) -> anyhow::Result<R>
    where
        F: for<'a> FnOnce(&'a mut A) -> ActorFut<'a, anyhow::Result<R>> + Send + 'static,
        R: Send + 'static,
    {
        let (rtx, rrx) = oneshot::channel::<anyhow::Result<R>>();
        let loc = std::panic::Location::caller();

        self.tx
            .send(Box::new(move |actor: &mut A| {
                Box::pin(async move {
                    let res = std::panic::AssertUnwindSafe(async move { f(actor).await })
                        .catch_unwind()
                        .await
                        .map_err(|p| {
                            let msg = if let Some(s) = p.downcast_ref::<&str>() {
                                (*s).to_string()
                            } else if let Some(s) = p.downcast_ref::<String>() {
                                s.clone()
                            } else {
                                "unknown panic".to_string()
                            };
                            anyhow!(
                                "panic in actor call at {}:{}: {}",
                                loc.file(),
                                loc.line(),
                                msg
                            )
                        })
                        .and_then(|r| r);

                    let _ = rtx.send(res);
                })
            }))
            .await
            .map_err(|_| anyhow!("actor stopped (call send at {}:{})", loc.file(), loc.line()))?;

        rrx.await
            .map_err(|_| anyhow!("actor stopped (call recv at {}:{})", loc.file(), loc.line()))?
    }
}
