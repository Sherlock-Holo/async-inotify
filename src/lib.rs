use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::io;
use std::io::ErrorKind;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::pin::Pin;
use std::sync::mpsc;
use std::sync::mpsc::Sender;
use std::task::{Context, Poll, Waker};
use std::thread;
use std::vec::IntoIter;

use futures::stream;
use futures::stream::Iter;
use futures::stream::Stream;
pub use inotify::{Event, EventMask, WatchDescriptor, WatchMask};
use inotify::Inotify;
use mio::{Poll as MioPoll, Token};
use mio::Events as MioEvents;
use mio::Interest;
use mio::unix::SourceFd;
use tokio::pin;
use tokio::sync::Mutex;

struct InnerInotify {
    inotify: Option<Inotify>,
    cached_events: Option<Iter<IntoIter<Event<OsString>>>>,
    sender: Sender<Waker>,
}

impl Drop for InnerInotify {
    fn drop(&mut self) {
        let _ = self.inotify.take().unwrap().close();
    }
}

/// Wraps an Inotify object and provides asynchronous methods based on the inner object.
pub struct AsyncInotify(Mutex<InnerInotify>);

impl AsyncInotify {
    /// Create a new inotify stream on the loop behind `handle`.
    pub fn init() -> io::Result<Self> {
        AsyncInotify::init_with_flags(0)
    }

    /// Create a new inotify stream with the given inotify flags (`IN_NONBLOCK` or `IN_CLOEXEC`).
    pub fn init_with_flags(_flags: i32) -> io::Result<Self> {
        let inotify = Inotify::init()?;

        let fd = inotify.as_raw_fd();

        let mio_poll = MioPoll::new()?;

        mio_poll.registry().register(&mut SourceFd(&fd), Token(0), Interest::READABLE)?;

        let mut mio_poll = MioPoll::new()?;

        let (sender, receiver) = mpsc::channel::<Waker>();

        let _executor_job = thread::spawn(move || {
            let mut events = MioEvents::with_capacity(1024);

            loop {
                let waker = match receiver.recv() {
                    Err(_) => break, // sender is dropped, means AsyncInotify is useless, can stop reactor thread.
                    Ok(waker) => waker
                };

                while let Err(err) = mio_poll.poll(&mut events, None) {
                    if let ErrorKind::Interrupted = err.kind() {
                        continue;
                    }

                    panic!(err); // TODO should I handle it?
                }

                waker.wake();
            };

            mio_poll.registry().deregister(&mut SourceFd(&fd))
        });

        let inner_inotify = InnerInotify {
            inotify: Some(inotify),
            cached_events: None,
            sender,
        };

        Ok(AsyncInotify(Mutex::new(inner_inotify)))
    }

    /// Monitor `path` for the events in `mask`. For a list of events, see
    /// https://docs.rs/tokio-inotify/0.2.1/tokio_inotify/struct.AsyncINotify.html (items prefixed with
    /// "Event")
    #[inline]
    pub async fn add_watch<P: AsRef<Path>>(&self, path: P, mask: WatchMask) -> io::Result<WatchDescriptor> {
        if let Some(inotify) = &mut self.0.lock().await.inotify {
            inotify.add_watch(path, mask)
        } else {
            unreachable!()
        }
    }

    /// Remove an element currently watched.
    #[inline]
    pub async fn rm_watch(&self, wd: WatchDescriptor) -> io::Result<()> {
        if let Some(inotify) = &mut self.0.lock().await.inotify {
            inotify.rm_watch(wd)
        } else {
            unreachable!()
        }
    }
}

impl Stream for AsyncInotify {
    type Item = io::Result<Event<OsString>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let guard = self.0.lock();

        pin!(guard);

        let mut guard = match guard.poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(guard) => guard
        };

        if let Some(events) = &mut guard.cached_events {
            pin!(events);

            match events.poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(event)) => return Poll::Ready(Some(Ok(event))),

                Poll::Ready(None) => guard.cached_events.take(),
            };
        }

        if let Some(inotify) = &mut guard.inotify {
            let mut buf = vec![0; 1024];

            match inotify.read_events(&mut buf) {
                Err(err) => Poll::Ready(Some(Err(err))),

                Ok(mut events) => {
                    if let Some(event) = events.next() {
                        let events: Vec<_> = events.map(|event| {
                            Event {
                                wd: event.wd,
                                mask: event.mask,
                                cookie: event.cookie,
                                name: event.name.map(OsStr::to_os_string),
                            }
                        }).collect();

                        guard.cached_events.replace(stream::iter(events));

                        Poll::Ready(Some(Ok(Event {
                            wd: event.wd,
                            mask: event.mask,
                            cookie: event.cookie,
                            name: event.name.map(OsStr::to_os_string),
                        })))
                    } else {
                        guard.sender.send(cx.waker().clone()).unwrap();
                        Poll::Pending
                    }
                }
            }
        } else {
            unreachable!()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::prelude::*;

    use futures::io::SeekFrom;
    use futures::StreamExt;
    use inotify::EventMask;
    use tempfile::NamedTempFile;

    use super::*;

    #[tokio::test]
    async fn test_oneshot() {
        let mut async_inotify = AsyncInotify::init().unwrap();

        let tmp_file = NamedTempFile::new().unwrap();

        let path = tmp_file.path().to_path_buf();

        let watch_descriptor = async_inotify.add_watch(&path, WatchMask::CLOSE).await.unwrap();

        tmp_file.close().unwrap();

        let access_event = async_inotify.next().await.unwrap().unwrap();

        assert_eq!(access_event.mask, EventMask::CLOSE_WRITE);

        assert_eq!(access_event.name, None); // because watch file directly

        assert_eq!(access_event.wd, watch_descriptor);
    }

    #[tokio::test]
    async fn test_multi() {
        let mut async_inotify = AsyncInotify::init().unwrap();

        let mut tmp_file = NamedTempFile::new().unwrap();

        let path = tmp_file.path().to_path_buf();

        let watch_descriptor = async_inotify.add_watch(&path, WatchMask::MODIFY | WatchMask::ACCESS).await.unwrap();

        writeln!(tmp_file, "test").unwrap();

        tmp_file.flush().unwrap();

        tmp_file.seek(SeekFrom::Start(0)).unwrap();

        let mut buf = vec![0; 1];

        tmp_file.read_exact(&mut buf).unwrap();

        let modify_event = async_inotify.next().await.unwrap().unwrap();

        assert_eq!(modify_event.mask, EventMask::MODIFY);

        assert_eq!(modify_event.name, None); // because watch file directly

        assert_eq!(modify_event.wd, watch_descriptor);

        let access_event = async_inotify.next().await.unwrap().unwrap();

        assert_eq!(access_event.mask, EventMask::ACCESS);

        assert_eq!(access_event.name, None); // because watch file directly

        assert_eq!(access_event.wd, watch_descriptor);
    }
}