use anyhow::anyhow;
use futures::future::BoxFuture;
use tokio::sync::{mpsc, oneshot};

pub type ActorFut<'a, T> = BoxFuture<'a, T>;
pub type Action<A> = Box<
    dyn for<'a> FnOnce(&'a mut A) -> ActorFut<'a, ()> + Send + 'static
>;

pub trait Actor: Send + 'static {
    fn run(&mut self) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
}

#[derive(Debug)]
pub struct Handle<A> {
    tx: mpsc::Sender<Action<A>>,
}

impl<A> Clone for Handle<A> {
    fn clone(&self) -> Self {
        Self { tx: self.tx.clone() }
    }
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
        R: Send + 'static,
        F: for<'a> FnOnce(&'a mut A) -> ActorFut<'a, anyhow::Result<R>>
            + Send
            + 'static,
    {
        let (rtx, rrx) = oneshot::channel::<anyhow::Result<R>>();
        self.tx
            .send(Box::new(move |actor: &mut A| {
                Box::pin(async move {
                    let res = f(actor).await;
                    let _ = rtx.send(res);
                })
            }))
            .await
            .map_err(|_| anyhow!("actor stopped"))?;
        rrx.await.map_err(|_| anyhow!("actor stopped"))?
    }

    #[allow(dead_code)]
    pub async fn cast<F>(&self, f: F) -> anyhow::Result<()>
    where
        F: for<'a> FnOnce(&'a mut A) -> ActorFut<'a, ()> + Send + 'static,
    {
        self.tx
            .send(Box::new(move |actor: &mut A| f(actor)))
            .await
            .map_err(|_| anyhow!("actor stopped"))
    }
}