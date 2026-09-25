//! The channel a streamed response travels through: a render thread sends,
//! the HTTP worker drains it as the client reads.
//!
//! Bounded by bytes, not messages, so a slow client holds a render thread
//! rather than growing its buffer. The sender waits with a deadline because
//! the execution-timeout hook cannot fire while it does.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use actix_web::web::Bytes;

use crate::runtime::LuaCookie;

/// Status, headers and cookies, fixed by the first flush.
pub struct Head {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub cookies: Vec<LuaCookie>,
}

pub enum Message {
    Head(Head),
    Chunk(Bytes),
}

#[derive(Debug, PartialEq, Eq)]
pub enum SendError {
    /// The client went away.
    Disconnected,
    /// The client stopped reading until the deadline passed.
    Timeout,
}

/// Why a stream ended without [`Sender::finish`]: the render failed after the
/// head went out, so the body must read as incomplete.
#[derive(Debug)]
pub struct Truncated;

impl std::fmt::Display for Truncated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "streamed response ended early")
    }
}

impl std::error::Error for Truncated {}

struct Inner {
    queue: VecDeque<Message>,
    queued_bytes: usize,
    finished: bool,
    sender_gone: bool,
    receiver_gone: bool,
    waker: Option<Waker>,
}

struct Shared {
    inner: Mutex<Inner>,
    space: Condvar,
    capacity: usize,
}

pub struct Sender {
    shared: Arc<Shared>,
}

pub struct Receiver {
    shared: Arc<Shared>,
}

/// A send waits while `capacity` bytes are queued; one into an empty queue
/// always goes through, however large.
pub fn channel(capacity: usize) -> (Sender, Receiver) {
    let shared = Arc::new(Shared {
        inner: Mutex::new(Inner {
            queue: VecDeque::new(),
            queued_bytes: 0,
            finished: false,
            sender_gone: false,
            receiver_gone: false,
            waker: None,
        }),
        space: Condvar::new(),
        capacity,
    });
    (
        Sender {
            shared: Arc::clone(&shared),
        },
        Receiver { shared },
    )
}

fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, Inner> {
    shared.inner.lock().unwrap_or_else(|e| e.into_inner())
}

impl Sender {
    pub fn send(&self, message: Message, deadline: Instant) -> Result<(), SendError> {
        let size = match &message {
            Message::Head(_) => 0,
            Message::Chunk(bytes) => bytes.len(),
        };
        let mut inner = lock(&self.shared);
        loop {
            if inner.receiver_gone {
                return Err(SendError::Disconnected);
            }
            if inner.queued_bytes < self.shared.capacity {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(SendError::Timeout);
            }
            inner = self
                .shared
                .space
                .wait_timeout(inner, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        inner.queued_bytes += size;
        inner.queue.push_back(message);
        if let Some(waker) = inner.waker.take() {
            waker.wake();
        }
        Ok(())
    }

    /// Ends the body cleanly. Dropping without this truncates it.
    pub fn finish(self) {
        lock(&self.shared).finished = true;
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        let mut inner = lock(&self.shared);
        inner.sender_gone = true;
        if let Some(waker) = inner.waker.take() {
            waker.wake();
        }
    }
}

impl Receiver {
    /// The next message; `None` once the sender is gone and the queue empty,
    /// `Some(Err)` if it went without finishing.
    pub fn poll_message(&self, cx: &mut Context<'_>) -> Poll<Option<Result<Message, Truncated>>> {
        let mut inner = lock(&self.shared);
        if let Some(message) = inner.queue.pop_front() {
            if let Message::Chunk(bytes) = &message {
                inner.queued_bytes -= bytes.len();
            }
            self.shared.space.notify_one();
            return Poll::Ready(Some(Ok(message)));
        }
        if inner.sender_gone {
            // Reported once: the stream ends after the error.
            if !std::mem::replace(&mut inner.finished, true) {
                return Poll::Ready(Some(Err(Truncated)));
            }
            return Poll::Ready(None);
        }
        inner.waker = Some(cx.waker().clone());
        Poll::Pending
    }

    pub async fn next_message(&mut self) -> Option<Result<Message, Truncated>> {
        std::future::poll_fn(|cx| self.poll_message(cx)).await
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        lock(&self.shared).receiver_gone = true;
        self.shared.space.notify_one();
    }
}

/// The body of a streamed response: chunks only, the head having been taken.
impl futures_util::Stream for Receiver {
    type Item = Result<Bytes, Truncated>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            return match self.poll_message(cx) {
                Poll::Ready(Some(Ok(Message::Chunk(bytes)))) => Poll::Ready(Some(Ok(bytes))),
                // Only the first message is a head.
                Poll::Ready(Some(Ok(Message::Head(_)))) => continue,
                Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn chunk(s: &'static str) -> Message {
        Message::Chunk(Bytes::from_static(s.as_bytes()))
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    fn drain(rx: &Receiver) -> Vec<Result<String, ()>> {
        let mut cx = Context::from_waker(Waker::noop());
        let mut out = Vec::new();
        while let Poll::Ready(Some(item)) = rx.poll_message(&mut cx) {
            out.push(match item {
                Ok(Message::Chunk(b)) => Ok(String::from_utf8(b.to_vec()).unwrap()),
                Ok(Message::Head(_)) => Ok("<head>".to_string()),
                Err(_) => Err(()),
            });
        }
        out
    }

    #[test]
    fn a_finished_stream_ends_cleanly() {
        let (tx, rx) = channel(1024);
        tx.send(chunk("a"), soon()).unwrap();
        tx.send(chunk("b"), soon()).unwrap();
        tx.finish();
        assert_eq!(drain(&rx), vec![Ok("a".into()), Ok("b".into())]);
    }

    #[test]
    fn an_unfinished_stream_ends_in_an_error() {
        let (tx, rx) = channel(1024);
        tx.send(chunk("a"), soon()).unwrap();
        drop(tx);
        assert_eq!(drain(&rx), vec![Ok("a".into()), Err(())]);
    }

    #[test]
    fn a_full_queue_times_out() {
        let (tx, _rx) = channel(2);
        tx.send(chunk("abc"), soon()).unwrap();
        let started = Instant::now();
        let deadline = started + Duration::from_millis(50);
        assert_eq!(tx.send(chunk("d"), deadline), Err(SendError::Timeout));
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn draining_unblocks_the_sender() {
        let (tx, rx) = channel(2);
        tx.send(chunk("abc"), soon()).unwrap();
        let sender = std::thread::spawn(move || tx.send(chunk("d"), soon()));
        std::thread::sleep(Duration::from_millis(20));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(rx.poll_message(&mut cx).is_ready());
        assert_eq!(sender.join().unwrap(), Ok(()));
    }

    #[test]
    fn a_dropped_receiver_disconnects_a_waiting_sender() {
        let (tx, rx) = channel(2);
        tx.send(chunk("abc"), soon()).unwrap();
        let sender = std::thread::spawn(move || tx.send(chunk("d"), soon()));
        std::thread::sleep(Duration::from_millis(20));
        drop(rx);
        assert_eq!(sender.join().unwrap(), Err(SendError::Disconnected));
    }
}
