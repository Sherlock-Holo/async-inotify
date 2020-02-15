use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::io;
use std::io::ErrorKind;
use std::ops::Deref;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::sync::mpsc::Sender;
use std::sync::Mutex as StdMutex;
use std::task::{Context, Poll, Waker};
use std::thread;

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
    inotify: Inotify,
    cached_events: VecDeque<Event<OsString>>,
    buf: Rc<RefCell<Vec<u8>>>,
    sender: Sender<Waker>,
    poll_error: Arc<StdMutex<Option<io::Error>>>,
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

        let (sender, receiver) = mpsc::channel();

        let inner_inotify = InnerInotify {
            inotify,
            cached_events: VecDeque::new(),
            buf: Rc::new(RefCell::new(vec![0; 1024])),
            sender,
            poll_error: Arc::new(StdMutex::new(None)),
        };

        let poll_error = Arc::clone(&inner_inotify.poll_error);

        let _executor_job = thread::spawn(move || {
            let mut events = MioEvents::with_capacity(1024);

            loop {
                let mut waker = match receiver.recv() {
                    Err(_) => break, // sender is dropped, means AsyncInotify is useless, can stop reactor thread.
                    Ok(waker) => waker
                };

                while let Err(err) = mio_poll.poll(&mut events, None) {
                    if let ErrorKind::Interrupted = err.kind() {
                        continue;
                    }

                    poll_error.lock().unwrap().replace(err);

                    let _ = mio_poll.registry().deregister(&mut SourceFd(&fd));

                    return;
                }

                while let Ok(new_waker) = receiver.try_recv() {
                    waker = new_waker;
                }

                waker.wake();
            };

            let _ = mio_poll.registry().deregister(&mut SourceFd(&fd));
        });

        Ok(AsyncInotify(Mutex::new(inner_inotify)))
    }

    /// Monitor `path` for the events in `mask`. For a list of events, see
    /// https://docs.rs/tokio-inotify/0.2.1/tokio_inotify/struct.AsyncINotify.html (items prefixed with
    /// "Event")
    pub async fn add_watch<P: AsRef<Path>>(&self, path: P, mask: WatchMask) -> io::Result<WatchDescriptor> {
        self.0.lock().await.inotify.add_watch(path, mask)
    }

    /// Remove an element currently watched.
    pub async fn rm_watch(&self, wd: WatchDescriptor) -> io::Result<()> {
        self.0.lock().await.inotify.rm_watch(wd)
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

        // check if poll thread fail
        {
            let mut poll_error_guard = guard.poll_error.lock().unwrap();
            if poll_error_guard.is_some() {
                let poll_error = poll_error_guard.take().unwrap();
                return Poll::Ready(Some(Err(poll_error)));
            }
        }

        if let Some(first_event) = guard.cached_events.pop_front() {
            return Poll::Ready(Some(Ok(first_event)));
        }

        let buf = Rc::clone(&guard.buf);

        let mut buf_mut = buf.deref().borrow_mut();

        match guard.inotify.read_events(&mut buf_mut) {
            Err(err) => Poll::Ready(Some(Err(err))),

            Ok(mut events) => {
                if let Some(first_event) = events.next() {
                    for event in events {
                        guard.cached_events.push_back(Event {
                            wd: event.wd,
                            mask: event.mask,
                            cookie: event.cookie,
                            name: event.name.map(OsStr::to_os_string),
                        });
                    }

                    Poll::Ready(Some(Ok(Event {
                        wd: first_event.wd,
                        mask: first_event.mask,
                        cookie: first_event.cookie,
                        name: first_event.name.map(OsStr::to_os_string),
                    })))
                } else if guard.sender.send(cx.waker().clone()).is_err() {
                    let mut poll_error_guard = guard.poll_error.lock().unwrap();

                    // check if poll thread fail
                    if poll_error_guard.is_some() {
                        let poll_error = poll_error_guard.take().unwrap();

                        Poll::Ready(Some(Err(poll_error)))
                    } else {
                        Poll::Ready(Some(Err(io::Error::from(ErrorKind::BrokenPipe))))
                    }
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::env::temp_dir;
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

        let watch_descriptor = async_inotify.add_watch(&tmp_file, WatchMask::CLOSE).await.unwrap();

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

        let watch_descriptor = async_inotify.add_watch(&tmp_file, WatchMask::MODIFY | WatchMask::ACCESS).await.unwrap();

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

    #[tokio::test]
    async fn test_dir() {
        let mut async_inotify = AsyncInotify::init().unwrap();

        let tmp_dir = tempfile::tempdir_in(temp_dir()).unwrap();

        let watch_descriptor = async_inotify.add_watch(&tmp_dir, WatchMask::CREATE).await.unwrap();

        let tmp_file = NamedTempFile::new_in(&tmp_dir).unwrap();

        let event = async_inotify.next().await.unwrap().unwrap();

        assert_eq!(event.wd, watch_descriptor);
        assert_eq!(event.mask, EventMask::CREATE);
        assert_eq!(event.name, Some(tmp_file.path().file_name().unwrap().to_os_string()));
    }
}