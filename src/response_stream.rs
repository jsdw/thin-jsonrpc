use futures::FutureExt;
use futures::stream::Stream;
use std::sync::{ Mutex, Arc };
use std::collections::HashMap;
use std::task::{ Poll, Waker };
use std::pin::Pin;
use std::future::Future;
use std::collections::VecDeque;
use crate::backend::{ BackendReceiver, BackendError };
use crate::response::{ Response, ResponseError };

/// An error returned by [`ResponseStreamMaster`] if something
/// goes wrong attempting to receive/parse a message.
#[derive(Debug, derive_more::From, derive_more::Display)]
pub enum ResponseStreamError {
    #[from]
    BackendError(BackendError),
    #[from]
    Response(ResponseError)
}

impl std::error::Error for ResponseStreamError {}

/// A single item emitted by a [`ResponseStream`].
pub type ResponseStreamItem = Arc<Response<'static>>;

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

        // Next, if there are items in the shared queue then put those into
        // our local queue and return from them.
        let instance = inner.instances.get_mut(&this.id).expect("instance should exist");
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

impl Drop for ResponseStream {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            // De-register this instance:
            inner.instances.remove(&self.id);
        }
    }
}

/// This implements [`Future`] and needs to be polled to drive the [`ResponseStream`].
/// [`ResponseStream`]s can be created from this.
pub struct ResponseStreamMaster {
    done: bool,
    // This is where messages from the backend come from.
    recv: Box<dyn BackendReceiver>,
    // The most recent recv future from the backend.
    recv_fut: Option<Pin<Box<dyn Future<Output = Result<Vec<u8>, BackendError>>>>>,
    // Shared state.
    inner: Arc<Mutex<ResponseStreamInner>>
}

impl ResponseStreamMaster {
    /// Create a new [`ResponseStreamMaster`].
    pub fn new(recv: Box<dyn BackendReceiver>) -> Self {
        ResponseStreamMaster {
            done: false,
            recv,
            recv_fut: None,
            inner: Arc::new(Mutex::new(ResponseStreamInner {
                done: false,
                next_id: 1,
                instances: HashMap::new()
            }))
        }
    }

    /// Create a [`ResponseStreamHandle`], which doesn't do anything on its
    /// own except allow the creation of [`ResponseStream`]s.
    pub fn handle(&self) -> ResponseStreamHandle {
        ResponseStreamHandle { inner: self.inner.clone() }
    }
}

impl Stream for ResponseStreamMaster {
    type Item = ResponseStreamError;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // We never return from this future unless the backend gives
            // us a Pending (so we know we'll wake again) or we get an error
            // (so we return it and end).
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

            // Get the message out of the result. If a backend error came back, we're done.
            let msg_bytes = match res {
                Err(err) => {
                    // Tell this to stop:
                    this.done = true;
                    // Tell streams to stop:
                    this.inner.lock().unwrap().done = true;
                    return Poll::Ready(Some(err.into()))
                },
                Ok(msg_bytes) => msg_bytes
            };

            // Deserialize the message, borrowing what we can. An error here is recoverable
            // and won't stop everything.
            let response = match Response::from_bytes(&msg_bytes) {
                Err(err) => {
                    // This error can be ignored but will be handed back.
                    // The calling code can call the future again to resume.
                    return Poll::Ready(Some(err.into()))
                },
                Ok(r) => r,
            };

            // At this point, we need to lock the shared state to find out
            // who to broadcast the message to.
            let mut inner = this.inner.lock().unwrap();
            let inner = &mut *inner;

            let mut cached_response_item = None;
            for instance in inner.instances.values_mut() {
                // This instance doesn't care; continue.
                if !(instance.filter_fn)(&response) {
                    continue
                }

                // Clone cached item if it's there (it's an Arc), or take ownership of
                // the `Response` and put it in an Arc for cheaper broadcasting.
                let response_item = match &cached_response_item {
                    None => {
                        let response_item = Arc::new(response.clone().into_owned());
                        cached_response_item = Some(response_item.clone());
                        response_item
                    },
                    Some(res) => res.clone()
                };

                // Tell the instance about the item it's interested in:
                instance.queue.push_back(response_item);
                if let Some(waker) = instance.waker.take() {
                    waker.wake();
                }
            }
        }
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

/// A handle to create [`ResponseStream`]s from.
#[derive(Clone)]
pub struct ResponseStreamHandle {
    inner: Arc<Mutex<ResponseStreamInner>>
}

impl ResponseStreamHandle {
    /// Filter the [`ResponseStream`] based on the function provided. This
    /// function is expected to be fast, and can block progression of other
    /// streams if it is too slow. Prefer the combinators on [`futures::StreamExt`]
    /// if you need to run slow or async logic.
    pub fn with_filter(&self, filter_fn: FilterFn) -> ResponseStream {
        create_response_stream(self.inner.clone(), filter_fn)
    }
}

/// The type of function used to filter the notification stream.
pub type FilterFn = Box<dyn FnMut(&Response<'_>) -> bool + Send + 'static>;

struct ResponseStreamInner {
    // Don't poll any more; we hit an error so we're done.
    done: bool,
    // ID to assign the next clone of the ResponseStream. Should be incremented.
    next_id: u64,
    // Track instance details.
    instances: HashMap<u64, Instance>,
}

/// Create a [`ResponseStream`] given the shared `inner` content.
fn create_response_stream(inner: Arc<Mutex<ResponseStreamInner>>, filter_fn: FilterFn) -> ResponseStream {
    let mut guard = inner.lock().unwrap();
    let id = guard.next_id;

    // "register" this copy of the stream to receive messages.
    guard.next_id += 1;
    guard.instances.insert(id, Instance {
        filter_fn,
        queue: VecDeque::new(),
        waker: None
    });

    drop(guard);

    ResponseStream {
        id,
        queue: VecDeque::new(),
        inner
    }
}

struct Instance {
    waker: Option<Waker>,
    filter_fn: FilterFn,
    queue: VecDeque<ResponseStreamItem>
}