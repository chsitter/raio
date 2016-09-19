extern crate libc;
#[macro_use]
extern crate log;

pub mod future;
mod kqueue;

use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{channel, Sender, Receiver};
use std::time::Duration;
use std::sync::{Arc, Mutex, Condvar};
use std::thread::JoinHandle;
use std::thread;
use std::io::prelude::*;
use std::os::unix::io::IntoRawFd;
use std::os::unix::io::FromRawFd;
use std::os::unix::io::AsRawFd;
use std::collections::VecDeque;
use future::Future;
use kqueue::{Kqueue, ReadEventType};
use std::collections::HashMap;

pub trait AsyncTcpListener {
    fn accept_async<'a, F, T: Executor>(&self, event_loop: &'a T, accept_cb: F) where F: Fn(&mut TcpListener) -> EventControl + Send + 'a;
}

pub trait AsyncTcpStream {
    fn read_async<'a, F, T: Executor>(&self, event_loop: &'a T, read_cb: F) where F: Fn(&mut TcpStream) -> EventControl + Send + 'a;
    fn write_async<'a, T: Executor>(&self, event_loop: &'a T, data: Vec<u8>) -> Future;
}

pub enum EventControl {
    KEEP,
    DELETE
}

pub trait Executor : Drop {
    fn new(name: &str) -> Self;
    fn execute<F: Fn() + Send + 'static>(&self, callback: F);
    fn schedule<F: Fn() -> EventControl + Send + 'static>(&self, callback: F, delay: Duration) -> Future;

    fn shutdown(&self);
    fn join(&mut self);

    fn accept<F: Fn(&mut TcpListener) -> EventControl + Send>(&self, listener: TcpListener, callback: F);
    fn read<F: Fn(&mut TcpStream) -> EventControl + Send>(&self, stream: TcpStream, callback: F);
    fn write(&self, stream: TcpStream, data: Vec<u8>) -> Future;

    fn notify(&self);
}

impl AsyncTcpListener for TcpListener {
    fn accept_async<'a, F, T: Executor>(&self, event_loop: &'a T, accept_cb: F) where F: Fn(&mut TcpListener) -> EventControl + Send + 'a {
        self.set_nonblocking(true).unwrap();

        event_loop.accept(self.try_clone().unwrap(), accept_cb);
    }
}

impl AsyncTcpStream for TcpStream {
    fn read_async<'a, F, T: Executor>(&self, event_loop: &'a T, read_cb: F) where F: Fn(&mut TcpStream) -> EventControl + Send + 'a {
        self.set_nonblocking(true).unwrap();

        event_loop.read(self.try_clone().unwrap(), read_cb);
    }

    fn write_async<'a, T: Executor>(&self, event_loop: &'a T, data: Vec<u8>) -> Future {
        self.set_nonblocking(true).unwrap();

        event_loop.write(self.try_clone().unwrap(), data)
    }
}

enum ThreadMessage {
    Shutdown,
    Execute {
        callback: Box<Fn() + Send>
    },
    Schedule {
        delay: Duration,
        callback: Box<Fn() -> EventControl + Send>
    },
    AddAcceptEvent {
        fd: i32,
        callback: Box<Fn(&mut TcpListener) -> EventControl + Send>
    },
    AddReadEvent {
        fd: i32,
        callback: Box<Fn(&mut TcpStream) -> EventControl + Send>
    },
    AddWriteEvent {
        fd: i32,
        payload: Vec<u8>,
        future: Future
    }
}

pub struct SingleThreadedExecutor {
    join_handle: Mutex<Option<JoinHandle<()>>>,
    kq: Kqueue,
    sender: Mutex<Sender<ThreadMessage>>
}

impl Executor for SingleThreadedExecutor {
    fn new(name: &str) -> Self {

        let (tx, rx): (Sender<ThreadMessage>, Receiver<ThreadMessage>) = channel();
        let pair = Arc::new((Mutex::new(false), Condvar::new()));

        let pair2 = pair.clone();
        let mut tmp = Kqueue::new();
        let x = SingleThreadedExecutor {
            sender: Mutex::new(tx),
            kq: tmp.clone(),
            join_handle: Mutex::new(Some(thread::Builder::new().name(name.to_string()).spawn( move || {
                executor_loop(tmp, rx, &*pair2); //Get this to work again
            }).unwrap()))
        };

        let &(ref lock, ref cvar) = &*pair;
        let mut started = lock.lock().unwrap();
        while !*started {
            started = cvar.wait(started).unwrap();
        }

        x
    }

    fn execute<F: Fn() + Send + 'static>(&self, callback: F) {
        let s = self.sender.lock().unwrap();
        s.send(ThreadMessage::Execute {
            callback: Box::new(callback)
        }).unwrap();

        self.notify();
    }

    fn schedule<F: Fn() -> EventControl + Send + 'static>(&self, callback: F, delay: Duration) -> Future {
        let s = self.sender.lock().unwrap();
        s.send(ThreadMessage::Schedule {
            delay: delay,
            callback: Box::new(callback)
        }).unwrap();

        self.notify();
        Future::new()
    }

    fn accept<F: Fn(&mut TcpListener) -> EventControl + Send + 'static>(&self, listener: TcpListener, callback: F) {
        let s = self.sender.lock().unwrap();
        s.send(ThreadMessage::AddAcceptEvent {
            fd: listener.into_raw_fd(),
            callback: Box::new(callback)
        }).unwrap();

        self.notify();
    }

    fn read<F: Fn(&mut TcpStream) -> EventControl + Send + 'static>(&self, stream: TcpStream, callback: F) {
        let s = self.sender.lock().unwrap();
        s.send(ThreadMessage::AddReadEvent {
            fd: stream.into_raw_fd(),
            callback: Box::new(callback)
        }).unwrap();

        self.notify();
    }

    fn write(&self, stream: TcpStream, data: Vec<u8>) -> Future {
        let s = self.sender.lock().unwrap();
        let future = Future::new();
        let fut1 = future.clone();
        s.send(ThreadMessage::AddWriteEvent {
            fd: stream.into_raw_fd(),
            payload: data,
            future: fut1
        }).unwrap();

        future
    }

    fn shutdown(&self) {
        let s = self.sender.lock().unwrap();
        match s.send(ThreadMessage::Shutdown) {
            Ok(()) => {},
            Err(e) => println!("Error occurred!! {}", e)
        }

        self.notify();
    }

    fn notify(&self) {
        self.kq.notify();
    }

    fn join(&mut self) {
        let mut handle = self.join_handle.lock().unwrap();
        if let Some(x) = handle.take() {
            x.join().unwrap();
        }
    }
}

impl Drop for SingleThreadedExecutor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

enum CallbackType {
    ACCEPT(Box<Fn(&mut TcpListener) -> EventControl>),
    READ(Box<Fn(&mut TcpStream) -> EventControl>)
}

fn executor_loop(mut kq: Kqueue, receiver: Receiver<ThreadMessage>, pair: &(Mutex<bool>, Condvar)) {
    let mut write_queues: Arc<Mutex<HashMap<usize, VecDeque<(Vec<u8>, Future)>>>> = Arc::new(Mutex::new(HashMap::new()));
    let &(ref lock, ref cvar) = pair;
    {
        let mut started = lock.lock().unwrap();
        *started = true;
    }
    cvar.notify_one();

    loop {
        loop {
            match receiver.try_recv() { //This should be registered with  kevent too
                Ok(ThreadMessage::Shutdown)        => break,
                Ok(ThreadMessage::AddAcceptEvent{ fd, callback }) => {
                    kq.add_read_event(fd as usize, ReadEventType::ACCEPT(callback));
                },
                Ok(ThreadMessage::AddReadEvent{ fd, callback }) => {
                    kq.add_read_event(fd as usize, ReadEventType::READ(callback));
                },
                Ok(ThreadMessage::AddWriteEvent{ fd, payload, future }) => {
                    {
                        let mut locked_write_queues = write_queues.lock().unwrap();
                        if !locked_write_queues.contains_key(&(fd as usize)) {
                        locked_write_queues.insert(fd as usize, VecDeque::new()); 
                        } 

                        if let Some(ref mut queue) = locked_write_queues.get_mut(&(fd as usize)) {
                            queue.push_back((payload, future));
                        }
                    }

                    let mut moved_queues = write_queues.clone();
                    kq.add_write_event(fd as usize, Box::new(move |s| {
                        println!("should be writing");
                        let mut write_queues = moved_queues.lock().unwrap();

                        if let Some(ref mut queue) = write_queues.get_mut(&(fd as usize)) {
                            let mut bucket_written = false;
                            println!("Writing 1");
                            if let Some(&mut (ref mut data, ref mut future)) = queue.front_mut() {
                                println!("Writing 2");
                                unsafe {
                                    let mut stream = TcpStream::from_raw_fd(fd);
                                    if let Ok(bytes_written) = stream.write(data) {
                                        let data_len = data.len();
                                        if bytes_written == data_len {
                                            bucket_written = true;
                                            println!("Bucket written");
                                            future.set();
                                        } else {
                                            println!("Partial write");
                                            for i in 0 .. data_len - bytes_written {
                                                data.swap(i, bytes_written + i);
                                            }
                                            data.truncate(data_len - bytes_written);
                                        }
                                    }
                                    stream.into_raw_fd();
                                }
                            } else {
                                println!("Delete write event as there's nothing left to write");
                                return EventControl::DELETE
                            }

                            if bucket_written == true {
                                println!("popping");
                                queue.pop_front();
                            }
                        } else {
                            println!("Delete write event");
                            return EventControl::DELETE
                        }

                        return EventControl::KEEP
                    })); 
                },
                Ok(ThreadMessage::Execute{ callback }) => {
                    callback();
                },
                Ok(ThreadMessage::Schedule{ delay, callback }) => {
                   kq.add_timer(callback, delay);
                },
                Err(_) => {
                    break;
                }
            }
        }

       kq.handle_events();
    }
}

