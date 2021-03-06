use bytes::IntoBuf;
use futures::{Async, Future, Poll, Stream};
use futures::future::{self, Either};
use futures::sync::mpsc;
use h2::client::{Builder, Handshake, SendRequest};
use tokio_io::{AsyncRead, AsyncWrite};

use body::Payload;
use ::common::{Exec, Never};
use headers;
use super::{PipeToSendStream, SendBuf};
use ::{Body, Request, Response};

type ClientRx<B> = ::client::dispatch::Receiver<Request<B>, Response<Body>>;
/// An mpsc channel is used to help notify the `Connection` task when *all*
/// other handles to it have been dropped, so that it can shutdown.
type ConnDropRef = mpsc::Sender<Never>;

pub struct Client<T, B>
where
    B: Payload,
{
    executor: Exec,
    rx: ClientRx<B>,
    state: State<T, SendBuf<B::Data>>,
}

enum State<T, B> where B: IntoBuf {
    Handshaking(Handshake<T, B>),
    Ready(SendRequest<B>, ConnDropRef),
}

impl<T, B> Client<T, B>
where
    T: AsyncRead + AsyncWrite + Send + 'static,
    B: Payload,
{
    pub(crate) fn new(io: T, rx: ClientRx<B>, exec: Exec) -> Client<T, B> {
        let handshake = Builder::new()
            // we don't expose PUSH promises yet
            .enable_push(false)
            .handshake(io);

        Client {
            executor: exec,
            rx: rx,
            state: State::Handshaking(handshake),
        }
    }
}

impl<T, B> Future for Client<T, B>
where
    T: AsyncRead + AsyncWrite + Send + 'static,
    B: Payload + 'static,
{
    type Item = ();
    type Error = ::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let next = match self.state {
                State::Handshaking(ref mut h) => {
                    let (request_tx, conn) = try_ready!(h.poll().map_err(::Error::new_h2));
                    // An mpsc channel is used entirely to detect when the
                    // 'Client' has been dropped. This is to get around a bug
                    // in h2 where dropping all SendRequests won't notify a
                    // parked Connection.
                    let (tx, rx) = mpsc::channel(0);
                    let rx = rx.into_future()
                        .map(|(msg, _)| match msg {
                            Some(never) => match never {},
                            None => (),
                        })
                        .map_err(|_| -> Never { unreachable!("mpsc cannot error") });
                    let fut = conn
                        .inspect(|_| trace!("connection complete"))
                        .map_err(|e| debug!("connection error: {}", e))
                        .select2(rx)
                        .then(|res| match res {
                            Ok(Either::A(((), _))) |
                            Err(Either::A(((), _))) => {
                                // conn has finished either way
                                Either::A(future::ok(()))
                            },
                            Ok(Either::B(((), conn))) => {
                                // mpsc has been dropped, hopefully polling
                                // the connection some more should start shutdown
                                // and then close
                                trace!("send_request dropped, starting conn shutdown");
                                Either::B(conn)
                            }
                            Err(Either::B((never, _))) => match never {},
                        });
                    self.executor.execute(fut);
                    State::Ready(request_tx, tx)
                },
                State::Ready(ref mut tx, ref conn_dropper) => {
                    try_ready!(tx.poll_ready().map_err(::Error::new_h2));
                    match self.rx.poll() {
                        Ok(Async::Ready(Some((req, mut cb)))) => {
                            // check that future hasn't been canceled already
                            if let Async::Ready(()) = cb.poll_cancel().expect("poll_cancel cannot error") {
                                trace!("request canceled");
                                continue;
                            }
                            let (head, body) = req.into_parts();
                            let mut req = ::http::Request::from_parts(head, ());
                            super::strip_connection_headers(req.headers_mut());
                            if let Some(len) = body.content_length() {
                                headers::set_content_length_if_missing(req.headers_mut(), len);
                            }
                            let eos = body.is_end_stream();
                            let (fut, body_tx) = match tx.send_request(req, eos) {
                                Ok(ok) => ok,
                                Err(err) => {
                                    debug!("client send request error: {}", err);
                                    let _ = cb.send(Err((::Error::new_h2(err), None)));
                                    continue;
                                }
                            };
                            if !eos {
                                let conn_drop_ref = conn_dropper.clone();
                                let pipe = PipeToSendStream::new(body, body_tx)
                                    .map_err(|e| debug!("client request body error: {}", e))
                                    .then(move |x| {
                                        drop(conn_drop_ref);
                                        x
                                    });
                                self.executor.execute(pipe);
                            }

                            let fut = fut
                                .then(move |result| {
                                    match result {
                                        Ok(res) => {
                                            let res = res.map(::Body::h2);
                                            let _ = cb.send(Ok(res));
                                        },
                                        Err(err) => {
                                            debug!("client response error: {}", err);
                                            let _ = cb.send(Err((::Error::new_h2(err), None)));
                                        }
                                    }
                                    Ok(())
                                });
                            self.executor.execute(fut);
                            continue;
                        },

                        Ok(Async::NotReady) => return Ok(Async::NotReady),

                        Ok(Async::Ready(None)) |
                        Err(_) => {
                            trace!("client::dispatch::Sender dropped");
                            return Ok(Async::Ready(()));
                        }
                    }
                },
            };
            self.state = next;
        }
    }
}
