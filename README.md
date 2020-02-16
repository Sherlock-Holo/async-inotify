# async-inotify

**NOTE: The `inotify` crate now offers a `Stream`-based API. It is recommended to use it directly.**

[![crates.io](https://img.shields.io/crates/v/tokio-inotify.svg)](https://crates.io/crates/tokio-inotify)

[Documentation](https://docs.rs/tokio-inotify/)

The `async_inotify` crate enables the use of inotify file descriptors in the `tokio` or `async-std` framework.
It builds on the [`inotify`](https://github.com/hannobraun/inotify-rs) crate by wrapping
the `Inotify` type into a new type called `AsyncInotify`, and implementing `Stream`.

This means that you can consume `inotify::Event`s from the `AsyncInotify` object and act on them.

## License

MIT