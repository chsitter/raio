extern crate libc;

use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{channel, Sender, Receiver};
use std::time::Duration;
use std::sync::{Arc, Mutex, Condvar};
use std::thread::JoinHandle;
use std::thread;
use std::io::prelude::*;
use std::os::unix::io::IntoRawFd;
use std::os::unix::io::FromRawFd;
use std::collections::VecDeque;

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

pub struct Future {
    condvar: Arc<((Mutex<bool>, Condvar))>
}

impl Clone for Future {
    fn clone(&self) -> Future {
        Future {
            condvar: self.condvar.clone()
        }
    }
}
impl Future {

    pub fn new() -> Future {
        Future {
            condvar: Arc::new((Mutex::new(false), Condvar::new()))
        }
    }

    pub fn get(self) {
        let &(ref lock, ref cvar) = &*self.condvar;
        let mut started = lock.lock().unwrap();
        while !*started {
            started = cvar.wait(started).unwrap();
        }

    }

    pub fn set(&mut self) {
        let &(ref lock, ref cvar) = &*self.condvar;
        let mut started = lock.lock().unwrap();
        *started = true;
        cvar.notify_one();
    }
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
    join_handle: Mutex<Option<JoinHandle<()>>>,  //TODO: make RefCell to not need to have mut executors?
    kqid: i32,
    sender: Mutex<Sender<ThreadMessage>>
}

impl Executor for SingleThreadedExecutor {
    fn new(name: &str) -> Self {

        let (tx, rx): (Sender<ThreadMessage>, Receiver<ThreadMessage>) = channel();
        let pair = Arc::new((Mutex::new(false), Condvar::new()));
        let kq = unsafe { libc::kqueue() as i32 };

        let pair2 = pair.clone();
        let x = SingleThreadedExecutor {
            sender: Mutex::new(tx),
            kqid: kq,
            join_handle: Mutex::new(Some(thread::Builder::new().name(name.to_string()).spawn( move || {
                executor_loop(kq, rx, &*pair2);
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
        let ev = libc::kevent {
            ident: 0,
            filter: libc::EVFILT_USER,
            flags: 0,
            fflags: libc::NOTE_TRIGGER,
            data: 0,
            udata: std::ptr::null_mut()
        };

        unsafe {
            libc::kevent(self.kqid, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null_mut());
        }
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

fn executor_loop(kq: i32, receiver: Receiver<ThreadMessage>, pair: &(Mutex<bool>, Condvar)) {
    let mut nev;

    let num_fds: usize = unsafe {
                let mut rlim: libc::rlimit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
                libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim as *mut libc::rlimit );
                rlim.rlim_cur   //TODO: should we be using rlim_max?
    } as usize;
    let max_timers:usize = 4096;
    let mut next_timer:usize = 0;

    let mut readevents: Vec<Option<(CallbackType)>> = Vec::with_capacity(num_fds);  //TODO: maybe i should put it on the heap and stick it on to the event context
    let mut write_queues: Vec<Option<VecDeque<(Vec<u8>, Future)>>> = Vec::with_capacity(num_fds);
    let mut timers: Vec<Option<(Box<Fn() -> EventControl>)>> = Vec::with_capacity(max_timers);  //TODO: maybe i should put it on the heap and stick it on to the event context
    for _ in 0..num_fds {
        readevents.push(None);
        write_queues.push(None);
    }
    for _ in 0..max_timers {
        timers.push(None);
    }

    let &(ref lock, ref cvar) = pair;
    {
        let mut started = lock.lock().unwrap();
        *started = true;
    }
    cvar.notify_one();


    let mut ev_list: [libc::kevent; 32] = [ libc::kevent { ident: 0, filter: 0, flags: 0, fflags: 0, data: 0, udata: std::ptr::null_mut() };32];

    let user_ev = libc::kevent {
        ident: 0,
        filter: libc::EVFILT_USER,
        flags: libc::EV_ADD | libc::EV_CLEAR,
        fflags: 0,
        data: 0,
        udata: std::ptr::null_mut()
    };

    unsafe {
        libc::kevent(kq, &user_ev, 1, std::ptr::null_mut(), 0, std::ptr::null_mut());
    }


    loop {
        loop {
            match receiver.try_recv() { //This should be registered with  kevent too
                Ok(ThreadMessage::Shutdown)        => break,
                Ok(ThreadMessage::AddAcceptEvent{ fd, callback }) => {
                    println!("registering accept event for fd {}", fd);
                    readevents[fd as usize] = Some((CallbackType::ACCEPT(callback))); //TOOD: check whether it's better this way or to move it on to the context of the kevent

                    let ev_set = libc::kevent {
                        ident: fd as libc::uintptr_t,
                        filter: libc::EVFILT_READ,
                        flags: libc::EV_ADD,
                        fflags: 0,
                        data: 0,
                        udata: std::ptr::null_mut()
                    };

                    unsafe {
                        libc::kevent(kq, &ev_set, 1, std::ptr::null_mut(), 0, std::ptr::null_mut());
                    }
                },
                Ok(ThreadMessage::AddReadEvent{ fd, callback }) => {
                    println!("added read event");
                    readevents[fd as usize] = Some((CallbackType::READ(callback)));

                    let ev_set = libc::kevent {
                        ident: fd as libc::uintptr_t,
                        filter: libc::EVFILT_READ,
                        flags: libc::EV_ADD,
                        fflags: 0,
                        data: 0,
                        udata: std::ptr::null_mut()
                    };

                    unsafe {
                        libc::kevent(kq, &ev_set, 1, std::ptr::null_mut(), 0, std::ptr::null_mut());
                    }
                },
                Ok(ThreadMessage::AddWriteEvent{ fd, payload, future }) => {
                    println!("added write event to fd {:?}", fd);
                    let mut found = false;

                    if let Some(_) = write_queues[fd as usize] {
                       found = true;
                    }

                    if found == true {
                        if let Some(ref mut x) = write_queues[fd as usize] {
                            x.push_back((payload, future));
                        }
                    } else {
                        let mut tmp = VecDeque::new();
                        tmp.push_back((payload, future));
                        write_queues[fd as usize] = Some(tmp);
                    }

                    let ev_set = libc::kevent {
                        ident: fd as libc::uintptr_t,
                        filter: libc::EVFILT_WRITE,
                        flags: libc::EV_ADD,
                        fflags: 0,
                        data: 0,
                        udata: std::ptr::null_mut()
                    };

                    unsafe {
                        libc::kevent(kq, &ev_set, 1, std::ptr::null_mut(), 0, std::ptr::null_mut());
                    }
                },
                Ok(ThreadMessage::Execute{ callback }) => {
                    callback();
                },
                Ok(ThreadMessage::Schedule{ delay, callback }) => {
                    while let Some(_) = timers[next_timer] {
                        next_timer = (next_timer + 1) % max_timers;
                    }
                    timers[next_timer] = Some(callback);

                    let ev_set = libc::kevent {
                        ident: next_timer,
                        filter: libc::EVFILT_TIMER,
                        flags: libc::EV_ADD | libc::EV_ENABLE,
                        fflags: 0,
                        data: (delay.as_secs() * 1000 + (delay.subsec_nanos() / 1000000u32) as u64) as isize,
                        udata: std::ptr::null_mut()
                    };

                    unsafe {
                        libc::kevent(kq, &ev_set, 1, std::ptr::null_mut(), 0, std::ptr::null_mut());
                    }
                },
                Err(_) => {
                    break;
                }
            }
        }

        unsafe {
            nev = libc::kevent(kq, std::ptr::null(), 0, ev_list.as_mut_ptr(), 32, std::ptr::null_mut());
        }

        match nev {
            -1   => println!("Error occured"),
            0   => {}, //println!("Fired without any events"),
            num => {
                //println!("got {} events", num);

                for i in  0..num {
                    let fd = ev_list[i as usize].ident as i32;
                    let filt = ev_list[i as usize].filter as i16;
                    match filt {
                        libc::EVFILT_READ => {
                            if readevents[fd as usize].is_some() {
                                let mut deleted = false;
                                if let Some(ref cb_type) = readevents[fd as usize] {
                                    match cb_type {
                                        &CallbackType::ACCEPT(ref cb)  => unsafe {
                                            println!("accepting from fd {}", fd);
                                            let mut listener = TcpListener::from_raw_fd(fd);
                                            match (cb)(&mut listener) {
                                                EventControl::DELETE => {
                                                    let ev_set = libc::kevent {
                                                        ident: fd as libc::uintptr_t,
                                                        filter: libc::EVFILT_READ,
                                                        flags: libc::EV_DELETE,
                                                        fflags: 0,
                                                        data: 0,
                                                        udata: std::ptr::null_mut()
                                                    };

                                                    libc::kevent(kq, &ev_set, 1, std::ptr::null_mut(), 0, std::ptr::null_mut());
                                                    deleted = true;
                                                },
                                                EventControl::KEEP => {
                                                    listener.into_raw_fd();
                                                }
                                            }
                                        },
                                        &CallbackType::READ(ref cb) => unsafe {
                                            println!("reading from fd {}", fd);
                                            let mut stream = TcpStream::from_raw_fd(fd);
                                            match (cb)(&mut stream) {
                                                EventControl::DELETE => {
                                                    let ev_set = libc::kevent {
                                                        ident: fd as libc::uintptr_t,
                                                        filter: libc::EVFILT_READ,
                                                        flags: libc::EV_DELETE,
                                                        fflags: 0,
                                                        data: 0,
                                                        udata: std::ptr::null_mut()
                                                    };

                                                    libc::kevent(kq, &ev_set, 1, std::ptr::null_mut(), 0, std::ptr::null_mut());
                                                    deleted = true;
                                                },
                                                EventControl::KEEP  => {
                                                    stream.into_raw_fd();
                                                }
                                            }
                                        }
                                    }
                                }
                                if deleted == true {
                                    readevents[fd as usize].take();
                                }
                            }
                        },
                        libc::EVFILT_WRITE => {
                            //TODO: allow for custom write routines to be registered and used
                            println!("Writing");
                            if let Some(ref mut queue) = write_queues[fd as usize] {
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
                                    let ev_set = libc::kevent {
                                        ident: fd as libc::uintptr_t,
                                        filter: libc::EVFILT_WRITE,
                                        flags: libc::EV_DELETE,
                                        fflags: 0,
                                        data: 0,
                                        udata: std::ptr::null_mut()
                                    };

                                    unsafe {
                                        libc::kevent(kq, &ev_set, 1, std::ptr::null_mut(), 0, std::ptr::null_mut());
                                    }
                                }
                                if bucket_written == true {
                                    println!("popping");
                                    queue.pop_front();
                                }
                            } else {
                                    println!("Delete write event");
                                let ev_set = libc::kevent {
                                    ident: fd as libc::uintptr_t,
                                    filter: libc::EVFILT_WRITE,
                                    flags: libc::EV_DELETE,
                                    fflags: 0,
                                    data: 0,
                                    udata: std::ptr::null_mut()
                                };

                                unsafe {
                                    libc::kevent(kq, &ev_set, 1, std::ptr::null_mut(), 0, std::ptr::null_mut());
                                }
                            }
                        },
                        libc::EVFILT_TIMER => {
                            let mut deleted = false;
                            if let Some(ref c) = timers[fd as usize] {
                                match (*c)() {
                                    EventControl::DELETE => {
                                        let ev_set = libc::kevent {
                                            ident: fd as libc::uintptr_t,
                                            filter: libc::EVFILT_TIMER,
                                            flags: libc::EV_DELETE,
                                            fflags: 0,
                                            data: 0,
                                            udata: std::ptr::null_mut()
                                        };

                                        unsafe {
                                            libc::kevent(kq, &ev_set, 1, std::ptr::null_mut(), 0, std::ptr::null_mut());
                                        }
                                        deleted = true;
                                    }
                                    EventControl::KEEP => {
                                        //noop
                                    }
                                }
                            }

                            if deleted == true {
                                timers[fd as usize].take();
                            }
                        }
                        libc::EVFILT_USER => {
                        }
                        x => {
                            println!("unhandled event {}", x);
                        }
                    }
                }
            }
        }
    }
}

