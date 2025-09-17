use std::{boxed, future::Future, pin::Pin};

use anyhow::anyhow;
use futures::FutureExt;
use tokio::sync::{mpsc, oneshot};

pub type PreBoxActorFut<'a, T> = dyn Future<Output = T> + Send + 'a;
pub type ActorFut<'a, T> = Pin<boxed::Box<PreBoxActorFut<'a, T>>>;
pub type Action<A> = Box<dyn for<'a> FnOnce(&'a mut A) -> ActorFut<'a, ()> + Send + 'static>;

// Ergonomic helper to avoid writing Box::pin(...) at call sites
pub trait IntoActorFut<'a, T>: Sized {
    fn into_actor_fut(self) -> ActorFut<'a, T>;
}
impl<'a, T, Fut> IntoActorFut<'a, T> for Fut
where
    Fut: Future<Output = T> + Send + 'a,
{
    fn into_actor_fut(self) -> ActorFut<'a, T> {
        Box::pin(self)
    }
}

#[macro_export]
macro_rules! act {
    ($actor:ident => $expr:expr) => {{ move |$actor| $crate::actor::box_fut(($expr)) }};
}

#[macro_export]
macro_rules! act_async {
    ($actor:ident => $body:block) => {{
        move |$actor| $crate::actor::box_fut(async move $body)
    }};
}

#[macro_export]

macro_rules! act_async_ok {
    ($actor:ident => $body:block) => {{ move |$actor| $crate::actor::box_fut(async move { Ok::<_, anyhow::Error>($body) }) }};
}

#[macro_export]
macro_rules! act_ok {
    ($actor:ident => $expr:expr) => {{
        move |$actor| {
            $crate::actor::box_fut(async move {
                $expr;
                Ok(())
            })
        }
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

    pub async fn cast<Fut, F>(&self, f: F) -> anyhow::Result<()>
    where
        F: for<'a> FnOnce(&'a mut A) -> ActorFut<'a, anyhow::Result<()>> + Send + 'static,
    {
        let loc = std::panic::Location::caller();

        self.tx
            .send(Box::new(move |actor: &mut A| {
                Box::pin(async move {
                    if let Err(p) = std::panic::AssertUnwindSafe(async move { f(actor).await })
                        .catch_unwind()
                        .await
                    {
                        let msg = if let Some(s) = p.downcast_ref::<&str>() {
                            (*s).to_string()
                        } else if let Some(s) = p.downcast_ref::<String>() {
                            s.clone()
                        } else {
                            "unknown panic".to_string()
                        };
                        eprintln!(
                            "panic in actor cast at {}:{}: {}",
                            loc.file(),
                            loc.line(),
                            msg
                        );
                    }
                })
            }))
            .await
            .map_err(|_| anyhow!("actor stopped (cast at {}:{})", loc.file(), loc.line()))
    }
}
