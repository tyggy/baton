//! Streaming response body fed from a channel: the producer task writes SSE
//! events, hyper sends each as soon as it arrives. When the client goes
//! away hyper drops the body, the receiver closes and the producer's next
//! `send` fails — the stream's "client gone" signal (Python: BrokenPipe on
//! the next write).

use bytes::Bytes;
use hyper::body::{Body, Frame};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;

pub struct ChannelBody {
    rx: mpsc::Receiver<Bytes>,
}

/// Capacity 1: like a blocking socket write, the producer waits while the
/// previous event is still unsent instead of queueing frames.
pub fn channel() -> (mpsc::Sender<Bytes>, ChannelBody) {
    let (tx, rx) = mpsc::channel(1);
    (tx, ChannelBody { rx })
}

impl Body for ChannelBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        self.rx.poll_recv(cx).map(|o| o.map(|b| Ok(Frame::data(b))))
    }
}
