use futures_util::FutureExt;
use futures_core::stream::Stream;
use std::sync::{ Mutex, Arc };
use std::collections::HashMap;
use std::task::{ Poll, Waker };
use std::pin::Pin;
use std::future::Future;
use std::collections::VecDeque;
use crate::backend::{ BackendReceiver, BackendError };
use crate::raw_response::{ RawResponse, ResponseError };

/// An error returned by [`ResponseStreamMaster`] if something
/// goes wrong attempting to receive/parse a message.
#[derive(Debug, derive_more::From, derive_more::Display)]
pub enum ResponseStreamError {
    #[from]
    BackendError(BackendError),
    #[from]
    Response(ResponseError)
}

impl ResponseStreamError {
    /// Is the error fatal, ie the client will no longer work.
    pub fn is_fatal(&self) -> bool {
        matches!(self, ResponseStreamError::BackendError(_))
    }
}

impl std::error::Error for ResponseStreamError {}

/// A single item emitted by a [`ResponseStream`].
pub type ResponseStreamItem = Arc<RawResponse<'static>>;

/// A stream of [`ResponseStreamItem`]s from the backend. This can be cloned, with
/// different streams filtering out different messages.
pub struct ResponseStream {
    id: u64,
    queue: VecDeque<ResponseStreamItem>,
    inner: Arc<Mutex<ResponseStreamInner>>
}

impl Stream for ResponseStream {
    type Item = ResponseStreamItem;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;

        // First, if there are items on our own queue (be it responses or errors),
        // just take and return from them:
        if let Some(item) = this.queue.pop_front() {
            return Poll::Ready(Some(item));
        }

        let mut inner = this.inner.lock().unwrap();
        let inner = &mut *inner;

        let instance = inner.instances.get_mut(&this.id)
            .expect("instance should always exist");

        // Next, if there are items in the shared queue then put those into
        // our local queue and return from them.
        if !instance.queue.is_empty() {
            std::mem::swap(&mut instance.queue, &mut this.queue);
            return Poll::Ready(Some(this.queue.pop_front().expect("queue shouldn't be empty")));
        }

        // If we're done, then stop after draining any messages.
        // Else save our waker and return pending.
        if inner.done {
            Poll::Ready(None)
        } else {
            instance.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl Clone for ResponseStream {
    // Cloning a stream creates a new stream which will receive
    // identical messages to the original.
    fn clone(&self) -> Self {
        let mut inner = self.inner.lock().expect("can obtain lock");

        let old_instance = inner.instances.get(&self.id)
                .expect("instance should exist");

        let new_instance = Instance {
            waker: None,
            queue: old_instance.queue.clone()
        };

        let new_id = inner.next_id;
        inner.next_id += 1;
        inner.instances.insert(new_id, new_instance);

        drop(inner);

        Self {
            id: new_id,
            queue: self.queue.clone(),
            inner: self.inner.clone(),
        }
    }
}

impl Drop for ResponseStream {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            // De-register this instance:
            inner.instances.remove(&self.id);
        }
    }
}

/// A handle to create response streams with.
#[derive(Clone, Debug)]
pub struct ResponseStreamHandle {
    inner: Arc<Mutex<ResponseStreamInner>>
}

impl ResponseStreamHandle {
    /// Create a [`ResponseStream`].
    pub fn response_stream(&self) -> ResponseStream {
        create_response_stream(self.inner.clone())
    }
}

/// This implements [`Future`] and needs to be polled to drive the [`ResponseStream`].
/// [`ResponseStream`]s can be created from this.
pub struct ResponseStreamMaster {
    done: bool,
    // This is where messages from the backend come from.
    recv: Box<dyn BackendReceiver + Send + 'static>,
    // The most recent recv future from the backend.
    recv_fut: Option<Pin<Box<dyn Future<Output = Option<Result<Vec<u8>, BackendError>>> + Send + 'static>>>,
    // Shared state.
    inner: Arc<Mutex<ResponseStreamInner>>
}

impl ResponseStreamMaster {
    /// Create a new [`ResponseStreamMaster`].
    pub fn new(recv: Box<dyn BackendReceiver + Send + 'static>) -> Self {
        ResponseStreamMaster {
            done: false,
            recv,
            recv_fut: None,
            inner: Arc::new(Mutex::new(ResponseStreamInner {
                done: false,
                next_id: 1,
                instances: HashMap::new(),
            }))
        }
    }

    /// Create a handle to use to build response streams.
    pub fn handle(&self) -> ResponseStreamHandle {
        ResponseStreamHandle { inner: self.inner.clone() }
    }
}

impl Stream for ResponseStreamMaster {
    type Item = Result<Arc<RawResponse<'static>>, ResponseStreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;

        if this.done {
            // This has already finished.
            return Poll::Ready(None);
        }

        // Get a future to receive the next item.
        let mut fut = this.recv_fut.take().unwrap_or_else(|| {
            this.recv.receive()
        });

        // Get the item out of the future, or return if it's not ready yet.
        let res = match fut.poll_unpin(cx) {
            Poll::Pending => {
                // Put the future back in the guard if it's not ready yet,
                // so that we re-use it next time we poll.
                this.recv_fut = Some(fut);
                return Poll::Pending
            },
            Poll::Ready(res) => {
                res
            }
        };

        // Get the message out of the result. If a backend error or None comes
        // back, we close any streams.
        let msg_bytes = match res {
            None => {
                // Tell this to stop:
                this.done = true;
                // Tell streams to stop:
                this.inner.lock().unwrap().done = true;
                return Poll::Ready(None)
            }
            Some(Err(err)) => {
                // Tell this to stop:
                this.done = true;
                // Tell streams to stop:
                this.inner.lock().unwrap().done = true;
                return Poll::Ready(Some(Err(err.into())))
            },
            Some(Ok(msg_bytes)) => msg_bytes
        };

        // Deserialize the message, borrowing what we can. An error here is recoverable
        // and won't stop everything.
        let response = match RawResponse::from_bytes(&msg_bytes) {
            Err(err) => {
                // This error can be ignored but will be handed back.
                // The calling code can call the future again to resume.
                return Poll::Ready(Some(Err(err.into())))
            },
            Ok(r) => Arc::new(r.into_owned()),
        };

        // At this point, we need to lock the shared state to
        // broadcast the response.
        let mut inner = this.inner.lock().unwrap();
        let inner = &mut *inner;

        for instance in inner.instances.values_mut() {
            instance.queue.push_back(response.clone());
            if let Some(waker) = instance.waker.take() {
                waker.wake();
            }
        }

        // We've successfully handled a message; allow the caller to react.
        Poll::Ready(Some(Ok(response)))
    }
}

impl Drop for ResponseStreamMaster {
    fn drop(&mut self) {
        if let Ok(mut lock) = self.inner.lock() {
            // if we drop the master, everything else is done.
            lock.done = true;
        }
    }
}

#[derive(Debug)]
struct ResponseStreamInner {
    // Don't poll any more; we hit an error so we're done.
    done: bool,
    // ID to assign the next clone of the ResponseStream. Should be incremented.
    next_id: u64,
    // Track instance details.
    instances: HashMap<u64, Instance>,
}

/// Create a [`ResponseStream`] given the shared `inner` content.
fn create_response_stream(
    inner: Arc<Mutex<ResponseStreamInner>>,
) -> ResponseStream {
    let mut guard = inner.lock().unwrap();
    let id = guard.next_id;

    // "register" this copy of the stream to receive messages.
    guard.next_id += 1;

    let instance = Instance {
        queue: VecDeque::new(),
        waker: None
    };

    guard.instances.insert(id, instance);

    drop(guard);

    ResponseStream {
        id,
        queue: VecDeque::new(),
        inner,
    }
}

/// This represents a single response stream.
#[derive(Debug)]
struct Instance {
    waker: Option<Waker>,
    queue: VecDeque<ResponseStreamItem>
}
