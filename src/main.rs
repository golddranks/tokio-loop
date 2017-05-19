extern crate tokio_core;
extern crate tokio_io;
extern crate futures;
#[macro_use]
extern crate log;
extern crate env_logger;
extern crate bytes;
extern crate regex;

use tokio_core::net::{TcpStream, TcpStreamNew};
use tokio_core::reactor::{Handle, Core, Timeout};
use tokio_io::codec::{Encoder, Decoder};
use tokio_io::AsyncRead;
use futures::{Stream, Poll, Async, task, Future, Sink};
use futures::unsync::mpsc;
use futures::task::Task;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;
use bytes::BytesMut;
use std::marker::PhantomData;
use regex::bytes::Regex;


const PORT: u16 = 80;
const HOST: &str = "ec2-52-199-164-246.ap-northeast-1.compute.amazonaws.com";
const HTTP_BODY: &[u8] = b"GET / HTTP/1.1\r\nHost: ec2-52-199-164-246.ap-northeast-1.compute.amazonaws.com\r\nConnection: keep-alive\r\n\r\n";

//const PORT: u16 = 8080;
//const HOST: &str = "127.0.0.1";
//const HTTP_BODY: &[u8] = b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: keep-alive\r\n\r\n";


struct SingleHostConnector {
    handle: Handle,
    addr: SocketAddr,
}

impl Stream for SingleHostConnector {
    type Item = TcpStreamNew;
    type Error = io::Error;
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {

        let c = TcpStream::connect(&self.addr, &self.handle);
        Ok(Async::Ready(Some(c)))

    }
}





#[derive(Debug)]
struct ReqStream {
    body: &'static [u8],
    streak: u32,
    yield_channel: mpsc::UnboundedSender<(Task, oneshot::Sender<()>)>,
    waiting_unpark: Option<oneshot::Receiver<()>>,
}

impl ReqStream {
    fn new(channel: mpsc::UnboundedSender<(Task, oneshot::Sender<()>)>) -> Self {
        Self  {
            body: HTTP_BODY,
            streak: 0,
            yield_channel: channel,
            waiting_unpark: None,
        }
    }
}


impl Stream for ReqStream {
    type Item = &'static [u8];
    type Error = io::Error;
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {

        if let Some(ref mut rx) = self.waiting_unpark {
            match rx.poll() {
                Ok(Async::Ready(())) => debug!("Unparked!"),
                Ok(Async::NotReady) => return { debug!("ReqStream is not unparked yet."); Ok(Async::NotReady) },
                Err(oneshot::Canceled) => panic!("oneshot canceled!"),
            }
        }
        self.waiting_unpark = None;

        if self.streak > 10 {
            debug!("Parking the request stream!");
            let (tx, rx) = oneshot::channel();
            self.waiting_unpark = Some(rx);
            let task = task::park();
            mpsc::UnboundedSender::send(&self.yield_channel, (task, tx)).unwrap();
            self.streak = 0;
            return Ok(Async::NotReady);
        }
        self.streak += 1;
        debug!("Sending request!!");
        Ok(Async::Ready(Some(&*self.body)))
    }
}





pub fn response_eater(_: BytesMut) -> Result<(), io::Error> {
    debug!("Got response!");
    Ok(())
}





#[derive(Debug)]
pub struct HttpCodec<'a> {
    phantom: PhantomData<&'a ()>,
    regex: Regex,
}

impl<'a> HttpCodec<'a> {
    pub fn new() -> Self {
        Self {
            phantom: PhantomData,
            regex: Regex::new("\r\n\r\n").expect("Invariant: the regex is always correct"),
        }
    }
}

impl<'a> Encoder for HttpCodec<'a> {
    type Item = &'a [u8];
    type Error = io::Error;
    fn encode(&mut self, item: Self::Item, dst: &mut BytesMut) -> Result<(), Self::Error> {
        dst.extend(item);
        Ok(())
    }
}

impl<'a> Decoder for HttpCodec<'a> {
    type Item = BytesMut;
    type Error = io::Error;
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let mut start = 0;
        let mut end = 0;
        if let Some(i) = self.regex.find(&*src) {
            start = i.start();
            end = i.end();
        }
        if end > 0 {
            // I guess that without lexical lifetimes splitting the if is the best we can do
            let mut line = src.split_to(end);
            let line = line.split_to(start);
            Ok(Some(line))
        } else {
            Ok(None)
        }
    }

    fn decode_eof(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        debug!("EOF reached! Remaining buffer contents: {:?}", src);
        Ok(None)
    }
}







use futures::unsync::oneshot;

fn main() {

    env_logger::init().expect("Can't initialize logging!");
    let mut core = Core::new().unwrap();
    let handle = core.handle();

    let addr = (HOST, PORT)
        .to_socket_addrs()
        .unwrap()
        .next()
        .unwrap();

    let connector = SingleHostConnector {
        addr,
        handle: handle.clone(),
    };

    let (tx, rx) = mpsc::unbounded();

    let unparker = rx.for_each(|item: (Task, oneshot::Sender<()>)| {
        debug!("Unparking the request stream");
        item.0.unpark();
        let _ = item.1.send(());
        Ok(())
    });
    
    let sender_stream = connector
        .map(|conn_futures| {
            conn_futures
                .and_then(|stream| {
                    println!("Stream: {:?}", stream.nodelay());
                    stream.set_nodelay(true).unwrap();
                    println!("Stream: {:?}", stream.nodelay());
                    let (writer_sink, reader_stream) = stream.framed(HttpCodec::new()).split();
                    let write_future = writer_sink
                        .send_all(ReqStream::new(tx.clone()))
                        .then(|r| {
                                  debug!("Writer closed. Result: {:?}", r);
                                  Ok::<(), io::Error>(())
                              });
                    let read_future = reader_stream
                        .for_each(response_eater)
                        .then(|r| {
                                  debug!("Reader closed. Result: {:?}", r);
                                  Ok::<(), io::Error>(())
                              });
                    write_future
                        .select(read_future)
                        .then(|_| { debug!("Then called."); Ok::<(), io::Error>(())} )
                })
                .map(|_| ())
        })
        .buffer_unordered(1)
        .for_each(|_| Ok(()));

    let end_time = Timeout::new(Duration::from_secs(30), &handle)
        .unwrap()
        .then(|_| {
                  debug!("timeout.");
                  Ok::<(), io::Error>(())
              });

    let all = end_time
        .select(sender_stream)
        .then(|_| Ok::<(), ()>(()))
        .select(unparker)
        .then(|_| Ok::<(), ()>(()));

    core.run(all).expect("FIXME");

}
