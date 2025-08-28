// actor_lite.rs
use anyhow::anyhow;
use futures::future::BoxFuture;
use tokio::sync::{mpsc, oneshot};

// Future that may borrow the actor for 'a.
pub type ActorFut<'a, T> = BoxFuture<'a, T>;

// An action: given a mutable borrow of the actor, run an async op that may
// borrow the actor for the duration of the await.
pub type Action<A> = Box<
    dyn for<'a> FnOnce(&'a mut A) -> ActorFut<'a, ()> + Send + 'static
>;

#[derive(Clone,Debug)]
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

    // Ask: produce a future that may borrow the actor (HRTB), not 'static.
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
                // We’re inside the actor task; run the user future that may borrow `actor`.
                Box::pin(async move {
                    let res = f(actor).await;
                    let _ = rtx.send(res);
                })
            }))
            .await
            .map_err(|_| anyhow!("actor stopped"))?;
        rrx.await.map_err(|_| anyhow!("actor stopped"))?
    }

    // Cast: fire-and-forget. Same HRTB pattern.
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