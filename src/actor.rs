#[macro_export]
macro_rules! api_methods {
    ($handle:ty, $actor:ty, {
        $( fn $name:ident ( $( $arg:ident : $ty:ty ),* $(,)? ) -> $ret:ty ; )*
        $( cast fn $cname:ident ( $( $carg:ident : $cty:ty ),* $(,)? ); )*
    }) => {
        impl $handle {
            $( pub async fn $name(&self, $( $arg : $ty ),* ) -> $ret {
                self.call(|a: &mut $actor| a.$name($( $arg ),*)).await
            } )*
            $( pub async fn $cname(&self, $( $carg : $cty ),* ) -> anyhow::Result<()> {
                self.cast(|a: &mut $actor| a.$cname($( $carg ),*)).await
            } )*
        }
    };
}

use anyhow::{Result, anyhow};
use std::future::Future;
use std::pin::Pin;
use tokio::sync::{mpsc, oneshot};

pub type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

// Note the HRTB on the trait object here:
pub type Action<A> = Box<dyn for<'a> FnOnce(&'a mut A) -> BoxFut<()> + Send + 'static>;

#[derive(Debug, Clone)]
pub struct Handle<A> {
    tx: mpsc::Sender<Action<A>>,
}

impl<A> Handle<A>
where
    A: Send + 'static,
{
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<Action<A>>) {
        let (tx, rx) = mpsc::channel(capacity);
        (Self { tx }, rx)
    }

    pub async fn call<R, Fut, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        Fut: Future<Output = Result<R>> + Send + 'static,
        F: for<'a> FnOnce(&'a mut A) -> Fut + Send + 'static,
    {
        let (rtx, rrx) = oneshot::channel::<Result<R>>();

        let send_res = self
            .tx
            .send(Box::new(move |actor: &mut A| {
                let fut = f(actor);
                Box::pin(async move {
                    let res = fut.await;
                    let _ = rtx.send(res);
                }) as BoxFut<()>
            }) as Action<A>)
            .await;

        if send_res.is_err() {
            return Err(anyhow!("actor stopped"));
        }

        rrx.await.map_err(|_| anyhow!("actor stopped"))?
    }

    pub async fn cast<Fut, F>(&self, f: F) -> Result<()>
    where
        Fut: Future<Output = ()> + Send + 'static,
        F: for<'a> FnOnce(&'a mut A) -> Fut + Send + 'static,
    {
        self.tx
            .send(Box::new(move |actor: &mut A| Box::pin(f(actor)) as BoxFut<()>) as Action<A>)
            .await
            .map_err(|_| anyhow!("actor stopped"))
    }
}
